//! The Projects page: chats grouped by what they are about.
//!
//! The list pane lists the projects as the chat list lists chats (a coloured
//! mark, the latest message of any of its chats, when, and the unread count),
//! then the chats still to file. Beside it, a project's feed: every message
//! of its chats in one timeline, with its summary, pending items and chats in
//! a column of cards. Clicking a message opens its chat.
//!
//! The page keeps its own state in egui's memory (`State`), so the map
//! written by the dashboard shows here and the map changed here shows there.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use egui::{Align, Color32, CornerRadius, Frame, Layout, Margin, Rect, Sense, Vec2, pos2, vec2};

use crate::app::App;
use crate::i18n::gettext;
use crate::model::{Action, Page};
use crate::projects::{Bucket, Digest, Feed, FeedKey, FeedMessage, Item, ProjectMap};
use crate::theme::{self, Icon};
use crate::ui::widgets;

/// What the page shows beside the list.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Selection {
    Project(String),
    Unassigned,
    Personal,
    Ignored,
}

/// The ranges a feed can cover, in days.
const RANGES: [(i64, &str); 4] = [(1, "Today"), (3, "3 days"), (7, "7 days"), (30, "30 days")];

/// The side column beside a feed, when the pane is wide enough for both.
const SIDE_WIDTH: f32 = 320.0;
const SIDE_MIN_FEED: f32 = 420.0;

struct State {
    map: ProjectMap,
    /// When the map file last changed, to pick up the dashboard's edits.
    map_modified: Option<SystemTime>,
    checked: Option<Instant>,
    selection: Option<Selection>,
    days: i64,
    feed: Feed,
    digest: Option<(String, Instant, Option<Digest>)>,
    adding: bool,
    draft: String,
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
            days: 7,
            feed: Feed::default(),
            digest: None,
            adding: false,
            draft: String::new(),
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

fn card_frame(palette: &theme::Palette) -> Frame {
    let fill = if palette.vibrant() {
        palette.tint(theme::TINT_FIELD)
    } else {
        palette.surface
    };
    Frame::new()
        .fill(fill)
        .stroke(palette.glass_edge())
        .corner_radius(CornerRadius::same(theme::RADIUS + 6))
        .inner_margin(Margin::same(14))
}

fn card_title(ui: &mut egui::Ui, palette: &theme::Palette, title: &str) {
    theme::text(ui, title, theme::medium(13.0), palette.accent);
    ui.add_space(6.0);
}

fn wrapped(ui: &mut egui::Ui, text: &str, font: egui::FontId, color: Color32) {
    ui.add(
        egui::Label::new(egui::RichText::new(text).font(font).color(color))
            .wrap()
            .selectable(true),
    );
}

// ---------------------------------------------------------------- list pane

/// The list pane on the Projects page.
pub fn list(app: &mut App, ui: &mut egui::Ui) {
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

    let mut rows: Vec<(String, String, Color32, i64, u32, Option<String>)> = state
        .map
        .projects
        .iter()
        .map(|project| {
            let (last, unread, preview) = activity(app, &project.chats);
            (
                project.id.clone(),
                project.name.clone(),
                project.color(),
                last,
                unread,
                preview,
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
            for (id, name, color, last, unread, preview) in &rows {
                let selected = state.selection == Some(Selection::Project(id.clone()));
                if project_row(
                    ui, &palette, locale, name, *color, *last, *unread, preview, selected,
                ) {
                    picked = Some(Selection::Project(id.clone()));
                }
            }
            ui.add_space(10.0);
            Frame::new()
                .inner_margin(Margin {
                    left: 16,
                    right: 14,
                    top: 4,
                    bottom: 6,
                })
                .show(ui, |ui| {
                    theme::text(
                        ui,
                        gettext(locale, "Organize"),
                        theme::medium(13.0),
                        palette.accent,
                    );
                });
            for (selection, icon, label, count) in [
                (
                    Selection::Unassigned,
                    Icon::ListChecks,
                    gettext(locale, "To file"),
                    unassigned,
                ),
                (
                    Selection::Personal,
                    Icon::User,
                    gettext(locale, "Personal"),
                    personal,
                ),
                (
                    Selection::Ignored,
                    Icon::EyeOff,
                    gettext(locale, "Ignored"),
                    ignored,
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
        state.selection = Some(selection);
        state.picked = true;
    }
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
        let badge = if unread > 0 {
            widgets::badge(
                ui,
                palette,
                pos2(right - 12.0, rect.top() + 46.0),
                unread,
                false,
            )
        } else {
            0.0
        };
        if let Some(preview) = preview {
            let width = (right - badge - 8.0 - left).max(0.0);
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
    count: usize,
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
        child.with_layout(Layout::right_to_left(Align::Center), |ui| {
            theme::text(ui, count.to_string(), theme::regular(12.5), palette.dim);
        });
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
        Some(Selection::Personal) => bucket(app, ui, &mut state, Bucket::Personal),
        Some(Selection::Ignored) => bucket(app, ui, &mut state, Bucket::Ignored),
        _ => bucket(app, ui, &mut state, Bucket::Unassigned),
    }
}

/// The header across the top of the pane: a mark or icon, a title and a
/// line under it, and whatever `right` draws at its end.
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
    let color = project.color();
    let (last, _, _) = activity(app, &project.chats);
    let since = jiff::Timestamp::now().as_second() - state.days * 86_400;
    let key = FeedKey {
        chats: project.chats.clone(),
        since,
        // A new message in any of its chats refreshes the feed; the range's
        // start moves only once a minute.
        activity: last,
    };
    let key = FeedKey {
        since: key.since - key.since % 60,
        ..key
    };
    let archive: PathBuf = app.account().dirs.archive_db();
    let (messages, loading, error) = if project.chats.is_empty() {
        (Arc::new(Vec::new()), false, None)
    } else {
        state.feed.get(ui.ctx(), &archive, key)
    };

    let subtitle = {
        let chats =
            gettext(locale, "{count} chats").replace("{count}", &project.chats.len().to_string());
        let shown =
            gettext(locale, "{count} messages").replace("{count}", &messages.len().to_string());
        format!("{chats} · {shown}")
    };
    let mut days = state.days;
    let mut remove = false;
    header(
        app,
        ui,
        Some((color, &project.name)),
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
        },
    );
    state.days = days;
    if remove {
        state.map.remove(id);
        save_map(app, state);
        state.selection = None;
        return;
    }

    let body = ui.available_rect_before_wrap();
    let side = body.width() >= SIDE_WIDTH + SIDE_MIN_FEED;
    let feed_rect = if side {
        Rect::from_min_max(body.min, pos2(body.right() - SIDE_WIDTH, body.bottom()))
    } else {
        body
    };
    let mut feed_ui = ui.new_child(egui::UiBuilder::new().max_rect(feed_rect));
    feed(
        app,
        &mut feed_ui,
        &project,
        &messages,
        loading,
        error.as_deref(),
    );
    if side {
        let side_rect = Rect::from_min_max(pos2(feed_rect.right(), body.top()), body.max);
        let mut side_ui = ui.new_child(egui::UiBuilder::new().max_rect(side_rect));
        side_column(app, &mut side_ui, state, &project);
    }
    ui.allocate_rect(body, Sense::hover());
}

fn feed(
    app: &mut App,
    ui: &mut egui::Ui,
    project: &crate::projects::Project,
    messages: &[FeedMessage],
    loading: bool,
    error: Option<&str>,
) {
    let palette = app.palette;
    let locale = app.locale;
    if project.chats.is_empty() {
        widgets::empty_state(
            ui,
            &palette,
            Icon::ListChecks,
            gettext(locale, "This project has no chats").as_ref(),
            gettext(locale, "File chats in it from To file.").as_ref(),
        );
        return;
    }
    if let Some(error) = error {
        widgets::empty_state(
            ui,
            &palette,
            Icon::CircleAlert,
            gettext(locale, "Could not load the messages").as_ref(),
            error,
        );
        return;
    }
    if messages.is_empty() {
        let (title, body) = if loading {
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
            ui,
            &palette,
            Icon::MessageCircle,
            title.as_ref(),
            body.as_ref(),
        );
        return;
    }
    // Each chat keeps a shade of the project's colour, so a run of messages
    // reads as one conversation inside the mix.
    let base = project.color();
    let shade = |chat: &str| {
        let index = project.chats.iter().position(|id| id == chat).unwrap_or(0);
        let t = (index as f32 * 0.17) % 0.6;
        base.lerp_to_gamma(palette.text, t)
    };
    let mut open = None;
    egui::ScrollArea::vertical()
        .id_salt(("project-feed", &project.id))
        .auto_shrink([false, false])
        .stick_to_bottom(true)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.add_space(8.0);
            let mut day = String::new();
            for message in messages {
                let label = crate::util::day_label(locale, message.timestamp);
                if label != day {
                    day_chip(ui, &palette, &label);
                    day = label;
                }
                if feed_row(app, ui, message, shade(&message.chat)) {
                    open = Some(message.chat.clone());
                }
            }
            ui.add_space(16.0);
        });
    if let Some(chat) = open {
        app.actions.push(Action::Open(Page::Chats));
        app.actions.push(Action::OpenChat(chat));
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

/// One message in the feed: its chat and sender, when, and what it says.
fn feed_row(app: &App, ui: &mut egui::Ui, message: &FeedMessage, color: Color32) -> bool {
    let palette = app.palette;
    let locale = app.locale;
    let sender = if message.from_me {
        gettext(locale, "You").into_owned()
    } else {
        message
            .sender_name
            .clone()
            .unwrap_or_else(|| app.display_name(&message.sender))
    };
    let text = app.resolve_mention_tokens(&message.text);
    let media = !matches!(message.kind.as_str(), "text" | "interactive");
    let inner = Frame::new()
        .inner_margin(Margin {
            left: 24,
            right: 20,
            top: 6,
            bottom: 6,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let (bar, _) = ui.allocate_exact_size(vec2(3.0, 16.0), Sense::hover());
                ui.painter().rect_filled(bar, 2.0, color);
                theme::text(ui, &message.chat_name, theme::semibold(13.0), color);
                theme::text(
                    ui,
                    &sender,
                    theme::regular(13.0),
                    if message.from_me {
                        palette.accent
                    } else {
                        palette.secondary
                    },
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    theme::text(
                        ui,
                        crate::util::clock(message.timestamp),
                        theme::regular(11.5),
                        palette.dim,
                    );
                });
            });
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.add_space(11.0);
                ui.vertical(|ui| {
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
                        if media {
                            palette.secondary
                        } else {
                            palette.text
                        },
                    );
                });
            });
        });
    let response = inner.response.interact(Sense::click());
    if response.hovered() {
        ui.painter().rect_filled(
            response.rect.shrink2(vec2(10.0, 0.0)),
            CornerRadius::same(theme::RADIUS + 2),
            palette.tint(0.035),
        );
    }
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(gettext(locale, "Open the chat"))
        .clicked()
}

fn side_column(
    app: &mut App,
    ui: &mut egui::Ui,
    state: &mut State,
    project: &crate::projects::Project,
) {
    let palette = app.palette;
    let locale = app.locale;
    // The summary is read again every ten seconds, so a new one shows soon.
    let stale = state
        .digest
        .as_ref()
        .is_none_or(|(id, at, _)| id != &project.id || at.elapsed() > Duration::from_secs(10));
    if stale {
        state.digest = Some((
            project.id.clone(),
            Instant::now(),
            Digest::load(&app.dirs.config, &project.id),
        ));
    }
    let digest = state
        .digest
        .as_ref()
        .and_then(|(_, _, digest)| digest.clone());
    let mut moves = Vec::new();
    egui::ScrollArea::vertical()
        .id_salt(("project-side", &project.id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            Frame::new()
                .inner_margin(Margin {
                    left: 4,
                    right: 16,
                    top: 14,
                    bottom: 16,
                })
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 10.0;
                    digest_cards(ui, &palette, locale, digest.as_ref());
                    card_frame(&palette).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        card_title(ui, &palette, gettext(locale, "Chats").as_ref());
                        let mut chats: Vec<String> = project.chats.clone();
                        chats.sort_by_key(|id| {
                            std::cmp::Reverse(app.chat(id).map_or(0, |c| c.last_activity))
                        });
                        for chat in chats {
                            if let Some(bucket) = chat_row(app, ui, &state.map, &chat, true) {
                                moves.push((chat, bucket));
                            }
                        }
                    });
                });
        });
    for (chat, bucket) in moves {
        state.map.assign(&chat, &bucket);
        save_map(app, state);
    }
}

fn digest_cards(
    ui: &mut egui::Ui,
    palette: &theme::Palette,
    locale: crate::i18n::Locale,
    digest: Option<&Digest>,
) {
    let Some(digest) = digest else {
        card_frame(palette).show(ui, |ui| {
            ui.set_width(ui.available_width());
            card_title(ui, palette, gettext(locale, "Summary").as_ref());
            wrapped(
                ui,
                gettext(locale, "No summary yet. When one is made, the summary, decisions and who owes what show here.").as_ref(),
                theme::regular(13.5),
                palette.secondary,
            );
        });
        return;
    };
    if let Some(summary) = &digest.summary {
        card_frame(palette).show(ui, |ui| {
            ui.set_width(ui.available_width());
            card_title(ui, palette, gettext(locale, "Summary").as_ref());
            wrapped(ui, summary, theme::regular(14.0), palette.text);
            if let Some(updated) = digest.updated {
                ui.add_space(6.0);
                theme::text(
                    ui,
                    gettext(locale, "Updated {when}")
                        .replace("{when}", &crate::util::moment_stamp(locale, updated)),
                    theme::regular(12.0),
                    palette.dim,
                );
            }
        });
    }
    let section =
        |ui: &mut egui::Ui, title: &str, lines: Vec<(Option<String>, String, Option<String>)>| {
            if lines.is_empty() {
                return;
            }
            card_frame(palette).show(ui, |ui| {
                ui.set_width(ui.available_width());
                card_title(ui, palette, title);
                for (who, text, due) in lines {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        theme::text(ui, "•  ", theme::regular(14.0), palette.dim);
                        if let Some(who) = who {
                            theme::text(
                                ui,
                                format!("{who} · "),
                                theme::semibold(14.0),
                                palette.accent,
                            );
                        }
                        wrapped(ui, &text, theme::regular(14.0), palette.text);
                        if let Some(due) = due {
                            theme::text(ui, format!(" · {due}"), theme::regular(12.5), palette.dim);
                        }
                    });
                }
            });
        };
    section(
        ui,
        gettext(locale, "Pending").as_ref(),
        digest
            .pending
            .iter()
            .map(|item| match item {
                Item::Text(text) => (None, text.clone(), None),
                Item::Full(p) => (p.owner.clone(), p.text.clone(), p.due.clone()),
            })
            .collect(),
    );
    section(
        ui,
        gettext(locale, "Waiting on others").as_ref(),
        digest
            .waiting_on
            .iter()
            .map(|item| match item {
                Item::Text(text) => (None, text.clone(), None),
                Item::Full(w) => (w.who.clone(), w.text.clone(), None),
            })
            .collect(),
    );
    let notes = |items: &[Item<crate::projects::Note>]| {
        items
            .iter()
            .map(|item| match item {
                Item::Text(text) => (None, text.clone(), None),
                Item::Full(note) => (None, note.text.clone(), None),
            })
            .collect::<Vec<_>>()
    };
    section(
        ui,
        gettext(locale, "Decisions").as_ref(),
        notes(&digest.decisions),
    );
    section(
        ui,
        gettext(locale, "Links with other projects").as_ref(),
        notes(&digest.links),
    );
}

/// One chat with its picture, name, unread count and when it was last
/// active, and a menu to file it elsewhere. Returns where it was moved.
fn chat_row(
    app: &mut App,
    ui: &mut egui::Ui,
    map: &ProjectMap,
    id: &str,
    compact: bool,
) -> Option<Bucket> {
    let palette = app.palette;
    let locale = app.locale;
    let name = app
        .chat(id)
        .map(|c| c.name.clone())
        .unwrap_or_else(|| app.display_name(id));
    let (unread, last, group) = app.chat(id).map_or((0, 0, false), |c| {
        (
            c.unread,
            c.last_activity,
            c.kind == crate::model::ChatKind::Group,
        )
    });
    let picture = app.avatar(id);
    let height = if compact { 44.0 } else { theme::ROW_HEIGHT };
    let avatar = if compact { 32.0 } else { 48.0 };
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click());
    let menu_rect = Rect::from_min_max(pos2(rect.right() - 36.0, rect.top()), rect.max);
    if ui.is_rect_visible(rect) {
        if response.hovered() {
            widgets::row_highlight(
                ui,
                &palette,
                rect.expand2(vec2(if compact { 6.0 } else { 0.0 }, 0.0)),
                palette.surface_hover,
            );
        }
        let inset = if compact { 0.0 } else { 14.0 };
        let avatar_rect = Rect::from_min_size(
            pos2(rect.left() + inset, rect.center().y - avatar / 2.0),
            Vec2::splat(avatar),
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
        let font = if compact {
            theme::regular(14.0)
        } else {
            theme::medium(15.0)
        };
        let line = widgets::line(ui, &name, font, palette.text, width, 1);
        if compact {
            line.paint(
                ui,
                pos2(left, rect.center().y - line.size().y / 2.0),
                palette.text,
            );
        } else {
            line.paint(ui, pos2(left, rect.top() + 13.0), palette.text);
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
            gettext(locale, "Personal chats stay out of projects and summaries."),
        ),
        Bucket::Ignored => (
            gettext(locale, "Ignored"),
            Icon::EyeOff,
            gettext(locale, "Ignored chats stay out of projects and summaries."),
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
                    if let Some(target) = chat_row(app, ui, &state.map, chat, false) {
                        moves.push((chat.clone(), target));
                    }
                }
            });
            ui.add_space(16.0);
        });
    for (chat, target) in moves {
        state.map.assign(&chat, &target);
        save_map(app, state);
    }
}

fn new_project(app: &mut App, ui: &mut egui::Ui, state: &mut State) {
    let palette = app.palette;
    let locale = app.locale;
    header(
        app,
        ui,
        None,
        gettext(locale, "New project").as_ref(),
        gettext(locale, "Name it, then file chats in it.").as_ref(),
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
            let buttons = 2.0 * 120.0 + 12.0;
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
            state.adding = false;
        }
        Some(false) => state.adding = false,
        _ => {}
    }
}

fn pill_button(
    ui: &mut egui::Ui,
    palette: &theme::Palette,
    label: &str,
    primary: bool,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(120.0, 36.0), Sense::click());
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
                palette.surface
            },
            palette.text,
        )
    };
    ui.painter().rect_filled(rect, 18.0, fill);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), theme::semibold(14.0), text);
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, text);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}
