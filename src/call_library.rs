//! Recorded and uploaded calls, kept as files first: a call is saved the
//! moment it ends or is added, before anything reads it, so it can be read
//! again later, by another engine, or by another tool altogether.
//!
//! Each call keeps its audio under `<state>/calls/<id>.<ext>`, every
//! transcript made of it (with the engine that made it), and an analysis
//! (summary, decisions, commitments, tasks, next steps). A call belongs to
//! a project, a lead, or both; Projects and Leads share the one record.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
}

impl Call {
    /// The newest transcript.
    pub fn transcript(&self) -> Option<&Transcript> {
        self.transcripts.last()
    }

    pub fn path(&self, state: &Path) -> PathBuf {
        Library::dir(state).join(&self.file)
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

    pub fn load(state: &Path) -> Self {
        std::fs::read_to_string(Self::file(state))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, state: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(Self::dir(state))?;
        let path = Self::file(state);
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(temporary, path)
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
        };
        let mut library = Self::load(state);
        library.calls.push(call.clone());
        library.save(state).map_err(|e| e.to_string())?;
        Ok(call)
    }

    /// Removes a call and its audio.
    pub fn remove(state: &Path, id: &str) -> std::io::Result<()> {
        let mut library = Self::load(state);
        if let Some(call) = library.get(id) {
            let _ = std::fs::remove_file(call.path(state));
        }
        library.calls.retain(|call| call.id != id);
        library.save(state)
    }

    /// Changes something of one call and saves it, read fresh so another
    /// thread's change is not lost.
    pub fn update(state: &Path, id: &str, change: impl FnOnce(&mut Call)) -> std::io::Result<()> {
        let mut library = Self::load(state);
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
}
