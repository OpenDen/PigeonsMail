# PigeonMail Implementation Plan

PigeonMail is a desktop mail client. Phase 1 delivers the complete mail client; Calendar is
Phase 2. For current tasks and verification gaps, see [Pending.md](./Pending.md). For setup,
architecture and commands, see [DEVELOPMENT.md](./DEVELOPMENT.md).

## Roadmap and exit criteria

| Phase | Scope | Exit criteria |
| --- | --- | --- |
| 1: Mail | Multi-account Gmail/Outlook OAuth and generic IMAP/SMTP; encrypted storage and search; sync; reader; compose/reply/forward; attachments; rules and labels. | Three accounts syncing concurrently, offline search and cached reading, rules on arrival, sending with attachments, and measured RSS under 120 MB idle / 150 MB active. Validate with documented workloads and memory metrics. |
| 2: Calendar and invites | Google Calendar and CalDAV, calendar views, recurring events, invitations and reminders. | Offline calendar views, invite responses reflected locally and on the server, and functioning reminders. |

These are goals, not verified claims. In particular, memory targets, three-account sync, live
mailbox workflows and the release executable's runtime remain unverified.

## Architecture

The five crates are already present:

| Crate | Responsibility |
| --- | --- |
| `core` | Synchronous domain models, commands/events and rule evaluation; no network or database implementation. |
| `store` | SQLCipher persistence, queries, full-text search, cached bodies and OS-keyring access. |
| `net` | OAuth, IMAP sync and actions, remote image downloads, and future SMTP. It does not access the database directly. |
| `render` | MIME interpretation and litehtml rendering; remote content and links are untrusted. |
| `app` | Slint UI, account wiring, message-list model and store-writer bridge. |

Use the actual manifests and source as the API reference. Old illustrative Cargo, schema and
implementation snippets have been removed from this plan to avoid copying obsolete APIs or
unsafe examples. In particular, preserve SQLCipher raw-key handling, FTS
`contentless_delete`, rule priority ordering, and the local MSVC patches documented in
[PATCHES.txt](./vendor/litehtml-sys/PATCHES.txt).

## Phase 1 milestones

### 1A: Storage and authentication — implemented

Encrypted storage, FTS, body LRU, keyring secrets, and OAuth loopback/PKCE are implemented.
Gmail OAuth account creation was verified; token refresh, restart behavior and Microsoft OAuth
still need live testing.

### 1B: Sync and rules engine — implemented, live testing pending

Per-account folder and header synchronization, Inbox IDLE/polling, backoff and rule evaluation
are implemented. Server-side reconciliation of changes to already-synced messages remains
pending. Validate sync, multiple accounts and actions against real servers.

### 1C: Reader and actions — implemented, interactive testing pending

Cached or on-demand body fetch, MIME interpretation, litehtml viewport rendering, link
confirmation, selection/copy, remote-image opt-in and archive/delete/star are implemented.
Body downloads use a separate on-demand IMAP task; resizing reuses the laid-out document.
Test real mailbox and UI behavior and measure opening times and memory before claiming a
performance improvement.

### 1D: Complete mail workflows — not started

Implement SMTP send and Sent-copy handling, compose and reply/forward, attachment save/open,
and rules-builder UI with a dry run. Preserve secure secret storage, validate untrusted
attachments, and add focused tests before enabling the UI.

## Phase 2: Calendar and invitations — not started

Extend the existing crate boundaries for Google Calendar and CalDAV, event storage, recurrence,
calendar views, invite handling and reminders. The `events` table and a disabled Calendar UI
placeholder exist, but they are not a calendar implementation. Request additional OAuth scopes
only when the corresponding functionality is built and its consent flow is designed.
