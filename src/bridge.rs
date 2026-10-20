//! A local, read-only HTTP bridge to the message archive, for tools on this
//! computer (a dashboard, an assistant) that organize chats.
//!
//! It listens on the loopback interface only and answers `GET` requests that
//! carry `Authorization: Bearer <token>`. The port and token live in
//! `bridge.json` in the config folder (created with mode 0600 on first run;
//! set `"enabled": false` there to turn the bridge off). Each account's
//! archive is opened through its own query-only connection, so the bridge can
//! never write, send, or mark anything read. Locked chats are never served.
//!
//! Routes (all JSON, `account` defaults to `1`):
//! - `/health`
//! - `/accounts`
//! - `/chats?account=`
//! - `/messages?account=&chat=&since=&before=&limit=`: with `since` (Unix
//!   seconds), the first `limit` messages from then on; otherwise the newest
//!   `limit` before `before`. Without `chat`, across every chat.
//! - `/search?account=&q=&chat=&limit=`

use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tiny_http::{Header, Method, Request, Response, Server};

use crate::archive::Archive;

pub const DEFAULT_PORT: u16 = 47120;
/// The largest attachment `/media` serves.
const MEDIA_LIMIT: u64 = 40 * 1024 * 1024;
/// How long `/media` waits for an attachment it asked WhatsApp for.
const MEDIA_WAIT: std::time::Duration = std::time::Duration::from_secs(40);

/// How long `/transcript` waits for a transcription it asked for.
const TRANSCRIPT_WAIT: std::time::Duration = std::time::Duration::from_secs(150);

type Commands = tokio::sync::mpsc::UnboundedSender<crate::backend::Command>;

/// Per account, the app's command channel: how `/media` asks for a download
/// and `/transcript` for a transcription, as opening the chat would.
fn backends() -> &'static std::sync::Mutex<HashMap<String, Commands>> {
    static BACKENDS: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Commands>>> =
        std::sync::OnceLock::new();
    BACKENDS.get_or_init(Default::default)
}

/// A header value that survives any UTF-8 text: percent-encoded.
fn utf8_header(text: &str) -> Vec<u8> {
    form_urlencoded::byte_serialize(text.as_bytes())
        .collect::<String>()
        .into_bytes()
}

/// Lets the bridge ask `account`'s backend to download or transcribe.
pub fn register_backend(account: &str, commands: Commands) {
    backends()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(account.to_owned(), commands);
}

fn send(account: &str, command: crate::backend::Command) -> bool {
    backends()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(account)
        .is_some_and(|commands| commands.send(command).is_ok())
}
const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 5000;

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    #[serde(default = "default_port")]
    pub port: u16,
    pub token: String,
}

fn enabled_by_default() -> bool {
    true
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

/// Reads `bridge.json`, creating it with a fresh token when missing.
pub fn config(config_dir: &Path) -> anyhow::Result<Config> {
    let path = config_dir.join("bridge.json");
    if let Ok(text) = fs::read_to_string(&path) {
        return Ok(serde_json::from_str(&text)?);
    }
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| anyhow::anyhow!("{error}"))?;
    let config = Config {
        enabled: true,
        port: DEFAULT_PORT,
        token: bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
    };
    fs::create_dir_all(config_dir)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(&path)?
        .write_all(serde_json::to_string_pretty(&config)?.as_bytes())?;
    Ok(config)
}

/// Starts the bridge on a thread of its own. Failures are logged, never fatal:
/// the app works the same without it.
pub fn start(config_dir: &Path, state_dir: &Path) {
    let config = match config(config_dir) {
        Ok(config) => config,
        Err(error) => {
            log::warn!("bridge: could not read or create its config: {error}");
            return;
        }
    };
    if !config.enabled {
        log::info!("bridge: disabled in bridge.json");
        return;
    }
    let server = match Server::http(("127.0.0.1", config.port)) {
        Ok(server) => server,
        Err(error) => {
            log::warn!(
                "bridge: could not listen on 127.0.0.1:{}: {error}",
                config.port
            );
            return;
        }
    };
    log::info!("bridge: listening on 127.0.0.1:{}", config.port);
    let bridge = Bridge {
        token: config.token,
        port: config.port,
        accounts_dir: state_dir.join("accounts"),
        config_dir: config_dir.to_path_buf(),
        open: HashMap::new(),
    };
    let spawned = std::thread::Builder::new()
        .name("bridge".into())
        .spawn(move || bridge.serve(server));
    if let Err(error) = spawned {
        log::warn!("bridge: could not start its thread: {error}");
    }
}

struct Bridge {
    token: String,
    port: u16,
    accounts_dir: PathBuf,
    /// Where settings.json says which engine transcribes.
    config_dir: PathBuf,
    open: HashMap<String, Archive>,
}

type Reply = (u16, serde_json::Value);

fn error(status: u16, message: &str) -> Reply {
    (status, serde_json::json!({ "error": message }))
}

impl Bridge {
    fn serve(mut self, server: Server) {
        for mut request in server.incoming_requests() {
            if request.url().split('?').next() == Some("/media") {
                let response = match self.media(&request) {
                    Ok((bytes, mime, path, name)) => Response::from_data(bytes)
                        .with_header(
                            Header::from_bytes("X-Path", utf8_header(&path))
                                .expect("encoded header"),
                        )
                        .with_header(
                            Header::from_bytes("X-File-Name", utf8_header(&name))
                                .expect("encoded header"),
                        )
                        .with_header(
                            Header::from_bytes("Content-Type", mime.as_bytes()).unwrap_or_else(
                                |_| {
                                    Header::from_bytes("Content-Type", "application/octet-stream")
                                        .expect("static header")
                                },
                            ),
                        )
                        .with_header(
                            Header::from_bytes("Cache-Control", "no-store").expect("static header"),
                        ),
                    Err((status, body)) => Response::from_data(body.to_string().into_bytes())
                        .with_status_code(status)
                        .with_header(
                            Header::from_bytes("Content-Type", "application/json; charset=utf-8")
                                .expect("static header"),
                        ),
                };
                if let Err(error) = request.respond(response) {
                    log::debug!("bridge: could not answer: {error}");
                }
                continue;
            }
            let (status, body) = if request.url().split('?').next() == Some("/action") {
                self.action(&mut request)
            } else {
                self.answer(&request)
            };
            let response = Response::from_string(body.to_string())
                .with_status_code(status)
                .with_header(
                    Header::from_bytes("Content-Type", "application/json; charset=utf-8")
                        .expect("static header"),
                )
                .with_header(
                    Header::from_bytes("Cache-Control", "no-store").expect("static header"),
                );
            if let Err(error) = request.respond(response) {
                log::debug!("bridge: could not answer: {error}");
            }
        }
    }

    fn header<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
        request
            .headers()
            .iter()
            .find(|header| header.field.as_str().as_str().eq_ignore_ascii_case(name))
            .map(|header| header.value.as_str())
    }

    fn authorized(&self, request: &Request) -> bool {
        Self::header(request, "Authorization")
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|given| bool::from(given.as_bytes().ct_eq(self.token.as_bytes())))
    }

    /// Refuses names other than the loopback ones, so a web page cannot reach
    /// the bridge through a rebound DNS name.
    fn local_host(&self, request: &Request) -> bool {
        Self::header(request, "Host").is_some_and(|host| {
            let host = host.trim();
            let port = self.port.to_string();
            ["127.0.0.1", "localhost", "[::1]"]
                .iter()
                .any(|name| host == *name || host == format!("{name}:{port}"))
        })
    }

    /// Method, host and token, as every route checks them.
    fn refuse(&self, request: &Request) -> Option<Reply> {
        if *request.method() != Method::Get {
            return Some(error(405, "only GET is supported"));
        }
        self.refuse_host_or_token(request)
    }

    fn refuse_host_or_token(&self, request: &Request) -> Option<Reply> {
        if !self.local_host(request) {
            return Some(error(403, "unexpected Host"));
        }
        if !self.authorized(request) {
            return Some(error(401, "missing or wrong bearer token"));
        }
        None
    }

    /// `/media?account=&chat=&id=`: a message's attachment as its bytes,
    /// downloaded from WhatsApp first when it is not on this computer.
    fn media(&mut self, request: &Request) -> Result<(Vec<u8>, String, String, String), Reply> {
        if let Some(refused) = self.refuse(request) {
            return Err(refused);
        }
        let query: HashMap<String, String> = form_urlencoded::parse(
            request
                .url()
                .split_once('?')
                .map_or("", |(_, q)| q)
                .as_bytes(),
        )
        .into_owned()
        .collect();
        let account = query.get("account").map_or("1", String::as_str).to_owned();
        let (Some(chat), Some(id)) = (query.get("chat"), query.get("id")) else {
            return Err(error(400, "chat and id are required"));
        };
        let lookup = |bridge: &mut Self| -> Result<crate::archive::BridgeMedia, Reply> {
            bridge
                .archive(&account)?
                .bridge_media(chat, id)
                .map_err(|failure| {
                    log::warn!("bridge: media lookup failed: {failure}");
                    error(500, "query failed")
                })?
                .ok_or_else(|| error(404, "no such message with an attachment"))
        };
        let mut found = lookup(self)?;
        if found.size > MEDIA_LIMIT {
            return Err(error(413, "the attachment is too large to serve"));
        }
        if found.path.is_none() {
            let asked = send(
                &account,
                crate::backend::Command::Download {
                    manual: true,
                    card: None,
                    chat: chat.clone(),
                    message: id.clone(),
                },
            );
            if !asked {
                return Err(error(503, "Whats Pro cannot download it right now"));
            }
            let started = std::time::Instant::now();
            while found.path.is_none() && started.elapsed() < MEDIA_WAIT {
                std::thread::sleep(std::time::Duration::from_millis(400));
                found = lookup(self)?;
            }
        }
        let Some(path) = found.path else {
            return Err(error(
                504,
                "WhatsApp did not deliver the attachment in time (the phone may need to be online); try again",
            ));
        };
        let bytes = fs::read(&path).map_err(|_| error(500, "the file could not be read"))?;
        let mime = if found.mime.is_empty() {
            "application/octet-stream".to_owned()
        } else {
            found.mime
        };
        let name = found.file_name.unwrap_or_default();
        Ok((bytes, mime, path.display().to_string(), name))
    }

    /// `/transcript?account=&chat=&id=`: what a voice message, audio or video
    /// says. Transcribed now with the engine chosen in Settings (and kept)
    /// when it never was.
    fn transcript(&mut self, query: &HashMap<String, String>) -> Reply {
        let account = query.get("account").map_or("1", String::as_str).to_owned();
        let (Some(chat), Some(id)) = (query.get("chat"), query.get("id")) else {
            return error(400, "chat and id are required");
        };
        let archive = match self.archive(&account) {
            Ok(archive) => archive,
            Err(reply) => return reply,
        };
        let media = match archive.bridge_media(chat, id) {
            Ok(Some(media)) => media,
            Ok(None) => return error(404, "no such message with an attachment"),
            Err(_) => return error(500, "query failed"),
        };
        if !matches!(media.kind.as_str(), "audio" | "video") {
            return error(
                400,
                "only voice messages, audio and videos have speech to transcribe",
            );
        }
        let reply = |text: String| (200, serde_json::json!({ "kind": media.kind, "text": text }));
        if let Ok(Some(text)) = archive.bridge_transcript(chat, id) {
            return reply(text);
        }
        let engines =
            crate::settings::Settings::load(&self.config_dir.join("settings.json")).media_engines;
        let asked = send(
            &account,
            crate::backend::Command::Transcribe {
                chat: chat.clone(),
                id: id.clone(),
                engines,
            },
        );
        if !asked {
            return error(503, "Whats Pro cannot transcribe right now");
        }
        let started = std::time::Instant::now();
        while started.elapsed() < TRANSCRIPT_WAIT {
            std::thread::sleep(std::time::Duration::from_millis(800));
            let archive = match self.archive(&account) {
                Ok(archive) => archive,
                Err(reply) => return reply,
            };
            if let Ok(Some(text)) = archive.bridge_transcript(chat, id) {
                return reply(text);
            }
        }
        error(504, "still transcribing; ask again in a minute")
    }

    /// `/backlog?account=&chat=&since=`: starts reading every voice message,
    /// audio, video and picture of a chat that nothing was read out of yet,
    /// and says how many.
    fn backlog(&mut self, query: &HashMap<String, String>) -> Reply {
        let account = query.get("account").map_or("1", String::as_str).to_owned();
        let Some(chat) = query.get("chat").cloned() else {
            return error(400, "chat is required");
        };
        let since = query
            .get("since")
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(0);
        let settings = crate::settings::Settings::load(&self.config_dir.join("settings.json"));
        let chats = vec![chat];
        let pending = match self.archive(&account) {
            Ok(archive) => archive
                .untranscribed(&chats, since, true, 2_000)
                .map(|found| found.len())
                .unwrap_or(0),
            Err(reply) => return reply,
        };
        if pending > 0 {
            let asked = send(
                &account,
                crate::backend::Command::TranscribeBacklog {
                    chats,
                    since,
                    pictures: true,
                    limit: 2_000,
                    engines: settings.media_engines,
                },
            );
            if !asked {
                return error(503, "Whats Pro cannot transcribe right now");
            }
        }
        (200, serde_json::json!({ "started": pending }))
    }

    fn answer(&mut self, request: &Request) -> Reply {
        if let Some(refused) = self.refuse(request) {
            return refused;
        }
        let (path, query) = request.url().split_once('?').unwrap_or((request.url(), ""));
        let query: HashMap<String, String> = form_urlencoded::parse(query.as_bytes())
            .into_owned()
            .collect();
        match path {
            "/health" => (
                200,
                serde_json::json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }),
            ),
            "/accounts" => (200, serde_json::json!(self.accounts())),
            "/chats" | "/messages" | "/search" => self.archive_route(path, &query),
            "/transcript" => self.transcript(&query),
            "/backlog" => self.backlog(&query),
            _ => error(404, "unknown route"),
        }
    }

    /// `POST /action` with `{"action", "args", "client"}`: a change to the
    /// projects, their notes and tasks, or the leads, made by an AI over MCP
    /// (src/project_actions.rs). The only route that writes.
    fn action(&mut self, request: &mut Request) -> Reply {
        if *request.method() != Method::Post {
            return error(405, "only POST is supported here");
        }
        if let Some(refused) = self.refuse_host_or_token(request) {
            return refused;
        }
        let mut body = String::new();
        if std::io::Read::read_to_string(
            &mut std::io::Read::take(request.as_reader(), 256 * 1024),
            &mut body,
        )
        .is_err()
        {
            return error(400, "unreadable body");
        }
        let Ok(body) = serde_json::from_str::<serde_json::Value>(&body) else {
            return error(400, "the body is not JSON");
        };
        let Some(action) = body.get("action").and_then(serde_json::Value::as_str) else {
            return error(400, "`action` is required");
        };
        let args = body.get("args").cloned().unwrap_or_default();
        let client = body
            .get("client")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        match crate::project_actions::apply(&self.config_dir, action, &args, client) {
            Ok(answer) => (200, answer),
            Err(message) => error(422, &message),
        }
    }

    fn accounts(&self) -> Vec<String> {
        let mut accounts: Vec<String> = fs::read_dir(&self.accounts_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.path().join("archive.db").is_file())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        accounts.sort();
        accounts
    }

    fn archive(&mut self, account: &str) -> Result<&Archive, Reply> {
        if !crate::model::AccountId::is_safe(account)
            || !self.accounts().iter().any(|a| a == account)
        {
            return Err(error(404, "unknown account"));
        }
        if !self.open.contains_key(account) {
            let path = self.accounts_dir.join(account).join("archive.db");
            let archive = Archive::open_reader(&path).map_err(|failure| {
                log::warn!("bridge: could not open account {account}'s archive: {failure}");
                error(503, "the archive could not be opened")
            })?;
            self.open.insert(account.to_owned(), archive);
        }
        Ok(&self.open[account])
    }

    fn archive_route(&mut self, path: &str, query: &HashMap<String, String>) -> Reply {
        let number = |name: &str| query.get(name).and_then(|value| value.parse::<i64>().ok());
        let limit = query
            .get("limit")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(DEFAULT_LIMIT)
            .clamp(1, MAX_LIMIT);
        let chat = query
            .get("chat")
            .map(String::as_str)
            .filter(|c| !c.is_empty());
        let account = query.get("account").map_or("1", String::as_str).to_owned();
        let archive = match self.archive(&account) {
            Ok(archive) => archive,
            Err(reply) => return reply,
        };
        let result = match path {
            "/chats" => archive.bridge_chats().map(|chats| serde_json::json!(chats)),
            "/messages" => archive
                .bridge_messages(chat, number("since"), number("before"), limit)
                .map(|messages| serde_json::json!(messages)),
            _ => {
                let filter = crate::archive::SearchFilter {
                    chat,
                    since: number("since"),
                    until: number("until"),
                    from: query
                        .get("from")
                        .map(String::as_str)
                        .filter(|f| !f.is_empty()),
                    kind: query
                        .get("kind")
                        .map(String::as_str)
                        .filter(|k| !k.is_empty()),
                };
                let needle = query.get("q").map_or("", String::as_str);
                archive
                    .bridge_search_filtered(needle, &filter, limit)
                    .map(|messages| serde_json::json!(messages))
            }
        };
        match result {
            Ok(body) => (200, body),
            Err(failure) => {
                log::warn!("bridge: query failed: {failure}");
                // A broken connection is dropped and reopened on the next request.
                self.open.remove(&account);
                error(500, "query failed")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_is_created_once_and_kept_private() {
        let root = tempfile::tempdir().unwrap();
        let first = config(root.path()).unwrap();
        assert!(first.enabled);
        assert_eq!(first.port, DEFAULT_PORT);
        assert_eq!(first.token.len(), 64);
        let again = config(root.path()).unwrap();
        assert_eq!(first.token, again.token);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(root.path().join("bridge.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
