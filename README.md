# PigeonMail

PigeonMail is a desktop mail client for Windows, written in Rust with a Slint interface. It
supports Gmail and Outlook (OAuth 2.0) and generic IMAP/SMTP accounts, keeps mail in an
encrypted local database, and works offline for cached mail and search.

Project status and the remaining work are in [DEVPLAN.md](./DEVPLAN.md).

## Features

- Multiple accounts; Gmail and Outlook sign-in with OAuth, or IMAP/SMTP with a password or app password.
- Header sync for the last 30 days, Inbox IDLE, and on-demand full-message download.
- Full-text search over the local database.
- Reader with HTML and plain-text rendering, link confirmation, text selection and copy.
- Remote images blocked by default; opt-in HTTPS loading with size and network limits.
- Archive, delete (to Trash), star, labels, reply, reply all and forward.
- Compose with To/Cc, subject, Markdown formatting, preview, attachments and a Sent copy.
- Rules with editing and a dry run against the latest 50 messages.
- Server-side flag, move and delete changes are reconciled for synced messages.

## Libraries

| Crate | Library | Used for |
| --- | --- | --- |
| `pigeonmail-app` | [slint](https://slint.dev) 1.8 | User interface (winit backend, FemtoVG and software renderers) |
| `pigeonmail-app` | [rfd](https://crates.io/crates/rfd) 0.15 | Native file open/save dialogs and confirmation prompts |
| `pigeonmail-app` | [pulldown-cmark](https://crates.io/crates/pulldown-cmark) | Markdown rendering in the composer preview |
| `pigeonmail-app` | [arboard](https://crates.io/crates/arboard) | Clipboard access |
| `pigeonmail-app` | [open](https://crates.io/crates/open) | Opening saved attachments and links with the OS |
| `pigeonmail-render` | [litehtml](https://crates.io/crates/litehtml) 0.2 (vendored, patched for MSVC) | HTML layout and drawing for message bodies |
| `pigeonmail-render` | [mail-parser](https://crates.io/crates/mail-parser) | MIME parsing, decoding and attachment extraction |
| `pigeonmail-store` | [rusqlite](https://crates.io/crates/rusqlite) 0.32 with bundled SQLCipher | Encrypted SQLite database, FTS search, body cache |
| `pigeonmail-store` | [keyring](https://crates.io/crates/keyring) 3.2 (Windows Credential Manager) | Database key, passwords and OAuth refresh tokens |
| `pigeonmail-net` | [async-imap](https://crates.io/crates/async-imap) 0.12 (Tokio) | IMAP sync, IDLE, body fetch and server actions |
| `pigeonmail-net` | [lettre](https://crates.io/crates/lettre) 0.11 | SMTP sending |
| `pigeonmail-net` | [oauth2](https://crates.io/crates/oauth2) 5 and [tiny_http](https://crates.io/crates/tiny_http) | OAuth authorization code with PKCE and a loopback redirect |
| `pigeonmail-net` | [reqwest](https://crates.io/crates/reqwest) (rustls) | OAuth token requests and remote image downloads |
| `pigeonmail-net` | [tokio](https://tokio.rs), [tokio-rustls](https://crates.io/crates/tokio-rustls), [webpki-roots](https://crates.io/crates/webpki-roots) | Async runtime and TLS for IMAP and SMTP |
| `pigeonmail-core` | [serde](https://serde.rs) | Serialization of domain types |

## Protocols

| Protocol | Details |
| --- | --- |
| IMAP over TLS | Header sync with `UID` commands; IDLE on Inbox with polling fallback and backoff; `BODY.PEEK[]` for full messages (40 MiB limit); `UID FLAGS`, `UID MOVE`/`COPY` and deletes for server actions and reconciliation. |
| SMTP | STARTTLS on port 587 (lettre); the sent message is appended to the Sent folder when available. |
| OAuth 2.0 | Authorization code flow with PKCE. Gmail and Outlook use a loopback redirect to a local listener. Refreshes for one account are serialized. |
| HTTPS | Remote images only, when enabled. HTTPS only, public DNS addresses only, redirects validated, 5 MiB per response and 16 MiB encoded per message. |

## Data storage

| Data | Where and how |
| --- | --- |
| Mail database | `%APPDATA%\PigeonMail\mail.db`, an SQLCipher-encrypted SQLite database in WAL mode. Set `PIGEONMAIL_DATA_DIR` to change the folder. |
| Headers, folders and metadata | Tables in the encrypted database, synced from the server per account and folder. |
| Full-text search index | FTS tables in the same database, populated from headers and cached body text. |
| Message bodies | Encrypted `bodies` table. Up to 300 bodies are kept; older entries are evicted. |
| Attachments | Not cached in the database. Extracted on demand when you choose to save or open one, with size limits. |
| Database key | Windows Credential Manager, service `pigeonmail`, user `database-key`. |
| Passwords and OAuth refresh tokens | Windows Credential Manager, service `pigeonmail`, user `account:<email>`. |
| In memory | At most four pages of 50 list rows. The reader holds the laid-out document for the open message only; closing or switching drops it. |

The UI and its data folder, keyring service and environment variables all use the PigeonMail name.
On first start after the rename, an installation from the old `Mailthin` name is migrated automatically: `%APPDATA%\Mailthin\mail.db` (with its `-wal`/`-shm` files) is copied to `%APPDATA%\PigeonMail` if no database exists there yet, and keyring entries under service `mailthin` are copied to `pigeonmail` when first needed. The old folder and entries are left in place as a backup. Migration is skipped when `PIGEONMAIL_DATA_DIR` is set.
Losing the database key makes the database unreadable.
Changing `PIGEONMAIL_DATA_DIR` moves the database location but not the keyring entries.

## Architecture

| Crate | Responsibility |
| --- | --- |
| `core` | Synchronous domain types, commands/events and rule evaluation. No network or database code. |
| `store` | SQLCipher persistence, queries, FTS, body cache and keyring access. All database access goes through this crate. |
| `net` | OAuth, IMAP sync and actions, SMTP sending and remote image downloads. Emits events; does not touch the database. |
| `render` | MIME interpretation and litehtml viewport rendering. Remote content and links are treated as untrusted. |
| `app` | Slint UI, message-list model, account wiring, and a store-writer thread. UI code is kept in `crates/app/ui`. |

A Tokio runtime runs account synchronization. A separate on-demand IMAP task fetches bodies so a
sync pass does not delay a message the user opened; it closes after 60 seconds idle. The UI and
store writer use separate SQLCipher connections. The UI drains worker updates on a 150 ms timer.

The vendored `litehtml-sys` carries local MSVC build patches; see
[PATCHES.txt](./vendor/litehtml-sys/PATCHES.txt).

## Build and run (Windows)

### Prerequisites

- Rust stable, MSVC toolchain.
- Visual Studio Build Tools with **Desktop development with C++**.
- Strawberry Perl for vendored OpenSSL. `scripts\dev-env.ps1` assumes `C:\Strawberry\perl\bin`; adjust it if installed elsewhere.

From the repository root, load the environment in each new PowerShell session:

```powershell
. .\scripts\dev-env.ps1
cargo run -p pigeonmail-app                       # debug
cargo build --release -p pigeonmail-app          # release (fat LTO; several minutes)
.\target\release\pigeonmail-app.exe
```

There is no installer yet. Close an older running instance before comparing a rebuilt version.

### Tests

```powershell
. .\scripts\dev-env.ps1
cargo test -p pigeonmail-app -p pigeonmail-core -p pigeonmail-net -p pigeonmail-render -p pigeonmail-store
cargo test -p pigeonmail-store -- --ignored       # keyring round trip; writes a Credential Manager entry
```

The last focused run passed 90 tests with one keyring test ignored. Tests do not replace live
mailbox, interactive UI or release-runtime checks; see [DEVPLAN.md](./DEVPLAN.md).

## Configuration

**OAuth (Google).** The desktop client ID is built in; `PIGEONMAIL_GOOGLE_CLIENT_ID` overrides it at
runtime. The optional client secret is read at build time from the git-ignored
`crates/net/oauth-secrets/google-client-secret.txt`; `PIGEONMAIL_GOOGLE_CLIENT_SECRET` overrides it.
If neither supplies it, Google sign-in reports that it is not configured.

**OAuth (Microsoft).** Requires `PIGEONMAIL_MICROSOFT_CLIENT_ID` at runtime, a desktop/public-client
loopback redirect, and the mail permissions listed in [oauth.rs](./crates/net/src/oauth.rs).

**Distribution.** A secret embedded in a desktop executable can be extracted, so it is not a
confidential credential. Google apps in testing mode allow only configured test users; general
release may require provider verification.

Never commit credentials, tokens, keys or real mail, and do not place them in documentation.

## Using the app

Add an account with OAuth or an app password. Shortcuts outside text inputs: `e` archive, `#`
delete, `s` star, `/` search, Ctrl+C copies reader selection. Links require confirmation. Remote
images require an explicit opt-in per message.

Delete moves a message to Trash. A message already in Trash is permanently deleted. If no Trash
folder is found, delete is refused.

Attachments are saved to a path you choose and opened only after a confirmation prompt.
Filenames are sanitized, and attachment counts and sizes are bounded.

## Privacy and security

- Remote content and links are untrusted; remote images are off by default.
- Bodies are fetched with `BODY.PEEK[]` so reading does not change server flags.
- Secrets live in the OS keyring; they are not written to the database, logs or config files.
- Logs must not contain secrets, message bodies or attachment contents.
- OAuth requests only the scopes the implemented features need.

## Troubleshooting

| Symptom | Check |
| --- | --- |
| `cargo` not recognized, or OpenSSL cannot find Perl modules | Dot-source `scripts\dev-env.ps1`; verify the Rust and Strawberry Perl paths. |
| Native litehtml/MSVC build errors | Confirm the local `litehtml-sys` patches are present; see `vendor/litehtml-sys/PATCHES.txt`. |
| Google sign-in is not configured | Check the build-time secret file or the runtime override. Never print the secret. |
| Microsoft sign-in is not configured | Set `PIGEONMAIL_MICROSOFT_CLIENT_ID`. |
| Login fails | Check the displayed error, account permissions, IMAP access, host/port and OAuth consent or app password. |
| Message body does not open | Check the reader status and connection. Messages over 40 MiB are refused. |
| Remote images stay blocked | Choose **Load remote images**. Non-HTTPS, private-network or oversized images are refused. |
| Database cannot be opened | Confirm the database key is still in Windows Credential Manager. Do not delete the database or key as a shortcut. |
| Build is slow | The first release build compiles all dependencies with fat LTO. Use the debug build for iteration. |
