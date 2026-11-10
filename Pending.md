Yes, compose and reply are still pending. Gmail sign-in (OAuth) now works and the account is added. The main pending components, roughly in the order I'd build them:

1. Real-server testing (partly done, still the biggest risk)

Gmail OAuth login is confirmed working. Sync, IDLE, body fetch, and archive, delete and star have only been checked with unit tests, never against a live mailbox.
I'd do this next, on the Gmail account already added, before building more on top of it.
2. Step 1D: sending mail

SMTP send: lettre is declared but no code uses it yet. Needs a send command, login from the keyring or OAuth token, and saving a copy to Sent.
Compose window: To/Cc, subject, body, attachment picker, Send button, and the c shortcut.
Reply, Reply all, Forward: pre-filled from the open message. The Reply button and r shortcut are disabled placeholders now.
Attachments: the reader lists them, but you can't open or save them yet. Planned as encrypted storage or save-to-disk.
Rules screen: the rules engine works, but rules can only be added through code today. Needs the builder UI and the "test against last 50 mails" preview.
3. OAuth (Gmail and Outlook): Gmail is verified; Outlook is not yet tested.
   * "Sign in with Google" works: it opens the browser, signs in with PKCE and a loopback redirect,
     tests the IMAP login, and then saves the account. The refresh token goes to the keyring; access
     tokens are refreshed on each reconnect.
   * Still needed: a Microsoft client ID for "Sign in with Microsoft" and a live test. Token refresh
     after expiry and the keyring-stored token after a restart still need checking.
   * Google requires app verification before general release; until then only listed test users can
     sign in (see DEVELOPMENT.md section 8.6).
   * Microsoft has largely turned off app-password IMAP for Outlook.com and Microsoft 365, so Outlook
     accounts depend on this path.
4. Smaller Phase 1 gaps

Changes made on the server from another device (read, starred, deleted) don't update messages already synced.
IDLE only watches the Inbox.
* No UI for labels or editing server settings after an account is added. Accounts can be removed.
No dedicated "Starred" folder. Starring only works as a filter.
5. Release readiness

Memory and performance haven't been measured against the plan's 120/150 MB targets.
The optimised release build is unverified, and there's no installer or CI.
Only Windows is covered so far.
6. Phase 2: Calendar

Not started. The database table and OAuth groundwork exist, and the sidebar button is a disabled placeholder.
I'd go real-server test on the Gmail account, then SMTP and compose, then reply and forward, then attachments, then the rules UI. Outlook sign-in comes once a Microsoft client ID is set up, and the other steps can be built and tested with a normal app-password mailbox.

Want me to start on Step 1D?