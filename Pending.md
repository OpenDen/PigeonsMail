# PigeonMail: Pending Work

This is the prioritized checklist. See [DEVELOPMENT.md](./DEVELOPMENT.md) for running and testing
the app and [Implementation_Plan.md](./Implementation_Plan.md) for the phase roadmap.

## 1. Verify the current reader and sync

- [ ] Test Gmail sync, Inbox IDLE, opening cached and uncached messages, reconnects, and
  archive/delete/star against a real mailbox. Gmail OAuth sign-in and account creation were
  previously confirmed; that does not verify the rest of the workflow.
- [ ] Test the separate on-demand body-fetch connection, including errors, switching messages,
  large messages, reconnects and multiple accounts. It uses `BODY.PEEK[]`, limits raw messages to
  40 MiB, reuses its connection for up to 60 seconds of inactivity and caches bodies on disk.
- [ ] In the Windows UI, test link clicks and confirmation, text selection and Ctrl+C, including
  multiline and scrolled content, links within a selection, search, and dialogs.
- [ ] Test OAuth refresh after expiry and across restarts. Configure and test Microsoft OAuth
  using a Microsoft client ID. Verify generic IMAP password login with a suitable test account.
- [ ] Verify remote-image loading remains opt-in, respects its limits and handles failures.

## 2. Measure and improve performance

- [ ] Measure the optimized release build's private working set at idle, with a message open,
  while scrolling, and after closing it. Wait at least 60 seconds after a body download before
  measuring the final idle state. Compare the same mailbox, messages, window size and renderer;
  repeat with `SLINT_BACKEND=winit-software`. Avoid recording message contents or credentials.
- [ ] Measure cached and uncached open times separately, including network fetch, store write,
  MIME parsing and rendering. Test small, large and attachment-heavy messages.
- [ ] Compare the results with the plan's 120 MB idle / 150 MB active RSS targets, documenting
  the exact Windows metric and workload. These targets have **not** been verified. A previous
  debug process measured about 201 MiB private working set; that is not a release-build result.
- [ ] Based on measurements, investigate UI-thread MIME/HTML preparation and decoded-image
  memory. Consider bounded, lower-priority prefetch and selective MIME-part downloads with
  on-demand attachments; preserve inline images, offline caching and privacy.

Already implemented, but not yet performance-validated: a separate body-download lane, reuse
of the selected folder and connection, resize relayout without rereading the body, direct
composition into the UI pixel buffer, and bounded, cancellable remote-image downloads.

## 3. Complete Phase 1

- [ ] Implement SMTP sending using password or OAuth credentials, explicit send errors and a
  Sent copy; build compose with To/Cc, subject, body, attachment picker and Send.
- [ ] Implement Reply, Reply all and Forward, including correct recipients and message threading.
- [ ] Add safe attachment save/open with a user-selected destination and on-demand extraction.
- [ ] Build the rules UI, including editing and a dry-run preview against the last 50 messages.
- [ ] Reconcile server-side flag changes, moves and deletions for already-synced messages.
  Inbox is the only folder currently watched with IDLE.
- [ ] Add label management/display and server-settings editing. Starred is currently a filter,
  not a dedicated virtual folder; account removal is implemented.
- [ ] Verify multi-account behavior, offline/reconnect handling and large-folder sync progress.

## 4. Release readiness

- [ ] Test the release executable with real mail and perform the measurements above. The release
  **build** succeeded; its runtime, installation and performance have not been verified.
- [ ] Add an installer, app icon and CI; verify clean-machine installation and supported Windows
  environments. macOS and Linux support need their own keyring configuration and build testing.
- [ ] Complete provider setup and any required Google verification before general distribution.
  Never commit credentials, tokens or real mail; desktop binaries cannot keep embedded OAuth
  client secrets confidential.

## 5. Phase 2: Calendar

Not started. The database has an `events` table and the Calendar UI is a disabled placeholder.
Calendar sync, views, invitations and reminders belong to Phase 2; request calendar OAuth scopes
only when that functionality is implemented.
