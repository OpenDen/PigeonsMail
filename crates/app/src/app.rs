//! Application wiring: store, account workers, message list, reading pane and actions.
//!
//! All UI state lives on the UI thread in [`App`]. Background work (sync workers, the store
//! writer thread, remote image downloads) reports back through a channel of [`UiMsg`] that a
//! timer drains on the UI thread.
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::rc::{Rc, Weak as RcWeak};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::future::BoxFuture;
use futures::StreamExt;
use mailthin_core::compose::{Draft, Original};
use mailthin_core::{
    evaluate_rules, flags, AccountId, Condition, MessageId, MessageSummary, OutgoingMail, Rule,
    RuleAction,
};
use mailthin_net::imap::{Credential, ImapSettings};
use mailthin_net::oauth::{self, OAuthProvider};
use mailthin_net::remote::fetch_remote_image;
use mailthin_net::worker::{
    spawn_account, static_credential, AccountCommand, AccountConfig, AccountHandle,
    CredentialProvider, MailAction,
};
use mailthin_net::SendRequest;
use mailthin_net::SmtpSettings;
use mailthin_net::{SyncEvent, SyncStatus};
use mailthin_render::{extract_attachment, parse_email, quote_source, EmailView, ParsedEmail};
use mailthin_store::{
    keystore, AccountRow, ListScope, MailStore, MessageLocation, MessageQuery, NewAccount,
};
use slint::{
    ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, Timer, TimerMode,
    VecModel,
};
use tokio::sync::mpsc;
use zeroize::Zeroizing;

use crate::actions::{self, Plan};
use crate::compose::{self as glue, ComposeState, ReplyKind};
use crate::format::{
    display_name, folder_icon, folder_title, format_full_date, human_size, initials, looks_like_email,
    provider_defaults, split_host_port, ServerDefaults, avatar_rgb,
};
use crate::image_cache::{self, ImageCache};
use crate::list_model::MessageListModel;
use crate::memory;
use crate::sync_bridge::{now_ts, spawn_store_writer, ServerAction};
use crate::{
    AttachmentChip, ComposeAttachment, MainWindow, ManageLabel, ManageRule, MessageItem,
    SidebarItem,
};

const POLL_INTERVAL: Duration = Duration::from_millis(150);
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(120);
/// Delay before trimming, so Slint has dropped the closed message's textures first.
const TRIM_AFTER_CLOSE: Duration = Duration::from_millis(500);
const IDLE_TRIM_INTERVAL: Duration = Duration::from_secs(120);
const SYNC_WINDOW_DAYS: i64 = 30;
/// Maximum total encoded remote image bytes loaded for one message.
const MAX_REMOTE_BYTES: usize = 16 * 1024 * 1024;
const MAX_REMOTE_IMAGES: usize = 24;
/// Pointer travel (in pixels) before a press becomes a text selection instead of a click.
const DRAG_THRESHOLD_PX: f32 = 4.0;

/// Messages sent from background threads to the UI thread.
enum UiMsg {
    Status {
        account_id: AccountId,
        status: SyncStatus,
    },
    FoldersChanged,
    MessagesChanged,
    BodyReady {
        account_id: AccountId,
        folder: String,
        uid: u32,
    },
    ActionFailed {
        account_id: AccountId,
        folder: String,
        uid: u32,
        error: String,
    },
    Sent {
        subject: String,
        copy_error: Option<String>,
    },
    SendFailed {
        subject: String,
        error: String,
    },
    RemoteImages {
        id: MessageId,
        request: u64,
        images: Vec<(String, Vec<u8>)>,
        failed: bool,
    },
    LoginChecked {
        email: String,
        error: Option<String>,
    },
}

struct RemoteImageBatch {
    limit: usize,
    total: usize,
    images: Vec<(String, Vec<u8>)>,
    failed: bool,
}

impl RemoteImageBatch {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            total: 0,
            images: Vec::new(),
            failed: false,
        }
    }

    fn add(&mut self, result: Result<(String, Vec<u8>)>) {
        match result {
            Ok((url, data)) if data.len() <= self.limit - self.total => {
                self.total += data.len();
                self.images.push((url, data));
            }
            Ok(_) | Err(_) => self.failed = true,
        }
    }
}

/// How a new account signs in. A password stays on the UI thread until the login test passes.
/// OAuth sign-in runs on a background task, which stores the refresh token on success.
enum AddSecret {
    Password(Zeroizing<String>),
    OAuth(&'static OAuthProvider),
}

/// A new account waiting for its login test.
struct PendingAdd {
    email: String,
    secret: AddSecret,
    imap_host: String,
    port: u16,
    smtp_host: String,
}

/// How long the browser sign-in may take before it is abandoned.
const OAUTH_SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_COMPOSE_ATTACHMENT_BYTES: usize = 5 * 1024 * 1024;
const MAX_COMPOSE_ATTACHMENTS: usize = 10;
const MAX_COMPOSE_TOTAL_ATTACHMENT_BYTES: usize = 10 * 1024 * 1024;
const MAX_SAVED_ATTACHMENT_BYTES: usize = 40 * 1024 * 1024;
// Markdown attachments up to this size can be rendered in the reader instead of saved.
const MAX_MARKDOWN_VIEW_BYTES: usize = 1024 * 1024;

fn is_markdown_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".md") || lower.ends_with(".markdown")
}

fn mime_for_name(name: &str) -> String {
    match name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "txt" => "text/plain",
        "html" | "htm" => "text/html",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "csv" => "text/csv",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn parse_rule_conditions(input: &str) -> Result<Vec<Condition>> {
    let mut conditions = Vec::new();
    for item in input.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if item.eq_ignore_ascii_case("attachment") || item.eq_ignore_ascii_case("has-attachment") {
            conditions.push(Condition::HasAttachment);
            continue;
        }
        let Some((kind, value)) = item.split_once(':') else {
            anyhow::bail!("Use from:text, to:text, subject:text, or attachment for a condition");
        };
        let value = value.trim();
        if value.is_empty() {
            anyhow::bail!("A text condition cannot be empty");
        }
        conditions.push(match kind.trim().to_ascii_lowercase().as_str() {
            "from" => Condition::FromContains(value.to_string()),
            "to" => Condition::ToContains(value.to_string()),
            "subject" => Condition::SubjectContains(value.to_string()),
            _ => anyhow::bail!("Unknown rule condition: {kind}"),
        });
    }
    if conditions.is_empty() {
        anyhow::bail!("Add at least one rule condition");
    }
    Ok(conditions)
}

fn parse_rule_actions(
    store: &MailStore,
    account_id: AccountId,
    input: &str,
) -> Result<Vec<RuleAction>> {
    let mut actions = Vec::new();
    for item in input.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (kind, value) = item
            .split_once(':')
            .map_or((item, ""), |(k, v)| (k.trim(), v.trim()));
        actions.push(match kind.to_ascii_lowercase().as_str() {
            "mark-read" | "markread" => RuleAction::MarkRead,
            "star" => RuleAction::Star,
            "archive" => RuleAction::Archive,
            "delete" => RuleAction::Delete,
            "stop" | "stop-processing" => RuleAction::StopProcessing,
            "label" => {
                let label = store
                    .list_labels()?
                    .into_iter()
                    .find(|l| l.name.eq_ignore_ascii_case(value))
                    .ok_or_else(|| {
                        anyhow!("Create the label \"{value}\" before using it in a rule")
                    })?;
                RuleAction::AddLabel(label.id)
            }
            "move" => {
                if value.is_empty() {
                    anyhow::bail!("Enter a destination folder after move:");
                }
                let folder = store
                    .folder_tree()?
                    .into_iter()
                    .find(|f| f.account_id == account_id && f.name.eq_ignore_ascii_case(value))
                    .ok_or_else(|| {
                        anyhow!("No folder named \"{value}\" exists for this account")
                    })?;
                RuleAction::MoveToFolder(folder.id)
            }
            _ => anyhow::bail!("Unknown rule action: {kind}"),
        });
    }
    if actions.is_empty() {
        anyhow::bail!("Add at least one rule action");
    }
    Ok(actions)
}

fn rule_conditions_text(conditions: &[Condition]) -> String {
    conditions
        .iter()
        .map(|condition| match condition {
            Condition::FromContains(value) => format!("from:{value}"),
            Condition::ToContains(value) => format!("to:{value}"),
            Condition::SubjectContains(value) => format!("subject:{value}"),
            Condition::HasAttachment => "attachment".to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn rule_actions_text(store: &MailStore, rule: &Rule) -> Result<String> {
    let labels = store.list_labels()?;
    let mut actions = Vec::with_capacity(rule.actions.len());
    for action in &rule.actions {
        actions.push(match action {
            RuleAction::MarkRead => "mark-read".to_string(),
            RuleAction::Star => "star".to_string(),
            RuleAction::Archive => "archive".to_string(),
            RuleAction::Delete => "delete".to_string(),
            RuleAction::StopProcessing => "stop".to_string(),
            RuleAction::AddLabel(id) => labels
                .iter()
                .find(|l| l.id == *id)
                .map_or_else(|| format!("label:{id}"), |l| format!("label:{}", l.name)),
            RuleAction::MoveToFolder(id) => store
                .folder_name(*id)?
                .map_or_else(|| format!("move:{id}"), |name| format!("move:{name}")),
        });
    }
    Ok(actions.join(", "))
}

fn rule_action_summary(actions: &[RuleAction]) -> String {
    actions
        .iter()
        .map(|action| match action {
            RuleAction::MarkRead => "mark read".to_string(),
            RuleAction::Star => "star".to_string(),
            RuleAction::Archive => "archive".to_string(),
            RuleAction::Delete => "delete".to_string(),
            RuleAction::StopProcessing => "stop".to_string(),
            RuleAction::AddLabel(id) => format!("add label {id}"),
            RuleAction::MoveToFolder(id) => format!("move to folder {id}"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn to_ui_msg(event: &SyncEvent) -> Option<UiMsg> {
    Some(match event {
        SyncEvent::Status { account_id, status } => UiMsg::Status {
            account_id: *account_id,
            status: status.clone(),
        },
        SyncEvent::Folders { .. } => UiMsg::FoldersChanged,
        SyncEvent::Messages { .. } => UiMsg::MessagesChanged,
        SyncEvent::FolderState { .. } => UiMsg::MessagesChanged,
        SyncEvent::BodyFetched {
            account_id,
            folder,
            uid,
            ..
        } => UiMsg::BodyReady {
            account_id: *account_id,
            folder: folder.clone(),
            uid: *uid,
        },
        SyncEvent::ActionFailed {
            account_id,
            folder,
            uid,
            error,
        } => UiMsg::ActionFailed {
            account_id: *account_id,
            folder: folder.clone(),
            uid: *uid,
            error: error.clone(),
        },
        SyncEvent::Sent {
            subject,
            copy_error,
            ..
        } => UiMsg::Sent {
            subject: subject.clone(),
            copy_error: copy_error.clone(),
        },
        SyncEvent::SendFailed { subject, error, .. } => UiMsg::SendFailed {
            subject: subject.clone(),
            error: error.clone(),
        },
    })
}

/// The message currently shown in the reading pane. Dropping it frees the rendered document.
struct Reader {
    loc: MessageLocation,
    awaiting_body: bool,
    view: Option<EmailView>,
    scroll: u32,
    pending_link: Option<String>,
    remote_bytes: usize,
    remote_download: Option<tokio::task::JoinHandle<()>>,
    remote_request: Option<u64>,
    /// Where the current mouse press started, in viewport coordinates.
    press: Option<(f32, f32)>,
    /// Set once the pointer moved far enough from `press` to count as a text selection.
    dragged: bool,
}

type Handles = Arc<Mutex<HashMap<AccountId, AccountHandle>>>;
type Shared = Rc<RefCell<App>>;

struct App {
    ui: slint::Weak<MainWindow>,
    store: Rc<RefCell<MailStore>>,
    model: Rc<MessageListModel>,
    handles: Handles,
    rt: tokio::runtime::Handle,
    events_tx: mpsc::Sender<SyncEvent>,
    ui_tx: std::sync::mpsc::Sender<UiMsg>,
    statuses: BTreeMap<AccountId, (String, SyncStatus)>,
    notice: Option<String>,
    reader: Option<Reader>,
    viewport: (u32, u32),
    link_hover: bool,
    /// Body downloads in flight, so a failure can be told apart from a failed mutation.
    pending_fetches: HashSet<(AccountId, String, u32)>,
    next_remote_request: u64,
    /// On-disk cache of remote images, or `None` if the cache directory could not be created.
    image_cache: Option<ImageCache>,
    /// Returns memory to the OS shortly after a message is closed.
    trim_timer: Timer,
    /// Periodic trim so idle memory is reclaimed even while a message stays open. Held here so
    /// the timer keeps running.
    _idle_trim_timer: Timer,
    resize_timer: Timer,
    /// Hosts last filled in from the typed address, so they can be replaced when the address changes.
    auto_hosts: Option<ServerDefaults>,
    /// The account being tested by a login attempt from the add dialog.
    pending_add: Option<PendingAdd>,
    /// The message currently open in the compose window, with its threading context.
    compose: Option<ComposeState>,
    /// The last message handed to the sender, kept so a failed send can reopen it for editing.
    last_sent: Option<OutgoingMail>,
    last_sent_draft: Option<Draft>,
    compose_attachments: Vec<(String, String, Vec<u8>)>,
    manage_account_id: Option<AccountId>,
    /// Accounts whose folder list is collapsed in the sidebar.
    collapsed_accounts: HashSet<AccountId>,
    self_ref: RcWeak<RefCell<App>>,
}

fn with_app(shared: &Shared, f: impl FnOnce(&mut App)) {
    if let Ok(mut app) = shared.try_borrow_mut() {
        f(&mut app);
    }
}

impl App {
    fn ui(&self) -> Option<MainWindow> {
        self.ui.upgrade()
    }

    fn notify(&mut self, text: impl Into<String>) {
        self.notice = Some(text.into());
        self.update_status();
    }

    fn update_status(&self) {
        let Some(ui) = self.ui() else { return };
        let mut parts: Vec<String> = Vec::new();
        if let Some(n) = &self.notice {
            parts.push(n.clone());
        }
        for (email, status) in self.statuses.values() {
            parts.push(match status {
                SyncStatus::Connecting => format!("{email}: connecting..."),
                SyncStatus::Syncing => format!("{email}: syncing..."),
                SyncStatus::Idle => format!("{email}: up to date"),
                SyncStatus::Error(e) => format!("{email}: {e}"),
            });
        }
        if parts.is_empty() {
            parts.push("No accounts yet. Click Add Account to get started.".into());
        }
        ui.set_status_text(parts.join("   |   ").into());
    }

    // ----------------------------------------------------------------- sidebar and list

    fn refresh_sidebar(&self) {
        let Some(ui) = self.ui() else { return };
        let (accounts, folders, labels, unified) = {
            let s = self.store.borrow();
            (
                s.list_accounts().unwrap_or_default(),
                s.folder_tree().unwrap_or_default(),
                s.list_labels().unwrap_or_default(),
                s.unified_inbox_unread().unwrap_or(0),
            )
        };
        let mut items = vec![SidebarItem {
            kind: 0,
            title: "Unified Inbox".into(),
            unread: unified as i32,
            folder_id: 0,
            account_id: -1,
            expanded: false,
            icon: "stack".into(),
        }];
        for acc in &accounts {
            let expanded = !self.collapsed_accounts.contains(&acc.id);
            items.push(SidebarItem {
                kind: 1,
                title: acc.email.as_str().into(),
                unread: 0,
                folder_id: -1,
                account_id: acc.id as i32,
                expanded,
                icon: "mail".into(),
            });
            if !expanded {
                continue;
            }
            for f in folders.iter().filter(|f| f.account_id == acc.id) {
                items.push(SidebarItem {
                    kind: 2,
                    title: folder_title(&f.name, f.role.as_deref()).into(),
                    unread: f.unread as i32,
                    folder_id: f.id as i32,
                    account_id: -1,
                    expanded: false,
                    icon: folder_icon(f.role.as_deref()).into(),
                });
            }
        }
        for label in &labels {
            items.push(SidebarItem {
                kind: 3,
                title: format!("Label: {}", label.name).into(),
                unread: 0,
                folder_id: -(label.id as i32) - 1,
                account_id: -1,
                expanded: false,
                icon: "tag".into(),
            });
        }
        ui.set_sidebar(ModelRc::new(VecModel::from(items)));
    }

    fn toggle_account(&mut self, account_id: i32) {
        let id = account_id as AccountId;
        if !self.collapsed_accounts.remove(&id) {
            self.collapsed_accounts.insert(id);
        }
        self.refresh_sidebar();
    }

    /// Removes an account, its synced mail, its keyring password, and stops its sync worker.
    fn remove_account(&mut self, account_id: i32) {
        let Some(ui) = self.ui() else { return };
        let Some(row) = self
            .store
            .borrow()
            .list_accounts()
            .ok()
            .and_then(|rows| rows.into_iter().find(|a| a.id as i32 == account_id))
        else {
            return;
        };
        if let Err(e) = self.store.borrow().delete_account(row.id) {
            ui.set_status_text(format!("Could not remove {}: {e:#}", row.email).into());
            return;
        }
        let worker = self.handles.lock().ok().and_then(|mut h| h.remove(&row.id));
        if let Some(worker) = worker {
            self.rt.spawn(worker.shutdown());
        }
        let _ = keystore::delete_account_secret(&row.email);
        self.statuses.remove(&row.id);
        self.pending_fetches.retain(|(id, _, _)| *id != row.id);
        self.refresh_sidebar();
        self.select_folder(0);
        self.update_status();
    }

    fn current_query(ui: &MainWindow) -> MessageQuery {
        let selected = ui.get_selected_folder();
        let scope = if selected > 0 {
            ListScope::Folder(selected as i64)
        } else if selected < -1 {
            ListScope::Label((-selected - 1) as i64)
        } else {
            ListScope::UnifiedInbox
        };
        let mut q = MessageQuery::new(scope);
        q.unread_only = ui.get_filter_unread();
        q.starred_only = ui.get_filter_starred();
        q.attachments_only = ui.get_filter_attachments();
        let text = ui.get_search_text();
        if !text.trim().is_empty() {
            q.search = Some(text.to_string());
        }
        q
    }

    /// Re-runs the current query (after a folder, filter or search change).
    fn apply_query(&self) {
        let Some(ui) = self.ui() else { return };
        self.model.set_query(Self::current_query(&ui));
        ui.set_message_count(self.model.len() as i32);
    }

    /// Re-reads list and sidebar after the store changed underneath them.
    fn refresh_all(&self) {
        let Some(ui) = self.ui() else { return };
        self.model.reload();
        ui.set_message_count(self.model.len() as i32);
        self.refresh_sidebar();
    }

    fn select_folder(&mut self, id: i32) {
        let Some(ui) = self.ui() else { return };
        ui.set_selected_folder(id);
        self.close_reader();
        self.apply_query();
    }

    // ----------------------------------------------------------------- rules, labels and settings

    fn account_for_manage(&self) -> Option<AccountId> {
        if let Some(account_id) = self.reader.as_ref().map(|r| r.loc.account_id) {
            return Some(account_id);
        }
        let selected = self.ui()?.get_selected_folder();
        let store = self.store.borrow();
        if selected > 0 {
            if let Ok(folders) = store.folder_tree() {
                if let Some(folder) = folders.iter().find(|f| f.id == selected as i64) {
                    return Some(folder.account_id);
                }
            }
        }
        store.list_accounts().ok()?.first().map(|a| a.id)
    }

    fn open_manage(&mut self) {
        let Some(account_id) = self.account_for_manage() else {
            self.notify("Add an account before managing rules or server settings.");
            return;
        };
        self.manage_account_id = Some(account_id);
        if let Some(ui) = self.ui() {
            ui.set_manage_mode(0);
            ui.set_show_manage(true);
        }
        self.refresh_manage();
    }

    fn close_manage(&mut self) {
        if let Some(ui) = self.ui() {
            ui.set_show_manage(false);
            ui.set_manage_error("".into());
        }
    }

    fn manage_select_account(&mut self, index: i32) {
        let accounts = match self.store.borrow().list_accounts() {
            Ok(accounts) => accounts,
            Err(e) => {
                if let Some(ui) = self.ui() {
                    ui.set_manage_error(format!("Cannot list accounts: {e:#}").into());
                }
                return;
            }
        };
        if let Some(account) = usize::try_from(index).ok().and_then(|i| accounts.get(i)) {
            self.manage_account_id = Some(account.id);
            self.refresh_manage();
        }
    }

    fn refresh_manage(&mut self) {
        let (Some(ui), Some(account_id)) = (self.ui(), self.manage_account_id) else {
            return;
        };
        let loaded = {
            let store = self.store.borrow();
            (
                store.list_accounts(),
                store.load_all_rules(account_id),
                store.list_labels(),
            )
        };
        let (Ok(accounts), Ok(rules), Ok(labels)) = loaded else {
            ui.set_manage_error("Could not load management data from the encrypted store.".into());
            return;
        };
        let Some(account) = accounts.iter().find(|a| a.id == account_id) else {
            ui.set_manage_error("The selected account no longer exists.".into());
            return;
        };
        let rule_items: Vec<ManageRule> = rules
            .iter()
            .map(|r| ManageRule {
                id: r.id as i32,
                name: r.name.as_str().into(),
                enabled: r.enabled,
            })
            .collect();
        let label_items: Vec<ManageLabel> = labels
            .iter()
            .map(|l| ManageLabel {
                id: l.id as i32,
                name: l.name.as_str().into(),
            })
            .collect();
        let account_emails: Vec<slint::SharedString> = accounts
            .iter()
            .map(|a| a.email.as_str().into())
            .collect();
        let account_index = accounts
            .iter()
            .position(|a| a.id == account_id)
            .map_or(-1, |i| i as i32);
        ui.set_manage_accounts(ModelRc::new(VecModel::from(account_emails)));
        ui.set_manage_account_index(account_index);
        ui.set_manage_account(account.email.as_str().into());
        ui.set_manage_rules(ModelRc::new(VecModel::from(rule_items)));
        ui.set_manage_labels(ModelRc::new(VecModel::from(label_items)));
        ui.set_manage_imap_host(account.imap_host.as_str().into());
        ui.set_manage_smtp_host(account.smtp_host.as_str().into());
        ui.set_manage_error("".into());
    }

    fn select_rule(&mut self, id: i32) {
        let (Some(ui), Some(account_id)) = (self.ui(), self.manage_account_id) else {
            return;
        };
        if id < 0 {
            ui.set_rule_id(-1);
            ui.set_rule_name("".into());
            ui.set_rule_condition("subject:".into());
            ui.set_rule_actions("star".into());
            ui.set_rule_enabled(true);
            ui.set_rule_match_all(true);
            ui.set_rule_preview("".into());
            return;
        }
        let result = self.store.borrow().load_all_rules(account_id);
        let Ok(rules) = result else {
            ui.set_manage_error("Could not load rules.".into());
            return;
        };
        let Some(rule) = rules.iter().find(|r| r.id == id as i64) else {
            ui.set_manage_error("That rule no longer exists.".into());
            return;
        };
        let store = self.store.borrow();
        ui.set_rule_id(id);
        ui.set_rule_name(rule.name.as_str().into());
        ui.set_rule_condition(rule_conditions_text(&rule.conditions).into());
        match rule_actions_text(&store, rule) {
            Ok(actions) => ui.set_rule_actions(actions.into()),
            Err(e) => {
                ui.set_manage_error(format!("Cannot display rule actions: {e:#}").into());
                return;
            }
        }
        ui.set_rule_enabled(rule.enabled);
        ui.set_rule_match_all(rule.match_all);
        ui.set_rule_preview("".into());
        ui.set_manage_error("".into());
    }

    fn rule_from_form(&self, account_id: AccountId) -> Result<Rule> {
        let ui = self
            .ui()
            .ok_or_else(|| anyhow!("the application window is closed"))?;
        let name = ui.get_rule_name().to_string().trim().to_string();
        if name.is_empty() {
            anyhow::bail!("Enter a rule name");
        }
        let id = ui.get_rule_id();
        let existing = self.store.borrow().load_all_rules(account_id)?;
        let priority = existing
            .iter()
            .find(|r| r.id == id as i64)
            .map_or(existing.len() as i32, |r| r.priority);
        let condition_text = ui.get_rule_condition().to_string();
        let action_text = ui.get_rule_actions().to_string();
        let conditions = parse_rule_conditions(&condition_text)?;
        let actions = parse_rule_actions(&self.store.borrow(), account_id, &action_text)?;
        Ok(Rule {
            id: id.max(0) as i64,
            name,
            enabled: ui.get_rule_enabled(),
            priority,
            match_all: ui.get_rule_match_all(),
            conditions,
            actions,
        })
    }

    fn save_rule(&mut self) {
        let (Some(ui), Some(account_id)) = (self.ui(), self.manage_account_id) else {
            return;
        };
        let result: Result<i64> = (|| {
            let rule = self.rule_from_form(account_id)?;
            let store = self.store.borrow();
            if ui.get_rule_id() >= 0 {
                store.update_rule(ui.get_rule_id() as i64, Some(account_id), &rule)?;
                Ok(rule.id)
            } else {
                store.save_rule(Some(account_id), &rule)
            }
        })();
        match result {
            Ok(id) => {
                ui.set_rule_id(id as i32);
                if let Err(e) = self.reload_account_rules(account_id) {
                    ui.set_manage_error(
                        format!("Rule saved, but sync could not reload it: {e:#}").into(),
                    );
                    return;
                }
                self.refresh_manage();
                ui.set_manage_error("Rule saved.".into());
            }
            Err(e) => ui.set_manage_error(format!("{e:#}").into()),
        }
    }

    fn delete_rule(&mut self) {
        let (Some(ui), Some(account_id)) = (self.ui(), self.manage_account_id) else {
            return;
        };
        let id = ui.get_rule_id();
        if id < 0 {
            ui.set_manage_error("Select a saved rule first.".into());
            return;
        }
        let result = self.store.borrow().delete_rule(id as i64);
        match result {
            Ok(()) => {
                if let Err(e) = self.reload_account_rules(account_id) {
                    ui.set_manage_error(
                        format!("Rule deleted, but sync could not reload it: {e:#}").into(),
                    );
                    return;
                }
                self.select_rule(-1);
                self.refresh_manage();
            }
            Err(e) => ui.set_manage_error(format!("Could not delete rule: {e:#}").into()),
        }
    }

    fn reload_account_rules(&mut self, account_id: AccountId) -> Result<()> {
        let rules = self.store.borrow().load_rules(account_id)?;
        self.send_command(account_id, AccountCommand::SetRules(rules));
        Ok(())
    }

    fn dry_run_rules(&mut self) {
        let (Some(ui), Some(account_id)) = (self.ui(), self.manage_account_id) else {
            return;
        };
        let result: Result<String> = (|| {
            let draft = self.rule_from_form(account_id)?;
            let store = self.store.borrow();
            let mut rules = store.load_all_rules(account_id)?;
            if ui.get_rule_id() >= 0 {
                if let Some(existing) = rules.iter_mut().find(|r| r.id == ui.get_rule_id() as i64) {
                    *existing = draft;
                } else {
                    rules.push(draft);
                }
            } else {
                rules.push(draft);
            }
            let query = MessageQuery::new(ListScope::Account(account_id));
            let messages = store.query_messages(&query, 0, 50)?;
            let mut output = Vec::new();
            for row in messages {
                let is_read = row.is_read();
                let is_starred = row.is_starred();
                let has_attachment = row.has_attachment();
                let summary = MessageSummary {
                    id: row.id,
                    account_id: row.account_id,
                    folder_id: row.folder_id,
                    uid: row.uid,
                    from: row.sender,
                    to: row.recipients,
                    subject: row.subject,
                    date_ts: row.date_ts,
                    snippet: row.snippet,
                    is_read,
                    is_starred,
                    has_attachment,
                };
                let actions = evaluate_rules(&rules, &summary);
                if !actions.is_empty() {
                    output.push(format!(
                        "{} → {}",
                        summary.subject,
                        rule_action_summary(&actions)
                    ));
                }
            }
            Ok(if output.is_empty() {
                "No actions matched among the newest 50 cached messages.".to_string()
            } else {
                output.join("\n")
            })
        })();
        match result {
            Ok(text) => ui.set_rule_preview(text.into()),
            Err(e) => ui.set_manage_error(format!("Dry run failed: {e:#}").into()),
        }
    }

    fn select_label(&mut self, id: i32) {
        let Some(ui) = self.ui() else { return };
        match self.store.borrow().list_labels() {
            Ok(labels) => {
                if let Some(label) = labels.iter().find(|label| label.id as i32 == id) {
                    ui.set_label_id(id);
                    ui.set_label_name(label.name.as_str().into());
                    ui.set_manage_error("".into());
                } else {
                    ui.set_manage_error("That label no longer exists.".into());
                }
            }
            Err(e) => ui.set_manage_error(format!("Cannot load labels: {e:#}").into()),
        }
    }

    fn create_label(&mut self) {
        let Some(ui) = self.ui() else { return };
        let name = ui.get_label_name().to_string();
        let name = name.trim();
        if name.is_empty() {
            ui.set_manage_error("Enter a label name.".into());
            return;
        }
        let result = { self.store.borrow().create_label(name, "#3b7ddd") };
        match result {
            Ok(id) => {
                ui.set_label_id(id as i32);
                self.refresh_manage();
                self.refresh_sidebar();
            }
            Err(e) => ui.set_manage_error(format!("Could not create label: {e:#}").into()),
        }
    }

    fn apply_label(&mut self) {
        let Some(ui) = self.ui() else { return };
        let Some(message_id) = self.reader.as_ref().map(|r| r.loc.id) else {
            ui.set_manage_error("Open a message before applying a label.".into());
            return;
        };
        let selected = ui.get_label_id();
        let typed_name = ui.get_label_name().to_string();
        let result = (|| {
            let store = self.store.borrow();
            let labels = store.list_labels()?;
            let label = labels
                .iter()
                .find(|l| l.id as i32 == selected)
                .or_else(|| {
                    labels
                        .iter()
                        .find(|l| l.name.eq_ignore_ascii_case(typed_name.trim()))
                })
                .ok_or_else(|| anyhow!("Select a label or enter an existing label name"))?;
            store.toggle_message_label(message_id, label.id)
        })();
        match result {
            Ok(added) => {
                ui.set_manage_error(
                    if added {
                        "Label applied."
                    } else {
                        "Label removed."
                    }
                    .into(),
                );
                self.update_message_labels(message_id);
                self.refresh_all();
            }
            Err(e) => ui.set_manage_error(format!("{e:#}").into()),
        }
    }

    fn rename_label(&mut self) {
        let Some(ui) = self.ui() else { return };
        let id = ui.get_label_id();
        let name = ui.get_label_name().to_string();
        let name = name.trim();
        if id < 0 || name.is_empty() {
            ui.set_manage_error("Select a label and enter its new name.".into());
            return;
        }
        let result = { self.store.borrow().rename_label(id as i64, name) };
        match result {
            Ok(()) => {
                self.refresh_manage();
                self.refresh_sidebar();
                if let Some(message_id) = self.reader.as_ref().map(|r| r.loc.id) {
                    self.update_message_labels(message_id);
                }
            }
            Err(e) => ui.set_manage_error(format!("Could not rename label: {e:#}").into()),
        }
    }

    fn delete_label(&mut self) {
        let Some(ui) = self.ui() else { return };
        let id = ui.get_label_id();
        if id < 0 {
            ui.set_manage_error("Select a label first.".into());
            return;
        }
        let result = { self.store.borrow().delete_label(id as i64) };
        match result {
            Ok(()) => {
                let was_selected = ui.get_selected_folder() == -id - 1;
                ui.set_label_id(-1);
                ui.set_label_name("".into());
                self.refresh_manage();
                self.refresh_sidebar();
                if was_selected {
                    self.select_folder(0);
                } else {
                    self.apply_query();
                }
                if let Some(message_id) = self.reader.as_ref().map(|r| r.loc.id) {
                    self.update_message_labels(message_id);
                }
            }
            Err(e) => ui.set_manage_error(format!("Could not delete label: {e:#}").into()),
        }
    }

    fn update_message_labels(&mut self, message_id: MessageId) {
        let result = { self.store.borrow().message_labels(message_id) };
        match result {
            Ok(labels) => {
                if let Some(ui) = self.ui() {
                    ui.set_msg_labels(
                        labels
                            .iter()
                            .map(|l| l.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                            .into(),
                    );
                }
            }
            Err(e) => self.notify(format!("Could not load message labels: {e:#}")),
        }
    }

    fn save_servers(&mut self) {
        let (Some(ui), Some(account_id)) = (self.ui(), self.manage_account_id) else {
            return;
        };
        let imap_input = ui.get_manage_imap_host().to_string();
        let smtp_host = ui.get_manage_smtp_host().to_string();
        let imap_input = imap_input.trim();
        let (imap_host, imap_port) = match imap_input.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() => match port.parse::<u16>() {
                Ok(port) if port != 0 => (host.to_string(), port),
                _ => {
                    ui.set_manage_error("IMAP port must be between 1 and 65535.".into());
                    return;
                }
            },
            _ => (imap_input.to_string(), 993),
        };
        if imap_host.trim().is_empty()
            || imap_host.chars().any(char::is_whitespace)
            || smtp_host.trim().is_empty()
            || smtp_host.chars().any(char::is_whitespace)
        {
            ui.set_manage_error("Enter both IMAP and SMTP hosts.".into());
            return;
        }
        let stored_imap = if imap_port == 993 {
            imap_host
        } else {
            format!("{imap_host}:{imap_port}")
        };
        let result =
            self.store
                .borrow()
                .update_account_servers(account_id, &stored_imap, smtp_host.trim());
        if let Err(e) = result {
            ui.set_manage_error(format!("Could not save server settings: {e:#}").into());
            return;
        }
        let row = match self.store.borrow().list_accounts() {
            Ok(rows) => rows.into_iter().find(|a| a.id == account_id),
            Err(e) => {
                ui.set_manage_error(
                    format!("Settings saved, but account reload failed: {e:#}").into(),
                );
                return;
            }
        };
        let Some(row) = row else {
            ui.set_manage_error("Settings saved, but the account no longer exists.".into());
            return;
        };
        if let Ok(mut handles) = self.handles.lock() {
            handles.remove(&account_id);
        }
        self.statuses.remove(&account_id);
        match self.start_account(&row) {
            Ok(()) => {
                ui.set_manage_error("Settings saved; reconnecting.".into());
                self.update_status();
            }
            Err(e) => {
                ui.set_manage_error(format!("Settings saved, but reconnect failed: {e:#}").into())
            }
        }
    }

    // ----------------------------------------------------------------- reading pane

    fn close_reader(&mut self) {
        if let Some(handle) = self.reader.as_mut().and_then(|r| r.remote_download.take()) {
            handle.abort();
        }
        self.reader = None;
        self.link_hover = false;
        self.trim_timer
            .start(TimerMode::SingleShot, TRIM_AFTER_CLOSE, memory::release_unused_memory);
        let Some(ui) = self.ui() else { return };
        ui.set_has_message(false);
        ui.set_selected_message(-1);
        ui.set_loading(false);
        ui.set_reader_error("".into());
        ui.set_body_image(Image::default());
        ui.set_msg_labels("".into());
        ui.set_attachments(ModelRc::new(VecModel::from(Vec::<AttachmentChip>::new())));
        ui.set_pending_link("".into());
        ui.set_remote_blocked(0);
        ui.set_remote_loading(false);
        ui.set_body_link_hover(false);
        ui.set_scroll_size(1.0);
    }

    fn send_command(&mut self, account_id: AccountId, cmd: AccountCommand) -> bool {
        let result = match self.handles.lock() {
            Ok(h) => h.get(&account_id).map(|h| h.try_send(cmd)),
            Err(_) => None,
        };
        match result {
            Some(Ok(())) => true,
            Some(Err(e)) => {
                self.notify(format!("{e:#}"));
                false
            }
            None => {
                self.notify("This account is not running");
                false
            }
        }
    }

    fn open_message(&mut self, id: i32) {
        let id = id as MessageId;
        self.close_reader();
        let Some(ui) = self.ui() else { return };

        let loaded = {
            let s = self.store.borrow();
            (
                s.get_message(id),
                s.message_location(id),
                s.get_body(id, now_ts()),
            )
        };
        let (Ok(Some(row)), Ok(Some(mut loc)), Ok(body)) = loaded else {
            self.notify("That message is no longer available");
            self.refresh_all();
            return;
        };

        ui.set_selected_message(id as i32);
        ui.set_has_message(true);
        ui.set_msg_subject(if row.subject.is_empty() {
            "(no subject)".into()
        } else {
            row.subject.as_str().into()
        });
        let sender_name = display_name(&row.sender);
        let (red, green, blue) = avatar_rgb(&sender_name);
        ui.set_msg_initials(initials(&sender_name).into());
        ui.set_msg_avatar(slint::Color::from_rgb_u8(red, green, blue));
        ui.set_msg_from(sender_name.into());
        let folder = folder_title(&loc.folder, None);
        ui.set_msg_inbox(format!("{} · {folder}", self.account_email(loc.account_id)).into());
        ui.set_msg_date(format_full_date(row.date_ts).into());
        ui.set_msg_starred(row.is_starred());
        self.update_message_labels(id);

        if let Some(action) = actions::mark_read_action(&loc) {
            let marked = self.store.borrow().update_flags(id, flags::SEEN, 0).is_ok();
            if marked {
                loc.flags |= flags::SEEN;
                self.send_command(loc.account_id, AccountCommand::Action(action));
                self.model.invalidate_message(id);
                self.refresh_sidebar();
            }
        }

        self.reader = Some(Reader {
            loc: loc.clone(),
            awaiting_body: body.is_none(),
            view: None,
            scroll: 0,
            pending_link: None,
            remote_bytes: 0,
            remote_download: None,
            remote_request: None,
            press: None,
            dragged: false,
        });

        match body {
            Some(raw) => self.show_body(&raw),
            None => {
                ui.set_loading(true);
                let key = (loc.account_id, loc.folder.clone(), loc.uid);
                if self.pending_fetches.insert(key) {
                    let action = MailAction::FetchBody {
                        folder: loc.folder.clone(),
                        uid: loc.uid,
                    };
                    if !self.send_command(loc.account_id, AccountCommand::Action(action)) {
                        self.pending_fetches
                            .remove(&(loc.account_id, loc.folder, loc.uid));
                        ui.set_loading(false);
                        ui.set_reader_error("Cannot download this message right now.".into());
                    }
                }
            }
        }
    }

    /// Parses `raw` and builds the render view for the open message.
    fn show_body(&mut self, raw: &[u8]) {
        let Some(ui) = self.ui() else { return };
        ui.set_reader_attachment("".into());
        let scale = ui.window().scale_factor();
        let (w, h) = self.viewport;
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        reader.awaiting_body = false;
        reader.view = None;
        ui.set_loading(false);

        let Some(parsed) = parse_email(raw) else {
            ui.set_reader_error("This message could not be parsed.".into());
            return;
        };
        let chips: Vec<AttachmentChip> = parsed
            .attachments
            .iter()
            .map(|a| AttachmentChip {
                name: a.filename.as_str().into(),
                size: human_size(a.size_bytes).into(),
                index: a.part_index as i32,
            })
            .collect();
        ui.set_attachments(ModelRc::new(VecModel::from(chips)));

        match EmailView::new(&parsed, w, h, scale) {
            Ok(view) => {
                ui.set_reader_error("".into());
                reader.view = Some(view);
            }
            Err(e) => ui.set_reader_error(format!("{e:#}").into()),
        }
        self.render();
    }

    /// Re-layout the existing view after a resize, keeping its decoded images.
    fn rebuild_view(&mut self) {
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        let Some(view) = reader.view.as_mut() else {
            return;
        };
        view.resize(self.viewport.0, self.viewport.1);
        self.render();
    }

    fn render(&mut self) {
        let Some(ui) = self.ui() else { return };
        let (_, viewport_h) = self.viewport;
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        let Some(view) = reader.view.as_mut() else {
            return;
        };

        let total = view.content_height();
        reader.scroll = reader.scroll.min(total.saturating_sub(viewport_h));
        let buf = view.render_viewport_with(reader.scroll, |width, height, premul| {
            let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
            mailthin_render::view::flatten_on_white_into(premul, buf.make_mut_bytes());
            buf
        });
        ui.set_body_image(Image::from_rgba8(buf));
        ui.set_remote_blocked(view.blocked_remote_images() as i32);

        let total_f = (total as f32).max(1.0);
        let h = viewport_h as f32;
        if total_f <= h {
            ui.set_scroll_size(1.0);
            ui.set_scroll_start(0.0);
        } else {
            ui.set_scroll_size(h / total_f);
            ui.set_scroll_start(reader.scroll as f32 / total_f);
        }
    }

    fn on_viewport_resized(&mut self, w: f32, h: f32) {
        let size = (w.max(1.0) as u32, h.max(1.0) as u32);
        if size == self.viewport {
            return;
        }
        self.viewport = size;
        let weak = self.self_ref.clone();
        self.resize_timer
            .start(TimerMode::SingleShot, RESIZE_DEBOUNCE, move || {
                if let Some(shared) = weak.upgrade() {
                    with_app(&shared, |app| app.rebuild_view());
                }
            });
    }

    fn on_scrolled(&mut self, delta_y: f32) {
        if let Some(r) = self.reader.as_mut() {
            r.scroll = (r.scroll as f32 - delta_y).max(0.0) as u32;
            self.render();
        }
    }

    fn on_hover(&mut self, x: f32, y: f32) {
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        let Some(view) = reader.view.as_mut() else {
            return;
        };
        let over = view.hover_is_link(x, y, reader.scroll);
        if over != self.link_hover {
            self.link_hover = over;
            if let Some(ui) = self.ui() {
                ui.set_body_link_hover(over);
            }
        }
    }

    fn on_pressed(&mut self, x: f32, y: f32) {
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        let Some(view) = reader.view.as_mut() else {
            return;
        };
        view.selection_start(x, y, reader.scroll);
        reader.press = Some((x, y));
        reader.dragged = false;
        self.render();
    }

    fn on_dragged(&mut self, x: f32, y: f32) {
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        let Some((px, py)) = reader.press else { return };
        if !reader.dragged && (x - px).abs().max((y - py).abs()) < DRAG_THRESHOLD_PX {
            return;
        }
        let Some(view) = reader.view.as_mut() else {
            return;
        };
        reader.dragged = true;
        view.selection_extend(x, y, reader.scroll);
        self.render();
    }

    fn on_clicked(&mut self, x: f32, y: f32) {
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        // A drag that selected text is not a link click.
        if reader.dragged {
            return;
        }
        let Some(view) = reader.view.as_mut() else {
            return;
        };
        if let Some(url) = view.link_at(x, y, reader.scroll) {
            reader.pending_link = Some(url.clone());
            if let Some(ui) = self.ui() {
                ui.set_pending_link(url.into());
            }
        }
    }

    fn copy_selection(&mut self) {
        let text = self
            .reader
            .as_ref()
            .and_then(|r| r.view.as_ref())
            .and_then(|v| v.selected_text());
        let Some(text) = text else { return };
        if let Err(e) = arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text)) {
            self.notify(format!("Cannot copy text: {e}"));
        }
    }

    fn open_pending_link(&mut self) {
        let url = self.reader.as_mut().and_then(|r| r.pending_link.take());
        if let Some(ui) = self.ui() {
            ui.set_pending_link("".into());
        }
        // Re-validated here as well, so nothing but web and mail links can leave the app.
        if let Some(url) = url.and_then(|u| mailthin_render::safe_link(&u)) {
            if let Err(e) = open::that_detached(&url) {
                self.notify(format!("Cannot open link: {e}"));
            }
        }
    }

    fn dismiss_link(&mut self) {
        if let Some(r) = self.reader.as_mut() {
            r.pending_link = None;
        }
        if let Some(ui) = self.ui() {
            ui.set_pending_link("".into());
        }
    }

    fn load_remote_images(&mut self) {
        let Some(ui) = self.ui() else { return };
        let Some(reader) = self.reader.as_ref() else {
            return;
        };
        let Some(view) = reader.view.as_ref() else {
            return;
        };
        if ui.get_remote_loading() {
            return;
        }
        let urls: Vec<String> = view
            .unloaded_remote_images()
            .take(MAX_REMOTE_IMAGES)
            .cloned()
            .collect();
        if urls.is_empty() {
            return;
        }
        let remaining = MAX_REMOTE_BYTES.saturating_sub(reader.remote_bytes);
        if remaining == 0 {
            self.notify("Remote image memory limit reached for this message");
            return;
        }
        ui.set_remote_loading(true);
        let id = reader.loc.id;
        self.next_remote_request = self.next_remote_request.wrapping_add(1);
        let request = self.next_remote_request;
        let tx = self.ui_tx.clone();
        let cache = self.image_cache.clone();
        let task = self.rt.spawn(async move {
            let mut downloads = futures::stream::iter(urls)
                .map(|u| {
                    let cache = cache.clone();
                    async move {
                        if let Some(cache) = cache.as_ref() {
                            if let Some(data) = cache.get(&u) {
                                return Ok((u, data));
                            }
                        }
                        let data = fetch_remote_image(&u).await?;
                        let data = tokio::task::spawn_blocking(move || image_cache::downscale(data))
                            .await
                            .unwrap_or_default();
                        if let Some(cache) = cache.as_ref() {
                            cache.put(&u, &data);
                        }
                        Ok::<_, anyhow::Error>((u, data))
                    }
                })
                .buffer_unordered(4);
            let mut batch = RemoteImageBatch::new(remaining);
            while let Some(result) = downloads.next().await {
                batch.add(result);
            }
            let _ = tx.send(UiMsg::RemoteImages {
                id,
                request,
                images: batch.images,
                failed: batch.failed,
            });
        });
        if let Some(reader) = self.reader.as_mut() {
            reader.remote_download = Some(task);
            reader.remote_request = Some(request);
        }
    }

    fn remote_images_arrived(
        &mut self,
        id: MessageId,
        request: u64,
        images: Vec<(String, Vec<u8>)>,
        mut failed: bool,
    ) {
        let Some(ui) = self.ui() else { return };
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        if reader.loc.id != id || reader.remote_request != Some(request) {
            return;
        }
        reader.remote_download = None;
        reader.remote_request = None;
        ui.set_remote_loading(false);
        let mut accepted = Vec::new();
        for (url, data) in images {
            if data.len() <= MAX_REMOTE_BYTES.saturating_sub(reader.remote_bytes) {
                reader.remote_bytes += data.len();
                accepted.push((url, data));
            } else {
                failed = true;
            }
        }
        if let Some(view) = reader.view.as_mut() {
            view.load_remote_images(accepted);
        }
        self.render();
        if failed {
            self.notify("Some remote images could not be loaded or exceeded the message limit");
        }
    }

    // ----------------------------------------------------------------- actions

    fn toggle_star(&mut self) {
        let Some(reader) = self.reader.as_ref() else {
            return;
        };
        let loc = reader.loc.clone();
        let (set, clear, action) = actions::plan_toggle_star(&loc);
        let new_flags = match self.store.borrow().update_flags(loc.id, set, clear) {
            Ok(Some(f)) => f,
            _ => return,
        };
        self.send_command(loc.account_id, AccountCommand::Action(action));
        if let Some(r) = self.reader.as_mut() {
            r.loc.flags = new_flags;
        }
        if let Some(ui) = self.ui() {
            ui.set_msg_starred(new_flags & flags::FLAGGED != 0);
        }
        self.model.invalidate_message(loc.id);
    }

    /// Archive (`delete == false`) or delete the open message, then open the next one.
    fn triage(&mut self, delete: bool) {
        let Some(reader) = self.reader.as_ref() else {
            return;
        };
        let loc = reader.loc.clone();
        let plan = {
            let s = self.store.borrow();
            if delete {
                actions::plan_delete(&s, &loc)
            } else {
                actions::plan_archive(&s, &loc)
            }
        };
        let action = match plan {
            Ok(Plan::Relocate(a)) => a,
            Ok(Plan::Refuse(why)) => return self.notify(why),
            Err(e) => return self.notify(format!("{e:#}")),
        };
        let is_move = matches!(action, MailAction::Move { .. });
        let position = self.model.position_of(loc.id);

        if !self.send_command(loc.account_id, AccountCommand::Action(action)) {
            return;
        }
        if is_move {
            // Pull the moved message into its destination folder.
            self.send_command(loc.account_id, AccountCommand::SyncNow);
        }
        let deleted = self.store.borrow_mut().delete_message(loc.id);
        if let Err(e) = deleted {
            self.notify(format!("{e:#}"));
        }
        self.close_reader();
        self.refresh_all();

        let len = self.model.len();
        if let (Some(p), true) = (position, len > 0) {
            if let Some(next) = self.model.row_at(p.min(len - 1)) {
                self.open_message(next.id as i32);
            }
        }
    }

    // ----------------------------------------------------------------- events from workers

    fn handle(&mut self, msg: UiMsg) {
        match msg {
            UiMsg::Status { account_id, status } => {
                if let Some(entry) = self.statuses.get_mut(&account_id) {
                    entry.1 = status;
                }
                self.update_status();
            }
            UiMsg::FoldersChanged => self.refresh_sidebar(),
            UiMsg::LoginChecked { email, error } => self.finish_add_account(&email, error),
            UiMsg::MessagesChanged => self.refresh_all(),
            UiMsg::BodyReady {
                account_id,
                folder,
                uid,
            } => {
                self.pending_fetches
                    .remove(&(account_id, folder.clone(), uid));
                let matches = self.reader.as_ref().is_some_and(|r| {
                    r.awaiting_body
                        && r.loc.account_id == account_id
                        && r.loc.folder == folder
                        && r.loc.uid == uid
                });
                if !matches {
                    return;
                }
                let id = self.reader.as_ref().map(|r| r.loc.id).unwrap_or_default();
                let body = self.store.borrow_mut().get_body(id, now_ts());
                match body {
                    Ok(Some(raw)) => self.show_body(&raw),
                    _ => self.show_reader_error("The downloaded message could not be saved."),
                }
                self.refresh_all();
            }
            UiMsg::ActionFailed {
                account_id,
                folder,
                uid,
                error,
            } => {
                let was_fetch = self
                    .pending_fetches
                    .remove(&(account_id, folder.clone(), uid));
                if was_fetch {
                    let current = self.reader.as_ref().is_some_and(|r| {
                        r.awaiting_body
                            && r.loc.account_id == account_id
                            && r.loc.folder == folder
                            && r.loc.uid == uid
                    });
                    if current {
                        self.show_reader_error(&error);
                    }
                } else {
                    // A flag/move/delete was rejected: re-fetch the folder to match the server.
                    self.notify(format!("Server rejected the change: {error}"));
                    self.send_command(account_id, AccountCommand::Resync { folder });
                }
            }
            UiMsg::Sent {
                subject,
                copy_error,
                ..
            } => {
                if self
                    .last_sent
                    .as_ref()
                    .is_some_and(|m| m.subject == subject)
                {
                    self.last_sent = None;
                    self.last_sent_draft = None;
                }
                match copy_error {
                    None => self.notify(format!("Sent \"{subject}\".")),
                    Some(e) => self.notify(format!(
                        "Sent \"{subject}\", but it could not be saved to Sent: {e}"
                    )),
                }
            }
            UiMsg::SendFailed { subject, error } => {
                self.notify(format!("Could not send \"{subject}\": {error}"));
                self.restore_failed_send(&subject, &error);
            }
            UiMsg::RemoteImages {
                id,
                request,
                images,
                failed,
            } => self.remote_images_arrived(id, request, images, failed),
        }
    }

    // ---------------- Compose, reply, forward ----------------

    fn compose_new(&mut self) {
        let first = self
            .store
            .borrow()
            .list_accounts()
            .unwrap_or_default()
            .into_iter()
            .next();
        match first {
            Some(account) => self.show_compose(account.id, &Draft::default()),
            None => self.notify("Add an account before composing a message"),
        }
    }

    fn reply_to_open(&mut self, kind: ReplyKind) {
        let Some(id) = self.reader.as_ref().map(|r| r.loc.id) else {
            self.notify("Open a message to reply to it");
            return;
        };
        let (row, raw, account_id) = {
            let s = self.store.borrow();
            let loc = s.message_location(id);
            (
                s.get_message(id),
                s.get_body(id, now_ts()),
                loc.ok().flatten().map(|l| l.account_id),
            )
        };
        let (Ok(Some(row)), Ok(Some(raw)), Some(account_id)) = (row, raw, account_id) else {
            self.notify("That message is no longer available");
            return;
        };
        let source = quote_source(&raw).unwrap_or(mailthin_render::QuoteSource {
            message_id: None,
            references: Vec::new(),
            text: String::new(),
        });
        let me = self.account_email(account_id);
        let orig = Original {
            sender: &row.sender,
            recipients: &row.recipients,
            subject: &row.subject,
            body_text: &source.text,
            message_id: source.message_id.as_deref(),
            references: &source.references,
        };
        let draft = glue::draft_for(kind, &orig, &me);
        self.show_compose(account_id, &draft);
    }

    fn account_email(&self, account_id: AccountId) -> String {
        self.store
            .borrow()
            .list_accounts()
            .unwrap_or_default()
            .into_iter()
            .find(|a| a.id == account_id)
            .map(|a| a.email)
            .unwrap_or_default()
    }

    fn show_compose(&mut self, account_id: AccountId, draft: &Draft) {
        self.show_compose_with_attachments(account_id, draft, Vec::new());
    }

    fn show_compose_with_attachments(
        &mut self,
        account_id: AccountId,
        draft: &Draft,
        attachments: Vec<(String, String, Vec<u8>)>,
    ) {
        let from = self.account_email(account_id);
        let Some(ui) = self.ui() else { return };
        ui.set_compose_from(from.into());
        ui.set_compose_to(draft.to.as_str().into());
        ui.set_compose_cc(draft.cc.as_str().into());
        ui.set_compose_subject(draft.subject.as_str().into());
        ui.set_compose_body(draft.body.as_str().into());
        ui.set_compose_error("".into());
        ui.set_compose_preview_image(Image::default());
        ui.set_compose_preview_mode(true);
        self.compose_attachments = attachments;
        self.refresh_compose_attachments();
        ui.set_show_compose(true);
        self.render_compose_preview();
        self.compose = Some(ComposeState {
            account_id,
            in_reply_to: draft.in_reply_to.clone(),
            references: draft.references.clone(),
        });
    }

    fn refresh_compose_attachments(&self) {
        let Some(ui) = self.ui() else { return };
        let chips: Vec<ComposeAttachment> = self
            .compose_attachments
            .iter()
            .enumerate()
            .map(|(index, (name, _, bytes))| ComposeAttachment {
                name: name.as_str().into(),
                size: human_size(bytes.len()).into(),
                index: index as i32,
            })
            .collect();
        ui.set_compose_attachments(ModelRc::new(VecModel::from(chips)));
    }

    fn add_compose_attachments(&mut self) {
        let paths = rfd::FileDialog::new().pick_files();
        let Some(paths) = paths else { return };
        let mut total: usize = self
            .compose_attachments
            .iter()
            .map(|(_, _, bytes)| bytes.len())
            .sum();
        for path in paths {
            if self.compose_attachments.len() >= MAX_COMPOSE_ATTACHMENTS {
                self.notify(format!(
                    "A message can have at most {MAX_COMPOSE_ATTACHMENTS} attachments."
                ));
                break;
            }
            let metadata = match fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(e) => {
                    self.notify(format!("Cannot read attachment metadata: {e}"));
                    continue;
                }
            };
            let len = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
            if len > MAX_COMPOSE_ATTACHMENT_BYTES
                || total.saturating_add(len) > MAX_COMPOSE_TOTAL_ATTACHMENT_BYTES
            {
                self.notify("Attachments are limited to 5 MiB each and 10 MiB total.");
                continue;
            }
            let mut file = match fs::File::open(&path) {
                Ok(file) => file,
                Err(e) => {
                    self.notify(format!("Cannot read attachment: {e}"));
                    continue;
                }
            };
            let mut bytes = Vec::with_capacity(len.min(MAX_COMPOSE_ATTACHMENT_BYTES));
            if let Err(e) = file
                .by_ref()
                .take((MAX_COMPOSE_ATTACHMENT_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
            {
                self.notify(format!("Cannot read attachment: {e}"));
                continue;
            }
            if bytes.len() > MAX_COMPOSE_ATTACHMENT_BYTES
                || total.saturating_add(bytes.len()) > MAX_COMPOSE_TOTAL_ATTACHMENT_BYTES
            {
                self.notify(
                    "The selected file changed while reading and exceeds the attachment limit.",
                );
                continue;
            }
            let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                self.notify("The selected attachment has no filename.");
                continue;
            };
            total += bytes.len();
            self.compose_attachments
                .push((name.clone(), mime_for_name(&name), bytes));
        }
        self.refresh_compose_attachments();
    }

    fn remove_compose_attachment(&mut self, index: i32) {
        if index >= 0 {
            let index = index as usize;
            if index < self.compose_attachments.len() {
                self.compose_attachments.remove(index);
            }
        }
        self.refresh_compose_attachments();
    }

    fn insert_compose_format(&mut self, format: &str, anchor: i32, cursor: i32) {
        let Some(ui) = self.ui() else { return };
        let body = ui.get_compose_body().to_string();
        let (text, start, end) = glue::apply_format(
            &body,
            anchor.max(0) as usize,
            cursor.max(0) as usize,
            format,
        );
        ui.set_compose_body(text.into());
        ui.set_compose_select_anchor(start as i32);
        ui.set_compose_select_cursor(end as i32);
        ui.set_compose_select_serial(ui.get_compose_select_serial().wrapping_add(1));
    }

    fn render_compose_preview(&mut self) {
        let Some(ui) = self.ui() else { return };
        let (_, html) = glue::render_markdown(ui.get_compose_body().as_str());
        let parsed = ParsedEmail {
            html: Some(html),
            attachments: Vec::new(),
            cid_images: Vec::new(),
        };
        let scale = ui.window().scale_factor();
        match EmailView::new(&parsed, 720, 340, scale) {
            Ok(mut view) => {
                let buf = view.render_viewport_with(0, |width, height, premul| {
                    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
                    mailthin_render::view::flatten_on_white_into(premul, buf.make_mut_bytes());
                    buf
                });
                ui.set_compose_preview_image(Image::from_rgba8(buf));
            }
            Err(e) => {
                ui.set_compose_error(format!("Cannot render preview: {e:#}").into());
                ui.set_compose_preview_mode(false);
            }
        }
    }

    /// Shows a Markdown attachment in the reader, rendered with the same sanitizing path as compose.
    fn show_markdown_attachment(&mut self, name: &str, bytes: &[u8]) {
        let Some(ui) = self.ui() else { return };
        let scale = ui.window().scale_factor();
        let (w, h) = self.viewport;
        let text = String::from_utf8_lossy(bytes);
        let (_, html) = glue::render_markdown(&text);
        let parsed = ParsedEmail {
            html: Some(html),
            attachments: Vec::new(),
            cid_images: Vec::new(),
        };
        let Some(reader) = self.reader.as_mut() else { return };
        match EmailView::new(&parsed, w, h, scale) {
            Ok(view) => {
                reader.view = Some(view);
                ui.set_reader_attachment(name.into());
                self.render();
            }
            Err(e) => self.notify(format!("Cannot render Markdown: {e:#}")),
        }
    }

    /// Returns from a rendered Markdown attachment to the message itself.
    fn close_markdown_view(&mut self) {
        let Some(id) = self.reader.as_ref().map(|r| r.loc.id) else {
            return;
        };
        let body = { self.store.borrow_mut().get_body(id, now_ts()) };
        if let Ok(Some(raw)) = body {
            self.show_body(&raw);
        }
    }

    fn open_attachment(&mut self, index: i32) {
        let Some(id) = self.reader.as_ref().map(|r| r.loc.id) else {
            return;
        };
        if index < 0 {
            self.notify("Invalid attachment selection");
            return;
        }
        let body = { self.store.borrow_mut().get_body(id, now_ts()) };
        let raw = match body {
            Ok(Some(raw)) => raw,
            Ok(None) => {
                self.notify("The message body is not cached yet. Open the message again after it downloads.");
                return;
            }
            Err(e) => {
                self.notify(format!("Cannot load the message body: {e:#}"));
                return;
            }
        };
        let Some((name, _, bytes)) = extract_attachment(&raw, index as usize) else {
            self.notify("This attachment is no longer available.");
            return;
        };
        if bytes.len() > MAX_SAVED_ATTACHMENT_BYTES {
            self.notify("This attachment exceeds the 40 MiB save limit.");
            return;
        }
        if is_markdown_name(&name) && bytes.len() <= MAX_MARKDOWN_VIEW_BYTES {
            let view_here = rfd::MessageDialog::new()
                .set_title("Show Markdown?")
                .set_description(
                    "Show this Markdown file rendered in the reader? Choose No to save it instead.",
                )
                .set_buttons(rfd::MessageButtons::YesNo)
                .show();
            if view_here == rfd::MessageDialogResult::Yes {
                self.show_markdown_attachment(&name, &bytes);
                return;
            }
        }
        let Some(path) = rfd::FileDialog::new().set_file_name(&name).save_file() else {
            return;
        };
        match fs::write(&path, bytes) {
            Ok(()) => {
                let open = rfd::MessageDialog::new()
                    .set_title("Open attachment?")
                    .set_description(
                        "Open this saved file with the system default app? Only open attachments you trust.",
                    )
                    .set_buttons(rfd::MessageButtons::YesNo)
                    .show();
                if open == rfd::MessageDialogResult::Yes {
                    match open::that_detached(&path) {
                        Ok(()) => {
                            self.notify(format!("Opened attachment from {}.", path.display()))
                        }
                        Err(e) => self.notify(format!(
                            "Attachment saved to {}, but could not be opened: {e}",
                            path.display()
                        )),
                    }
                } else {
                    self.notify(format!("Saved attachment to {}.", path.display()));
                }
            }
            Err(e) => self.notify(format!("Cannot save attachment: {e}")),
        }
    }

    fn send_compose(&mut self) {
        let (Some(state), Some(ui)) = (self.compose.clone(), self.ui()) else {
            return;
        };
        if self.last_sent.is_some() {
            ui.set_compose_error("Wait for the current message to finish sending.".into());
            return;
        }
        let fields = Draft {
            to: ui.get_compose_to().to_string(),
            cc: ui.get_compose_cc().to_string(),
            subject: ui.get_compose_subject().to_string(),
            body: ui.get_compose_body().to_string(),
            in_reply_to: state.in_reply_to.clone(),
            references: state.references.clone(),
        };
        let mut mail = glue::to_outgoing(&state, &fields);
        mail.attachments = self.compose_attachments.clone();
        if mail.to.is_empty() {
            ui.set_compose_error("Add at least one recipient.".into());
            return;
        }
        let sent_folder = self
            .store
            .borrow()
            .folder_by_role(state.account_id, "sent")
            .ok()
            .flatten()
            .map(|(_, name)| name);
        let queued = match self.handles.lock() {
            Ok(h) => h.get(&state.account_id).map(|h| {
                h.try_send_mail(SendRequest {
                    mail: mail.clone(),
                    sent_folder,
                })
            }),
            Err(_) => None,
        };
        match queued {
            Some(Ok(())) => {
                self.notify(format!("Sending \"{}\"...", mail.subject));
                self.last_sent = Some(mail);
                self.last_sent_draft = Some(fields);
                self.compose = None;
                self.compose_attachments.clear();
                ui.set_show_compose(false);
            }
            Some(Err(e)) => ui.set_compose_error(format!("{e:#}").into()),
            None => ui.set_compose_error("This account is not running.".into()),
        }
    }

    fn discard_compose(&mut self) {
        self.compose = None;
        self.compose_attachments.clear();
        if let Some(ui) = self.ui() {
            ui.set_show_compose(false);
            ui.set_compose_attachments(ModelRc::new(VecModel::from(
                Vec::<ComposeAttachment>::new(),
            )));
        }
    }

    /// Reopens a failed message for editing, unless the user has already started another one.
    fn restore_failed_send(&mut self, subject: &str, error: &str) {
        let Some(mail) = self.last_sent.take().filter(|m| m.subject == subject) else {
            return;
        };
        let draft = self
            .last_sent_draft
            .take()
            .unwrap_or_else(|| glue::draft_of(&mail));
        if self.compose.is_some() {
            return;
        }
        self.show_compose_with_attachments(mail.account_id, &draft, mail.attachments.clone());
        if let Some(ui) = self.ui() {
            ui.set_compose_error(format!("Not sent: {error}").into());
        }
    }

    fn show_reader_error(&mut self, text: &str) {
        if let Some(r) = self.reader.as_mut() {
            r.awaiting_body = false;
        }
        if let Some(ui) = self.ui() {
            ui.set_loading(false);
            ui.set_reader_error(text.into());
        }
    }

    // ----------------------------------------------------------------- accounts

    fn start_account(&mut self, row: &AccountRow) -> Result<()> {
        let credentials = match oauth::provider_for_auth_type(&row.auth_type) {
            Some(provider) => oauth_credential_provider(provider, row.email.clone())?,
            None => {
                let secret = keystore::get_account_secret(&row.email)?
                    .ok_or_else(|| anyhow!("no stored password for {}", row.email))?;
                static_credential(Credential::Password(secret))
            }
        };
        let (host, port) = split_host_port(&row.imap_host, 993);
        let (rules, folder_states) = {
            let s = self.store.borrow();
            (s.load_rules(row.id)?, s.folder_states(row.id)?)
        };
        let _guard = self.rt.enter();
        let handle = spawn_account(
            AccountConfig {
                account_id: row.id,
                settings: ImapSettings {
                    host,
                    port,
                    username: row.email.clone(),
                },
                smtp: SmtpSettings {
                    host: row.smtp_host.clone(),
                    username: row.email.clone(),
                },
                credentials,
                rules,
                folder_states,
                sync_window_days: SYNC_WINDOW_DAYS,
            },
            self.events_tx.clone(),
        );
        self.handles
            .lock()
            .map_err(|_| anyhow!("account table poisoned"))?
            .insert(row.id, handle);
        self.statuses
            .insert(row.id, (row.email.clone(), SyncStatus::Connecting));
        Ok(())
    }

    /// Fills the server fields from the typed address. A field is only replaced when it is blank
    /// or still holds the previous automatic guess, so hosts the user typed are never overwritten.
    fn fill_server_defaults(&mut self, email: &str) {
        let Some(ui) = self.ui() else { return };
        let guess = provider_defaults(email);
        let previous = self.auto_hosts.take();
        let replaceable =
            |current: &str, prev: Option<&str>| current.trim().is_empty() || prev == Some(current);

        let imap = ui.get_acc_imap_host();
        if replaceable(
            imap.as_str(),
            previous.as_ref().map(|d| d.imap_host.as_str()),
        ) {
            ui.set_acc_imap_host(
                guess
                    .as_ref()
                    .map(|d| d.imap_host.clone())
                    .unwrap_or_default()
                    .into(),
            );
        }
        let smtp = ui.get_acc_smtp_host();
        if replaceable(
            smtp.as_str(),
            previous.as_ref().map(|d| d.smtp_host.as_str()),
        ) {
            ui.set_acc_smtp_host(
                guess
                    .as_ref()
                    .map(|d| d.smtp_host.clone())
                    .unwrap_or_default()
                    .into(),
            );
        }
        self.auto_hosts = guess;
    }

    fn add_account(
        &mut self,
        email: &str,
        password: &str,
        imap_host: &str,
        port: &str,
        smtp_host: &str,
    ) {
        let secret = AddSecret::Password(Zeroizing::new(password.to_string()));
        self.start_add(email, imap_host, port, smtp_host, secret);
    }

    fn sign_in(
        &mut self,
        provider: &'static OAuthProvider,
        email: &str,
        imap_host: &str,
        port: &str,
        smtp_host: &str,
    ) {
        self.start_add(
            email,
            imap_host,
            port,
            smtp_host,
            AddSecret::OAuth(provider),
        );
    }

    fn start_add(
        &mut self,
        email: &str,
        imap_host: &str,
        port: &str,
        smtp_host: &str,
        secret: AddSecret,
    ) {
        let Some(ui) = self.ui() else { return };
        if self.pending_add.is_some() {
            ui.set_add_account_error("Another sign-in is still in progress".into());
            return;
        }
        let progress = match secret {
            AddSecret::Password(_) => "Checking the login...",
            AddSecret::OAuth(_) => "Waiting for sign-in in your browser...",
        };
        match self.prepare_add_account(email, imap_host, port, smtp_host, secret) {
            Ok(pending) => {
                ui.set_add_account_error(progress.into());
                self.check_login(pending);
            }
            Err(e) => ui.set_add_account_error(format!("{e:#}").into()),
        }
    }

    /// Tests the login on a background task. The account is saved only if the test passes.
    fn check_login(&mut self, pending: PendingAdd) {
        let settings = ImapSettings {
            host: pending.imap_host.clone(),
            port: pending.port,
            username: pending.email.clone(),
        };
        let email = pending.email.clone();
        let oauth_provider = match &pending.secret {
            AddSecret::OAuth(provider) => Some(*provider),
            AddSecret::Password(_) => None,
        };
        let password = match &pending.secret {
            AddSecret::Password(p) => Some(p.clone()),
            AddSecret::OAuth(_) => None,
        };
        self.pending_add = Some(pending);
        let tx = self.ui_tx.clone();
        self.rt.spawn(async move {
            let result = match (oauth_provider, password) {
                (Some(provider), _) => oauth_sign_in(provider, &email, &settings).await,
                (None, Some(password)) => {
                    test_imap_login(&settings, &Credential::Password(password)).await
                }
                (None, None) => Err(anyhow!("no credentials to test")),
            };
            let error = result.err().map(|e| format!("{e:#}"));
            let _ = tx.send(UiMsg::LoginChecked { email, error });
        });
    }

    fn finish_add_account(&mut self, email: &str, error: Option<String>) {
        let Some(ui) = self.ui() else { return };
        if !self.pending_add.as_ref().is_some_and(|p| p.email == email) {
            return;
        }
        let Some(pending) = self.pending_add.take() else {
            return;
        };
        if let Some(e) = error {
            ui.set_add_account_error(format!("Login failed: {e}").into());
            return;
        }
        match self.save_account(pending) {
            Ok(()) => {
                ui.set_add_account_error("".into());
                ui.set_acc_password("".into());
                ui.set_show_add_account(false);
                self.refresh_sidebar();
                self.update_status();
            }
            Err(e) => ui.set_add_account_error(format!("{e:#}").into()),
        }
    }

    fn prepare_add_account(
        &self,
        email: &str,
        imap_host: &str,
        port: &str,
        smtp_host: &str,
        secret: AddSecret,
    ) -> Result<PendingAdd> {
        let email = email.trim();
        if !looks_like_email(email) {
            return Err(anyhow!("Enter a valid email address"));
        }
        if matches!(&secret, AddSecret::Password(p) if p.is_empty()) {
            return Err(anyhow!("Enter the account's app password"));
        }
        let defaults =
            provider_defaults(email).ok_or_else(|| anyhow!("Cannot guess the mail servers"))?;
        let host = match imap_host.trim() {
            "" => defaults.imap_host,
            h => h.to_string(),
        };
        let smtp = match smtp_host.trim() {
            "" => defaults.smtp_host,
            h => h.to_string(),
        };
        let port: u16 = match port.trim() {
            "" => 993,
            p => p
                .parse()
                .map_err(|_| anyhow!("The IMAP port must be a number"))?,
        };
        if self
            .store
            .borrow()
            .list_accounts()?
            .iter()
            .any(|a| a.email.eq_ignore_ascii_case(email))
        {
            return Err(anyhow!("That account is already added"));
        }
        Ok(PendingAdd {
            email: email.into(),
            secret,
            imap_host: host,
            port,
            smtp_host: smtp,
        })
    }

    fn save_account(&mut self, pending: PendingAdd) -> Result<()> {
        let PendingAdd {
            email,
            secret,
            imap_host,
            port,
            smtp_host,
        } = pending;
        let auth_type = match &secret {
            AddSecret::Password(password) => {
                keystore::set_account_secret(&email, password.as_str())
                    .context("cannot save the password to the OS keyring")?;
                "password".to_string()
            }
            // The refresh token was stored by the sign-in task once the login test passed.
            AddSecret::OAuth(provider) => provider.auth_type(),
        };
        let stored_host = if port == 993 {
            imap_host
        } else {
            format!("{imap_host}:{port}")
        };
        let id = match self.store.borrow().add_account(&NewAccount {
            kind: "imap",
            email: &email,
            imap_host: &stored_host,
            smtp_host: &smtp_host,
            auth_type: &auth_type,
        }) {
            Ok(id) => id,
            Err(e) => {
                let _ = keystore::delete_account_secret(&email);
                return Err(e);
            }
        };
        let row = AccountRow {
            id,
            kind: "imap".into(),
            email,
            imap_host: stored_host,
            smtp_host,
            auth_type,
        };
        self.start_account(&row)
    }

    fn sync_all_accounts(&mut self) {
        let ids: Vec<AccountId> = self
            .handles
            .lock()
            .map(|h| h.keys().copied().collect())
            .unwrap_or_default();
        for id in ids {
            self.send_command(id, AccountCommand::SyncNow);
        }
        self.notice = None;
        self.update_status();
    }
}

// --------------------------------------------------------------------------- startup

fn data_dir() -> Result<PathBuf> {
    let dir = std::env::var_os("MAILTHIN_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("APPDATA").map(|p| PathBuf::from(p).join("Mailthin")))
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/mailthin"))
        })
        .ok_or_else(|| anyhow!("cannot determine a data directory"))?;
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    Ok(dir)
}

pub fn run() -> Result<()> {
    let data_dir = data_dir()?;
    let db_path = data_dir.join("mail.db");
    let image_cache = ImageCache::open(&data_dir).ok();
    if let Some(cache) = image_cache.as_ref() {
        cache.sweep();
    }
    let idle_trim_timer = Timer::default();
    idle_trim_timer.start(TimerMode::Repeated, IDLE_TRIM_INTERVAL, memory::release_unused_memory);
    let db_path = db_path.to_string_lossy().into_owned();
    let key = keystore::get_or_create_db_key()?;

    // The writer thread owns one connection; the UI thread uses a second one (WAL mode).
    let writer_store = MailStore::open_encrypted(&db_path, &key)?;
    let ui_store = Rc::new(RefCell::new(MailStore::open_encrypted(&db_path, &key)?));
    drop(key);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let (events_tx, events_rx) = mpsc::channel::<SyncEvent>(64);
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiMsg>();
    let (server_tx, server_rx) = std::sync::mpsc::channel::<ServerAction>();
    let handles: Handles = Arc::new(Mutex::new(HashMap::new()));

    let notify_tx = ui_tx.clone();
    let writer = spawn_store_writer(writer_store, events_rx, server_tx, move |event| {
        if let Some(msg) = to_ui_msg(event) {
            let _ = notify_tx.send(msg);
        }
    });

    // Rule-triggered server actions (from the writer thread) are routed to the right account.
    let forward_handles = handles.clone();
    std::thread::spawn(move || {
        while let Ok(a) = server_rx.recv() {
            if let Ok(h) = forward_handles.lock() {
                if let Some(handle) = h.get(&a.account_id) {
                    let _ = handle.try_send(AccountCommand::Action(a.action));
                }
            }
        }
    });

    let ui = MainWindow::new()?;
    let model = MessageListModel::new(ui_store.clone(), MessageQuery::new(ListScope::UnifiedInbox));
    ui.set_messages(ModelRc::from(
        model.clone() as Rc<dyn Model<Data = MessageItem>>
    ));
    ui.set_selected_folder(0);

    let shared: Shared = Rc::new_cyclic(|weak| {
        RefCell::new(App {
            ui: ui.as_weak(),
            store: ui_store,
            model,
            handles: handles.clone(),
            rt: runtime.handle().clone(),
            events_tx: events_tx.clone(),
            ui_tx,
            statuses: BTreeMap::new(),
            notice: None,
            reader: None,
            viewport: (600, 400),
            link_hover: false,
            pending_fetches: HashSet::new(),
            next_remote_request: 0,
            image_cache,
            resize_timer: Timer::default(),
            trim_timer: Timer::default(),
            _idle_trim_timer: idle_trim_timer,
            auto_hosts: None,
            pending_add: None,
            compose: None,
            last_sent: None,
            last_sent_draft: None,
            compose_attachments: Vec::new(),
            manage_account_id: None,
            collapsed_accounts: HashSet::new(),
            self_ref: weak.clone(),
        })
    });

    {
        let mut app = shared.borrow_mut();
        let accounts = app.store.borrow().list_accounts()?;
        for row in &accounts {
            if let Err(e) = app.start_account(row) {
                app.statuses.insert(
                    row.id,
                    (row.email.clone(), SyncStatus::Error(format!("{e:#}"))),
                );
            }
        }
        app.refresh_all();
        app.update_status();
    }

    wire_callbacks(&ui, &shared);

    let poll = Timer::default();
    {
        let shared = shared.clone();
        poll.start(TimerMode::Repeated, POLL_INTERVAL, move || {
            while let Ok(msg) = ui_rx.try_recv() {
                shared.borrow_mut().handle(msg);
            }
        });
    }

    ui.run()?;

    // Orderly shutdown: stop the workers, let the writer drain, then exit.
    drop(poll);
    drop(ui);
    drop(shared);
    let workers: Vec<AccountHandle> = handles
        .lock()
        .map(|mut h| h.drain().map(|(_, w)| w).collect())
        .unwrap_or_default();
    let stopped = runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(3),
            futures::future::join_all(workers.into_iter().map(|w| w.shutdown())),
        )
        .await
        .is_ok()
    });
    drop(events_tx);
    if stopped {
        let _ = writer.join();
    }
    Ok(())
}

/// Builds a credential source that exchanges the stored refresh token for a fresh access token on
/// every connection. A rotated refresh token returned by the provider replaces the stored one.
fn oauth_credential_provider(
    provider: &'static OAuthProvider,
    email: String,
) -> Result<CredentialProvider> {
    let config = provider.client_config_from_env()?;
    let refresh_lock = Arc::new(tokio::sync::Mutex::new(()));
    Ok(Arc::new(
        move || -> BoxFuture<'static, Result<Credential>> {
            let config = config.clone();
            let email = email.clone();
            let refresh_lock = refresh_lock.clone();
            Box::pin(async move {
                let _guard = refresh_lock.lock().await;
                let refresh = keystore::get_account_secret(&email)?.ok_or_else(|| {
                    anyhow!("no stored sign-in for {email}; add the account again")
                })?;
                let tokens = oauth::refresh_access_token(provider, &config, &refresh).await?;
                if let Some(rotated) = &tokens.refresh_token {
                    keystore::set_account_secret(&email, rotated.as_str())?;
                }
                Ok(Credential::OAuth2AccessToken(tokens.access_token))
            })
        },
    ))
}

#[cfg(test)]
mod remote_batch_tests {
    use super::*;

    #[test]
    fn limits_accumulated_image_bytes_and_reports_failures() {
        let mut batch = RemoteImageBatch::new(5);
        batch.add(Ok(("first".into(), vec![1; 3])));
        batch.add(Ok(("too-large".into(), vec![2; 3])));
        batch.add(Ok(("second".into(), vec![3; 2])));
        batch.add(Err(anyhow!("network error")));
        assert_eq!(batch.total, 5);
        assert_eq!(batch.images.len(), 2);
        assert!(batch.failed);
    }

    #[test]
    fn rule_condition_editor_parses_multiple_supported_conditions() {
        assert_eq!(
            parse_rule_conditions("from:alice@example.com, subject:invoice, attachment").unwrap(),
            vec![
                Condition::FromContains("alice@example.com".into()),
                Condition::SubjectContains("invoice".into()),
                Condition::HasAttachment,
            ]
        );
    }

    #[test]
    fn rule_condition_editor_reports_invalid_input() {
        assert!(parse_rule_conditions("unknown:value").is_err());
        assert!(parse_rule_conditions("").is_err());
    }
}

/// Runs the browser sign-in, tests the IMAP login with the new access token, and stores the
/// refresh token only after that test passes.
async fn oauth_sign_in(
    provider: &'static OAuthProvider,
    email: &str,
    settings: &ImapSettings,
) -> Result<()> {
    let config = provider.client_config_from_env()?;
    let pending = oauth::begin_authorization(provider, &config)?;
    open::that_detached(pending.auth_url.as_str())
        .context("cannot open the browser for sign-in")?;
    let tokens = pending.complete(OAUTH_SIGN_IN_TIMEOUT).await?;
    let refresh = tokens.refresh_token.as_ref().ok_or_else(|| {
        anyhow!(
            "{} did not issue a refresh token; try signing in again",
            provider.name
        )
    })?;
    test_imap_login(
        settings,
        &Credential::OAuth2AccessToken(tokens.access_token.clone()),
    )
    .await?;
    keystore::set_account_secret(email, refresh.as_str())
        .context("cannot save the sign-in to the OS keyring")?;
    Ok(())
}

async fn test_imap_login(settings: &ImapSettings, credential: &Credential) -> Result<()> {
    let mut session = mailthin_net::imap::connect(settings, credential).await?;
    let _ = session.logout().await;
    Ok(())
}

fn wire_callbacks(ui: &MainWindow, shared: &Shared) {
    macro_rules! on {
        ($setter:ident, |$app:ident $(, $arg:ident)*| $body:expr) => {{
            let s = shared.clone();
            ui.$setter(move |$($arg),*| with_app(&s, |$app| $body));
        }};
    }

    on!(on_select_folder, |app, id| app.select_folder(id));
    on!(on_toggle_account, |app, id| app.toggle_account(id));
    on!(on_select_message, |app, id| app.open_message(id));
    on!(on_compose_new, |app| app.compose_new());
    on!(on_reply_message, |app| app.reply_to_open(ReplyKind::Reply));
    on!(on_reply_all_message, |app| app
        .reply_to_open(ReplyKind::ReplyAll));
    on!(on_forward_message, |app| app
        .reply_to_open(ReplyKind::Forward));
    on!(on_compose_send, |app| app.send_compose());
    on!(on_compose_discard, |app| app.discard_compose());
    on!(on_compose_add_attachment, |app| app
        .add_compose_attachments());
    on!(on_compose_remove_attachment, |app, index| app
        .remove_compose_attachment(index));
    on!(on_compose_format, |app, format, anchor, cursor| app
        .insert_compose_format(format.as_str(), anchor, cursor));
    on!(on_compose_preview, |app| app.render_compose_preview());
    on!(on_open_attachment, |app, index| app.open_attachment(index));
    on!(on_close_attachment, |app| app.close_markdown_view());
    on!(on_manage_open, |app| app.open_manage());
    on!(on_close_manage, |app| app.close_manage());
    on!(on_manage_select_account, |app, index| app.manage_select_account(index));
    on!(on_select_rule, |app, id| app.select_rule(id));
    on!(on_save_rule, |app| app.save_rule());
    on!(on_delete_rule, |app| app.delete_rule());
    on!(on_dry_run_rules, |app| app.dry_run_rules());
    on!(on_select_label, |app, id| app.select_label(id));
    on!(on_create_label, |app| app.create_label());
    on!(on_rename_label, |app| app.rename_label());
    on!(on_apply_label, |app| app.apply_label());
    on!(on_delete_label, |app| app.delete_label());
    on!(on_save_servers, |app| app.save_servers());
    on!(on_search_changed, |app, _text| app.apply_query());
    on!(on_filters_changed, |app| app.apply_query());
    on!(on_viewport_resized, |app, w, h| app
        .on_viewport_resized(w, h));
    on!(on_body_scrolled, |app, dy| app.on_scrolled(dy));
    on!(on_body_hover, |app, x, y| app.on_hover(x, y));
    on!(on_body_pressed, |app, x, y| app.on_pressed(x, y));
    on!(on_body_dragged, |app, x, y| app.on_dragged(x, y));
    on!(on_body_clicked, |app, x, y| app.on_clicked(x, y));
    on!(on_copy_selection, |app| app.copy_selection());
    on!(on_toggle_remote, |app| app.load_remote_images());
    on!(on_close_reader, |app| app.close_reader());
    on!(on_open_link, |app| app.open_pending_link());
    on!(on_dismiss_link, |app| app.dismiss_link());
    on!(on_star, |app| app.toggle_star());
    on!(on_archive, |app| app.triage(false));
    on!(on_delete_message, |app| app.triage(true));
    on!(on_refresh, |app| app.sync_all_accounts());
    on!(on_remove_account, |app, id| app.remove_account(id));
    on!(on_email_edited, |app, email| app
        .fill_server_defaults(email.as_str()));
    on!(on_add_account, |app, email, password, host, port, smtp| {
        app.add_account(&email, &password, &host, &port, &smtp)
    });
    on!(on_sign_in_google, |app, email, host, port, smtp| {
        app.sign_in(&oauth::GMAIL, &email, &host, &port, &smtp)
    });
    on!(on_sign_in_microsoft, |app, email, host, port, smtp| {
        app.sign_in(&oauth::OUTLOOK, &email, &host, &port, &smtp)
    });
}
