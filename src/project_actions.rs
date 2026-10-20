//! What an AI connected over MCP may change in Whats Pro: projects (their
//! chats, members and context), notes, tasks, closing or reopening the
//! panel's items, running the analysis, and leads.
//!
//! The bridge runs inside the app, so these change the same files and the
//! same in-memory leads the app uses: the Projects page reloads the project
//! map when its file changes, and the leads go through their shared store.
//! Every change carries who made it ("Claude"), shown in the project's
//! history, where it can be undone like any other.

use std::{path::Path, sync::Mutex};

use serde_json::{Value, json};

use crate::projects::{
    Finding, Journal, Member, Mode, NoteKind, Project, ProjectMap, Tasks, Ticks, Verdict,
};

/// One change at a time: the map is read, changed and written whole.
static LOCK: Mutex<()> = Mutex::new(());

type Answer = Result<Value, String>;

/// Applies `action` with its arguments, made by `client`.
pub fn apply(config: &Path, action: &str, args: &Value, client: &str) -> Answer {
    let _held = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let by = Some(client_name(client));
    match action {
        "create_project" => create_project(config, args),
        "update_project" => update_project(config, args),
        "add_note" => add_note(config, args, by),
        "add_task" => add_task(config, args, by),
        "resolve_item" => judge(config, args, by, true),
        "reopen_item" => judge(config, args, by, false),
        "run_analysis" => run_analysis(config, args),
        "create_lead" => create_lead(config, args, by),
        "update_lead" => update_lead(config, args, by),
        "add_lead_note" => add_lead_note(config, args, by),
        _ => Err(format!("Unknown action {action}.")),
    }
}

/// "claude-ai" or "Claude Code" → "Claude"; anything else as given.
fn client_name(client: &str) -> String {
    let lower = client.to_lowercase();
    if lower.contains("claude") {
        "Claude".into()
    } else if lower.contains("codex") {
        "Codex".into()
    } else if lower.contains("gemini") || lower.contains("antigravity") {
        "Gemini".into()
    } else if client.trim().is_empty() || lower == "unknown" {
        "IA".into()
    } else {
        client.trim().to_owned()
    }
}

fn text(args: &Value, name: &str) -> Option<String> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn required(args: &Value, name: &str) -> Result<String, String> {
    text(args, name).ok_or_else(|| format!("`{name}` is required."))
}

/// A project by id or by (part of) its name, case-insensitively.
fn find<'a>(map: &'a ProjectMap, wanted: &str) -> Result<&'a Project, String> {
    let lower = wanted.to_lowercase();
    if let Some(project) = map.project(wanted) {
        return Ok(project);
    }
    let matches: Vec<&Project> =
        map.projects
            .iter()
            .filter(|p| p.name.to_lowercase() == lower)
            .chain(map.projects.iter().filter(|p| {
                p.name.to_lowercase() != lower && p.name.to_lowercase().contains(&lower)
            }))
            .collect();
    match matches.as_slice() {
        [] => Err(format!(
            "No project matches \"{wanted}\". Use list_projects."
        )),
        [one, ..] if one.name.to_lowercase() == lower => Ok(one),
        [one] => Ok(one),
        many => Err(format!(
            "\"{wanted}\" matches several projects: {}. Use its id.",
            many.iter()
                .map(|p| format!("{} ({})", p.name, p.id))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn project_id(config: &Path, args: &Value) -> Result<String, String> {
    let map = ProjectMap::load(config);
    Ok(find(&map, &required(args, "project")?)?.id.clone())
}

fn mode(value: Option<&str>) -> Option<Mode> {
    match value?.to_lowercase().as_str() {
        "all" | "todo" => Some(Mode::All),
        "smart" | "inteligente" => Some(Mode::Smart),
        "selected" | "elegidos" => Some(Mode::Selected),
        _ => None,
    }
}

/// `sources: [{chat, mode?}]` or `["chat id", …]`.
fn sources(args: &Value, name: &str) -> Vec<(String, Option<Mode>)> {
    args.get(name)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| match item {
                    Value::String(chat) => Some((chat.trim().to_owned(), None)),
                    Value::Object(_) => text(item, "chat")
                        .map(|chat| (chat, mode(item.get("mode").and_then(Value::as_str)))),
                    _ => None,
                })
                .filter(|(chat, _)| !chat.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// `members: [{id?, name, role?, about?}]`. A bare phone becomes its digits.
fn members(args: &Value) -> Vec<Member> {
    args.get("members")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let name = text(item, "name")?;
                    let id = text(item, "id")
                        .or_else(|| text(item, "phone"))
                        .map(|id| {
                            if id.contains('@') {
                                id
                            } else {
                                id.chars().filter(char::is_ascii_digit).collect()
                            }
                        })
                        .unwrap_or_default();
                    Some(Member {
                        id,
                        name,
                        role: text(item, "role").unwrap_or_default(),
                        about: text(item, "about").unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn save_map(config: &Path, map: &ProjectMap) -> Result<(), String> {
    map.save(config)
        .map_err(|error| format!("The project could not be saved: {error}"))
}

fn create_project(config: &Path, args: &Value) -> Answer {
    let name = required(args, "name")?;
    let mut map = ProjectMap::load(config);
    let id = map.add(&name);
    for (chat, wanted) in sources(args, "sources") {
        let mode = wanted.unwrap_or_else(|| map.default_mode(&id, &chat));
        map.add_source(&id, &chat, mode);
    }
    let added = members(args);
    if let Some(project) = map.project_mut(&id) {
        if let Some(context) = text(args, "context").or_else(|| text(args, "description")) {
            project.context = context;
        }
        if let Some(color) = text(args, "color") {
            project.color = color;
        }
        project.members = added;
    }
    save_map(config, &map)?;
    let project = map.project(&id).expect("just added");
    Ok(json!({
        "id": id,
        "name": project.name,
        "sources": project.sources.len(),
        "members": project.members.len(),
        "message": format!("Project \"{}\" created.", project.name),
    }))
}

fn update_project(config: &Path, args: &Value) -> Answer {
    let mut map = ProjectMap::load(config);
    let id = find(&map, &required(args, "project")?)?.id.clone();
    for (chat, wanted) in sources(args, "add_sources") {
        let mode = wanted.unwrap_or_else(|| map.default_mode(&id, &chat));
        map.add_source(&id, &chat, mode);
    }
    for (chat, _) in sources(args, "remove_sources") {
        map.remove_source(&id, &chat);
    }
    let added = members(args);
    let removed: Vec<String> = args
        .get("remove_members")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_lowercase)
                .collect()
        })
        .unwrap_or_default();
    let project = map.project_mut(&id).expect("found");
    if let Some(name) = text(args, "name") {
        project.name = name;
    }
    if let Some(context) = text(args, "context").or_else(|| text(args, "description")) {
        project.context = context;
    }
    if let Some(color) = text(args, "color") {
        project.color = color;
    }
    // Members are matched by id, or by name when they have none.
    for member in added {
        let same = project.members.iter_mut().find(|m| {
            (!member.id.is_empty() && m.id == member.id)
                || m.name.to_lowercase() == member.name.to_lowercase()
        });
        match same {
            Some(existing) => {
                existing.name = member.name;
                if !member.id.is_empty() {
                    existing.id = member.id;
                }
                if !member.role.is_empty() {
                    existing.role = member.role;
                }
                if !member.about.is_empty() {
                    existing.about = member.about;
                }
            }
            None => project.members.push(member),
        }
    }
    project.members.retain(|m| {
        !removed.contains(&m.id.to_lowercase()) && !removed.contains(&m.name.to_lowercase())
    });
    let summary = json!({
        "id": project.id,
        "name": project.name,
        "sources": project.sources.len(),
        "members": project.members.iter().map(|m| json!({"id": m.id, "name": m.name, "role": m.role})).collect::<Vec<_>>(),
        "message": format!("Project \"{}\" updated.", project.name),
    });
    save_map(config, &map)?;
    Ok(summary)
}

fn add_note(config: &Path, args: &Value, by: Option<String>) -> Answer {
    let id = project_id(config, args)?;
    let note = required(args, "text")?;
    let kind = match text(args, "kind").as_deref() {
        Some("memo") | Some("apunte") => NoteKind::Memo,
        _ => NoteKind::Log,
    };
    let mut journal = Journal::load(config, &id);
    journal.add_by(&note, false, kind, by);
    journal
        .save(config, &id)
        .map_err(|error| format!("The note could not be saved: {error}"))?;
    // A note can say something was done: let the analysis look again.
    crate::projects::engine::refresh(&id);
    Ok(json!({"project": id, "message": "Note added. The analysis will read it."}))
}

fn add_task(config: &Path, args: &Value, by: Option<String>) -> Answer {
    let id = project_id(config, args)?;
    let mut tasks = Tasks::load(config, &id);
    let task = tasks.add(
        &required(args, "text")?,
        text(args, "who"),
        text(args, "due"),
        by,
    );
    tasks
        .save(config, &id)
        .map_err(|error| format!("The task could not be saved: {error}"))?;
    Ok(json!({"project": id, "id": task.id, "message": "Task added to the panel's To do."}))
}

/// Every item on a project's panel: the analysis' findings and the tasks.
fn items(config: &Path, project: &str) -> Vec<Finding> {
    let mut all: Vec<Finding> = crate::projects::Analysis::load(config, project)
        .map(|a| {
            a.todo
                .into_iter()
                .chain(a.follow_up)
                .chain(a.leaks)
                .chain(a.needs)
                .chain(a.highlights)
                .collect()
        })
        .unwrap_or_default();
    all.extend(
        Tasks::load(config, project)
            .items
            .into_iter()
            .map(|t| t.finding),
    );
    all
}

/// Closes (`done`) or reopens an item, by id or by words in its text.
fn judge(config: &Path, args: &Value, by: Option<String>, done: bool) -> Answer {
    let id = project_id(config, args)?;
    let wanted = required(args, "item")?;
    let lower = wanted.to_lowercase();
    let all = items(config, &id);
    let matches: Vec<&Finding> = match all.iter().find(|f| f.id == wanted) {
        Some(exact) => vec![exact],
        None => all
            .iter()
            .filter(|f| f.text.to_lowercase().contains(&lower))
            .collect(),
    };
    let finding = match matches.as_slice() {
        [] => {
            return Err(format!(
                "No item on the panel matches \"{wanted}\". Use project_overview to see the ids."
            ));
        }
        [one] => *one,
        many => {
            return Err(format!(
                "\"{wanted}\" matches several items: {}. Use its id.",
                many.iter()
                    .map(|f| format!("{} ({})", f.text, f.id))
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
    };
    let mut ticks = Ticks::load(config, &id);
    if done {
        let verdict = if args.get("dismiss").and_then(Value::as_bool) == Some(true) {
            Verdict::Dismissed
        } else {
            Verdict::Done
        };
        ticks.judge_by(finding, verdict, text(args, "how"), by);
    } else {
        ticks.reopen(&finding.id);
    }
    ticks
        .save(config, &id)
        .map_err(|error| format!("The panel could not be saved: {error}"))?;
    Ok(json!({
        "project": id,
        "item": finding.id,
        "text": finding.text,
        "message": if done { "Closed. It can be undone from the project's History." } else { "Back on the panel." },
    }))
}

fn run_analysis(config: &Path, args: &Value) -> Answer {
    let id = project_id(config, args)?;
    let settings = crate::projects::engine::EngineConfig::load(config);
    if settings.provider == crate::projects::engine::Provider::Off {
        return Err(
            "The analysis is off. Turn it on in Whats Pro > Projects > Organize > AI analysis."
                .into(),
        );
    }
    crate::projects::engine::refresh(&id);
    let status = crate::projects::engine::status();
    Ok(json!({
        "project": id,
        "running": status.running,
        "last_run": status.last_run,
        "message": "Analysis started. It usually takes under a minute; then read project_overview.",
    }))
}

// ------------------------------------------------------------------ leads

fn stage(value: &str) -> Option<crate::leads::Stage> {
    use crate::leads::Stage;
    Some(match value.to_lowercase().as_str() {
        "new" | "nuevo" => Stage::New,
        "contacted" | "contactado" => Stage::Contacted,
        "quoted" | "cotizado" => Stage::Quoted,
        "customer" | "cliente" => Stage::Customer,
        "lost" | "perdido" => Stage::Lost,
        _ => return None,
    })
}

fn find_lead(store: &crate::leads::Leads, wanted: &str) -> Result<String, String> {
    if store.get(wanted).is_some() {
        return Ok(wanted.to_owned());
    }
    let lower = wanted.to_lowercase();
    let digits: String = wanted.chars().filter(char::is_ascii_digit).collect();
    let matches: Vec<&crate::leads::Lead> = store
        .leads
        .iter()
        .filter(|lead| {
            lead.name.to_lowercase().contains(&lower)
                || (digits.len() >= 6
                    && lead.phone.as_deref().is_some_and(|p| p.ends_with(&digits)))
                || lead.chat.as_deref() == Some(wanted)
        })
        .collect();
    match matches.as_slice() {
        [] => Err(format!("No lead matches \"{wanted}\". Use list_leads.")),
        [one] => Ok(one.id.clone()),
        many => Err(format!(
            "\"{wanted}\" matches several leads: {}. Use its id.",
            many.iter()
                .map(|l| format!("{} ({})", l.name, l.id))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn note_by(text: &str, by: &Option<String>) -> String {
    match by {
        Some(by) => format!("{text} ({by})"),
        None => text.to_owned(),
    }
}

fn create_lead(config: &Path, args: &Value, by: Option<String>) -> Answer {
    let project = project_id(config, args)?;
    let name = required(args, "name")?;
    let chat = text(args, "chat");
    let phone =
        text(args, "phone").map(|p| p.chars().filter(char::is_ascii_digit).collect::<String>());
    let mut store = crate::leads::Store::open(config);
    if let Some(existing) = chat
        .as_deref()
        .and_then(|chat| store.by_chat(&project, chat))
        .or_else(|| phone.as_deref().and_then(|p| store.by_phone(&project, p)))
    {
        return Err(format!(
            "{} is already a lead in this project (id {}). Use update_lead.",
            existing.name, existing.id
        ));
    }
    let mut lead = crate::leads::Lead::new(&project, &name, chat, phone);
    if let Some(stage) = text(args, "stage").as_deref().and_then(stage) {
        lead.set_stage(stage, text(args, "lost_reason").as_deref());
    }
    if let Some(value) = text(args, "value") {
        lead.value = Some(value);
    }
    if let Some(next) = text(args, "next") {
        lead.set_next(&next, text(args, "due"));
    }
    if let Some(note) = text(args, "note") {
        lead.add(crate::leads::Kind::Note, &note_by(&note, &by));
    }
    let id = store.add(lead);
    store.save();
    Ok(json!({"lead": id, "project": project, "message": format!("Lead \"{name}\" created.")}))
}

fn update_lead(config: &Path, args: &Value, by: Option<String>) -> Answer {
    let mut store = crate::leads::Store::open(config);
    let id = find_lead(&store, &required(args, "lead")?)?;
    let lead = store.get_mut(&id).expect("found");
    if let Some(name) = text(args, "name") {
        lead.name = name;
    }
    if let Some(chat) = text(args, "chat") {
        lead.chat = Some(chat);
    }
    if let Some(phone) = text(args, "phone") {
        lead.phone = Some(phone.chars().filter(char::is_ascii_digit).collect());
    }
    if let Some(stage) = text(args, "stage").as_deref().and_then(stage) {
        lead.set_stage(stage, text(args, "lost_reason").as_deref());
    }
    if let Some(value) = text(args, "value") {
        lead.value = Some(value);
    }
    if let Some(next) = text(args, "next") {
        lead.set_next(&next, text(args, "due"));
    }
    if let Some(note) = text(args, "note") {
        lead.add(crate::leads::Kind::Note, &note_by(&note, &by));
    }
    let name = lead.name.clone();
    let stage = lead.stage;
    store.save();
    Ok(json!({"lead": id, "name": name, "stage": stage, "message": "Lead updated."}))
}

fn add_lead_note(config: &Path, args: &Value, by: Option<String>) -> Answer {
    let mut store = crate::leads::Store::open(config);
    let id = find_lead(&store, &required(args, "lead")?)?;
    let note = required(args, "text")?;
    store
        .get_mut(&id)
        .expect("found")
        .add(crate::leads::Kind::Note, &note_by(&note, &by));
    store.save();
    Ok(json!({"lead": id, "message": "Note added to the lead's history."}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_is_made_from_a_chat_then_given_a_task_closed_and_reopened() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path();
        let made = apply(
            config,
            "create_project",
            &json!({
                "name": "Bordados Ana",
                "context": "Uniformes bordados para su local",
                "sources": [{"chat": "5491111@s.whatsapp.net", "mode": "all"}],
                "members": [{"name": "Ana", "phone": "+54 9 11 1111", "role": "Clienta"}]
            }),
            "claude-ai",
        )
        .unwrap();
        let id = made["id"].as_str().unwrap().to_owned();
        let map = ProjectMap::load(config);
        let project = map.project(&id).unwrap();
        assert_eq!(project.context, "Uniformes bordados para su local");
        assert_eq!(project.sources[0].mode, Mode::All);
        assert_eq!(project.members[0].id, "549111111");

        let task = apply(
            config,
            "add_task",
            &json!({"project": "bordados", "text": "Mandar presupuesto"}),
            "Claude Code",
        )
        .unwrap();
        let task_id = task["id"].as_str().unwrap().to_owned();
        assert!(Ticks::load(config, &id).open(&task_id));

        apply(
            config,
            "resolve_item",
            &json!({"project": id, "item": "presupuesto", "how": "Lo mandé por mail"}),
            "claude-ai",
        )
        .unwrap();
        let ticks = Ticks::load(config, &id);
        assert!(!ticks.open(&task_id));
        let feedback = ticks.feedback.last().unwrap();
        assert_eq!(feedback.by.as_deref(), Some("Claude"));
        assert_eq!(feedback.note.as_deref(), Some("Lo mandé por mail"));

        apply(
            config,
            "reopen_item",
            &json!({"project": id, "item": task_id}),
            "claude-ai",
        )
        .unwrap();
        assert!(Ticks::load(config, &id).open(&task_id));
    }

    #[test]
    fn an_update_adds_sources_and_members_and_a_note_says_who_wrote_it() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path();
        apply(config, "create_project", &json!({"name": "Obra"}), "x").unwrap();
        apply(
            config,
            "update_project",
            &json!({"project": "obra", "add_sources": ["123@g.us"], "members": [{"name": "Juan", "role": "Arquitecto"}]}),
            "x",
        )
        .unwrap();
        let map = ProjectMap::load(config);
        assert_eq!(map.project("obra").unwrap().sources.len(), 1);
        assert_eq!(map.project("obra").unwrap().members[0].role, "Arquitecto");
        apply(
            config,
            "add_note",
            &json!({"project": "obra", "text": "Se firmó el contrato"}),
            "codex-mcp-client",
        )
        .unwrap();
        let journal = Journal::load(config, "obra");
        assert_eq!(journal.notes[0].by.as_deref(), Some("Codex"));
    }

    #[test]
    fn unknown_projects_and_items_say_what_to_use_instead() {
        let dir = tempfile::tempdir().unwrap();
        let error = apply(
            dir.path(),
            "add_task",
            &json!({"project": "nada", "text": "x"}),
            "x",
        )
        .unwrap_err();
        assert!(error.contains("list_projects"), "{error}");
    }
}
