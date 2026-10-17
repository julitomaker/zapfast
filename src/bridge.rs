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
    open: HashMap<String, Archive>,
}

type Reply = (u16, serde_json::Value);

fn error(status: u16, message: &str) -> Reply {
    (status, serde_json::json!({ "error": message }))
}

impl Bridge {
    fn serve(mut self, server: Server) {
        for request in server.incoming_requests() {
            let (status, body) = self.answer(&request);
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

    fn answer(&mut self, request: &Request) -> Reply {
        if *request.method() != Method::Get {
            return error(405, "only GET is supported");
        }
        if !self.local_host(request) {
            return error(403, "unexpected Host");
        }
        if !self.authorized(request) {
            return error(401, "missing or wrong bearer token");
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
            _ => error(404, "unknown route"),
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
            _ => match query.get("q") {
                Some(needle) => archive
                    .bridge_search(needle, chat, limit)
                    .map(|messages| serde_json::json!(messages)),
                None => return error(400, "missing q"),
            },
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
