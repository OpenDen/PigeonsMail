//! Encrypted SQLCipher persistence, FTS5 search and the lazy body cache.
pub mod keystore;
pub mod legacy;
pub mod queries;

pub use queries::{
    fts_query_from_user, AccountRow, FolderRow, LabelRow, ListScope, MessageLocation, MessageQuery,
    MessageRow,
};

use std::collections::{HashMap, HashSet};

use anyhow::{bail, Context, Result};
use pigeonmail_core::{AccountId, LabelId, MessageId, Rule};
use rusqlite::{params, Connection, OptionalExtension};
use zeroize::Zeroizing;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS accounts (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    email TEXT NOT NULL UNIQUE,
    imap_host TEXT NOT NULL,
    smtp_host TEXT NOT NULL,
    auth_type TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS folders (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    role TEXT,
    uidvalidity INTEGER NOT NULL DEFAULT 0,
    last_uid INTEGER NOT NULL DEFAULT 0,
    UNIQUE(account_id, name)
);

CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    folder_id INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    uid INTEGER NOT NULL,
    msgid TEXT,
    thread_id TEXT,
    sender TEXT NOT NULL,
    recipients TEXT NOT NULL,
    subject TEXT NOT NULL,
    date_ts INTEGER NOT NULL,
    flags INTEGER NOT NULL DEFAULT 0,
    snippet TEXT NOT NULL DEFAULT '',
    body_state INTEGER NOT NULL DEFAULT 0,
    UNIQUE(folder_id, uid)
);
CREATE INDEX IF NOT EXISTS idx_messages_folder_date ON messages(folder_id, date_ts DESC);

CREATE TABLE IF NOT EXISTS bodies (
    message_id INTEGER PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    raw_blob BLOB NOT NULL,
    last_accessed INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS attachments (
    id INTEGER PRIMARY KEY,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    mime TEXT NOT NULL,
    size INTEGER NOT NULL,
    inline_blob BLOB,
    encrypted_path TEXT
);

CREATE TABLE IF NOT EXISTS labels (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    color TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS message_labels (
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    label_id INTEGER NOT NULL REFERENCES labels(id) ON DELETE CASCADE,
    PRIMARY KEY(message_id, label_id)
);

CREATE TABLE IF NOT EXISTS rules (
    id INTEGER PRIMARY KEY,
    account_id INTEGER,
    enabled INTEGER NOT NULL DEFAULT 1,
    priority INTEGER NOT NULL DEFAULT 0,
    rule_json TEXT NOT NULL
);

CREATE VIRTUAL TABLE IF NOT EXISTS fts USING fts5(
    subject,
    sender,
    body_text,
    content='',
    contentless_delete=1
);

-- Reserved for Phase 2 (Calendar)
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL,
    uid TEXT NOT NULL,
    start_ts INTEGER NOT NULL,
    end_ts INTEGER NOT NULL,
    rrule TEXT,
    summary TEXT NOT NULL,
    location TEXT,
    etag TEXT,
    UNIQUE(account_id, uid)
);
";

pub struct NewAccount<'a> {
    pub kind: &'a str,
    pub email: &'a str,
    pub imap_host: &'a str,
    pub smtp_host: &'a str,
    pub auth_type: &'a str,
}

pub struct NewMessage<'a> {
    pub account_id: AccountId,
    pub folder_id: i64,
    pub uid: u32,
    pub msgid: Option<&'a str>,
    pub sender: &'a str,
    pub recipients: &'a str,
    pub subject: &'a str,
    pub date_ts: i64,
    pub flags: u32,
    pub snippet: &'a str,
}

pub struct MailStore {
    conn: Connection,
}

impl MailStore {
    /// Opens (or creates) the database using a raw 256-bit key given as 64 hex characters.
    pub fn open_encrypted(db_path: &str, raw_key_hex: &Zeroizing<String>) -> Result<Self> {
        if raw_key_hex.len() != 64 || !raw_key_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("database key must be 64 hex characters");
        }
        let conn = Connection::open(db_path).context("Failed to open SQLCipher DB")?;

        // The key must be applied before any other statement. Raw-key syntax skips the KDF.
        let pragma = Zeroizing::new(format!("PRAGMA key = \"x'{}'\";", raw_key_hex.as_str()));
        conn.execute_batch(&pragma)?;
        conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
            r.get::<_, i64>(0)
        })
        .context("Cannot read database: wrong key or not a SQLCipher database")?;

        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA temp_store = MEMORY;
             PRAGMA cache_size = -8000;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )?;

        let store = Self { conn };
        store.conn.execute_batch(SCHEMA)?;
        Ok(store)
    }

    pub fn add_account(&self, a: &NewAccount) -> Result<AccountId> {
        self.conn.execute(
            "INSERT INTO accounts (kind, email, imap_host, smtp_host, auth_type)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![a.kind, a.email, a.imap_host, a.smtp_host, a.auth_type],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn update_rule(&self, id: i64, account_id: Option<AccountId>, rule: &Rule) -> Result<()> {
        let json = serde_json::to_string(rule)?;
        self.conn.execute(
            "UPDATE rules SET account_id = ?2, enabled = ?3, priority = ?4, rule_json = ?5 WHERE id = ?1",
            params![id, account_id, rule.enabled, rule.priority, json],
        )?;
        Ok(())
    }

    pub fn delete_rule(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM rules WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Loads enabled and disabled rules for editing, limited to those applicable to the account.
    pub fn load_all_rules(&self, account_id: AccountId) -> Result<Vec<Rule>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, rule_json FROM rules
             WHERE account_id IS NULL OR account_id = ?1
             ORDER BY priority, id",
        )?;
        let rows = stmt
            .query_map(params![account_id], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(id, json)| {
                let mut rule: Rule = serde_json::from_str(&json)?;
                rule.id = id;
                Ok(rule)
            })
            .collect()
    }

    /// Folders, messages and bodies of the account are removed by `ON DELETE CASCADE`.
    pub fn delete_account(&self, id: AccountId) -> Result<()> {
        self.conn
            .execute("DELETE FROM accounts WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn update_account_servers(
        &self,
        id: AccountId,
        imap_host: &str,
        smtp_host: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE accounts SET imap_host = ?2, smtp_host = ?3 WHERE id = ?1",
            params![id, imap_host, smtp_host],
        )?;
        Ok(())
    }

    pub fn upsert_folder(
        &self,
        account_id: AccountId,
        name: &str,
        role: Option<&str>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO folders (account_id, name, role) VALUES (?1, ?2, ?3)
             ON CONFLICT(account_id, name) DO UPDATE SET role = COALESCE(excluded.role, folders.role)",
            params![account_id, name, role],
        )?;
        Ok(self.conn.query_row(
            "SELECT id FROM folders WHERE account_id = ?1 AND name = ?2",
            params![account_id, name],
            |r| r.get(0),
        )?)
    }

    /// Inserts or updates a message header and (re)indexes it for full-text search.
    pub fn upsert_message(&mut self, m: &NewMessage) -> Result<MessageId> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO messages
                (account_id, folder_id, uid, msgid, sender, recipients, subject, date_ts, flags, snippet)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(folder_id, uid) DO UPDATE SET
                msgid = excluded.msgid, sender = excluded.sender, recipients = excluded.recipients,
                subject = excluded.subject, date_ts = excluded.date_ts, flags = excluded.flags,
                snippet = excluded.snippet",
            params![
                m.account_id, m.folder_id, m.uid, m.msgid, m.sender, m.recipients,
                m.subject, m.date_ts, m.flags, m.snippet
            ],
        )?;
        let id: MessageId = tx.query_row(
            "SELECT id FROM messages WHERE folder_id = ?1 AND uid = ?2",
            params![m.folder_id, m.uid],
            |r| r.get(0),
        )?;
        tx.execute("DELETE FROM fts WHERE rowid = ?1", params![id])?;
        tx.execute(
            "INSERT INTO fts (rowid, subject, sender, body_text) VALUES (?1, ?2, ?3, ?4)",
            params![id, m.subject, m.sender, m.snippet],
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// Full-text search; `fts_query` is an FTS5 MATCH expression. Newest messages first.
    pub fn search(&self, fts_query: &str, limit: usize) -> Result<Vec<MessageId>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.id FROM fts JOIN messages m ON m.id = fts.rowid
             WHERE fts MATCH ?1 ORDER BY m.date_ts DESC LIMIT ?2",
        )?;
        let ids = stmt
            .query_map(params![fts_query, limit as i64], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids)
    }

    /// Caches a raw RFC 822 body and adds its text to the search index.
    pub fn put_body(
        &mut self,
        message_id: MessageId,
        raw: &[u8],
        body_text: &str,
        now_ts: i64,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO bodies (message_id, raw_blob, last_accessed) VALUES (?1, ?2, ?3)
             ON CONFLICT(message_id) DO UPDATE SET raw_blob = excluded.raw_blob,
                last_accessed = excluded.last_accessed",
            params![message_id, raw, now_ts],
        )?;
        tx.execute(
            "UPDATE messages SET body_state = 1 WHERE id = ?1",
            params![message_id],
        )?;
        let (subject, sender): (String, String) = tx.query_row(
            "SELECT subject, sender FROM messages WHERE id = ?1",
            params![message_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        tx.execute("DELETE FROM fts WHERE rowid = ?1", params![message_id])?;
        tx.execute(
            "INSERT INTO fts (rowid, subject, sender, body_text) VALUES (?1, ?2, ?3, ?4)",
            params![message_id, subject, sender, body_text],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn remove_message_label(&self, message_id: MessageId, label_id: LabelId) -> Result<()> {
        self.conn.execute(
            "DELETE FROM message_labels WHERE message_id = ?1 AND label_id = ?2",
            params![message_id, label_id],
        )?;
        Ok(())
    }

    pub fn toggle_message_label(&self, message_id: MessageId, label_id: LabelId) -> Result<bool> {
        let changed = self.conn.execute(
            "DELETE FROM message_labels WHERE message_id = ?1 AND label_id = ?2",
            params![message_id, label_id],
        )?;
        if changed != 0 {
            return Ok(false);
        }
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO message_labels (message_id, label_id)
             SELECT ?1, id FROM labels WHERE id = ?2",
            params![message_id, label_id],
        )?;
        if inserted == 0 {
            bail!("The selected label no longer exists");
        }
        Ok(true)
    }

    pub fn message_labels(&self, message_id: MessageId) -> Result<Vec<LabelRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT l.id, l.name, l.color FROM labels l
             JOIN message_labels ml ON ml.label_id = l.id
             WHERE ml.message_id = ?1 ORDER BY l.name COLLATE NOCASE",
        )?;
        let labels = stmt
            .query_map(params![message_id], |r| {
                Ok(LabelRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    color: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(labels)
    }

    pub fn list_labels(&self) -> Result<Vec<LabelRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, color FROM labels ORDER BY name COLLATE NOCASE")?;
        let labels = stmt
            .query_map([], |r| {
                Ok(LabelRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    color: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(labels)
    }

    pub fn create_label(&self, name: &str, color: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO labels (name, color) VALUES (?1, ?2)",
            params![name, color],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn rename_label(&self, label_id: i64, name: &str) -> Result<()> {
        let updated = self.conn.execute(
            "UPDATE labels SET name = ?2 WHERE id = ?1",
            params![label_id, name],
        )?;
        if updated == 0 {
            bail!("The selected label no longer exists");
        }
        Ok(())
    }

    pub fn delete_label(&self, label_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM labels WHERE id = ?1", params![label_id])?;
        Ok(())
    }

    pub fn get_body(&self, message_id: MessageId, now_ts: i64) -> Result<Option<Vec<u8>>> {
        let blob: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT raw_blob FROM bodies WHERE message_id = ?1",
                params![message_id],
                |r| r.get(0),
            )
            .optional()?;
        if blob.is_some() {
            self.conn.execute(
                "UPDATE bodies SET last_accessed = ?2 WHERE message_id = ?1",
                params![message_id, now_ts],
            )?;
        }
        Ok(blob)
    }

    /// Keeps only the `max_cached_bodies` most recently accessed bodies (LRU).
    pub fn evict_old_bodies(&self, max_cached_bodies: usize) -> Result<usize> {
        let removed = self.conn.execute(
            "DELETE FROM bodies WHERE message_id IN (
                SELECT message_id FROM bodies
                ORDER BY last_accessed DESC, message_id DESC
                LIMIT -1 OFFSET ?1
            )",
            params![max_cached_bodies as i64],
        )?;
        self.conn.execute(
            "UPDATE messages SET body_state = 0
             WHERE body_state = 1 AND id NOT IN (SELECT message_id FROM bodies)",
            [],
        )?;
        Ok(removed)
    }

    /// Cached `(id, uidvalidity, last_uid)` for a folder, if known.
    pub fn folder_sync_state(
        &self,
        account_id: AccountId,
        name: &str,
    ) -> Result<Option<(i64, u32, u32)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, uidvalidity, last_uid FROM folders WHERE account_id = ?1 AND name = ?2",
                params![account_id, name],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?)
    }

    /// `(uidvalidity, last_uid)` per folder name, used to resume incremental sync.
    /// Folders that were never synced (`uidvalidity = 0`) are omitted.
    pub fn folder_states(&self, account_id: AccountId) -> Result<HashMap<String, (u32, u32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, uidvalidity, last_uid FROM folders WHERE account_id = ?1 AND uidvalidity != 0",
        )?;
        let rows = stmt
            .query_map(params![account_id], |r| {
                Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?)))
            })?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?;
        Ok(rows)
    }

    pub fn set_folder_sync_state(
        &self,
        folder_id: i64,
        uidvalidity: u32,
        last_uid: u32,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE folders SET uidvalidity = ?2, last_uid = ?3 WHERE id = ?1",
            params![folder_id, uidvalidity, last_uid],
        )?;
        Ok(())
    }

    /// Drops every cached message (and index entry) of a folder, e.g. after UIDVALIDITY changes.
    pub fn clear_folder_messages(&mut self, folder_id: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM fts WHERE rowid IN (SELECT id FROM messages WHERE folder_id = ?1)",
            params![folder_id],
        )?;
        tx.execute(
            "DELETE FROM messages WHERE folder_id = ?1",
            params![folder_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Attaches a label; silently does nothing if the label no longer exists.
    pub fn add_message_label(&self, message_id: MessageId, label_id: LabelId) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO message_labels (message_id, label_id)
             SELECT ?1, id FROM labels WHERE id = ?2",
            params![message_id, label_id],
        )?;
        Ok(())
    }

    pub fn message_count(&self, folder_id: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT count(*) FROM messages WHERE folder_id = ?1",
            params![folder_id],
            |r| r.get(0),
        )?)
    }

    pub fn save_rule(&self, account_id: Option<AccountId>, rule: &Rule) -> Result<i64> {
        let json = serde_json::to_string(rule)?;
        self.conn.execute(
            "INSERT INTO rules (account_id, enabled, priority, rule_json) VALUES (?1, ?2, ?3, ?4)",
            params![account_id, rule.enabled, rule.priority, json],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Enabled rules applying to the account (or to all accounts), ordered by priority.
    pub fn load_rules(&self, account_id: AccountId) -> Result<Vec<Rule>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, rule_json FROM rules
             WHERE enabled = 1 AND (account_id IS NULL OR account_id = ?1)
             ORDER BY priority, id",
        )?;
        let rows = stmt
            .query_map(params![account_id], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(id, json)| {
                let mut rule: Rule = serde_json::from_str(&json)?;
                rule.id = id;
                Ok(rule)
            })
            .collect()
    }

    /// Updates cached flags and removes messages that no longer exist in the server folder.
    /// The caller supplies one folder's current UID/flag snapshot.
    pub fn reconcile_folder_state(
        &mut self,
        account_id: AccountId,
        folder: &str,
        states: &[(u32, u32)],
    ) -> Result<()> {
        let Some(folder_id) = self
            .conn
            .query_row(
                "SELECT id FROM folders WHERE account_id = ?1 AND name = ?2",
                params![account_id, folder],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
        else {
            return Ok(());
        };
        let remote_uids: HashSet<u32> = states.iter().map(|(uid, _)| *uid).collect();
        let tx = self.conn.transaction()?;
        for (uid, flags) in states {
            tx.execute(
                "UPDATE messages SET flags = (?3 | (flags & ?4)) WHERE folder_id = ?1 AND uid = ?2",
                params![folder_id, uid, flags, pigeonmail_core::flags::HAS_ATTACHMENT],
            )?;
        }
        let local = {
            let mut stmt = tx.prepare("SELECT id, uid FROM messages WHERE folder_id = ?1")?;
            let rows = stmt
                .query_map(params![folder_id], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, u32>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        for (id, uid) in local {
            if !remote_uids.contains(&uid) {
                tx.execute("DELETE FROM fts WHERE rowid = ?1", params![id])?;
                tx.execute("DELETE FROM messages WHERE id = ?1", params![id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pigeonmail_core::{Condition, RuleAction};

    const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn key() -> Zeroizing<String> {
        Zeroizing::new(KEY.to_string())
    }

    fn open_mem() -> (tempfile::TempDir, MailStore) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        let store = MailStore::open_encrypted(path.to_str().unwrap(), &key()).unwrap();
        (dir, store)
    }

    fn seed(store: &mut MailStore, uid: u32, subject: &str, date_ts: i64) -> MessageId {
        let acc = store
            .add_account(&NewAccount {
                kind: "imap",
                email: &format!("u{uid}@example.com"),
                imap_host: "imap.example.com",
                smtp_host: "smtp.example.com",
                auth_type: "password",
            })
            .unwrap();
        let folder = store.upsert_folder(acc, "INBOX", Some("inbox")).unwrap();
        store
            .upsert_message(&NewMessage {
                account_id: acc,
                folder_id: folder,
                uid,
                msgid: None,
                sender: "alice@example.com",
                recipients: "me@example.com",
                subject,
                date_ts,
                flags: 0,
                snippet: "hello",
            })
            .unwrap()
    }

    #[test]
    fn database_file_is_encrypted_and_requires_key() {
        let (dir, store) = open_mem();
        drop(store);
        let path = dir.path().join("mail.db");
        let header = std::fs::read(&path).unwrap();
        assert!(!header.starts_with(b"SQLite format 3"));

        let wrong = Zeroizing::new("ff".repeat(32));
        assert!(MailStore::open_encrypted(path.to_str().unwrap(), &wrong).is_err());
        assert!(MailStore::open_encrypted(path.to_str().unwrap(), &key()).is_ok());
    }

    #[test]
    fn rejects_malformed_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.db");
        let bad = Zeroizing::new("x'; DROP TABLE x;--".to_string());
        assert!(MailStore::open_encrypted(path.to_str().unwrap(), &bad).is_err());
    }

    #[test]
    fn deleting_an_account_removes_its_folders_and_messages() {
        let (_d, mut store) = open_mem();
        seed(&mut store, 1, "to be removed", 100);
        assert_eq!(store.message_count(1).unwrap(), 1);

        let account = store.list_accounts().unwrap()[0].id;
        store.delete_account(account).unwrap();

        assert!(store.list_accounts().unwrap().is_empty());
        assert_eq!(store.message_count(1).unwrap(), 0);
    }

    #[test]
    fn fts_finds_subject_and_body_and_reindexes_on_upsert() {
        let (_d, mut store) = open_mem();
        let id = seed(&mut store, 1, "Quarterly invoice", 100);
        assert_eq!(store.search("invoice", 10).unwrap(), vec![id]);
        assert!(store.search("zebra", 10).unwrap().is_empty());

        store
            .put_body(id, b"raw", "the zebra crossing", 200)
            .unwrap();
        assert_eq!(store.search("zebra", 10).unwrap(), vec![id]);
    }

    #[test]
    fn evicts_least_recently_used_bodies() {
        let (_d, mut store) = open_mem();
        let a = seed(&mut store, 1, "a", 1);
        let b = seed(&mut store, 2, "b", 2);
        let c = seed(&mut store, 3, "c", 3);
        store.put_body(a, b"A", "a", 10).unwrap();
        store.put_body(b, b"B", "b", 20).unwrap();
        store.put_body(c, b"C", "c", 30).unwrap();
        store.get_body(a, 40).unwrap(); // touch a => b is now the oldest

        assert_eq!(store.evict_old_bodies(2).unwrap(), 1);
        assert!(store.get_body(b, 50).unwrap().is_none());
        assert!(store.get_body(a, 50).unwrap().is_some());
        assert!(store.get_body(c, 50).unwrap().is_some());
    }

    #[test]
    fn rules_roundtrip_in_priority_order() {
        let (_d, store) = open_mem();
        let mk = |p: i32, name: &str| Rule {
            id: 0,
            name: name.into(),
            enabled: true,
            priority: p,
            match_all: true,
            conditions: vec![Condition::HasAttachment],
            actions: vec![RuleAction::Star],
        };
        store.save_rule(None, &mk(5, "late")).unwrap();
        store.save_rule(None, &mk(1, "early")).unwrap();
        let names: Vec<_> = store
            .load_rules(1)
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(names, ["early", "late"]);
    }
}
