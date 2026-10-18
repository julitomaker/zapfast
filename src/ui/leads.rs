//! A project's Leads tab, and the sales cards on its panel.
//!
//! The tab lists the project's leads by what needs doing first (late, for
//! today, with nothing planned, coming up), with customers and lost sales
//! folded at the end, and the stages as filters on top. Beside the list,
//! the picked lead: its stage as steps, its next action with quick days,
//! its history with a box to note a call, a follow-up or a note (typed or
//! said), a summary of its chat written by the analysis, and its link to
//! ElevaLeads.
//!
//! On the panel, a Sales card (what is late, for today, quoted) and, for a
//! project connected to an ElevaLeads brand, the brand's WhatsApp inbox:
//! who is waiting for a person, the alerts to attend and the agenda.
//!
//! The leads are drawn from a copy; what the user does is collected as
//! `Change`s and applied to the shared store after the frame, so the thread
//! that talks to ElevaLeads never waits for the interface.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use egui::{Align, Color32, CornerRadius, Frame, Layout, Margin, Sense, Vec2, vec2};

use crate::app::App;
use crate::eleva;
use crate::i18n::{Locale, gettext};
use crate::leads::{
    self, Brand, ElevaCustomer, Kind, LOST_REASONS, Lead, Leads, Settings, Stage, Store, Sync,
    Urgency,
};
use crate::model::{Action, Page};
use crate::projects::Project;
use crate::theme::{self, Icon};
use crate::ui::widgets;

/// List and lead side by side from this width on.
const SIDE_BY_SIDE: f32 = 820.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Adding {
    Chats,
    Numbers,
}

struct State {
    project: String,
    selected: Option<String>,
    filter: Option<Stage>,
    search: String,
    adding: Option<Adding>,
    add_search: String,
    numbers: String,
    /// The history box, its kind, and a spoken note being recorded or read.
    composer: String,
    kind: Kind,
    recorder: Option<crate::audio::Recorder>,
    /// A voice note just recorded, waiting to be transcribed or discarded.
    take: Option<super::take::Take>,
    /// A spoken note's text, with the lead it was said for.
    spoken: Arc<Mutex<Option<Spoken>>>,
    recording_for: Option<String>,
    /// Notes being written for leads not on screen, kept until they come
    /// back: the text and its kind.
    drafts: std::collections::HashMap<String, (String, Kind)>,
    /// The next action's fields are read again from the lead.
    reload_next: bool,
    transcribing: bool,
    /// The next action being edited, and for which lead.
    next_text: String,
    next_due: Option<String>,
    next_for: Option<String>,
    value: String,
    /// The ElevaLeads connection being set up.
    connecting: bool,
    token: String,
    connect_error: Option<String>,
    /// Leads whose customer was looked for and not found, or failed.
    not_found: HashSet<String>,
    find_error: Option<(String, String)>,
    /// A chat summary being written, for which lead, and its answer.
    summarizing: Option<String>,
    summary: Arc<Mutex<Option<Summary>>>,
    template: String,
    show_customers: bool,
    show_lost: bool,
    last_push: Option<Instant>,
    notice: Option<(String, Instant)>,
    /// The project's sales settings being edited: buying cycle, cadence and
    /// daily cap, as typed.
    settings_draft: Option<(String, String, String)>,
    /// The analysis' buyers for the project, read now and then.
    hints: Option<(String, Instant, Vec<crate::projects::LeadHint>)>,
    show_hints: bool,
    /// Whether each call in the library has been summarized, read from it
    /// when a call entry is drawn and again once a call is noted.
    summarized: std::collections::HashMap<String, bool>,
    /// The demo's view of a call just noted, since when: the history in
    /// sight for a moment.
    #[cfg(any(test, feature = "demo"))]
    demo_view: Option<Instant>,
}

/// Recording a call or reading one into a lead's history, for the whole
/// app: it outlives the window (closing it keeps Whats Pro running) and the
/// project on screen, so a call can always be ended. The thread that reads
/// a call saves it to the lead itself.
struct CallJob {
    /// The lead it is for, its name and its project.
    lead: String,
    name: String,
    project: String,
    phase: CallPhase,
    stop: Option<std::sync::mpsc::Sender<()>>,
    /// The lead a call was just noted for: the page says so.
    noted: Option<String>,
    /// What a call said to do next: the lead, the action and its day.
    suggestion: Option<(String, String, Option<String>)>,
}

#[derive(Clone)]
enum CallPhase {
    Idle,
    /// The file picker is open.
    Picking,
    Starting,
    /// Since when, and whether only the microphone is heard.
    Recording(Instant, bool),
    /// Ended, waiting for the user to keep it or throw it away.
    Review(super::take::Take),
    Transcribing,
    Summarizing,
    Failed(String),
}

impl CallPhase {
    fn busy(&self) -> bool {
        !matches!(self, CallPhase::Idle | CallPhase::Failed(_))
    }
}

static CALL: Mutex<CallJob> = Mutex::new(CallJob {
    lead: String::new(),
    name: String::new(),
    project: String::new(),
    phase: CallPhase::Idle,
    stop: None,
    noted: None,
    suggestion: None,
});

/// The call job. Never held while calling something that takes it.
fn job() -> std::sync::MutexGuard<'static, CallJob> {
    CALL.lock().unwrap_or_else(|p| p.into_inner())
}

impl Default for State {
    fn default() -> Self {
        Self {
            project: String::new(),
            selected: None,
            filter: None,
            search: String::new(),
            adding: None,
            add_search: String::new(),
            numbers: String::new(),
            composer: String::new(),
            kind: Kind::Note,
            recorder: None,
            take: None,
            spoken: Arc::default(),
            recording_for: None,
            drafts: std::collections::HashMap::new(),
            reload_next: false,
            transcribing: false,
            next_text: String::new(),
            next_due: None,
            next_for: None,
            value: String::new(),
            connecting: false,
            token: String::new(),
            connect_error: None,
            not_found: HashSet::new(),
            find_error: None,
            summarizing: None,
            summary: Arc::default(),
            template: String::new(),
            show_customers: false,
            show_lost: false,
            last_push: None,
            notice: None,
            settings_draft: None,
            hints: None,
            show_hints: false,
            summarized: std::collections::HashMap::new(),
            #[cfg(any(test, feature = "demo"))]
            demo_view: None,
        }
    }
}

type Shared = Arc<Mutex<State>>;

/// A spoken note's text, with the lead it was said for.
type Spoken = (String, Result<String, String>);

/// A chat summary's answer, with the lead it is for.
type Summary = (String, Result<String, String>);

fn state(ctx: &egui::Context) -> Shared {
    ctx.data_mut(|data| {
        data.get_temp_mut_or_insert_with(egui::Id::new("leads-state"), Shared::default)
            .clone()
    })
}

/// What the user did, applied to the store after the frame.
enum Change {
    Add(Box<Lead>),
    Remove(String),
    Stage(String, Stage, Option<String>),
    Next(String, String, Option<String>),
    /// The next action was done: noted, and cleared.
    Done(String),
    Postpone(String, i64),
    Entry(String, Kind, String),
    Value(String, String),
    Link(String, ElevaCustomer),
    Unlink(String),
    Retry(String),
    Connect(Brand),
    Disconnect,
    AddTemplate(String),
    RemoveTemplate(usize),
    Settings(Settings),
    AddHint(Box<crate::projects::LeadHint>),
    DismissHint(String),
}

fn apply(leads: &mut Leads, project: &str, change: Change) -> Option<String> {
    match change {
        Change::Add(lead) => return Some(leads.add(*lead)),
        Change::Remove(id) => leads.remove(&id),
        Change::Stage(id, stage, reason) => {
            if let Some(lead) = leads.get_mut(&id) {
                lead.set_stage(stage, reason.as_deref());
            }
        }
        Change::Next(id, text, due) => {
            if let Some(lead) = leads.get_mut(&id) {
                lead.set_next(&text, due);
            }
        }
        Change::Done(id) => {
            if let Some(lead) = leads.get_mut(&id)
                && lead.finish_next().is_some()
                // The first step done with someone new is the first contact.
                && lead.stage == Stage::New
            {
                lead.set_stage(Stage::Contacted, None);
            }
        }
        Change::Postpone(id, days) => {
            if let Some(lead) = leads.get_mut(&id) {
                lead.postpone(days);
            }
        }
        Change::Entry(id, kind, text) => {
            if let Some(lead) = leads.get_mut(&id)
                && !text.trim().is_empty()
            {
                lead.add(kind, &text);
            }
        }
        Change::Value(id, value) => {
            if let Some(lead) = leads.get_mut(&id) {
                let value = value.trim();
                lead.value = (!value.is_empty()).then(|| value.to_owned());
            }
        }
        Change::Link(id, customer) => {
            if let Some(lead) = leads.get_mut(&id) {
                lead.link(customer);
            }
        }
        Change::Unlink(id) => leads.unlink(&id),
        Change::Retry(id) => {
            if let Some(lead) = leads.get_mut(&id) {
                lead.retry();
            }
        }

        Change::Connect(brand) => {
            let moved = leads
                .brands
                .get(project)
                .is_some_and(|old| old.slug != brand.slug);
            if moved {
                leads.disconnect(project);
            }
            leads.brands.insert(project.to_owned(), brand);
        }
        Change::Disconnect => leads.disconnect(project),
        Change::AddTemplate(text) => {
            let text = text.trim();
            if !text.is_empty() {
                leads
                    .templates
                    .entry(project.to_owned())
                    .or_default()
                    .push(text.to_owned());
            }
        }
        Change::AddHint(hint) => {
            return Some(leads.add(Leads::from_hint(project, &hint)));
        }
        Change::DismissHint(id) => {
            let list = leads.dismissed.entry(project.to_owned()).or_default();
            if !list.contains(&id) {
                list.push(id);
            }
        }
        Change::Settings(settings) => {
            leads.settings.insert(project.to_owned(), settings);
        }
        Change::RemoveTemplate(index) => {
            if let Some(list) = leads.templates.get_mut(project)
                && index < list.len()
            {
                list.remove(index);
            }
        }
    }
    None
}

/// Applies the changes, saves, and sends what waits for ElevaLeads.
fn commit(app: &App, ui: &egui::Ui, state: &mut State, project: &str, changes: Vec<Change>) {
    if changes.is_empty() {
        return;
    }
    let mut store = Store::open(&app.dirs.config);
    for change in changes {
        if let Change::Remove(id) = &change {
            // A call being recorded with it ends; it stays the project's.
            let recording = {
                let held = job();
                matches!(held.phase, CallPhase::Recording(..)) && held.lead == *id
            };
            if recording {
                end_call();
            }
            forget_calls(&app.dirs.state, id);
        }
        if let Some(id) = apply(&mut store, project, change) {
            state.selected = Some(id);
        }
    }
    store.save();
    drop(store);
    eleva::push(ui.ctx(), &app.dirs.config);
    state.last_push = Some(Instant::now());
}

// ------------------------------------------------------------ small parts

fn card_fill(palette: &theme::Palette) -> Color32 {
    if palette.vibrant() {
        palette.tint(theme::TINT_FIELD)
    } else {
        palette.surface
    }
}

pub(super) fn card_frame(palette: &theme::Palette) -> Frame {
    Frame::new()
        .fill(card_fill(palette))
        .stroke(palette.glass_edge())
        .corner_radius(CornerRadius::same(theme::RADIUS + 6))
        .inner_margin(Margin::same(14))
}

pub(super) fn field_frame(palette: &theme::Palette) -> Frame {
    Frame::new()
        .fill(palette.surface)
        .stroke(palette.glass_edge())
        .corner_radius(CornerRadius::same(theme::RADIUS + 2))
        .inner_margin(Margin::symmetric(10, 7))
}

pub(super) fn wrapped(ui: &mut egui::Ui, text: &str, font: egui::FontId, color: Color32) {
    ui.add(
        egui::Label::new(egui::RichText::new(text).font(font).color(color))
            .wrap()
            .selectable(false),
    );
}

/// A small rounded label.
pub(super) fn chip(ui: &mut egui::Ui, text: &str, color: Color32) {
    Frame::new()
        .fill(color.gamma_multiply(0.14))
        .corner_radius(CornerRadius::same(theme::RADIUS))
        .inner_margin(Margin::symmetric(7, 2))
        .show(ui, |ui| {
            theme::text(ui, text, theme::medium(11.5), color);
        });
}

pub(super) fn button(
    ui: &mut egui::Ui,
    palette: &theme::Palette,
    label: &str,
    primary: bool,
) -> egui::Response {
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), theme::semibold(13.0), palette.text);
    let width = (galley.size().x + 24.0).max(64.0);
    let (rect, response) = ui.allocate_exact_size(vec2(width, 30.0), Sense::click());
    theme::reveal_focus(&response);
    let hovered = response.hovered();
    let (fill, text) = match (primary, hovered) {
        (true, false) => (palette.accent, palette.on_accent),
        (true, true) => (palette.accent_hover, palette.on_accent),
        (false, false) => (card_fill(palette), palette.text),
        (false, true) => (palette.surface_hover, palette.text),
    };
    ui.painter().rect_filled(rect, 15.0, fill);
    if !primary {
        ui.painter()
            .rect_stroke(rect, 15.0, palette.glass_edge(), egui::StrokeKind::Inside);
    }
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), theme::semibold(13.0), text);
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, text);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn stage_color(palette: &theme::Palette, stage: Stage) -> Color32 {
    match stage {
        Stage::New => palette.link,
        Stage::Contacted => Color32::from_rgb(0xa7, 0x8b, 0xfa),
        Stage::Quoted => palette.warning,
        Stage::Customer => palette.accent,
        Stage::Lost => palette.dim,
    }
}

fn stage_label(locale: Locale, stage: Stage) -> String {
    match stage {
        Stage::New => gettext(locale, "New"),
        Stage::Contacted => gettext(locale, "Contacted"),
        Stage::Quoted => gettext(locale, "Quoted"),
        Stage::Customer => gettext(locale, "Customer"),
        Stage::Lost => gettext(locale, "Lost"),
    }
    .into_owned()
}

fn reason_label(locale: Locale, reason: &str) -> String {
    match reason {
        "Price" => gettext(locale, "Price"),
        "Chose someone else" => gettext(locale, "Chose someone else"),
        "No answer" => gettext(locale, "No answer"),
        "Later on" => gettext(locale, "Later on"),
        _ => gettext(locale, "Other"),
    }
    .into_owned()
}

fn urgency_label(locale: Locale, urgency: Urgency) -> String {
    match urgency {
        Urgency::Reply => gettext(locale, "Your turn to answer"),
        Urgency::Overdue => gettext(locale, "Late"),
        Urgency::Today => gettext(locale, "For today"),
        Urgency::Collect => gettext(locale, "To collect"),
        Urgency::Rebuy => gettext(locale, "To sell again"),
        Urgency::NoAction => gettext(locale, "Nothing planned"),
        Urgency::Upcoming => gettext(locale, "Coming up"),
        Urgency::Customers => gettext(locale, "Customers"),
        Urgency::Lost => gettext(locale, "Lost sales"),
    }
    .into_owned()
}

/// What became of the brand's automatic message after a sale, as
/// ElevaLeads names it.
pub(super) fn notice_text(locale: Locale, notice: &str) -> String {
    match notice {
        "enviado" | "sent" => gettext(locale, "sent"),
        "fuera_ventana" => gettext(locale, "not sent: outside the 24-hour window"),
        "baja" | "opt_out" => gettext(locale, "not sent: they unsubscribed"),
        "sin_mensaje" | "omitido" | "skipped" => gettext(locale, "none"),
        other => return other.replace('_', " "),
    }
    .into_owned()
}

/// Whats Pro's own errors in the user's language; ElevaLeads already
/// answers in Spanish.
pub(super) fn error_text(locale: Locale, error: &str) -> String {
    match error {
        "An ElevaLeads token starts with elmcp_" => {
            gettext(locale, "An ElevaLeads token starts with elmcp_")
        }
        "This token opens no brand" => gettext(locale, "This token opens no brand"),
        "This token opens every brand of the account: make one limited to a single brand" => {
            gettext(
                locale,
                "This token opens every brand of the account: make one limited to a single brand",
            )
        }
        "The brand's token is not in this computer's keyring" | "The brand's token is missing" => {
            gettext(
                locale,
                "The brand's token is not in this computer's keyring: connect the brand again",
            )
        }
        "ElevaLeads token is read-only" => gettext(locale, "The ElevaLeads token is read-only"),
        "An ElevaLeads API key starts with eleva_" => {
            gettext(locale, "An ElevaLeads API key starts with eleva_")
        }
        "The brand's API key is missing" => gettext(locale, "The brand's API key is missing"),
        "ElevaLeads already has them as a customer" => gettext(
            locale,
            "ElevaLeads already has them as a customer: the stage was not moved back",
        ),
        "ElevaLeads refused the call" | "ElevaLeads refused the sale" => {
            gettext(locale, "ElevaLeads refused it")
        }
        "no messages in the last 30 days" => {
            gettext(locale, "the chat has no messages in the last 30 days")
        }
        "the model answered nothing" => gettext(locale, "the analysis answered nothing"),
        _ if error.starts_with("already:") => {
            return gettext(
                locale,
                "That sale was already in ElevaLeads as order {code}: nothing new was made.",
            )
            .replace("{code}", &error["already:".len()..]);
        }
        _ if error.starts_with("ElevaLeads wrote it on another customer: ") => {
            return gettext(
                locale,
                "ElevaLeads put it on another customer's file: {name}. Check the link.",
            )
            .replace(
                "{name}",
                &error["ElevaLeads wrote it on another customer: ".len()..],
            );
        }
        _ if error.starts_with("ElevaLeads answered ") => {
            return gettext(
                locale,
                "ElevaLeads did not answer properly ({status}); it will be tried again.",
            )
            .replace("{status}", &error["ElevaLeads answered ".len()..]);
        }
        _ if error.contains("error sending request") || error.contains("dns error") => gettext(
            locale,
            "ElevaLeads could not be reached: check the connection",
        ),
        _ => return error.to_owned(),
    }
    .into_owned()
}

fn kind_label(locale: Locale, kind: Kind) -> String {
    match kind {
        Kind::Note => gettext(locale, "Note"),
        Kind::Call => gettext(locale, "Call"),
        Kind::FollowUp => gettext(locale, "Follow-up"),
        Kind::Summary => gettext(locale, "Chat summary"),
        Kind::Stage => gettext(locale, "Stage"),
        Kind::Task => gettext(locale, "Next action"),
    }
    .into_owned()
}

fn kind_icon(kind: Kind) -> Icon {
    match kind {
        Kind::Note => Icon::Pencil,
        Kind::Call => Icon::Phone,
        Kind::FollowUp => Icon::CircleCheck,
        Kind::Summary => Icon::FileText,
        Kind::Stage => Icon::Tag,
        Kind::Task => Icon::Calendar,
    }
}

/// When a next action is due, said the short way, and its colour.
fn due_label(app: &App, due: &str) -> (String, Color32) {
    let palette = app.palette;
    let locale = app.locale;
    let today = leads::today();
    let tomorrow = leads::in_days(1);
    if due == today {
        return (gettext(locale, "today").into_owned(), palette.warning);
    }
    if due == tomorrow {
        return (gettext(locale, "tomorrow").into_owned(), palette.secondary);
    }
    let parsed = due.parse::<jiff::civil::Date>().ok();
    let short = parsed
        .map(|date| format!("{:02}/{:02}", date.day(), date.month()))
        .unwrap_or_else(|| due.to_owned());
    if due < today.as_str() {
        let days = parsed
            .and_then(|date| crate::util::today().since(date).ok())
            .map(|span| span.get_days())
            .unwrap_or(0);
        return (
            gettext(locale, "{n} d late").replace("{n}", &days.max(1).to_string()),
            palette.danger,
        );
    }
    (short, palette.secondary)
}

fn ago(locale: Locale, seconds: i64) -> String {
    let delta = (jiff::Timestamp::now().as_second() - seconds).max(0);
    if delta < 3600 {
        gettext(locale, "{n} min").replace("{n}", &(delta / 60).max(1).to_string())
    } else if delta < 86_400 {
        gettext(locale, "{n} h").replace("{n}", &(delta / 3600).to_string())
    } else {
        gettext(locale, "{n} d").replace("{n}", &(delta / 86_400).to_string())
    }
}

/// The text of an entry as shown: a stage change by the stage's name.
fn entry_text(locale: Locale, kind: Kind, text: &str) -> String {
    if kind == Kind::Stage
        && let Ok(stage) = serde_json::from_value::<Stage>(serde_json::Value::String(text.into()))
    {
        return gettext(locale, "Moved to {stage}").replace("{stage}", &stage_label(locale, stage));
    }
    text.to_owned()
}

/// Who wrote last in a lead's chat with one person, and when.
struct Turn {
    /// They did: the next word is ours.
    theirs: bool,
    at: i64,
}

/// The lead's chat: its own, or the one its number already has.
fn chat_of<'a>(app: &'a App, lead: &Lead) -> Option<&'a crate::model::Chat> {
    let id = lead.chat.clone().or_else(|| {
        lead.phone
            .as_deref()
            .map(|phone| format!("{}@s.whatsapp.net", leads::digits(phone)))
    })?;
    app.chat(&id)
}

fn turn(app: &App, lead: &Lead) -> Option<Turn> {
    let chat = chat_of(app, lead)?;
    let last = chat.last.as_ref()?;
    let direct = crate::model::ChatKind::from_id(&chat.id) == crate::model::ChatKind::Direct;
    Some(Turn {
        theirs: direct && !last.from_me,
        at: chat.last_activity,
    })
}

/// Opens a lead's chat, starting one with its number when there is none.
fn open_chat(app: &mut App, lead: &Lead) -> Option<String> {
    let chat = lead.chat.clone().or_else(|| {
        lead.phone
            .as_deref()
            .map(|phone| format!("{}@s.whatsapp.net", leads::digits(phone)))
    })?;
    app.actions.push(Action::Open(Page::Chats));
    if app.chat(&chat).is_some() {
        app.actions.push(Action::OpenChat(chat.clone()));
    } else {
        app.actions.push(Action::StartChat {
            id: chat.clone(),
            name: lead.name.clone(),
            dismiss_dialog: false,
        });
    }
    Some(chat)
}

// --------------------------------------------------------------- the tab

/// The Leads tab of `project`.
pub fn tab(app: &mut App, ui: &mut egui::Ui, project: &Project) {
    let shared = state(ui.ctx());
    let mut state = shared.lock().unwrap_or_else(|p| p.into_inner());
    if state.project != project.id {
        *state = State {
            project: project.id.clone(),
            ..State::default()
        };
    }
    let mut changes = Vec::new();
    collect_answers(app, &mut state, project, &mut changes);
    let leads = Store::open(&app.dirs.config).clone();
    // Entries that wait are sent again now and then: the network may have
    // been down, or the token renewed.
    let waiting = leads.of_project(&project.id).any(|lead| lead.to_send());
    if waiting
        && state
            .last_push
            .is_none_or(|at| at.elapsed() > Duration::from_secs(60))
    {
        eleva::push(ui.ctx(), &app.dirs.config);
        state.last_push = Some(Instant::now());
    }
    let brand = leads.brands.get(&project.id).cloned();
    let selected = state.selected.clone().and_then(|id| {
        leads
            .get(&id)
            .filter(|lead| lead.project == project.id)
            .cloned()
    });
    if selected.is_none() {
        state.selected = None;
    }
    let wide = ui.available_width() >= SIDE_BY_SIDE;
    Frame::new()
        .inner_margin(Margin::symmetric(20, 14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            toolbar(
                app,
                ui,
                &mut state,
                &leads,
                project,
                brand.as_ref(),
                &mut changes,
            );
            results(app, ui, &mut state, &leads, project, &mut changes);
            if let Some((text, at)) = &state.notice
                && at.elapsed() < Duration::from_secs(6)
            {
                ui.add_space(6.0);
                chip(ui, text, app.palette.accent);
                ui.ctx().request_repaint_after(Duration::from_secs(1));
            }
            ui.add_space(10.0);
            if state.connecting || brand.is_none() && state.connect_error.is_some() {
                connect_card(app, ui, &mut state, project, brand.as_ref(), &mut changes);
                ui.add_space(10.0);
            }
            if let Some(adding) = state.adding {
                add_card(app, ui, &mut state, &leads, project, adding, &mut changes);
                ui.add_space(10.0);
            }
            let height = ui.available_height();
            if wide {
                let width = ui.available_width();
                let list_width = (width * 0.44).clamp(320.0, 460.0);
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(
                        vec2(list_width, height),
                        Layout::top_down(Align::Min),
                        |ui| list(app, ui, &mut state, &leads, project, &mut changes),
                    );
                    ui.add_space(14.0);
                    ui.allocate_ui_with_layout(
                        vec2(width - list_width - 14.0, height),
                        Layout::top_down(Align::Min),
                        |ui| match &selected {
                            Some(lead) => detail(
                                app,
                                ui,
                                &mut state,
                                &leads,
                                lead,
                                brand.as_ref(),
                                &mut changes,
                            ),
                            None => nothing_picked(app, ui, &leads, project),
                        },
                    );
                });
            } else if let Some(lead) = &selected {
                if theme::icon_button(
                    ui,
                    Icon::ArrowLeft,
                    16.0,
                    app.palette.secondary,
                    app.palette.text,
                    &gettext(app.locale, "Back to the leads"),
                )
                .clicked()
                {
                    state.selected = None;
                }
                detail(
                    app,
                    ui,
                    &mut state,
                    &leads,
                    lead,
                    brand.as_ref(),
                    &mut changes,
                );
            } else {
                list(app, ui, &mut state, &leads, project, &mut changes);
            }
        });
    let project_id = project.id.clone();
    commit(app, ui, &mut state, &project_id, changes);
}

/// Takes in what the threads answered: a connection, a customer looked
/// for, a summary, a spoken note.
fn collect_answers(app: &App, state: &mut State, project: &Project, changes: &mut Vec<Change>) {
    let locale = app.locale;
    if let Some(Some(answer)) = eleva::connection(&project.id) {
        match answer {
            Ok(connection) => {
                state.connecting = false;
                state.connect_error = None;
                state.token.clear();
                state.notice = Some((
                    gettext(locale, "Connected to {brand}").replace("{brand}", &connection.name),
                    Instant::now(),
                ));
                changes.push(Change::Connect(Brand {
                    slug: connection.slug,
                    name: connection.name,
                    write: connection.write,
                }));
            }
            Err(error) => state.connect_error = Some(error),
        }
    }
    if let Some(id) = state.selected.clone()
        && let Some(Some(answer)) = eleva::found(&id)
    {
        match answer {
            Ok(Some(customer)) => {
                state.not_found.remove(&id);
                changes.push(Change::Link(id, customer));
            }
            Ok(None) => {
                state.not_found.insert(id);
            }
            Err(error) => state.find_error = Some((id, error)),
        }
    }
    if let Some((id, answer)) = state
        .summary
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
    {
        state.summarizing = None;
        match answer {
            Ok(text) => changes.push(Change::Entry(id, Kind::Summary, text)),
            Err(error) => {
                state.notice = Some((
                    gettext(locale, "The chat could not be summarized: {error}")
                        .replace("{error}", &error),
                    Instant::now(),
                ))
            }
        }
    }
    let noted = job().noted.take();
    if noted.is_some() {
        state.summarized.clear();
        state.notice = Some((
            gettext(locale, "Call noted in the history").into_owned(),
            Instant::now(),
        ));
    }
    if let Some((lead, result)) = state
        .spoken
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
    {
        state.transcribing = false;
        match result {
            Ok(text) => {
                // Into the box of the lead it was said for, on screen or not.
                let current = state.next_for.as_deref() == Some(lead.as_str());
                let draft = if current {
                    &mut state.composer
                } else {
                    &mut state
                        .drafts
                        .entry(lead)
                        .or_insert((String::new(), Kind::Note))
                        .0
                };
                if !draft.trim().is_empty() {
                    draft.push(' ');
                }
                draft.push_str(text.trim());
            }
            Err(error) => {
                state.notice = Some((
                    gettext(locale, "The spoken note could not be transcribed: {error}")
                        .replace("{error}", &error),
                    Instant::now(),
                ));
            }
        }
    }
}

/// Stage filters with their counts, a search, adding leads, and the
/// ElevaLeads connection.
fn toolbar(
    app: &App,
    ui: &mut egui::Ui,
    state: &mut State,
    leads: &Leads,
    project: &Project,
    brand: Option<&Brand>,
    changes: &mut Vec<Change>,
) {
    let palette = app.palette;
    let locale = app.locale;
    let counts = leads.counts(&project.id);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(4.0, 6.0);
        let total: usize = counts.values().sum();
        if widgets::filter_chip(
            ui,
            &palette,
            gettext(locale, "All").as_ref(),
            total,
            state.filter.is_none(),
        )
        .clicked()
        {
            state.filter = None;
        }
        for stage in Stage::ALL {
            let count = counts.get(&stage).copied().unwrap_or(0);
            let label = stage_label(locale, stage);
            if widgets::dotted_chip(
                ui,
                &palette,
                Some(stage_color(&palette, stage)),
                &label,
                count,
                state.filter == Some(stage),
            )
            .clicked()
            {
                state.filter = (state.filter != Some(stage)).then_some(stage);
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let add = theme::icon_button(
                ui,
                Icon::Plus,
                18.0,
                palette.accent,
                palette.accent_hover,
                &gettext(locale, "New lead"),
            );
            let from_chat = gettext(locale, "From a chat");
            let by_number = gettext(locale, "By number");
            egui::Popup::menu(&add)
                .width(widgets::menu_width(
                    ui,
                    &[from_chat.as_ref(), by_number.as_ref()],
                    true,
                ))
                .frame(widgets::menu_frame(&palette))
                .show(|ui| {
                    if widgets::menu_item(ui, &palette, Some(Icon::MessageCircle), &from_chat) {
                        state.adding = Some(Adding::Chats);
                        ui.close();
                    }
                    if widgets::menu_item(ui, &palette, Some(Icon::PhonePlus), &by_number) {
                        state.adding = Some(Adding::Numbers);
                        ui.close();
                    }
                });
            // The brand, or a way to connect one.
            let (label, color) = match brand {
                Some(brand) => (format!("ElevaLeads · {}", brand.name), palette.accent),
                None => (
                    gettext(locale, "Connect ElevaLeads").into_owned(),
                    palette.secondary,
                ),
            };
            let response =
                widgets::dotted_chip(ui, &palette, Some(color), &label, 0, state.connecting);
            if brand.is_none() && response.clicked() {
                state.connecting = !state.connecting;
            }
            if let Some(brand) = brand {
                let mode = if brand.write {
                    gettext(locale, "Read and write")
                } else {
                    gettext(locale, "Read only")
                };
                let change = gettext(locale, "Change token");
                let open = gettext(locale, "Open the inbox in ElevaLeads");
                let disconnect = gettext(locale, "Disconnect");
                egui::Popup::menu(&response)
                    .width(widgets::menu_width(
                        ui,
                        &[
                            change.as_ref(),
                            open.as_ref(),
                            disconnect.as_ref(),
                            mode.as_ref(),
                        ],
                        true,
                    ))
                    .frame(widgets::menu_frame(&palette))
                    .show(|ui| {
                        theme::text(ui, mode.as_ref(), theme::regular(12.0), palette.dim);
                        widgets::menu_separator(ui, &palette);
                        if widgets::menu_item(ui, &palette, Some(Icon::Settings), &change) {
                            state.connecting = true;
                            ui.close();
                        }
                        if widgets::menu_item(ui, &palette, Some(Icon::ExternalLink), &open) {
                            ui.ctx().open_url(egui::OpenUrl::new_tab(eleva::inbox_url(
                                &brand.slug,
                                None,
                            )));
                            ui.close();
                        }
                        if widgets::menu_item(ui, &palette, Some(Icon::LogOut), &disconnect) {
                            changes.push(Change::Disconnect);
                            ui.close();
                        }
                    });
            }
            let width = (ui.available_width() - 8.0).clamp(140.0, 240.0);
            widgets::search_field(
                ui,
                &palette,
                egui::Id::new(("leads-search", &project.id)),
                &mut state.search,
                gettext(locale, "Search leads").as_ref(),
                width,
            );
        });
    });
}

/// The month in one line: leads in, first contacts today against the safe
/// cap, won, worth, conversion and why sales were lost; and the project's
/// sales settings behind a button at its end.
fn results(
    app: &App,
    ui: &mut egui::Ui,
    state: &mut State,
    leads: &Leads,
    project: &Project,
    changes: &mut Vec<Change>,
) {
    let palette = app.palette;
    let locale = app.locale;
    if leads.of_project(&project.id).next().is_none() {
        return;
    }
    let month = leads.month(&project.id, leads::month_start());
    let settings = leads.settings_of(&project.id);
    let contacts = leads.first_contacts(&project.id, leads::today_start());
    ui.add_space(8.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(14.0, 4.0);
        let stat = |ui: &mut egui::Ui, value: String, label: &str, color: Color32| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                theme::text(ui, value, theme::semibold(13.5), color);
                theme::text(ui, label, theme::regular(12.5), palette.secondary);
            });
        };
        theme::text(ui, gettext(locale, "This month").to_uppercase(), theme::semibold(11.0), palette.dim);
        let one = |n: usize, singular: std::borrow::Cow<'static, str>, plural: std::borrow::Cow<'static, str>| {
            if n == 1 { singular } else { plural }
        };
        stat(ui, month.new.to_string(), &one(month.new, gettext(locale, "new lead"), gettext(locale, "new")), palette.text);
        stat(ui, month.won.to_string(), &one(month.won, gettext(locale, "won one"), gettext(locale, "won")), palette.accent);
        if month.won_value > 0.0 {
            stat(ui, super::sales::money(month.won_value), &gettext(locale, "sold"), palette.accent);
        }
        if let Some(rate) = month.conversion() {
            stat(ui, format!("{rate}%"), &gettext(locale, "closing rate"), palette.text);
        }
        if month.lost > 0 {
            let why = month
                .lost_reasons
                .iter()
                .take(2)
                .map(|(reason, n)| format!("{} {n}", reason_label(locale, reason).to_lowercase()))
                .collect::<Vec<_>>()
                .join(", ");
            stat(ui, month.lost.to_string(), &format!("{} ({why})", one(month.lost, gettext(locale, "lost one"), gettext(locale, "lost"))), palette.secondary);
        }
        // Too many first messages a day and WhatsApp takes it for spam.
        let over = contacts >= settings.daily_cap as usize;
        let near = contacts * 5 >= settings.daily_cap as usize * 4;
        if contacts > 0 {
            let color = if over { palette.danger } else if near { palette.warning } else { palette.secondary };
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                theme::text(ui, format!("{contacts}/{}", settings.daily_cap), theme::semibold(13.5), color);
                theme::text(ui, gettext(locale, "first contacts today").as_ref(), theme::regular(12.5), palette.secondary);
            })
            .response
            .on_hover_text(gettext(locale, "WhatsApp may block a number that writes to too many new people in a day. Keep under the cap, and spread first messages over the day."));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            settings_button(app, ui, state, &settings, changes);
        });
    });
}

/// The project's sales settings: buying cycle, follow-up cadence and the
/// daily cap of first messages.
fn settings_button(
    app: &App,
    ui: &mut egui::Ui,
    state: &mut State,
    settings: &Settings,
    changes: &mut Vec<Change>,
) {
    let palette = app.palette;
    let locale = app.locale;
    let response = theme::icon_button(
        ui,
        Icon::Settings,
        15.0,
        palette.secondary,
        palette.text,
        &gettext(locale, "How this project sells"),
    );
    if response.clicked() {
        state.settings_draft = Some((
            settings
                .rebuy_days
                .map(|d| d.to_string())
                .unwrap_or_default(),
            settings
                .cadence
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            settings.daily_cap.to_string(),
        ));
    }
    egui::Popup::menu(&response)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .width(320.0)
        .frame(widgets::menu_frame(&palette))
        .show(|ui| {
            let Some((rebuy, cadence, cap)) = state.settings_draft.as_mut() else {
                return;
            };
            ui.spacing_mut().item_spacing.y = 4.0;
            theme::text(ui, gettext(locale, "How this project sells").as_ref(), theme::semibold(13.5), palette.text);
            ui.add_space(4.0);
            let field = |ui: &mut egui::Ui, label: &str, hint: &str, help: &str, text: &mut String| {
                theme::text(ui, label, theme::medium(12.5), palette.secondary);
                field_frame(&palette).show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::singleline(text)
                            .hint_text(hint)
                            .font(theme::regular(13.5))
                            .frame(Frame::NONE)
                            .desired_width(f32::INFINITY),
                    );
                });
                wrapped(ui, help, theme::regular(11.5), palette.dim);
                ui.add_space(4.0);
            };
            field(
                ui,
                &gettext(locale, "Customers buy again every (days)"),
                "45",
                &gettext(locale, "Empty for one-off sales. Customers show up in “To sell again” after this long."),
                rebuy,
            );
            field(
                ui,
                &gettext(locale, "Follow up someone who does not answer after (days)"),
                "2, 5, 10",
                &gettext(locale, "One number per follow-up. After the last one, the lead can be closed as lost."),
                cadence,
            );
            field(
                ui,
                &gettext(locale, "First contacts a day, at most"),
                "25",
                &gettext(locale, "Writing to many new people in a day can get the number blocked."),
                cap,
            );
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if button(ui, &palette, gettext(locale, "Save").as_ref(), true).clicked() {
                        let days: Vec<u32> = cadence
                            .split(|c: char| !c.is_ascii_digit())
                            .filter_map(|n| n.parse().ok())
                            .filter(|n| *n > 0)
                            .take(8)
                            .collect();
                        changes.push(Change::Settings(Settings {
                            rebuy_days: rebuy.trim().parse().ok().filter(|d| *d > 0),
                            cadence: if days.is_empty() { Settings::default().cadence } else { days },
                            daily_cap: cap.trim().parse().ok().filter(|c| *c > 0).unwrap_or(25),
                        }));
                        ui.close();
                    }
                });
            });
        });
}

/// Pasting the brand's token: what it is, where to get it, and checking it.
fn connect_card(
    app: &App,
    ui: &mut egui::Ui,
    state: &mut State,
    project: &Project,
    brand: Option<&Brand>,
    _changes: &mut Vec<Change>,
) {
    let palette = app.palette;
    let locale = app.locale;
    let checking = eleva::connection_pending(&project.id);
    card_frame(&palette).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            theme::icon(ui, Icon::Link, 16.0, palette.accent);
            theme::text(
                ui,
                gettext(locale, "Connect this project to ElevaLeads"),
                theme::semibold(14.5),
                palette.text,
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if theme::icon_button(ui, Icon::X, 15.0, palette.secondary, palette.text, &gettext(locale, "Close"))
                    .clicked()
                {
                    state.connecting = false;
                    state.connect_error = None;
                }
            });
        });
        wrapped(
            ui,
            gettext(locale, "Paste a token limited to one brand (ElevaLeads → Settings → Connect with AI). Read only shows the brand's inbox here; read and write also sends your notes, stages and next actions to the customer's file. It is kept in this computer's keyring.").as_ref(),
            theme::regular(12.5),
            palette.secondary,
        );
        if let Some(brand) = brand {
            ui.add_space(4.0);
            chip(
                ui,
                &gettext(locale, "Now: {brand}").replace("{brand}", &brand.name),
                palette.accent,
            );
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            field_frame(&palette).show(ui, |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut state.token)
                        .id(egui::Id::new(("leads-token", &project.id)))
                        .hint_text("elmcp_…")
                        .password(true)
                        .font(theme::regular(14.0))
                        .frame(Frame::NONE)
                        .desired_width((ui.available_width() - 120.0).max(200.0)),
                );
            });
            if checking {
                ui.add(egui::Spinner::new().size(16.0).color(palette.accent));
            } else if button(ui, &palette, gettext(locale, "Connect").as_ref(), true).clicked()
                && !state.token.trim().is_empty()
            {
                state.connect_error = None;
                eleva::start_connect(ui.ctx(), &project.id, state.token.trim().to_owned());
            }
        });
        if let Some(error) = &state.connect_error {
            ui.add_space(4.0);
            wrapped(ui, &error_text(locale, error), theme::regular(12.5), palette.danger);
        }
    });
}

/// Adding leads: from the chats, or numbers to write to.
fn add_card(
    app: &mut App,
    ui: &mut egui::Ui,
    state: &mut State,
    leads: &Leads,
    project: &Project,
    adding: Adding,
    changes: &mut Vec<Change>,
) {
    let palette = app.palette;
    let locale = app.locale;
    card_frame(&palette).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            theme::icon(ui, Icon::Plus, 16.0, palette.accent);
            theme::text(ui, gettext(locale, "New lead").as_ref(), theme::semibold(14.5), palette.text);
            ui.add_space(6.0);
            for (value, label) in [
                (Adding::Chats, gettext(locale, "From a chat")),
                (Adding::Numbers, gettext(locale, "By number")),
            ] {
                if widgets::filter_chip(ui, &palette, &label, 0, adding == value).clicked() {
                    state.adding = Some(value);
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if theme::icon_button(ui, Icon::X, 15.0, palette.secondary, palette.text, &gettext(locale, "Close"))
                    .clicked()
                {
                    state.adding = None;
                }
            });
        });
        ui.add_space(6.0);
        match adding {
            Adding::Chats => {
                let width = ui.available_width();
                widgets::search_field(
                    ui,
                    &palette,
                    egui::Id::new("leads-chat-search"),
                    &mut state.add_search,
                    gettext(locale, "Search chats").as_ref(),
                    width,
                );
                ui.add_space(6.0);
                let needle = crate::util::search_key(state.add_search.trim());
                // The project's own chats first, then any other.
                let mut candidates: Vec<(bool, String, String)> = app
                    .chats
                    .iter()
                    .filter(|chat| chat.last.is_some())
                    .filter(|chat| leads.by_chat(&project.id, &chat.id).is_none())
                    .filter(|chat| needle.is_empty() || crate::util::search_key(&chat.name).contains(&needle))
                    .map(|chat| (project.source(&chat.id).is_none(), chat.id.clone(), chat.name.clone()))
                    .collect();
                candidates.sort_by_key(|(other, _, _)| *other);
                for (other, chat, name) in candidates.into_iter().take(12) {
                    let picture = app.avatar(&chat);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 10.0;
                        let (avatar, _) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::hover());
                        widgets::paint_avatar(ui, &palette, avatar, &name, &chat, picture.as_deref());
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            ui.set_max_width((ui.available_width() - 110.0).max(100.0));
                            theme::text(ui, &name, theme::regular(14.0), palette.text);
                            if !other {
                                theme::text(ui, gettext(locale, "In this project").as_ref(), theme::regular(11.5), palette.dim);
                            }
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if button(ui, &palette, gettext(locale, "Add").as_ref(), false).clicked() {
                                let phone = leads::phone_of_chat(&chat);
                                changes.push(Change::Add(Box::new(Lead::new(&project.id, &name, Some(chat.clone()), phone))));
                            }
                        });
                    });
                    ui.add_space(4.0);
                }
            }
            Adding::Numbers => {
                wrapped(
                    ui,
                    gettext(locale, "One per line: a name and a number, for the people you are going to write to. Each starts as New, with “First message” for today.").as_ref(),
                    theme::regular(12.5),
                    palette.secondary,
                );
                let cap = leads.settings_of(&project.id).daily_cap as usize;
                let today = leads.first_contacts(&project.id, leads::today_start());
                let waiting = leads
                    .of_project(&project.id)
                    .filter(|lead| lead.stage == Stage::New)
                    .count();
                wrapped(
                    ui,
                    &gettext(locale, "Today: {n} first contacts of {cap}; {waiting} new leads still to write to. Spread first messages over the day so WhatsApp does not take them for spam.")
                        .replace("{n}", &today.to_string())
                        .replace("{cap}", &cap.to_string())
                        .replace("{waiting}", &waiting.to_string()),
                    theme::regular(12.0),
                    if today >= cap { palette.danger } else { palette.dim },
                );
                ui.add_space(6.0);
                field_frame(&palette).show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut state.numbers)
                            .id(egui::Id::new("leads-numbers"))
                            .hint_text("Laboratorio Ruiz, +54 9 11 5555-0000")
                            .font(theme::regular(14.0))
                            .frame(Frame::NONE)
                            .desired_rows(3)
                            .desired_width(f32::INFINITY),
                    );
                });
                ui.add_space(6.0);
                let parsed = parse_numbers(&state.numbers);
                ui.horizontal(|ui| {
                    let bad = parsed.iter().filter(|line| line.is_err()).count();
                    if bad > 0 {
                        theme::text(
                            ui,
                            gettext(locale, "{n} lines without a valid number").replace("{n}", &bad.to_string()),
                            theme::regular(12.5),
                            palette.danger,
                        );
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let good: Vec<_> = parsed.iter().filter_map(|line| line.as_ref().ok()).collect();
                        let label = gettext(locale, "Add {n}").replace("{n}", &good.len().to_string());
                        if !good.is_empty() && button(ui, &palette, &label, true).clicked() {
                            let first = gettext(locale, "First message").into_owned();
                            let mut known = 0;
                            for (name, phone) in good {
                                if leads.by_phone(&project.id, phone).is_some() {
                                    known += 1;
                                    continue;
                                }
                                let chat = format!("{phone}@s.whatsapp.net");
                                let chat = app.chat(&chat).map(|_| chat);
                                let mut lead = Lead::new(&project.id, name, chat, Some(phone.clone()));
                                lead.set_next(&first, Some(leads::today()));
                                changes.push(Change::Add(Box::new(lead)));
                            }
                            // Lines that could not be read stay to fix.
                            let bad: Vec<String> = parsed.iter().filter_map(|line| line.as_ref().err().cloned()).collect();
                            state.numbers = bad.join("\n");
                            if state.numbers.is_empty() {
                                state.adding = None;
                            }
                            if known > 0 {
                                state.notice = Some((
                                    gettext(locale, "{n} were leads already").replace("{n}", &known.to_string()),
                                    Instant::now(),
                                ));
                            }
                        }
                    });
                });
            }
        }
    });
}

/// `Name, number` lines; a line with only a number takes it as the name.
fn parse_numbers(text: &str) -> Vec<Result<(String, String), String>> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            let (name, number) = match line.rsplit_once([',', ';', '\t']) {
                Some((name, number)) => (name.trim(), number.trim()),
                None => ("", line),
            };
            match crate::util::international_phone(number) {
                Some(phone) => Ok((
                    if name.is_empty() {
                        format!("+{phone}")
                    } else {
                        name.to_owned()
                    },
                    phone,
                )),
                None => Err(line.to_owned()),
            }
        })
        .collect()
}

/// The leads by urgency, filtered by stage and search.
fn list(
    app: &mut App,
    ui: &mut egui::Ui,
    state: &mut State,
    leads: &Leads,
    project: &Project,
    changes: &mut Vec<Change>,
) {
    let palette = app.palette;
    let locale = app.locale;
    let today = leads::today();
    let needle = crate::util::search_key(state.search.trim());
    let groups = {
        let app: &App = app;
        leads.grouped_with(&project.id, &today, &|lead| {
            turn(app, lead).is_some_and(|turn| turn.theirs)
        })
    };
    let any = leads.of_project(&project.id).next().is_some();
    egui::ScrollArea::vertical()
        .id_salt(("leads-list", &project.id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            suggestions_block(app, ui, state, leads, project, changes);
            if !any {
                widgets::empty_state(
                    ui,
                    &palette,
                    Icon::Users,
                    gettext(locale, "No leads yet").as_ref(),
                    gettext(locale, "Add the people you are selling to, from a chat or by number, and follow each sale here: its stage, what comes next and what was said.").as_ref(),
                );
                ui.add_space(12.0);
                ui.vertical_centered(|ui| {
                    if button(ui, &palette, gettext(locale, "Add a lead").as_ref(), true).clicked() {
                        state.adding = Some(Adding::Chats);
                    }
                });
                return;
            }
            let mut shown = 0;
            for (urgency, group) in groups {
                let group: Vec<&Lead> = group
                    .into_iter()
                    .filter(|lead| state.filter.is_none_or(|stage| lead.stage == stage))
                    .filter(|lead| {
                        needle.is_empty()
                            || crate::util::search_key(&lead.name).contains(&needle)
                            || lead.phone.as_deref().is_some_and(|p| p.contains(state.search.trim()))
                    })
                    .collect();
                if group.is_empty() {
                    continue;
                }
                shown += group.len();
                let folded = match urgency {
                    Urgency::Customers => Some(&mut state.show_customers),
                    Urgency::Lost => Some(&mut state.show_lost),
                    _ => None,
                };
                let color = match urgency {
                    Urgency::Reply => palette.accent,
                    Urgency::Overdue => palette.danger,
                    Urgency::Today | Urgency::Collect => palette.warning,
                    Urgency::NoAction => palette.secondary,
                    Urgency::Rebuy => palette.link,
                    _ => palette.dim,
                };
                let open = folded.as_ref().is_none_or(|open| **open) || state.filter.is_some() || !needle.is_empty();
                let response = ui
                    .horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        ui.add_space(4.0);
                        theme::text(
                            ui,
                            urgency_label(locale, urgency).to_uppercase(),
                            theme::semibold(11.5),
                            color,
                        );
                        theme::text(ui, group.len().to_string(), theme::medium(11.5), palette.dim);
                        if folded.is_some() {
                            theme::icon(ui, if open { Icon::ChevronDown } else { Icon::ChevronRight }, 13.0, palette.dim);
                        }
                    })
                    .response
                    .interact(Sense::click());
                if let Some(folded) = folded
                    && response.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
                {
                    *folded = !*folded;
                }
                let hint = match urgency {
                    Urgency::NoAction => Some(gettext(locale, "Open sales with no next step: the ones going cold")),
                    Urgency::Reply => Some(gettext(locale, "They wrote last: the next word is yours")),
                    Urgency::Collect => Some(gettext(locale, "Sales with part of the payment still due")),
                    Urgency::Rebuy => Some(gettext(locale, "Customers whose last purchase is a buying cycle ago")),
                    _ => None,
                };
                if let Some(hint) = hint {
                    ui.add_space(1.0);
                    ui.horizontal(|ui| {
                        ui.add_space(4.0);
                        theme::text(ui, hint.as_ref(), theme::regular(11.5), palette.dim);
                    });
                }
                ui.add_space(4.0);
                if !open {
                    ui.add_space(8.0);
                    continue;
                }
                for lead in group {
                    lead_row(app, ui, state, lead, urgency, changes);
                }
                ui.add_space(10.0);
            }
            if shown == 0 {
                widgets::empty_state(
                    ui,
                    &palette,
                    Icon::Search,
                    gettext(locale, "No lead matches").as_ref(),
                    gettext(locale, "Try another stage or search.").as_ref(),
                );
            }
        });
}

/// The analysis' buyers with no lead yet: who, what they want, the stage
/// the messages show; added or turned down with a click.
fn suggestions_block(
    app: &mut App,
    ui: &mut egui::Ui,
    state: &mut State,
    leads: &Leads,
    project: &Project,
    changes: &mut Vec<Change>,
) {
    let palette = app.palette;
    let locale = app.locale;
    let stale = state
        .hints
        .as_ref()
        .is_none_or(|(id, at, _)| *id != project.id || at.elapsed() > Duration::from_secs(5));
    if stale {
        let hints = crate::projects::Analysis::load(&app.dirs.config, &project.id)
            .map(|analysis| analysis.leads)
            .unwrap_or_default();
        state.hints = Some((project.id.clone(), Instant::now(), hints));
    }
    let Some((_, _, hints)) = &state.hints else {
        return;
    };
    let shown = leads.suggestions(project, hints);
    if shown.is_empty() {
        return;
    }
    let total = shown.len();
    let limit = if state.show_hints { total } else { 3 };
    let mut toggle = false;
    card_frame(&palette).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            theme::icon(ui, Icon::Star, 15.0, palette.warning);
            theme::text(
                ui,
                gettext(locale, "Suggested by the analysis"),
                theme::semibold(14.0),
                palette.text,
            );
            chip(ui, &total.to_string(), palette.warning);
        });
        wrapped(
            ui,
            gettext(
                locale,
                "People who asked for a price, a quote or stock, or bought, and are not leads yet.",
            )
            .as_ref(),
            theme::regular(12.0),
            palette.dim,
        );
        ui.add_space(4.0);
        for hint in shown.iter().take(limit) {
            let picture = app.avatar(&hint.person);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let (avatar, _) = ui.allocate_exact_size(Vec2::splat(32.0), Sense::hover());
                widgets::paint_avatar(
                    ui,
                    &palette,
                    avatar,
                    &hint.name,
                    &hint.person,
                    picture.as_deref(),
                );
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.set_max_width((ui.available_width() - 120.0).max(120.0));
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        theme::text(ui, &hint.name, theme::semibold(13.5), palette.text);
                        let stage = match hint.stage.as_str() {
                            "quoted" => Stage::Quoted,
                            "customer" => Stage::Customer,
                            _ => Stage::Contacted,
                        };
                        chip(
                            ui,
                            &stage_label(locale, stage),
                            stage_color(&palette, stage),
                        );
                        if hint.paid {
                            chip(ui, &gettext(locale, "says they paid"), palette.accent);
                        }
                        if let Some(amount) = &hint.amount {
                            theme::text(ui, amount, theme::medium(12.5), palette.secondary);
                        }
                    });
                    if !hint.why.is_empty() {
                        theme::text(ui, &hint.why, theme::regular(12.5), palette.secondary);
                    }
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    if theme::icon_button(
                        ui,
                        Icon::X,
                        14.0,
                        palette.dim,
                        palette.text,
                        &gettext(locale, "Not a lead"),
                    )
                    .clicked()
                    {
                        changes.push(Change::DismissHint(hint.id.clone()));
                    }
                    if let (Some(chat), Some(message)) = (&hint.chat, &hint.message)
                        && theme::icon_button(
                            ui,
                            Icon::ExternalLink,
                            14.0,
                            palette.dim,
                            palette.text,
                            &gettext(locale, "Open the message"),
                        )
                        .clicked()
                    {
                        app.actions.push(Action::Open(Page::Chats));
                        app.actions.push(Action::OpenMessage {
                            chat: chat.clone(),
                            message: message.clone(),
                        });
                    }
                    if button(ui, &palette, gettext(locale, "Add").as_ref(), true).clicked() {
                        changes.push(Change::AddHint(Box::new((*hint).clone())));
                    }
                });
            });
            ui.add_space(4.0);
        }
        if total > 3 {
            let label = if state.show_hints {
                gettext(locale, "Show less").into_owned()
            } else {
                gettext(locale, "Show {n} more").replace("{n}", &(total - 3).to_string())
            };
            if button(ui, &palette, &label, false).clicked() {
                toggle = true;
            }
        }
    });
    if toggle {
        state.show_hints = !state.show_hints;
    }
    ui.add_space(10.0);
}

/// One lead: its picture, name, stage, next step and when, and quick ways
/// to tick it off or put it off.
fn lead_row(
    app: &mut App,
    ui: &mut egui::Ui,
    state: &mut State,
    lead: &Lead,
    urgency: Urgency,
    changes: &mut Vec<Change>,
) {
    let palette = app.palette;
    let locale = app.locale;
    let selected = state.selected.as_deref() == Some(lead.id.as_str());
    let id = ui.id().with(("lead-row", &lead.id));
    let previous = ui.ctx().data(|data| data.get_temp::<egui::Rect>(id));
    let row_response = previous.map(|rect| ui.interact(rect, id, Sense::click()));
    let hovered = previous.is_some_and(|rect| ui.rect_contains_pointer(rect));
    if let Some(rect) = previous {
        let fill = if selected {
            palette.tint(0.08)
        } else if hovered {
            palette.tint(0.04)
        } else {
            Color32::TRANSPARENT
        };
        ui.painter()
            .rect_filled(rect, CornerRadius::same(theme::RADIUS + 2), fill);
    }
    let avatar_id = lead.chat.clone().unwrap_or_else(|| lead.id.clone());
    let picture = lead.chat.as_deref().and_then(|chat| app.avatar(chat));
    let row = Frame::new()
        .inner_margin(Margin::symmetric(8, 7))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let (avatar, _) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::hover());
                widgets::paint_avatar(
                    ui,
                    &palette,
                    avatar,
                    &lead.name,
                    &avatar_id,
                    picture.as_deref(),
                );
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.set_max_width((ui.available_width() - 150.0).max(120.0));
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        theme::text(ui, &lead.name, theme::semibold(14.0), palette.text);
                        if lead.eleva.is_some() {
                            theme::icon(ui, Icon::Link, 11.0, palette.accent)
                                .on_hover_text(gettext(locale, "Linked to ElevaLeads"));
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        chip(
                            ui,
                            &stage_label(locale, lead.stage),
                            stage_color(&palette, lead.stage),
                        );
                        match &lead.next {
                            Some(next) => {
                                if let Some(due) = &next.due {
                                    let (label, color) = due_label(app, due);
                                    theme::text(ui, label, theme::medium(12.0), color);
                                }
                                theme::text(
                                    ui,
                                    &next.text,
                                    theme::regular(12.5),
                                    palette.secondary,
                                );
                            }
                            None => {
                                let text = match (lead.stage, &lead.lost_reason) {
                                    (Stage::Lost, Some(reason)) => reason_label(locale, reason),
                                    _ => lead.value.clone().unwrap_or_default(),
                                };
                                theme::text(ui, text, theme::regular(12.5), palette.dim);
                            }
                        }
                    });
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    if hovered
                        && lead.next.is_some()
                        && matches!(
                            urgency,
                            Urgency::Overdue | Urgency::Today | Urgency::Upcoming
                        )
                    {
                        let later = theme::icon_button(
                            ui,
                            Icon::Clock,
                            15.0,
                            palette.secondary,
                            palette.text,
                            &gettext(locale, "Put it off"),
                        );
                        postpone_menu(app, ui, &later, &lead.id, changes);
                        if theme::icon_button(
                            ui,
                            Icon::Check,
                            15.0,
                            palette.accent,
                            palette.accent_hover,
                            &gettext(locale, "Done: plan the next step"),
                        )
                        .clicked()
                        {
                            changes.push(Change::Done(lead.id.clone()));
                            state.selected = Some(lead.id.clone());
                        }
                    } else {
                        let (text, color) = match turn(app, lead) {
                            Some(turn) if turn.theirs && lead.stage != Stage::Lost => (
                                gettext(locale, "wrote {when} ago")
                                    .replace("{when}", &ago(locale, turn.at)),
                                palette.accent,
                            ),
                            Some(turn) if lead.stage.open() => (
                                gettext(locale, "no answer for {when}")
                                    .replace("{when}", &ago(locale, turn.at)),
                                palette.dim,
                            ),
                            Some(turn) => (ago(locale, turn.at), palette.dim),
                            None => (ago(locale, lead.last_touch()), palette.dim),
                        };
                        theme::text(ui, text, theme::regular(11.5), color);
                    }
                });
            });
        });
    ui.ctx()
        .data_mut(|data| data.insert_temp(id, row.response.rect));
    if previous.is_none() {
        ui.ctx().request_repaint();
    }
    if let Some(response) = row_response
        && response
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked()
    {
        state.selected = Some(lead.id.clone());
    }
}

fn postpone_menu(
    app: &App,
    ui: &egui::Ui,
    button: &egui::Response,
    lead: &str,
    changes: &mut Vec<Change>,
) {
    let palette = app.palette;
    let locale = app.locale;
    let options = [
        (1, gettext(locale, "Tomorrow")),
        (3, gettext(locale, "In 3 days")),
        (7, gettext(locale, "Next week")),
    ];
    let labels: Vec<&str> = options.iter().map(|(_, label)| label.as_ref()).collect();
    egui::Popup::menu(button)
        .width(widgets::menu_width(ui, &labels, true))
        .frame(widgets::menu_frame(&palette))
        .show(|ui| {
            for (days, label) in &options {
                if widgets::menu_item(ui, &palette, Some(Icon::Calendar), label) {
                    changes.push(Change::Postpone(lead.to_owned(), *days));
                    ui.close();
                }
            }
        });
}

fn nothing_picked(app: &App, ui: &mut egui::Ui, leads: &Leads, project: &Project) {
    let palette = app.palette;
    let locale = app.locale;
    if leads.of_project(&project.id).next().is_none() {
        return;
    }
    widgets::empty_state(
        ui,
        &palette,
        Icon::User,
        gettext(locale, "Pick a lead").as_ref(),
        gettext(
            locale,
            "Its stage, what comes next, and everything said and done with it.",
        )
        .as_ref(),
    );
}

// ------------------------------------------------------------ the lead

#[allow(clippy::too_many_arguments)]
fn detail(
    app: &mut App,
    ui: &mut egui::Ui,
    state: &mut State,
    leads: &Leads,
    lead: &Lead,
    brand: Option<&Brand>,
    changes: &mut Vec<Change>,
) {
    let palette = app.palette;
    let locale = app.locale;
    if state.next_for.as_deref() != Some(lead.id.as_str()) {
        // What was being written for the lead before stays for it.
        if let Some(previous) = state.next_for.take() {
            let draft = (std::mem::take(&mut state.composer), state.kind);
            if !draft.0.trim().is_empty() {
                state.drafts.insert(previous, draft);
            }
        }
        (state.composer, state.kind) = state
            .drafts
            .remove(&lead.id)
            .unwrap_or((String::new(), Kind::Note));
        state.next_for = Some(lead.id.clone());
        state.reload_next = true;
    }
    if std::mem::take(&mut state.reload_next) {
        state.next_text = lead
            .next
            .as_ref()
            .map(|n| n.text.clone())
            .unwrap_or_default();
        state.next_due = lead.next.as_ref().and_then(|n| n.due.clone());
        state.value = lead.value.clone().unwrap_or_default();
    }
    egui::ScrollArea::vertical()
        .id_salt(("lead-detail", &lead.id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // Who, and the ways to reach them.
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                let avatar_id = lead.chat.clone().unwrap_or_else(|| lead.id.clone());
                let picture = lead.chat.as_deref().and_then(|chat| app.avatar(chat));
                let (avatar, _) = ui.allocate_exact_size(Vec2::splat(46.0), Sense::hover());
                widgets::paint_avatar(
                    ui,
                    &palette,
                    avatar,
                    &lead.name,
                    &avatar_id,
                    picture.as_deref(),
                );
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.set_max_width((ui.available_width() - 120.0).max(140.0));
                    theme::text(ui, &lead.name, theme::semibold(17.0), palette.text);
                    let mut line = lead
                        .phone
                        .as_deref()
                        .map(|p| format!("+{}", leads::digits(p)))
                        .unwrap_or_default();
                    if lead.chat.is_none() {
                        line.push_str(&format!(" · {}", gettext(locale, "no chat yet")));
                    }
                    theme::text(ui, line, theme::regular(12.5), palette.secondary);
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let more = theme::icon_button(
                        ui,
                        Icon::Ellipsis,
                        17.0,
                        palette.secondary,
                        palette.text,
                        &gettext(locale, "Lead options"),
                    );
                    let remove = gettext(locale, "Remove lead");
                    egui::Popup::menu(&more)
                        .width(widgets::menu_width(ui, &[remove.as_ref()], true))
                        .frame(widgets::menu_frame(&palette))
                        .show(|ui| {
                            if widgets::menu_item(ui, &palette, Some(Icon::Trash), &remove) {
                                changes.push(Change::Remove(lead.id.clone()));
                                ui.close();
                            }
                        });
                    if (lead.chat.is_some() || lead.phone.is_some())
                        && button(ui, &palette, gettext(locale, "Open chat").as_ref(), false)
                            .clicked()
                    {
                        open_chat(app, lead);
                    }
                });
            });
            ui.add_space(12.0);
            stage_steps(app, ui, lead, changes);
            ui.add_space(12.0);
            // A customer's orders come first: a sale the shop does not know
            // about is the thing to fix.
            let customer = lead.stage == Stage::Customer;
            if customer && let Some(brand) = brand {
                super::sales::card(app, ui, lead, brand);
                ui.add_space(10.0);
            }
            next_card(
                app,
                ui,
                state,
                lead,
                &leads.settings_of(&lead.project),
                changes,
            );
            ui.add_space(10.0);
            if let Some(brand) = brand {
                eleva_card(app, ui, state, lead, brand, changes);
                ui.add_space(10.0);
                if !customer {
                    super::sales::card(app, ui, lead, brand);
                    ui.add_space(10.0);
                }
            }
            templates_card(app, ui, state, leads, lead, changes);
            ui.add_space(10.0);
            history(app, ui, state, lead, brand, changes);
        });
}

/// The stages as steps: the ones done filled, the current one ringed.
/// Lost asks why.
fn stage_steps(app: &App, ui: &mut egui::Ui, lead: &Lead, changes: &mut Vec<Change>) {
    let palette = app.palette;
    let locale = app.locale;
    let current = Stage::ALL
        .iter()
        .position(|s| *s == lead.stage)
        .unwrap_or(0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let width = ((ui.available_width() - 16.0) / 5.0).max(64.0);
        for (index, stage) in Stage::ALL.into_iter().enumerate() {
            let color = stage_color(&palette, stage);
            let (rect, response) = ui.allocate_exact_size(vec2(width, 34.0), Sense::click());
            theme::reveal_focus(&response);
            let is = stage == lead.stage;
            let reached = lead.stage != Stage::Lost && index <= current && stage != Stage::Lost;
            let fill = if is {
                color.gamma_multiply(0.22)
            } else if reached {
                color.gamma_multiply(0.10)
            } else if response.hovered() {
                palette.surface_hover
            } else {
                card_fill(&palette)
            };
            ui.painter().rect_filled(rect, 9.0, fill);
            let stroke = if is {
                egui::Stroke::new(1.5, color)
            } else {
                palette.glass_edge()
            };
            ui.painter()
                .rect_stroke(rect, 9.0, stroke, egui::StrokeKind::Inside);
            let text_color = if is || reached {
                color
            } else {
                palette.secondary
            };
            let galley = ui.painter().layout_no_wrap(
                stage_label(locale, stage),
                if is {
                    theme::semibold(12.5)
                } else {
                    theme::medium(12.5)
                },
                text_color,
            );
            ui.painter()
                .galley(rect.center() - galley.size() / 2.0, galley, text_color);
            let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
            if stage == Stage::Lost {
                let labels: Vec<String> = LOST_REASONS
                    .iter()
                    .map(|r| reason_label(locale, r))
                    .collect();
                let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                egui::Popup::menu(&response)
                    .width(widgets::menu_width(ui, &refs, false))
                    .frame(widgets::menu_frame(&palette))
                    .show(|ui| {
                        theme::text(
                            ui,
                            gettext(locale, "Why was it lost?").as_ref(),
                            theme::regular(12.0),
                            palette.dim,
                        );
                        for (reason, label) in LOST_REASONS.iter().zip(&labels) {
                            if widgets::menu_item(ui, &palette, None, label) {
                                changes.push(Change::Stage(
                                    lead.id.clone(),
                                    Stage::Lost,
                                    Some((*reason).to_owned()),
                                ));
                                ui.close();
                            }
                        }
                    });
            } else if response.clicked() && !is {
                changes.push(Change::Stage(lead.id.clone(), stage, None));
            }
        }
    });
}

/// What comes next: the action, a day picked with a click, and its worth.
/// What usually comes next for a lead with nothing planned, by its stage
/// and how many times it was chased, as action and days from today.
fn suggestions(locale: Locale, lead: &Lead, settings: &Settings) -> Vec<(String, i64)> {
    let follow = |text: std::borrow::Cow<'static, str>, days: i64| (text.into_owned(), days);
    match lead.stage {
        Stage::New => vec![
            follow(gettext(locale, "First message"), 0),
            follow(gettext(locale, "Call them"), 0),
        ],
        Stage::Contacted => match settings.cadence.get(lead.chases()) {
            Some(days) => vec![
                follow(gettext(locale, "Follow up"), i64::from(*days)),
                follow(gettext(locale, "Call them"), 1),
            ],
            None => vec![follow(gettext(locale, "Last try"), 3)],
        },
        Stage::Quoted => vec![
            follow(gettext(locale, "Ask about the quote"), 2),
            follow(gettext(locale, "Call to close"), 1),
            follow(gettext(locale, "Send a reminder of the offer"), 4),
        ],
        Stage::Customer => {
            let mut list = vec![
                follow(gettext(locale, "Confirm the delivery"), 2),
                follow(gettext(locale, "Ask for a review or a referral"), 7),
            ];
            if let Some(days) = settings.rebuy_days {
                list.push(follow(
                    gettext(locale, "Offer to buy again"),
                    lead.rebuy_in(days),
                ));
            }
            list
        }
        Stage::Lost => vec![follow(gettext(locale, "Try again"), 60)],
    }
}

fn next_card(
    app: &App,
    ui: &mut egui::Ui,
    state: &mut State,
    lead: &Lead,
    settings: &Settings,
    changes: &mut Vec<Change>,
) {
    let palette = app.palette;
    let locale = app.locale;
    card_frame(&palette).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            theme::icon(ui, Icon::Calendar, 15.0, palette.warning);
            theme::text(
                ui,
                gettext(locale, "Next action").as_ref(),
                theme::semibold(14.0),
                palette.text,
            );
            if let Some(next) = &lead.next {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if button(ui, &palette, gettext(locale, "Done").as_ref(), false).clicked() {
                        changes.push(Change::Done(lead.id.clone()));
                        state.reload_next = true;
                    }
                    if let Some(due) = &next.due {
                        let (label, color) = due_label(app, due);
                        chip(ui, &label, color);
                    }
                });
            }
        });
        if lead.next.is_none() {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
                theme::text(
                    ui,
                    gettext(locale, "Usually next:").as_ref(),
                    theme::regular(12.0),
                    palette.dim,
                );
                for (text, days) in suggestions(locale, lead, settings) {
                    let when = match days {
                        0 => gettext(locale, "today").into_owned(),
                        1 => gettext(locale, "tomorrow").into_owned(),
                        n => gettext(locale, "in {n} days").replace("{n}", &n.to_string()),
                    };
                    if widgets::filter_chip(ui, &palette, &format!("{text} · {when}"), 0, false)
                        .clicked()
                    {
                        changes.push(Change::Next(
                            lead.id.clone(),
                            text.clone(),
                            Some(leads::in_days(days)),
                        ));
                        state.reload_next = true;
                    }
                }
                // Chased every step of the cadence without an answer.
                if lead.stage == Stage::Contacted
                    && lead.chases() >= settings.cadence.len()
                    && widgets::filter_chip(
                        ui,
                        &palette,
                        gettext(locale, "Lost: no answer").as_ref(),
                        0,
                        false,
                    )
                    .clicked()
                {
                    changes.push(Change::Stage(
                        lead.id.clone(),
                        Stage::Lost,
                        Some("No answer".into()),
                    ));
                }
            });
        }
        ui.add_space(6.0);
        field_frame(&palette).show(ui, |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut state.next_text)
                    .id(egui::Id::new(("lead-next", &lead.id)))
                    .hint_text(
                        gettext(
                            locale,
                            "Send the quote, call to close, follow up on the sample…",
                        )
                        .as_ref(),
                    )
                    .font(theme::regular(14.0))
                    .frame(Frame::NONE)
                    .desired_width(f32::INFINITY),
            );
        });
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
            for (days, label) in [
                (Some(0), gettext(locale, "Today")),
                (Some(1), gettext(locale, "Tomorrow")),
                (Some(3), gettext(locale, "In 3 days")),
                (Some(7), gettext(locale, "Next week")),
                (None, gettext(locale, "No date")),
            ] {
                let due = days.map(leads::in_days);
                if widgets::filter_chip(ui, &palette, &label, 0, state.next_due == due).clicked() {
                    state.next_due = due;
                }
            }
            if let Some(due) = &state.next_due
                && ![0, 1, 3, 7].iter().any(|d| leads::in_days(*d) == *due)
            {
                chip(ui, &due_label(app, due).0, palette.secondary);
            }
        });
        let unchanged = lead
            .next
            .as_ref()
            .map(|n| (n.text.as_str(), n.due.as_deref()))
            == Some((state.next_text.trim(), state.next_due.as_deref()))
            || (lead.next.is_none() && state.next_text.trim().is_empty());
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            theme::text(
                ui,
                gettext(locale, "Worth").as_ref(),
                theme::regular(12.5),
                palette.secondary,
            );
            let response = field_frame(&palette)
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut state.value)
                            .id(egui::Id::new(("lead-value", &lead.id)))
                            .hint_text("$ 850.000")
                            .font(theme::regular(13.5))
                            .frame(Frame::NONE)
                            .desired_width(120.0),
                    )
                })
                .inner;
            if response.lost_focus() && state.value.trim() != lead.value.as_deref().unwrap_or("") {
                changes.push(Change::Value(lead.id.clone(), state.value.clone()));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if !unchanged
                    && button(ui, &palette, gettext(locale, "Save").as_ref(), true).clicked()
                {
                    changes.push(Change::Next(
                        lead.id.clone(),
                        state.next_text.clone(),
                        state.next_due.clone(),
                    ));
                }
            });
        });
    });
}

/// The lead's link to the brand's customer, and the way to make it.
fn eleva_card(
    app: &App,
    ui: &mut egui::Ui,
    state: &mut State,
    lead: &Lead,
    brand: &Brand,
    changes: &mut Vec<Change>,
) {
    let palette = app.palette;
    let locale = app.locale;
    let looking = eleva::found_pending(&lead.id);
    card_frame(&palette).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            theme::icon(ui, Icon::Link, 15.0, palette.accent);
            theme::text(ui, format!("ElevaLeads · {}", brand.name), theme::semibold(14.0), palette.text);
            if !brand.write {
                chip(ui, &gettext(locale, "read only"), palette.dim);
            }
        });
        ui.add_space(4.0);
        match &lead.eleva {
            Some(customer) => {
                let failed = lead.entries.iter().filter(|e| matches!(e.sync, Sync::Failed(_))).count();
                let pending = lead.pending();
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = vec2(6.0, 4.0);
                    theme::icon(ui, Icon::CircleCheck, 14.0, palette.accent);
                    theme::text(
                        ui,
                        gettext(locale, "Linked to {name}").replace("{name}", &customer.name),
                        theme::regular(13.0),
                        palette.text,
                    );
                    if let Some(stage) = &customer.stage {
                        chip(ui, &gettext(locale, "there: {stage}").replace("{stage}", stage), palette.secondary);
                    }
                    if pending > 0 {
                        chip(ui, &gettext(locale, "{n} sending").replace("{n}", &pending.to_string()), palette.warning);
                    }
                    if failed > 0 {
                        chip(ui, &gettext(locale, "{n} not sent").replace("{n}", &failed.to_string()), palette.danger);
                    }
                });
                if !brand.write {
                    wrapped(ui, gettext(locale, "The token is read-only: what you write stays here. Change it for one that can write and send it again.").as_ref(), theme::regular(12.0), palette.secondary);
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if failed > 0 && button(ui, &palette, gettext(locale, "Send again").as_ref(), false).clicked() {
                        changes.push(Change::Retry(lead.id.clone()));
                    }
                    if button(ui, &palette, gettext(locale, "Open in ElevaLeads").as_ref(), false).clicked() {
                        let query = lead.phone.clone().unwrap_or_else(|| customer.name.clone());
                        ui.ctx().open_url(egui::OpenUrl::new_tab(format!(
                            "https://elevaleads.com/app/{}/clientes?q={}",
                            brand.slug,
                            leads::digits(&query)
                        )));
                    }
                    if button(ui, &palette, gettext(locale, "Unlink").as_ref(), false).clicked() {
                        changes.push(Change::Unlink(lead.id.clone()));
                    }
                });
            }
            None => {
                let phone = lead.phone.clone();
                if looking {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(14.0).color(palette.accent));
                        theme::text(ui, gettext(locale, "Looking for them in the brand…").as_ref(), theme::regular(13.0), palette.secondary);
                    });
                } else if state.not_found.contains(&lead.id) {
                    wrapped(
                        ui,
                        &gettext(locale, "{brand} has no customer with this number yet. Create them in ElevaLeads, then look again.").replace("{brand}", &brand.name),
                        theme::regular(13.0),
                        palette.secondary,
                    );
                } else {
                    wrapped(
                        ui,
                        gettext(locale, "Link this lead to the brand's customer with the same number: your notes, calls, stages and next actions go to their file in ElevaLeads.").as_ref(),
                        theme::regular(12.5),
                        palette.secondary,
                    );
                }
                if let Some((id, error)) = &state.find_error
                    && *id == lead.id
                {
                    wrapped(ui, &error_text(locale, error), theme::regular(12.5), palette.danger);
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    match phone {
                        Some(phone) if !looking => {
                            let label = if state.not_found.contains(&lead.id) {
                                gettext(locale, "Look again")
                            } else {
                                gettext(locale, "Link")
                            };
                            if button(ui, &palette, &label, true).clicked() {
                                state.find_error = None;
                                eleva::start_find(ui.ctx(), &lead.id, &brand.slug, &phone);
                            }
                        }
                        None => {
                            theme::text(ui, gettext(locale, "This lead has no number to look for.").as_ref(), theme::regular(12.5), palette.dim);
                        }
                        _ => {}
                    }
                    if state.not_found.contains(&lead.id)
                        && button(ui, &palette, gettext(locale, "Open ElevaLeads").as_ref(), false).clicked()
                    {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(format!("https://elevaleads.com/app/{}/clientes", brand.slug)));
                    }
                });
            }
        }
    });
}

/// Saved first messages, put in the lead's chat with its name, never sent.
fn templates_card(
    app: &mut App,
    ui: &mut egui::Ui,
    state: &mut State,
    leads: &Leads,
    lead: &Lead,
    changes: &mut Vec<Change>,
) {
    let palette = app.palette;
    let locale = app.locale;
    let templates = leads
        .templates
        .get(&lead.project)
        .cloned()
        .unwrap_or_default();
    let mut put = None;
    egui::CollapsingHeader::new(
        egui::RichText::new(gettext(locale, "Saved messages"))
            .font(theme::semibold(13.5))
            .color(palette.text),
    )
    .id_salt(("lead-templates", &lead.project))
    .default_open(lead.stage == Stage::New)
    .show(ui, |ui| {
        wrapped(
            ui,
            gettext(locale, "Click one to put it in the chat with the name filled in. It is never sent on its own: you send it. {name} is the first name.").as_ref(),
            theme::regular(12.0),
            palette.secondary,
        );
        ui.add_space(4.0);
        for (index, template) in templates.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let text = leads::fill(template, &lead.name);
                let response = Frame::new()
                    .fill(palette.surface)
                    .stroke(palette.glass_edge())
                    .corner_radius(CornerRadius::same(theme::RADIUS + 2))
                    .inner_margin(Margin::symmetric(10, 6))
                    .show(ui, |ui| {
                        ui.set_max_width((ui.available_width() - 30.0).max(120.0));
                        wrapped(ui, &text, theme::regular(13.0), palette.text);
                    })
                    .response
                    .interact(Sense::click())
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(gettext(locale, "Put it in the chat"));
                if response.clicked() {
                    put = Some(text);
                }
                if theme::icon_button(ui, Icon::Trash, 13.0, palette.dim, palette.text, &gettext(locale, "Delete"))
                    .clicked()
                {
                    changes.push(Change::RemoveTemplate(index));
                }
            });
            ui.add_space(4.0);
        }
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            field_frame(&palette).show(ui, |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut state.template)
                        .id(egui::Id::new(("lead-template", &lead.project)))
                        .hint_text(gettext(locale, "Hi {name}, I'm writing from…").as_ref())
                        .font(theme::regular(13.5))
                        .frame(Frame::NONE)
                        .desired_width((ui.available_width() - 90.0).max(140.0)),
                );
            });
            if !state.template.trim().is_empty() && button(ui, &palette, gettext(locale, "Save").as_ref(), false).clicked() {
                changes.push(Change::AddTemplate(std::mem::take(&mut state.template)));
            }
        });
    });
    if let Some(text) = put
        && let Some(chat) = open_chat(app, lead)
    {
        app.actions.push(Action::PrefillComposer { chat, text });
    }
}

/// The box to note what happened, and everything noted, newest first.
fn history(
    app: &mut App,
    ui: &mut egui::Ui,
    state: &mut State,
    lead: &Lead,
    brand: Option<&Brand>,
    changes: &mut Vec<Change>,
) {
    let palette = app.palette;
    let locale = app.locale;
    card_frame(&palette).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for kind in [Kind::Note, Kind::Call, Kind::FollowUp] {
                if widgets::filter_chip(
                    ui,
                    &palette,
                    &kind_label(locale, kind),
                    0,
                    state.kind == kind,
                )
                .clicked()
                {
                    state.kind = kind;
                }
            }
        });
        ui.add_space(6.0);
        let hint = match state.kind {
            Kind::Call => gettext(
                locale,
                "What was said on the call: what they want, prices, what you agreed…",
            ),
            Kind::FollowUp => gettext(locale, "What you did to move the sale on…"),
            _ => gettext(locale, "Anything worth remembering about this lead…"),
        };
        field_frame(&palette).show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut state.composer)
                    .id(egui::Id::new(("lead-composer", &lead.id)))
                    .hint_text(hint.as_ref())
                    .font(theme::regular(14.0))
                    .frame(Frame::NONE)
                    .desired_rows(2)
                    .desired_width(f32::INFINITY),
            );
        });
        ui.add_space(6.0);
        // Room for the call actions spelled out, beside "Note it".
        let wide = ui.available_width() >= 520.0;
        let calling = job().phase.busy();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if !calling && state.take.is_none() {
                record_button(app, ui, state, &lead.id);
            }
            if state.recorder.is_none() && !state.transcribing && state.take.is_none() {
                call_controls(app, ui, state, lead, wide);
            }
            let summarizing = state.summarizing.as_deref() == Some(lead.id.as_str());
            if summarizing {
                ui.add(egui::Spinner::new().size(15.0).color(palette.accent));
                theme::text(
                    ui,
                    gettext(locale, "Summarizing the chat…").as_ref(),
                    theme::regular(12.5),
                    palette.secondary,
                );
            } else if let Some(chat) = &lead.chat
                && state.summarizing.is_none()
                && theme::icon_button(
                    ui,
                    Icon::FileText,
                    17.0,
                    palette.secondary,
                    palette.text,
                    &gettext(locale, "Summarize the chat and note it"),
                )
                .clicked()
            {
                summarize(app, ui, state, &lead.id, chat);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if !state.composer.trim().is_empty()
                    && button(ui, &palette, gettext(locale, "Note it").as_ref(), true).clicked()
                {
                    changes.push(Change::Entry(
                        lead.id.clone(),
                        state.kind,
                        std::mem::take(&mut state.composer),
                    ));
                }
            });
        });
        voice_take(app, ui, state);
        call_lines(app, ui, lead);
        suggestion_row(app, ui, state, lead, changes);
        #[cfg(any(test, feature = "demo"))]
        if let Some(since) = state.demo_view {
            if since.elapsed() < Duration::from_secs(3) {
                state.kind = Kind::Call;
                ui.scroll_to_cursor(Some(Align::Center));
            } else {
                state.demo_view = None;
            }
        }
    });
    ui.add_space(10.0);
    if lead.entries.is_empty() {
        theme::text(
            ui,
            gettext(locale, "Nothing noted yet.").as_ref(),
            theme::regular(13.0),
            palette.dim,
        );
        return;
    }
    let mut day = String::new();
    for entry in lead.entries.iter().rev() {
        let label = crate::util::day_label(locale, entry.at);
        if label != day {
            ui.add_space(4.0);
            theme::text(ui, &label, theme::semibold(11.5), palette.dim);
            ui.add_space(2.0);
            day = label;
        }
        let quiet = matches!(entry.kind, Kind::Stage | Kind::Task);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            let color = if quiet { palette.dim } else { palette.accent };
            theme::icon(ui, kind_icon(entry.kind), 14.0, color);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.set_max_width(ui.available_width() - 4.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    theme::text(
                        ui,
                        kind_label(locale, entry.kind),
                        theme::medium(12.0),
                        palette.secondary,
                    );
                    theme::text(
                        ui,
                        crate::util::clock(entry.at),
                        theme::regular(11.5),
                        palette.dim,
                    );
                    if brand.is_some() {
                        match &entry.sync {
                            Sync::Sent => {
                                theme::icon(ui, Icon::CheckCheck, 12.0, palette.accent)
                                    .on_hover_text(gettext(locale, "In ElevaLeads"));
                            }
                            Sync::Pending => {
                                theme::icon(ui, Icon::Clock, 12.0, palette.warning)
                                    .on_hover_text(gettext(locale, "Sending to ElevaLeads"));
                            }
                            Sync::Failed(why) => {
                                theme::icon(ui, Icon::CircleAlert, 12.0, palette.danger)
                                    .on_hover_text(error_text(locale, why));
                            }
                            Sync::Local => {}
                        }
                    }
                });
                let text = entry_text(locale, entry.kind, &entry.text);
                wrapped(
                    ui,
                    &text,
                    theme::regular(if quiet { 12.5 } else { 14.0 }),
                    if quiet {
                        palette.secondary
                    } else {
                        palette.text
                    },
                );
                if entry.kind == Kind::Call && entry.call.is_some() {
                    call_entry(app, ui, state, lead, entry);
                }
            });
        });
        ui.add_space(8.0);
    }
}

/// The microphone: says a note into the box, read by the transcription
/// engines the app already has.
fn record_button(app: &App, ui: &mut egui::Ui, state: &mut State, lead: &str) {
    let palette = app.palette;
    let locale = app.locale;
    if let Some(recorder) = &state.recorder {
        let seconds = recorder.elapsed().as_secs();
        ui.painter().circle_filled(
            ui.cursor().left_center() + vec2(6.0, 0.0),
            5.0,
            palette.danger,
        );
        ui.add_space(14.0);
        theme::text(
            ui,
            format!("{}:{:02}", seconds / 60, seconds % 60),
            theme::medium(13.5),
            palette.text,
        );
        ui.ctx().request_repaint_after(Duration::from_millis(250));
        if button(ui, &palette, gettext(locale, "Stop").as_ref(), true).clicked()
            && let Some(recorder) = state.recorder.take()
        {
            let seconds = recorder.elapsed().as_secs();
            match recorder.finish() {
                // Kept aside until the user says what to do with it.
                Ok(samples) => {
                    let file = std::env::temp_dir().join(format!(
                        "whatspro-lead-{}-{}.wav",
                        std::process::id(),
                        jiff::Timestamp::now().as_millisecond()
                    ));
                    match std::fs::write(&file, crate::projects::wav(&samples, 48_000)) {
                        Ok(()) => state.take = Some(super::take::Take::new(file, seconds)),
                        Err(error) => log::warn!("leads: a voice note could not be kept: {error}"),
                    }
                }
                Err(error) => {
                    state.notice = Some((
                        gettext(locale, "The recording failed: {error}")
                            .replace("{error}", &error.to_string()),
                        Instant::now(),
                    ));
                }
            }
        }
    } else if state.transcribing {
        ui.add(egui::Spinner::new().size(15.0).color(palette.accent));
        theme::text(
            ui,
            gettext(locale, "Transcribing…").as_ref(),
            theme::regular(12.5),
            palette.secondary,
        );
    } else if theme::icon_button(
        ui,
        Icon::Mic,
        17.0,
        palette.secondary,
        palette.text,
        &gettext(locale, "Say it"),
    )
    .clicked()
    {
        state.recorder = Some(crate::audio::Recorder::start(
            crate::backend::Waker::default(),
        ));
        state.recording_for = Some(lead.to_owned());
    }
}

fn set_phase(ctx: &egui::Context, phase: CallPhase) {
    job().phase = phase;
    ctx.request_repaint();
}

/// What a call's thread needs: the media engines, the state and config
/// folders, the language for its errors, and the window to wake.
struct Reader {
    engines: crate::media_ai::MediaEngines,
    state: std::path::PathBuf,
    config: std::path::PathBuf,
    locale: Locale,
    ctx: egui::Context,
}

fn reader(app: &App, ctx: &egui::Context) -> Reader {
    Reader {
        engines: app.settings.media_engines.clone(),
        state: app.dirs.state.clone(),
        config: app.dirs.config.clone(),
        locale: app.locale,
        ctx: ctx.clone(),
    }
}

/// A call to keep in the library and read into a lead's history.
struct CallFile {
    path: std::path::PathBuf,
    origin: crate::call_library::Origin,
    seconds: u64,
    /// What its note starts with: how long it was, or the file's name.
    heading: String,
    lead: String,
    project: String,
    name: String,
}

/// A call in the library to read into a lead's entry.
struct Kept {
    call: String,
    heading: String,
    lead: String,
    name: String,
    entry: String,
}

/// The recorder's and the call library's errors, in the user's language;
/// what follows a colon is the system's own words.
fn translated(locale: Locale, error: String) -> String {
    let (head, detail) = match error.split_once(": ") {
        Some((head, detail)) => (head, Some(detail)),
        None => (error.as_str(), None),
    };
    let head = match head {
        "Nothing was heard in it" => gettext(locale, "Nothing was heard in it"),
        "Nothing was recorded" => gettext(locale, "Nothing was recorded"),
        "The call is not in the library" => gettext(locale, "The call is not in the library"),
        "Could not open the microphone" => gettext(locale, "Could not open the microphone"),
        "The microphone did not answer" => gettext(locale, "The microphone did not answer"),
        "The recording did not start" => gettext(locale, "The recording did not start"),
        _ => return error,
    };
    match detail {
        Some(detail) => format!("{head}: {detail}"),
        None => head.into_owned(),
    }
}

/// Changes lead `id` in the shared store and saves it, from a call's
/// thread; none when the lead was removed meanwhile.
fn with_lead<T>(
    config: &std::path::Path,
    id: &str,
    change: impl FnOnce(&mut Lead) -> T,
) -> Option<T> {
    let mut store = Store::open(config);
    let done = store.get_mut(id).map(change);
    if done.is_some() {
        store.save();
    }
    done
}

/// A removed lead's calls stay in the library, as its project's.
fn forget_calls(state: &std::path::Path, lead: &str) {
    use crate::call_library::Library;
    for call in Library::load(state)
        .calls
        .iter()
        .filter(|call| call.lead.as_deref() == Some(lead))
    {
        if let Err(error) = Library::update(state, &call.id, |call| call.lead = None) {
            log::warn!("leads: a removed lead's call could not be kept: {error}");
        }
    }
}

/// Keeps the call in the library and notes it in the lead's history at
/// once, as not transcribed yet: whatever happens while it is read (the
/// window closed, the app quit), the audio is safe and the lead has it,
/// to read again.
fn read_call(reader: Reader, call: CallFile) {
    use crate::call_library::Library;
    set_phase(&reader.ctx, CallPhase::Transcribing);
    let title = gettext(reader.locale, "Call with {name}").replace("{name}", &call.name);
    let kept = match Library::keep(
        &reader.state,
        &call.path,
        call.origin,
        call.seconds,
        &title,
        Some(&call.project),
        Some(&call.lead),
    ) {
        Ok(kept) => kept,
        Err(error) => {
            let error = translated(reader.locale, error);
            return set_phase(&reader.ctx, CallPhase::Failed(error));
        }
    };
    let placeholder = format!("{} {}", call.heading, leads::NOT_TRANSCRIBED);
    let entry = with_lead(&reader.config, &call.lead, |lead| {
        lead.add_call(&placeholder, "", &kept.id).id.clone()
    });
    let Some(entry) = entry else {
        forget_calls(&reader.state, &call.lead);
        return set_phase(&reader.ctx, CallPhase::Idle);
    };
    // The stage it may have moved to.
    eleva::push(&reader.ctx, &reader.config);
    let kept = Kept {
        call: kept.id,
        heading: call.heading,
        lead: call.lead,
        name: call.name,
        entry,
    };
    read_kept(reader, kept);
}

/// Reads a kept call with the chosen media engine (unless the library
/// already has its transcript) and has the analysis summarize it, with
/// what was agreed and what to do next, into its entry. With no model at
/// hand the call's opening stands in for the summary.
fn read_kept(reader: Reader, kept: Kept) {
    use crate::call_library::{self as library, Library};
    set_phase(&reader.ctx, CallPhase::Transcribing);
    let heard = Library::load(&reader.state)
        .get(&kept.call)
        .and_then(|call| call.transcript())
        .map(|transcript| transcript.text.clone());
    let transcript = match heard {
        Some(text) => text,
        None => match library::transcribe(&reader.engines, &reader.state, &kept.call) {
            Ok(text) => text,
            Err(error) => {
                log::warn!("leads: a call could not be transcribed");
                log::debug!("leads: transcription: {error}");
                let error = translated(reader.locale, error);
                return set_phase(&reader.ctx, CallPhase::Failed(error));
            }
        },
    };
    set_phase(&reader.ctx, CallPhase::Summarizing);
    let today = leads::today();
    let context = format!(
        "Lead (posible cliente): {}\nHoy es {today}. En commitments.due escribí la fecha como \
         AAAA-MM-DD, o dejala vacía si no se dijo un día.\n",
        kept.name
    );
    let (text, next) = match library::analyze(&reader.config, &reader.state, &kept.call, &context) {
        Ok(analysis) if !analysis.summary.trim().is_empty() => (
            leads::call_note(&kept.heading, &analysis),
            next_from(&analysis, &today),
        ),
        answer => {
            if let Err(error) = answer {
                log::warn!("leads: a call could not be summarized");
                log::debug!("leads: analysis: {error}");
            }
            let mut start: String = transcript.chars().take(280).collect();
            if start.len() < transcript.len() {
                start.push('…');
            }
            (format!("{} {start}", kept.heading).trim().to_owned(), None)
        }
    };
    with_lead(&reader.config, &kept.lead, |lead| {
        lead.redo_call(&kept.entry, &text, &transcript)
    });
    eleva::push(&reader.ctx, &reader.config);
    {
        let mut held = job();
        held.phase = CallPhase::Idle;
        held.noted = Some(kept.lead.clone());
        if let Some((action, due)) = next {
            held.suggestion = Some((kept.lead, action, due));
        }
    }
    reader.ctx.request_repaint();
}

/// What the call says to do next: what the user promised (with its day,
/// when it names one from `today` on), else the first task or next step.
fn next_from(
    analysis: &crate::call_library::CallAnalysis,
    today: &str,
) -> Option<(String, Option<String>)> {
    let mine = analysis.commitments.iter().find(|c| {
        let who = c.who.trim().to_lowercase();
        !c.what.trim().is_empty()
            && (matches!(who.as_str(), "yo" | "me" | "i" | "nosotros" | "we")
                || who.starts_with("yo "))
    });
    if let Some(promise) = mine {
        let due = promise
            .due
            .as_deref()
            .and_then(|due| due.trim().parse::<jiff::civil::Date>().ok())
            .map(|date| date.to_string())
            .filter(|date| date.as_str() >= today);
        return Some((promise.what.trim().to_owned(), due));
    }
    analysis
        .tasks
        .iter()
        .chain(&analysis.next_steps)
        .map(|step| step.trim())
        .find(|step| !step.is_empty())
        .map(|step| (step.to_owned(), None))
}

/// Takes the call job for `lead`, unless another call has it.
fn claim(lead: &Lead, phase: CallPhase) -> bool {
    let mut held = job();
    if held.phase.busy() {
        return false;
    }
    held.lead = lead.id.clone();
    held.name = lead.name.clone();
    held.project = lead.project.clone();
    held.phase = phase;
    held.stop = None;
    true
}

/// Records a call with `lead` until it is ended, then reads it into its
/// history. The other side comes through the computer's sound on macOS;
/// elsewhere, or when macOS does not allow it, the microphone alone hears
/// it (a call on speaker).
fn record_call(app: &App, ctx: &egui::Context, lead: &Lead) {
    use crate::recorder::{Recording, Source};
    if !claim(lead, CallPhase::Starting) {
        return;
    }
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    job().stop = Some(stop);
    let reader = reader(app, ctx);
    std::thread::spawn(move || {
        let both = cfg!(target_os = "macos").then(|| Recording::start(&reader.state, Source::Both));
        let (recording, mic_only) = match both {
            Some(Ok(recording)) => (recording, false),
            other => {
                if let Some(Err(error)) = other {
                    log::warn!("leads: only the microphone can be recorded: {error}");
                }
                match Recording::start(&reader.state, Source::Microphone) {
                    Ok(recording) => (recording, true),
                    Err(error) => {
                        job().stop = None;
                        let error = translated(reader.locale, error);
                        return set_phase(&reader.ctx, CallPhase::Failed(error));
                    }
                }
            }
        };
        set_phase(&reader.ctx, CallPhase::Recording(Instant::now(), mic_only));
        let _ = stopped.recv();
        let seconds = recording.elapsed().as_secs();
        let path = match recording.stop() {
            Ok(path) => path,
            Err(error) => {
                let error = translated(reader.locale, error);
                return set_phase(&reader.ctx, CallPhase::Failed(error));
            }
        };
        // Nothing is kept or read until the user says so.
        set_phase(
            &reader.ctx,
            CallPhase::Review(super::take::Take::new(path, seconds)),
        );
    });
}

/// Keeps a reviewed call: into the library and the lead's history, then
/// read, on a thread of its own.
fn keep_call(app: &App, ctx: &egui::Context, lead: &Lead, take: super::take::Take) {
    set_phase(ctx, CallPhase::Transcribing);
    let reader = reader(app, ctx);
    let call = CallFile {
        path: take.path,
        origin: crate::call_library::Origin::Recorded,
        seconds: take.seconds,
        heading: leads::call_heading(take.seconds),
        lead: lead.id.clone(),
        project: lead.project.clone(),
        name: lead.name.clone(),
    };
    std::thread::spawn(move || read_call(reader, call));
}

/// Keeps a reviewed call whose lead was removed meanwhile, as its
/// project's, for Notes.
fn keep_orphan(app: &App, ctx: &egui::Context, take: super::take::Take) {
    let (project, name) = {
        let held = job();
        (held.project.clone(), held.name.clone())
    };
    let title = gettext(app.locale, "Call with {name}").replace("{name}", &name);
    let phase = match crate::call_library::Library::keep(
        &app.dirs.state,
        &take.path,
        crate::call_library::Origin::Recorded,
        take.seconds,
        &title,
        Some(&project),
        None,
    ) {
        Ok(_) => CallPhase::Idle,
        Err(error) => CallPhase::Failed(translated(app.locale, error)),
    };
    set_phase(ctx, phase);
}

/// The voice note just recorded: transcribed into the box, or discarded.
fn voice_take(app: &mut App, ui: &mut egui::Ui, state: &mut State) {
    let Some(mut take) = state.take.take() else {
        return;
    };
    let locale = app.locale;
    ui.add_space(8.0);
    match super::take::card(app, ui, &mut take, &gettext(locale, "Transcribe")) {
        Some(super::take::Choice::Keep) => {
            state.transcribing = true;
            let engines = app.settings.media_engines.clone();
            let slot = state.spoken.clone();
            // Into the box of the lead it was said for.
            let said_for = state
                .recording_for
                .take()
                .or_else(|| state.next_for.clone())
                .unwrap_or_default();
            let ctx = ui.ctx().clone();
            let state_dir = app.dirs.state.clone();
            std::thread::spawn(move || {
                let result = crate::media_ai::speech(&engines, &state_dir, &take.path);
                take.discard();
                *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some((said_for, result));
                ctx.request_repaint();
            });
        }
        Some(super::take::Choice::Discard) => take.discard(),
        None => state.take = Some(take),
    }
}

/// Picks a recorded call (an audio or video file) and reads it into the
/// history of `lead`.
fn attach_audio(app: &App, ctx: &egui::Context, lead: &Lead) {
    if !claim(lead, CallPhase::Picking) {
        return;
    }
    let label = gettext(app.locale, "Audio or video").into_owned();
    let reader = reader(app, ctx);
    let (id, project, name) = (lead.id.clone(), lead.project.clone(), lead.name.clone());
    std::thread::spawn(move || {
        let Some(path) = rfd::FileDialog::new()
            .add_filter(
                &label,
                &[
                    "ogg", "opus", "mp3", "m4a", "aac", "wav", "mp4", "mov", "webm",
                ],
            )
            .pick_file()
        else {
            return set_phase(&reader.ctx, CallPhase::Idle);
        };
        let file = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let call = CallFile {
            path,
            origin: crate::call_library::Origin::Uploaded,
            seconds: 0,
            // Its own parentheses would end the heading early.
            heading: format!("({})", file.replace('(', "[").replace(')', "]")),
            lead: id,
            project,
            name,
        };
        read_call(reader, call);
    });
}

/// Reads again a call that could not be transcribed or summarized.
fn retry_call(app: &App, ctx: &egui::Context, lead: &Lead, entry: &leads::Entry) {
    let Some(call) = entry.call.clone() else {
        return;
    };
    if !claim(lead, CallPhase::Transcribing) {
        return;
    }
    let reader = reader(app, ctx);
    let kept = Kept {
        call,
        heading: leads::heading_of(&entry.text).to_owned(),
        lead: lead.id.clone(),
        name: lead.name.clone(),
        entry: entry.id.clone(),
    };
    std::thread::spawn(move || read_kept(reader, kept));
}

/// Ends the call being recorded, whichever lead it is for.
fn end_call() {
    let stop = job().stop.take();
    if let Some(stop) = stop {
        let _ = stop.send(());
    }
}

/// Opens a call's recording with the computer's player.
fn open_recording(app: &App, call: &str) -> Result<(), String> {
    let state_dir = &app.dirs.state;
    let path = crate::call_library::Library::load(state_dir)
        .get(call)
        .map(|call| call.path(state_dir))
        .filter(|path| path.is_file())
        .ok_or_else(|| {
            gettext(app.locale, "The recording is no longer in the call library").into_owned()
        })?;
    open::that_detached(path).map_err(|e| e.to_string())
}

/// Whether the library has a summary of `call`. A call it does not have
/// cannot be read again, so it counts as summarized.
fn summarized(app: &App, state: &mut State, call: &str) -> bool {
    if !state.summarized.contains_key(call) {
        let library = crate::call_library::Library::load(&app.dirs.state);
        for kept in &library.calls {
            let summary = kept.analysis.as_ref().map(|a| a.summary.trim());
            state
                .summarized
                .insert(kept.id.clone(), summary.is_some_and(|s| !s.is_empty()));
        }
        state.summarized.entry(call.to_owned()).or_insert(true);
    }
    state.summarized[call]
}

/// Below a recorded call: listening to it, reading it again when it could
/// not be transcribed or summarized, and its transcript to read or copy.
fn call_entry(app: &App, ui: &mut egui::Ui, state: &mut State, lead: &Lead, entry: &leads::Entry) {
    let palette = app.palette;
    let locale = app.locale;
    let Some(call) = entry.call.as_deref() else {
        return;
    };
    let again = if job().phase.busy() {
        None
    } else if entry.waiting() {
        Some(gettext(locale, "Transcribe it again"))
    } else if !summarized(app, state, call) {
        Some(gettext(locale, "Summarize it again"))
    } else {
        None
    };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        if theme::icon_button(
            ui,
            Icon::Play,
            14.0,
            palette.secondary,
            palette.text,
            &gettext(locale, "Listen to the call"),
        )
        .clicked()
            && let Err(error) = open_recording(app, call)
        {
            state.notice = Some((error, Instant::now()));
        }
        if let Some(again) = again
            && theme::icon_button(
                ui,
                Icon::Refresh,
                14.0,
                palette.secondary,
                palette.text,
                &again,
            )
            .clicked()
        {
            retry_call(app, ui.ctx(), lead, entry);
        }
        if let Some(transcript) = &entry.transcript
            && theme::icon_button(
                ui,
                Icon::Copy,
                14.0,
                palette.secondary,
                palette.text,
                &gettext(locale, "Copy the transcript"),
            )
            .clicked()
        {
            ui.ctx().copy_text(transcript.to_string());
            state.notice = Some((
                gettext(locale, "Transcript copied").into_owned(),
                Instant::now(),
            ));
        }
    });
    if let Some(transcript) = &entry.transcript {
        egui::CollapsingHeader::new(
            egui::RichText::new(gettext(locale, "Transcript"))
                .font(theme::medium(12.0))
                .color(palette.secondary),
        )
        .id_salt(("lead-transcript", &entry.id))
        .show(ui, |ui| {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(transcript.as_ref())
                        .font(theme::regular(13.0))
                        .color(palette.secondary),
                )
                .wrap()
                .selectable(true),
            );
        });
    }
}

/// What a call said to do next, offered as the lead's next action.
fn suggestion_row(
    app: &App,
    ui: &mut egui::Ui,
    state: &mut State,
    lead: &Lead,
    changes: &mut Vec<Change>,
) {
    let palette = app.palette;
    let locale = app.locale;
    let Some((for_lead, text, due)) = job().suggestion.clone() else {
        return;
    };
    if for_lead != lead.id {
        return;
    }
    if lead.next.as_ref().is_some_and(|next| next.text == text) {
        job().suggestion = None;
        return;
    }
    ui.add_space(8.0);
    // Stacked, so a long action never runs under the buttons.
    field_frame(&palette).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            theme::icon(ui, Icon::Calendar, 14.0, palette.accent);
            theme::text(
                ui,
                gettext(locale, "From the call, next:"),
                theme::medium(12.0),
                palette.secondary,
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if theme::icon_button(
                    ui,
                    Icon::X,
                    14.0,
                    palette.secondary,
                    palette.text,
                    &gettext(locale, "Dismiss"),
                )
                .clicked()
                {
                    job().suggestion = None;
                }
            });
        });
        let line = match &due {
            Some(day) => format!("{text} · {}", due_label(app, day).0),
            None => text.clone(),
        };
        wrapped(ui, &line, theme::regular(13.5), palette.text);
        ui.add_space(6.0);
        if button(
            ui,
            &palette,
            gettext(locale, "Make it the next action").as_ref(),
            true,
        )
        .clicked()
        {
            // The next action card shows it at once.
            state.next_text = text.clone();
            state.next_due = due.clone();
            changes.push(Change::Next(lead.id.clone(), text, due));
            job().suggestion = None;
        }
    });
}

/// Recording a call or attaching one, and what that is doing. A call with
/// another lead shows on a line of its own (`call_lines`). Spelled out
/// when noting a call and there is room; icons otherwise.
fn call_controls(app: &App, ui: &mut egui::Ui, state: &State, lead: &Lead, wide: bool) {
    let palette = app.palette;
    let locale = app.locale;
    let (phase, mine) = {
        let held = job();
        (held.phase.clone(), held.lead == lead.id)
    };
    if phase.busy() && !mine {
        return;
    }
    let working = |ui: &mut egui::Ui, label: std::borrow::Cow<'_, str>| {
        ui.add(egui::Spinner::new().size(15.0).color(palette.accent));
        theme::text(ui, label.as_ref(), theme::regular(12.5), palette.secondary);
    };
    match phase {
        CallPhase::Recording(started, _) => {
            let seconds = started.elapsed().as_secs();
            ui.painter().circle_filled(
                ui.cursor().left_center() + vec2(6.0, 0.0),
                5.0,
                palette.danger,
            );
            ui.add_space(14.0);
            theme::text(
                ui,
                gettext(locale, "Call {time}")
                    .replace("{time}", &format!("{}:{:02}", seconds / 60, seconds % 60)),
                theme::medium(13.5),
                palette.text,
            );
            ui.ctx().request_repaint_after(Duration::from_millis(500));
            if button(ui, &palette, gettext(locale, "End call").as_ref(), true).clicked() {
                end_call();
            }
        }
        CallPhase::Starting if cfg!(target_os = "macos") => {
            working(ui, gettext(locale, "Asking macOS for permission…"));
        }
        CallPhase::Starting => working(ui, gettext(locale, "Starting the recording…")),
        CallPhase::Picking => working(ui, gettext(locale, "Choosing a file…")),
        CallPhase::Review(_) => {}
        CallPhase::Transcribing => working(ui, gettext(locale, "Transcribing the call…")),
        CallPhase::Summarizing => working(ui, gettext(locale, "Summarizing the call…")),
        CallPhase::Idle | CallPhase::Failed(_) => {
            let record_tip = gettext(
                locale,
                "Record a call (your microphone and the computer's sound)",
            );
            let attach_tip = gettext(locale, "Attach a recorded call (audio or video)");
            let (record, attach) = if state.kind == Kind::Call && wide {
                (
                    button(ui, &palette, gettext(locale, "Record call").as_ref(), false)
                        .on_hover_text(record_tip.as_ref()),
                    button(
                        ui,
                        &palette,
                        gettext(locale, "Attach audio").as_ref(),
                        false,
                    )
                    .on_hover_text(attach_tip.as_ref()),
                )
            } else {
                (
                    theme::icon_button(
                        ui,
                        Icon::Phone,
                        17.0,
                        palette.secondary,
                        palette.text,
                        &record_tip,
                    ),
                    theme::icon_button(
                        ui,
                        Icon::Paperclip,
                        17.0,
                        palette.secondary,
                        palette.text,
                        &attach_tip,
                    ),
                )
            };
            if record.clicked() {
                record_call(app, ui.ctx(), lead);
            }
            if attach.clicked() {
                attach_audio(app, ui.ctx(), lead);
            }
        }
    }
}

/// Under the history box: a call with another lead (so it can be ended
/// from here), the reminder to tell the other person, a note when only the
/// microphone is heard, and what went wrong, each on lines of their own.
fn call_lines(app: &mut App, ui: &mut egui::Ui, lead: &Lead) {
    let palette = app.palette;
    let locale = app.locale;
    let (phase, mine, name) = {
        let held = job();
        (held.phase.clone(), held.lead == lead.id, held.name.clone())
    };
    match phase {
        // Kept or discarded from any lead, saved to its own.
        CallPhase::Review(mut take) => {
            ui.add_space(8.0);
            if !mine {
                theme::text(
                    ui,
                    gettext(locale, "Call with {name}").replace("{name}", &name),
                    theme::medium(12.5),
                    palette.secondary,
                );
                ui.add_space(4.0);
            }
            let keep = gettext(locale, "Save and transcribe");
            let choice = super::take::card(app, ui, &mut take, &keep);
            match choice {
                Some(super::take::Choice::Keep) => {
                    let owner = if mine {
                        Some(lead.clone())
                    } else {
                        let id = job().lead.clone();
                        Store::open(&app.dirs.config).get(&id).cloned()
                    };
                    match owner {
                        Some(owner) => keep_call(app, ui.ctx(), &owner, take),
                        // Its lead went: the call is kept as the project's.
                        None => keep_orphan(app, ui.ctx(), take),
                    }
                }
                Some(super::take::Choice::Discard) => {
                    take.discard();
                    set_phase(ui.ctx(), CallPhase::Idle);
                }
                None => {
                    let mut held = job();
                    if let CallPhase::Review(kept) = &mut held.phase {
                        kept.confirming = take.confirming;
                    }
                }
            }
        }
        phase if phase.busy() && !mine => {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                if let CallPhase::Recording(started, _) = phase {
                    if button(ui, &palette, gettext(locale, "End call").as_ref(), false).clicked() {
                        end_call();
                    }
                    let seconds = started.elapsed().as_secs();
                    theme::text(
                        ui,
                        format!("{}:{:02}", seconds / 60, seconds % 60),
                        theme::medium(12.5),
                        palette.danger,
                    );
                    ui.ctx().request_repaint_after(Duration::from_millis(500));
                } else {
                    ui.add(egui::Spinner::new().size(13.0).color(palette.accent));
                }
                // Last, so a long name is what gets cut.
                theme::text(
                    ui,
                    gettext(locale, "Call with {name}").replace("{name}", &name),
                    theme::regular(12.5),
                    palette.secondary,
                );
            });
        }
        CallPhase::Recording(_, mic_only) => {
            ui.add_space(4.0);
            wrapped(
                ui,
                &gettext(locale, "Tell the other person the call is being recorded."),
                theme::regular(12.0),
                palette.dim,
            );
            if mic_only {
                let note = if cfg!(target_os = "macos") {
                    gettext(
                        locale,
                        "Only your microphone is being recorded. To record the other side too, \
                         allow Whats Pro in System Settings > Privacy & Security > Screen & \
                         System Audio Recording.",
                    )
                } else {
                    gettext(
                        locale,
                        "Only your microphone is being recorded: put the call on speaker so the \
                         other side is heard too.",
                    )
                };
                wrapped(ui, &note, theme::regular(12.0), palette.warning);
            }
        }
        CallPhase::Failed(error) if mine => {
            ui.add_space(6.0);
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                theme::icon(ui, Icon::CircleAlert, 14.0, palette.danger);
                ui.vertical(|ui| {
                    ui.set_max_width((ui.available_width() - 30.0).max(120.0));
                    wrapped(ui, &error, theme::regular(12.5), palette.danger);
                });
                if theme::icon_button(
                    ui,
                    Icon::X,
                    13.0,
                    palette.secondary,
                    palette.text,
                    &gettext(locale, "Dismiss"),
                )
                .clicked()
                {
                    let mut held = job();
                    if matches!(held.phase, CallPhase::Failed(_)) {
                        held.phase = CallPhase::Idle;
                    }
                }
            });
        }
        _ => {}
    }
}

/// Asks the analysis's model for a summary of the last month of the chat,
/// to note in the lead's history.
fn summarize(app: &App, ui: &egui::Ui, state: &mut State, lead: &str, chat: &str) {
    state.summarizing = Some(lead.to_owned());
    let archive = app.account().dirs.archive_db();
    let config = app.dirs.config.clone();
    let slot = state.summary.clone();
    let ctx = ui.ctx().clone();
    let (lead, chat) = (lead.to_owned(), chat.to_owned());
    std::thread::spawn(move || {
        let result = summary(&config, &archive, &chat).map_err(|e| e.to_string());
        *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some((lead, result));
        ctx.request_repaint();
    });
}

const SUMMARY_PROMPT: &str = "You write the note a salesperson keeps in their CRM after a WhatsApp conversation with a lead. \
Write it in the language of the conversation, in at most six short lines: what the person wants, what was offered or quoted \
(with amounts and quantities), what was agreed, what is pending and who owes it. Use only what the messages say; \
if something is not there, leave it out. No greeting, no title, no markdown.";

fn summary(
    config: &std::path::Path,
    archive: &std::path::Path,
    chat: &str,
) -> anyhow::Result<String> {
    let since = jiff::Timestamp::now().as_second() - 30 * 86_400;
    let messages = crate::archive::Archive::open_reader(archive)?.bridge_messages(
        Some(chat),
        Some(since),
        None,
        400,
    )?;
    if messages.is_empty() {
        anyhow::bail!("no messages in the last 30 days");
    }
    let mut transcript = String::new();
    for message in &messages {
        let who = if message.from_me {
            "Me".to_owned()
        } else {
            message.sender_name.clone().unwrap_or_else(|| "Them".into())
        };
        let text = match &message.transcript {
            Some(heard) if !heard.trim().is_empty() => format!("{} [{heard}]", message.text),
            _ => message.text.clone(),
        };
        transcript.push_str(&format!(
            "[{}] {who}: {}\n",
            crate::util::copy_stamp(message.timestamp),
            text.trim()
        ));
    }
    let text = crate::projects::engine::ask(config, SUMMARY_PROMPT, &transcript)?;
    let text = text.trim();
    if text.is_empty() {
        anyhow::bail!("the model answered nothing");
    }
    Ok(text.to_owned())
}

// ---------------------------------------------------------------- panel

/// What the panel's cards ask of the project page.
pub enum PanelEvent {
    /// Show the Leads tab, with this lead picked.
    OpenLeads(Option<String>),
}

/// Whether `project` has sales cards to show on its panel.
pub fn has_cards(app: &App, project: &Project) -> bool {
    let leads = Store::open(&app.dirs.config);
    leads.brands.contains_key(&project.id) || leads.of_project(&project.id).next().is_some()
}

/// The Sales card and the brand's inbox, for the top of the panel.
pub fn panel_cards(app: &mut App, ui: &mut egui::Ui, project: &Project) -> Option<PanelEvent> {
    let leads = Store::open(&app.dirs.config).clone();
    let mut event = None;
    if leads.of_project(&project.id).next().is_some() {
        sales_card(app, ui, &leads, project, &mut event);
        ui.add_space(12.0);
    }
    event
}

/// ElevaLeads in the project panel: one quiet line with what waits in the
/// brand's inbox and a link to it. Its inbox lives in ElevaLeads; here it
/// only adds context to what happens in these chats.
pub fn eleva_strip(app: &App, ui: &mut egui::Ui, project: &Project) {
    let palette = app.palette;
    let locale = app.locale;
    let leads = Store::open(&app.dirs.config).clone();
    let Some(brand) = leads.brands.get(&project.id) else {
        return;
    };
    let inbox = eleva::inbox(ui.ctx(), &brand.slug, false);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        theme::icon(ui, Icon::MessageCircle, 14.0, palette.dim);
        theme::text(
            ui,
            gettext(locale, "ElevaLeads · {brand}").replace("{brand}", &brand.name),
            theme::medium(12.5),
            palette.secondary,
        );
        match &inbox {
            Some(inbox) if inbox.error.is_some() => {
                theme::text(
                    ui,
                    gettext(locale, "could not be read"),
                    theme::regular(12.5),
                    palette.dim,
                );
            }
            Some(inbox) => {
                let parts = [
                    (inbox.total, gettext(locale, "{n} waiting")),
                    (inbox.alerts.len(), gettext(locale, "{n} situations")),
                    (inbox.agenda.len(), gettext(locale, "{n} in the agenda")),
                ];
                let text = parts
                    .iter()
                    .filter(|(n, _)| *n > 0)
                    .map(|(n, label)| label.replace("{n}", &n.to_string()))
                    .collect::<Vec<_>>()
                    .join(" · ");
                let text = if text.is_empty() {
                    gettext(locale, "nothing waiting").into_owned()
                } else {
                    text
                };
                theme::text(ui, &text, theme::regular(12.5), palette.dim);
            }
            None => {}
        }
        if theme::link(
            ui,
            gettext(locale, "Open in ElevaLeads"),
            theme::regular(12.5),
            palette.link,
        )
        .clicked()
        {
            ui.ctx()
                .open_url(egui::OpenUrl::new_tab(eleva::inbox_url(&brand.slug, None)));
        }
    });
}

fn sales_card(
    app: &App,
    ui: &mut egui::Ui,
    leads: &Leads,
    project: &Project,
    event: &mut Option<PanelEvent>,
) {
    let palette = app.palette;
    let locale = app.locale;
    let today = leads::today();
    let groups = leads.grouped_with(&project.id, &today, &|lead| {
        turn(app, lead).is_some_and(|turn| turn.theirs)
    });
    let count = |urgency: Urgency| {
        groups
            .iter()
            .find(|(u, _)| *u == urgency)
            .map_or(0, |(_, list)| list.len())
    };
    let quoted = leads
        .of_project(&project.id)
        .filter(|l| l.stage == Stage::Quoted)
        .count();
    card_frame(&palette).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            theme::icon(ui, Icon::Users, 16.0, palette.accent);
            theme::text(
                ui,
                gettext(locale, "Sales").as_ref(),
                theme::semibold(14.5),
                palette.text,
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if button(ui, &palette, gettext(locale, "See leads").as_ref(), false).clicked() {
                    *event = Some(PanelEvent::OpenLeads(None));
                }
            });
        });
        ui.add_space(8.0);
        ui.columns(5, |columns| {
            let cells = [
                (
                    gettext(locale, "Your turn"),
                    count(Urgency::Reply),
                    palette.accent,
                ),
                (
                    gettext(locale, "Late"),
                    count(Urgency::Overdue),
                    palette.danger,
                ),
                (
                    gettext(locale, "For today"),
                    count(Urgency::Today),
                    palette.warning,
                ),
                (gettext(locale, "Quoted"), quoted, palette.warning),
                (
                    gettext(locale, "To collect"),
                    count(Urgency::Collect),
                    palette.warning,
                ),
            ];
            for (column, (label, value, color)) in columns.iter_mut().zip(cells) {
                column.spacing_mut().item_spacing.y = 0.0;
                theme::text(
                    column,
                    label.as_ref(),
                    theme::regular(12.0),
                    palette.secondary,
                );
                theme::text(
                    column,
                    value.to_string(),
                    theme::semibold(22.0),
                    if value > 0 { color } else { palette.dim },
                );
            }
        });
        let urgent: Vec<&Lead> = groups
            .iter()
            .filter(|(u, _)| matches!(u, Urgency::Reply | Urgency::Overdue | Urgency::Today))
            .flat_map(|(_, list)| list.iter().copied())
            .take(4)
            .collect();
        if !urgent.is_empty() {
            ui.add_space(6.0);
        }
        for lead in urgent {
            let response = ui
                .horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if let Some(due) = lead.next.as_ref().and_then(|n| n.due.as_deref()) {
                        let (label, color) = due_label(app, due);
                        chip(ui, &label, color);
                    }
                    theme::text(ui, &lead.name, theme::medium(13.5), palette.text);
                    if let Some(next) = &lead.next {
                        theme::text(ui, &next.text, theme::regular(13.0), palette.secondary);
                    }
                })
                .response
                .interact(Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if response.clicked() {
                *event = Some(PanelEvent::OpenLeads(Some(lead.id.clone())));
            }
        }
        if let Some(id) = super::sales::panel_line(app, ui, leads, project) {
            *event = Some(PanelEvent::OpenLeads(Some(id)));
        }
    });
}

/// A project to open from outside the page (the demo), on its Leads tab or
/// its panel, with a lead picked.
static REQUEST: Mutex<Option<(String, bool, Option<String>)>> = Mutex::new(None);

/// Asks the Projects page to open `project` when it next draws.
pub fn request_open(project: &str, leads_tab: bool, lead: Option<&str>) {
    *REQUEST.lock().unwrap_or_else(|p| p.into_inner()) =
        Some((project.to_owned(), leads_tab, lead.map(str::to_owned)));
}

/// The project asked for, once: its id, whether to show the Leads tab, and
/// the lead to pick.
pub fn take_request() -> Option<(String, bool, Option<String>)> {
    REQUEST.lock().unwrap_or_else(|p| p.into_inner()).take()
}

/// Picks `lead` of `project` in the Leads tab, for a card that opens it.
pub fn select(ctx: &egui::Context, project: &str, lead: Option<String>) {
    let shared = state(ctx);
    let mut state = shared.lock().unwrap_or_else(|p| p.into_inner());
    if state.project != project {
        *state = State {
            project: project.to_owned(),
            ..State::default()
        };
    }
    if lead.is_some() {
        state.selected = lead;
    }
    #[cfg(any(test, feature = "demo"))]
    if std::mem::take(&mut *DEMO_VIEW.lock().unwrap_or_else(|p| p.into_inner())) {
        state.demo_view = Some(Instant::now());
    }
}

#[cfg(any(test, feature = "demo"))]
static DEMO_VIEW: Mutex<bool> = Mutex::new(false);

/// Shows, for the demo, what a call suggested doing next with `lead`, with
/// the history box noting a call and in sight.
#[cfg(any(test, feature = "demo"))]
pub fn demo_call(lead: &str, text: &str, due: Option<String>) {
    job().suggestion = Some((lead.to_owned(), text.to_owned(), due));
    *DEMO_VIEW.lock().unwrap_or_else(|p| p.into_inner()) = true;
}

/// Shows, for the demo, a call with the lead `demo_call` named just ended
/// and waiting to be kept or discarded, instead of what it suggested.
#[cfg(any(test, feature = "demo"))]
pub fn demo_review(name: &str, seconds: u64) {
    let mut held = job();
    if let Some((lead, ..)) = held.suggestion.take() {
        held.lead = lead;
        held.name = name.to_owned();
        held.phase = CallPhase::Review(super::take::Take::new(
            std::env::temp_dir().join("whatspro-demo-call.wav"),
            seconds,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call_library::{CallAnalysis, Commitment};

    fn analysis() -> CallAnalysis {
        CallAnalysis {
            summary: " Quieren 500 guardapolvos bordados. ".into(),
            commitments: vec![
                Commitment {
                    who: "Cliente".into(),
                    what: "Mandar el logo".into(),
                    due: None,
                },
                Commitment {
                    who: "Yo".into(),
                    what: "Mandar la cotización con las dos telas".into(),
                    due: Some("2026-10-20".into()),
                },
                Commitment {
                    who: "Yo".into(),
                    what: " ".into(),
                    due: None,
                },
            ],
            tasks: vec!["Pedir muestras".into()],
            ..CallAnalysis::default()
        }
    }

    #[test]
    fn call_errors_read_in_the_users_language() {
        let locale = Locale::Spanish;
        assert_eq!(
            translated(locale, "Could not open the microphone: busy".into()),
            "No se pudo abrir el micrófono: busy"
        );
        assert_eq!(
            translated(locale, "Nothing was heard in it".into()),
            "No se escuchó nada"
        );
        assert_eq!(translated(locale, "Other: x".into()), "Other: x");
    }

    #[test]
    fn the_next_action_is_what_the_user_promised() {
        assert_eq!(
            next_from(&analysis(), "2026-10-17"),
            Some((
                "Mandar la cotización con las dos telas".into(),
                Some("2026-10-20".into())
            ))
        );
        // A day already gone, or not a date, is left for the user to pick.
        assert_eq!(next_from(&analysis(), "2026-10-21").unwrap().1, None);
        let mut vague = analysis();
        vague.commitments[1].due = Some("el viernes".into());
        assert_eq!(next_from(&vague, "2026-10-17").unwrap().1, None);
        // Nothing promised: the first task, then the first next step.
        let mut tasks = analysis();
        tasks.commitments.clear();
        assert_eq!(
            next_from(&tasks, "2026-10-17"),
            Some(("Pedir muestras".into(), None))
        );
        tasks.tasks.clear();
        tasks.next_steps = vec!["Volver a llamar".into()];
        assert_eq!(
            next_from(&tasks, "2026-10-17").unwrap().0,
            "Volver a llamar"
        );
        tasks.next_steps.clear();
        assert_eq!(next_from(&tasks, "2026-10-17"), None);
    }

    #[test]
    fn numbers_are_read_one_per_line() {
        let parsed = parse_numbers(
            "Laboratorio Ruiz, +54 9 11 5555-0000\n\n+56 9 4073 7324\nnot a number\nAna; 5491122223333",
        );
        assert_eq!(parsed.len(), 4);
        assert_eq!(
            parsed[0],
            Ok(("Laboratorio Ruiz".into(), "5491155550000".into()))
        );
        assert_eq!(parsed[1], Ok(("+56940737324".into(), "56940737324".into())));
        assert!(parsed[2].is_err());
        assert_eq!(parsed[3], Ok(("Ana".into(), "5491122223333".into())));
    }

    #[test]
    fn changes_apply_to_the_project() {
        let mut leads = Leads::default();
        let id = apply(
            &mut leads,
            "p1",
            Change::Add(Box::new(Lead::new(
                "p1",
                "Ana",
                None,
                Some("5491100000000".into()),
            ))),
        )
        .unwrap();
        apply(
            &mut leads,
            "p1",
            Change::Next(id.clone(), "Call".into(), Some("2026-10-01".into())),
        );
        apply(&mut leads, "p1", Change::Done(id.clone()));
        let lead = leads.get(&id).unwrap();
        assert!(lead.next.is_none());
        let n = lead.entries.len();
        assert_eq!(lead.entries[n - 2].text, "✓ Call");
        assert_eq!(
            lead.entries[n - 1].text,
            "contacted",
            "done with someone new: contacted"
        );
        apply(&mut leads, "p1", Change::AddTemplate("Hola {name}".into()));
        apply(
            &mut leads,
            "p1",
            Change::Connect(Brand {
                slug: "a".into(),
                name: "A".into(),
                write: true,
            }),
        );
        apply(
            &mut leads,
            "p1",
            Change::Link(
                id.clone(),
                ElevaCustomer {
                    id: "c".into(),
                    name: "Ana".into(),
                    stage: None,
                    phone: None,
                },
            ),
        );
        // The follow-up and the current stage go; the action it closed does not.
        assert_eq!(leads.get(&id).unwrap().pending(), 2);
        // Another brand: the old links go.
        apply(
            &mut leads,
            "p1",
            Change::Connect(Brand {
                slug: "b".into(),
                name: "B".into(),
                write: false,
            }),
        );
        assert!(leads.get(&id).unwrap().eleva.is_none());
        assert_eq!(leads.templates["p1"], ["Hola {name}"]);
    }
}
