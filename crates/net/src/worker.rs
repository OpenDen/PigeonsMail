//! Per-account sync and on-demand body tasks: initial sync, then IMAP IDLE (or polling),
//! with interactive message downloads on a separate connection.
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_imap::extensions::idle::IdleResponse;
use futures::future::BoxFuture;
use mailthin_core::{AccountId, OutgoingMail, Rule};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::events::{SyncEvent, SyncStatus};
use crate::imap::{self, Caps, Credential, FolderSync, ImapFlag, ImapSession, ImapSettings};
use crate::smtp::{self, SmtpSettings};

const IDLE_TIMEOUT: Duration = Duration::from_secs(25 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);
const BACKOFF_MIN: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);
const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const BODY_FETCH_TIMEOUT: Duration = Duration::from_secs(60);

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
    pub smtp: SmtpSettings,
    pub credentials: CredentialProvider,
    pub rules: Vec<Rule>,
    /// Cached `(uidvalidity, last_uid)` per folder name, from the store.
    pub folder_states: HashMap<String, (u32, u32)>,
    pub sync_window_days: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailAction {
    FetchBody {
        folder: String,
        uid: u32,
    },
    SetFlags {
        folder: String,
        uid: u32,
        add: Vec<ImapFlag>,
        remove: Vec<ImapFlag>,
    },
    Move {
        folder: String,
        to: String,
        uid: u32,
    },
    /// Permanently delete (used for messages already in Trash or when no Trash folder exists).
    Expunge {
        folder: String,
        uid: u32,
    },
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
    Resync {
        folder: String,
    },
    Action(MailAction),
    Shutdown,
}

/// An outgoing message for the account's send task.
#[derive(Debug)]
pub struct SendRequest {
    pub mail: OutgoingMail,
    /// Folder that receives a copy of the sent message (usually Sent), if the account has one.
    pub sent_folder: Option<String>,
}

pub struct AccountHandle {
    commands: mpsc::Sender<AccountCommand>,
    body_commands: mpsc::Sender<(String, u32)>,
    send_commands: mpsc::Sender<SendRequest>,
    task: JoinHandle<()>,
    body_task: JoinHandle<()>,
    send_task: JoinHandle<()>,
}

impl AccountHandle {
    pub async fn send(&self, cmd: AccountCommand) -> Result<()> {
        match cmd {
            AccountCommand::Action(MailAction::FetchBody { folder, uid }) => self
                .body_commands
                .send((folder, uid))
                .await
                .map_err(|_| anyhow::anyhow!("message download worker has stopped")),
            other => self
                .commands
                .send(other)
                .await
                .map_err(|_| anyhow::anyhow!("account worker has stopped")),
        }
    }

    /// Non-blocking send for callers outside the async runtime (e.g. the UI thread).
    pub fn try_send(&self, cmd: AccountCommand) -> Result<()> {
        match cmd {
            AccountCommand::Action(MailAction::FetchBody { folder, uid }) => self
                .body_commands
                .try_send((folder, uid))
                .map_err(|e| anyhow::anyhow!("cannot queue message download: {e}")),
            other => self
                .commands
                .try_send(other)
                .map_err(|e| anyhow::anyhow!("cannot reach account worker: {e}")),
        }
    }

    /// Queues an outgoing message. Sending runs on its own task, so it never waits for sync.
    pub fn try_send_mail(&self, request: SendRequest) -> Result<()> {
        self.send_commands
            .try_send(request)
            .map_err(|e| anyhow::anyhow!("cannot queue message for sending: {e}"))
    }

    pub async fn shutdown(self) {
        self.body_task.abort();
        let _ = self.commands.send(AccountCommand::Shutdown).await;
        let _ = self.task.await;
        let _ = self.body_task.await;
        // Dropping the sender lets the send task finish any message already accepted.
        drop(self.send_commands);
        let _ = self.send_task.await;
    }
}

pub fn spawn_account(config: AccountConfig, events: mpsc::Sender<SyncEvent>) -> AccountHandle {
    let (tx, rx) = mpsc::channel(16);
    let (body_tx, body_rx) = mpsc::channel(16);
    let (send_tx, send_rx) = mpsc::channel(8);
    let body_task = tokio::spawn(body_worker(
        config.account_id,
        config.settings.clone(),
        config.credentials.clone(),
        events.clone(),
        body_rx,
    ));
    let send_task = tokio::spawn(send_worker(
        config.account_id,
        config.settings.clone(),
        config.smtp,
        config.credentials.clone(),
        events.clone(),
        send_rx,
    ));
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
    AccountHandle {
        commands: tx,
        body_commands: body_tx,
        send_commands: send_tx,
        task: tokio::spawn(worker.run()),
        body_task,
        send_task,
    }
}

/// Sends queued messages one at a time. Each send opens its own SMTP connection and, when a Sent
/// folder is known, a short IMAP connection to file the copy, so no session is kept idle.
async fn send_worker(
    account_id: AccountId,
    settings: ImapSettings,
    smtp_settings: SmtpSettings,
    credentials: CredentialProvider,
    events: mpsc::Sender<SyncEvent>,
    mut requests: mpsc::Receiver<SendRequest>,
) {
    while let Some(request) = requests.recv().await {
        let subject = request.mail.subject.clone();
        let event = match deliver(&settings, &smtp_settings, &credentials, &request).await {
            Ok(copy_error) => SyncEvent::Sent {
                account_id,
                subject,
                copy_error,
            },
            Err(e) => SyncEvent::SendFailed {
                account_id,
                subject,
                error: format!("{e:#}"),
            },
        };
        if events.send(event).await.is_err() {
            break;
        }
    }
}

/// Returns `Ok(copy_error)`: the message was sent, and `copy_error` says whether the Sent copy failed.
async fn deliver(
    settings: &ImapSettings,
    smtp_settings: &SmtpSettings,
    credentials: &CredentialProvider,
    request: &SendRequest,
) -> Result<Option<String>> {
    let credential = credentials().await.context("cannot obtain credentials")?;
    let raw = smtp::send(
        smtp_settings,
        &credential,
        &smtp_settings.username,
        &request.mail,
    )
    .await?;
    let Some(folder) = &request.sent_folder else {
        return Ok(None);
    };
    let copy: Result<()> = async {
        let mut session = imap::connect(settings, &credential).await?;
        imap::append_message(&mut session, folder, &raw).await?;
        let _ = session.logout().await;
        Ok(())
    }
    .await;
    Ok(copy.err().map(|e| format!("{e:#}")))
}

/// A separate, lazily connected IMAP session keeps interactive downloads independent of sync.
async fn body_worker(
    account_id: AccountId,
    settings: ImapSettings,
    credentials: CredentialProvider,
    events: mpsc::Sender<SyncEvent>,
    mut commands: mpsc::Receiver<(String, u32)>,
) {
    let mut session: Option<ImapSession> = None;
    let mut selected: Option<String> = None;
    loop {
        let next = if session.is_some() {
            match tokio::time::timeout(BODY_IDLE_TIMEOUT, commands.recv()).await {
                Ok(next) => next,
                Err(_) => {
                    session = None;
                    selected = None;
                    continue;
                }
            }
        } else {
            commands.recv().await
        };
        let Some((folder, uid)) = next else { break };
        let result = tokio::time::timeout(BODY_FETCH_TIMEOUT, async {
            if session.is_none() {
                let credential = credentials().await.context("cannot obtain credentials")?;
                session = Some(imap::connect(&settings, &credential).await?);
            }
            let current = session.as_mut().expect("connected above");
            if selected.as_deref() != Some(folder.as_str()) {
                current
                    .select(&folder)
                    .await
                    .with_context(|| format!("cannot select {folder}"))?;
                selected = Some(folder.clone());
            }
            imap::fetch_selected_body(current, uid).await
        })
        .await;
        let event = match result {
            Ok(Ok(raw)) => SyncEvent::BodyFetched {
                account_id,
                folder,
                uid,
                raw,
            },
            failure => {
                session = None;
                selected = None;
                let error = match failure {
                    Ok(Err(e)) => format!("{e:#}"),
                    Err(_) => "message download timed out".to_string(),
                    Ok(Ok(_)) => unreachable!(),
                };
                SyncEvent::ActionFailed {
                    account_id,
                    folder,
                    uid,
                    error,
                }
            }
        };
        if events.send(event).await.is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod body_worker_tests {
    use super::*;

    #[tokio::test]
    async fn body_requests_bypass_the_sync_queue() {
        let (commands, mut command_rx) = mpsc::channel(1);
        let (body_commands, mut body_rx) = mpsc::channel(1);
        let (send_commands, _send_rx) = mpsc::channel(1);
        let task = tokio::spawn(async {});
        let body_task = tokio::spawn(async {});
        let send_task = tokio::spawn(async {});
        let handle = AccountHandle {
            commands,
            body_commands,
            send_commands,
            task,
            body_task,
            send_task,
        };
        handle.try_send(AccountCommand::SyncNow).unwrap();
        handle
            .try_send(AccountCommand::Action(MailAction::FetchBody {
                folder: "INBOX".into(),
                uid: 42,
            }))
            .unwrap();
        assert!(matches!(
            command_rx.recv().await,
            Some(AccountCommand::SyncNow)
        ));
        assert_eq!(body_rx.recv().await, Some(("INBOX".into(), 42)));
    }

    #[tokio::test]
    async fn body_worker_reports_credential_failure() {
        let credentials: CredentialProvider =
            Arc::new(|| Box::pin(async { Err(anyhow::anyhow!("credentials unavailable")) }));
        let settings = ImapSettings {
            host: "invalid".into(),
            port: 993,
            username: "test".into(),
        };
        let (events, mut event_rx) = mpsc::channel(1);
        let (commands, rx) = mpsc::channel(1);
        let worker = tokio::spawn(body_worker(1, settings, credentials, events, rx));
        commands.send(("INBOX".into(), 7)).await.unwrap();
        match event_rx.recv().await.unwrap() {
            SyncEvent::ActionFailed {
                folder, uid, error, ..
            } => {
                assert_eq!((folder.as_str(), uid), ("INBOX", 7));
                assert!(error.contains("credentials unavailable"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
        drop(commands);
        worker.await.unwrap();
    }
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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Worker {
    async fn emit(&self, ev: SyncEvent) -> Result<()> {
        self.events.send(ev).await.map_err(|_| ChannelClosed.into())
    }

    async fn status(&self, status: SyncStatus) -> Result<()> {
        self.emit(SyncEvent::Status {
            account_id: self.account_id,
            status,
        })
        .await
    }

    async fn run(mut self) {
        loop {
            let _ = self.status(SyncStatus::Connecting).await;
            match self.session().await {
                Ok(()) => return,
                Err(e) if e.downcast_ref::<ChannelClosed>().is_some() => return,
                Err(e) => {
                    if self
                        .status(SyncStatus::Error(format!("{e:#}")))
                        .await
                        .is_err()
                    {
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
        let credential = (self.credentials)()
            .await
            .context("cannot obtain credentials")?;
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

    async fn run_action(
        &mut self,
        session: &mut ImapSession,
        caps: Caps,
        action: MailAction,
    ) -> Result<()> {
        let (folder, uid) = {
            let (f, u) = action.target();
            (f.to_string(), u)
        };
        let result: Result<Option<Vec<u8>>> = match &action {
            MailAction::FetchBody { folder, uid } => {
                imap::fetch_body(session, folder, *uid).await.map(Some)
            }
            MailAction::SetFlags {
                folder,
                uid,
                add,
                remove,
            } => imap::set_flags(session, folder, *uid, add, remove)
                .await
                .map(|()| None),
            MailAction::Move { folder, to, uid } => {
                imap::move_message(session, folder, to, *uid, caps)
                    .await
                    .map(|()| None)
            }
            MailAction::Expunge { folder, uid } => {
                imap::expunge_message(session, folder, *uid, caps)
                    .await
                    .map(|()| None)
            }
        };
        match result {
            Ok(Some(raw)) => {
                self.emit(SyncEvent::BodyFetched {
                    account_id: self.account_id,
                    folder,
                    uid,
                    raw,
                })
                .await
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
        self.emit(SyncEvent::Folders {
            account_id: self.account_id,
            folders,
        })
        .await?;
        for name in names {
            self.sync_one(session, &name).await?;
        }
        Ok(())
    }

    async fn sync_one(&mut self, session: &mut ImapSession, folder: &str) -> Result<()> {
        let known = self.states.get(folder).copied();
        let now = now_ts();
        let result = imap::sync_folder(
            session,
            folder,
            known,
            now - self.window_secs,
            now,
            &self.rules,
        )
        .await;
        let FolderSync {
            uidvalidity,
            reset,
            messages,
        } = match result {
            Ok(r) => r,
            // A folder-level failure (e.g. no permission) must not stop the other folders.
            Err(e) => {
                self.status(SyncStatus::Error(format!("{folder}: {e:#}")))
                    .await?;
                return Ok(());
            }
        };

        let base = if reset {
            0
        } else {
            known.map_or(0, |(_, last)| last)
        };
        let last_uid = messages.iter().map(|m| m.uid).max().unwrap_or(0).max(base);
        let changed = known != Some((uidvalidity, last_uid));
        self.states
            .insert(folder.to_string(), (uidvalidity, last_uid));

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
        let states = imap::folder_state(session, folder).await?;
        self.emit(SyncEvent::FolderState {
            account_id: self.account_id,
            folder: folder.to_string(),
            states,
        })
        .await?;
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
        session
            .select("INBOX")
            .await
            .context("cannot select INBOX for IDLE")?;
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
