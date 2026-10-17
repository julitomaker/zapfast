//! The analysis engine: reads each project's recent messages and asks a
//! language model what to do, whom to follow up, what is slipping, what came
//! up, which messages of its smart sources belong, and a summary.
//!
//! It is off until chosen on the Projects page, because it sends the
//! messages of the projects' sources to the chosen provider: Claude Code on
//! this computer, or the Anthropic, OpenAI or OpenRouter API with a key kept
//! in the OS keyring. Personal and ignored chats, and chats in no project,
//! are never sent. It runs on a thread of its own, every so often and on
//! demand, and only for projects with new messages.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use super::{Analysis, FeedMessage, Finding, Mode, ProjectMap};

/// How far back an analysis reads.
const WINDOW: i64 = 7 * 86_400;
/// The most messages one analysis reads, newest kept.
const MAX_MESSAGES: usize = 900;
/// The longest a message is quoted.
const MAX_CHARS: usize = 600;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    #[default]
    Off,
    ClaudeCode,
    Anthropic,
    OpenAi,
    OpenRouter,
}

impl Provider {
    pub const EVERY: [Self; 5] = [
        Self::Off,
        Self::ClaudeCode,
        Self::Anthropic,
        Self::OpenAi,
        Self::OpenRouter,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::ClaudeCode => "Claude Code",
            Self::Anthropic => "Anthropic",
            Self::OpenAi => "OpenAI",
            Self::OpenRouter => "OpenRouter",
        }
    }

    pub fn needs_key(self) -> bool {
        matches!(self, Self::Anthropic | Self::OpenAi | Self::OpenRouter)
    }

    /// The model it starts with; any other can be typed.
    pub fn default_model(self) -> &'static str {
        match self {
            Self::Anthropic => "claude-opus-5-5",
            Self::OpenRouter => "anthropic/claude-opus-5.5",
            _ => "",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EngineConfig {
    #[serde(default)]
    pub provider: Provider,
    #[serde(default)]
    pub model: String,
    /// Minutes between runs.
    #[serde(default = "default_every")]
    pub every: u32,
}

fn default_every() -> u32 {
    120
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            provider: Provider::Off,
            model: String::new(),
            every: default_every(),
        }
    }
}

impl EngineConfig {
    fn file(config: &Path) -> PathBuf {
        config.join("ai.json")
    }

    pub fn load(config: &Path) -> Self {
        std::fs::read_to_string(Self::file(config))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, config: &Path) -> std::io::Result<()> {
        std::fs::write(Self::file(config), serde_json::to_vec_pretty(self)?)
    }

    fn model(&self) -> String {
        if self.model.trim().is_empty() {
            self.provider.default_model().to_owned()
        } else {
            self.model.trim().to_owned()
        }
    }
}

// ------------------------------------------------------------------ keys

const KEY_SERVICE: &str = "com.julitomaker.whatspro.ai";

fn key_store() -> anyhow::Result<Arc<dyn keyring_core::api::CredentialStoreApi>> {
    #[cfg(target_os = "linux")]
    let store = zbus_secret_service_keyring_store::Store::new();
    #[cfg(target_os = "macos")]
    let store = apple_native_keyring_store::keychain::Store::new();
    #[cfg(windows)]
    let store = windows_native_keyring_store::Store::new();
    Ok(store.map_err(|error| anyhow::anyhow!("{error}"))?
        as Arc<dyn keyring_core::api::CredentialStoreApi>)
}

fn key_entry(provider: Provider) -> anyhow::Result<keyring_core::Entry> {
    key_store()?
        .build(KEY_SERVICE, &format!("{provider:?}"), None)
        .map_err(|error| anyhow::anyhow!("{error}"))
}

/// Keeps `key` for `provider` in the OS keyring (an empty key removes it).
pub fn set_key(provider: Provider, key: &str) -> anyhow::Result<()> {
    let entry = key_entry(provider)?;
    if key.trim().is_empty() {
        match entry.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(error) => Err(anyhow::anyhow!("{error}")),
        }
    } else {
        entry
            .set_password(key.trim())
            .map_err(|error| anyhow::anyhow!("{error}"))
    }
}

pub fn has_key(provider: Provider) -> bool {
    key(provider).is_some()
}

fn key(provider: Provider) -> Option<String> {
    key_entry(provider).ok()?.get_password().ok()
}

// ---------------------------------------------------------------- status

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub running: Option<String>,
    pub last_run: Option<i64>,
    pub last_error: Option<String>,
}

struct Engine {
    status: Status,
    run_now: bool,
    started: bool,
    last: Option<Instant>,
}

fn engine() -> &'static Mutex<Engine> {
    static ENGINE: OnceLock<Mutex<Engine>> = OnceLock::new();
    ENGINE.get_or_init(|| {
        Mutex::new(Engine {
            status: Status::default(),
            run_now: false,
            started: false,
            last: None,
        })
    })
}

pub fn status() -> Status {
    engine()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .status
        .clone()
}

/// Asks for a run as soon as the engine is free.
pub fn run_now() {
    engine().lock().unwrap_or_else(|p| p.into_inner()).run_now = true;
}

/// Starts the engine's thread once. It checks every half minute whether a
/// run is due or asked for.
pub fn start(ctx: &egui::Context, config: &Path, archive: &Path) {
    let mut state = engine().lock().unwrap_or_else(|p| p.into_inner());
    if state.started {
        return;
    }
    state.started = true;
    drop(state);
    let (ctx, config, archive) = (ctx.clone(), config.to_owned(), archive.to_owned());
    let spawned = std::thread::Builder::new()
        .name("project-analysis".into())
        .spawn(move || {
            loop {
                let settings = EngineConfig::load(&config);
                let due = {
                    let mut state = engine().lock().unwrap_or_else(|p| p.into_inner());
                    let every = Duration::from_secs(u64::from(settings.every.max(15)) * 60);
                    let due = settings.provider != Provider::Off
                        && (std::mem::take(&mut state.run_now)
                            || state.last.is_none_or(|at| at.elapsed() >= every));
                    if due {
                        state.last = Some(Instant::now());
                    }
                    due
                };
                if due {
                    run(&ctx, &config, &archive, &settings);
                }
                std::thread::sleep(Duration::from_secs(30));
            }
        });
    if let Err(error) = spawned {
        log::warn!("analysis: could not start its thread: {error}");
    }
}

fn set_status(ctx: &egui::Context, change: impl FnOnce(&mut Status)) {
    change(&mut engine().lock().unwrap_or_else(|p| p.into_inner()).status);
    ctx.request_repaint();
}

/// Analyzes every project with messages newer than its last analysis.
fn run(ctx: &egui::Context, config: &Path, archive: &Path, settings: &EngineConfig) {
    let map = ProjectMap::load(config);
    let now = jiff::Timestamp::now().as_second();
    let mut error = None;
    for project in &map.projects {
        if project.sources.is_empty() {
            continue;
        }
        let previous = super::Analysis::load(config, &project.id);
        let since = now - WINDOW;
        let messages = match super::read_messages(archive, &project.chat_ids(), since) {
            Ok(messages) => messages,
            Err(failure) => {
                error = Some(failure);
                continue;
            }
        };
        let newest = messages.last().map_or(0, |m| m.timestamp);
        if messages.is_empty() || previous.as_ref().is_some_and(|p| p.updated >= newest) {
            continue;
        }
        set_status(ctx, |status| status.running = Some(project.name.clone()));
        match analyze(config, settings, &map, project, &messages, now) {
            Ok(analysis) => {
                if let Err(failure) = analysis.save(config, &project.id) {
                    error = Some(failure.to_string());
                }
            }
            Err(failure) => {
                log::warn!("analysis: {} failed: {failure}", project.id);
                error = Some(failure.to_string());
            }
        }
    }
    set_status(ctx, |status| {
        status.running = None;
        status.last_run = Some(now);
        status.last_error = error;
    });
}

// -------------------------------------------------------------- the prompt

const SYSTEM: &str = "Sos el asistente de operaciones de una persona que lleva varios proyectos por WhatsApp. \
Leés los mensajes recientes de un proyecto (de varios chats y grupos mezclados) y armás un tablero claro y accionable, en español rioplatense, breve y concreto. \
\"Yo\" es el dueño del tablero: sus mensajes vienen marcados como \"Yo\". \
Respondé SOLO con un objeto JSON válido, sin texto antes ni después, con esta forma:\n\
{\"summaries\":{\"today\":\"\",\"yesterday\":\"\",\"week\":\"\"},\
\"todo\":[{\"text\":\"\",\"who\":\"\",\"ref\":0,\"due\":\"\"}],\
\"follow_up\":[{\"text\":\"\",\"who\":\"\",\"ref\":0}],\
\"leaks\":[{\"text\":\"\",\"who\":\"\",\"ref\":0}],\
\"needs\":[{\"text\":\"\",\"who\":\"\",\"ref\":0}],\
\"highlights\":[{\"text\":\"\",\"who\":\"\",\"ref\":0}],\
\"relevant\":[0]}\n\
Reglas:\n\
- summaries: qué pasó hoy, ayer y en la semana, 2 a 4 oraciones cada uno; vacío si no hubo nada.\n\
- todo: lo que Yo me comprometí a hacer o me pidieron y no está hecho (\"who\" = quién lo pidió).\n\
- follow_up: lo que Yo pregunté o pedí y no tuvo respuesta, o lo que otro prometió y no llegó (\"who\" = de quién se espera).\n\
- leaks: pedidos, consultas o problemas de clientes/proveedores que nadie respondió o que se están cayendo.\n\
- needs: necesidades nuevas que aparecieron (materiales, compras, decisiones, gente).\n\
- highlights: hasta 6 mensajes importantes para releer (montos, acuerdos, fechas, cambios).\n\
- ref es el número entre corchetes del mensaje que lo origina.\n\
- relevant: los números de los mensajes marcados con ~ (chats mixtos) que SÍ tratan sobre este proyecto. Los demás mensajes ~ no van.\n\
- No inventes nada que no esté en los mensajes. Si algo ya se resolvió en un mensaje posterior, no lo pongas.\n\
- Máximo 8 elementos por lista, los más importantes primero.";

/// What a message says, with the speech or picture text read out of it on
/// this computer.
fn said(message: &FeedMessage) -> String {
    match message.transcript.as_deref().map(str::trim) {
        Some(transcript) if !transcript.is_empty() => {
            format!("{} [transcripción: {transcript}]", message.text)
        }
        _ => message.text.clone(),
    }
}

fn clip(text: &str) -> String {
    let flat = text.replace('\n', " ⏎ ");
    if flat.chars().count() > MAX_CHARS {
        format!("{}…", flat.chars().take(MAX_CHARS).collect::<String>())
    } else {
        flat
    }
}

fn stamp(seconds: i64) -> String {
    jiff::Timestamp::from_second(seconds)
        .map(|t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%a %d/%m %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

/// The user prompt: the project, how its sources are taken, what was picked
/// by hand, and the numbered messages.
fn prompt(
    map: &ProjectMap,
    project: &super::Project,
    messages: &[&FeedMessage],
    now: i64,
) -> String {
    let mut text = format!(
        "Proyecto: {}\nAhora: {}\nOtros proyectos: {}\n\n",
        project.name,
        stamp(now),
        map.projects
            .iter()
            .filter(|p| p.id != project.id)
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    // What was filed by hand teaches what this project is about.
    let mut examples = Vec::new();
    let by_id: HashMap<(&str, &str), &FeedMessage> = messages
        .iter()
        .map(|m| ((m.chat.as_str(), m.id.as_str()), *m))
        .collect();
    for pick in map
        .picks
        .iter()
        .filter(|pick| pick.project == project.id)
        .rev()
        .take(40)
    {
        if let Some(message) = by_id.get(&(pick.chat.as_str(), pick.message.as_str())) {
            examples.push(format!(
                "{} \"{}\"",
                if pick.include { "SÍ va:" } else { "NO va:" },
                clip(&said(message))
            ));
        }
    }
    if !examples.is_empty() {
        text.push_str("Ejemplos elegidos a mano para este proyecto:\n");
        text.push_str(&examples.join("\n"));
        text.push_str("\n\n");
    }
    text.push_str("Mensajes (~ = chat mixto, decidir si va):\n");
    for (index, message) in messages.iter().enumerate() {
        let smart = project
            .source(&message.chat)
            .is_some_and(|source| source.mode == Mode::Smart)
            && map
                .picked(&message.chat, &message.id, &project.id)
                .is_none();
        let who = if message.from_me {
            "Yo".to_owned()
        } else {
            message
                .sender_name
                .clone()
                .unwrap_or_else(|| message.sender.split('@').next().unwrap_or("").to_owned())
        };
        text.push_str(&format!(
            "[{index}]{} {} | {} | {}: {}\n",
            if smart { "~" } else { "" },
            stamp(message.timestamp),
            message.chat_name,
            who,
            clip(&said(message))
        ));
    }
    text
}

#[derive(Default, Deserialize)]
struct Reply {
    #[serde(default)]
    summaries: HashMap<String, String>,
    #[serde(default)]
    todo: Vec<ReplyItem>,
    #[serde(default)]
    follow_up: Vec<ReplyItem>,
    #[serde(default)]
    leaks: Vec<ReplyItem>,
    #[serde(default)]
    needs: Vec<ReplyItem>,
    #[serde(default)]
    highlights: Vec<ReplyItem>,
    #[serde(default)]
    relevant: Vec<serde_json::Value>,
}

#[derive(Default, Deserialize)]
struct ReplyItem {
    #[serde(default)]
    text: String,
    #[serde(default)]
    who: Option<String>,
    #[serde(default)]
    r#ref: Option<serde_json::Value>,
    #[serde(default)]
    due: Option<String>,
}

fn number(value: &serde_json::Value) -> Option<usize> {
    value
        .as_u64()
        .map(|n| n as usize)
        .or_else(|| value.as_str()?.trim_matches(['[', ']', '~']).parse().ok())
}

/// The first JSON object in `text`, which may come wrapped in prose or a
/// code fence.
fn json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end > start).then(|| &text[start..=end])
}

fn analyze(
    config: &Path,
    settings: &EngineConfig,
    map: &ProjectMap,
    project: &super::Project,
    all: &[FeedMessage],
    now: i64,
) -> anyhow::Result<Analysis> {
    // Messages kept out by hand never reach the model.
    let messages: Vec<&FeedMessage> = all
        .iter()
        .filter(|m| map.picked(&m.chat, &m.id, &project.id) != Some(false))
        .filter(|m| {
            project
                .source(&m.chat)
                .is_some_and(|s| s.mode != Mode::Selected)
                || map.picked(&m.chat, &m.id, &project.id) == Some(true)
        })
        .collect();
    let messages = &messages[messages.len().saturating_sub(MAX_MESSAGES)..];
    let reply = complete(
        config,
        settings,
        SYSTEM,
        &prompt(map, project, messages, now),
    )?;
    let reply: Reply = serde_json::from_str(
        json_object(&reply).ok_or_else(|| anyhow::anyhow!("the model answered without JSON"))?,
    )?;
    let findings = |kind: &str, items: Vec<ReplyItem>| -> Vec<Finding> {
        items
            .into_iter()
            .filter(|item| !item.text.trim().is_empty())
            .map(|item| {
                let source = item
                    .r#ref
                    .as_ref()
                    .and_then(number)
                    .and_then(|index| messages.get(index));
                // Stable across runs: the kind and the message it came from,
                // or its words when it has none.
                let id = match source {
                    Some(m) => format!("{kind}:{}:{}", m.chat, m.id),
                    None => format!("{kind}:{}", item.text.to_lowercase()),
                };
                Finding {
                    id,
                    text: item.text.trim().to_owned(),
                    who: item.who.filter(|w| !w.trim().is_empty()),
                    chat: source.map(|m| m.chat.clone()),
                    message: source.map(|m| m.id.clone()),
                    since: source.map(|m| m.timestamp),
                    due: item.due.filter(|d| !d.trim().is_empty()),
                }
            })
            .collect()
    };
    let mut relevant: HashMap<String, Vec<String>> = HashMap::new();
    for index in reply.relevant.iter().filter_map(number) {
        if let Some(message) = messages.get(index) {
            relevant
                .entry(message.chat.clone())
                .or_default()
                .push(message.id.clone());
        }
    }
    let read = project
        .sources
        .iter()
        .filter(|source| source.mode == Mode::Smart)
        .map(|source| (source.chat.clone(), now))
        .collect();
    let _ = config;
    Ok(Analysis {
        updated: all.last().map_or(now, |m| m.timestamp),
        summaries: reply
            .summaries
            .into_iter()
            .filter(|(_, text)| !text.trim().is_empty())
            .collect(),
        todo: findings("todo", reply.todo),
        follow_up: findings("follow", reply.follow_up),
        leaks: findings("leak", reply.leaks),
        needs: findings("need", reply.needs),
        highlights: findings("highlight", reply.highlights),
        relevant,
        read,
    })
}

// -------------------------------------------------------------- providers

fn http() -> anyhow::Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()?)
}

fn complete(
    _config: &Path,
    settings: &EngineConfig,
    system: &str,
    user: &str,
) -> anyhow::Result<String> {
    let model = settings.model();
    match settings.provider {
        Provider::Off => anyhow::bail!("the analysis is off"),
        Provider::ClaudeCode => claude_code(system, user),
        Provider::Anthropic => {
            let key = key(Provider::Anthropic)
                .ok_or_else(|| anyhow::anyhow!("Add your Anthropic API key"))?;
            let body = serde_json::json!({
                "model": model,
                "max_tokens": 16000,
                "system": system,
                "messages": [{"role": "user", "content": user}],
            });
            let response = http()?
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .body(serde_json::to_vec(&body)?)
                .send()?
                .bytes()?;
            let response: serde_json::Value = serde_json::from_slice(&response)?;
            if let Some(error) = response.get("error") {
                anyhow::bail!(
                    "Anthropic: {}",
                    error["message"].as_str().unwrap_or("error")
                );
            }
            if response["stop_reason"] == "refusal" {
                anyhow::bail!("Anthropic declined this request");
            }
            Ok(response["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|block| block["type"] == "text")
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join(""))
        }
        Provider::OpenAi | Provider::OpenRouter => {
            let provider = settings.provider;
            let key = key(provider)
                .ok_or_else(|| anyhow::anyhow!("Add your {} API key", provider.label()))?;
            if model.is_empty() {
                anyhow::bail!("Choose a {} model", provider.label());
            }
            let url = if provider == Provider::OpenAi {
                "https://api.openai.com/v1/chat/completions"
            } else {
                "https://openrouter.ai/api/v1/chat/completions"
            };
            let body = serde_json::json!({
                "model": model,
                "messages": [
                    {"role": "system", "content": system},
                    {"role": "user", "content": user},
                ],
                "response_format": {"type": "json_object"},
            });
            let response = http()?
                .post(url)
                .bearer_auth(key)
                .header("X-Title", "Whats Pro")
                .header("content-type", "application/json")
                .body(serde_json::to_vec(&body)?)
                .send()?
                .bytes()?;
            let response: serde_json::Value = serde_json::from_slice(&response)?;
            if let Some(error) = response.get("error") {
                anyhow::bail!(
                    "{}: {}",
                    provider.label(),
                    error["message"].as_str().unwrap_or("error")
                );
            }
            Ok(response["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or_default()
                .to_owned())
        }
    }
}

/// Where Claude Code is installed, if it is.
pub fn claude_code_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join("claude"))
                .collect()
        })
        .unwrap_or_default();
    candidates.extend(["/opt/homebrew/bin/claude", "/usr/local/bin/claude"].map(PathBuf::from));
    if let Some(home) = home {
        candidates.push(home.join(".claude/local/claude"));
        candidates.push(home.join(".local/bin/claude"));
        candidates.push(home.join(".npm-global/bin/claude"));
    }
    candidates.into_iter().find(|path| path.is_file())
}

fn claude_code(system: &str, user: &str) -> anyhow::Result<String> {
    use std::io::Write;
    let binary =
        claude_code_path().ok_or_else(|| anyhow::anyhow!("Claude Code is not installed"))?;
    let mut child = std::process::Command::new(binary)
        .args([
            "-p",
            "--output-format",
            "json",
            "--append-system-prompt",
            system,
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("Claude Code did not take the prompt"))?
        .write_all(user.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        anyhow::bail!("Claude Code stopped with {}", output.status);
    }
    let reply: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    Ok(reply["result"].as_str().unwrap_or_default().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(chat: &str, id: &str, timestamp: i64, text: &str) -> FeedMessage {
        FeedMessage {
            chat: chat.into(),
            chat_name: "Obra".into(),
            id: id.into(),
            sender: "1@s.whatsapp.net".into(),
            sender_name: Some("Ana".into()),
            from_me: false,
            timestamp,
            kind: "text".into(),
            text: text.into(),
            reply_to: None,
            edited: false,
            transcript: None,
        }
    }

    #[test]
    fn a_transcript_reaches_the_prompt() {
        let mut voice = message("c@g.us", "v", 10, "Voice message");
        voice.transcript = Some("mañana llega el cemento".into());
        assert_eq!(
            said(&voice),
            "Voice message [transcripción: mañana llega el cemento]"
        );
        assert_eq!(said(&message("c@g.us", "t", 10, "hola")), "hola");
    }

    #[test]
    fn the_prompt_marks_mixed_chats_and_carries_hand_picks() {
        let mut map = ProjectMap::default();
        let id = map.add("JAS");
        map.add_source(&id, "g@g.us", Mode::All);
        map.add_source(&id, "p@s.whatsapp.net", Mode::Smart);
        map.pick("p@s.whatsapp.net", "b", &id, true);
        let a = message("g@g.us", "a", 100, "presupuesto");
        let b = message("p@s.whatsapp.net", "b", 200, "las prendas");
        let c = message("p@s.whatsapp.net", "c", 300, "el asado");
        let project = map.project(&id).unwrap().clone();
        let text = prompt(&map, &project, &[&a, &b, &c], 400);
        assert!(text.contains("[0] "));
        assert!(text.contains("[2]~ "), "{text}");
        assert!(
            !text.contains("[1]~"),
            "a picked message is not asked about"
        );
        assert!(text.contains("SÍ va: \"las prendas\""));
    }

    #[test]
    fn replies_map_back_to_their_messages() {
        assert_eq!(json_object("```json\n{\"a\":1}\n```"), Some("{\"a\":1}"));
        assert_eq!(number(&serde_json::json!(3)), Some(3));
        assert_eq!(number(&serde_json::json!("[4]")), Some(4));
        assert_eq!(Provider::Anthropic.default_model(), "claude-opus-5-5");
    }
}
