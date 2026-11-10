//! Events and data produced by account workers. Contains no database types so `net`
//! stays independent of `store`.
use mailthin_core::{AccountId, RuleAction};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderInfo {
    pub name: String,
    /// `inbox`, `sent`, `drafts`, `archive`, `trash`, `junk`, `starred` or `None` for custom folders.
    pub role: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FetchedMessage {
    pub uid: u32,
    pub msgid: Option<String>,
    pub from: String,
    pub to: String,
    pub subject: String,
    pub date_ts: i64,
    /// Bit set from `mailthin_core::flags`.
    pub flags: u32,
    pub has_attachment: bool,
    /// Actions triggered by the user's rules when the message was first fetched.
    pub rule_actions: Vec<RuleAction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncStatus {
    Connecting,
    Syncing,
    Idle,
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageState {
    pub uid: u32,
    pub flags: u32,
}

#[derive(Debug, Clone)]
pub enum SyncEvent {
    Status {
        account_id: AccountId,
        status: SyncStatus,
    },
    Folders {
        account_id: AccountId,
        folders: Vec<FolderInfo>,
    },
    /// `reset` means the server's UIDVALIDITY changed and cached messages for the folder are stale.
    Messages {
        account_id: AccountId,
        folder: String,
        uidvalidity: u32,
        reset: bool,
        messages: Vec<FetchedMessage>,
    },
    FolderState {
        account_id: AccountId,
        folder: String,
        states: Vec<MessageState>,
    },
    /// A full raw message downloaded on demand.
    BodyFetched {
        account_id: AccountId,
        folder: String,
        uid: u32,
        raw: Vec<u8>,
    },
    /// A user action (body fetch, flag change, move, delete) failed on the server.
    ActionFailed {
        account_id: AccountId,
        folder: String,
        uid: u32,
        error: String,
    },
    /// The message was accepted by the SMTP server. `copy_error` is set when the Sent copy could not be saved.
    Sent {
        account_id: AccountId,
        subject: String,
        copy_error: Option<String>,
    },
    /// The message was not sent.
    SendFailed {
        account_id: AccountId,
        subject: String,
        error: String,
    },
}
