//! Account sync actors, OAuth, IMAP actions and SMTP sending.
pub mod events;
pub mod imap;
pub mod oauth;
pub mod remote;
pub mod smtp;
pub mod worker;

pub use events::{FetchedMessage, FolderInfo, MessageState, NewMailNotice, SyncEvent, SyncStatus};
pub use smtp::SmtpSettings;
pub use worker::{AccountCommand, AccountConfig, AccountHandle, MailAction, SendRequest};
