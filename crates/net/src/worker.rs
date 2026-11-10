//! One lightweight async task per account: initial sync, then IMAP IDLE (or polling) with
//! commands multiplexed in via `tokio::select!`.
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_imap::extensions::idle::IdleResponse;
use futures::future::BoxFuture;
use mailthin_core::{AccountId, Rule};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::events::{SyncEvent, SyncStatus};
use crate::imap::{self, Caps, Credential, FolderSync, ImapFlag, ImapSession, ImapSettings};

const IDLE_TIMEOUT: Duration = Duration::from_secs(25 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);
const BACKOFF_MIN: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);

/// Called before every (re)connect so OAuth access tokens can be refreshed.
pub type CredentialProvider = Arc<dyn Fn() -> BoxFuture<'static, Result<Credential>> + Send + Sync>;

pub fn static_credential(credential: Credential) -> CredentialProvider {
    Arc::new(move || {
        let c = credential.clone();
        Box::pin(async move { Ok(c) })
    })
}

pub struct AccountConfig {
    pub account_id: AccountId,
    pub settings: ImapSettings,
    pub credentials: CredentialProvider,
    pub rules: Vec<Rule>,
    /// Cached `(uidvalidity, last_uid)` per folder name, from the store.
    pub folder_states: HashMap<String, (u32, u32)>,
    pub sync_window_days: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailAction {
    FetchBody { folder: String, uid: u32 },
    SetFlags { folder: String, uid: u32, add: Vec<ImapFlag>, remove: Vec<ImapFlag> },
    Move { folder: String, to: String, uid: u32 },
    /// Permanently delete (used for messages already in Trash or when no Trash folder exists).
    Expunge { folder: String, uid: u32 },
}

impl MailAction {
    fn target(&self) -> (&str, u32) {
        match self {
            MailAction::FetchBody { folder, uid }
            | MailAction::SetFlags { folder, uid, .. }
            | MailAction::Move { folder, uid, .. }
            | MailAction::Expunge { folder, uid } => (folder, *uid),
        }
    }
}

#[derive(Debug)]
pub enum AccountCommand {
    SetRules(Vec<Rule>),
    SyncNow,
    /// Forget the cached sync position of a folder and re-fetch it (recovery after a failed action).
    Resync { folder: String },
    Action(MailAction),
    Shutdown,
}

pub struct AccountHandle {
    commands: mpsc::Sender<AccountCommand>,
    task: JoinHandle<()>,
}

impl AccountHandle {
    pub async fn send(&self, cmd: AccountCommand) -> Result<()> {
        self.commands.send(cmd).await.map_err(|_| anyhow::anyhow!("account worker has stopped"))
    }

    /// Non-blocking send for callers outside the async runtime (e.g. the UI thread).
    pub fn try_send(&self, cmd: AccountCommand) -> Result<()> {
        self.commands.try_send(cmd).map_err(|e| anyhow::anyhow!("cannot reach account worker: {e}"))
    }

    pub async fn shutdown(self) {
        let _ = self.commands.send(AccountCommand::Shutdown).await;
        let _ = self.task.await;
    }
}

pub fn spawn_account(config: AccountConfig, events: mpsc::Sender<SyncEvent>) -> AccountHandle {
    let (tx, rx) = mpsc::channel(16);
    let worker = Worker {
        account_id: config.account_id,
        settings: config.settings,
        credentials: config.credentials,
        rules: config.rules,
        states: config.folder_states,
        window_secs: config.sync_window_days.max(1) * 86_400,
        events,
        commands: rx,
        backoff: BACKOFF_MIN,
    };
    AccountHandle { commands: tx, task: tokio::spawn(worker.run()) }
}

#[derive(Debug)]
struct ChannelClosed;
impl fmt::Display for ChannelClosed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("event channel closed")
    }
}
impl std::error::Error for ChannelClosed {}

#[derive(Clone, Copy)]
enum Scope {
    Nothing,
    Inbox,
    All,
}

enum Wake {
    Sync(Scope),
    Action(MailAction),
    Shutdown,
}

struct Worker {
    account_id: AccountId,
    settings: ImapSettings,
    credentials: CredentialProvider,
    rules: Vec<Rule>,
    states: HashMap<String, (u32, u32)>,
    window_secs: i64,
    events: mpsc::Sender<SyncEvent>,
    commands: mpsc::Receiver<AccountCommand>,
    backoff: Duration,
}

fn now_ts() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl Worker {
    async fn emit(&self, ev: SyncEvent) -> Result<()> {
        self.events.send(ev).await.map_err(|_| ChannelClosed.into())
    }

    async fn status(&self, status: SyncStatus) -> Result<()> {
        self.emit(SyncEvent::Status { account_id: self.account_id, status }).await
    }

    async fn run(mut self) {
        loop {
            let _ = self.status(SyncStatus::Connecting).await;
            match self.session().await {
                Ok(()) => return,
                Err(e) if e.downcast_ref::<ChannelClosed>().is_some() => return,
                Err(e) => {
                    if self.status(SyncStatus::Error(format!("{e:#}"))).await.is_err() {
                        return;
                    }
                    if !self.wait_before_retry().await {
                        return;
                    }
                }
            }
        }
    }

    /// Returns `false` when the worker should stop.
    async fn wait_before_retry(&mut self) -> bool {
        let sleep = tokio::time::sleep(self.backoff);
        tokio::pin!(sleep);
        self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
        loop {
            tokio::select! {
                _ = &mut sleep => return true,
                cmd = self.commands.recv() => match cmd {
                    None | Some(AccountCommand::Shutdown) => return false,
                    Some(AccountCommand::SetRules(r)) => self.rules = r,
                    Some(AccountCommand::SyncNow) => return true,
                    Some(AccountCommand::Resync { folder }) => {
                        self.states.remove(&folder);
                        return true;
                    }
                    Some(AccountCommand::Action(a)) => {
                        let (folder, uid) = a.target();
                        let ev = SyncEvent::ActionFailed {
                            account_id: self.account_id,
                            folder: folder.to_string(),
                            uid,
                            error: "account is offline; try again once it reconnects".to_string(),
                        };
                        if self.emit(ev).await.is_err() {
                            return false;
                        }
                    }
                },
            }
        }
    }

    /// Runs one connection until it fails (`Err`) or the worker is told to stop (`Ok`).
    async fn session(&mut self) -> Result<()> {
        let credential = (self.credentials)().await.context("cannot obtain credentials")?;
        let mut session = imap::connect(&self.settings, &credential).await?;
        drop(credential);
        self.backoff = BACKOFF_MIN;
        let caps = imap::capabilities(&mut session).await?;

        let mut scope = Scope::All;
        loop {
            if !matches!(scope, Scope::Nothing) {
                self.status(SyncStatus::Syncing).await?;
                match scope {
                    Scope::All => self.sync_all(&mut session).await?,
                    Scope::Inbox => self.sync_one(&mut session, "INBOX").await?,
                    Scope::Nothing => {}
                }
            }
            self.status(SyncStatus::Idle).await?;

            let wake;
            (session, wake) = if caps.idle {
                self.idle(session).await?
            } else {
                let w = self.poll().await;
                (session, w)
            };
            match wake {
                Wake::Shutdown => {
                    let _ = session.logout().await;
                    return Ok(());
                }
                Wake::Sync(s) => scope = s,
                Wake::Action(a) => {
                    self.run_action(&mut session, caps, a).await?;
                    scope = Scope::Nothing;
                }
            }
        }
    }

    async fn run_action(&mut self, session: &mut ImapSession, caps: Caps, action: MailAction) -> Result<()> {
        let (folder, uid) = {
            let (f, u) = action.target();
            (f.to_string(), u)
        };
        let result: Result<Option<Vec<u8>>> = match &action {
            MailAction::FetchBody { folder, uid } => imap::fetch_body(session, folder, *uid).await.map(Some),
            MailAction::SetFlags { folder, uid, add, remove } => {
                imap::set_flags(session, folder, *uid, add, remove).await.map(|()| None)
            }
            MailAction::Move { folder, to, uid } => {
                imap::move_message(session, folder, to, *uid, caps).await.map(|()| None)
            }
            MailAction::Expunge { folder, uid } => {
                imap::expunge_message(session, folder, *uid, caps).await.map(|()| None)
            }
        };
        match result {
            Ok(Some(raw)) => {
                self.emit(SyncEvent::BodyFetched { account_id: self.account_id, folder, uid, raw }).await
            }
            Ok(None) => Ok(()),
            Err(e) => {
                self.emit(SyncEvent::ActionFailed {
                    account_id: self.account_id,
                    folder,
                    uid,
                    error: format!("{e:#}"),
                })
                .await
            }
        }
    }

    async fn sync_all(&mut self, session: &mut ImapSession) -> Result<()> {
        let folders = imap::list_folders(session).await?;
        let names: Vec<String> = folders.iter().map(|f| f.name.clone()).collect();
        self.emit(SyncEvent::Folders { account_id: self.account_id, folders }).await?;
        for name in names {
            self.sync_one(session, &name).await?;
        }
        Ok(())
    }

    async fn sync_one(&mut self, session: &mut ImapSession, folder: &str) -> Result<()> {
        let known = self.states.get(folder).copied();
        let now = now_ts();
        let result = imap::sync_folder(session, folder, known, now - self.window_secs, now, &self.rules).await;
        let FolderSync { uidvalidity, reset, messages } = match result {
            Ok(r) => r,
            // A folder-level failure (e.g. no permission) must not stop the other folders.
            Err(e) => {
                self.status(SyncStatus::Error(format!("{folder}: {e:#}"))).await?;
                return Ok(());
            }
        };

        let base = if reset { 0 } else { known.map_or(0, |(_, last)| last) };
        let last_uid = messages.iter().map(|m| m.uid).max().unwrap_or(0).max(base);
        let changed = known != Some((uidvalidity, last_uid));
        self.states.insert(folder.to_string(), (uidvalidity, last_uid));

        if changed || reset || !messages.is_empty() {
            self.emit(SyncEvent::Messages {
                account_id: self.account_id,
                folder: folder.to_string(),
                uidvalidity,
                reset,
                messages,
            })
            .await?;
        }
        Ok(())
    }

    async fn poll(&mut self) -> Wake {
        let sleep = tokio::time::sleep(POLL_INTERVAL);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => return Wake::Sync(Scope::All),
                cmd = self.commands.recv() => match self.handle(cmd) {
                    Some(w) => return w,
                    None => continue,
                },
            }
        }
    }

    /// Applies a command; returns a wake-up reason if it should end the current wait.
    fn handle(&mut self, cmd: Option<AccountCommand>) -> Option<Wake> {
        match cmd {
            None | Some(AccountCommand::Shutdown) => Some(Wake::Shutdown),
            Some(AccountCommand::SyncNow) => Some(Wake::Sync(Scope::All)),
            Some(AccountCommand::Resync { folder }) => {
                self.states.remove(&folder);
                Some(Wake::Sync(Scope::All))
            }
            Some(AccountCommand::Action(a)) => Some(Wake::Action(a)),
            Some(AccountCommand::SetRules(r)) => {
                self.rules = r;
                None
            }
        }
    }

    async fn idle(&mut self, mut session: ImapSession) -> Result<(ImapSession, Wake)> {
        session.select("INBOX").await.context("cannot select INBOX for IDLE")?;
        let mut handle = session.idle();
        handle.init().await?;

        let wake = {
            let (wait, interrupt) = handle.wait_with_timeout(IDLE_TIMEOUT);
            tokio::pin!(wait);
            loop {
                tokio::select! {
                    r = &mut wait => {
                        drop(interrupt);
                        break match r? {
                            IdleResponse::NewData(_) => Wake::Sync(Scope::Inbox),
                            IdleResponse::Timeout => Wake::Sync(Scope::All),
                            IdleResponse::ManualInterrupt => Wake::Sync(Scope::Nothing),
                        };
                    }
                    cmd = self.commands.recv() => {
                        if let Some(w) = self.handle(cmd) {
                            drop(interrupt);
                            let _ = (&mut wait).await;
                            break w;
                        }
                    }
                }
            }
        };
        let session = handle.done().await?;
        Ok((session, wake))
    }
}
