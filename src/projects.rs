//! Projects: chats grouped by what they are about, for the Projects page.
//!
//! The map lives in `projects.json` in the config folder, shared with the
//! local dashboard (`whatspro-dashboard`). A project draws on sources: chats,
//! each with a mode. `All` takes the whole chat, `Selected` only the
//! messages picked by hand, and `Smart` the messages the analysis judges to
//! belong (a chat with one person, or one shared with another project, also
//! talks about other things). A chat may be a source of several projects.
//! Picks made by hand always win, and they are what the analysis learns
//! from.
//!
//! The analysis of each project (what to do, whom to follow up, what is
//! slipping, what came up, the summary) is `analysis/<project>.json`,
//! written by `engine`; what the user ticked off is kept beside it.
//!
//! A project's feed (every message of its sources in one timeline) is read
//! on a thread of its own through a query-only archive connection, the
//! bridge's, so the interface never waits for it.

pub mod engine;

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use egui::Color32;
use serde::{Deserialize, Serialize};

pub use crate::archive::BridgeMessage as FeedMessage;

/// Colours a new project takes in turn.
pub const COLORS: [&str; 12] = [
    "#25d366", "#53bdeb", "#a78bfa", "#f59e0b", "#f472b6", "#38bdf8", "#ef4444", "#14b8a6",
    "#eab308", "#fb7185", "#22d3ee", "#c084fc",
];

/// The most messages one feed holds.
pub const FEED_LIMIT: usize = 3000;

/// How much of a chat a project takes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Every message.
    #[default]
    All,
    /// The messages the analysis judges to belong, and the ones picked.
    Smart,
    /// Only the messages picked by hand.
    Selected,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub chat: String,
    #[serde(default)]
    pub mode: Mode,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub color: String,
    #[serde(default)]
    pub sources: Vec<Source>,
    /// The first map's whole chats, moved into `sources` on load.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chats: Vec<String>,
    /// What the project is about, in the owner's words, for the analysis.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub context: String,
    /// Who takes part, and in what role.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<Member>,
}

/// Someone in a project: a WhatsApp id (saved contact or not) or a bare
/// phone number, the name to call them by, and their role.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Member {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub role: String,
}

impl Project {
    pub fn color(&self) -> Color32 {
        parse_color(&self.color).unwrap_or(Color32::from_rgb(0x25, 0xd3, 0x66))
    }

    pub fn source(&self, chat: &str) -> Option<&Source> {
        self.sources.iter().find(|source| source.chat == chat)
    }

    pub fn chat_ids(&self) -> Vec<String> {
        self.sources
            .iter()
            .map(|source| source.chat.clone())
            .collect()
    }
}

/// A message filed in, or kept out of, a project by hand.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pick {
    pub chat: String,
    pub message: String,
    pub project: String,
    pub include: bool,
}

/// Where a chat stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Bucket {
    Project(String),
    Personal,
    Ignored,
    Unassigned,
}

/// Whether a message belongs to a project.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inclusion {
    In,
    Out,
    /// A smart source the analysis has not read this far yet.
    Unsure,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectMap {
    #[serde(default)]
    pub projects: Vec<Project>,
    #[serde(default)]
    pub personal: Vec<String>,
    #[serde(default)]
    pub ignored: Vec<String>,
    #[serde(default)]
    pub picks: Vec<Pick>,
}

impl ProjectMap {
    pub fn file(config: &Path) -> PathBuf {
        config.join("projects.json")
    }

    /// The saved map, or an empty one. Whole chats of the first map become
    /// `All` sources.
    pub fn load(config: &Path) -> Self {
        let mut map: Self = std::fs::read_to_string(Self::file(config))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        for project in &mut map.projects {
            for chat in std::mem::take(&mut project.chats) {
                if project.source(&chat).is_none() {
                    project.sources.push(Source {
                        chat,
                        mode: Mode::All,
                    });
                }
            }
        }
        map
    }

    /// Writes the map through a temporary file, so a reader never sees half.
    pub fn save(&self, config: &Path) -> std::io::Result<()> {
        let path = Self::file(config);
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(temporary, path)
    }

    pub fn project(&self, id: &str) -> Option<&Project> {
        self.projects.iter().find(|project| project.id == id)
    }

    pub fn project_mut(&mut self, id: &str) -> Option<&mut Project> {
        self.projects.iter_mut().find(|project| project.id == id)
    }

    /// The projects that draw on `chat`, with their mode.
    pub fn projects_of(&self, chat: &str) -> Vec<(&Project, Mode)> {
        self.projects
            .iter()
            .filter_map(|project| project.source(chat).map(|source| (project, source.mode)))
            .collect()
    }

    pub fn bucket_of(&self, chat: &str) -> Bucket {
        if let Some((project, _)) = self.projects_of(chat).first() {
            return Bucket::Project(project.id.clone());
        }
        if self.personal.iter().any(|id| id == chat) {
            return Bucket::Personal;
        }
        if self.ignored.iter().any(|id| id == chat) {
            return Bucket::Ignored;
        }
        Bucket::Unassigned
    }

    /// The mode a chat starts with in `project`: a group no other project
    /// uses brings everything; a chat with one person, or one another
    /// project draws on too, talks about other things as well.
    pub fn default_mode(&self, project: &str, chat: &str) -> Mode {
        let group = crate::model::ChatKind::from_id(chat) == crate::model::ChatKind::Group;
        let elsewhere = self
            .projects_of(chat)
            .iter()
            .any(|(other, _)| other.id != project);
        if group && !elsewhere {
            Mode::All
        } else {
            Mode::Smart
        }
    }

    /// Adds `chat` to `project` (taking it out of Personal and Ignored).
    pub fn add_source(&mut self, project: &str, chat: &str, mode: Mode) {
        self.personal.retain(|id| id != chat);
        self.ignored.retain(|id| id != chat);
        if let Some(project) = self.project_mut(project) {
            match project
                .sources
                .iter_mut()
                .find(|source| source.chat == chat)
            {
                Some(source) => source.mode = mode,
                None => project.sources.push(Source {
                    chat: chat.to_owned(),
                    mode,
                }),
            }
        }
    }

    pub fn remove_source(&mut self, project: &str, chat: &str) {
        if let Some(project) = self.project_mut(project) {
            project.sources.retain(|source| source.chat != chat);
        }
        self.picks
            .retain(|pick| !(pick.project == project && pick.chat == chat));
    }

    /// Takes `chat` out of every project and files it as personal or
    /// ignored, or leaves it to file.
    pub fn set_bucket(&mut self, chat: &str, bucket: &Bucket) {
        for project in &mut self.projects {
            project.sources.retain(|source| source.chat != chat);
        }
        self.picks.retain(|pick| pick.chat != chat);
        self.personal.retain(|id| id != chat);
        self.ignored.retain(|id| id != chat);
        match bucket {
            Bucket::Project(id) => {
                let mode = self.default_mode(id, chat);
                self.add_source(id, chat, mode);
            }
            Bucket::Personal => self.personal.push(chat.to_owned()),
            Bucket::Ignored => self.ignored.push(chat.to_owned()),
            Bucket::Unassigned => {}
        }
    }

    /// Files a message in a project, or keeps it out, by hand. A message
    /// filed in a project its chat is not a source of makes the chat a
    /// `Selected` source there.
    pub fn pick(&mut self, chat: &str, message: &str, project: &str, include: bool) {
        self.picks.retain(|pick| {
            !(pick.chat == chat && pick.message == message && pick.project == project)
        });
        self.picks.push(Pick {
            chat: chat.to_owned(),
            message: message.to_owned(),
            project: project.to_owned(),
            include,
        });
        if include
            && self
                .project(project)
                .is_some_and(|p| p.source(chat).is_none())
        {
            self.add_source(project, chat, Mode::Selected);
        }
    }

    pub fn picked(&self, chat: &str, message: &str, project: &str) -> Option<bool> {
        self.picks
            .iter()
            .rev()
            .find(|pick| pick.chat == chat && pick.message == message && pick.project == project)
            .map(|pick| pick.include)
    }

    /// Whether `message` belongs to `project`, given its analysis.
    pub fn inclusion(
        &self,
        project: &Project,
        message: &FeedMessage,
        analysis: Option<&Analysis>,
    ) -> Inclusion {
        if let Some(include) = self.picked(&message.chat, &message.id, &project.id) {
            return if include {
                Inclusion::In
            } else {
                Inclusion::Out
            };
        }
        match project.source(&message.chat).map(|source| source.mode) {
            None | Some(Mode::Selected) => Inclusion::Out,
            Some(Mode::All) => Inclusion::In,
            Some(Mode::Smart) => match analysis {
                Some(analysis) if analysis.read_through(&message.chat) >= message.timestamp => {
                    if analysis.is_relevant(&message.chat, &message.id) {
                        Inclusion::In
                    } else {
                        Inclusion::Out
                    }
                }
                _ => Inclusion::Unsure,
            },
        }
    }

    /// Adds a project named `name` and returns its id.
    pub fn add(&mut self, name: &str) -> String {
        let base: String = name
            .trim()
            .to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .split('-')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        let base = if base.is_empty() {
            "proyecto".to_owned()
        } else {
            base
        };
        let mut id = base.clone();
        let mut n = 2;
        while self.project(&id).is_some() {
            id = format!("{base}-{n}");
            n += 1;
        }
        self.projects.push(Project {
            id: id.clone(),
            name: name.trim().to_owned(),
            color: COLORS[self.projects.len() % COLORS.len()].to_owned(),
            sources: Vec::new(),
            chats: Vec::new(),
            context: String::new(),
            members: Vec::new(),
        });
        id
    }

    pub fn remove(&mut self, id: &str) {
        self.projects.retain(|project| project.id != id);
        self.picks.retain(|pick| pick.project != id);
    }
}

/// `#rrggbb` as a colour.
pub fn parse_color(hex: &str) -> Option<Color32> {
    let hex = hex.trim().strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    Some(Color32::from_rgb(
        (value >> 16) as u8,
        (value >> 8) as u8,
        value as u8,
    ))
}

/// One thing the analysis found: something to do, someone to follow up,
/// something slipping, a need, or a message worth reading.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    /// Stable across runs, so a ticked-off item stays ticked off.
    pub id: String,
    pub text: String,
    /// Who it waits on, or who asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub who: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// When it was said, Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due: Option<String>,
}

/// A project's analysis.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Analysis {
    #[serde(default)]
    pub updated: i64,
    /// Summaries by period: "today", "yesterday", "week".
    #[serde(default)]
    pub summaries: HashMap<String, String>,
    /// What we committed to.
    #[serde(default)]
    pub todo: Vec<Finding>,
    /// What we wait on: questions with no answer, promises not kept.
    #[serde(default)]
    pub follow_up: Vec<Finding>,
    /// Asks nobody answered, things dropped.
    #[serde(default)]
    pub leaks: Vec<Finding>,
    /// Needs that came up.
    #[serde(default)]
    pub needs: Vec<Finding>,
    /// Messages worth reading.
    #[serde(default)]
    pub highlights: Vec<Finding>,
    /// For smart sources: the messages judged to belong, by chat.
    #[serde(default)]
    pub relevant: HashMap<String, Vec<String>>,
    /// For smart sources: how far each chat has been read, Unix seconds.
    #[serde(default)]
    pub read: HashMap<String, i64>,
}

impl Analysis {
    pub fn file(config: &Path, project: &str) -> PathBuf {
        config.join("analysis").join(format!("{project}.json"))
    }

    pub fn load(config: &Path, project: &str) -> Option<Self> {
        serde_json::from_str(&std::fs::read_to_string(Self::file(config, project)).ok()?).ok()
    }

    pub fn save(&self, config: &Path, project: &str) -> std::io::Result<()> {
        let path = Self::file(config, project);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(temporary, path)
    }

    pub fn read_through(&self, chat: &str) -> i64 {
        self.read.get(chat).copied().unwrap_or(0)
    }

    pub fn is_relevant(&self, chat: &str, message: &str) -> bool {
        self.relevant
            .get(chat)
            .is_some_and(|ids| ids.iter().any(|id| id == message))
    }
}

/// What the user did with the analysis' findings: ticked off, dismissed,
/// and judged good or bad advice. The texts go back to the analysis, so a
/// closed item does not come back and bad advice is not repeated.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Ticks {
    #[serde(default)]
    pub done: HashSet<String>,
    #[serde(default)]
    pub dismissed: HashSet<String>,
    #[serde(default)]
    pub feedback: Vec<Feedback>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Done,
    Dismissed,
    Good,
    Bad,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Feedback {
    pub id: String,
    pub text: String,
    pub verdict: Verdict,
    pub at: i64,
}

impl Ticks {
    fn file(config: &Path, project: &str) -> PathBuf {
        config
            .join("analysis")
            .join(format!("{project}.ticks.json"))
    }

    pub fn load(config: &Path, project: &str) -> Self {
        std::fs::read_to_string(Self::file(config, project))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, config: &Path, project: &str) -> std::io::Result<()> {
        let path = Self::file(config, project);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)
    }

    pub fn open(&self, id: &str) -> bool {
        !self.done.contains(id) && !self.dismissed.contains(id)
    }

    /// Records what the user made of a finding.
    pub fn judge(&mut self, finding: &Finding, verdict: Verdict) {
        match verdict {
            Verdict::Done => {
                self.done.insert(finding.id.clone());
            }
            Verdict::Dismissed => {
                self.dismissed.insert(finding.id.clone());
            }
            Verdict::Good | Verdict::Bad => {}
        }
        self.feedback
            .retain(|f| !(f.id == finding.id && f.verdict == verdict));
        self.feedback.push(Feedback {
            id: finding.id.clone(),
            text: finding.text.clone(),
            verdict,
            at: jiff::Timestamp::now().as_second(),
        });
        // The newest 200 are plenty to learn from.
        if self.feedback.len() > 200 {
            self.feedback.drain(..self.feedback.len() - 200);
        }
    }

    pub fn verdict(&self, id: &str) -> Option<Verdict> {
        self.feedback
            .iter()
            .rev()
            .find(|f| f.id == id && matches!(f.verdict, Verdict::Good | Verdict::Bad))
            .map(|f| f.verdict)
    }
}

/// A note the owner wrote or said about the project: what was done, a
/// call, a commitment. The analysis reads them to close items and add new
/// ones.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub at: i64,
    pub text: String,
    #[serde(default)]
    pub spoken: bool,
}

/// A project's notes, oldest first.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Journal {
    #[serde(default)]
    pub notes: Vec<Note>,
}

impl Journal {
    fn file(config: &Path, project: &str) -> PathBuf {
        config
            .join("analysis")
            .join(format!("{project}.journal.json"))
    }

    pub fn load(config: &Path, project: &str) -> Self {
        std::fs::read_to_string(Self::file(config, project))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, config: &Path, project: &str) -> std::io::Result<()> {
        let path = Self::file(config, project);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)
    }

    pub fn add(&mut self, text: &str, spoken: bool) {
        self.notes.push(Note {
            at: jiff::Timestamp::now().as_second(),
            text: text.trim().to_owned(),
            spoken,
        });
    }
}

/// Mono samples as a 16-bit WAV file, for transcribing a spoken note.
pub fn wav(samples: &[f32], rate: u32) -> Vec<u8> {
    let data = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    for sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

/// What a feed request asks for: these chats, since then. Newer activity in
/// the chats makes a new request.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FeedKey {
    pub chats: Vec<String>,
    pub since: i64,
    pub activity: i64,
}

#[derive(Default)]
pub struct FeedState {
    pub key: Option<FeedKey>,
    pub loading: bool,
    pub messages: Arc<Vec<FeedMessage>>,
    pub error: Option<String>,
}

/// The messages of a project's sources, loaded off the interface thread.
#[derive(Clone, Default)]
pub struct Feed(Arc<Mutex<FeedState>>);

impl Feed {
    /// The messages for `key`, asking for them when they are not the ones
    /// held. Returns what is held meanwhile and whether it is loading.
    pub fn get(
        &self,
        ctx: &egui::Context,
        archive: &Path,
        key: FeedKey,
    ) -> (Arc<Vec<FeedMessage>>, bool, Option<String>) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if state.key.as_ref() != Some(&key) && !state.loading {
            // A different project or range drops what was held at once; a
            // refresh of the same one keeps it on screen until the new one.
            let same_chats = state
                .key
                .as_ref()
                .is_some_and(|held| held.chats == key.chats && held.since == key.since);
            if !same_chats {
                state.messages = Arc::new(Vec::new());
            }
            state.key = Some(key.clone());
            state.loading = true;
            state.error = None;
            let feed = self.0.clone();
            let archive = archive.to_owned();
            let ctx = ctx.clone();
            let spawned = std::thread::Builder::new()
                .name("project-feed".into())
                .spawn(move || {
                    let result = read_messages(&archive, &key.chats, key.since);
                    let mut state = feed.lock().unwrap_or_else(|p| p.into_inner());
                    state.loading = false;
                    if state.key.as_ref() == Some(&key) {
                        match result {
                            Ok(messages) => state.messages = Arc::new(messages),
                            Err(error) => state.error = Some(error),
                        }
                    }
                    ctx.request_repaint();
                });
            if spawned.is_err() {
                state.loading = false;
            }
        }
        (state.messages.clone(), state.loading, state.error.clone())
    }
}

/// The messages of `chats` since `since`, in time order.
pub fn read_messages(
    archive: &Path,
    chats: &[String],
    since: i64,
) -> Result<Vec<FeedMessage>, String> {
    let archive = crate::archive::Archive::open_reader(archive).map_err(|error| {
        log::warn!("projects: could not open the archive: {error}");
        "The archive could not be read".to_owned()
    })?;
    let mut messages = Vec::new();
    for chat in chats {
        match archive.bridge_messages(Some(chat), Some(since), None, FEED_LIMIT) {
            Ok(mut batch) => messages.append(&mut batch),
            Err(error) => log::warn!("projects: a chat's messages could not be read: {error}"),
        }
    }
    messages.sort_by_key(|message| message.timestamp);
    if messages.len() > FEED_LIMIT {
        messages.drain(..messages.len() - FEED_LIMIT);
    }
    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(chat: &str, id: &str, timestamp: i64) -> FeedMessage {
        FeedMessage {
            chat: chat.into(),
            chat_name: chat.into(),
            id: id.into(),
            sender: chat.into(),
            sender_name: None,
            from_me: false,
            timestamp,
            kind: "text".into(),
            text: "x".into(),
            reply_to: None,
            edited: false,
            transcript: None,
        }
    }

    #[test]
    fn a_chat_can_feed_several_projects_with_its_own_mode() {
        let mut map = ProjectMap::default();
        let obra = map.add("Obra Norte");
        assert_eq!(obra, "obra-norte");
        assert_eq!(map.add("Obra Norte"), "obra-norte-2");
        let jas = map.add("JAS");
        // A group nobody else uses brings everything; once another project
        // draws on it, it talks about other things too.
        assert_eq!(map.default_mode(&obra, "g@g.us"), Mode::All);
        map.add_source(&obra, "g@g.us", Mode::All);
        assert_eq!(map.default_mode(&jas, "g@g.us"), Mode::Smart);
        assert_eq!(map.default_mode(&jas, "p@s.whatsapp.net"), Mode::Smart);
        map.add_source(&jas, "g@g.us", Mode::Smart);
        assert_eq!(map.projects_of("g@g.us").len(), 2);
        map.set_bucket("g@g.us", &Bucket::Personal);
        assert!(map.projects_of("g@g.us").is_empty());
        assert_eq!(map.bucket_of("g@g.us"), Bucket::Personal);
    }

    #[test]
    fn picks_win_over_modes_and_the_analysis() {
        let mut map = ProjectMap::default();
        let jas = map.add("JAS");
        map.add_source(&jas, "p@s.whatsapp.net", Mode::Smart);
        let mut analysis = Analysis::default();
        let project = map.project(&jas).unwrap().clone();
        let early = message("p@s.whatsapp.net", "a", 100);
        assert_eq!(map.inclusion(&project, &early, None), Inclusion::Unsure);
        analysis.read.insert("p@s.whatsapp.net".into(), 200);
        analysis
            .relevant
            .insert("p@s.whatsapp.net".into(), vec!["a".into()]);
        assert_eq!(
            map.inclusion(&project, &early, Some(&analysis)),
            Inclusion::In
        );
        let other = message("p@s.whatsapp.net", "b", 150);
        assert_eq!(
            map.inclusion(&project, &other, Some(&analysis)),
            Inclusion::Out
        );
        map.pick("p@s.whatsapp.net", "b", &jas, true);
        map.pick("p@s.whatsapp.net", "a", &jas, false);
        let project = map.project(&jas).unwrap().clone();
        assert_eq!(
            map.inclusion(&project, &other, Some(&analysis)),
            Inclusion::In
        );
        assert_eq!(
            map.inclusion(&project, &early, Some(&analysis)),
            Inclusion::Out
        );
    }

    #[test]
    fn picking_into_a_project_adds_its_chat_as_selected() {
        let mut map = ProjectMap::default();
        let obra = map.add("Obra");
        map.pick("x@s.whatsapp.net", "m1", &obra, true);
        assert_eq!(
            map.project(&obra)
                .unwrap()
                .source("x@s.whatsapp.net")
                .map(|s| s.mode),
            Some(Mode::Selected)
        );
    }

    #[test]
    fn judging_a_finding_closes_it_and_keeps_why() {
        let finding = Finding {
            id: "todo:a@g.us:m1".into(),
            text: "Mandar la cotización".into(),
            ..Finding::default()
        };
        let mut ticks = Ticks::default();
        assert!(ticks.open(&finding.id));
        ticks.judge(&finding, Verdict::Bad);
        assert!(ticks.open(&finding.id), "bad advice stays until closed");
        assert_eq!(ticks.verdict(&finding.id), Some(Verdict::Bad));
        ticks.judge(&finding, Verdict::Done);
        assert!(!ticks.open(&finding.id));
        assert_eq!(ticks.feedback.len(), 2);
        assert_eq!(ticks.feedback[1].text, "Mandar la cotización");
    }

    #[test]
    fn notes_and_spoken_notes_round_trip() {
        let root = tempfile::tempdir().unwrap();
        let mut journal = Journal::default();
        journal.add("  Llamé a Diego ", true);
        journal.save(root.path(), "jas").unwrap();
        let back = Journal::load(root.path(), "jas");
        assert_eq!(back.notes[0].text, "Llamé a Diego");
        assert!(back.notes[0].spoken);
        let wav = wav(&[0.0, 1.0], 48_000);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(wav.len(), 44 + 4);
    }

    #[test]
    fn the_first_maps_chats_become_whole_sources() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            ProjectMap::file(root.path()),
            r##"{"projects":[{"id":"jas","name":"JAS","color":"#25d366","chats":["a@g.us"]}],"personal":[],"ignored":[]}"##,
        )
        .unwrap();
        let map = ProjectMap::load(root.path());
        let project = map.project("jas").unwrap();
        assert!(project.chats.is_empty());
        assert_eq!(project.source("a@g.us").map(|s| s.mode), Some(Mode::All));
        map.save(root.path()).unwrap();
        assert_eq!(ProjectMap::load(root.path()), map);
        assert_eq!(
            parse_color("#21c063"),
            Some(Color32::from_rgb(0x21, 0xc0, 0x63))
        );
    }
}
