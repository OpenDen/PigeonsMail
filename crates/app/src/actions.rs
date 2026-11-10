//! Decides which server-side action a keyboard/toolbar command maps to.
use anyhow::Result;
use mailthin_core::flags;
use mailthin_net::imap::ImapFlag;
use mailthin_net::worker::MailAction;
use mailthin_store::{MailStore, MessageLocation};

/// Result of planning a command for one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Remove the message from the current folder view and do this on the server.
    Relocate(MailAction),
    Refuse(&'static str),
}

/// Flag bits to set/clear locally and the matching server action for toggling the star.
pub fn plan_toggle_star(loc: &MessageLocation) -> (u32, u32, MailAction) {
    let starred = loc.flags & flags::FLAGGED != 0;
    let (set, clear, add, remove) = if starred {
        (0, flags::FLAGGED, vec![], vec![ImapFlag::Flagged])
    } else {
        (flags::FLAGGED, 0, vec![ImapFlag::Flagged], vec![])
    };
    (set, clear, MailAction::SetFlags { folder: loc.folder.clone(), uid: loc.uid, add, remove })
}

pub fn mark_read_action(loc: &MessageLocation) -> Option<MailAction> {
    (loc.flags & flags::SEEN == 0).then(|| MailAction::SetFlags {
        folder: loc.folder.clone(),
        uid: loc.uid,
        add: vec![ImapFlag::Seen],
        remove: vec![],
    })
}

pub fn plan_archive(store: &MailStore, loc: &MessageLocation) -> Result<Plan> {
    if loc.folder_role.as_deref() == Some("archive") {
        return Ok(Plan::Refuse("Already in Archive"));
    }
    Ok(match store.folder_by_role(loc.account_id, "archive")? {
        Some((_, to)) => Plan::Relocate(MailAction::Move { folder: loc.folder.clone(), to, uid: loc.uid }),
        None => Plan::Refuse("This account has no Archive folder"),
    })
}

/// Deleting moves to Trash; deleting from Trash is permanent. Never expunges when no Trash folder
/// is known, to avoid destroying mail by accident.
pub fn plan_delete(store: &MailStore, loc: &MessageLocation) -> Result<Plan> {
    if loc.folder_role.as_deref() == Some("trash") {
        return Ok(Plan::Relocate(MailAction::Expunge { folder: loc.folder.clone(), uid: loc.uid }));
    }
    Ok(match store.folder_by_role(loc.account_id, "trash")? {
        Some((_, to)) => Plan::Relocate(MailAction::Move { folder: loc.folder.clone(), to, uid: loc.uid }),
        None => Plan::Refuse("This account has no Trash folder; refusing to delete permanently"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mailthin_store::{NewAccount, NewMessage};
    use zeroize::Zeroizing;

    fn setup(with_archive: bool, with_trash: bool) -> (tempfile::TempDir, MailStore, MessageLocation) {
        let dir = tempfile::tempdir().unwrap();
        let key = Zeroizing::new("ef".repeat(32));
        let mut store = MailStore::open_encrypted(dir.path().join("a.db").to_str().unwrap(), &key).unwrap();
        let acc = store
            .add_account(&NewAccount { kind: "imap", email: "a@b.co", imap_host: "i", smtp_host: "s", auth_type: "password" })
            .unwrap();
        let inbox = store.upsert_folder(acc, "INBOX", Some("inbox")).unwrap();
        if with_archive {
            store.upsert_folder(acc, "Archive", Some("archive")).unwrap();
        }
        if with_trash {
            store.upsert_folder(acc, "Trash", Some("trash")).unwrap();
        }
        let id = store
            .upsert_message(&NewMessage {
                account_id: acc,
                folder_id: inbox,
                uid: 5,
                msgid: None,
                sender: "x",
                recipients: "y",
                subject: "s",
                date_ts: 1,
                flags: 0,
                snippet: "",
            })
            .unwrap();
        let loc = store.message_location(id).unwrap().unwrap();
        (dir, store, loc)
    }

    #[test]
    fn star_toggles_both_ways() {
        let (_d, _s, mut loc) = setup(false, false);
        let (set, clear, a) = plan_toggle_star(&loc);
        assert_eq!((set, clear), (flags::FLAGGED, 0));
        assert!(matches!(a, MailAction::SetFlags { add, .. } if add == [ImapFlag::Flagged]));
        loc.flags |= flags::FLAGGED;
        let (set, clear, a) = plan_toggle_star(&loc);
        assert_eq!((set, clear), (0, flags::FLAGGED));
        assert!(matches!(a, MailAction::SetFlags { remove, .. } if remove == [ImapFlag::Flagged]));
    }

    #[test]
    fn mark_read_only_when_unread() {
        let (_d, _s, mut loc) = setup(false, false);
        assert!(mark_read_action(&loc).is_some());
        loc.flags |= flags::SEEN;
        assert!(mark_read_action(&loc).is_none());
    }

    #[test]
    fn archive_moves_to_archive_folder_or_refuses() {
        let (_d, store, loc) = setup(true, false);
        assert_eq!(
            plan_archive(&store, &loc).unwrap(),
            Plan::Relocate(MailAction::Move { folder: "INBOX".into(), to: "Archive".into(), uid: 5 })
        );
        let (_d2, store2, loc2) = setup(false, false);
        assert!(matches!(plan_archive(&store2, &loc2).unwrap(), Plan::Refuse(_)));
        let mut in_archive = loc.clone();
        in_archive.folder_role = Some("archive".into());
        assert!(matches!(plan_archive(&store, &in_archive).unwrap(), Plan::Refuse(_)));
    }

    #[test]
    fn delete_moves_to_trash_and_only_expunges_from_trash() {
        let (_d, store, loc) = setup(false, true);
        assert_eq!(
            plan_delete(&store, &loc).unwrap(),
            Plan::Relocate(MailAction::Move { folder: "INBOX".into(), to: "Trash".into(), uid: 5 })
        );
        let mut in_trash = loc.clone();
        in_trash.folder = "Trash".into();
        in_trash.folder_role = Some("trash".into());
        assert_eq!(
            plan_delete(&store, &in_trash).unwrap(),
            Plan::Relocate(MailAction::Expunge { folder: "Trash".into(), uid: 5 })
        );
        let (_d2, store2, loc2) = setup(false, false);
        assert!(matches!(plan_delete(&store2, &loc2).unwrap(), Plan::Refuse(_)));
    }
}
