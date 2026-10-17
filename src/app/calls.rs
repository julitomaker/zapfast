//! The window's side of a call: which account it is on, what the call
//! surface shows, the desktop notification, and the actions the surface
//! pushes, routed to the backend of the account the call belongs to.
//!
//! The call's state comes from the worker that owns it (`Event::Call`); the
//! window never decides what a call is doing, with one exception: a call
//! whose button was just pressed is drawn as placing at once, before the
//! worker has answered, and the worker's updates take over from there. One
//! call at a time across every account, since there is one microphone: an
//! offer to another account while a call is up is declined there.
//!
//! The tones follow the phase: the ringback while the peer's phone rings (the
//! protocol's `<preaccept>`), the ringtone while a call to us waits.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::{App, CALL_FAREWELL, notification_sound};
use crate::backend::{Backend, Command};
use crate::call_tones::Tone;
use crate::calls::{CallPhase, CallUpdate, RecordingOp};
use crate::model::{AccountId, Action, CallDirection, Chat, ChatId, PendingRecord};
use crate::settings::NotificationSound;

/// One person on a group call, as the call screen draws them.
#[derive(Clone, Debug, PartialEq)]
pub struct CallPerson {
    pub id: String,
    pub name: String,
    pub picture: Option<std::path::PathBuf>,
    /// Whether the call service counts them as in the call, not only asked.
    pub joined: bool,
    /// Whether their camera's picture is arriving.
    pub video: bool,
}

/// How long a call may stay placing with no word from its worker before the
/// screen gives up on it. Placing waits on device checks, the camera (whose
/// first use on macOS asks permission) and the peer's devices, so this is
/// generous; it is only there so a lost answer never leaves the screen stuck.
const PLACING_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a recording asked of the worker holds the record button before
/// it may be asked again: the worker answers at once unless it is gone.
const RECORDING_ANSWER: Duration = Duration::from_secs(10);

impl App {
    /// Whether a call is up or ringing, on any account.
    pub fn call_live(&self) -> bool {
        self.call.as_ref().is_some_and(|call| call.phase.is_live())
    }

    /// Whether the full call screen is what the reader is looking at: a call
    /// the reader has not stepped away from. A ringing call is answered, not
    /// stepped away from, and a finished one is only saying how it ended.
    pub fn call_surface_open(&self) -> bool {
        self.call
            .as_ref()
            .is_some_and(|call| call.phase.is_live() && call.phase != CallPhase::Incoming)
            && !self.call_surface_hidden
    }

    /// Whether a recording of the call `generation` was asked of the worker
    /// and its answer has not come yet: the record button waits meanwhile.
    pub fn call_record_pending(&self, generation: u64) -> bool {
        self.call_record_requested.is_some_and(|(requested, at)| {
            requested == generation && at.elapsed() < RECORDING_ANSWER
        })
    }

    /// Whether this chat of the account on screen has the live call, which is
    /// what the chat header's call button reflects. A finished call keeps its
    /// snapshot for the farewell, but that is no call to hang up.
    pub fn call_live_here(&self, id: &str) -> bool {
        self.call_account.as_ref() == Some(&self.account().id)
            && self
                .call
                .as_ref()
                .is_some_and(|call| call.chat == id && call.phase.is_live())
    }

    /// Runs `f` with the call's account as the active one, so names and
    /// pictures come from the account the call is on.
    fn with_call_account<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let index = self
            .call_account
            .as_ref()
            .and_then(|id| self.accounts.iter().position(|account| &account.id == id))
            .unwrap_or(self.active);
        let real = std::mem::replace(&mut self.active, index);
        let result = f(self);
        self.active = real;
        result
    }

    /// Whether the call belongs to a chat the lock is hiding right now: a
    /// locked chat says nothing until its folder is open, and a folder is only
    /// ever open for the account on screen.
    pub(crate) fn call_is_private(&self) -> bool {
        let (Some(call), Some(account)) = (&self.call, &self.call_account) else {
            return false;
        };
        let Some(owner) = self.accounts.iter().find(|owner| &owner.id == account) else {
            return false;
        };
        let locked = owner
            .chats
            .iter()
            .any(|chat| chat.id == call.chat && chat.locked);
        let on_screen = !self.events_hidden && owner.id == self.account().id;
        locked && !(on_screen && self.locked_folder_open())
    }

    /// The name the call shows for the other side: the chat's own title (a
    /// group's subject for a group call), or "Locked chat" while the lock
    /// hides it.
    pub fn call_name(&mut self) -> String {
        let Some(call) = self.call.as_ref() else {
            return String::new();
        };
        // A call joined through a link belongs to no chat.
        if call.group.as_ref().is_some_and(|group| group.link) {
            return crate::i18n::gettext(self.locale, "Call link").into_owned();
        }
        let chat = call.chat.clone();
        if self.call_is_private() {
            return crate::i18n::gettext(self.locale, "Locked chat").into_owned();
        }
        self.with_call_account(|app| app.chat_title_of(&chat))
    }

    /// Who rang a group call, by name: `None` for a one-to-one call, whose
    /// chat already says who, and while the lock hides the chat.
    pub fn call_caller_name(&mut self) -> Option<String> {
        let caller = self.call.as_ref()?.group.as_ref()?.caller.clone()?;
        if self.call_is_private() {
            return None;
        }
        Some(self.with_call_account(|app| app.display_name(&caller)))
    }

    /// Ourselves on a group call, as our own tile shows us: our photo under
    /// "You". `None` on a one-to-one call.
    pub fn call_self(&mut self) -> Option<CallPerson> {
        self.call.as_ref()?.group.as_ref()?;
        let you = crate::i18n::gettext(self.locale, "You").into_owned();
        self.with_call_account(|app| {
            let id = app.me.clone().unwrap_or_default();
            Some(CallPerson {
                picture: (!id.is_empty()).then(|| app.avatar(&id)).flatten(),
                name: you,
                id,
                joined: true,
                video: false,
            })
        })
    }

    /// The people on a group call, named and pictured by the call's own
    /// account. Nobody while the lock hides the chat: a member's name says as
    /// much about the chat as its own does.
    pub fn call_people(&mut self) -> Vec<CallPerson> {
        let Some(group) = self.call.as_ref().and_then(|call| call.group.clone()) else {
            return Vec::new();
        };
        if self.call_is_private() {
            return Vec::new();
        }
        self.with_call_account(|app| {
            group
                .participants
                .into_iter()
                .map(|person| CallPerson {
                    name: app.display_name(&person.id),
                    picture: app.avatar(&person.id),
                    joined: person.joined,
                    video: person.video,
                    id: person.id,
                })
                .collect()
        })
    }

    /// The picture the call shows for the other side: none while the chat is
    /// locked, because a photo is the same disclosure the name is.
    pub fn call_avatar(&mut self) -> Option<std::path::PathBuf> {
        let call = self.call.as_ref()?;
        if call.chat.is_empty() {
            return None;
        }
        let chat = call.chat.clone();
        if self.call_is_private() {
            return None;
        }
        self.with_call_account(|app| app.avatar(&chat))
    }

    /// The backend of the account the call is on.
    fn call_backend(&self) -> Option<&Backend> {
        let account = self.call_account.as_ref()?;
        self.accounts
            .iter()
            .find(|owner| &owner.id == account)
            .map(|owner| &owner.backend)
    }

    /// Sends a command to the call's own account, whichever is on screen.
    pub(super) fn send_call_command(&self, command: Command) {
        if let Some(backend) = self.call_backend() {
            backend.send(command);
        }
    }

    /// Takes the window in or out of full screen for the call surface. Only
    /// the window moves; the call is not touched.
    fn set_call_fullscreen(&mut self, ctx: &egui::Context, fullscreen: bool) {
        if self.call_fullscreen != fullscreen {
            self.call_fullscreen = fullscreen;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(fullscreen));
        }
    }

    /// Puts a live call back behind the bar when its chat is locked and the
    /// folder closes: otherwise a call opened while the folder was open would
    /// keep painting the whole window after it closed.
    pub(super) fn hide_private_call(&mut self) {
        if self.call_is_private() {
            self.call_surface_hidden = true;
            // No picture lingers for the strip to lift.
            self.drop_call_frames();
        }
    }

    /// Forgets a video call's pictures, so nothing frozen is drawn as live.
    fn drop_call_frames(&mut self) {
        self.call_local_frame = None;
        self.call_remote_frame = None;
        self.call_local_texture = None;
        self.call_remote_texture = None;
        self.call_member_frames.clear();
        self.call_member_textures.clear();
    }

    /// A video call's newest pictures, from the account the call is on.
    pub(super) fn handle_call_video(
        &mut self,
        local: Option<std::sync::Arc<egui::ColorImage>>,
        remote: Option<std::sync::Arc<egui::ColorImage>>,
        from: Option<String>,
    ) {
        if self.call_account.as_ref() != Some(&self.account().id) || !self.call_live() {
            return;
        }
        if let Some(image) = local {
            self.call_local_frame = Some(image);
        }
        match (remote, from) {
            (Some(image), Some(member)) => {
                self.call_member_frames.insert(member, image);
            }
            (Some(image), None) => self.call_remote_frame = Some(image),
            (None, _) => {}
        }
    }

    /// Applies one call state from the backend of the account being handled.
    ///
    /// A finished call keeps its surface for a moment so the outcome can be
    /// read, then goes; the backend has already released the media.
    pub(super) fn handle_call_update(&mut self, update: CallUpdate) {
        let account = self.account().id.clone();
        let other_account = self
            .call_account
            .as_ref()
            .is_some_and(|current| current != &account);
        if other_account && self.call_live() {
            // One call at a time: an offer to another account while a call is
            // up is declined there, and its later updates are not this call's.
            if update.phase == CallPhase::Incoming {
                self.backend.send(Command::DeclineCall);
            }
            return;
        }
        // The call the reader hung up or declined here already left the
        // screen; whatever the worker still says about it is not drawn.
        if self.call_dismissed == Some(update.generation) {
            return;
        }
        // An older call's update is stale, except that an incoming call takes
        // the place of one that is only being placed: the worker refuses the
        // placing one as busy.
        if !other_account
            && self.call.as_ref().is_some_and(|current| {
                current.generation > update.generation && current.phase != CallPhase::Placing
            })
        {
            return;
        }
        // The end of a call other than the one on screen, such as one placed
        // here and refused because a call to us came first, is not drawn over
        // the live one.
        if !other_account
            && !update.phase.is_live()
            && self.call.as_ref().is_some_and(|current| {
                current.generation != update.generation && current.phase.is_live()
            })
        {
            return;
        }
        if update.phase != CallPhase::Placing {
            self.call_placed = None;
        }
        // A call we announced that no longer rings was picked up or given up:
        // its notification goes, and only its own.
        if update.phase != CallPhase::Incoming && self.call_notified == Some(update.generation) {
            self.call_notified = None;
            self.notifications.clear_call(&account, &update.chat);
        }
        let ringing = update.phase == CallPhase::Incoming
            && self
                .call
                .as_ref()
                .is_none_or(|current| current.generation != update.generation);
        let finished = !update.phase.is_live();
        let incoming = update.direction == CallDirection::Incoming;
        let was_live = self.call_live() && !other_account;
        // A peer who stopped sending video leaves no picture: the last frame
        // is not a live one, so the surface goes back to their photo.
        if !update.remote_video || other_account {
            self.call_remote_frame = None;
        }
        if !update.camera_on || other_account {
            self.call_local_frame = None;
        }
        // Likewise a member whose video stopped, or who left: their tile goes back to their photo.
        let sending: Vec<&str> = update
            .group
            .iter()
            .flat_map(|group| &group.participants)
            .filter(|person| person.video && !other_account)
            .map(|person| person.id.as_str())
            .collect();
        self.call_member_frames
            .retain(|member, _| sending.contains(&member.as_str()));
        self.call_member_textures
            .retain(|member, _| sending.contains(&member.as_str()));
        let failed_before = self
            .call
            .as_ref()
            .filter(|current| current.generation == update.generation)
            .and_then(|current| current.recording.as_ref())
            .and_then(|recording| recording.failed.clone());
        self.call = Some(update);
        self.call_account = Some(account);
        self.follow_recording(failed_before);
        if finished {
            if !was_live && self.call_surface_until.is_none() {
                // Nothing was drawn for this call, so there is nothing to
                // take down or say farewell to.
                self.call = None;
                self.call_account = None;
                return;
            }
            self.call_surface_until = Some(Instant::now() + CALL_FAREWELL);
            self.drop_call_frames();
            // How a call ended is the reason the surface lingers, but a locked
            // chat still says nothing: its farewell stays behind the bar.
            self.call_surface_hidden = self.call_is_private();
        } else {
            self.call_surface_until = None;
            // A call that has just begun takes the screen, unless its chat is
            // locked: then it waits in the bar under the folder's name.
            if !was_live {
                self.call_surface_hidden = self.call_is_private();
            }
        }
        if finished || incoming {
            self.call_cancel = None;
        }
        self.request_call_repaint();
        if ringing {
            self.notify_incoming_call();
        }
        self.sync_call_tone();
    }

    /// Places a call from the chat header or the Calls page: the screen shows
    /// it as placing in this same frame, with the peer's name and picture,
    /// and the worker's updates move it on once its offer is out.
    fn place_call(&mut self, chat: ChatId, video: bool) {
        let generation = crate::calls::next_generation();
        let cancel = Arc::new(AtomicBool::new(false));
        self.backend.send(Command::StartCall {
            chat: chat.clone(),
            video,
            generation,
            cancel: Arc::clone(&cancel),
        });
        self.handle_call_update(CallUpdate::placing(generation, chat.to_string(), video));
        self.call_cancel = Some(cancel);
        self.call_placed = Some(Instant::now());
    }

    /// Joins a call link from a message: drawn as connecting in this same
    /// frame, as a placed call is, while the worker asks to be let in.
    pub(super) fn join_call_link(&mut self, token: String, video: bool) {
        if self.call_live() {
            self.toast_error(crate::i18n::gettext(
                self.locale,
                "Hang up the current call first",
            ));
            return;
        }
        let generation = crate::calls::next_generation();
        let cancel = Arc::new(AtomicBool::new(false));
        self.backend.send(Command::JoinCallLink {
            token,
            video,
            generation,
            cancel: Arc::clone(&cancel),
        });
        let mut placing = CallUpdate::placing(generation, String::new(), video);
        placing.group = Some(crate::calls::GroupCall {
            link: true,
            ..crate::calls::GroupCall::default()
        });
        self.handle_call_update(placing);
        self.call_cancel = Some(cancel);
        self.call_placed = Some(Instant::now());
    }

    /// Takes down the call the reader hung up or declined here at once: they
    /// know how it ended, so there is no farewell to wait through. Its
    /// notification goes with it, and the worker's last updates about it are
    /// not drawn again.
    fn dismiss_call(&mut self) {
        if let Some(cancel) = self.call_cancel.take() {
            // A call still being placed is not offered at all.
            cancel.store(true, Ordering::Relaxed);
        }
        let Some(call) = self.call.take() else {
            return;
        };
        self.call_dismissed = Some(call.generation);
        // A call hung up before it was up is never recorded.
        self.call_record_armed = None;
        self.call_record_intro = None;
        if self.call_notified == Some(call.generation) {
            self.call_notified = None;
            if let Some(account) = self.call_account.clone() {
                self.notifications.clear_call(&account, &call.chat);
            }
        }
        self.call_account = None;
        self.call_surface_until = None;
        self.call_placed = None;
        self.call_rings = None;
        self.drop_call_frames();
        self.sync_call_tone();
        self.request_call_repaint();
    }

    /// Plays the tone the call's phase calls for, and stops the one it no
    /// longer does: the ringback only while the peer's phone rings, the
    /// ringtone only while a call to us waits and its notification may sound.
    fn sync_call_tone(&mut self) {
        let tone = match &self.call {
            Some(call)
                if call.phase == CallPhase::Ringing
                    && call.direction == CallDirection::Outgoing =>
            {
                Some(Tone::Ringback)
            }
            Some(call)
                if call.phase == CallPhase::Incoming
                    && self.call_rings == Some(call.generation) =>
            {
                Some(Tone::Ringtone)
            }
            _ => None,
        };
        self.call_tones.set(tone);
    }

    /// The tone playing for the call now, if any.
    pub fn call_tone(&self) -> Option<Tone> {
        self.call_tones.current()
    }

    fn request_call_repaint(&self) {
        self.waker.wake();
    }

    /// Tells the desktop a call is waiting: the window may be in the tray or
    /// behind another program. The click brings the window up, where the
    /// call surface waits for Accept or Decline.
    fn notify_incoming_call(&mut self) {
        let Some(call) = self.call.clone() else {
            return;
        };
        if !self.account().settings.notifications {
            return;
        }
        let now = crate::util::now();
        // The chat's own rules: a call in a chat that is muted, archived or
        // locked stays as quiet as a message in it. A caller without a chat
        // row is a stranger, and rings with the ordinary sound.
        let chat_sound = match self.chat(&call.chat) {
            Some(chat) if !call_notification_eligible(chat, now) => return,
            Some(chat) => chat.notification_sound.clone(),
            None => None,
        };
        let (title, body, picture) = if self.app_lock.is_locked() {
            // Behind the app lock the desktop learns that a call came, and
            // nothing about who is calling.
            (
                "Whats Pro".to_owned(),
                crate::i18n::gettext(self.locale, "Incoming call").into_owned(),
                None,
            )
        } else {
            // A group call names its group, and in the body who is calling.
            let caller = self.call_caller_name();
            (
                self.call_name(),
                crate::ui::call::incoming_text(self.locale, &call, caller.as_deref()),
                self.call_avatar(),
            )
        };
        // A call rings with the ringtone whenever its notification would make
        // a sound at all, whichever sound that is; with sounds off it only
        // shows. The notification itself stays quiet so the two never play
        // over each other.
        let sound = notification_sound(&self.settings, chat_sound, false, false);
        if sound != NotificationSound::None {
            self.call_rings = Some(call.generation);
        }
        let waker = self.waker.clone();
        self.call_notified = Some(call.generation);
        self.wants_attention = true;
        self.notifications.show(
            title,
            body,
            picture,
            NotificationSound::None,
            crate::notify::NotificationTarget {
                account: self.account().id.clone(),
                chat: call.chat.clone(),
                message: None,
            },
            std::sync::Arc::clone(&self.notification_opens),
            move || waker.wake(),
            // A ringing call always notifies; the limit is for repeated messages.
            false,
        );
    }

    /// Per-frame upkeep: the farewell's end, a placing call that never heard
    /// back, full screen following the surface, the running timer, and the
    /// call's tone.
    pub(super) fn tick_call(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        if self.call_surface_until.is_some_and(|until| now >= until) {
            self.call_surface_until = None;
            self.call = None;
            self.call_account = None;
            self.drop_call_frames();
        }
        if let Some(placed) = self.call_placed
            && now.duration_since(placed) >= PLACING_TIMEOUT
            && let Some(call) = self
                .call
                .clone()
                .filter(|call| call.phase == CallPhase::Placing)
        {
            log::warn!("[CALL] the call was not placed in time");
            if let Some(cancel) = self.call_cancel.take() {
                cancel.store(true, Ordering::Relaxed);
            }
            self.handle_call_update(call.not_placed());
        }
        // Full screen belongs to the call surface while the reader is looking
        // at it: however the surface was put aside, the window comes back.
        if self.call_fullscreen && !self.call_surface_open() {
            self.set_call_fullscreen(ctx, false);
        }
        self.sync_call_tone();
        self.announce_recorded_calls(ctx);
        if let Some(until) = self.call_surface_until {
            // The farewell goes on time even with nothing else to draw.
            ctx.request_repaint_after(until.saturating_duration_since(now));
        }
        if self.call.as_ref().is_some_and(|call| call.phase.is_live()) {
            // The duration changes every second.
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    /// Applies the actions the call surface and the chat header push.
    /// Returns the action back when it is not a call action.
    pub(super) fn apply_call_action(
        &mut self,
        action: Action,
        ctx: &egui::Context,
    ) -> Option<Action> {
        match action {
            Action::StartCall(chat) | Action::StartVideoCall(chat) if self.call_live() => {
                let _ = chat;
                self.toast_error(crate::i18n::gettext(
                    self.locale,
                    "Hang up the current call first",
                ));
            }
            Action::StartCall(chat) => self.place_call(chat, false),
            Action::StartVideoCall(chat) => self.place_call(chat, true),
            Action::SetCallCamera(on) => self.send_call_command(Command::SetCallCamera(on)),
            Action::SetCallCameraDevice(device) => {
                self.send_call_command(Command::SetCallCameraDevice(device));
            }
            Action::AnswerCall => self.send_call_command(Command::AnswerCall),
            Action::DeclineCall => {
                self.send_call_command(Command::DeclineCall);
                self.dismiss_call();
            }
            Action::HangupCall => {
                self.send_call_command(Command::HangupCall);
                self.dismiss_call();
            }
            Action::SetCallMuted(muted) => self.send_call_command(Command::SetCallMuted(muted)),
            Action::SetCallMicrophone(device) => {
                self.send_call_command(Command::SetCallMicrophone(device));
            }
            Action::SetCallSpeaker(device) => {
                self.send_call_command(Command::SetCallSpeaker(device));
            }
            Action::LeaveCallSurface => self.call_surface_hidden = true,
            // Returning to a locked chat's call cannot lift the lock on its
            // own: it asks for the code instead.
            Action::ReturnToCall if self.call.is_some() => {
                if !self.call_is_private() {
                    self.call_surface_hidden = false;
                } else if self.call_account.as_ref() == Some(&self.account().id) {
                    self.apply(Action::OpenLockedFolder, ctx);
                }
            }
            Action::ReturnToCall => {}
            Action::ToggleCallFullscreen => {
                if self.call.is_some() {
                    let fullscreen = !self.call_fullscreen;
                    self.set_call_fullscreen(ctx, fullscreen);
                }
            }
            Action::ToggleCallRecording => self.toggle_call_recording(),
            Action::AnswerAndRecord => {
                if self.settings.call_record_intro_done {
                    self.answer_and_record();
                } else {
                    self.call_record_intro = Some(PendingRecord::AnswerAndRecord);
                }
            }
            Action::ConfirmCallRecording { notice } => {
                self.settings.call_record_notice = notice;
                self.settings.call_record_intro_done = true;
                self.mark_settings_dirty();
                match self.call_record_intro.take() {
                    Some(PendingRecord::Toggle) => self.toggle_call_recording(),
                    Some(PendingRecord::AnswerAndRecord) => self.answer_and_record(),
                    None => {}
                }
            }
            Action::CancelCallRecordingIntro => self.call_record_intro = None,
            Action::DiscardCallRecording => self.discard_call_recording(),
            Action::DraftRecordingNotice => self.draft_recording_notice(ctx),
            Action::OpenRecordedCall(id) => {
                // A locked chat's call waits for its folder, which asks to
                // be opened.
                let private = crate::call_library::Library::cached(&self.dirs.state)
                    .get(&id)
                    .is_some_and(|call| call.private);
                if private && !self.locked_folder_open() {
                    self.apply(Action::OpenLockedFolder, ctx);
                }
                self.recorded_call_open = Some(id);
            }
            Action::CloseRecordedCall => {
                // Its player stops with the sheet.
                if let Some(id) = self.recorded_call_open.take()
                    && self
                        .player
                        .status(&crate::ui::call_review::player_key(&id))
                        .state
                        != crate::audio::State::Idle
                {
                    self.player.stop();
                }
            }
            Action::TranscribeRecordedCall(id) => crate::call_capture::save_and_read(self, &id),
            Action::MoveRecordedCall { id, project } => {
                crate::call_capture::move_to(self, &id, project);
            }
            Action::DeleteRecordedCall(id) => {
                crate::call_capture::delete(self, &id);
                if self.recorded_call_open.as_deref() == Some(id.as_str()) {
                    self.recorded_call_open = None;
                }
            }
            Action::RenameRecordedCall { id, title } => {
                let title = title.trim().to_owned();
                if !title.is_empty()
                    && let Err(error) =
                        crate::call_library::Library::update(&self.dirs.state, &id, |call| {
                            call.title = title
                        })
                {
                    log::warn!("call capture: the call could not be renamed: {error}");
                }
            }
            Action::DraftCallRecap(id) => self.draft_call_recap(&id, ctx),
            other => return Some(other),
        }
        None
    }

    /// The record button: stops a recording in progress, disarms an armed
    /// one, shows the first-time sheet, or starts (or arms, while the call is
    /// not up yet) a recording.
    fn toggle_call_recording(&mut self) {
        let Some(call) = self.call.clone().filter(|call| call.phase.is_live()) else {
            return;
        };
        let generation = call.generation;
        if call.recording.is_some() {
            self.send_call_command(Command::CallRecording {
                generation,
                op: RecordingOp::Stop,
            });
        } else if self.call_record_armed == Some(generation) {
            self.call_record_armed = None;
        } else if self.call_record_pending(generation) {
            // Already asked for; the worker's answer is on its way.
        } else if !self.settings.call_record_intro_done {
            self.call_record_intro = Some(PendingRecord::Toggle);
        } else if call.phase == CallPhase::Active {
            self.start_recording();
        } else {
            self.call_record_armed = Some(generation);
        }
    }

    /// Answers the ringing call armed to record from its first moment.
    fn answer_and_record(&mut self) {
        let Some(call) = self
            .call
            .as_ref()
            .filter(|call| call.phase == CallPhase::Incoming)
        else {
            return;
        };
        // Commands run in order: the arm is in place before the call is up.
        self.call_record_armed = Some(call.generation);
        self.send_call_command(Command::AnswerCall);
    }

    /// Asks the call's worker to record the call that is up, with its context
    /// written beside the file first.
    fn start_recording(&mut self) {
        let Some(call) = self.call.clone() else {
            return;
        };
        self.call_record_armed = None;
        let prepared = crate::call_capture::prepare(self)
            .and_then(|(path, meta)| meta.write(&path).map(|()| path));
        let path = match prepared {
            Ok(path) => path,
            Err(reason) => {
                self.toast_error(
                    crate::i18n::gettext(self.locale, "Could not start recording: {reason}")
                        .replace(
                            "{reason}",
                            &crate::call_capture::reason(self.locale, &reason),
                        ),
                );
                return;
            }
        };
        self.call_record_roster = call
            .group
            .iter()
            .flat_map(|group| &group.participants)
            .filter(|person| person.joined)
            .map(|person| person.id.clone())
            .collect();
        self.send_call_command(Command::CallRecording {
            generation: call.generation,
            op: RecordingOp::Start {
                path: path.clone(),
                notice: self.settings.call_record_notice,
            },
        });
        self.call_record_requested = Some((call.generation, Instant::now()));
        self.call_record_file = Some((call.generation, path));
        self.call_record_discarded = None;
    }

    /// Keeps the recording in step with each update of the call, window or
    /// not: an armed call starts recording once it is up, and an arm goes
    /// with a call that ended first.
    fn follow_recording(&mut self, failed_before: Option<String>) {
        let Some(call) = self.call.clone() else {
            return;
        };
        let generation = call.generation;
        if self
            .call_record_armed
            .is_some_and(|armed| armed != generation)
        {
            self.call_record_armed = None;
        }
        if !call.phase.is_live() {
            self.call_record_armed = None;
            self.call_record_intro = None;
            if self
                .call_record_requested
                .is_some_and(|(requested, _)| requested == generation)
            {
                self.call_record_requested = None;
            }
            return;
        }
        if call.phase == CallPhase::Active && self.call_record_armed == Some(generation) {
            self.start_recording();
        }
        let Some(recording) = &call.recording else {
            return;
        };
        if self
            .call_record_requested
            .is_some_and(|(requested, _)| requested == generation)
        {
            self.call_record_requested = None;
        }
        if let Some(reason) = &recording.failed
            && failed_before.as_ref() != Some(reason)
        {
            self.toast_error(
                crate::i18n::gettext(
                    self.locale,
                    "The recording stopped: {reason}. What was recorded so far is kept.",
                )
                .replace("{reason}", reason),
            );
        }
        // Who joined a group call while it was recorded.
        let joined: Vec<String> = call
            .group
            .iter()
            .flat_map(|group| &group.participants)
            .filter(|person| person.joined && !self.call_record_roster.contains(&person.id))
            .map(|person| person.id.clone())
            .collect();
        if joined.is_empty() {
            return;
        }
        self.call_record_roster.extend(joined.iter().cloned());
        let names: Vec<String> = self
            .call_people()
            .into_iter()
            .filter(|person| joined.contains(&person.id))
            .map(|person| person.name)
            .collect();
        if let Some((_, path)) = self
            .call_record_file
            .as_ref()
            .filter(|(recorded, _)| *recorded == generation)
        {
            crate::call_capture::add_participants(path, names);
        }
    }

    /// Stops the recording in progress and deletes it with its context.
    fn discard_call_recording(&mut self) {
        let Some(generation) = self
            .call
            .as_ref()
            .filter(|call| call.phase.is_live())
            .map(|call| call.generation)
        else {
            return;
        };
        self.send_call_command(Command::CallRecording {
            generation,
            op: RecordingOp::Discard,
        });
        self.call_record_requested = None;
        // A call that ends before the worker reads this hands the recording
        // on anyway; it is deleted then.
        self.call_record_discarded = Some(generation);
        if let Some((_, path)) = self
            .call_record_file
            .take_if(|(recorded, _)| *recorded == generation)
        {
            crate::call_capture::forget(&path);
        }
    }

    /// Puts a note that the call is being recorded in its chat's composer, as
    /// a draft: it is never sent on its own. Only for the account on screen,
    /// whose composer it is.
    fn draft_recording_notice(&mut self, ctx: &egui::Context) {
        let Some(call) = self.call.clone() else {
            return;
        };
        if self.call_account.as_ref() != Some(&self.account().id) || call.chat.is_empty() {
            self.toast(crate::i18n::gettext(
                self.locale,
                "Open the call's chat to send the notice",
            ));
            return;
        }
        let text = crate::i18n::gettext(
            self.locale,
            "Just so you know, I'm recording this call to keep notes of what we agree on. \
             If you'd rather I didn't, tell me and I'll stop.",
        )
        .into_owned();
        self.call_surface_hidden = true;
        self.apply(Action::OpenChat(call.chat.clone()), ctx);
        self.apply(
            Action::PrefillComposer {
                chat: call.chat,
                text,
            },
            ctx,
        );
    }

    /// Puts the recap of a read call in its chat's composer, as a draft: it
    /// is never sent on its own. Only for the account on screen, whose
    /// composer it is. A chat that already has a draft keeps it, and the
    /// recap goes to the clipboard instead.
    fn draft_call_recap(&mut self, id: &str, ctx: &egui::Context) {
        let library = crate::call_library::Library::cached(&self.dirs.state);
        let Some(call) = library.get(id) else {
            return;
        };
        let (Some(chat), Some(analysis)) = (call.chat.clone(), call.analysis.as_ref()) else {
            return;
        };
        if call.account.as_deref() != Some(self.account().id.as_str()) {
            self.toast(crate::i18n::gettext(
                self.locale,
                "Switch to the account that had the call to send its summary",
            ));
            return;
        }
        let text = crate::ui::call_review::recap(self.locale, analysis);
        self.recorded_call_open = None;
        if !self.chat_view() {
            self.apply(Action::Open(crate::model::Page::Chats), ctx);
        }
        self.apply(Action::OpenChat(chat.clone()), ctx);
        self.apply(
            Action::PrefillComposer {
                chat: chat.clone(),
                text: text.clone(),
            },
            ctx,
        );
        if self.open_chat.as_deref() == Some(chat.as_str()) && self.composer != text {
            ctx.copy_text(text);
            self.toast(crate::i18n::gettext(
                self.locale,
                "The chat already had a draft: the summary was copied instead",
            ));
        }
    }

    /// A recording the worker handed on: kept in the call library for review.
    pub(super) fn call_recorded(&mut self, report: crate::call_tape::Report) {
        let generation = report.generation;
        if self
            .call_record_requested
            .is_some_and(|(requested, _)| requested == generation)
        {
            self.call_record_requested = None;
        }
        if self
            .call_record_file
            .take_if(|(recorded, _)| *recorded == generation)
            .is_some()
        {
            self.call_record_roster.clear();
        }
        if self
            .call_record_discarded
            .take_if(|discarded| *discarded == generation)
            .is_some()
        {
            let _ = std::fs::remove_file(&report.path);
            crate::call_capture::forget(&report.path);
            return;
        }
        crate::call_capture::kept(self, report);
    }

    /// The worker could not do what was asked of a recording. Only the call it
    /// was asked for is touched: a late answer about an earlier call leaves the
    /// current one alone.
    pub(super) fn call_recording_failed(&mut self, generation: u64, reason: &str) {
        if self
            .call_record_requested
            .is_some_and(|(requested, _)| requested == generation)
        {
            self.call_record_requested = None;
        }
        let Some((_, path)) = self
            .call_record_file
            .take_if(|(recorded, _)| *recorded == generation)
        else {
            log::info!("[CALL] a recording request for an earlier call failed: {reason}");
            return;
        };
        crate::call_capture::forget(&path);
        self.call_record_roster.clear();
        self.toast_error(
            crate::i18n::gettext(self.locale, "Could not record the call: {reason}").replace(
                "{reason}",
                &crate::call_capture::reason(self.locale, reason),
            ),
        );
    }

    /// Says what happened to recorded calls being read, and what the launch
    /// rescued.
    fn announce_recorded_calls(&mut self, ctx: &egui::Context) {
        let locale = self.locale;
        for id in crate::ui::projects::drain_recordings() {
            self.recorded_call_open = Some(id.clone());
            self.toast_with(
                crate::i18n::gettext(locale, "Recording ready to review"),
                crate::i18n::gettext(locale, "Review").into_owned(),
                Action::OpenRecordedCall(id),
            );
        }
        for finished in crate::call_capture::drain_finished() {
            if crate::call_library::Library::cached(&self.dirs.state)
                .get(&finished.id)
                .is_some_and(|call| call.lead.is_some() && call.transcript().is_some())
            {
                crate::eleva::push(ctx, &self.dirs.config);
            }
            match finished.outcome {
                Ok(_) => self.toast_with(
                    crate::i18n::gettext(locale, "Summary ready: {title}")
                        .replace("{title}", &finished.title),
                    crate::i18n::gettext(locale, "View").into_owned(),
                    Action::OpenRecordedCall(finished.id),
                ),
                Err(reason) => self.toast_error(
                    crate::i18n::gettext(locale, "Could not read the call: {reason}")
                        .replace("{reason}", &reason),
                ),
            }
        }
        if let Some(recovery) = crate::call_capture::drain_recovery()
            && let Some((message, action)) = crate::call_capture::recovery_toast(locale, &recovery)
        {
            let label = crate::i18n::gettext(locale, "Review").into_owned();
            self.toast_with(message, label, action);
        }
    }

    /// Whether the account being removed or unlinked has the call: its
    /// backend ends it, and the surface goes with it.
    pub(super) fn forget_call_of(&mut self, account: &AccountId) {
        if self.call_account.as_ref() == Some(account) {
            self.call = None;
            self.call_account = None;
            self.call_surface_until = None;
            self.call_rings = None;
            self.call_cancel = None;
            self.call_placed = None;
            self.drop_call_frames();
            self.sync_call_tone();
        }
    }
}

/// Whether a call in this chat may raise a notification: the quiet rules a
/// message follows, without the unread one, since a call often comes to a
/// chat with nothing unread.
pub(super) fn call_notification_eligible(chat: &Chat, now: i64) -> bool {
    !chat.archived && !chat.muted(now) && !chat.locked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::Account;
    use crate::backend::LinkStatus;
    use crate::calls::CallOutcome;
    use crate::model::{CallDirection, Dialog};
    use crate::paths::AppDirs;
    use crate::settings::Settings;

    const ADA: &str = "15550001111@s.whatsapp.net";

    /// A window with one account whose commands a test can read.
    fn app(directory: &std::path::Path) -> (App, tokio::sync::mpsc::UnboundedReceiver<Command>) {
        let mut app = App::headless(AppDirs::under(directory), Settings::default()).0;
        let (backend, commands) = Backend::recording();
        app.backend = backend;
        app.chats.push(Chat::new(ADA.into(), "Ada".into()));
        (app, commands)
    }

    /// Adds a second account, not on screen, with its own command queue.
    fn second_account(app: &mut App) -> tokio::sync::mpsc::UnboundedReceiver<Command> {
        let mut second = Account::detached(
            &app.dirs,
            AccountId::parse("2").unwrap(),
            crate::settings::AccountSettings::default(),
        )
        .unwrap()
        .0;
        let (backend, commands) = Backend::recording();
        second.backend = backend;
        second.link = LinkStatus::Connected;
        second
            .chats
            .push(Chat::new(ADA.into(), "Ada at work".into()));
        app.accounts.push(second);
        commands
    }

    fn sent(commands: &mut tokio::sync::mpsc::UnboundedReceiver<Command>) -> Vec<Command> {
        std::iter::from_fn(|| commands.try_recv().ok()).collect()
    }

    fn update(generation: u64, phase: CallPhase) -> CallUpdate {
        CallUpdate {
            generation,
            chat: ADA.to_owned(),
            direction: CallDirection::Incoming,
            video: false,
            phase,
            started: (phase == CallPhase::Active).then(Instant::now),
            muted: false,
            camera_on: false,
            remote_video: false,
            video_requested: false,
            outcome: None,
            peer_audio: None,
            lost_devices: Vec::new(),
            microphone: None,
            speaker: None,
            camera: None,
            group: None,
            recording: None,
        }
    }

    fn ended(generation: u64, outcome: CallOutcome) -> CallUpdate {
        let mut update = update(generation, CallPhase::Ended);
        update.outcome = Some(outcome);
        update
    }

    #[test]
    fn a_ringing_call_takes_the_screen_and_is_announced_once() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _) = app(directory.path());
        app.handle_call_update(update(1, CallPhase::Incoming));
        assert!(app.call_live());
        assert!(!app.call_surface_hidden, "a ringing call takes the screen");
        assert!(
            !app.call_surface_open(),
            "but it is answered, not stepped away from"
        );
        assert_eq!(app.notifications.shown.len(), 1, "the desktop is told");
        assert_eq!(app.notifications.shown[0].title, "Ada");
        // The same call again is not a second ring.
        app.handle_call_update(update(1, CallPhase::Incoming));
        assert_eq!(app.notifications.shown.len(), 1);
        // Answered, its notification is taken back and the call stays up.
        app.handle_call_update(update(1, CallPhase::Active));
        assert_eq!(app.call_notified, None);
        assert!(app.call_surface_open(), "the running call is the screen");
    }

    #[test]
    fn a_call_that_ends_says_how_then_goes() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _) = app(directory.path());
        let ctx = egui::Context::default();
        app.handle_call_update(update(1, CallPhase::Active));
        app.apply(Action::LeaveCallSurface, &ctx);
        app.handle_call_update(ended(1, CallOutcome::Answered));
        assert!(
            !app.call_surface_hidden,
            "a call that ended is never left behind the strip"
        );
        assert!(app.call.is_some(), "the farewell is on screen");
        assert!(!app.call_live_here(ADA), "but the header offers a new call");
        app.call_surface_until = Some(Instant::now() - Duration::from_secs(1));
        app.tick_call(&ctx);
        assert!(app.call.is_none(), "and then it goes");
        assert!(app.call_account.is_none());
    }

    #[test]
    fn a_finished_call_nobody_saw_is_not_drawn() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _) = app(directory.path());
        app.handle_call_update(ended(3, CallOutcome::Failed));
        assert!(app.call.is_none());
    }

    #[test]
    fn stepping_away_and_full_screen_leave_the_call_alone() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, mut commands) = app(directory.path());
        let ctx = egui::Context::default();
        app.handle_call_update(update(1, CallPhase::Active));
        app.apply(Action::LeaveCallSurface, &ctx);
        assert!(app.call_surface_hidden);
        assert!(!app.call_surface_open(), "the strip shows instead");
        app.apply(Action::ReturnToCall, &ctx);
        assert!(app.call_surface_open());
        app.apply(Action::ToggleCallFullscreen, &ctx);
        assert!(app.call_fullscreen);
        app.apply(Action::ToggleCallFullscreen, &ctx);
        assert!(!app.call_fullscreen);
        app.apply(Action::OpenChat(ADA.into()), &ctx);
        assert!(
            !sent(&mut commands)
                .iter()
                .any(|command| matches!(command, Command::HangupCall)),
            "none of that hangs up"
        );
        app.apply(Action::HangupCall, &ctx);
        assert!(
            sent(&mut commands)
                .iter()
                .any(|command| matches!(command, Command::HangupCall)),
            "the hang-up button does"
        );
    }

    #[test]
    fn a_call_that_ends_puts_the_window_back_out_of_full_screen() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _) = app(directory.path());
        let ctx = egui::Context::default();
        app.handle_call_update(update(1, CallPhase::Active));
        app.apply(Action::ToggleCallFullscreen, &ctx);
        app.handle_call_update(ended(1, CallOutcome::Answered));
        app.tick_call(&ctx);
        assert!(!app.call_fullscreen);
    }

    #[test]
    fn hiding_the_window_keeps_the_call_and_drops_full_screen() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, mut commands) = app(directory.path());
        app.handle_call_update(update(1, CallPhase::Active));
        app.call_fullscreen = true;
        app.window_gone();
        assert!(app.call_live(), "a hidden window is still a call");
        assert!(!app.call_fullscreen);
        assert!(sent(&mut commands).is_empty());
    }

    #[test]
    fn a_locked_chat_says_nothing_on_a_call_until_its_folder_opens() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _) = app(directory.path());
        let ctx = egui::Context::default();
        app.chats[0].locked = true;
        app.settings.set_chat_lock_code(Some("test-code"));
        app.handle_call_update(update(1, CallPhase::Incoming));
        assert!(
            app.call_surface_hidden,
            "the locked caller waits in the strip"
        );
        assert_eq!(app.call_name(), "Locked chat");
        assert!(app.call_avatar().is_none(), "no photo either");
        assert!(
            app.notifications.shown.is_empty(),
            "a locked chat's call is as quiet as its messages"
        );
        // Returning asks for the code instead of lifting the lock.
        app.apply(Action::ReturnToCall, &ctx);
        assert!(app.call_surface_hidden);
        assert_eq!(app.dialog, Some(Dialog::UnlockLockedChats));
        app.apply(Action::UnlockLockedFolder("test-code".into()), &ctx);
        assert_eq!(app.call_name(), "Ada");
        app.apply(Action::ReturnToCall, &ctx);
        assert!(!app.call_surface_hidden);
        // Closing the folder sends the call back behind the strip.
        app.close_locked_folder();
        assert!(app.call_surface_hidden);
    }

    /// A group member's picture stays on their tile while their video runs, and goes when it
    /// stops; it never stands in for a one-to-one peer's.
    #[test]
    fn a_members_picture_lasts_as_long_as_their_video() {
        use crate::calls::{GroupCall, Participant};
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _) = app(directory.path());
        let with = |video: bool| {
            let mut update = update(1, CallPhase::Active);
            update.video = true;
            update.group = Some(GroupCall {
                link: false,
                caller: None,
                participants: vec![Participant {
                    id: ADA.to_owned(),
                    joined: true,
                    video,
                }],
            });
            update
        };
        app.handle_call_update(with(false));
        let image = Arc::new(egui::ColorImage::filled([2, 2], egui::Color32::BLACK));
        app.handle_call_video(None, Some(image), Some(ADA.to_owned()));
        app.handle_call_update(with(true));
        assert!(app.call_member_frames.contains_key(ADA));
        assert!(app.call_remote_frame.is_none(), "not a one-to-one picture");
        assert!(app.call_people()[0].video);
        app.handle_call_update(with(false));
        assert!(
            app.call_member_frames.is_empty(),
            "a stopped camera leaves no frozen picture"
        );
    }

    /// A call link from a message is joined here rather than opened in the browser, and is
    /// drawn at once under the link's name, with no chat behind it.
    #[test]
    fn a_call_link_is_joined_here() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, mut commands) = app(directory.path());
        let ctx = egui::Context::default();
        app.apply(
            Action::OpenUrl("https://call.whatsapp.com/video/AbCdEf1234567890".to_owned()),
            &ctx,
        );
        let sent = sent(&mut commands);
        assert!(
            matches!(
                sent.as_slice(),
                [Command::JoinCallLink { token, video: true, .. }] if token == "AbCdEf1234567890"
            ),
            "{sent:?}"
        );
        let call = app.call.clone().expect("on screen at once");
        assert_eq!(call.phase, CallPhase::Placing);
        assert_eq!(app.call_name(), "Call link");
        assert!(app.call_avatar().is_none());
        // A second link while that call is up is not joined.
        app.apply(
            Action::OpenUrl("https://call.whatsapp.com/voice/AbCdEf1234567890".to_owned()),
            &ctx,
        );
        assert!(self::sent(&mut commands).is_empty());
    }

    #[test]
    fn a_call_in_a_muted_chat_is_not_announced() {
        let now = crate::util::now();
        let mut chat = Chat::new(ADA.into(), "Ada".into());
        assert!(call_notification_eligible(&chat, now));
        chat.muted_until = Some(now + 3600);
        assert!(!call_notification_eligible(&chat, now));
        chat.muted_until = None;
        chat.archived = true;
        assert!(!call_notification_eligible(&chat, now));
        chat.archived = false;
        chat.locked = true;
        assert!(!call_notification_eligible(&chat, now));
    }

    #[test]
    fn the_calls_actions_reach_the_account_it_is_on() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, mut first) = app(directory.path());
        let mut second = second_account(&mut app);
        let ctx = egui::Context::default();
        // The second account rings while the first is on screen.
        app.active = 1;
        app.handle_call_update(update(1, CallPhase::Incoming));
        app.active = 0;
        assert_eq!(app.call_account, AccountId::parse("2"));
        assert_eq!(app.call_name(), "Ada at work", "named by its own account");
        assert!(
            !app.call_live_here(ADA),
            "the account on screen has no call"
        );
        app.apply(Action::AnswerCall, &ctx);
        app.apply(Action::SetCallMuted(true), &ctx);
        let calls = |commands: Vec<Command>| -> Vec<Command> {
            commands
                .into_iter()
                .filter(|command| matches!(command, Command::AnswerCall | Command::SetCallMuted(_)))
                .collect()
        };
        assert!(
            calls(sent(&mut first)).is_empty(),
            "not the account on screen"
        );
        assert!(matches!(
            calls(sent(&mut second)).as_slice(),
            [Command::AnswerCall, Command::SetCallMuted(true)]
        ));
    }

    #[test]
    fn a_second_accounts_offer_is_declined_while_a_call_is_up() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, mut first) = app(directory.path());
        let mut second = second_account(&mut app);
        let ctx = egui::Context::default();
        app.handle_call_update(update(1, CallPhase::Active));
        app.active = 1;
        app.handle_call_update(update(2, CallPhase::Incoming));
        app.active = 0;
        assert!(matches!(
            sent(&mut second).as_slice(),
            [Command::DeclineCall]
        ));
        assert_eq!(app.call.as_ref().map(|call| call.generation), Some(1));
        assert_eq!(app.call_account, Some(AccountId::first()));
        // And no second call is started from the window either.
        app.apply(Action::StartCall(ADA.into()), &ctx);
        assert!(sent(&mut first).is_empty());
    }

    #[test]
    fn removing_the_calls_account_takes_its_surface_away() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _) = app(directory.path());
        let _second = second_account(&mut app);
        app.active = 1;
        app.handle_call_update(update(1, CallPhase::Active));
        app.active = 0;
        app.forget_call_of(&AccountId::parse("2").unwrap());
        assert!(app.call.is_none());
    }

    /// An update from the worker about a call placed here.
    fn outgoing(generation: u64, phase: CallPhase) -> CallUpdate {
        let mut update = update(generation, phase);
        update.direction = CallDirection::Outgoing;
        update
    }

    /// Presses the call button, and returns the generation and the cancel
    /// flag the worker was handed.
    fn press_call(
        app: &mut App,
        commands: &mut tokio::sync::mpsc::UnboundedReceiver<Command>,
        video: bool,
    ) -> (u64, Arc<AtomicBool>) {
        let ctx = egui::Context::default();
        let action = if video {
            Action::StartVideoCall(ADA.into())
        } else {
            Action::StartCall(ADA.into())
        };
        app.apply(action, &ctx);
        let sent = sent(commands);
        let [
            Command::StartCall {
                chat,
                video: offered,
                generation,
                cancel,
            },
        ] = sent.as_slice()
        else {
            panic!("one StartCall, got {sent:?}");
        };
        assert_eq!(chat.as_str(), ADA);
        assert_eq!(*offered, video);
        (*generation, Arc::clone(cancel))
    }

    #[test]
    fn a_call_shows_connecting_the_moment_its_button_is_pressed() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, mut commands) = app(directory.path());
        let ctx = egui::Context::default();
        let (generation, _) = press_call(&mut app, &mut commands, true);
        // In the same apply, before the worker has said anything.
        let call = app.call.clone().expect("the call is on screen at once");
        assert_eq!(call.phase, CallPhase::Placing);
        assert_eq!(call.generation, generation);
        assert!(call.video);
        assert!(app.call_surface_open());
        assert!(app.call_live_here(ADA), "the header offers to hang up");
        assert_eq!(app.call_name(), "Ada");
        assert_eq!(
            crate::ui::call::status(app.locale, &call),
            "Connecting…".to_owned()
        );
        assert_eq!(app.call_tone(), None, "nothing rings yet");
        // A second press while it is placing does not place a second call.
        app.apply(Action::StartCall(ADA.into()), &ctx);
        assert!(sent(&mut commands).is_empty());

        // The protocol moves it on: offer out, the peer's phone rings, answered.
        app.handle_call_update(outgoing(generation, CallPhase::Dialing));
        assert_eq!(app.call.as_ref().unwrap().phase, CallPhase::Dialing);
        assert_eq!(app.call_tone(), None, "no device has said it rings");
        app.handle_call_update(outgoing(generation, CallPhase::Ringing));
        assert_eq!(app.call_tone(), Some(Tone::Ringback));
        app.tick_call(&ctx);
        assert_eq!(app.call_tone(), Some(Tone::Ringback), "it keeps ringing");
        app.handle_call_update(outgoing(generation, CallPhase::Connecting));
        assert_eq!(app.call_tone(), None, "the answer stops the ringback");
        app.handle_call_update(outgoing(generation, CallPhase::Active));
        assert!(app.call_surface_open());

        // The peer hangs up: "Call ended" for about a second and a half.
        let mut over = outgoing(generation, CallPhase::Ended);
        over.outcome = Some(CallOutcome::Answered);
        let before = Instant::now();
        app.handle_call_update(over);
        let until = app.call_surface_until.expect("a farewell");
        let lingers = until.duration_since(before);
        assert!(
            lingers >= CALL_FAREWELL && lingers < Duration::from_millis(1600),
            "{lingers:?}"
        );
        app.tick_call(&ctx);
        assert!(app.call.is_some(), "still saying how it ended");
        app.call_surface_until = Some(Instant::now() - Duration::from_millis(1));
        app.tick_call(&ctx);
        assert!(app.call.is_none(), "and then it goes");
    }

    #[test]
    fn the_peer_declining_stops_the_ringback_and_says_so_briefly() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, mut commands) = app(directory.path());
        let (generation, _) = press_call(&mut app, &mut commands, false);
        app.handle_call_update(outgoing(generation, CallPhase::Ringing));
        assert_eq!(app.call_tone(), Some(Tone::Ringback));
        let mut declined = outgoing(generation, CallPhase::Failed);
        declined.outcome = Some(CallOutcome::Declined);
        app.handle_call_update(declined);
        assert_eq!(app.call_tone(), None);
        assert_eq!(
            crate::ui::call::status(app.locale, app.call.as_ref().unwrap()),
            "Declined".to_owned()
        );
        assert!(app.call_surface_until.is_some(), "shown briefly, then gone");
    }

    #[test]
    fn hanging_up_while_connecting_never_rings_the_peer() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, mut commands) = app(directory.path());
        let ctx = egui::Context::default();
        let (generation, cancel) = press_call(&mut app, &mut commands, false);
        app.apply(Action::HangupCall, &ctx);
        assert!(
            cancel.load(Ordering::Relaxed),
            "the worker is told not to offer it"
        );
        assert!(
            sent(&mut commands)
                .iter()
                .any(|command| matches!(command, Command::HangupCall)),
            "and to end it if the offer already went"
        );
        assert!(app.call.is_none(), "the screen goes at once");
        assert!(!app.call_live());
        // Whatever the worker still reports about it is not drawn again.
        app.handle_call_update(outgoing(generation, CallPhase::Dialing));
        assert!(app.call.is_none());
        app.handle_call_update(outgoing(generation, CallPhase::Placing).not_placed());
        assert!(app.call.is_none());
        assert_eq!(app.call_tone(), None);
        // And a new call can be placed straight away.
        let (next, _) = press_call(&mut app, &mut commands, false);
        assert_ne!(next, generation);
        assert_eq!(app.call.as_ref().unwrap().phase, CallPhase::Placing);
    }

    #[test]
    fn hanging_up_a_ringing_call_ends_it_at_once() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, mut commands) = app(directory.path());
        let ctx = egui::Context::default();
        let (generation, _) = press_call(&mut app, &mut commands, false);
        app.handle_call_update(outgoing(generation, CallPhase::Ringing));
        app.apply(Action::HangupCall, &ctx);
        assert!(app.call.is_none(), "no farewell for our own hang-up");
        assert_eq!(app.call_tone(), None, "the ringback stops");
        let mut ended = outgoing(generation, CallPhase::Ended);
        ended.outcome = Some(CallOutcome::NoAnswer);
        app.handle_call_update(ended);
        assert!(app.call.is_none());
    }

    #[test]
    fn a_call_that_cannot_be_placed_says_so_and_goes() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, mut commands) = app(directory.path());
        let ctx = egui::Context::default();
        let (generation, _) = press_call(&mut app, &mut commands, false);
        app.handle_call_update(outgoing(generation, CallPhase::Placing).not_placed());
        let call = app.call.clone().unwrap();
        assert_eq!(call.phase, CallPhase::Failed);
        assert_eq!(
            crate::ui::call::status(app.locale, &call),
            "Could not connect".to_owned()
        );
        assert!(!app.call_live(), "nothing is left to hang up");
        assert!(app.call_surface_until.is_some());
        app.call_surface_until = Some(Instant::now() - Duration::from_millis(1));
        app.tick_call(&ctx);
        assert!(app.call.is_none());
    }

    #[test]
    fn a_call_the_worker_never_answers_for_does_not_stay_connecting() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, mut commands) = app(directory.path());
        let ctx = egui::Context::default();
        let (_, cancel) = press_call(&mut app, &mut commands, false);
        app.call_placed = Some(Instant::now() - PLACING_TIMEOUT);
        app.tick_call(&ctx);
        assert_eq!(app.call.as_ref().unwrap().phase, CallPhase::Failed);
        assert!(cancel.load(Ordering::Relaxed), "and it is not placed late");
    }

    #[test]
    fn a_call_to_us_takes_the_place_of_one_still_being_placed() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, mut commands) = app(directory.path());
        let (generation, _) = press_call(&mut app, &mut commands, false);
        // The offer reached the worker first, under an older generation (no
        // call is ever given generation 0).
        app.handle_call_update(update(0, CallPhase::Incoming));
        assert_eq!(app.call.as_ref().unwrap().phase, CallPhase::Incoming);
        // The worker refuses the placing call as busy; that is not drawn.
        app.handle_call_update(outgoing(generation, CallPhase::Placing).not_placed());
        assert_eq!(app.call.as_ref().unwrap().phase, CallPhase::Incoming);
    }

    #[test]
    fn a_call_to_us_rings_until_it_is_answered() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _) = app(directory.path());
        app.handle_call_update(update(1, CallPhase::Incoming));
        assert_eq!(app.call_tone(), Some(Tone::Ringtone));
        assert_eq!(
            app.notifications.shown[0].sound,
            NotificationSound::None,
            "the ringtone is the sound, not the notification"
        );
        app.handle_call_update(update(1, CallPhase::Accepted));
        assert_eq!(app.call_tone(), None);
    }

    #[test]
    fn declining_a_call_silences_it_and_takes_it_down_at_once() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, mut commands) = app(directory.path());
        let ctx = egui::Context::default();
        app.handle_call_update(update(1, CallPhase::Incoming));
        app.apply(Action::DeclineCall, &ctx);
        assert!(
            sent(&mut commands)
                .iter()
                .any(|command| matches!(command, Command::DeclineCall))
        );
        assert!(app.call.is_none());
        assert_eq!(app.call_tone(), None);
        assert_eq!(app.call_notified, None, "its notification goes too");
        app.handle_call_update(ended(1, CallOutcome::Declined));
        assert!(app.call.is_none(), "the worker's own end is not drawn");
    }

    #[test]
    fn a_call_the_caller_gives_up_on_stops_ringing_and_says_missed() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _) = app(directory.path());
        app.handle_call_update(update(1, CallPhase::Incoming));
        let mut missed = update(1, CallPhase::Failed);
        missed.outcome = Some(CallOutcome::Missed);
        app.handle_call_update(missed);
        assert_eq!(app.call_tone(), None);
        assert!(app.call_surface_until.is_some(), "missed, briefly");
    }

    #[test]
    fn with_notification_sounds_off_a_call_shows_without_ringing() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _) = app(directory.path());
        app.settings.message_sound = NotificationSound::None;
        app.handle_call_update(update(1, CallPhase::Incoming));
        assert_eq!(app.notifications.shown.len(), 1, "still announced");
        assert_eq!(app.call_tone(), None);

        let directory = tempfile::tempdir().unwrap();
        let (mut app, _) = self::app(directory.path());
        app.chats[0].muted_until = Some(crate::util::now() + 3600);
        app.handle_call_update(update(1, CallPhase::Incoming));
        assert_eq!(app.call_tone(), None, "a muted chat does not ring");
    }

    /// A call update that says it is being recorded.
    fn recorded(generation: u64) -> CallUpdate {
        let mut update = update(generation, CallPhase::Active);
        update.recording = Some(crate::call_tape::RecordingView {
            since: Instant::now(),
            notice: crate::call_tape::NoticeState::Pending,
            failed: None,
        });
        update
    }

    /// The recordings asked of the worker: generation, file and notice.
    fn starts(commands: Vec<Command>) -> Vec<(u64, std::path::PathBuf, bool)> {
        commands
            .into_iter()
            .filter_map(|command| match command {
                Command::CallRecording {
                    generation,
                    op: RecordingOp::Start { path, notice },
                } => Some((generation, path, notice)),
                _ => None,
            })
            .collect()
    }

    /// Hands the window an event from the call's account.
    fn deliver(app: &mut App, event: crate::backend::Event) {
        app.apply_backend_event(event, true);
    }

    fn intro_done(app: &mut App) {
        app.settings.call_record_intro_done = true;
    }

    #[test]
    fn an_armed_call_starts_recording_once_when_it_is_up() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, mut commands) = app(directory.path());
        let ctx = egui::Context::default();
        intro_done(&mut app);
        app.handle_call_update(update(4, CallPhase::Incoming));
        sent(&mut commands);
        app.apply(Action::AnswerAndRecord, &ctx);
        let sent_now = sent(&mut commands);
        assert!(
            matches!(sent_now.as_slice(), [Command::AnswerCall]),
            "{sent_now:?}"
        );
        assert_eq!(app.call_record_armed, Some(4));
        app.handle_call_update(update(4, CallPhase::Accepted));
        assert!(
            starts(sent(&mut commands)).is_empty(),
            "not before it is up"
        );
        app.handle_call_update(update(4, CallPhase::Active));
        let started = starts(sent(&mut commands));
        let [(4, path, true)] = started.as_slice() else {
            panic!("one start with the notice, got {started:?}");
        };
        assert!(
            path.starts_with(crate::call_capture::dir(&app.dirs.state)),
            "{path:?}"
        );
        let meta = crate::call_capture::Meta::read(path).expect("its context comes first");
        assert_eq!(meta.title, "Call with Ada");
        assert_eq!(meta.chat.as_deref(), Some(ADA));
        assert_eq!(meta.direction.as_deref(), Some("incoming"));
        assert!(!meta.private);
        // The next updates while the worker sets it up start nothing more.
        app.handle_call_update(update(4, CallPhase::Active));
        assert!(starts(sent(&mut commands)).is_empty());
        app.handle_call_update(recorded(4));
        assert_eq!(app.call_record_requested, None, "the recording showed up");
        assert!(starts(sent(&mut commands)).is_empty());
        // The button now stops it.
        app.apply(Action::ToggleCallRecording, &ctx);
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [Command::CallRecording {
                generation: 4,
                op: RecordingOp::Stop
            }]
        ));
    }

    #[test]
    fn hanging_up_before_the_call_is_up_drops_the_arm() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, mut commands) = app(directory.path());
        let ctx = egui::Context::default();
        intro_done(&mut app);
        let (generation, _) = press_call(&mut app, &mut commands, false);
        app.handle_call_update(outgoing(generation, CallPhase::Ringing));
        app.apply(Action::ToggleCallRecording, &ctx);
        assert_eq!(
            app.call_record_armed,
            Some(generation),
            "armed while it rings"
        );
        app.apply(Action::HangupCall, &ctx);
        assert_eq!(app.call_record_armed, None);
        sent(&mut commands);
        // An arm also goes with a call that ends on the other side.
        let (generation, _) = press_call(&mut app, &mut commands, false);
        app.handle_call_update(outgoing(generation, CallPhase::Ringing));
        app.apply(Action::ToggleCallRecording, &ctx);
        let mut over = outgoing(generation, CallPhase::Failed);
        over.outcome = Some(CallOutcome::Declined);
        app.handle_call_update(over);
        assert_eq!(app.call_record_armed, None);
        // Pressing again while armed disarms.
        let (generation, _) = press_call(&mut app, &mut commands, false);
        app.handle_call_update(outgoing(generation, CallPhase::Ringing));
        app.apply(Action::ToggleCallRecording, &ctx);
        app.apply(Action::ToggleCallRecording, &ctx);
        assert_eq!(app.call_record_armed, None);
    }

    #[test]
    fn a_late_failure_for_an_earlier_call_leaves_the_current_one_alone() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, mut commands) = app(directory.path());
        let ctx = egui::Context::default();
        intro_done(&mut app);
        app.handle_call_update(update(2, CallPhase::Active));
        app.apply(Action::ToggleCallRecording, &ctx);
        let started = starts(sent(&mut commands));
        let [(2, path, _)] = started.as_slice() else {
            panic!("{started:?}");
        };
        let context = crate::call_capture::sidecar(path);
        deliver(
            &mut app,
            crate::backend::Event::CallRecordingFailed {
                generation: 1,
                reason: "The call is over".into(),
            },
        );
        assert_eq!(app.call_record_requested.map(|(g, _)| g), Some(2));
        assert!(context.exists(), "the current recording's context stays");
        assert!(app.toasts.is_empty(), "{:?}", app.toasts);
        // Its own failure is said, and its context goes.
        deliver(
            &mut app,
            crate::backend::Event::CallRecordingFailed {
                generation: 2,
                reason: crate::call_tape::BUSY.into(),
            },
        );
        assert_eq!(app.call_record_requested, None);
        assert!(!context.exists());
        assert_eq!(
            app.toasts.last().map(|toast| toast.message.as_str()),
            Some("Could not record the call: Another call is already being recorded")
        );
    }

    #[test]
    fn the_first_recording_asks_once_and_then_never_again() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, mut commands) = app(directory.path());
        let ctx = egui::Context::default();
        app.handle_call_update(update(1, CallPhase::Active));
        app.apply(Action::ToggleCallRecording, &ctx);
        assert_eq!(app.call_record_intro, Some(PendingRecord::Toggle));
        assert!(
            starts(sent(&mut commands)).is_empty(),
            "not before agreeing"
        );
        app.apply(Action::ConfirmCallRecording { notice: false }, &ctx);
        assert_eq!(app.call_record_intro, None);
        assert!(app.settings.call_record_intro_done);
        assert!(!app.settings.call_record_notice);
        let started = starts(sent(&mut commands));
        assert!(matches!(started.as_slice(), [(1, _, false)]), "{started:?}");
        app.handle_call_update(recorded(1));
        app.apply(Action::ToggleCallRecording, &ctx);
        app.handle_call_update(update(1, CallPhase::Active));
        sent(&mut commands);
        // The next one starts at once.
        app.apply(Action::ToggleCallRecording, &ctx);
        assert_eq!(app.call_record_intro, None);
        assert_eq!(starts(sent(&mut commands)).len(), 1);
        // Closing the sheet records nothing.
        let directory = tempfile::tempdir().unwrap();
        let (mut app, mut commands) = self::app(directory.path());
        app.handle_call_update(update(1, CallPhase::Incoming));
        sent(&mut commands);
        app.apply(Action::AnswerAndRecord, &ctx);
        assert_eq!(app.call_record_intro, Some(PendingRecord::AnswerAndRecord));
        assert!(sent(&mut commands).is_empty(), "the call still rings");
        app.apply(Action::CancelCallRecordingIntro, &ctx);
        assert_eq!(app.call_record_intro, None);
        assert!(!app.settings.call_record_intro_done);
    }

    #[test]
    fn a_locked_chats_recording_is_titled_without_its_name() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, mut commands) = app(directory.path());
        let ctx = egui::Context::default();
        intro_done(&mut app);
        app.chats[0].locked = true;
        app.settings.set_chat_lock_code(Some("test-code"));
        // Even with its folder open: the files outlive the folder.
        app.apply(Action::UnlockLockedFolder("test-code".into()), &ctx);
        app.handle_call_update(update(1, CallPhase::Active));
        assert_eq!(app.call_name(), "Ada");
        app.apply(Action::ToggleCallRecording, &ctx);
        let started = starts(sent(&mut commands));
        let [(1, path, _)] = started.as_slice() else {
            panic!("{started:?}");
        };
        let meta = crate::call_capture::Meta::read(path).unwrap();
        assert_eq!(meta.title, "Call in a locked chat");
        assert!(meta.private);
        assert_eq!(meta.chat.as_deref(), Some(ADA), "kept for matching");
        let written = std::fs::read_to_string(crate::call_capture::sidecar(path)).unwrap();
        assert!(!written.contains("Ada\""), "{written}");
    }

    #[test]
    fn a_stopped_recording_is_kept_for_review() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, mut commands) = app(directory.path());
        let ctx = egui::Context::default();
        intro_done(&mut app);
        app.handle_call_update(update(3, CallPhase::Active));
        app.apply(Action::ToggleCallRecording, &ctx);
        let started = starts(sent(&mut commands));
        let [(3, path, _)] = started.as_slice() else {
            panic!("{started:?}");
        };
        std::fs::write(path, b"RIFF").unwrap();
        deliver(
            &mut app,
            crate::backend::Event::CallRecorded(Box::new(crate::call_tape::Report {
                generation: 3,
                path: path.clone(),
                seconds: 75,
                levels: [0.2, 0.1],
                notice_ms: Some(0),
                error: None,
            })),
        );
        assert!(!path.exists() && !crate::call_capture::sidecar(path).exists());
        let library = crate::call_library::Library::load(&app.dirs.state);
        let [call] = library.calls.as_slice() else {
            panic!("{library:?}");
        };
        assert_eq!(call.title, "Call with Ada");
        assert_eq!(call.chat.as_deref(), Some(ADA));
        assert_eq!(call.seconds, 75);
        assert_eq!(call.notice_ms, Some(0));
        assert!(call.stereo && call.pending_review && !call.pending_read);
        assert!(
            crate::call_capture::job(&call.id).is_none(),
            "nothing reads it before the review"
        );
        let toast = app.toasts.last().unwrap();
        assert_eq!(
            toast.message,
            "Call recorded (1:15). Find it in Calls > Recorded."
        );
        assert_eq!(
            toast.action,
            Some((
                "Review".to_owned(),
                Action::OpenRecordedCall(call.id.clone())
            ))
        );
        assert_eq!(toast.lifetime(), crate::model::ACTION_TOAST_LIFETIME);
    }

    #[test]
    fn a_discarded_recording_is_deleted_even_if_the_call_ends_first() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, mut commands) = app(directory.path());
        let ctx = egui::Context::default();
        intro_done(&mut app);
        app.handle_call_update(update(5, CallPhase::Active));
        app.apply(Action::ToggleCallRecording, &ctx);
        let started = starts(sent(&mut commands));
        let [(5, path, _)] = started.as_slice() else {
            panic!("{started:?}");
        };
        app.handle_call_update(recorded(5));
        app.apply(Action::DiscardCallRecording, &ctx);
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [Command::CallRecording {
                generation: 5,
                op: RecordingOp::Discard
            }]
        ));
        assert!(!crate::call_capture::sidecar(path).exists());
        std::fs::write(path, b"RIFF").unwrap();
        deliver(
            &mut app,
            crate::backend::Event::CallRecorded(Box::new(crate::call_tape::Report {
                generation: 5,
                path: path.clone(),
                seconds: 3,
                levels: [0.0; 2],
                notice_ms: None,
                error: None,
            })),
        );
        assert!(!path.exists());
        assert!(
            crate::call_library::Library::load(&app.dirs.state)
                .calls
                .is_empty()
        );
    }

    #[test]
    fn the_recording_notice_is_only_ever_a_draft() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, mut commands) = app(directory.path());
        let ctx = egui::Context::default();
        app.handle_call_update(update(1, CallPhase::Active));
        app.apply(Action::DraftRecordingNotice, &ctx);
        assert_eq!(app.open_chat.as_deref(), Some(ADA));
        assert!(
            app.composer.starts_with("Just so you know"),
            "{}",
            app.composer
        );
        assert!(app.call_surface_hidden, "the chat shows beside the call");
        assert!(
            !sent(&mut commands)
                .iter()
                .any(|command| matches!(command, Command::SendText { .. })),
            "nothing is sent"
        );
    }
}
