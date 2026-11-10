# PigeonMail Development Plan

This plan covers what is implemented, what still needs live or manual verification, and what
remains for later phases. For setup, libraries, protocols and data storage, see
[README.md](./README.md).

## Status

| Area | Status |
| --- | --- |
| Storage, FTS search, keyring secrets | Implemented and unit-tested |
| Gmail OAuth | Sign-in and account creation verified; refresh and restart pending |
| Outlook OAuth | Implemented; needs a Microsoft client ID and live test |
| IMAP sync, IDLE, body fetch, server actions, reconciliation | Implemented; live-mailbox verification pending |
| Reader | Implemented; interactive Windows testing pending |
| Compose, send, reply/forward, attachments, rules, labels, server settings | Implemented; live and interactive verification pending |
| Release build | Builds successfully; runtime, installer and performance unverified |
| Phase 2 (Calendar) | Not started |

Focused tests for the five crates pass (90 tests, one keyring test ignored). Passing tests do not
replace live-server, interactive UI, release-runtime or performance checks.

## Implemented (Phase 1)

### 1A: Storage and authentication

- [x] SQLCipher database with a keyring-held key, WAL mode and FTS search.
- [x] Keyring storage for passwords and OAuth refresh tokens.
- [x] OAuth 2.0 authorization code flow with PKCE and a loopback listener.
- [x] Gmail account creation (verified).

### 1B: Sync and rules

- [x] Per-account folder and header sync (last 30 days), Inbox IDLE with polling fallback, backoff.
- [x] Rule evaluation on arrival (from, to, subject, attachment conditions).
- [x] Reconciliation of server-side flag changes, moves and deletions for already-synced messages.
  Uses `UID SEARCH ALL` and chunked `UID FLAGS` fetches; removes local messages missing on the server.

### 1C: Reader and actions

- [x] On-demand full-body fetch (`BODY.PEEK[]`, 40 MiB limit) with a separate connection lane.
- [x] Encrypted body cache (300 bodies) and a 4-page list cache of 50 rows.
- [x] HTML and plain-text rendering with litehtml; viewport drawing; resize without reloading.
- [x] Link confirmation, text selection and Ctrl+C copy.
- [x] Remote images blocked by default; opt-in HTTPS loading with per-response and per-message limits.
- [x] Archive, delete (to Trash; permanent from Trash), star.

### 1D: Complete mail workflows

- [x] SMTP send over STARTTLS (port 587) with explicit errors and recovery of failed sends.
- [x] Sent copy appended to the Sent folder; a failed copy is reported separately from the send.
- [x] Compose with To/Cc, subject, Markdown formatting controls and a rendered preview.
- [x] Reply, reply all and forward, with recipients and threading headers.
- [x] Attachment picking (native dialog) with count and size limits; sanitized filenames.
- [x] Attachment save to a chosen path, then an explicit confirmation before opening.
- [x] Rule editing with a dry run against the latest 50 messages.
- [x] Label management and display.
- [x] Editable IMAP host/port and SMTP host settings.

Starred is a filter, not a dedicated folder. The composer is Markdown with a preview, not an
inline WYSIWYG editor; raw HTML and images are dropped and only http(s)/mailto links are kept.

## Manual verification (Phase 1)

These require a real mailbox or an interactive Windows session. No live or interactive
verification has been recorded for the items below.

### Mail accounts and sync

- [ ] Gmail sync, Inbox IDLE, opening cached and uncached messages, reconnects, archive/delete/star.
- [ ] Outlook (Microsoft) OAuth sign-in with a configured client ID.
- [ ] OAuth refresh after token expiry and across restarts.
- [ ] Generic IMAP password login with a test account.
- [ ] Reconciliation of server-side flag changes, moves and deletes for already-synced messages.
- [ ] Multiple accounts syncing concurrently; offline search and cached reading; reconnect behavior.
- [ ] Large-folder initial sync progress.

### Sending and composing

- [ ] Send with attachments; Sent copy saved; failed-send recovery.
- [ ] Reply, reply all and forward threading in a real mail client.
- [ ] Attachment picker, save and open-confirmation flow.

### Rules and labels

- [ ] Rules firing on arrival against real messages.
- [ ] Rule dry run against the latest 50 messages matches expectations.
- [ ] Label create, rename, apply and display.

### Reader and interactive UI

- [ ] Link click and confirmation; text selection and Ctrl+C with multiline and scrolled content.
- [ ] Links within a selection; search; dialogs.
- [ ] Remote-image opt-in: loads within limits, handles failures and blocks private/non-HTTPS URLs.
- [ ] Interactive Windows layout at common window sizes, including the neumorphic UI.

## Performance and memory (Phase 1 targets)

The plan targets are 120 MB idle and 150 MB active RSS. They have **not** been verified. A
previous debug process measured about 201 MiB private working set; that is not a release result.

- [ ] Measure the release build's private working set at idle, with a message open, while
  scrolling and after closing it. Wait at least 60 seconds after a body download before the idle
  measurement. Use the same mailbox, messages, window size and renderer; repeat with
  `SLINT_BACKEND=winit-software`. Do not record message contents or credentials.
- [ ] Measure cached and uncached open times separately (network fetch, store write, MIME parsing,
  rendering) for small, large and attachment-heavy messages.
- [ ] Compare results with the targets, documenting the exact Windows metric and workload.
- [ ] If measurements exceed targets, investigate UI-thread MIME/HTML preparation and decoded-image
  memory. Options: bounded low-priority prefetch, selective MIME-part downloads with on-demand
  attachments. Preserve inline images, offline caching and privacy.

Already implemented, not yet performance-validated: a separate body-download lane, folder and
connection reuse, resize relayout without rereading the body, direct composition into the UI pixel
buffer, and bounded, cancellable remote-image downloads.

## Release readiness

- [ ] Run the release executable against real mail and complete the performance checks above.
- [ ] Add an installer, app icon and CI; verify clean-machine installation on supported Windows
  versions.
- [ ] macOS and Linux support: keyring configuration and build testing.
- [ ] Provider setup and any required Google verification before general distribution.
- [ ] Never commit credentials, tokens or real mail. Desktop binaries cannot keep embedded OAuth
  client secrets confidential.

## Cleanup candidates

Found during the review; not yet changed.

- [ ] The `attachments` table in [store/src/lib.rs](./crates/store/src/lib.rs) has columns
  (`inline_blob`, `encrypted_path`) that nothing writes or reads. Remove them or use them.
- [ ] `argon2` and `chacha20poly1305` are declared in the workspace
  [Cargo.toml](./Cargo.toml) but no crate uses them. Remove them if no planned feature needs them.

## Phase 2: Calendar and invitations (not started)

- [ ] Google Calendar and CalDAV sync, storage of events, and recurrence.
- [ ] Calendar views, offline viewing.
- [ ] Invitation handling: responses reflected locally and on the server.
- [ ] Reminders.

The `events` table and a disabled Calendar UI placeholder exist, but they are not a calendar
implementation. Request calendar OAuth scopes only when that functionality is built and its
consent flow is designed.

## Manual test checklist (quick run)

1. Build with `cargo build --release -p mailthin-app`; start `target\release\mailthin-app.exe`.
2. Sign in to an account; confirm headers sync and the list populates.
3. Open a cached and an uncached message; check the reader, links and selection.
4. Archive, delete and star a message; confirm the change on the server (webmail).
5. Reply, send a message with an attachment, and check the Sent folder.
6. Save an attachment, decline the open prompt, then open it.
7. Create a rule, run its dry run, and let a new message trigger it.
8. Disconnect the network, confirm cached reading and search work, then reconnect.
9. Record the Task Manager private working set at idle and with a message open.
