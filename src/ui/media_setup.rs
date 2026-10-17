//! The first-run choice of who transcribes voice messages and videos and
//! reads pictures, with a suggestion made from this computer's chip, memory
//! and free disk (src/capabilities.rs). Shown once, after linking.

use egui::{Align, Layout, RichText};

use crate::app::App;
use crate::capabilities::{Advice, Machine};
use crate::media_ai::{ImageEngine, MediaEngines, SpeechEngine};
use crate::model::Action;
use crate::theme;

/// The machine read when the dialog opened, the advice, and the choices.
#[derive(Clone, Debug)]
pub struct State {
    pub machine: Machine,
    pub advice: Advice,
    pub engines: MediaEngines,
    pub all_chats: bool,
    pub key: String,
}

impl State {
    pub fn new(state_dir: &std::path::Path) -> Self {
        let machine = Machine::detect(state_dir);
        let advice = crate::capabilities::advise(&machine);
        Self {
            engines: MediaEngines {
                speech: advice.speech,
                image: advice.image,
                ..MediaEngines::default()
            },
            machine,
            advice,
            all_chats: false,
            key: String::new(),
        }
    }
}

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let Some(mut state) = app.media_setup.clone() else {
        app.actions.push(Action::CloseDialog);
        return;
    };
    let palette = app.palette;
    let locale = app.locale;
    let t = |source: &'static str| crate::i18n::gettext(locale, source).into_owned();
    let small = |text: &str, color| RichText::new(text).font(theme::regular(12.5)).color(color);

    theme::text(
        ui,
        t("Voice messages and pictures"),
        theme::bold(18.0),
        palette.text,
    );
    theme::paragraph(
        ui,
        t(
            "Whats Pro can transcribe voice messages and videos, and read pictures, in the chats you choose. This is what we suggest for this computer:",
        ),
        theme::regular(13.5),
        palette.text,
    );
    ui.label(small(&state.machine.summary(&t("free")), palette.dim));
    ui.add_space(6.0);

    // Speech.
    theme::text(
        ui,
        t("Voice messages and videos"),
        theme::semibold(14.0),
        palette.text,
    );
    for engine in [
        SpeechEngine::Local,
        SpeechEngine::Groq,
        SpeechEngine::OpenAi,
        SpeechEngine::Gemini,
    ] {
        let mut label = t(engine.label());
        if engine == state.advice.speech {
            label = format!("{label} · {}", t("suggested"));
        }
        ui.radio_value(
            &mut state.engines.speech,
            engine,
            RichText::new(label)
                .font(theme::regular(13.5))
                .color(palette.text),
        );
    }
    ui.label(small(&t(state.advice.speech_reason), palette.secondary));
    if let Some(account) = state.engines.speech.key_account() {
        let saved = crate::keys::get(account).is_some();
        let hint = if saved {
            t("Key saved. Type to replace it")
        } else {
            t("API key")
        };
        ui.add(
            egui::TextEdit::singleline(&mut state.key)
                .password(true)
                .hint_text(format!("{account} · {hint}"))
                .font(theme::regular(13.0))
                .desired_width(f32::INFINITY),
        );
    }
    ui.add_space(6.0);

    // Pictures.
    theme::text(ui, t("Pictures"), theme::semibold(14.0), palette.text);
    let mut images = vec![ImageEngine::Local];
    for cli in &state.machine.clis {
        images.push(match *cli {
            "claude" => ImageEngine::ClaudeCode,
            "codex" => ImageEngine::CodexCli,
            _ => ImageEngine::GeminiCli,
        });
    }
    for engine in images {
        let mut label = t(engine.label());
        if engine == state.advice.image {
            label = format!("{label} · {}", t("suggested"));
        }
        ui.radio_value(
            &mut state.engines.image,
            engine,
            RichText::new(label)
                .font(theme::regular(13.5))
                .color(palette.text),
        );
    }
    ui.label(small(&t(state.advice.image_reason), palette.secondary));
    ui.add_space(6.0);

    ui.checkbox(
        &mut state.all_chats,
        RichText::new(t("Do it in every chat"))
            .font(theme::regular(13.5))
            .color(palette.text),
    );
    ui.label(small(
        &t("Otherwise, choose chats from their right-click menu. All of this can be changed in Settings > Transcription."),
        palette.dim,
    ));
    ui.add_space(10.0);

    let mut done = false;
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if theme::pill_button(ui, &palette, &t("Save"), true).clicked() {
                if let Some(account) = state.engines.speech.key_account()
                    && !state.key.trim().is_empty()
                    && let Err(error) = crate::keys::set(account, &state.key)
                {
                    app.toast_error(error.to_string());
                }
                app.settings.media_engines = state.engines.clone();
                if state.all_chats {
                    let settings = &mut app.account_mut().settings;
                    settings.speech = crate::settings::ReadScope::All;
                    settings.images = crate::settings::ReadScope::All;
                    app.account_mut().mark_settings_dirty();
                }
                done = true;
            }
            if theme::pill_button(ui, &palette, &t("Later"), false).clicked() {
                done = true;
            }
        });
    });
    if done {
        app.settings.media_setup_done = true;
        app.media_setup = None;
        app.actions.push(Action::SettingsChanged);
        app.actions.push(Action::CloseDialog);
    } else {
        app.media_setup = Some(state);
    }
}
