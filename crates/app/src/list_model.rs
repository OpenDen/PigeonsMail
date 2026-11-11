//! Virtualised message list: only the pages near the viewport are kept in memory, however large
//! the mailbox is. Pages are loaded lazily from the store as Slint asks for rows.
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use mailthin_store::{MailStore, MessageQuery, MessageRow};
use slint::{Model, ModelNotify, ModelTracker};

use crate::format::{avatar_rgb, date_group, display_name, format_list_date, initials};
use crate::MessageItem;

pub const PAGE_SIZE: usize = 50;
const MAX_CACHED_PAGES: usize = 4;
/// How far `position_of` searches (rows) before giving up.
const POSITION_SEARCH_LIMIT: usize = 5_000;

pub struct MessageListModel {
    store: Rc<RefCell<MailStore>>,
    query: RefCell<MessageQuery>,
    count: Cell<usize>,
    /// `(page index, rows)`, most recently used last.
    pages: RefCell<Vec<(usize, Vec<MessageRow>)>>,
    notify: ModelNotify,
}

impl MessageListModel {
    pub fn new(store: Rc<RefCell<MailStore>>, query: MessageQuery) -> Rc<Self> {
        let m = Rc::new(Self {
            store,
            query: RefCell::new(query),
            count: Cell::new(0),
            pages: RefCell::new(Vec::new()),
            notify: ModelNotify::default(),
        });
        m.recount();
        m
    }

    fn recount(&self) {
        let n = self
            .store
            .try_borrow()
            .ok()
            .and_then(|s| s.count_messages(&self.query.borrow()).ok())
            .unwrap_or(0);
        self.count.set(n);
    }

    pub fn set_query(&self, query: MessageQuery) {
        *self.query.borrow_mut() = query;
        self.reload();
    }

    /// Re-reads the count and drops cached pages; use after the store changed.
    pub fn reload(&self) {
        self.pages.borrow_mut().clear();
        self.recount();
        self.notify.reset();
    }

    /// Refreshes a single row (e.g. after its flags changed) without resetting the whole list.
    pub fn invalidate_message(&self, id: i64) {
        let rows: Vec<usize> = self
            .pages
            .borrow()
            .iter()
            .flat_map(|(p, rows)| {
                rows.iter().enumerate().filter(|(_, r)| r.id == id).map(move |(i, _)| p * PAGE_SIZE + i)
            })
            .collect();
        self.pages.borrow_mut().clear();
        for r in rows {
            self.notify.row_changed(r);
        }
    }

    pub fn row_at(&self, index: usize) -> Option<MessageRow> {
        if index >= self.count.get() {
            return None;
        }
        let page = index / PAGE_SIZE;
        let mut pages = self.pages.borrow_mut();
        if let Some(pos) = pages.iter().position(|(p, _)| *p == page) {
            let entry = pages.remove(pos);
            pages.push(entry);
        } else {
            let store = self.store.try_borrow().ok()?;
            let rows = store.query_messages(&self.query.borrow(), page * PAGE_SIZE, PAGE_SIZE).ok()?;
            if pages.len() >= MAX_CACHED_PAGES {
                pages.remove(0);
            }
            pages.push((page, rows));
        }
        pages.last().and_then(|(_, rows)| rows.get(index % PAGE_SIZE).cloned())
    }

    /// Position of a message in the current list (searches the first few thousand rows).
    pub fn position_of(&self, id: i64) -> Option<usize> {
        let store = self.store.try_borrow().ok()?;
        let query = self.query.borrow();
        let mut offset = 0;
        while offset < POSITION_SEARCH_LIMIT.min(self.count.get()) {
            let rows = store.query_messages(&query, offset, PAGE_SIZE).ok()?;
            if rows.is_empty() {
                break;
            }
            if let Some(i) = rows.iter().position(|r| r.id == id) {
                return Some(offset + i);
            }
            offset += PAGE_SIZE;
        }
        None
    }

    pub fn len(&self) -> usize {
        self.count.get()
    }

    pub fn is_empty(&self) -> bool {
        self.count.get() == 0
    }
}

pub fn to_item(r: &MessageRow, now_ts: i64) -> MessageItem {
    let name = display_name(&r.sender);
    let (red, green, blue) = avatar_rgb(&name);
    MessageItem {
        id: r.id as i32,
        initials: initials(&name).into(),
        avatar: slint::Color::from_rgb_u8(red, green, blue),
        sender: name.into(),
        subject: if r.subject.is_empty() { "(no subject)".into() } else { r.subject.as_str().into() },
        snippet: r.snippet.as_str().into(),
        date: format_list_date(r.date_ts, now_ts).into(),
        group: date_group(r.date_ts, now_ts).into(),
        unread: !r.is_read(),
        starred: r.is_starred(),
        has_attachment: r.has_attachment(),
    }
}

impl Model for MessageListModel {
    type Data = MessageItem;

    fn row_count(&self) -> usize {
        self.count.get()
    }

    fn row_data(&self, row: usize) -> Option<MessageItem> {
        self.row_at(row).map(|r| to_item(&r, crate::sync_bridge::now_ts()))
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mailthin_core::flags;
    use mailthin_store::{ListScope, NewAccount, NewMessage};
    use zeroize::Zeroizing;

    fn store_with(n: u32) -> (tempfile::TempDir, Rc<RefCell<MailStore>>) {
        let dir = tempfile::tempdir().unwrap();
        let key = Zeroizing::new("12".repeat(32));
        let mut store = MailStore::open_encrypted(dir.path().join("l.db").to_str().unwrap(), &key).unwrap();
        let acc = store
            .add_account(&NewAccount { kind: "imap", email: "a@b.co", imap_host: "i", smtp_host: "s", auth_type: "p" })
            .unwrap();
        let inbox = store.upsert_folder(acc, "INBOX", Some("inbox")).unwrap();
        for uid in 1..=n {
            let subject = format!("Message {uid}");
            store
                .upsert_message(&NewMessage {
                    account_id: acc,
                    folder_id: inbox,
                    uid,
                    msgid: None,
                    sender: "Alice <a@x.org>",
                    recipients: "me",
                    subject: &subject,
                    date_ts: 1_000 + uid as i64,
                    flags: 0,
                    snippet: "",
                })
                .unwrap();
        }
        (dir, Rc::new(RefCell::new(store)))
    }

    #[test]
    fn serves_rows_newest_first_across_pages_with_bounded_cache() {
        let (_d, store) = store_with(180);
        let model = MessageListModel::new(store, MessageQuery::new(ListScope::UnifiedInbox));
        assert_eq!(model.row_count(), 180);
        assert_eq!(model.row_data(0).unwrap().subject.as_str(), "Message 180");
        assert_eq!(model.row_data(179).unwrap().subject.as_str(), "Message 1");
        assert_eq!(model.row_data(0).unwrap().sender.as_str(), "Alice");
        assert!(model.row_data(180).is_none());

        for row in [0, 60, 120, 170, 10, 70, 130] {
            assert!(model.row_data(row).is_some());
        }
        assert!(model.pages.borrow().len() <= MAX_CACHED_PAGES);
    }

    #[test]
    fn reload_picks_up_changes_and_invalidate_refreshes_a_row() {
        let (_d, store) = store_with(3);
        let model = MessageListModel::new(store.clone(), MessageQuery::new(ListScope::UnifiedInbox));
        let first = model.row_at(0).unwrap();
        assert!(model.row_data(0).unwrap().unread);

        store.borrow().update_flags(first.id, flags::SEEN, 0).unwrap();
        assert!(model.row_data(0).unwrap().unread, "stale until invalidated");
        model.invalidate_message(first.id);
        assert!(!model.row_data(0).unwrap().unread);

        store.borrow_mut().delete_message(first.id).unwrap();
        model.reload();
        assert_eq!(model.row_count(), 2);
    }

    #[test]
    fn position_of_finds_rows_beyond_the_first_page() {
        let (_d, store) = store_with(120);
        let model = MessageListModel::new(store, MessageQuery::new(ListScope::UnifiedInbox));
        let target = model.row_at(75).unwrap().id;
        assert_eq!(model.position_of(target), Some(75));
        assert_eq!(model.position_of(-5), None);
    }
}
