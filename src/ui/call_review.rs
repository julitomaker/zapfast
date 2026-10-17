//! A recorded call's sheet, over whatever page is open: what the call was,
//! which project it belongs to, what reading it is doing, what it said, and
//! what can be done with it. A recording nobody has looked at yet asks first
//! whether to keep it and have it read, or to discard it.
//!
//! The toast a recording leaves opens it, and so do the Calls page, the
//! call's entry in its chat and its project's list of calls. The sheet only
//! pushes actions: moving, renaming, reading and deleting are the app's.

use egui::{Align, Color32, CornerRadius, Frame, Layout, Margin, vec2};

use crate::app::App;
use crate::call_capture::JobPhase;
use crate::call_library::{Call, CallAnalysis};
use crate::i18n::{Locale, gettext};
use crate::model::Action;
use crate::theme::{self, Icon};
use crate::ui::projects::{call_details, chip, commitment};

/// The sheet's id, for its egui state and for tests.
pub fn sheet_id() -> egui::Id {
    egui::Id::new("recorded-call-sheet")
}

/// A question the sheet asks in place before something that cannot be undone.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Confirm {
    /// A recording not reviewed yet: discard it instead of keeping it.
    Discard,
    /// A kept call: delete its audio, transcripts and summary.
    Delete,
}

/// What the sheet keeps between frames: read once as it opens, not every
/// frame, and dropped when it closes.
#[derive(Clone, Debug, Default)]
struct Sheet {
    id: String,
    /// The projects to offer, the call's chat's first, by id and name.
    projects: Vec<(String, String)>,
    /// How many of `projects` hold the call's chat.
    chat_projects: usize,
    /// The lead the name was read for, and its name.
    lead: Option<(String, String)>,
    renaming: Option<String>,
    confirm: Option<Confirm>,
    focus_title: bool,
}

/// Whether the sheet is on screen: one is open, the call screen is not over
/// it, and a locked chat's call waits for its folder.
pub fn visible(app: &App) -> bool {
    let Some(id) = app.recorded_call_open.as_deref() else {
        return false;
    };
    if app.call_surface_open() {
        return false;
    }
    crate::call_library::Library::cached(&app.dirs.state)
        .get(id)
        .is_some_and(|call| !call.private || app.locked_folder_open())
}

/// Draws the open recorded call's sheet, if there is one.
pub fn show(app: &mut App, ctx: &egui::Context) {
    let Some(id) = app.recorded_call_open.clone() else {
        ctx.data_mut(|data| data.remove::<Sheet>(sheet_id()));
        return;
    };
    let library = crate::call_library::Library::cached(&app.dirs.state);
    let Some(call) = library.get(&id) else {
        // Deleted, here or by another window.
        app.actions.push(Action::CloseRecordedCall);
        return;
    };
    if !visible(app) {
        return;
    }
    let mut sheet = ctx
        .data(|data| data.get_temp::<Sheet>(sheet_id()))
        .filter(|sheet| sheet.id == id)
        .unwrap_or_else(|| open(app, call));
    if sheet.lead.as_ref().map(|(lead, _)| lead) != call.lead.as_ref() {
        sheet.lead = lead_name(app, call);
    }
    let palette = app.palette;
    let frame = Frame::new()
        .fill(palette.overlay)
        .stroke(egui::Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(theme::RADIUS + 4))
        .inner_margin(Margin::same(22))
        .shadow(palette.modal_shadow());
    let mut actions = Vec::new();
    let screen = ctx.content_rect();
    let response = egui::Modal::new(sheet_id())
        .frame(frame)
        .backdrop_color(palette.shadow)
        .show(ctx, |ui| {
            ui.set_width(480.0_f32.min((screen.width() - 48.0).max(240.0)));
            ui.spacing_mut().item_spacing.y = 8.0;
            header(app, ui, call, &mut sheet, &mut actions);
            egui::ScrollArea::vertical()
                .id_salt(("recorded-call-body", &call.id))
                .max_height((screen.height() - 220.0).max(160.0))
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 8.0;
                    body(app, ui, call, &mut sheet, &mut actions);
                });
            ui.add_space(4.0);
            footer(app, ui, call, &mut sheet, &mut actions);
        });
    if response.should_close() && actions.is_empty() && sheet.renaming.is_none() {
        actions.push(Action::CloseRecordedCall);
    }
    ctx.data_mut(|data| data.insert_temp(sheet_id(), sheet));
    app.actions.extend(actions);
}

/// Reads what the sheet offers as it opens: the projects, the call's chat's
/// first, and its lead's name.
fn open(app: &App, call: &Call) -> Sheet {
    let map = crate::projects::ProjectMap::load(&app.dirs.config);
    let mut projects: Vec<(String, String)> = call
        .chat
        .as_deref()
        .map(|chat| {
            map.projects_of(chat)
                .into_iter()
                .map(|(project, _)| (project.id.clone(), project.name.clone()))
                .collect()
        })
        .unwrap_or_default();
    let chat_projects = projects.len();
    for project in &map.projects {
        if !projects.iter().any(|(id, _)| *id == project.id) {
            projects.push((project.id.clone(), project.name.clone()));
        }
    }
    Sheet {
        id: call.id.clone(),
        projects,
        chat_projects,
        lead: lead_name(app, call),
        renaming: call.pending_review.then(|| call.title.clone()),
        confirm: None,
        focus_title: call.pending_review,
    }
}

fn lead_name(app: &App, call: &Call) -> Option<(String, String)> {
    let lead = call.lead.as_ref()?;
    let name = crate::leads::Store::open(&app.dirs.config)
        .get(lead)
        .map(|lead| lead.name.clone())?;
    Some((lead.clone(), name))
}

/// The title (renamed in place), when the call was, how long, and what was
/// heard, with the way out at the right.
fn header(app: &App, ui: &mut egui::Ui, call: &Call, sheet: &mut Sheet, actions: &mut Vec<Action>) {
    let palette = app.palette;
    let locale = app.locale;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        theme::icon(ui, Icon::Record, 18.0, palette.danger);
        let buttons = 2.0 * (16.0 + theme::ICON_BUTTON_PAD) + 8.0;
        let width = (ui.available_width() - buttons).max(80.0);
        match sheet.renaming.as_mut() {
            Some(title) => {
                let field = ui.add(
                    egui::TextEdit::singleline(title)
                        .id(sheet_id().with("title"))
                        .font(theme::semibold(16.0))
                        .desired_width(width),
                );
                if sheet.focus_title {
                    field.request_focus();
                    sheet.focus_title = false;
                }
                if field.changed() || field.lost_focus() {
                    actions.push(Action::RenameRecordedCall {
                        id: call.id.clone(),
                        title: title.clone(),
                    });
                    if !call.pending_review {
                        sheet.renaming = None;
                    }
                }
            }
            None => {
                ui.allocate_ui_with_layout(
                    vec2(width, 22.0),
                    Layout::left_to_right(Align::Center),
                    |ui| theme::text(ui, &call.title, theme::bold(17.0), palette.text),
                );
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let close = gettext(locale, "Close");
            if theme::icon_button(ui, Icon::X, 16.0, palette.secondary, palette.text, &close)
                .clicked()
            {
                actions.push(Action::CloseRecordedCall);
            }
            if sheet.renaming.is_none()
                && theme::icon_button(
                    ui,
                    Icon::Pencil,
                    15.0,
                    palette.secondary,
                    palette.text,
                    &gettext(locale, "Rename"),
                )
                .clicked()
            {
                sheet.renaming = Some(call.title.clone());
                sheet.focus_title = true;
            }
        });
    });
    theme::paragraph(
        ui,
        subtitle(locale, call),
        theme::regular(13.0),
        palette.secondary,
    );
    if !call.participants.is_empty() {
        theme::paragraph(
            ui,
            gettext(locale, "With {names}").replace("{names}", &call.participants.join(", ")),
            theme::regular(13.0),
            palette.secondary,
        );
    }
}

/// "Yesterday at 14:03 · 23 min · Both voices".
pub fn subtitle(locale: Locale, call: &Call) -> String {
    let mut parts = vec![crate::util::moment_stamp(locale, call.when())];
    if call.seconds > 0 {
        parts.push(crate::util::call_length(locale, call.seconds));
    }
    if call.stereo {
        parts.push(gettext(locale, "Both voices").into_owned());
    }
    parts.retain(|part| !part.is_empty());
    parts.join(" · ")
}

/// Where the call belongs and what is known about how it was recorded, then
/// what reading it is doing and what it said.
fn body(app: &App, ui: &mut egui::Ui, call: &Call, sheet: &mut Sheet, actions: &mut Vec<Action>) {
    let palette = app.palette;
    let locale = app.locale;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        theme::text(
            ui,
            gettext(locale, "Project"),
            theme::medium(13.5),
            palette.text,
        );
        project_combo(app, ui, call, sheet, actions);
    });
    if let Some((_, name)) = &sheet.lead {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            theme::text(
                ui,
                gettext(locale, "Lead"),
                theme::medium(13.5),
                palette.text,
            );
            theme::text(ui, name, theme::regular(13.5), palette.secondary);
        });
    }
    let marks = marks(call);
    if !marks.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(6.0, 4.0);
            for mark in marks {
                let (text, color) = mark.label(locale, &palette);
                chip(ui, &text, color);
            }
        });
    }
    if call.pending_review {
        theme::paragraph(
            ui,
            gettext(
                locale,
                "Keep it to have it transcribed and summarized, or discard it if you don't need it.",
            ),
            theme::regular(13.5),
            palette.text,
        );
        return;
    }
    phase(app, ui, call, actions);
    ui.add_space(2.0);
    Frame::new().show(ui, |ui| call_details(ui, &palette, locale, call, None));
}

/// The project combo: the chat's projects first, then the rest, then none.
fn project_combo(
    app: &App,
    ui: &mut egui::Ui,
    call: &Call,
    sheet: &Sheet,
    actions: &mut Vec<Action>,
) {
    let locale = app.locale;
    let none = gettext(locale, "No project").into_owned();
    let current = call
        .project
        .as_ref()
        .and_then(|project| sheet.projects.iter().find(|(id, _)| id == project))
        .map_or(none.clone(), |(_, name)| name.clone());
    let mut picked: Option<Option<String>> = None;
    let response = egui::ComboBox::from_id_salt(sheet_id().with("project"))
        .selected_text(current)
        .width(220.0_f32.min(ui.available_width()))
        .show_ui(ui, |ui| {
            for (index, (id, name)) in sheet.projects.iter().enumerate() {
                if index == sheet.chat_projects && index > 0 {
                    ui.separator();
                }
                let selected = call.project.as_deref() == Some(id.as_str());
                if ui.selectable_label(selected, name).clicked() && !selected {
                    picked = Some(Some(id.clone()));
                }
            }
            if !sheet.projects.is_empty() {
                ui.separator();
            }
            if ui.selectable_label(call.project.is_none(), &none).clicked()
                && call.project.is_some()
            {
                picked = Some(None);
            }
        });
    theme::reveal_focus(&response.response);
    if let Some(project) = picked {
        actions.push(Action::MoveRecordedCall {
            id: call.id.clone(),
            project,
        });
    }
}

/// Something the sheet says about how a call was recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// The notice tone played this many seconds into the recording.
    Notice(u64),
    /// No notice is on the recording.
    NoNotice,
    /// The other side stayed silent the whole call.
    OneSide,
    /// Longer than the hour a transcript covers.
    FirstHour,
    /// Rescued from a recording a quit or a crash left behind.
    Recovered,
}

impl Mark {
    fn label(self, locale: Locale, palette: &theme::Palette) -> (String, Color32) {
        match self {
            Self::Notice(seconds) => (
                gettext(locale, "Notice: tone at {time}")
                    .replace("{time}", &crate::util::call_duration(seconds)),
                palette.secondary,
            ),
            Self::NoNotice => (
                gettext(locale, "No notice recorded").into_owned(),
                palette.warning,
            ),
            Self::OneSide => (
                gettext(locale, "Only one side is heard").into_owned(),
                palette.warning,
            ),
            Self::FirstHour => (
                gettext(locale, "Only the first hour was transcribed").into_owned(),
                palette.secondary,
            ),
            Self::Recovered => (
                gettext(locale, "Recovered after the app closed").into_owned(),
                palette.secondary,
            ),
        }
    }
}

/// How loud the other side has to have been, on average, to count as heard.
const HEARD: f32 = 0.002;

/// What the sheet says about how `call` was recorded. Only a call recorded
/// in Whats Pro knows about its notice and its two sides.
pub fn marks(call: &Call) -> Vec<Mark> {
    let mut marks = Vec::new();
    if call.stereo {
        marks.push(match call.notice_ms {
            Some(ms) => Mark::Notice(ms / 1000),
            None => Mark::NoNotice,
        });
        if call.levels.is_some_and(|levels| levels[1] < HEARD) {
            marks.push(Mark::OneSide);
        }
    }
    if call.seconds > 3600 && call.transcript().is_some() {
        marks.push(Mark::FirstHour);
    }
    if call.recovered {
        marks.push(Mark::Recovered);
    }
    marks
}

/// What reading the call is doing, or a way to have it read.
fn phase(app: &App, ui: &mut egui::Ui, call: &Call, actions: &mut Vec<Action>) {
    let palette = app.palette;
    let locale = app.locale;
    let busy = |ui: &mut egui::Ui, text: &str| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.add(egui::Spinner::new().size(15.0).color(palette.accent));
            theme::text(ui, text, theme::regular(13.5), palette.secondary);
        });
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(500));
    };
    let offer = |ui: &mut egui::Ui, text: &str, color: Color32, button: &str| {
        let mut clicked = false;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            theme::paragraph(ui, text, theme::regular(13.5), color);
            clicked = theme::pill_button(ui, &palette, button, false).clicked();
        });
        clicked
    };
    let again = Action::TranscribeRecordedCall(call.id.clone());
    match crate::call_capture::job(&call.id) {
        Some(JobPhase::Reading) => {
            let engine = gettext(locale, app.settings.media_engines.speech.label());
            busy(
                ui,
                &gettext(locale, "Transcribing with {engine}…").replace("{engine}", &engine),
            );
        }
        Some(JobPhase::Summarizing) => busy(ui, &gettext(locale, "Summarizing…")),
        Some(JobPhase::Failed(reason)) => {
            let text =
                gettext(locale, "Could not read the call: {reason}").replace("{reason}", &reason);
            if offer(ui, &text, palette.danger, &gettext(locale, "Retry")) {
                actions.push(again);
            }
        }
        _ if call.pending_read => busy(ui, &gettext(locale, "Waiting to be read…")),
        _ if call.transcript().is_none() => {
            let text = gettext(locale, "Not transcribed");
            if offer(ui, &text, palette.dim, &gettext(locale, "Transcribe")) {
                actions.push(again);
            }
        }
        _ if call.analysis.is_none() => {
            let text = gettext(locale, "No summary yet");
            if offer(ui, &text, palette.dim, &gettext(locale, "Summarize")) {
                actions.push(again);
            }
        }
        _ => {}
    }
}

/// The sheet's buttons: keep or discard a recording not reviewed yet;
/// otherwise play, show, copy, send its recap, or delete it. Each one that
/// cannot be undone asks in place first.
fn footer(app: &App, ui: &mut egui::Ui, call: &Call, sheet: &mut Sheet, actions: &mut Vec<Action>) {
    let palette = app.palette;
    let locale = app.locale;
    if let Some(confirm) = sheet.confirm {
        let (question, yes, no) = match confirm {
            Confirm::Discard => (
                gettext(
                    locale,
                    "Discard the recording? The audio is deleted and nothing is kept.",
                ),
                gettext(locale, "Discard"),
                gettext(locale, "Back"),
            ),
            Confirm::Delete => (
                gettext(
                    locale,
                    "Delete the call? Its audio, transcript and summary are deleted.",
                ),
                gettext(locale, "Delete"),
                gettext(locale, "Cancel"),
            ),
        };
        theme::paragraph(ui, question, theme::medium(13.5), palette.text);
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if danger_pill(ui, &palette, &yes).clicked() {
                    actions.push(Action::DeleteRecordedCall(call.id.clone()));
                    sheet.confirm = None;
                }
                if theme::pill_button(ui, &palette, &no, false).clicked() {
                    sheet.confirm = None;
                }
            });
        });
        return;
    }
    let path = call.path(&app.dirs.state);
    if call.pending_review {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
            if theme::pill_button(ui, &palette, &gettext(locale, "Play"), false).clicked() {
                crate::ui::projects::reveal(&path, false);
            }
            if theme::pill_button(ui, &palette, &gettext(locale, "Show in Finder"), false).clicked()
            {
                crate::ui::projects::reveal(&path, true);
            }
        });
        ui.horizontal_wrapped(|ui| {
            let save = gettext(locale, "Save and transcribe");
            if theme::pill_button(ui, &palette, &save, true).clicked() {
                actions.push(Action::TranscribeRecordedCall(call.id.clone()));
            }
            if theme::pill_button(ui, &palette, &gettext(locale, "Discard"), false).clicked() {
                sheet.confirm = Some(Confirm::Discard);
            }
        });
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
        if theme::pill_button(ui, &palette, &gettext(locale, "Play"), false).clicked() {
            crate::ui::projects::reveal(&path, false);
        }
        if theme::pill_button(ui, &palette, &gettext(locale, "Show in Finder"), false).clicked() {
            crate::ui::projects::reveal(&path, true);
        }
        if theme::pill_button(ui, &palette, &gettext(locale, "Copy"), false)
            .on_hover_text(gettext(locale, "Copy summary and transcript"))
            .clicked()
        {
            actions.push(Action::CopyText(call.markdown()));
        }
        let (enabled, why) = recap_state(app, call);
        let send = ui
            .add_enabled_ui(enabled, |ui| {
                theme::pill_button(ui, &palette, &gettext(locale, "Send summary to chat"), true)
            })
            .inner;
        let send = if enabled {
            send.on_hover_text(gettext(
                locale,
                "Puts a short recap in the chat's message box. You choose whether to send it.",
            ))
        } else {
            send.on_disabled_hover_text(why)
        };
        if send.clicked() {
            actions.push(Action::DraftCallRecap(call.id.clone()));
        }
    });
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if theme::link(
                ui,
                gettext(locale, "Delete recording…"),
                theme::regular(13.0),
                palette.danger,
            )
            .clicked()
            {
                sheet.confirm = Some(Confirm::Delete);
            }
        });
    });
}

/// Whether the call's recap can go to its chat now, and why not.
fn recap_state(app: &App, call: &Call) -> (bool, String) {
    let locale = app.locale;
    let why = if call.chat.is_none() {
        gettext(locale, "This call has no chat")
    } else if call.analysis.is_none() {
        gettext(locale, "Available once the call has its summary")
    } else if call.account.as_deref() != Some(app.account().id.as_str()) {
        gettext(
            locale,
            "Switch to the account that had the call to send its summary",
        )
    } else {
        return (true, String::new());
    };
    (false, why.into_owned())
}

/// A filled red button for the question's yes.
fn danger_pill(ui: &mut egui::Ui, palette: &theme::Palette, label: &str) -> egui::Response {
    let galley =
        ui.painter()
            .layout_no_wrap(label.to_owned(), theme::semibold(13.0), Color32::WHITE);
    let size = galley.size() + vec2(36.0, 16.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    theme::reveal_focus(&response);
    theme::focus_outline(ui, response.id, rect, rect.height() / 2.0);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    if ui.is_rect_visible(rect) {
        let fill = if response.hovered() {
            palette.danger.gamma_multiply(0.85)
        } else {
            palette.danger
        };
        ui.painter().rect_filled(rect, rect.height() / 2.0, fill);
        ui.painter()
            .galley(rect.center() - galley.size() / 2.0, galley, Color32::WHITE);
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A short recap of a read call in WhatsApp's own markup, for its chat: the
/// summary, what was agreed and what comes next. Tasks stay out: they are
/// the person's own notes.
pub fn recap(locale: Locale, analysis: &CallAnalysis) -> String {
    let mut parts = Vec::new();
    let summary = analysis.summary.trim();
    if !summary.is_empty() {
        parts.push(summary.to_owned());
    }
    let section = |title: &str, items: Vec<String>| -> Option<String> {
        let items: Vec<String> = items
            .into_iter()
            .map(|item| item.trim().to_owned())
            .filter(|item| !item.is_empty())
            .collect();
        if items.is_empty() {
            return None;
        }
        let mut text = format!("*{title}*");
        for item in items {
            text.push_str("\n• ");
            text.push_str(&item);
        }
        Some(text)
    };
    parts.extend(section(
        &gettext(locale, "We agreed:"),
        analysis.commitments.iter().map(commitment).collect(),
    ));
    parts.extend(section(
        &gettext(locale, "Next steps:"),
        analysis.next_steps.clone(),
    ));
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call_library::Commitment;

    fn call() -> Call {
        Call {
            id: "a".into(),
            title: "Call with Ada".into(),
            at: 0,
            seconds: 1_380,
            origin: crate::call_library::Origin::Recorded,
            file: "a.wav".into(),
            project: None,
            lead: None,
            transcripts: Vec::new(),
            analysis: None,
            chat: Some("1@s.whatsapp.net".into()),
            account: Some("1".into()),
            started_at: 0,
            video: false,
            direction: None,
            group: None,
            participants: Vec::new(),
            private: false,
            notice_ms: Some(1_200),
            levels: Some([0.1, 0.08]),
            stereo: true,
            recovered: false,
            pending_read: false,
            pending_review: false,
        }
    }

    #[test]
    fn the_recap_is_the_summary_what_was_agreed_and_what_comes_next() {
        let analysis = CallAnalysis {
            summary: "  We went over the quote for the 15 garments. ".into(),
            decisions: vec!["Blue thread".into()],
            commitments: vec![
                Commitment {
                    who: "Ada".into(),
                    what: "send the logo".into(),
                    due: Some("Friday".into()),
                },
                Commitment {
                    who: String::new(),
                    what: "confirm the price".into(),
                    due: None,
                },
            ],
            tasks: vec!["Buy thread".into()],
            next_steps: vec!["Sample on Monday".into(), " ".into()],
            at: 0,
        };
        assert_eq!(
            recap(Locale::default(), &analysis),
            "We went over the quote for the 15 garments.\n\n\
             *We agreed:*\n• Ada: send the logo (Friday)\n• confirm the price\n\n\
             *Next steps:*\n• Sample on Monday"
        );
        let bare = CallAnalysis {
            summary: "Only a summary.".into(),
            ..CallAnalysis::default()
        };
        assert_eq!(recap(Locale::default(), &bare), "Only a summary.");
    }

    #[test]
    fn the_sheet_says_how_a_call_was_recorded() {
        let mut heard = call();
        assert_eq!(marks(&heard), [Mark::Notice(1)]);
        heard.notice_ms = None;
        heard.levels = Some([0.1, 0.0]);
        heard.seconds = 4_000;
        heard.recovered = true;
        heard.transcripts.push(crate::call_library::Transcript {
            engine: "local".into(),
            text: "Hola".into(),
            at: 0,
        });
        assert_eq!(
            marks(&heard),
            [
                Mark::NoNotice,
                Mark::OneSide,
                Mark::FirstHour,
                Mark::Recovered
            ]
        );
        // A recording of the computer's sound knows nothing of its sides.
        let mut plain = call();
        plain.stereo = false;
        assert!(marks(&plain).is_empty());
        assert_eq!(
            subtitle(Locale::default(), &plain).rsplit(" · ").next(),
            Some("23 min")
        );
    }
}
