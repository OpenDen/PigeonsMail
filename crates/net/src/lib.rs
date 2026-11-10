//! Account sync actors, OAuth, IMAP actions and SMTP sending.
pub mod events;
pub mod imap;
pub mod oauth;
pub mod remote;
pub mod worker;

pub use events::{FetchedMessage, FolderInfo, SyncEvent, SyncStatus};
pub use worker::{AccountCommand, AccountConfig, AccountHandle, MailAction};
