//! Recording a call made in Whats Pro: the microphone and the computer's
//! sound (which carries the other side), until the call ends. The file is
//! kept in the call library under the project its chat belongs to, then
//! read and summarized like any recorded call.

use std::{
    sync::{Mutex, mpsc},
    time::Instant,
};

use crate::app::App;

struct Active {
    generation: u64,
    started: Instant,
    stop: mpsc::Sender<()>,
}

static ACTIVE: Mutex<Option<Active>> = Mutex::new(None);

/// When the call `generation` started being recorded, if it is.
pub fn recording(generation: u64) -> Option<Instant> {
    ACTIVE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .filter(|active| active.generation == generation)
        .map(|active| active.started)
}

/// Starts recording the call on screen.
pub fn start(app: &App, ctx: &egui::Context) {
    let Some(call) = &app.call else {
        return;
    };
    let mut held = ACTIVE.lock().unwrap_or_else(|p| p.into_inner());
    if held.is_some() {
        return;
    }
    let (stop, stopped) = mpsc::channel::<()>();
    *held = Some(Active {
        generation: call.generation,
        started: Instant::now(),
        stop,
    });
    drop(held);
    let name = app.display_name(&call.chat);
    let title = if call.video {
        crate::i18n::gettext(app.locale, "Video call with {name}")
    } else {
        crate::i18n::gettext(app.locale, "Call with {name}")
    }
    .replace("{name}", &name);
    let project = crate::projects::ProjectMap::load(&app.dirs.config)
        .projects_of(&call.chat)
        .first()
        .map(|(project, _)| project.id.clone());
    let reader = crate::ui::projects::CallReader::of(app);
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let recording =
            match crate::recorder::Recording::start(&reader.state, crate::recorder::Source::Both) {
                Ok(recording) => recording,
                Err(error) => {
                    log::warn!("call capture: could not start: {error}");
                    *ACTIVE.lock().unwrap_or_else(|p| p.into_inner()) = None;
                    ctx.request_repaint();
                    return;
                }
            };
        let _ = stopped.recv();
        let seconds = recording.elapsed().as_secs();
        let path = match recording.stop() {
            Ok(path) => path,
            Err(error) => return log::warn!("call capture: could not stop: {error}"),
        };
        // The file first, kept even if nothing reads it.
        let call = match crate::call_library::Library::keep(
            &reader.state,
            &path,
            crate::call_library::Origin::Recorded,
            seconds,
            &title,
            project.as_deref(),
            None,
        ) {
            Ok(call) => call,
            Err(error) => return log::warn!("call capture: could not keep the call: {error}"),
        };
        ctx.request_repaint();
        if let Some(project) = project {
            crate::ui::projects::read_call(reader, project, call.id, Default::default(), ctx);
        } else if let Err(error) =
            crate::call_library::transcribe(&reader.engines, &reader.state, &call.id)
        {
            log::warn!("call capture: could not read the call: {error}");
        }
    });
}

/// Stops the recording; the call is kept and read.
pub fn stop() {
    if let Some(active) = ACTIVE.lock().unwrap_or_else(|p| p.into_inner()).take() {
        let _ = active.stop.send(());
    }
}

/// Ends a recording whose call has ended. Called every frame.
pub fn follow(app: &App) {
    let generation = ACTIVE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .map(|active| active.generation);
    let Some(generation) = generation else {
        return;
    };
    let ongoing = app
        .call
        .as_ref()
        .is_some_and(|call| call.generation == generation && call.outcome.is_none());
    if !ongoing {
        stop();
    }
}
