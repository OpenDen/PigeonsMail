//! Read/update queries used by the UI: message lists, folder tree, filters and search.
use anyhow::Result;
use pigeonmail_core::{flags, AccountId, LabelId, MessageId};
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, OptionalExtension};

use crate::MailStore;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListScope {
    /// Inbox folders of every account.
    UnifiedInbox,
    Folder(i64),
    Label(LabelId),
    Account(AccountId),
}

#[derive(Debug, Clone)]
pub struct MessageQuery {
    pub scope: ListScope,
    pub unread_only: bool,
    pub starred_only: bool,
    pub attachments_only: bool,
    /// Raw text typed by the user; converted to a safe FTS5 expression internally.
    pub search: Option<String>,
    /// Order by sender domain, then sender address, instead of by date.
    pub group_by_domain: bool,
}

impl MessageQuery {
    pub fn new(scope: ListScope) -> Self {
        Self {
            scope,
            unread_only: false,
            starred_only: false,
            attachments_only: false,
            search: None,
            group_by_domain: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRow {
    pub id: MessageId,
    pub account_id: AccountId,
    pub folder_id: i64,
    pub uid: u32,
    pub sender: String,
    pub recipients: String,
    pub subject: String,
    pub date_ts: i64,
    pub flags: u32,
    pub snippet: String,
}

impl MessageRow {
    pub fn is_read(&self) -> bool {
        self.flags & flags::SEEN != 0
    }
    pub fn is_starred(&self) -> bool {
        self.flags & flags::FLAGGED != 0
    }
    pub fn has_attachment(&self) -> bool {
        self.flags & flags::HAS_ATTACHMENT != 0
    }
}

/// Where a message lives on the server, plus its folder role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageLocation {
    pub id: MessageId,
    pub account_id: AccountId,
    pub folder_id: i64,
    pub folder: String,
    pub folder_role: Option<String>,
    pub uid: u32,
    pub flags: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRow {
    pub id: AccountId,
    pub kind: String,
    pub email: String,
    pub imap_host: String,
    pub smtp_host: String,
    pub auth_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderRow {
    pub id: i64,
    pub account_id: AccountId,
    pub account_email: String,
    pub name: String,
    pub role: Option<String>,
    pub unread: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelRow {
    pub id: LabelId,
    pub name: String,
    pub color: String,
}

/// Turns free text into a safe FTS5 expression: each word becomes a quoted prefix term, so user
/// input can never inject FTS operators or cause a syntax error.
pub fn fts_query_from_user(input: &str) -> Option<String> {
    let terms: Vec<String> = input
        .split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric() || matches!(c, '@' | '.' | '_' | '-'))
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{w}\"*"))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

const SELECT_COLS: &str =
    "m.id, m.account_id, m.folder_id, m.uid, m.sender, m.recipients, m.subject, m.date_ts, m.flags, m.snippet";

/// Lower-cased address of the first sender: the text inside `<...>` when present, otherwise the
/// whole value. Must match `format::sender_address` in the app crate.
const SENDER_ADDR_SQL: &str = "lower(trim(CASE WHEN instr(m.sender, '<') > 0 AND instr(m.sender, '>') > instr(m.sender, '<') \
     THEN substr(m.sender, instr(m.sender, '<') + 1, instr(m.sender, '>') - instr(m.sender, '<') - 1) \
     ELSE m.sender END))";

fn order_by(q: &MessageQuery) -> String {
    if q.group_by_domain {
        let domain = format!("substr({SENDER_ADDR_SQL}, instr({SENDER_ADDR_SQL}, '@') + 1)");
        format!("ORDER BY {domain}, {SENDER_ADDR_SQL}, m.date_ts DESC, m.id DESC")
    } else {
        "ORDER BY m.date_ts DESC, m.id DESC".into()
    }
}

fn build(q: &MessageQuery) -> (String, Vec<Value>) {
    let mut from = String::from("messages m JOIN folders f ON f.id = m.folder_id");
    let mut conds: Vec<String> = Vec::new();
    let mut args: Vec<Value> = Vec::new();

    match &q.scope {
        ListScope::UnifiedInbox => conds.push("f.role = 'inbox'".into()),
        ListScope::Folder(id) => {
            conds.push("m.folder_id = ?".into());
            args.push(Value::Integer(*id));
        }
        ListScope::Label(id) => {
            from.push_str(" JOIN message_labels ml ON ml.message_id = m.id");
            conds.push("ml.label_id = ?".into());
            args.push(Value::Integer(*id));
        }
        ListScope::Account(id) => {
            conds.push("m.account_id = ?".into());
            args.push(Value::Integer(*id));
        }
    }
    if q.unread_only {
        conds.push(format!("(m.flags & {}) = 0", flags::SEEN));
    }
    if q.starred_only {
        conds.push(format!("(m.flags & {}) != 0", flags::FLAGGED));
    }
    if q.attachments_only {
        conds.push(format!("(m.flags & {}) != 0", flags::HAS_ATTACHMENT));
    }
    if let Some(fts) = q.search.as_deref().and_then(fts_query_from_user) {
        from.push_str(" JOIN fts ON fts.rowid = m.id");
        conds.push("fts MATCH ?".into());
        args.push(Value::Text(fts));
    }
    let clause = format!("FROM {from} WHERE {}", conds.join(" AND "));
    (clause, args)
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<MessageRow> {
    Ok(MessageRow {
        id: r.get(0)?,
        account_id: r.get(1)?,
        folder_id: r.get(2)?,
        uid: r.get(3)?,
        sender: r.get(4)?,
        recipients: r.get(5)?,
        subject: r.get(6)?,
        date_ts: r.get(7)?,
        flags: r.get(8)?,
        snippet: r.get(9)?,
    })
}

impl MailStore {
    /// One page of the message list, newest first.
    pub fn query_messages(
        &self,
        q: &MessageQuery,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<MessageRow>> {
        let (clause, mut args) = build(q);
        args.push(Value::Integer(limit as i64));
        args.push(Value::Integer(offset as i64));
        let sql = format!("SELECT {SELECT_COLS} {clause} {} LIMIT ? OFFSET ?", order_by(q));
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(args), row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every message id matching the query, in list order (used for select-all).
    pub fn query_message_ids(&self, q: &MessageQuery) -> Result<Vec<MessageId>> {
        let (clause, args) = build(q);
        let sql = format!("SELECT m.id {clause} {}", order_by(q));
        let mut stmt = self.conn.prepare(&sql)?;
        let ids = stmt
            .query_map(params_from_iter(args), |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids)
    }

    pub fn count_messages(&self, q: &MessageQuery) -> Result<usize> {
        let (clause, args) = build(q);
        let n: i64 = self.conn.query_row(
            &format!("SELECT count(*) {clause}"),
            params_from_iter(args),
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    pub fn get_message(&self, id: MessageId) -> Result<Option<MessageRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {SELECT_COLS} FROM messages m WHERE m.id = ?1"),
                params![id],
                row,
            )
            .optional()?)
    }

    pub fn message_location(&self, id: MessageId) -> Result<Option<MessageLocation>> {
        Ok(self
            .conn
            .query_row(
                "SELECT m.id, m.account_id, m.folder_id, f.name, f.role, m.uid, m.flags
                 FROM messages m JOIN folders f ON f.id = m.folder_id WHERE m.id = ?1",
                params![id],
                |r| {
                    Ok(MessageLocation {
                        id: r.get(0)?,
                        account_id: r.get(1)?,
                        folder_id: r.get(2)?,
                        folder: r.get(3)?,
                        folder_role: r.get(4)?,
                        uid: r.get(5)?,
                        flags: r.get(6)?,
                    })
                },
            )
            .optional()?)
    }

    /// Finds a message by its server identity (used to apply server-side results).
    pub fn find_message(
        &self,
        account_id: AccountId,
        folder: &str,
        uid: u32,
    ) -> Result<Option<MessageId>> {
        Ok(self
            .conn
            .query_row(
                "SELECT m.id FROM messages m JOIN folders f ON f.id = m.folder_id
                 WHERE m.account_id = ?1 AND f.name = ?2 AND m.uid = ?3",
                params![account_id, folder, uid],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Sets and clears flag bits locally. Returns the new flag value.
    pub fn update_flags(&self, id: MessageId, set: u32, clear: u32) -> Result<Option<u32>> {
        self.conn.execute(
            "UPDATE messages SET flags = (flags | ?2) & ~?3 WHERE id = ?1",
            params![id, set, clear],
        )?;
        Ok(self
            .conn
            .query_row(
                "SELECT flags FROM messages WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Removes a message and its cached body and index entry from the local cache.
    pub fn delete_message(&mut self, id: MessageId) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM fts WHERE rowid = ?1", params![id])?;
        tx.execute("DELETE FROM messages WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(())
    }

    /// Updates stored attachment flag/snippet once the full message has been parsed.
    pub fn set_message_preview(
        &self,
        id: MessageId,
        snippet: &str,
        has_attachment: bool,
    ) -> Result<()> {
        let bit = if has_attachment {
            flags::HAS_ATTACHMENT
        } else {
            0
        };
        self.conn.execute(
            "UPDATE messages SET snippet = ?2, flags = (flags & ~?4) | ?3 WHERE id = ?1",
            params![id, snippet, bit, flags::HAS_ATTACHMENT],
        )?;
        Ok(())
    }

    /// All folders with unread counts, grouped by account.
    pub fn folder_tree(&self) -> Result<Vec<FolderRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT f.id, f.account_id, a.email, f.name, f.role,
                    (SELECT count(*) FROM messages m WHERE m.folder_id = f.id AND (m.flags & {}) = 0)
             FROM folders f JOIN accounts a ON a.id = f.account_id
             ORDER BY a.email, CASE f.role
                 WHEN 'inbox' THEN 0 WHEN 'starred' THEN 1 WHEN 'drafts' THEN 2 WHEN 'sent' THEN 3
                 WHEN 'archive' THEN 4 WHEN 'trash' THEN 6 WHEN 'junk' THEN 7 ELSE 5 END, f.name",
            flags::SEEN
        ))?;
        let rows = stmt
            .query_map([], |r| {
                Ok(FolderRow {
                    id: r.get(0)?,
                    account_id: r.get(1)?,
                    account_email: r.get(2)?,
                    name: r.get(3)?,
                    role: r.get(4)?,
                    unread: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn unified_inbox_unread(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            &format!(
                "SELECT count(*) FROM messages m JOIN folders f ON f.id = m.folder_id
                 WHERE f.role = 'inbox' AND (m.flags & {}) = 0",
                flags::SEEN
            ),
            [],
            |r| r.get(0),
        )?)
    }

    pub fn list_accounts(&self) -> Result<Vec<AccountRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, email, imap_host, smtp_host, auth_type FROM accounts ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(AccountRow {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    email: r.get(2)?,
                    imap_host: r.get(3)?,
                    smtp_host: r.get(4)?,
                    auth_type: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Name of a folder by id.
    pub fn folder_name(&self, folder_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT name FROM folders WHERE id = ?1",
                params![folder_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// `(id, name)` of the account's folder with the given role.
    pub fn folder_by_role(
        &self,
        account_id: AccountId,
        role: &str,
    ) -> Result<Option<(i64, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, name FROM folders WHERE account_id = ?1 AND role = ?2 ORDER BY id LIMIT 1",
                params![account_id, role],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NewAccount, NewMessage};
    use pigeonmail_core::{Condition, Rule, RuleAction};
    use zeroize::Zeroizing;

    fn open() -> (tempfile::TempDir, MailStore) {
        let dir = tempfile::tempdir().unwrap();
        let key = Zeroizing::new("cd".repeat(32));
        let store =
            MailStore::open_encrypted(dir.path().join("q.db").to_str().unwrap(), &key).unwrap();
        (dir, store)
    }

    struct Fixture {
        acc1: AccountId,
        acc2: AccountId,
        inbox1: i64,
        inbox2: i64,
        sent1: i64,
    }

    fn msg<'a>(
        acc: AccountId,
        folder: i64,
        uid: u32,
        subject: &'a str,
        date_ts: i64,
        flags: u32,
    ) -> NewMessage<'a> {
        NewMessage {
            account_id: acc,
            folder_id: folder,
            uid,
            msgid: None,
            sender: "alice@example.com",
            recipients: "me@example.com",
            subject,
            date_ts,
            flags,
            snippet: "",
        }
    }

    fn fixture(store: &mut MailStore) -> Fixture {
        let add = |s: &MailStore, email: &str| {
            s.add_account(&NewAccount {
                kind: "imap",
                email,
                imap_host: "i",
                smtp_host: "s",
                auth_type: "password",
            })
            .unwrap()
        };
        let acc1 = add(store, "a@example.com");
        let acc2 = add(store, "b@example.com");
        let inbox1 = store.upsert_folder(acc1, "INBOX", Some("inbox")).unwrap();
        let sent1 = store.upsert_folder(acc1, "Sent", Some("sent")).unwrap();
        let inbox2 = store.upsert_folder(acc2, "INBOX", Some("inbox")).unwrap();
        store
            .upsert_message(&msg(acc1, inbox1, 1, "Invoice March", 100, flags::SEEN))
            .unwrap();
        store
            .upsert_message(&msg(
                acc1,
                inbox1,
                2,
                "Holiday photos",
                300,
                flags::FLAGGED | flags::HAS_ATTACHMENT,
            ))
            .unwrap();
        store
            .upsert_message(&msg(acc2, inbox2, 1, "Invoice April", 200, 0))
            .unwrap();
        store
            .upsert_message(&msg(acc1, sent1, 1, "Sent invoice", 400, flags::SEEN))
            .unwrap();
        Fixture {
            acc1,
            acc2,
            inbox1,
            inbox2,
            sent1,
        }
    }

    #[test]
    fn unified_inbox_merges_accounts_newest_first_and_excludes_other_folders() {
        let (_d, mut store) = open();
        let fx = fixture(&mut store);
        let q = MessageQuery::new(ListScope::UnifiedInbox);
        let subjects: Vec<_> = store
            .query_messages(&q, 0, 10)
            .unwrap()
            .into_iter()
            .map(|m| m.subject)
            .collect();
        assert_eq!(
            subjects,
            ["Holiday photos", "Invoice April", "Invoice March"]
        );
        assert_eq!(store.count_messages(&q).unwrap(), 3);
        assert_eq!(store.unified_inbox_unread().unwrap(), 2);

        let sent = MessageQuery::new(ListScope::Folder(fx.sent1));
        assert_eq!(store.count_messages(&sent).unwrap(), 1);
    }

    #[test]
    fn paging_uses_offset_and_limit() {
        let (_d, mut store) = open();
        fixture(&mut store);
        let q = MessageQuery::new(ListScope::UnifiedInbox);
        let page: Vec<_> = store
            .query_messages(&q, 1, 1)
            .unwrap()
            .into_iter()
            .map(|m| m.subject)
            .collect();
        assert_eq!(page, ["Invoice April"]);
    }

    #[test]
    fn filters_and_search_combine() {
        let (_d, mut store) = open();
        let fx = fixture(&mut store);
        let mut q = MessageQuery::new(ListScope::UnifiedInbox);
        q.unread_only = true;
        assert_eq!(store.count_messages(&q).unwrap(), 2);
        q.unread_only = false;
        q.starred_only = true;
        assert_eq!(
            store.query_messages(&q, 0, 10).unwrap()[0].subject,
            "Holiday photos"
        );
        q.starred_only = false;
        q.attachments_only = true;
        assert_eq!(store.count_messages(&q).unwrap(), 1);

        let mut s = MessageQuery::new(ListScope::UnifiedInbox);
        s.search = Some("invo".into());
        assert_eq!(store.count_messages(&s).unwrap(), 2);
        s.scope = ListScope::Folder(fx.inbox2);
        assert_eq!(store.count_messages(&s).unwrap(), 1);
        s.search = Some("\" OR NEAR( ) * ^".into());
        assert!(store.query_messages(&s, 0, 10).is_ok());
    }

    #[test]
    fn group_by_domain_orders_by_domain_then_address() {
        let (_d, mut store) = open();
        let fx = fixture(&mut store);
        let senders = [
            ("Bob <bob@other.org>", 10),
            ("Info <info@jobs.com>", 20),
            ("alert@jobs.com", 30),
            ("Carol <info@jobs.com>", 40),
        ];
        for (i, (sender, ts)) in senders.iter().enumerate() {
            store
                .upsert_message(&NewMessage {
                    account_id: fx.acc1,
                    folder_id: fx.inbox1,
                    uid: 100 + i as u32,
                    msgid: None,
                    sender,
                    recipients: "me@example.com",
                    subject: "grouped",
                    date_ts: *ts,
                    flags: 0,
                    snippet: "",
                })
                .unwrap();
        }
        let mut q = MessageQuery::new(ListScope::Folder(fx.inbox1));
        q.group_by_domain = true;
        let senders: Vec<String> = store
            .query_messages(&q, 0, 10)
            .unwrap()
            .into_iter()
            .map(|m| m.sender)
            .collect();
        assert_eq!(
            senders,
            [
                "alice@example.com",
                "alice@example.com",
                "alert@jobs.com",
                "Carol <info@jobs.com>",
                "Info <info@jobs.com>",
                "Bob <bob@other.org>",
            ]
        );
        assert_eq!(store.query_message_ids(&q).unwrap().len(), 6);
    }

    #[test]
    fn label_scope_filters_messages() {
        let (_d, mut store) = open();
        let fx = fixture(&mut store);
        let id = store.find_message(fx.acc1, "INBOX", 1).unwrap().unwrap();
        let label = store.create_label("Work", "#3b7ddd").unwrap();
        store.add_message_label(id, label).unwrap();
        let q = MessageQuery::new(ListScope::Label(label));
        assert_eq!(store.count_messages(&q).unwrap(), 1);
        assert_eq!(store.query_messages(&q, 0, 10).unwrap()[0].id, id);
    }

    #[test]
    fn labels_can_be_renamed_toggled_and_deleted() {
        let (_d, mut store) = open();
        let fx = fixture(&mut store);
        let id = store.find_message(fx.acc1, "INBOX", 1).unwrap().unwrap();
        let label = store.create_label("Work", "#3b7ddd").unwrap();
        assert!(store.toggle_message_label(id, label).unwrap());
        assert_eq!(store.message_labels(id).unwrap()[0].name, "Work");
        store.rename_label(label, "Projects").unwrap();
        assert_eq!(store.message_labels(id).unwrap()[0].name, "Projects");
        assert!(!store.toggle_message_label(id, label).unwrap());
        assert!(store.message_labels(id).unwrap().is_empty());
        store.delete_label(label).unwrap();
        assert!(store.list_labels().unwrap().is_empty());
    }

    #[test]
    fn rules_can_be_updated_disabled_and_deleted() {
        let (_d, mut store) = open();
        let fx = fixture(&mut store);
        let mut rule = Rule {
            id: 0,
            name: "Invoices".into(),
            enabled: true,
            priority: 2,
            match_all: true,
            conditions: vec![Condition::SubjectContains("invoice".into())],
            actions: vec![RuleAction::Star],
        };
        let id = store.save_rule(Some(fx.acc1), &rule).unwrap();
        rule.id = id;
        rule.enabled = false;
        store.update_rule(id, Some(fx.acc1), &rule).unwrap();
        assert!(!store.load_all_rules(fx.acc1).unwrap()[0].enabled);
        assert!(store.load_rules(fx.acc1).unwrap().is_empty());
        store.delete_rule(id).unwrap();
        assert!(store.load_all_rules(fx.acc1).unwrap().is_empty());
    }

    #[test]
    fn fts_query_is_sanitised() {
        assert_eq!(
            fts_query_from_user("  foo  bar\"baz ").as_deref(),
            Some("\"foo\"* \"barbaz\"*")
        );
        assert_eq!(fts_query_from_user("\"\" () *"), None);
    }

    #[test]
    fn flags_location_and_deletion() {
        let (_d, mut store) = open();
        let fx = fixture(&mut store);
        let id = store.find_message(fx.acc2, "INBOX", 1).unwrap().unwrap();
        assert_eq!(
            store
                .update_flags(id, flags::SEEN | flags::FLAGGED, 0)
                .unwrap(),
            Some(3)
        );
        assert_eq!(store.update_flags(id, 0, flags::FLAGGED).unwrap(), Some(1));

        let loc = store.message_location(id).unwrap().unwrap();
        assert_eq!(
            (loc.folder.as_str(), loc.folder_role.as_deref(), loc.uid),
            ("INBOX", Some("inbox"), 1)
        );

        store.delete_message(id).unwrap();
        assert!(store.get_message(id).unwrap().is_none());
        assert!(store.search("april", 10).unwrap().is_empty());
    }

    #[test]
    fn folder_reconciliation_updates_flags_and_removes_server_missing_messages() {
        let (_d, mut store) = open();
        let fx = fixture(&mut store);
        let existing = store.find_message(fx.acc1, "INBOX", 1).unwrap().unwrap();
        let removed = store.find_message(fx.acc1, "INBOX", 2).unwrap().unwrap();

        store
            .reconcile_folder_state(fx.acc1, "INBOX", &[(1, flags::FLAGGED)])
            .unwrap();

        assert!(store.get_message(removed).unwrap().is_none());
        assert_eq!(
            store.get_message(existing).unwrap().unwrap().flags,
            flags::FLAGGED
        );
        assert!(store.find_message(fx.acc2, "INBOX", 1).unwrap().is_some());
    }

    #[test]
    fn folder_tree_counts_unread_and_orders_by_role() {
        let (_d, mut store) = open();
        let fx = fixture(&mut store);
        let tree = store.folder_tree().unwrap();
        assert_eq!(tree.len(), 3);
        assert_eq!(tree[0].account_email, "a@example.com");
        assert_eq!(tree[0].role.as_deref(), Some("inbox"));
        assert_eq!(tree[0].unread, 1);
        assert_eq!(tree[1].role.as_deref(), Some("sent"));
        assert_eq!(
            store.folder_by_role(fx.acc1, "sent").unwrap().unwrap().0,
            fx.sent1
        );
        assert_eq!(fx.inbox1, tree[0].id);
    }

    #[test]
    fn preview_updates_snippet_and_attachment_flag() {
        let (_d, mut store) = open();
        let fx = fixture(&mut store);
        let id = store.find_message(fx.acc1, "INBOX", 1).unwrap().unwrap();
        store.set_message_preview(id, "hello there", true).unwrap();
        let m = store.get_message(id).unwrap().unwrap();
        assert_eq!(m.snippet, "hello there");
        assert!(m.has_attachment() && m.is_read());
        store.set_message_preview(id, "x", false).unwrap();
        assert!(!store.get_message(id).unwrap().unwrap().has_attachment());
    }
}
