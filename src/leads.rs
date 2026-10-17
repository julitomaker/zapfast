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
}

/// Where a lead sits in the list: what needs doing first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Urgency {
    Overdue,
    Today,
    /// Open, with nothing planned: the ones going cold.
    NoAction,
    Upcoming,
    Customers,
    Lost,
}

impl Urgency {
    pub const ALL: [Urgency; 6] = [
        Urgency::Overdue,
        Urgency::Today,
        Urgency::NoAction,
        Urgency::Upcoming,
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

    /// Where it goes in the list on `today` (`YYYY-MM-DD`).
    pub fn urgency(&self, today: &str) -> Urgency {
        match self.stage {
            Stage::Customer => return Urgency::Customers,
            Stage::Lost => return Urgency::Lost,
            _ => {}
        }
        match self.next.as_ref().and_then(|next| next.due.as_deref()) {
            Some(due) if due < today => Urgency::Overdue,
            Some(due) if due == today => Urgency::Today,
            Some(_) => Urgency::Upcoming,
            None if self.next.is_some() => Urgency::Today,
            None => Urgency::NoAction,
        }
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
        }
    }

    /// Plans what comes next; an empty text clears it.
    pub fn set_next(&mut self, text: &str, due: Option<String>) {
        let text = text.trim();
        if text.is_empty() {
            self.next = None;
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
        self.add(Kind::Task, &line);
    }

    /// Links the lead to an ElevaLeads customer, and sends what was
    /// written before: every note, but only the stage it is in now and the
    /// next action still open, not each step it took to get here.
    pub fn link(&mut self, customer: ElevaCustomer) {
        self.eleva = Some(customer);
        let last = |kind: Kind| self.entries.iter().rposition(|entry| entry.kind == kind);
        let stage = last(Kind::Stage);
        let task = last(Kind::Task).filter(|_| self.next.is_some());
        for (index, entry) in self.entries.iter_mut().enumerate() {
            let wanted = match entry.kind {
                Kind::Stage => Some(index) == stage,
                Kind::Task => Some(index) == task,
                _ => true,
            };
            if wanted && !entry.waiting() && matches!(entry.sync, Sync::Local | Sync::Failed(_)) {
                entry.sync = Sync::Pending;
            }
        }
        self.updated = now();
    }

    pub fn unlink(&mut self) {
        self.eleva = None;
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
    pub fn next_sale_id(&self) -> String {
        format!("whatspro-{}-{}", self.id, self.sales.len() + 1)
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
}

impl Leads {
    pub fn file(config: &Path) -> PathBuf {
        config.join("leads.json")
    }

    pub fn load(config: &Path) -> Self {
        std::fs::read_to_string(Self::file(config))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
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
        let phone = digits(phone);
        (!phone.is_empty())
            .then(|| {
                self.of_project(project)
                    .find(|lead| lead.phone.as_deref().map(digits).as_deref() == Some(&phone))
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
        self.leads.retain(|lead| lead.id != id);
    }

    /// How many of the project's leads are in each stage.
    pub fn counts(&self, project: &str) -> BTreeMap<Stage, usize> {
        let mut counts = BTreeMap::new();
        for lead in self.of_project(project) {
            *counts.entry(lead.stage).or_default() += 1;
        }
        counts
    }

    /// The project's leads grouped by urgency, each group by its day and
    /// then by who waited longest.
    pub fn grouped(&self, project: &str, today: &str) -> Vec<(Urgency, Vec<&Lead>)> {
        let mut groups: BTreeMap<Urgency, Vec<&Lead>> = BTreeMap::new();
        for lead in self.of_project(project) {
            groups.entry(lead.urgency(today)).or_default().push(lead);
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
                Urgency::NoAction => leads.sort_by_key(|lead| lead.last_touch()),
                Urgency::Customers | Urgency::Lost => {
                    leads.sort_by_key(|lead| std::cmp::Reverse(lead.updated))
                }
            }
        }
        out
    }

    /// The project's customers sold on WhatsApp with no order in the brand.
    pub fn unregistered(&self, project: &str) -> Vec<&Lead> {
        self.of_project(project)
            .filter(|lead| lead.needs_sale())
            .collect()
    }

    /// Removes the project's brand and forgets the links to its customers.
    pub fn disconnect(&mut self, project: &str) {
        self.brands.remove(project);
        for lead in self.leads.iter_mut().filter(|lead| lead.project == project) {
            lead.unlink();
        }
    }

    /// Removes everything of a project that no longer exists.
    pub fn forget_project(&mut self, project: &str) {
        self.leads.retain(|lead| lead.project != project);
        self.brands.remove(project);
        self.templates.remove(project);
    }
}

/// The leads, shared by the page and the thread that talks to ElevaLeads.
/// Loaded once from the config folder; every change is saved at once.
static STORE: Mutex<Option<(PathBuf, Leads)>> = Mutex::new(None);

/// The guard over the leads of `config`, loading them the first time.
pub struct Store(MutexGuard<'static, Option<(PathBuf, Leads)>>);

impl Store {
    pub fn open(config: &Path) -> Self {
        let mut guard = STORE.lock().unwrap_or_else(|p| p.into_inner());
        if guard.as_ref().is_none_or(|(path, _)| path != config) {
            *guard = Some((config.to_owned(), Leads::load(config)));
        }
        Self(guard)
    }

    pub fn save(&self) {
        if let Some((config, leads)) = self.0.as_ref()
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
