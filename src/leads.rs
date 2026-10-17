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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub at: i64,
    pub kind: Kind,
    pub text: String,
    #[serde(default)]
    pub sync: Sync,
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
    pub created: i64,
    pub updated: i64,
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
        });
        self.updated = now;
        self.entries.last().expect("just pushed")
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
            if wanted && matches!(entry.sync, Sync::Local | Sync::Failed(_)) {
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
