//! A customer's orders in ElevaLeads, and sales closed on WhatsApp put
//! there.
//!
//! `ver_cliente` says, by phone, how many orders a customer has, what they
//! spent and their latest orders: enough to tell a sale the shop already
//! knows from one closed only in a chat. The MCP server creates no orders;
//! the brand's inbound API does (`POST /api/v1/orders`), with a key
//! (`eleva_…`) of its own, kept in the keyring beside the token. Whats Pro
//! can make that key itself (`crear_conexion`, a token that writes).
//!
//! An order created there runs the brand's automations: a status with a
//! follow-up message sends it to the customer on WhatsApp. `automations`
//! reads which statuses do, so the page can say so before anything is sent;
//! closed and paid, the default here, sends nothing unless the brand wrote
//! a message for it.

use std::{collections::HashMap, sync::Mutex, time::Duration};

use serde::{Deserialize, Serialize};

use super::{call, token, without_brand};
use crate::leads::{self, ElevaCustomer};

const ORDERS_ENDPOINT: &str = "https://elevaleads.com/api/v1/orders";

/// The statuses an order may be registered with, the default first.
pub const STATUSES: [&str; 5] = ["cerrado", "confirmado", "pago", "envio", "recepcion"];
/// The payment states, the default first.
pub const PAYMENTS: [&str; 3] = ["pagado", "pendiente", "parcial"];

/// One of a customer's latest orders, as ElevaLeads lists it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Order {
    pub code: String,
    pub status: String,
    pub payment: String,
    pub total: String,
    pub date: String,
}

/// What a customer bought in the brand.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Orders {
    /// Orders that count as bought (drafts and cancelled ones do not).
    pub count: u32,
    /// What they spent, as ElevaLeads writes it ("$180.000").
    pub spent: String,
    /// The latest orders, of any status.
    pub recent: Vec<Order>,
}

/// The brand's automatic messages on order statuses.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Automations {
    /// Whether the follow-up of orders is on.
    pub active: bool,
    /// Status to the message it sends.
    pub messages: Vec<(String, String)>,
}

impl Automations {
    /// The message an order with `status` sends to the customer, if any.
    pub fn message(&self, status: &str) -> Option<&str> {
        self.active
            .then(|| {
                self.messages
                    .iter()
                    .find(|(key, _)| key == status)
                    .map(|(_, text)| text.as_str())
            })
            .flatten()
    }
}

/// A sale to put in the brand.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NewSale {
    /// Ours, so sending it twice makes one order.
    pub external_id: String,
    pub name: String,
    pub phone: String,
    /// What was bought: description, quantity, unit price.
    pub items: Vec<(String, f64, f64)>,
    pub total: f64,
    pub status: String,
    pub payment: String,
    pub notes: String,
}

/// What the brand answered to a sale.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registered {
    pub customer: String,
    pub code: String,
    /// Whether this call made the order (false: it was there already).
    pub created: bool,
    /// What happened to the automatic message, if one ran.
    pub notice: Option<String>,
}

// ------------------------------------------------------------------ calls

/// The keyring account of a brand's inbound API key.
pub fn key_account(slug: &str) -> String {
    format!("ElevaLeads API:{slug}")
}

pub fn has_key(slug: &str) -> bool {
    crate::keys::get(&key_account(slug)).is_some()
}

/// Saves the brand's inbound key; an empty one removes it.
pub fn set_key(slug: &str, key: &str) -> anyhow::Result<()> {
    let key = key.trim();
    if !key.is_empty() && !key.starts_with("eleva_") {
        anyhow::bail!("An ElevaLeads API key starts with eleva_");
    }
    crate::keys::set(&key_account(slug), key)
}

/// The brand's customer with this phone and their orders, or `None` when
/// the brand has no such customer.
pub fn lookup(slug: &str, phone: &str) -> anyhow::Result<Option<(ElevaCustomer, Orders)>> {
    let token = token(slug).ok_or_else(|| anyhow::anyhow!("The brand's token is missing"))?;
    match call(
        &token,
        "ver_cliente",
        serde_json::json!({ "cliente": phone }),
    ) {
        Ok(text) => {
            Ok(super::parse_customer(&text).map(|customer| (customer, parse_orders(&text))))
        }
        Err(error) if error.to_string().starts_with("No encontré") => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn automations(slug: &str) -> anyhow::Result<Automations> {
    let token = token(slug).ok_or_else(|| anyhow::anyhow!("The brand's token is missing"))?;
    Ok(parse_automations(&call(
        &token,
        "ver_automatizaciones",
        serde_json::json!({}),
    )?))
}

/// Makes an inbound connection named "Whats Pro" in the brand and keeps its
/// key. Needs a token that writes and an admin or manager role there.
pub fn create_key(slug: &str) -> anyhow::Result<()> {
    let token = token(slug).ok_or_else(|| anyhow::anyhow!("The brand's token is missing"))?;
    let text = call(
        &token,
        "crear_conexion",
        serde_json::json!({ "nombre": "Whats Pro", "direccion": "entrante", "proposito": "orders" }),
    )?;
    let key = parse_key(&text).ok_or_else(|| anyhow::anyhow!("{}", text.trim()))?;
    set_key(slug, &key)
}

/// Puts a sale in the brand: the customer (made if missing) and its order.
pub fn register(slug: &str, sale: &NewSale) -> anyhow::Result<Registered> {
    let key = crate::keys::get(&key_account(slug))
        .ok_or_else(|| anyhow::anyhow!("The brand's API key is missing"))?;
    let response = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(45))
        .build()?
        .post(ORDERS_ENDPOINT)
        .bearer_auth(key)
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&order_body(sale))?)
        .send()?;
    let status = response.status();
    let value: serde_json::Value = serde_json::from_slice(&response.bytes()?)
        .map_err(|_| anyhow::anyhow!("ElevaLeads answered {status}"))?;
    parse_registered(&value)
}

fn order_body(sale: &NewSale) -> serde_json::Value {
    let items: Vec<serde_json::Value> = sale
        .items
        .iter()
        .filter(|(description, _, _)| !description.trim().is_empty())
        .map(|(description, qty, price)| {
            serde_json::json!({ "description": description.trim(), "qty": qty, "unitPrice": price })
        })
        .collect();
    let mut order = serde_json::json!({
        "externalId": sale.external_id,
        "total": sale.total,
        "status": sale.status,
        "paymentStatus": sale.payment,
        "items": items,
    });
    let notes: String = sale.notes.trim().chars().take(1000).collect();
    order["notes"] = serde_json::Value::String(if notes.is_empty() {
        "Venta cerrada por WhatsApp, registrada desde Whats Pro".into()
    } else {
        notes
    });
    serde_json::json!({
        "customer": { "name": sale.name.trim(), "phone": format!("+{}", leads::digits(&sale.phone)) },
        "order": order,
    })
}

fn parse_registered(value: &serde_json::Value) -> anyhow::Result<Registered> {
    if value["ok"].as_bool() != Some(true) {
        let error = value["error"]
            .as_str()
            .unwrap_or("ElevaLeads refused the sale");
        let details: Vec<String> = value["detalles"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| {
                Some(format!(
                    "{}: {}",
                    d["campo"].as_str()?,
                    d["error"].as_str()?
                ))
            })
            .collect();
        if details.is_empty() {
            anyhow::bail!("{error}");
        }
        anyhow::bail!("{error} ({})", details.join(", "));
    }
    Ok(Registered {
        customer: value["customerId"].as_str().unwrap_or_default().to_owned(),
        code: value["order"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        created: value["order"]["created"].as_bool().unwrap_or(true),
        notice: value["order"]["aviso"].as_str().map(str::to_owned),
    })
}

// ----------------------------------------------------------------- readers

/// `- Compró: 2 pedido(s) · $180.000` and the lines under
/// `**Últimos pedidos:**` (`- P-0003 · cerrado · pago pagado · $90.000 · 12/10/2026`).
fn parse_orders(text: &str) -> Orders {
    let mut orders = Orders::default();
    let mut in_recent = false;
    for line in without_brand(text).lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("- Compró:") {
            let mut parts = rest.split(" · ");
            orders.count = parts
                .next()
                .and_then(|n| n.split_whitespace().next())
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            orders.spent = parts.next().unwrap_or_default().trim().to_owned();
        } else if line.starts_with("**Últimos pedidos") {
            in_recent = true;
        } else if line.starts_with("**") || line.is_empty() {
            in_recent = in_recent && line.is_empty() && orders.recent.is_empty();
        } else if in_recent && let Some(rest) = line.strip_prefix("- ") {
            let parts: Vec<&str> = rest.split(" · ").collect();
            if parts.len() >= 4 {
                orders.recent.push(Order {
                    code: parts[0].trim().to_owned(),
                    status: parts[1].trim().to_owned(),
                    payment: parts[2].trim().trim_start_matches("pago ").to_owned(),
                    total: parts[3].trim().to_owned(),
                    date: parts
                        .get(4)
                        .map(|d| d.trim().to_owned())
                        .unwrap_or_default(),
                });
            }
        }
    }
    orders
}

/// `## Seguimiento del pedido — ACTIVA` and `  · confirmado: "…"` lines.
fn parse_automations(text: &str) -> Automations {
    let mut automations = Automations::default();
    let mut in_follow_up = false;
    for line in without_brand(text).lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("## ") {
            in_follow_up = rest.starts_with("Seguimiento del pedido");
            if in_follow_up {
                automations.active = rest.contains("ACTIVA");
            }
        } else if in_follow_up
            && let Some(rest) = trimmed.strip_prefix("· ")
            && let Some((status, message)) = rest.split_once(": ")
        {
            automations.messages.push((
                status.trim().to_owned(),
                message.trim().trim_matches('"').to_owned(),
            ));
        }
    }
    automations
}

/// The `eleva_…` key in `crear_conexion`'s answer.
fn parse_key(text: &str) -> Option<String> {
    text.split(|c: char| c.is_whitespace() || c == '`')
        .find(|word| word.starts_with("eleva_") && word.len() > 20)
        .map(str::to_owned)
}

// --------------------------------------------------------------- threads

/// An answer from a thread.
#[derive(Clone, Debug)]
pub enum Answer {
    Lookup(Option<(ElevaCustomer, Orders)>),
    Automations(Automations),
    Key,
    Registered(Registered),
}

type Jobs = HashMap<String, Option<Result<Answer, String>>>;

static JOBS: Mutex<Option<Jobs>> = Mutex::new(None);

fn jobs<T>(f: impl FnOnce(&mut Jobs) -> T) -> T {
    let mut guard = JOBS.lock().unwrap_or_else(|p| p.into_inner());
    f(guard.get_or_insert_with(HashMap::new))
}

/// Runs `work` on a thread under `key`, unless it is already running.
pub fn start(
    ctx: &egui::Context,
    key: &str,
    work: impl FnOnce() -> anyhow::Result<Answer> + Send + 'static,
) {
    let fresh = jobs(|jobs| {
        if matches!(jobs.get(key), Some(None)) {
            false
        } else {
            jobs.insert(key.to_owned(), None);
            true
        }
    });
    if !fresh {
        return;
    }
    let (ctx, key) = (ctx.clone(), key.to_owned());
    std::thread::spawn(move || {
        let result = work().map_err(|e| e.to_string());
        jobs(|jobs| jobs.insert(key, Some(result)));
        ctx.request_repaint();
    });
}

/// Puts an answer in place without running anything, for the demo.
pub fn preload(key: &str, answer: Answer) {
    jobs(|jobs| jobs.insert(key.to_owned(), Some(Ok(answer))));
}

/// Whether the job under `key` is running.
pub fn running(key: &str) -> bool {
    jobs(|jobs| matches!(jobs.get(key), Some(None)))
}

/// The answer of the job under `key`, once.
pub fn take(key: &str) -> Option<Result<Answer, String>> {
    jobs(|jobs| match jobs.get(key) {
        Some(Some(_)) => jobs.remove(key).flatten(),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CUSTOMER: &str = "[Marca: Cinturity]\n# Ana Paz\nid:c1\n- Contacto: +5491100000000\n- Etapa: Cliente · Tipo: minorista\n- Compró: 2 pedido(s) · $180.000\n\n**Últimos pedidos:**\n- P-0003 · cerrado · pago pagado · $90.000 · 12/10/2026\n- #1043 · envio · pago pagado · $90.000 · 1/10/2026\n\n**Charla:** último mensaje 12/10 · id:x\n\n**Notas del equipo:** ninguna.";

    #[test]
    fn reads_orders() {
        let orders = parse_orders(CUSTOMER);
        assert_eq!(orders.count, 2);
        assert_eq!(orders.spent, "$180.000");
        assert_eq!(orders.recent.len(), 2);
        assert_eq!(orders.recent[0].code, "P-0003");
        assert_eq!(orders.recent[0].payment, "pagado");
        assert_eq!(orders.recent[1].status, "envio");
        assert_eq!(orders.recent[1].date, "1/10/2026");
        let none = parse_orders(
            "# Augusto\nid:c2\n- Compró: 0 pedido(s) · $0\n\n**Charla:** nunca conversó.",
        );
        assert_eq!(none.count, 0);
        assert!(none.recent.is_empty());
    }

    #[test]
    fn reads_automations() {
        let text = "[Marca: Blackyak Labs]\n# Automatizaciones de Blackyak Labs\n\n## Seguimiento del pedido — ACTIVA\nAvisa al cliente por WhatsApp en cada cambio de estado:\n  · confirmado: \"¡Listo! Confirmamos tu cotización {code} 🙌\"\n  · pago: \"Registramos el pago\"\n\n## Post-venta — ACTIVA\nSe envía 7 días después de entregado: \"¡Hola!\"";
        let automations = parse_automations(text);
        assert!(automations.active);
        assert_eq!(automations.messages.len(), 2);
        assert_eq!(
            automations.message("confirmado"),
            Some("¡Listo! Confirmamos tu cotización {code} 🙌")
        );
        assert_eq!(automations.message("cerrado"), None);
        let off = parse_automations("## Seguimiento del pedido — apagada\n  · confirmado: \"x\"");
        assert_eq!(off.message("confirmado"), None);
    }

    #[test]
    fn reads_the_new_key_and_the_answer() {
        let text = "Conexión ENTRANTE creada en Cinturity: **Whats Pro**\n\nClave de API (guardala):\n```\neleva_0123456789abcdef0123456789abcdef0123456789abcdef\n```\n";
        assert_eq!(
            parse_key(text).as_deref(),
            Some("eleva_0123456789abcdef0123456789abcdef0123456789abcdef")
        );
        assert_eq!(parse_key("Necesitás rol admin"), None);

        let ok = serde_json::json!({ "ok": true, "customerId": "c9", "order": { "id": "o1", "code": "P-0004", "created": true, "aviso": "fuera_ventana" } });
        let registered = parse_registered(&ok).unwrap();
        assert_eq!(registered.code, "P-0004");
        assert_eq!(registered.notice.as_deref(), Some("fuera_ventana"));
        let bad = serde_json::json!({ "error": "Datos inválidos", "detalles": [{ "campo": "order.total", "error": "Number must be >= 0" }] });
        assert_eq!(
            parse_registered(&bad).unwrap_err().to_string(),
            "Datos inválidos (order.total: Number must be >= 0)"
        );
    }

    #[test]
    fn the_order_carries_the_sale() {
        let body = order_body(&NewSale {
            external_id: "whatspro-lead-1-1".into(),
            name: "Ana Paz".into(),
            phone: "54 9 11 0000-0000".into(),
            items: vec![
                ("Cinturón negro".into(), 2.0, 45_000.0),
                ("  ".into(), 1.0, 0.0),
            ],
            total: 90_000.0,
            status: "cerrado".into(),
            payment: "pagado".into(),
            notes: String::new(),
        });
        assert_eq!(body["customer"]["phone"], "+5491100000000");
        assert_eq!(body["order"]["externalId"], "whatspro-lead-1-1");
        assert_eq!(body["order"]["items"].as_array().unwrap().len(), 1);
        assert_eq!(body["order"]["status"], "cerrado");
        assert_eq!(body["order"]["paymentStatus"], "pagado");
        assert!(
            body["order"]["notes"]
                .as_str()
                .unwrap()
                .contains("Whats Pro")
        );
    }
}
