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
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chats => "chats",
            Self::Projects => "projects",
            Self::Calls => "calls",
            Self::Leads => "leads",
        }
    }

    fn settings_label(self) -> &'static str {
        match self {
            Self::Chats => "Chats",
            Self::Projects => "Proyectos y notas",
            Self::Calls => "Llamadas (resúmenes)",
            Self::Leads => "Leads",
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
        "list_chats" | "read_chat" | "recent_activity" | "search_messages" => Some(Scope::Chats),
        "list_projects" | "project_overview" | "project_notes" => Some(Scope::Projects),
        "list_calls" | "read_call" => Some(Scope::Calls),
        "list_leads" | "read_lead" => Some(Scope::Leads),
        _ => None,
    }
}

/// All tools with their JSON schemas.
fn all_tools() -> Vec<Value> {
    vec![
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
                    "limit": {"type": "number", "description": "At most this many messages (default 200, max 2000)."}
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
                    "limit": {"type": "number", "description": "At most this many messages (default 1500, max 5000)."}
                }
            },
            "annotations": {"readOnlyHint": true}
        }),
        json!({
            "name": "search_messages",
            "description": "Search WhatsApp messages by text, caption or file name, newest first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "chat": {"type": "string", "description": "Only in this chat (name or id)."},
                    "limit": {"type": "number", "description": "Default 50, max 500."}
                },
                "required": ["query"]
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
    ]
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
    if let Some(transcript) = message["transcript"].as_str().filter(|t| !t.is_empty()) {
        text = format!("{text} [transcript: {transcript}]");
    }
    if let Some(reply) = message["reply_to"]["text"].as_str() {
        text = format!("(replying to \"{reply}\") {text}");
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
    if details.is_empty() {
        format!("- {}", finding.text)
    } else {
        format!("- {} ({})", finding.text, details.join(", "))
    }
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

        let sections = [
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
            "read_chat" => {
                let excluded = self.excluded_chats(&permissions);
                let wanted = text(arguments, "chat").ok_or("`chat` is required.")?;
                let (id, chat_name) = self.resolve(&wanted, &excluded)?;
                let limit = number(arguments, "limit")
                    .unwrap_or(200.0)
                    .clamp(1.0, 2000.0) as usize;
                let mut pairs = vec![("chat", id), ("limit", limit.to_string())];
                if let Some(hours) = number(arguments, "since_hours") {
                    pairs.push(("since", (now() - (hours * 3600.0) as i64).to_string()));
                }
                let messages = self.get(&format!("/messages?{}", query(&pairs)))?;
                let lines: Vec<String> = messages
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|message| line(message, false))
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
                let mut by_chat: Vec<(String, Vec<String>)> = Vec::new();
                for message in messages.as_array().into_iter().flatten() {
                    let chat = message["chat"].as_str().unwrap_or("");
                    if excluded.contains(chat) {
                        continue;
                    }
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
                let needle = text(arguments, "query").ok_or("`query` is required.")?;
                let limit = number(arguments, "limit").unwrap_or(50.0).clamp(1.0, 500.0) as usize;
                let mut pairs = vec![("q", needle), ("limit", limit.to_string())];
                if let Some(wanted) = text(arguments, "chat") {
                    pairs.push(("chat", self.resolve(&wanted, &excluded)?.0));
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
                "instructions": "Read-only access to the user's WhatsApp chats, projects, calls and leads in Whats Pro. Chat tools require Whats Pro to be open; projects, calls and leads work from disk."
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(overview.contains("Confirmar 500 guardapolvos (who: Diego, due: viernes)"));
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
}
