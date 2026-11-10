//! Application wiring: store, account workers, message list, reading pane and actions.
//!
//! All UI state lives on the UI thread in [`App`]. Background work (sync workers, the store
//! writer thread, remote image downloads) reports back through a channel of [`UiMsg`] that a
//! timer drains on the UI thread.
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::rc::{Rc, Weak as RcWeak};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::StreamExt;
use mailthin_core::{flags, AccountId, MessageId};
use mailthin_net::imap::{Credential, ImapSettings};
use mailthin_net::remote::fetch_remote_image;
use mailthin_net::worker::{spawn_account, static_credential, AccountCommand, AccountConfig, AccountHandle, MailAction};
use mailthin_net::{SyncEvent, SyncStatus};
use mailthin_render::{parse_email, EmailView};
use mailthin_store::{keystore, AccountRow, ListScope, MailStore, MessageLocation, MessageQuery, NewAccount};
use slint::{ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, Timer, TimerMode, VecModel};
use tokio::sync::mpsc;
use zeroize::Zeroizing;

use crate::actions::{self, Plan};
use crate::format::{
    folder_title, format_full_date, human_size, looks_like_email, provider_defaults, split_host_port, ServerDefaults,
};
use crate::list_model::MessageListModel;
use crate::sync_bridge::{now_ts, spawn_store_writer, ServerAction};
use crate::{AttachmentChip, MainWindow, MessageItem, SidebarItem};

const POLL_INTERVAL: Duration = Duration::from_millis(150);
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(120);
const SYNC_WINDOW_DAYS: i64 = 30;
/// Remote images kept in memory per open message, so a window resize can restore them.
const MAX_REMOTE_BYTES: usize = 16 * 1024 * 1024;
const MAX_REMOTE_IMAGES: usize = 24;

/// Messages sent from background threads to the UI thread.
enum UiMsg {
    Status { account_id: AccountId, status: SyncStatus },
    FoldersChanged,
    MessagesChanged,
    BodyReady { account_id: AccountId, folder: String, uid: u32 },
    ActionFailed { account_id: AccountId, folder: String, uid: u32, error: String },
    RemoteImages { id: MessageId, images: Vec<(String, Vec<u8>)> },
    LoginChecked { email: String, error: Option<String> },
}

/// A new account waiting for its login test. The password stays on the UI thread until the test passes.
struct PendingAdd {
    email: String,
    password: Zeroizing<String>,
    imap_host: String,
    port: u16,
    smtp_host: String,
}

fn to_ui_msg(event: &SyncEvent) -> Option<UiMsg> {
    Some(match event {
        SyncEvent::Status { account_id, status } => {
            UiMsg::Status { account_id: *account_id, status: status.clone() }
        }
        SyncEvent::Folders { .. } => UiMsg::FoldersChanged,
        SyncEvent::Messages { .. } => UiMsg::MessagesChanged,
        SyncEvent::BodyFetched { account_id, folder, uid, .. } => {
            UiMsg::BodyReady { account_id: *account_id, folder: folder.clone(), uid: *uid }
        }
        SyncEvent::ActionFailed { account_id, folder, uid, error } => UiMsg::ActionFailed {
            account_id: *account_id,
            folder: folder.clone(),
            uid: *uid,
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
    remote_loaded: Vec<(String, Vec<u8>)>,
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
    resize_timer: Timer,
    /// Hosts last filled in from the typed address, so they can be replaced when the address changes.
    auto_hosts: Option<ServerDefaults>,
    /// The account being tested by a login attempt from the add dialog.
    pending_add: Option<PendingAdd>,
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
        let (accounts, folders, unified) = {
            let s = self.store.borrow();
            (
                s.list_accounts().unwrap_or_default(),
                s.folder_tree().unwrap_or_default(),
                s.unified_inbox_unread().unwrap_or(0),
            )
        };
        let mut items = vec![SidebarItem {
            kind: 0,
            title: "Unified Inbox".into(),
            unread: unified as i32,
            folder_id: 0,
            account_id: -1,
        }];
        for acc in &accounts {
            items.push(SidebarItem {
                kind: 1,
                title: acc.email.as_str().into(),
                unread: 0,
                folder_id: -1,
                account_id: acc.id as i32,
            });
            for f in folders.iter().filter(|f| f.account_id == acc.id) {
                items.push(SidebarItem {
                    kind: 2,
                    title: folder_title(&f.name, f.role.as_deref()).into(),
                    unread: f.unread as i32,
                    folder_id: f.id as i32,
                    account_id: -1,
                });
            }
        }
        ui.set_sidebar(ModelRc::new(VecModel::from(items)));
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
        let scope = if selected > 0 { ListScope::Folder(selected as i64) } else { ListScope::UnifiedInbox };
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

    // ----------------------------------------------------------------- reading pane

    fn close_reader(&mut self) {
        self.reader = None;
        self.link_hover = false;
        let Some(ui) = self.ui() else { return };
        ui.set_has_message(false);
        ui.set_selected_message(-1);
        ui.set_loading(false);
        ui.set_reader_error("".into());
        ui.set_body_image(Image::default());
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
            (s.get_message(id), s.message_location(id), s.get_body(id, now_ts()))
        };
        let (Ok(Some(row)), Ok(Some(mut loc)), Ok(body)) = loaded else {
            self.notify("That message is no longer available");
            self.refresh_all();
            return;
        };

        ui.set_selected_message(id as i32);
        ui.set_has_message(true);
        ui.set_msg_subject(if row.subject.is_empty() { "(no subject)".into() } else { row.subject.as_str().into() });
        ui.set_msg_from(row.sender.as_str().into());
        ui.set_msg_to(row.recipients.as_str().into());
        ui.set_msg_date(format_full_date(row.date_ts).into());
        ui.set_msg_starred(row.is_starred());

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
            remote_loaded: Vec::new(),
        });

        match body {
            Some(raw) => self.show_body(&raw),
            None => {
                ui.set_loading(true);
                let key = (loc.account_id, loc.folder.clone(), loc.uid);
                if self.pending_fetches.insert(key) {
                    let action = MailAction::FetchBody { folder: loc.folder.clone(), uid: loc.uid };
                    if !self.send_command(loc.account_id, AccountCommand::Action(action)) {
                        self.pending_fetches.remove(&(loc.account_id, loc.folder, loc.uid));
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
        let scale = ui.window().scale_factor();
        let (w, h) = self.viewport;
        let Some(reader) = self.reader.as_mut() else { return };
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
            .map(|a| AttachmentChip { name: a.filename.as_str().into(), size: human_size(a.size_bytes).into() })
            .collect();
        ui.set_attachments(ModelRc::new(VecModel::from(chips)));

        match EmailView::new(&parsed, w, h, scale) {
            Ok(mut view) => {
                if !reader.remote_loaded.is_empty() {
                    view.load_remote_images(reader.remote_loaded.clone());
                }
                ui.set_reader_error("".into());
                reader.view = Some(view);
            }
            Err(e) => ui.set_reader_error(format!("{e:#}").into()),
        }
        self.render();
    }

    /// Re-creates the view from the cached body (after a resize), keeping scroll position.
    fn rebuild_view(&mut self) {
        let Some(reader) = self.reader.as_ref() else { return };
        if reader.awaiting_body || reader.view.is_none() {
            return;
        }
        let id = reader.loc.id;
        let raw = self.store.borrow_mut().get_body(id, now_ts());
        if let Ok(Some(raw)) = raw {
            self.show_body(&raw);
        }
    }

    fn render(&mut self) {
        let Some(ui) = self.ui() else { return };
        let (_, viewport_h) = self.viewport;
        let Some(reader) = self.reader.as_mut() else { return };
        let Some(view) = reader.view.as_mut() else { return };

        let total = view.content_height();
        reader.scroll = reader.scroll.min(total.saturating_sub(viewport_h));
        let frame = view.render_viewport(reader.scroll);

        let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&frame.rgba, frame.width, frame.height);
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
        self.resize_timer.start(TimerMode::SingleShot, RESIZE_DEBOUNCE, move || {
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
        let Some(reader) = self.reader.as_mut() else { return };
        let Some(view) = reader.view.as_mut() else { return };
        let over = view.hover_is_link(x, y, reader.scroll);
        if over != self.link_hover {
            self.link_hover = over;
            if let Some(ui) = self.ui() {
                ui.set_body_link_hover(over);
            }
        }
    }

    fn on_clicked(&mut self, x: f32, y: f32) {
        let Some(reader) = self.reader.as_mut() else { return };
        let Some(view) = reader.view.as_mut() else { return };
        if let Some(url) = view.link_at(x, y, reader.scroll) {
            reader.pending_link = Some(url.clone());
            if let Some(ui) = self.ui() {
                ui.set_pending_link(url.into());
            }
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
        let Some(reader) = self.reader.as_ref() else { return };
        let Some(view) = reader.view.as_ref() else { return };
        let urls: Vec<String> = view.remote_images().iter().take(MAX_REMOTE_IMAGES).cloned().collect();
        if urls.is_empty() {
            return;
        }
        ui.set_remote_loading(true);
        let id = reader.loc.id;
        let tx = self.ui_tx.clone();
        self.rt.spawn(async move {
            let images: Vec<(String, Vec<u8>)> = futures::stream::iter(urls)
                .map(|u| async move {
                    let data = fetch_remote_image(&u).await.ok()?;
                    Some((u, data))
                })
                .buffer_unordered(4)
                .filter_map(futures::future::ready)
                .collect()
                .await;
            let _ = tx.send(UiMsg::RemoteImages { id, images });
        });
    }

    fn remote_images_arrived(&mut self, id: MessageId, images: Vec<(String, Vec<u8>)>) {
        let Some(ui) = self.ui() else { return };
        ui.set_remote_loading(false);
        let Some(reader) = self.reader.as_mut() else { return };
        if reader.loc.id != id {
            return;
        }
        let failed = images.len() < reader.view.as_ref().map_or(0, |v| v.remote_images().len().min(MAX_REMOTE_IMAGES));
        if let Some(view) = reader.view.as_mut() {
            view.load_remote_images(images.clone());
        }
        let mut total: usize = reader.remote_loaded.iter().map(|(_, d)| d.len()).sum();
        for (url, data) in images {
            if total + data.len() <= MAX_REMOTE_BYTES {
                total += data.len();
                reader.remote_loaded.push((url, data));
            }
        }
        self.render();
        if failed {
            self.notify("Some remote images could not be loaded");
        }
    }

    // ----------------------------------------------------------------- actions

    fn toggle_star(&mut self) {
        let Some(reader) = self.reader.as_ref() else { return };
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
        let Some(reader) = self.reader.as_ref() else { return };
        let loc = reader.loc.clone();
        let plan = {
            let s = self.store.borrow();
            if delete { actions::plan_delete(&s, &loc) } else { actions::plan_archive(&s, &loc) }
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
            UiMsg::BodyReady { account_id, folder, uid } => {
                self.pending_fetches.remove(&(account_id, folder.clone(), uid));
                let matches = self.reader.as_ref().is_some_and(|r| {
                    r.awaiting_body && r.loc.account_id == account_id && r.loc.folder == folder && r.loc.uid == uid
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
            UiMsg::ActionFailed { account_id, folder, uid, error } => {
                let was_fetch = self.pending_fetches.remove(&(account_id, folder.clone(), uid));
                if was_fetch {
                    let current = self.reader.as_ref().is_some_and(|r| {
                        r.awaiting_body && r.loc.account_id == account_id && r.loc.folder == folder && r.loc.uid == uid
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
            UiMsg::RemoteImages { id, images } => self.remote_images_arrived(id, images),
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
        let secret = keystore::get_account_secret(&row.email)?
            .ok_or_else(|| anyhow!("no stored password for {}", row.email))?;
        let (host, port) = split_host_port(&row.imap_host, 993);
        let (rules, folder_states) = {
            let s = self.store.borrow();
            (s.load_rules(row.id)?, s.folder_states(row.id)?)
        };
        let _guard = self.rt.enter();
        let handle = spawn_account(
            AccountConfig {
                account_id: row.id,
                settings: ImapSettings { host, port, username: row.email.clone() },
                credentials: static_credential(Credential::Password(secret)),
                rules,
                folder_states,
                sync_window_days: SYNC_WINDOW_DAYS,
            },
            self.events_tx.clone(),
        );
        self.handles.lock().map_err(|_| anyhow!("account table poisoned"))?.insert(row.id, handle);
        self.statuses.insert(row.id, (row.email.clone(), SyncStatus::Connecting));
        Ok(())
    }

    /// Fills the server fields from the typed address. A field is only replaced when it is blank
    /// or still holds the previous automatic guess, so hosts the user typed are never overwritten.
    fn fill_server_defaults(&mut self, email: &str) {
        let Some(ui) = self.ui() else { return };
        let guess = provider_defaults(email);
        let previous = self.auto_hosts.take();
        let replaceable = |current: &str, prev: Option<&str>| current.trim().is_empty() || prev == Some(current);

        let imap = ui.get_acc_imap_host();
        if replaceable(imap.as_str(), previous.as_ref().map(|d| d.imap_host.as_str())) {
            ui.set_acc_imap_host(guess.as_ref().map(|d| d.imap_host.clone()).unwrap_or_default().into());
        }
        let smtp = ui.get_acc_smtp_host();
        if replaceable(smtp.as_str(), previous.as_ref().map(|d| d.smtp_host.as_str())) {
            ui.set_acc_smtp_host(guess.as_ref().map(|d| d.smtp_host.clone()).unwrap_or_default().into());
        }
        self.auto_hosts = guess;
    }

    fn add_account(&mut self, email: &str, password: &str, imap_host: &str, port: &str, smtp_host: &str) {
        let Some(ui) = self.ui() else { return };
        match self.prepare_add_account(email, password, imap_host, port, smtp_host) {
            Ok(pending) => {
                ui.set_add_account_error("Checking the login...".into());
                self.check_login(pending);
            }
            Err(e) => ui.set_add_account_error(format!("{e:#}").into()),
        }
    }

    /// Tests the IMAP login on a background task. The account is saved only if the test passes.
    fn check_login(&mut self, pending: PendingAdd) {
        let settings = ImapSettings {
            host: pending.imap_host.clone(),
            port: pending.port,
            username: pending.email.clone(),
        };
        let credential = Credential::Password(pending.password.clone());
        let email = pending.email.clone();
        self.pending_add = Some(pending);
        let tx = self.ui_tx.clone();
        self.rt.spawn(async move {
            let error = match mailthin_net::imap::connect(&settings, &credential).await {
                Ok(mut session) => {
                    let _ = session.logout().await;
                    None
                }
                Err(e) => Some(format!("{e:#}")),
            };
            let _ = tx.send(UiMsg::LoginChecked { email, error });
        });
    }

    fn finish_add_account(&mut self, email: &str, error: Option<String>) {
        let Some(ui) = self.ui() else { return };
        if !self.pending_add.as_ref().is_some_and(|p| p.email == email) {
            return;
        }
        let Some(pending) = self.pending_add.take() else { return };
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

    fn prepare_add_account(&self, email: &str, password: &str, imap_host: &str, port: &str, smtp_host: &str) -> Result<PendingAdd> {
        let email = email.trim();
        if !looks_like_email(email) {
            return Err(anyhow!("Enter a valid email address"));
        }
        if password.is_empty() {
            return Err(anyhow!("Enter the account's app password"));
        }
        let defaults = provider_defaults(email).ok_or_else(|| anyhow!("Cannot guess the mail servers"))?;
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
            p => p.parse().map_err(|_| anyhow!("The IMAP port must be a number"))?,
        };
        if self.store.borrow().list_accounts()?.iter().any(|a| a.email.eq_ignore_ascii_case(email)) {
            return Err(anyhow!("That account is already added"));
        }
        Ok(PendingAdd {
            email: email.into(),
            password: Zeroizing::new(password.to_string()),
            imap_host: host,
            port,
            smtp_host: smtp,
        })
    }

    fn save_account(&mut self, pending: PendingAdd) -> Result<()> {
        let PendingAdd { email, password, imap_host, port, smtp_host } = pending;
        keystore::set_account_secret(&email, password.as_str()).context("cannot save the password to the OS keyring")?;
        let stored_host = if port == 993 { imap_host } else { format!("{imap_host}:{port}") };
        let id = match self.store.borrow().add_account(&NewAccount {
            kind: "imap",
            email: &email,
            imap_host: &stored_host,
            smtp_host: &smtp_host,
            auth_type: "password",
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
            auth_type: "password".into(),
        };
        self.start_account(&row)
    }

    fn sync_all_accounts(&mut self) {
        let ids: Vec<AccountId> = self.handles.lock().map(|h| h.keys().copied().collect()).unwrap_or_default();
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
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/mailthin")))
        .ok_or_else(|| anyhow!("cannot determine a data directory"))?;
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    Ok(dir)
}

pub fn run() -> Result<()> {
    let db_path = data_dir()?.join("mail.db");
    let db_path = db_path.to_string_lossy().into_owned();
    let key = keystore::get_or_create_db_key()?;

    // The writer thread owns one connection; the UI thread uses a second one (WAL mode).
    let writer_store = MailStore::open_encrypted(&db_path, &key)?;
    let ui_store = Rc::new(RefCell::new(MailStore::open_encrypted(&db_path, &key)?));
    drop(key);

    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
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
    ui.set_messages(ModelRc::from(model.clone() as Rc<dyn Model<Data = MessageItem>>));
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
            resize_timer: Timer::default(),
            auto_hosts: None,
            pending_add: None,
            self_ref: weak.clone(),
        })
    });

    {
        let mut app = shared.borrow_mut();
        let accounts = app.store.borrow().list_accounts()?;
        for row in &accounts {
            if let Err(e) = app.start_account(row) {
                app.statuses.insert(row.id, (row.email.clone(), SyncStatus::Error(format!("{e:#}"))));
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
    let workers: Vec<AccountHandle> = handles.lock().map(|mut h| h.drain().map(|(_, w)| w).collect()).unwrap_or_default();
    let stopped = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(3), futures::future::join_all(workers.into_iter().map(|w| w.shutdown())))
            .await
            .is_ok()
    });
    drop(events_tx);
    if stopped {
        let _ = writer.join();
    }
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
    on!(on_select_message, |app, id| app.open_message(id));
    on!(on_search_changed, |app, _text| app.apply_query());
    on!(on_filters_changed, |app| app.apply_query());
    on!(on_viewport_resized, |app, w, h| app.on_viewport_resized(w, h));
    on!(on_body_scrolled, |app, dy| app.on_scrolled(dy));
    on!(on_body_hover, |app, x, y| app.on_hover(x, y));
    on!(on_body_clicked, |app, x, y| app.on_clicked(x, y));
    on!(on_toggle_remote, |app| app.load_remote_images());
    on!(on_open_link, |app| app.open_pending_link());
    on!(on_dismiss_link, |app| app.dismiss_link());
    on!(on_star, |app| app.toggle_star());
    on!(on_archive, |app| app.triage(false));
    on!(on_delete_message, |app| app.triage(true));
    on!(on_refresh, |app| app.sync_all_accounts());
    on!(on_remove_account, |app, id| app.remove_account(id));
    on!(on_email_edited, |app, email| app.fill_server_defaults(email.as_str()));
    on!(on_add_account, |app, email, password, host, port, smtp| {
        app.add_account(&email, &password, &host, &port, &smtp)
    });
}
