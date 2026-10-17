//! The Projects page: chats grouped by what they are about.
//!
//! The list pane lists the projects as the chat list lists chats (a coloured
//! mark, the latest message of any of its sources, when, and the unread
//! count), then the chats still to file. Beside it, a project has three
//! tabs:
//!
//! - Panel: what the analysis found (what to do, whom to follow up, what is
//!   slipping, needs, messages worth reading) and the summary by period.
//! - Messages: the messages of its sources in one timeline, each shown as in
//!   or out of the project; selected messages are filed in or out by hand,
//!   or into another project.
//! - Sources: the chats it draws on, each taken whole, judged message by
//!   message, or only for the messages picked, with a warning on chats that
//!   also talk about other things.
//!
//! The page keeps its own state in egui's memory (`State`), so the map
//! written by the dashboard shows here and the map changed here shows there.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant, SystemTime},
};

use egui::{Align, Color32, CornerRadius, Frame, Layout, Margin, Rect, Sense, Vec2, pos2, vec2};

use crate::app::App;
use crate::i18n::gettext;
use crate::model::{Action, ChatKind, Page};
use crate::projects::engine::{self, EngineConfig, Provider};
use crate::projects::{
    Analysis, Bucket, Feed, FeedKey, FeedMessage, Finding, Inclusion, Journal, Member, Mode,
    Project, ProjectMap, Ticks, Verdict,
};
use crate::theme::{self, Icon};
use crate::ui::widgets;

/// What the page shows beside the list.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Selection {
    Project(String),
    Unassigned,
    Personal,
    Ignored,
    Engine,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Tab {
    #[default]
    Panel,
    Leads,
    Journal,
    History,
    Context,
    Messages,
    Sources,
}

/// The ranges the messages tab can cover, in days.
const RANGES: [(i64, &str); 4] = [(1, "Today"), (3, "3 days"), (7, "7 days"), (30, "30 days")];

/// Two columns of cards from this width on.
const TWO_COLUMNS: f32 = 860.0;

struct State {
    map: ProjectMap,
    /// When the map file last changed, to pick up the dashboard's edits.
    map_modified: Option<SystemTime>,
    checked: Option<Instant>,
    selection: Option<Selection>,
    tab: Tab,
    days: i64,
    feed: Feed,
    analysis: Option<(String, Instant, Option<Analysis>, Ticks)>,
    period: &'static str,
    /// The messages tab: every message of the sources, or only the
    /// project's; and one source alone.
    show_all: bool,
    only_chat: Option<String>,
    selected: HashSet<(String, String)>,
    /// The sources tab's chat picker.
    picking: bool,
    search: String,
    adding: bool,
    draft: String,
    engine_draft: Option<EngineConfig>,
    key_draft: String,
    model_search: String,
    /// In a narrow window, whether the project (or bucket) beside the list
    /// is on screen instead of the list.
    picked: bool,
    /// The note being written, and a spoken one being recorded or read.
    note: String,
    /// The context being edited, for which project, and a member search.
    context: Option<(String, String)>,
    member_search: String,
    /// A finding just ticked or dismissed, kept a few seconds to undo it
    /// or to say how it was solved.
    pending: Option<Pending>,
    /// The messages a finding came from, shown and marked in Messages.
    highlight: Option<Highlight>,
    note_kind: crate::projects::NoteKind,
    notes_filter: Option<crate::projects::NoteKind>,
    history_filter: Option<Verdict>,
    /// A call being recorded, or a recording or file being read.
    call: Arc<Mutex<CallJob>>,
    /// Calls being read again, by id, with what is happening (or `!error`).
    call_busy: Arc<Mutex<HashMap<String, String>>>,
    call_open: Option<String>,
    renaming: Option<(String, String)>,
}

/// Recording a call or reading an audio file into the project's notes. It
/// runs on a thread of its own: starting waits for macOS to allow the
/// computer's sound, and reading takes a while.
#[derive(Default)]
pub(crate) struct CallJob {
    phase: CallPhase,
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Clone, Default)]
pub(crate) enum CallPhase {
    #[default]
    Idle,
    Starting,
    Recording(Instant),
    /// The file is kept; reading it comes next.
    Kept,
    Reading,
    Saved(Instant),
    Failed(String),
}

/// The recorder outlives the window that started it.
static CALL_JOB: LazyLock<Arc<Mutex<CallJob>>> = LazyLock::new(Arc::default);
static RECORDINGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Recordings ready for review, drained even when the Projects page is closed.
pub(crate) fn drain_recordings() -> Vec<String> {
    std::mem::take(&mut *RECORDINGS.lock().unwrap_or_else(|p| p.into_inner()))
}

/// Ends a meeting recorder before the process leaves. Its thread keeps the
/// audio for review before signalling completion.
pub(crate) fn stop_call_recorder() {
    let thread = {
        let mut job = CALL_JOB.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(stop) = job.stop.take() {
            let _ = stop.send(());
        }
        job.thread.take()
    };
    if let Some(thread) = thread {
        let _ = thread.join();
    }
}

/// How long a ticked or dismissed finding can be undone.
const UNDO_SECONDS: f64 = 5.0;

#[derive(Clone)]
struct Pending {
    finding: Finding,
    verdict: Verdict,
    started: f64,
    note: String,
    /// While the person writes how it was solved, it waits for them.
    writing: bool,
    undone: bool,
}

#[derive(Clone)]
struct Highlight {
    chat: String,
    message: String,
    at: i64,
    text: String,
    scroll: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            map: ProjectMap::default(),
            map_modified: None,
            checked: None,
            selection: None,
            tab: Tab::Panel,
            days: 7,
            feed: Feed::default(),
            analysis: None,
            period: "today",
            show_all: false,
            only_chat: None,
            selected: HashSet::new(),
            picking: false,
            search: String::new(),
            adding: false,
            draft: String::new(),
            engine_draft: None,
            key_draft: String::new(),
            model_search: String::new(),
            picked: false,
            note: String::new(),
            context: None,
            member_search: String::new(),
            pending: None,
            highlight: None,
            note_kind: crate::projects::NoteKind::Log,
            notes_filter: None,
            history_filter: None,
            call: CALL_JOB.clone(),
            call_busy: Arc::default(),
            call_open: None,
            renaming: None,
        }
    }
}

type Shared = Arc<Mutex<State>>;

/// A demo screenshot's starting tab, and whether a finding is pending.
#[cfg(any(test, feature = "demo"))]
pub static DEMO: Mutex<Option<(&'static str, bool)>> = Mutex::new(None);

fn back_id() -> egui::Id {
    egui::Id::new("projects-back")
}

/// Whether a narrow window shows what was picked from the list rather than
/// the list itself.
pub fn picked(ctx: &egui::Context) -> bool {
    let shared = state(ctx);
    let state = shared.lock().unwrap_or_else(|p| p.into_inner());
    state.picked || state.adding
}

fn state(ctx: &egui::Context) -> Shared {
    ctx.data_mut(|data| {
        data.get_temp_mut_or_insert_with(egui::Id::new("projects-state"), Shared::default)
            .clone()
    })
}

/// Reloads the map when its file changed, at most every two seconds.
fn refresh_map(app: &App, state: &mut State) {
    if state
        .checked
        .is_some_and(|at| at.elapsed() < Duration::from_secs(2))
    {
        return;
    }
    state.checked = Some(Instant::now());
    let file = ProjectMap::file(&app.dirs.config);
    let modified = std::fs::metadata(&file).and_then(|m| m.modified()).ok();
    if modified != state.map_modified || state.map_modified.is_none() {
        state.map = ProjectMap::load(&app.dirs.config);
        state.map_modified = modified;
    }
}

fn save_map(app: &App, state: &mut State) {
    if let Err(error) = state.map.save(&app.dirs.config) {
        log::warn!("projects: the map could not be saved: {error}");
    }
    state.map_modified = std::fs::metadata(ProjectMap::file(&app.dirs.config))
        .and_then(|m| m.modified())
        .ok();
}

/// The analysis and ticks of `project`, read again every five seconds.
fn analysis(app: &App, state: &mut State, project: &str) -> (Option<Analysis>, Ticks) {
    let stale = state
        .analysis
        .as_ref()
        .is_none_or(|(id, at, _, _)| id != project || at.elapsed() > Duration::from_secs(5));
    if stale {
        state.analysis = Some((
            project.to_owned(),
            Instant::now(),
            Analysis::load(&app.dirs.config, project),
            Ticks::load(&app.dirs.config, project),
        ));
    }
    let (_, _, analysis, ticks) = state.analysis.as_ref().expect("just loaded");
    (analysis.clone(), ticks.clone())
}

/// A project's latest activity and unread count, from the chats in memory.
fn activity(app: &App, chats: &[String]) -> (i64, u32, Option<String>) {
    let mut last = 0;
    let mut unread = 0;
    let mut preview = None;
    for id in chats {
        let Some(chat) = app.chat(id) else { continue };
        unread += chat.unread;
        if chat.last_activity > last {
            last = chat.last_activity;
            preview = chat.last.as_ref().map(|message| {
                let text = app.resolve_mention_tokens(&message.summary);
                format!("{}: {}", chat.name, text)
            });
        }
    }
    (last, unread, preview)
}

fn initials(name: &str) -> String {
    let mut letters = name
        .split_whitespace()
        .filter_map(|word| word.chars().find(|c| c.is_alphanumeric()))
        .take(2)
        .collect::<String>();
    if letters.is_empty() {
        letters.push('#');
    }
    letters.to_uppercase()
}

/// A project's mark: its initials on its colour, the size of an avatar.
fn paint_mark(ui: &egui::Ui, rect: Rect, color: Color32, name: &str) {
    ui.painter()
        .circle_filled(rect.center(), rect.width() / 2.0, color);
    let galley = ui.painter().layout_no_wrap(
        initials(name),
        theme::semibold(rect.width() * 0.36),
        Color32::from_rgb(0x0b, 0x14, 0x1a),
    );
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, Color32::WHITE);
}

fn card_fill(palette: &theme::Palette) -> Color32 {
    if palette.vibrant() {
        palette.tint(theme::TINT_FIELD)
    } else {
        palette.surface
    }
}

fn card_frame(palette: &theme::Palette) -> Frame {
    Frame::new()
        .fill(card_fill(palette))
        .stroke(palette.glass_edge())
        .corner_radius(CornerRadius::same(theme::RADIUS + 6))
        .inner_margin(Margin::same(14))
}

/// A card's title row: an icon, the title, a count, and whatever `right`
/// draws at its end.
fn card_title(
    ui: &mut egui::Ui,
    palette: &theme::Palette,
    icon: Icon,
    title: &str,
    color: Color32,
    right: impl FnOnce(&mut egui::Ui),
) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        theme::icon(ui, icon, 16.0, color);
        theme::text(ui, title, theme::semibold(14.0), color);
        ui.with_layout(Layout::right_to_left(Align::Center), right);
    });
    ui.add_space(6.0);
    let _ = palette;
}

/// `add` in a column `width` wide, beside what a row already holds. Setting
/// a row's own width would move its cursor back to the start and draw over
/// what came before.
fn column<R>(ui: &mut egui::Ui, width: f32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let width = width.max(40.0);
    ui.allocate_ui_with_layout(vec2(width, 0.0), Layout::top_down(Align::Min), |ui| {
        ui.set_max_width(width);
        add(ui)
    })
    .inner
}

fn wrapped(ui: &mut egui::Ui, text: &str, font: egui::FontId, color: Color32) {
    ui.add(
        egui::Label::new(egui::RichText::new(text).font(font).color(color))
            .wrap()
            .selectable(false),
    );
}

fn ago(locale: crate::i18n::Locale, seconds: i64) -> String {
    let delta = (jiff::Timestamp::now().as_second() - seconds).max(0);
    if delta < 3600 {
        gettext(locale, "{n} min").replace("{n}", &(delta / 60).max(1).to_string())
    } else if delta < 86_400 {
        gettext(locale, "{n} h").replace("{n}", &(delta / 3600).to_string())
    } else {
        gettext(locale, "{n} d").replace("{n}", &(delta / 86_400).to_string())
    }
}

fn pill_button(
    ui: &mut egui::Ui,
    palette: &theme::Palette,
    label: &str,
    primary: bool,
) -> egui::Response {
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), theme::semibold(13.5), palette.text);
    let width = (galley.size().x + 28.0).max(88.0);
    let (rect, response) = ui.allocate_exact_size(vec2(width, 32.0), Sense::click());
    theme::reveal_focus(&response);
    let (fill, text) = if primary {
        (
            if response.hovered() {
                palette.accent_hover
            } else {
                palette.accent
            },
            palette.on_accent,
        )
    } else {
        (
            if response.hovered() {
                palette.surface_hover
            } else {
                card_fill(palette)
            },
            palette.text,
        )
    };
    ui.painter().rect_filled(rect, 16.0, fill);
    if !primary {
        ui.painter()
            .rect_stroke(rect, 16.0, palette.glass_edge(), egui::StrokeKind::Inside);
    }
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), theme::semibold(13.5), text);
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, text);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Three chips that pick one of `options`.
fn segmented<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    palette: &theme::Palette,
    options: &[(T, String)],
    current: T,
) -> Option<T> {
    let mut chosen = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (value, label) in options {
            if widgets::filter_chip(ui, palette, label, 0, *value == current).clicked()
                && *value != current
            {
                chosen = Some(*value);
            }
        }
    });
    chosen
}

fn mode_label(locale: crate::i18n::Locale, mode: Mode) -> String {
    match mode {
        Mode::All => gettext(locale, "Everything"),
        Mode::Smart => gettext(locale, "Smart"),
        Mode::Selected => gettext(locale, "Picked"),
    }
    .into_owned()
}

/// Why a chat may bring things that have nothing to do with a project.
fn caution(app: &App, map: &ProjectMap, project: &str, chat: &str) -> Option<String> {
    let locale = app.locale;
    let elsewhere: Vec<String> = map
        .projects_of(chat)
        .into_iter()
        .filter(|(other, _)| other.id != project)
        .map(|(other, _)| other.name.clone())
        .collect();
    if !elsewhere.is_empty() {
        return Some(
            gettext(locale, "Also in {projects}").replace("{projects}", &elsewhere.join(", ")),
        );
    }
    (ChatKind::from_id(chat) == ChatKind::Direct)
        .then(|| gettext(locale, "A chat with one person: other topics come up too").into_owned())
}

// ---------------------------------------------------------------- list pane

/// A project row: id, name, colour, latest activity, unread messages, the
/// latest message, and the open items the analysis found.
type ListRow = (String, String, Color32, i64, u32, Option<String>, usize);

/// The list pane on the Projects page.
pub fn list(app: &mut App, ui: &mut egui::Ui) {
    engine::start(ui.ctx(), &app.dirs.config, &app.account().dirs.archive_db());
    let shared = state(ui.ctx());
    let mut state = shared.lock().unwrap_or_else(|p| p.into_inner());
    refresh_map(app, &mut state);
    let palette = app.palette;
    let locale = app.locale;
    if state.selection.is_none() {
        state.selection = Some(match state.map.projects.first() {
            Some(project) => Selection::Project(project.id.clone()),
            None => Selection::Unassigned,
        });
    }

    let mut rows: Vec<ListRow> = state
        .map
        .projects
        .iter()
        .map(|project| {
            let (last, unread, preview) = activity(app, &project.chat_ids());
            let open = Analysis::load(&app.dirs.config, &project.id).map_or(0, |analysis| {
                let ticks = Ticks::load(&app.dirs.config, &project.id);
                analysis
                    .todo
                    .iter()
                    .chain(&analysis.follow_up)
                    .chain(&analysis.leaks)
                    .filter(|finding| ticks.open(&finding.id))
                    .count()
            });
            (
                project.id.clone(),
                project.name.clone(),
                project.color(),
                last,
                unread,
                preview,
                open,
            )
        })
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.3));
    let unassigned = app
        .chats
        .iter()
        .filter(|chat| chat.last.is_some() && state.map.bucket_of(&chat.id) == Bucket::Unassigned)
        .count();
    let personal = state.map.personal.len();
    let ignored = state.map.ignored.len();

    let mut picked = None;
    egui::ScrollArea::vertical()
        .id_salt("projects-list")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.add_space(4.0);
            if rows.is_empty() {
                Frame::new()
                    .inner_margin(Margin::symmetric(24, 0))
                    .show(ui, |ui| {
                        widgets::empty_state(
                            ui,
                            &palette,
                            Icon::Tag,
                            gettext(locale, "No projects yet").as_ref(),
                            gettext(locale, "Create one with + and file your chats in it.")
                                .as_ref(),
                        );
                    });
            }
            for (id, name, color, last, unread, preview, open) in &rows {
                let selected = state.selection == Some(Selection::Project(id.clone()));
                if project_row(
                    ui, &palette, locale, name, *color, *last, *unread, preview, *open, selected,
                ) {
                    picked = Some(Selection::Project(id.clone()));
                }
            }
            ui.add_space(10.0);
            section_label(ui, &palette, gettext(locale, "Organize").as_ref());
            for (selection, icon, label, count) in [
                (
                    Selection::Unassigned,
                    Icon::ListChecks,
                    gettext(locale, "To file"),
                    Some(unassigned),
                ),
                (
                    Selection::Personal,
                    Icon::User,
                    gettext(locale, "Personal"),
                    Some(personal),
                ),
                (
                    Selection::Ignored,
                    Icon::EyeOff,
                    gettext(locale, "Ignored"),
                    Some(ignored),
                ),
                (
                    Selection::Engine,
                    Icon::Settings,
                    gettext(locale, "Analysis"),
                    None,
                ),
            ] {
                let selected = state.selection.as_ref() == Some(&selection);
                if bucket_row(ui, &palette, icon, label.as_ref(), count, selected) {
                    picked = Some(selection);
                }
            }
            ui.add_space(12.0);
        });
    if let Some(selection) = picked {
        if state.selection.as_ref() != Some(&selection) {
            state.selected.clear();
            state.only_chat = None;
            state.picking = false;
        }
        state.selection = Some(selection);
        state.picked = true;
    }
}

fn section_label(ui: &mut egui::Ui, palette: &theme::Palette, label: &str) {
    Frame::new()
        .inner_margin(Margin {
            left: 16,
            right: 14,
            top: 4,
            bottom: 6,
        })
        .show(ui, |ui| {
            theme::text(ui, label, theme::medium(13.0), palette.accent);
        });
}

/// The list pane's header button: a new project.
pub fn add_button(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    let label = gettext(app.locale, "New project");
    if theme::icon_button(
        ui,
        Icon::Plus,
        18.0,
        palette.secondary,
        palette.text,
        &label,
    )
    .clicked()
    {
        let shared = state(ui.ctx());
        let mut state = shared.lock().unwrap_or_else(|p| p.into_inner());
        state.adding = true;
        state.draft.clear();
    }
}

#[allow(clippy::too_many_arguments)]
fn project_row(
    ui: &mut egui::Ui,
    palette: &theme::Palette,
    locale: crate::i18n::Locale,
    name: &str,
    color: Color32,
    last: i64,
    unread: u32,
    preview: &Option<String>,
    open: usize,
    selected: bool,
) -> bool {
    let (rect, response) = ui.allocate_exact_size(
        vec2(ui.available_width(), theme::ROW_HEIGHT),
        Sense::click(),
    );
    theme::reveal_focus(&response);
    if ui.is_rect_visible(rect) {
        if selected {
            widgets::row_highlight(ui, palette, rect, palette.surface_active);
        } else if response.hovered() {
            widgets::row_highlight(ui, palette, rect, palette.surface_hover);
        }
        let mark =
            Rect::from_center_size(pos2(rect.left() + 38.0, rect.center().y), Vec2::splat(48.0));
        paint_mark(ui, mark, color, name);
        let left = rect.left() + 76.0;
        let stamp = if last > 0 {
            crate::util::chat_stamp(locale, last)
        } else {
            String::new()
        };
        let stamp_line = widgets::line(
            ui,
            &stamp,
            theme::regular(12.0),
            if unread > 0 {
                palette.accent
            } else {
                palette.dim
            },
            80.0,
            1,
        );
        let stamp_width = stamp_line.size().x;
        let right = rect.right() - 16.0;
        stamp_line.paint(
            ui,
            pos2(right - stamp_width, rect.top() + 16.0),
            palette.dim,
        );
        let width = (right - stamp_width - 8.0 - left).max(0.0);
        widgets::line(ui, name, theme::medium(15.0), palette.text, width, 1).paint(
            ui,
            pos2(left, rect.top() + 13.0),
            palette.text,
        );
        // Unread messages, then the open items the analysis found.
        let mut badges = 0.0;
        if unread > 0 {
            badges += widgets::badge(
                ui,
                palette,
                pos2(right - 12.0, rect.top() + 46.0),
                unread,
                false,
            );
        }
        if open > 0 {
            let label = open.to_string();
            let galley = ui
                .painter()
                .layout_no_wrap(label, theme::semibold(11.0), palette.warning);
            let width = (galley.size().x + 12.0).max(20.0);
            let center = pos2(right - badges - 6.0 - width / 2.0, rect.top() + 46.0);
            let pill = Rect::from_center_size(center, vec2(width, 20.0));
            ui.painter().rect_stroke(
                pill,
                10.0,
                egui::Stroke::new(1.0, palette.warning),
                egui::StrokeKind::Inside,
            );
            ui.painter()
                .galley(pill.center() - galley.size() / 2.0, galley, palette.warning);
            badges += width + 6.0;
        }
        if let Some(preview) = preview {
            let width = (right - badges - 8.0 - left).max(0.0);
            widgets::line(
                ui,
                preview,
                theme::regular(13.5),
                palette.secondary,
                width,
                1,
            )
            .paint(ui, pos2(left, rect.top() + 37.0), palette.secondary);
        }
    }
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
}

fn bucket_row(
    ui: &mut egui::Ui,
    palette: &theme::Palette,
    icon: Icon,
    label: &str,
    count: Option<usize>,
    selected: bool,
) -> bool {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 48.0), Sense::click());
    theme::reveal_focus(&response);
    if ui.is_rect_visible(rect) {
        if selected {
            widgets::row_highlight(ui, palette, rect, palette.surface_active);
        } else if response.hovered() {
            widgets::row_highlight(ui, palette, rect, palette.surface_hover);
        }
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect.shrink2(vec2(28.0, 0.0)))
                .layout(Layout::left_to_right(Align::Center)),
        );
        theme::icon(&mut child, icon, 18.0, palette.secondary);
        child.add_space(14.0);
        theme::text(&mut child, label, theme::regular(14.5), palette.text);
        if let Some(count) = count {
            child.with_layout(Layout::right_to_left(Align::Center), |ui| {
                theme::text(ui, count.to_string(), theme::regular(12.5), palette.dim);
            });
        }
    }
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
}

// ---------------------------------------------------------------- main pane

/// The pane beside the list on the Projects page.
pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let shared = state(ui.ctx());
    let mut state = shared.lock().unwrap_or_else(|p| p.into_inner());
    if ui
        .ctx()
        .data_mut(|data| data.remove_temp::<bool>(back_id()))
        .is_some()
    {
        state.picked = false;
        state.adding = false;
        ui.ctx().request_repaint();
    }
    refresh_map(app, &mut state);
    #[cfg(any(test, feature = "demo"))]
    if let Some((tab, pending)) = DEMO.lock().unwrap_or_else(|p| p.into_inner()).take()
        && let Some(project) = state.map.projects.first().cloned()
    {
        state.selection = Some(Selection::Project(project.id.clone()));
        state.picked = true;
        state.tab = match tab {
            "notes" => Tab::Journal,
            "history" => Tab::History,
            "context" => Tab::Context,
            "messages" => Tab::Messages,
            "sources" => Tab::Sources,
            _ => Tab::Panel,
        };
        if pending
            && let Some(finding) =
                Analysis::load(&app.dirs.config, &project.id).and_then(|a| a.todo.first().cloned())
        {
            state.pending = Some(Pending {
                finding,
                verdict: Verdict::Done,
                started: ui.ctx().input(|i| i.time),
                note: String::new(),
                writing: false,
                undone: false,
            });
        }
    }
    if let Some((project, leads_tab, lead)) = super::leads::take_request() {
        state.tab = if leads_tab { Tab::Leads } else { Tab::Panel };
        super::leads::select(ui.ctx(), &project, lead);
        state.selection = Some(Selection::Project(project));
        state.picked = true;
    }
    if state.adding {
        new_project(app, ui, &mut state);
        return;
    }
    match state.selection.clone() {
        Some(Selection::Project(id)) if state.map.project(&id).is_some() => {
            project(app, ui, &mut state, &id)
        }
        Some(Selection::Engine) => engine_page(app, ui, &mut state),
        Some(Selection::Personal) => bucket(app, ui, &mut state, Bucket::Personal),
        Some(Selection::Ignored) => bucket(app, ui, &mut state, Bucket::Ignored),
        _ => bucket(app, ui, &mut state, Bucket::Unassigned),
    }
}

/// The header across the top of the pane: a mark, a title and a line under
/// it, and whatever `right` draws at its end.
fn header(
    app: &App,
    ui: &mut egui::Ui,
    mark: Option<(Color32, &str)>,
    title: &str,
    subtitle: &str,
    right: impl FnOnce(&mut egui::Ui),
) {
    let palette = app.palette;
    let (rect, _) = ui.allocate_exact_size(
        vec2(ui.available_width(), theme::TOP_BAR_HEIGHT),
        Sense::hover(),
    );
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(vec2(20.0, 0.0)))
            .layout(Layout::left_to_right(Align::Center)),
    );
    // A narrow window shows the page instead of the list: Back returns.
    if super::narrow(ui.ctx())
        && theme::icon_button(
            &mut child,
            Icon::ArrowLeft,
            18.0,
            palette.secondary,
            palette.text,
            gettext(app.locale, "Back to the projects").as_ref(),
        )
        .clicked()
    {
        // The page's state is held while it draws; `show` reads this next.
        child
            .ctx()
            .data_mut(|data| data.insert_temp(back_id(), true));
    }
    if let Some((color, name)) = mark {
        let (mark, _) = child.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
        paint_mark(&child, mark, color, name);
        child.add_space(12.0);
    }
    child.vertical(|ui| {
        ui.add_space(10.0);
        ui.spacing_mut().item_spacing.y = 1.0;
        theme::text(ui, title, theme::semibold(16.0), palette.text);
        theme::text(ui, subtitle, theme::regular(12.5), palette.secondary);
    });
    child.with_layout(Layout::right_to_left(Align::Center), right);
    ui.painter()
        .hline(rect.x_range(), rect.bottom() - 0.5, palette.glass_edge());
}

fn project(app: &mut App, ui: &mut egui::Ui, state: &mut State, id: &str) {
    let palette = app.palette;
    let locale = app.locale;
    let Some(project) = state.map.project(id).cloned() else {
        return;
    };
    let (analysis, ticks) = analysis(app, state, id);
    let status = engine::status();
    let subtitle = {
        let sources = gettext(locale, "{count} sources")
            .replace("{count}", &project.sources.len().to_string());
        let when = match (&status.running, &analysis) {
            (Some(running), _) if *running == project.name => {
                gettext(locale, "analyzing…").into_owned()
            }
            (_, Some(analysis)) if analysis.updated > 0 => gettext(locale, "updated {when} ago")
                .replace("{when}", &ago(locale, analysis.updated)),
            _ => gettext(locale, "not analyzed yet").into_owned(),
        };
        format!("{sources} · {when}")
    };
    let mut tab = state.tab;
    let mut remove = false;
    let mut refresh = false;
    let analyzing =
        status.running.as_deref() == Some(project.name.as_str()) || engine::pending(&project.id);
    header(
        app,
        ui,
        Some((project.color(), &project.name)),
        &project.name,
        &subtitle,
        |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let more = theme::icon_button(
                ui,
                Icon::Ellipsis,
                18.0,
                palette.secondary,
                palette.text,
                &gettext(locale, "Project options"),
            );
            let label = gettext(locale, "Delete project");
            egui::Popup::menu(&more)
                .width(widgets::menu_width(ui, &[label.as_ref()], true))
                .frame(widgets::menu_frame(&palette))
                .show(|ui| {
                    if widgets::menu_item(ui, &palette, Some(Icon::Trash), &label) {
                        remove = true;
                        ui.close();
                    }
                });
            if analyzing {
                ui.add(egui::Spinner::new().size(16.0).color(palette.accent));
            } else if theme::icon_button(
                ui,
                Icon::Refresh,
                17.0,
                palette.secondary,
                palette.text,
                &gettext(locale, "Update the analysis"),
            )
            .clicked()
            {
                refresh = true;
            }
            ui.add_space(6.0);
            for (value, label) in [
                (Tab::Sources, gettext(locale, "Sources")),
                (Tab::Messages, gettext(locale, "Messages")),
                (Tab::Context, gettext(locale, "Context")),
                (Tab::History, gettext(locale, "History")),
                (Tab::Journal, gettext(locale, "Notes")),
                (Tab::Leads, gettext(locale, "Leads")),
                (Tab::Panel, gettext(locale, "Panel")),
            ] {
                if widgets::filter_chip(ui, &palette, label.as_ref(), 0, tab == value).clicked() {
                    tab = value;
                }
            }
        },
    );
    if tab != state.tab {
        state.tab = tab;
        state.picking = false;
    }
    if remove {
        state.map.remove(id);
        save_map(app, state);
        // Its leads, brand and saved messages go with it.
        // Through the shared store: its cached copy would bring them back.
        let mut leads = crate::leads::Store::open(&app.dirs.config);
        leads.forget_project(id);
        leads.save();
        state.selection = None;
        return;
    }
    if refresh {
        engine::refresh(&project.id);
    }
    match state.tab {
        Tab::Panel => panel(app, ui, state, &project, analysis.as_ref(), &ticks),
        Tab::Leads => super::leads::tab(app, ui, &project),
        Tab::Journal => journal(app, ui, state, &project),
        Tab::History => history(app, ui, state, &project, &ticks),
        Tab::Context => context(app, ui, state, &project),
        Tab::Messages => messages(app, ui, state, &project, analysis.as_ref()),
        Tab::Sources => sources(app, ui, state, &project),
    }
}

// -------------------------------------------------------------------- panel

/// How old a finding is, coloured by how long it has waited.
fn age_chip(app: &App, since: i64) -> (String, Color32) {
    let palette = app.palette;
    let days = (jiff::Timestamp::now().as_second() - since) / 86_400;
    let color = if days >= 7 {
        palette.danger
    } else if days >= 3 {
        palette.warning
    } else {
        palette.dim
    };
    (ago(app.locale, since), color)
}

/// A small rounded label.
pub(crate) fn chip(ui: &mut egui::Ui, text: &str, color: Color32) {
    Frame::new()
        .fill(color.gamma_multiply(0.14))
        .corner_radius(CornerRadius::same(theme::RADIUS))
        .inner_margin(Margin::symmetric(7, 2))
        .show(ui, |ui| {
            theme::text(ui, text, theme::medium(11.5), color);
        });
}

/// What the person did to a finding.
enum FindingAction {
    Judge(Finding, Verdict),
    Open(String),
    /// Show the messages it came from.
    Show(Finding),
}

/// One finding: a tick box where it can be done, its text, and chips for
/// who, where, how long ago and when due. On hover, a way to open its
/// message's chat and to dismiss it; a right click also judges it as good
/// or bad advice. The row's own response is registered before its
/// contents, from where it was last drawn, so the box and buttons on it
/// take their own clicks.
fn finding_row(
    app: &App,
    ui: &mut egui::Ui,
    finding: &Finding,
    tickable: bool,
    ticks: &Ticks,
    action: &mut Option<FindingAction>,
    pending: &mut Option<Pending>,
) {
    let palette = app.palette;
    let locale = app.locale;
    if let Some(pending) = pending.as_mut().filter(|p| p.finding.id == finding.id) {
        pending_row(app, ui, pending, action);
        return;
    }
    let id = ui.id().with(("finding", &finding.id));
    let previous = ui.ctx().data(|data| data.get_temp::<Rect>(id));
    let row_response = previous.map(|rect| ui.interact(rect, id, Sense::click()));
    let hovered = row_response.as_ref().is_some_and(|r| r.hovered())
        || previous.is_some_and(|rect| ui.rect_contains_pointer(rect));
    if let Some(rect) = previous
        && hovered
    {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(theme::RADIUS + 2),
            palette.tint(0.04),
        );
    }
    let verdict = ticks.verdict(&finding.id);
    let chat_name = finding
        .chat
        .as_ref()
        .and_then(|chat| app.chat(chat))
        .map(|chat| chat.name.clone());
    let row = Frame::new()
        .inner_margin(Margin::symmetric(8, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                if tickable {
                    let (rect, response) =
                        ui.allocate_exact_size(Vec2::splat(20.0), Sense::click());
                    let over = response.hovered();
                    ui.painter().circle_stroke(
                        rect.center(),
                        8.5,
                        egui::Stroke::new(1.6, if over { palette.accent } else { palette.dim }),
                    );
                    if over {
                        theme::icon(
                            &mut ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink(3.0))),
                            Icon::Check,
                            14.0,
                            palette.accent,
                        );
                    }
                    if response
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text(gettext(locale, "Mark as done"))
                        .clicked()
                    {
                        *action = Some(FindingAction::Judge(finding.clone(), Verdict::Done));
                    }
                } else {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::hover());
                    theme::icon(
                        &mut ui.new_child(egui::UiBuilder::new().max_rect(rect)),
                        Icon::Star,
                        16.0,
                        palette.warning,
                    );
                }
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 5.0;
                    ui.set_max_width((ui.available_width() - 64.0).max(120.0));
                    wrapped(ui, &finding.text, theme::regular(14.0), palette.text);
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = vec2(5.0, 4.0);
                        if let Some(who) = &finding.who {
                            chip(ui, who, palette.accent);
                        }
                        if let Some(name) = &chat_name {
                            chip(ui, name, palette.secondary);
                        }
                        if let Some(since) = finding.since {
                            let (label, color) = age_chip(app, since);
                            chip(ui, &label, color);
                        }
                        if let Some(due) = &finding.due {
                            chip(ui, &format!("⏰ {due}"), palette.warning);
                        }
                        match verdict {
                            Some(Verdict::Good) => chip(ui, "👍", palette.accent),
                            Some(Verdict::Bad) => chip(ui, "👎", palette.danger),
                            _ => {}
                        }
                    });
                });
                ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    if !hovered {
                        return;
                    }
                    if theme::icon_button(
                        ui,
                        Icon::X,
                        14.0,
                        palette.dim,
                        palette.text,
                        &gettext(locale, "Dismiss"),
                    )
                    .clicked()
                    {
                        *action = Some(FindingAction::Judge(finding.clone(), Verdict::Dismissed));
                    }
                    if let Some(chat) = &finding.chat
                        && theme::icon_button(
                            ui,
                            Icon::ExternalLink,
                            14.0,
                            palette.dim,
                            palette.text,
                            &gettext(locale, "Open the chat"),
                        )
                        .clicked()
                    {
                        *action = Some(FindingAction::Open(chat.clone()));
                    }
                });
            });
        });
    ui.ctx()
        .data_mut(|data| data.insert_temp(id, row.response.rect));
    if previous.is_none() {
        ui.ctx().request_repaint();
    }
    if let Some(response) = &row_response
        && finding.message.is_some()
        && action.is_none()
        && response
            .clone()
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(gettext(locale, "See the messages it came from"))
            .clicked()
    {
        *action = Some(FindingAction::Show(finding.clone()));
    }
    if let Some(response) = row_response {
        let done = gettext(locale, "Mark as done");
        let dismiss = gettext(locale, "Dismiss");
        let good = gettext(locale, "Good recommendation");
        let bad = gettext(locale, "Bad recommendation");
        let open = gettext(locale, "Open the chat");
        let labels = [
            done.as_ref(),
            dismiss.as_ref(),
            good.as_ref(),
            bad.as_ref(),
            open.as_ref(),
        ];
        egui::Popup::context_menu(&response)
            .width(widgets::menu_width(ui, &labels, true))
            .frame(widgets::menu_frame(&palette))
            .show(|ui| {
                let mut pick = |ui: &mut egui::Ui, icon, label: &str, verdict| {
                    if widgets::menu_item(ui, &palette, Some(icon), label) {
                        *action = Some(FindingAction::Judge(finding.clone(), verdict));
                        ui.close();
                    }
                };
                if tickable {
                    pick(ui, Icon::Check, &done, Verdict::Done);
                }
                pick(ui, Icon::X, &dismiss, Verdict::Dismissed);
                widgets::menu_separator(ui, &palette);
                pick(ui, Icon::Heart, &good, Verdict::Good);
                pick(ui, Icon::Ban, &bad, Verdict::Bad);
                if finding.message.is_some() {
                    widgets::menu_separator(ui, &palette);
                    if widgets::menu_item(
                        ui,
                        &palette,
                        Some(Icon::MessageCircle),
                        &gettext(locale, "See the messages it came from"),
                    ) {
                        *action = Some(FindingAction::Show(finding.clone()));
                        ui.close();
                    }
                }
                if let Some(chat) = &finding.chat {
                    widgets::menu_separator(ui, &palette);
                    if widgets::menu_item(ui, &palette, Some(Icon::ExternalLink), &open) {
                        *action = Some(FindingAction::Open(chat.clone()));
                        ui.close();
                    }
                }
            });
    }
}

/// A finding just ticked or dismissed: it can be undone for a few seconds,
/// and a ticked one can say how it was solved, which waits for the person.
fn pending_row(
    app: &App,
    ui: &mut egui::Ui,
    pending: &mut Pending,
    action: &mut Option<FindingAction>,
) {
    let palette = app.palette;
    let locale = app.locale;
    let now = ui.ctx().input(|input| input.time);
    let left = (UNDO_SECONDS - (now - pending.started)).max(0.0).ceil() as i64;
    let done = pending.verdict == Verdict::Done;
    let color = if done {
        palette.accent
    } else {
        palette.secondary
    };
    Frame::new()
        .fill(color.gamma_multiply(0.10))
        .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.35)))
        .corner_radius(CornerRadius::same(theme::RADIUS + 2))
        .inner_margin(Margin::symmetric(12, 10))
        .outer_margin(Margin::symmetric(0, 4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // What happened to it, and the way back.
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                theme::icon(
                    ui,
                    if done { Icon::CircleCheck } else { Icon::X },
                    16.0,
                    color,
                );
                let label = if done {
                    gettext(locale, "Done")
                } else {
                    gettext(locale, "Dismissed")
                };
                theme::text(ui, label, theme::semibold(14.0), color);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    if pill_button(ui, &palette, gettext(locale, "Undo").as_ref(), false).clicked()
                    {
                        pending.undone = true;
                    }
                    if !pending.writing && pending.note.trim().is_empty() {
                        theme::text(
                            ui,
                            gettext(locale, "closes in {n} s").replace("{n}", &left.to_string()),
                            theme::regular(12.5),
                            palette.secondary,
                        );
                    }
                });
            });
            ui.add_space(4.0);
            wrapped(
                ui,
                &pending.finding.text,
                theme::regular(13.5),
                palette.secondary,
            );
            if done {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let width = ui.available_width() - 100.0;
                    let field = column(ui, width, |ui| {
                        Frame::new()
                            .fill(palette.surface)
                            .stroke(palette.glass_edge())
                            .corner_radius(CornerRadius::same(theme::RADIUS))
                            .inner_margin(Margin::symmetric(10, 6))
                            .show(ui, |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut pending.note)
                                        .hint_text(
                                            gettext(
                                                locale,
                                                "How was it solved? (optional, helps the analysis)",
                                            )
                                            .as_ref(),
                                        )
                                        .font(theme::regular(13.5))
                                        .frame(Frame::NONE)
                                        .desired_width(f32::INFINITY),
                                )
                            })
                            .inner
                    });
                    if field.has_focus() || !pending.note.trim().is_empty() {
                        pending.writing = true;
                    }
                    let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if pending.writing
                        && (pill_button(ui, &palette, gettext(locale, "Save").as_ref(), true)
                            .clicked()
                            || enter)
                    {
                        pending.writing = false;
                        pending.started = f64::NEG_INFINITY;
                    }
                });
            }
        });
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(250));
    let _ = action;
}

#[allow(clippy::too_many_arguments)]
fn findings_card(
    app: &App,
    ui: &mut egui::Ui,
    icon: Icon,
    title: &str,
    color: Color32,
    findings: &[Finding],
    ticks: &Ticks,
    tickable: bool,
    empty: &str,
    action: &mut Option<FindingAction>,
    pending: &mut Option<Pending>,
) {
    let palette = app.palette;
    // A finding waiting to be closed stays in place until it is.
    let waiting = pending.as_ref().map(|p| p.finding.id.clone());
    let open: Vec<&Finding> = findings
        .iter()
        .filter(|f| ticks.open(&f.id) || waiting.as_deref() == Some(f.id.as_str()))
        .collect();
    let closed = findings.len() - open.len();
    card_frame(&palette).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            theme::icon(ui, icon, 16.0, color);
            theme::text(ui, title, theme::semibold(14.5), palette.text);
            if !open.is_empty() {
                chip(ui, &open.len().to_string(), color);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if closed > 0 {
                    theme::text(
                        ui,
                        gettext(app.locale, "{n} closed").replace("{n}", &closed.to_string()),
                        theme::regular(12.0),
                        palette.dim,
                    );
                }
            });
        });
        ui.add_space(4.0);
        if open.is_empty() {
            ui.horizontal(|ui| {
                theme::icon(ui, Icon::CircleCheck, 14.0, palette.dim);
                theme::text(ui, empty, theme::regular(13.0), palette.dim);
            });
        }
        for (index, finding) in open.iter().enumerate() {
            if index > 0 {
                let x = ui.max_rect().x_range();
                let y = ui.cursor().top();
                ui.painter().hline(x, y, palette.glass_edge());
            }
            finding_row(app, ui, finding, tickable, ticks, action, pending);
        }
    });
    ui.add_space(12.0);
}

fn metric(
    ui: &mut egui::Ui,
    palette: &theme::Palette,
    icon: Icon,
    label: &str,
    value: usize,
    color: Color32,
) {
    Frame::new()
        .fill(card_fill(palette))
        .stroke(palette.glass_edge())
        .corner_radius(CornerRadius::same(theme::RADIUS + 4))
        .inner_margin(Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                theme::icon(ui, icon, 14.0, if value > 0 { color } else { palette.dim });
                theme::text(ui, label, theme::regular(12.5), palette.secondary);
            });
            theme::text(
                ui,
                value.to_string(),
                theme::semibold(24.0),
                if value > 0 { color } else { palette.dim },
            );
        });
}

fn panel(
    app: &mut App,
    ui: &mut egui::Ui,
    state: &mut State,
    project: &Project,
    analysis: Option<&Analysis>,
    ticks: &Ticks,
) {
    let palette = app.palette;
    let locale = app.locale;
    let Some(analysis) = analysis else {
        if super::leads::has_cards(app, project) {
            egui::ScrollArea::vertical()
                .id_salt(("project-panel-sales", &project.id))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    Frame::new()
                        .inner_margin(Margin::symmetric(20, 16))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            sales_cards(app, ui, state, project);
                        });
                    no_analysis(app, ui, state, project);
                });
        } else {
            no_analysis(app, ui, state, project);
        }
        return;
    };
    let open = |list: &[Finding]| list.iter().filter(|f| ticks.open(&f.id)).count();
    let mut action = None;
    let mut period = state.period;
    let mut pending = state.pending.take();
    egui::ScrollArea::vertical()
        .id_salt(("project-panel", &project.id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            Frame::new()
                .inner_margin(Margin::symmetric(20, 16))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    // First what happened, then the numbers, then the work.
                    card_frame(&palette).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            theme::icon(ui, Icon::Calendar, 16.0, palette.accent);
                            theme::text(
                                ui,
                                gettext(locale, "What happened"),
                                theme::semibold(14.5),
                                palette.text,
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;
                                for (value, label) in [
                                    ("week", gettext(locale, "Week")),
                                    ("yesterday", gettext(locale, "Yesterday")),
                                    ("today", gettext(locale, "Today")),
                                ] {
                                    if widgets::filter_chip(ui, &palette, label.as_ref(), 0, period == value)
                                        .clicked()
                                    {
                                        period = value;
                                    }
                                }
                            });
                        });
                        ui.add_space(6.0);
                        match analysis.summaries.get(period) {
                            Some(text) => wrapped(ui, text, theme::regular(15.0), palette.text),
                            None => {
                                theme::text(
                                    ui,
                                    gettext(locale, "Nothing happened in this period."),
                                    theme::regular(13.0),
                                    palette.dim,
                                );
                            }
                        }
                    });
                    ui.add_space(12.0);
                    ui.columns(4, |columns| {
                        metric(&mut columns[0], &palette, Icon::ListChecks, gettext(locale, "To do").as_ref(), open(&analysis.todo), palette.text);
                        metric(&mut columns[1], &palette, Icon::Clock, gettext(locale, "To follow up").as_ref(), open(&analysis.follow_up), palette.warning);
                        metric(&mut columns[2], &palette, Icon::CircleAlert, gettext(locale, "Slipping").as_ref(), open(&analysis.leaks), palette.danger);
                        metric(&mut columns[3], &palette, Icon::Bell, gettext(locale, "New needs").as_ref(), open(&analysis.needs), palette.accent);
                    });
                    ui.add_space(12.0);
                    sales_cards(app, ui, state, project);
                    let left = |app: &App, ui: &mut egui::Ui, action: &mut Option<FindingAction>, pending: &mut Option<Pending>| {
                        findings_card(app, ui, Icon::CircleAlert, gettext(locale, "Slipping").as_ref(), palette.danger, &analysis.leaks, ticks, true, gettext(locale, "Nothing is slipping.").as_ref(), action, pending);
                        findings_card(app, ui, Icon::ListChecks, gettext(locale, "To do · what we committed to").as_ref(), palette.text, &analysis.todo, ticks, true, gettext(locale, "Nothing pending.").as_ref(), action, pending);
                        findings_card(app, ui, Icon::Clock, gettext(locale, "To follow up · waiting on others").as_ref(), palette.warning, &analysis.follow_up, ticks, true, gettext(locale, "Nobody owes us an answer.").as_ref(), action, pending);
                    };
                    let right = |app: &App, ui: &mut egui::Ui, action: &mut Option<FindingAction>, pending: &mut Option<Pending>| {
                        findings_card(app, ui, Icon::Bell, gettext(locale, "Needs that came up").as_ref(), palette.accent, &analysis.needs, ticks, true, gettext(locale, "No new needs.").as_ref(), action, pending);
                        findings_card(app, ui, Icon::Star, gettext(locale, "Messages worth reading").as_ref(), palette.warning, &analysis.highlights, ticks, false, gettext(locale, "Nothing stands out.").as_ref(), action, pending);
                    };
                    if ui.available_width() >= TWO_COLUMNS {
                        let width = ui.available_width();
                        let gap = 14.0;
                        // The row spaces its two columns too.
                        let usable = width - gap - 2.0 * ui.spacing().item_spacing.x;
                        let left_width = usable * 0.58;
                        ui.horizontal_top(|ui| {
                            ui.allocate_ui_with_layout(vec2(left_width, 0.0), Layout::top_down(Align::Min), |ui| {
                                left(app, ui, &mut action, &mut pending)
                            });
                            ui.add_space(gap);
                            ui.allocate_ui_with_layout(vec2(usable - left_width, 0.0), Layout::top_down(Align::Min), |ui| {
                                note_card(app, ui, state, project);
                                ui.add_space(12.0);
                                right(app, ui, &mut action, &mut pending)
                            });
                        });
                    } else {
                        note_card(app, ui, state, project);
                        ui.add_space(12.0);
                        left(app, ui, &mut action, &mut pending);
                        right(app, ui, &mut action, &mut pending);
                    }
                    theme::text(
                        ui,
                        gettext(locale, "Right-click an item to mark it as a good or bad recommendation: the analysis learns from it."),
                        theme::regular(12.0),
                        palette.dim,
                    );
                });
        });
    state.period = period;
    let now = ui.ctx().input(|input| input.time);
    // A pending finding closes once its time is up, or when saved, unless
    // it was undone or the person is still writing how it was solved.
    if let Some(waiting) = pending.take() {
        if waiting.undone {
        } else if !waiting.writing && now - waiting.started >= UNDO_SECONDS {
            commit(
                app,
                state,
                project,
                &waiting.finding,
                waiting.verdict,
                Some(waiting.note),
            );
        } else {
            pending = Some(waiting);
        }
    }
    match action {
        Some(FindingAction::Judge(finding, verdict)) => match verdict {
            Verdict::Done | Verdict::Dismissed => {
                // Another one already waiting closes now.
                if let Some(earlier) = pending.take()
                    && !earlier.undone
                {
                    commit(
                        app,
                        state,
                        project,
                        &earlier.finding,
                        earlier.verdict,
                        Some(earlier.note),
                    );
                }
                pending = Some(Pending {
                    finding,
                    verdict,
                    started: now,
                    note: String::new(),
                    writing: false,
                    undone: false,
                });
            }
            Verdict::Good | Verdict::Bad => commit(app, state, project, &finding, verdict, None),
        },
        Some(FindingAction::Open(chat)) => {
            app.actions.push(Action::Open(Page::Chats));
            app.actions.push(Action::OpenChat(chat));
        }
        Some(FindingAction::Show(finding)) => show_origin(state, &finding),
        None => {}
    }
    state.pending = pending;
}

/// Keeps what the person made of a finding, with how it was solved; a
/// resolution in their words makes the analysis read the project again.
#[allow(clippy::too_many_arguments)]
fn commit(
    app: &App,
    state: &mut State,
    project: &Project,
    finding: &Finding,
    verdict: Verdict,
    note: Option<String>,
) {
    let mut ticks = Ticks::load(&app.dirs.config, &project.id);
    let explained = note.as_ref().is_some_and(|n| !n.trim().is_empty());
    ticks.judge_with(finding, verdict, note);
    if let Err(error) = ticks.save(&app.dirs.config, &project.id) {
        log::warn!("projects: ticks could not be saved: {error}");
    }
    state.analysis = None;
    if explained || matches!(verdict, Verdict::Bad) {
        engine::refresh(&project.id);
    }
}

/// Opens Messages on where a finding came from, marked.
fn show_origin(state: &mut State, finding: &Finding) {
    let (Some(chat), Some(message)) = (&finding.chat, &finding.message) else {
        return;
    };
    let at = finding
        .since
        .unwrap_or_else(|| jiff::Timestamp::now().as_second());
    let days = ((jiff::Timestamp::now().as_second() - at) / 86_400 + 1).clamp(1, 30);
    state.days = RANGES
        .iter()
        .map(|(d, _)| *d)
        .find(|d| *d >= days)
        .unwrap_or(30)
        .max(state.days);
    state.only_chat = None;
    state.show_all = true;
    state.selected.clear();
    state.highlight = Some(Highlight {
        chat: chat.clone(),
        message: message.clone(),
        at,
        text: finding.text.clone(),
        scroll: true,
    });
    state.tab = Tab::Messages;
}

/// The Leads tab's cards (sales, and the ElevaLeads brand's inbox); one of
/// them may open the tab on a lead.
fn sales_cards(app: &mut App, ui: &mut egui::Ui, state: &mut State, project: &Project) {
    if let Some(super::leads::PanelEvent::OpenLeads(lead)) =
        super::leads::panel_cards(app, ui, project)
    {
        super::leads::select(ui.ctx(), &project.id, lead);
        state.tab = Tab::Leads;
    }
}

/// The panel before the first analysis: what it will show, and how to turn
/// it on or run it.
fn no_analysis(app: &mut App, ui: &mut egui::Ui, state: &mut State, project: &Project) {
    let palette = app.palette;
    let locale = app.locale;
    let config = EngineConfig::load(&app.dirs.config);
    let status = engine::status();
    let height = ui.available_height();
    ui.vertical_centered(|ui| {
        ui.add_space((height / 2.0 - 140.0).max(24.0));
        theme::icon(ui, Icon::ListChecks, 40.0, palette.dim);
        ui.add_space(10.0);
        theme::text(
            ui,
            gettext(locale, "Your project panel"),
            theme::semibold(17.0),
            palette.text,
        );
        ui.add_space(6.0);
        ui.set_max_width(440.0);
        wrapped(
            ui,
            gettext(locale, "What you have to do, whom to follow up, what is slipping, the needs that came up and a summary by day, read from this project's chats.").as_ref(),
            theme::regular(14.0),
            palette.secondary,
        );
        ui.add_space(18.0);
        if project.sources.is_empty() {
            if pill_button(ui, &palette, gettext(locale, "Add chats").as_ref(), true).clicked() {
                state.tab = Tab::Sources;
                state.picking = true;
            }
        } else if config.provider == Provider::Off {
            if pill_button(ui, &palette, gettext(locale, "Turn on the analysis").as_ref(), true)
                .clicked()
            {
                state.selection = Some(Selection::Engine);
            }
        } else if status.running.is_some() {
            theme::text(
                ui,
                gettext(locale, "Analyzing…"),
                theme::regular(13.5),
                palette.secondary,
            );
        } else {
            if pill_button(ui, &palette, gettext(locale, "Analyze now").as_ref(), true).clicked() {
                engine::run_now();
            }
            if let Some(error) = status.last_error {
                ui.add_space(10.0);
                wrapped(ui, &error, theme::regular(12.5), palette.danger);
            }
        }
    });
}

// ------------------------------------------------------------------ history

/// Everything closed or judged, newest first: what, when, how it was
/// solved. A closed item can be opened again, and shows where it came from.
fn history(app: &mut App, ui: &mut egui::Ui, state: &mut State, project: &Project, ticks: &Ticks) {
    let palette = app.palette;
    let locale = app.locale;
    let mut filter = state.history_filter;
    let mut reopen = None;
    let mut show = None;
    Frame::new()
        .inner_margin(Margin::symmetric(20, 10))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for (value, label) in [
                    (None, gettext(locale, "Everything")),
                    (Some(Verdict::Done), gettext(locale, "Done")),
                    (Some(Verdict::Dismissed), gettext(locale, "Dismissed")),
                    (Some(Verdict::Good), gettext(locale, "Good recommendation")),
                    (Some(Verdict::Bad), gettext(locale, "Bad recommendation")),
                ] {
                    if widgets::filter_chip(ui, &palette, label.as_ref(), 0, filter == value)
                        .clicked()
                    {
                        filter = value;
                    }
                }
            });
        });
    state.history_filter = filter;
    let items: Vec<&crate::projects::Feedback> = ticks
        .feedback
        .iter()
        .rev()
        .filter(|f| filter.is_none_or(|v| v == f.verdict))
        .collect();
    if items.is_empty() {
        widgets::empty_state(
            ui,
            &palette,
            Icon::Clock,
            gettext(locale, "Nothing closed yet").as_ref(),
            gettext(
                locale,
                "What you tick off, dismiss or judge on the panel is kept here.",
            )
            .as_ref(),
        );
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt(("project-history", &project.id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            Frame::new()
                .inner_margin(Margin {
                    left: 20,
                    right: 20,
                    top: 0,
                    bottom: 16,
                })
                .show(ui, |ui| {
                    ui.set_width(ui.available_width().min(820.0));
                    let mut day = String::new();
                    for item in items {
                        let label = crate::util::day_label(locale, item.at);
                        if label != day {
                            day_chip(ui, &palette, &label);
                            day = label;
                        }
                        let (icon, color, verdict) = match item.verdict {
                            Verdict::Done => {
                                (Icon::CircleCheck, palette.accent, gettext(locale, "Done"))
                            }
                            Verdict::Dismissed => {
                                (Icon::X, palette.secondary, gettext(locale, "Dismissed"))
                            }
                            Verdict::Good => (
                                Icon::Heart,
                                palette.accent,
                                gettext(locale, "Good recommendation"),
                            ),
                            Verdict::Bad => (
                                Icon::Ban,
                                palette.danger,
                                gettext(locale, "Bad recommendation"),
                            ),
                        };
                        card_frame(&palette).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 8.0;
                                theme::icon(ui, icon, 15.0, color);
                                chip(ui, verdict.as_ref(), color);
                                theme::text(
                                    ui,
                                    crate::util::clock(item.at),
                                    theme::regular(12.0),
                                    palette.dim,
                                );
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    ui.spacing_mut().item_spacing.x = 6.0;
                                    if matches!(item.verdict, Verdict::Done | Verdict::Dismissed)
                                        && pill_button(
                                            ui,
                                            &palette,
                                            gettext(locale, "Reopen").as_ref(),
                                            false,
                                        )
                                        .clicked()
                                    {
                                        reopen = Some(item.id.clone());
                                    }
                                    if item.message.is_some()
                                        && theme::icon_button(
                                            ui,
                                            Icon::MessageCircle,
                                            15.0,
                                            palette.secondary,
                                            palette.text,
                                            &gettext(locale, "See the messages it came from"),
                                        )
                                        .clicked()
                                    {
                                        show = Some(Finding {
                                            id: item.id.clone(),
                                            text: item.text.clone(),
                                            chat: item.chat.clone(),
                                            message: item.message.clone(),
                                            since: Some(item.at),
                                            ..Finding::default()
                                        });
                                    }
                                });
                            });
                            ui.add_space(4.0);
                            wrapped(ui, &item.text, theme::regular(14.0), palette.text);
                            if let Some(note) = &item.note {
                                ui.add_space(2.0);
                                wrapped(
                                    ui,
                                    &gettext(locale, "How it was solved: {note}")
                                        .replace("{note}", note),
                                    theme::regular(13.0),
                                    palette.secondary,
                                );
                            }
                        });
                        ui.add_space(8.0);
                    }
                });
        });
    if let Some(id) = reopen {
        let mut ticks = Ticks::load(&app.dirs.config, &project.id);
        ticks.reopen(&id);
        if let Err(error) = ticks.save(&app.dirs.config, &project.id) {
            log::warn!("projects: ticks could not be saved: {error}");
        }
        state.analysis = None;
    }
    if let Some(finding) = show {
        show_origin(state, &finding);
    }
}

// ------------------------------------------------------------ notes, context

fn set_phase(job: &Arc<Mutex<CallJob>>, ctx: &egui::Context, phase: CallPhase) {
    job.lock().unwrap_or_else(|p| p.into_inner()).phase = phase;
    ctx.request_repaint();
}

/// What the calls pipeline needs from the app, to run on a thread.
#[derive(Clone)]
pub(crate) struct CallReader {
    pub(crate) engines: crate::media_ai::MediaEngines,
    pub(crate) state: std::path::PathBuf,
    pub(crate) config: std::path::PathBuf,
}

impl CallReader {
    pub(crate) fn of(app: &App) -> Self {
        Self {
            engines: app.settings.media_engines.clone(),
            state: app.dirs.state.clone(),
            config: app.dirs.config.clone(),
        }
    }
}

/// The project's context, for reading its calls.
pub(crate) fn call_context(config: &std::path::Path, project: &str) -> String {
    let map = ProjectMap::load(config);
    let Some(project) = map.project(project) else {
        return String::new();
    };
    let mut text = format!("Proyecto: {}\n", project.name);
    if !project.context.trim().is_empty() {
        text.push_str(&format!("Contexto: {}\n", project.context.trim()));
    }
    for member in &project.members {
        text.push_str(&format!("- {} {}\n", member.name, member.role));
    }
    text.push('\n');
    text
}

/// After a call is kept: read it, analyze it, and note what it said in the
/// project, so its panel picks up the commitments. Each step is kept as it
/// finishes; a step that fails leaves the call and what came before.
pub(crate) fn read_call(
    reader: CallReader,
    project: String,
    id: String,
    job: Arc<Mutex<CallJob>>,
    ctx: egui::Context,
) {
    set_phase(&job, &ctx, CallPhase::Reading);
    if let Err(error) = crate::call_library::transcribe(&reader.engines, &reader.state, &id) {
        return set_phase(&job, &ctx, CallPhase::Failed(error));
    }
    analyze_call(&reader, &project, &id);
    set_phase(&job, &ctx, CallPhase::Saved(Instant::now()));
}

/// Analyzes a transcribed call and notes it in the project's journal.
fn analyze_call(reader: &CallReader, project: &str, id: &str) {
    let analysis = crate::call_library::analyze(
        &reader.config,
        &reader.state,
        id,
        &call_context(&reader.config, project),
    );
    if let Some(call) = crate::call_library::Library::load(&reader.state).get(id) {
        note_call(reader, project, call, &analysis);
    }
}

/// How much of a transcript a note keeps when there is no summary: the
/// journal goes to the dashboard's engine, and a whole call does not belong
/// there.
const NOTE_EXCERPT: usize = 400;

/// Notes a read call in the project's journal, so its panel picks up the
/// commitments: its summary, or that it has none, with a short excerpt.
pub(crate) fn note_call(
    reader: &CallReader,
    project: &str,
    call: &crate::call_library::Call,
    analysis: &Result<crate::call_library::CallAnalysis, String>,
) {
    let mut note = format!("{}:", call.title);
    match analysis {
        Ok(analysis) => {
            note.push(' ');
            note.push_str(&analysis.summary);
            for c in &analysis.commitments {
                let due = c
                    .due
                    .as_deref()
                    .map(|d| format!(" (para {d})"))
                    .unwrap_or_default();
                note.push_str(&format!("\nCompromiso: {}: {}{due}", c.who, c.what));
            }
            for task in &analysis.tasks {
                note.push_str(&format!("\nTarea: {task}"));
            }
        }
        Err(reason) => {
            note.push_str(&format!(" transcripta, sin resumen ({reason})"));
            if let Some(transcript) = call.transcript() {
                let text = transcript.text.trim();
                let excerpt: String = text.chars().take(NOTE_EXCERPT).collect();
                note.push('\n');
                note.push_str(&excerpt);
                if excerpt.len() < text.len() {
                    note.push('…');
                }
            }
        }
    }
    let mut journal = Journal::load(&reader.config, project);
    journal.add_kind(&note, true, crate::projects::NoteKind::Log);
    if journal.save(&reader.config, project).is_ok() {
        engine::refresh(project);
    }
}

/// Records a call (the microphone and the computer's sound) until it is
/// ended, then keeps the file for review.
fn record_call(
    app: &App,
    project: &str,
    job: Arc<Mutex<CallJob>>,
    ctx: egui::Context,
    title: String,
) {
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    {
        let mut held = job.lock().unwrap_or_else(|p| p.into_inner());
        held.phase = CallPhase::Starting;
        held.stop = Some(stop);
    }
    let reader = CallReader::of(app);
    let project = project.to_owned();
    let worker_job = job.clone();
    let thread = std::thread::spawn(move || {
        let job = worker_job;
        let recording =
            match crate::recorder::Recording::start(&reader.state, crate::recorder::Source::Both) {
                Ok(recording) => recording,
                Err(error) => return set_phase(&job, &ctx, CallPhase::Failed(error)),
            };
        set_phase(&job, &ctx, CallPhase::Recording(Instant::now()));
        let _ = stopped.recv();
        let seconds = recording.elapsed().as_secs();
        let path = match recording.stop() {
            Ok(path) => path,
            Err(error) => return set_phase(&job, &ctx, CallPhase::Failed(error)),
        };
        // The file first: kept before anything reads it.
        let call = match crate::call_library::Library::keep_for_review(
            &reader.state,
            &path,
            seconds,
            &title,
            Some(&project),
            None,
        ) {
            Ok(call) => call,
            Err(error) => return set_phase(&job, &ctx, CallPhase::Failed(error)),
        };
        RECORDINGS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(call.id);
        set_phase(&job, &ctx, CallPhase::Idle);
    });
    job.lock().unwrap_or_else(|p| p.into_inner()).thread = Some(thread);
}

/// Picks an audio or video file, keeps a copy, then reads it.
fn add_file(app: &App, project: &str, job: Arc<Mutex<CallJob>>, ctx: egui::Context, label: String) {
    let reader = CallReader::of(app);
    let project = project.to_owned();
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
            return;
        };
        let title = path
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let call = match crate::call_library::Library::keep(
            &reader.state,
            &path,
            crate::call_library::Origin::Uploaded,
            0,
            &title,
            Some(&project),
            None,
        ) {
            Ok(call) => call,
            Err(error) => return set_phase(&job, &ctx, CallPhase::Failed(error)),
        };
        set_phase(&job, &ctx, CallPhase::Kept);
        read_call(reader, project, call.id, job, ctx);
    });
}

/// The call and file buttons, and what they are doing.
fn call_controls(app: &App, ui: &mut egui::Ui, state: &State, project: &Project) {
    let palette = app.palette;
    let locale = app.locale;
    let phase = state
        .call
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .phase
        .clone();
    match phase {
        CallPhase::Recording(started) => {
            let seconds = started.elapsed().as_secs();
            let dot = ui.cursor().left_center() + vec2(6.0, 0.0);
            ui.painter().circle_filled(dot, 5.0, palette.danger);
            ui.add_space(14.0);
            theme::text(
                ui,
                gettext(locale, "Call {time}")
                    .replace("{time}", &crate::util::call_duration(seconds)),
                theme::medium(13.5),
                palette.text,
            );
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(500));
            if pill_button(ui, &palette, gettext(locale, "End call").as_ref(), true).clicked()
                && let Some(stop) = state
                    .call
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .stop
                    .take()
            {
                let _ = stop.send(());
            }
        }
        CallPhase::Starting => {
            ui.add(egui::Spinner::new().size(16.0).color(palette.accent));
            theme::text(
                ui,
                gettext(locale, "Asking macOS for permission…"),
                theme::regular(12.5),
                palette.secondary,
            );
        }
        CallPhase::Reading | CallPhase::Kept => {
            ui.add(egui::Spinner::new().size(16.0).color(palette.accent));
            theme::text(
                ui,
                gettext(locale, "Recording saved in Calls. Reading it…"),
                theme::regular(12.5),
                palette.secondary,
            );
        }
        phase => {
            // A WhatsApp call records through its own button: two recorders
            // at once would each take the other's sound.
            let on_call = app.call_live();
            let record = ui
                .add_enabled_ui(!on_call, |ui| {
                    theme::icon_button(
                        ui,
                        Icon::Phone,
                        17.0,
                        palette.secondary,
                        palette.text,
                        &gettext(
                            locale,
                            "Record a call (your microphone and the computer's sound)",
                        ),
                    )
                })
                .inner
                .on_disabled_hover_text(gettext(
                    locale,
                    "You are on a WhatsApp call: record it with the call's button",
                ));
            if record.clicked() {
                let title = gettext(locale, "Call of {date}").replace(
                    "{date}",
                    &crate::util::moment_stamp(locale, jiff::Timestamp::now().as_second()),
                );
                record_call(
                    app,
                    &project.id,
                    state.call.clone(),
                    ui.ctx().clone(),
                    title,
                );
            }
            if theme::icon_button(
                ui,
                Icon::Paperclip,
                17.0,
                palette.secondary,
                palette.text,
                &gettext(locale, "Add an audio or video file"),
            )
            .clicked()
            {
                add_file(
                    app,
                    &project.id,
                    state.call.clone(),
                    ui.ctx().clone(),
                    gettext(locale, "Audio or video").into_owned(),
                );
            }
            match phase {
                CallPhase::Saved(at) if at.elapsed().as_secs() < 8 => {
                    theme::icon(ui, Icon::CircleCheck, 14.0, palette.accent);
                    theme::text(
                        ui,
                        gettext(locale, "Saved in Calls and Notes"),
                        theme::regular(12.5),
                        palette.accent,
                    );
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_secs(1));
                }
                CallPhase::Failed(error) => {
                    theme::text(ui, error, theme::regular(12.5), palette.danger);
                }
                _ => {}
            }
        }
    }
}

/// Writing or saying what happened: a call, what was solved, what was
/// agreed. The note joins the project's journal and the panel is read
/// again with it.
fn note_card(app: &mut App, ui: &mut egui::Ui, state: &mut State, project: &Project) {
    let palette = app.palette;
    let locale = app.locale;
    let voice_job = crate::note_capture::job(app, &project.id);
    let mut voice = voice_job.lock().unwrap_or_else(|p| p.into_inner());
    let mut save = false;
    card_frame(&palette).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            theme::icon(ui, Icon::Pencil, 16.0, palette.accent);
            theme::text(
                ui,
                gettext(locale, "Tell the project what happened"),
                theme::semibold(14.5),
                palette.text,
            );
        });
        wrapped(
            ui,
            gettext(locale, "A call, what was solved, what you agreed to. It goes to the journal and the panel updates with it.").as_ref(),
            theme::regular(12.5),
            palette.secondary,
        );
        ui.add_space(6.0);
        let options = [
            (crate::projects::NoteKind::Log, gettext(locale, "What happened").into_owned()),
            (crate::projects::NoteKind::Memo, gettext(locale, "Note to keep").into_owned()),
        ];
        let note_kind = voice.pending.as_ref().map_or(state.note_kind, |take| take.kind);
        if let Some(kind) = segmented(ui, &palette, &options, note_kind) {
            state.note_kind = kind;
            if let Some(take) = voice.pending.as_mut() {
                take.kind = kind;
            }
        }
        ui.add_space(4.0);
        Frame::new()
            .fill(palette.surface)
            .corner_radius(CornerRadius::same(theme::RADIUS + 2))
            .inner_margin(Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut state.note)
                        .id(egui::Id::new(("project-note", &project.id)))
                        .hint_text(gettext(locale, "Today I spoke with Diego: we confirmed 15 garments for Friday…").as_ref())
                        .font(theme::regular(14.0))
                        .frame(Frame::NONE)
                        .desired_rows(2)
                        .desired_width(f32::INFINITY),
                );
            });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if let Some(recorder) = &voice.recorder {
                let seconds = recorder.elapsed().as_secs();
                ui.painter().circle_filled(
                    ui.cursor().left_center() + vec2(6.0, 0.0),
                    5.0,
                    palette.danger,
                );
                ui.add_space(14.0);
                theme::text(
                    ui,
                    crate::util::call_duration(seconds),
                    theme::medium(13.5),
                    palette.text,
                );
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
                if pill_button(ui, &palette, gettext(locale, "Stop").as_ref(), true).clicked() {
                    crate::note_capture::stop(&voice_job, &mut voice, ui.ctx());
                }
            } else if voice.busy {
                ui.add(egui::Spinner::new().size(16.0).color(palette.accent));
                let label = if voice.pending.is_some() { gettext(locale, "Transcribing…") } else { gettext(locale, "Saving recording…") };
                theme::text(ui, label, theme::regular(13.0), palette.secondary);
            } else if voice.pending.is_none() && theme::icon_button(
                ui,
                Icon::Mic,
                18.0,
                palette.secondary,
                palette.text,
                &gettext(locale, "Say it"),
            )
            .clicked()
            {
                let title = gettext(locale, "Voice note of {date}").replace("{date}", &crate::util::moment_stamp(locale, crate::util::now()));
                crate::note_capture::start(&mut voice, app, state.note_kind, title);
            }
            if voice.recorder.is_none() && !voice.busy {
                call_controls(app, ui, state, project);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if !state.note.trim().is_empty()
                    && pill_button(ui, &palette, gettext(locale, "Save note").as_ref(), true).clicked()
                {
                    save = true;
                }
                if engine::pending(&project.id) {
                    theme::text(
                        ui,
                        gettext(locale, "Updating the panel…"),
                        theme::regular(12.5),
                        palette.secondary,
                    );
                }
            });
        });
        if !voice.busy && let Some(pending) = voice.pending.as_mut() {
            ui.add_space(8.0);
            ui.add(egui::TextEdit::singleline(&mut pending.title).font(theme::medium(14.0)).desired_width(f32::INFINITY));
            let mut take = super::take::Take::new(pending.path.clone(), pending.seconds);
            take.confirming = voice.confirming;
            let choice = super::take::card(app, ui, &mut take, &gettext(locale, "Save and transcribe"));
            voice.confirming = take.confirming;
            if let Some(choice) = choice {
                crate::note_capture::decide(&voice_job, &mut voice, app, ui.ctx(), choice == super::take::Choice::Keep);
            }
        }
        if let Some(error) = &voice.error {
            theme::paragraph(ui, error, theme::regular(13.0), palette.danger);
        }
        if voice.saved.is_some_and(|at| at.elapsed().as_secs() < 8) {
            theme::text(ui, gettext(locale, "Note saved"), theme::regular(13.0), palette.accent);
        }
    });
    if save {
        let mut journal = Journal::load(&app.dirs.config, &project.id);
        journal.add_kind(&state.note, false, state.note_kind);
        if let Err(error) = journal.save(&app.dirs.config, &project.id) {
            log::warn!("projects: the journal could not be saved: {error}");
        }
        state.note.clear();
        engine::refresh(&project.id);
    }
}

/// Opens `path` with the system, off the interface thread: the app for its
/// kind, or its folder (with the file picked where the system can).
pub(crate) fn reveal(path: &std::path::Path, in_folder: bool) {
    let path = path.to_owned();
    std::thread::spawn(move || {
        let opened = if in_folder {
            show_in_folder(&path)
        } else {
            crate::opener::open_or_reveal(&path)
        };
        if let Err(error) = opened {
            log::warn!("projects: the call could not be opened: {error}");
        }
    });
}

#[cfg(target_os = "macos")]
fn show_in_folder(path: &std::path::Path) -> Result<(), String> {
    std::process::Command::new("/usr/bin/open")
        .arg("-R")
        .arg(path)
        .status()
        .map_err(|e| e.to_string())
        .and_then(|status| {
            status
                .success()
                .then_some(())
                .ok_or_else(|| status.to_string())
        })
}

#[cfg(not(target_os = "macos"))]
fn show_in_folder(path: &std::path::Path) -> Result<(), String> {
    let folder = path.parent().unwrap_or(path);
    open::that(folder).map_err(|e| e.to_string())
}

/// Runs `work` on a call off the interface thread, marking it busy with
/// `label` meanwhile.
fn call_job(
    busy: &Arc<Mutex<HashMap<String, String>>>,
    ctx: &egui::Context,
    id: &str,
    label: String,
    work: impl FnOnce() -> Result<(), String> + Send + 'static,
) {
    busy.lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(id.to_owned(), label);
    let (busy, ctx, id) = (busy.clone(), ctx.clone(), id.to_owned());
    std::thread::spawn(move || {
        let result = work();
        let mut held = busy.lock().unwrap_or_else(|p| p.into_inner());
        match result {
            Ok(()) => {
                held.remove(&id);
            }
            Err(error) => {
                held.insert(id, format!("!{error}"));
            }
        }
        ctx.request_repaint();
    });
}

enum CallAction {
    Toggle,
    Review,
    Play,
    Finder,
    Retranscribe(crate::media_ai::SpeechEngine),
    Reanalyze,
    Copy,
    Export,
    Rename,
    Delete,
}

/// What a read call said, as the project's calls and a recorded call's sheet
/// both show it: its summary, decisions, commitments, tasks and next steps,
/// then its transcript, folded. `empty` stands in for a missing summary.
pub(crate) fn call_details(
    ui: &mut egui::Ui,
    palette: &theme::Palette,
    locale: crate::i18n::Locale,
    call: &crate::call_library::Call,
    empty: Option<&str>,
) {
    ui.set_width(ui.available_width());
    ui.spacing_mut().item_spacing.y = 6.0;
    match &call.analysis {
        Some(analysis) => {
            if !analysis.summary.is_empty() {
                wrapped(ui, &analysis.summary, theme::regular(14.0), palette.text);
            }
            let list = |ui: &mut egui::Ui, title: &str, items: Vec<String>| {
                if items.is_empty() {
                    return;
                }
                theme::text(ui, title, theme::semibold(13.0), palette.accent);
                for item in items {
                    wrapped(
                        ui,
                        &format!("•  {item}"),
                        theme::regular(13.5),
                        palette.text,
                    );
                }
            };
            list(
                ui,
                gettext(locale, "Decisions").as_ref(),
                analysis.decisions.clone(),
            );
            list(
                ui,
                gettext(locale, "Commitments").as_ref(),
                analysis.commitments.iter().map(commitment).collect(),
            );
            list(
                ui,
                gettext(locale, "Tasks").as_ref(),
                analysis.tasks.clone(),
            );
            list(
                ui,
                gettext(locale, "Next steps").as_ref(),
                analysis.next_steps.clone(),
            );
        }
        None => {
            if let Some(empty) = empty {
                theme::text(ui, empty, theme::regular(13.0), palette.dim);
            }
        }
    }
    if let Some(transcript) = call.transcript() {
        ui.add_space(4.0);
        egui::CollapsingHeader::new(
            egui::RichText::new(
                gettext(locale, "Transcript · {engine} · {n} versions")
                    .replace("{engine}", &transcript.engine)
                    .replace("{n}", &call.transcripts.len().to_string()),
            )
            .font(theme::medium(13.0))
            .color(palette.secondary),
        )
        .id_salt(("call-transcript", &call.id))
        .show(ui, |ui| {
            wrapped(ui, &transcript.text, theme::regular(13.5), palette.text);
        });
    }
}

/// A commitment on one line: who, what, and by when.
pub(crate) fn commitment(c: &crate::call_library::Commitment) -> String {
    let who = c.who.trim();
    let what = if who.is_empty() {
        c.what.clone()
    } else {
        format!("{who}: {}", c.what)
    };
    match &c.due {
        Some(due) => format!("{what} ({due})"),
        None => what,
    }
}

/// The project's calls: each kept file with what was made of it, and what
/// can be done with it.
fn calls_section(app: &mut App, ui: &mut egui::Ui, state: &mut State, project: &Project) {
    let palette = app.palette;
    let locale = app.locale;
    let library = crate::call_library::Library::cached(&app.dirs.state);
    // A locked chat's call stays out while its folder is closed.
    let calls: Vec<crate::call_library::Call> = library
        .of_project(&project.id)
        .into_iter()
        .filter(|call| !call.private || app.locked_folder_open())
        .cloned()
        .collect();
    if calls.is_empty() {
        return;
    }
    let busy = state
        .call_busy
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    // What reading each call is doing, for the calls recorded in a WhatsApp
    // call: the read runs on its own, outside this page.
    let jobs: HashMap<String, crate::call_capture::JobPhase> = calls
        .iter()
        .filter_map(|call| crate::call_capture::job(&call.id).map(|job| (call.id.clone(), job)))
        .collect();
    let reading = jobs.values().any(|job| {
        matches!(
            job,
            crate::call_capture::JobPhase::Reading | crate::call_capture::JobPhase::Summarizing
        )
    });
    if !busy.is_empty() || reading {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(500));
    }
    let mut actions: Vec<(String, CallAction)> = Vec::new();
    card_frame(&palette).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            theme::icon(ui, Icon::Phone, 16.0, palette.accent);
            theme::text(ui, gettext(locale, "Calls"), theme::semibold(14.5), palette.text);
            chip(ui, &calls.len().to_string(), palette.accent);
        });
        wrapped(
            ui,
            gettext(locale, "Each call keeps its audio file. Read it again with another engine, redo its summary, or take it to another tool.").as_ref(),
            theme::regular(12.5),
            palette.secondary,
        );
        ui.add_space(6.0);
        for (index, call) in calls.iter().enumerate() {
            if index > 0 {
                let x = ui.max_rect().x_range();
                let y = ui.cursor().top();
                ui.painter().hline(x, y, palette.glass_edge());
            }
            let open = state.call_open.as_deref() == Some(call.id.as_str());
            let status = busy.get(&call.id);
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let icon = match call.origin {
                    crate::call_library::Origin::Recorded => Icon::Mic,
                    crate::call_library::Origin::Uploaded => Icon::Paperclip,
                };
                theme::icon(ui, icon, 15.0, palette.secondary);
                let width = (ui.available_width() - 150.0).max(120.0);
                let clicked = column(ui, width, |ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    if let Some((id, title)) = state.renaming.as_mut()
                        && id == &call.id
                    {
                        let field = field_frame(ui, &palette, |ui| {
                            ui.add(
                                egui::TextEdit::singleline(title)
                                    .font(theme::medium(14.0))
                                    .frame(Frame::NONE)
                                    .desired_width(f32::INFINITY),
                            )
                        });
                        if !field.has_focus() && !field.lost_focus() {
                            field.request_focus();
                        }
                        if field.lost_focus() {
                            actions.push((call.id.clone(), CallAction::Rename));
                        }
                        return false;
                    }
                    let title = ui
                        .add(
                            egui::Label::new(
                                egui::RichText::new(&call.title)
                                    .font(theme::medium(14.5))
                                    .color(palette.text),
                            )
                            .truncate()
                            .sense(Sense::click()),
                        )
                        .on_hover_cursor(egui::CursorIcon::PointingHand);
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = vec2(5.0, 4.0);
                        chip(ui, &crate::util::moment_stamp(locale, call.at), palette.secondary);
                        if call.seconds > 0 {
                            chip(ui, &crate::util::call_duration(call.seconds), palette.secondary);
                        }
                        if call.pending_review {
                            chip(ui, gettext(locale, "Not reviewed").as_ref(), palette.warning);
                            if theme::link(
                                ui,
                                gettext(locale, "Review"),
                                theme::medium(11.5),
                                palette.link,
                            )
                            .clicked()
                            {
                                actions.push((call.id.clone(), CallAction::Review));
                            }
                        }
                        let job = jobs.get(&call.id);
                        match (status, job) {
                            (None, Some(crate::call_capture::JobPhase::Reading)) => {
                                chip(ui, gettext(locale, "Transcribing…").as_ref(), palette.warning);
                            }
                            (None, Some(crate::call_capture::JobPhase::Summarizing)) => {
                                chip(ui, gettext(locale, "Summarizing…").as_ref(), palette.warning);
                            }
                            (None, Some(crate::call_capture::JobPhase::Failed(error)))
                                if call.analysis.is_none() =>
                            {
                                chip(ui, gettext(locale, "Error").as_ref(), palette.danger);
                                ui.label(
                                    egui::RichText::new(error)
                                        .font(theme::regular(11.5))
                                        .color(palette.danger),
                                );
                            }
                            (Some(label), _) if !label.starts_with('!') => {
                                chip(ui, label, palette.warning);
                            }
                            (Some(error), _) => {
                                chip(ui, gettext(locale, "Error").as_ref(), palette.danger);
                                ui.label(
                                    egui::RichText::new(error.trim_start_matches('!'))
                                        .font(theme::regular(11.5))
                                        .color(palette.danger),
                                );
                            }
                            (None, _) if call.analysis.is_some() => {
                                chip(ui, gettext(locale, "Summarized").as_ref(), palette.accent);
                            }
                            (None, _) if call.pending_review => {}
                            (None, _) => match call.transcript() {
                                Some(t) => chip(
                                    ui,
                                    &gettext(locale, "Transcribed · {engine}").replace("{engine}", &t.engine),
                                    palette.accent,
                                ),
                                None => chip(ui, gettext(locale, "Not transcribed").as_ref(), palette.dim),
                            },
                        }
                    });
                    title.clicked()
                });
                if clicked {
                    actions.push((call.id.clone(), CallAction::Toggle));
                }
                ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    let more = theme::icon_button(
                        ui,
                        Icon::Ellipsis,
                        16.0,
                        palette.secondary,
                        palette.text,
                        &gettext(locale, "Call options"),
                    );
                    let again = gettext(locale, "Transcribe again with {engine}");
                    let options: Vec<(String, CallAction)> = crate::media_ai::SpeechEngine::EVERY
                        .into_iter()
                        .map(|engine| {
                            (
                                again.replace("{engine}", engine.label()),
                                CallAction::Retranscribe(engine),
                            )
                        })
                        .chain([
                            (
                                gettext(locale, "View recording and summary").into_owned(),
                                CallAction::Review,
                            ),
                            (gettext(locale, "Redo the summary").into_owned(), CallAction::Reanalyze),
                            (gettext(locale, "Copy summary and transcript").into_owned(), CallAction::Copy),
                            (gettext(locale, "Export as Markdown").into_owned(), CallAction::Export),
                            (gettext(locale, "Rename").into_owned(), CallAction::Rename),
                            (gettext(locale, "Delete call").into_owned(), CallAction::Delete),
                        ])
                        .collect();
                    let labels: Vec<&str> = options.iter().map(|(l, _)| l.as_str()).collect();
                    egui::Popup::menu(&more)
                        .width(widgets::menu_width(ui, &labels, true))
                        .frame(widgets::menu_frame(&palette))
                        .show(|ui| {
                            for (label, action) in options {
                                let icon = match action {
                                    CallAction::Review => Icon::Record,
                                    CallAction::Retranscribe(_) => Icon::Refresh,
                                    CallAction::Reanalyze => Icon::ListChecks,
                                    CallAction::Copy => Icon::Copy,
                                    CallAction::Export => Icon::Download,
                                    CallAction::Rename => Icon::Pencil,
                                    _ => Icon::Trash,
                                };
                                if widgets::menu_item(ui, &palette, Some(icon), &label) {
                                    actions.push((call.id.clone(), action));
                                    ui.close();
                                }
                            }
                        });
                    if theme::icon_button(ui, Icon::ExternalLink, 15.0, palette.secondary, palette.text, &gettext(locale, "Show in Finder")).clicked() {
                        actions.push((call.id.clone(), CallAction::Finder));
                    }
                    if theme::icon_button(ui, Icon::Play, 16.0, palette.accent, palette.text, &gettext(locale, "Play")).clicked() {
                        actions.push((call.id.clone(), CallAction::Play));
                    }
                    if theme::icon_button(
                        ui,
                        if open { Icon::ChevronUp } else { Icon::ChevronDown },
                        16.0,
                        palette.secondary,
                        palette.text,
                        &gettext(locale, "Details"),
                    )
                    .clicked()
                    {
                        actions.push((call.id.clone(), CallAction::Toggle));
                    }
                });
            });
            if open {
                ui.add_space(6.0);
                Frame::new()
                    .inner_margin(Margin { left: 23, right: 4, top: 0, bottom: 6 })
                    .show(ui, |ui| {
                        let empty = gettext(
                            locale,
                            "No summary yet. Turn on the analysis, or use Redo the summary.",
                        );
                        call_details(ui, &palette, locale, call, Some(empty.as_ref()));
                    });
            }
            ui.add_space(6.0);
        }
    });
    ui.add_space(12.0);
    let reader = CallReader::of(app);
    for (id, action) in actions {
        let Some(call) = calls.iter().find(|c| c.id == id) else {
            continue;
        };
        match action {
            CallAction::Toggle => {
                state.call_open = if state.call_open.as_deref() == Some(id.as_str()) {
                    None
                } else {
                    Some(id)
                };
            }
            CallAction::Review => app.actions.push(Action::OpenRecordedCall(id)),
            CallAction::Play => reveal(&call.path(&reader.state), false),
            CallAction::Finder => reveal(&call.path(&reader.state), true),
            CallAction::Retranscribe(_) | CallAction::Reanalyze if call.pending_review => {
                app.actions.push(Action::OpenRecordedCall(id));
            }
            CallAction::Retranscribe(engine) => {
                let mut engines = reader.engines.clone();
                engines.speech = engine;
                engines.speech_model = String::new();
                let (state_dir, label) = (
                    reader.state.clone(),
                    gettext(locale, "Transcribing…").into_owned(),
                );
                call_job(&state.call_busy, ui.ctx(), &id, label, {
                    let id = id.clone();
                    move || crate::call_library::transcribe(&engines, &state_dir, &id).map(|_| ())
                });
            }
            CallAction::Reanalyze => {
                let reader = reader.clone();
                let project = project.id.clone();
                let label = gettext(locale, "Summarizing…").into_owned();
                call_job(&state.call_busy, ui.ctx(), &id, label, {
                    let id = id.clone();
                    move || {
                        crate::call_library::analyze(
                            &reader.config,
                            &reader.state,
                            &id,
                            &call_context(&reader.config, &project),
                        )
                        .map(|_| engine::refresh(&project))
                    }
                });
            }
            CallAction::Copy => ui.ctx().copy_text(call.markdown()),
            CallAction::Export => {
                let (markdown, name) = (call.markdown(), format!("{}.md", call.title));
                std::thread::spawn(move || {
                    if let Some(path) = rfd::FileDialog::new().set_file_name(&name).save_file()
                        && let Err(error) = std::fs::write(&path, markdown)
                    {
                        log::warn!("projects: the call could not be exported: {error}");
                    }
                });
            }
            CallAction::Rename => match state.renaming.take() {
                Some((renamed, title)) if renamed == id => {
                    if !title.trim().is_empty() {
                        let _ = crate::call_library::Library::update(&reader.state, &id, |c| {
                            c.title = title.trim().to_owned();
                        });
                    }
                }
                _ => state.renaming = Some((id, call.title.clone())),
            },
            CallAction::Delete => {
                if let Err(error) = crate::call_library::Library::remove(&reader.state, &id) {
                    log::warn!("projects: the call could not be deleted: {error}");
                }
            }
        }
    }
}

/// The journal: every note, newest first, under the box to add one.
fn journal(app: &mut App, ui: &mut egui::Ui, state: &mut State, project: &Project) {
    let palette = app.palette;
    let locale = app.locale;
    let mut notes = Journal::load(&app.dirs.config, &project.id);
    let mut remove = None;
    egui::ScrollArea::vertical()
        .id_salt(("project-journal", &project.id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            Frame::new()
                .inner_margin(Margin::symmetric(20, 16))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width().min(760.0));
                    note_card(app, ui, state, project);
                    ui.add_space(14.0);
                    calls_section(app, ui, state, project);
                    let mut filter = state.notes_filter;
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        for (value, label) in [
                            (None, gettext(locale, "Everything")),
                            (Some(crate::projects::NoteKind::Log), gettext(locale, "What happened")),
                            (Some(crate::projects::NoteKind::Memo), gettext(locale, "Notes to keep")),
                        ] {
                            if widgets::filter_chip(ui, &palette, label.as_ref(), 0, filter == value).clicked() {
                                filter = value;
                            }
                        }
                    });
                    state.notes_filter = filter;
                    ui.add_space(6.0);
                    if notes.notes.is_empty() {
                        widgets::empty_state(
                            ui,
                            &palette,
                            Icon::Pencil,
                            gettext(locale, "No notes yet").as_ref(),
                            gettext(locale, "What you tell the project helps it know what is done and what is new.").as_ref(),
                        );
                    }
                    let mut day = String::new();
                    for (index, note) in notes
                        .notes
                        .iter()
                        .enumerate()
                        .rev()
                        .filter(|(_, note)| filter.is_none_or(|kind| kind == note.kind))
                    {
                        let label = crate::util::day_label(locale, note.at);
                        if label != day {
                            day_chip(ui, &palette, &label);
                            day = label;
                        }
                        card_frame(&palette).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                theme::icon(
                                    ui,
                                    if note.spoken { Icon::Mic } else { Icon::Pencil },
                                    13.0,
                                    palette.dim,
                                );
                                theme::text(ui, crate::util::clock(note.at), theme::regular(12.0), palette.dim);
                                if note.kind == crate::projects::NoteKind::Memo {
                                    chip(ui, gettext(locale, "Note to keep").as_ref(), palette.accent);
                                }
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if theme::icon_button(
                                        ui,
                                        Icon::Trash,
                                        13.0,
                                        palette.dim,
                                        palette.text,
                                        &gettext(locale, "Delete note"),
                                    )
                                    .clicked()
                                    {
                                        remove = Some(index);
                                    }
                                });
                            });
                            wrapped(ui, &note.text, theme::regular(14.0), palette.text);
                        });
                        ui.add_space(8.0);
                    }
                });
        });
    if let Some(index) = remove {
        notes.notes.remove(index);
        if let Err(error) = notes.save(&app.dirs.config, &project.id) {
            log::warn!("projects: the journal could not be saved: {error}");
        }
        engine::refresh(&project.id);
    }
}

/// People who may take part: the members of the project's groups first,
/// then chats with one person.
fn people(app: &App, project: &Project) -> Vec<(String, String, String)> {
    let mut seen = HashSet::new();
    let mut people = Vec::new();
    for source in &project.sources {
        if let Some(chat) = app.chat(&source.chat) {
            for id in &chat.participants {
                if seen.insert(id.clone()) {
                    people.push((id.clone(), app.display_name(id), chat.name.clone()));
                }
            }
        }
    }
    for chat in app.chats.iter().filter(|c| c.kind == ChatKind::Direct) {
        if seen.insert(chat.id.clone()) {
            people.push((chat.id.clone(), chat.name.clone(), String::new()));
        }
    }
    people
}

/// Matches for `needle` among `people`, members first; a number typed in
/// is offered as it is.
fn suggestions(
    project: &Project,
    people: &[(String, String, String)],
    needle: &str,
) -> Vec<(String, String, String)> {
    let needle = crate::util::search_key(needle.trim());
    let mut found: Vec<(String, String, String)> = project
        .members
        .iter()
        .filter(|m| needle.is_empty() || crate::util::search_key(&m.name).contains(&needle))
        .map(|m| (m.id.clone(), m.name.clone(), m.role.clone()))
        .collect();
    for (id, name, hint) in people {
        if found.len() >= 8 {
            break;
        }
        let phone = id.split('@').next().unwrap_or("");
        if !project.members.iter().any(|m| &m.id == id)
            && (needle.is_empty()
                || crate::util::search_key(name).contains(&needle)
                || phone.contains(&needle))
        {
            found.push((id.clone(), name.clone(), hint.clone()));
        }
    }
    let digits: String = needle.chars().filter(char::is_ascii_digit).collect();
    if digits.len() >= 8 && !found.iter().any(|(id, _, _)| id.starts_with(&digits)) {
        found.push((
            format!("{digits}@s.whatsapp.net"),
            format!("+{digits}"),
            String::new(),
        ));
    }
    found
}

/// What the project is about and who takes part, for the analysis. Typing
/// @ in the text offers the members and the people of its chats.
fn context(app: &mut App, ui: &mut egui::Ui, state: &mut State, project: &Project) {
    let palette = app.palette;
    let locale = app.locale;
    if state
        .context
        .as_ref()
        .is_none_or(|(id, _)| id != &project.id)
    {
        state.context = Some((project.id.clone(), project.context.clone()));
    }
    let people = people(app, project);
    let mut draft = state
        .context
        .as_ref()
        .map(|(_, text)| text.clone())
        .unwrap_or_default();
    let mut save_context = false;
    let mut add: Option<Member> = None;
    let mut remove = None;
    let mut roles: Vec<(usize, String)> = Vec::new();
    let mut abouts: Vec<(usize, String)> = Vec::new();
    egui::ScrollArea::vertical()
        .id_salt(("project-context", &project.id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            Frame::new()
                .inner_margin(Margin::symmetric(20, 16))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width().min(760.0));
                    card_frame(&palette).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            theme::icon(ui, Icon::Info, 16.0, palette.accent);
                            theme::text(ui, gettext(locale, "About the project"), theme::semibold(14.5), palette.text);
                        });
                        wrapped(
                            ui,
                            gettext(locale, "What it is, how it is organized, who decides, what matters now. Mention people with @. The analysis reads this before the messages.").as_ref(),
                            theme::regular(12.5),
                            palette.secondary,
                        );
                        ui.add_space(6.0);
                        Frame::new()
                            .fill(palette.surface)
                            .corner_radius(CornerRadius::same(theme::RADIUS + 2))
                            .inner_margin(Margin::symmetric(10, 8))
                            .show(ui, |ui| {
                                ui.add(
                                    egui::TextEdit::multiline(&mut draft)
                                        .id(egui::Id::new(("project-context-text", &project.id)))
                                        .hint_text(gettext(locale, "JAS is our embroidery shop. @Papá handles payments, @Liz and @Juan are on the machines. Orders come from Bordados JAS…").as_ref())
                                        .font(theme::regular(14.0))
                                        .frame(Frame::NONE)
                                        .desired_rows(5)
                                        .desired_width(f32::INFINITY),
                                );
                            });
                        // @ at the end of the text: offer people to mention.
                        let token = draft
                            .rsplit(|c: char| c.is_whitespace())
                            .next()
                            .filter(|word| word.starts_with('@'))
                            .map(|word| word.trim_start_matches('@').to_owned());
                        if let Some(token) = token {
                            ui.add_space(4.0);
                            for (id, name, hint) in suggestions(project, &people, &token) {
                                ui.horizontal(|ui| {
                                    let label = if hint.is_empty() { name.clone() } else { format!("{name}  ·  {hint}") };
                                    if ui
                                        .add(egui::Button::new(egui::RichText::new(format!("@{label}")).font(theme::regular(13.5))).frame(false))
                                        .clicked()
                                    {
                                        let cut = draft.len() - token.len() - 1;
                                        draft.truncate(cut);
                                        let first = name.split_whitespace().next().unwrap_or(&name).to_owned();
                                        draft.push_str(&format!("@{first} "));
                                        if !project.members.iter().any(|m| m.id == id) {
                                            add = Some(Member { id: id.clone(), name: name.clone(), ..Member::default() });
                                        }
                                    }
                                });
                            }
                        }
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if draft.trim() != project.context.trim()
                                && pill_button(ui, &palette, gettext(locale, "Save context").as_ref(), true).clicked()
                            {
                                save_context = true;
                            }
                        });
                    });
                    ui.add_space(12.0);
                    card_frame(&palette).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            theme::icon(ui, Icon::Users, 16.0, palette.accent);
                            theme::text(ui, gettext(locale, "Members"), theme::semibold(14.5), palette.text);
                            if !project.members.is_empty() {
                                chip(ui, &project.members.len().to_string(), palette.accent);
                            }
                        });
                        wrapped(
                            ui,
                            gettext(locale, "Saved contacts or any number, with their role. The analysis uses it to know who is who.").as_ref(),
                            theme::regular(12.5),
                            palette.secondary,
                        );
                        ui.add_space(6.0);
                        for (index, member) in project.members.iter().enumerate() {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 10.0;
                                let (avatar, _) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::hover());
                                widgets::paint_avatar(ui, &palette, avatar, &member.name, &member.id, None);
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 0.0;
                                    theme::text(ui, &member.name, theme::medium(14.0), palette.text);
                                    let phone = member.id.split('@').next().unwrap_or("");
                                    theme::text(ui, format!("+{phone}"), theme::regular(11.5), palette.dim);
                                });
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if theme::icon_button(ui, Icon::X, 14.0, palette.dim, palette.text, &gettext(locale, "Remove")).clicked() {
                                        remove = Some(index);
                                    }
                                    // The role being typed is kept until the field
                                    // is left, then saved.
                                    let key = egui::Id::new(("role", &project.id, &member.id));
                                    let mut role = ui
                                        .ctx()
                                        .data(|d| d.get_temp::<String>(key))
                                        .unwrap_or_else(|| member.role.clone());
                                    let field = field_frame(ui, &palette, |ui| {
                                        ui.add(
                                            egui::TextEdit::singleline(&mut role)
                                                .hint_text(gettext(locale, "Role").as_ref())
                                                .font(theme::regular(13.5))
                                                .frame(Frame::NONE)
                                                .desired_width(180.0),
                                        )
                                    });
                                    if field.lost_focus() {
                                        ui.ctx().data_mut(|d| d.remove_temp::<String>(key));
                                        if role != member.role {
                                            roles.push((index, role));
                                        }
                                    } else if field.changed() {
                                        ui.ctx().data_mut(|d| d.insert_temp(key, role));
                                    }
                                });
                            });
                            // Who they are, in a few lines, for the analysis.
                            let key = egui::Id::new(("about", &project.id, &member.id));
                            let mut about = ui
                                .ctx()
                                .data(|d| d.get_temp::<String>(key))
                                .unwrap_or_else(|| member.about.clone());
                            ui.horizontal(|ui| {
                                ui.add_space(40.0);
                                let field = field_frame(ui, &palette, |ui| {
                                    ui.add(
                                        egui::TextEdit::multiline(&mut about)
                                            .hint_text(gettext(locale, "Who they are: what they do, how to deal with them, what they care about…").as_ref())
                                            .font(theme::regular(13.0))
                                            .frame(Frame::NONE)
                                            .desired_rows(1)
                                            .desired_width(f32::INFINITY),
                                    )
                                });
                                if field.lost_focus() {
                                    ui.ctx().data_mut(|d| d.remove_temp::<String>(key));
                                    if about != member.about {
                                        abouts.push((index, about.clone()));
                                    }
                                } else if field.changed() {
                                    ui.ctx().data_mut(|d| d.insert_temp(key, about.clone()));
                                }
                            });
                            ui.add_space(8.0);
                        }
                        ui.add_space(4.0);
                        let width = ui.available_width();
                        widgets::search_field(
                            ui,
                            &palette,
                            egui::Id::new(("member-search", &project.id)),
                            &mut state.member_search,
                            gettext(locale, "Add: a name or a number").as_ref(),
                            width,
                        );
                        if !state.member_search.trim().is_empty() {
                            for (id, name, hint) in suggestions(project, &people, &state.member_search) {
                                if project.members.iter().any(|m| m.id == id) {
                                    continue;
                                }
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 10.0;
                                    theme::text(ui, &name, theme::regular(14.0), palette.text);
                                    if !hint.is_empty() {
                                        theme::text(ui, &hint, theme::regular(12.0), palette.dim);
                                    }
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        if pill_button(ui, &palette, gettext(locale, "Add").as_ref(), false).clicked() {
                                            add = Some(Member { id: id.clone(), name: name.clone(), ..Member::default() });
                                        }
                                    });
                                });
                            }
                        }
                    });
                });
        });
    let mut changed = false;
    if let Some(project) = state.map.project_mut(&project.id) {
        if save_context {
            project.context = draft.trim().to_owned();
            changed = true;
        }
        if let Some(member) = add {
            project.members.push(member);
            state.member_search.clear();
            changed = true;
        }
        for (index, role) in roles {
            if let Some(member) = project.members.get_mut(index) {
                member.role = role.trim().to_owned();
                changed = true;
            }
        }
        for (index, about) in abouts {
            if let Some(member) = project.members.get_mut(index) {
                member.about = about.trim().to_owned();
                changed = true;
            }
        }
        if let Some(index) = remove {
            project.members.remove(index);
            changed = true;
        }
    }
    state.context = Some((project.id.clone(), draft));
    if changed {
        save_map(app, state);
        engine::refresh(&project.id);
    }
}

// ----------------------------------------------------------------- messages

fn messages(
    app: &mut App,
    ui: &mut egui::Ui,
    state: &mut State,
    project: &Project,
    analysis: Option<&Analysis>,
) {
    let palette = app.palette;
    let locale = app.locale;
    if project.sources.is_empty() {
        widgets::empty_state(
            ui,
            &palette,
            Icon::ListChecks,
            gettext(locale, "This project has no chats").as_ref(),
            gettext(locale, "Add chats in Sources.").as_ref(),
        );
        return;
    }
    let chats = match &state.only_chat {
        Some(chat) => vec![chat.clone()],
        None => project.chat_ids(),
    };
    let (last, _, _) = activity(app, &chats);
    let since = jiff::Timestamp::now().as_second() - state.days * 86_400;
    let key = FeedKey {
        chats,
        since: since - since % 60,
        activity: last,
    };
    let archive: PathBuf = app.account().dirs.archive_db();
    let (all, loading, error) = state.feed.get(ui.ctx(), &archive, key);

    // A finding's origin: say which, and let it go.
    let highlight = state.highlight.clone();
    if let Some(origin) = &highlight {
        let mut clear = false;
        Frame::new()
            .fill(palette.warning.gamma_multiply(0.12))
            .corner_radius(CornerRadius::same(theme::RADIUS + 4))
            .inner_margin(Margin::symmetric(12, 8))
            .outer_margin(Margin {
                left: 20,
                right: 20,
                top: 10,
                bottom: 0,
            })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    theme::icon(ui, Icon::MessageCircle, 15.0, palette.warning);
                    let width = ui.available_width() - 34.0;
                    column(ui, width, |ui| {
                        theme::text(
                            ui,
                            gettext(locale, "Where it came from: {finding}")
                                .replace("{finding}", &origin.text),
                            theme::medium(13.0),
                            palette.text,
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if theme::icon_button(
                            ui,
                            Icon::X,
                            14.0,
                            palette.secondary,
                            palette.text,
                            &gettext(locale, "Clear"),
                        )
                        .clicked()
                        {
                            clear = true;
                        }
                    });
                });
            });
        if clear {
            state.highlight = None;
        }
    }
    // The bar: range, whose messages, and one source alone.
    let mut days = state.days;
    let mut show_all = state.show_all;
    let mut clear_chat = false;
    Frame::new()
        .inner_margin(Margin::symmetric(20, 10))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                if let Some(chat) = &state.only_chat {
                    let name = app.chat(chat).map(|c| c.name.clone()).unwrap_or_default();
                    widgets::filter_chip(ui, &palette, &name, 0, true);
                    if theme::icon_button(
                        ui,
                        Icon::X,
                        14.0,
                        palette.secondary,
                        palette.text,
                        &gettext(locale, "Every source"),
                    )
                    .clicked()
                    {
                        clear_chat = true;
                    }
                    ui.add_space(10.0);
                }
                for (value, label) in [
                    (false, gettext(locale, "In the project")),
                    (true, gettext(locale, "Everything")),
                ] {
                    if widgets::filter_chip(ui, &palette, label.as_ref(), 0, show_all == value)
                        .clicked()
                    {
                        show_all = value;
                    }
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for (range, label) in RANGES.iter().rev() {
                        if widgets::filter_chip(
                            ui,
                            &palette,
                            gettext(locale, label).as_ref(),
                            0,
                            days == *range,
                        )
                        .clicked()
                        {
                            days = *range;
                        }
                    }
                });
            });
        });
    state.days = days;
    state.show_all = show_all;
    if clear_chat {
        state.only_chat = None;
    }

    let shown: Vec<(&FeedMessage, Inclusion)> = all
        .iter()
        .map(|m| (m, state.map.inclusion(project, m, analysis)))
        .filter(|(_, inclusion)| show_all || *inclusion != Inclusion::Out)
        .collect();
    let bar_height = if state.selected.is_empty() { 0.0 } else { 56.0 };
    let body = ui.available_rect_before_wrap();
    let list_rect = Rect::from_min_max(body.min, pos2(body.right(), body.bottom() - bar_height));
    let mut list_ui = ui.new_child(egui::UiBuilder::new().max_rect(list_rect));
    if let Some(error) = error {
        widgets::empty_state(
            &mut list_ui,
            &palette,
            Icon::CircleAlert,
            gettext(locale, "Could not load the messages").as_ref(),
            &error,
        );
    } else if shown.is_empty() {
        let (title, text) = if loading {
            (
                gettext(locale, "Loading…"),
                gettext(locale, "Reading this project's chats."),
            )
        } else {
            (
                gettext(locale, "No messages in this range"),
                gettext(locale, "Choose a longer range above."),
            )
        };
        widgets::empty_state(
            &mut list_ui,
            &palette,
            Icon::MessageCircle,
            title.as_ref(),
            text.as_ref(),
        );
    } else {
        let base = project.color();
        let ids = project.chat_ids();
        let shade = |chat: &str| {
            let index = ids.iter().position(|id| id == chat).unwrap_or(0);
            base.lerp_to_gamma(palette.text, (index as f32 * 0.17) % 0.6)
        };
        let mut toggle = None;
        let mut open = None;
        let selecting = !state.selected.is_empty();
        list_ui
            .ctx()
            .data_mut(|data| data.insert_temp(egui::Id::new("projects-selecting"), selecting));
        egui::ScrollArea::vertical()
            .id_salt(("project-feed", &project.id, &state.only_chat))
            .auto_shrink([false, false])
            .stick_to_bottom(highlight.is_none())
            .show(&mut list_ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.add_space(4.0);
                let mut day = String::new();
                for (message, inclusion) in &shown {
                    let label = crate::util::day_label(locale, message.timestamp);
                    if label != day {
                        day_chip(ui, &palette, &label);
                        day = label;
                    }
                    let key = (message.chat.clone(), message.id.clone());
                    let selected = state.selected.contains(&key);
                    let mark = highlight.as_ref().map_or(0, |h| {
                        if h.chat == message.chat && h.message == message.id {
                            2
                        } else if h.chat == message.chat && (message.timestamp - h.at).abs() <= 600
                        {
                            1
                        } else {
                            0
                        }
                    });
                    let scroll = mark == 2 && highlight.as_ref().is_some_and(|h| h.scroll);
                    match feed_row(
                        app,
                        ui,
                        message,
                        shade(&message.chat),
                        *inclusion,
                        selected,
                        mark,
                        scroll,
                    ) {
                        RowClick::Toggle => toggle = Some(key),
                        RowClick::Open => open = Some(message.chat.clone()),
                        RowClick::None => {}
                    }
                }
                ui.add_space(16.0);
            });
        if let Some(origin) = state.highlight.as_mut() {
            origin.scroll = false;
        }
        if let Some(key) = toggle
            && !state.selected.remove(&key)
        {
            state.selected.insert(key);
        }
        if let Some(chat) = open {
            app.actions.push(Action::Open(Page::Chats));
            app.actions.push(Action::OpenChat(chat));
        }
    }
    if !state.selected.is_empty() {
        let bar = Rect::from_min_max(pos2(body.left(), body.bottom() - bar_height), body.max);
        let mut bar_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(bar.shrink2(vec2(20.0, 10.0)))
                .layout(Layout::left_to_right(Align::Center)),
        );
        selection_bar(app, &mut bar_ui, state, project);
    }
    ui.allocate_rect(body, Sense::hover());
}

/// What to do with the selected messages: in or out of this project, or
/// into another.
fn selection_bar(app: &mut App, ui: &mut egui::Ui, state: &mut State, project: &Project) {
    let palette = app.palette;
    let locale = app.locale;
    ui.painter().rect_filled(
        ui.max_rect().expand2(vec2(12.0, 6.0)),
        CornerRadius::same(theme::RADIUS + 8),
        if palette.vibrant() {
            palette.glass()
        } else {
            palette.surface_hover
        },
    );
    ui.spacing_mut().item_spacing.x = 8.0;
    theme::text(
        ui,
        gettext(locale, "{n} selected").replace("{n}", &state.selected.len().to_string()),
        theme::semibold(14.0),
        palette.text,
    );
    ui.add_space(8.0);
    let mut action: Option<(String, bool)> = None;
    if pill_button(ui, &palette, gettext(locale, "Belongs here").as_ref(), true).clicked() {
        action = Some((project.id.clone(), true));
    }
    if pill_button(
        ui,
        &palette,
        gettext(locale, "Not this project").as_ref(),
        false,
    )
    .clicked()
    {
        action = Some((project.id.clone(), false));
    }
    let other = pill_button(ui, &palette, gettext(locale, "File in…").as_ref(), false);
    let names: Vec<(String, String)> = state
        .map
        .projects
        .iter()
        .filter(|p| p.id != project.id)
        .map(|p| (p.id.clone(), p.name.clone()))
        .collect();
    let labels: Vec<&str> = names.iter().map(|(_, n)| n.as_str()).collect();
    egui::Popup::menu(&other)
        .width(widgets::menu_width(ui, &labels, false).max(180.0))
        .frame(widgets::menu_frame(&palette))
        .show(|ui| {
            for (id, name) in &names {
                if widgets::menu_item(ui, &palette, None, name) {
                    action = Some((id.clone(), true));
                    ui.close();
                }
            }
        });
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if theme::icon_button(
            ui,
            Icon::X,
            16.0,
            palette.secondary,
            palette.text,
            &gettext(locale, "Clear the selection"),
        )
        .clicked()
        {
            state.selected.clear();
        }
    });
    if let Some((target, include)) = action {
        for (chat, message) in std::mem::take(&mut state.selected) {
            state.map.pick(&chat, &message, &target, include);
            // Filing it in another project takes it out of this one.
            if target != project.id {
                state.map.pick(&chat, &message, &project.id, false);
            }
        }
        save_map(app, state);
        engine::refresh(&project.id);
        if target != project.id {
            engine::refresh(&target);
        }
    }
}

fn day_chip(ui: &mut egui::Ui, palette: &theme::Palette, label: &str) {
    // Painted rather than laid out: a column with a minimum width would
    // stretch a frame across it.
    ui.add_space(10.0);
    let galley =
        ui.painter()
            .layout_no_wrap(label.to_owned(), theme::medium(12.0), palette.secondary);
    let size = galley.size() + vec2(20.0, 8.0);
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), size.y), Sense::hover());
    let chip = Rect::from_center_size(row.center(), size);
    ui.painter().rect(
        chip,
        CornerRadius::same(theme::RADIUS),
        if palette.vibrant() {
            palette.glass()
        } else {
            palette.surface
        },
        palette.glass_edge(),
        egui::StrokeKind::Inside,
    );
    ui.painter().galley(
        chip.center() - galley.size() / 2.0,
        galley,
        palette.secondary,
    );
    ui.add_space(6.0);
}

/// A text field's box: the surface, a faint edge, rounded.
fn field_frame<R>(
    ui: &mut egui::Ui,
    palette: &theme::Palette,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    Frame::new()
        .fill(palette.surface)
        .stroke(palette.glass_edge())
        .corner_radius(CornerRadius::same(theme::RADIUS))
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, add)
        .inner
}

enum RowClick {
    None,
    Toggle,
    Open,
}

#[allow(clippy::too_many_arguments)]
/// One message: a box to select it, its chat and sender, when, what it
/// says, and whether it belongs. Messages outside the project are dimmed.
fn feed_row(
    app: &App,
    ui: &mut egui::Ui,
    message: &FeedMessage,
    color: Color32,
    inclusion: Inclusion,
    selected: bool,
    mark: u8,
    scroll: bool,
) -> RowClick {
    let palette = app.palette;
    let locale = app.locale;
    let out = inclusion == Inclusion::Out;
    let fade = |color: Color32| {
        if out {
            color.gamma_multiply(0.45)
        } else {
            color
        }
    };
    let sender = if message.from_me {
        gettext(locale, "You").into_owned()
    } else {
        message
            .sender_name
            .clone()
            .unwrap_or_else(|| app.display_name(&message.sender))
    };
    let mut text = app.resolve_mention_tokens(&message.text);
    // Speech or picture text read on this computer (src/transcribe.rs).
    if let Some(transcript) = message.transcript.as_deref().map(str::trim)
        && !transcript.is_empty()
    {
        text = format!("{text}: «{transcript}»");
    }
    let media = !matches!(message.kind.as_str(), "text" | "interactive");
    let mut toggled = false;
    let inner = Frame::new()
        .inner_margin(Margin {
            left: 16,
            right: 20,
            top: 6,
            bottom: 6,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let (check, response) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::click());
                let check = check.translate(vec2(0.0, 1.0));
                if selected {
                    ui.painter().rect_filled(check, 5.0, palette.accent);
                    theme::icon(
                        &mut ui.new_child(egui::UiBuilder::new().max_rect(check.shrink(2.0))),
                        Icon::Check,
                        14.0,
                        palette.on_accent,
                    );
                } else {
                    ui.painter().rect_stroke(
                        check,
                        5.0,
                        egui::Stroke::new(1.5, palette.dim.gamma_multiply(0.7)),
                        egui::StrokeKind::Inside,
                    );
                }
                if response.on_hover_text(gettext(locale, "Select")).clicked() {
                    toggled = true;
                }
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        let (bar, _) = ui.allocate_exact_size(vec2(3.0, 14.0), Sense::hover());
                        ui.painter().rect_filled(bar, 2.0, fade(color));
                        theme::text(ui, &message.chat_name, theme::semibold(13.0), fade(color));
                        theme::text(
                            ui,
                            &sender,
                            theme::regular(13.0),
                            fade(if message.from_me {
                                palette.accent
                            } else {
                                palette.secondary
                            }),
                        );
                        if inclusion == Inclusion::Unsure {
                            theme::text(
                                ui,
                                gettext(locale, "not classified yet"),
                                theme::regular(11.5),
                                palette.warning,
                            );
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            theme::text(
                                ui,
                                crate::util::clock(message.timestamp),
                                theme::regular(11.5),
                                palette.dim,
                            );
                        });
                    });
                    if let Some(reply) = &message.reply_to {
                        let who = reply.sender_name.clone().unwrap_or_default();
                        let quote = if who.is_empty() {
                            format!("↪ {}", reply.text)
                        } else {
                            format!("↪ {who}: {}", reply.text)
                        };
                        theme::text(ui, quote, theme::regular(12.5), palette.dim);
                    }
                    wrapped(
                        ui,
                        &text,
                        theme::regular(14.0),
                        fade(if media {
                            palette.secondary
                        } else {
                            palette.text
                        }),
                    );
                });
            });
        });
    let rect = inner.response.rect;
    // Where a finding came from: the message itself, and the talk around it.
    if mark > 0 {
        let band = rect.shrink2(vec2(8.0, 0.0));
        ui.painter().rect_filled(
            band,
            CornerRadius::same(theme::RADIUS + 2),
            palette
                .warning
                .gamma_multiply(if mark == 2 { 0.20 } else { 0.08 }),
        );
        if mark == 2 {
            ui.painter().rect_filled(
                Rect::from_min_size(band.min, vec2(3.0, band.height())),
                CornerRadius::same(2),
                palette.warning,
            );
        }
        if scroll {
            ui.scroll_to_rect(rect, Some(Align::Center));
        }
    }
    if selected {
        ui.painter().rect_filled(
            rect.shrink2(vec2(8.0, 0.0)),
            CornerRadius::same(theme::RADIUS + 2),
            palette.accent.gamma_multiply(0.10),
        );
    }
    if toggled {
        return RowClick::Toggle;
    }
    let response = ui.interact(
        rect,
        ui.id().with(("row", &message.chat, &message.id)),
        Sense::click(),
    );
    if response.hovered() && !selected {
        ui.painter().rect_filled(
            rect.shrink2(vec2(8.0, 0.0)),
            CornerRadius::same(theme::RADIUS + 2),
            palette.tint(0.035),
        );
    }
    // A click selects while a selection is under way; otherwise it opens
    // the chat.
    let selecting = ui
        .ctx()
        .data(|data| data.get_temp::<bool>(egui::Id::new("projects-selecting")))
        .unwrap_or(false);
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    if response.clicked() {
        if selecting {
            RowClick::Toggle
        } else {
            RowClick::Open
        }
    } else {
        RowClick::None
    }
}

// ------------------------------------------------------------------ sources

fn sources(app: &mut App, ui: &mut egui::Ui, state: &mut State, project: &Project) {
    let palette = app.palette;
    let locale = app.locale;
    let mut change: Option<SourceChange> = None;
    egui::ScrollArea::vertical()
        .id_salt(("project-sources", &project.id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            Frame::new()
                .inner_margin(Margin::symmetric(20, 16))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width().min(820.0));
                    card_frame(&palette).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        let mut picking = state.picking;
                        card_title(
                            ui,
                            &palette,
                            Icon::MessageCircle,
                            gettext(locale, "Sources").as_ref(),
                            palette.text,
                            |ui| {
                                if !picking
                                    && pill_button(
                                        ui,
                                        &palette,
                                        gettext(locale, "Add chat").as_ref(),
                                        false,
                                    )
                                    .clicked()
                                {
                                    picking = true;
                                }
                            },
                        );
                        state.picking = picking;
                        wrapped(
                            ui,
                            gettext(locale, "Everything brings the whole chat. Smart lets the analysis keep only what is about this project. Picked brings only the messages you choose in Messages.").as_ref(),
                            theme::regular(12.5),
                            palette.secondary,
                        );
                        ui.add_space(8.0);
                        if project.sources.is_empty() {
                            theme::text(
                                ui,
                                gettext(locale, "No chats yet."),
                                theme::regular(13.0),
                                palette.dim,
                            );
                        }
                        let mut sources = project.sources.clone();
                        sources.sort_by_key(|s| {
                            std::cmp::Reverse(app.chat(&s.chat).map_or(0, |c| c.last_activity))
                        });
                        for source in &sources {
                            if let Some(found) =
                                source_row(app, ui, &state.map, project, &source.chat, source.mode)
                            {
                                change = Some(found);
                            }
                        }
                    });
                    if state.picking {
                        ui.add_space(12.0);
                        if let Some(found) = picker(app, ui, state, project) {
                            change = Some(found);
                        }
                    }
                });
        });
    match change {
        Some(SourceChange::Add(chat, mode)) => {
            state.map.add_source(&project.id, &chat, mode);
            save_map(app, state);
            engine::refresh(&project.id);
        }
        Some(SourceChange::Mode(chat, mode)) => {
            state.map.add_source(&project.id, &chat, mode);
            save_map(app, state);
            engine::refresh(&project.id);
        }
        Some(SourceChange::Remove(chat)) => {
            state.map.remove_source(&project.id, &chat);
            save_map(app, state);
            engine::refresh(&project.id);
        }
        Some(SourceChange::Show(chat)) => {
            state.only_chat = Some(chat);
            state.show_all = true;
            state.tab = Tab::Messages;
        }
        Some(SourceChange::ClosePicker) => state.picking = false,
        None => {}
    }
}

enum SourceChange {
    Add(String, Mode),
    Mode(String, Mode),
    Remove(String),
    Show(String),
    ClosePicker,
}

/// One source: its picture and name, a warning when it also talks about
/// other things, its mode, and a menu.
fn source_row(
    app: &mut App,
    ui: &mut egui::Ui,
    map: &ProjectMap,
    project: &Project,
    chat: &str,
    mode: Mode,
) -> Option<SourceChange> {
    let palette = app.palette;
    let locale = app.locale;
    let name = app
        .chat(chat)
        .map(|c| c.name.clone())
        .unwrap_or_else(|| app.display_name(chat));
    let picture = app.avatar(chat);
    let warning = caution(app, map, &project.id, chat);
    let mut change = None;
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        let (avatar, _) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::hover());
        widgets::paint_avatar(ui, &palette, avatar, &name, chat, picture.as_deref());
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.set_max_width((ui.available_width() - 300.0).max(120.0));
            theme::text(ui, &name, theme::medium(14.5), palette.text);
            match &warning {
                Some(warning) if mode == Mode::All => {
                    theme::text(
                        ui,
                        format!("⚠ {warning}"),
                        theme::regular(12.0),
                        palette.warning,
                    );
                }
                Some(warning) => {
                    theme::text(ui, warning, theme::regular(12.0), palette.dim);
                }
                None => {
                    let kind = if ChatKind::from_id(chat) == ChatKind::Group {
                        gettext(locale, "Group")
                    } else {
                        gettext(locale, "Chat")
                    };
                    theme::text(ui, kind, theme::regular(12.0), palette.dim);
                }
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let more = theme::icon_button(
                ui,
                Icon::Ellipsis,
                16.0,
                palette.secondary,
                palette.text,
                &gettext(locale, "Source options"),
            );
            let show = gettext(locale, "See its messages");
            let remove = gettext(locale, "Remove from the project");
            egui::Popup::menu(&more)
                .width(widgets::menu_width(
                    ui,
                    &[show.as_ref(), remove.as_ref()],
                    true,
                ))
                .frame(widgets::menu_frame(&palette))
                .show(|ui| {
                    if widgets::menu_item(ui, &palette, Some(Icon::MessageCircle), &show) {
                        change = Some(SourceChange::Show(chat.to_owned()));
                        ui.close();
                    }
                    if widgets::menu_item(ui, &palette, Some(Icon::Trash), &remove) {
                        change = Some(SourceChange::Remove(chat.to_owned()));
                        ui.close();
                    }
                });
            ui.add_space(4.0);
            let options: Vec<(Mode, String)> = [Mode::Selected, Mode::Smart, Mode::All]
                .into_iter()
                .map(|m| (m, mode_label(locale, m)))
                .collect();
            if let Some(mode) = segmented(ui, &palette, &options, mode) {
                change = Some(SourceChange::Mode(chat.to_owned(), mode));
            }
        });
    });
    ui.add_space(4.0);
    change
}

/// The chats that can be added, most recent first, with the mode each would
/// start in and why care is needed.
fn picker(
    app: &mut App,
    ui: &mut egui::Ui,
    state: &mut State,
    project: &Project,
) -> Option<SourceChange> {
    let palette = app.palette;
    let locale = app.locale;
    let mut change = None;
    let mut search = state.search.clone();
    card_frame(&palette).show(ui, |ui| {
        ui.set_width(ui.available_width());
        card_title(
            ui,
            &palette,
            Icon::Plus,
            gettext(locale, "Add a chat").as_ref(),
            palette.text,
            |ui| {
                if theme::icon_button(
                    ui,
                    Icon::X,
                    16.0,
                    palette.secondary,
                    palette.text,
                    &gettext(locale, "Close"),
                )
                .clicked()
                {
                    change = Some(SourceChange::ClosePicker);
                }
            },
        );
        let width = ui.available_width();
        widgets::search_field(
            ui,
            &palette,
            egui::Id::new("projects-chat-search"),
            &mut search,
            gettext(locale, "Search chats").as_ref(),
            width,
        );
        ui.add_space(8.0);
        let needle = crate::util::search_key(search.trim());
        let candidates: Vec<(String, String)> = app
            .chats
            .iter()
            .filter(|chat| chat.last.is_some() && project.source(&chat.id).is_none())
            .filter(|chat| {
                needle.is_empty() || crate::util::search_key(&chat.name).contains(&needle)
            })
            .take(30)
            .map(|chat| (chat.id.clone(), chat.name.clone()))
            .collect();
        for (chat, name) in candidates {
            let mode = state.map.default_mode(&project.id, &chat);
            let warning = caution(app, &state.map, &project.id, &chat);
            let picture = app.avatar(&chat);
            ui.add_space(3.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                let (avatar, _) = ui.allocate_exact_size(Vec2::splat(32.0), Sense::hover());
                widgets::paint_avatar(ui, &palette, avatar, &name, &chat, picture.as_deref());
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.set_max_width((ui.available_width() - 160.0).max(120.0));
                    theme::text(ui, &name, theme::regular(14.0), palette.text);
                    if let Some(warning) = &warning {
                        theme::text(
                            ui,
                            format!("⚠ {warning}"),
                            theme::regular(12.0),
                            palette.warning,
                        );
                    }
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let label = gettext(locale, "Add · {mode}")
                        .replace("{mode}", &mode_label(locale, mode));
                    if pill_button(ui, &palette, &label, false).clicked() {
                        change = Some(SourceChange::Add(chat.clone(), mode));
                    }
                });
            });
            ui.add_space(3.0);
        }
    });
    state.search = search;
    change
}

// ------------------------------------------------------------------ buckets

/// One chat with its picture, name, unread count and when it was last
/// active, and a menu to file it. Returns where it was moved.
fn chat_row(app: &mut App, ui: &mut egui::Ui, map: &ProjectMap, id: &str) -> Option<Bucket> {
    let palette = app.palette;
    let locale = app.locale;
    let name = app
        .chat(id)
        .map(|c| c.name.clone())
        .unwrap_or_else(|| app.display_name(id));
    let (unread, last, group) = app.chat(id).map_or((0, 0, false), |c| {
        (c.unread, c.last_activity, c.kind == ChatKind::Group)
    });
    let picture = app.avatar(id);
    let (rect, response) = ui.allocate_exact_size(
        vec2(ui.available_width(), theme::ROW_HEIGHT),
        Sense::click(),
    );
    let menu_rect = Rect::from_min_max(pos2(rect.right() - 36.0, rect.top()), rect.max);
    if ui.is_rect_visible(rect) {
        if response.hovered() {
            widgets::row_highlight(ui, &palette, rect, palette.surface_hover);
        }
        let avatar_rect = Rect::from_min_size(
            pos2(rect.left() + 14.0, rect.center().y - 24.0),
            Vec2::splat(48.0),
        );
        widgets::paint_avatar(ui, &palette, avatar_rect, &name, id, picture.as_deref());
        let left = avatar_rect.right() + 12.0;
        let stamp = if last > 0 {
            crate::util::chat_stamp(locale, last)
        } else {
            String::new()
        };
        let stamp_line = widgets::line(ui, &stamp, theme::regular(11.5), palette.dim, 80.0, 1);
        let right = menu_rect.left() - 4.0;
        let badge = if unread > 0 {
            widgets::badge(
                ui,
                &palette,
                pos2(right - 10.0, rect.center().y),
                unread,
                false,
            ) + 6.0
        } else {
            0.0
        };
        let stamp_x = right - badge - stamp_line.size().x;
        stamp_line.paint(
            ui,
            pos2(stamp_x, rect.center().y - stamp_line.size().y / 2.0),
            palette.dim,
        );
        let width = (stamp_x - 8.0 - left).max(0.0);
        widgets::line(ui, &name, theme::medium(15.0), palette.text, width, 1).paint(
            ui,
            pos2(left, rect.top() + 13.0),
            palette.text,
        );
        let kind = if group {
            gettext(locale, "Group")
        } else {
            gettext(locale, "Chat")
        };
        widgets::line(
            ui,
            kind.as_ref(),
            theme::regular(13.0),
            palette.secondary,
            width,
            1,
        )
        .paint(ui, pos2(left, rect.top() + 37.0), palette.secondary);
    }
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(menu_rect)
            .layout(Layout::right_to_left(Align::Center)),
    );
    let more = theme::icon_button(
        &mut child,
        Icon::Ellipsis,
        16.0,
        palette.secondary,
        palette.text,
        &gettext(locale, "Move to"),
    );
    let current = map.bucket_of(id);
    let mut targets: Vec<(Bucket, Option<Color32>, String)> = map
        .projects
        .iter()
        .map(|p| {
            (
                Bucket::Project(p.id.clone()),
                Some(p.color()),
                p.name.clone(),
            )
        })
        .collect();
    targets.push((
        Bucket::Personal,
        None,
        gettext(locale, "Personal").into_owned(),
    ));
    targets.push((
        Bucket::Ignored,
        None,
        gettext(locale, "Ignore").into_owned(),
    ));
    targets.push((
        Bucket::Unassigned,
        None,
        gettext(locale, "To file").into_owned(),
    ));
    let labels: Vec<&str> = targets.iter().map(|t| t.2.as_str()).collect();
    let mut chosen = None;
    egui::Popup::menu(&more)
        .width(widgets::menu_width(&child, &labels, true).max(200.0))
        .frame(widgets::menu_frame(&palette))
        .show(|ui| {
            egui::ScrollArea::vertical()
                .max_height(360.0)
                .show(ui, |ui| {
                    for (bucket, color, label) in &targets {
                        if *bucket == current {
                            continue;
                        }
                        let icon = match bucket {
                            Bucket::Project(_) => None,
                            Bucket::Personal => Some(Icon::User),
                            Bucket::Ignored => Some(Icon::EyeOff),
                            Bucket::Unassigned => Some(Icon::ListChecks),
                        };
                        let item_top = ui.cursor().top();
                        if widgets::menu_item(ui, &palette, icon, label) {
                            chosen = Some(bucket.clone());
                            ui.close();
                        }
                        if let Some(color) = color {
                            ui.painter().circle_filled(
                                pos2(ui.min_rect().left() + 19.0, item_top + 14.0),
                                5.0,
                                *color,
                            );
                        }
                    }
                });
        });
    if response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
    {
        app.actions.push(Action::Open(Page::Chats));
        app.actions.push(Action::OpenChat(id.to_owned()));
    }
    chosen
}

fn bucket(app: &mut App, ui: &mut egui::Ui, state: &mut State, bucket: Bucket) {
    let palette = app.palette;
    let locale = app.locale;
    let mut chats: Vec<(String, i64)> = app
        .chats
        .iter()
        .filter(|chat| {
            state.map.bucket_of(&chat.id) == bucket
                && (bucket != Bucket::Unassigned || chat.last.is_some())
        })
        .map(|chat| (chat.id.clone(), chat.last_activity))
        .collect();
    chats.sort_by_key(|(_, last)| std::cmp::Reverse(*last));
    let (title, icon, body) = match bucket {
        Bucket::Personal => (
            gettext(locale, "Personal"),
            Icon::User,
            gettext(
                locale,
                "Personal chats stay out of projects and the analysis.",
            ),
        ),
        Bucket::Ignored => (
            gettext(locale, "Ignored"),
            Icon::EyeOff,
            gettext(
                locale,
                "Ignored chats stay out of projects and the analysis.",
            ),
        ),
        _ => (
            gettext(locale, "To file"),
            Icon::ListChecks,
            gettext(locale, "Choose a project for each chat with its menu."),
        ),
    };
    let subtitle = gettext(locale, "{count} chats").replace("{count}", &chats.len().to_string());
    header(app, ui, None, title.as_ref(), &subtitle, |_| {});
    if chats.is_empty() {
        widgets::empty_state(
            ui,
            &palette,
            icon,
            gettext(locale, "Nothing here").as_ref(),
            body.as_ref(),
        );
        return;
    }
    let mut moves = Vec::new();
    egui::ScrollArea::vertical()
        .id_salt(("projects-bucket", format!("{bucket:?}")))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            Frame::new()
                .inner_margin(Margin {
                    left: 20,
                    right: 20,
                    top: 10,
                    bottom: 4,
                })
                .show(ui, |ui| {
                    theme::text(ui, body.as_ref(), theme::regular(13.0), palette.secondary);
                });
            let width = ui.available_width().min(760.0);
            ui.vertical_centered(|ui| {
                ui.set_max_width(width);
                for (chat, _) in &chats {
                    if let Some(target) = chat_row(app, ui, &state.map, chat) {
                        moves.push((chat.clone(), target));
                    }
                }
            });
            ui.add_space(16.0);
        });
    for (chat, target) in moves {
        state.map.set_bucket(&chat, &target);
        save_map(app, state);
    }
}

// ------------------------------------------------------------------- engine

/// Where the analysis is chosen: who reads the messages (a subscription's
/// own tool, or an API with a key), with what model, and how often.
fn engine_page(app: &mut App, ui: &mut egui::Ui, state: &mut State) {
    let palette = app.palette;
    let locale = app.locale;
    let saved = EngineConfig::load(&app.dirs.config);
    let mut draft = state.engine_draft.clone().unwrap_or_else(|| saved.clone());
    let status = engine::status();
    header(
        app,
        ui,
        None,
        gettext(locale, "Analysis").as_ref(),
        gettext(
            locale,
            "Who reads your projects' chats to fill their panels",
        )
        .as_ref(),
        |_| {},
    );
    let mut save_key = false;
    let ctx = ui.ctx().clone();
    let config_dir = app.dirs.config.clone();
    egui::ScrollArea::vertical()
        .id_salt("projects-engine")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            Frame::new()
                .inner_margin(Margin::symmetric(20, 16))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width().min(680.0));
                    ui.spacing_mut().item_spacing.y = 12.0;
                    // Where things stand, in one line.
                    let connected = matches!(engine::tested(draft.provider), Some(Ok(_)));
                    let (icon, color, line) = if draft.provider == Provider::Off {
                        (
                            Icon::CircleAlert,
                            palette.secondary,
                            gettext(locale, "The analysis is off: choose an engine below.").into_owned(),
                        )
                    } else if connected {
                        (
                            Icon::CircleCheck,
                            palette.accent,
                            gettext(locale, "Ready: {engine} is connected and analyzes your projects.")
                                .replace("{engine}", draft.provider.label()),
                        )
                    } else {
                        (
                            Icon::Clock,
                            palette.warning,
                            gettext(locale, "{engine} is chosen. Finish the steps below to connect it.")
                                .replace("{engine}", draft.provider.label()),
                        )
                    };
                    Frame::new()
                        .fill(color.gamma_multiply(0.12))
                        .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.5)))
                        .corner_radius(CornerRadius::same(theme::RADIUS + 6))
                        .inner_margin(Margin::symmetric(14, 12))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 10.0;
                                theme::icon(ui, icon, 18.0, color);
                                let width = ui.available_width();
                                column(ui, width, |ui| {
                                    wrapped(ui, &line, theme::medium(14.0), palette.text);
                                });
                            });
                        });
                    card_frame(&palette).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        card_title(
                            ui,
                            &palette,
                            Icon::Settings,
                            gettext(locale, "Engine").as_ref(),
                            palette.text,
                            |_| {},
                        );
                        let mut pick = |ui: &mut egui::Ui, label: &str, providers: &[Provider]| {
                            theme::text(ui, label, theme::regular(12.5), palette.secondary);
                            let options: Vec<(Provider, String)> = providers
                                .iter()
                                .map(|p| {
                                    let label = if *p == Provider::Off {
                                        gettext(locale, "Off").into_owned()
                                    } else {
                                        p.label().to_owned()
                                    };
                                    (*p, label)
                                })
                                .collect();
                            ui.horizontal_wrapped(|ui| {
                                if let Some(provider) =
                                    segmented(ui, &palette, &options, draft.provider)
                                {
                                    draft.provider = provider;
                                    draft.model = provider.default_model().to_owned();
                                    state.key_draft.clear();
                                    state.model_search.clear();
                                }
                            });
                        };
                        pick(
                            ui,
                            gettext(locale, "With your subscription, no key").as_ref(),
                            &[Provider::ClaudeCode, Provider::CodexCli],
                        );
                        theme::text(
                            ui,
                            gettext(locale, "Google no longer lets personal accounts use Gemini this way; for Gemini, use Gemini API below (Google AI Studio gives free keys)."),
                            theme::regular(12.0),
                            palette.dim,
                        );
                        ui.add_space(4.0);
                        pick(
                            ui,
                            gettext(locale, "With an API key, paid by use").as_ref(),
                            &[
                                Provider::Anthropic,
                                Provider::OpenAi,
                                Provider::GeminiApi,
                                Provider::OpenRouter,
                            ],
                        );
                        ui.add_space(4.0);
                        pick(ui, "", &[Provider::Off]);
                        ui.add_space(8.0);
                        if draft.provider == Provider::Off {
                            wrapped(
                                ui,
                                gettext(locale, "The panels stay empty and nothing leaves this computer.").as_ref(),
                                theme::regular(13.5),
                                palette.secondary,
                            );
                            return;
                        }
                        if let Some(tool) = draft.provider.tool() {
                            tool_setup(&ctx, ui, &palette, locale, tool, &config_dir, &draft);
                        } else {
                            let has = engine::has_key(draft.provider);
                            ui.horizontal(|ui| {
                                theme::text(
                                    ui,
                                    gettext(locale, "API key"),
                                    theme::regular(13.5),
                                    palette.secondary,
                                );
                                let hint = if has {
                                    gettext(locale, "Saved in the keychain")
                                } else {
                                    gettext(locale, "Paste your key")
                                };
                                ui.add(
                                    egui::TextEdit::singleline(&mut state.key_draft)
                                        .password(true)
                                        .hint_text(hint.as_ref())
                                        .desired_width(300.0),
                                );
                                if !state.key_draft.trim().is_empty()
                                    && pill_button(
                                        ui,
                                        &palette,
                                        gettext(locale, "Save key").as_ref(),
                                        true,
                                    )
                                    .clicked()
                                {
                                    save_key = true;
                                }
                            });
                        }
                        if let Some(url) = key_page(draft.provider) {
                            ui.hyperlink_to(gettext(locale, "Get a key"), url);
                        }
                        ui.add_space(6.0);
                        if let Some(model) =
                            model_picker(&ctx, ui, &palette, locale, draft.provider, &draft.model, &mut state.model_search)
                        {
                            draft.model = model;
                        }
                        if draft.provider.tool().is_none() {
                            ui.add_space(6.0);
                            connection_test(&ctx, ui, &palette, locale, &config_dir, &draft);
                        }
                    });
                    card_frame(&palette).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        card_title(
                            ui,
                            &palette,
                            Icon::Clock,
                            gettext(locale, "How often").as_ref(),
                            palette.text,
                            |_| {},
                        );
                        let options: Vec<(u32, String)> =
                            [(30, "30 min"), (60, "1 h"), (120, "2 h"), (360, "6 h")]
                                .into_iter()
                                .map(|(m, l)| (m, l.to_owned()))
                                .collect();
                        if let Some(every) = segmented(ui, &palette, &options, draft.every) {
                            draft.every = every;
                        }
                        wrapped(
                            ui,
                            gettext(locale, "Only projects with new messages are read again.").as_ref(),
                            theme::regular(12.5),
                            palette.dim,
                        );
                    });
                    card_frame(&palette).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        card_title(
                            ui,
                            &palette,
                            Icon::Lock,
                            gettext(locale, "Privacy").as_ref(),
                            palette.text,
                            |_| {},
                        );
                        wrapped(
                            ui,
                            gettext(locale, "The analysis sends the last week of messages from your projects' sources to the provider you choose. Personal and ignored chats, and chats in no project, are never sent. Messages you mark as not belonging to a project are not sent for it.").as_ref(),
                            theme::regular(13.0),
                            palette.secondary,
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 10.0;
                        if saved.provider != Provider::Off
                            && status.running.is_none()
                            && pill_button(
                                ui,
                                &palette,
                                gettext(locale, "Analyze now").as_ref(),
                                draft == saved,
                            )
                            .clicked()
                        {
                            engine::run_now();
                        }
                        if let Some(running) = &status.running {
                            theme::text(
                                ui,
                                gettext(locale, "Analyzing {project}…").replace("{project}", running),
                                theme::regular(13.0),
                                palette.secondary,
                            );
                        } else if let Some(at) = status.last_run {
                            theme::text(
                                ui,
                                gettext(locale, "Last run {when} ago")
                                    .replace("{when}", &ago(locale, at)),
                                theme::regular(13.0),
                                palette.dim,
                            );
                        }
                    });
                    if let Some(error) = &status.last_error {
                        wrapped(ui, error, theme::regular(12.5), palette.danger);
                    }
                });
        });
    if save_key {
        if let Err(error) = engine::set_key(draft.provider, &state.key_draft) {
            log::warn!("analysis: the key could not be saved: {error}");
        }
        state.key_draft.clear();
        // The list of models needs the key.
        let _ = engine::models(&ctx, draft.provider, true);
    }
    // Every choice is kept as it is made.
    if draft != saved
        && let Err(error) = draft.save(&app.dirs.config)
    {
        log::warn!("analysis: settings could not be saved: {error}");
    }
    state.engine_draft = None;
}

/// Where each API's keys are made.
fn key_page(provider: Provider) -> Option<&'static str> {
    match provider {
        Provider::Anthropic => Some("https://console.anthropic.com/settings/keys"),
        Provider::OpenAi => Some("https://platform.openai.com/api-keys"),
        Provider::OpenRouter => Some("https://openrouter.ai/keys"),
        Provider::GeminiApi => Some("https://aistudio.google.com/apikey"),
        _ => None,
    }
}

/// A numbered step: its number, title and line, and whatever `right` draws.
fn step(
    ui: &mut egui::Ui,
    palette: &theme::Palette,
    number: usize,
    done: bool,
    title: &str,
    detail: &str,
    right: impl FnOnce(&mut egui::Ui),
) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        let (dot, _) = ui.allocate_exact_size(Vec2::splat(26.0), Sense::hover());
        ui.painter().circle_filled(
            dot.center(),
            13.0,
            if done {
                palette.accent
            } else {
                palette.surface_active
            },
        );
        if done {
            theme::icon(
                &mut ui.new_child(egui::UiBuilder::new().max_rect(dot.shrink(5.0))),
                Icon::Check,
                16.0,
                palette.on_accent,
            );
        } else {
            let galley = ui.painter().layout_no_wrap(
                number.to_string(),
                theme::semibold(13.0),
                palette.text,
            );
            ui.painter()
                .galley(dot.center() - galley.size() / 2.0, galley, palette.text);
        }
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.set_max_width((ui.available_width() - 190.0).max(160.0));
            theme::text(ui, title, theme::medium(14.0), palette.text);
            if !detail.is_empty() {
                wrapped(ui, detail, theme::regular(12.5), palette.secondary);
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), right);
    });
    ui.add_space(6.0);
}

/// Setting up a subscription's tool from here: install it, sign in once,
/// and the connection test below confirms it.
fn tool_setup(
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    palette: &theme::Palette,
    locale: crate::i18n::Locale,
    tool: crate::ai_tools::Tool,
    config: &std::path::Path,
    draft: &EngineConfig,
) {
    use crate::ai_tools;
    let tested = engine::tested(draft.provider);
    let connected = matches!(tested, Some(Ok(_)));
    let testing = matches!(tested, Some(Err(ref e)) if e.is_empty());
    // After Sign in, the connection is tried every few seconds for three
    // minutes, so the steps tick themselves once the person is in.
    let started_id = egui::Id::new(("sign-in-started", tool.binary()));
    let tried_id = egui::Id::new(("sign-in-tried", tool.binary()));
    let now = ctx.input(|input| input.time);
    let started = ctx.data(|data| data.get_temp::<f64>(started_id));
    let waiting = !connected && started.is_some_and(|at| now - at < 180.0);
    if waiting {
        let tried = ctx
            .data(|data| data.get_temp::<f64>(tried_id))
            .unwrap_or(0.0);
        if !testing && now - tried >= 6.0 {
            ctx.data_mut(|data| data.insert_temp(tried_id, now));
            engine::test(ctx, config, draft);
        }
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
    }
    let installed = ai_tools::installed(tool);
    let job = ai_tools::job(tool);
    let running = job.as_ref().is_some_and(|job| job.running);
    let install_detail = if installed {
        gettext(locale, "Installed on this Mac.").into_owned()
    } else if running {
        gettext(locale, "Installing… this takes a minute or two.").into_owned()
    } else {
        gettext(
            locale,
            "Downloads it from its maker. No administrator password needed.",
        )
        .into_owned()
    };
    step(
        ui,
        palette,
        1,
        installed,
        gettext(locale, "Install").as_ref(),
        &install_detail,
        |ui| {
            if running {
                ui.add(egui::Spinner::new().size(18.0).color(palette.accent));
            } else if !installed
                && pill_button(ui, palette, gettext(locale, "Install").as_ref(), true).clicked()
            {
                ai_tools::install(ctx, tool);
            }
        },
    );
    if let Some(job) = &job {
        if running && !job.log.is_empty() {
            let tail: Vec<&str> = job.log.lines().rev().take(2).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            theme::text(
                ui,
                tail.join("  ·  "),
                egui::FontId::monospace(11.5),
                palette.dim,
            );
        }
        if let Some(Err(error)) = &job.result {
            wrapped(ui, error, theme::regular(12.5), palette.danger);
        }
    }
    let sign_in_error = egui::Id::new(("sign-in-error", tool.binary()));
    let sign_in_detail = if connected {
        gettext(locale, "Signed in.").into_owned()
    } else if waiting {
        gettext(
            locale,
            "Waiting for you to sign in in the Terminal and the browser…",
        )
        .into_owned()
    } else {
        gettext(locale, "Opens the Terminal on its sign-in. Follow it in the browser, then close the window. Only once.").into_owned()
    };
    step(
        ui,
        palette,
        2,
        connected,
        &gettext(locale, "Sign in with your {account} account")
            .replace("{account}", tool.account()),
        &sign_in_detail,
        |ui| {
            if waiting {
                ui.add(egui::Spinner::new().size(18.0).color(palette.accent));
            } else if installed
                && pill_button(
                    ui,
                    palette,
                    if connected {
                        gettext(locale, "Sign in again")
                    } else {
                        gettext(locale, "Sign in")
                    }
                    .as_ref(),
                    !connected,
                )
                .clicked()
            {
                let result = ai_tools::sign_in(tool, locale).err();
                if result.is_none() {
                    let now = ui.ctx().input(|input| input.time);
                    ui.ctx().data_mut(|data| {
                        data.insert_temp(started_id, now);
                        data.insert_temp(tried_id, now);
                    });
                }
                ui.ctx()
                    .data_mut(|data| data.insert_temp(sign_in_error, result));
            }
        },
    );
    if let Some(Some(error)) = ui
        .ctx()
        .data(|data| data.get_temp::<Option<String>>(sign_in_error))
    {
        wrapped(ui, &error, theme::regular(12.5), palette.danger);
    }
    let check_detail = match &tested {
        Some(Ok(_)) => gettext(locale, "It answers. The analysis is ready to run.").into_owned(),
        Some(Err(error)) if !error.is_empty() && !waiting => error.clone(),
        _ => gettext(locale, "Asks it for one word to confirm everything works.").into_owned(),
    };
    step(
        ui,
        palette,
        3,
        connected,
        gettext(locale, "Test the connection").as_ref(),
        &check_detail,
        |ui| {
            if testing {
                ui.add(egui::Spinner::new().size(18.0).color(palette.accent));
            } else if installed
                && pill_button(ui, palette, gettext(locale, "Test").as_ref(), false).clicked()
            {
                engine::test(ctx, config, draft);
            }
        },
    );
}

/// Asks the chosen engine for one word and says whether it answered.
fn connection_test(
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    palette: &theme::Palette,
    locale: crate::i18n::Locale,
    config: &std::path::Path,
    draft: &EngineConfig,
) {
    let result = engine::tested(draft.provider);
    let testing = matches!(result, Some(Err(ref e)) if e.is_empty());
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        if testing {
            ui.add(egui::Spinner::new().size(16.0).color(palette.accent));
            theme::text(
                ui,
                gettext(locale, "Testing…"),
                theme::regular(13.0),
                palette.secondary,
            );
        } else if pill_button(
            ui,
            palette,
            gettext(locale, "Test the connection").as_ref(),
            false,
        )
        .clicked()
        {
            engine::test(ctx, config, draft);
        }
        if let Some(Ok(_)) = &result {
            theme::icon(ui, Icon::CircleCheck, 16.0, palette.accent);
            theme::text(
                ui,
                gettext(locale, "Connected"),
                theme::medium(13.0),
                palette.accent,
            );
        }
    });
    if let Some(Err(error)) = &result
        && !error.is_empty()
    {
        wrapped(ui, error, theme::regular(12.5), palette.danger);
    }
}

/// The model in use, as a button that opens a searchable list: the
/// recommended models first, with why, then every model the provider
/// offers. A name typed that is not listed can be used as it is.
#[allow(clippy::too_many_arguments)]
fn model_picker(
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    palette: &theme::Palette,
    locale: crate::i18n::Locale,
    provider: Provider,
    current: &str,
    search: &mut String,
) -> Option<String> {
    let suggestions = provider.suggestions();
    let note = suggestions.iter().find(|s| s.id == current).map(|s| s.note);
    let shown = if current.is_empty() {
        gettext(locale, "Default model").into_owned()
    } else {
        current.to_owned()
    };
    let recommended = suggestions.first().is_some_and(|s| s.id == current);
    let mut chosen = None;
    ui.horizontal(|ui| {
        theme::text(
            ui,
            gettext(locale, "Model"),
            theme::regular(13.5),
            palette.secondary,
        );
        let label = if recommended {
            format!("{shown}  ·  {}", gettext(locale, "recommended"))
        } else {
            shown.clone()
        };
        let galley = ui
            .painter()
            .layout_no_wrap(label.clone(), theme::medium(13.5), palette.text);
        let width = (galley.size().x + 44.0).max(240.0);
        let (rect, button) = ui.allocate_exact_size(vec2(width, 32.0), Sense::click());
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(theme::RADIUS + 2),
            if button.hovered() {
                palette.surface_hover
            } else {
                palette.surface
            },
        );
        ui.painter().galley(
            pos2(rect.left() + 12.0, rect.center().y - galley.size().y / 2.0),
            galley,
            palette.text,
        );
        let chevron = Rect::from_center_size(
            pos2(rect.right() - 16.0, rect.center().y),
            Vec2::splat(14.0),
        );
        theme::icon(
            &mut ui.new_child(egui::UiBuilder::new().max_rect(chevron)),
            Icon::ChevronDown,
            14.0,
            palette.secondary,
        );
        let button = button.on_hover_cursor(egui::CursorIcon::PointingHand);
        egui::Popup::menu(&button)
            .width(380.0)
            .frame(widgets::menu_frame(palette))
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                ui.set_width(380.0);
                widgets::search_field(
                    ui,
                    palette,
                    egui::Id::new("model-search"),
                    search,
                    gettext(locale, "Search models").as_ref(),
                    380.0,
                );
                ui.add_space(6.0);
                let needle = search.trim().to_lowercase();
                egui::ScrollArea::vertical()
                    .max_height(340.0)
                    .show(ui, |ui| {
                        let row =
                            |ui: &mut egui::Ui, id: &str, note: Option<&'static str>| -> bool {
                                let label = if id.is_empty() {
                                    gettext(locale, "Default model").into_owned()
                                } else {
                                    id.to_owned()
                                };
                                let height = if note.is_some() { 44.0 } else { 30.0 };
                                let (rect, response) = ui.allocate_exact_size(
                                    vec2(ui.available_width(), height),
                                    Sense::click(),
                                );
                                if response.hovered() || id == current {
                                    ui.painter().rect_filled(
                                        rect,
                                        CornerRadius::same(theme::RADIUS),
                                        if id == current {
                                            palette.surface_active
                                        } else {
                                            palette.surface_hover
                                        },
                                    );
                                }
                                let text = ui.painter().layout_no_wrap(
                                    label,
                                    theme::medium(13.5),
                                    palette.text,
                                );
                                ui.painter().galley(
                                    pos2(rect.left() + 10.0, rect.top() + 6.0),
                                    text,
                                    palette.text,
                                );
                                if let Some(note) = note {
                                    let note = ui.painter().layout_no_wrap(
                                        gettext(locale, note).into_owned(),
                                        theme::regular(12.0),
                                        palette.secondary,
                                    );
                                    ui.painter().galley(
                                        pos2(rect.left() + 10.0, rect.top() + 24.0),
                                        note,
                                        palette.secondary,
                                    );
                                }
                                response
                                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                                    .clicked()
                            };
                        theme::text(
                            ui,
                            gettext(locale, "Recommended"),
                            theme::medium(12.5),
                            palette.accent,
                        );
                        for suggestion in suggestions.iter().filter(|s| {
                            needle.is_empty()
                                || s.id.contains(&needle)
                                || s.note.to_lowercase().contains(&needle)
                        }) {
                            if row(ui, suggestion.id, Some(suggestion.note)) {
                                chosen = Some(suggestion.id.to_owned());
                            }
                        }
                        if provider.lists_models() {
                            ui.add_space(6.0);
                            theme::text(
                                ui,
                                gettext(locale, "Every model"),
                                theme::medium(12.5),
                                palette.accent,
                            );
                            match engine::models(ctx, provider, false) {
                                None => {
                                    theme::text(
                                        ui,
                                        gettext(locale, "Loading…"),
                                        theme::regular(12.5),
                                        palette.dim,
                                    );
                                }
                                Some(Err(error)) => {
                                    wrapped(ui, &error, theme::regular(12.5), palette.warning);
                                }
                                Some(Ok(list)) => {
                                    for id in list
                                        .iter()
                                        .filter(|id| {
                                            needle.is_empty() || id.to_lowercase().contains(&needle)
                                        })
                                        .filter(|id| {
                                            !suggestions.iter().any(|s| s.id == id.as_str())
                                        })
                                        .take(300)
                                    {
                                        if row(ui, id, None) {
                                            chosen = Some(id.clone());
                                        }
                                    }
                                }
                            }
                        }
                        let typed = search.trim();
                        if !typed.is_empty() && !suggestions.iter().any(|s| s.id == typed) {
                            ui.add_space(6.0);
                            let label = gettext(locale, "Use “{model}”").replace("{model}", typed);
                            if widgets::menu_item(ui, palette, Some(Icon::Pencil), &label) {
                                chosen = Some(typed.to_owned());
                            }
                        }
                    });
                if chosen.is_some() {
                    ui.close();
                }
            });
    });
    if let Some(note) = note {
        theme::text(ui, gettext(locale, note), theme::regular(12.0), palette.dim);
    }
    if chosen.is_some() {
        search.clear();
    }
    chosen
}

// -------------------------------------------------------------- new project

fn new_project(app: &mut App, ui: &mut egui::Ui, state: &mut State) {
    let palette = app.palette;
    let locale = app.locale;
    header(
        app,
        ui,
        None,
        gettext(locale, "New project").as_ref(),
        gettext(locale, "Name it, then add its chats.").as_ref(),
        |_| {},
    );
    let height = ui.available_height();
    let mut done = None;
    ui.vertical_centered(|ui| {
        ui.add_space((height / 2.0 - 120.0).max(24.0));
        let (mark, _) = ui.allocate_exact_size(Vec2::splat(72.0), Sense::hover());
        let color = crate::projects::parse_color(
            crate::projects::COLORS[state.map.projects.len() % crate::projects::COLORS.len()],
        )
        .unwrap_or(palette.accent);
        paint_mark(
            ui,
            mark,
            color,
            if state.draft.trim().is_empty() {
                "?"
            } else {
                &state.draft
            },
        );
        ui.add_space(18.0);
        let field = Frame::new()
            .fill(palette.surface)
            .corner_radius(CornerRadius::same(theme::RADIUS + 2))
            .inner_margin(Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut state.draft)
                        .id(egui::Id::new("new-project-name"))
                        .hint_text(
                            egui::RichText::new(gettext(locale, "Project name").as_ref())
                                .color(palette.dim)
                                .font(theme::semibold(16.0)),
                        )
                        .font(theme::semibold(16.0))
                        .text_color(palette.text)
                        .frame(Frame::NONE)
                        .desired_width(280.0)
                        .horizontal_align(Align::Center),
                )
            });
        if ui.memory(|m| m.focused().is_none()) {
            field.inner.request_focus();
        }
        let enter = field.inner.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        theme::focus_outline(
            ui,
            field.inner.id,
            field.response.rect,
            f32::from(theme::RADIUS + 2),
        );
        ui.add_space(18.0);
        ui.horizontal(|ui| {
            let buttons = 2.0 * 100.0 + 12.0;
            ui.add_space(((ui.available_width() - buttons) / 2.0).max(0.0));
            if pill_button(ui, &palette, gettext(locale, "Cancel").as_ref(), false).clicked() {
                done = Some(false);
            }
            ui.add_space(12.0);
            if pill_button(ui, &palette, gettext(locale, "Create").as_ref(), true).clicked()
                || enter
            {
                done = Some(true);
            }
        });
    });
    match done {
        Some(true) if !state.draft.trim().is_empty() => {
            let name = state.draft.clone();
            let id = state.map.add(&name);
            save_map(app, state);
            state.selection = Some(Selection::Project(id));
            state.tab = Tab::Sources;
            state.picking = true;
            state.adding = false;
        }
        Some(false) => state.adding = false,
        _ => {}
    }
}
