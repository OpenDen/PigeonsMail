//! Outgoing mail over SMTP (STARTTLS, port 587). Builds the RFC 5322 message and returns its bytes
//! so the caller can file a copy in the Sent folder.
use std::time::Duration;

use anyhow::{bail, Context, Result};
use lettre::message::header::ContentType;
use lettre::message::{Attachment, Mailbox, Message, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use mailthin_core::OutgoingMail;

use crate::imap::Credential;

const SMTP_PORT: u16 = 587;
const SMTP_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct SmtpSettings {
    pub host: String,
    pub username: String,
}

/// Builds the message without sending it. Errors on invalid addresses or when there is no recipient.
pub fn build_message(from: &str, mail: &OutgoingMail) -> Result<Message> {
    if mail.to.is_empty() && mail.cc.is_empty() {
        bail!("add at least one recipient");
    }
    let mut builder = Message::builder()
        .from(from.parse::<Mailbox>().context("invalid sender address")?)
        .subject(mail.subject.as_str());
    if let Some(id) = &mail.in_reply_to {
        builder = builder.in_reply_to(id.clone());
    }
    if let Some(refs) = &mail.references {
        builder = builder.references(refs.clone());
    }
    for to in &mail.to {
        builder = builder.to(to
            .parse::<Mailbox>()
            .with_context(|| format!("invalid recipient {to}"))?);
    }
    for cc in &mail.cc {
        builder = builder.cc(cc
            .parse::<Mailbox>()
            .with_context(|| format!("invalid Cc address {cc}"))?);
    }

    let body = match &mail.body_html {
        Some(html) => MultiPart::alternative()
            .singlepart(SinglePart::plain(mail.body_text.clone()))
            .singlepart(SinglePart::html(html.clone())),
        None => MultiPart::alternative().singlepart(SinglePart::plain(mail.body_text.clone())),
    };
    if mail.attachments.is_empty() {
        return Ok(builder.multipart(body)?);
    }
    let mut mixed = MultiPart::mixed().multipart(body);
    for (name, mime, bytes) in &mail.attachments {
        let content_type =
            ContentType::parse(mime).unwrap_or(ContentType::parse("application/octet-stream")?);
        mixed = mixed
            .singlepart(Attachment::new(safe_file_name(name)).body(bytes.clone(), content_type));
    }
    Ok(builder.multipart(mixed)?)
}

/// Keeps only the final path component so attachment names cannot carry directories.
fn safe_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("").trim();
    let cleaned: String = base.chars().filter(|c| !c.is_control()).collect();
    if cleaned.is_empty() {
        "attachment".to_string()
    } else {
        cleaned
    }
}

/// Sends the message and returns its raw bytes for the Sent folder copy.
pub async fn send(
    settings: &SmtpSettings,
    credential: &Credential,
    from: &str,
    mail: &OutgoingMail,
) -> Result<Vec<u8>> {
    let message = build_message(from, mail)?;
    let raw = message.formatted();
    let transport = transport(settings, credential)?;
    transport.send(message).await.context("SMTP send failed")?;
    Ok(raw)
}

fn transport(
    settings: &SmtpSettings,
    credential: &Credential,
) -> Result<AsyncSmtpTransport<Tokio1Executor>> {
    let builder = AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&settings.host)
        .context("cannot set up SMTP connection")?
        .port(SMTP_PORT)
        .timeout(Some(SMTP_TIMEOUT));
    let transport = match credential {
        Credential::Password(pw) => {
            builder.credentials(Credentials::new(settings.username.clone(), pw.to_string()))
        }
        Credential::OAuth2AccessToken(tok) => builder
            .credentials(Credentials::new(settings.username.clone(), tok.to_string()))
            .authentication(vec![Mechanism::Xoauth2]),
    };
    Ok(transport.build())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mail() -> OutgoingMail {
        OutgoingMail {
            account_id: 1,
            to: vec!["Bob <bob@example.com>".into()],
            cc: vec![],
            subject: "Hello".into(),
            body_text: "Hi there".into(),
            body_html: None,
            attachments: vec![(
                "../../secret.txt".into(),
                "text/plain".into(),
                b"x".to_vec(),
            )],
            in_reply_to: None,
            references: None,
        }
    }

    #[test]
    fn writes_threading_headers_when_replying() {
        let mut m = mail();
        m.in_reply_to = Some("<orig@example.com>".into());
        m.references = Some("<root@example.com> <orig@example.com>".into());
        let raw = build_message("me@example.com", &m).unwrap().formatted();
        let text = String::from_utf8_lossy(&raw);
        assert!(text.contains("In-Reply-To: <orig@example.com>"));
        assert!(text.contains("References: <root@example.com> <orig@example.com>"));
    }

    #[test]
    fn builds_message_with_sanitized_attachment_name() {
        let raw = build_message("me@example.com", &mail())
            .unwrap()
            .formatted();
        let text = String::from_utf8_lossy(&raw);
        assert!(text.contains("To: Bob <bob@example.com>"));
        assert!(text.contains("filename=\"secret.txt\""));
        assert!(!text.contains("../"));
    }

    #[test]
    fn rejects_mail_without_recipients() {
        let mut m = mail();
        m.to.clear();
        assert!(build_message("me@example.com", &m).is_err());
    }

    #[test]
    fn rejects_invalid_recipient() {
        let mut m = mail();
        m.to = vec!["not an address".into()];
        assert!(build_message("me@example.com", &m).is_err());
    }
}
