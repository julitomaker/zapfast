//! A Model Context Protocol server over stdio, so Claude Desktop (or any MCP
//! client) can read the chats, projects, calls and leads: `<app binary> mcp`.
//!
//! It is a read-only server: chats come from the local bridge (`crate::bridge`,
//! which needs Whats Pro running and reads `bridge.json`), and projects, calls
//! and leads are read straight from disk. Permissions live in `mcp.json` in
//! the config folder and are read on every request so changes apply at once.
//! Locked chats and private calls are never served.
//!
//! [`connect_claude_desktop`] adds this server to Claude Desktop's config.

use std::{
    collections::HashSet,
    io::{BufRead, Write},
    path::{Path, PathBuf},
};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const PROTOCOL: &str = "2025-06-18";
const SERVER_NAME: &str = "whats-pro";

fn default_true() -> bool {
    true
}

/// Which group of data a tool reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Chats,
    Projects,
    Calls,
    Leads,
    /// Creating and changing projects, notes, tasks and leads.
    Write,
    /// Writing WhatsApp messages that wait in the chat's box for the user.
    Messages,
    /// Sending WhatsApp messages without the user pressing send.
    Send,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chats => "chats",
            Self::Projects => "projects",
            Self::Calls => "calls",
            Self::Leads => "leads",
            Self::Write => "write",
            Self::Messages => "messages",
            Self::Send => "send",
        }
    }

    fn settings_label(self) -> &'static str {
        match self {
            Self::Chats => "Chats",
            Self::Projects => "Proyectos y notas",
            Self::Calls => "Llamadas (resúmenes)",
            Self::Leads => "Leads",
            Self::Write => "Crear y editar",
            Self::Messages => "Escribir mensajes",
            Self::Send => "Enviar mensajes",
        }
    }
}

const LOG_LIMIT: usize = 1000;

/// One MCP tool call recorded in `mcp-log.jsonl` in the config folder.
/// Never records tool arguments, results, or message bodies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub at: i64,
    pub tool: String,
    pub scope: String,
    pub client: String,
}

/// Path to `mcp-log.jsonl` inside the config folder.
pub fn log_file(config_dir: &Path) -> PathBuf {
    config_dir.join("mcp-log.jsonl")
}

fn append_log(config_dir: &Path, entry: &LogEntry) -> std::io::Result<()> {
    std::fs::create_dir_all(config_dir)?;
    let path = log_file(config_dir);
    let mut lines: Vec<String> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect();
    lines.push(serde_json::to_string(entry)?);
    let start = lines.len().saturating_sub(LOG_LIMIT);
    let mut text = lines[start..].join("\n");
    text.push('\n');
    let temporary = path.with_extension("jsonl.tmp");
    std::fs::write(&temporary, text)?;
    std::fs::rename(temporary, path)
}

/// Returns the most recent MCP access log entries (newest first), up to `limit`.
pub fn recent_log(config_dir: &Path, limit: usize) -> Vec<LogEntry> {
    std::fs::read_to_string(log_file(config_dir))
        .unwrap_or_default()
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<LogEntry>(line).ok())
        .take(limit)
        .collect()
}

/// What MCP clients are allowed to read, stored in `mcp.json` in the config
/// folder and read on each request so changes apply live.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permissions {
    /// Master switch for all MCP access.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// WhatsApp chats and messages through the local bridge.
    #[serde(default = "default_true")]
    pub chats: bool,
    /// Projects, analysis findings and notes.
    #[serde(default = "default_true")]
    pub projects: bool,
    /// Recorded and uploaded call summaries, decisions, commitments and tasks.
    #[serde(default = "default_true")]
    pub calls: bool,
    /// Full call transcripts (off by default).
    #[serde(default)]
    pub call_transcripts: bool,
    /// Sales leads, stages, next actions and history.
    #[serde(default = "default_true")]
    pub leads: bool,
    /// Never serve chats marked as Personal or Ignored in `projects.json`.
    #[serde(default = "default_true")]
    pub exclude_personal_and_ignored: bool,
    /// Creating and changing projects, notes, tasks and leads. Every change
    /// says who made it and can be undone from the project's history.
    #[serde(default = "default_true")]
    pub write: bool,
    /// Writing WhatsApp messages left in the chat's box for the user to send.
    #[serde(default = "default_true")]
    pub messages: bool,
    /// Sending WhatsApp messages directly, to chats that already had a
    /// conversation and at a person's pace (off by default).
    #[serde(default)]
    pub send: bool,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            enabled: true,
            chats: true,
            projects: true,
            calls: true,
            call_transcripts: false,
            leads: true,
            exclude_personal_and_ignored: true,
            write: true,
            messages: true,
            send: false,
        }
    }
}

impl Permissions {
    pub fn file(config_dir: &Path) -> PathBuf {
        config_dir.join("mcp.json")
    }

    /// Reads `mcp.json`, or returns safe defaults when missing or unreadable.
    pub fn load(config_dir: &Path) -> Self {
        std::fs::read_to_string(Self::file(config_dir))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Writes `mcp.json` atomically through a temporary file.
    pub fn save(&self, config_dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(config_dir)?;
        let path = Self::file(config_dir);
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(temporary, path)
    }

    /// Whether `scope` is enabled.
    pub fn allows(&self, scope: Scope) -> bool {
        self.enabled
            && match scope {
                Scope::Chats => self.chats,
                Scope::Projects => self.projects,
                Scope::Calls => self.calls,
                Scope::Leads => self.leads,
                Scope::Write => self.write,
                Scope::Messages => self.messages || self.send,
                Scope::Send => self.send,
            }
    }

    /// Clear error explaining how to turn on a disabled scope in the app.
    pub fn check(&self, scope: Scope) -> Result<(), String> {
        if self.allows(scope) {
            return Ok(());
        }
        if !self.enabled {
            return Err(
                "MCP access is turned off. Enable \"Permitir que otras IA lean Whats Pro\" in Whats Pro > Ajustes > Conectar con IA."
                    .to_owned(),
            );
        }
        Err(format!(
            "Access to \"{}\" ({}) is turned off. Enable it in Whats Pro > Ajustes > Conectar con IA.",
            scope.settings_label(),
            scope.as_str(),
        ))
    }
}

/// The scope a tool belongs to.
fn tool_scope(name: &str) -> Option<Scope> {
    match name {
        "list_chats" | "read_chat" | "recent_activity" | "search_messages" | "list_media"
        | "get_media" | "transcribe_chat" => Some(Scope::Chats),
        "list_projects" | "project_overview" | "project_notes" => Some(Scope::Projects),
        "list_calls" | "read_call" => Some(Scope::Calls),
        "list_leads" | "read_lead" => Some(Scope::Leads),
        "create_project" | "update_project" | "add_note" | "add_task" | "resolve_item"
        | "reopen_item" | "run_analysis" => Some(Scope::Write),
        _ if crate::leads::actions::handles(name) => Some(Scope::Write),
        "draft_message" => Some(Scope::Messages),
        "send_message" => Some(Scope::Send),
        _ => None,
    }
}

/// All tools with their JSON schemas.
fn all_tools() -> Vec<Value> {
    vec![
        json!({
            "name": "list_media",
            "description": "List the photos, videos, voice messages, audio files, documents and stickers of one WhatsApp chat, newest last, with each message's id for get_media.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat": {"type": "string", "description": "Chat name, part of it, or id."},
                    "kind": {"type": "string", "enum": ["all", "image", "video", "audio", "document", "sticker"], "description": "Only this kind (default all)."},
                    "since_hours": {"type": "number", "description": "Only from the last N hours."},
                    "limit": {"type": "number", "description": "At most this many (default 50)."}
                },
                "required": ["chat"]
            },
            "annotations": {"readOnlyHint": true}
        }),
        json!({
            "name": "transcribe_chat",
            "description": "Start reading every voice message, audio, video and picture of a chat that has no text yet (speech transcribed, picture text read), with the engines chosen in Whats Pro. Returns at once with how many were started; they appear in read_chat as [transcript: …] as each one finishes (local Whisper does about a minute of audio in a few seconds).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat": {"type": "string", "description": "Chat name, part of it, or id."},
                    "since_hours": {"type": "number", "description": "Only from the last N hours (default: everything)."}
                },
                "required": ["chat"]
            },
            "annotations": {"readOnlyHint": false}
        }),
        json!({
            "name": "get_media",
            "description": "Open the attachment of one WhatsApp message (id from read_chat or list_media). Photos and stickers come back as images you can see. Voice messages, audio and videos come back transcribed: if they never were, Whats Pro transcribes them now with the engine chosen in its Settings (this can take up to a couple of minutes for long ones) and keeps the text. Documents come back as their type. Whats Pro downloads the file from WhatsApp if it was never opened.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat": {"type": "string", "description": "Chat name, part of it, or id."},
                    "message_id": {"type": "string", "description": "The message id."}
                },
                "required": ["chat", "message_id"]
            },
            "annotations": {"readOnlyHint": true}
        }),
        json!({
            "name": "list_chats",
            "description": "List WhatsApp chats (groups and people) from Whats Pro, most recent activity first, with unread counts. Use it to find a chat's exact name or id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": ["all", "group", "direct"], "description": "Only groups, only people, or all (default)."},
                    "active_within_days": {"type": "number", "description": "Only chats with activity in this many days."},
                    "query": {"type": "string", "description": "Only chats whose name contains this text."},
                    "limit": {"type": "number", "description": "At most this many chats (default 100)."}
                }
            },
            "annotations": {"readOnlyHint": true}
        }),
        json!({
            "name": "read_chat",
            "description": "Read the messages of one WhatsApp chat, oldest first. `chat` is the chat's name (or part of it) or its id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat": {"type": "string", "description": "Chat name, part of it, or id."},
                    "since_hours": {"type": "number", "description": "Only messages from the last N hours. Without it, the latest `limit` messages."},
                    "limit": {"type": "number", "description": "At most this many messages (default 200, max 2000)."},
                    "project": {"type": "string", "description": "Only the messages that belong to this project (name or id), by the chat's mode in its sources (a smart chat gives only the project's messages), with people named as the project's members sheet says."}
                },
                "required": ["chat"]
            },
            "annotations": {"readOnlyHint": true}
        }),
        json!({
            "name": "recent_activity",
            "description": "Everything said across all WhatsApp chats in the last hours, grouped by chat. Good for daily summaries, pending items and who owes what.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "hours": {"type": "number", "description": "How far back (default 24)."},
                    "kind": {"type": "string", "enum": ["all", "group", "direct"]},
                    "limit": {"type": "number", "description": "At most this many messages (default 1500, max 5000)."},
                    "project": {"type": "string", "description": "Only this project's chats and messages (name or id), by each source's mode, with people named as its members sheet says."}
                }
            },
            "annotations": {"readOnlyHint": true}
        }),
        json!({
            "name": "search_messages",
            "description": "Search WhatsApp messages by text, caption, file name or transcript, newest first. Narrow it by chat, time, who said it and kind; with only filters (no query) it lists what matches them, e.g. every PDF of a chat this month.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Words to look for (optional when a filter is given)."},
                    "chat": {"type": "string", "description": "Only in this chat (name or id)."},
                    "since_hours": {"type": "number", "description": "Only from the last N hours."},
                    "since": {"type": "string", "description": "Only from this date on (YYYY-MM-DD)."},
                    "until": {"type": "string", "description": "Only before this date (YYYY-MM-DD)."},
                    "from": {"type": "string", "description": "Only from this person: part of their name or number, or \"me\"."},
                    "kind": {"type": "string", "enum": ["text", "image", "video", "audio", "document", "sticker"], "description": "Only this kind of message."},
                    "limit": {"type": "number", "description": "Default 50, max 500."}
                }
            },
            "annotations": {"readOnlyHint": true}
        }),
        json!({
            "name": "list_projects",
            "description": "List all projects in Whats Pro with their id, name, source chat count, last analysis time, and open item counts per section (to-dos, follow-ups, slipping, needs, highlights).",
            "inputSchema": {
                "type": "object",
                "properties": {}
            },
            "annotations": {"readOnlyHint": true}
        }),
        json!({
            "name": "project_overview",
            "description": "Read a project's full panel by name or id: summaries (today, yesterday, week), open to-dos, follow-ups, slipping items, needs, highlights (excluding done or dismissed items), members and context.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": {"type": "string", "description": "Project name, part of it, or id."}
                },
                "required": ["project"]
            },
            "annotations": {"readOnlyHint": true}
        }),
        json!({
            "name": "project_notes",
            "description": "Read the journal notes and memos of a project, optionally limited to the last N days.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": {"type": "string", "description": "Project name, part of it, or id."},
                    "days": {"type": "number", "description": "Only notes from the last N days."}
                },
                "required": ["project"]
            },
            "annotations": {"readOnlyHint": true}
        }),
        json!({
            "name": "list_calls",
            "description": "List recorded and uploaded calls from Whats Pro (newest first) with title, date, duration, project, lead and summary. Private calls are never included.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": {"type": "string", "description": "Only calls in this project (name or id)."},
                    "days": {"type": "number", "description": "Only calls from the last N days."},
                    "limit": {"type": "number", "description": "At most this many calls (default 50, max 500)."}
                }
            },
            "annotations": {"readOnlyHint": true}
        }),
        json!({
            "name": "read_call",
            "description": "Read one call by id or title: date, duration, project, lead, summary, decisions, commitments, tasks, next steps, and (when the call_transcripts permission is enabled) the full transcript.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "call": {"type": "string", "description": "Call id, title, or part of its title."}
                },
                "required": ["call"]
            },
            "annotations": {"readOnlyHint": true}
        }),
        json!({
            "name": "list_leads",
            "description": "List sales leads from Whats Pro with stage, amount, next action and due date, optionally filtered by project or stage.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": {"type": "string", "description": "Only leads in this project (name or id)."},
                    "stage": {"type": "string", "enum": ["new", "contacted", "quoted", "customer", "lost"], "description": "Only leads in this stage."}
                }
            },
            "annotations": {"readOnlyHint": true}
        }),
        json!({
            "name": "read_lead",
            "description": "Read one sales lead by id, name or phone: stage, amount, next action with due date, sales, and full history entries.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "lead": {"type": "string", "description": "Lead id, name, part of name, or phone number."}
                },
                "required": ["lead"]
            },
            "annotations": {"readOnlyHint": true}
        }),
        json!({
            "name": "create_project",
            "description": "Create a project in Whats Pro, for example from the chats with a possible client: its name, what it is about (context), the chats it follows (sources, by chat id from list_chats) and the people in it (members). Returns the new project's id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "context": {"type": "string", "description": "What the project is about, in plain words; the analysis reads it."},
                    "sources": {"type": "array", "description": "Chats to follow. mode: all (the whole chat, for chats only about this project), smart (the analysis keeps the project's messages, for chats that also talk about other things), selected (only messages picked by hand). Without mode, Whats Pro picks by the kind of chat.", "items": {"type": "object", "properties": {"chat": {"type": "string", "description": "Chat id from list_chats."}, "mode": {"type": "string", "enum": ["all", "smart", "selected"]}}, "required": ["chat"]}},
                    "members": {"type": "array", "items": {"type": "object", "properties": {"name": {"type": "string"}, "id": {"type": "string", "description": "WhatsApp id or phone number."}, "role": {"type": "string"}, "about": {"type": "string", "description": "Who they are and how to deal with them."}, "aliases": {"type": "array", "items": {"type": "string"}, "description": "Other names they show up as: the shop account they write from, their saved name in another chat."}, "chats": {"type": "array", "items": {"type": "string"}, "description": "Other WhatsApp ids or numbers they write from."}}, "required": ["name"]}},
                    "color": {"type": "string", "description": "Hex color, optional."}
                },
                "required": ["name"]
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": false}
        }),
        json!({
            "name": "update_project",
            "description": "Change a project: its name, context, color, add or remove chats (sources), and add, update or remove members.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": {"type": "string", "description": "Project name or id."},
                    "name": {"type": "string"},
                    "context": {"type": "string"},
                    "color": {"type": "string"},
                    "add_sources": {"type": "array", "items": {"type": "object", "properties": {"chat": {"type": "string"}, "mode": {"type": "string", "enum": ["all", "smart", "selected"]}}, "required": ["chat"]}},
                    "remove_sources": {"type": "array", "items": {"type": "string", "description": "Chat id."}},
                    "members": {"type": "array", "description": "Added, or updated when the id or name matches (aliases and chats are added to theirs).", "items": {"type": "object", "properties": {"name": {"type": "string"}, "id": {"type": "string"}, "role": {"type": "string"}, "about": {"type": "string"}, "aliases": {"type": "array", "items": {"type": "string"}}, "chats": {"type": "array", "items": {"type": "string"}}}, "required": ["name"]}},
                    "remove_members": {"type": "array", "items": {"type": "string", "description": "Member id or name."}}
                },
                "required": ["project"]
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": false}
        }),
        json!({
            "name": "draft_message",
            "description": "Write a WhatsApp message and leave it in the chat's text box in Whats Pro, so the user reads it and presses send. Only for chats where someone already wrote to the user. Read the chat first (read_chat) and write as the user would, in their language and tone.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat": {"type": "string", "description": "Chat name, part of it, or id."},
                    "text": {"type": "string", "description": "The message, as the user would write it."}
                },
                "required": ["chat", "text"]
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": false}
        }),
        json!({
            "name": "send_message",
            "description": "Send a WhatsApp message from the user's account, as the user. Only to chats where someone already wrote to the user, one message every 20 seconds, at most 30 an hour, and the same text to at most 3 chats a day: WhatsApp bans accounts that behave like bulk senders. Read the chat first (read_chat), write as the user would, personalize every message, and when unsure what the user wants, use draft_message instead.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat": {"type": "string", "description": "Chat name, part of it, or id."},
                    "text": {"type": "string", "description": "The message, as the user would write it."}
                },
                "required": ["chat", "text"]
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "openWorldHint": true}
        }),
        json!({
            "name": "add_note",
            "description": "Add a note to a project's Notes: something that happened (log) or something to remember (memo). The analysis reads notes, and a note that says something was done can close it on the panel.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": {"type": "string"},
                    "text": {"type": "string"},
                    "kind": {"type": "string", "enum": ["log", "memo"]}
                },
                "required": ["project", "text"]
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": false}
        }),
        json!({
            "name": "add_task",
            "description": "Add a task to a project's panel (To do). It stays until it is closed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": {"type": "string"},
                    "text": {"type": "string"},
                    "who": {"type": "string", "description": "Who does it or who it waits on."},
                    "due": {"type": "string", "description": "When, as YYYY-MM-DD or in words."}
                },
                "required": ["project", "text"]
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": false}
        }),
        json!({
            "name": "resolve_item",
            "description": "Close an item of a project's panel (a to-do, follow-up, slipping item, need or task) because it was done, saying how. Set dismiss to true when it no longer applies instead. Use the item's id from project_overview, or words of its text. It can be undone.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": {"type": "string"},
                    "item": {"type": "string", "description": "Item id, or words of its text."},
                    "how": {"type": "string", "description": "How it was solved, or why it is dismissed."},
                    "dismiss": {"type": "boolean"}
                },
                "required": ["project", "item"]
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": false}
        }),
        json!({
            "name": "reopen_item",
            "description": "Put a closed item back on a project's panel.",
            "inputSchema": {
                "type": "object",
                "properties": {"project": {"type": "string"}, "item": {"type": "string"}},
                "required": ["project", "item"]
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": false}
        }),
        json!({
            "name": "run_analysis",
            "description": "Run the AI analysis of a project now: it rereads its chats, notes, calls and leads, updates the panel, and closes items that were done. It takes about a minute; then read project_overview.",
            "inputSchema": {
                "type": "object",
                "properties": {"project": {"type": "string"}},
                "required": ["project"]
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": false}
        }),
    ]
    .into_iter()
    .chain(crate::leads::actions::tools())
    .collect()
}

/// The tools currently enabled by `permissions`.
fn tools(permissions: &Permissions) -> Value {
    let enabled: Vec<Value> = all_tools()
        .into_iter()
        .filter(|tool| {
            tool["name"]
                .as_str()
                .and_then(tool_scope)
                .is_some_and(|scope| permissions.allows(scope))
        })
        .collect();
    Value::Array(enabled)
}

/// The bridge's address and token, plus the folders where projects, calls and
/// leads live. Permissions are read on each request so changes apply live.
struct Bridge {
    config_dir: PathBuf,
    state_dir: PathBuf,
    client: std::sync::Mutex<String>,
}

impl Bridge {
    fn new(config_dir: impl Into<PathBuf>, state_dir: impl Into<PathBuf>) -> Self {
        Self {
            config_dir: config_dir.into(),
            state_dir: state_dir.into(),
            client: std::sync::Mutex::new("unknown".to_owned()),
        }
    }

    fn set_client(&self, name: Option<&str>) {
        let name = name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown");
        if let Ok(mut client) = self.client.lock() {
            *client = name.to_owned();
        }
    }

    fn client_name(&self) -> String {
        self.client
            .lock()
            .map(|c| c.clone())
            .unwrap_or_else(|_| "unknown".to_owned())
    }

    fn permissions(&self) -> Permissions {
        Permissions::load(&self.config_dir)
    }

    /// Chat ids marked as Personal or Ignored in `projects.json`, when the
    /// permission excludes them.
    fn excluded_chats(&self, permissions: &Permissions) -> HashSet<String> {
        if !permissions.exclude_personal_and_ignored {
            return HashSet::new();
        }
        let map = crate::projects::ProjectMap::load(&self.config_dir);
        map.personal.into_iter().chain(map.ignored).collect()
    }

    /// The project a read is narrowed to, when `project` is given: its map,
    /// itself and its analysis (for the smart sources).
    fn project_scope(
        &self,
        arguments: &Value,
    ) -> Result<
        Option<(
            crate::projects::ProjectMap,
            crate::projects::Project,
            Option<crate::projects::Analysis>,
        )>,
        String,
    > {
        let Some(wanted) = text(arguments, "project") else {
            return Ok(None);
        };
        self.permissions().check(Scope::Projects)?;
        let map = crate::projects::ProjectMap::load(&self.config_dir);
        let project = self.resolve_project(&map, &wanted)?.clone();
        let analysis = crate::projects::Analysis::load(&self.config_dir, &project.id);
        Ok(Some((map, project, analysis)))
    }

    /// `message` if it belongs to the project in `scope` (or there is none),
    /// with its sender named as the project's members sheet says. A smart
    /// chat's message the analysis has not read yet is kept, marked.
    fn in_project(
        scope: &Option<(
            crate::projects::ProjectMap,
            crate::projects::Project,
            Option<crate::projects::Analysis>,
        )>,
        message: &Value,
    ) -> Option<Value> {
        let Some((map, project, analysis)) = scope else {
            return Some(message.clone());
        };
        let chat = message["chat"].as_str().unwrap_or("");
        let inclusion = map.inclusion_of(
            project,
            chat,
            message["id"].as_str().unwrap_or(""),
            message["timestamp"].as_i64().unwrap_or(0),
            analysis.as_ref(),
        );
        if inclusion == crate::projects::Inclusion::Out {
            return None;
        }
        let mut message = message.clone();
        if message["from_me"].as_bool() != Some(true)
            && let Some(member) = project.member_for(
                message["sender"].as_str().unwrap_or(""),
                message["sender_name"].as_str(),
                chat,
            )
        {
            message["sender_name"] = Value::String(member.label());
        }
        if inclusion == crate::projects::Inclusion::Unsure
            && let Some(text) = message["text"].as_str()
        {
            message["text"] = Value::String(format!("{text} [not sorted into the project yet]"));
        }
        Some(message)
    }

    /// A change, applied by the app through the bridge so it never races the
    /// app's own copy of projects and leads (src/project_actions.rs).
    fn write(&self, action: &str, arguments: &Value) -> Result<String, String> {
        let body = json!({"action": action, "args": arguments, "client": self.client_name()});
        let answer = self.post("/action", &body)?;
        Ok(serde_json::to_string_pretty(&answer).unwrap_or_default())
    }

    /// A message written over MCP: sent, or left in the chat's box when
    /// `draft` or when the user only lets AIs write drafts.
    fn message(&self, arguments: &Value, draft: bool) -> Result<String, String> {
        let wanted = text(arguments, "chat").ok_or("`chat` is required.")?;
        let message = text(arguments, "text").ok_or("`text` is required.")?;
        let excluded = self.excluded_chats(&self.permissions());
        let (chat, name) = self.resolve(&wanted, &excluded)?;
        let body = json!({
            "chat": chat,
            "text": message,
            "draft": draft,
            "client": self.client_name(),
        });
        let answer = self.post("/send", &body)?;
        Ok(if answer["status"] == "sent" {
            format!("Sent to {name}.")
        } else {
            format!(
                "Left in {name}'s text box in Whats Pro: the user reads it and presses send."
            )
        })
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        let config = crate::bridge::config(&self.config_dir)
            .map_err(|error| format!("Could not read Whats Pro's bridge settings: {error}"))?;
        if !config.enabled {
            return Err("The Whats Pro bridge is turned off in bridge.json.".into());
        }
        let url = format!("http://127.0.0.1:{}{path}", config.port);
        let mut response = ureq::post(&url)
            .header("Authorization", &format!("Bearer {}", config.token))
            .header("Content-Type", "application/json")
            .config()
            .http_status_as_error(false)
            .build()
            .send(body.to_string())
            .map_err(|_| {
                "Whats Pro is not open. Open the app and try again: changes are made by the app."
                    .to_owned()
            })?;
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|error| format!("Could not read Whats Pro's answer: {error}"))?;
        let answer: Value = serde_json::from_str(&text).unwrap_or(json!({"error": text}));
        if status == 404 && path == "/send" && answer["error"] == "unknown route" {
            return Err("This Whats Pro is older than the MCP: update the app to write messages.".into());
        }
        if status != 200 {
            return Err(answer["error"]
                .as_str()
                .unwrap_or("Whats Pro could not make the change.")
                .to_owned());
        }
        Ok(answer)
    }

    fn get(&self, path: &str) -> Result<Value, String> {
        let config = crate::bridge::config(&self.config_dir)
            .map_err(|error| format!("Could not read Whats Pro's bridge settings: {error}"))?;
        if !config.enabled {
            return Err("The Whats Pro bridge is turned off in bridge.json.".into());
        }
        let url = format!("http://127.0.0.1:{}{path}", config.port);
        let mut response = ureq::get(&url)
            .header("Authorization", &format!("Bearer {}", config.token))
            .call()
            .map_err(|error| match error {
                ureq::Error::StatusCode(status) => format!("Whats Pro answered {status}."),
                _ => "Whats Pro is not open. Open the app and try again.".to_owned(),
            })?;
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|error| format!("Could not read Whats Pro's answer: {error}"))?;
        serde_json::from_str(&text).map_err(|error| format!("Unexpected answer: {error}"))
    }

    /// Visible chats from the bridge, excluding locked and (when configured)
    /// personal or ignored chats.
    fn chats(&self, excluded: &HashSet<String>) -> Result<Vec<Value>, String> {
        let chats = self.get("/chats")?;
        Ok(chats
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|chat| chat["locked"].as_bool() != Some(true))
            .filter(|chat| chat["id"].as_str().is_none_or(|id| !excluded.contains(id)))
            .collect())
    }
}

fn query(pairs: &[(&str, String)]) -> String {
    form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs.iter().map(|(key, value)| (*key, value.as_str())))
        .finish()
}

fn number(arguments: &Value, name: &str) -> Option<f64> {
    arguments.get(name).and_then(Value::as_f64)
}

fn text(arguments: &Value, name: &str) -> Option<String> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn when(seconds: i64) -> String {
    jiff::Timestamp::from_second(seconds)
        .map(|at| {
            at.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

fn kind_matches(chat: &Value, kind: Option<&str>) -> bool {
    match kind {
        Some("group") => chat["kind"] == "group",
        Some("direct") => chat["kind"] == "direct",
        _ => true,
    }
}

/// One message as a line: `[time] Sender: text`, with its reply context.
fn line(message: &Value, with_chat: bool) -> String {
    let sender = if message["from_me"].as_bool() == Some(true) {
        "Me".to_owned()
    } else {
        message["sender_name"]
            .as_str()
            .filter(|name| !name.is_empty())
            .map_or_else(
                || {
                    message["sender"]
                        .as_str()
                        .unwrap_or("?")
                        .split('@')
                        .next()
                        .unwrap_or("?")
                        .to_owned()
                },
                str::to_owned,
            )
    };
    let mut text = message["text"].as_str().unwrap_or("").to_owned();
    match message["transcript"].as_str() {
        Some(transcript) if !transcript.trim().is_empty() => {
            text = format!("{text} [transcript: {transcript}]");
        }
        Some(_) if matches!(message["kind"].as_str(), Some("audio" | "video")) => {
            text = format!("{text} [no speech in it]");
        }
        _ => {}
    }
    if let Some(reply) = message["reply_to"]["text"].as_str() {
        text = format!("(replying to \"{reply}\") {text}");
    }
    if message["pending"].as_bool() == Some(true) {
        text = "(pending: WhatsApp has not delivered this message to this computer yet; it arrives when the phone is online)".to_owned();
    }
    if message["forwarded"].as_bool() == Some(true) {
        text = format!("↪ forwarded (written by someone else first): {text}");
    }
    if let Some(kind) = message["kind"].as_str()
        && matches!(kind, "image" | "video" | "audio" | "document" | "sticker")
    {
        text = format!(
            "{text} ({kind}, id {} — open it with get_media)",
            message["id"].as_str().unwrap_or("")
        );
    }
    let chat = if with_chat {
        format!(" [{}]", message["chat_name"].as_str().unwrap_or(""))
    } else {
        String::new()
    };
    format!(
        "[{}]{chat} {sender}: {text}",
        when(message["timestamp"].as_i64().unwrap_or(0))
    )
}

fn format_finding(finding: &crate::projects::Finding) -> String {
    let mut details = Vec::new();
    if let Some(who) = finding.who.as_deref().filter(|w| !w.trim().is_empty()) {
        details.push(format!("who: {who}"));
    }
    if let Some(due) = finding.due.as_deref().filter(|d| !d.trim().is_empty()) {
        details.push(format!("due: {due}"));
    }
    if let Some(since) = finding.since.filter(|&s| s > 0) {
        details.push(format!("since: {}", when(since)));
    }
    if let Some(chat) = finding.chat.as_deref().filter(|c| !c.trim().is_empty()) {
        details.push(format!("chat: {chat}"));
    }
    // The id lets resolve_item close exactly this one.
    details.push(format!("id: {}", finding.id));
    format!("- {} ({})", finding.text, details.join(", "))
}

fn stage_slug(stage: crate::leads::Stage) -> &'static str {
    match stage {
        crate::leads::Stage::New => "new",
        crate::leads::Stage::Contacted => "contacted",
        crate::leads::Stage::Quoted => "quoted",
        crate::leads::Stage::Customer => "customer",
        crate::leads::Stage::Lost => "lost",
    }
}

fn parse_stage(value: &str) -> Option<crate::leads::Stage> {
    match value.trim().to_lowercase().as_str() {
        "new" | "nuevo" => Some(crate::leads::Stage::New),
        "contacted" | "contactado" => Some(crate::leads::Stage::Contacted),
        "quoted" | "cotizado" => Some(crate::leads::Stage::Quoted),
        "customer" | "cliente" => Some(crate::leads::Stage::Customer),
        "lost" | "perdido" => Some(crate::leads::Stage::Lost),
        _ => None,
    }
}

fn entry_kind_label(kind: crate::leads::Kind) -> &'static str {
    match kind {
        crate::leads::Kind::Note => "note",
        crate::leads::Kind::Call => "call",
        crate::leads::Kind::FollowUp => "follow_up",
        crate::leads::Kind::Summary => "summary",
        crate::leads::Kind::Stage => "stage",
        crate::leads::Kind::Task => "task",
    }
}

impl Bridge {
    /// A chat id from an id, a name or part of a name.
    fn resolve(
        &self,
        wanted: &str,
        excluded: &HashSet<String>,
    ) -> Result<(String, String), String> {
        let chats = self.chats(excluded)?;
        if let Some(chat) = chats.iter().find(|chat| chat["id"] == wanted) {
            return Ok((
                wanted.to_owned(),
                chat["name"].as_str().unwrap_or("").to_owned(),
            ));
        }
        let needle = wanted.to_lowercase();
        let named = |chat: &&Value| chat["name"].as_str().unwrap_or("").to_lowercase();
        let exact: Vec<&Value> = chats.iter().filter(|c| named(c) == needle).collect();
        let found: Vec<&Value> = if exact.is_empty() {
            chats
                .iter()
                .filter(|c| named(c).contains(&needle))
                .collect()
        } else {
            exact
        };
        match found.as_slice() {
            [] => Err(format!(
                "No chat is called \"{wanted}\". Use list_chats to find it."
            )),
            [chat] => Ok((
                chat["id"].as_str().unwrap_or("").to_owned(),
                chat["name"].as_str().unwrap_or("").to_owned(),
            )),
            several => Err(format!(
                "Several chats match \"{wanted}\": {}. Ask again with the exact name or id.",
                several
                    .iter()
                    .take(15)
                    .map(|chat| format!(
                        "{} ({})",
                        chat["name"].as_str().unwrap_or(""),
                        chat["id"].as_str().unwrap_or("")
                    ))
                    .collect::<Vec<_>>()
                    .join("; ")
            )),
        }
    }

    /// Finds a project by id, exact name, or substring of its name.
    fn resolve_project<'a>(
        &self,
        map: &'a crate::projects::ProjectMap,
        wanted: &str,
    ) -> Result<&'a crate::projects::Project, String> {
        if let Some(project) = map.project(wanted) {
            return Ok(project);
        }
        let needle = wanted.trim().to_lowercase();
        let exact: Vec<&crate::projects::Project> = map
            .projects
            .iter()
            .filter(|p| p.name.to_lowercase() == needle || p.id.to_lowercase() == needle)
            .collect();
        let found = if exact.is_empty() {
            map.projects
                .iter()
                .filter(|p| p.name.to_lowercase().contains(&needle))
                .collect()
        } else {
            exact
        };
        match found.as_slice() {
            [] => Err(format!(
                "No project is called \"{wanted}\". Use list_projects to find it."
            )),
            [project] => Ok(project),
            several => Err(format!(
                "Several projects match \"{wanted}\": {}. Ask again with the exact name or id.",
                several
                    .iter()
                    .map(|p| format!("{} ({})", p.name, p.id))
                    .collect::<Vec<_>>()
                    .join("; ")
            )),
        }
    }

    fn list_projects(&self) -> Result<String, String> {
        use crate::projects::{Analysis, ProjectMap, Ticks};
        let map = ProjectMap::load(&self.config_dir);
        if map.projects.is_empty() {
            return Ok("No projects yet.".into());
        }
        let mut lines = Vec::with_capacity(map.projects.len());
        for project in &map.projects {
            let analysis = Analysis::load(&self.config_dir, &project.id).unwrap_or_default();
            let ticks = Ticks::load(&self.config_dir, &project.id);
            let open = |items: &[crate::projects::Finding]| {
                items.iter().filter(|f| ticks.open(&f.id)).count()
            };
            let updated = if analysis.updated > 0 {
                when(analysis.updated)
            } else {
                "never".to_owned()
            };
            lines.push(format!(
                "{} | {} | {} sources | last analysis: {} | todo: {} | follow_up: {} | slipping: {} | needs: {} | highlights: {}",
                project.name,
                project.id,
                project.sources.len(),
                updated,
                open(&analysis.todo),
                open(&analysis.follow_up),
                open(&analysis.leaks),
                open(&analysis.needs),
                open(&analysis.highlights),
            ));
        }
        Ok(format!(
            "name | id | sources | last analysis | todo | follow_up | slipping | needs | highlights\n{}",
            lines.join("\n")
        ))
    }

    fn project_overview(&self, arguments: &Value) -> Result<String, String> {
        use crate::projects::{Analysis, ProjectMap, Ticks};
        let wanted = text(arguments, "project").ok_or("`project` is required.")?;
        let map = ProjectMap::load(&self.config_dir);
        let project = self.resolve_project(&map, &wanted)?;
        let analysis = Analysis::load(&self.config_dir, &project.id).unwrap_or_default();
        let ticks = Ticks::load(&self.config_dir, &project.id);
        let updated = if analysis.updated > 0 {
            when(analysis.updated)
        } else {
            "not analyzed yet".to_owned()
        };

        let mut out = format!(
            "# {} ({})\nSources: {} chats | Last analysis: {}\n",
            project.name,
            project.id,
            project.sources.len(),
            updated
        );

        if !project.context.trim().is_empty() {
            out.push_str(&format!("\n## Context\n{}\n", project.context.trim()));
        }

        if !project.members.is_empty() {
            out.push_str("\n## Members\n");
            for member in &project.members {
                let mut line = format!("- {} ({})", member.name, member.id);
                if !member.role.trim().is_empty() {
                    line.push_str(&format!(" - {}", member.role.trim()));
                }
                if !member.about.trim().is_empty() {
                    line.push_str(&format!(": {}", member.about.trim()));
                }
                if !member.aliases.is_empty() {
                    line.push_str(&format!(
                        " | also shows up as: {}",
                        member.aliases.join(", ")
                    ));
                }
                if !member.chats.is_empty() {
                    line.push_str(&format!(" | also writes from: {}", member.chats.join(", ")));
                }
                out.push_str(&line);
                out.push('\n');
            }
        }

        out.push_str("\n## Summaries\n");
        for (key, label) in [
            ("today", "Today"),
            ("yesterday", "Yesterday"),
            ("week", "Week"),
        ] {
            let summary = analysis
                .summaries
                .get(key)
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .unwrap_or("(none)");
            out.push_str(&format!("- {label}: {summary}\n"));
        }

        let tasks: Vec<crate::projects::Finding> =
            crate::projects::Tasks::load(&self.config_dir, &project.id)
                .items
                .into_iter()
                .map(|task| task.finding)
                .collect();
        if !ticks.suggestions.is_empty() {
            out.push_str("\n## Maybe done (waiting for the owner's yes or no)\n");
            for suggestion in &ticks.suggestions {
                out.push_str(&format!(
                    "- {} (evidence: \"{}\", id: {})\n",
                    suggestion.text, suggestion.quote, suggestion.id
                ));
            }
        }
        let sections = [
            ("Tasks added by hand or by an AI", &tasks),
            ("Open to-dos", &analysis.todo),
            ("Follow-ups", &analysis.follow_up),
            ("Slipping", &analysis.leaks),
            ("Needs", &analysis.needs),
            ("Highlights", &analysis.highlights),
        ];
        for (heading, items) in sections {
            let open: Vec<String> = items
                .iter()
                .filter(|f| ticks.open(&f.id))
                .map(format_finding)
                .collect();
            out.push_str(&format!("\n## {heading} ({})\n", open.len()));
            if open.is_empty() {
                out.push_str("(none)\n");
            } else {
                out.push_str(&open.join("\n"));
                out.push('\n');
            }
        }

        Ok(out.trim_end().to_owned())
    }

    fn project_notes(&self, arguments: &Value) -> Result<String, String> {
        use crate::projects::{Journal, NoteKind, ProjectMap};
        let wanted = text(arguments, "project").ok_or("`project` is required.")?;
        let map = ProjectMap::load(&self.config_dir);
        let project = self.resolve_project(&map, &wanted)?;
        let since = number(arguments, "days").map(|days| now() - (days * 86_400.0) as i64);
        let journal = Journal::load(&self.config_dir, &project.id);
        let notes: Vec<String> = journal
            .notes
            .iter()
            .filter(|note| since.is_none_or(|since| note.at >= since))
            .map(|note| {
                let kind = match note.kind {
                    NoteKind::Log => "log",
                    NoteKind::Memo => "memo",
                };
                let spoken = if note.spoken { ", spoken" } else { "" };
                format!("[{}] ({kind}{spoken}) {}", when(note.at), note.text)
            })
            .collect();
        if notes.is_empty() {
            return Ok(format!("No notes for {}.", project.name));
        }
        Ok(format!(
            "# Notes: {} ({})\n{}",
            project.name,
            notes.len(),
            notes.join("\n")
        ))
    }

    fn list_calls(&self, arguments: &Value) -> Result<String, String> {
        let library = crate::call_library::Library::load(&self.state_dir);
        let map = crate::projects::ProjectMap::load(&self.config_dir);
        let leads = crate::leads::Leads::load(&self.config_dir);
        let project_filter = text(arguments, "project").map(|wanted| {
            self.resolve_project(&map, &wanted)
                .map(|p| p.id.clone())
                .unwrap_or(wanted)
        });
        let since = number(arguments, "days").map(|days| now() - (days * 86_400.0) as i64);
        let limit = number(arguments, "limit").unwrap_or(50.0).clamp(1.0, 500.0) as usize;

        let mut calls: Vec<&crate::call_library::Call> = library
            .calls
            .iter()
            .filter(|call| !call.private)
            .filter(|call| {
                project_filter.as_ref().is_none_or(|wanted| {
                    call.project
                        .as_deref()
                        .is_some_and(|p| p.eq_ignore_ascii_case(wanted))
                })
            })
            .filter(|call| since.is_none_or(|since| call.when() >= since))
            .collect();
        calls.sort_by_key(|call| std::cmp::Reverse(call.when()));

        let lines: Vec<String> = calls
            .into_iter()
            .take(limit)
            .map(|call| {
                let project = call
                    .project
                    .as_deref()
                    .map(|id| {
                        map.project(id)
                            .map(|p| format!("{} ({})", p.name, p.id))
                            .unwrap_or_else(|| id.to_owned())
                    })
                    .unwrap_or_else(|| "-".to_owned());
                let lead = call
                    .lead
                    .as_deref()
                    .map(|id| {
                        leads
                            .get(id)
                            .map(|l| format!("{} ({})", l.name, l.id))
                            .unwrap_or_else(|| id.to_owned())
                    })
                    .unwrap_or_else(|| "-".to_owned());
                let summary = call
                    .analysis
                    .as_ref()
                    .map(|a| a.summary.trim())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("(no summary)");
                format!(
                    "{} | {} | {} | {}:{:02} | project: {} | lead: {} | {}",
                    call.id,
                    call.title,
                    when(call.when()),
                    call.seconds / 60,
                    call.seconds % 60,
                    project,
                    lead,
                    summary,
                )
            })
            .collect();

        if lines.is_empty() {
            return Ok("No calls match.".into());
        }
        Ok(format!(
            "id | title | date | duration | project | lead | summary\n{}",
            lines.join("\n")
        ))
    }

    fn read_call(&self, arguments: &Value, permissions: &Permissions) -> Result<String, String> {
        let wanted = text(arguments, "call").ok_or("`call` is required.")?;
        let library = crate::call_library::Library::load(&self.state_dir);
        let visible: Vec<&crate::call_library::Call> =
            library.calls.iter().filter(|call| !call.private).collect();

        let call = if let Some(call) = visible.iter().find(|c| c.id == wanted).copied() {
            call
        } else {
            let needle = wanted.to_lowercase();
            let exact: Vec<&crate::call_library::Call> = visible
                .iter()
                .copied()
                .filter(|c| c.title.to_lowercase() == needle)
                .collect();
            let found = if exact.is_empty() {
                visible
                    .iter()
                    .copied()
                    .filter(|c| c.title.to_lowercase().contains(&needle))
                    .collect()
            } else {
                exact
            };
            match found.as_slice() {
                [] => {
                    return Err(format!(
                        "No call matches \"{wanted}\". Use list_calls to find it."
                    ));
                }
                [call] => *call,
                several => {
                    return Err(format!(
                        "Several calls match \"{wanted}\": {}. Ask again with the exact id.",
                        several
                            .iter()
                            .take(15)
                            .map(|c| format!("{} ({})", c.title, c.id))
                            .collect::<Vec<_>>()
                            .join("; ")
                    ));
                }
            }
        };

        let map = crate::projects::ProjectMap::load(&self.config_dir);
        let leads = crate::leads::Leads::load(&self.config_dir);
        let project = call
            .project
            .as_deref()
            .map(|id| {
                map.project(id)
                    .map(|p| format!("{} ({})", p.name, p.id))
                    .unwrap_or_else(|| id.to_owned())
            })
            .unwrap_or_else(|| "-".to_owned());
        let lead = call
            .lead
            .as_deref()
            .map(|id| {
                leads
                    .get(id)
                    .map(|l| format!("{} ({})", l.name, l.id))
                    .unwrap_or_else(|| id.to_owned())
            })
            .unwrap_or_else(|| "-".to_owned());

        let mut out = format!(
            "# {} ({})\n- Date: {}\n- Duration: {}:{:02}\n- Project: {}\n- Lead: {}\n",
            call.title,
            call.id,
            when(call.when()),
            call.seconds / 60,
            call.seconds % 60,
            project,
            lead,
        );

        if let Some(analysis) = &call.analysis {
            if !analysis.summary.trim().is_empty() {
                out.push_str(&format!("\n## Summary\n{}\n", analysis.summary.trim()));
            }
            if !analysis.decisions.is_empty() {
                out.push_str("\n## Decisions\n");
                for decision in &analysis.decisions {
                    out.push_str(&format!("- {decision}\n"));
                }
            }
            if !analysis.commitments.is_empty() {
                out.push_str("\n## Commitments\n");
                for commitment in &analysis.commitments {
                    let due = commitment
                        .due
                        .as_deref()
                        .filter(|d| !d.trim().is_empty())
                        .map(|d| format!(" (due {d})"))
                        .unwrap_or_default();
                    out.push_str(&format!(
                        "- {}: {}{due}\n",
                        commitment.who.trim(),
                        commitment.what.trim()
                    ));
                }
            }
            if !analysis.tasks.is_empty() {
                out.push_str("\n## Tasks\n");
                for task in &analysis.tasks {
                    out.push_str(&format!("- {task}\n"));
                }
            }
            if !analysis.next_steps.is_empty() {
                out.push_str("\n## Next steps\n");
                for step in &analysis.next_steps {
                    out.push_str(&format!("- {step}\n"));
                }
            }
        } else {
            out.push_str("\n(No analysis yet)\n");
        }

        if permissions.call_transcripts
            && let Some(transcript) = call.transcript()
        {
            out.push_str(&format!(
                "\n## Transcript ({})\n{}\n",
                transcript.engine,
                transcript.text.trim()
            ));
        }

        Ok(out.trim_end().to_owned())
    }

    fn list_leads(&self, arguments: &Value) -> Result<String, String> {
        let leads = crate::leads::Leads::load(&self.config_dir);
        let map = crate::projects::ProjectMap::load(&self.config_dir);
        let project_filter = text(arguments, "project").map(|wanted| {
            self.resolve_project(&map, &wanted)
                .map(|p| p.id.clone())
                .unwrap_or(wanted)
        });
        let stage_filter = match text(arguments, "stage") {
            Some(stage) => Some(parse_stage(&stage).ok_or_else(|| {
                format!(
                    "Unknown stage \"{stage}\". Expected one of: new, contacted, quoted, customer, lost."
                )
            })?),
            None => None,
        };

        let lines: Vec<String> = leads
            .leads
            .iter()
            .filter(|lead| {
                project_filter
                    .as_ref()
                    .is_none_or(|wanted| lead.project.eq_ignore_ascii_case(wanted))
            })
            .filter(|lead| stage_filter.is_none_or(|stage| lead.stage == stage))
            .map(|lead| {
                let project = map
                    .project(&lead.project)
                    .map(|p| format!("{} ({})", p.name, p.id))
                    .unwrap_or_else(|| lead.project.clone());
                let amount = lead.value.as_deref().unwrap_or("-");
                let next_action = lead
                    .next
                    .as_ref()
                    .map(|n| n.text.as_str())
                    .unwrap_or("-");
                let due = lead
                    .next
                    .as_ref()
                    .and_then(|n| n.due.as_deref())
                    .unwrap_or("-");
                let phone = lead.phone.as_deref().unwrap_or("-");
                format!(
                    "{} | {} | project: {} | stage: {} | amount: {} | next: {} | due: {} | phone: {}",
                    lead.id,
                    lead.name,
                    project,
                    stage_slug(lead.stage),
                    amount,
                    next_action,
                    due,
                    phone,
                )
            })
            .collect();

        if lines.is_empty() {
            return Ok("No leads match.".into());
        }
        Ok(format!(
            "id | name | project | stage | amount | next | due | phone\n{}",
            lines.join("\n")
        ))
    }

    fn read_lead(&self, arguments: &Value, permissions: &Permissions) -> Result<String, String> {
        let wanted = text(arguments, "lead").ok_or("`lead` is required.")?;
        let leads = crate::leads::Leads::load(&self.config_dir);
        let map = crate::projects::ProjectMap::load(&self.config_dir);

        let lead = if let Some(lead) = leads.get(&wanted) {
            lead
        } else {
            let needle = wanted.to_lowercase();
            let wanted_digits = crate::leads::digits(&wanted);
            let exact: Vec<&crate::leads::Lead> = leads
                .leads
                .iter()
                .filter(|l| l.name.to_lowercase() == needle)
                .collect();
            let found = if exact.is_empty() {
                leads
                    .leads
                    .iter()
                    .filter(|l| {
                        l.name.to_lowercase().contains(&needle)
                            || (!wanted_digits.is_empty()
                                && l.phone.as_deref().is_some_and(|p| {
                                    crate::leads::digits(p).contains(&wanted_digits)
                                }))
                    })
                    .collect()
            } else {
                exact
            };
            match found.as_slice() {
                [] => {
                    return Err(format!(
                        "No lead matches \"{wanted}\". Use list_leads to find it."
                    ));
                }
                [lead] => *lead,
                several => {
                    return Err(format!(
                        "Several leads match \"{wanted}\": {}. Ask again with the exact id.",
                        several
                            .iter()
                            .take(15)
                            .map(|l| format!("{} ({})", l.name, l.id))
                            .collect::<Vec<_>>()
                            .join("; ")
                    ));
                }
            }
        };

        let project = map
            .project(&lead.project)
            .map(|p| format!("{} ({})", p.name, p.id))
            .unwrap_or_else(|| lead.project.clone());
        let mut stage_line = stage_slug(lead.stage).to_owned();
        if let Some(reason) = lead.lost_reason.as_deref().filter(|r| !r.is_empty()) {
            stage_line.push_str(&format!(" (reason: {reason})"));
        }
        let amount = lead.value.as_deref().unwrap_or("-");
        let phone = lead.phone.as_deref().unwrap_or("-");
        let chat = lead.chat.as_deref().unwrap_or("-");
        let next = match &lead.next {
            Some(next) => match &next.due {
                Some(due) => format!("{} (due {due})", next.text),
                None => next.text.clone(),
            },
            None => "(none)".to_owned(),
        };

        let mut out = format!(
            "# {} ({})\n- Project: {}\n- Stage: {}\n- Amount: {}\n- Next action: {}\n- Phone: {}\n- Chat: {}\n",
            lead.name, lead.id, project, stage_line, amount, next, phone, chat,
        );

        if !lead.sales.is_empty() {
            out.push_str("\n## Sales\n");
            for sale in &lead.sales {
                out.push_str(&format!(
                    "- [{}] {} - total: {} ({})\n",
                    when(sale.at),
                    sale.summary,
                    sale.total,
                    sale.payment
                ));
            }
        }

        out.push_str(&format!("\n## History ({} entries)\n", lead.entries.len()));
        if lead.entries.is_empty() {
            out.push_str("(none)\n");
        } else {
            for entry in &lead.entries {
                out.push_str(&format!(
                    "- [{}] ({}) {}\n",
                    when(entry.at),
                    entry_kind_label(entry.kind),
                    entry.text
                ));
                if permissions.call_transcripts
                    && let Some(transcript) = entry.transcript.as_deref().filter(|t| !t.is_empty())
                {
                    out.push_str(&format!("  Transcript: {transcript}\n"));
                }
            }
        }

        Ok(out.trim_end().to_owned())
    }

    /// A message's attachment as MCP content: an image the client can see,
    /// or a short text for anything else.
    fn media(&self, arguments: &Value) -> Result<Vec<Value>, String> {
        let permissions = self.permissions();
        permissions.check(Scope::Chats)?;
        let _ = append_log(
            &self.config_dir,
            &LogEntry {
                at: now(),
                tool: "get_media".to_owned(),
                scope: Scope::Chats.as_str().to_owned(),
                client: self.client_name(),
            },
        );
        let excluded = self.excluded_chats(&permissions);
        let wanted = text(arguments, "chat").ok_or("`chat` is required.")?;
        let message = text(arguments, "message_id").ok_or("`message_id` is required.")?;
        let (chat, chat_name) = self.resolve(&wanted, &excluded)?;
        let (bytes, mime, on_disk, file_name) = self.get_bytes(&format!(
            "/media?{}",
            query(&[("chat", chat), ("id", message.clone())])
        ))?;
        let size = bytes.len();
        if matches!(
            mime.as_str(),
            "image/jpeg" | "image/png" | "image/gif" | "image/webp"
        ) {
            return Ok(vec![
                json!({"type": "image", "data": base64::engine::general_purpose::STANDARD.encode(&bytes), "mimeType": mime}),
                json!({"type": "text", "text": format!("Picture from {chat_name}, message {message} ({} KB).", size / 1024)}),
            ]);
        }
        if mime.starts_with("audio/") || mime.starts_with("video/") {
            let (chat, _) = self.resolve(&wanted, &excluded)?;
            let answer = self.get(&format!(
                "/transcript?{}",
                query(&[("chat", chat), ("id", message.clone())])
            ))?;
            let said = answer["text"].as_str().unwrap_or("").trim();
            let what = if mime.starts_with("video/") {
                "Video"
            } else {
                "Voice message or audio"
            };
            return Ok(vec![json!({"type": "text", "text": if said.is_empty() {
                format!("{what} from {chat_name}, message {message}: nothing was said in it.")
            } else {
                format!("{what} from {chat_name}, message {message}, transcribed: {said}")
            }})]);
        }
        let named = if file_name.is_empty() {
            String::new()
        } else {
            format!(" \"{file_name}\"")
        };
        if mime == "application/pdf" {
            let text = crate::ocr::pdf_text(&bytes).unwrap_or_default();
            return Ok(vec![json!({"type": "text", "text": if text.is_empty() {
                format!("PDF{named} from {chat_name}, message {message} ({} KB): it has no text layer (a scan or a picture). On disk: {on_disk}", size / 1024)
            } else {
                format!("PDF{named} from {chat_name}, message {message} ({} KB). On disk: {on_disk}\nText:\n{}", size / 1024, Self::clip_document(&text))
            }})]);
        }
        if mime.starts_with("text/") || mime == "application/json" || mime == "text/csv" {
            let text = String::from_utf8_lossy(&bytes);
            return Ok(vec![json!({"type": "text", "text": format!(
                "Document{named} from {chat_name}, message {message} ({mime}). On disk: {on_disk}\nText:\n{}",
                Self::clip_document(&text)
            )})]);
        }
        Ok(vec![json!({"type": "text", "text": format!(
            "Document{named} from {chat_name}, message {message}: {mime}, {} KB. Its contents cannot be read here. On disk: {on_disk}",
            size / 1024
        )})])
    }

    /// A document's text, cut where it would flood the conversation.
    fn clip_document(text: &str) -> String {
        const MAX: usize = 60_000;
        if text.chars().count() <= MAX {
            return text.to_owned();
        }
        let kept: String = text.chars().take(MAX).collect();
        format!("{kept}\n… (cut: the document goes on)")
    }

    /// A bridge route that answers with a file: its bytes and type.
    fn get_bytes(&self, path: &str) -> Result<(Vec<u8>, String, String, String), String> {
        let config = crate::bridge::config(&self.config_dir)
            .map_err(|error| format!("Could not read Whats Pro's bridge settings: {error}"))?;
        if !config.enabled {
            return Err("The Whats Pro bridge is turned off in bridge.json.".into());
        }
        let url = format!("http://127.0.0.1:{}{path}", config.port);
        let mut response = ureq::get(&url)
            .header("Authorization", &format!("Bearer {}", config.token))
            .config()
            .timeout_global(Some(std::time::Duration::from_secs(60)))
            .build()
            .call()
            .map_err(|error| match error {
                ureq::Error::StatusCode(404) => "That message has no attachment, or its chat is not shared.".to_owned(),
                ureq::Error::StatusCode(504) => "WhatsApp did not deliver the file in time; the phone may need to be online. Try again.".to_owned(),
                ureq::Error::StatusCode(status) => format!("Whats Pro answered {status}."),
                _ => "Whats Pro is not open. Open the app and try again.".to_owned(),
            })?;
        let mime = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_owned();
        let decoded = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(|value| {
                    form_urlencoded::parse(format!("v={value}").as_bytes())
                        .next()
                        .map(|(_, v)| v.into_owned())
                        .unwrap_or_default()
                })
                .unwrap_or_default()
        };
        let (on_disk, name) = (decoded("x-path"), decoded("x-file-name"));
        let bytes = response
            .body_mut()
            .with_config()
            .limit(64 * 1024 * 1024)
            .read_to_vec()
            .map_err(|error| format!("Could not read the file: {error}"))?;
        Ok((bytes, mime, on_disk, name))
    }

    fn call(&self, name: &str, arguments: &Value) -> Result<String, String> {
        let Some(scope) = tool_scope(name) else {
            return Err(format!("Unknown tool {name}."));
        };
        let permissions = self.permissions();
        permissions.check(scope)?;
        let _ = append_log(
            &self.config_dir,
            &LogEntry {
                at: now(),
                tool: name.to_owned(),
                scope: scope.as_str().to_owned(),
                client: self.client_name(),
            },
        );
        match name {
            "list_chats" => {
                let excluded = self.excluded_chats(&permissions);
                let kind = text(arguments, "kind");
                let since = number(arguments, "active_within_days")
                    .map(|days| now() - (days * 86_400.0) as i64);
                let wanted = text(arguments, "query").map(|q| q.to_lowercase());
                let limit = number(arguments, "limit").unwrap_or(100.0) as usize;
                let chats = self.chats(&excluded)?;
                let lines: Vec<String> = chats
                    .iter()
                    .filter(|chat| kind_matches(chat, kind.as_deref()))
                    .filter(|chat| {
                        since.is_none_or(|since| {
                            chat["last_activity"].as_i64().unwrap_or(0) >= since
                        })
                    })
                    .filter(|chat| {
                        wanted.as_ref().is_none_or(|wanted| {
                            chat["name"]
                                .as_str()
                                .unwrap_or("")
                                .to_lowercase()
                                .contains(wanted)
                        })
                    })
                    .take(limit.max(1))
                    .map(|chat| {
                        format!(
                            "{} | {} | {} | unread {} | last {} | {} messages{}",
                            chat["name"].as_str().unwrap_or(""),
                            chat["kind"].as_str().unwrap_or(""),
                            chat["id"].as_str().unwrap_or(""),
                            chat["unread"],
                            when(chat["last_activity"].as_i64().unwrap_or(0)),
                            chat["messages"],
                            if chat["archived"] == true {
                                " | archived"
                            } else {
                                ""
                            },
                        )
                    })
                    .collect();
                Ok(if lines.is_empty() {
                    "No chats match.".into()
                } else {
                    format!(
                        "name | kind | id | unread | last activity | messages\n{}",
                        lines.join("\n")
                    )
                })
            }
            "transcribe_chat" => {
                let excluded = self.excluded_chats(&permissions);
                let wanted = text(arguments, "chat").ok_or("`chat` is required.")?;
                let (id, chat_name) = self.resolve(&wanted, &excluded)?;
                let since = number(arguments, "since_hours")
                    .map(|hours| now() - (hours * 3600.0) as i64)
                    .unwrap_or(0);
                let answer = self.get(&format!(
                    "/backlog?{}",
                    query(&[("chat", id), ("since", since.to_string())])
                ))?;
                let started = answer["started"].as_u64().unwrap_or(0);
                Ok(if started == 0 {
                    format!("Everything in {chat_name} already has its text.")
                } else {
                    format!(
                        "Started reading {started} voice messages, videos and pictures of {chat_name}. Read the chat again in a minute or two to see them as [transcript: …]."
                    )
                })
            }
            "list_media" => {
                let excluded = self.excluded_chats(&permissions);
                let wanted = text(arguments, "chat").ok_or("`chat` is required.")?;
                let (id, chat_name) = self.resolve(&wanted, &excluded)?;
                let kind = text(arguments, "kind").filter(|k| k != "all");
                let limit = number(arguments, "limit").unwrap_or(50.0).clamp(1.0, 500.0) as usize;
                let mut pairs = vec![("chat", id), ("limit", "2000".to_owned())];
                if let Some(hours) = number(arguments, "since_hours") {
                    pairs.push(("since", (now() - (hours * 3600.0) as i64).to_string()));
                }
                let messages = self.get(&format!("/messages?{}", query(&pairs)))?;
                let media: Vec<&Value> = messages
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|m| {
                        let k = m["kind"].as_str().unwrap_or("");
                        matches!(k, "image" | "video" | "audio" | "document" | "sticker")
                            && kind.as_deref().is_none_or(|wanted| wanted == k)
                    })
                    .collect();
                let start = media.len().saturating_sub(limit);
                let lines: Vec<String> = media[start..].iter().map(|m| line(m, false)).collect();
                Ok(if lines.is_empty() {
                    format!("No attachments in {chat_name}.")
                } else {
                    format!(
                        "{chat_name}: {} attachments\n{}",
                        lines.len(),
                        lines.join("\n")
                    )
                })
            }
            "read_chat" => {
                let excluded = self.excluded_chats(&permissions);
                let wanted = text(arguments, "chat").ok_or("`chat` is required.")?;
                let (id, chat_name) = self.resolve(&wanted, &excluded)?;
                let limit = number(arguments, "limit")
                    .unwrap_or(200.0)
                    .clamp(1.0, 2000.0) as usize;
                let mut pairs = vec![("chat", id.clone()), ("limit", limit.to_string())];
                if let Some(hours) = number(arguments, "since_hours") {
                    pairs.push(("since", (now() - (hours * 3600.0) as i64).to_string()));
                }
                let messages = self.get(&format!("/messages?{}", query(&pairs)))?;
                let scope = self.project_scope(arguments)?;
                if let Some((map, project, _)) = &scope
                    && project.source(&id).is_none()
                    && !map
                        .picks
                        .iter()
                        .any(|p| p.project == project.id && p.chat == id)
                {
                    return Err(format!(
                        "{chat_name} is not one of {}'s chats. Read it without `project`, or add it with update_project.",
                        project.name
                    ));
                }
                let lines: Vec<String> = messages
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|message| Self::in_project(&scope, message))
                    .map(|message| line(&message, false))
                    .collect();
                Ok(format!(
                    "Chat: {chat_name} ({} messages)\n{}",
                    lines.len(),
                    lines.join("\n")
                ))
            }
            "recent_activity" => {
                let excluded = self.excluded_chats(&permissions);
                let hours = number(arguments, "hours").unwrap_or(24.0);
                let limit = number(arguments, "limit")
                    .unwrap_or(1500.0)
                    .clamp(1.0, 5000.0) as usize;
                let kind = text(arguments, "kind");
                let since = now() - (hours * 3600.0) as i64;
                let kinds: std::collections::HashMap<String, String> = self
                    .chats(&excluded)?
                    .into_iter()
                    .map(|chat| {
                        (
                            chat["id"].as_str().unwrap_or("").to_owned(),
                            chat["kind"].as_str().unwrap_or("").to_owned(),
                        )
                    })
                    .collect();
                let messages = self.get(&format!(
                    "/messages?{}",
                    query(&[("since", since.to_string()), ("limit", limit.to_string())])
                ))?;
                let scope = self.project_scope(arguments)?;
                let mut by_chat: Vec<(String, Vec<String>)> = Vec::new();
                for message in messages.as_array().into_iter().flatten() {
                    let chat = message["chat"].as_str().unwrap_or("");
                    if excluded.contains(chat) {
                        continue;
                    }
                    let Some(message) = Self::in_project(&scope, message) else {
                        continue;
                    };
                    let message = &message;
                    let wanted = match kind.as_deref() {
                        Some("group") => kinds.get(chat).is_some_and(|k| k == "group"),
                        Some("direct") => kinds.get(chat).is_some_and(|k| k == "direct"),
                        _ => kinds.contains_key(chat) || !excluded.contains(chat),
                    };
                    if !wanted {
                        continue;
                    }
                    let name = message["chat_name"].as_str().unwrap_or("").to_owned();
                    match by_chat.iter_mut().find(|(chat_name, _)| *chat_name == name) {
                        Some((_, lines)) => lines.push(line(message, false)),
                        None => by_chat.push((name, vec![line(message, false)])),
                    }
                }
                if by_chat.is_empty() {
                    return Ok(format!("No messages in the last {hours} hours."));
                }
                Ok(by_chat
                    .into_iter()
                    .map(|(name, lines)| {
                        format!("## {name} ({} messages)\n{}", lines.len(), lines.join("\n"))
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n"))
            }
            "search_messages" => {
                let excluded = self.excluded_chats(&permissions);
                let needle = text(arguments, "query").unwrap_or_default();
                let limit = number(arguments, "limit").unwrap_or(50.0).clamp(1.0, 500.0) as usize;
                let mut pairs = vec![("q", needle.clone()), ("limit", limit.to_string())];
                if let Some(wanted) = text(arguments, "chat") {
                    pairs.push(("chat", self.resolve(&wanted, &excluded)?.0));
                }
                let day = |name: &str| -> Result<Option<i64>, String> {
                    text(arguments, name)
                        .map(|value| {
                            value
                                .parse::<jiff::civil::Date>()
                                .map_err(|_| format!("`{name}` must be a date like 2026-10-20"))
                                .and_then(|date| {
                                    date.to_zoned(jiff::tz::TimeZone::system())
                                        .map(|at| at.timestamp().as_second())
                                        .map_err(|error| error.to_string())
                                })
                        })
                        .transpose()
                };
                let since = number(arguments, "since_hours")
                    .map(|hours| now() - (hours * 3600.0) as i64)
                    .or(day("since")?);
                if let Some(since) = since {
                    pairs.push(("since", since.to_string()));
                }
                if let Some(until) = day("until")? {
                    pairs.push(("until", until.to_string()));
                }
                for name in ["from", "kind"] {
                    if let Some(value) = text(arguments, name) {
                        pairs.push((name, value));
                    }
                }
                let filtered = pairs
                    .iter()
                    .any(|(name, _)| matches!(*name, "since" | "until" | "from" | "kind"));
                if needle.is_empty() && !filtered {
                    return Err(
                        "Give a query, or at least one filter (since, until, from or kind). To read a whole chat use read_chat.".into(),
                    );
                }
                let messages = self.get(&format!("/search?{}", query(&pairs)))?;
                let lines: Vec<String> = messages
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|message| {
                        message["chat"]
                            .as_str()
                            .is_none_or(|chat| !excluded.contains(chat))
                    })
                    .map(|message| line(message, true))
                    .collect();
                Ok(if lines.is_empty() {
                    "No messages match.".into()
                } else {
                    lines.join("\n")
                })
            }
            "list_projects" => self.list_projects(),
            "project_overview" => self.project_overview(arguments),
            "project_notes" => self.project_notes(arguments),
            "list_calls" => self.list_calls(arguments),
            "read_call" => self.read_call(arguments, &permissions),
            "list_leads" => self.list_leads(arguments),
            "read_lead" => self.read_lead(arguments, &permissions),
            "draft_message" => self.message(arguments, true),
            "send_message" => self.message(arguments, false),
            _ if scope == Scope::Write => self.write(name, arguments),
            _ => Err(format!("Unknown tool {name}.")),
        }
    }
}

/// Answers one JSON-RPC request; `None` for notifications.
fn answer(bridge: &Bridge, request: &Value) -> Option<Value> {
    let id = request.get("id")?.clone();
    let method = request["method"].as_str().unwrap_or("");
    let result = match method {
        "initialize" => {
            bridge.set_client(request["params"]["clientInfo"]["name"].as_str());
            json!({
                "protocolVersion": request["params"]["protocolVersion"].as_str().unwrap_or(PROTOCOL),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": SERVER_NAME, "title": "Whats Pro", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "Access to the user's WhatsApp chats, projects, calls and leads in Whats Pro. Reading projects, calls and leads works from disk; chats and every change need Whats Pro open. Changes (create_project, add_task, resolve_item, create_lead...) say who made them and can be undone in the app. To turn chats into a project: read them (read_chat, get_media), then create_project, create_lead and add_task. To refresh a project's panel: run_analysis, then project_overview."
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools(&bridge.permissions())}),
        "tools/call" => {
            let name = request["params"]["name"].as_str().unwrap_or("");
            let arguments = request["params"]
                .get("arguments")
                .cloned()
                .unwrap_or(json!({}));
            if name == "get_media" {
                return Some(json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": match bridge.media(&arguments) {
                        Ok(content) => json!({"content": content}),
                        Err(error) => json!({"content": [{"type": "text", "text": error}], "isError": true}),
                    }
                }));
            }
            match bridge.call(name, &arguments) {
                Ok(text) => json!({"content": [{"type": "text", "text": text}]}),
                Err(error) => {
                    json!({"content": [{"type": "text", "text": error}], "isError": true})
                }
            }
        }
        _ => {
            return Some(json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": format!("Method not found: {method}")}
            }));
        }
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// Serves MCP on stdin and stdout until stdin closes.
pub fn run(config_dir: &Path, state_dir: &Path) -> i32 {
    let bridge = Bridge::new(config_dir, state_dir);
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(request) => answer(&bridge, &request),
            Err(error) => Some(json!({
                "jsonrpc": "2.0", "id": null,
                "error": {"code": -32700, "message": format!("Parse error: {error}")}
            })),
        };
        if let Some(reply) = reply
            && writeln!(stdout, "{reply}")
                .and_then(|()| stdout.flush())
                .is_err()
        {
            break;
        }
    }
    0
}

/// Claude Desktop's config file on this platform.
pub fn claude_desktop_config() -> Option<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME")?).join("Library/Application Support")
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?)
    } else {
        PathBuf::from(std::env::var_os("HOME")?).join(".config")
    };
    Some(base.join("Claude").join("claude_desktop_config.json"))
}

/// Adds (or updates) this app as the `whats-pro` MCP server in Claude
/// Desktop's config, keeping every other setting. The previous file is kept
/// beside it as `.bak`. Returns the config's path.
pub fn connect_claude_desktop() -> Result<PathBuf, String> {
    let path = claude_desktop_config().ok_or("Could not find Claude Desktop's settings folder")?;
    let executable =
        std::env::current_exe().map_err(|error| format!("Could not locate this app: {error}"))?;
    connect_at(&path, &executable)?;
    Ok(path)
}

fn connect_at(path: &Path, executable: &Path) -> Result<(), String> {
    let mut config: Value = match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text).map_err(|error| {
            format!(
                "Claude Desktop's settings are not valid JSON, so they were left alone: {error}"
            )
        })?,
        _ => json!({}),
    };
    if !config.is_object() {
        return Err(
            "Claude Desktop's settings are not a JSON object, so they were left alone".into(),
        );
    }
    if path.exists() {
        std::fs::copy(path, path.with_extension("json.bak"))
            .map_err(|error| format!("Could not back up Claude Desktop's settings: {error}"))?;
    }
    let servers = config
        .as_object_mut()
        .expect("checked above")
        .entry("mcpServers")
        .or_insert_with(|| json!({}));
    if !servers.is_object() {
        *servers = json!({});
    }
    servers[SERVER_NAME] = json!({
        "command": executable.to_string_lossy(),
        "args": ["mcp"],
    });
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            format!("Could not create Claude Desktop's settings folder: {error}")
        })?;
    }
    let text = serde_json::to_string_pretty(&config).expect("JSON values serialize");
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, text)
        .and_then(|()| std::fs::rename(&temporary, path))
        .map_err(|error| format!("Could not save Claude Desktop's settings: {error}"))
}

/// AI clients that can connect to Whats Pro's MCP server with one click.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Client {
    ClaudeDesktop,
    ClaudeCode,
    Codex,
    Antigravity,
    GeminiCli,
}

impl Client {
    pub const ALL: [Self; 5] = [
        Self::ClaudeDesktop,
        Self::ClaudeCode,
        Self::Codex,
        Self::Antigravity,
        Self::GeminiCli,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::ClaudeDesktop => "Claude Desktop",
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
            Self::Antigravity => "Antigravity",
            Self::GeminiCli => "Gemini CLI",
        }
    }

    /// Locates the CLI binary for this client, if it uses one.
    pub fn binary(self) -> Option<PathBuf> {
        match self {
            Self::ClaudeDesktop => None,
            Self::ClaudeCode => crate::ai_tools::find("claude"),
            Self::Codex => crate::ai_tools::find("codex"),
            Self::Antigravity => {
                let app = PathBuf::from("/Applications/Gemini.app/Contents/Helpers/agy");
                if app.is_file() {
                    Some(app)
                } else {
                    crate::ai_tools::find("agy")
                }
            }
            Self::GeminiCli => crate::ai_tools::find("gemini"),
        }
    }

    /// Whether the client application or CLI is installed on this computer.
    pub fn is_installed(self) -> bool {
        match self {
            Self::ClaudeDesktop => {
                Path::new("/Applications/Claude.app").exists()
                    || claude_desktop_config()
                        .is_some_and(|path| path.parent().is_some_and(Path::exists))
            }
            _ => self.binary().is_some(),
        }
    }

    /// Exact command-line arguments passed to the CLI binary to register `whats-pro`.
    pub fn mcp_add_args(self, executable: &Path) -> Option<Vec<String>> {
        let exe = executable.to_string_lossy().into_owned();
        match self {
            Self::ClaudeDesktop => None,
            Self::ClaudeCode => Some(vec![
                "mcp".into(),
                "add".into(),
                "--scope".into(),
                "user".into(),
                SERVER_NAME.into(),
                "--".into(),
                exe,
                "mcp".into(),
            ]),
            Self::Codex => Some(vec![
                "mcp".into(),
                "add".into(),
                SERVER_NAME.into(),
                "--".into(),
                exe,
                "mcp".into(),
            ]),
            Self::Antigravity | Self::GeminiCli => Some(vec![
                "mcp".into(),
                "add".into(),
                SERVER_NAME.into(),
                exe,
                "mcp".into(),
            ]),
        }
    }
}

fn has_json_mcp_server(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .is_some_and(|json| json["mcpServers"][SERVER_NAME].is_object())
}

/// Checks whether `whats-pro` is present in `client`'s configuration files under `home`.
pub fn config_has_server(client: Client, home: &Path, claude_desktop_path: Option<&Path>) -> bool {
    match client {
        Client::ClaudeDesktop => claude_desktop_path.is_some_and(has_json_mcp_server),
        Client::ClaudeCode => has_json_mcp_server(&home.join(".claude.json")),
        Client::Codex => std::fs::read_to_string(home.join(".codex/config.toml"))
            .is_ok_and(|text| text.contains(SERVER_NAME)),
        Client::Antigravity => {
            has_json_mcp_server(&home.join(".gemini/antigravity/mcp_config.json"))
                || has_json_mcp_server(&home.join(".gemini/antigravity-cli/mcp_config.json"))
        }
        Client::GeminiCli => has_json_mcp_server(&home.join(".gemini/settings.json")),
    }
}

fn connected_overrides() -> &'static std::sync::Mutex<HashSet<Client>> {
    static CONNECTED: std::sync::OnceLock<std::sync::Mutex<HashSet<Client>>> =
        std::sync::OnceLock::new();
    CONNECTED.get_or_init(Default::default)
}

/// Whether `whats-pro` is already connected in `client`.
pub fn is_connected(client: Client) -> bool {
    if connected_overrides()
        .lock()
        .is_ok_and(|set| set.contains(&client))
    {
        return true;
    }
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return false;
    };
    let desktop = claude_desktop_config();
    config_has_server(client, &home, desktop.as_deref())
}

#[derive(Default)]
struct StatusCache {
    updated: Option<std::time::Instant>,
    states: std::collections::HashMap<Client, (bool, bool)>,
}

fn status_cache() -> &'static std::sync::Mutex<StatusCache> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<StatusCache>> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Returns `(installed, connected)` for `client`, cached briefly so drawing
/// the Settings page never hits disk on every frame.
pub fn client_status(client: Client) -> (bool, bool) {
    let mut cache = status_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fresh = cache
        .updated
        .is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(5));
    if !fresh {
        cache.states.clear();
        for item in Client::ALL {
            cache
                .states
                .insert(item, (item.is_installed(), is_connected(item)));
        }
        cache.updated = Some(std::time::Instant::now());
    }
    let (_, mut connected) = cache.states.get(&client).copied().unwrap_or((false, false));
    if connected_overrides()
        .lock()
        .is_ok_and(|set| set.contains(&client))
    {
        connected = true;
    }
    let installed = cache.states.get(&client).is_some_and(|(inst, _)| *inst);
    (installed, connected)
}

fn mark_connected(client: Client) {
    if let Ok(mut set) = connected_overrides().lock() {
        set.insert(client);
    }
    if let Ok(mut cache) = status_cache().lock() {
        cache.updated = None;
    }
}

/// Registers Whats Pro as the `whats-pro` MCP server in `client`.
pub fn connect_client(client: Client) -> Result<(), String> {
    if client == Client::ClaudeDesktop {
        connect_claude_desktop()?;
        mark_connected(client);
        return Ok(());
    }
    let binary = client
        .binary()
        .ok_or_else(|| format!("{} is not installed on this computer.", client.label()))?;
    let executable =
        std::env::current_exe().map_err(|error| format!("Could not locate this app: {error}"))?;
    let args = client
        .mcp_add_args(&executable)
        .expect("CLI client has mcp add args");
    let output = std::process::Command::new(&binary)
        .args(&args)
        .env("PATH", crate::ai_tools::path())
        .output()
        .map_err(|error| format!("Could not run {}: {error}", client.label()))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success()
        && !stderr.to_lowercase().contains("already exists")
        && !stdout.to_lowercase().contains("already exists")
    {
        let detail = stderr.trim();
        let detail = if detail.is_empty() {
            stdout.trim()
        } else {
            detail
        };
        return Err(format!("Could not connect {}: {detail}", client.label()));
    }
    mark_connected(client);
    Ok(())
}

/// Standard `mcpServers` JSON block for Cursor, Windsurf or any other MCP client.
pub fn standard_config_json(executable: &Path) -> String {
    let command = serde_json::to_string(&executable.to_string_lossy()).expect("string serializes");
    format!(r#"{{"mcpServers":{{"{SERVER_NAME}":{{"command":{command},"args":["mcp"]}}}}}}"#)
}

/// Friendly display name for a client recorded in `mcp-log.jsonl`.
pub fn friendly_client_name(raw: &str) -> &str {
    let lower = raw.trim().to_lowercase();
    if lower.contains("claude-code") {
        "Claude Code"
    } else if lower.contains("claude") {
        "Claude"
    } else if lower.contains("codex") {
        "Codex"
    } else if lower.contains("antigravity") || lower == "agy" {
        "Antigravity"
    } else if lower.contains("gemini") {
        "Gemini"
    } else if lower.is_empty() || lower == "unknown" {
        "MCP"
    } else {
        raw.trim()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sending_is_listed_only_when_allowed() {
        let listed = |permissions: &Permissions| -> Vec<String> {
            tools(permissions)
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
                .filter(|name| name.ends_with("_message"))
                .collect()
        };
        let mut permissions = Permissions::default();
        assert_eq!(listed(&permissions), ["draft_message"]);
        permissions.send = true;
        assert_eq!(listed(&permissions), ["draft_message", "send_message"]);
        permissions.messages = false;
        permissions.send = false;
        assert!(listed(&permissions).is_empty());
    }

    #[test]
    fn initialize_and_list_tools() {
        let bridge = Bridge::new("/nonexistent", "/nonexistent");
        let reply = answer(
            &bridge,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}}),
        )
        .unwrap();
        assert_eq!(reply["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(reply["result"]["serverInfo"]["name"], "whats-pro");
        let tools = answer(
            &bridge,
            &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        )
        .unwrap();
        let names: Vec<&str> = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "list_media",
                "transcribe_chat",
                "get_media",
                "list_chats",
                "read_chat",
                "recent_activity",
                "search_messages",
                "list_projects",
                "project_overview",
                "project_notes",
                "list_calls",
                "read_call",
                "list_leads",
                "read_lead",
                "create_project",
                "update_project",
                "draft_message",
                "add_note",
                "add_task",
                "resolve_item",
                "reopen_item",
                "run_analysis",
                "create_lead",
                "update_lead",
                "add_lead_note",
                "link_chat_to_lead",
            ]
        );
        assert!(
            answer(
                &bridge,
                &json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
            )
            .is_none()
        );
        let unknown = answer(
            &bridge,
            &json!({"jsonrpc": "2.0", "id": 3, "method": "nope"}),
        )
        .unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
    }

    #[test]
    fn permissions_have_safe_defaults_and_apply_live() {
        let root = tempfile::tempdir().unwrap();
        let bridge = Bridge::new(root.path(), root.path());
        let defaults = Permissions::load(root.path());
        assert!(defaults.enabled);
        assert!(defaults.chats);
        assert!(defaults.projects);
        assert!(defaults.calls);
        assert!(!defaults.call_transcripts, "transcripts are off by default");
        assert!(defaults.leads);
        assert!(defaults.exclude_personal_and_ignored);

        for (tool, disable) in [
            (
                "list_chats",
                (|p: &mut Permissions| p.chats = false) as fn(&mut Permissions),
            ),
            ("list_projects", |p| p.projects = false),
            ("project_overview", |p| p.projects = false),
            ("project_notes", |p| p.projects = false),
            ("list_calls", |p| p.calls = false),
            ("read_call", |p| p.calls = false),
            ("list_leads", |p| p.leads = false),
            ("read_lead", |p| p.leads = false),
        ] {
            let mut permissions = defaults.clone();
            disable(&mut permissions);
            permissions.save(root.path()).unwrap();
            let listed = answer(
                &bridge,
                &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
            )
            .unwrap();
            let names: Vec<&str> = listed["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["name"].as_str().unwrap())
                .collect();
            assert!(!names.contains(&tool), "{tool} should be hidden");
            let denied = answer(
                &bridge,
                &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": tool}}),
            )
            .unwrap();
            assert_eq!(denied["result"]["isError"], true);
            let message = denied["result"]["content"][0]["text"].as_str().unwrap();
            assert!(message.contains("Whats Pro > Ajustes > Conectar con IA"));
        }

        // Turning off the master switch hides every tool.
        let mut permissions = defaults;
        permissions.enabled = false;
        permissions.save(root.path()).unwrap();
        let listed = answer(
            &bridge,
            &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}),
        )
        .unwrap();
        assert!(listed["result"]["tools"].as_array().unwrap().is_empty());
    }

    #[test]
    fn project_tools_read_overview_ticks_and_notes_from_disk() {
        use crate::projects::{
            Analysis, Finding, Journal, Member, Mode, NoteKind, Project, ProjectMap, Source, Ticks,
            Verdict,
        };
        let root = tempfile::tempdir().unwrap();
        let config = root.path();
        let mut map = ProjectMap::default();
        map.projects.push(Project {
            id: "bordados-jas".into(),
            name: "Bordados Jas".into(),
            color: "#25d366".into(),
            sources: vec![Source {
                chat: "123@g.us".into(),
                mode: Mode::All,
            }],
            chats: Vec::new(),
            context: "Fábrica de guardapolvos y bordados.".into(),
            members: vec![Member {
                id: "5491100001111@s.whatsapp.net".into(),
                name: "Diego".into(),
                role: "Taller".into(),
                about: "Entrega los viernes".into(),
                aliases: Vec::new(),
                chats: Vec::new(),
            }],
        });
        map.save(config).unwrap();

        let done_finding = Finding {
            id: "todo-done".into(),
            text: "Mandar matriz vieja".into(),
            ..Finding::default()
        };
        let open_finding = Finding {
            id: "todo-open".into(),
            text: "Confirmar 500 guardapolvos".into(),
            who: Some("Diego".into()),
            due: Some("viernes".into()),
            ..Finding::default()
        };
        let mut analysis = Analysis {
            updated: 1_760_000_000,
            todo: vec![done_finding.clone(), open_finding],
            follow_up: vec![Finding {
                id: "fu-1".into(),
                text: "Respuesta por hilo azul".into(),
                who: Some("Proveedor".into()),
                ..Finding::default()
            }],
            leaks: vec![Finding {
                id: "leak-1".into(),
                text: "Pedido sin cotizar".into(),
                ..Finding::default()
            }],
            needs: vec![Finding {
                id: "need-1".into(),
                text: "Comprar entretela".into(),
                ..Finding::default()
            }],
            highlights: vec![Finding {
                id: "hi-1".into(),
                text: "Cerramos entrega mensual".into(),
                ..Finding::default()
            }],
            ..Analysis::default()
        };
        analysis
            .summaries
            .insert("today".into(), "Se avanzó con el taller.".into());
        analysis
            .summaries
            .insert("week".into(), "Semana de entregas.".into());
        analysis.save(config, "bordados-jas").unwrap();

        let mut ticks = Ticks::default();
        ticks.judge(&done_finding, Verdict::Done);
        ticks.save(config, "bordados-jas").unwrap();

        let mut journal = Journal::default();
        journal.add_kind("Hablé con Diego por los talles", true, NoteKind::Log);
        journal.save(config, "bordados-jas").unwrap();

        let bridge = Bridge::new(config, config);

        let list = bridge.call("list_projects", &json!({})).unwrap();
        assert!(list.contains("Bordados Jas | bordados-jas | 1 sources"));
        assert!(list.contains("todo: 1 | follow_up: 1 | slipping: 1 | needs: 1 | highlights: 1"));

        let overview = bridge
            .call("project_overview", &json!({"project": "Bordados"}))
            .unwrap();
        assert!(overview.contains("# Bordados Jas (bordados-jas)"));
        assert!(overview.contains("Fábrica de guardapolvos y bordados."));
        assert!(
            overview.contains("Diego (5491100001111@s.whatsapp.net) - Taller: Entrega los viernes")
        );
        assert!(overview.contains("- Today: Se avanzó con el taller."));
        assert!(overview.contains("- Week: Semana de entregas."));
        assert!(overview.contains("Confirmar 500 guardapolvos (who: Diego, due: viernes, id: "));
        assert!(
            !overview.contains("Mandar matriz vieja"),
            "done findings are skipped"
        );

        let notes = bridge
            .call(
                "project_notes",
                &json!({"project": "bordados-jas", "days": 7}),
            )
            .unwrap();
        assert!(notes.contains("Hablé con Diego por los talles"));
        assert!(notes.contains("(log, spoken)"));
    }

    #[test]
    fn call_tools_hide_private_calls_and_gate_transcripts_by_permission() {
        use crate::call_library::{Call, CallAnalysis, Commitment, Library, Origin, Transcript};
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config");
        let state = root.path().join("state");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&state).unwrap();

        let now = now();
        let library = Library {
            calls: vec![
                Call {
                    id: "call-1".into(),
                    title: "Llamada con Diego".into(),
                    at: now - 600,
                    seconds: 185,
                    origin: Origin::Recorded,
                    file: "call-1.wav".into(),
                    project: Some("bordados-jas".into()),
                    lead: Some("lead-1".into()),
                    transcripts: vec![Transcript {
                        engine: "Whisper".into(),
                        text: "Hola Diego, necesitamos 500 unidades para el viernes.".into(),
                        at: now - 500,
                    }],
                    analysis: Some(CallAnalysis {
                        summary: "Acordamos 500 unidades para el viernes.".into(),
                        decisions: vec!["Usar tela arciel".into()],
                        commitments: vec![Commitment {
                            who: "Diego".into(),
                            what: "Entregar el viernes".into(),
                            due: Some("2026-10-23".into()),
                        }],
                        tasks: vec!["Mandar orden de compra".into()],
                        next_steps: vec!["Revisar muestras el miércoles".into()],
                        at: now - 400,
                    }),
                    chat: None,
                    account: None,
                    started_at: now - 600,
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
                    pending_review: false,
                },
                Call {
                    id: "call-private".into(),
                    title: "Llamada secreta".into(),
                    at: now - 300,
                    seconds: 60,
                    origin: Origin::Recorded,
                    file: "call-private.wav".into(),
                    project: None,
                    lead: None,
                    transcripts: Vec::new(),
                    analysis: None,
                    chat: None,
                    account: None,
                    started_at: now - 300,
                    video: false,
                    direction: None,
                    group: None,
                    participants: Vec::new(),
                    private: true,
                    notice_ms: None,
                    levels: None,
                    stereo: false,
                    recovered: false,
                    pending_read: false,
                    pending_review: false,
                },
            ],
        };
        library.save(&state).unwrap();

        let bridge = Bridge::new(&config, &state);

        let list = bridge.call("list_calls", &json!({"days": 7})).unwrap();
        assert!(list.contains("call-1 | Llamada con Diego"));
        assert!(list.contains("3:05"));
        assert!(list.contains("Acordamos 500 unidades para el viernes."));
        assert!(
            !list.contains("call-private") && !list.contains("Llamada secreta"),
            "private calls are never listed"
        );
        assert!(
            bridge
                .call("read_call", &json!({"call": "call-private"}))
                .is_err(),
            "private calls can never be read"
        );

        // By default call_transcripts is false: summary and commitments show, transcript does not.
        let detail = bridge
            .call("read_call", &json!({"call": "Llamada con Diego"}))
            .unwrap();
        assert!(detail.contains("Acordamos 500 unidades para el viernes."));
        assert!(detail.contains("Usar tela arciel"));
        assert!(detail.contains("Diego: Entregar el viernes (due 2026-10-23)"));
        assert!(detail.contains("Mandar orden de compra"));
        assert!(detail.contains("Revisar muestras el miércoles"));
        assert!(
            !detail.contains("Hola Diego, necesitamos 500 unidades"),
            "transcript is omitted when call_transcripts is off"
        );

        // Enabling call_transcripts includes the transcript live.
        let mut permissions = Permissions::load(&config);
        permissions.call_transcripts = true;
        permissions.save(&config).unwrap();
        let with_transcript = bridge
            .call("read_call", &json!({"call": "call-1"}))
            .unwrap();
        assert!(with_transcript.contains("## Transcript (Whisper)"));
        assert!(with_transcript.contains("Hola Diego, necesitamos 500 unidades para el viernes."));
    }

    #[test]
    fn lead_tools_list_and_read_stage_amount_next_action_and_history() {
        use crate::leads::{Kind, Lead, Leads, Stage};
        let root = tempfile::tempdir().unwrap();
        let config = root.path();

        let mut lead = Lead::new(
            "bordados-jas",
            "Farmacia Sur",
            Some("5491122334455@s.whatsapp.net".into()),
            Some("5491122334455".into()),
        );
        lead.id = "lead-sur".into();
        lead.set_stage(Stage::Quoted, None);
        lead.value = Some("$ 450.000".into());
        lead.set_next("Confirmar talles", Some("2026-10-22".into()));
        lead.add(Kind::Note, "Pidieron muestra de guardapolvo");

        let mut leads = Leads::default();
        leads.leads.push(lead);
        leads.save(config).unwrap();

        let bridge = Bridge::new(config, config);

        let listed = bridge
            .call(
                "list_leads",
                &json!({"project": "bordados-jas", "stage": "quoted"}),
            )
            .unwrap();
        assert!(listed.contains("lead-sur | Farmacia Sur"));
        assert!(listed.contains("stage: quoted"));
        assert!(listed.contains("amount: $ 450.000"));
        assert!(listed.contains("next: Confirmar talles | due: 2026-10-22"));

        let empty = bridge
            .call("list_leads", &json!({"stage": "customer"}))
            .unwrap();
        assert_eq!(empty, "No leads match.");

        let detail = bridge
            .call("read_lead", &json!({"lead": "Farmacia Sur"}))
            .unwrap();
        assert!(detail.contains("# Farmacia Sur (lead-sur)"));
        assert!(detail.contains("- Stage: quoted"));
        assert!(detail.contains("- Amount: $ 450.000"));
        assert!(detail.contains("- Next action: Confirmar talles (due 2026-10-22)"));
        assert!(detail.contains("Pidieron muestra de guardapolvo"));
    }

    #[test]
    fn access_log_records_client_and_caps_at_thousand_lines() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path();
        let bridge = Bridge::new(config, config);

        // Before initialize, client defaults to "unknown".
        let _ = answer(
            &bridge,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "list_projects"}}),
        )
        .unwrap();

        // After initialize with clientInfo.name, tool calls record that client.
        let _ = answer(
            &bridge,
            &json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "initialize",
                "params": {"clientInfo": {"name": "claude-code", "version": "1.0.0"}}
            }),
        )
        .unwrap();
        let _ = answer(
            &bridge,
            &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "list_leads"}}),
        )
        .unwrap();

        let recent = recent_log(config, 5);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].tool, "list_leads");
        assert_eq!(recent[0].scope, "leads");
        assert_eq!(recent[0].client, "claude-code");
        assert_eq!(recent[1].tool, "list_projects");
        assert_eq!(recent[1].scope, "projects");
        assert_eq!(recent[1].client, "unknown");

        // Cap at 1,000 lines, trimming the oldest entries.
        for index in 0..1005 {
            append_log(
                config,
                &LogEntry {
                    at: 1_760_000_000 + index,
                    tool: format!("tool-{index}"),
                    scope: "projects".into(),
                    client: "claude-code".into(),
                },
            )
            .unwrap();
        }
        let raw = std::fs::read_to_string(log_file(config)).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 1000);
        let newest = recent_log(config, 1);
        assert_eq!(newest[0].tool, "tool-1004");
        let oldest: LogEntry = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(oldest.tool, "tool-5");
    }

    #[test]
    fn connecting_keeps_other_servers_and_backs_up() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("claude_desktop_config.json");
        std::fs::write(
            &path,
            r#"{"mcpServers": {"other": {"command": "x"}}, "theme": "dark"}"#,
        )
        .unwrap();
        connect_at(
            &path,
            Path::new("/Applications/Whats Pro.app/Contents/MacOS/zapfast"),
        )
        .unwrap();
        let config: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(config["theme"], "dark");
        assert_eq!(config["mcpServers"]["other"]["command"], "x");
        assert_eq!(config["mcpServers"]["whats-pro"]["args"], json!(["mcp"]));
        assert!(path.with_extension("json.bak").exists());
        // A broken file is never overwritten.
        std::fs::write(&path, "{ not json").unwrap();
        assert!(connect_at(&path, Path::new("/x")).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn client_args_status_and_standard_config_json() {
        let exe = Path::new("/Applications/Whats Pro.app/Contents/MacOS/zapfast");
        assert_eq!(Client::ClaudeDesktop.mcp_add_args(exe), None);
        assert_eq!(
            Client::ClaudeCode.mcp_add_args(exe).unwrap(),
            [
                "mcp",
                "add",
                "--scope",
                "user",
                "whats-pro",
                "--",
                "/Applications/Whats Pro.app/Contents/MacOS/zapfast",
                "mcp"
            ]
        );
        assert_eq!(
            Client::Codex.mcp_add_args(exe).unwrap(),
            [
                "mcp",
                "add",
                "whats-pro",
                "--",
                "/Applications/Whats Pro.app/Contents/MacOS/zapfast",
                "mcp"
            ]
        );
        assert_eq!(
            Client::Antigravity.mcp_add_args(exe).unwrap(),
            [
                "mcp",
                "add",
                "whats-pro",
                "/Applications/Whats Pro.app/Contents/MacOS/zapfast",
                "mcp"
            ]
        );
        assert_eq!(
            Client::GeminiCli.mcp_add_args(exe).unwrap(),
            [
                "mcp",
                "add",
                "whats-pro",
                "/Applications/Whats Pro.app/Contents/MacOS/zapfast",
                "mcp"
            ]
        );

        let snippet = standard_config_json(exe);
        assert_eq!(
            snippet,
            r#"{"mcpServers":{"whats-pro":{"command":"/Applications/Whats Pro.app/Contents/MacOS/zapfast","args":["mcp"]}}}"#
        );

        let home = tempfile::tempdir().unwrap();
        let desktop = home.path().join("claude_desktop_config.json");
        for client in Client::ALL {
            assert!(!config_has_server(client, home.path(), Some(&desktop)));
        }

        connect_at(&desktop, exe).unwrap();
        assert!(config_has_server(
            Client::ClaudeDesktop,
            home.path(),
            Some(&desktop)
        ));

        std::fs::write(home.path().join(".claude.json"), &snippet).unwrap();
        assert!(config_has_server(
            Client::ClaudeCode,
            home.path(),
            Some(&desktop)
        ));

        std::fs::create_dir_all(home.path().join(".codex")).unwrap();
        std::fs::write(
            home.path().join(".codex/config.toml"),
            "[mcp_servers.whats-pro]\ncommand = \"zapfast\"\n",
        )
        .unwrap();
        assert!(config_has_server(
            Client::Codex,
            home.path(),
            Some(&desktop)
        ));

        std::fs::create_dir_all(home.path().join(".gemini/antigravity")).unwrap();
        std::fs::write(
            home.path().join(".gemini/antigravity/mcp_config.json"),
            &snippet,
        )
        .unwrap();
        assert!(config_has_server(
            Client::Antigravity,
            home.path(),
            Some(&desktop)
        ));

        std::fs::write(home.path().join(".gemini/settings.json"), &snippet).unwrap();
        assert!(config_has_server(
            Client::GeminiCli,
            home.path(),
            Some(&desktop)
        ));

        assert_eq!(friendly_client_name("claude-code"), "Claude Code");
        assert_eq!(friendly_client_name("claude-ai"), "Claude");
        assert_eq!(friendly_client_name("codex-cli"), "Codex");
        assert_eq!(friendly_client_name("antigravity"), "Antigravity");
        assert_eq!(friendly_client_name("gemini-cli"), "Gemini");
    }
}
