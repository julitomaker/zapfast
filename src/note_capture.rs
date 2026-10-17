//! Spoken Notes keep their audio before asking to transcribe. Jobs belong
//! to the process, and pending takes survive a restart on disk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Instant, SystemTime};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::app::App;
use crate::projects::{Journal, NoteKind};
use crate::ui::projects::CallReader;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Pending {
    pub title: String,
    pub path: PathBuf,
    pub seconds: u64,
    pub kind: NoteKind,
}

pub(crate) struct Job {
    pub recorder: Option<crate::audio::Recorder>,
    pub pending: Option<Pending>,
    pub confirming: bool,
    pub busy: bool,
    pub error: Option<String>,
    pub saved: Option<Instant>,
    reader: CallReader,
    project: String,
    kind: NoteKind,
    title: String,
    storing: Option<std::thread::JoinHandle<()>>,
}

type Shared = Arc<Mutex<Job>>;
type Key = (PathBuf, String);
static JOBS: LazyLock<Mutex<HashMap<Key, Shared>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn file(state: &Path, project: &str) -> PathBuf {
    let name = Sha256::digest(project.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    state.join("note-takes").join(format!("{name}.json"))
}

fn load(state: &Path, project: &str) -> Option<Pending> {
    let bytes = std::fs::read(file(state, project)).ok()?;
    serde_json::from_slice::<Pending>(&bytes)
        .ok()
        .filter(|take| take.path.is_file())
}

fn write(state: &Path, project: &str, pending: &Pending) -> Result<(), String> {
    let path = file(state, project);
    std::fs::create_dir_all(path.parent().expect("note-takes folder"))
        .map_err(|e| e.to_string())?;
    let bytes = serde_json::to_vec_pretty(pending).map_err(|e| e.to_string())?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(temporary, path).map_err(|e| e.to_string())
}

pub(crate) fn job(app: &App, project: &str) -> Shared {
    JOBS.lock()
        .unwrap_or_else(|p| p.into_inner())
        .entry((app.dirs.state.clone(), project.into()))
        .or_insert_with(|| {
            Arc::new(Mutex::new(Job {
                recorder: None,
                pending: load(&app.dirs.state, project),
                confirming: false,
                busy: false,
                error: None,
                saved: None,
                reader: CallReader::of(app),
                project: project.into(),
                kind: NoteKind::Log,
                title: String::new(),
                storing: None,
            }))
        })
        .clone()
}

pub(crate) fn start(held: &mut Job, app: &App, kind: NoteKind, title: String) {
    held.reader = CallReader::of(app);
    held.kind = kind;
    held.title = title;
    held.error = None;
    held.saved = None;
    held.recorder = Some(crate::audio::Recorder::start(app.waker()));
}

/// Finishing a take only stores it. No speech engine is called here.
pub(crate) fn stop(shared: &Shared, held: &mut Job, ctx: &egui::Context) {
    let Some(recorder) = held.recorder.take() else {
        return;
    };
    let seconds = recorder.elapsed().as_secs();
    let samples = match recorder.finish() {
        Ok(samples) => samples,
        Err(error) => {
            held.error = Some(error);
            return;
        }
    };
    held.busy = true;
    let (shared, ctx) = (shared.clone(), ctx.clone());
    let (reader, project, title, kind) = (
        held.reader.clone(),
        held.project.clone(),
        held.title.clone(),
        held.kind,
    );
    held.storing = Some(std::thread::spawn(move || {
        let result = keep(&reader.state, &project, title, kind, seconds, &samples);
        let mut job = shared.lock().unwrap_or_else(|p| p.into_inner());
        job.busy = false;
        match result {
            Ok(take) => job.pending = Some(take),
            Err(error) => job.error = Some(error),
        }
        ctx.request_repaint();
    }));
}

fn keep(
    state: &Path,
    project: &str,
    title: String,
    kind: NoteKind,
    seconds: u64,
    samples: &[f32],
) -> Result<Pending, String> {
    let folder = state.join("note-takes");
    std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = folder.join(format!("note-{stamp}.wav"));
    // The metadata precedes the audio: a crash cannot orphan a completed WAV.
    let take = Pending {
        title,
        path,
        seconds,
        kind,
    };
    write(state, project, &take)?;
    std::fs::write(&take.path, crate::projects::wav(samples, 48_000)).map_err(|e| e.to_string())?;
    Ok(take)
}

fn remove(state: &Path, project: &str, take: &Pending) -> Result<(), String> {
    for path in [take.path.clone(), file(state, project)] {
        if let Err(error) = std::fs::remove_file(path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(error.to_string());
        }
    }
    Ok(())
}

/// The save decision writes a spoken journal note after transcription. A
/// failure keeps the audio and the review card for another try.
pub(crate) fn decide(shared: &Shared, held: &mut Job, app: &App, ctx: &egui::Context, keep: bool) {
    let Some(take) = held.pending.clone() else {
        return;
    };
    held.busy = true;
    held.error = None;
    held.confirming = false;
    let (shared, ctx, project, reader) = (
        shared.clone(),
        ctx.clone(),
        held.project.clone(),
        CallReader::of(app),
    );
    std::thread::spawn(move || {
        let result = (|| {
            if keep {
                write(&reader.state, &project, &take)?;
                let text = crate::media_ai::speech(&reader.engines, &reader.state, &take.path)?;
                let mut journal = Journal::load(&reader.config, &project);
                journal.add_kind(
                    &format!("{}: {}", take.title.trim(), text.trim()),
                    true,
                    take.kind,
                );
                journal
                    .save(&reader.config, &project)
                    .map_err(|e| e.to_string())?;
                crate::projects::engine::refresh(&project);
            }
            remove(&reader.state, &project, &take)
        })();
        let mut job = shared.lock().unwrap_or_else(|p| p.into_inner());
        job.busy = false;
        match result {
            Ok(()) => {
                job.pending = None;
                job.saved = keep.then(Instant::now);
            }
            Err(error) => job.error = Some(error),
        }
        ctx.request_repaint();
    });
}

/// A live microphone note is kept before quitting, just like pressing Stop.
pub(crate) fn shutdown() {
    let jobs: Vec<_> = JOBS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .values()
        .cloned()
        .collect();
    for shared in jobs {
        let thread = {
            let mut job = shared.lock().unwrap_or_else(|p| p.into_inner());
            stop(&shared, &mut job, &egui::Context::default());
            job.storing.take()
        };
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spoken_note_is_recoverable_before_any_transcription_and_discard_removes_it() {
        let root = tempfile::tempdir().unwrap();
        let take = keep(
            root.path(),
            "project",
            "Named note".into(),
            NoteKind::Memo,
            2,
            &[0.1; 96_000],
        )
        .unwrap();
        let restored = load(root.path(), "project").unwrap();
        assert_eq!(restored.title, "Named note");
        assert_eq!(restored.seconds, 2);
        assert_eq!(restored.kind, NoteKind::Memo);
        assert!(load(root.path(), "another-project").is_none());
        assert!(crate::transcribe::samples(&take.path).is_ok());
        remove(root.path(), "project", &restored).unwrap();
        assert!(!take.path.exists() && load(root.path(), "project").is_none());
    }
}
