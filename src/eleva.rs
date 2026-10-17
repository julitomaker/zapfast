//! ElevaLeads, the WhatsApp CRM a project's brand may run on.
//!
//! ElevaLeads has a remote MCP server at `https://elevaleads.com/api/mcp`:
//! JSON-RPC over HTTPS, with a token made in ElevaLeads (Settings →
//! Connect with AI) that is limited to one brand and is either read-only or
//! read and write. Whats Pro uses it for two things:
//!
//! - reading what waits for a person in the brand (`pendientes`, and the
//!   `situaciones` to attend and on the agenda) into the project's panel;
//! - sending what is written about a linked lead to that customer's file
//!   (`nota_cliente`, `mover_etapa_cliente`, `crear_situacion_manual`), and
//!   taking a conversation out of the brand's queue (`resolver_pendiente`,
//!   `atender_situacion`). None of them sends anything to the customer.
//!
//! The tools answer in text, one item per line ending in `id:<id>`; the
//! readers here take those lines apart and skip what they do not know. The
//! token goes in the `Authorization` header, never in the address, and lives
//! in the OS keyring (`leads::token_account`).
//!
//! The calls run on threads of their own; the page reads the latest result
//! and asks for a new one, so it never waits for the network.

use std::{
    collections::HashMap,
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};

use crate::leads::{self, ElevaCustomer, Kind, Stage, Store, Sync};

pub mod sales;

const ENDPOINT: &str = "https://elevaleads.com/api/mcp";
/// How often a brand's inbox is read again while the page shows it.
pub const REFRESH: Duration = Duration::from_secs(5 * 60);

/// What a token gives access to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connection {
    pub slug: String,
    pub name: String,
    pub write: bool,
}

/// A conversation waiting for a person in the brand's inbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Waiting {
    /// Why it waits: "Reclamo", "Pide una persona", "No se entregó"…
    pub reason: String,
    /// How long ago, as ElevaLeads writes it ("3 días").
    pub ago: String,
    pub who: String,
    pub phone: Option<String>,
    pub detail: Option<String>,
    /// The conversation's id.
    pub id: String,
}

/// A situation the brand's agent caught: an alert to attend or a date.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Situation {
    /// Its kind's key (`pago_informado`, `retiro_local`…).
    pub key: String,
    pub title: String,
    pub when: Option<String>,
    pub who: Option<String>,
    pub phone: Option<String>,
    pub excerpt: Option<String>,
    pub working: bool,
    pub id: String,
}

/// What was last read from a brand.
#[derive(Clone, Debug, Default)]
pub struct Inbox {
    pub waiting: Vec<Waiting>,
    /// How many wait in all; the list holds the most urgent.
    pub total: usize,
    pub alerts: Vec<Situation>,
    pub agenda: Vec<Situation>,
    /// When it was read, as Unix seconds.
    pub read_at: i64,
    pub error: Option<String>,
}

// ------------------------------------------------------------------ client

fn token(slug: &str) -> Option<String> {
    crate::keys::get(&leads::token_account(slug))
}

/// Saves a brand's token in the keyring; an empty one removes it.
pub fn set_token(slug: &str, token: &str) -> anyhow::Result<()> {
    crate::keys::set(&leads::token_account(slug), token)
}

fn http() -> anyhow::Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(45))
        .build()?)
}

fn rpc(token: &str, method: &str, params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    let body = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let response = http()?
        .post(ENDPOINT)
        .bearer_auth(token)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(serde_json::to_vec(&body)?)
        .send()?;
    let status = response.status();
    let value: serde_json::Value = serde_json::from_slice(&response.bytes()?)
        .map_err(|_| anyhow::anyhow!("ElevaLeads answered {status}"))?;
    if let Some(error) = value.get("error") {
        anyhow::bail!(
            "{}",
            error["message"]
                .as_str()
                .unwrap_or("ElevaLeads refused the call")
        );
    }
    Ok(value["result"].clone())
}

/// Calls a tool and returns its text, or its own words when it refused.
fn call(token: &str, tool: &str, arguments: serde_json::Value) -> anyhow::Result<String> {
    let result = rpc(
        token,
        "tools/call",
        serde_json::json!({ "name": tool, "arguments": arguments }),
    )?;
    let text = result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if result["isError"].as_bool() == Some(true) {
        anyhow::bail!("{}", without_brand(&text).trim());
    }
    Ok(without_brand(&text).to_owned())
}

/// Checks a token and says which brand it opens and whether it may write.
pub fn connect(token: &str) -> anyhow::Result<Connection> {
    let token = token.trim();
    if !token.starts_with("elmcp_") {
        anyhow::bail!("An ElevaLeads token starts with elmcp_");
    }
    let hello = rpc(
        token,
        "initialize",
        serde_json::json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "Whats Pro", "version": env!("CARGO_PKG_VERSION") } }),
    )?;
    let write = parse_write(hello["instructions"].as_str().unwrap_or_default());
    let brands = parse_brands(&call(token, "listar_marcas", serde_json::json!({}))?);
    match brands.as_slice() {
        [(slug, name)] => Ok(Connection {
            slug: slug.clone(),
            name: name.clone(),
            write,
        }),
        [] => anyhow::bail!("This token opens no brand"),
        _ => anyhow::bail!(
            "This token opens every brand of the account: make one limited to a single brand"
        ),
    }
}

/// Reads what waits in a brand.
pub fn read_inbox(slug: &str) -> Inbox {
    let mut inbox = Inbox {
        read_at: jiff::Timestamp::now().as_second(),
        ..Inbox::default()
    };
    let Some(token) = token(slug) else {
        inbox.error = Some("The brand's token is not in this computer's keyring".into());
        return inbox;
    };
    match call(&token, "pendientes", serde_json::json!({ "limite": 40 })) {
        Ok(text) => (inbox.total, inbox.waiting) = parse_waiting(&text),
        Err(error) => inbox.error = Some(error.to_string()),
    }
    for (view, list) in [
        ("atencion", &mut inbox.alerts),
        ("agenda", &mut inbox.agenda),
    ] {
        match call(&token, "situaciones", serde_json::json!({ "vista": view })) {
            Ok(text) => *list = parse_situations(&text),
            Err(error) => {
                inbox.error.get_or_insert(error.to_string());
            }
        }
    }
    inbox
}

/// Looks for the brand's customer with this phone.
pub fn find_customer(slug: &str, phone: &str) -> anyhow::Result<Option<ElevaCustomer>> {
    let token = token(slug).ok_or_else(|| anyhow::anyhow!("The brand's token is missing"))?;
    match call(
        &token,
        "ver_cliente",
        serde_json::json!({ "cliente": phone }),
    ) {
        Ok(text) => Ok(parse_customer(&text)),
        Err(error) if error.to_string().starts_with("No encontré") => Ok(None),
        Err(error) => Err(error),
    }
}

/// Takes a conversation out of the brand's queue, or marks a situation as
/// attended. Sends nothing to the customer.
pub fn resolve(slug: &str, id: &str, situation: bool) -> anyhow::Result<String> {
    let token = token(slug).ok_or_else(|| anyhow::anyhow!("The brand's token is missing"))?;
    let answer = if situation {
        call(&token, "atender_situacion", serde_json::json!({ "id": id }))?
    } else {
        call(
            &token,
            "resolver_pendiente",
            serde_json::json!({ "id": id, "nota": "Atendido desde Whats Pro" }),
        )?
    };
    // `resolver_pendiente` says it found nothing without flagging an error.
    if refused(&answer) {
        anyhow::bail!("{}", answer.trim());
    }
    Ok(answer)
}

/// The tool call that carries an entry to the customer's file. Most tools
/// take the customer's id; a situation finds its customer by name or phone
/// only, so it gets `contact`.
fn entry_call(
    customer: &str,
    contact: &str,
    kind: Kind,
    text: &str,
) -> Option<(&'static str, serde_json::Value)> {
    let note = |label: &str| {
        let text: String = format!("[Whats Pro] {label}: {text}")
            .chars()
            .take(1000)
            .collect();
        serde_json::json!({ "cliente": customer, "nota": text })
    };
    match kind {
        Kind::Note => Some(("nota_cliente", note("Nota"))),
        Kind::Call => Some(("nota_cliente", note("Llamada"))),
        Kind::FollowUp => Some(("nota_cliente", note("Seguimiento"))),
        Kind::Summary => Some(("nota_cliente", note("Resumen del chat"))),
        Kind::Stage => {
            let stage: Stage =
                serde_json::from_value(serde_json::Value::String(text.into())).ok()?;
            Some((
                "mover_etapa_cliente",
                serde_json::json!({ "cliente": customer, "etapa": stage.eleva() }),
            ))
        }
        Kind::Task => Some((
            "crear_situacion_manual",
            serde_json::json!({
                "titulo": text.chars().take(200).collect::<String>(),
                "cliente": contact,
                "detalle": "Próxima acción anotada en Whats Pro",
            }),
        )),
    }
}

// --------------------------------------------------------------- threads

#[derive(Default)]
struct Shared {
    inboxes: HashMap<String, Inbox>,
    reading: HashMap<String, Instant>,
    pushing: bool,
    /// Connections being checked, by project, and their answer.
    connecting: HashMap<String, Option<Result<Connection, String>>>,
    /// Lookups of a lead's customer, by lead, and their answer.
    finding: HashMap<String, Option<Result<Option<ElevaCustomer>, String>>>,
    /// The last answer to resolving an item, by its id.
    resolved: HashMap<String, Result<String, String>>,
}

static SHARED: Mutex<Option<Shared>> = Mutex::new(None);

fn shared<T>(f: impl FnOnce(&mut Shared) -> T) -> T {
    let mut guard = SHARED.lock().unwrap_or_else(|p| p.into_inner());
    f(guard.get_or_insert_with(Shared::default))
}

/// The brand's inbox as last read, asking for a fresh one when it is older
/// than `REFRESH` (or `force`).
pub fn inbox(ctx: &egui::Context, slug: &str, force: bool) -> Option<Inbox> {
    let (current, start) = shared(|s| {
        let current = s.inboxes.get(slug).cloned();
        let stale = current.as_ref().is_none_or(|inbox| {
            jiff::Timestamp::now().as_second() - inbox.read_at > REFRESH.as_secs() as i64
        });
        let busy = s
            .reading
            .get(slug)
            .is_some_and(|at| at.elapsed() < Duration::from_secs(90));
        let start = (stale || force) && !busy;
        if start {
            s.reading.insert(slug.to_owned(), Instant::now());
        }
        (current, start)
    });
    if start {
        let ctx = ctx.clone();
        let slug = slug.to_owned();
        std::thread::spawn(move || {
            let inbox = read_inbox(&slug);
            shared(|s| {
                s.reading.remove(&slug);
                s.inboxes.insert(slug, inbox);
            });
            ctx.request_repaint();
        });
    }
    ctx.request_repaint_after(REFRESH);
    current
}

/// Puts a brand's inbox in place without reading it, for the demo.
pub fn preload(slug: &str, inbox: Inbox) {
    shared(|s| s.inboxes.insert(slug.to_owned(), inbox));
}

/// Whether the brand's inbox is being read right now.
pub fn reading(slug: &str) -> bool {
    shared(|s| s.reading.contains_key(slug))
}

/// Checks a token for `project` on a thread; `connection` has the answer.
pub fn start_connect(ctx: &egui::Context, project: &str, token: String) {
    shared(|s| s.connecting.insert(project.to_owned(), None));
    let ctx = ctx.clone();
    let project = project.to_owned();
    std::thread::spawn(move || {
        let result = connect(&token).and_then(|connection| {
            set_token(&connection.slug, &token)?;
            Ok(connection)
        });
        shared(|s| {
            s.connecting
                .insert(project, Some(result.map_err(|e| e.to_string())))
        });
        ctx.request_repaint();
    });
}

/// `None` when nothing is being checked for `project`; `Some(None)` while it
/// is; then the answer, once, after which it is forgotten.
pub fn connection(project: &str) -> Option<Option<Result<Connection, String>>> {
    shared(|s| match s.connecting.get(project) {
        Some(None) => Some(None),
        Some(Some(_)) => s.connecting.remove(project),
        None => None,
    })
}

/// Whether a token for `project` is being checked.
pub fn connection_pending(project: &str) -> bool {
    shared(|s| matches!(s.connecting.get(project), Some(None)))
}

/// Whether a lead's customer is being looked for.
pub fn found_pending(lead: &str) -> bool {
    shared(|s| matches!(s.finding.get(lead), Some(None)))
}

/// Looks for a lead's customer on a thread; `found` has the answer.
pub fn start_find(ctx: &egui::Context, lead: &str, slug: &str, phone: &str) {
    shared(|s| s.finding.insert(lead.to_owned(), None));
    let (ctx, lead, slug, phone) = (
        ctx.clone(),
        lead.to_owned(),
        slug.to_owned(),
        phone.to_owned(),
    );
    std::thread::spawn(move || {
        let result = find_customer(&slug, &phone).map_err(|e| e.to_string());
        shared(|s| s.finding.insert(lead, Some(result)));
        ctx.request_repaint();
    });
}

/// As `connection`, for a lead's lookup.
pub fn found(lead: &str) -> Option<Option<Result<Option<ElevaCustomer>, String>>> {
    shared(|s| match s.finding.get(lead) {
        Some(None) => Some(None),
        Some(Some(_)) => s.finding.remove(lead),
        None => None,
    })
}

/// Resolves an inbox item on a thread, then reads the inbox again.
pub fn start_resolve(ctx: &egui::Context, slug: &str, id: &str, situation: bool) {
    let (ctx, slug, id) = (ctx.clone(), slug.to_owned(), id.to_owned());
    std::thread::spawn(move || {
        let result = resolve(&slug, &id, situation).map_err(|e| e.to_string());
        if result.is_ok() {
            shared(|s| {
                if let Some(inbox) = s.inboxes.get_mut(&slug) {
                    inbox.waiting.retain(|item| item.id != id);
                    inbox.alerts.retain(|item| item.id != id);
                    inbox.agenda.retain(|item| item.id != id);
                    inbox.total = inbox.total.saturating_sub(usize::from(!situation));
                }
            });
        }
        shared(|s| s.resolved.insert(id, result));
        ctx.request_repaint();
    });
}

/// The answer to resolving `id`, once.
pub fn resolved(id: &str) -> Option<Result<String, String>> {
    shared(|s| s.resolved.remove(id))
}

/// Sends every pending entry of the linked leads to their brands, on a
/// thread; one push at a time.
pub fn push(ctx: &egui::Context, config: &Path) {
    let start = shared(|s| !std::mem::replace(&mut s.pushing, true));
    if !start {
        return;
    }
    let (ctx, config) = (ctx.clone(), config.to_owned());
    std::thread::spawn(move || {
        // What to send is read under the lock, sent without it, and written
        // back under it, so the page never waits for the network.
        type Work = (String, String, bool, (String, String), String, Kind, String);
        let work: Vec<Work> = {
            let store = Store::open(&config);
            store
                .leads
                .iter()
                .filter_map(|lead| {
                    let brand = store.brands.get(&lead.project)?;
                    let customer = lead.eleva.as_ref()?;
                    Some(
                        lead.entries
                            .iter()
                            .filter(|e| e.sync == Sync::Pending)
                            .map(|entry| {
                                (
                                    lead.id.clone(),
                                    brand.slug.clone(),
                                    brand.write,
                                    (
                                        customer.id.clone(),
                                        lead.phone.clone().unwrap_or_else(|| customer.name.clone()),
                                    ),
                                    entry.id.clone(),
                                    entry.kind,
                                    entry.text.clone(),
                                )
                            }),
                    )
                })
                .flatten()
                .collect()
        };
        let mut results = Vec::new();
        for (lead, slug, write, (customer, contact), entry, kind, text) in work {
            let result = if !write {
                Sync::Failed("ElevaLeads token is read-only".into())
            } else if let Some((tool, arguments)) = entry_call(&customer, &contact, kind, &text) {
                match token(&slug).map(|token| call(&token, tool, arguments)) {
                    Some(Ok(_)) => Sync::Sent,
                    Some(Err(error)) => Sync::Failed(error.to_string()),
                    None => Sync::Failed("The brand's token is missing".into()),
                }
            } else {
                Sync::Sent
            };
            results.push((lead, entry, result));
        }
        if !results.is_empty() {
            let mut store = Store::open(&config);
            for (lead, entry, result) in results {
                if let Some(entry) = store
                    .get_mut(&lead)
                    .and_then(|lead| lead.entries.iter_mut().find(|e| e.id == entry))
                    && entry.sync == Sync::Pending
                {
                    entry.sync = result;
                }
            }
            store.save();
        }
        shared(|s| s.pushing = false);
        ctx.request_repaint();
    });
}

// ----------------------------------------------------------------- readers

/// Whether an answer that is not flagged as an error still did nothing.
fn refused(answer: &str) -> bool {
    let answer = answer.trim_start();
    answer.starts_with("No encontré") || answer.starts_with("Falta")
}

/// Drops the `[Marca: X]` line every answer starts with.
fn without_brand(text: &str) -> &str {
    match text.strip_prefix("[Marca:") {
        Some(rest) => rest.split_once('\n').map_or("", |(_, rest)| rest),
        None => text,
    }
}

fn parse_write(instructions: &str) -> bool {
    instructions.contains("lectura y edición")
}

/// `- Bordados JAS (marca: bordados-jas) · tipo mixto · …` → slug and name.
fn parse_brands(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim().strip_prefix("- ")?;
            let (name, rest) = line.split_once(" (marca: ")?;
            let (slug, _) = rest.split_once(')')?;
            Some((slug.trim().to_owned(), name.trim().to_owned()))
        })
        .collect()
}

/// A line's parts between ` · `, and its trailing id.
fn parts(line: &str) -> Option<(Vec<&str>, String)> {
    let line = line.trim().strip_prefix("- ")?;
    let (body, id) = line.rsplit_once(" · id:")?;
    let id = id.trim();
    (!id.is_empty()).then(|| (body.split(" · ").collect(), id.to_owned()))
}

/// `Name (+5491…)` → the name and the phone; a name alone stays whole.
fn person(text: &str) -> (String, Option<String>) {
    if let Some(open) = text.rfind(" (")
        && let Some(inner) = text[open + 2..].strip_suffix(')')
    {
        let phone = (leads::digits(inner).len() >= 7).then(|| inner.to_owned());
        return (text[..open].trim().to_owned(), phone);
    }
    (text.trim().to_owned(), None)
}

/// The `pendientes` answer: how many wait, and the listed ones.
fn parse_waiting(text: &str) -> (usize, Vec<Waiting>) {
    let total = text
        .lines()
        .find_map(|line| {
            let rest = line.trim().strip_prefix("ESPERANDO (")?;
            rest.split_whitespace().next()?.parse().ok()
        })
        .unwrap_or(0);
    let items: Vec<Waiting> = text
        .lines()
        .filter_map(|line| {
            let (parts, id) = parts(line)?;
            let mut parts = parts.into_iter();
            let reason = parts.next()?.trim().to_owned();
            let mut rest: Vec<&str> = parts.collect();
            let ago = rest
                .first()
                .and_then(|first| first.strip_prefix("hace "))
                .map(str::to_owned);
            if ago.is_some() {
                rest.remove(0);
            }
            let (who, phone) = rest.first().map(|who| person(who)).unwrap_or_default();
            let detail = (rest.len() > 1).then(|| rest[1..].join(" · "));
            Some(Waiting {
                reason,
                ago: ago.unwrap_or_default(),
                who,
                phone,
                detail,
                id,
            })
        })
        .collect();
    (total.max(items.len()), items)
}

/// The `situaciones` answer.
fn parse_situations(text: &str) -> Vec<Situation> {
    text.lines()
        .filter_map(|line| {
            let (mut parts, id) = parts(line)?;
            let head = parts.first_mut()?;
            let rest = head.strip_prefix('[')?;
            let (key, title) = rest.split_once(']')?;
            let (working, title) = match title.trim().strip_prefix("(en curso)") {
                Some(title) => (true, title.trim()),
                None => (false, title.trim()),
            };
            let mut situation = Situation {
                key: key.to_owned(),
                title: title.to_owned(),
                when: None,
                who: None,
                phone: None,
                excerpt: None,
                working,
                id,
            };
            // From the end: an excerpt in quotes, then the customer with a
            // phone, then a time. What is left belongs to the title.
            let mut tail: Vec<&str> = parts[1..].to_vec();
            if let Some(last) = tail.last()
                && last.starts_with('"')
            {
                situation.excerpt = Some(last.trim_matches('"').to_owned());
                tail.pop();
            }
            if let Some(last) = tail.last() {
                let (who, phone) = person(last);
                if phone.is_some() || last.ends_with("(s/tel)") {
                    situation.who = Some(who);
                    situation.phone = phone;
                    tail.pop();
                }
            }
            if let Some(index) = tail.iter().position(|part| part.starts_with('⏰')) {
                situation.when = Some(
                    tail.remove(index)
                        .trim_start_matches('⏰')
                        .trim()
                        .to_owned(),
                );
            }
            if !tail.is_empty() {
                situation.title = std::iter::once(situation.title.as_str())
                    .chain(tail)
                    .collect::<Vec<_>>()
                    .join(" · ");
            }
            Some(situation)
        })
        .collect()
}

/// The `ver_cliente` answer: `# Name`, `id:…`, `- Etapa: Label · …`.
fn parse_customer(text: &str) -> Option<ElevaCustomer> {
    let mut name = None;
    let mut id = None;
    let mut stage = None;
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("# ") {
            name.get_or_insert_with(|| rest.split(" · ").next().unwrap_or(rest).to_owned());
        } else if let Some(rest) = line.strip_prefix("id:") {
            id.get_or_insert_with(|| rest.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("- Etapa:") {
            stage = rest
                .split(" · ")
                .next()
                .map(|label| label.trim().to_owned());
        }
    }
    Some(ElevaCustomer {
        id: id.filter(|id| !id.is_empty())?,
        name: name.unwrap_or_default(),
        stage,
    })
}

/// The address of a conversation, or of the inbox, in ElevaLeads.
pub fn inbox_url(slug: &str, conversation: Option<&str>) -> String {
    match conversation {
        Some(id) => format!("https://elevaleads.com/app/{slug}/inbox?c={id}"),
        None => format!("https://elevaleads.com/app/{slug}/inbox"),
    }
}

/// The address of the brand's situations.
pub fn situations_url(slug: &str) -> String {
    format!("https://elevaleads.com/app/{slug}/situaciones")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Answers as the server gives them, with made-up people.
    const WAITING: &str = "[Marca: Blackyak Labs]\nESPERANDO (80 en total, las 4 más urgentes):\n\
- No se entregó · hace 9 días · David Paz (+5493425000000) · Meta frenó este mensaje de marketing · id:cmA\n\
- Reclamo · hace 3 días · Mike (+5491132000000) · id:cmB\n\
- Pide una persona · hace 21 días · tatuador profesional realismo y más estilos, (+5491154000000) · id:cmC\n\
- Sin confirmar · hace 11 días · Ana García · quedó sin confirmación de WhatsApp — puede haber llegado igual · id:cmD";

    const SITUATIONS: &str = "[Marca: Bordados JAS]\nPARA ATENDER (la que más espera, primero):\n\
- [pago_informado] Pago informado — $185.000 · 30/09/2026 · BANCO X · 56930000000 (56930000000) · \"Comprobante de transferencia\" · id:s1\n\
- [reclamo] (en curso) Reclamo / queja — molestia · Evelyn Paz (56961000000) · id:s2\n\
- [retiro_local] Retiro en el local — cajas · ⏰ 16/10, 11:00 a. m. · Cris Rig (56994000000) · \"mañana van a retirar\" · id:s3\n\
- [tarea_manual] Llamar a Pérez · id:s4";

    #[test]
    fn reads_the_waiting_queue() {
        let (total, items) = parse_waiting(WAITING);
        assert_eq!(total, 80);
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].reason, "No se entregó");
        assert_eq!(items[0].ago, "9 días");
        assert_eq!(items[0].who, "David Paz");
        assert_eq!(items[0].phone.as_deref(), Some("+5493425000000"));
        assert_eq!(
            items[0].detail.as_deref(),
            Some("Meta frenó este mensaje de marketing")
        );
        assert_eq!(items[1].detail, None);
        assert_eq!(items[2].who, "tatuador profesional realismo y más estilos,");
        assert_eq!(items[3].phone, None);
        assert_eq!(items[3].id, "cmD");
        assert_eq!(parse_waiting("Nadie esperando. 👌"), (0, vec![]));
    }

    #[test]
    fn reads_situations() {
        let items = parse_situations(SITUATIONS);
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].key, "pago_informado");
        assert_eq!(
            items[0].title,
            "Pago informado — $185.000 · 30/09/2026 · BANCO X"
        );
        assert_eq!(items[0].who.as_deref(), Some("56930000000"));
        assert_eq!(
            items[0].excerpt.as_deref(),
            Some("Comprobante de transferencia")
        );
        assert!(items[1].working);
        assert_eq!(items[1].title, "Reclamo / queja — molestia");
        assert_eq!(items[2].when.as_deref(), Some("16/10, 11:00 a. m."));
        assert_eq!(items[2].who.as_deref(), Some("Cris Rig"));
        assert_eq!(items[3].title, "Llamar a Pérez");
        assert_eq!(items[3].who, None);
    }

    #[test]
    fn reads_a_customer_and_the_connection() {
        let text = "[Marca: Bordados JAS]\n# Edson Bord\nid:cmvbz\n- Contacto: 56950000000\n- Etapa: En conversación · Tipo: sin_clasificar";
        let customer = parse_customer(without_brand(text)).unwrap();
        assert_eq!(customer.id, "cmvbz");
        assert_eq!(customer.name, "Edson Bord");
        assert_eq!(customer.stage.as_deref(), Some("En conversación"));
        assert_eq!(parse_customer("No encontré a \"x\" en esta marca."), None);

        let brands = parse_brands(
            "Este token está limitado a 1 marca:\n- Bordados JAS (marca: bordados-jas) · tipo mixto · rubro servicios · tu rol: admin",
        );
        assert_eq!(
            brands,
            [("bordados-jas".to_owned(), "Bordados JAS".to_owned())]
        );
        assert!(parse_write(
            "Este token opera la marca X (lectura y edición)."
        ));
        assert!(!parse_write("Este token opera la marca X (solo lectura)."));
        assert!(refused(without_brand(
            "[Marca: Blackyak Labs]\nNo encontré esa conversación en esta marca."
        )));
        assert!(!refused("Listo: la conversación salió de Pendientes."));
    }

    #[test]
    fn entries_become_the_right_calls() {
        let (tool, args) = entry_call("c1", "569", Kind::Call, "Quiere 500").unwrap();
        assert_eq!(tool, "nota_cliente");
        assert_eq!(args["nota"], "[Whats Pro] Llamada: Quiere 500");
        let (tool, args) = entry_call("c1", "569", Kind::Stage, "quoted").unwrap();
        assert_eq!(tool, "mover_etapa_cliente");
        assert_eq!(args["etapa"], "interesado");
        let (tool, args) =
            entry_call("c1", "569", Kind::Task, "Mandar precio · 2026-10-20").unwrap();
        assert_eq!(tool, "crear_situacion_manual");
        assert_eq!(args["cliente"], "569");
        assert!(entry_call("c1", "569", Kind::Stage, "nonsense").is_none());
    }
}
