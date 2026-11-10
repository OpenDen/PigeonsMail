Yes, OAuth, compose and reply are all still pending. The main pending components, roughly in the order I'd build them:

1. Real-server testing (not built yet, but the biggest risk)

The IMAP sync, IDLE, body fetch, and archive, delete and star code has only been checked with unit tests, never against a live mailbox.
I'd do this first, with an app-password account, before building more on top of it.
2. Step 1D: sending mail

SMTP send: lettre is declared but no code uses it yet. Needs a send command, login from the keyring or OAuth token, and saving a copy to Sent.
Compose window: To/Cc, subject, body, attachment picker, Send button, and the c shortcut.
Reply, Reply all, Forward: pre-filled from the open message. The Reply button and r shortcut are disabled placeholders now.
Attachments: the reader lists them, but you can't open or save them yet. Planned as encrypted storage or save-to-disk.
Rules screen: the rules engine works, but rules can only be added through code today. Needs the builder UI and the "test against last 50 mails" preview.
3. OAuth (Gmail and Outlook)

The sign-in code itself is written and tested: browser flow, PKCE, token refresh.
It isn't connected to the UI. Still missing:
a "Sign in with Google/Microsoft" button
your own client IDs from Google and Microsoft (none are embedded)
storing the refresh token in the keyring
refreshing the access token on reconnect
Microsoft has largely turned off app-password IMAP for Outlook.com and Microsoft 365, so Outlook accounts will need this.
4. Smaller Phase 1 gaps

Changes made on the server from another device (read, starred, deleted) don't update messages already synced.
IDLE only watches the Inbox.
No UI for labels, removing an account, or editing server settings.
No dedicated "Starred" folder. Starring only works as a filter.
5. Release readiness

Memory and performance haven't been measured against the plan's 120/150 MB targets.
The optimised release build is unverified, and there's no installer or CI.
Only Windows is covered so far.
6. Phase 2: Calendar

Not started. The database table and OAuth groundwork exist, and the sidebar button is a disabled placeholder.
I'd go real-server test, then SMTP and compose, then reply and forward, then attachments, then the rules UI, then OAuth. OAuth comes last because it needs Google and Microsoft developer accounts and client IDs from you, while the other steps can be built and tested with a normal app-password mailbox.

Want me to start on Step 1D?