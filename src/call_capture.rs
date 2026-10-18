//! Recording a call made in Whats Pro, the app's side. The call's worker taps
//! both voices into a stereo WAV (`call_tape`); this names the recording,
//! writes its context beside it before it starts, keeps it in the call
//! library once it stops, and reads it (transcript, summary, a note in its
//! project) only after the person chooses to keep it. A recording a quit or a
//! crash left behind is picked up on the next launch.
//!
//! Everything here runs from `handle_events` and `tick`, so it works with the
//! window hidden or locked: the call screen only pushes actions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::backend::Waker;
use crate::call_library::{CallAnalysis, Library};
use crate::i18n::{Locale, gettext};
use crate::model::{Action, CallDirection, Page};
use crate::ui::projects::CallReader;

/// What a recording is about, written to `<recording>.json` before it is
/// asked to start, so a crash still knows whose call it was.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Meta {
    pub title: String,
    /// The chat and account the call was on; kept for a locked chat too, for
    /// matching, while its title and people leave the name out.
    pub chat: Option<String>,
    pub account: Option<String>,
    pub project: Option<String>,
    pub lead: Option<String>,
    pub video: bool,
    /// "incoming" or "outgoing".
    pub direction: Option<String>,
    /// The group's name, for a group call.
    pub group: Option<String>,
    /// Who was on a group call, by name.
    pub participants: Vec<String>,
    /// A locked chat's call.
    pub private: bool,
    /// When the call started, Unix seconds.
    pub started_at: i64,
    /// Whether the notice was asked for.
    pub notice: bool,
}

impl Meta {
    /// Writes the context beside the recording at `recording`.
    pub fn write(&self, recording: &Path) -> Result<(), String> {
        let path = sidecar(recording);
        let temporary = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&temporary, bytes).map_err(|e| e.to_string())?;
        std::fs::rename(&temporary, &path).map_err(|e| e.to_string())
    }

    /// The context written beside the recording at `recording`, if any.
    pub fn read(recording: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(sidecar(recording)).ok()?;
        serde_json::from_str(&text).ok()
    }
}

/// Where recordings are written until they are kept in the library.
pub fn dir(state: &Path) -> PathBuf {
    state.join("recordings")
}

/// The context file of the recording at `recording` (its `.wav`, or the
/// `.wav.part` it is written to): `<name>.wav.json`.
pub fn sidecar(recording: &Path) -> PathBuf {
    let name = recording
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = name.strip_suffix(".part").unwrap_or(&name);
    recording.with_file_name(format!("{name}.json"))
}

/// Deletes the context of a recording that will never be kept.
pub fn forget(recording: &Path) {
    let _ = std::fs::remove_file(sidecar(recording));
}

/// Names the recording of the call on screen and gathers its context: where
/// it goes, its title under the call's own account, its project and lead.
pub fn prepare(app: &mut App) -> Result<(PathBuf, Meta), String> {
    let call = app.call.clone().ok_or("There is no call to record")?;
    let locale = app.locale;
    let account = app.call_account.clone();
    let link = call.group.as_ref().is_some_and(|group| group.link);
    let group = call.group.is_some() && !link;
    // Locked whether or not its folder is open: the files outlive the folder.
    let private = !link
        && account
            .as_ref()
            .and_then(|id| app.accounts.iter().find(|owner| &owner.id == id))
            .is_some_and(|owner| {
                owner
                    .chats
                    .iter()
                    .any(|chat| chat.id == call.chat && chat.locked)
            });
    let name = app.call_name();
    let title = if link {
        gettext(locale, "Call by link").into_owned()
    } else if private {
        gettext(locale, "Call in a locked chat").into_owned()
    } else if group {
        gettext(locale, "Group call: {group}").replace("{group}", &name)
    } else if call.video {
        gettext(locale, "Video call with {name}").replace("{name}", &name)
    } else {
        gettext(locale, "Call with {name}").replace("{name}", &name)
    };
    let participants = if private {
        Vec::new()
    } else {
        app.call_people()
            .into_iter()
            .filter(|person| person.joined)
            .map(|person| person.name)
            .collect()
    };
    let project = (!link)
        .then(|| {
            crate::projects::ProjectMap::load(&app.dirs.config)
                .projects_of(&call.chat)
                .first()
                .map(|(project, _)| project.id.clone())
        })
        .flatten();
    let lead = project.as_ref().and_then(|project| {
        crate::leads::Store::open(&app.dirs.config)
            .by_chat(project, &call.chat)
            .map(|lead| lead.id.clone())
    });
    let now = jiff::Timestamp::now();
    let started_at = call
        .started
        .map(|started| now.as_second() - started.elapsed().as_secs() as i64)
        .unwrap_or(now.as_second());
    let meta = Meta {
        title,
        chat: (!link).then(|| call.chat.clone()),
        account: account.map(|account| account.0),
        project,
        lead,
        video: call.video,
        direction: Some(
            match call.direction {
                CallDirection::Incoming => "incoming",
                CallDirection::Outgoing => "outgoing",
            }
            .to_owned(),
        ),
        group: (group && !private).then_some(name),
        participants,
        private,
        started_at,
        notice: app.settings.call_record_notice,
    };
    let folder = dir(&app.dirs.state);
    std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
    // The time and the call's generation: two calls never share a file.
    let path = folder.join(format!(
        "call-{}-{}.wav",
        now.as_millisecond(),
        call.generation
    ));
    Ok((path, meta))
}

/// Why a recording failed, in the reader's language when it is one of the
/// reasons the call's worker gives.
pub fn reason(locale: Locale, reason: &str) -> String {
    let known = [
        crate::call_tape::BUSY,
        crate::call_tape::ALREADY,
        "The call is not connected yet",
        "The call is over",
        "There is no call to record",
    ];
    match known.into_iter().find(|known| *known == reason) {
        Some(known) => gettext(locale, known).into_owned(),
        None => reason.to_owned(),
    }
}

/// Adds the people who joined a group call after its recording started.
pub fn add_participants(recording: &Path, names: Vec<String>) {
    // A locked chat's recording names nobody.
    let Some(mut meta) = Meta::read(recording).filter(|meta| !meta.private) else {
        return;
    };
    let before = meta.participants.len();
    for name in names {
        if !meta.participants.contains(&name) {
            meta.participants.push(name);
        }
    }
    if meta.participants.len() != before
        && let Err(error) = meta.write(recording)
    {
        log::warn!("call capture: the participants could not be noted: {error}");
    }
}

/// A recording the worker stopped: kept in the library with its context,
/// waiting for the person to keep it and have it read, or to discard it.
pub fn kept(app: &mut App, report: crate::call_tape::Report) {
    let locale = app.locale;
    if let Some(error) = &report.error {
        log::warn!("call capture: the recording ended with an error: {error}");
    }
    let wav = finished(&report.path);
    let meta = Meta::read(&wav).unwrap_or_else(|| Meta {
        title: gettext(locale, "Recorded call").into_owned(),
        started_at: jiff::Timestamp::now().as_second() - report.seconds as i64,
        ..Meta::default()
    });
    let details = Details {
        seconds: report.seconds,
        notice_ms: report.notice_ms,
        levels: Some(report.levels),
        recovered: false,
    };
    let call = match file(&app.dirs.state, &wav, &meta, &details) {
        Ok(call) => call,
        Err(error) => {
            // The file and its context stay in the recordings folder, and the
            // next launch keeps them.
            app.toast_error(
                gettext(locale, "Could not keep the recording: {reason}")
                    .replace("{reason}", &error),
            );
            return;
        }
    };
    let time = crate::util::call_duration(call.seconds);
    let project = call.project.as_deref().and_then(|id| {
        crate::projects::ProjectMap::load(&app.dirs.config)
            .project(id)
            .map(|project| project.name.clone())
    });
    let message = match project {
        Some(project) => gettext(locale, "Call recorded ({time}). Kept in {project}.")
            .replace("{time}", &time)
            .replace("{project}", &project),
        None => gettext(
            locale,
            "Call recorded ({time}). Find it in Calls > Recorded.",
        )
        .replace("{time}", &time),
    };
    let label = gettext(locale, "Review").into_owned();
    app.toast_with(message, label, Action::OpenRecordedCall(call.id));
}

/// The `.wav` of a stopped recording: one left as `.part` (its rename
/// failed) is fixed first.
fn finished(path: &Path) -> PathBuf {
    if path
        .extension()
        .is_some_and(|extension| extension == "part")
    {
        match crate::call_tape::recover_part(path) {
            Ok(wav) => return wav,
            Err(error) => log::warn!("call capture: the recording could not be closed: {error}"),
        }
    }
    path.to_owned()
}

/// What the tape says about a recording, beside its context.
struct Details {
    seconds: u64,
    notice_ms: Option<u64>,
    levels: Option<[f32; 2]>,
    recovered: bool,
}

/// Moves the recording at `wav` into the library with its context, waiting
/// for review, and drops what is left in the recordings folder.
fn file(
    state: &Path,
    wav: &Path,
    meta: &Meta,
    details: &Details,
) -> Result<crate::call_library::Call, String> {
    let mut call = Library::keep_for_review(
        state,
        wav,
        details.seconds,
        &meta.title,
        meta.project.as_deref(),
        meta.lead.as_deref(),
    )?;
    let fill = |call: &mut crate::call_library::Call| {
        call.chat = meta.chat.clone();
        call.account = meta.account.clone();
        call.started_at = meta.started_at;
        call.video = meta.video;
        call.direction = meta.direction.clone();
        call.group = meta.group.clone();
        call.participants = meta.participants.clone();
        call.private = meta.private;
        call.notice_ms = details.notice_ms;
        call.levels = details.levels;
        call.stereo = true;
        call.recovered = details.recovered;
        call.pending_review = true;
    };
    fill(&mut call);
    // The audio is kept already; a context that could not be saved only
    // leaves the call plainer.
    if let Err(error) = Library::update(state, &call.id, fill) {
        log::warn!("call capture: the call's details could not be saved: {error}");
    }
    forget(wav);
    // Kept as a copy when it could not be moved.
    let _ = std::fs::remove_file(wav);
    Ok(call)
}

/// What reading a call is doing.
#[derive(Clone, Debug, PartialEq)]
pub enum JobPhase {
    Reading,
    Summarizing,
    /// Read, with its summary (empty without one).
    Done(String),
    Failed(String),
}

/// A read that finished, for the window to announce.
#[derive(Clone, Debug, PartialEq)]
pub struct Finished {
    pub id: String,
    pub title: String,
    pub outcome: Result<String, String>,
}

/// Every call read in this run, by id. Process-wide, so closing a window or
/// a page never orphans a read.
static JOBS: LazyLock<Mutex<HashMap<String, JobPhase>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static FINISHED: Mutex<Vec<Finished>> = Mutex::new(Vec::new());
static QUITTING: AtomicBool = AtomicBool::new(false);

/// What reading the call `id` is doing in this run, if it is being read.
pub fn job(id: &str) -> Option<JobPhase> {
    JOBS.lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(id)
        .cloned()
}

fn set_job(id: &str, phase: JobPhase) {
    JOBS.lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(id.to_owned(), phase);
}

fn finish(id: &str, title: &str, outcome: Result<String, String>) {
    set_job(
        id,
        match &outcome {
            Ok(summary) => JobPhase::Done(summary.clone()),
            Err(reason) => JobPhase::Failed(reason.clone()),
        },
    );
    FINISHED
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(Finished {
            id: id.to_owned(),
            title: title.to_owned(),
            outcome,
        });
}

/// The reads that finished since the last call, for `tick` to announce.
pub fn drain_finished() -> Vec<Finished> {
    std::mem::take(&mut *FINISHED.lock().unwrap_or_else(|p| p.into_inner()))
}

/// Says the app is closing: a read that has not started does not start, and
/// its call stays waiting to be read on the next launch.
pub fn quitting() {
    QUITTING.store(true, Ordering::Relaxed);
}

fn closing() -> bool {
    QUITTING.load(Ordering::Relaxed)
}

/// "Save and transcribe": the person keeps the recording, and it is read on
/// a thread of its own.
pub fn save_and_read(app: &mut App, id: &str) {
    if matches!(job(id), Some(JobPhase::Reading | JobPhase::Summarizing)) {
        return;
    }
    let reader = CallReader::of(app);
    if let Err(error) = Library::update(&reader.state, id, |call| {
        call.pending_review = false;
        call.pending_read = true;
    }) {
        app.toast_error(
            gettext(app.locale, "Could not read the call: {reason}")
                .replace("{reason}", &error.to_string()),
        );
        return;
    }
    // Shown at once, before the thread gets going.
    set_job(id, JobPhase::Reading);
    let (id, waker) = (id.to_owned(), app.waker());
    let failed_id = id.clone();
    let spawned = std::thread::Builder::new()
        .name("call-read".into())
        .spawn(move || read(&reader, &id, &waker));
    if let Err(error) = spawned {
        set_job(&failed_id, JobPhase::Failed(error.to_string()));
        app.toast_error(
            gettext(app.locale, "Could not read the call: {reason}")
                .replace("{reason}", &error.to_string()),
        );
    }
}

/// Transcribes a kept call, summarizes it and notes it in its project. Each
/// step is kept as it finishes; a quit in between leaves it waiting to be
/// read again. Blocks.
fn read(reader: &CallReader, id: &str, waker: &Waker) {
    let library = Library::load(&reader.state);
    let Some(call) = library.get(id).cloned() else {
        return;
    };
    if closing() {
        return;
    }
    set_job(id, JobPhase::Reading);
    waker.wake();
    // A read a quit cut short already has its transcript.
    if call.transcript().is_none()
        && let Err(error) = crate::call_library::transcribe(&reader.engines, &reader.state, id)
    {
        if closing() {
            return;
        }
        done_reading(&reader.state, id);
        finish(id, &call.title, Err(error));
        return waker.wake();
    }
    if closing() {
        return;
    }
    set_job(id, JobPhase::Summarizing);
    waker.wake();
    let analysis = match &call.analysis {
        Some(analysis) => Ok(analysis.clone()),
        None => crate::call_library::analyze(
            &reader.config,
            &reader.state,
            id,
            &context(&reader.config, &call),
        ),
    };
    if let Some(project) = &call.project
        && let Some(call) = Library::load(&reader.state).get(id)
    {
        crate::ui::projects::note_call(reader, project, call, &analysis);
    }
    if let Some(call) = Library::load(&reader.state).get(id) {
        lead_note(reader, call, &analysis);
    }
    done_reading(&reader.state, id);
    finish(
        id,
        &call.title,
        analysis.map(|analysis: CallAnalysis| analysis.summary),
    );
    waker.wake();
}

/// A read call updates its one entry in the lead's history on retries.
fn lead_note(
    reader: &CallReader,
    call: &crate::call_library::Call,
    analysis: &Result<CallAnalysis, String>,
) {
    let (Some(lead), Some(transcript)) = (&call.lead, call.transcript()) else {
        return;
    };
    let mut store = crate::leads::Store::open(&reader.config);
    let Some(lead) = store.get_mut(lead) else {
        return;
    };
    // The same note the lead's own recordings get: how long it was, what
    // happened, and who agreed to what.
    let heading = crate::leads::call_heading(call.seconds);
    let summary = analysis
        .as_ref()
        .map(|analysis| crate::leads::call_note(&heading, analysis))
        .unwrap_or_else(|_| {
            format!(
                "{}: {}",
                call.title,
                transcript.text.chars().take(400).collect::<String>()
            )
        });
    let existing = lead
        .entries
        .iter()
        .find(|entry| entry.call.as_deref() == Some(call.id.as_str()))
        .map(|entry| entry.id.clone());
    if let Some(entry) = existing {
        lead.redo_call(&entry, &summary, &transcript.text);
    } else {
        lead.add_call(&summary, &transcript.text, &call.id);
    }
    store.save();
}

fn done_reading(state: &Path, id: &str) {
    if let Err(error) = Library::update(state, id, |call| call.pending_read = false) {
        log::warn!("call capture: the call could not be marked as read: {error}");
    }
}

/// What the summary is told about a call: its project's context, or that it
/// was a WhatsApp call, and who was on it.
fn context(config: &Path, call: &crate::call_library::Call) -> String {
    let mut text = match &call.project {
        Some(project) => crate::ui::projects::call_context(config, project),
        None => String::new(),
    };
    if text.is_empty() {
        text.push_str("Llamada de WhatsApp grabada en Whats Pro.\n");
    }
    if !call.participants.is_empty() {
        text.push_str(&format!(
            "Participantes: {}\n",
            call.participants.join(", ")
        ));
    }
    if !text.ends_with("\n\n") {
        text.push('\n');
    }
    text
}

/// Moves a recorded call to `project` (or out of every project), with the
/// chat's lead there, and notes it in the new project if it was read.
pub fn move_to(app: &mut App, id: &str, project: Option<String>) {
    let reader = CallReader::of(app);
    let Some(chat) = Library::load(&reader.state)
        .get(id)
        .map(|call| call.chat.clone())
    else {
        return;
    };
    let lead = project.as_ref().zip(chat).and_then(|(project, chat)| {
        crate::leads::Store::open(&reader.config)
            .by_chat(project, &chat)
            .map(|lead| lead.id.clone())
    });
    let moved = Library::update(&reader.state, id, |call| {
        call.project = project.clone();
        call.lead = lead;
    });
    if let Err(error) = moved {
        app.toast_error(
            gettext(app.locale, "Could not move the call: {reason}")
                .replace("{reason}", &error.to_string()),
        );
        return;
    }
    if let Some(project) = &project
        && let Some(call) = Library::load(&reader.state).get(id)
        && let Some(analysis) = &call.analysis
    {
        crate::ui::projects::note_call(&reader, project, call, &Ok(analysis.clone()));
    }
}

/// Deletes a recorded call: its audio, transcripts and summary.
pub fn delete(app: &mut App, id: &str) {
    if let Err(error) = Library::remove(&app.dirs.state, id) {
        app.toast_error(
            gettext(app.locale, "Could not delete the call: {reason}")
                .replace("{reason}", &error.to_string()),
        );
        return;
    }
    JOBS.lock().unwrap_or_else(|p| p.into_inner()).remove(id);
}

/// What the launch found: recordings rescued from a quit or a crash, and the
/// recordings still waiting for review (the rescued ones among them).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Recovery {
    pub recovered: Vec<String>,
    pub unreviewed: Vec<String>,
}

static RECOVERY: Mutex<Option<Recovery>> = Mutex::new(None);

/// Rescues what the last run left behind, on a thread: unfinished files are
/// closed and kept, and calls whose read was cut short are read again. `tick`
/// announces it through [`drain_recovery`].
pub(crate) fn recover(reader: CallReader, locale: Locale, waker: Waker) {
    // Anything this run writes is newer than this.
    let since = SystemTime::now();
    let spawned = std::thread::Builder::new()
        .name("call-recover".into())
        .spawn(move || {
            let recovery = recover_now(&reader.state, locale, since);
            if !recovery.recovered.is_empty() || !recovery.unreviewed.is_empty() {
                *RECOVERY.lock().unwrap_or_else(|p| p.into_inner()) = Some(recovery);
                waker.wake();
            }
            let unread: Vec<String> = Library::load(&reader.state)
                .recorded()
                .into_iter()
                .filter(|call| call.pending_read && !call.pending_review)
                .map(|call| call.id.clone())
                .collect();
            for id in unread {
                read(&reader, &id, &waker);
            }
        });
    if let Err(error) = spawned {
        log::warn!("call capture: recordings could not be recovered: {error}");
    }
}

/// What startup recovery found, once.
pub fn drain_recovery() -> Option<Recovery> {
    RECOVERY.lock().unwrap_or_else(|p| p.into_inner()).take()
}

/// Closes the `.part` files and keeps the `.wav` files the recordings folder
/// holds from before `since`, and drops context files left with no audio.
/// Never touches a recording that is live in this process. Blocks.
fn recover_now(state: &Path, locale: Locale, since: SystemTime) -> Recovery {
    let folder = dir(state);
    let older = |path: &Path| {
        std::fs::metadata(path)
            .and_then(|m| m.modified())
            .is_ok_and(|modified| modified < since)
    };
    let names = |suffix: &str| -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(&folder) else {
            return Vec::new();
        };
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("call-") && name.ends_with(suffix))
            })
            .filter(|path| older(path) && !live(path))
            .collect();
        paths.sort();
        paths
    };
    for part in names(".wav.part") {
        if let Err(error) = crate::call_tape::recover_part(&part) {
            log::warn!(
                "call capture: {} could not be recovered: {error}",
                part.display()
            );
        }
    }
    let mut recovery = Recovery::default();
    for wav in names(".wav") {
        let meta = match Meta::read(&wav) {
            Some(meta) => Meta {
                title: gettext(locale, "Recovered: {title}").replace("{title}", &meta.title),
                ..meta
            },
            None => Meta {
                title: gettext(locale, "Recovered recording {date}")
                    .replace("{date}", &stamp(&wav)),
                ..Meta::default()
            },
        };
        let seconds = std::fs::metadata(&wav)
            .map(|m| m.len().saturating_sub(44) / (4 * u64::from(crate::call_tape::RATE)))
            .unwrap_or(0);
        let details = Details {
            seconds,
            notice_ms: None,
            levels: None,
            recovered: true,
        };
        match file(state, &wav, &meta, &details) {
            Ok(call) => recovery.recovered.push(call.id),
            Err(error) => log::warn!("call capture: a recording could not be kept: {error}"),
        }
    }
    // A context whose recording never started, or was discarded.
    for json in names(".wav.json") {
        let audio = json.with_extension("");
        let mut part = audio.clone().into_os_string();
        part.push(".part");
        if !audio.exists() && !Path::new(&part).exists() {
            let _ = std::fs::remove_file(&json);
        }
    }
    recovery.unreviewed = Library::load(state)
        .recorded()
        .into_iter()
        .filter(|call| call.pending_review)
        .map(|call| call.id.clone())
        .collect();
    recovery
}

/// Whether the recording at `path` is being written in this process now.
fn live(path: &Path) -> bool {
    let generation = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.split('.').next())
        .and_then(|stem| stem.rsplit('-').next())
        .and_then(|generation| generation.parse::<u64>().ok());
    generation.is_some_and(|generation| crate::call_tape::live_levels(generation).is_some())
}

/// When a recording was last written, as a date and time.
fn stamp(path: &Path) -> String {
    let seconds = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|at| at.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |at| at.as_secs() as i64);
    jiff::Timestamp::from_second(seconds)
        .map(|t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%d/%m/%Y %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

/// The toasts a launch's recovery deserves, for `tick` to show.
pub(crate) fn recovery_toast(locale: Locale, recovery: &Recovery) -> Option<(String, Action)> {
    let (message, ids) = if !recovery.recovered.is_empty() {
        let n = recovery.recovered.len();
        (
            crate::i18n::ngettext(
                locale,
                "We recovered {} recording that was left half way",
                "We recovered {} recordings that were left half way",
                n as u32,
            )
            .replace("{}", &n.to_string()),
            &recovery.recovered,
        )
    } else if !recovery.unreviewed.is_empty() {
        let n = recovery.unreviewed.len();
        (
            crate::i18n::ngettext(
                locale,
                "You have {} recording not reviewed",
                "You have {} recordings not reviewed",
                n as u32,
            )
            .replace("{}", &n.to_string()),
            &recovery.unreviewed,
        )
    } else {
        return None;
    };
    let action = match ids.as_slice() {
        [id] => Action::OpenRecordedCall(id.clone()),
        _ => Action::Open(Page::Calls),
    };
    Some((message, action))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stereo WAV a crash cut short: a header that counts nothing, then a
    /// second of samples and half of one.
    fn crashed(path: &Path) {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&36u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16_000u32.to_le_bytes());
        bytes.extend_from_slice(&64_000u32.to_le_bytes());
        bytes.extend_from_slice(&4u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend(std::iter::repeat_n(1u8, 64_000 + 2));
        std::fs::write(path, bytes).unwrap();
    }

    fn later() -> SystemTime {
        SystemTime::now() + std::time::Duration::from_secs(60)
    }

    #[test]
    fn the_context_file_sits_beside_the_recording_and_its_part() {
        let wav = Path::new("/r/call-1-2.wav");
        assert_eq!(sidecar(wav), Path::new("/r/call-1-2.wav.json"));
        assert_eq!(
            sidecar(Path::new("/r/call-1-2.wav.part")),
            Path::new("/r/call-1-2.wav.json")
        );
        // An old context without the newer fields still reads.
        let meta: Meta = serde_json::from_str(r#"{"title":"Call with Ada"}"#).unwrap();
        assert_eq!(meta.title, "Call with Ada");
        assert!(!meta.private);
    }

    #[test]
    fn a_crash_leaves_a_recording_the_next_launch_keeps_for_review() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path();
        std::fs::create_dir_all(dir(state)).unwrap();
        let part = dir(state).join("call-1700000000000-987654.wav.part");
        crashed(&part);
        let meta = Meta {
            title: "Call with Ada".into(),
            chat: Some("ada".into()),
            account: Some("1".into()),
            started_at: 1_700_000_000,
            ..Meta::default()
        };
        meta.write(&part).unwrap();
        // A context whose recording never started.
        let lonely = dir(state).join("call-1700000000001-987655.wav");
        Meta::default().write(&lonely).unwrap();

        let recovery = recover_now(state, Locale::English, later());
        assert_eq!(recovery.recovered.len(), 1);
        assert_eq!(
            recovery.unreviewed, recovery.recovered,
            "waiting for review"
        );
        let library = Library::load(state);
        let call = library.get(&recovery.recovered[0]).unwrap();
        assert_eq!(call.title, "Recovered: Call with Ada");
        assert_eq!(call.chat.as_deref(), Some("ada"));
        assert_eq!(call.started_at, 1_700_000_000);
        assert_eq!(call.seconds, 1);
        assert!(call.recovered && call.stereo && call.pending_review);
        assert!(!call.pending_read, "nothing reads it before the review");
        let left: Vec<_> = std::fs::read_dir(dir(state)).unwrap().flatten().collect();
        assert!(left.is_empty(), "nothing is left behind: {left:?}");
        let decoder = rodio::Decoder::new(std::fs::File::open(call.path(state)).unwrap()).unwrap();
        assert_eq!(rodio::Source::channels(&decoder).get(), 2);

        // A second launch finds nothing new, and still the one to review.
        let again = recover_now(state, Locale::English, later());
        assert!(again.recovered.is_empty());
        assert_eq!(again.unreviewed, recovery.recovered);
    }

    #[test]
    fn a_recording_newer_than_the_launch_is_left_alone() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path();
        std::fs::create_dir_all(dir(state)).unwrap();
        let part = dir(state).join("call-1700000000000-5.wav.part");
        crashed(&part);
        let since = SystemTime::now() - std::time::Duration::from_secs(60);
        let recovery = recover_now(state, Locale::English, since);
        assert_eq!(recovery, Recovery::default());
        assert!(part.exists());
    }

    #[test]
    fn a_recording_without_its_context_is_still_kept() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path();
        std::fs::create_dir_all(dir(state)).unwrap();
        crashed(&dir(state).join("call-1700000000000-6.wav.part"));
        let recovery = recover_now(state, Locale::English, later());
        let call = Library::load(state)
            .get(&recovery.recovered[0])
            .cloned()
            .unwrap();
        assert!(
            call.title.starts_with("Recovered recording "),
            "{}",
            call.title
        );
        assert!(call.chat.is_none());
    }

    #[test]
    fn the_launch_toast_points_at_the_one_recording_or_at_the_calls() {
        let one = Recovery {
            recovered: vec!["a".into()],
            unreviewed: vec!["a".into(), "b".into()],
        };
        let (message, action) = recovery_toast(Locale::English, &one).unwrap();
        assert_eq!(message, "We recovered 1 recording that was left half way");
        assert_eq!(action, Action::OpenRecordedCall("a".into()));
        let waiting = Recovery {
            recovered: Vec::new(),
            unreviewed: vec!["a".into(), "b".into()],
        };
        let (message, action) = recovery_toast(Locale::English, &waiting).unwrap();
        assert_eq!(message, "You have 2 recordings not reviewed");
        assert_eq!(action, Action::Open(Page::Calls));
        assert!(recovery_toast(Locale::English, &Recovery::default()).is_none());
    }

    #[test]
    fn a_read_does_not_start_while_the_app_quits() {
        let directory = tempfile::tempdir().unwrap();
        let reader = CallReader {
            engines: crate::media_ai::MediaEngines::default(),
            state: directory.path().to_owned(),
            config: directory.path().to_owned(),
        };
        let source = directory.path().join("source.wav");
        crashed(&source);
        let call = Library::keep_for_review(&reader.state, &source, 1, "Call", None, None).unwrap();
        Library::update(&reader.state, &call.id, |call| call.pending_read = true).unwrap();
        QUITTING.store(true, Ordering::Relaxed);
        read(&reader, &call.id, &Waker::default());
        QUITTING.store(false, Ordering::Relaxed);
        assert!(job(&call.id).is_none(), "nothing was started");
        assert!(
            Library::load(&reader.state)
                .get(&call.id)
                .unwrap()
                .pending_read
        );
    }

    #[test]
    fn reading_a_call_again_updates_one_lead_history_entry() {
        let directory = tempfile::tempdir().unwrap();
        let reader = CallReader {
            engines: crate::media_ai::MediaEngines::default(),
            state: directory.path().join("state"),
            config: directory.path().join("config"),
        };
        let lead = {
            let mut store = crate::leads::Store::open(&reader.config);
            let id = store.add(crate::leads::Lead::new("p", "Fixture", None, None));
            store.save();
            id
        };
        let mut call: crate::call_library::Call = serde_json::from_value(serde_json::json!({
            "id": "fixture", "title": "Fixture call", "at": 0, "file": "fixture.wav", "lead": lead,
        }))
        .unwrap();
        call.transcripts.push(crate::call_library::Transcript {
            engine: "fixture".into(),
            text: "Fixture transcript".into(),
            at: 0,
        });
        for summary in ["First summary", "Updated summary"] {
            lead_note(
                &reader,
                &call,
                &Ok(CallAnalysis {
                    summary: summary.into(),
                    ..Default::default()
                }),
            );
        }
        let store = crate::leads::Store::open(&reader.config);
        let calls: Vec<_> = store
            .get(&lead)
            .unwrap()
            .entries
            .iter()
            .filter(|entry| entry.kind == crate::leads::Kind::Call)
            .collect();
        assert_eq!(calls.len(), 1);
        // The lead's note: the call's length, then its summary.
        assert!(calls[0].text.starts_with('('), "{}", calls[0].text);
        assert!(
            calls[0].text.ends_with("Updated summary"),
            "{}",
            calls[0].text
        );
        assert_eq!(calls[0].transcript.as_deref(), Some("Fixture transcript"));
    }

    #[test]
    fn a_call_without_a_summary_notes_only_an_excerpt() {
        let directory = tempfile::tempdir().unwrap();
        let reader = CallReader {
            engines: crate::media_ai::MediaEngines::default(),
            state: directory.path().join("state"),
            config: directory.path().join("config"),
        };
        let mut call: crate::call_library::Call = serde_json::from_value(serde_json::json!({
            "id": "1", "title": "Call with Ada", "at": 0, "file": "1.wav",
        }))
        .unwrap();
        call.transcripts.push(crate::call_library::Transcript {
            engine: "local".into(),
            text: "palabra ".repeat(200),
            at: 0,
        });
        crate::ui::projects::note_call(&reader, "p", &call, &Err("no engine".into()));
        let analysis = CallAnalysis {
            summary: "Quedamos en mandar el presupuesto.".into(),
            commitments: vec![crate::call_library::Commitment {
                who: "Yo".into(),
                what: "mandar el presupuesto".into(),
                due: Some("el lunes".into()),
            }],
            ..CallAnalysis::default()
        };
        crate::ui::projects::note_call(&reader, "p", &call, &Ok(analysis));
        let journal = crate::projects::Journal::load(&reader.config, "p");
        let texts: Vec<&str> = journal
            .notes
            .iter()
            .map(|note| note.text.as_str())
            .collect();
        let without = texts
            .iter()
            .find(|text| text.contains("sin resumen"))
            .expect("a note without a summary");
        assert!(
            without.starts_with("Call with Ada: transcripta, sin resumen (no engine)\n"),
            "{without}"
        );
        assert!(without.chars().count() < 500, "{}", without.chars().count());
        assert!(without.ends_with('…'));
        assert!(
            texts
                .iter()
                .any(|text| text.contains("Compromiso: Yo: mandar el presupuesto (para el lunes)")),
            "{texts:?}"
        );
    }
}
