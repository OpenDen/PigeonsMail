//! Writes sync events from the account workers into the encrypted store on one dedicated
//! thread, so multiple syncing accounts never contend for the SQLite write lock.
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use mailthin_core::{flags, AccountId, RuleAction};
use mailthin_net::imap::ImapFlag;
use mailthin_net::worker::MailAction;
use mailthin_net::{SyncEvent, SyncStatus};
use mailthin_store::{MailStore, NewMessage};
use tokio::sync::mpsc;

/// Cached message bodies kept on disk before the least recently used are evicted.
pub const MAX_CACHED_BODIES: usize = 300;

/// An action that must also be applied on the IMAP server.
#[derive(Debug, Clone)]
pub struct ServerAction {
    pub account_id: AccountId,
    pub action: MailAction,
}

pub fn now_ts() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Applies one event to the store and returns the server-side actions it triggered.
pub fn apply_event(store: &mut MailStore, event: &SyncEvent) -> Result<Vec<ServerAction>> {
    let mut server = Vec::new();
    match event {
        SyncEvent::Status { .. } | SyncEvent::ActionFailed { .. } => {}
        SyncEvent::Folders { account_id, folders } => {
            for f in folders {
                store.upsert_folder(*account_id, &f.name, f.role.as_deref())?;
            }
        }
        SyncEvent::Messages { account_id, folder, uidvalidity, reset, messages } => {
            let folder_id = store.upsert_folder(*account_id, folder, None)?;
            let stored_last = store.folder_sync_state(*account_id, folder)?.map_or(0, |(_, _, last)| last);
            if *reset {
                store.clear_folder_messages(folder_id)?;
            }

            let mut last_uid = if *reset { 0 } else { stored_last };
            for m in messages {
                let mut bits = m.flags;
                if m.has_attachment {
                    bits |= flags::HAS_ATTACHMENT;
                }
                let mut labels = Vec::new();
                let mut relocate = None;
                for action in &m.rule_actions {
                    match action {
                        RuleAction::Star => {
                            bits |= flags::FLAGGED;
                            server.push(flag_action(*account_id, folder, m.uid, ImapFlag::Flagged));
                        }
                        RuleAction::MarkRead => {
                            bits |= flags::SEEN;
                            server.push(flag_action(*account_id, folder, m.uid, ImapFlag::Seen));
                        }
                        RuleAction::AddLabel(l) => labels.push(*l),
                        RuleAction::Archive if relocate.is_none() => {
                            relocate = store.folder_by_role(*account_id, "archive")?.map(|(_, n)| n);
                        }
                        RuleAction::Delete if relocate.is_none() => {
                            relocate = store.folder_by_role(*account_id, "trash")?.map(|(_, n)| n);
                        }
                        RuleAction::MoveToFolder(id) if relocate.is_none() => {
                            relocate = store.folder_name(*id)?;
                        }
                        _ => {}
                    }
                }

                let id = store.upsert_message(&NewMessage {
                    account_id: *account_id,
                    folder_id,
                    uid: m.uid,
                    msgid: m.msgid.as_deref(),
                    sender: &m.from,
                    recipients: &m.to,
                    subject: &m.subject,
                    date_ts: m.date_ts,
                    flags: bits,
                    snippet: "",
                })?;
                for l in labels {
                    store.add_message_label(id, l)?;
                }
                last_uid = last_uid.max(m.uid);

                if let Some(to) = relocate.filter(|to| to != folder) {
                    server.push(ServerAction {
                        account_id: *account_id,
                        action: MailAction::Move { folder: folder.clone(), to, uid: m.uid },
                    });
                    store.delete_message(id)?;
                }
            }
            store.set_folder_sync_state(folder_id, *uidvalidity, last_uid)?;
        }
        SyncEvent::BodyFetched { account_id, folder, uid, raw } => {
            // The message may have been moved or deleted while the download was in flight.
            if let Some(id) = store.find_message(*account_id, folder, *uid)? {
                let preview = mailthin_render::preview(raw);
                let text = preview.as_ref().map_or("", |p| p.text.as_str());
                store.put_body(id, raw, text, now_ts())?;
                if let Some(p) = &preview {
                    store.set_message_preview(id, &p.snippet, p.has_attachment)?;
                }
                store.evict_old_bodies(MAX_CACHED_BODIES)?;
            }
        }
    }
    Ok(server)
}

fn flag_action(account_id: AccountId, folder: &str, uid: u32, flag: ImapFlag) -> ServerAction {
    ServerAction {
        account_id,
        action: MailAction::SetFlags { folder: folder.to_string(), uid, add: vec![flag], remove: vec![] },
    }
}

fn account_of(event: &SyncEvent) -> AccountId {
    match event {
        SyncEvent::Status { account_id, .. }
        | SyncEvent::Folders { account_id, .. }
        | SyncEvent::Messages { account_id, .. }
        | SyncEvent::BodyFetched { account_id, .. }
        | SyncEvent::ActionFailed { account_id, .. } => *account_id,
    }
}

/// Spawns the writer thread. `on_event` runs after each event has been persisted (for UI updates);
/// server-side actions are forwarded on `server_actions`. The thread ends when all senders drop.
pub fn spawn_store_writer(
    mut store: MailStore,
    mut events: mpsc::Receiver<SyncEvent>,
    server_actions: std::sync::mpsc::Sender<ServerAction>,
    on_event: impl Fn(&SyncEvent) + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while let Some(event) = events.blocking_recv() {
            match apply_event(&mut store, &event) {
                Ok(actions) => {
                    for a in actions {
                        let _ = server_actions.send(a);
                    }
                    on_event(&event);
                }
                Err(e) => {
                    let error = format!("store write failed: {e:#}");
                    on_event(&SyncEvent::Status {
                        account_id: account_of(&event),
                        status: SyncStatus::Error(error.clone()),
                    });
                    if let SyncEvent::BodyFetched { account_id, folder, uid, .. } = &event {
                        on_event(&SyncEvent::ActionFailed {
                            account_id: *account_id,
                            folder: folder.clone(),
                            uid: *uid,
                            error,
                        });
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mailthin_net::{FetchedMessage, FolderInfo};
    use mailthin_store::{ListScope, MessageQuery, NewAccount};
    use zeroize::Zeroizing;

    fn open() -> (tempfile::TempDir, MailStore, AccountId) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.db");
        let key = Zeroizing::new("ab".repeat(32));
        let store = MailStore::open_encrypted(path.to_str().unwrap(), &key).unwrap();
        let acc = store
            .add_account(&NewAccount {
                kind: "imap",
                email: "me@example.com",
                imap_host: "imap.example.com",
                smtp_host: "smtp.example.com",
                auth_type: "password",
            })
            .unwrap();
        (dir, store, acc)
    }

    fn msg(uid: u32, subject: &str, actions: Vec<RuleAction>) -> FetchedMessage {
        FetchedMessage {
            uid,
            msgid: None,
            from: "alice@example.com".into(),
            to: "me@example.com".into(),
            subject: subject.into(),
            date_ts: 1000 + uid as i64,
            flags: 0,
            has_attachment: false,
            rule_actions: actions,
        }
    }

    fn messages(acc: AccountId, uidvalidity: u32, reset: bool, m: Vec<FetchedMessage>) -> SyncEvent {
        SyncEvent::Messages { account_id: acc, folder: "INBOX".into(), uidvalidity, reset, messages: m }
    }

    fn folders(acc: AccountId, list: &[(&str, &str)]) -> SyncEvent {
        SyncEvent::Folders {
            account_id: acc,
            folders: list
                .iter()
                .map(|(n, r)| FolderInfo { name: n.to_string(), role: Some(r.to_string()) })
                .collect(),
        }
    }

    #[test]
    fn stores_folders_messages_and_resume_state() {
        let (_d, mut store, acc) = open();
        apply_event(&mut store, &folders(acc, &[("INBOX", "inbox")])).unwrap();
        apply_event(&mut store, &messages(acc, 42, false, vec![msg(5, "Invoice", vec![]), msg(9, "Hello", vec![])]))
            .unwrap();

        assert_eq!(store.folder_states(acc).unwrap()["INBOX"], (42, 9));
        assert_eq!(store.search("invoice", 10).unwrap().len(), 1);
        let (id, _, _) = store.folder_sync_state(acc, "INBOX").unwrap().unwrap();
        assert_eq!(store.message_count(id).unwrap(), 2);
        // The folder role set by the Folders event is not wiped by Messages events.
        assert_eq!(store.folder_by_role(acc, "inbox").unwrap().unwrap().0, id);
    }

    #[test]
    fn rule_actions_update_flags_and_resolve_server_actions() {
        let (_d, mut store, acc) = open();
        apply_event(&mut store, &folders(acc, &[("INBOX", "inbox"), ("Archive", "archive")])).unwrap();
        let actions = apply_event(
            &mut store,
            &messages(
                acc,
                1,
                false,
                vec![
                    msg(3, "starred", vec![RuleAction::Star, RuleAction::AddLabel(99)]),
                    msg(4, "archived", vec![RuleAction::Archive]),
                    msg(5, "plain", vec![]),
                ],
            ),
        )
        .unwrap();

        assert_eq!(actions.len(), 2);
        assert!(matches!(&actions[0].action, MailAction::SetFlags { uid: 3, add, .. } if add == &[ImapFlag::Flagged]));
        assert!(matches!(&actions[1].action, MailAction::Move { uid: 4, to, .. } if to == "Archive"));

        let q = MessageQuery::new(ListScope::UnifiedInbox);
        let subjects: Vec<_> = store.query_messages(&q, 0, 10).unwrap().into_iter().map(|m| m.subject).collect();
        assert_eq!(subjects, ["plain", "starred"]);
        let starred = store.find_message(acc, "INBOX", 3).unwrap().unwrap();
        assert!(store.get_message(starred).unwrap().unwrap().is_starred());
    }

    #[test]
    fn unresolvable_move_is_ignored_and_keeps_the_message() {
        let (_d, mut store, acc) = open();
        let actions =
            apply_event(&mut store, &messages(acc, 1, false, vec![msg(1, "x", vec![RuleAction::Archive])])).unwrap();
        assert!(actions.is_empty());
        assert!(store.find_message(acc, "INBOX", 1).unwrap().is_some());
    }

    #[test]
    fn body_fetch_caches_body_and_updates_preview() {
        let (_d, mut store, acc) = open();
        apply_event(&mut store, &messages(acc, 1, false, vec![msg(7, "Subject", vec![])])).unwrap();
        let raw = b"Subject: Subject\r\nContent-Type: text/plain\r\n\r\nhello zebra world".to_vec();
        apply_event(
            &mut store,
            &SyncEvent::BodyFetched { account_id: acc, folder: "INBOX".into(), uid: 7, raw: raw.clone() },
        )
        .unwrap();

        let id = store.find_message(acc, "INBOX", 7).unwrap().unwrap();
        assert_eq!(store.get_body(id, 1).unwrap().unwrap(), raw);
        assert_eq!(store.get_message(id).unwrap().unwrap().snippet, "hello zebra world");
        assert_eq!(store.search("zebra", 10).unwrap(), vec![id]);

        // A body for a message that no longer exists is dropped without error.
        apply_event(&mut store, &SyncEvent::BodyFetched { account_id: acc, folder: "INBOX".into(), uid: 99, raw })
            .unwrap();
    }

    #[test]
    fn uidvalidity_reset_clears_old_messages_and_index() {
        let (_d, mut store, acc) = open();
        apply_event(&mut store, &messages(acc, 1, false, vec![msg(1, "oldsubject", vec![])])).unwrap();
        apply_event(&mut store, &messages(acc, 2, true, vec![msg(1, "newsubject", vec![])])).unwrap();

        assert!(store.search("oldsubject", 10).unwrap().is_empty());
        assert_eq!(store.search("newsubject", 10).unwrap().len(), 1);
        assert_eq!(store.folder_states(acc).unwrap()["INBOX"], (2, 1));
    }

    #[test]
    fn writer_thread_persists_events_until_senders_drop() {
        let (dir, store, acc) = open();
        let (tx, rx) = mpsc::channel(8);
        let (ptx, _prx) = std::sync::mpsc::channel();
        let h = spawn_store_writer(store, rx, ptx, |_| {});
        tx.blocking_send(messages(acc, 7, false, vec![msg(2, "threaded", vec![])])).unwrap();
        drop(tx);
        h.join().unwrap();

        let key = Zeroizing::new("ab".repeat(32));
        let store = MailStore::open_encrypted(dir.path().join("m.db").to_str().unwrap(), &key).unwrap();
        assert_eq!(store.folder_states(acc).unwrap()["INBOX"], (7, 2));
    }
}
