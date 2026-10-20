//! Read-only queries for the local bridge (`crate::bridge`).
//!
//! The bridge opens its own query-only connection, so these never write.
//! Locked chats stay out of every answer.

use std::path::Path;

use rusqlite::{Row, params};
use serde::Serialize;

use super::{Archive, Result, SEARCHED_TEXT, encryption, search_pattern};
use crate::model::{Content, Quoted};

/// A chat as the bridge reports it.
#[derive(Debug, Serialize)]
pub struct BridgeChat {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub last_activity: i64,
    pub unread: u32,
    pub archived: bool,
    pub pinned: bool,
    pub muted: bool,
    pub messages: i64,
}

/// A message flattened for reading: who, when, and its text.
#[derive(Debug, Serialize)]
pub struct BridgeMessage {
    pub chat: String,
    pub chat_name: String,
    pub id: String,
    pub sender: String,
    pub sender_name: Option<String>,
    pub from_me: bool,
    pub timestamp: i64,
    pub kind: String,
    pub text: String,
    pub reply_to: Option<BridgeQuote>,
    pub edited: bool,
    /// What was transcribed or read out of the message on this computer.
    pub transcript: Option<String>,
    /// Forwarded from another chat: someone else wrote it first.
    pub forwarded: bool,
    /// WhatsApp has not delivered it to this computer yet (the phone must be
    /// online to resend it).
    pub pending: bool,
}

/// What `bridge_search_filtered` narrows a search to.
#[derive(Debug, Default)]
pub struct SearchFilter<'a> {
    pub chat: Option<&'a str>,
    /// Unix seconds, inclusive.
    pub since: Option<i64>,
    /// Unix seconds, exclusive.
    pub until: Option<i64>,
    /// Part of the sender's name or number; "me" for our own messages.
    pub from: Option<&'a str>,
    /// A message kind: image, video, audio, document, sticker, text…
    pub kind: Option<&'a str>,
}

#[derive(Debug, Serialize)]
pub struct BridgeQuote {
    pub id: String,
    pub sender_name: Option<String>,
    pub text: String,
}

/// Columns every message query selects, in the order `bridge_row` reads.
/// A sender behind a privacy id is named through its phone number.
const COLUMNS: &str = "m.chat, c.name, m.id, m.sender,
    COALESCE(NULLIF(ct.full_name, ''), NULLIF(ct.push_name, ''), m.sender_name),
    m.from_me, m.timestamp, m.content, m.quoted, m.edited, t.text, m.forwarded
    FROM messages m
    JOIN chats c ON c.id = m.chat
    LEFT JOIN lids l ON l.lid = m.sender
    LEFT JOIN contacts ct ON ct.id = COALESCE(l.pn, m.sender)
    LEFT JOIN transcripts t ON t.chat = m.chat AND t.id = m.id";

fn bridge_row(row: &Row) -> Result<BridgeMessage> {
    let content: String = row.get(7)?;
    let quoted: Option<String> = row.get(8)?;
    let kind = serde_json::from_str::<serde_json::Value>(&content)
        .ok()
        .and_then(|value| value.get("kind")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into());
    let parsed = serde_json::from_str::<Content>(&content);
    let pending = matches!(&parsed, Ok(Content::Unsupported { what }) if what.starts_with("Waiting for this message"));
    let text = match parsed {
        Ok(Content::Text { text, .. } | Content::Interactive { text, .. }) => text,
        Ok(content) => content.summary(),
        Err(_) => String::new(),
    };
    Ok(BridgeMessage {
        chat: row.get(0)?,
        chat_name: row.get(1)?,
        id: row.get(2)?,
        sender: row.get(3)?,
        sender_name: row.get(4)?,
        from_me: row.get(5)?,
        timestamp: row.get(6)?,
        kind,
        text,
        reply_to: quoted
            .and_then(|quoted| serde_json::from_str::<Quoted>(&quoted).ok())
            .map(|quoted| BridgeQuote {
                id: quoted.id,
                sender_name: quoted.sender_name,
                text: quoted.summary,
            }),
        edited: row.get(9)?,
        transcript: row.get(10)?,
        forwarded: row.get::<_, Option<bool>>(11)?.unwrap_or(false),
        pending,
    })
}

impl Archive {
    /// Opens a query-only view of an archive the app already keeps.
    pub fn open_reader(path: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            connection: encryption::open_reader(path)?,
        })
    }

    /// Every unlocked chat, most recent first, with its archived message count.
    pub fn bridge_chats(&self) -> Result<Vec<BridgeChat>> {
        let now = jiff::Timestamp::now().as_second();
        let mut statement = self.connection.prepare(
            "SELECT c.id, c.name, c.kind, c.last_activity, c.unread, c.archived, c.pinned,
                    c.muted_until, (SELECT count(*) FROM messages m WHERE m.chat = c.id)
             FROM chats c WHERE c.locked = 0
             ORDER BY c.last_activity DESC",
        )?;
        let rows = statement.query_map([], |row| {
            let muted_until: Option<i64> = row.get(7)?;
            Ok(BridgeChat {
                id: row.get(0)?,
                name: row.get(1)?,
                kind: row.get(2)?,
                last_activity: row.get(3)?,
                unread: row.get(4)?,
                archived: row.get(5)?,
                pinned: row.get(6)?,
                muted: muted_until.is_some_and(|until| until < 0 || until > now),
                messages: row.get(8)?,
            })
        })?;
        rows.collect()
    }

    /// Messages in time order. With `since`, the first `limit` at or after it
    /// (Unix seconds); otherwise the newest `limit` before `before`. `chat`
    /// narrows it to one chat.
    pub fn bridge_messages(
        &self,
        chat: Option<&str>,
        since: Option<i64>,
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<BridgeMessage>> {
        let before = before.unwrap_or(i64::MAX);
        let limit = limit as i64;
        if let Some(since) = since {
            let mut statement = self.connection.prepare(&format!(
                "SELECT {COLUMNS}
                 WHERE c.locked = 0 AND (?1 IS NULL OR m.chat = ?1)
                   AND m.timestamp >= ?2 AND m.timestamp < ?3
                 ORDER BY m.timestamp ASC, m.rowid ASC LIMIT ?4"
            ))?;
            let rows = statement.query_map(params![chat, since, before, limit], bridge_row)?;
            self.name_mentions(rows.collect::<Result<_>>()?)
        } else {
            let mut statement = self.connection.prepare(&format!(
                "SELECT {COLUMNS}
                 WHERE c.locked = 0 AND (?1 IS NULL OR m.chat = ?1) AND m.timestamp < ?2
                 ORDER BY m.timestamp DESC, m.rowid DESC LIMIT ?3"
            ))?;
            let rows = statement.query_map(params![chat, before, limit], bridge_row)?;
            let mut messages: Vec<_> = rows.collect::<Result<_>>()?;
            messages.reverse();
            self.name_mentions(messages)
        }
    }

    /// Messages whose text, caption, file name or similar contains `needle`,
    /// newest first.
    pub fn bridge_search(
        &self,
        needle: &str,
        chat: Option<&str>,
        limit: usize,
    ) -> Result<Vec<BridgeMessage>> {
        self.bridge_search_filtered(
            needle,
            &SearchFilter {
                chat,
                ..SearchFilter::default()
            },
            limit,
        )
    }

    /// Like `bridge_search`, narrowed by time, sender and kind. An empty
    /// needle with a filter lists what matches the filter alone.
    pub fn bridge_search_filtered(
        &self,
        needle: &str,
        filter: &SearchFilter<'_>,
        limit: usize,
    ) -> Result<Vec<BridgeMessage>> {
        let pattern = search_pattern(needle);
        let unfiltered = filter.since.is_none()
            && filter.until.is_none()
            && filter.from.is_none()
            && filter.kind.is_none();
        if pattern.is_none() && unfiltered {
            return Ok(Vec::new());
        }
        let from_me = filter
            .from
            .is_some_and(|from| from.eq_ignore_ascii_case("me"));
        let from = filter
            .from
            .filter(|_| !from_me)
            .map(|from| format!("%{}%", from.to_lowercase()));
        let mut statement = self.connection.prepare(&format!(
            "SELECT {COLUMNS}
             WHERE c.locked = 0 AND (?1 IS NULL OR m.chat = ?1)
               AND (?2 IS NULL
                    OR (json_valid(m.content) AND {SEARCHED_TEXT} LIKE ?2 ESCAPE '\\')
                    OR lower(coalesce(t.text, '')) LIKE ?2 ESCAPE '\\')
               AND (?4 IS NULL OR m.timestamp >= ?4)
               AND (?5 IS NULL OR m.timestamp < ?5)
               AND (?6 = 0 OR m.from_me = 1)
               AND (?7 IS NULL OR (m.from_me = 0 AND (
                    lower(coalesce(ct.full_name, '')) LIKE ?7
                    OR lower(coalesce(ct.push_name, '')) LIKE ?7
                    OR lower(coalesce(m.sender_name, '')) LIKE ?7
                    OR lower(COALESCE(l.pn, m.sender)) LIKE ?7)))
               AND (?8 IS NULL OR (json_valid(m.content) AND json_extract(m.content, '$.kind') = ?8))
             ORDER BY m.timestamp DESC, m.rowid DESC LIMIT ?3"
        ))?;
        let rows = statement.query_map(
            params![
                filter.chat,
                pattern,
                limit as i64,
                filter.since,
                filter.until,
                from_me,
                from,
                filter.kind
            ],
            bridge_row,
        )?;
        self.name_mentions(rows.collect::<Result<_>>()?)
    }

    /// Replaces `@<number>` mentions, which WhatsApp sends as phone numbers
    /// or privacy ids, with the contact's name where it is known.
    fn name_mentions(&self, mut messages: Vec<BridgeMessage>) -> Result<Vec<BridgeMessage>> {
        let mut names: std::collections::HashMap<String, Option<String>> =
            std::collections::HashMap::new();
        for message in &mut messages {
            if message.text.contains('@') {
                message.text = self.replace_mentions(&message.text, &mut names)?;
            }
            if let Some(reply) = &mut message.reply_to
                && reply.text.contains('@')
            {
                reply.text = self.replace_mentions(&reply.text, &mut names)?;
            }
        }
        Ok(messages)
    }

    fn replace_mentions(
        &self,
        text: &str,
        names: &mut std::collections::HashMap<String, Option<String>>,
    ) -> Result<String> {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(at) = rest.find('@') {
            out.push_str(&rest[..at]);
            let after = &rest[at + 1..];
            let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
            if digits.len() < 6 {
                out.push('@');
                rest = after;
                continue;
            }
            if !names.contains_key(&digits) {
                let name = self.mention_name(&digits)?;
                names.insert(digits.clone(), name);
            }
            match &names[&digits] {
                Some(name) => {
                    out.push('@');
                    out.push_str(name);
                }
                None => {
                    out.push('@');
                    out.push_str(&digits);
                }
            }
            rest = &after[digits.len()..];
        }
        out.push_str(rest);
        Ok(out)
    }

    /// A mentioned number's contact name: as a privacy id (`@lid`) mapped to
    /// a phone number, or as a phone number.
    fn mention_name(&self, digits: &str) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        for id in [format!("{digits}@lid"), format!("{digits}@s.whatsapp.net")] {
            let name: Option<Option<String>> = self
                .connection
                .query_row(
                    "SELECT COALESCE(NULLIF(ct.full_name, ''), NULLIF(ct.push_name, ''))
                     FROM (SELECT ?1 AS id) x
                     LEFT JOIN lids l ON l.lid = x.id
                     LEFT JOIN contacts ct ON ct.id = COALESCE(l.pn, x.id)",
                    params![id],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(Some(name)) = name {
                return Ok(Some(name));
            }
        }
        Ok(None)
    }
}

/// A message's attachment as the bridge reports it.
#[derive(Debug, Serialize)]
pub struct BridgeMedia {
    pub kind: String,
    pub mime: String,
    pub size: u64,
    /// The downloaded file, when it is on this computer.
    pub path: Option<std::path::PathBuf>,
    pub caption: Option<String>,
    /// A document's own name.
    pub file_name: Option<String>,
}

impl Archive {
    /// What was read out of one message of an unlocked chat, if anything.
    pub fn bridge_transcript(&self, chat: &str, id: &str) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        self.connection
            .query_row(
                "SELECT t.text FROM transcripts t JOIN chats c ON c.id = t.chat
                 WHERE c.locked = 0 AND t.chat = ?1 AND t.id = ?2",
                params![chat, id],
                |row| row.get(0),
            )
            .optional()
    }

    /// The attachment of one message of an unlocked chat, if it has one.
    pub fn bridge_media(&self, chat: &str, id: &str) -> Result<Option<BridgeMedia>> {
        use rusqlite::OptionalExtension;
        let content: Option<String> = self
            .connection
            .query_row(
                "SELECT m.content FROM messages m JOIN chats c ON c.id = m.chat
                 WHERE c.locked = 0 AND m.chat = ?1 AND m.id = ?2",
                params![chat, id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(content) = content else {
            return Ok(None);
        };
        let kind = serde_json::from_str::<serde_json::Value>(&content)
            .ok()
            .and_then(|value| value.get("kind")?.as_str().map(str::to_owned))
            .unwrap_or_default();
        let Ok(parsed) = serde_json::from_str::<Content>(&content) else {
            return Ok(None);
        };
        let caption = match &parsed {
            Content::Image { caption, .. }
            | Content::Video { caption, .. }
            | Content::Document { caption, .. } => caption.clone(),
            _ => None,
        };
        let file_name = match &parsed {
            Content::Document { file_name, .. } => Some(file_name.clone()),
            _ => None,
        };
        Ok(parsed.media().map(|media| BridgeMedia {
            file_name,
            kind,
            mime: media.mime.clone(),
            size: media.size,
            path: media.path.clone().filter(|path| path.is_file()),
            caption,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Delivery, Message};

    fn message(chat: &str, id: &str, timestamp: i64, text: &str) -> Message {
        Message {
            id: id.into(),
            chat: chat.into(),
            sender: "111@s.whatsapp.net".into(),
            sender_name: Some("Ana".into()),
            from_me: false,
            timestamp,
            history_order: None,
            content: Content::text(text.to_owned()),
            status: Delivery::None,
            delivered_at: None,
            read_at: None,
            quoted: None,
            reactions: Vec::new(),
            edited: false,
            mentions: Vec::new(),
            forwarded: false,
            thumbnail: None,
        }
    }

    fn archive() -> Archive {
        let archive = Archive::in_memory().unwrap();
        archive.ensure_chat("g@g.us", "Obra").unwrap();
        archive.ensure_chat("s@g.us", "Secreto").unwrap();
        archive.set_locked("s@g.us", true).unwrap();
        for (chat, id, timestamp, text) in [
            ("g@g.us", "a", 100, "presupuesto listo"),
            ("g@g.us", "b", 200, "falta el pago"),
            ("s@g.us", "c", 150, "no mostrar"),
        ] {
            archive
                .insert_message(&message(chat, id, timestamp, text), None)
                .unwrap();
        }
        archive
    }

    #[test]
    fn locked_chats_stay_out() {
        let archive = archive();
        let chats = archive.bridge_chats().unwrap();
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].name, "Obra");
        assert_eq!(chats[0].messages, 2);
        let all = archive.bridge_messages(None, Some(0), None, 50).unwrap();
        assert_eq!(
            all.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert!(
            archive
                .bridge_search("mostrar", None, 50)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn since_and_latest_read_in_time_order() {
        let archive = archive();
        let since = archive
            .bridge_messages(Some("g@g.us"), Some(150), None, 50)
            .unwrap();
        assert_eq!(since.len(), 1);
        assert_eq!(since[0].text, "falta el pago");
        assert_eq!(since[0].sender_name.as_deref(), Some("Ana"));
        let latest = archive
            .bridge_messages(Some("g@g.us"), None, None, 1)
            .unwrap();
        assert_eq!(latest[0].id, "b");
        let hits = archive.bridge_search("PAGO", None, 50).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chat_name, "Obra");
    }
}
