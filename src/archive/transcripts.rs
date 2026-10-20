//! Text read out of messages on this computer: the speech in voice messages,
//! audio and videos, and the text in pictures. Kept beside the message, never
//! inside its content, so history replays and edits never lose it.

use std::collections::HashMap;

use rusqlite::params;

use super::{Archive, Result};

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS transcripts (
    chat TEXT NOT NULL,
    id TEXT NOT NULL,
    kind TEXT NOT NULL,
    text TEXT NOT NULL,
    model TEXT NOT NULL,
    created INTEGER NOT NULL,
    PRIMARY KEY (chat, id)
);
CREATE TRIGGER IF NOT EXISTS delete_transcripts AFTER DELETE ON messages BEGIN
    DELETE FROM transcripts WHERE chat = OLD.chat AND id = OLD.id;
END;
";

/// What was read out of a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transcript {
    /// `speech` for sound, `image` for the text in a picture.
    pub kind: String,
    pub text: String,
}

impl Archive {
    pub fn put_transcript(
        &self,
        chat: &str,
        id: &str,
        kind: &str,
        text: &str,
        model: &str,
    ) -> Result<()> {
        self.connection.execute(
            "INSERT INTO transcripts (chat, id, kind, text, model, created)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (chat, id) DO UPDATE SET
                kind = excluded.kind, text = excluded.text,
                model = excluded.model, created = excluded.created",
            params![chat, id, kind, text, model, crate::util::now()],
        )?;
        Ok(())
    }

    pub fn transcript(&self, chat: &str, id: &str) -> Result<Option<Transcript>> {
        use rusqlite::OptionalExtension;
        self.connection
            .query_row(
                "SELECT kind, text FROM transcripts WHERE chat = ?1 AND id = ?2",
                params![chat, id],
                |row| {
                    Ok(Transcript {
                        kind: row.get(0)?,
                        text: row.get(1)?,
                    })
                },
            )
            .optional()
    }

    /// Messages of `chats` since `since` (Unix seconds) with speech, or a
    /// picture when `pictures`, that nothing was read out of yet; newest first.
    pub fn untranscribed(
        &self,
        chats: &[String],
        since: i64,
        pictures: bool,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        if chats.is_empty() {
            return Ok(Vec::new());
        }
        let marks = vec!["?"; chats.len()].join(",");
        let sql = format!(
            "SELECT m.chat, m.id FROM messages m
             LEFT JOIN transcripts t ON t.chat = m.chat AND t.id = m.id
             WHERE t.id IS NULL AND m.timestamp >= ? AND m.chat IN ({marks})
               AND json_valid(m.content)
               AND (json_extract(m.content, '$.kind') = 'audio'
                    OR (json_extract(m.content, '$.kind') = 'video'
                        AND coalesce(json_extract(m.content, '$.gif'), 0) = 0)
                    OR (? AND json_extract(m.content, '$.kind') = 'image'))
             ORDER BY m.timestamp DESC LIMIT ?"
        );
        let mut values: Vec<rusqlite::types::Value> = vec![since.into()];
        values.extend(chats.iter().map(|chat| chat.clone().into()));
        values.push((pictures as i64).into());
        values.push((limit as i64).into());
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map(rusqlite::params_from_iter(values), |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        rows.collect()
    }

    /// The transcripts of some of a chat's messages, by message id.
    pub fn transcripts_for(&self, chat: &str, ids: &[&str]) -> Result<HashMap<String, Transcript>> {
        let mut found = HashMap::new();
        let mut statement = self
            .connection
            .prepare_cached("SELECT kind, text FROM transcripts WHERE chat = ?1 AND id = ?2")?;
        for id in ids {
            let mut rows = statement.query(params![chat, id])?;
            if let Some(row) = rows.next()? {
                found.insert(
                    (*id).to_owned(),
                    Transcript {
                        kind: row.get(0)?,
                        text: row.get(1)?,
                    },
                );
            }
        }
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transcript_is_kept_replaced_and_dropped_with_its_message() {
        let archive = Archive::in_memory().unwrap();
        archive.ensure_chat("c@g.us", "Obra").unwrap();
        let message = crate::archive::tests::message("c@g.us", "a", 10, false);
        archive.insert_message(&message, None).unwrap();
        archive
            .put_transcript("c@g.us", "a", "speech", "hola", "turbo")
            .unwrap();
        archive
            .put_transcript("c@g.us", "a", "speech", "hola, ¿cómo va?", "turbo")
            .unwrap();
        assert_eq!(
            archive.transcript("c@g.us", "a").unwrap().unwrap().text,
            "hola, ¿cómo va?"
        );
        assert_eq!(
            archive
                .transcripts_for("c@g.us", &["a", "b"])
                .unwrap()
                .len(),
            1
        );
        archive.delete_message("c@g.us", "a").unwrap();
        assert!(archive.transcript("c@g.us", "a").unwrap().is_none());
    }
}
