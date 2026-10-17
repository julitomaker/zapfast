//! The call surface: the screen of a live call, the prompt of one that
//! rings, and the strip a call the reader stepped away from keeps at the
//! bottom of the window.
//!
//! Nothing here decides what a call is doing. The phase, the point the
//! duration counts from, the mute state and the devices all come from
//! [`crate::calls::CallUpdate`], which the worker fills from the protocol's
//! own events. This module draws them and pushes [`Action`]s.

use std::time::Instant;

use egui::{Align, Color32, CornerRadius, Frame, Layout, Margin, Rect, TextureOptions, Vec2, vec2};

use crate::app::{App, CallPerson};
use crate::call_tape::{NoticeState, RecordingView};
use crate::calls::{CallOutcome, CallPhase, CallUpdate, PeerAudio, SilenceReason};
use crate::i18n::{Locale, gettext};
use crate::model::{Action, PendingRecord};
use crate::theme::{self, Icon, Palette};
use crate::ui::widgets;

/// The area's id, so the surface keeps its place in the layer order.
const SURFACE: &str = "zapfast-call-surface";
/// Padding between the window edge and everything the surface draws.
const PADDING: f32 = 24.0;
/// The round controls along the bottom.
const CONTROL: f32 = 56.0;
/// The gap between them.
const CONTROL_GAP: f32 = 16.0;
/// How much of the surface the corner preview of our own camera may take.
const PREVIEW_SHARE: f32 = 0.24;
/// The plate under words drawn over a video picture: dark in either theme,
/// because the picture is not themed.
const VIDEO_SCRIM: Color32 = Color32::from_black_alpha(150);

/// Draws the call surface over the window, when a call wants the screen.
/// A call the reader stepped away from is drawn by [`strip`] instead.
pub fn show(app: &mut App, ctx: &egui::Context) {
    let Some(call) = app.call.clone() else {
        return;
    };
    if app.call_surface_hidden {
        return;
    }
    let palette = app.palette;
    let peer = app.call_name();
    let picture = app.call_avatar();
    let people = app.call_people();
    let me = app.call_self();
    let caller = app.call_caller_name();
    // The whole window, minus the strip macOS keeps for the traffic lights.
    let screen = ctx.content_rect();
    let screen = screen.with_min_y(screen.top() + theme::titlebar_inset(ctx));
    egui::Area::new(egui::Id::new(SURFACE))
        .order(egui::Order::Foreground)
        .fixed_pos(screen.min)
        .show(ctx, |ui| {
            ui.set_min_size(screen.size());
            // A peer's picture is letterboxed in black, as any video is.
            let backdrop = if call.video && call.remote_video && app.call_remote_frame.is_some() {
                Color32::BLACK
            } else {
                palette.window
            };
            ui.painter().rect_filled(screen, 0.0, backdrop);
            // Clicks and scrolling stop here rather than reaching the chat
            // drawn underneath.
            ui.interact(
                screen,
                egui::Id::new(SURFACE).with("backdrop"),
                egui::Sense::click_and_drag(),
            );
            // Where Whats Pro draws the title bar, the screen's top drags the
            // window, beneath the corner's buttons.
            super::caption_drag(
                ui,
                Rect::from_min_size(screen.min, vec2(screen.width(), theme::TOP_BAR_HEIGHT)),
            );
            let inner = screen.shrink(PADDING);
            ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
                let picture = picture.as_deref();
                let who = Who {
                    peer: &peer,
                    picture,
                    caller: caller.as_deref(),
                    people: &people,
                    me: me.as_ref(),
                };
                if call.phase == CallPhase::Incoming {
                    ringing(ui, app, &call, &who, &palette, inner);
                } else if call.group.is_some() && !people.is_empty() {
                    group_live(ui, app, &call, &who, &palette, inner);
                } else {
                    live(ui, app, &call, &peer, picture, &palette, inner);
                }
            });
        });
    match app.call_record_intro {
        Some(pending) => record_intro(app, ctx, pending),
        None => ctx.data_mut(|data| data.remove::<Intro>(intro_id())),
    }
}

/// The strip a call keeps at the bottom of the window while the reader is
/// elsewhere: who is on it, how long it has been up, and the way back.
pub fn strip(app: &mut App, ui: &mut egui::Ui) {
    let Some(call) = app.call.clone() else {
        return;
    };
    if !app.call_surface_hidden {
        return;
    }
    let palette = app.palette;
    let peer = app.call_name();
    let status = status(app.locale, &call);
    egui::Panel::bottom("call-strip")
        .show_separator_line(false)
        .frame(
            Frame::new()
                .fill(palette.accent)
                .inner_margin(Margin::symmetric(14, 6)),
        )
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let icon = if call.video { Icon::Video } else { Icon::Phone };
                theme::icon(ui, icon, 15.0, palette.on_accent);
                theme::text(ui, peer, theme::semibold(13.0), palette.on_accent);
                theme::text(ui, status, theme::medium(13.0), palette.on_accent);
                if !call.phase.is_live() {
                    return;
                }
                // A recording keeps saying so away from the call screen.
                if let Some(recording) = &call.recording {
                    ui.add_space(6.0);
                    let (dot, _) = ui.allocate_exact_size(Vec2::splat(8.0), egui::Sense::hover());
                    ui.painter()
                        .circle_filled(dot.center(), 4.0, palette.on_accent);
                    theme::text(
                        ui,
                        rec_time(app.locale, recording),
                        theme::tabular(fastframe_fonts::Weight::SemiBold, 13.0),
                        palette.on_accent,
                    );
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(500));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let locale = app.locale;
                    let hang_up = gettext(locale, "Hang up").into_owned();
                    if theme::icon_button(
                        ui,
                        Icon::PhoneOff,
                        16.0,
                        palette.on_accent,
                        palette.on_accent,
                        &hang_up,
                    )
                    .clicked()
                    {
                        app.actions.push(Action::HangupCall);
                    }
                    if call.recording.is_some() {
                        let stop = gettext(locale, "Stop recording").into_owned();
                        if theme::icon_button(
                            ui,
                            Icon::Stop,
                            15.0,
                            palette.on_accent,
                            palette.on_accent,
                            &stop,
                        )
                        .clicked()
                        {
                            app.actions.push(Action::ToggleCallRecording);
                        }
                    }
                    let back = gettext(locale, "Return to the call").into_owned();
                    let response = ui.add(
                        egui::Button::new(
                            egui::RichText::new(&back)
                                .font(theme::semibold(13.0))
                                .color(palette.on_accent),
                        )
                        .frame(false),
                    );
                    if response
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked()
                    {
                        app.actions.push(Action::ReturnToCall);
                    }
                });
            });
        });
}

/// A call that is up, coming up, or just over.
fn live(
    ui: &mut egui::Ui,
    app: &mut App,
    call: &CallUpdate,
    peer: &str,
    picture: Option<&std::path::Path>,
    palette: &Palette,
    area: Rect,
) {
    let locale = app.locale;
    let screen = area.expand(PADDING);
    let record = record_lines(ui.ctx(), app, call, peer);
    // The peer's picture fills the window when there is one, and everything
    // else is drawn over it.
    let remote = (call.video && call.remote_video)
        .then(|| app.call_remote_frame.clone())
        .flatten();
    if let Some(image) = &remote {
        let at = fit_inside(screen, image_size(image));
        let texture = call_texture(
            ui,
            &mut app.call_remote_texture,
            "zapfast-call-remote",
            image,
        );
        egui::Image::new((texture, at.size())).paint_at(ui, at);
    }
    // Our own camera sits in the top corner at its own shape.
    own_preview(ui, app, call, palette, area);
    if remote.is_none() {
        ui.vertical_centered(|ui| {
            ui.add_space(area.height() * 0.16);
            let size = (area.height() * 0.26).clamp(96.0, 168.0);
            if call.group.as_ref().is_some_and(|group| group.link) {
                // A call link has no chat and no face: its own sign instead.
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
                paint_link_avatar(ui, palette, rect);
            } else {
                widgets::avatar(ui, palette, peer, &call.chat, size, picture);
            }
            ui.add_space(18.0);
            theme::text(ui, peer, theme::bold(26.0), palette.text);
            ui.add_space(6.0);
            theme::text(
                ui,
                status(locale, call),
                theme::medium(15.0),
                status_color(call, palette),
            );
            if call.video_requested {
                // The camera button below answers it.
                ui.add_space(4.0);
                theme::text(
                    ui,
                    gettext(
                        locale,
                        "Wants to switch to video: turn on your camera to answer",
                    ),
                    theme::medium(12.5),
                    palette.accent,
                );
            }
            if call.video && call.phase == CallPhase::Active && !call.remote_video {
                ui.add_space(4.0);
                theme::text(
                    ui,
                    gettext(locale, "Their camera is off"),
                    theme::medium(12.5),
                    palette.secondary,
                );
            }
            notes(ui, locale, call, palette, &record);
        });
    } else {
        // Over a picture, the name and the length sit on a plate of their own,
        // as wide as they are.
        let name =
            ui.painter()
                .layout_no_wrap(peer.to_owned(), theme::semibold(15.0), Color32::WHITE);
        let line =
            ui.painter()
                .layout_no_wrap(status(locale, call), theme::medium(13.0), Color32::WHITE);
        let inner = vec2(
            name.size().x.max(line.size().x),
            name.size().y + line.size().y,
        );
        let plate = Rect::from_center_size(
            egui::pos2(area.center().x, area.top() + 4.0 + inner.y / 2.0 + 6.0),
            inner + vec2(28.0, 12.0),
        );
        ui.painter()
            .rect_filled(plate, CornerRadius::same(14), VIDEO_SCRIM);
        let top = plate.top() + 6.0;
        ui.painter().galley(
            egui::pos2(plate.center().x - name.size().x / 2.0, top),
            name.clone(),
            Color32::WHITE,
        );
        ui.painter().galley(
            egui::pos2(plate.center().x - line.size().x / 2.0, top + name.size().y),
            line,
            Color32::WHITE,
        );
        ui.ctx()
            .data_mut(|data| data.insert_temp(plate_id(), plate));
        // A recording says so under the plate, where the corner's buttons
        // leave the picture's top to the name.
        let mut top = plate.bottom() + 6.0;
        if call.recording.is_some() {
            let center = egui::pos2(area.center().x, top + PILL_HEIGHT / 2.0);
            if let Some(pill) = rec_pill(ui, app, call, peer, center, area.width(), true) {
                top = pill.bottom() + 6.0;
            }
        }
        let below = Rect::from_min_max(
            egui::pos2(area.left(), top),
            egui::pos2(area.right(), top + 80.0),
        );
        ui.scope_builder(egui::UiBuilder::new().max_rect(below), |ui| {
            ui.vertical_centered(|ui| notes(ui, locale, call, palette, &record));
        });
    }

    let pill = remote.is_none().then_some(peer);
    corner_buttons(ui, app, call, area, remote.is_some(), pill);

    let controls_row = bottom_row(ui, area, CONTROL, |ui| controls(ui, app, call, palette));
    if app.call_devices_open && call.phase.is_live() {
        let above = area.with_max_y(controls_row.top() - 20.0);
        bottom_row(ui, above, PICKER_HEIGHT, |ui| {
            devices(ui, app, call, palette)
        });
    }
}

/// Stepping back to the chat and full screen, along the top: neither ends
/// the call. Over a picture they are white, as the plate's words are.
/// Between them goes the recording pill, naming the other side `pill`.
fn corner_buttons(
    ui: &mut egui::Ui,
    app: &mut App,
    call: &CallUpdate,
    area: Rect,
    over_picture: bool,
    pill: Option<&str>,
) {
    let locale = app.locale;
    let palette = app.palette;
    let (corner_tint, corner_hover) = if over_picture {
        (Color32::from_white_alpha(200), Color32::WHITE)
    } else {
        (palette.secondary, palette.text)
    };
    let corner = Rect::from_min_size(area.min, vec2(area.width(), 40.0));
    // Clear of the caption buttons Whats Pro draws over the window's corners.
    let room = super::caption_room(ui.ctx(), corner, 120.0);
    let corner = Rect::from_min_max(
        egui::pos2(corner.left() + room.left, corner.top() + room.top),
        egui::pos2(corner.right() - room.right, corner.bottom() + room.top),
    );
    if let Some(them) = pill {
        // Clear of a button at either end.
        let ends = 2.0 * (20.0 + theme::ICON_BUTTON_PAD + 12.0);
        rec_pill(
            ui,
            app,
            call,
            them,
            corner.center(),
            corner.width() - ends,
            over_picture,
        );
    }
    ui.scope_builder(egui::UiBuilder::new().max_rect(corner), |ui| {
        ui.horizontal(|ui| {
            if call.phase.is_live() {
                let back = gettext(locale, "Hide the call screen").into_owned();
                if theme::icon_button(ui, Icon::ArrowLeft, 20.0, corner_tint, corner_hover, &back)
                    .clicked()
                {
                    app.actions.push(Action::LeaveCallSurface);
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (icon, tip) = if app.call_fullscreen {
                    (Icon::Minimize, gettext(locale, "Exit full screen"))
                } else {
                    (Icon::Maximize, gettext(locale, "Full screen"))
                };
                if theme::icon_button(ui, icon, 18.0, corner_tint, corner_hover, &tip).clicked() {
                    app.actions.push(Action::ToggleCallFullscreen);
                }
            });
        });
    });
}

/// Our own camera, in the top corner at its own shape.
fn own_preview(ui: &mut egui::Ui, app: &mut App, call: &CallUpdate, palette: &Palette, area: Rect) {
    if call.video
        && call.camera_on
        && let Some(image) = app.call_local_frame.clone()
    {
        let bounds = vec2(area.width(), area.height()) * PREVIEW_SHARE;
        let size = fit_inside(Rect::from_min_size(area.min, bounds), image_size(&image)).size();
        let at = Rect::from_min_size(egui::pos2(area.right() - size.x, area.top() + 48.0), size);
        let texture = call_texture(
            ui,
            &mut app.call_local_texture,
            "zapfast-call-local",
            &image,
        );
        egui::Image::new((texture, size))
            .corner_radius(CornerRadius::same(12))
            .paint_at(ui, at);
        ui.painter().rect_stroke(
            at,
            CornerRadius::same(12),
            egui::Stroke::new(1.0, palette.outline),
            egui::StrokeKind::Outside,
        );
    }
}

/// Lays `add` out as a row of `height` along the bottom of `area` and
/// returns where it went. Placed by rectangle rather than by layout, so the
/// controls keep their place whatever the column above them holds.
fn bottom_row(ui: &mut egui::Ui, area: Rect, height: f32, add: impl FnOnce(&mut egui::Ui)) -> Rect {
    let rect = Rect::from_min_max(egui::pos2(area.left(), area.bottom() - height), area.max);
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::left_to_right(Align::Center)),
        add,
    );
    rect
}

/// The line under the peer's name, from the backend's phase and nothing else.
pub(crate) fn status(locale: Locale, call: &CallUpdate) -> String {
    match call.phase {
        // The button was pressed and the offer is still on its way out.
        CallPhase::Placing => gettext(locale, "Connecting…").into_owned(),
        // The offer is out; no device of the peer's has said it rings yet.
        CallPhase::Dialing => gettext(locale, "Calling…").into_owned(),
        // The peer's phone rings: its preaccept arrived.
        CallPhase::Ringing => gettext(locale, "Ringing…").into_owned(),
        // Answered, and the media is still coming up.
        CallPhase::Connecting | CallPhase::Accepted => gettext(locale, "Connecting…").into_owned(),
        // The length runs from when the media came up, not from the button.
        CallPhase::Active => call
            .started
            .map(elapsed)
            .unwrap_or_else(|| gettext(locale, "Connecting…").into_owned()),
        CallPhase::Ended | CallPhase::Failed => call
            .outcome
            .map(|outcome| outcome_text(locale, outcome, call.phase))
            .unwrap_or_else(|| gettext(locale, "Call ended").into_owned()),
        CallPhase::Incoming => incoming_text(locale, call, None),
    }
}

/// How a call that is over reads.
fn outcome_text(locale: Locale, outcome: CallOutcome, phase: CallPhase) -> String {
    match outcome {
        CallOutcome::Answered => gettext(locale, "Call ended").into_owned(),
        CallOutcome::Missed => gettext(locale, "Missed call").into_owned(),
        CallOutcome::Declined => gettext(locale, "Declined").into_owned(),
        CallOutcome::Busy => gettext(locale, "Busy").into_owned(),
        CallOutcome::Failed => gettext(locale, "Could not connect").into_owned(),
        CallOutcome::NoAnswer => gettext(locale, "No answer").into_owned(),
        CallOutcome::ConnectionLost if phase == CallPhase::Ended => {
            gettext(locale, "Connection lost").into_owned()
        }
        CallOutcome::ConnectionLost => gettext(locale, "Could not connect").into_owned(),
        CallOutcome::AnsweredElsewhere => {
            gettext(locale, "Answered on another device").into_owned()
        }
        CallOutcome::DeclinedElsewhere => {
            gettext(locale, "Declined on another device").into_owned()
        }
    }
}

fn status_color(call: &CallUpdate, palette: &Palette) -> Color32 {
    match call.phase {
        CallPhase::Failed => palette.danger,
        CallPhase::Ended => palette.secondary,
        _ => palette.accent,
    }
}

/// What moved under the call: a device that went away, or the peer's audio
/// not arriving, said rather than swallowed, then what a recording has to
/// say (`record`).
fn notes(
    ui: &mut egui::Ui,
    locale: Locale,
    call: &CallUpdate,
    palette: &Palette,
    record: &[(String, Color32)],
) {
    for lost in &call.lost_devices {
        ui.add_space(4.0);
        let template = match lost.kind {
            crate::calls::DeviceKind::Microphone => gettext(
                locale,
                "Microphone “{name}” is not available; using the system default",
            ),
            crate::calls::DeviceKind::Speaker => gettext(
                locale,
                "Speaker “{name}” is not available; using the system default",
            ),
            crate::calls::DeviceKind::Camera => gettext(locale, "Camera “{name}” is not available"),
        };
        theme::text(
            ui,
            template.replace("{name}", &lost.name),
            theme::medium(12.5),
            palette.warning,
        );
    }
    if let Some(audio) = call.peer_audio {
        ui.add_space(4.0);
        theme::text(
            ui,
            peer_audio_text(locale, audio),
            theme::medium(12.5),
            palette.warning,
        );
    }
    for (line, color) in record {
        ui.add_space(4.0);
        theme::text(ui, line, theme::medium(12.5), *color);
    }
}

/// What the engine says about the peer's audio, in the reader's language.
fn peer_audio_text(locale: Locale, audio: PeerAudio) -> String {
    match audio {
        PeerAudio::Stalled => gettext(locale, "No audio is arriving from this call").into_owned(),
        PeerAudio::Silent(SilenceReason::NoDecoder) => {
            gettext(locale, "This call's audio cannot be decoded").into_owned()
        }
        PeerAudio::Silent(
            SilenceReason::AuthenticationFailing
            | SilenceReason::UnexpectedPayloadType
            | SilenceReason::CodecRejectingFrames
            | SilenceReason::CodecFlapping
            | SilenceReason::Unknown,
        ) => gettext(locale, "The other side's audio is not getting through").into_owned(),
    }
}

/// `m:ss`, or `h:mm:ss` past an hour.
fn elapsed(started: Instant) -> String {
    crate::util::call_duration(started.elapsed().as_secs())
}

/// One round control; a control that does not apply yet still reads as one.
fn control(
    ui: &mut egui::Ui,
    icon: Icon,
    fill: Color32,
    tint: Color32,
    tooltip: &str,
    enabled: bool,
) -> egui::Response {
    ui.add_enabled_ui(enabled, |ui| {
        let hover = theme::blend(fill, tint, 0.12);
        let fill = if enabled {
            fill
        } else {
            fill.gamma_multiply(0.6)
        };
        theme::circle_button(ui, icon, CONTROL, fill, hover, tint, tooltip)
    })
    .inner
}

/// Mute, the device pickers, and hang up.
fn controls(ui: &mut egui::Ui, app: &mut App, call: &CallUpdate, palette: &Palette) {
    let locale = app.locale;
    let live = call.phase.is_live();
    // A group call takes video only when it began as a video call: the
    // library refuses to turn a group's voice call into one, so no button
    // offers it.
    let camera = call.group.is_none() || call.video;
    let buttons = if camera { 5.0 } else { 4.0 };
    {
        ui.spacing_mut().item_spacing.x = CONTROL_GAP;
        let width = buttons * CONTROL + (buttons - 1.0) * CONTROL_GAP;
        ui.add_space(((ui.available_width() - width) / 2.0).max(0.0));

        // The engine's own mute, which also tells the peer.
        let (icon, tip, fill, tint) = if call.muted {
            (
                Icon::MicOff,
                gettext(locale, "Unmute"),
                palette.text,
                palette.window,
            )
        } else {
            (
                Icon::Mic,
                gettext(locale, "Mute"),
                palette.surface_active,
                palette.text,
            )
        };
        if control(ui, icon, fill, tint, &tip, call.phase.is_connected()).clicked() {
            app.actions.push(Action::SetCallMuted(!call.muted));
        }

        // The camera: starts video on a voice call, and stops or starts
        // sending our picture on a video call.
        let (icon, tip, fill, tint) = if !call.video {
            (
                Icon::Video,
                gettext(locale, "Start video"),
                palette.surface_active,
                palette.text,
            )
        } else if call.camera_on {
            (
                Icon::Video,
                gettext(locale, "Turn the camera off"),
                palette.surface_active,
                palette.text,
            )
        } else {
            (
                Icon::VideoOff,
                gettext(locale, "Turn the camera on"),
                palette.text,
                palette.window,
            )
        };
        if camera && control(ui, icon, fill, tint, &tip, call.phase.is_connected()).clicked() {
            app.actions.push(Action::SetCallCamera(!call.camera_on));
        }

        let open = app.call_devices_open;
        let tip = gettext(locale, "Devices");
        let (fill, tint) = if open {
            (palette.text, palette.window)
        } else {
            (palette.surface_active, palette.text)
        };
        if control(ui, Icon::Volume2, fill, tint, &tip, live).clicked() {
            app.call_devices_open = !open;
        }

        // Recording both voices, kept in the call library once it stops. The
        // worker owns the recording; the button only asks.
        let state = record_state(app, call);
        let (icon, tip, fill, tint) = match state {
            RecordState::Idle if call.video => (
                Icon::Record,
                gettext(locale, "Record the call (audio only)"),
                palette.surface_active,
                palette.text,
            ),
            RecordState::Idle => (
                Icon::Record,
                gettext(locale, "Record the call"),
                palette.surface_active,
                palette.text,
            ),
            RecordState::Armed => (
                Icon::Record,
                gettext(locale, "Recording starts as soon as the call connects"),
                palette.surface_active,
                palette.danger,
            ),
            // The spinner takes the icon's place.
            RecordState::Requested => (
                Icon::Record,
                gettext(locale, "Getting the recording ready…"),
                palette.surface_active,
                Color32::TRANSPARENT,
            ),
            RecordState::Recording => (
                Icon::Stop,
                gettext(locale, "Stop and save the recording"),
                palette.danger,
                Color32::WHITE,
            ),
        };
        let response = control(ui, icon, fill, tint, &tip, live);
        match state {
            RecordState::Armed => {
                ui.painter().circle_stroke(
                    response.rect.center(),
                    CONTROL / 2.0 - 1.0,
                    egui::Stroke::new(2.0, palette.danger),
                );
            }
            RecordState::Requested => {
                theme::paint_spinner(ui, response.rect, 22.0, palette.text);
            }
            RecordState::Idle | RecordState::Recording => {}
        }
        if response.clicked() {
            app.actions.push(Action::ToggleCallRecording);
        }

        let tip = gettext(locale, "Hang up");
        if control(
            ui,
            Icon::PhoneOff,
            palette.danger,
            Color32::WHITE,
            &tip,
            live,
        )
        .clicked()
        {
            app.actions.push(Action::HangupCall);
        }
    }
}

/// The microphone and speaker pickers. Each one rebinds a live stream, so
/// what it shows is what is in use; nothing about the system changes.
fn devices(ui: &mut egui::Ui, app: &mut App, call: &CallUpdate, palette: &Palette) {
    let locale = app.locale;
    let default_label = gettext(locale, "System default").into_owned();
    let microphones: Vec<(String, String)> = app
        .call_devices
        .microphones
        .iter()
        .map(|device| (device.id.clone(), device.label.clone()))
        .collect();
    let speakers: Vec<(String, String)> = app
        .call_devices
        .speakers
        .iter()
        .map(|device| (device.id.clone(), device.label.clone()))
        .collect();
    let cameras: Vec<(String, String)> = app
        .call_devices
        .cameras
        .iter()
        .map(|device| (device.id.clone(), device.label.clone()))
        .collect();
    let mut actions = Vec::new();
    {
        let pickers = if call.video { 3.0 } else { 2.0 };
        let width = pickers * PICKER + (pickers - 1.0) * 16.0;
        ui.add_space(((ui.available_width() - width) / 2.0).max(0.0));
        Picker {
            salt: "call-microphone",
            icon: Icon::Mic,
            title: &gettext(locale, "Microphone"),
            default_label: &default_label,
            current: call.microphone.as_deref(),
            devices: &microphones,
            palette,
        }
        .show(ui, &mut actions, Action::SetCallMicrophone);
        ui.add_space(16.0);
        Picker {
            salt: "call-speaker",
            icon: Icon::Volume2,
            title: &gettext(locale, "Speaker"),
            default_label: &default_label,
            current: call.speaker.as_deref(),
            devices: &speakers,
            palette,
        }
        .show(ui, &mut actions, Action::SetCallSpeaker);
        if call.video {
            ui.add_space(16.0);
            Picker {
                salt: "call-camera",
                icon: Icon::Video,
                title: &gettext(locale, "Camera"),
                default_label: &gettext(locale, "First camera"),
                current: call.camera.as_deref(),
                devices: &cameras,
                palette,
            }
            .show(ui, &mut actions, Action::SetCallCameraDevice);
        }
    }
    app.actions.extend(actions);
}

/// The width of one device picker, and the height of the row they make.
const PICKER: f32 = 200.0;
const PICKER_HEIGHT: f32 = 60.0;

/// One device picker: its title with an icon, and what the machine has.
struct Picker<'a> {
    salt: &'a str,
    icon: Icon,
    title: &'a str,
    default_label: &'a str,
    current: Option<&'a str>,
    devices: &'a [(String, String)],
    palette: &'a Palette,
}

impl Picker<'_> {
    fn show(
        &self,
        ui: &mut egui::Ui,
        actions: &mut Vec<Action>,
        action: impl Fn(Option<String>) -> Action,
    ) {
        let label = self
            .current
            .and_then(|id| {
                self.devices
                    .iter()
                    .find(|(known, _)| known == id)
                    .map(|(_, label)| label.clone())
            })
            .or_else(|| self.current.map(str::to_owned))
            .unwrap_or_else(|| self.default_label.to_owned());
        ui.vertical(|ui| {
            ui.set_width(PICKER);
            ui.horizontal(|ui| {
                theme::icon(ui, self.icon, 13.0, self.palette.secondary);
                theme::text(ui, self.title, theme::medium(12.0), self.palette.secondary);
            });
            egui::ComboBox::from_id_salt(self.salt)
                .selected_text(label)
                .width(PICKER)
                .truncate()
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(self.current.is_none(), self.default_label)
                        .clicked()
                    {
                        actions.push(action(None));
                    }
                    for (id, name) in self.devices {
                        if ui
                            .selectable_label(self.current == Some(id.as_str()), name)
                            .clicked()
                        {
                            actions.push(action(Some(id.clone())));
                        }
                    }
                });
        });
    }
}

/// Who a call is with, as the surface names them: the chat (a group's own
/// name for a group call), and for a group call who rang and who is on it.
struct Who<'a> {
    peer: &'a str,
    picture: Option<&'a std::path::Path>,
    caller: Option<&'a str>,
    people: &'a [CallPerson],
    /// Ourselves, as our own tile on a group call shows us.
    me: Option<&'a CallPerson>,
}

/// What a ringing call says it is: a one-to-one call its kind, a group call
/// also who is calling, as WhatsApp's own prompt does. The desktop
/// notification says the same.
pub(crate) fn incoming_text(locale: Locale, call: &CallUpdate, caller: Option<&str>) -> String {
    match (call.group.is_some(), caller, call.video) {
        (false, _, false) => gettext(locale, "Incoming voice call").into_owned(),
        (false, _, true) => gettext(locale, "Incoming video call").into_owned(),
        (true, Some(name), false) => {
            gettext(locale, "Group voice call from {name}").replace("{name}", name)
        }
        (true, Some(name), true) => {
            gettext(locale, "Group video call from {name}").replace("{name}", name)
        }
        (true, None, false) => gettext(locale, "Incoming group voice call").into_owned(),
        (true, None, true) => gettext(locale, "Incoming group video call").into_owned(),
    }
}

/// A call that rings: who it is, and the two answers.
fn ringing(
    ui: &mut egui::Ui,
    app: &mut App,
    call: &CallUpdate,
    who: &Who<'_>,
    palette: &Palette,
    area: Rect,
) {
    let locale = app.locale;
    ui.vertical_centered(|ui| {
        ui.add_space(area.height() * 0.18);
        let size = (area.height() * 0.24).clamp(96.0, 160.0);
        widgets::avatar(ui, palette, who.peer, &call.chat, size, who.picture);
        ui.add_space(18.0);
        theme::text(ui, who.peer, theme::bold(26.0), palette.text);
        ui.add_space(6.0);
        theme::text(
            ui,
            incoming_text(locale, call, who.caller),
            theme::medium(15.0),
            palette.secondary,
        );
        // Who is on the group call already, by their pictures.
        let joined: Vec<&CallPerson> = who.people.iter().filter(|person| person.joined).collect();
        if !joined.is_empty() {
            ui.add_space(16.0);
            faces(ui, palette, &joined);
        }
    });
    bottom_row(ui, area, CONTROL + 26.0, |ui| {
        let answers = [
            (
                Icon::PhoneOff,
                palette.danger,
                Color32::WHITE,
                gettext(locale, "Decline"),
                Action::DeclineCall,
            ),
            (
                Icon::Record,
                palette.accent,
                palette.on_accent,
                gettext(locale, "Answer and record"),
                Action::AnswerAndRecord,
            ),
            (
                Icon::Phone,
                palette.accent,
                palette.on_accent,
                gettext(locale, "Accept"),
                Action::AnswerCall,
            ),
        ];
        // Every cell as wide as the widest word, so the circles keep an even
        // step whatever the language.
        let word = answers
            .iter()
            .map(|(_, _, _, label, _)| {
                ui.painter()
                    .layout_no_wrap(label.to_string(), theme::medium(13.0), palette.secondary)
                    .size()
                    .x
            })
            .fold(0.0, f32::max);
        let (cell, spacing) = answer_cells(word, ui.available_width());
        ui.spacing_mut().item_spacing.x = spacing;
        let width = 3.0 * cell + 2.0 * spacing;
        ui.add_space(((ui.available_width() - width) / 2.0).max(0.0));
        for (icon, fill, tint, label, action) in answers {
            if answer(ui, icon, fill, tint, &label, cell, palette).clicked() {
                app.actions.push(action);
            }
        }
    });
}

/// The gap between a ringing call's answers: wide as WhatsApp's own prompt,
/// closer in a narrow window.
const ANSWER_GAP: f32 = 72.0;
const ANSWER_GAP_NARROW: f32 = 40.0;

/// The width of each of a ringing call's three answers and the spacing
/// between them, for words up to `word` wide in `room`: the circles stand
/// [`ANSWER_GAP`] apart, or [`ANSWER_GAP_NARROW`] when that does not fit,
/// and the words under them never touch. In a window too narrow even for
/// that, the words are cut short; the circles say them in full on hover.
pub(crate) fn answer_cells(word: f32, room: f32) -> (f32, f32) {
    const APART: f32 = 12.0;
    let cell = CONTROL.max(word);
    let spacing = |gap: f32| (gap - (cell - CONTROL)).max(APART);
    let wide = spacing(ANSWER_GAP);
    let narrow = spacing(ANSWER_GAP_NARROW);
    if 3.0 * cell + 2.0 * wide <= room {
        (cell, wide)
    } else if 3.0 * cell + 2.0 * narrow <= room {
        (cell, narrow)
    } else {
        let cell = ((room - 2.0 * APART) / 3.0).clamp(CONTROL, cell);
        (cell, spacing(ANSWER_GAP_NARROW))
    }
}

/// How many faces the ringing prompt shows before it counts the rest.
const FACES: usize = 5;
/// The size of one of them.
const FACE: f32 = 36.0;

/// A row of the pictures of the people on a group call, overlapping as
/// WhatsApp draws them, and "+N" for those past the first few.
fn faces(ui: &mut egui::Ui, palette: &Palette, people: &[&CallPerson]) {
    let shown = people.len().min(FACES);
    let extra = people.len() - shown;
    let step = FACE * 0.72;
    let count = shown + usize::from(extra > 0);
    let width = FACE + step * (count.saturating_sub(1)) as f32;
    let (rect, _) = ui.allocate_exact_size(vec2(width, FACE), egui::Sense::hover());
    for (index, person) in people.iter().take(shown).enumerate() {
        let at = Rect::from_min_size(
            egui::pos2(rect.left() + step * index as f32, rect.top()),
            Vec2::splat(FACE),
        );
        // A ring of the window's colour keeps overlapping faces apart.
        ui.painter()
            .circle_filled(at.center(), FACE / 2.0 + 2.0, palette.window);
        widgets::paint_avatar(
            ui,
            palette,
            at,
            &person.name,
            &person.id,
            person.picture.as_deref(),
        );
    }
    if extra > 0 {
        let at = Rect::from_min_size(
            egui::pos2(rect.left() + step * shown as f32, rect.top()),
            Vec2::splat(FACE),
        );
        ui.painter()
            .circle_filled(at.center(), FACE / 2.0 + 2.0, palette.window);
        ui.painter()
            .circle_filled(at.center(), FACE / 2.0, palette.surface_active);
        ui.painter().text(
            at.center(),
            egui::Align2::CENTER_CENTER,
            format!("+{extra}"),
            theme::semibold(13.0),
            palette.text,
        );
    }
}

/// The most tiles a group call lays out; the rest are counted on the last.
const MAX_TILES: usize = 12;
/// The gap between tiles.
const TILE_GAP: f32 = 10.0;
/// The shape a tile is laid out at, wide as a call's picture.
const TILE_ASPECT: f32 = 4.0 / 3.0;

/// Lays out `count` tiles of [`TILE_ASPECT`] in `area`, as large as they
/// fit: the column count that leaves the biggest tile wins, the grid sits
/// in the middle, and a short last row is centred under the others.
pub(crate) fn tile_grid(count: usize, area: Rect, gap: f32) -> Vec<Rect> {
    if count == 0 || area.width() <= 0.0 || area.height() <= 0.0 {
        return Vec::new();
    }
    let mut best: Option<(usize, Vec2)> = None;
    for columns in 1..=count {
        let rows = count.div_ceil(columns);
        let cell = vec2(
            (area.width() - gap * (columns - 1) as f32) / columns as f32,
            (area.height() - gap * (rows - 1) as f32) / rows as f32,
        );
        if cell.x <= 0.0 || cell.y <= 0.0 {
            continue;
        }
        let tile = if cell.x / cell.y > TILE_ASPECT {
            vec2(cell.y * TILE_ASPECT, cell.y)
        } else {
            vec2(cell.x, cell.x / TILE_ASPECT)
        };
        if best.is_none_or(|(_, size)| tile.x * tile.y > size.x * size.y) {
            best = Some((columns, tile));
        }
    }
    let Some((columns, tile)) = best else {
        return Vec::new();
    };
    let rows = count.div_ceil(columns);
    let height = rows as f32 * tile.y + gap * (rows - 1) as f32;
    let top = area.center().y - height / 2.0;
    (0..count)
        .map(|index| {
            let (row, column) = (index / columns, index % columns);
            let in_row = if row == rows - 1 {
                count - row * columns
            } else {
                columns
            };
            let width = in_row as f32 * tile.x + gap * (in_row - 1) as f32;
            let left = area.center().x - width / 2.0;
            Rect::from_min_size(
                egui::pos2(
                    left + column as f32 * (tile.x + gap),
                    top + row as f32 * (tile.y + gap),
                ),
                tile,
            )
        })
        .collect()
}

/// A group call that is up, coming up, or just over: the group's name and
/// the call's length on top, a tile for each person on it, and the controls.
fn group_live(
    ui: &mut egui::Ui,
    app: &mut App,
    call: &CallUpdate,
    who: &Who<'_>,
    palette: &Palette,
    area: Rect,
) {
    let locale = app.locale;
    let record = record_lines(ui.ctx(), app, call, who.peer);
    // The group's name and how the call is going, over the tiles, with the
    // recording pill beside the length while one runs.
    // Inset past the corner buttons on either side.
    let height = if call.recording.is_some() { 58.0 } else { 52.0 };
    let header = Rect::from_min_size(area.min, vec2(area.width(), height)).shrink2(vec2(56.0, 0.0));
    ui.scope_builder(egui::UiBuilder::new().max_rect(header), |ui| {
        ui.vertical_centered(|ui| {
            theme::text(ui, who.peer, theme::bold(19.0), palette.text);
            ui.add_space(2.0);
            if call.recording.is_some() {
                let line = ui.painter().layout_no_wrap(
                    status(locale, call),
                    theme::medium(13.5),
                    status_color(call, palette),
                );
                let (row, _) = ui.allocate_exact_size(
                    vec2(ui.available_width(), PILL_HEIGHT),
                    egui::Sense::hover(),
                );
                let room = row.width() - line.size().x - 10.0;
                let width = Pill::lay_out(ui, locale, call, who.peer, palette.text, room)
                    .map_or(0.0, |pill| pill.size.x);
                let left = row.center().x - (line.size().x + 10.0 + width) / 2.0;
                ui.painter().galley(
                    egui::pos2(left, row.center().y - line.size().y / 2.0),
                    line.clone(),
                    status_color(call, palette),
                );
                let center = egui::pos2(left + line.size().x + 10.0 + width / 2.0, row.center().y);
                rec_pill(ui, app, call, who.peer, center, room, false);
            } else {
                theme::text(
                    ui,
                    status(locale, call),
                    theme::medium(13.5),
                    status_color(call, palette),
                );
            }
        });
    });
    corner_buttons(ui, app, call, area, false, None);

    let controls_row = bottom_row(ui, area, CONTROL, |ui| controls(ui, app, call, palette));
    let mut bottom = controls_row.top() - 20.0;
    if app.call_devices_open && call.phase.is_live() {
        let above = area.with_max_y(controls_row.top() - 20.0);
        let pickers = bottom_row(ui, above, PICKER_HEIGHT, |ui| {
            devices(ui, app, call, palette)
        });
        bottom = pickers.top() - 16.0;
    }
    // What moved under the call, between the tiles and the controls.
    let notes_height = notes_height(call, record.len());
    if notes_height > 0.0 {
        let room = Rect::from_min_max(
            egui::pos2(area.left(), bottom - notes_height),
            egui::pos2(area.right(), bottom),
        );
        ui.scope_builder(egui::UiBuilder::new().max_rect(room), |ui| {
            ui.vertical_centered(|ui| notes(ui, locale, call, palette, &record));
        });
        bottom -= notes_height + 8.0;
    }

    // Those in the call first, then our own camera while it runs, then those
    // only asked to it; past the most a grid holds, the last tile counts the
    // rest.
    let camera = (call.video && call.camera_on)
        .then(|| app.call_local_frame.clone())
        .flatten();
    let mut tiles: Vec<Tile<'_>> = who
        .people
        .iter()
        .filter(|person| person.joined)
        .map(Tile::Person)
        .collect();
    // Our own tile, in a voice call as in a video one: our camera while it
    // runs, else our photo.
    match (camera, who.me) {
        (Some(image), _) => tiles.push(Tile::Me(image)),
        (None, Some(me)) => tiles.push(Tile::MyPhoto(me)),
        (None, None) => {}
    }
    tiles.extend(
        who.people
            .iter()
            .filter(|person| !person.joined)
            .map(Tile::Person),
    );
    if tiles.len() > MAX_TILES {
        let rest = tiles.len() - (MAX_TILES - 1);
        tiles.truncate(MAX_TILES - 1);
        tiles.push(Tile::More(rest));
    }
    let grid = Rect::from_min_max(
        egui::pos2(area.left(), header.bottom() + 12.0),
        egui::pos2(area.right(), bottom.max(header.bottom() + 60.0)),
    );
    let rects = tile_grid(tiles.len(), grid, TILE_GAP);
    let you = gettext(locale, "You").into_owned();
    for (tile, rect) in tiles.into_iter().zip(rects) {
        match tile {
            // A member whose camera is on fills their tile with it; anyone
            // else keeps their photo, so nobody leaves the grid by turning it
            // off.
            Tile::Person(person) => match person
                .video
                .then(|| app.call_member_frames.get(&person.id).cloned())
                .flatten()
            {
                Some(image) => {
                    let mut kept = app.call_member_textures.remove(&person.id);
                    let texture = call_texture(ui, &mut kept, "zapfast-call-member", &image);
                    if let Some(kept) = kept {
                        app.call_member_textures.insert(person.id.clone(), kept);
                    }
                    video_tile(ui, &person.name, texture, image_size(&image), rect);
                }
                None => person_tile(ui, locale, palette, person, rect),
            },
            // Our own camera takes a tile of its own beside the others rather
            // than floating over one of them.
            Tile::Me(image) => {
                let texture = call_texture(
                    ui,
                    &mut app.call_local_texture,
                    "zapfast-call-local",
                    &image,
                );
                video_tile(ui, &you, texture, image_size(&image), rect);
                if call.muted {
                    muted_badge(ui, palette, rect);
                }
            }
            Tile::MyPhoto(me) => {
                person_tile(ui, locale, palette, me, rect);
                if call.muted {
                    muted_badge(ui, palette, rect);
                }
            }
            Tile::More(rest) => {
                ui.painter()
                    .rect_filled(rect, CornerRadius::same(14), tile_fill(palette));
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    format!("+{rest}"),
                    theme::bold(24.0),
                    palette.secondary,
                );
            }
        }
    }
}

/// One tile of a group call's grid.
enum Tile<'a> {
    Person(&'a CallPerson),
    /// Our own camera's picture.
    Me(std::sync::Arc<egui::ColorImage>),
    /// Ourselves while our camera is off, or on a voice call.
    MyPhoto(&'a CallPerson),
    /// How many more are on the call than the grid holds.
    More(usize),
}

/// The sign a muted microphone wears in the corner of our own tile: only
/// ours, since the library says nothing of anyone else's.
fn muted_badge(ui: &egui::Ui, palette: &Palette, tile: Rect) {
    let size = 30.0;
    let at = Rect::from_min_size(
        egui::pos2(tile.right() - size - 10.0, tile.top() + 10.0),
        Vec2::splat(size),
    );
    ui.painter()
        .circle_filled(at.center(), size / 2.0, palette.danger);
    theme::paint_icon(ui, Icon::MicOff, at, 16.0, Color32::WHITE);
}

/// A call link's picture: a link on a plain disc, where a chat would have
/// its photo.
pub(crate) fn paint_link_avatar(ui: &egui::Ui, palette: &Palette, rect: Rect) {
    ui.painter()
        .circle_filled(rect.center(), rect.width() / 2.0, palette.surface_active);
    theme::paint_icon(ui, Icon::Link, rect, rect.width() * 0.45, palette.text);
}

/// A tile's ground: a shade off the window in either theme, where a light
/// theme's surface is the window's own colour.
fn tile_fill(palette: &Palette) -> Color32 {
    theme::blend(
        palette.window,
        palette.text,
        if palette.dark { 0.08 } else { 0.06 },
    )
}

/// The room the notes under a call take, so a group call's tiles leave it,
/// with `record` lines about a recording.
fn notes_height(call: &CallUpdate, record: usize) -> f32 {
    let lines = call.lost_devices.len() + usize::from(call.peer_audio.is_some()) + record;
    lines as f32 * 22.0
}

/// A member's tile while their camera is on: the picture fills it, cropped
/// to its shape as WhatsApp's grid does, with their name on a plate in the
/// corner.
fn video_tile(ui: &mut egui::Ui, name: &str, texture: egui::TextureId, size: Vec2, rect: Rect) {
    egui::Image::new((texture, rect.size()))
        .uv(cover_uv(size, rect.size()))
        .corner_radius(CornerRadius::same(14))
        .paint_at(ui, rect);
    let label = ui
        .painter()
        .layout_no_wrap(name.to_owned(), theme::semibold(13.0), Color32::WHITE);
    let width = label.size().x.min(rect.width() - 36.0).max(0.0);
    let plate = Rect::from_min_size(
        egui::pos2(
            rect.left() + 10.0,
            rect.bottom() - 10.0 - label.size().y - 8.0,
        ),
        vec2(width + 16.0, label.size().y + 8.0),
    );
    ui.painter()
        .rect_filled(plate, CornerRadius::same(8), VIDEO_SCRIM);
    let clip = ui.painter().with_clip_rect(plate.shrink2(vec2(8.0, 0.0)));
    clip.galley(
        egui::pos2(plate.left() + 8.0, plate.top() + 4.0),
        label,
        Color32::WHITE,
    );
}

/// The part of a picture of `image` size that covers a tile of `tile` size:
/// the middle of it, cut down on whichever side is too long.
fn cover_uv(image: Vec2, tile: Vec2) -> Rect {
    let full = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
    if image.x <= 0.0 || image.y <= 0.0 || tile.x <= 0.0 || tile.y <= 0.0 {
        return full;
    }
    let (picture, frame) = (image.x / image.y, tile.x / tile.y);
    if picture > frame {
        let keep = frame / picture;
        Rect::from_min_max(
            egui::pos2((1.0 - keep) / 2.0, 0.0),
            egui::pos2((1.0 + keep) / 2.0, 1.0),
        )
    } else {
        let keep = picture / frame;
        Rect::from_min_max(
            egui::pos2(0.0, (1.0 - keep) / 2.0),
            egui::pos2(1.0, (1.0 + keep) / 2.0),
        )
    }
}

/// One person's tile on a group call: their picture and name, dimmed while
/// they are only asked to the call rather than in it.
fn person_tile(
    ui: &mut egui::Ui,
    locale: Locale,
    palette: &Palette,
    person: &CallPerson,
    rect: Rect,
) {
    ui.painter()
        .rect_filled(rect, CornerRadius::same(14), tile_fill(palette));
    let label_height = if person.joined { 22.0 } else { 40.0 };
    let size = (rect.height() - label_height - 24.0)
        .min(rect.width() * 0.5)
        .clamp(28.0, 120.0);
    let block = size + 8.0 + label_height;
    let top = rect.center().y - block / 2.0;
    let face = Rect::from_center_size(
        egui::pos2(rect.center().x, top + size / 2.0),
        Vec2::splat(size),
    );
    widgets::paint_avatar(
        ui,
        palette,
        face,
        &person.name,
        &person.id,
        person.picture.as_deref(),
    );
    if !person.joined {
        // Asked, not in yet: the face waits behind a veil.
        ui.painter().circle_filled(
            face.center(),
            size / 2.0,
            tile_fill(palette).gamma_multiply(0.55),
        );
    }
    let label = Rect::from_min_size(
        egui::pos2(rect.left() + 10.0, face.bottom() + 8.0),
        vec2(rect.width() - 20.0, label_height),
    );
    ui.scope_builder(egui::UiBuilder::new().max_rect(label), |ui| {
        ui.vertical_centered(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            let color = if person.joined {
                palette.text
            } else {
                palette.secondary
            };
            theme::text(ui, &person.name, theme::semibold(14.0), color);
            if !person.joined {
                theme::text(
                    ui,
                    gettext(locale, "Invited"),
                    theme::medium(12.0),
                    palette.dim,
                );
            }
        });
    });
}

/// One of a ringing call's answers, its word under it, in a cell `width`
/// wide.
fn answer(
    ui: &mut egui::Ui,
    icon: Icon,
    fill: Color32,
    tint: Color32,
    label: &str,
    width: f32,
    palette: &Palette,
) -> egui::Response {
    ui.allocate_ui_with_layout(
        vec2(width, CONTROL + 26.0),
        Layout::top_down(Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            let response = control(ui, icon, fill, tint, label, true);
            theme::text(ui, label, theme::medium(13.0), palette.secondary);
            response
        },
    )
    .inner
}

/// What the record control shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecordState {
    Idle,
    /// It starts as soon as the call is up.
    Armed,
    /// Asked of the worker, whose answer has not come yet.
    Requested,
    Recording,
}

/// Where the call's recording stands, as the record control draws it.
pub(crate) fn record_state(app: &App, call: &CallUpdate) -> RecordState {
    if call.recording.is_some() {
        RecordState::Recording
    } else if app.call_record_pending(call.generation) {
        RecordState::Requested
    } else if app.call_record_armed == Some(call.generation) {
        RecordState::Armed
    } else {
        RecordState::Idle
    }
}

/// "REC" and how long the recording has run.
fn rec_time(locale: Locale, recording: &RecordingView) -> String {
    gettext(locale, "REC {time}").replace(
        "{time}",
        &crate::util::call_duration(recording.since.elapsed().as_secs()),
    )
}

/// The recording pill's height.
const PILL_HEIGHT: f32 = 30.0;
/// Its padding at either end.
const PILL_PAD: f32 = 12.0;
/// The red dot it starts with.
const PILL_DOT: f32 = 8.0;
/// The width of one of its level bars.
const LEVEL_BAR: f32 = 26.0;
/// The widest a name beside a level bar is drawn.
const LEVEL_NAME: f32 = 84.0;

/// The recording pill's words and size, laid out before it is placed.
struct Pill {
    time: std::sync::Arc<egui::Galley>,
    /// Our side's and the other side's names over their level bars, left
    /// out where there is no room for them.
    sides: Option<[std::sync::Arc<egui::Galley>; 2]>,
    size: Vec2,
}

impl Pill {
    /// The pill for the call's recording in at most `room`, or `None` while
    /// nothing is recorded.
    fn lay_out(
        ui: &egui::Ui,
        locale: Locale,
        call: &CallUpdate,
        peer: &str,
        ink: Color32,
        room: f32,
    ) -> Option<Self> {
        let recording = call.recording.as_ref()?;
        let mut words = rec_time(locale, recording);
        if call.muted {
            words = gettext(locale, "{rec} · without your voice").replace("{rec}", &words);
        }
        let time = ui.painter().layout_no_wrap(
            words,
            theme::tabular(fastframe_fonts::Weight::SemiBold, 12.5),
            ink,
        );
        let bare = 2.0 * PILL_PAD + PILL_DOT + 6.0 + time.size().x;
        let you = ui.painter().layout_no_wrap(
            gettext(locale, "You").into_owned(),
            theme::medium(11.5),
            ink,
        );
        let them = if call.group.is_some() {
            gettext(locale, "Them").into_owned()
        } else {
            peer.to_owned()
        };
        let mut job = egui::text::LayoutJob::simple_singleline(them, theme::medium(11.5), ink);
        job.wrap = egui::text::TextWrapping {
            max_width: LEVEL_NAME,
            max_rows: 1,
            break_anywhere: true,
            overflow_character: Some('\u{2026}'),
        };
        let them = ui.painter().layout_job(job);
        let sides = 12.0 + you.size().x + 5.0 + LEVEL_BAR + 10.0 + them.size().x + 5.0 + LEVEL_BAR;
        let (width, sides) = if bare + sides <= room {
            (bare + sides, Some([you, them]))
        } else {
            (bare, None)
        };
        Some(Self {
            time,
            sides,
            size: vec2(width, PILL_HEIGHT),
        })
    }
}

/// Where the recording pill was drawn this frame, for layout tests.
pub fn pill_id() -> egui::Id {
    egui::Id::new(SURFACE).with("recording-pill")
}

/// Where the name plate over a peer's picture was drawn, for layout tests.
pub fn plate_id() -> egui::Id {
    egui::Id::new(SURFACE).with("plate")
}

/// The recording pill: "REC" and how long, how loud our side and the other
/// side come in, and a menu to stop, tell the other side in the chat, or
/// discard. Centred on `center` and no wider than `room`; returns where it
/// went, or `None` while nothing is recorded.
fn rec_pill(
    ui: &mut egui::Ui,
    app: &mut App,
    call: &CallUpdate,
    peer: &str,
    center: egui::Pos2,
    room: f32,
    over_picture: bool,
) -> Option<Rect> {
    let locale = app.locale;
    let palette = app.palette;
    let (fill, ink) = if over_picture {
        (VIDEO_SCRIM, Color32::WHITE)
    } else {
        let tint = if palette.dark { 0.22 } else { 0.12 };
        (
            theme::blend(palette.window, palette.danger, tint),
            palette.text,
        )
    };
    let pill = Pill::lay_out(ui, locale, call, peer, ink, room)?;
    let rect = Rect::from_center_size(center, pill.size);
    let response = ui.interact(rect, pill_id(), egui::Sense::click());
    let label = gettext(locale, "Recording options");
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label.as_ref())
    });
    theme::reveal_focus(&response);
    theme::focus_outline(ui, response.id, rect, rect.height() / 2.0);
    let open = egui::Popup::is_id_open(ui.ctx(), response.id.with("popup"));
    let fill = if response.hovered() || open {
        theme::blend(fill, ink, 0.08)
    } else {
        fill
    };
    let painter = ui.painter();
    painter.rect_filled(rect, rect.height() / 2.0, fill);
    let mut x = rect.left() + PILL_PAD;
    painter.circle_filled(
        egui::pos2(x + PILL_DOT / 2.0, rect.center().y),
        PILL_DOT / 2.0,
        palette.danger,
    );
    x += PILL_DOT + 6.0;
    let middle = |galley: &egui::Galley| rect.center().y - galley.size().y / 2.0;
    painter.galley(egui::pos2(x, middle(&pill.time)), pill.time.clone(), ink);
    x += pill.time.size().x;
    if let Some(sides) = &pill.sides {
        let levels = levels(app, call).unwrap_or_default();
        x += 12.0;
        for (side, level) in sides.iter().zip(levels) {
            painter.galley(
                egui::pos2(x, middle(side)),
                side.clone(),
                ink.gamma_multiply(0.75),
            );
            x += side.size().x + 5.0;
            let track =
                Rect::from_min_size(egui::pos2(x, rect.center().y - 2.0), vec2(LEVEL_BAR, 4.0));
            painter.rect_filled(track, 2.0, ink.gamma_multiply(0.18));
            let shown = level_shown(level);
            if shown > 0.0 {
                painter.rect_filled(
                    track.with_max_x(track.left() + LEVEL_BAR * shown),
                    2.0,
                    palette.accent,
                );
            }
            x += LEVEL_BAR + 10.0;
        }
    }
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(100));
    ui.ctx().data_mut(|data| data.insert_temp(pill_id(), rect));
    let mut response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    if call.muted && !open {
        let tip = if call.group.is_some() {
            gettext(
                locale,
                "You are muted: your voice is not recorded, just as the others do not hear it",
            )
            .into_owned()
        } else {
            gettext(
                locale,
                "You are muted: your voice is not recorded, just as {name} does not hear it",
            )
            .replace("{name}", peer)
        };
        response = response.on_hover_text(tip);
    }
    recording_menu(ui, app, call, &response);
    Some(rect)
}

/// How much of a level bar a loudness fills: speech sits far below full
/// scale, so the bar is scaled up and eased for it to move visibly.
fn level_shown(level: f32) -> f32 {
    (level / 0.3).clamp(0.0, 1.0).sqrt()
}

/// The loudness of each side of the call's recording, while one runs.
fn levels(app: &App, call: &CallUpdate) -> Option<[f32; 2]> {
    #[cfg(any(test, feature = "demo"))]
    if let Some(levels) = app.call_demo_levels {
        return Some(levels);
    }
    #[cfg(not(any(test, feature = "demo")))]
    let _ = app;
    crate::call_tape::live_levels(call.generation)
}

/// The recording pill's menu: stop and keep, a note for the chat, and a
/// discard that asks first, in place.
fn recording_menu(ui: &egui::Ui, app: &mut App, call: &CallUpdate, pill: &egui::Response) {
    let palette = app.palette;
    let locale = app.locale;
    let asking = pill.id.with(("discard", call.generation));
    #[cfg(any(test, feature = "demo"))]
    if let Some(discard) = app.open_recording_menu {
        egui::Popup::open_id(ui.ctx(), pill.id.with("popup"));
        if discard {
            ui.ctx().data_mut(|data| data.insert_temp(asking, true));
        }
    }
    let mut confirm = ui
        .ctx()
        .data(|data| data.get_temp::<bool>(asking))
        .unwrap_or(false);
    let stop = gettext(locale, "Stop and save");
    let notify = gettext(locale, "Notify by chat");
    let discard = gettext(locale, "Discard recording…");
    let width = widgets::menu_width(ui, &[&stop, &notify, &discard], true).max(260.0);
    let mut actions = Vec::new();
    let shown = egui::Popup::menu(pill)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .width(width)
        .frame(widgets::menu_frame(&palette))
        .show(|ui| {
            if confirm {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    ui.set_width(width - 34.0);
                    theme::paragraph(
                        ui,
                        gettext(
                            locale,
                            "Discard the recording? It is deleted and nothing is kept.",
                        ),
                        theme::regular(13.5),
                        palette.text,
                    );
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add_space(4.0);
                        let discard = gettext(locale, "Discard");
                        if super::dialogs::danger_button(ui, app, &discard).clicked() {
                            actions.push(Action::DiscardCallRecording);
                            confirm = false;
                            ui.close();
                        }
                        let keep = gettext(locale, "Keep recording");
                        if theme::pill_button(ui, &palette, &keep, false).clicked() {
                            confirm = false;
                        }
                    });
                });
                ui.add_space(4.0);
            } else {
                if widgets::menu_item(ui, &palette, Some(Icon::Stop), &stop) {
                    actions.push(Action::ToggleCallRecording);
                }
                // A call link has no chat to write in.
                if widgets::menu_item_enabled(
                    ui,
                    &palette,
                    Some(Icon::MessageCircle),
                    &notify,
                    !call.chat.is_empty(),
                ) {
                    actions.push(Action::DraftRecordingNotice);
                }
                widgets::menu_separator(ui, &palette);
                if widgets::menu_item_staying(ui, &palette, Some(Icon::Trash), &discard, true) {
                    confirm = true;
                }
            }
        });
    // A menu that closed takes its question with it.
    let confirm = confirm && shown.is_some();
    ui.ctx().data_mut(|data| data.insert_temp(asking, confirm));
    app.actions.extend(actions);
}

/// How long the line saying the notice played stays.
const NOTICE_SHOWN: std::time::Duration = std::time::Duration::from_secs(4);
/// How long the other side may stay silent on a recording before the call
/// screen says it is not coming through.
const SILENCE_SHOWN: std::time::Duration = std::time::Duration::from_secs(20);
/// The loudness under which the other side counts as silent.
const HEARD: f32 = 0.01;

/// What the call screen follows of a recording from frame to frame.
#[derive(Clone, Copy, Debug)]
struct Watch {
    generation: u64,
    notice: NoticeState,
    /// When the notice finished playing, seen here.
    played: Option<Instant>,
    /// When the other side was last heard.
    heard: Instant,
}

impl Watch {
    /// The first look at a recording. A notice that played long before the
    /// screen came back is not news any more.
    fn new(call: &CallUpdate, recording: &RecordingView, now: Instant) -> Self {
        let fresh = now.saturating_duration_since(recording.since) < NOTICE_SHOWN;
        Self {
            generation: call.generation,
            notice: recording.notice,
            played: (recording.notice == NoticeState::Played && fresh).then_some(recording.since),
            heard: recording.since,
        }
    }

    /// The next frame's look.
    fn step(
        mut self,
        call: &CallUpdate,
        recording: &RecordingView,
        levels: Option<[f32; 2]>,
        now: Instant,
    ) -> Self {
        if recording.notice == NoticeState::Played && self.notice != NoticeState::Played {
            self.played = Some(now);
        }
        self.notice = recording.notice;
        // Silence counts only on a call that is up with nothing else said
        // about the other side's audio, and only with levels to go by.
        let silent = call.phase == CallPhase::Active
            && call.peer_audio.is_none()
            && levels.is_some_and(|levels| levels[1] < HEARD);
        if !silent {
            self.heard = now;
        }
        self
    }
}

/// What a recording in progress has to say under the call.
fn record_lines(
    ctx: &egui::Context,
    app: &App,
    call: &CallUpdate,
    peer: &str,
) -> Vec<(String, Color32)> {
    let id = egui::Id::new(SURFACE).with("recording-watch");
    let Some(recording) = &call.recording else {
        ctx.data_mut(|data| data.remove::<Watch>(id));
        return Vec::new();
    };
    let now = Instant::now();
    let watch = ctx
        .data(|data| data.get_temp::<Watch>(id))
        .filter(|watch| watch.generation == call.generation)
        .unwrap_or_else(|| Watch::new(call, recording, now))
        .step(call, recording, levels(app, call), now);
    ctx.data_mut(|data| data.insert_temp(id, watch));
    recording_notes(app.locale, call, recording, &watch, now, peer, &app.palette)
}

/// The lines a recording adds under the call: the notice that played or
/// waits for the microphone, and the other side heard as silence.
fn recording_notes(
    locale: Locale,
    call: &CallUpdate,
    recording: &RecordingView,
    watch: &Watch,
    now: Instant,
    peer: &str,
    palette: &Palette,
) -> Vec<(String, Color32)> {
    let mut lines = Vec::new();
    match recording.notice {
        NoticeState::Held => lines.push((
            gettext(
                locale,
                "The recording notice plays when you turn your microphone on",
            )
            .into_owned(),
            palette.warning,
        )),
        NoticeState::Played
            if watch
                .played
                .is_some_and(|played| now.saturating_duration_since(played) < NOTICE_SHOWN) =>
        {
            lines.push((
                gettext(locale, "The recording notice played").into_owned(),
                palette.secondary,
            ));
        }
        NoticeState::Off | NoticeState::Pending | NoticeState::Played => {}
    }
    if now.saturating_duration_since(watch.heard) >= SILENCE_SHOWN {
        let line = if call.group.is_some() {
            gettext(locale, "The others' voices are not being recorded").into_owned()
        } else {
            gettext(locale, "The voice of {name} is not being recorded").replace("{name}", peer)
        };
        lines.push((line, palette.warning));
    }
    lines
}

/// The first-time sheet's id, and the facts it shows, read once when it
/// opens rather than every frame.
fn intro_id() -> egui::Id {
    egui::Id::new(SURFACE).with("record-intro")
}

#[derive(Clone, Debug)]
struct Intro {
    /// Whether the other side hears the notice, as the switch now says.
    notice: bool,
    /// Whether the local speech model is already on this computer.
    model_ready: bool,
    /// Who summarizes the call.
    provider: crate::projects::engine::Provider,
}

/// "Before recording": what is recorded, where it stays, who reads it,
/// the law, and the notice the other side hears, before the first one.
fn record_intro(app: &mut App, ctx: &egui::Context, pending: PendingRecord) {
    let palette = app.palette;
    let locale = app.locale;
    let mut intro = ctx
        .data(|data| data.get_temp::<Intro>(intro_id()))
        .unwrap_or_else(|| Intro {
            notice: app.settings.call_record_notice,
            model_ready: crate::transcribe::model_ready(&app.dirs.state),
            provider: crate::projects::engine::EngineConfig::load(&app.dirs.config).provider,
        });
    let speech = app.settings.media_engines.speech;
    let frame = Frame::new()
        .fill(palette.overlay)
        .stroke(egui::Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(theme::RADIUS + 4))
        .inner_margin(Margin::same(22))
        .shadow(palette.modal_shadow());
    let mut actions = Vec::new();
    let response = egui::Modal::new(intro_id())
        .frame(frame)
        .backdrop_color(palette.shadow)
        .show(ctx, |ui| {
            ui.set_width(400.0_f32.min((ctx.content_rect().width() - 64.0).max(220.0)));
            ui.spacing_mut().item_spacing.y = 8.0;
            ui.horizontal(|ui| {
                theme::icon(ui, Icon::Record, 18.0, palette.danger);
                theme::text(
                    ui,
                    gettext(locale, "Before recording"),
                    theme::bold(18.0),
                    palette.text,
                );
            });
            ui.add_space(2.0);
            let point = |ui: &mut egui::Ui, text: &str, color: Color32| {
                theme::paragraph(ui, text, theme::regular(13.5), color);
            };
            point(
                ui,
                &gettext(
                    locale,
                    "Both voices are recorded. On video calls, only the audio.",
                ),
                palette.text,
            );
            point(
                ui,
                &gettext(
                    locale,
                    "It stays on this computer. It is not sent to WhatsApp or to the other person.",
                ),
                palette.text,
            );
            if speech == crate::media_ai::SpeechEngine::Local {
                let text = if intro.model_ready {
                    gettext(locale, "When you save it, it is transcribed on this computer.")
                } else {
                    gettext(
                        locale,
                        "When you save it, it is transcribed on this computer. The first time, about 550 MB download.",
                    )
                };
                point(ui, &text, palette.text);
            } else {
                point(
                    ui,
                    &gettext(
                        locale,
                        "When you save it, {engine} transcribes it: the audio leaves your computer.",
                    )
                    .replace("{engine}", &gettext(locale, speech.label())),
                    palette.warning,
                );
            }
            let summary = if intro.provider == crate::projects::engine::Provider::Off {
                gettext(locale, "Nobody summarizes it: summaries are off.").into_owned()
            } else {
                gettext(locale, "{engine} summarizes it.")
                    .replace("{engine}", &gettext(locale, intro.provider.label()))
            };
            point(ui, &summary, palette.text);
            point(
                ui,
                &gettext(
                    locale,
                    "The rules vary by country: in many places you have to tell the other person.",
                ),
                palette.secondary,
            );
            ui.add_space(4.0);
            let label = gettext(locale, "Play a short notice the other person hears");
            ui.horizontal(|ui| {
                let response = widgets::switch(ui, &palette, &mut intro.notice);
                theme::reveal_focus(&response);
                response.widget_info(|| {
                    egui::WidgetInfo::selected(
                        egui::WidgetType::Checkbox,
                        ui.is_enabled(),
                        intro.notice,
                        label.as_ref(),
                    )
                });
                ui.add_space(4.0);
                theme::paragraph(ui, label.as_ref(), theme::medium(13.5), palette.text);
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let record = match pending {
                        PendingRecord::Toggle => gettext(locale, "Record"),
                        PendingRecord::AnswerAndRecord => gettext(locale, "Answer and record"),
                    };
                    if theme::pill_button(ui, &palette, &record, true).clicked() {
                        actions.push(Action::ConfirmCallRecording {
                            notice: intro.notice,
                        });
                    }
                    let later = gettext(locale, "Not now");
                    if theme::pill_button(ui, &palette, &later, false).clicked() {
                        actions.push(Action::CancelCallRecordingIntro);
                    }
                });
            });
        });
    ctx.move_to_top(response.response.layer_id);
    if response.should_close() && actions.is_empty() {
        actions.push(Action::CancelCallRecordingIntro);
    }
    ctx.data_mut(|data| data.insert_temp(intro_id(), intro));
    app.actions.extend(actions);
}

/// A picture's size as a vector.
fn image_size(image: &egui::ColorImage) -> Vec2 {
    vec2(image.width() as f32, image.height() as f32)
}

/// The largest rect with `size`'s shape that fits inside `area`, centred.
fn fit_inside(area: Rect, size: Vec2) -> Rect {
    if size.x <= 0.0 || size.y <= 0.0 {
        return area;
    }
    let scale = (area.width() / size.x).min(area.height() / size.y);
    Rect::from_center_size(area.center(), size * scale)
}

/// Draws `image` through the kept texture, uploaded only when a new frame
/// arrived and then in place: the surface also repaints for its timer, and a
/// texture per repaint would churn GPU memory.
fn call_texture(
    ui: &egui::Ui,
    kept: &mut Option<crate::app::CallTexture>,
    name: &str,
    image: &std::sync::Arc<egui::ColorImage>,
) -> egui::TextureId {
    match kept {
        Some(kept) if std::sync::Arc::ptr_eq(&kept.frame, image) => kept.texture.id(),
        Some(kept) => {
            kept.texture.set(
                egui::ImageData::from((**image).clone()),
                TextureOptions::LINEAR,
            );
            kept.frame = std::sync::Arc::clone(image);
            kept.texture.id()
        }
        None => {
            let texture = ui
                .ctx()
                .load_texture(name, (**image).clone(), TextureOptions::LINEAR);
            let id = texture.id();
            *kept = Some(crate::app::CallTexture {
                texture,
                frame: std::sync::Arc::clone(image),
            });
            id
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn tiles_fill_the_room_without_overlapping() {
        let area = Rect::from_min_size(egui::pos2(0.0, 0.0), vec2(1200.0, 600.0));
        for count in 1..=MAX_TILES {
            let tiles = tile_grid(count, area, TILE_GAP);
            assert_eq!(tiles.len(), count);
            for (index, tile) in tiles.iter().enumerate() {
                assert!(
                    area.expand(0.5).contains_rect(*tile),
                    "{count} tiles: {tile:?} leaves the room"
                );
                assert!((tile.width() / tile.height() - TILE_ASPECT).abs() < 0.01);
                for other in &tiles[index + 1..] {
                    assert!(
                        !tile.shrink(0.5).intersects(other.shrink(0.5)),
                        "{count} tiles overlap"
                    );
                }
            }
        }
        // Two people on a wide window sit side by side, as large as fits.
        let two = tile_grid(2, area, TILE_GAP);
        assert!((two[0].center().y - two[1].center().y).abs() < 0.5);
        // Five are three over two, the short row centred.
        let five = tile_grid(5, area, TILE_GAP);
        let middle = (five[3].left() + five[4].right()) / 2.0;
        assert!((middle - area.center().x).abs() < 0.5);
        assert!(tile_grid(0, area, TILE_GAP).is_empty());
    }

    /// Three answers keep WhatsApp's gap between their circles where it fits and a narrower one
    /// where it does not, and a long word never runs into its neighbour's.
    #[test]
    fn a_ringing_calls_answers_space_out_by_their_words() {
        let (cell, spacing) = answer_cells(40.0, 1000.0);
        assert_eq!(
            cell, CONTROL,
            "short words leave the cells as wide as a circle"
        );
        assert_eq!(spacing, ANSWER_GAP);
        let (cell, spacing) = answer_cells(40.0, 3.0 * CONTROL + 2.0 * ANSWER_GAP - 1.0);
        assert_eq!((cell, spacing), (CONTROL, ANSWER_GAP_NARROW));
        // "Answer and record" is wider than a circle: the circles stay a gap apart, the words at
        // least 12 points.
        let (cell, spacing) = answer_cells(100.0, 1000.0);
        assert_eq!(cell, 100.0);
        assert_eq!(spacing + cell - CONTROL, ANSWER_GAP);
        let (_, spacing) = answer_cells(200.0, 1000.0);
        assert_eq!(spacing, 12.0);
        // Too narrow even for that: the cells shrink to fit and the words are cut short.
        let (cell, spacing) = answer_cells(120.0, 290.0);
        assert!(3.0 * cell + 2.0 * spacing <= 290.0 + 0.01);
        assert!(cell >= CONTROL);
    }

    fn recorded(notice: NoticeState, since: Duration) -> (CallUpdate, RecordingView) {
        let mut call = crate::calls::CallUpdate::placing(1, "123@s.whatsapp.net".into(), false);
        call.phase = CallPhase::Active;
        let view = RecordingView {
            since: Instant::now() - since,
            notice,
            failed: None,
        };
        call.recording = Some(view.clone());
        (call, view)
    }

    fn said(lines: &[(String, Color32)]) -> Vec<&str> {
        lines.iter().map(|(line, _)| line.as_str()).collect()
    }

    /// The notice is said when it plays and while it waits for the microphone, and only for a
    /// few seconds once it has played.
    #[test]
    fn the_call_screen_says_when_the_notice_plays() {
        let palette = Palette::dark();
        let (call, view) = recorded(NoticeState::Held, Duration::from_secs(30));
        let now = Instant::now();
        let watch = Watch::new(&call, &view, now).step(&call, &view, Some([0.2, 0.2]), now);
        let lines = recording_notes(Locale::English, &call, &view, &watch, now, "Ada", &palette);
        assert_eq!(
            said(&lines),
            ["The recording notice plays when you turn your microphone on"]
        );

        let (call, played) = recorded(NoticeState::Played, Duration::from_secs(30));
        let watch = watch.step(&call, &played, Some([0.2, 0.2]), now);
        let lines = recording_notes(
            Locale::English,
            &call,
            &played,
            &watch,
            now,
            "Ada",
            &palette,
        );
        assert_eq!(said(&lines), ["The recording notice played"]);
        let later = now + NOTICE_SHOWN;
        let lines = recording_notes(
            Locale::English,
            &call,
            &played,
            &watch,
            later,
            "Ada",
            &palette,
        );
        assert!(lines.is_empty(), "{lines:?}");

        // A screen that comes back long after the notice played does not announce it.
        let watch = Watch::new(&call, &played, now);
        assert!(watch.played.is_none());
    }

    /// The other side heard as silence for twenty seconds on a call that is up is said, unless
    /// the engine already says its audio is not arriving; a sound resets the count.
    #[test]
    fn a_silent_other_side_is_said_after_twenty_seconds() {
        let palette = Palette::dark();
        let (mut call, view) = recorded(NoticeState::Off, Duration::from_secs(19));
        let now = Instant::now();
        let silent = Some([0.2, 0.0]);
        let watch = Watch::new(&call, &view, now).step(&call, &view, silent, now);
        let soon = recording_notes(Locale::English, &call, &view, &watch, now, "Ada", &palette);
        assert!(soon.is_empty(), "{soon:?}");
        let later = now + Duration::from_secs(1);
        let watch = watch.step(&call, &view, silent, later);
        let lines = recording_notes(
            Locale::English,
            &call,
            &view,
            &watch,
            later,
            "Ada",
            &palette,
        );
        assert_eq!(said(&lines), ["The voice of Ada is not being recorded"]);

        let heard = watch.step(&call, &view, Some([0.2, 0.2]), later);
        assert!(
            recording_notes(
                Locale::English,
                &call,
                &view,
                &heard,
                later,
                "Ada",
                &palette
            )
            .is_empty()
        );
        // No levels to go by, or audio the engine already reports missing, is not silence.
        let unknown = Watch::new(&call, &view, now).step(&call, &view, None, later);
        assert!(
            recording_notes(
                Locale::English,
                &call,
                &view,
                &unknown,
                later,
                "Ada",
                &palette
            )
            .is_empty()
        );
        call.peer_audio = Some(PeerAudio::Stalled);
        let stalled = Watch::new(&call, &view, now).step(&call, &view, silent, later);
        assert!(
            recording_notes(
                Locale::English,
                &call,
                &view,
                &stalled,
                later,
                "Ada",
                &palette
            )
            .is_empty()
        );
    }

    #[test]
    fn a_members_picture_covers_their_tile_from_its_middle() {
        // A wide picture in a 4:3 tile loses its sides.
        let wide = cover_uv(vec2(1600.0, 900.0), vec2(400.0, 300.0));
        assert!((wide.top() - 0.0).abs() < 1e-6 && (wide.bottom() - 1.0).abs() < 1e-6);
        assert!((wide.center().x - 0.5).abs() < 1e-6);
        assert!((wide.width() - (4.0 / 3.0) / (16.0 / 9.0)).abs() < 1e-4);
        // A portrait one loses its top and bottom.
        let tall = cover_uv(vec2(720.0, 1280.0), vec2(400.0, 300.0));
        assert!((tall.width() - 1.0).abs() < 1e-6);
        assert!((tall.center().y - 0.5).abs() < 1e-6);
        assert!(tall.height() < 1.0);
    }
}
