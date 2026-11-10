# Project Guidance

## Current State and Scope

- Phase 1 milestones 1A to 1D are implemented; live-mailbox, interactive UI and performance checks remain (see `DEVPLAN.md`). Verify the actual files and dependency APIs before copying example code from the plan; several plan snippets were corrected (raw SQLCipher key syntax, `contentless_delete`, rule priority ordering).
- On Windows, run `. .\scripts\dev-env.ps1` before `cargo` (Strawberry Perl is needed for the vendored OpenSSL). `vendor/litehtml-sys` carries local MSVC build patches; see its `PATCHES.txt`.
- The intended product is PigeonMail, a desktop mail client. Phase 1 is the complete mail client; Calendar is Phase 2 and should not expand Phase 1 work unless explicitly requested.
- The workspace has five crates: `core`, `store`, `net`, `render`, and `app`. Keep their dependency direction and responsibilities clear.

## Architecture

- `core`: synchronous domain models, commands/events, and mail-rule evaluation. Keep it independent of network and database implementations.
- `store`: encrypted SQLite/SQLCipher persistence, schema, search, and cached message content. Keep database access behind this crate.
- `net`: asynchronous account synchronization, OAuth, IMAP actions, and SMTP sending. Keep account-specific work isolated and communicate through explicit commands/events.
- `render`: MIME interpretation and email HTML rendering. Treat remote content and links as untrusted; remote image loading should be controllable.
- `app`: Slint UI and application wiring. Keep UI state and rendering work bounded; avoid retaining closed-message content unnecessarily.

## Security and Privacy

- Never commit credentials, OAuth tokens, encryption keys, or real user mail data.
- Store secrets with the operating-system keyring where appropriate. Avoid plaintext secret persistence, logging secrets, or logging message bodies and attachment contents.
- Request only the OAuth scopes needed for the implemented functionality. Validate and safely handle untrusted message content, links, MIME parts, and attachments.

## Implementation and Verification

- Follow the milestone order in `DEVPLAN.md` where practical, but validate architectural and dependency assumptions against current crate documentation and compiler diagnostics.
- Keep changes focused on the requested subsystem and preserve the crate boundaries above.
- Add or update focused tests for behavior changes, especially rule matching, storage/query behavior, MIME handling, and account actions.
- Run the narrowest relevant tests or checks first, then workspace checks when available. Do not claim performance or memory targets without measurement.
