Consolidating all email features into **Phase 1** and isolating Calendar into **Phase 2** requires building the final 5-crate workspace immediately. Instead of writing throwaway spike code, you build the complete encrypted storage, multi-account sync engine, rules processor, and SMTP composer right out of the gate.

---

### Consolidated 2-Phase Roadmap

| Phase | Scope | Core Subsystems Built | Hard Exit Criteria |
| --- | --- | --- | --- |
| **Phase 1: Complete Mail Client** | Multi-account (Gmail/Outlook OAuth2 + Generic IMAP/SMTP), Unified Inbox, SQLCipher + FTS5 Search, Lazy Body & Attachment LRU Cache, `litehtml` Viewport Renderer, Compose/Reply/Forward, Rules & Labels Engine, Neumorphic/Simple Slint UI. | `mailthin-core`<br>

<br>`mailthin-store`<br>

<br>`mailthin-net`<br>

<br>`mailthin-render`<br>

<br>`mailthin-app` | 3 accounts syncing concurrently via IDLE; full offline encrypted search & read; rules executing on arrival; sending mail with attachments; **RSS under 120 MB idle / 150 MB active**. |
| **Phase 2: Calendar & Invites** | Google Calendar REST API + Generic CalDAV sync, `icalendar` + `rrule` recurrence expansion, Month/Week/Agenda Slint views, automatic `.ics` invite parsing in emails (`METHOD:REPLY`), desktop reminders (`notify-rust`). | Extends `store`, `net`, and `app` using existing OAuth tokens & SQLCipher DB. | Offline calendar views populated; accepting an `.ics` email invite updates the server and local calendar; background reminders fire cleanly. |

---

### Phase 1: Full Workspace & Crate Architecture

Set up the final 5-crate repository structure from day one so `Phase 2` simply adds modules without refactoring:

```text
mailthin/
├── Cargo.toml
└── crates/
    ├── core/       # Data models, Rules Engine, Command/Event enums
    ├── store/      # SQLCipher + FTS5 + OS Keyring + Attachment LRU
    ├── net/        # Multi-account Actor pool (IMAP IDLE, SMTP, OAuth2)
    ├── render/     # MIME parser + litehtml viewport rasterizer + link hit-test
    └── app/        # Slint UI (Sidebar, Unified List, Reader, Composer, Rules UI)

```

#### Root `Cargo.toml`

Centralizes all Phase 1 dependencies so versions stay locked across your five crates.

```toml
[workspace]
members = [
    "crates/core",
    "crates/store",
    "crates/net",
    "crates/render",
    "crates/app",
]
resolver = "2"

[workspace.package]
version = "0.1.0"
edition = "2021"

[workspace.dependencies]
anyhow = "1.0"
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
tokio = { version = "1.40", features = ["rt-multi-thread", "net", "sync", "time", "macros", "fs"] }
rusqlite = { version = "0.32", features = ["bundled-sqlcipher-vendored-openssl", "backup"] }
keyring = "3.2"
argon2 = "0.5"
chacha20poly1305 = "0.10"
zeroize = { version = "1.8", features = ["derive"] }
async-imap = { version = "0.12", default-features = false, features = ["runtime-tokio"] }
lettre = { version = "0.11", default-features = false, features = ["tokio1-rustls-tls", "smtp-transport", "builder"] }
mail-parser = "0.9"
oauth2 = "4.4"
tiny_http = "0.12"
litehtml = { version = "0.2.6", features = ["vendored", "pixbuf", "email"] }
slint = { version = "1.8", default-features = false, features = ["compat-1-2", "std", "backend-winit", "renderer-femtovg", "renderer-software"] }

[profile.release]
opt-level = "s"
lto = "fat"
codegen-units = 1
panic = "abort"
strip = true

```

---

### Core Implementation Blueprint for Phase 1

#### 1. `crates/core` — Domain Models, Commands & Rules Engine

Keep `core` 100% synchronous and free of network or database dependencies. It defines the message bus protocols (`BackendCmd` and `UiEvent`) and evaluates user rules on incoming messages before they hit the UI.

```rust
// crates/core/src/lib.rs
use serde::{Deserialize, Serialize};

pub type AccountId = i64;
pub type FolderId = i64;
pub type MessageId = i64;
pub type LabelId = i64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageSummary {
    pub id: MessageId,
    pub account_id: AccountId,
    pub folder_id: FolderId,
    pub uid: u32,
    pub from: String,
    pub to: String,
    pub subject: String,
    pub date_ts: i64,
    pub snippet: String,
    pub is_read: bool,
    pub is_starred: bool,
    pub has_attachment: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutgoingMail {
    pub account_id: AccountId,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub body_text: String,
    pub body_html: Option<String>,
    pub attachments: Vec<(String, String, Vec<u8>)>, // (filename, mime, bytes)
}

// Rules Engine
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub id: i64,
    pub name: String,
    pub enabled: bool,
    pub priority: i32,
    pub match_all: bool,
    pub conditions: Vec<Condition>,
    pub actions: Vec<RuleAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Condition {
    FromContains(String),
    ToContains(String),
    SubjectContains(String),
    HasAttachment,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RuleAction {
    MoveToFolder(FolderId),
    AddLabel(LabelId),
    MarkRead,
    Star,
    Delete,
    Archive,
    StopProcessing,
}

impl Rule {
    pub fn matches(&self, msg: &MessageSummary) -> bool {
        if !self.enabled || self.conditions.is_empty() {
            return false;
        }
        let eval = |c: &Condition| match c {
            Condition::FromContains(s) => msg.from.to_lowercase().contains(&s.to_lowercase()),
            Condition::ToContains(s) => msg.to.to_lowercase().contains(&s.to_lowercase()),
            Condition::SubjectContains(s) => msg.subject.to_lowercase().contains(&s.to_lowercase()),
            Condition::HasAttachment => msg.has_attachment,
        };
        if self.match_all {
            self.conditions.iter().all(eval)
        } else {
            self.conditions.iter().any(eval)
        }
    }
}

pub fn evaluate_rules(rules: &[Rule], msg: &MessageSummary) -> Vec<RuleAction> {
    let mut triggered = Vec::new();
    for rule in rules {
        if rule.matches(msg) {
            for action in &rule.actions {
                triggered.push(action.clone());
                if matches!(action, RuleAction::StopProcessing) {
                    return triggered;
                }
            }
        }
    }
    triggered
}

```

---

#### 2. `crates/store` — Encrypted SQLCipher, FTS5 & Lazy Body Cache

Initializes the entire mail schema (plus the empty `events` table reserved for Phase 2 so you don't need a schema migration next month). All queries run on a dedicated single-writer background thread to prevent SQLite lock contention across multiple syncing accounts.

```rust
// crates/store/src/lib.rs
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use zeroize::Zeroizing;

pub struct MailStore {
    conn: Connection,
}

impl MailStore {
    pub fn open_encrypted(db_path: &str, raw_key_hex: Zeroizing<String>) -> Result<Self> {
        let conn = Connection::open(db_path).context("Failed to open SQLCipher DB")?;
        
        // Apply 256-bit key immediately before any schema operations
        conn.pragma_update(None, "key", &*raw_key_hex)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        conn.pragma_update(None, "cache_size", -8000)?; // Cap SQLite page cache at ~8 MB

        let store = Self { conn };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS accounts (
                id INTEGER PRIMARY KEY,
                kind TEXT NOT NULL,
                email TEXT NOT NULL UNIQUE,
                imap_host TEXT NOT NULL,
                smtp_host TEXT NOT NULL,
                auth_type TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS folders (
                id INTEGER PRIMARY KEY,
                account_id INTEGER NOT NULL,
                name TEXT NOT NULL,
                role TEXT,
                uidvalidity INTEGER NOT NULL DEFAULT 0,
                last_uid INTEGER NOT NULL DEFAULT 0,
                UNIQUE(account_id, name)
            );

            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY,
                account_id INTEGER NOT NULL,
                folder_id INTEGER NOT NULL,
                uid INTEGER NOT NULL,
                msgid TEXT,
                thread_id TEXT,
                sender TEXT NOT NULL,
                recipients TEXT NOT NULL,
                subject TEXT NOT NULL,
                date_ts INTEGER NOT NULL,
                flags INTEGER NOT NULL DEFAULT 0,
                snippet TEXT NOT NULL DEFAULT '',
                body_state INTEGER NOT NULL DEFAULT 0,
                UNIQUE(folder_id, uid)
            );

            CREATE TABLE IF NOT EXISTS bodies (
                message_id INTEGER PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
                raw_blob BLOB NOT NULL,
                last_accessed INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS attachments (
                id INTEGER PRIMARY KEY,
                message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                mime TEXT NOT NULL,
                size INTEGER NOT NULL,
                inline_blob BLOB,
                encrypted_path TEXT
            );

            CREATE TABLE IF NOT EXISTS labels (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                color TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS message_labels (
                message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
                label_id INTEGER NOT NULL REFERENCES labels(id) ON DELETE CASCADE,
                PRIMARY KEY(message_id, label_id)
            );

            CREATE TABLE IF NOT EXISTS rules (
                id INTEGER PRIMARY KEY,
                account_id INTEGER,
                enabled INTEGER NOT NULL DEFAULT 1,
                priority INTEGER NOT NULL DEFAULT 0,
                rule_json TEXT NOT NULL
            );

            -- Full-Text Search (Encrypted automatically inside SQLCipher)
            CREATE VIRTUAL TABLE IF NOT EXISTS fts USING fts5(
                subject,
                sender,
                body_text,
                content=''
            );

            -- Phase 2 Reserved Table (Calendar)
            CREATE TABLE IF NOT EXISTS events (
                id INTEGER PRIMARY KEY,
                account_id INTEGER NOT NULL,
                uid TEXT NOT NULL,
                start_ts INTEGER NOT NULL,
                end_ts INTEGER NOT NULL,
                rrule TEXT,
                summary TEXT NOT NULL,
                location TEXT,
                etag TEXT,
                UNIQUE(account_id, uid)
            );
            ",
        )?;
        Ok(())
    }

    /// Evicts oldest cached message bodies to keep disk and memory lean (LRU)
    pub fn evict_old_bodies(&self, max_cached_bodies: usize) -> Result<()> {
        self.conn.execute(
            "DELETE FROM bodies WHERE message_id IN (
                SELECT message_id FROM bodies
                ORDER BY last_accessed DESC
                LIMIT -1 OFFSET ?1
            )",
            params![max_cached_bodies as i64],
        )?;
        Ok(())
    }
}

```

---

#### 3. `crates/net` — Multi-Account Sync Actors, Action Queue & SMTP

To support multiple accounts (Gmail XOAUTH2, Outlook XOAUTH2, and generic App Password IMAP) without spiking RAM, spawn **one lightweight async task per account** on your 2-thread Tokio pool.

Each account worker runs a `tokio::select!` loop balancing two responsibilities:

1. **Server-to-Local (IMAP IDLE):** Listens for push notifications of new arrivals, downloads headers, runs `mailthin_core::evaluate_rules()`, and sends a batch write to the `store` channel.
2. **Local-to-Server (User Actions & Lazy Fetch):** When you click an uncached email, star/delete/archive a message, or hit "Send" in the composer, the worker interrupts IDLE, executes the IMAP `FETCH`/`MOVE`/`STORE` or `lettre` SMTP transmission, and resumes IDLE.

```rust
// crates/net/src/lib.rs
use anyhow::Result;
use lettre::{
    message::{header::ContentType, Attachment, MultiPart, SinglePart},
    transport::smtp::{authentication::Credentials, AsyncSmtpTransport, Tokio1Executor},
    AsyncTransport, Message,
};
use mailthin_core::{AccountId, MessageId, OutgoingMail};

pub enum NetCommand {
    FetchBody { account_id: AccountId, folder: String, uid: u32, msg_id: MessageId },
    ApplyFlags { account_id: AccountId, folder: String, uid: u32, add_flags: Vec<String>, remove_flags: Vec<String> },
    MoveMessage { account_id: AccountId, from_folder: String, to_folder: String, uid: u32 },
    SendMail { mail: OutgoingMail, smtp_host: String, user: String, secret: String },
}

/// Sends an email with optional attachments via Lettre + Rustls
pub async fn send_smtp_mail(
    mail: OutgoingMail,
    smtp_host: &str,
    user: &str,
    secret: &str,
) -> Result<()> {
    let mut builder = Message::builder()
        .from(user.parse()?)
        .subject(&mail.subject);

    for rcpt in &mail.to {
        builder = builder.to(rcpt.parse()?);
    }
    for cc in &mail.cc {
        builder = builder.cc(cc.parse()?);
    }

    let body_part = if let Some(html) = mail.body_html {
        MultiPart::alternative_plain_html(mail.body_text, html)
    } else {
        MultiPart::mixed().singlepart(SinglePart::plain(mail.body_text))
    };

    let email = if mail.attachments.is_empty() {
        builder.multipart(body_part)?
    } else {
        let mut mixed = MultiPart::mixed().multipart(body_part);
        for (filename, mime_str, bytes) in mail.attachments {
            let ct = ContentType::parse(&mime_str).unwrap_or(ContentType::TEXT_PLAIN);
            mixed = mixed.singlepart(Attachment::new(filename).body(bytes, ct));
        }
        builder.multipart(mixed)?
    };

    let creds = Credentials::new(user.to_string(), secret.to_string());
    let mailer: AsyncSmtpTransport<Tokio1Executor> =
        AsyncSmtpTransport::<Tokio1Executor>::relay(smtp_host)?
            .credentials(creds)
            .build();

    mailer.send(email).await?;
    Ok(())
}

```

---

#### 4. `crates/render` — Viewport Rasterizer + Link Hit-Testing

Upgrade the renderer from the spike so it handles **Link Hit-Testing** (`anchor_at(x, y)`), **Remote Image Blocking Toggle** ("Load remote images for this message"), and **Attachment Metadata Extraction** so the UI can display downloadable attachment chips at the top of the reading pane.

```rust
// crates/render/src/lib.rs
pub struct RenderedEmail {
    pub viewport_rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub total_height: u32,
    pub attachments: Vec<AttachmentMeta>,
    pub blocked_remote_images: usize,
}

pub struct AttachmentMeta {
    pub part_index: usize,
    pub filename: String,
    pub mime: String,
    pub size_bytes: usize,
}

```

* **How Link Clicking Works without a DOM:** Keep the parsed `litehtml::Document` alive in the render worker ONLY for the currently open email. When the user clicks at `(mouse_x, mouse_y)` on the Slint `Image` element, Slint sends `ClickViewport(x, y + scroll_y)` to the render worker. The worker checks the `<a>` bounding boxes in `litehtml`, returns the target `href` URL to Slint to display a confirmation tooltip, and opens it via the OS default browser. When the user selects another email or closes the pane, the `litehtml::Document` and `SharedPixelBuffer` are dropped immediately.

---

#### 5. `crates/app` — Complete Phase 1 Slint UI Layout

Your `.slint` UI now incorporates all Phase 1 views inside a single window using state-driven visibility (avoiding multi-window memory overhead):

1. **Left Sidebar (`220px`):**
* **Unified Inbox Toggle** at the very top (queries `messages` across all `account_id`s where `folder.role = 'inbox'`).
* **Per-Account Folder Trees** (`Inbox`, `Starred`, `Sent`, `Drafts`, `Archive`, `Trash`, plus custom Labels).
* Bottom action bar: `Compose (c)`, `Rules`, `Settings / Add Account`, and the Phase 2 placeholder tab `Calendar`.


2. **Center Pane (`320px`):**
* **FTS5 Search Bar (`/`)** at the top with instant filter chips (`Unread`, `Starred`, `Has Attachment`).
* **Virtual `ListView**` bound to a sliding window of `MessageSummary` structs so even a 50,000-email inbox only holds ~100 row structs in Slint memory.


3. **Right Reading Pane (Flexible Stretch):**
* **Action Toolbar:** `Reply (r)`, `Archive (e)`, `Delete (#)`, `Star`, `Load Remote Images`, and Attachment chips.
* **Viewport Image Canvas:** Displays the `litehtml` RGBA slice and forwards mouse wheel, hover, and click coordinates.


4. **Slide-Over Modals (Zero-RAM when closed):**
* **Compose Drawer:** To/Cc, Subject, plain/markdown editor, attachment picker, and Send button.
* **Rules Builder Modal:** List of rules, condition/action dropdowns, and the **"Test against last 50 mails"** dry-run preview.



---

### Execution Order Within Phase 1 (4 Milestones, 1 Unified Deliverable)

To build all of Phase 1 cleanly without getting blocked by interconnected parts, write the code inside your 5 crates in this exact sequence:

1. **Step 1A — Storage & Auth (`store` + `net/oauth`):**
Bring up `MailStore::open_encrypted` with OS `keyring` key generation, plus the loopback OAuth2 listener (`tiny_http`) for Gmail/Outlook and App Password storage.
2. **Step 1B — Multi-Account Sync & Rules (`core` + `net/imap`):**
Wire the per-account Tokio tasks to sync folder lists and the last 30 days of headers into `messages` and `fts`, running `evaluate_rules()` on every new UID fetched.
3. **Step 1C — Reading Pane & Actions (`render` + `app`):**
Connect the Slint Unified Inbox and folder views to `store`, lazy-fetch bodies into `bodies` on click, render the viewport via `litehtml`, and wire `e` (Archive), `#` (Delete), and `s` (Star) to push flags back to IMAP.
4. **Step 1D — Compose, Attachments & Rules UI (`net/smtp` + `app`):**
Hook up the Slint Compose modal to `send_smtp_mail()`, add encrypted attachment save/open, and wire the Rules Builder UI to the `rules` table.

---

### How Phase 2 (Calendar) Plugs In Later with Zero Refactoring

Because Phase 1 already sets up:

* Google/Microsoft OAuth2 loopback flows in `crates/net` (just include the `[https://www.googleapis.com/auth/calendar](https://www.googleapis.com/auth/calendar)` scope during Phase 1 login),
* The encrypted `events` table in `crates/store`, and
* The 2-thread Tokio worker + `mpsc` bridge in `crates/app`,

**Phase 2** simply adds:

1. `crates/net/src/calendar.rs` (`reqwest` calls to Google Calendar REST API + generic CalDAV XML/ICS sync).
2. `crates/core/src/calendar.rs` (`icalendar` parsing + `rrule` recurrence expansion into day/week slots + `METHOD:REPLY` generator for `.ics` email attachments).
3. The `CalendarView` component inside `crates/app/ui/main.slint` (toggled via the sidebar tab) and a background timer task calling `notify-rust` for upcoming event reminders.