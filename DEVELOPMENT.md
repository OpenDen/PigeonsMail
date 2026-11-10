# Mailthin — Development Guide and Status

Mailthin is a desktop mail client written in Rust with a Slint UI. This document records what has
been built so far, the libraries used, how the work progressed, what is still pending, and how to
build, run and test the app.

> **Status in one line:** Phase 1, Steps 1A–1C of [Implementation_Plan.md](./Implementation_Plan.md)
> are implemented and unit-tested (60 tests pass). Step 1D (compose/send, attachment save/open,
> rules UI) and Phase 2 (Calendar) are not started. The IMAP/SMTP code has **not yet been run
> against a real mail server**; see [Verification status](#6-verification-status).

---

## 1. Contents

1. [Contents](#1-contents)
2. [What exists today](#2-what-exists-today)
3. [Architecture](#3-architecture)
4. [Libraries and why they were chosen](#4-libraries-and-why-they-were-chosen)
5. [Development path](#5-development-path)
6. [Verification status](#6-verification-status)
7. [Pending work](#7-pending-work)
8. [Build, run and test](#8-build-run-and-test)
9. [Where data and secrets live](#9-where-data-and-secrets-live)
10. [Troubleshooting](#10-troubleshooting)
11. [Repository layout](#11-repository-layout)

---

## 2. What exists today

### Feature status

| Area | Status | Notes |
| --- | --- | --- |
| 5-crate workspace | Done | `core`, `store`, `net`, `render`, `app` |
| Encrypted storage (SQLCipher) | Done | Raw 256-bit key kept in the OS keyring |
| Full-text search (FTS5) | Done | Subject, sender, body text; user input is sanitised |
| Body cache with LRU eviction | Done | 300 bodies kept; oldest evicted |
| Account secrets in OS keyring | Done | Windows Credential Manager |
| IMAP login (app password) | Done, untested on a live server | TLS (port 993) via rustls |
| IMAP login (OAuth2 XOAUTH2) | Library done, **not wired to UI** | Gmail and Outlook loopback + PKCE flow |
| Multi-account sync (one Tokio task per account) | Done, untested on a live server | Folder list, 30-day header window, incremental UID sync, UIDVALIDITY handling |
| IMAP IDLE with polling fallback | Done, untested on a live server | IDLE watches INBOX only |
| Rules engine | Done | Priority ordered; runs on every newly fetched message |
| Rule actions applied | Done | Star and mark-read (local + server); archive, delete, move (server move) |
| Unified inbox + per-account folder tree | Done | Unread counts, folder roles |
| Virtualised message list | Done | Only ~4 pages of 50 rows are held in memory |
| Search bar + Unread/Starred/Attachments chips | Done | |
| Reading pane (litehtml) | Done | RGBA viewport, scrolling, resize, link hit-testing |
| Remote image blocking + opt-in load | Done | HTTPS only, size cap, private addresses refused |
| Archive / Delete / Star (`e`, `#`, `s`) | Done, untested on a live server | Delete moves to Trash; deleting from Trash is permanent |
| Attachment list in reader | Done (names and sizes only) | Cannot open or save yet |
| Add Account dialog | Done | App-password IMAP only |
| Reply / Compose / Send | **Not started** | Step 1D |
| Attachment save / open | **Not started** | Step 1D |
| Rules builder UI | **Not started** | Step 1D (rules can only be added through the store API) |
| Calendar | **Not started** | Phase 2 |

### Size of the codebase

About 4,400 lines of Rust and 560 lines of Slint, plus the vendored `litehtml-sys` patch set.

| Crate | Rust lines | Tests |
| --- | --- | --- |
| `core` | ~170 | 5 |
| `store` | ~920 | 12 (+1 opt-in keyring test) |
| `net` | ~1,230 | 13 |
| `render` | ~460 | 11 |
| `app` | ~1,580 (+ 560 Slint) | 19 |

---

## 3. Architecture

### 3.1 Crates and dependency direction

```text
                 +-------+
                 |  app  |  Slint UI, wiring, actions, list model
                 +---+---+
        +------------+------------+-------------+
        v            v            v             v
   +--------+   +--------+   +----------+   +--------+
   | render |   |  net   |   |  store   |   |  core  |
   +--------+   +---+----+   +----+-----+   +--------+
                    |             |              ^
                    +-------------+--------------+
                       (net and store depend on core;
                        net does NOT depend on store)
```

| Crate | Responsibility | Depends on |
| --- | --- | --- |
| `core` | Synchronous domain models, flag constants, rule evaluation. No network or database code. | `serde` |
| `store` | SQLCipher database, schema, FTS5, body cache, queries, OS keyring access. All database access lives here. | `core` |
| `net` | OAuth, IMAP connection and sync, per-account workers, user actions (fetch body, flags, move, expunge), remote image download. | `core` |
| `render` | MIME parsing for display, attachment metadata, HTML rendering to RGBA via litehtml, link hit-testing, link safety filter. | `litehtml`, `mail-parser` |
| `app` | Slint UI, application state, store-writer thread, message list model, action planning, formatting helpers. | all of the above |

`net` never touches the database. It reports what happened as `SyncEvent` values, and the `app`
crate persists them. That keeps account-specific work isolated.

### 3.2 Threads and data flow

```text
 UI thread (Slint)                                   Tokio runtime (2 worker threads)
 +------------------------------+                    +-------------------------------+
 | App state, reading pane,     |  AccountCommand    | one task per account:         |
 | message list model           | -----------------> |  connect -> sync -> IDLE/poll |
 | own SQLCipher connection     |  (mpsc, try_send)  |  executes MailAction          |
 +--------------^---------------+                    +---------------+---------------+
                | UiMsg (mpsc, drained by a 150 ms timer)            | SyncEvent (mpsc)
                |                                                    v
 +--------------+---------------+  on_event callback  +-------------------------------+
 | store-writer thread          | <------------------ | (same channel)                |
 | own SQLCipher connection     |                     +-------------------------------+
 | apply_event() -> store       |
 +--------------+---------------+
                | ServerAction (rule-triggered)
                v
        forwarder thread -> the right account worker
```

* **Two SQLCipher connections** (UI thread and writer thread) share the database file in WAL mode,
  so reads never block behind sync writes. A 5 s `busy_timeout` covers short overlaps.
* **Account workers** run a `tokio::select!` loop: wait in IDLE (or poll every 5 minutes when the
  server has no IDLE), and wake for a server push, a user command, or shutdown. Connection errors
  retry with exponential backoff (5 s up to 5 min), and credentials are re-fetched on every
  reconnect so OAuth tokens can be refreshed.
* **UI updates** from background threads go through one channel that a repeating timer drains on
  the UI thread, so no Slint object is ever touched from another thread.

### 3.3 Database schema (SQLCipher)

Created by `MailStore::open_encrypted` in [crates/store/src/lib.rs](./crates/store/src/lib.rs).

| Table | Purpose |
| --- | --- |
| `accounts` | kind, email, imap host (optionally `host:port`), smtp host, auth type |
| `folders` | per account; role (inbox/sent/drafts/archive/trash/junk/starred), `uidvalidity`, `last_uid` |
| `messages` | headers, flags bit set, snippet, body state; unique on `(folder_id, uid)` |
| `bodies` | raw RFC 822 blob + `last_accessed` for LRU eviction |
| `attachments` | reserved for Step 1D (encrypted save/open) |
| `labels`, `message_labels` | label storage (UI not built) |
| `rules` | rule JSON + priority + account scope |
| `fts` | FTS5, contentless with `contentless_delete=1` (subject, sender, body_text) |
| `events` | reserved for Phase 2 (Calendar) |

Message flags are bits defined in `mailthin_core::flags` (`SEEN`, `FLAGGED`, `ANSWERED`, `DRAFT`,
`DELETED`, `HAS_ATTACHMENT`).

### 3.4 Reading-pane pipeline

1. User selects a message. If its body is cached, it is read from `bodies`; otherwise the app asks
   the account worker to `FetchBody` (`BODY.PEEK[]`, so it is not marked read by the server fetch;
   a 40 MB cap applies).
2. The store writer saves the raw body, indexes its text, and updates the snippet and attachment
   flag.
3. `render::parse_email` selects the HTML body (or escapes the plain text), collects attachment
   metadata and inline `cid:` images.
4. `EmailView` sanitises the HTML (litehtml's `prepare_email_html`), lays it out, and renders only the
   visible slice to an RGBA buffer, which Slint shows as an `Image`.
5. Closing or switching messages drops the `EmailView`, freeing the document and pixel buffer.
   Window resizes rebuild the view after a 120 ms debounce.

---

## 4. Libraries and why they were chosen

Versions are the ones resolved in `Cargo.lock` (the lock file is committed in the repo so builds are
reproducible). Toolchain used: **Rust 1.99.1 (stable, MSVC target)**.

### Core runtime and utilities

| Library | Version | Used for |
| --- | --- | --- |
| `tokio` | 1.54 | Async runtime (2 worker threads), channels, timers, DNS lookup |
| `futures` | 0.3 | Stream helpers (`try_collect`, `buffer_unordered`) |
| `serde`, `serde_json` | 1.0 | Rule (de)serialisation |
| `anyhow` | 1.0 | Error handling |
| `zeroize` | 1.9 | Wipes the DB key and secrets from memory |
| `rand`, `hex` | 0.8, 0.4 | Generating the 256-bit database key |
| `url` | 2.5 | URL parsing and validation |
| `chrono` | 0.4 | Local-time formatting in the UI |
| `tempfile` | 3 | Test databases (dev-dependency) |

### Storage and secrets

| Library | Version | Used for |
| --- | --- | --- |
| `rusqlite` | 0.32 (`bundled-sqlcipher-vendored-openssl`) | SQLite with SQLCipher encryption; builds OpenSSL from source (needs Perl) |
| `libsqlite3-sys` / `openssl-src` | 0.30 / 3.6 | Native SQLCipher and OpenSSL builds |
| `keyring` | 3.6 (`windows-native` only) | Windows Credential Manager for the DB key and account passwords |

### Networking

| Library | Version | Used for |
| --- | --- | --- |
| `async-imap` | 0.12 (`runtime-tokio`) | IMAP client: login, XOAUTH2, IDLE, search, fetch, store, move, expunge |
| `tokio-rustls` / `rustls` / `webpki-roots` | 0.26 / 0.23 / 1.0 | TLS for IMAP (ring crypto provider, Mozilla root certificates) |
| `oauth2` | 5.0 | Authorization code + PKCE flow |
| `tiny_http` | 0.12 | Loopback listener for the OAuth redirect |
| `reqwest` | 0.12 (`rustls-tls`) | Remote image download (and OAuth token requests) |
| `lettre` | 0.11 | **Declared in the workspace, not used yet** (SMTP in Step 1D) |

### Mail parsing and rendering

| Library | Version | Used for |
| --- | --- | --- |
| `mail-parser` | 1.0 | Headers, MIME tree, body selection, attachments, HTML-to-text |
| `litehtml` | 0.2.6 (`vendored`, `pixbuf`, `email`) | HTML/CSS layout and drawing |
| `litehtml-sys` | 0.2.5 (**patched, vendored**) | C++ litehtml bindings; see [vendor/litehtml-sys/PATCHES.txt](./vendor/litehtml-sys/PATCHES.txt) |
| `tiny-skia`, `cosmic-text` | 0.11 / 0.14 | CPU rasteriser and text shaping used by litehtml's pixel-buffer backend |

### UI

| Library | Version | Used for |
| --- | --- | --- |
| `slint` | 1.18 (`backend-winit`, `renderer-femtovg`, `renderer-software`) | UI toolkit |
| `slint-build` | 1.18 | Compiles `ui/main.slint` at build time |
| `open` | 5.4 | Opens confirmed links in the default browser |

### Declared but not used yet

`argon2` and `chacha20poly1305` are in the workspace manifest for the planned encrypted attachment
storage (Step 1D). No crate depends on them yet. `lettre` is the same, waiting for SMTP.

### Differences from the original plan's versions

| Plan said | Used | Reason |
| --- | --- | --- |
| `oauth2` 4.4 | 5.0 | Current release; API differs (typestate client) |
| `mail-parser` 0.9 | 1.0 | Current release; new address and part APIs |
| `PRAGMA key` via `pragma_update` with hex string | Raw-key form `PRAGMA key = "x'<hex>'"` | The plan's form treats the hex as a passphrase and runs a slow KDF |
| FTS5 `content=''` | Added `contentless_delete=1` | Needed to delete/re-index rows without duplicates |
| `evaluate_rules` in listed order | Sorted by `priority` first | The plan's code ignored priority |
| `keyring` default features | `windows-native` only | No default backend is bundled by `keyring` 3 |

---

## 5. Development path

The work followed the plan's order (1A → 1B → 1C) in three working sessions.

### Session 1 — Toolchain, workspace, Step 1A (storage and auth)

1. Found the folder held only the plan. Installed **Rustup** (winget: `Rustlang.Rustup`) and the
   stable toolchain.
2. Created the 5-crate workspace and root `Cargo.toml` (shared dependency versions, release
   profile: `opt-level = "s"`, fat LTO, `panic = "abort"`, stripped).
3. `core`: models, flags, `Rule`/`Condition`/`RuleAction`, `evaluate_rules` (fixed to honour
   priority), with 5 unit tests.
4. `store`: schema, `open_encrypted` (raw key, wrong-key and malformed-key rejection), FTS indexing,
   body cache and LRU eviction, rule persistence; `keystore.rs` for the OS keyring.
5. **Problem:** the vendored OpenSSL build failed with Git's bundled Perl (missing modules).
   **Fix:** installed Strawberry Perl (`StrawberryPerl.StrawberryPerl`); first full build took about
   7 minutes.

### Session 2 — Step 1B (multi-account sync and rules)

1. `net/oauth.rs`: loopback listener on `127.0.0.1:0`, PKCE (S256), CSRF state check, token
   exchange and refresh, XOAUTH2 string builder. Tested including a real HTTP round-trip against
   the listener. Redirects are disabled on the token client.
2. `net/imap.rs`: TLS connect, password and XOAUTH2 login, folder listing with role detection
   (SPECIAL-USE then name heuristics), incremental header sync (last 30 days first, then `UID n:*`),
   UIDVALIDITY reset detection, header parsing, and rule evaluation on every new message.
3. `net/worker.rs`: one task per account, IDLE or polling, commands, backoff reconnect.
4. `net/events.rs`: `SyncEvent` types so `net` stays independent of `store`.
5. `app/sync_bridge.rs`: single store-writer thread applying events. A rule pointing at a
   deleted label caused a foreign-key failure; fixed by making label attachment a no-op when the
   label is missing.
6. **Problem:** the `app` test executable was blocked by a Windows Application Control policy
   (error 4551). **Fix:** moved logic into a `lib.rs` so tests run from a library target.

### Session 3 — Step 1C (reading pane, UI, actions)

1. `render/mime.rs`: body selection, attachment metadata, inline images, filename sanitising,
   preview text for the list and search index.
2. `render/view.rs`: litehtml wrapper, viewport rendering over a white background, scrolling,
   link hit-testing, link scheme filter (http, https, mailto only), remote image reporting.
3. **Problem:** `litehtml-sys` does not build on MSVC. Four separate failures were fixed in a
   vendored, patched copy (`vendor/litehtml-sys`, wired in with `[patch.crates-io]`):
   * missing POSIX `<strings.h>` -> small compat header;
   * source files read as the wrong code page -> `/utf-8` and `/permissive-` flags;
   * a link error on `parse_css_stylesheet<const char*>` -> pass a `std::string`;
   * unconditional `stdc++` link -> skipped on MSVC.
   A fifth patch makes custom CSS **add to** the built-in master CSS instead of replacing it;
   otherwise links lost their pointer cursor and default element styling.
4. `net/imap.rs` actions: body fetch (`BODY.PEEK[]` with a 40 MB cap), flag store, move
   (`MOVE` or copy + delete + expunge), expunge (`UID EXPUNGE` when UIDPLUS exists).
5. `net/remote.rs`: opt-in image downloader with SSRF defences (HTTPS only, no credentials, public
   IPs only including after DNS resolution and on each redirect, image content type only, 5 MB cap,
   10 s timeout).
6. `store/queries.rs`: paged message queries, unified inbox, filters, search, folder tree with
   unread counts, location lookups, flag updates.
7. `app`: `list_model.rs` (virtualised model), `actions.rs` (archive/delete/star planning),
   `format.rs` (dates, names, provider defaults), `app.rs` (wiring), `ui/main.slint` (3-pane layout
   with add-account dialog and overlay banners).
8. Checked the UI by launching the app against a throwaway database, capturing the window, and
   rendering a seeded HTML message offline to confirm layout, link hit-testing and image blocking.
   Interactive clicking could not be completed because another actor was moving the mouse on the
   machine. The temporary seeding and preview code was deleted afterwards.

### Design decisions worth remembering

* **Never delete mail by accident:** deleting moves to Trash; only a message already in Trash is
  expunged; with no Trash folder the action is refused.
* **Remote content is untrusted:** images are blocked until the user opts in; links need a
  confirmation click and are filtered by scheme.
* **Bounded memory:** list pages, cached bodies, remote images per message (24 images, 16 MB) and
  the open message's render buffers are all capped; closing a message frees its document.
* **Secrets:** the DB key and account passwords never touch disk in plaintext and are not logged.
  Message bodies are never logged.

---

## 6. Verification status

| Check | Result |
| --- | --- |
| `cargo test --workspace` | **60 passed**, 0 failed, 1 ignored (keyring test, run separately) |
| Keyring round-trip test (`-- --ignored`) | Passed against the real Windows credential store |
| App launches, opens window, creates encrypted DB | Verified (about 57 MB working set, empty, debug build) |
| HTML render, table, links, safe-link filter, remote-image blocking | Verified by offline render and tests |
| Opening/clicking/scrolling in the live window | **Not verified** (see Session 3 note) |
| IMAP connect, sync, IDLE, fetch, flags, move, expunge against a real server | **Not verified** |
| OAuth flow against Google/Microsoft | **Not verified** (no client IDs available; also not wired to UI) |
| Release build | **Not built** |
| Memory targets (under 120 MB idle / 150 MB active) | **Not measured**; no claim is made |
| 3 accounts syncing concurrently | **Not tested** |

---

## 7. Pending work

### 7.1 Step 1D (next milestone)

* SMTP: build `send_smtp_mail` on `lettre` (rustls), a `SendMail` command, credentials from the
  keyring or an OAuth access token, saving a copy to Sent.
* Compose drawer: To/Cc, subject, body, attachment picker, Send; Reply, Reply all, Forward
  (`r`, `c` shortcuts currently disabled).
* Attachments: save and open from the reader chips (encrypted storage using the declared
  `argon2`/`chacha20poly1305`, or save-to-disk with a user-chosen path).
* Rules builder UI: list, condition/action editors, and the "test against last 50 mails" dry run.

### 7.2 Phase 1 gaps found during Steps 1A–1C

* **Real-server testing** of everything in `net` (highest priority). Start with an app-password
  IMAP account on a throwaway mailbox.
* **OAuth in the UI:** a "Sign in with Google/Microsoft" button, client ID configuration, and a
  `CredentialProvider` that refreshes access tokens and stores the refresh token in the keyring.
  Microsoft has largely disabled basic-auth IMAP for Outlook.com / Microsoft 365, so Outlook
  accounts need this.
* **Server-side changes are not picked up:** flags changed or messages deleted elsewhere are not
  reflected for already-synced messages (needs `CONDSTORE`/`UID FETCH 1:* FLAGS` reconciliation and
  expunge detection).
* **IDLE watches INBOX only.** Other folders are re-synced on the 25-minute IDLE timeout, a manual
  Refresh, or after a move.
* **Attachment detection at list time** is a header heuristic (`multipart/mixed`); it is corrected
  once a body is opened.
* **Label UI:** labels are stored and rules can attach them, but nothing creates or shows labels.
* **Unified inbox "Starred"** virtual folder is not built (starred is a filter chip).
* **Account removal**, editing server settings, and unread badges per account header.
* **Per-folder cursor for large mailboxes:** the first sync fetches headers folder by folder, in
  chunks of 100, with no progress indicator.
* **Pause on offline / network change** handling beyond the backoff loop.
* **Performance and memory measurement** against the plan's targets, on a release build.
* **Packaging:** installer, app icon, release build verification.
* **Cross-platform:** `keyring` is built with the Windows backend only; the `litehtml-sys` patches
  target MSVC. macOS/Linux need keyring features and a build check.
* **CI:** no automated build or test pipeline yet.

### 7.3 Phase 2 (Calendar), not started

Google Calendar REST and CalDAV sync, `icalendar` + `rrule` expansion, Month/Week/Agenda views,
`.ics` invite handling (`METHOD:REPLY`), desktop reminders. The `events` table and OAuth scopes
groundwork exist; the Calendar button in the sidebar is a disabled placeholder.

---

## 8. Build, run and test

These steps are for **Windows 10/11 (x64)**, which is the platform this was developed and checked on.

### 8.1 Prerequisites

| Tool | Why | Install |
| --- | --- | --- |
| Rust (stable, MSVC target) | Compiler and Cargo | `winget install --id Rustlang.Rustup -e` then `rustup default stable` |
| Visual Studio or Build Tools with the **Desktop development with C++** workload | MSVC compiler and linker for SQLCipher, OpenSSL and litehtml | Visual Studio Installer |
| Strawberry Perl | Builds the vendored OpenSSL (Git's bundled Perl is not enough) | `winget install --id StrawberryPerl.StrawberryPerl -e` |
| Git | Source control (optional for building) | `winget install --id Git.Git -e` |

Open a **new** terminal after installing so `PATH` updates apply. The scripts assume Strawberry Perl
is at `C:\Strawberry\perl\bin` and Rust at `%USERPROFILE%\.cargo\bin`.

### 8.2 Build

From the repository root, in PowerShell:

```powershell
# 1. Put Rust and Strawberry Perl on PATH for this terminal (do this in every new terminal)
. .\scripts\dev-env.ps1

# 2. Debug build of everything
cargo build --workspace

# 3. (optional) Optimised release build; slow because of fat LTO. Not yet verified.
cargo build --release -p mailthin-app
```

Expect the **first** build to take several minutes (OpenSSL, SQLCipher, litehtml and Slint are
compiled from source). Later builds are incremental. You will see repeated
`ignoring unknown option '-std:c99'` and `LNK4099 ... ossl_static.pdb` warnings; they are harmless.

### 8.3 Run

```powershell
. .\scripts\dev-env.ps1
cargo run -p mailthin-app
```

Or run the built binary directly: `target\debug\mailthin-app.exe` (release: `target\release\mailthin-app.exe`).

On first launch the app creates the encrypted database and a key in the Windows Credential Manager,
and shows an empty window. To try it with a real mailbox:

1. Click **Add Account**.
2. Enter your email address and an **app password** (not your normal password):
   * Gmail: turn on 2-step verification, create an app password, and make sure IMAP is enabled.
   * Other providers: use the provider's app-password or IMAP password feature.
3. Leave the server fields blank to guess them from the address (for example `imap.gmail.com`), or
   enter them; use `host` and a port field (default 993, implicit TLS only).
4. The status bar shows connecting, syncing and up to date. The last 30 days of headers appear;
   click a message to download and display it.

Keyboard shortcuts (when the search box is not focused): `e` archive, `#` delete, `s` star, `/`
focus search.

To use a separate throwaway data location (recommended while testing):

```powershell
$env:MAILTHIN_DATA_DIR = "$env:TEMP\mailthin-test"
cargo run -p mailthin-app
```

### 8.4 Test

```powershell
. .\scripts\dev-env.ps1

cargo test --workspace                       # all unit tests (60)
cargo test -p mailthin-store -- --ignored    # real OS keyring round-trip (writes and removes one entry)

# A single crate
cargo test -p mailthin-net
cargo test -p mailthin-render
```

### 8.5 Check without building binaries

```powershell
cargo check --workspace
```

---

## 9. Where data and secrets live

| Item | Location |
| --- | --- |
| Database (SQLCipher, WAL mode) | `%APPDATA%\Mailthin\mail.db` (plus `-wal`, `-shm`); override the folder with `MAILTHIN_DATA_DIR` |
| Database key (256-bit, hex) | Windows Credential Manager, service `mailthin`, user `database-key` |
| Account passwords / refresh tokens | Windows Credential Manager, service `mailthin`, user `account:<email>` |
| Cached message bodies | Inside the encrypted database (`bodies` table) |

**Important:** if the database key is deleted from the Credential Manager, the database can no longer
be opened. If you delete the database file but keep the key, the app simply creates a new database.
Nothing secret is committed to the repository; there are no embedded OAuth client IDs.

OAuth scopes requested (only what IMAP/SMTP needs): Gmail `https://mail.google.com/`; Outlook
`IMAP.AccessAsUser.All`, `SMTP.Send`, `offline_access`.

---

## 10. Troubleshooting

| Symptom | Cause and fix |
| --- | --- |
| `cargo` is not recognised | Run `. .\scripts\dev-env.ps1`, or open a new terminal after installing Rust |
| OpenSSL build fails with `Can't locate Locale/Maketext/Simple.pm` | The wrong Perl (Git's) is first on `PATH`; use Strawberry Perl via `dev-env.ps1` |
| `Cannot open include file: 'strings.h'`, `C2015`, `LNK2019 parse_css_stylesheet`, or `cannot open 'stdc++.lib'` | The patched `vendor/litehtml-sys` is not being used. Check the `[patch.crates-io]` entry in the root `Cargo.toml` and that `vendor/litehtml-sys` exists |
| `An Application Control policy has blocked this file (os error 4551)` | Windows policy blocked a freshly built executable. Re-run the command; if it persists, the folder may need an allow rule. Test code runs from library targets for this reason |
| Window text input looks dark/unreadable | The UI forces the light colour scheme; make sure you are on the current build |
| Status bar says `no stored password for <email>` | The account row exists but its keyring entry is missing (for example created by a script). Remove the account row or re-add the account |
| Login fails | Use an app password, confirm IMAP is enabled on the mailbox, and check host and port |
| Messages appear but body never loads | Check the status bar for a server error; large messages over 40 MB are refused by design |
| Remote images stay blocked | Click **Load remote images**; images over 5 MB, non-HTTPS, or on private addresses are refused by design |

---

## 11. Repository layout

```text
Mail_Client/
├── Cargo.toml                 Workspace, shared dependency versions, [patch.crates-io], release profile
├── Cargo.lock                 Locked versions (committed for reproducible builds)
├── Implementation_Plan.md     Original plan (several snippets were corrected during development)
├── AGENTS.md                  Project guidance for AI coding agents
├── DEVELOPMENT.md             This document
├── .gitignore
├── scripts/
│   └── dev-env.ps1            Puts Rust and Strawberry Perl on PATH
├── vendor/
│   └── litehtml-sys/          Locally patched litehtml bindings (see PATCHES.txt)
└── crates/
    ├── core/                  Models, flags, rules engine
    │   └── src/lib.rs
    ├── store/
    │   └── src/
    │       ├── lib.rs         Schema, open/encrypt, messages, FTS, body cache, rules
    │       ├── queries.rs     Paged queries, filters, folder tree, flag updates
    │       └── keystore.rs    OS keyring access
    ├── net/
    │   └── src/
    │       ├── oauth.rs       Loopback OAuth2 + PKCE, XOAUTH2
    │       ├── imap.rs        Connect, folders, header sync, body fetch, flags, move, expunge
    │       ├── worker.rs      Per-account task: sync, IDLE/poll, commands, backoff
    │       ├── events.rs      SyncEvent types
    │       └── remote.rs      Hardened remote image downloader
    ├── render/
    │   └── src/
    │       ├── mime.rs        Body/attachment extraction, previews
    │       └── view.rs        litehtml viewport rendering, hit-testing, link filter
    └── app/
        ├── build.rs           Compiles the Slint UI
        ├── ui/main.slint      3-pane layout, add-account dialog
        └── src/
            ├── main.rs        Entry point
            ├── lib.rs         Module wiring
            ├── app.rs         Application state, reading pane, actions, startup/shutdown
            ├── sync_bridge.rs Store-writer thread, rule-action resolution
            ├── list_model.rs  Virtualised message list model
            ├── actions.rs     Archive/delete/star planning
            └── format.rs      Dates, names, provider defaults, validation
```
