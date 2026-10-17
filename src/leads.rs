//! Leads: the people a project is selling to, and how each sale is going.
//!
//! Every lead belongs to a project and usually to a chat. It has a stage
//! (new, contacted, quoted, customer, lost), a next action with a day, and a
//! history of notes, calls, follow-ups and stage changes. A lead is told
//! from a customer by its stage alone: a customer has bought.
//!
//! A project may be connected to a brand of ElevaLeads (`crate::eleva`):
//! then a lead can be linked to the brand's customer with the same phone,
//! and what is written here reaches that customer's file there. Each entry
//! waits in `Sync::Pending` until the brand takes it; one the brand cannot
//! take (a read-only token, a customer that is not there) says so and stays
//! here, so nothing written is lost.
//!
//! Everything lives in `leads.json` in the config folder, beside
//! `projects.json`. The ElevaLeads tokens never do: they are in the OS
//! keyring (`crate::keys`), under the brand.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use serde::{Deserialize, Serialize};

/// How far a sale has gone.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    /// Nobody has worked it yet.
    #[default]
    New,
    /// There is a conversation going.
    Contacted,
    /// They have a price or a quote.
    Quoted,
    /// They bought.
    Customer,
    /// It did not happen.
    Lost,
}

impl Stage {
    pub const ALL: [Stage; 5] = [
        Stage::New,
        Stage::Contacted,
        Stage::Quoted,
        Stage::Customer,
        Stage::Lost,
    ];

    /// The English label, the key of its translation.
    pub fn label(self) -> &'static str {
        match self {
            Stage::New => "New",
            Stage::Contacted => "Contacted",
            Stage::Quoted => "Quoted",
            Stage::Customer => "Customer",
            Stage::Lost => "Lost",
        }
    }

    /// The stage ElevaLeads gives the same customer. It has no quoted or
    /// lost: interested and dormant are the nearest.
    pub fn eleva(self) -> &'static str {
        match self {
            Stage::New => "nuevo",
            Stage::Contacted => "en_charla",
            Stage::Quoted => "interesado",
            Stage::Customer => "cliente",
            Stage::Lost => "dormido",
        }
    }

    /// Ours, from an ElevaLeads stage label as `ver_cliente` writes it.
    pub fn from_eleva_label(label: &str) -> Option<Stage> {
        match label.trim().to_lowercase().as_str() {
            "nuevo" => Some(Stage::New),
            "en conversación" | "en_charla" => Some(Stage::Contacted),
            "interesado" => Some(Stage::Quoted),
            "cliente" => Some(Stage::Customer),
            "dormido" => Some(Stage::Lost),
            _ => None,
        }
    }

    /// Whether the sale is still being worked.
    pub fn open(self) -> bool {
        matches!(self, Stage::New | Stage::Contacted | Stage::Quoted)
    }
}

/// Why a sale was lost.
pub const LOST_REASONS: [&str; 5] = [
    "Price",
    "Chose someone else",
    "No answer",
    "Later on",
    "Other",
];

/// What an entry in a lead's history is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Note,
    Call,
    FollowUp,
    /// A summary of the chat, written by the analysis.
    Summary,
    /// The stage changed; the text is the new stage's key.
    Stage,
    /// A next action was set; the text is the action and its day.
    Task,
}

/// Where an entry stands with ElevaLeads.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "state", content = "why")]
pub enum Sync {
    /// The lead is not linked: the entry lives here only.
    #[default]
    Local,
    /// Waiting to be sent.
    Pending,
    Sent,
    /// The brand did not take it, and why.
    Failed(String),
}

/// What a call's placeholder says until it is transcribed. In Spanish
/// whatever the interface's language, like the analysis it stands for.
pub const NOT_TRANSCRIBED: &str = "Todavía sin transcribir";

/// What ElevaLeads takes in a note, less the `[Whats Pro] Llamada: ` that
/// `eleva` puts before it, with a little room to spare.
pub const CALL_NOTE_LIMIT: usize = 970;

/// How long a call was, as its note starts: `(4:12)`.
pub fn call_heading(seconds: u64) -> String {
    format!("({}:{:02})", seconds / 60, seconds % 60)
}

/// The heading a call's note starts with: up to the parenthesis that
/// closes the first one, so a file name with parentheses stays whole.
pub fn heading_of(text: &str) -> &str {
    if !text.starts_with('(') {
        return "";
    }
    let mut depth = 0usize;
    for (index, c) in text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return &text[..=index];
                }
            }
            _ => {}
        }
    }
    ""
}

/// A call's note, what ElevaLeads gets: its summary after `heading`, then
/// who agreed to what. The summary gives way first when it is too long,
/// since what was agreed is what the team acts on.
pub fn call_note(heading: &str, analysis: &crate::call_library::CallAnalysis) -> String {
    let lines: Vec<String> = analysis
        .commitments
        .iter()
        .filter(|c| !c.what.trim().is_empty())
        .map(|c| {
            let mut line = match c.who.trim() {
                "" => c.what.trim().to_owned(),
                who => format!("{who}: {}", c.what.trim()),
            };
            if let Some(due) = c.due.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
                line.push_str(&format!(" ({due})"));
            }
            format!("• {line}")
        })
        .collect();
    let heading = if heading.is_empty() {
        String::new()
    } else {
        format!("{heading} ")
    };
    // At least this much summary; past it, the last commitments go too.
    const SUMMARY_FLOOR: usize = 160;
    let mut lines = lines;
    let mut agreed;
    loop {
        agreed = if lines.is_empty() {
            String::new()
        } else {
            format!("\n\nAcordado:\n{}", lines.join("\n"))
        };
        let used = heading.chars().count() + agreed.chars().count() + SUMMARY_FLOOR;
        if used <= CALL_NOTE_LIMIT || lines.len() <= 1 {
            break;
        }
        lines.pop();
        if lines.last().is_some_and(|line| line != "• …") {
            lines.pop();
            lines.push("• …".to_owned());
        }
    }
    let room = CALL_NOTE_LIMIT
        .saturating_sub(heading.chars().count() + agreed.chars().count())
        .max(SUMMARY_FLOOR);
    let summary = analysis.summary.trim();
    let summary = if summary.chars().count() > room {
        let cut: String = summary.chars().take(room - 1).collect();
        format!("{}…", cut.trim_end())
    } else {
        summary.to_owned()
    };
    format!("{heading}{summary}{agreed}")
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub at: i64,
    pub kind: Kind,
    pub text: String,
    #[serde(default)]
    pub sync: Sync,
    /// What ElevaLeads made of it: the situation a next action opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    /// A recorded call's full transcript. It stays here: ElevaLeads gets
    /// the text, the call's summary. Shared, so copying the leads for a
    /// frame does not copy it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<std::sync::Arc<str>>,
    /// The call's id in `crate::call_library`, where its audio is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<String>,
}

impl Entry {
    /// A call kept in the library but not transcribed yet: here only.
    pub fn waiting(&self) -> bool {
        self.kind == Kind::Call && self.call.is_some() && self.transcript.is_none()
    }
}

/// What to do next with a lead, and on which day (`YYYY-MM-DD`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NextAction {
    pub text: String,
    #[serde(default)]
    pub due: Option<String>,
}

/// The ElevaLeads customer a lead is linked to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ElevaCustomer {
    pub id: String,
    pub name: String,
    /// The stage it had there when it was last read.
    #[serde(default)]
    pub stage: Option<String>,
    /// The phone as the brand has it, which its searches match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Lead {
    pub id: String,
    pub project: String,
    /// The chat with the person, when there is one.
    #[serde(default)]
    pub chat: Option<String>,
    pub name: String,
    /// International digits, no plus.
    #[serde(default)]
    pub phone: Option<String>,
    #[serde(default)]
    pub stage: Stage,
    #[serde(default)]
    pub next: Option<NextAction>,
    /// What the sale is worth, as written.
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub lost_reason: Option<String>,
    #[serde(default)]
    pub entries: Vec<Entry>,
    #[serde(default)]
    pub eleva: Option<ElevaCustomer>,
    /// What the brand's customer with this phone bought, as last read.
    #[serde(default)]
    pub orders: Option<OrdersSeen>,
    /// Sales closed on WhatsApp and put in ElevaLeads from here.
    #[serde(default)]
    pub sales: Vec<Sale>,
    /// The ElevaLeads situation the current next action opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_task: Option<String>,
    /// Situations of next actions that were done or replaced, to mark as
    /// attended in ElevaLeads.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub close: Vec<String>,
    pub created: i64,
    pub updated: i64,
}

/// A customer's orders in ElevaLeads, and when they were read.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OrdersSeen {
    /// Whether the brand has a customer with the lead's phone at all.
    pub known: bool,
    #[serde(default)]
    pub orders: crate::eleva::sales::Orders,
    pub read_at: i64,
}

/// A sale put in ElevaLeads from here.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sale {
    /// The order's id on our side, so a retry makes no second order.
    pub id: String,
    pub at: i64,
    pub total: f64,
    pub summary: String,
    /// The order's code in ElevaLeads once it took it.
    #[serde(default)]
    pub code: Option<String>,
    /// What happened to the brand's automatic message, if one ran.
    #[serde(default)]
    pub notice: Option<String>,
    /// `pagado`, `pendiente` or `parcial`: as registered, then as the
    /// brand last said.
    #[serde(default = "paid")]
    pub payment: String,
    /// Marked collected here by hand: it wins over what the brand says.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub settled: bool,
}

fn paid() -> String {
    "pagado".into()
}

/// How a project sells: how often its customers buy again, how follow-ups
/// are spaced, and how many first messages a day are safe to send.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// Days after a purchase to offer again; none for one-off sales.
    #[serde(default)]
    pub rebuy_days: Option<u32>,
    /// Days between follow-ups of someone who does not answer.
    #[serde(default = "cadence")]
    pub cadence: Vec<u32>,
    /// First messages a day before WhatsApp may take it for spam.
    #[serde(default = "daily_cap")]
    pub daily_cap: u32,
}

fn cadence() -> Vec<u32> {
    vec![2, 5, 10]
}

fn daily_cap() -> u32 {
    25
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            rebuy_days: None,
            cadence: cadence(),
            daily_cap: daily_cap(),
        }
    }
}

/// A month of a project's sales.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Month {
    /// Leads added.
    pub new: usize,
    /// Leads that became customers.
    pub won: usize,
    /// What the won sales were worth.
    pub won_value: f64,
    pub lost: usize,
    /// Why they were lost, the most common first.
    pub lost_reasons: Vec<(String, usize)>,
}

impl Month {
    /// Won out of those that closed one way or the other, in percent.
    pub fn conversion(&self) -> Option<u32> {
        let closed = self.won + self.lost;
        (closed > 0).then(|| (self.won * 100 / closed) as u32)
    }
}

/// Where a lead sits in the list: what needs doing first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Urgency {
    /// They wrote last: the next word is ours.
    Reply,
    Overdue,
    Today,
    /// Customers who still owe part of a sale.
    Collect,
    /// Open, with nothing planned: the ones going cold.
    NoAction,
    Upcoming,
    /// Customers whose last purchase is a buying cycle ago.
    Rebuy,
    Customers,
    Lost,
}

impl Urgency {
    pub const ALL: [Urgency; 9] = [
        Urgency::Reply,
        Urgency::Overdue,
        Urgency::Today,
        Urgency::Collect,
        Urgency::NoAction,
        Urgency::Upcoming,
        Urgency::Rebuy,
        Urgency::Customers,
        Urgency::Lost,
    ];
}

impl Lead {
    pub fn new(project: &str, name: &str, chat: Option<String>, phone: Option<String>) -> Self {
        let now = now();
        Self {
            id: new_id("lead"),
            project: project.to_owned(),
            chat,
            name: name.trim().to_owned(),
            phone,
            created: now,
            updated: now,
            ..Self::default()
        }
    }

    /// Where it goes in the list on `today` (`YYYY-MM-DD`), without
    /// knowing who wrote last.
    pub fn urgency(&self, today: &str) -> Urgency {
        self.urgency_with(today, false, None)
    }

    /// Where it goes in the list: `replied` when they wrote last, and the
    /// project's buying cycle for customers.
    pub fn urgency_with(&self, today: &str, replied: bool, rebuy_days: Option<u32>) -> Urgency {
        let planned = match self.next.as_ref().map(|next| next.due.as_deref()) {
            Some(Some(due)) if due < today => Some(Urgency::Overdue),
            Some(Some(due)) if due == today => Some(Urgency::Today),
            Some(Some(_)) => Some(Urgency::Upcoming),
            Some(None) => Some(Urgency::Today),
            None => None,
        };
        let due_now = matches!(planned, Some(Urgency::Overdue | Urgency::Today));
        if self.stage == Stage::Lost {
            // A lost sale comes back only when its reminder is due.
            return if due_now {
                planned.unwrap_or(Urgency::Lost)
            } else {
                Urgency::Lost
            };
        }
        if replied {
            return Urgency::Reply;
        }
        if self.stage == Stage::Customer {
            // What is due today goes before what is owed.
            if due_now {
                return planned.unwrap_or(Urgency::Today);
            }
            if self.owes() {
                return Urgency::Collect;
            }
            if let Some(planned) = planned {
                return planned;
            }
            let now = jiff::Timestamp::now().as_second();
            let due = rebuy_days
                .zip(self.last_purchase())
                .is_some_and(|(days, at)| now - at > i64::from(days) * 86_400);
            return if due {
                Urgency::Rebuy
            } else {
                Urgency::Customers
            };
        }
        planned.unwrap_or(Urgency::NoAction)
    }

    /// Whether part of a sale is still to be paid.
    ///
    /// A sale registered here follows its order in the brand once the order
    /// was read after it: when the shop marks it paid, it stops being owed.
    pub fn owes(&self) -> bool {
        self.sales.iter().any(|sale| self.sale_owed(sale))
            || self.orders.as_ref().is_some_and(|seen| {
                seen.orders.recent.iter().any(|order| {
                    unpaid(order)
                        && !self
                            .sales
                            .iter()
                            .any(|sale| sale.code.as_deref() == Some(order.code.as_str()))
                })
            })
    }

    /// Whether a sale registered here is still owed: collected by hand, no;
    /// otherwise what its order in the brand says once read after it, or
    /// what was last known.
    pub fn sale_owed(&self, sale: &Sale) -> bool {
        if sale.settled {
            return false;
        }
        let order = self.orders.as_ref().and_then(|seen| {
            let code = sale.code.as_deref()?;
            (seen.read_at >= sale.at)
                .then(|| seen.orders.recent.iter().find(|o| o.code == code))
                .flatten()
        });
        match order {
            Some(order) => unpaid(order),
            None => sale.payment != "pagado",
        }
    }

    /// Takes the payment of each sale's order as the brand last said it,
    /// so it is known even when the order leaves its latest few.
    pub fn follow_orders(&mut self) {
        let Some(seen) = self.orders.as_ref() else {
            return;
        };
        for sale in &mut self.sales {
            if let Some(order) = sale
                .code
                .as_deref()
                .and_then(|code| seen.orders.recent.iter().find(|o| o.code == code))
                .filter(|_| seen.read_at >= sale.at)
            {
                sale.payment = order.payment.clone();
            }
        }
    }

    /// Marks every sale registered here as paid, for money collected by
    /// hand.
    pub fn mark_paid(&mut self) {
        for sale in &mut self.sales {
            sale.settled = true;
            sale.payment = "pagado".into();
        }
        self.updated = now();
    }

    /// When they last bought: a sale noted here, an order in the brand, or
    /// the move to customer.
    pub fn last_purchase(&self) -> Option<i64> {
        let sold = self.sales.iter().map(|sale| sale.at);
        let won = self
            .entries
            .iter()
            .filter(|entry| entry.kind == Kind::Stage && entry.text == "customer")
            .map(|entry| entry.at);
        let ordered = self
            .orders
            .iter()
            .flat_map(|seen| seen.orders.recent.iter())
            .filter(|order| !matches!(order.status.as_str(), "borrador" | "cancelado"))
            .filter_map(|order| day_of(&order.date));
        // Moving to customer is not a purchase: only when nothing else says.
        sold.chain(ordered).max().or_else(|| won.max())
    }

    /// Days from today until the buying cycle comes round, never negative.
    pub fn rebuy_in(&self, rebuy_days: u32) -> i64 {
        let Some(at) = self.last_purchase() else {
            return i64::from(rebuy_days);
        };
        let due = at + i64::from(rebuy_days) * 86_400;
        ((due - now()) / 86_400).max(0)
    }

    /// Follow-ups ticked off since the stage last changed: how many times
    /// someone was chased without moving on.
    pub fn chases(&self) -> usize {
        self.entries
            .iter()
            .rev()
            .take_while(|entry| entry.kind != Kind::Stage)
            .filter(|entry| entry.kind == Kind::FollowUp && entry.text.starts_with('✓'))
            .count()
    }

    /// When it moved to `stage`, last.
    pub fn moved_to(&self, stage: Stage) -> Option<i64> {
        let key = serde_json::to_value(stage).ok()?;
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.kind == Kind::Stage && Some(entry.text.as_str()) == key.as_str())
            .map(|entry| entry.at)
    }

    /// Adds an entry, waiting for ElevaLeads when the lead is linked.
    pub fn add(&mut self, kind: Kind, text: &str) -> &Entry {
        let now = now();
        self.entries.push(Entry {
            id: new_id("entry"),
            at: now,
            kind,
            text: text.trim().to_owned(),
            sync: if self.eleva.is_some() {
                Sync::Pending
            } else {
                Sync::Local
            },
            transcript: None,
            call: None,
            remote: None,
        });
        self.updated = now;
        self.entries.last().expect("just pushed")
    }

    /// Notes a recorded call: its summary, and the transcript kept here
    /// beside the call's id in the library. A call with no transcript yet
    /// is a placeholder (`NOT_TRANSCRIBED`) that waits here until
    /// `redo_call` fills it, so ElevaLeads never gets it. Talking to a new
    /// lead is contacting it, so it moves on, unless ElevaLeads already has
    /// it further along (or at a stage this app does not know): a call must
    /// not move a customer back.
    pub fn add_call(&mut self, summary: &str, transcript: &str, call: &str) -> &Entry {
        let behind = match &self.eleva {
            None => true,
            Some(customer) => customer
                .stage
                .as_deref()
                .and_then(Stage::from_eleva_label)
                .is_some_and(|there| there <= Stage::Contacted),
        };
        if self.stage == Stage::New && behind {
            self.set_stage(Stage::Contacted, None);
        }
        self.add(Kind::Call, summary);
        let entry = self.entries.last_mut().expect("just added");
        let transcript = transcript.trim();
        entry.transcript = (!transcript.is_empty()).then(|| transcript.into());
        entry.call = (!call.is_empty()).then(|| call.to_owned());
        if entry.waiting() {
            entry.sync = Sync::Local;
        }
        entry
    }

    /// A call read again, or read at last: its new summary and transcript,
    /// sent to ElevaLeads when linked.
    pub fn redo_call(&mut self, entry: &str, summary: &str, transcript: &str) {
        let linked = self.eleva.is_some();
        let Some(entry) = self.entries.iter_mut().find(|e| e.id == entry) else {
            return;
        };
        entry.text = summary.trim().to_owned();
        let transcript = transcript.trim();
        entry.transcript = (!transcript.is_empty()).then(|| transcript.into());
        entry.sync = if linked && !entry.waiting() {
            Sync::Pending
        } else {
            Sync::Local
        };
        self.updated = now();
    }

    /// Moves the lead to `stage`, noting it in its history.
    pub fn set_stage(&mut self, stage: Stage, lost_reason: Option<&str>) {
        if self.stage == stage {
            return;
        }
        self.stage = stage;
        self.lost_reason = (stage == Stage::Lost)
            .then(|| lost_reason.map(str::to_owned))
            .flatten();
        let key = serde_json::to_value(stage)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default();
        self.add(Kind::Stage, &key);
        // A sale that closed has nothing left to do.
        if !stage.open() {
            self.next = None;
            self.retire_task();
        }
    }

    /// The situation of the current next action, to close in ElevaLeads;
    /// a next action not yet sent is not sent any more.
    fn retire_task(&mut self) {
        if let Some(remote) = self.open_task.take() {
            self.close.push(remote);
        }
        for entry in &mut self.entries {
            if entry.kind == Kind::Task && entry.sync == Sync::Pending {
                entry.sync = Sync::Local;
            }
        }
    }

    /// Ticks the next action off: it is noted as done and its situation
    /// closed. Returns what it was.
    pub fn finish_next(&mut self) -> Option<NextAction> {
        let next = self.next.take()?;
        self.retire_task();
        self.add(Kind::FollowUp, &format!("✓ {}", next.text));
        Some(next)
    }

    /// Puts the next action off by `days`, from its day or from today,
    /// whichever is later: putting off never brings it closer.
    pub fn postpone(&mut self, days: i64) {
        let Some(next) = self.next.clone() else {
            return;
        };
        let today = crate::util::today();
        let base = next
            .due
            .as_deref()
            .and_then(|due| due.parse::<jiff::civil::Date>().ok())
            .filter(|due| *due > today)
            .unwrap_or(today);
        let due = base
            .checked_add(jiff::Span::new().days(days))
            .unwrap_or(base)
            .to_string();
        self.set_next(&next.text, Some(due));
    }

    /// Plans what comes next; an empty text clears it.
    pub fn set_next(&mut self, text: &str, due: Option<String>) {
        let text = text.trim();
        if text.is_empty() {
            self.next = None;
            self.retire_task();
            self.updated = now();
            return;
        }
        let next = NextAction {
            text: text.to_owned(),
            due,
        };
        if self.next.as_ref() == Some(&next) {
            return;
        }
        let line = match &next.due {
            Some(due) => format!("{text} · {due}"),
            None => text.to_owned(),
        };
        self.next = Some(next);
        // Only the newest plan goes to the brand, and the one before is closed.
        self.retire_task();
        self.add(Kind::Task, &line);
    }

    /// Links the lead to an ElevaLeads customer, and sends what was
    /// written before: every note, but only the stage it is in now and the
    /// next action still open, not each step it took to get here.
    pub fn link(&mut self, customer: ElevaCustomer) {
        self.eleva = Some(customer);
        self.repend(|sync| matches!(sync, Sync::Local | Sync::Failed(_)));
    }

    /// Sends again what the brand did not take, by the same rule as a link:
    /// stages and plans that were overtaken stay here.
    pub fn retry(&mut self) {
        self.repend(|sync| matches!(sync, Sync::Failed(_)));
    }

    /// Marks for sending the entries `from` picks: every note, but only the
    /// stage the lead is in now and the next action still open.
    fn repend(&mut self, from: impl Fn(&Sync) -> bool) {
        let last = |kind: Kind| self.entries.iter().rposition(|entry| entry.kind == kind);
        let stage = last(Kind::Stage);
        let task = last(Kind::Task).filter(|_| self.next.is_some() && self.open_task.is_none());
        for (index, entry) in self.entries.iter_mut().enumerate() {
            let wanted = match entry.kind {
                Kind::Stage => Some(index) == stage,
                Kind::Task => Some(index) == task,
                _ => true,
            };
            if wanted && !entry.waiting() && from(&entry.sync) {
                entry.sync = Sync::Pending;
            } else if !wanted && matches!(entry.sync, Sync::Failed(_)) {
                entry.sync = Sync::Local;
            }
        }
        self.updated = now();
    }

    pub fn unlink(&mut self) {
        self.eleva = None;
        self.open_task = None;
        self.close.clear();
        for entry in &mut self.entries {
            if entry.sync == Sync::Pending {
                entry.sync = Sync::Local;
            }
        }
        self.updated = now();
    }

    /// When the person last wrote or was written to, from the history.
    pub fn last_touch(&self) -> i64 {
        self.entries
            .iter()
            .map(|entry| entry.at)
            .max()
            .unwrap_or(self.created)
    }

    /// Sold (a customer here), yet the brand has no order for them and none
    /// was put there from here: a sale closed on WhatsApp the shop does not
    /// know about.
    pub fn needs_sale(&self) -> bool {
        self.stage == Stage::Customer
            && self.sales.is_empty()
            && self
                .orders
                .as_ref()
                .is_some_and(|seen| seen.orders.count == 0)
    }

    /// The id the next sale of this lead goes with.
    ///
    /// New for each sale, and kept by the form across retries, so a retry
    /// makes no second order and a second sale is never taken for the first.
    pub fn next_sale_id(&self) -> String {
        format!("whatspro-{}-{}", self.id, self.sales.len() + 1)
    }

    /// Whether something waits to go to the brand: entries, or situations
    /// to close.
    pub fn to_send(&self) -> bool {
        self.pending() > 0 || !self.close.is_empty()
    }

    pub fn pending(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.sync == Sync::Pending)
            .count()
    }
}

/// The project's link to a brand of ElevaLeads. The token is in the
/// keyring under `token_account(slug)`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Brand {
    pub slug: String,
    pub name: String,
    /// Whether the token may write: notes, stages, tasks, resolving.
    pub write: bool,
}

/// The keyring account of a brand's token.
pub fn token_account(slug: &str) -> String {
    format!("ElevaLeads:{slug}")
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Leads {
    #[serde(default)]
    pub leads: Vec<Lead>,
    /// Project id to the brand it is connected to.
    #[serde(default)]
    pub brands: BTreeMap<String, Brand>,
    /// Project id to its saved first messages, `{name}` for the first name.
    #[serde(default)]
    pub templates: BTreeMap<String, Vec<String>>,
    /// Project id to how it sells.
    #[serde(default)]
    pub settings: BTreeMap<String, Settings>,
    /// Project id to the analysis' suggestions the user turned down.
    #[serde(default)]
    pub dismissed: BTreeMap<String, Vec<String>>,
    /// Brand slug to situations still open there of leads removed or
    /// unlinked here, to mark as attended.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub orphans: BTreeMap<String, Vec<String>>,
}

impl Leads {
    pub fn file(config: &Path) -> PathBuf {
        config.join("leads.json")
    }

    pub fn load(config: &Path) -> Self {
        Self::load_checked(config).0
    }

    /// The leads in the config folder, and whether saving over the file is
    /// safe. One that does not parse is set aside as
    /// `leads.json.broken-<time>`; one that cannot be read at all is left
    /// alone and not saved over this run.
    pub fn load_checked(config: &Path) -> (Self, bool) {
        let path = Self::file(config);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return (Self::default(), true);
            }
            Err(error) => {
                log::warn!("leads: leads.json could not be read ({error}); it is left as it is");
                return (Self::default(), false);
            }
        };
        match serde_json::from_str(&text) {
            Ok(leads) => (leads, true),
            Err(error) => {
                let aside = path.with_extension(format!("json.broken-{}", now()));
                log::warn!(
                    "leads: leads.json could not be parsed ({error}); kept aside as {}",
                    aside.display()
                );
                let moved = std::fs::rename(&path, &aside).is_ok();
                (Self::default(), moved)
            }
        }
    }

    /// Writes through a temporary file, so a crash never leaves half a file.
    pub fn save(&self, config: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(config)?;
        let path = Self::file(config);
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(temporary, path)
    }

    pub fn of_project<'a>(&'a self, project: &str) -> impl Iterator<Item = &'a Lead> + use<'a> {
        let project = project.to_owned();
        self.leads
            .iter()
            .filter(move |lead| lead.project == project)
    }

    pub fn get(&self, id: &str) -> Option<&Lead> {
        self.leads.iter().find(|lead| lead.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Lead> {
        self.leads.iter_mut().find(|lead| lead.id == id)
    }

    /// The project's lead for `chat`, if it has one.
    pub fn by_chat(&self, project: &str, chat: &str) -> Option<&Lead> {
        self.of_project(project)
            .find(|lead| lead.chat.as_deref() == Some(chat))
    }

    /// The project's lead with these phone digits, if it has one.
    pub fn by_phone(&self, project: &str, phone: &str) -> Option<&Lead> {
        let phone = phone_key(phone);
        (!phone.is_empty())
            .then(|| {
                self.of_project(project)
                    .find(|lead| lead.phone.as_deref().map(phone_key).as_deref() == Some(&phone))
            })
            .flatten()
    }

    /// Adds `lead` unless the project already has one for its chat or
    /// phone; returns the id of the one kept.
    pub fn add(&mut self, lead: Lead) -> String {
        let existing = lead
            .chat
            .as_deref()
            .and_then(|chat| self.by_chat(&lead.project, chat))
            .or_else(|| {
                lead.phone
                    .as_deref()
                    .and_then(|phone| self.by_phone(&lead.project, phone))
            });
        if let Some(existing) = existing {
            return existing.id.clone();
        }
        let id = lead.id.clone();
        self.leads.push(lead);
        id
    }

    pub fn remove(&mut self, id: &str) {
        self.release(id);
        self.leads.retain(|lead| lead.id != id);
    }

    /// Unlinks a lead from its customer, keeping its open situations to close.
    pub fn unlink(&mut self, id: &str) {
        self.release(id);
        if let Some(lead) = self.get_mut(id) {
            lead.unlink();
        }
    }

    /// Moves a lead's open situations in its brand to the brand's list of
    /// ones to close, before the lead or its link goes.
    fn release(&mut self, id: &str) {
        let Some(lead) = self.leads.iter_mut().find(|lead| lead.id == id) else {
            return;
        };
        let Some(slug) = self.brands.get(&lead.project).map(|b| b.slug.clone()) else {
            return;
        };
        let mut open: Vec<String> = lead.open_task.take().into_iter().collect();
        open.append(&mut lead.close);
        if !open.is_empty() {
            self.orphans.entry(slug).or_default().extend(open);
        }
    }

    /// How many of the project's leads are in each stage.
    pub fn counts(&self, project: &str) -> BTreeMap<Stage, usize> {
        let mut counts = BTreeMap::new();
        for lead in self.of_project(project) {
            *counts.entry(lead.stage).or_default() += 1;
        }
        counts
    }

    /// How `project` sells.
    pub fn settings_of(&self, project: &str) -> Settings {
        self.settings.get(project).cloned().unwrap_or_default()
    }

    /// The project's leads grouped by urgency, each group by its day and
    /// then by who waited longest.
    pub fn grouped(&self, project: &str, today: &str) -> Vec<(Urgency, Vec<&Lead>)> {
        self.grouped_with(project, today, &|_| false)
    }

    /// As `grouped`, knowing who wrote last (`replied`).
    pub fn grouped_with(
        &self,
        project: &str,
        today: &str,
        replied: &dyn Fn(&Lead) -> bool,
    ) -> Vec<(Urgency, Vec<&Lead>)> {
        let rebuy = self.settings_of(project).rebuy_days;
        let mut groups: BTreeMap<Urgency, Vec<&Lead>> = BTreeMap::new();
        for lead in self.of_project(project) {
            let urgency = lead.urgency_with(today, replied(lead), rebuy);
            groups.entry(urgency).or_default().push(lead);
        }
        let mut out: Vec<_> = groups.into_iter().collect();
        for (urgency, leads) in &mut out {
            match urgency {
                Urgency::Overdue | Urgency::Today | Urgency::Upcoming => leads.sort_by(|a, b| {
                    let due = |lead: &Lead| lead.next.as_ref().and_then(|n| n.due.clone());
                    due(a)
                        .cmp(&due(b))
                        .then(a.last_touch().cmp(&b.last_touch()))
                }),
                Urgency::NoAction | Urgency::Reply | Urgency::Collect | Urgency::Rebuy => {
                    leads.sort_by_key(|lead| lead.last_touch())
                }
                Urgency::Customers | Urgency::Lost => {
                    leads.sort_by_key(|lead| std::cmp::Reverse(lead.updated))
                }
            }
        }
        out
    }

    /// The project's sales since `since` (Unix seconds).
    pub fn month(&self, project: &str, since: i64) -> Month {
        let mut month = Month::default();
        let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
        for lead in self.of_project(project) {
            if lead.created >= since {
                month.new += 1;
            }
            if lead.stage == Stage::Customer
                && lead.moved_to(Stage::Customer).is_some_and(|at| at >= since)
            {
                month.won += 1;
                let sold: f64 = lead
                    .sales
                    .iter()
                    .filter(|s| s.at >= since)
                    .map(|s| s.total)
                    .sum();
                month.won_value += if sold > 0.0 {
                    sold
                } else {
                    lead.value.as_deref().and_then(amount).unwrap_or(0.0)
                };
            }
            if lead.stage == Stage::Lost && lead.moved_to(Stage::Lost).is_some_and(|at| at >= since)
            {
                month.lost += 1;
                *reasons
                    .entry(lead.lost_reason.clone().unwrap_or_else(|| "Other".into()))
                    .or_default() += 1;
            }
        }
        month.lost_reasons = reasons.into_iter().collect();
        month
            .lost_reasons
            .sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        month
    }

    /// How many leads of `project` were first contacted on the day that
    /// starts at `since`: the count WhatsApp watches for spam.
    pub fn first_contacts(&self, project: &str, since: i64) -> usize {
        self.of_project(project)
            .filter(|lead| {
                lead.moved_to(Stage::Contacted)
                    .is_some_and(|at| at >= since)
            })
            .filter(|lead| {
                lead.entries
                    .iter()
                    .filter(|e| e.kind == Kind::Stage && e.text == "contacted")
                    .count()
                    == 1
            })
            .count()
    }

    /// The project's customers sold on WhatsApp with no order in the brand.
    pub fn unregistered(&self, project: &str) -> Vec<&Lead> {
        self.of_project(project)
            .filter(|lead| lead.needs_sale())
            .collect()
    }

    /// Removes the project's brand and forgets the links to its customers.
    pub fn disconnect(&mut self, project: &str) {
        let ids: Vec<String> = self
            .of_project(project)
            .map(|lead| lead.id.clone())
            .collect();
        for id in &ids {
            self.release(id);
        }
        self.brands.remove(project);
        for lead in self.leads.iter_mut().filter(|lead| lead.project == project) {
            lead.unlink();
            lead.orders = None;
        }
    }

    /// Removes everything of a project that no longer exists.
    pub fn forget_project(&mut self, project: &str) {
        let ids: Vec<String> = self
            .of_project(project)
            .map(|lead| lead.id.clone())
            .collect();
        for id in &ids {
            self.release(id);
        }
        self.leads.retain(|lead| lead.project != project);
        self.brands.remove(project);
        self.templates.remove(project);
        self.settings.remove(project);
        self.dismissed.remove(project);
    }

    /// The analysis' suggestions for `project` worth showing: people with
    /// no lead in it yet, not turned down, and not on the team.
    pub fn suggestions<'a>(
        &self,
        project: &crate::projects::Project,
        hints: &'a [crate::projects::LeadHint],
    ) -> Vec<&'a crate::projects::LeadHint> {
        let dismissed = self.dismissed.get(&project.id);
        hints
            .iter()
            .filter(|hint| dismissed.is_none_or(|ids| !ids.contains(&hint.id)))
            .filter(|hint| self.by_chat(&project.id, &hint.person).is_none())
            .filter(|hint| {
                phone_of_chat(&hint.person)
                    .is_none_or(|phone| self.by_phone(&project.id, &phone).is_none())
            })
            .filter(|hint| {
                !project
                    .members
                    .iter()
                    .any(|member| member.id == hint.person)
            })
            .collect()
    }

    /// A lead made from the analysis' suggestion.
    pub fn from_hint(project: &str, hint: &crate::projects::LeadHint) -> Lead {
        let mut lead = Lead::new(
            project,
            &hint.name,
            Some(hint.person.clone()),
            phone_of_chat(&hint.person),
        );
        let stage = match hint.stage.as_str() {
            "quoted" => Stage::Quoted,
            "customer" => Stage::Customer,
            _ => Stage::Contacted,
        };
        if !hint.why.is_empty() {
            lead.add(Kind::Note, &hint.why);
        }
        lead.set_stage(stage, None);
        lead.value = hint.amount.clone();
        if hint.paid {
            let line = match &hint.amount {
                Some(amount) => format!("Dice que pagó {amount}"),
                None => "Dice que pagó".to_owned(),
            };
            lead.add(Kind::Note, &line);
        } else if stage == Stage::Customer {
            lead.set_next("Confirmar el pago", Some(today()));
        }
        // It happened when they wrote, not now: it is no first contact of
        // today, nor a sale of this month unless it was.
        if let Some(since) = hint.since {
            lead.created = since;
            for entry in &mut lead.entries {
                entry.at = since;
            }
        }
        lead
    }
}

/// The leads, shared by the page and the thread that talks to ElevaLeads.
/// Loaded once from the config folder; every change is saved at once.
static STORE: Mutex<Option<(PathBuf, Leads, bool)>> = Mutex::new(None);

/// The guard over the leads of `config`, loading them the first time.
pub struct Store(MutexGuard<'static, Option<(PathBuf, Leads, bool)>>);

impl Store {
    pub fn open(config: &Path) -> Self {
        let mut guard = STORE.lock().unwrap_or_else(|p| p.into_inner());
        if guard.as_ref().is_none_or(|(path, _, _)| path != config) {
            let (leads, writable) = Leads::load_checked(config);
            *guard = Some((config.to_owned(), leads, writable));
        }
        Self(guard)
    }

    pub fn save(&self) {
        // A file that could not be read is never saved over.
        if let Some((config, leads, true)) = self.0.as_ref()
            && let Err(error) = leads.save(config)
        {
            log::warn!("leads: could not be saved: {error}");
        }
    }
}

impl std::ops::Deref for Store {
    type Target = Leads;
    fn deref(&self) -> &Leads {
        &self.0.as_ref().expect("opened").1
    }
}

impl std::ops::DerefMut for Store {
    fn deref_mut(&mut self) -> &mut Leads {
        &mut self.0.as_mut().expect("opened").1
    }
}

/// An amount as people write it here: `45.000`, `45.000,50`, `$ 1,5`.
pub fn amount(text: &str) -> Option<f64> {
    let clean: String = text
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == ',')
        .collect();
    if clean.is_empty() {
        return None;
    }
    let normal = if clean.contains(',') {
        clean.replace('.', "").replace(',', ".")
    } else if clean.matches('.').count() > 1
        || clean
            .rsplit_once('.')
            .is_some_and(|(_, decimals)| decimals.len() == 3)
    {
        clean.replace('.', "")
    } else {
        clean
    };
    normal
        .parse()
        .ok()
        .filter(|n: &f64| n.is_finite() && *n >= 0.0)
}

/// An order the brand counts as not fully paid.
fn unpaid(order: &crate::eleva::sales::Order) -> bool {
    matches!(order.payment.as_str(), "pendiente" | "parcial")
        && !matches!(order.status.as_str(), "borrador" | "cancelado")
}

/// `12/10/2026` (as ElevaLeads writes dates) to the start of that day.
fn day_of(text: &str) -> Option<i64> {
    let mut parts = text
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty());
    let day: i8 = parts.next()?.parse().ok()?;
    let month: i8 = parts.next()?.parse().ok()?;
    let year: i16 = parts.next()?.parse().ok()?;
    let date = jiff::civil::Date::new(year, month, day).ok()?;
    crate::util::day_bounds(date).map(|(start, _)| start)
}

/// The start of today in the local time zone, as Unix seconds.
pub fn today_start() -> i64 {
    crate::util::day_bounds(crate::util::today()).map_or(0, |(start, _)| start)
}

/// The start of this month in the local time zone, as Unix seconds.
pub fn month_start() -> i64 {
    let today = crate::util::today();
    jiff::civil::Date::new(today.year(), today.month(), 1)
        .ok()
        .and_then(crate::util::day_bounds)
        .map_or(0, |(start, _)| start)
}

/// A phone's digits as WhatsApp writes them, for comparing: an Argentine
/// mobile written without the 9 (`54 11 …`) is the same as `54 9 11 …`.
pub fn phone_key(phone: &str) -> String {
    let digits = digits(phone);
    match digits.strip_prefix("54") {
        Some(rest) if !rest.starts_with('9') && rest.len() == 10 => format!("549{rest}"),
        _ => digits,
    }
}

/// Only the digits of a phone number.
pub fn digits(phone: &str) -> String {
    phone.chars().filter(char::is_ascii_digit).collect()
}

/// The phone digits of a chat with one person (`5491112345678@s.whatsapp.net`).
pub fn phone_of_chat(chat: &str) -> Option<String> {
    let (user, server) = chat.split_once('@')?;
    let user = user.split(':').next().unwrap_or(user);
    (server == "s.whatsapp.net" && user.len() >= 7 && user.chars().all(|c| c.is_ascii_digit()))
        .then(|| user.to_owned())
}

/// The first name, for `{name}` in a saved message.
pub fn first_name(name: &str) -> &str {
    name.split_whitespace().next().unwrap_or(name)
}

/// A saved message with the lead's first name put in.
pub fn fill(template: &str, name: &str) -> String {
    template.replace("{name}", first_name(name))
}

/// Today in the local time zone, as `YYYY-MM-DD`.
pub fn today() -> String {
    crate::util::today().to_string()
}

/// The day `days` from today, as `YYYY-MM-DD`.
pub fn in_days(days: i64) -> String {
    crate::util::today()
        .checked_add(jiff::Span::new().days(days))
        .unwrap_or_else(|_| crate::util::today())
        .to_string()
}

fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

fn new_id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = jiff::Timestamp::now().as_nanosecond();
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{nanos:x}-{count}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lead(name: &str, chat: Option<&str>) -> Lead {
        Lead::new("p1", name, chat.map(str::to_owned), None)
    }

    #[test]
    fn urgency_follows_the_next_action() {
        let mut lead = lead("Lab Ruiz", None);
        assert_eq!(lead.urgency("2026-10-17"), Urgency::NoAction);
        lead.set_next("Send the price", Some("2026-10-16".into()));
        assert_eq!(lead.urgency("2026-10-17"), Urgency::Overdue);
        lead.set_next("Send the price", Some("2026-10-17".into()));
        assert_eq!(lead.urgency("2026-10-17"), Urgency::Today);
        lead.set_next("Send the price", Some("2026-10-20".into()));
        assert_eq!(lead.urgency("2026-10-17"), Urgency::Upcoming);
        lead.set_next("Call back", None);
        assert_eq!(lead.urgency("2026-10-17"), Urgency::Today);
        lead.set_stage(Stage::Customer, None);
        assert_eq!(lead.urgency("2026-10-17"), Urgency::Customers);
        assert!(lead.next.is_none(), "a closed sale has nothing left to do");
    }

    #[test]
    fn stage_changes_are_kept_and_lost_needs_its_reason() {
        let mut lead = lead("Dr Gómez", None);
        lead.set_stage(Stage::Quoted, None);
        lead.set_stage(Stage::Quoted, None);
        lead.set_stage(Stage::Lost, Some("Price"));
        let stages: Vec<_> = lead
            .entries
            .iter()
            .filter(|entry| entry.kind == Kind::Stage)
            .map(|entry| entry.text.as_str())
            .collect();
        assert_eq!(stages, ["quoted", "lost"]);
        assert_eq!(lead.lost_reason.as_deref(), Some("Price"));
        lead.set_stage(Stage::Contacted, None);
        assert_eq!(lead.lost_reason, None);
    }

    #[test]
    fn a_recorded_call_keeps_its_transcript_beside_the_summary() {
        let mut lead = lead("Farmacia Sur", None);
        let entry = lead.add_call(
            "(2:05) Wants 500 units",
            "  Hola, sí, queremos 500…  ",
            "1760-ab",
        );
        assert_eq!(entry.kind, Kind::Call);
        assert_eq!(entry.text, "(2:05) Wants 500 units");
        assert_eq!(entry.transcript.as_deref(), Some("Hola, sí, queremos 500…"));
        assert_eq!(entry.call.as_deref(), Some("1760-ab"));
        assert_eq!(entry.sync, Sync::Local);
        assert_eq!(lead.stage, Stage::Contacted, "a call is a contact");
        assert_eq!(lead.add_call("Short", " ", "").transcript, None);
        assert_eq!(
            lead.entries
                .iter()
                .filter(|e| e.kind == Kind::Stage)
                .count(),
            1
        );
        // Entries noted before there were transcripts still read.
        let old: Entry = serde_json::from_str(
            r#"{"id":"e","at":1,"kind":"call","text":"Old","sync":{"state":"sent"}}"#,
        )
        .unwrap();
        assert_eq!(old.transcript, None);
        assert_eq!(old.call, None);
        assert!(!old.waiting(), "no call in the library: nothing to read");
        assert!(!serde_json::to_string(&old).unwrap().contains("transcript"));
    }

    #[test]
    fn a_call_waits_here_until_it_is_read_then_goes_to_eleva() {
        let mut lead = lead("Farmacia Sur", None);
        lead.link(ElevaCustomer {
            id: "c1".into(),
            name: "Farmacia Sur".into(),
            stage: Some("En conversación".into()),
            phone: None,
        });
        let placeholder = format!("(1:00) {NOT_TRANSCRIBED}");
        let id = lead.add_call(&placeholder, "", "1760-cd").id.clone();
        let entry = lead.entries.iter().find(|e| e.id == id).unwrap();
        assert!(entry.waiting());
        assert_eq!(entry.sync, Sync::Local, "a placeholder is never sent");
        assert_eq!(lead.stage, Stage::Contacted);
        // Linked again, the placeholder still waits.
        lead.unlink();
        lead.link(ElevaCustomer {
            id: "c1".into(),
            name: "Farmacia Sur".into(),
            stage: None,
            phone: None,
        });
        let entry = lead.entries.iter().find(|e| e.id == id).unwrap();
        assert_eq!(entry.sync, Sync::Local);
        // Still not heard: still here.
        lead.redo_call(&id, &placeholder, "");
        assert_eq!(
            lead.entries.iter().find(|e| e.id == id).unwrap().sync,
            Sync::Local
        );
        // Read at last: sent, once.
        lead.redo_call(&id, "(1:00) Wants a quote", "Hola…");
        let entry = lead.entries.iter().find(|e| e.id == id).unwrap();
        assert_eq!(entry.text, "(1:00) Wants a quote");
        assert_eq!(entry.transcript.as_deref(), Some("Hola…"));
        assert_eq!(entry.sync, Sync::Pending);
        assert_eq!(entry.call.as_deref(), Some("1760-cd"));
    }

    #[test]
    fn a_call_never_moves_a_customer_back_in_eleva() {
        for (there, moved) in [
            (Some("Cliente"), false),
            (Some("Interesado"), false),
            (Some("algo nuevo"), false),
            (None, false),
            (Some("Nuevo"), true),
            (Some("En conversación"), true),
        ] {
            let mut lead = lead("Ana", None);
            lead.link(ElevaCustomer {
                id: "c1".into(),
                name: "Ana".into(),
                stage: there.map(str::to_owned),
                phone: None,
            });
            lead.add_call("(0:40) Hola", "Hola", "c");
            assert_eq!(lead.stage == Stage::Contacted, moved, "{there:?}");
        }
    }

    #[test]
    fn a_call_note_keeps_what_was_agreed_within_elevas_limit() {
        use crate::call_library::{CallAnalysis, Commitment};
        let mut analysis = CallAnalysis {
            summary: " Quieren 500 guardapolvos bordados. ".into(),
            commitments: vec![
                Commitment {
                    who: "Cliente".into(),
                    what: "Mandar el logo".into(),
                    due: None,
                },
                Commitment {
                    who: "Yo".into(),
                    what: "Mandar la cotización".into(),
                    due: Some("2026-10-20".into()),
                },
                Commitment {
                    who: "Yo".into(),
                    what: " ".into(),
                    due: None,
                },
            ],
            ..CallAnalysis::default()
        };
        assert_eq!(
            call_note("(4:12)", &analysis),
            "(4:12) Quieren 500 guardapolvos bordados.\n\nAcordado:\n\
             • Cliente: Mandar el logo\n\
             • Yo: Mandar la cotización (2026-10-20)"
        );
        analysis.summary = "palabra ".repeat(300);
        let note = call_note("(30:00)", &analysis);
        assert!(note.chars().count() <= CALL_NOTE_LIMIT);
        assert!(note.ends_with("• Yo: Mandar la cotización (2026-10-20)"));
        assert!(note.contains("…\n\nAcordado:"));
        // Many long commitments: the last give way, the note still fits.
        analysis.commitments = (0..20)
            .map(|n| Commitment {
                who: "Yo".into(),
                what: format!("{n} {}", "detalle ".repeat(10)),
                due: None,
            })
            .collect();
        let note = call_note("(30:00)", &analysis);
        assert!(
            note.chars().count() <= CALL_NOTE_LIMIT,
            "{}",
            note.chars().count()
        );
        assert!(note.ends_with("• …"));
        assert!(note.contains("• Yo: 0 detalle"));
        analysis.commitments.clear();
        analysis.summary = "Nada pactado".into();
        assert_eq!(call_note("", &analysis), "Nada pactado");
    }

    #[test]
    fn a_heading_is_read_back_whole() {
        assert_eq!(call_heading(252), "(4:12)");
        assert_eq!(heading_of("(4:12) Quiere precio"), "(4:12)");
        assert_eq!(
            heading_of("(Llamada (Ana) 17-10.m4a) Todavía sin transcribir"),
            "(Llamada (Ana) 17-10.m4a)"
        );
        assert_eq!(heading_of("Sin título"), "");
        assert_eq!(heading_of("(sin cerrar"), "");
    }

    #[test]
    fn entries_wait_for_eleva_only_when_linked() {
        let mut lead = lead("Farmacia Sur", None);
        lead.add(Kind::Note, "First visit");
        assert_eq!(lead.entries[0].sync, Sync::Local);
        lead.link(ElevaCustomer {
            id: "c1".into(),
            name: "Farmacia Sur".into(),
            stage: None,
            phone: None,
        });
        assert_eq!(
            lead.entries[0].sync,
            Sync::Pending,
            "what came before is sent too"
        );
        lead.add(Kind::Call, "Wants 500 units");
        assert_eq!(lead.pending(), 2);
        lead.unlink();
        // Linked again later: one stage and the open action, not the trail.
        lead.set_stage(Stage::Contacted, None);
        lead.set_stage(Stage::Quoted, None);
        lead.set_next("Old", Some("2026-10-01".into()));
        lead.set_next("Send the price", Some("2026-10-20".into()));
        lead.link(ElevaCustomer {
            id: "c1".into(),
            name: "Farmacia Sur".into(),
            stage: None,
            phone: None,
        });
        let sent: Vec<_> = lead
            .entries
            .iter()
            .filter(|entry| entry.sync == Sync::Pending)
            .map(|entry| entry.text.as_str())
            .collect();
        assert_eq!(
            sent,
            [
                "First visit",
                "Wants 500 units",
                "quoted",
                "Send the price · 2026-10-20"
            ]
        );
        lead.unlink();
        assert_eq!(lead.pending(), 0);
    }

    #[test]
    fn a_chat_or_phone_has_one_lead_per_project() {
        let mut leads = Leads::default();
        let first = leads.add(lead("Ana", Some("5491111111111@s.whatsapp.net")));
        let again = leads.add(lead("Ana again", Some("5491111111111@s.whatsapp.net")));
        assert_eq!(first, again);
        let by_phone = Lead::new("p1", "Bea", None, Some("+54 9 11 2222-2222".into()));
        let bea = leads.add(by_phone);
        let twin = leads.add(Lead::new("p1", "Bea", None, Some("5491122222222".into())));
        assert_eq!(bea, twin);
        let other_project = leads.add(Lead::new(
            "p2",
            "Ana",
            Some("5491111111111@s.whatsapp.net".into()),
            None,
        ));
        assert_ne!(other_project, first);
        assert_eq!(leads.leads.len(), 3);
    }

    #[test]
    fn groups_put_what_is_late_first() {
        let mut leads = Leads::default();
        let mut late = lead("Late", None);
        late.set_next("Call", Some("2026-10-10".into()));
        let mut later = lead("Later", None);
        later.set_next("Call", Some("2026-10-01".into()));
        let mut won = lead("Won", None);
        won.set_stage(Stage::Customer, None);
        let cold = lead("Cold", None);
        for lead in [late, later, won, cold] {
            leads.add(lead);
        }
        let groups = leads.grouped("p1", "2026-10-17");
        let order: Vec<_> = groups.iter().map(|(urgency, _)| *urgency).collect();
        assert_eq!(
            order,
            [Urgency::Overdue, Urgency::NoAction, Urgency::Customers]
        );
        let overdue: Vec<_> = groups[0].1.iter().map(|lead| lead.name.as_str()).collect();
        assert_eq!(overdue, ["Later", "Late"]);
        assert_eq!(leads.counts("p1").get(&Stage::New), Some(&3));
    }

    #[test]
    fn stages_map_to_elevaleads_and_back() {
        for stage in Stage::ALL {
            let label = match stage.eleva() {
                "en_charla" => "En conversación",
                other => other,
            };
            assert_eq!(Stage::from_eleva_label(label), Some(stage));
        }
    }

    #[test]
    fn a_customer_without_orders_needs_a_sale() {
        let mut lead = lead("Ana", None);
        lead.set_stage(Stage::Customer, None);
        assert!(!lead.needs_sale(), "not read from the brand yet");
        lead.orders = Some(OrdersSeen {
            known: true,
            orders: crate::eleva::sales::Orders::default(),
            read_at: 1,
        });
        assert!(lead.needs_sale());
        assert_eq!(lead.next_sale_id(), format!("whatspro-{}-1", lead.id));
        lead.sales.push(Sale {
            id: lead.next_sale_id(),
            at: 1,
            total: 90_000.0,
            summary: "2 cinturones".into(),
            code: Some("P-0004".into()),
            notice: None,
            payment: "pagado".into(),
            settled: false,
        });
        assert!(!lead.needs_sale());
        let mut bought = self::lead("Bea", None);
        bought.set_stage(Stage::Customer, None);
        bought.orders = Some(OrdersSeen {
            known: true,
            orders: crate::eleva::sales::Orders {
                count: 1,
                ..Default::default()
            },
            read_at: 1,
        });
        assert!(!bought.needs_sale(), "the shop already has their order");
    }

    #[test]
    fn who_wrote_last_and_what_customers_need_come_first() {
        let mut open = lead("Open", None);
        open.set_next("Call", Some("2026-10-30".into()));
        assert_eq!(open.urgency_with("2026-10-17", true, None), Urgency::Reply);
        assert_eq!(
            open.urgency_with("2026-10-17", false, None),
            Urgency::Upcoming
        );
        let mut lost = lead("Lost", None);
        lost.set_stage(Stage::Lost, Some("Price"));
        assert_eq!(lost.urgency_with("2026-10-17", true, None), Urgency::Lost);

        let mut owes = lead("Owes", None);
        owes.set_stage(Stage::Customer, None);
        // Bought long ago.
        for entry in &mut owes.entries {
            entry.at = 1;
        }
        owes.sales.push(Sale {
            id: "s".into(),
            at: 1,
            total: 10.0,
            summary: String::new(),
            code: None,
            notice: None,
            payment: "parcial".into(),
            settled: false,
        });
        assert_eq!(
            owes.urgency_with("2026-10-17", false, Some(30)),
            Urgency::Collect
        );
        owes.sales[0].payment = "pagado".into();
        assert_eq!(
            owes.urgency_with("2026-10-17", false, Some(30)),
            Urgency::Rebuy,
            "bought long ago"
        );
        assert_eq!(
            owes.urgency_with("2026-10-17", false, None),
            Urgency::Customers
        );
        owes.set_next("Deliver", Some("2026-10-17".into()));
        assert_eq!(
            owes.urgency_with("2026-10-17", false, Some(30)),
            Urgency::Today
        );
    }

    #[test]
    fn chases_count_since_the_stage_moved() {
        let mut lead = lead("Ana", None);
        lead.set_stage(Stage::Contacted, None);
        lead.add(Kind::FollowUp, "✓ Follow up");
        lead.add(Kind::Note, "no answer");
        lead.add(Kind::FollowUp, "✓ Follow up");
        assert_eq!(lead.chases(), 2);
        lead.set_stage(Stage::Quoted, None);
        assert_eq!(lead.chases(), 0);
    }

    #[test]
    fn a_month_of_sales() {
        let mut leads = Leads::default();
        let mut won = lead("Won", None);
        won.value = Some("$ 90.000".into());
        won.set_stage(Stage::Customer, None);
        let mut lost = lead("Lost", None);
        lost.set_stage(Stage::Lost, Some("Price"));
        let mut contacted = lead("New contact", None);
        contacted.set_stage(Stage::Contacted, None);
        for lead in [won, lost, contacted] {
            leads.add(lead);
        }
        let month = leads.month("p1", 0);
        assert_eq!(month.new, 3);
        assert_eq!(month.won, 1);
        assert_eq!(month.won_value, 90_000.0);
        assert_eq!(month.lost_reasons, [("Price".to_owned(), 1)]);
        assert_eq!(month.conversion(), Some(50));
        assert_eq!(leads.first_contacts("p1", 0), 1);
        assert_eq!(amount("45.000,50"), Some(45_000.5));
        assert_eq!(amount("$ 1.250.000"), Some(1_250_000.0));
    }

    #[test]
    fn suggestions_leave_out_known_people_and_the_team() {
        let mut map = crate::projects::ProjectMap::default();
        let id = map.add("Cinturity");
        let mut project = map.project(&id).unwrap().clone();
        project.members.push(crate::projects::Member {
            id: "5491133333333@s.whatsapp.net".into(),
            name: "Socia".into(),
            role: String::new(),
            about: String::new(),
        });
        let hint = |person: &str| crate::projects::LeadHint {
            id: format!("lead:{person}"),
            name: "X".into(),
            person: person.into(),
            why: "50 cinturones".into(),
            stage: "quoted".into(),
            amount: Some("$ 90.000".into()),
            ..Default::default()
        };
        let hints = vec![
            hint("5491100000000@s.whatsapp.net"),
            hint("5491111111111@s.whatsapp.net"),
            hint("5491122222222@s.whatsapp.net"),
            hint("5491133333333@s.whatsapp.net"),
        ];
        let mut leads = Leads::default();
        leads.add(Lead::new(
            &id,
            "Ya está",
            None,
            Some("+54 9 11 0000-0000".into()),
        ));
        leads
            .dismissed
            .insert(id.clone(), vec!["lead:5491111111111@s.whatsapp.net".into()]);
        let shown = leads.suggestions(&project, &hints);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].person, "5491122222222@s.whatsapp.net");
        let lead = Leads::from_hint(&id, shown[0]);
        assert_eq!(lead.stage, Stage::Quoted);
        assert_eq!(lead.phone.as_deref(), Some("5491122222222"));
        assert_eq!(lead.value.as_deref(), Some("$ 90.000"));
        assert_eq!(lead.entries[0].text, "50 cinturones");
    }

    #[test]
    fn phones_come_from_direct_chats_only() {
        assert_eq!(
            phone_of_chat("56950862615@s.whatsapp.net").as_deref(),
            Some("56950862615")
        );
        assert_eq!(phone_of_chat("120363000000@g.us"), None);
        assert_eq!(phone_of_chat("12345@lid"), None);
        assert_eq!(
            fill("Hola {name}, ¿cómo va?", "Ana María"),
            "Hola Ana, ¿cómo va?"
        );
    }

    #[test]
    fn the_file_round_trips() {
        let dir = std::env::temp_dir().join(format!("whatspro-leads-{}", std::process::id()));
        let mut leads = Leads::default();
        let mut one = lead("Ana", None);
        one.set_stage(Stage::Quoted, None);
        one.entries[0].sync = Sync::Failed("read only".into());
        leads.add(one);
        leads.brands.insert(
            "p1".into(),
            Brand {
                slug: "blackyak-labs".into(),
                name: "Blackyak Labs".into(),
                write: true,
            },
        );
        leads.save(&dir).unwrap();
        assert_eq!(Leads::load(&dir), leads);
        let _ = std::fs::remove_dir_all(dir);
    }
}
