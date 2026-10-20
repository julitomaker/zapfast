//! Changes to leads asked for by another AI through the MCP server: make a
//! lead, move it, plan its next step, note something, tie it to a chat.
//!
//! The MCP server only queues the request; the app applies it here, on the
//! shared store, so nothing it has cached writes over it. Every change is
//! noted in the lead's history as made by the client that asked (that note
//! stays here). What it changes reaches ElevaLeads the way a change made in
//! the app does: as pending entries of a linked lead, sent by the usual push
//! only with a token that writes. Nothing is ever sent to a WhatsApp chat.

use serde_json::{Value, json};

use super::{Kind, Lead, Leads, Stage, Sync};
use crate::projects::{Project, ProjectMap};

/// The write tools, as the MCP server lists them.
pub fn tools() -> Vec<Value> {
    let stage = json!({"type": "string", "enum": ["new", "contacted", "quoted", "customer", "lost"], "description": "new, contacted, quoted (price or quote given), customer (bought), lost."});
    let due = json!({"type": "string", "description": "When: YYYY-MM-DD, \"today\", \"tomorrow\", or a number of days from today."});
    vec![
        json!({
            "name": "create_lead",
            "description": "Create a lead (someone the project is selling to) in a Whats Pro project, from a chat or a phone number. If the project already has a lead for that chat or number, that one is updated instead. Nothing is sent to the person.",
            "inputSchema": {"type": "object", "properties": {
                "project": {"type": "string", "description": "Project name, part of it, or id."},
                "name": {"type": "string", "description": "The person's or business' name."},
                "chat": {"type": "string", "description": "The WhatsApp chat id with the person (from list_chats), if there is one."},
                "phone": {"type": "string", "description": "Phone number with country code, if there is no chat."},
                "stage": stage,
                "amount": {"type": "string", "description": "What the sale is worth, as written (\"$ 850.000\")."},
                "next_action": {"type": "string", "description": "What to do next with them."},
                "due": due,
                "note": {"type": "string", "description": "Why they are a lead, what they want."}
            }, "required": ["project"]},
            "annotations": {"readOnlyHint": false, "destructiveHint": false}
        }),
        json!({
            "name": "update_lead",
            "description": "Change a lead: its stage, what it is worth, its next action and day, or tick the next action off. Nothing is sent to the person.",
            "inputSchema": {"type": "object", "properties": {
                "lead": {"type": "string", "description": "Lead id, name, part of it, or phone."},
                "project": {"type": "string", "description": "Only in this project, when the name is in several."},
                "stage": stage,
                "lost_reason": {"type": "string", "enum": ["Price", "Chose someone else", "No answer", "Later on", "Other"], "description": "Why it was lost, with stage lost."},
                "amount": {"type": "string"},
                "next_action": {"type": "string", "description": "The new next action; an empty text clears it."},
                "due": due,
                "done": {"type": "boolean", "description": "Tick the current next action off."}
            }, "required": ["lead"]},
            "annotations": {"readOnlyHint": false, "destructiveHint": false}
        }),
        json!({
            "name": "add_lead_note",
            "description": "Add a note, a call summary or a follow-up to a lead's history. Nothing is sent to the person.",
            "inputSchema": {"type": "object", "properties": {
                "lead": {"type": "string", "description": "Lead id, name, part of it, or phone."},
                "project": {"type": "string"},
                "text": {"type": "string"},
                "kind": {"type": "string", "enum": ["note", "call", "follow_up"], "description": "Default note."}
            }, "required": ["lead", "text"]},
            "annotations": {"readOnlyHint": false, "destructiveHint": false}
        }),
        json!({
            "name": "link_chat_to_lead",
            "description": "Tie a WhatsApp chat (id from list_chats) to a lead, so its messages and replies count for it.",
            "inputSchema": {"type": "object", "properties": {
                "lead": {"type": "string", "description": "Lead id, name, part of it, or phone."},
                "project": {"type": "string"},
                "chat": {"type": "string", "description": "The chat id."}
            }, "required": ["lead", "chat"]},
            "annotations": {"readOnlyHint": false, "destructiveHint": false}
        }),
    ]
}

/// Whether `tool` is one of these.
pub fn handles(tool: &str) -> bool {
    matches!(
        tool,
        "create_lead" | "update_lead" | "add_lead_note" | "link_chat_to_lead"
    )
}

/// Applies one write tool and says what it did, in words for the AI.
pub fn apply(
    leads: &mut Leads,
    map: &ProjectMap,
    tool: &str,
    args: &Value,
    client: &str,
) -> Result<String, String> {
    // Also the names the project tools use: `value` and `next`.
    let mut args = args.clone();
    if let Some(object) = args.as_object_mut() {
        for (alias, name) in [("value", "amount"), ("next", "next_action")] {
            if !object.contains_key(name)
                && let Some(value) = object.remove(alias)
            {
                object.insert(name.to_owned(), value);
            }
        }
    }
    let args = &args;
    let client = if client.trim().is_empty() {
        "IA"
    } else {
        client.trim()
    };
    match tool {
        "create_lead" => create(leads, map, args, client),
        "update_lead" => update(leads, map, args, client),
        "add_lead_note" => note(leads, map, args, client),
        "link_chat_to_lead" => link_chat(leads, map, args, client),
        _ => Err(format!("Unknown tool {tool}.")),
    }
}

fn text(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| match v {
            Value::String(s) => Some(s.trim().to_owned()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .filter(|s| !s.is_empty())
}

fn stage_of(value: &str) -> Result<Stage, String> {
    match value.trim().to_lowercase().as_str() {
        "new" | "nuevo" => Ok(Stage::New),
        "contacted" | "contactado" | "en_charla" => Ok(Stage::Contacted),
        "quoted" | "cotizado" | "interesado" => Ok(Stage::Quoted),
        "customer" | "cliente" => Ok(Stage::Customer),
        "lost" | "perdido" | "dormido" => Ok(Stage::Lost),
        other => Err(format!(
            "Unknown stage \"{other}\": use new, contacted, quoted, customer or lost."
        )),
    }
}

/// `YYYY-MM-DD`, today, tomorrow, or days from today.
fn due_of(value: &str) -> Result<String, String> {
    let value = value.trim().to_lowercase();
    match value.as_str() {
        "today" | "hoy" => return Ok(super::today()),
        "tomorrow" | "mañana" => return Ok(super::in_days(1)),
        _ => {}
    }
    if let Ok(days) = value.trim_start_matches('+').parse::<i64>() {
        return Ok(super::in_days(days.clamp(0, 3650)));
    }
    value
        .parse::<jiff::civil::Date>()
        .map(|date| date.to_string())
        .map_err(|_| {
            format!("Unknown day \"{value}\": use YYYY-MM-DD, today, tomorrow or a number of days.")
        })
}

fn project<'a>(map: &'a ProjectMap, wanted: &str) -> Result<&'a Project, String> {
    if let Some(project) = map.project(wanted) {
        return Ok(project);
    }
    let needle = wanted.trim().to_lowercase();
    let exact: Vec<&Project> = map
        .projects
        .iter()
        .filter(|p| p.name.to_lowercase() == needle)
        .collect();
    let found: Vec<&Project> = if exact.is_empty() {
        map.projects
            .iter()
            .filter(|p| p.name.to_lowercase().contains(&needle))
            .collect()
    } else {
        exact
    };
    match found.as_slice() {
        [project] => Ok(project),
        [] => Err(format!(
            "No project is called \"{wanted}\". Use list_projects to find it."
        )),
        several => Err(format!(
            "Several projects match \"{wanted}\": {}. Ask again with the exact name or id.",
            several
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// The lead `wanted` names: its id, exact name, part of it, or phone,
/// within `project` when given.
fn lead_id(leads: &Leads, map: &ProjectMap, args: &Value) -> Result<String, String> {
    let wanted = text(args, "lead").ok_or("`lead` is required.")?;
    let only = text(args, "project")
        .map(|p| project(map, &p))
        .transpose()?;
    let pool = || {
        leads
            .leads
            .iter()
            .filter(|l| only.is_none_or(|p| l.project == p.id))
    };
    if let Some(lead) = pool().find(|l| l.id == wanted) {
        return Ok(lead.id.clone());
    }
    let needle = wanted.to_lowercase();
    let digits = super::phone_key(&wanted);
    let exact: Vec<&Lead> = pool().filter(|l| l.name.to_lowercase() == needle).collect();
    let found: Vec<&Lead> = if exact.is_empty() {
        pool()
            .filter(|l| {
                l.name.to_lowercase().contains(&needle)
                    || (digits.len() >= 6
                        && l.phone
                            .as_deref()
                            .is_some_and(|p| super::phone_key(p).contains(&digits)))
            })
            .collect()
    } else {
        exact
    };
    match found.as_slice() {
        [lead] => Ok(lead.id.clone()),
        [] => Err(format!(
            "No lead is called \"{wanted}\". Use list_leads to find it."
        )),
        several => Err(format!(
            "Several leads match \"{wanted}\": {}. Ask again with the lead id or the project.",
            several
                .iter()
                .map(|l| format!("{} ({})", l.name, l.id))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// A line in the lead's history saying who changed what. It stays here:
/// ElevaLeads gets the changes themselves.
fn by(lead: &mut Lead, client: &str, what: &str) {
    let line = format!("{what} — por {client}");
    lead.add(Kind::Note, &line);
    if let Some(entry) = lead.entries.last_mut() {
        entry.sync = Sync::Local;
    }
}

/// Stage, amount and next action from `args`, as one list of what changed.
fn changes(lead: &mut Lead, args: &Value) -> Result<Vec<String>, String> {
    let mut changed = Vec::new();
    if let Some(stage) = text(args, "stage") {
        let stage = stage_of(&stage)?;
        if stage != lead.stage {
            let reason = text(args, "lost_reason");
            lead.set_stage(
                stage,
                reason
                    .as_deref()
                    .or(Some("Other"))
                    .filter(|_| stage == Stage::Lost),
            );
            changed.push(format!("etapa: {}", stage.label()));
        }
    }
    if let Some(amount) = text(args, "amount") {
        lead.value = Some(amount.clone());
        changed.push(format!("monto: {amount}"));
    }
    if args.get("done").and_then(Value::as_bool) == Some(true)
        && let Some(next) = lead.finish_next()
    {
        changed.push(format!("hecho: {}", next.text));
        if lead.stage == Stage::New {
            lead.set_stage(Stage::Contacted, None);
        }
    }
    let due = text(args, "due").map(|d| due_of(&d)).transpose()?;
    match (args.get("next_action").and_then(Value::as_str), due) {
        (Some(next), due) => {
            lead.set_next(
                next,
                due.or_else(|| lead.next.as_ref().and_then(|n| n.due.clone())),
            );
            changed.push(if next.trim().is_empty() {
                "sin próxima acción".to_owned()
            } else {
                format!("próxima acción: {}", next.trim())
            });
        }
        (None, Some(due)) => {
            let next = lead
                .next
                .clone()
                .ok_or("The lead has no next action to give a day to.")?;
            lead.set_next(&next.text, Some(due.clone()));
            changed.push(format!("para el {due}"));
        }
        (None, None) => {}
    }
    Ok(changed)
}

fn summary(lead: &Lead) -> String {
    let next = lead
        .next
        .as_ref()
        .map(|n| match &n.due {
            Some(due) => format!("; next: {} on {due}", n.text),
            None => format!("; next: {}", n.text),
        })
        .unwrap_or_default();
    format!(
        "{} ({}) is {}{next}. Lead id: {}.",
        lead.name,
        lead.project,
        lead.stage.label(),
        lead.id
    )
}

fn create(
    leads: &mut Leads,
    map: &ProjectMap,
    args: &Value,
    client: &str,
) -> Result<String, String> {
    let project = project(map, &text(args, "project").ok_or("`project` is required.")?)?;
    let chat = text(args, "chat");
    if let Some(chat) = &chat
        && !chat.contains('@')
    {
        return Err(format!(
            "\"{chat}\" is not a chat id: take it from list_chats (it ends in @s.whatsapp.net)."
        ));
    }
    let phone = text(args, "phone")
        .map(|p| super::digits(&p))
        .filter(|p| p.len() >= 7)
        .or_else(|| chat.as_deref().and_then(super::phone_of_chat));
    let name = text(args, "name")
        .or_else(|| phone.as_ref().map(|p| format!("+{p}")))
        .ok_or("Give the lead a `name`, a `chat` or a `phone`.")?;
    let fresh = Lead::new(&project.id, &name, chat, phone);
    let new_id = fresh.id.clone();
    let id = leads.add(fresh);
    let existed = id != new_id;
    let lead = leads.get_mut(&id).ok_or("The lead could not be kept.")?;
    if let Some(note) = text(args, "note") {
        lead.add(Kind::Note, &note);
    }
    let changed = changes(lead, args)?;
    let what = if existed {
        format!("Actualizado ({})", changed.join(", "))
    } else {
        "Creado".to_owned()
    };
    by(lead, client, &what);
    Ok(format!(
        "{} {}",
        if existed {
            "The project already had this lead; updated it."
        } else {
            "Created."
        },
        summary(lead)
    ))
}

fn update(
    leads: &mut Leads,
    map: &ProjectMap,
    args: &Value,
    client: &str,
) -> Result<String, String> {
    let id = lead_id(leads, map, args)?;
    let lead = leads.get_mut(&id).ok_or("The lead is gone.")?;
    let changed = changes(lead, args)?;
    if changed.is_empty() {
        return Err("Nothing to change: give stage, amount, next_action, due or done.".into());
    }
    by(lead, client, &format!("Cambió {}", changed.join(", ")));
    Ok(format!("Updated. {}", summary(lead)))
}

fn note(leads: &mut Leads, map: &ProjectMap, args: &Value, client: &str) -> Result<String, String> {
    let id = lead_id(leads, map, args)?;
    let body = text(args, "text").ok_or("`text` is required.")?;
    let kind = match text(args, "kind").as_deref() {
        None | Some("note") => Kind::Note,
        Some("call") => Kind::Call,
        Some("follow_up") => Kind::FollowUp,
        Some(other) => {
            return Err(format!(
                "Unknown kind \"{other}\": use note, call or follow_up."
            ));
        }
    };
    let lead = leads.get_mut(&id).ok_or("The lead is gone.")?;
    lead.add(kind, &format!("{body} — por {client}"));
    Ok(format!("Noted. {}", summary(lead)))
}

fn link_chat(
    leads: &mut Leads,
    map: &ProjectMap,
    args: &Value,
    client: &str,
) -> Result<String, String> {
    let id = lead_id(leads, map, args)?;
    let chat = text(args, "chat").ok_or("`chat` is required.")?;
    if !chat.contains('@') {
        return Err(format!(
            "\"{chat}\" is not a chat id: take it from list_chats."
        ));
    }
    let project = leads
        .get(&id)
        .map(|l| l.project.clone())
        .unwrap_or_default();
    if let Some(other) = leads.by_chat(&project, &chat).filter(|l| l.id != id) {
        return Err(format!(
            "That chat is already the lead \"{}\" ({}).",
            other.name, other.id
        ));
    }
    let lead = leads.get_mut(&id).ok_or("The lead is gone.")?;
    lead.chat = Some(chat.clone());
    if lead.phone.is_none() {
        lead.phone = super::phone_of_chat(&chat);
    }
    by(lead, client, "Vinculó el chat");
    Ok(format!("Linked. {}", summary(lead)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (Leads, ProjectMap, String) {
        let mut map = ProjectMap::default();
        let id = map.add("Blackyak Labs");
        map.add("Bordados JAS");
        (Leads::default(), map, id)
    }

    #[test]
    fn makes_moves_notes_and_links_a_lead() {
        let (mut leads, map, project) = setup();
        let made = apply(
            &mut leads,
            &map,
            "create_lead",
            &json!({"project": "blackyak", "name": "Lab Ruiz", "phone": "+54 9 11 5555-0000", "stage": "quoted", "amount": "$ 850.000", "next_action": "Mandar la cotización", "due": "tomorrow", "note": "Pidió 500 guardapolvos"}),
            "claude-code",
        )
        .unwrap();
        assert!(made.starts_with("Created."), "{made}");
        let lead = &leads.leads[0];
        assert_eq!(lead.project, project);
        assert_eq!(lead.stage, Stage::Quoted);
        assert_eq!(
            lead.next.as_ref().unwrap().due.as_deref(),
            Some(super::super::in_days(1).as_str())
        );
        assert!(
            lead.entries
                .iter()
                .any(|e| e.text == "Creado — por claude-code" && e.sync == Sync::Local)
        );

        // The same number again updates it.
        let again = apply(
            &mut leads,
            &map,
            "create_lead",
            &json!({"project": "Blackyak Labs", "phone": "541155550000", "amount": "$ 900.000"}),
            "claude",
        )
        .unwrap();
        assert!(again.starts_with("The project already had"), "{again}");
        assert_eq!(leads.leads.len(), 1);
        assert_eq!(leads.leads[0].value.as_deref(), Some("$ 900.000"));

        apply(
            &mut leads,
            &map,
            "update_lead",
            &json!({"lead": "ruiz", "done": true, "next_action": "Llamar", "due": "3"}),
            "claude",
        )
        .unwrap();
        let lead = &leads.leads[0];
        assert!(
            lead.entries
                .iter()
                .any(|e| e.text == "✓ Mandar la cotización")
        );
        assert_eq!(lead.next.as_ref().unwrap().text, "Llamar");

        apply(
            &mut leads,
            &map,
            "add_lead_note",
            &json!({"lead": "5555-0000", "text": "Quiere factura A", "kind": "call"}),
            "claude",
        )
        .unwrap();
        assert!(
            leads.leads[0]
                .entries
                .iter()
                .any(|e| e.kind == Kind::Call && e.text == "Quiere factura A — por claude")
        );

        apply(
            &mut leads,
            &map,
            "link_chat_to_lead",
            &json!({"lead": "Lab Ruiz", "chat": "5491155550000@s.whatsapp.net"}),
            "claude",
        )
        .unwrap();
        assert_eq!(
            leads.leads[0].chat.as_deref(),
            Some("5491155550000@s.whatsapp.net")
        );
    }

    #[test]
    fn says_what_is_wrong() {
        let (mut leads, map, _) = setup();
        assert!(
            apply(
                &mut leads,
                &map,
                "create_lead",
                &json!({"project": "nada"}),
                "c"
            )
            .unwrap_err()
            .contains("No project")
        );
        assert!(
            apply(
                &mut leads,
                &map,
                "create_lead",
                &json!({"project": "JAS"}),
                "c"
            )
            .unwrap_err()
            .contains("name")
        );
        assert!(
            apply(
                &mut leads,
                &map,
                "create_lead",
                &json!({"project": "JAS", "name": "X", "stage": "maybe"}),
                "c"
            )
            .unwrap_err()
            .contains("Unknown stage")
        );
        assert!(
            apply(
                &mut leads,
                &map,
                "update_lead",
                &json!({"lead": "nadie"}),
                "c"
            )
            .unwrap_err()
            .contains("No lead")
        );
        apply(
            &mut leads,
            &map,
            "create_lead",
            &json!({"project": "JAS", "name": "Ana"}),
            "c",
        )
        .unwrap();
        assert!(
            apply(
                &mut leads,
                &map,
                "update_lead",
                &json!({"lead": "Ana"}),
                "c"
            )
            .unwrap_err()
            .contains("Nothing to change")
        );
        assert!(
            apply(
                &mut leads,
                &map,
                "link_chat_to_lead",
                &json!({"lead": "Ana", "chat": "Ana"}),
                "c"
            )
            .unwrap_err()
            .contains("not a chat id")
        );
        apply(
            &mut leads,
            &map,
            "update_lead",
            &json!({"lead": "Ana", "value": "$ 10", "next": "Llamar"}),
            "c",
        )
        .unwrap();
        let ana = leads.leads.iter().find(|l| l.name == "Ana").unwrap();
        assert_eq!(ana.value.as_deref(), Some("$ 10"));
        assert_eq!(ana.next.as_ref().unwrap().text, "Llamar");
        assert!(handles("update_lead") && !handles("list_leads"));
        assert_eq!(tools().len(), 4);
    }
}
