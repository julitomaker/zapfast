//! Messages an AI sends or drafts through the MCP server, with the limits
//! that keep the account clear of WhatsApp's anti-spam checks.
//!
//! WhatsApp bans automated, bulk behaviour, not a linked device: so an AI
//! only writes to chats that already had a conversation, at a person's pace
//! (one message every 20 seconds, at most 30 an hour), and never sends the
//! same text to many chats. Every message sent is logged in
//! `<config>/ai-sent.jsonl` with who sent it, which also keeps the limits
//! across restarts.

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

/// Shortest gap between two messages an AI sends.
pub const GAP_SECONDS: i64 = 20;
/// Most messages an AI sends in an hour.
pub const PER_HOUR: usize = 30;
/// Most chats one text may go to in a day.
pub const SAME_TEXT_CHATS: usize = 3;
/// Longest message.
pub const MAX_CHARS: usize = 4_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sent {
    pub at: i64,
    pub chat: String,
    pub text: String,
    pub client: String,
}

fn log_file(config: &Path) -> PathBuf {
    config.join("ai-sent.jsonl")
}

/// The messages sent in the last day.
pub fn recent(config: &Path, now: i64) -> Vec<Sent> {
    std::fs::read_to_string(log_file(config))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Sent>(line).ok())
        .filter(|sent| now - sent.at < 86_400)
        .collect()
}

pub fn record(config: &Path, sent: &Sent) {
    let line = match serde_json::to_string(sent) {
        Ok(line) => line,
        Err(_) => return,
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file(config))
    {
        let _ = writeln!(file, "{line}");
    }
}

fn normalized(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Why a message may not be sent now, or `None` when it may.
pub fn refuse(history: &[Sent], chat: &str, text: &str, now: i64) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Some("The message is empty.".into());
    }
    if trimmed.chars().count() > MAX_CHARS {
        return Some(format!("The message is longer than {MAX_CHARS} characters."));
    }
    if let Some(last) = history.iter().map(|sent| sent.at).max()
        && now - last < GAP_SECONDS
    {
        return Some(format!(
            "Wait {} more seconds: messages go out at a person's pace to keep the account safe.",
            GAP_SECONDS - (now - last)
        ));
    }
    let last_hour = history.iter().filter(|sent| now - sent.at < 3_600).count();
    if last_hour >= PER_HOUR {
        return Some(format!(
            "{PER_HOUR} messages were already sent this hour; WhatsApp flags more as automated. Try later, or send it from Whats Pro."
        ));
    }
    let same = normalized(trimmed);
    let mut chats: Vec<&str> = history
        .iter()
        .filter(|sent| normalized(&sent.text) == same && sent.chat != chat)
        .map(|sent| sent.chat.as_str())
        .collect();
    chats.sort_unstable();
    chats.dedup();
    if chats.len() >= SAME_TEXT_CHATS {
        return Some(
            "This same text already went to several chats today; sending it to more looks like bulk messaging. Personalize it for this chat.".into(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent(at: i64, chat: &str, text: &str) -> Sent {
        Sent {
            at,
            chat: chat.into(),
            text: text.into(),
            client: "claude".into(),
        }
    }

    #[test]
    fn a_person_pace_is_kept() {
        let history = vec![sent(1_000, "a", "hola")];
        assert!(refuse(&history, "b", "otra cosa", 1_010).is_some(), "too soon");
        assert!(refuse(&history, "b", "otra cosa", 1_021).is_none());
        assert!(refuse(&[], "a", "   ", 0).is_some(), "empty");
    }

    #[test]
    fn an_hour_and_bulk_are_limited() {
        let full: Vec<Sent> = (0..PER_HOUR as i64)
            .map(|i| sent(i * 60, &format!("c{i}"), &format!("texto {i}")))
            .collect();
        assert!(refuse(&full, "x", "nuevo", 3_000).is_some(), "30 in the hour");
        let bulk = vec![
            sent(0, "a", "Promo de hoy"),
            sent(100, "b", "promo  de HOY"),
            sent(200, "c", "Promo de hoy"),
        ];
        assert!(refuse(&bulk, "d", "Promo de hoy", 1_000).is_some(), "same text to a fourth chat");
        assert!(refuse(&bulk, "a", "Promo de hoy", 1_000).is_none(), "again to a chat it went to");
        assert!(refuse(&bulk, "d", "Hola Dani, ¿cómo va?", 1_000).is_none());
    }
}
