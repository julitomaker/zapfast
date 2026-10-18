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
    #[allow(dead_code)]
    state_dir: PathBuf,
}

impl Bridge {
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

    fn call(&self, name: &str, arguments: &Value) -> Result<String, String> {
        let Some(scope) = tool_scope(name) else {
            return Err(format!("Unknown tool {name}."));
        };
        let permissions = self.permissions();
        permissions.check(scope)?;
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
            _ => Err(format!("Unknown tool {name}.")),
        }
    }
}

/// Answers one JSON-RPC request; `None` for notifications.
fn answer(bridge: &Bridge, request: &Value) -> Option<Value> {
    let id = request.get("id")?.clone();
    let method = request["method"].as_str().unwrap_or("");
    let result = match method {
        "initialize" => json!({
            "protocolVersion": request["params"]["protocolVersion"].as_str().unwrap_or(PROTOCOL),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": SERVER_NAME, "title": "Whats Pro", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "Read-only access to the user's WhatsApp chats, projects, calls and leads in Whats Pro. Chat tools require Whats Pro to be open; projects, calls and leads work from disk."
        }),
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
    let bridge = Bridge {
        config_dir: config_dir.to_path_buf(),
        state_dir: state_dir.to_path_buf(),
    };
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
        let bridge = Bridge {
            config_dir: PathBuf::from("/nonexistent"),
            state_dir: PathBuf::from("/nonexistent"),
        };
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
                "search_messages"
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
        let bridge = Bridge {
            config_dir: root.path().to_path_buf(),
            state_dir: root.path().to_path_buf(),
        };
        let defaults = Permissions::load(root.path());
        assert!(defaults.enabled);
        assert!(defaults.chats);
        assert!(defaults.projects);
        assert!(defaults.calls);
        assert!(!defaults.call_transcripts, "transcripts are off by default");
        assert!(defaults.leads);
        assert!(defaults.exclude_personal_and_ignored);

        // Disabling the chats scope removes chat tools from tools/list and
        // rejects calls with instructions to enable it in settings.
        let mut permissions = defaults.clone();
        permissions.chats = false;
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
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert!(!names.contains(&"list_chats"));
        let denied = answer(
            &bridge,
            &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "list_chats"}}),
        )
        .unwrap();
        assert_eq!(denied["result"]["isError"], true);
        let message = denied["result"]["content"][0]["text"].as_str().unwrap();
        assert!(message.contains("Whats Pro > Ajustes > Conectar con IA"));

        // Turning off the master switch hides every tool.
        permissions.chats = true;
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
