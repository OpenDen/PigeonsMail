# PigeonMail Development Guide

PigeonMail is a Rust desktop mail client with a Slint interface. Steps 1A-1C of
[Implementation_Plan.md](./Implementation_Plan.md) are implemented. Compose/send, attachment
save/open and the rules UI are not yet implemented. See [Pending.md](./Pending.md) for the
prioritized remaining work.

## Current status

| Area | Status |
| --- | --- |
| SQLCipher storage, FTS search, keyring secrets | Implemented and unit-tested |
| Gmail OAuth | Sign-in and account creation previously verified; refresh and restart need testing |
| Outlook OAuth | Implemented but not live-tested; requires a Microsoft client ID |
| IMAP sync, IDLE, body fetch and server actions | Implemented; live-mailbox verification is pending |
| Reading pane | HTML/plain-text rendering, scrolling, confirmed links, selection and copy implemented; interactive Windows testing is pending |
| Remote images | Blocked by default; opt-in HTTPS loading with network and size restrictions |
| Compose/send, reply/forward, attachment save/open, rules UI | Not started |
| Release build | Built successfully on the development machine; runtime, installation and performance remain unverified |
| Calendar | Phase 2; not started |

The latest recorded workspace test run passed 68 tests, with one keyring test ignored. This is
not a substitute for live-server, interactive UI or release-runtime testing. Performance and
the plan's memory targets have not been verified.

## Build and run on Windows

### Prerequisites

- Rust stable with the MSVC toolchain.
- Visual Studio Build Tools with **Desktop development with C++**.
- Strawberry Perl for vendored OpenSSL. The provided script assumes
  `C:\Strawberry\perl\bin`; adjust the environment if installed elsewhere.

Open PowerShell in the repository root. Dot-source the environment script before running Cargo
in each new terminal:

```powershell
. .\scripts\dev-env.ps1
cargo build --release -p mailthin-app
.\target\release\mailthin-app.exe
```

The initial release build may take several minutes because dependencies are built from source
and release uses fat LTO. To run the debug build instead:

```powershell
. .\scripts\dev-env.ps1
cargo run -p mailthin-app
```

An already-built executable can be started directly without Cargo or the environment script:
`target\release\mailthin-app.exe` for release or `target\debug\mailthin-app.exe` for debug.
Close an older running instance before comparing a rebuilt version. There is no installer yet.
The release executable has been built locally, not certified for distribution.

### Tests

```powershell
. .\scripts\dev-env.ps1
cargo test --workspace
cargo test -p mailthin-render
cargo test -p mailthin-net
```

The ignored keyring round-trip test writes and removes a Windows Credential Manager entry; run
it separately only in a suitable environment:

```powershell
cargo test -p mailthin-store -- --ignored
```

### Using the app

Add an account and use Google/Microsoft OAuth or an appropriate provider-specific app password.
The initial sync fetches headers from the last 30 days; selecting an uncached message downloads
its full raw body. Shortcuts outside text inputs include `e` archive, `#` delete, `s` star,
`/` search and Ctrl+C for reader selection. Links require confirmation; remote images require
an explicit opt-in.

Deleting normally moves the message to Trash. A message already in Trash can be permanently
deleted; if no Trash folder is found, the delete action is refused.

## OAuth setup

The Google desktop client ID is built in; `MAILTHIN_GOOGLE_CLIENT_ID` can override it at
runtime. The optional Google client secret is read at build time from the git-ignored
`crates/net/oauth-secrets/google-client-secret.txt`; `MAILTHIN_GOOGLE_CLIENT_SECRET` can
override it at runtime. If neither source supplies it, Google sign-in reports that it is not
configured. Do not commit the file or place real credentials in documentation.

Microsoft sign-in requires `MAILTHIN_MICROSOFT_CLIENT_ID` at runtime. Configure the provider's
desktop/public-client loopback redirect and the mail permissions listed in
[oauth.rs](./crates/net/src/oauth.rs). Google OAuth apps in testing mode restrict sign-in to
configured test users; broader release can require provider verification.

**Distribution:** a secret embedded in a desktop executable is extractable. Do not treat it
as a confidential server-side credential; review provider requirements and the distribution
model before publishing a build.

## Data and privacy

| Data | Location |
| --- | --- |
| Encrypted database | `%APPDATA%\Mailthin\mail.db`, plus SQLite WAL files; `MAILTHIN_DATA_DIR` overrides the folder |
| Database key | Windows Credential Manager, service `mailthin`, user `database-key` |
| Account passwords and OAuth refresh tokens | Windows Credential Manager, service `mailthin`, user `account:<email>` |
| Cached message bodies | Encrypted `bodies` table in the database |

The UI name is PigeonMail; the data folder, keyring service and environment variables retain
the `Mailthin` name for compatibility. Losing the database key makes the existing database
unreadable. A separate `MAILTHIN_DATA_DIR` changes the database location, not the keyring
service; avoid using real accounts or mail in test data. Never commit secrets, keys or real mail.

## Architecture and data flow

The workspace has five crates:

- `core`: synchronous domain types and rule evaluation.
- `store`: encrypted SQLite/SQLCipher, FTS, queries, body caching and keyring access.
- `net`: async OAuth, IMAP sync and actions, plus remote-image downloads. It emits events and
  does not depend on the database implementation.
- `render`: MIME interpretation and litehtml viewport rendering.
- `app`: Slint UI, message-list model, application wiring and store-writer thread.

The UI and writer use separate SQLCipher connections in WAL mode. A Tokio runtime handles
account synchronization; a separate on-demand IMAP task handles body requests, so a sync pass
does not block a clicked-message request. It reuses the selected folder and connection while
active, then closes the connection after 60 seconds idle. The UI drains worker updates on a
150 ms timer. OAuth refresh requests for one account are serialized.

The reader loads a cached body or fetches the full raw message with `BODY.PEEK[]`, limited to
40 MiB. The writer caches the body and derives preview/search text. The renderer interprets MIME,
sanitizes HTML and draws the visible viewport. Resizing reuses the existing document; closing or
switching messages drops it. The list caches up to four pages of 50 rows; the on-disk body cache
evicts older entries beyond 300 bodies.

Remote images are opt-in. The downloader accepts HTTPS only, validates redirects and public DNS
addresses, limits individual responses to 5 MiB and bounds the encoded bytes accepted for one
message to 16 MiB. It cancels outstanding downloads when the reader closes. Links are limited
to HTTP, HTTPS and mailto and require user confirmation. These limits do not establish an overall
memory ceiling, especially for decoded images.

The workspace patches `litehtml-sys` for MSVC; see
[vendor/litehtml-sys/PATCHES.txt](./vendor/litehtml-sys/PATCHES.txt). Check the actual manifest,
lockfile and source for dependency versions and APIs rather than copying old plan snippets.

## Measuring memory and opening speed

Use the release build and document the account count, mailbox, message, window size and renderer.
Record Task Manager's **Memory (private working set)** at idle, with a message open, during
scrolling and after closing it. Wait at least 60 seconds after a body download before the final
idle measurement. Compare like-for-like runs; do not compare a debug build directly with a release
build or mix private working set with another memory metric.

To compare the software renderer, start a separate PowerShell session and run:

```powershell
$env:SLINT_BACKEND = "winit-software"
.\target\release\mailthin-app.exe
```

Measure cached and uncached opening separately; network and server latency affect uncached
messages. The plan's 120 MB idle / 150 MB active RSS targets are unverified, and Windows private
working set is a different metric; document the metric when comparing.

## Troubleshooting

| Symptom | Check |
| --- | --- |
| Cargo is not recognized or OpenSSL cannot find Perl modules | Dot-source `scripts\dev-env.ps1`; verify Rust and Strawberry Perl paths. |
| Native litehtml/MSVC build errors | Ensure the workspace's local `litehtml-sys` patch is present; see its `PATCHES.txt`. |
| Google sign-in is not configured | Check the build-time secret file or runtime secret override; never print the secret in logs. |
| Microsoft sign-in is not configured | Supply a configured `MAILTHIN_MICROSOFT_CLIENT_ID`. |
| Login fails | Check the displayed error, account permissions, IMAP access, host/port and OAuth consent or app password. |
| Message body does not open | Check the reader/status error and connection. Messages larger than 40 MiB are refused. |
| Remote images remain blocked | Choose **Load remote images**; invalid, non-HTTPS, private-network or oversized images are refused. |
| Database cannot be opened | Verify that the original database key is still in Windows Credential Manager; do not delete the database or key as a troubleshooting shortcut. |
