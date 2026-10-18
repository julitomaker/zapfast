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

/// Set by the demo: nothing reaches ElevaLeads or the keyring.
static OFFLINE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Keeps every ElevaLeads call from leaving this computer, for the demo.
pub fn go_offline() {
    OFFLINE.store(true, std::sync::atomic::Ordering::Relaxed);
}

pub fn offline() -> bool {
    OFFLINE.load(std::sync::atomic::Ordering::Relaxed)
}

fn token(slug: &str) -> Option<String> {
    if offline() {
        return None;
    }
    crate::keys::get(&leads::token_account(slug))
}

/// Calls a tool of the brand `slug`, with its token, naming the brand so a
/// token that opens several brands still writes to the right one.
fn brand_call(slug: &str, tool: &str, mut arguments: serde_json::Value) -> anyhow::Result<String> {
    let token = token(slug).ok_or_else(|| anyhow::anyhow!("The brand's token is missing"))?;
    if let Some(object) = arguments.as_object_mut() {
        object.insert("marca".into(), serde_json::Value::String(slug.to_owned()));
    }
    call(&token, tool, arguments)
}

/// Whether an error is the network's or the server's, worth trying again
/// later, rather than the brand saying no.
fn transient(error: &anyhow::Error) -> bool {
    let text = error.to_string();
    error.downcast_ref::<reqwest::Error>().is_some()
        || text.starts_with("ElevaLeads answered 5")
        || text.contains("Demasiadas solicitudes")
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
    if token(slug).is_none() {
        inbox.error = Some("The brand's token is not in this computer's keyring".into());
        return inbox;
    }
    match brand_call(slug, "pendientes", serde_json::json!({ "limite": 40 })) {
        Ok(text) => (inbox.total, inbox.waiting) = parse_waiting(&text),
        Err(error) => inbox.error = Some(error.to_string()),
    }
    for (view, list) in [
        ("atencion", &mut inbox.alerts),
        ("agenda", &mut inbox.agenda),
    ] {
        match brand_call(slug, "situaciones", serde_json::json!({ "vista": view })) {
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
    match brand_call(slug, "ver_cliente", serde_json::json!({ "cliente": phone })) {
        Ok(text) => Ok(parse_customer(&text)),
        Err(error) if error.to_string().starts_with("No encontré") => Ok(None),
        Err(error) => Err(error),
    }
}

/// Takes a conversation out of the brand's queue, or marks a situation as
/// attended. Sends nothing to the customer.
pub fn resolve(slug: &str, id: &str, situation: bool) -> anyhow::Result<String> {
    let answer = if situation {
        brand_call(slug, "atender_situacion", serde_json::json!({ "id": id }))?
    } else {
        brand_call(
            slug,
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
    /// Items resolved this session, by brand: a read already under way
    /// must not bring them back.
    gone: std::collections::HashSet<(String, String)>,
    /// A push was asked for while one was running.
    again: bool,
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
        let start = (stale || force) && !busy && !offline();
        if start {
            s.reading.insert(slug.to_owned(), Instant::now());
        }
        (current, start)
    });
    if start {
        let ctx = ctx.clone();
        let slug = slug.to_owned();
        std::thread::spawn(move || {
            let mut inbox = read_inbox(&slug);
            shared(|s| {
                let gone = |id: &str| s.gone.contains(&(slug.clone(), id.to_owned()));
                let before = inbox.waiting.len();
                inbox.waiting.retain(|item| !gone(&item.id));
                inbox.total = inbox.total.saturating_sub(before - inbox.waiting.len());
                inbox.alerts.retain(|item| !gone(&item.id));
                inbox.agenda.retain(|item| !gone(&item.id));
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

/// The brand's inbox as last read, without starting a network call.
pub fn cached_inbox(slug: &str) -> Option<Inbox> {
    shared(|s| s.inboxes.get(slug).cloned())
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
                s.gone.insert((slug.clone(), id.clone()));
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

/// One entry to send, with what is needed to send it without the store.
struct Work {
    lead: String,
    slug: String,
    write: bool,
    customer: ElevaCustomer,
    /// How a situation finds the customer: their phone as the brand has it.
    contact: String,
    entry: String,
    kind: Kind,
    text: String,
}

/// What became of a call.
enum Outcome {
    Sent(Option<String>),
    Failed(String),
    /// The network failed: it stays pending for the next push.
    Later,
}

/// Sends every pending entry of the linked leads to their brands, and
/// closes the situations of next actions that were done or replaced, on a
/// thread; one push at a time, and one more if asked while it ran.
pub fn push(ctx: &egui::Context, config: &Path) {
    if offline() {
        return;
    }
    let start = shared(|s| {
        if s.pushing {
            s.again = true;
            false
        } else {
            s.pushing = true;
            true
        }
    });
    if !start {
        return;
    }
    let (ctx, config) = (ctx.clone(), config.to_owned());
    std::thread::spawn(move || {
        loop {
            push_once(&config);
            ctx.request_repaint();
            let again = shared(|s| {
                let again = std::mem::take(&mut s.again);
                if !again {
                    s.pushing = false;
                }
                again
            });
            if !again {
                break;
            }
        }
    });
}

fn push_once(config: &Path) {
    // What to send is read under the lock, sent without it, and written
    // back under it, so the page never waits for the network.
    let (work, closing): (Vec<Work>, Vec<(String, String, String)>) = {
        let store = Store::open(config);
        let mut work = Vec::new();
        let mut closing = Vec::new();
        for lead in &store.leads {
            let (Some(brand), Some(customer)) =
                (store.brands.get(&lead.project), lead.eleva.as_ref())
            else {
                continue;
            };
            if brand.write {
                for remote in &lead.close {
                    closing.push((lead.id.clone(), brand.slug.clone(), remote.clone()));
                }
            }
            let contact = customer
                .phone
                .clone()
                .or_else(|| lead.phone.clone())
                .unwrap_or_else(|| customer.name.clone());
            for entry in lead.entries.iter().filter(|e| e.sync == Sync::Pending) {
                work.push(Work {
                    lead: lead.id.clone(),
                    slug: brand.slug.clone(),
                    write: brand.write,
                    customer: customer.clone(),
                    contact: contact.clone(),
                    entry: entry.id.clone(),
                    kind: entry.kind,
                    text: entry.text.clone(),
                });
            }
        }
        (work, closing)
    };
    if work.is_empty() && closing.is_empty() {
        return;
    }
    let mut results = Vec::new();
    for item in work {
        let outcome = send(&item);
        results.push((item.lead, item.entry, outcome, item.text));
    }
    let mut closed = Vec::new();
    for (lead, slug, remote) in closing {
        match resolve(&slug, &remote, true) {
            Ok(_) => closed.push((lead, remote)),
            // Gone already, or attended there: nothing left to close.
            Err(error) if error.to_string().starts_with("No encontré") => {
                closed.push((lead, remote))
            }
            Err(error) => log::info!(
                "leads: a situation could not be closed yet: {}",
                transient(&error)
            ),
        }
    }
    let mut store = Store::open(config);
    for (lead, entry, outcome, sent) in results {
        let Some(lead) = store.get_mut(&lead) else {
            continue;
        };
        let next_open = lead.next.is_some();
        let Some(index) = lead.entries.iter().position(|e| e.id == entry) else {
            continue;
        };
        // A call read again meanwhile has new text, still to send.
        if lead.entries[index].text != sent && lead.entries[index].sync == Sync::Pending {
            continue;
        }
        match outcome {
            // What reached the brand is marked so even if it was unlinked
            // or overtaken meanwhile, so it is never sent twice.
            Outcome::Sent(remote) => {
                let still_wanted = lead.entries[index].sync == Sync::Pending;
                lead.entries[index].sync = Sync::Sent;
                if let Some(remote) = remote {
                    lead.entries[index].remote = Some(remote.clone());
                    let latest =
                        lead.entries.iter().rposition(|e| e.kind == Kind::Task) == Some(index);
                    if still_wanted && latest && next_open && lead.open_task.is_none() {
                        lead.open_task = Some(remote);
                    } else {
                        lead.close.push(remote);
                    }
                }
            }
            Outcome::Failed(why) => {
                if lead.entries[index].sync == Sync::Pending {
                    lead.entries[index].sync = Sync::Failed(why);
                }
            }
            Outcome::Later => {}
        }
    }
    for (lead, remote) in closed {
        if let Some(lead) = store.get_mut(&lead) {
            lead.close.retain(|id| *id != remote);
        }
    }
    store.save();
}

fn send(item: &Work) -> Outcome {
    if !item.write {
        return Outcome::Failed("ElevaLeads token is read-only".into());
    }
    // The brand's own record wins over an older stage from here: someone
    // who bought there is not moved back.
    if item.kind == Kind::Stage
        && item.text != "customer"
        && item
            .customer
            .stage
            .as_deref()
            .and_then(Stage::from_eleva_label)
            == Some(Stage::Customer)
    {
        return Outcome::Failed("ElevaLeads already has them as a customer".into());
    }
    let Some((tool, arguments)) =
        entry_call(&item.customer.id, &item.contact, item.kind, &item.text)
    else {
        return Outcome::Sent(None);
    };
    match brand_call(&item.slug, tool, arguments) {
        Ok(answer) => {
            // Only a note's answer names whose file it went to.
            if tool == "nota_cliente"
                && !item.customer.name.is_empty()
                && let Some(other) =
                    landed_on(&answer).filter(|name| !same_name(name, &item.customer.name))
            {
                return Outcome::Failed(format!(
                    "ElevaLeads wrote it on another customer: {other}"
                ));
            }
            Outcome::Sent(situation_id(&answer))
        }
        Err(error) if transient(&error) => Outcome::Later,
        Err(error) => Outcome::Failed(error.to_string()),
    }
}

/// Whose file an answer says it wrote on: `Anotado en la ficha de X.`,
/// `X pasó a …`, `X ya estaba en …`.
fn landed_on(answer: &str) -> Option<String> {
    let answer = answer.trim();
    if let Some(rest) = answer.strip_prefix("Anotado en la ficha de ") {
        return Some(rest.strip_suffix('.').unwrap_or(rest).trim().to_owned());
    }
    for marker in [" pasó a ", " ya estaba en "] {
        if let Some((name, _)) = answer.split_once(marker) {
            return Some(name.trim().to_owned());
        }
    }
    None
}

fn same_name(a: &str, b: &str) -> bool {
    let clean = |s: &str| s.trim().trim_end_matches('.').to_lowercase();
    clean(a) == clean(b)
}

/// The id in `… quedó en la bandeja de Situaciones (id cmx…).`.
fn situation_id(answer: &str) -> Option<String> {
    let (_, rest) = answer.rsplit_once("(id ")?;
    let id: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    (!id.is_empty()).then_some(id)
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
    let mut phone = None;
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("# ") {
            name.get_or_insert_with(|| rest.split(" · ").next().unwrap_or(rest).to_owned());
        } else if let Some(rest) = line.strip_prefix("id:") {
            id.get_or_insert_with(|| rest.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("- Contacto:") {
            phone = rest
                .split(" · ")
                .next()
                .map(str::trim)
                .filter(|p| leads::digits(p).len() >= 7)
                .map(str::to_owned);
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
        phone,
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
        assert_eq!(customer.phone.as_deref(), Some("56950000000"));
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
    fn answers_say_whose_file_and_which_situation() {
        assert_eq!(
            landed_on("Anotado en la ficha de Ana Paz.").as_deref(),
            Some("Ana Paz")
        );
        assert_eq!(
            landed_on("Ana Paz pasó a Interesado.").as_deref(),
            Some("Ana Paz")
        );
        assert_eq!(
            landed_on("Ana Paz ya estaba en Cliente.").as_deref(),
            Some("Ana Paz")
        );
        assert_eq!(landed_on("Listo"), None);
        assert!(same_name(" ana paz", "Ana Paz"));
        assert_eq!(
            situation_id("Listo: “Llamar” quedó en la bandeja de Situaciones (id cmabc123).")
                .as_deref(),
            Some("cmabc123")
        );
        assert_eq!(situation_id("Listo"), None);
        assert!(transient(&anyhow::anyhow!(
            "ElevaLeads answered 502 Bad Gateway"
        )));
        assert!(transient(&anyhow::anyhow!(
            "Demasiadas solicitudes, probá en un minuto."
        )));
        assert!(!transient(&anyhow::anyhow!(
            "No encontré a \"x\" en esta marca."
        )));
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
        let long = "a".repeat(1500);
        let (_, args) = entry_call("c1", "569", Kind::Call, &long).unwrap();
        assert_eq!(args["nota"].as_str().unwrap().chars().count(), 1000);
    }
}
