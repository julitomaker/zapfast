//! Recorded and uploaded calls, kept as files first: a call is saved the
//! moment it ends or is added, before anything reads it, so it can be read
//! again later, by another engine, or by another tool altogether.
//!
//! Each call keeps its audio under `<state>/calls/<id>.<ext>`, every
//! transcript made of it (with the engine that made it), and an analysis
//! (summary, decisions, commitments, tasks, next steps). A call belongs to
//! a project, a lead, or both; Projects and Leads share the one record.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// Held around every read-change-save of the index, so two threads (a
/// call's reader and the person renaming it) never save over each other.
static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The index the views read, kept while the file's stamp and size hold.
type Cached = (Option<SystemTime>, u64, Arc<Library>);
static CACHE: Mutex<Option<HashMap<PathBuf, Cached>>> = Mutex::new(None);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// Recorded here: the microphone, the computer's sound, or both.
    #[default]
    Recorded,
    /// An audio or video file added by hand.
    Uploaded,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    /// Which engine and model read it (`MediaEngines::speech_name`).
    pub engine: String,
    pub text: String,
    pub at: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Commitment {
    #[serde(default)]
    pub who: String,
    pub what: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due: Option<String>,
}

/// What a call said, read by the analysis engine.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CallAnalysis {
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub commitments: Vec<Commitment>,
    #[serde(default)]
    pub tasks: Vec<String>,
    #[serde(default)]
    pub next_steps: Vec<String>,
    #[serde(default)]
    pub at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Call {
    pub id: String,
    pub title: String,
    /// When it happened (or was added), Unix seconds.
    pub at: i64,
    #[serde(default)]
    pub seconds: u64,
    #[serde(default)]
    pub origin: Origin,
    /// The audio's file name inside the calls folder.
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead: Option<String>,
    #[serde(default)]
    pub transcripts: Vec<Transcript>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analysis: Option<CallAnalysis>,
    /// The WhatsApp chat of a call recorded in Whats Pro, and the account
    /// that had it. Never set for a locked chat's call shown by name only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// When the call itself started, Unix seconds (`at` is when it was
    /// kept); 0 when unknown.
    #[serde(default)]
    pub started_at: i64,
    #[serde(default)]
    pub video: bool,
    /// "incoming" or "outgoing".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    /// The group's name, for a group call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub participants: Vec<String>,
    /// A locked chat's call: shown only while the locked folder is open.
    #[serde(default)]
    pub private: bool,
    /// Where in the audio the recording notice started, if one played.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice_ms: Option<u64>,
    /// How loud each side was on average (0 to 1): left is this side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub levels: Option<[f32; 2]>,
    /// Left is this side and right the other one.
    #[serde(default)]
    pub stereo: bool,
    /// Rescued from a recording a crash or a quit left behind.
    #[serde(default)]
    pub recovered: bool,
    /// Kept but not read yet: waiting for its transcript and analysis.
    #[serde(default)]
    pub pending_read: bool,
    /// Kept but not looked at yet: the person still chooses to transcribe
    /// it or to discard it.
    #[serde(default)]
    pub pending_review: bool,
}

impl Call {
    /// The newest transcript.
    pub fn transcript(&self) -> Option<&Transcript> {
        self.transcripts.last()
    }

    pub fn path(&self, state: &Path) -> PathBuf {
        Library::dir(state).join(&self.file)
    }

    /// When the call happened: its start when known, else when it was kept.
    pub fn when(&self) -> i64 {
        if self.started_at > 0 {
            self.started_at
        } else {
            self.at
        }
    }

    /// The call as Markdown: what it was, its analysis and its transcript.
    pub fn markdown(&self) -> String {
        let mut text = format!("# {}\n\n", self.title);
        text.push_str(&format!(
            "- Fecha: {}\n- Duración: {}:{:02}\n\n",
            stamp(self.at),
            self.seconds / 60,
            self.seconds % 60
        ));
        if let Some(analysis) = &self.analysis {
            if !analysis.summary.is_empty() {
                text.push_str(&format!("## Resumen\n\n{}\n\n", analysis.summary));
            }
            let list = |title: &str, items: &[String], text: &mut String| {
                if !items.is_empty() {
                    text.push_str(&format!("## {title}\n\n"));
                    for item in items {
                        text.push_str(&format!("- {item}\n"));
                    }
                    text.push('\n');
                }
            };
            list("Decisiones", &analysis.decisions, &mut text);
            if !analysis.commitments.is_empty() {
                text.push_str("## Compromisos\n\n");
                for c in &analysis.commitments {
                    let due = c
                        .due
                        .as_deref()
                        .map(|d| format!(" (para {d})"))
                        .unwrap_or_default();
                    text.push_str(&format!("- {}: {}{due}\n", c.who, c.what));
                }
                text.push('\n');
            }
            list("Tareas", &analysis.tasks, &mut text);
            list("Próximos pasos", &analysis.next_steps, &mut text);
        }
        if let Some(transcript) = self.transcript() {
            text.push_str(&format!(
                "## Transcripción ({})\n\n{}\n",
                transcript.engine, transcript.text
            ));
        }
        text
    }
}

fn stamp(seconds: i64) -> String {
    jiff::Timestamp::from_second(seconds)
        .map(|t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%d/%m/%Y %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

/// Every call, oldest first.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Library {
    #[serde(default)]
    pub calls: Vec<Call>,
}

impl Library {
    pub fn dir(state: &Path) -> PathBuf {
        state.join("calls")
    }

    fn file(state: &Path) -> PathBuf {
        Self::dir(state).join("calls.json")
    }

    /// The index, or an empty one when it cannot be read (logged).
    pub fn load(state: &Path) -> Self {
        Self::load_checked(state).unwrap_or_else(|error| {
            log::warn!("call library: {error}");
            Self::default()
        })
    }

    /// The index, empty when there is none yet. One that does not parse is
    /// set aside as `calls.json.bad` first, so the next save starts a new
    /// list instead of wiping the old one; when it cannot be set aside this
    /// fails and nothing may be saved.
    pub fn load_checked(state: &Path) -> Result<Self, String> {
        let _held = lock();
        Self::read(state)
    }

    /// `load_checked` with the lock already held.
    fn read(state: &Path) -> Result<Self, String> {
        let path = Self::file(state);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(format!("calls.json could not be read: {error}")),
        };
        match serde_json::from_str(&text) {
            Ok(library) => Ok(library),
            Err(error) => {
                let aside = Self::aside(state);
                std::fs::rename(&path, &aside).map_err(|e| {
                    format!("calls.json is broken ({error}) and could not be set aside: {e}")
                })?;
                log::warn!(
                    "call library: calls.json is broken ({error}); kept as {}",
                    aside.display()
                );
                Self::forget(state);
                Ok(Self::default())
            }
        }
    }

    /// A name for a broken index that does not replace an earlier one.
    fn aside(state: &Path) -> PathBuf {
        let dir = Self::dir(state);
        let first = dir.join("calls.json.bad");
        if !first.exists() {
            return first;
        }
        let at = jiff::Timestamp::now().as_millisecond();
        dir.join(format!("calls-{at}.json.bad"))
    }

    /// Saves the index. Callers that change it go through `keep`, `update`
    /// or `remove`, which hold the lock around the read and the save.
    pub fn save(&self, state: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(Self::dir(state))?;
        let path = Self::file(state);
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(temporary, path)?;
        Self::forget(state);
        Ok(())
    }

    /// The index for the views, read again only when the file changed, so a
    /// page drawn every frame does not parse it every frame.
    pub fn cached(state: &Path) -> Arc<Self> {
        let path = Self::file(state);
        let (stamp, size) = std::fs::metadata(&path)
            .map(|m| (m.modified().ok(), m.len()))
            .unwrap_or((None, 0));
        if let Some((s, n, library)) = CACHE
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .and_then(|cache| cache.get(&path))
            && *s == stamp
            && *n == size
        {
            return Arc::clone(library);
        }
        // Read without the cache held: reading takes the library's lock,
        // and a save under that lock clears the cache.
        let library = Arc::new(Self::load(state));
        CACHE
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_or_insert_with(HashMap::new)
            .insert(path, (stamp, size, Arc::clone(&library)));
        library
    }

    /// Drops the cached index, after this process changed the file.
    fn forget(state: &Path) {
        let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(cache) = cache.as_mut() {
            cache.remove(&Self::file(state));
        }
    }

    pub fn get(&self, id: &str) -> Option<&Call> {
        self.calls.iter().find(|call| call.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Call> {
        self.calls.iter_mut().find(|call| call.id == id)
    }

    /// The calls of `project`, newest first.
    pub fn of_project(&self, project: &str) -> Vec<&Call> {
        let mut calls: Vec<&Call> = self
            .calls
            .iter()
            .filter(|call| call.project.as_deref() == Some(project))
            .collect();
        calls.sort_by_key(|call| std::cmp::Reverse(call.at));
        calls
    }

    /// The calls recorded in `account`'s chat `chat`, newest first.
    pub fn of_chat(&self, account: &str, chat: &str) -> Vec<&Call> {
        let mut calls: Vec<&Call> = self
            .calls
            .iter()
            .filter(|call| {
                call.account.as_deref() == Some(account) && call.chat.as_deref() == Some(chat)
            })
            .collect();
        calls.sort_by_key(|call| std::cmp::Reverse(call.when()));
        calls
    }

    /// Every recording made here, newest first.
    pub fn recorded(&self) -> Vec<&Call> {
        let mut calls: Vec<&Call> = self
            .calls
            .iter()
            .filter(|call| call.origin == Origin::Recorded)
            .collect();
        calls.sort_by_key(|call| std::cmp::Reverse(call.when()));
        calls
    }

    /// The recording of the chat's call entry stamped `timestamp` (Unix
    /// seconds): the closest one within [`crate::model::CALL_ENTRY_WINDOW`],
    /// since the entry and the recording are stamped by different clocks.
    /// Matched by chat and time, never by the entry's id.
    pub fn near(&self, chat: &str, timestamp: i64) -> Option<&Call> {
        self.calls
            .iter()
            .filter(|call| call.chat.as_deref() == Some(chat))
            .map(|call| ((call.when() - timestamp).abs(), call))
            .filter(|(gap, _)| *gap <= crate::model::CALL_ENTRY_WINDOW)
            .min_by_key(|(gap, _)| *gap)
            .map(|(_, call)| call)
    }

    /// Matches a call within one account; chat ids alone are not unique.
    pub fn near_account(&self, account: &str, chat: &str, timestamp: i64) -> Option<&Call> {
        self.of_chat(account, chat)
            .into_iter()
            .filter(|call| (call.when() - timestamp).abs() <= crate::model::CALL_ENTRY_WINDOW)
            .min_by_key(|call| (call.when() - timestamp).abs())
    }

    /// Keeps the audio at `source` in the library, moved (a recording made
    /// here) or copied (a file the person added, which stays where it is),
    /// and records it. Saved at once, before anything reads it.
    #[allow(clippy::too_many_arguments)]
    pub fn keep(
        state: &Path,
        source: &Path,
        origin: Origin,
        seconds: u64,
        title: &str,
        project: Option<&str>,
        lead: Option<&str>,
    ) -> Result<Call, String> {
        let dir = Self::dir(state);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let _held = lock();
        // Read first: a broken index leaves the audio where it was.
        let mut library = Self::read(state)?;
        let at = jiff::Timestamp::now().as_second();
        let mut random = [0u8; 4];
        let _ = getrandom::fill(&mut random);
        let id = format!(
            "{at}-{}",
            random
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let extension = source
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_else(|| "wav".to_owned());
        let file = format!("{id}.{extension}");
        let target = dir.join(&file);
        let moved = origin == Origin::Recorded && std::fs::rename(source, &target).is_ok();
        if !moved {
            std::fs::copy(source, &target).map_err(|e| e.to_string())?;
        }
        let call = Call {
            id,
            title: title.trim().to_owned(),
            at,
            seconds,
            origin,
            file,
            project: project.map(str::to_owned),
            lead: lead.map(str::to_owned),
            transcripts: Vec::new(),
            analysis: None,
            chat: None,
            account: None,
            started_at: 0,
            video: false,
            direction: None,
            group: None,
            participants: Vec::new(),
            private: false,
            notice_ms: None,
            levels: None,
            stereo: false,
            recovered: false,
            pending_read: false,
            pending_review: origin == Origin::Recorded,
        };
        library.calls.push(call.clone());
        library.save(state).map_err(|e| e.to_string())?;
        Ok(call)
    }

    /// Removes a call and its audio.
    pub fn remove(state: &Path, id: &str) -> std::io::Result<()> {
        let _held = lock();
        let mut library = Self::read(state).map_err(std::io::Error::other)?;
        if let Some(call) = library.get(id) {
            let _ = std::fs::remove_file(call.path(state));
        }
        library.calls.retain(|call| call.id != id);
        library.save(state)
    }

    /// Changes something of one call and saves it, read fresh so another
    /// thread's change is not lost. `change` runs under the library's lock,
    /// so it must not use the library itself.
    pub fn update(state: &Path, id: &str, change: impl FnOnce(&mut Call)) -> std::io::Result<()> {
        let _held = lock();
        let mut library = Self::read(state).map_err(std::io::Error::other)?;
        if let Some(call) = library.get_mut(id) {
            change(call);
        }
        library.save(state)
    }
}

/// Reads a call's audio with `engines` and keeps the transcript beside the
/// ones it had. Blocks; run it off the interface thread.
pub fn transcribe(
    engines: &crate::media_ai::MediaEngines,
    state: &Path,
    id: &str,
) -> Result<String, String> {
    let call = Library::load(state)
        .get(id)
        .cloned()
        .ok_or("The call is not in the library")?;
    let text = crate::media_ai::speech(engines, state, &call.path(state))?;
    let text = text.trim().to_owned();
    if text.is_empty() {
        return Err("Nothing was heard in it".to_owned());
    }
    Library::update(state, id, |call| {
        call.transcripts.push(Transcript {
            engine: engines.speech_name(),
            text: text.clone(),
            at: jiff::Timestamp::now().as_second(),
        });
    })
    .map_err(|e| e.to_string())?;
    Ok(text)
}

const ANALYSIS: &str = "Leés la transcripción de una llamada de trabajo y la ordenás, en español rioplatense, breve y concreto. \
Respondé SOLO con un objeto JSON válido con esta forma:\n\
{\"summary\":\"\",\"decisions\":[\"\"],\"commitments\":[{\"who\":\"\",\"what\":\"\",\"due\":\"\"}],\"tasks\":[\"\"],\"next_steps\":[\"\"]}\n\
- summary: de qué se habló y cómo quedó, 3 a 6 oraciones.\n\
- decisions: lo que se decidió.\n\
- commitments: quién se comprometió a qué y para cuándo (\"Yo\" es el dueño de la grabación).\n\
- tasks: lo que hay que hacer después de la llamada.\n\
- next_steps: lo que sigue en la relación o el proyecto.\n\
- No inventes nada que no esté en la transcripción.";

/// Reads a call's transcript with the analysis engine and keeps what it
/// found. Blocks; run it off the interface thread.
pub fn analyze(
    config: &Path,
    state: &Path,
    id: &str,
    context: &str,
) -> Result<CallAnalysis, String> {
    let call = Library::load(state)
        .get(id)
        .cloned()
        .ok_or("The call is not in the library")?;
    let transcript = call
        .transcript()
        .ok_or("Transcribe the call first")?
        .text
        .clone();
    let user = format!(
        "{context}Llamada: {}\nFecha: {}\nDuración: {}:{:02}\n\nTranscripción:\n{transcript}",
        call.title,
        stamp(call.at),
        call.seconds / 60,
        call.seconds % 60
    );
    let reply = crate::projects::engine::ask(config, ANALYSIS, &user).map_err(|e| e.to_string())?;
    let start = reply.find('{').ok_or("The engine answered without JSON")?;
    let end = reply.rfind('}').ok_or("The engine answered without JSON")?;
    let mut analysis: CallAnalysis =
        serde_json::from_str(&reply[start..=end]).map_err(|e| e.to_string())?;
    analysis.at = jiff::Timestamp::now().as_second();
    let kept = analysis.clone();
    Library::update(state, id, |call| call.analysis = Some(kept)).map_err(|e| e.to_string())?;
    Ok(analysis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_is_kept_before_it_is_read_and_removed_with_its_file() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("Llamada.wav");
        std::fs::write(&source, b"RIFF").unwrap();
        let call = Library::keep(
            root.path(),
            &source,
            Origin::Recorded,
            65,
            "Con Diego",
            Some("jas"),
            None,
        )
        .unwrap();
        assert!(!source.exists(), "a recording is moved in");
        assert!(call.path(root.path()).is_file());
        let library = Library::load(root.path());
        assert_eq!(library.of_project("jas").len(), 1);
        assert!(library.get(&call.id).unwrap().transcript().is_none());
        Library::update(root.path(), &call.id, |c| {
            c.transcripts.push(Transcript {
                engine: "a".into(),
                text: "uno".into(),
                at: 1,
            });
            c.transcripts.push(Transcript {
                engine: "b".into(),
                text: "dos".into(),
                at: 2,
            });
        })
        .unwrap();
        let call = Library::load(root.path()).get(&call.id).cloned().unwrap();
        assert_eq!(call.transcript().unwrap().text, "dos");
        assert!(call.markdown().contains("## Transcripción (b)"));
        Library::remove(root.path(), &call.id).unwrap();
        assert!(!call.path(root.path()).exists());
        assert!(Library::load(root.path()).calls.is_empty());
    }

    #[test]
    fn an_added_file_is_copied_and_left_where_it_was() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("nota.m4a");
        std::fs::write(&source, b"x").unwrap();
        let call = Library::keep(
            root.path(),
            &source,
            Origin::Uploaded,
            0,
            "Nota",
            None,
            Some("lead-1"),
        )
        .unwrap();
        assert!(source.exists());
        assert!(call.file.ends_with(".m4a"));
        assert_eq!(call.lead.as_deref(), Some("lead-1"));
    }

    fn kept(state: &Path, title: &str, change: impl FnOnce(&mut Call)) -> Call {
        let source = state.join(format!("{title}.wav"));
        std::fs::write(&source, b"RIFF").unwrap();
        let call = Library::keep(state, &source, Origin::Recorded, 1, title, None, None).unwrap();
        Library::update(state, &call.id, change).unwrap();
        Library::load(state).get(&call.id).cloned().unwrap()
    }

    #[test]
    fn an_index_from_before_the_call_details_still_loads() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(Library::dir(root.path())).unwrap();
        std::fs::write(
            Library::file(root.path()),
            r#"{"calls":[{"id":"1-ab","title":"Con Diego","at":5,"seconds":65,
               "origin":"recorded","file":"1-ab.wav","project":"jas"}]}"#,
        )
        .unwrap();
        let library = Library::load_checked(root.path()).unwrap();
        let call = library.get("1-ab").unwrap();
        assert_eq!(call.project.as_deref(), Some("jas"));
        assert_eq!((call.chat.as_deref(), call.started_at), (None, 0));
        assert!(!call.stereo && !call.pending_read && !call.pending_review);
        assert_eq!(
            call.when(),
            5,
            "an unknown start falls back to when it was kept"
        );
    }

    #[test]
    fn a_broken_index_is_set_aside_and_never_saved_over() {
        let root = tempfile::tempdir().unwrap();
        let dir = Library::dir(root.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(Library::file(root.path()), b"{\"calls\": [ oops").unwrap();
        let call = kept(root.path(), "Nueva", |_| {});
        assert_eq!(
            std::fs::read(dir.join("calls.json.bad")).unwrap(),
            b"{\"calls\": [ oops"
        );
        assert_eq!(Library::load(root.path()).calls, vec![call]);
        // A second break does not replace the first one set aside.
        std::fs::write(Library::file(root.path()), b"nope").unwrap();
        assert!(Library::load_checked(root.path()).unwrap().calls.is_empty());
        assert!(dir.join("calls.json.bad").is_file());
        let aside = std::fs::read_dir(&dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".bad")
            })
            .count();
        assert_eq!(aside, 2);
    }

    #[cfg(unix)]
    #[test]
    fn a_broken_index_that_cannot_be_set_aside_stops_every_change() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dir = Library::dir(root.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(Library::file(root.path()), b"oops").unwrap();
        let source = root.path().join("Llamada.wav");
        std::fs::write(&source, b"RIFF").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let keep = Library::keep(root.path(), &source, Origin::Recorded, 1, "x", None, None);
        let update = Library::update(root.path(), "1-ab", |c| c.title.clear());
        let remove = Library::remove(root.path(), "1-ab");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(keep.is_err() && update.is_err() && remove.is_err());
        assert!(source.is_file(), "the recording stays where it was");
        assert_eq!(std::fs::read(Library::file(root.path())).unwrap(), b"oops");
    }

    #[test]
    fn a_call_entry_finds_its_recording_by_chat_within_two_minutes() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path();
        let diego = kept(state, "Diego", |c| {
            c.chat = Some("diego@s.whatsapp.net".into());
            c.account = Some("a1".into());
            c.started_at = 1_000;
        });
        let later = kept(state, "Diego otra vez", |c| {
            c.chat = Some("diego@s.whatsapp.net".into());
            c.account = Some("a1".into());
            c.started_at = 1_200;
        });
        kept(state, "Ana", |c| {
            c.chat = Some("ana@s.whatsapp.net".into());
            c.started_at = 1_000;
        });
        let library = Library::load(state);
        let chat = "diego@s.whatsapp.net";
        assert_eq!(library.near(chat, 880).map(|c| &c.id), Some(&diego.id));
        assert_eq!(library.near(chat, 1_090).map(|c| &c.id), Some(&diego.id));
        assert_eq!(library.near(chat, 1_110).map(|c| &c.id), Some(&later.id));
        assert_eq!(library.near(chat, 1_320).map(|c| &c.id), Some(&later.id));
        assert!(library.near(chat, 879).is_none());
        assert!(library.near(chat, 1_321).is_none());
        assert!(library.near("otro@s.whatsapp.net", 1_000).is_none());
        let ids = |calls: Vec<&Call>| calls.into_iter().map(|c| c.id.clone()).collect::<Vec<_>>();
        assert_eq!(
            ids(library.of_chat("a1", chat)),
            [later.id.clone(), diego.id.clone()]
        );
        assert!(library.of_chat("a2", chat).is_empty());
        assert_eq!(library.recorded().len(), 3);
        assert_eq!(library.recorded()[0].id, later.id);
    }

    #[test]
    fn recording_matches_do_not_cross_accounts() {
        let root = tempfile::tempdir().unwrap();
        let chat = "fixture@s.whatsapp.net";
        let first = kept(root.path(), "First", |call| {
            call.chat = Some(chat.into());
            call.account = Some("first".into());
            call.started_at = 1_000;
        });
        let second = kept(root.path(), "Second", |call| {
            call.chat = Some(chat.into());
            call.account = Some("second".into());
            call.started_at = 1_001;
        });
        let library = Library::load(root.path());
        assert_eq!(
            library.near_account("first", chat, 1_001).unwrap().id,
            first.id
        );
        assert_eq!(
            library.near_account("second", chat, 1_000).unwrap().id,
            second.id
        );
        assert!(library.near_account("unknown", chat, 1_000).is_none());
        assert!(library.near_account("first", chat, 2_000).is_none());
        assert!(library.get(&first.id).unwrap().pending_review);
    }

    #[test]
    fn the_views_reread_the_index_only_when_it_changes() {
        let root = tempfile::tempdir().unwrap();
        let call = kept(root.path(), "Uno", |_| {});
        let first = Library::cached(root.path());
        assert!(Arc::ptr_eq(&first, &Library::cached(root.path())));
        Library::update(root.path(), &call.id, |c| c.title = "Dos".into()).unwrap();
        let second = Library::cached(root.path());
        assert!(!Arc::ptr_eq(&first, &second));
        assert_eq!(second.get(&call.id).unwrap().title, "Dos");
    }

    #[test]
    fn changes_from_many_threads_are_all_kept() {
        let root = tempfile::tempdir().unwrap();
        let call = kept(root.path(), "Uno", |_| {});
        std::thread::scope(|scope| {
            for n in 0..8 {
                let (state, id) = (root.path(), &call.id);
                scope.spawn(move || {
                    Library::update(state, id, |c| c.participants.push(n.to_string())).unwrap();
                });
            }
        });
        let call = Library::load(root.path()).get(&call.id).cloned().unwrap();
        assert_eq!(call.participants.len(), 8);
    }
}
