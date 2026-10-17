//! A recording just made, before anything is done with it: how long it is,
//! listening to it, and keeping it (to be transcribed) or throwing it away,
//! which asks once more. Leads and Notes show the same card, so a voice
//! note or a call is never transcribed without the user saying so.

use std::path::{Path, PathBuf};

use egui::{Sense, vec2};

use crate::app::App;
use crate::audio::State as Playback;
use crate::i18n::gettext;
use crate::model::Action;
use crate::theme::{self, Icon};

/// A recording waiting for the user's word.
#[derive(Clone, Debug, PartialEq)]
pub struct Take {
    pub path: PathBuf,
    pub seconds: u64,
    /// Discard was clicked once: it now asks to be sure.
    pub confirming: bool,
}

impl Take {
    pub fn new(path: PathBuf, seconds: u64) -> Self {
        Self {
            path,
            seconds,
            confirming: false,
        }
    }

    /// Throws the recording away, file and all.
    pub fn discard(&self) {
        remove(&self.path);
    }
}

/// Removes a recording's file; one already gone is fine.
pub fn remove(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("take: a discarded recording could not be removed: {error}");
    }
}

/// What the user decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Keep,
    Discard,
}

/// The player's name for a take, so its playback is told apart from a
/// message's.
fn clip(take: &Take) -> String {
    format!("take:{}", take.path.display())
}

fn clock(seconds: u64) -> String {
    crate::util::call_duration(seconds)
}

/// The card: a play button with the time, the save action (`keep`) and
/// Discard, which turns into a confirmation. Returns the choice once made;
/// `take.confirming` is the card's own state, kept by the caller.
pub fn card(app: &mut App, ui: &mut egui::Ui, take: &mut Take, keep: &str) -> Option<Choice> {
    let palette = app.palette;
    let locale = app.locale;
    let keep = if keep == gettext(locale, "Transcribe") {
        gettext(locale, "Save and transcribe")
    } else {
        std::borrow::Cow::Borrowed(keep)
    };
    let id = clip(take);
    let status = app.player.status(&id);
    let mut choice = None;
    egui::Frame::new()
        .fill(palette.surface)
        .stroke(palette.glass_edge())
        .corner_radius(egui::CornerRadius::same(theme::RADIUS))
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let playing = matches!(status.state, Playback::Playing | Playback::Loading);
                let (icon, tip) = if playing {
                    (Icon::Pause, gettext(locale, "Pause"))
                } else {
                    (Icon::Play, gettext(locale, "Listen"))
                };
                if theme::icon_button(ui, icon, 17.0, palette.accent, palette.text, &tip).clicked()
                {
                    app.actions.push(Action::PlayVoice {
                        message: id.clone(),
                        path: take.path.clone(),
                    });
                }
                let shown = if matches!(status.state, Playback::Idle) {
                    clock(take.seconds)
                } else {
                    format!(
                        "{} / {}",
                        clock(status.position.as_secs()),
                        clock(take.seconds.max(status.total.as_secs()))
                    )
                };
                if playing {
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(250));
                }
                theme::text(
                    ui,
                    gettext(locale, "Recording · {time}").replace("{time}", &shown),
                    theme::medium(13.0),
                    palette.text,
                );
            });
            ui.add_space(8.0);
            if take.confirming {
                theme::paragraph(
                    ui,
                    gettext(
                        locale,
                        "Discard the recording? The audio is deleted and nothing is kept.",
                    ),
                    theme::medium(13.0),
                    palette.danger,
                );
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if pill(ui, &palette, &gettext(locale, "Discard"), Pill::Danger).clicked() {
                        choice = Some(Choice::Discard);
                    }
                    if pill(ui, &palette, &gettext(locale, "Back"), Pill::Plain).clicked() {
                        take.confirming = false;
                    }
                });
            } else {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if pill(ui, &palette, &keep, Pill::Primary).clicked() {
                        choice = Some(Choice::Keep);
                    }
                    if pill(ui, &palette, &gettext(locale, "Discard"), Pill::Plain).clicked() {
                        take.confirming = true;
                    }
                });
            }
        });
    if choice.is_some() && !matches!(status.state, Playback::Idle) {
        // Its playback ends with it.
        app.actions.push(Action::PlayVoice {
            message: id,
            path: take.path.clone(),
        });
    }
    choice
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pill {
    Primary,
    Plain,
    Danger,
}

fn pill(ui: &mut egui::Ui, palette: &theme::Palette, label: &str, kind: Pill) -> egui::Response {
    let font = theme::semibold(13.0);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font.clone(), palette.text);
    let width = (galley.size().x + 24.0).max(64.0);
    let (rect, response) = ui.allocate_exact_size(vec2(width, 30.0), Sense::click());
    theme::reveal_focus(&response);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    let hovered = response.hovered();
    let (fill, text) = match kind {
        Pill::Primary if hovered => (palette.accent_hover, palette.on_accent),
        Pill::Primary => (palette.accent, palette.on_accent),
        Pill::Danger => (palette.danger, palette.on_accent),
        Pill::Plain if hovered => (palette.surface_hover, palette.text),
        Pill::Plain => (palette.surface, palette.text),
    };
    ui.painter().rect_filled(rect, 15.0, fill);
    if kind == Pill::Plain {
        ui.painter()
            .rect_stroke(rect, 15.0, palette.glass_edge(), egui::StrokeKind::Inside);
    }
    let galley = ui.painter().layout_no_wrap(label.to_owned(), font, text);
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, text);
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_discarded_take_leaves_no_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("Llamada.wav");
        std::fs::write(&path, b"RIFF").unwrap();
        let take = Take::new(path.clone(), 75);
        assert_eq!(clock(take.seconds), "1:15");
        assert!(clip(&take).starts_with("take:"));
        take.discard();
        assert!(!path.exists());
        // Gone already: nothing to do.
        take.discard();
    }
}
