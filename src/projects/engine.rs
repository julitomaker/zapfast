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
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use super::{Analysis, FeedMessage, Finding, Mode, ProjectMap};

/// How far back an analysis reads.
const WINDOW: i64 = 7 * 86_400;
/// How far back recorded calls of the project are read into the prompt.
const CALL_WINDOW: i64 = 14 * 86_400;
/// The most messages one analysis reads, newest kept.
const MAX_MESSAGES: usize = 900;
/// The longest a message is quoted.
const MAX_CHARS: usize = 600;
/// The most relevant message ids kept per smart source across runs.
const MAX_RELEVANT: usize = 1000;
/// The largest prompt sent to the model (in bytes), oldest items dropped first.
const PROMPT_BUDGET: usize = 64_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    #[default]
    Off,
    /// Claude Code, signed in with a Claude subscription.
    ClaudeCode,
    /// Codex CLI, signed in with a ChatGPT subscription.
    CodexCli,
    /// Gemini CLI, signed in with a Google account.
    GeminiCli,
    Anthropic,
    OpenAi,
    OpenRouter,
    /// Google's Gemini API, with a key from Google AI Studio.
    GeminiApi,
}

/// A model worth suggesting, with why.
pub struct Suggestion {
    /// Empty for the tool's own default.
    pub id: &'static str,
    pub note: &'static str,
}

const fn suggest(id: &'static str, note: &'static str) -> Suggestion {
    Suggestion { id, note }
}

impl Provider {
    pub const EVERY: [Self; 8] = [
        Self::Off,
        Self::ClaudeCode,
        Self::CodexCli,
        Self::GeminiCli,
        Self::Anthropic,
        Self::OpenAi,
        Self::OpenRouter,
        Self::GeminiApi,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::ClaudeCode => "Claude",
            Self::CodexCli => "ChatGPT · Codex",
            Self::GeminiCli => "Gemini",
            Self::Anthropic => "Anthropic API",
            Self::OpenAi => "OpenAI API",
            Self::OpenRouter => "OpenRouter",
            Self::GeminiApi => "Gemini API",
        }
    }

    pub fn needs_key(self) -> bool {
        matches!(
            self,
            Self::Anthropic | Self::OpenAi | Self::OpenRouter | Self::GeminiApi
        )
    }

    /// The command-line tool it runs, for the providers that use a
    /// subscription instead of a key.
    pub fn binary(self) -> Option<&'static str> {
        match self {
            Self::ClaudeCode => Some("claude"),
            Self::CodexCli => Some("codex"),
            Self::GeminiCli => Some("gemini"),
            _ => None,
        }
    }

    /// The command-line tool a subscription works through.
    pub fn tool(self) -> Option<crate::ai_tools::Tool> {
        match self {
            Self::ClaudeCode => Some(crate::ai_tools::Tool::Claude),
            Self::CodexCli => Some(crate::ai_tools::Tool::Codex),
            Self::GeminiCli => Some(crate::ai_tools::Tool::Gemini),
            _ => None,
        }
    }

    /// The models to suggest, the recommended one first.
    pub fn suggestions(self) -> &'static [Suggestion] {
        const CLAUDE: &[Suggestion] = &[
            suggest("opus", "Claude Opus: the most capable"),
            suggest("sonnet", "Claude Sonnet: faster, uses less of your plan"),
            suggest("haiku", "Claude Haiku: the lightest"),
        ];
        const CODEX: &[Suggestion] = &[suggest("", "Codex's own default model")];
        const GEMINI: &[Suggestion] = &[
            suggest("", "Gemini CLI's own default model"),
            suggest("gemini-2.5-pro", "Gemini Pro: the most capable"),
            suggest("gemini-2.5-flash", "Gemini Flash: faster, lighter"),
        ];
        const ANTHROPIC: &[Suggestion] = &[
            suggest("claude-opus-5-5", "Claude Opus 5.5: the most capable"),
            suggest("claude-sonnet-5-5", "Claude Sonnet 5.5: faster and cheaper"),
            suggest("claude-haiku-5-5", "Claude Haiku 5.5: the cheapest"),
        ];
        const OPENAI: &[Suggestion] = &[suggest("gpt-5", "GPT-5")];
        const GEMINI_API: &[Suggestion] = &[
            suggest("gemini-2.5-pro", "Gemini Pro: the most capable"),
            suggest("gemini-2.5-flash", "Gemini Flash: faster, lighter"),
        ];
        const OPENROUTER: &[Suggestion] = &[
            suggest(
                "anthropic/claude-opus-5.5",
                "Claude Opus 5.5 through OpenRouter",
            ),
            suggest(
                "anthropic/claude-sonnet-5.5",
                "Claude Sonnet 5.5 through OpenRouter",
            ),
        ];
        match self {
            Self::Off => &[],
            Self::ClaudeCode => CLAUDE,
            Self::CodexCli => CODEX,
            Self::GeminiCli => GEMINI,
            Self::Anthropic => ANTHROPIC,
            Self::OpenAi => OPENAI,
            Self::OpenRouter => OPENROUTER,
            Self::GeminiApi => GEMINI_API,
        }
    }

    /// The model it starts with.
    pub fn default_model(self) -> &'static str {
        self.suggestions().first().map_or("", |s| s.id)
    }

    /// Whether the provider can list every model it offers.
    pub fn lists_models(self) -> bool {
        self.needs_key()
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

// Keys live in `crate::keys`, shared with media, under the provider's name.

/// Keeps `key` for `provider` in the OS keyring (an empty key removes it).
pub fn set_key(provider: Provider, key: &str) -> anyhow::Result<()> {
    crate::keys::set(&key_account(provider), key)
}

/// The keyring account of `provider`'s key: the name media uses too.
fn key_account(provider: Provider) -> String {
    match provider {
        Provider::GeminiApi => "Gemini".to_owned(),
        other => format!("{other:?}"),
    }
}

pub fn has_key(provider: Provider) -> bool {
    key(provider).is_some()
}

fn key(provider: Provider) -> Option<String> {
    crate::keys::get(&key_account(provider))
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
    /// Projects to read again even without new messages: their context,
    /// sources or notes changed.
    force: std::collections::HashSet<String>,
    started: bool,
    last: Option<Instant>,
}

fn engine() -> &'static Mutex<Engine> {
    static ENGINE: OnceLock<Mutex<Engine>> = OnceLock::new();
    ENGINE.get_or_init(|| {
        Mutex::new(Engine {
            status: Status::default(),
            run_now: false,
            force: Default::default(),
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

/// Asks for `project` to be read again as soon as the engine is free, even
/// with no new messages: its context, sources or notes changed.
pub fn refresh(project: &str) {
    let mut state = engine().lock().unwrap_or_else(|p| p.into_inner());
    state.force.insert(project.to_owned());
    state.run_now = true;
}

/// Whether `project` waits to be read again.
pub fn pending(project: &str) -> bool {
    engine()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .force
        .contains(project)
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

/// The app's state folder from an account's `archive.db` path.
fn state_dir(archive: &Path) -> &Path {
    let dir = archive.parent().unwrap_or(archive);
    match dir.parent() {
        Some(accounts) if accounts.file_name().and_then(|n| n.to_str()) == Some("accounts") => {
            accounts.parent().unwrap_or(dir)
        }
        _ => dir,
    }
}

/// Analyzes every project with messages newer than its last analysis.
fn run(ctx: &egui::Context, config: &Path, archive: &Path, settings: &EngineConfig) {
    let map = ProjectMap::load(config);
    let forced = engine()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .force
        .clone();
    let now = jiff::Timestamp::now().as_second();
    let state = state_dir(archive);
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
        let force = forced.contains(&project.id);
        if !force && (messages.is_empty() || previous.as_ref().is_some_and(|p| p.updated >= newest))
        {
            continue;
        }
        set_status(ctx, |status| status.running = Some(project.name.clone()));
        let result = analyze(
            (config, state),
            settings,
            &map,
            project,
            &messages,
            previous.as_ref(),
            now,
        );
        engine()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .force
            .remove(&project.id);
        match result {
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
Leés todo el cuadro de un proyecto junto (chats y grupos, notas de bitácora, llamadas grabadas, leads del proyecto y el contexto secundario del CRM de la empresa) y armás un tablero claro y accionable, en español rioplatense, breve y concreto. \
\"Yo\" es el dueño del tablero: sus mensajes vienen marcados como \"Yo\". \
Respondé SOLO con un objeto JSON válido, sin texto antes ni después, con esta forma:\n\
{\"summaries\":{\"today\":\"\",\"yesterday\":\"\",\"week\":\"\"},\
\"todo\":[{\"text\":\"\",\"who\":\"\",\"ref\":0,\"due\":\"\"}],\
\"follow_up\":[{\"text\":\"\",\"who\":\"\",\"ref\":0}],\
\"leaks\":[{\"text\":\"\",\"who\":\"\",\"ref\":0}],\
\"needs\":[{\"text\":\"\",\"who\":\"\",\"ref\":0}],\
\"highlights\":[{\"text\":\"\",\"who\":\"\",\"ref\":0}],\
\"leads\":[{\"who\":\"\",\"ref\":0,\"why\":\"\",\"stage\":\"\",\"paid\":false,\"amount\":\"\"}],\
\"relevant\":[0]}\n\
Reglas:\n\
- summaries: qué pasó hoy, ayer y en la semana, 2 a 4 oraciones cada uno; vacío si no hubo nada.\n\
- todo: lo que Yo me comprometí a hacer o me pidieron y no está hecho (\"who\" = quién lo pidió).\n\
- follow_up: lo que Yo pregunté o pedí y no tuvo respuesta, o lo que otro prometió y no llegó (\"who\" = de quién se espera).\n\
- leaks: pedidos, consultas o problemas de clientes/proveedores que nadie respondió o que se están cayendo.\n\
- needs: necesidades nuevas que aparecieron (materiales, compras, decisiones, gente).\n\
- highlights: hasta 6 mensajes importantes para releer (montos, acuerdos, fechas, cambios).\n\
- leads: personas que quieren comprar o contratar: pidieron precio, catálogo, stock, envío o una cotización, o ya compraron. \
\"who\" = su nombre; \"ref\" = un mensaje escrito por esa persona (nunca uno de Yo); \"why\" = qué quiere, en pocas palabras; \
\"stage\" = \"contacted\" si consultó, \"quoted\" si se le pasó precio o cotización, \"customer\" si confirmó, pagó o recibió; \
\"paid\" = true solo si dice que pagó o mandó el comprobante; \"amount\" = el monto si se habló de uno. \
Solo clientes o posibles clientes: no proveedores, ni compañeros, ni integrantes del proyecto. Máximo 8.\n\
- ref es el número entre corchetes del mensaje que lo origina.\n\
- relevant: los números de los mensajes marcados con ~ (chats mixtos) que SÍ tratan sobre este proyecto. Los demás mensajes ~ no van.\n\
- No inventes nada que no esté en las fuentes del proyecto. Si algo ya se resolvió en un mensaje o nota posterior, no lo pongas.\n\
- Máximo 8 elementos por lista, los más importantes primero.\n\
- Usá el contexto y los integrantes para entender quién es quién, qué rol tiene cada uno y cómo se organiza el proyecto.\n\
- Las notas del dueño son lo más confiable sobre lo que ya pasó: si una nota dice que algo se resolvió o se hizo, sacalo de todas las listas; si trae un compromiso, una tarea, una llamada o una necesidad nueva, sumalo a la lista que corresponda (con ref null).\n\
- Cruzá todas las fuentes entre sí:\n\
  * Compromiso o próximo paso en una llamada sin seguimiento ni confirmación en el chat → sumalo en follow_up o leaks (con ref null o el mensaje relacionado).\n\
  * Lead del proyecto sin próxima acción definida (o con próxima acción vencida sin avance en el chat) → sumalo en follow_up.\n\
  * Algo prometido o acordado en el chat (precio, pago, pedido, entrega o reclamo) que no está reflejado en el CRM o en el lead → señalalo como nota en todo o needs.\n\
  * El bloque de ElevaLeads (\"del CRM de la empresa\") es solo contexto secundario de referencia: nunca uses sus ítems por sí solos como hallazgos primarios si no se cruzan con los chats, llamadas, notas o leads del proyecto.\n\
- No vuelvas a poner lo que el dueño marcó como hecho o descartado, salvo que un mensaje posterior lo reabra.\n\
- Evitá recomendaciones parecidas a las que el dueño marcó como malas; las marcadas como buenas muestran qué le sirve.";

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

/// Everything outside the message feed that gives the prompt one picture of
/// the project: notes, verdicts, recorded calls, leads and the brand's CRM.
#[derive(Default)]
struct PromptContext<'a> {
    journal: Option<&'a super::Journal>,
    ticks: Option<&'a super::Ticks>,
    calls: &'a [crate::call_library::Call],
    leads: &'a [crate::leads::Lead],
    crm: Option<(&'a crate::leads::Brand, &'a crate::eleva::Inbox)>,
}

/// Formats one recorded call of the project on a single compact line.
fn format_call_line(call: &crate::call_library::Call) -> String {
    let mut line = format!("- {} · {}", stamp(call.when()), clip(call.title.trim()));
    if let Some(analysis) = &call.analysis {
        if !analysis.summary.trim().is_empty() {
            line.push_str(&format!(": {}", clip(analysis.summary.trim())));
        }
        let commitments: Vec<String> = analysis
            .commitments
            .iter()
            .filter(|c| !c.what.trim().is_empty())
            .map(|c| {
                let who = c.who.trim();
                let mut item = if who.is_empty() {
                    clip(c.what.trim())
                } else {
                    format!("{who}: {}", clip(c.what.trim()))
                };
                if let Some(due) = c.due.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
                    item.push_str(&format!(" (para {due})"));
                }
                item
            })
            .collect();
        if !commitments.is_empty() {
            line.push_str(&format!(" | Compromisos: {}", commitments.join("; ")));
        }
        let steps: Vec<String> = analysis
            .next_steps
            .iter()
            .chain(analysis.tasks.iter())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(clip)
            .collect();
        if !steps.is_empty() {
            line.push_str(&format!(" | Próximos pasos: {}", steps.join("; ")));
        }
    } else if let Some(transcript) = call.transcript()
        && !transcript.text.trim().is_empty()
    {
        line.push_str(&format!(": {}", clip(transcript.text.trim())));
    }
    line
}

/// Formats one lead's current state on a single compact line.
fn format_lead_line(lead: &crate::leads::Lead) -> String {
    let who = match lead
        .phone
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        Some(phone) => format!("{} ({phone})", lead.name.trim()),
        None => lead.name.trim().to_owned(),
    };
    let stage = match lead.stage {
        crate::leads::Stage::New => "nuevo",
        crate::leads::Stage::Contacted => "contactado",
        crate::leads::Stage::Quoted => "cotizado",
        crate::leads::Stage::Customer => "cliente",
        crate::leads::Stage::Lost => "perdido",
    };
    let amount = lead
        .value
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| format!(" · monto: {v}"))
        .unwrap_or_default();
    let next = match &lead.next {
        Some(next) if !next.text.trim().is_empty() => {
            match next.due.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
                Some(due) => format!(
                    " · próxima acción: {} (vence {due})",
                    clip(next.text.trim())
                ),
                None => format!(" · próxima acción: {}", clip(next.text.trim())),
            }
        }
        _ if lead.stage.open() => " · sin próxima acción".to_owned(),
        _ => String::new(),
    };
    let mut entries: Vec<String> = lead
        .entries
        .iter()
        .rev()
        .filter(|e| e.kind != crate::leads::Kind::Stage && !e.text.trim().is_empty())
        .take(3)
        .map(|e| format!("[{}] {}", stamp(e.at), clip(e.text.trim())))
        .collect();
    entries.reverse();
    let history = if entries.is_empty() {
        String::new()
    } else {
        format!(" · últimas entradas: {}", entries.join("; "))
    };
    format!("- {who} · etapa: {stage}{amount}{next}{history}")
}

/// Formats the brand's cached ElevaLeads inbox as secondary context only.
fn format_crm_block(brand: &crate::leads::Brand, inbox: &crate::eleva::Inbox) -> String {
    if inbox.read_at == 0
        && inbox.total == 0
        && inbox.waiting.is_empty()
        && inbox.alerts.is_empty()
        && inbox.agenda.is_empty()
    {
        return String::new();
    }
    let waiting_count = inbox.total.max(inbox.waiting.len());
    let mut block = format!(
        "Contexto secundario del CRM de la empresa (ElevaLeads · {} — del CRM de la empresa, solo contexto secundario, nunca usar como hallazgos primarios):\n\
         Totales en CRM: {} esperando respuesta, {} alertas de atención, {} en agenda.\n",
        brand.name,
        waiting_count,
        inbox.alerts.len(),
        inbox.agenda.len(),
    );
    for item in inbox.waiting.iter().take(6) {
        let detail = item
            .detail
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(|d| format!(": {}", clip(d)))
            .unwrap_or_default();
        block.push_str(&format!(
            "- Esperando en CRM: {} ({}, {}){detail}\n",
            item.who, item.reason, item.ago
        ));
    }
    for alert in inbox.alerts.iter().take(6) {
        let who = alert.who.as_deref().unwrap_or("sin nombre");
        let when = alert
            .when
            .as_deref()
            .map(str::trim)
            .filter(|w| !w.is_empty())
            .map(|w| format!(" ({w})"))
            .unwrap_or_default();
        let excerpt = alert
            .excerpt
            .as_deref()
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .map(|e| format!(": {}", clip(e)))
            .unwrap_or_default();
        block.push_str(&format!(
            "- Alerta en CRM: {} · {who}{when}{excerpt}\n",
            alert.title
        ));
    }
    for item in inbox.agenda.iter().take(4) {
        let who = item.who.as_deref().unwrap_or("sin nombre");
        let when = item
            .when
            .as_deref()
            .map(str::trim)
            .filter(|w| !w.is_empty())
            .map(|w| format!(" ({w})"))
            .unwrap_or_default();
        block.push_str(&format!("- Agenda en CRM: {} · {who}{when}\n", item.title));
    }
    block.push('\n');
    block
}

/// The user prompt: the project, its notes, recent recorded calls, leads,
/// secondary CRM context, hand-picked examples, and the numbered messages,
/// kept within `PROMPT_BUDGET` by dropping the oldest items first.
fn prompt(
    map: &ProjectMap,
    project: &super::Project,
    messages: &[&FeedMessage],
    all: &[FeedMessage],
    ctx: PromptContext<'_>,
    now: i64,
) -> String {
    let mut header = format!(
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
    if !project.context.trim().is_empty() {
        header.push_str(&format!(
            "Contexto del proyecto (escrito por el dueño):\n{}\n\n",
            project.context.trim()
        ));
    }
    if !project.members.is_empty() {
        header.push_str("Integrantes:\n");
        for member in &project.members {
            let phone = member.id.split('@').next().unwrap_or("");
            let mut line = format!("- {}", member.name);
            if !member.role.trim().is_empty() {
                line.push_str(&format!(": {}", member.role.trim()));
            }
            line.push_str(&format!(" ({phone})"));
            if !member.about.trim().is_empty() {
                line.push_str(&format!(". {}", clip(member.about.trim())));
            }
            header.push_str(&line);
            header.push('\n');
        }
        header.push('\n');
    }

    let note_lines: Vec<String> = ctx
        .journal
        .map(|journal| {
            let recent: Vec<&super::Note> = journal
                .notes
                .iter()
                .filter(|note| now - note.at < 30 * 86_400)
                .collect();
            recent
                .into_iter()
                .rev()
                .take(40)
                .rev()
                .map(|note| {
                    let kind = match note.kind {
                        super::NoteKind::Log => "pasó",
                        super::NoteKind::Memo => "apunte",
                    };
                    format!("- {} ({kind}): {}", stamp(note.at), clip(&note.text))
                })
                .collect()
        })
        .unwrap_or_default();

    let mut project_calls: Vec<&crate::call_library::Call> = ctx
        .calls
        .iter()
        .filter(|call| !call.private && now - call.when() < CALL_WINDOW)
        .filter(|call| {
            call.project.as_deref() == Some(project.id.as_str())
                || call
                    .chat
                    .as_deref()
                    .is_some_and(|chat| project.source(chat).is_some())
                || call.lead.as_deref().is_some_and(|lead_id| {
                    ctx.leads
                        .iter()
                        .any(|l| l.project == project.id && l.id == lead_id)
                })
        })
        .collect();
    project_calls.sort_by_key(|call| call.when());
    let call_lines: Vec<String> = project_calls
        .into_iter()
        .rev()
        .take(20)
        .rev()
        .map(format_call_line)
        .collect();

    let mut project_leads: Vec<&crate::leads::Lead> = ctx
        .leads
        .iter()
        .filter(|lead| lead.project == project.id)
        .collect();
    project_leads.sort_by_key(|lead| lead.updated);
    let lead_lines: Vec<String> = project_leads
        .into_iter()
        .rev()
        .take(40)
        .rev()
        .map(format_lead_line)
        .collect();

    let crm_block = ctx
        .crm
        .map(|(brand, inbox)| format_crm_block(brand, inbox))
        .unwrap_or_default();

    let mut verdicts_and_examples = String::new();
    if let Some(ticks) = ctx.ticks {
        let verdicts = |verdicts: &[super::Verdict]| -> Vec<String> {
            ticks
                .feedback
                .iter()
                .rev()
                .filter(|f| verdicts.contains(&f.verdict))
                .take(40)
                .map(|f| match &f.note {
                    Some(note) => {
                        format!("- {} (cómo se resolvió: {})", clip(&f.text), clip(note))
                    }
                    None => format!("- {}", clip(&f.text)),
                })
                .collect()
        };
        for (title, kinds) in [
            (
                "Ya hecho o descartado por el dueño (no repetir):",
                &[super::Verdict::Done, super::Verdict::Dismissed][..],
            ),
            (
                "Recomendaciones que el dueño marcó como malas:",
                &[super::Verdict::Bad][..],
            ),
            (
                "Recomendaciones que el dueño marcó como buenas:",
                &[super::Verdict::Good][..],
            ),
        ] {
            let lines = verdicts(kinds);
            if !lines.is_empty() {
                verdicts_and_examples.push_str(title);
                verdicts_and_examples.push('\n');
                verdicts_and_examples.push_str(&lines.join("\n"));
                verdicts_and_examples.push_str("\n\n");
            }
        }
    }
    // What was filed by hand teaches what this project is about, including
    // messages marked "not this project" that are left out of `messages`.
    let mut examples = Vec::new();
    let by_id: HashMap<(&str, &str), &FeedMessage> = all
        .iter()
        .chain(messages.iter().copied())
        .map(|m| ((m.chat.as_str(), m.id.as_str()), m))
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
        verdicts_and_examples.push_str("Ejemplos elegidos a mano para este proyecto:\n");
        verdicts_and_examples.push_str(&examples.join("\n"));
        verdicts_and_examples.push_str("\n\n");
    }

    let message_lines: Vec<String> = messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            let smart = project
                .source(&message.chat)
                .is_some_and(|source| source.mode == Mode::Smart)
                && map
                    .picked(&message.chat, &message.id, &project.id)
                    .is_none();
            let phone = message.sender.split('@').next().unwrap_or("");
            let member = project.members.iter().find(|m| {
                m.id == message.sender
                    || (!phone.is_empty() && m.id.split('@').next() == Some(phone))
            });
            let who = if message.from_me {
                "Yo".to_owned()
            } else if let Some(member) = member {
                member.name.clone()
            } else {
                message
                    .sender_name
                    .clone()
                    .unwrap_or_else(|| phone.to_owned())
            };
            format!(
                "[{index}]{} {} | {} | {}: {}",
                if smart { "~" } else { "" },
                stamp(message.timestamp),
                message.chat_name,
                who,
                clip(&said(message))
            )
        })
        .collect();

    let render =
        |note_from: usize, call_from: usize, lead_from: usize, msg_from: usize| -> String {
            let mut text = header.clone();
            if note_from < note_lines.len() {
                text.push_str("Notas del dueño (bitácora y apuntes, la más nueva al final):\n");
                for line in &note_lines[note_from..] {
                    text.push_str(line);
                    text.push('\n');
                }
                text.push('\n');
            }
            if call_from < call_lines.len() {
                text.push_str("Llamadas del proyecto (últimos 14 días, la más nueva al final):\n");
                for line in &call_lines[call_from..] {
                    text.push_str(line);
                    text.push('\n');
                }
                text.push('\n');
            }
            if lead_from < lead_lines.len() {
                text.push_str("Leads del proyecto (estado actual):\n");
                for line in &lead_lines[lead_from..] {
                    text.push_str(line);
                    text.push('\n');
                }
                text.push('\n');
            }
            text.push_str(&crm_block);
            text.push_str(&verdicts_and_examples);
            text.push_str("Mensajes (~ = chat mixto, decidir si va):\n");
            for line in &message_lines[msg_from..] {
                text.push_str(line);
                text.push('\n');
            }
            text
        };

    let full = render(0, 0, 0, 0);
    if full.len() <= PROMPT_BUDGET {
        return full;
    }
    let mut over = full.len() - PROMPT_BUDGET;
    let (mut note_from, mut call_from, mut lead_from, mut msg_from) = (0, 0, 0, 0);
    while over > 0 && msg_from + 1 < message_lines.len() {
        over = over.saturating_sub(message_lines[msg_from].len() + 1);
        msg_from += 1;
    }
    while over > 0 && note_from + 1 < note_lines.len() {
        over = over.saturating_sub(note_lines[note_from].len() + 1);
        note_from += 1;
    }
    while over > 0 && call_from + 1 < call_lines.len() {
        over = over.saturating_sub(call_lines[call_from].len() + 1);
        call_from += 1;
    }
    while over > 0 && lead_from + 1 < lead_lines.len() {
        over = over.saturating_sub(lead_lines[lead_from].len() + 1);
        lead_from += 1;
    }
    render(note_from, call_from, lead_from, msg_from)
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
    leads: Vec<ReplyLead>,
    #[serde(default)]
    relevant: Vec<serde_json::Value>,
}

#[derive(Default, Deserialize)]
struct ReplyLead {
    #[serde(default)]
    who: String,
    #[serde(default)]
    r#ref: Option<serde_json::Value>,
    #[serde(default)]
    why: String,
    #[serde(default)]
    stage: String,
    #[serde(default)]
    paid: Option<bool>,
    #[serde(default)]
    amount: Option<serde_json::Value>,
}

/// The people the reply reads as buyers, each tied to their own chat
/// through a message they wrote; one per person, the first kept.
fn lead_hints(items: Vec<ReplyLead>, messages: &[&FeedMessage]) -> Vec<super::LeadHint> {
    let mut seen = std::collections::HashSet::new();
    items
        .into_iter()
        .filter_map(|item| {
            let message = item
                .r#ref
                .as_ref()
                .and_then(number)
                .and_then(|i| messages.get(i))?;
            if message.from_me {
                return None;
            }
            // In a chat with one person it is theirs; in a group, the sender's.
            let person = if message.chat.ends_with("@s.whatsapp.net") {
                message.chat.clone()
            } else {
                message.sender.clone()
            };
            if !person.ends_with("@s.whatsapp.net") || !seen.insert(person.clone()) {
                return None;
            }
            let name = Some(item.who.trim())
                .filter(|w| !w.is_empty())
                .map(str::to_owned)
                .or_else(|| message.sender_name.clone())
                .unwrap_or_else(|| person.split('@').next().unwrap_or_default().to_owned());
            let stage = match item.stage.trim() {
                stage @ ("contacted" | "quoted" | "customer") => stage.to_owned(),
                _ => "contacted".to_owned(),
            };
            let amount = item
                .amount
                .and_then(|a| match a {
                    serde_json::Value::String(s) => Some(s),
                    serde_json::Value::Number(n) => Some(n.to_string()),
                    _ => None,
                })
                .map(|a| a.trim().to_owned())
                .filter(|a| !a.is_empty() && a != "0");
            Some(super::LeadHint {
                id: format!("lead:{person}"),
                name,
                person,
                why: item.why.trim().to_owned(),
                stage,
                paid: item.paid.unwrap_or(false),
                amount,
                chat: Some(message.chat.clone()),
                message: Some(message.id.clone()),
                since: Some(message.timestamp),
            })
        })
        .take(8)
        .collect()
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

/// Messages of `all` that enter `project`'s numbered feed: messages marked
/// "not this project" stay out, and a `Selected` source brings only what was
/// picked by hand.
fn feed_messages<'a>(
    map: &ProjectMap,
    project: &super::Project,
    all: &'a [FeedMessage],
) -> Vec<&'a FeedMessage> {
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
    let start = messages.len().saturating_sub(MAX_MESSAGES);
    messages[start..].to_vec()
}

/// Merges the messages the model just judged to belong with those kept from
/// earlier runs, dropping any later marked "not this project" or from sources
/// no longer in smart mode, and capping each chat's list at `cap` (newest kept).
fn merge_relevant(
    map: &ProjectMap,
    project: &super::Project,
    previous: Option<&Analysis>,
    messages: &[&FeedMessage],
    picked_by_model: &[serde_json::Value],
    cap: usize,
) -> HashMap<String, Vec<String>> {
    let mut relevant: HashMap<String, Vec<String>> = HashMap::new();
    if let Some(previous) = previous {
        for source in project.sources.iter().filter(|s| s.mode == Mode::Smart) {
            if let Some(old) = previous.relevant.get(&source.chat) {
                let kept: Vec<String> = old
                    .iter()
                    .filter(|id| map.picked(&source.chat, id, &project.id) != Some(false))
                    .cloned()
                    .collect();
                if !kept.is_empty() {
                    relevant.insert(source.chat.clone(), kept);
                }
            }
        }
    }
    for index in picked_by_model.iter().filter_map(number) {
        if let Some(message) = messages.get(index)
            && project
                .source(&message.chat)
                .is_some_and(|s| s.mode == Mode::Smart)
            && map.picked(&message.chat, &message.id, &project.id) != Some(false)
        {
            let ids = relevant.entry(message.chat.clone()).or_default();
            ids.retain(|id| id != &message.id);
            ids.push(message.id.clone());
        }
    }
    for ids in relevant.values_mut() {
        if ids.len() > cap {
            ids.drain(..ids.len() - cap);
        }
    }
    relevant.retain(|_, ids| !ids.is_empty());
    relevant
}

fn analyze(
    (config, state): (&Path, &Path),
    settings: &EngineConfig,
    map: &ProjectMap,
    project: &super::Project,
    all: &[FeedMessage],
    previous: Option<&Analysis>,
    now: i64,
) -> anyhow::Result<Analysis> {
    let messages = feed_messages(map, project, all);
    let messages = messages.as_slice();
    let journal = super::Journal::load(config, &project.id);
    let ticks = super::Ticks::load(config, &project.id);
    let library = crate::call_library::Library::load(state);
    let store = (*crate::leads::Store::open(config)).clone();
    let brand = store.brands.get(&project.id);
    let inbox = brand.and_then(|b| crate::eleva::cached_inbox(&b.slug));
    let ctx = PromptContext {
        journal: Some(&journal),
        ticks: Some(&ticks),
        calls: &library.calls,
        leads: &store.leads,
        crm: brand.zip(inbox.as_ref()),
    };
    let reply = complete(
        config,
        settings,
        SYSTEM,
        &prompt(map, project, messages, all, ctx, now),
        Answer::Json,
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
    let relevant = merge_relevant(
        map,
        project,
        previous,
        messages,
        &reply.relevant,
        MAX_RELEVANT,
    );
    let read = project
        .sources
        .iter()
        .filter(|source| source.mode == Mode::Smart)
        .map(|source| {
            let prev = previous.map_or(0, |p| p.read_through(&source.chat));
            (source.chat.clone(), now.max(prev))
        })
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
        leads: lead_hints(reply.leads, messages),
        relevant,
        read,
    })
}

// -------------------------------------------------------------- providers

/// What shape of answer a prompt asks the model for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Json,
    Text,
}

fn http() -> anyhow::Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()?)
}

/// One question to the analysis's provider and model, for pages other
/// than the analysis (the Leads tab's chat summary). Blocks: call it off
/// the interface thread.
pub fn ask(config: &Path, system: &str, user: &str, answer: Answer) -> anyhow::Result<String> {
    complete(config, &EngineConfig::load(config), system, user, answer)
}

fn request_body(
    provider: Provider,
    model: &str,
    system: &str,
    user: &str,
    answer: Answer,
) -> serde_json::Value {
    match provider {
        Provider::Anthropic => serde_json::json!({
            "model": model,
            "max_tokens": 16000,
            "system": system,
            "messages": [{"role": "user", "content": user}],
        }),
        Provider::GeminiApi => {
            let mut body = serde_json::json!({
                "system_instruction": {"parts": [{"text": system}]},
                "contents": [{"role": "user", "parts": [{"text": user}]}],
            });
            if answer == Answer::Json {
                body["generationConfig"] =
                    serde_json::json!({"responseMimeType": "application/json"});
            }
            body
        }
        _ => {
            let mut body = serde_json::json!({
                "model": model,
                "messages": [
                    {"role": "system", "content": system},
                    {"role": "user", "content": user},
                ],
            });
            if answer == Answer::Json {
                body["response_format"] = serde_json::json!({"type": "json_object"});
            }
            body
        }
    }
}

fn complete(
    _config: &Path,
    settings: &EngineConfig,
    system: &str,
    user: &str,
    answer: Answer,
) -> anyhow::Result<String> {
    let model = settings.model();
    match settings.provider {
        Provider::Off => anyhow::bail!("the analysis is off"),
        Provider::ClaudeCode | Provider::CodexCli | Provider::GeminiCli => {
            cli(settings.provider, &model, system, user)
        }
        Provider::Anthropic => {
            let key = key(Provider::Anthropic)
                .ok_or_else(|| anyhow::anyhow!("Add your Anthropic API key"))?;
            let body = request_body(Provider::Anthropic, &model, system, user, answer);
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
        Provider::GeminiApi => {
            let key = key(Provider::GeminiApi)
                .ok_or_else(|| anyhow::anyhow!("Add your Gemini API key"))?;
            let body = request_body(Provider::GeminiApi, &model, system, user, answer);
            let response = http()?
                .post(format!(
                    "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent"
                ))
                .header("x-goog-api-key", key)
                .header("content-type", "application/json")
                .body(serde_json::to_vec(&body)?)
                .send()?
                .bytes()?;
            let response: serde_json::Value = serde_json::from_slice(&response)?;
            if let Some(error) = response.get("error") {
                anyhow::bail!(
                    "Gemini API: {}",
                    error["message"].as_str().unwrap_or("error")
                );
            }
            Ok(response["candidates"][0]["content"]["parts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|part| part["text"].as_str())
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
            let body = request_body(provider, &model, system, user, answer);
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

/// Where `provider`'s command-line tool is installed, if it is.
pub fn cli_path(provider: Provider) -> Option<PathBuf> {
    crate::ai_tools::find(provider.binary()?)
}

/// Runs a subscription's command-line tool once, with `user` on its input,
/// and returns its answer.
fn cli(provider: Provider, model: &str, system: &str, user: &str) -> anyhow::Result<String> {
    use std::io::Write;
    let binary = cli_path(provider)
        .ok_or_else(|| anyhow::anyhow!("{} is not installed", provider.label()))?;
    let work = std::env::temp_dir().join("whatspro-analysis");
    std::fs::create_dir_all(&work)?;
    let answer = work.join(format!("answer-{}.txt", std::process::id()));
    let mut command = std::process::Command::new(&binary);
    let mut input = user.to_owned();
    match provider {
        Provider::ClaudeCode => {
            command.args([
                "-p",
                "--output-format",
                "json",
                "--append-system-prompt",
                system,
            ]);
            if !model.is_empty() {
                command.args(["--model", model]);
            }
        }
        Provider::CodexCli => {
            // No tools are needed: it only reads and answers.
            command.args(["exec", "--skip-git-repo-check", "--sandbox", "read-only"]);
            command.arg("--output-last-message").arg(&answer);
            if !model.is_empty() {
                command.args(["--model", model]);
            }
            command.arg("-");
            input = format!("{system}\n\n{user}");
        }
        Provider::GeminiCli => {
            command.args(["-p", system]);
            if !model.is_empty() {
                command.args(["--model", model]);
            }
        }
        _ => anyhow::bail!("{} has no command-line tool", provider.label()),
    }
    let mut child = command
        .current_dir(&work)
        .env("PATH", crate::ai_tools::path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("{} did not take the prompt", provider.label()))?
        .write_all(input.as_bytes())?;
    let output = child.wait_with_output()?;
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if provider == Provider::GeminiCli && said.contains("no longer supported") {
        anyhow::bail!(
            "Google no longer lets personal accounts sign in to Gemini CLI. Choose Gemini API with a key from Google AI Studio."
        );
    }
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("");
        anyhow::bail!("{} stopped ({}): {detail}", provider.label(), output.status);
    }
    let text = match provider {
        Provider::ClaudeCode => {
            let reply: serde_json::Value = serde_json::from_slice(&output.stdout)?;
            reply["result"].as_str().unwrap_or_default().to_owned()
        }
        Provider::CodexCli => {
            let text = std::fs::read_to_string(&answer).unwrap_or_default();
            let _ = std::fs::remove_file(&answer);
            text
        }
        _ => String::from_utf8_lossy(&output.stdout).into_owned(),
    };
    Ok(text)
}

// ---------------------------------------------------------- connection test

fn tests() -> &'static Mutex<HashMap<Provider, Result<String, String>>> {
    static TESTS: OnceLock<Mutex<HashMap<Provider, Result<String, String>>>> = OnceLock::new();
    TESTS.get_or_init(Default::default)
}

/// How the last connection test of `provider` went; `Err("")` while one
/// runs.
pub fn tested(provider: Provider) -> Option<Result<String, String>> {
    tests()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&provider)
        .cloned()
}

/// Asks `settings`' provider for a one-word answer, on a thread of its own,
/// to show whether it is installed, signed in and reachable.
pub fn test(ctx: &egui::Context, config: &Path, settings: &EngineConfig) {
    let provider = settings.provider;
    if matches!(tested(provider), Some(Err(ref e)) if e.is_empty()) {
        return;
    }
    tests()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(provider, Err(String::new()));
    let (ctx, config, settings) = (ctx.clone(), config.to_owned(), settings.clone());
    let _ = std::thread::Builder::new()
        .name("engine-test".into())
        .spawn(move || {
            let result = complete(
                &config,
                &settings,
                "Answer with the single word OK.",
                "Say OK.",
                Answer::Text,
            )
            .map_err(|e| e.to_string())
            .and_then(|answer| {
                if answer.trim().is_empty() {
                    Err("It answered nothing. Sign in and try again.".to_owned())
                } else {
                    Ok(answer.trim().chars().take(40).collect())
                }
            });
            tests()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(provider, result);
            ctx.request_repaint();
        });
}

// ------------------------------------------------------------- model lists

type Listing = (Instant, Result<Vec<String>, String>);

fn listings() -> &'static Mutex<HashMap<Provider, Listing>> {
    static LISTINGS: OnceLock<Mutex<HashMap<Provider, Listing>>> = OnceLock::new();
    LISTINGS.get_or_init(Default::default)
}

/// Every model `provider` offers, as last fetched. Asks for them in the
/// background when they are missing or an hour old (or `refresh`), and
/// repaints when they arrive. `None` while the first answer is on its way.
pub fn models(
    ctx: &egui::Context,
    provider: Provider,
    refresh: bool,
) -> Option<Result<Vec<String>, String>> {
    if !provider.lists_models() {
        return Some(Ok(Vec::new()));
    }
    let mut held = listings().lock().unwrap_or_else(|p| p.into_inner());
    let stale = held
        .get(&provider)
        .is_none_or(|(at, _)| refresh || at.elapsed() > Duration::from_secs(3600));
    if stale {
        // Marks it fetched now so a second frame does not ask again.
        let previous = held.get(&provider).map(|(_, r)| r.clone());
        held.insert(
            provider,
            (
                Instant::now(),
                previous.clone().unwrap_or(Err(String::new())),
            ),
        );
        let ctx = ctx.clone();
        let _ = std::thread::Builder::new()
            .name("model-list".into())
            .spawn(move || {
                let result = fetch_models(provider).map_err(|e| e.to_string());
                listings()
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(provider, (Instant::now(), result));
                ctx.request_repaint();
            });
        return previous.filter(|r| r.is_ok());
    }
    held.get(&provider)
        .map(|(_, result)| result.clone())
        .filter(|r| !matches!(r, Err(e) if e.is_empty()))
}

fn fetch_models(provider: Provider) -> anyhow::Result<Vec<String>> {
    let client = http()?;
    let request = match provider {
        Provider::Anthropic => client
            .get("https://api.anthropic.com/v1/models?limit=1000")
            .header(
                "x-api-key",
                key(provider).ok_or_else(|| anyhow::anyhow!("Add your key first"))?,
            )
            .header("anthropic-version", "2023-06-01"),
        Provider::OpenAi => client
            .get("https://api.openai.com/v1/models")
            .bearer_auth(key(provider).ok_or_else(|| anyhow::anyhow!("Add your key first"))?),
        Provider::OpenRouter => client.get("https://openrouter.ai/api/v1/models"),
        Provider::GeminiApi => client
            .get("https://generativelanguage.googleapis.com/v1beta/models?pageSize=1000")
            .header(
                "x-goog-api-key",
                key(provider).ok_or_else(|| anyhow::anyhow!("Add your key first"))?,
            ),
        _ => return Ok(Vec::new()),
    };
    let body = request.send()?.bytes()?;
    let reply: serde_json::Value = serde_json::from_slice(&body)?;
    if let Some(error) = reply.get("error") {
        anyhow::bail!(
            "{}",
            error["message"]
                .as_str()
                .unwrap_or("the models could not be listed")
        );
    }
    // Google lists `models`, named `models/<id>`, with what each can do.
    if provider == Provider::GeminiApi {
        let mut ids: Vec<String> = reply["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|model| {
                model["supportedGenerationMethods"]
                    .as_array()
                    .is_some_and(|methods| methods.iter().any(|m| m == "generateContent"))
            })
            .filter_map(|model| model["name"].as_str())
            .map(|name| name.trim_start_matches("models/").to_owned())
            .filter(|id| id.starts_with("gemini"))
            .collect();
        ids.sort();
        ids.dedup();
        return Ok(ids);
    }
    let mut ids: Vec<String> = reply["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|model| model["id"].as_str().map(str::to_owned))
        // OpenAI also lists embeddings, audio and image models.
        .filter(|id| {
            provider != Provider::OpenAi
                || ((id.starts_with("gpt-") || id.starts_with('o'))
                    && ![
                        "audio",
                        "realtime",
                        "tts",
                        "transcribe",
                        "image",
                        "embedding",
                        "search",
                    ]
                    .iter()
                    .any(|kind| id.contains(kind)))
        })
        .collect();
    ids.sort();
    ids.dedup();
    Ok(ids)
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
        let mut journal = super::super::Journal::default();
        journal.add("Ya mandé la cotización", false);
        let all = [a, b, c];
        let messages = feed_messages(&map, &project, &all);
        let text = prompt(
            &map,
            &project,
            &messages,
            &all,
            PromptContext {
                journal: Some(&journal),
                ..Default::default()
            },
            jiff::Timestamp::now().as_second(),
        );
        assert!(text.contains("Ya mandé la cotización"), "{text}");
        assert!(text.contains("[0] "));
        assert!(text.contains("[2]~ "), "{text}");
        assert!(
            !text.contains("[1]~"),
            "a picked message is not asked about"
        );
        assert!(text.contains("SÍ va: \"las prendas\""));
    }

    #[test]
    fn negative_hand_picks_teach_the_prompt_without_entering_the_feed() {
        let mut map = ProjectMap::default();
        let id = map.add("JAS");
        map.add_source(&id, "p@s.whatsapp.net", Mode::Smart);
        map.pick("p@s.whatsapp.net", "yes", &id, true);
        map.pick("p@s.whatsapp.net", "no", &id, false);
        let yes = message("p@s.whatsapp.net", "yes", 100, "las gorras bordadas");
        let no = message("p@s.whatsapp.net", "no", 200, "partido del domingo");
        let maybe = message("p@s.whatsapp.net", "maybe", 300, "el hilo rojo");
        let all = [yes, no, maybe];
        let project = map.project(&id).unwrap().clone();
        let messages = feed_messages(&map, &project, &all);
        assert_eq!(messages.len(), 2, "negative pick stays out of the feed");
        assert!(messages.iter().all(|m| m.id != "no"));
        let text = prompt(
            &map,
            &project,
            &messages,
            &all,
            PromptContext::default(),
            jiff::Timestamp::now().as_second(),
        );
        assert!(
            text.contains("SÍ va: \"las gorras bordadas\""),
            "positive pick is shown as SÍ va: {text}"
        );
        assert!(
            text.contains("NO va: \"partido del domingo\""),
            "negative pick is shown as NO va even though it is filtered out of messages: {text}"
        );
        let feed_section = text.split("Mensajes (~ = chat mixto").nth(1).unwrap_or("");
        assert!(
            !feed_section.contains("partido del domingo"),
            "negative pick must not appear in the numbered messages section: {feed_section}"
        );
    }

    #[test]
    fn buyers_are_tied_to_their_own_chat() {
        let direct = message("5491100000000@s.whatsapp.net", "a", 10, "¿precio de 50?");
        let mut group = message("g@g.us", "b", 20, "pagué la seña");
        group.sender = "5491122222222@s.whatsapp.net".into();
        let mut mine = message("5491100000000@s.whatsapp.net", "c", 30, "son $90.000");
        mine.from_me = true;
        let reply: Reply = serde_json::from_str(
            r#"{"leads":[
                {"who":"Ana","ref":0,"why":"50 cinturones","stage":"quoted","amount":"$ 90.000"},
                {"who":"","ref":1,"why":"seña","stage":"customer","paid":true,"amount":45000},
                {"who":"Yo","ref":2,"why":"nada","stage":"x"},
                {"who":"Ana otra vez","ref":0,"why":"repetida","stage":"contacted"}
            ]}"#,
        )
        .unwrap();
        let hints = lead_hints(reply.leads, &[&direct, &group, &mine]);
        assert_eq!(
            hints.len(),
            2,
            "our own message and a second hint of Ana are left out"
        );
        assert_eq!(hints[0].person, "5491100000000@s.whatsapp.net");
        assert_eq!(hints[0].stage, "quoted");
        assert_eq!(hints[0].amount.as_deref(), Some("$ 90.000"));
        assert_eq!(hints[1].person, "5491122222222@s.whatsapp.net");
        assert_eq!(
            hints[1].name, "Ana",
            "the sender's name when the reply gives none"
        );
        assert!(hints[1].paid);
        assert_eq!(hints[1].amount.as_deref(), Some("45000"));
        assert_eq!(hints[1].chat.as_deref(), Some("g@g.us"));
    }

    #[test]
    fn replies_map_back_to_their_messages() {
        assert_eq!(json_object("```json\n{\"a\":1}\n```"), Some("{\"a\":1}"));
        assert_eq!(number(&serde_json::json!(3)), Some(3));
        assert_eq!(number(&serde_json::json!("[4]")), Some(4));
        assert_eq!(Provider::Anthropic.default_model(), "claude-opus-5-5");
        assert_eq!(Provider::ClaudeCode.default_model(), "opus");
        assert_eq!(Provider::CodexCli.default_model(), "");
    }

    #[test]
    fn json_format_is_only_requested_for_json_answers() {
        for provider in [Provider::OpenAi, Provider::OpenRouter] {
            let json = request_body(provider, "gpt-5", "sys", "usr", Answer::Json);
            assert_eq!(
                json["response_format"],
                serde_json::json!({"type": "json_object"})
            );
            let text = request_body(provider, "gpt-5", "sys", "usr", Answer::Text);
            assert!(
                text.get("response_format").is_none(),
                "plain-text prompts must not ask {provider:?} for json_object: {text}"
            );
        }
        let gemini_json = request_body(
            Provider::GeminiApi,
            "gemini-2.5-pro",
            "sys",
            "usr",
            Answer::Json,
        );
        assert_eq!(
            gemini_json["generationConfig"]["responseMimeType"],
            "application/json"
        );
        let gemini_text = request_body(
            Provider::GeminiApi,
            "gemini-2.5-pro",
            "sys",
            "usr",
            Answer::Text,
        );
        assert!(gemini_text.get("generationConfig").is_none());
    }

    #[test]
    fn smart_sources_keep_earlier_relevant_messages_across_runs() {
        let mut map = ProjectMap::default();
        let id = map.add("JAS");
        map.add_source(&id, "p@s.whatsapp.net", Mode::Smart);
        map.pick("p@s.whatsapp.net", "excluded", &id, false);
        let project = map.project(&id).unwrap().clone();
        let mut previous = Analysis::default();
        previous.relevant.insert(
            "p@s.whatsapp.net".into(),
            vec![
                "old-1".into(),
                "old-2".into(),
                "excluded".into(),
                "m1".into(),
            ],
        );
        previous
            .relevant
            .insert("gone@s.whatsapp.net".into(), vec!["x".into()]);
        let m1 = message("p@s.whatsapp.net", "m1", 100, "pedido");
        let m2 = message("p@s.whatsapp.net", "m2", 200, "envío");
        let merged = merge_relevant(
            &map,
            &project,
            Some(&previous),
            &[&m1, &m2],
            &[serde_json::json!(0), serde_json::json!(1)],
            3,
        );
        assert!(
            !merged.contains_key("gone@s.whatsapp.net"),
            "removed sources are dropped"
        );
        assert_eq!(
            merged.get("p@s.whatsapp.net").map(Vec::as_slice),
            Some(&["old-2".to_owned(), "m1".to_owned(), "m2".to_owned()][..]),
            "earlier relevant ids stay, excluded ids drop, and the cap keeps the newest"
        );
    }

    #[test]
    fn the_prompt_unifies_all_sources_and_stays_under_budget() {
        let now = jiff::Timestamp::now().as_second();
        let mut map = ProjectMap::default();
        let id = map.add("JAS");
        map.add_source(&id, "g@g.us", Mode::All);
        let project = map.project(&id).unwrap().clone();

        let mut journal = super::super::Journal::default();
        journal.add("Se aprobó el diseño de hebillas", false);

        let call = crate::call_library::Call {
            id: "call-1".into(),
            title: "Revisión con taller".into(),
            at: now - 2 * 86_400,
            seconds: 420,
            origin: crate::call_library::Origin::Recorded,
            file: "call-1.wav".into(),
            project: Some(id.clone()),
            lead: None,
            transcripts: Vec::new(),
            analysis: Some(crate::call_library::CallAnalysis {
                summary: "Acordaron entregar la tanda el jueves.".into(),
                decisions: Vec::new(),
                commitments: vec![crate::call_library::Commitment {
                    who: "Martín".into(),
                    what: "mandar remito".into(),
                    due: Some("jueves".into()),
                }],
                tasks: Vec::new(),
                next_steps: vec!["Confirmar transporte".into()],
                at: now - 2 * 86_400,
            }),
            chat: None,
            account: None,
            started_at: now - 2 * 86_400,
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
        };

        let mut lead = crate::leads::Lead::new(
            &id,
            "Lucía Gómez",
            Some("5491133334444@s.whatsapp.net".into()),
            Some("5491133334444".into()),
        );
        lead.set_stage(crate::leads::Stage::Quoted, None);
        lead.value = Some("$ 180.000".into());
        lead.set_next("Confirmar pago de seña", Some("2026-10-20".into()));
        lead.add(crate::leads::Kind::Note, "Pidió 100 cinturones negros");

        let brand = crate::leads::Brand {
            slug: "jas".into(),
            name: "JAS Marroquinería".into(),
            write: true,
        };
        let inbox = crate::eleva::Inbox {
            waiting: vec![crate::eleva::Waiting {
                reason: "Reclamo".into(),
                ago: "hace 2 días".into(),
                who: "Carlos".into(),
                phone: Some("5491199990000".into()),
                detail: Some("Falta guía de correo".into()),
                id: "w-1".into(),
            }],
            total: 3,
            alerts: vec![crate::eleva::Situation {
                key: "pago_informado".into(),
                title: "Pago informado".into(),
                when: Some("hoy".into()),
                who: Some("Sofía".into()),
                phone: None,
                excerpt: Some("Adjuntó comprobante".into()),
                working: false,
                id: "s-1".into(),
            }],
            agenda: Vec::new(),
            read_at: now,
            error: None,
        };

        let filler = "x".repeat(450);
        let mut all: Vec<FeedMessage> = (0..180)
            .map(|i| {
                message(
                    "g@g.us",
                    &format!("m-{i}"),
                    now - (200 - i) * 60,
                    &format!("mensaje-{i} {filler}"),
                )
            })
            .collect();
        all.push(message(
            "g@g.us",
            "m-newest",
            now - 10,
            "Último mensaje clave del taller",
        ));
        let messages = feed_messages(&map, &project, &all);

        let text = prompt(
            &map,
            &project,
            &messages,
            &all,
            PromptContext {
                journal: Some(&journal),
                ticks: None,
                calls: std::slice::from_ref(&call),
                leads: std::slice::from_ref(&lead),
                crm: Some((&brand, &inbox)),
            },
            now,
        );

        assert!(
            text.len() <= PROMPT_BUDGET,
            "prompt must stay within PROMPT_BUDGET ({} > {PROMPT_BUDGET})",
            text.len()
        );
        assert!(
            text.contains("Se aprobó el diseño de hebillas"),
            "journal note included"
        );
        assert!(
            text.contains("Revisión con taller")
                && text.contains("Martín: mandar remito (para jueves)")
                && text.contains("Confirmar transporte"),
            "recent call summary, commitments and next steps included: {text}"
        );
        assert!(
            text.contains("Lucía Gómez")
                && text.contains("etapa: cotizado")
                && text.contains("monto: $ 180.000")
                && text.contains("Confirmar pago de seña (vence 2026-10-20)")
                && text.contains("Pidió 100 cinturones negros"),
            "lead stage, amount, next action + due and recent entries included: {text}"
        );
        assert!(
            text.contains("del CRM de la empresa")
                && text.contains("3 esperando respuesta, 1 alertas de atención")
                && text.contains("Carlos (Reclamo, hace 2 días)")
                && text.contains("Pago informado · Sofía"),
            "secondary ElevaLeads CRM context included: {text}"
        );
        assert!(
            text.contains("Último mensaje clave del taller"),
            "newest chat message kept"
        );
        assert!(
            !text.contains("mensaje-0 "),
            "oldest chat messages truncated first to fit budget"
        );
    }
}
