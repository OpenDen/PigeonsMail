//! IMAP connection, folder listing and incremental header sync.
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use async_imap::types::{Flag, NameAttribute};
use async_imap::Authenticator;
use futures::TryStreamExt;
use mail_parser::{AddressList, MessageParser};
use mailthin_core::{evaluate_rules, flags, MessageSummary, Rule};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{self, ClientConfig, RootCertStore};
use tokio_rustls::{client::TlsStream, TlsConnector};
use zeroize::Zeroizing;

use crate::events::{FetchedMessage, FolderInfo};
use crate::oauth::xoauth2_string;

pub type ImapSession = async_imap::Session<TlsStream<TcpStream>>;

const FETCH_CHUNK: usize = 100;

#[derive(Clone)]
pub enum Credential {
    Password(Zeroizing<String>),
    OAuth2AccessToken(Zeroizing<String>),
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credential(..)")
    }
}

#[derive(Debug, Clone)]
pub struct ImapSettings {
    pub host: String,
    pub port: u16,
    pub username: String,
}

struct XOAuth2 {
    payload: Zeroizing<String>,
}

impl Authenticator for XOAuth2 {
    type Response = String;
    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        // After a failure the server sends a JSON error challenge; reply empty to finish.
        std::mem::take(&mut *self.payload)
    }
}

fn tls_connector() -> TlsConnector {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("ring provider supports the default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
}

/// Connects over implicit TLS (port 993) and authenticates.
pub async fn connect(settings: &ImapSettings, credential: &Credential) -> Result<ImapSession> {
    let tcp = TcpStream::connect((settings.host.as_str(), settings.port))
        .await
        .with_context(|| format!("cannot reach {}:{}", settings.host, settings.port))?;
    let server_name = ServerName::try_from(settings.host.clone()).context("invalid IMAP host name")?;
    let tls = tls_connector().connect(server_name, tcp).await.context("TLS handshake failed")?;

    let mut client = async_imap::Client::new(tls);
    client.read_response().await.context("no IMAP greeting")?;

    let session = match credential {
        Credential::Password(pw) => client
            .login(&settings.username, pw.as_str())
            .await
            .map_err(|(e, _)| anyhow!("IMAP login failed: {e}"))?,
        Credential::OAuth2AccessToken(tok) => client
            .authenticate(
                "XOAUTH2",
                XOAuth2 { payload: Zeroizing::new(xoauth2_string(&settings.username, tok)) },
            )
            .await
            .map_err(|(e, _)| anyhow!("IMAP XOAUTH2 failed: {e}"))?,
    };
    Ok(session)
}

pub async fn supports_idle(session: &mut ImapSession) -> Result<bool> {
    Ok(session.capabilities().await?.has_str("IDLE"))
}

/// Lists selectable folders with their roles.
pub async fn list_folders(session: &mut ImapSession) -> Result<Vec<FolderInfo>> {
    let names: Vec<_> = session.list(Some(""), Some("*")).await?.try_collect().await?;
    let mut out = Vec::new();
    for n in &names {
        if n.attributes().iter().any(|a| matches!(a, NameAttribute::NoSelect)) {
            continue;
        }
        let role = folder_role(n.name(), n.attributes());
        out.push(FolderInfo { name: n.name().to_string(), role });
    }
    Ok(out)
}

fn folder_role(name: &str, attrs: &[NameAttribute<'_>]) -> Option<String> {
    if name.eq_ignore_ascii_case("INBOX") {
        return Some("inbox".into());
    }
    for a in attrs {
        let role = match a {
            NameAttribute::Sent => "sent",
            NameAttribute::Drafts => "drafts",
            NameAttribute::Archive | NameAttribute::All => "archive",
            NameAttribute::Trash => "trash",
            NameAttribute::Junk => "junk",
            NameAttribute::Flagged => "starred",
            _ => continue,
        };
        return Some(role.into());
    }
    // Servers without SPECIAL-USE: fall back to common names.
    let leaf = name.rsplit(['/', '.']).next().unwrap_or(name).to_ascii_lowercase();
    match leaf.as_str() {
        "sent" | "sent items" | "sent mail" => Some("sent".into()),
        "drafts" => Some("drafts".into()),
        "archive" | "archives" => Some("archive".into()),
        "trash" | "deleted items" | "deleted" => Some("trash".into()),
        "junk" | "spam" | "junk email" => Some("junk".into()),
        _ => None,
    }
}

pub struct FolderSync {
    pub uidvalidity: u32,
    pub reset: bool,
    pub messages: Vec<FetchedMessage>,
}

/// Fetches headers for new messages in `folder`.
///
/// `known` is the `(uidvalidity, last_uid)` already cached locally. With no usable state the
/// last `since_ts`-onwards window is fetched; otherwise only UIDs above `last_uid`.
pub async fn sync_folder(
    session: &mut ImapSession,
    folder: &str,
    known: Option<(u32, u32)>,
    since_ts: i64,
    now_ts: i64,
    rules: &[Rule],
) -> Result<FolderSync> {
    let mailbox = session.select(folder).await.with_context(|| format!("cannot select {folder}"))?;
    let uidvalidity = mailbox.uid_validity.unwrap_or(0);

    let (reset, last_uid) = match known {
        Some((v, last)) if v == uidvalidity => (false, last),
        Some(_) => (true, 0),
        None => (false, 0),
    };

    let query = if last_uid == 0 {
        format!("SINCE {}", imap_date(since_ts))
    } else {
        format!("UID {}:*", last_uid.saturating_add(1))
    };
    let mut uids: Vec<u32> = session.uid_search(&query).await?.into_iter().filter(|u| *u > last_uid).collect();
    uids.sort_unstable();

    let mut messages = Vec::with_capacity(uids.len());
    for chunk in uids.chunks(FETCH_CHUNK) {
        let set = chunk.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
        let fetches: Vec<_> = session
            .uid_fetch(&set, "(UID FLAGS BODY.PEEK[HEADER])")
            .await?
            .try_collect()
            .await?;
        for f in &fetches {
            let (Some(uid), Some(header)) = (f.uid, f.header()) else { continue };
            let mut bits = 0;
            for flag in f.flags() {
                bits |= match flag {
                    Flag::Seen => flags::SEEN,
                    Flag::Flagged => flags::FLAGGED,
                    Flag::Answered => flags::ANSWERED,
                    Flag::Draft => flags::DRAFT,
                    Flag::Deleted => flags::DELETED,
                    _ => 0,
                };
            }
            messages.push(parse_header(uid, bits, header, now_ts, rules));
        }
    }
    Ok(FolderSync { uidvalidity, reset, messages })
}

/// Largest message the lazy fetch will download.
pub const MAX_BODY_BYTES: u32 = 40 * 1024 * 1024;

/// Flags the UI can change on the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImapFlag {
    Seen,
    Flagged,
    Answered,
}

impl ImapFlag {
    fn token(self) -> &'static str {
        match self {
            ImapFlag::Seen => "\\Seen",
            ImapFlag::Flagged => "\\Flagged",
            ImapFlag::Answered => "\\Answered",
        }
    }
}

/// Capabilities that change how actions are issued.
#[derive(Debug, Clone, Copy, Default)]
pub struct Caps {
    pub idle: bool,
    pub move_ext: bool,
    pub uidplus: bool,
}

pub async fn capabilities(session: &mut ImapSession) -> Result<Caps> {
    let c = session.capabilities().await?;
    Ok(Caps { idle: c.has_str("IDLE"), move_ext: c.has_str("MOVE"), uidplus: c.has_str("UIDPLUS") })
}

/// Downloads the full raw message without marking it as read.
pub async fn fetch_body(session: &mut ImapSession, folder: &str, uid: u32) -> Result<Vec<u8>> {
    session.select(folder).await.with_context(|| format!("cannot select {folder}"))?;
    fetch_selected_body(session, uid).await
}

/// Fetches a message from the currently selected folder.
pub async fn fetch_selected_body(session: &mut ImapSession, uid: u32) -> Result<Vec<u8>> {
    let sizes: Vec<_> = session.uid_fetch(uid.to_string(), "(RFC822.SIZE)").await?.try_collect().await?;
    match sizes.iter().find_map(|f| f.size) {
        Some(sz) if sz > MAX_BODY_BYTES => {
            anyhow::bail!("message is too large to display ({} MB)", sz / (1024 * 1024))
        }
        None if sizes.is_empty() => anyhow::bail!("message no longer exists on the server"),
        _ => {}
    }

    let fetches: Vec<_> = session.uid_fetch(uid.to_string(), "BODY.PEEK[]").await?.try_collect().await?;
    fetches
        .iter()
        .find_map(|f| f.body().map(<[u8]>::to_vec))
        .ok_or_else(|| anyhow!("server returned no message body"))
}

pub async fn set_flags(
    session: &mut ImapSession,
    folder: &str,
    uid: u32,
    add: &[ImapFlag],
    remove: &[ImapFlag],
) -> Result<()> {
    session.select(folder).await.with_context(|| format!("cannot select {folder}"))?;
    for (sign, flags) in [("+", add), ("-", remove)] {
        if flags.is_empty() {
            continue;
        }
        let list = flags.iter().map(|f| f.token()).collect::<Vec<_>>().join(" ");
        let query = format!("{sign}FLAGS.SILENT ({list})");
        let _: Vec<_> = session.uid_store(uid.to_string(), query).await?.try_collect().await?;
    }
    Ok(())
}

/// Moves a message (server-side MOVE, or COPY + delete + expunge as a fallback).
pub async fn move_message(session: &mut ImapSession, from: &str, to: &str, uid: u32, caps: Caps) -> Result<()> {
    session.select(from).await.with_context(|| format!("cannot select {from}"))?;
    if caps.move_ext {
        session.uid_mv(uid.to_string(), to).await?;
    } else {
        session.uid_copy(uid.to_string(), to).await?;
        expunge_selected(session, uid, caps).await?;
    }
    Ok(())
}

/// Permanently deletes a message from `folder`.
pub async fn expunge_message(session: &mut ImapSession, folder: &str, uid: u32, caps: Caps) -> Result<()> {
    session.select(folder).await.with_context(|| format!("cannot select {folder}"))?;
    expunge_selected(session, uid, caps).await
}

async fn expunge_selected(session: &mut ImapSession, uid: u32, caps: Caps) -> Result<()> {
    let _: Vec<_> = session.uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)").await?.try_collect().await?;
    if caps.uidplus {
        let _: Vec<_> = session.uid_expunge(uid.to_string()).await?.try_collect().await?;
    } else {
        // Without UIDPLUS this also removes other messages already flagged \Deleted.
        let _: Vec<_> = session.expunge().await?.try_collect().await?;
    }
    Ok(())
}

fn format_addresses(list: Option<AddressList<'_>>) -> String {
    let Some(list) = list else { return String::new() };
    list.mailboxes()
        .map(|a| match (a.name(), a.address()) {
            (Some(n), Some(e)) if !n.is_empty() => format!("{n} <{e}>"),
            (_, Some(e)) => e.to_string(),
            (Some(n), None) => n.to_string(),
            _ => String::new(),
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Builds a message summary from raw header bytes and evaluates the rules against it.
pub fn parse_header(uid: u32, flag_bits: u32, header: &[u8], now_ts: i64, rules: &[Rule]) -> FetchedMessage {
    let parsed = MessageParser::default().parse_headers(header);
    let (msgid, from, to, subject, date_ts, has_attachment) = match &parsed {
        Some(m) => {
            // Without a body structure, `multipart/mixed` is the practical attachment signal.
            let has_attachment = m
                .content_type()
                .map(|ct| ct.ctype().eq_ignore_ascii_case("multipart")
                    && ct.subtype().is_some_and(|s| s.eq_ignore_ascii_case("mixed")))
                .unwrap_or(false);
            (
                m.message_id().map(str::to_string),
                format_addresses(m.from()),
                format_addresses(m.to()),
                m.subject().unwrap_or("").to_string(),
                m.date().map(|d| d.to_timestamp()).unwrap_or(now_ts),
                has_attachment,
            )
        }
        None => (None, String::new(), String::new(), String::new(), now_ts, false),
    };

    let summary = MessageSummary {
        id: 0,
        account_id: 0,
        folder_id: 0,
        uid,
        from: from.clone(),
        to: to.clone(),
        subject: subject.clone(),
        date_ts,
        snippet: String::new(),
        is_read: flag_bits & flags::SEEN != 0,
        is_starred: flag_bits & flags::FLAGGED != 0,
        has_attachment,
    };
    let rule_actions = evaluate_rules(rules, &summary);

    FetchedMessage { uid, msgid, from, to, subject, date_ts, flags: flag_bits, has_attachment, rule_actions }
}

/// Formats a Unix timestamp as an IMAP `date` (`1-Oct-2026`) in UTC.
pub fn imap_date(ts: i64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    // Howard Hinnant's civil-from-days algorithm.
    let z = ts.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{day}-{}-{year}", MONTHS[(month - 1) as usize])
}

#[cfg(test)]
mod tests {
    use super::*;
    use mailthin_core::{Condition, RuleAction};

    #[test]
    fn imap_date_formats_utc() {
        assert_eq!(imap_date(0), "1-Jan-1970");
        assert_eq!(imap_date(1_709_164_800), "29-Feb-2024");
        assert_eq!(imap_date(1_793_664_000 + 5), "3-Nov-2026");
    }

    #[test]
    fn roles_from_attributes_and_names() {
        assert_eq!(folder_role("inbox", &[]).as_deref(), Some("inbox"));
        assert_eq!(folder_role("[Gmail]/Sent Mail", &[NameAttribute::Sent]).as_deref(), Some("sent"));
        assert_eq!(folder_role("INBOX.Trash", &[]).as_deref(), Some("trash"));
        assert_eq!(folder_role("Projects", &[]), None);
    }

    const HEADER: &[u8] = b"From: \"Alice Example\" <alice@example.com>\r\n\
To: me@example.org, Bob <bob@example.org>\r\n\
Subject: Quarterly invoice\r\n\
Date: Tue, 03 Nov 2026 10:00:00 +0000\r\n\
Message-ID: <abc@example.com>\r\n\
Content-Type: multipart/mixed; boundary=xyz\r\n\r\n";

    #[test]
    fn parses_headers_and_applies_rules() {
        let rules = vec![Rule {
            id: 1,
            name: "invoices".into(),
            enabled: true,
            priority: 0,
            match_all: true,
            conditions: vec![Condition::SubjectContains("invoice".into()), Condition::HasAttachment],
            actions: vec![RuleAction::Star],
        }];
        let m = parse_header(7, flags::SEEN, HEADER, 99, &rules);
        assert_eq!(m.uid, 7);
        assert_eq!(m.from, "Alice Example <alice@example.com>");
        assert_eq!(m.to, "me@example.org, Bob <bob@example.org>");
        assert_eq!(m.subject, "Quarterly invoice");
        assert_eq!(m.msgid.as_deref(), Some("abc@example.com"));
        assert_eq!(m.date_ts, 1_793_700_000);
        assert!(m.has_attachment);
        assert_eq!(m.rule_actions, vec![RuleAction::Star]);
    }

    #[test]
    fn unparseable_header_falls_back_to_now() {
        let m = parse_header(1, 0, b"", 99, &[]);
        assert_eq!(m.date_ts, 99);
        assert!(m.subject.is_empty());
    }
}
