//! Projects: chats grouped by what they are about, for the Projects page.
//!
//! The map lives in `projects.json` in the config folder, shared with the
//! local dashboard (`whatspro-dashboard`), and each project's summary in
//! `digests/<project id>.json` beside it. A project's feed (every message of
//! its chats in one timeline) is read on a thread of its own through a
//! query-only archive connection, the bridge's, so the interface never waits
//! for it.

use std::{
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
pub const FEED_LIMIT: usize = 1500;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub color: String,
    #[serde(default)]
    pub chats: Vec<String>,
}

impl Project {
    pub fn color(&self) -> Color32 {
        parse_color(&self.color).unwrap_or(Color32::from_rgb(0x25, 0xd3, 0x66))
    }
}

/// Where a chat is filed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Bucket {
    Project(String),
    Personal,
    Ignored,
    Unassigned,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectMap {
    #[serde(default)]
    pub projects: Vec<Project>,
    #[serde(default)]
    pub personal: Vec<String>,
    #[serde(default)]
    pub ignored: Vec<String>,
}

impl ProjectMap {
    pub fn file(config: &Path) -> PathBuf {
        config.join("projects.json")
    }

    /// The saved map, or an empty one.
    pub fn load(config: &Path) -> Self {
        std::fs::read_to_string(Self::file(config))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
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

    pub fn bucket_of(&self, chat: &str) -> Bucket {
        if let Some(project) = self
            .projects
            .iter()
            .find(|project| project.chats.iter().any(|id| id == chat))
        {
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

    /// Files `chat` in `bucket`, taking it out of wherever it was.
    pub fn assign(&mut self, chat: &str, bucket: &Bucket) {
        for project in &mut self.projects {
            project.chats.retain(|id| id != chat);
        }
        self.personal.retain(|id| id != chat);
        self.ignored.retain(|id| id != chat);
        match bucket {
            Bucket::Project(id) => {
                if let Some(project) = self.projects.iter_mut().find(|p| &p.id == id) {
                    project.chats.push(chat.to_owned());
                }
            }
            Bucket::Personal => self.personal.push(chat.to_owned()),
            Bucket::Ignored => self.ignored.push(chat.to_owned()),
            Bucket::Unassigned => {}
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
            chats: Vec::new(),
        });
        id
    }

    pub fn remove(&mut self, id: &str) {
        self.projects.retain(|project| project.id != id);
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

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Pending {
    #[serde(default)]
    pub owner: Option<String>,
    pub text: String,
    #[serde(default)]
    pub due: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Waiting {
    #[serde(default)]
    pub who: Option<String>,
    pub text: String,
}

/// An item that may be a plain string or `{ "text": ... }`.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum Item<T> {
    Text(String),
    Full(T),
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Note {
    pub text: String,
}

/// A project's summary, written by whatever summarizes the chats.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Digest {
    #[serde(default)]
    pub updated: Option<i64>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub pending: Vec<Item<Pending>>,
    #[serde(default)]
    pub decisions: Vec<Item<Note>>,
    #[serde(default)]
    pub waiting_on: Vec<Item<Waiting>>,
    #[serde(default)]
    pub links: Vec<Item<Note>>,
}

impl Digest {
    pub fn load(config: &Path, project: &str) -> Option<Self> {
        let path = config.join("digests").join(format!("{project}.json"));
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
    }
}

/// What a feed request asks for: these chats, since then. A newer version of
/// the map or newer activity in the chats makes a new request.
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

/// The feed of one project, loaded off the interface thread.
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
                    let result = load(&archive, &key);
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

fn load(archive: &Path, key: &FeedKey) -> Result<Vec<FeedMessage>, String> {
    let archive = crate::archive::Archive::open_reader(archive).map_err(|error| {
        log::warn!("projects: could not open the archive: {error}");
        "The archive could not be read".to_owned()
    })?;
    let mut messages = Vec::new();
    for chat in &key.chats {
        match archive.bridge_messages(Some(chat), Some(key.since), None, FEED_LIMIT) {
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

    #[test]
    fn assigning_moves_a_chat_between_buckets() {
        let mut map = ProjectMap::default();
        let obra = map.add("Obra Norte");
        assert_eq!(obra, "obra-norte");
        assert_eq!(map.add("Obra Norte"), "obra-norte-2");
        map.assign("a@g.us", &Bucket::Project(obra.clone()));
        assert_eq!(map.bucket_of("a@g.us"), Bucket::Project(obra.clone()));
        map.assign("a@g.us", &Bucket::Personal);
        assert_eq!(map.bucket_of("a@g.us"), Bucket::Personal);
        assert!(map.project(&obra).unwrap().chats.is_empty());
        map.assign("a@g.us", &Bucket::Unassigned);
        assert_eq!(map.bucket_of("a@g.us"), Bucket::Unassigned);
    }

    #[test]
    fn map_round_trips_through_its_file() {
        let root = tempfile::tempdir().unwrap();
        let mut map = ProjectMap::default();
        let id = map.add("JAS");
        map.assign("b@g.us", &Bucket::Project(id));
        map.save(root.path()).unwrap();
        assert_eq!(ProjectMap::load(root.path()), map);
    }

    #[test]
    fn digests_take_strings_or_objects() {
        let digest: Digest = serde_json::from_str(
            r#"{"summary":"x","pending":["a",{"owner":"Vos","text":"b","due":"lunes"}],"decisions":[{"text":"c"}]}"#,
        )
        .unwrap();
        assert_eq!(digest.pending.len(), 2);
        assert!(matches!(&digest.pending[1], Item::Full(p) if p.owner.as_deref() == Some("Vos")));
        assert_eq!(
            parse_color("#21c063"),
            Some(Color32::from_rgb(0x21, 0xc0, 0x63))
        );
    }
}
