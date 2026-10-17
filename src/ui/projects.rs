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
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use egui::{Align, Color32, CornerRadius, Frame, Layout, Margin, Rect, Sense, Vec2, pos2, vec2};

use crate::app::App;
use crate::i18n::gettext;
use crate::model::{Action, ChatKind, Page};
use crate::projects::engine::{self, EngineConfig, Provider};
use crate::projects::{
    Analysis, Bucket, Feed, FeedKey, FeedMessage, Finding, Inclusion, Mode, Project, ProjectMap,
    Ticks,
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
        }
    }
}

type Shared = Arc<Mutex<State>>;

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
            ui.add_space(6.0);
            for (value, label) in [
                (Tab::Sources, gettext(locale, "Sources")),
                (Tab::Messages, gettext(locale, "Messages")),
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
        state.selection = None;
        return;
    }
    match state.tab {
        Tab::Panel => panel(app, ui, state, &project, analysis.as_ref(), &ticks),
        Tab::Messages => messages(app, ui, state, &project, analysis.as_ref()),
        Tab::Sources => sources(app, ui, state, &project),
    }
}

// -------------------------------------------------------------------- panel

/// A finding's line under its text: who, the chat, how long ago, when due.
fn finding_meta(app: &App, finding: &Finding) -> String {
    let locale = app.locale;
    let mut parts = Vec::new();
    if let Some(who) = &finding.who {
        parts.push(who.clone());
    }
    if let Some(chat) = finding.chat.as_ref().and_then(|id| app.chat(id)) {
        parts.push(chat.name.clone());
    }
    if let Some(since) = finding.since {
        parts.push(ago(locale, since));
    }
    parts.join(" · ")
}

/// One finding: a tick box where it can be done, its text, who and where,
/// and on hover a way to dismiss it. Clicking it opens its message's chat.
fn finding_row(
    app: &mut App,
    ui: &mut egui::Ui,
    finding: &Finding,
    tickable: bool,
    tick: &mut Option<(String, bool)>,
) {
    let palette = app.palette;
    let locale = app.locale;
    let meta = finding_meta(app, finding);
    let row = Frame::new()
        .inner_margin(Margin::symmetric(4, 7))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                if tickable {
                    let (rect, response) =
                        ui.allocate_exact_size(Vec2::splat(18.0), Sense::click());
                    ui.painter().rect_stroke(
                        rect,
                        5.0,
                        egui::Stroke::new(1.5, palette.dim),
                        egui::StrokeKind::Inside,
                    );
                    if response.hovered() {
                        theme::icon(
                            &mut ui.new_child(egui::UiBuilder::new().max_rect(rect)),
                            Icon::Check,
                            14.0,
                            palette.accent,
                        );
                    }
                    if response
                        .on_hover_text(gettext(locale, "Mark as done"))
                        .clicked()
                    {
                        *tick = Some((finding.id.clone(), true));
                    }
                }
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    let width = ui.available_width() - 30.0;
                    ui.set_max_width(width.max(80.0));
                    wrapped(ui, &finding.text, theme::regular(14.0), palette.text);
                    let mut line = meta.clone();
                    if let Some(due) = &finding.due {
                        line = if line.is_empty() {
                            due.clone()
                        } else {
                            format!("{line} · {due}")
                        };
                    }
                    if !line.is_empty() {
                        theme::text(ui, line, theme::regular(12.0), palette.dim);
                    }
                });
            });
        });
    let rect = row.response.rect;
    let response = ui.interact(rect, ui.id().with(("finding", &finding.id)), Sense::click());
    if response.hovered() {
        let close = Rect::from_center_size(
            pos2(rect.right() - 14.0, rect.top() + 18.0),
            Vec2::splat(22.0),
        );
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(close));
        if theme::icon_button(
            &mut child,
            Icon::X,
            14.0,
            palette.dim,
            palette.text,
            &gettext(locale, "Dismiss"),
        )
        .clicked()
        {
            *tick = Some((finding.id.clone(), false));
        }
    }
    if response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
        && let Some(chat) = &finding.chat
    {
        app.actions.push(Action::Open(Page::Chats));
        app.actions.push(Action::OpenChat(chat.clone()));
    }
}

#[allow(clippy::too_many_arguments)]
fn findings_card(
    app: &mut App,
    ui: &mut egui::Ui,
    icon: Icon,
    title: &str,
    color: Color32,
    findings: &[Finding],
    ticks: &Ticks,
    tickable: bool,
    empty: &str,
    tick: &mut Option<(String, bool)>,
) {
    let palette = app.palette;
    let open: Vec<&Finding> = findings.iter().filter(|f| ticks.open(&f.id)).collect();
    let closed = findings.len() - open.len();
    card_frame(&palette).show(ui, |ui| {
        ui.set_width(ui.available_width());
        card_title(ui, &palette, icon, title, color, |ui| {
            if closed > 0 {
                theme::text(
                    ui,
                    gettext(app.locale, "{n} closed").replace("{n}", &closed.to_string()),
                    theme::regular(12.0),
                    palette.dim,
                );
            }
        });
        if open.is_empty() {
            theme::text(ui, empty, theme::regular(13.0), palette.dim);
        }
        for (index, finding) in open.iter().enumerate() {
            if index > 0 {
                let x = ui.max_rect().x_range();
                let y = ui.cursor().top();
                ui.painter().hline(x, y, palette.glass_edge());
            }
            finding_row(app, ui, finding, tickable, tick);
        }
    });
    ui.add_space(12.0);
}

fn metric(ui: &mut egui::Ui, palette: &theme::Palette, label: &str, value: usize, alert: bool) {
    Frame::new()
        .fill(card_fill(palette))
        .stroke(palette.glass_edge())
        .corner_radius(CornerRadius::same(theme::RADIUS + 4))
        .inner_margin(Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 2.0;
            theme::text(ui, label, theme::regular(12.5), palette.secondary);
            theme::text(
                ui,
                value.to_string(),
                theme::semibold(22.0),
                if alert && value > 0 {
                    palette.danger
                } else {
                    palette.text
                },
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
        no_analysis(app, ui, state, project);
        return;
    };
    let open = |list: &[Finding]| list.iter().filter(|f| ticks.open(&f.id)).count();
    let mut tick = None;
    let mut period = state.period;
    egui::ScrollArea::vertical()
        .id_salt(("project-panel", &project.id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            Frame::new()
                .inner_margin(Margin::symmetric(20, 16))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    // The numbers at a glance.
                    ui.columns(4, |columns| {
                        metric(
                            &mut columns[0],
                            &palette,
                            gettext(locale, "To do").as_ref(),
                            open(&analysis.todo),
                            false,
                        );
                        metric(
                            &mut columns[1],
                            &palette,
                            gettext(locale, "To follow up").as_ref(),
                            open(&analysis.follow_up),
                            false,
                        );
                        metric(
                            &mut columns[2],
                            &palette,
                            gettext(locale, "Slipping").as_ref(),
                            open(&analysis.leaks),
                            true,
                        );
                        metric(
                            &mut columns[3],
                            &palette,
                            gettext(locale, "New needs").as_ref(),
                            open(&analysis.needs),
                            false,
                        );
                    });
                    ui.add_space(14.0);
                    let left =
                        |app: &mut App, ui: &mut egui::Ui, tick: &mut Option<(String, bool)>| {
                            findings_card(
                                app,
                                ui,
                                Icon::ListChecks,
                                gettext(locale, "To do · what we committed to").as_ref(),
                                palette.text,
                                &analysis.todo,
                                ticks,
                                true,
                                gettext(locale, "Nothing pending.").as_ref(),
                                tick,
                            );
                            findings_card(
                                app,
                                ui,
                                Icon::Clock,
                                gettext(locale, "To follow up · waiting on others").as_ref(),
                                palette.text,
                                &analysis.follow_up,
                                ticks,
                                true,
                                gettext(locale, "Nobody owes us an answer.").as_ref(),
                                tick,
                            );
                            findings_card(
                                app,
                                ui,
                                Icon::CircleAlert,
                                gettext(locale, "Slipping").as_ref(),
                                palette.danger,
                                &analysis.leaks,
                                ticks,
                                true,
                                gettext(locale, "Nothing is slipping.").as_ref(),
                                tick,
                            );
                        };
                    let mut right =
                        |app: &mut App, ui: &mut egui::Ui, tick: &mut Option<(String, bool)>| {
                            card_frame(&palette).show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                card_title(
                                    ui,
                                    &palette,
                                    Icon::Calendar,
                                    gettext(locale, "Summary").as_ref(),
                                    palette.text,
                                    |ui| {
                                        let options = [
                                            ("week", gettext(locale, "Week").into_owned()),
                                            (
                                                "yesterday",
                                                gettext(locale, "Yesterday").into_owned(),
                                            ),
                                            ("today", gettext(locale, "Today").into_owned()),
                                        ];
                                        ui.spacing_mut().item_spacing.x = 4.0;
                                        for (value, label) in options {
                                            if widgets::filter_chip(
                                                ui,
                                                &palette,
                                                &label,
                                                0,
                                                period == value,
                                            )
                                            .clicked()
                                            {
                                                period = value;
                                            }
                                        }
                                    },
                                );
                                match analysis.summaries.get(period) {
                                    Some(text) => {
                                        wrapped(ui, text, theme::regular(14.0), palette.text)
                                    }
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
                            findings_card(
                                app,
                                ui,
                                Icon::Bell,
                                gettext(locale, "Needs that came up").as_ref(),
                                palette.text,
                                &analysis.needs,
                                ticks,
                                true,
                                gettext(locale, "No new needs.").as_ref(),
                                tick,
                            );
                            findings_card(
                                app,
                                ui,
                                Icon::Star,
                                gettext(locale, "Messages worth reading").as_ref(),
                                palette.text,
                                &analysis.highlights,
                                ticks,
                                false,
                                gettext(locale, "Nothing stands out.").as_ref(),
                                tick,
                            );
                        };
                    if ui.available_width() >= TWO_COLUMNS {
                        let width = ui.available_width();
                        let gap = 14.0;
                        let left_width = (width - gap) * 0.56;
                        ui.horizontal_top(|ui| {
                            ui.allocate_ui_with_layout(
                                vec2(left_width, 0.0),
                                Layout::top_down(Align::Min),
                                |ui| left(app, ui, &mut tick),
                            );
                            ui.add_space(gap);
                            ui.allocate_ui_with_layout(
                                vec2(width - left_width - gap, 0.0),
                                Layout::top_down(Align::Min),
                                |ui| right(app, ui, &mut tick),
                            );
                        });
                    } else {
                        left(app, ui, &mut tick);
                        right(app, ui, &mut tick);
                    }
                });
        });
    state.period = period;
    if let Some((id, done)) = tick {
        let mut ticks = ticks.clone();
        if done {
            ticks.done.insert(id);
        } else {
            ticks.dismissed.insert(id);
        }
        if let Err(error) = ticks.save(&app.dirs.config, &project.id) {
            log::warn!("projects: ticks could not be saved: {error}");
        }
        state.analysis = None;
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
            .stick_to_bottom(true)
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
                    match feed_row(app, ui, message, shade(&message.chat), *inclusion, selected) {
                        RowClick::Toggle => toggle = Some(key),
                        RowClick::Open => open = Some(message.chat.clone()),
                        RowClick::None => {}
                    }
                }
                ui.add_space(16.0);
            });
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
    }
}

fn day_chip(ui: &mut egui::Ui, palette: &theme::Palette, label: &str) {
    ui.add_space(10.0);
    ui.vertical_centered(|ui| {
        Frame::new()
            .fill(if palette.vibrant() {
                palette.glass()
            } else {
                palette.surface
            })
            .stroke(palette.glass_edge())
            .corner_radius(CornerRadius::same(theme::RADIUS))
            .inner_margin(Margin::symmetric(10, 4))
            .show(ui, |ui| {
                theme::text(ui, label, theme::medium(12.0), palette.secondary);
            });
    });
    ui.add_space(6.0);
}

enum RowClick {
    None,
    Toggle,
    Open,
}

/// One message: a box to select it, its chat and sender, when, what it
/// says, and whether it belongs. Messages outside the project are dimmed.
fn feed_row(
    app: &App,
    ui: &mut egui::Ui,
    message: &FeedMessage,
    color: Color32,
    inclusion: Inclusion,
    selected: bool,
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
        }
        Some(SourceChange::Mode(chat, mode)) => {
            state.map.add_source(&project.id, &chat, mode);
            save_map(app, state);
        }
        Some(SourceChange::Remove(chat)) => {
            state.map.remove_source(&project.id, &chat);
            save_map(app, state);
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
                                ui.set_max_width(ui.available_width() - 150.0);
                                wrapped(ui, &line, theme::medium(14.0), palette.text);
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
