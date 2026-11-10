//! Compose glue: converts between the compose window's fields and outgoing mail. Holds no UI calls.
use mailthin_core::compose::{self, Draft, Original};
use mailthin_core::{AccountId, OutgoingMail};
use pulldown_cmark::{html, Event, Options, Parser, Tag};

/// Threading and account context of the message being composed.
#[derive(Debug, Clone, Default)]
pub struct ComposeState {
    pub account_id: AccountId,
    pub in_reply_to: Option<String>,
    pub references: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyKind {
    Reply,
    ReplyAll,
    Forward,
}

/// Builds the draft for a reply or forward. `me` is the sending account's address.
pub fn draft_for(kind: ReplyKind, orig: &Original<'_>, me: &str) -> Draft {
    match kind {
        ReplyKind::Reply => compose::reply(orig, me, false),
        ReplyKind::ReplyAll => compose::reply(orig, me, true),
        ReplyKind::Forward => compose::forward(orig),
    }
}

/// The outgoing message for the fields typed in the compose window.
pub fn to_outgoing(state: &ComposeState, fields: &Draft) -> OutgoingMail {
    let (body_text, body_html) = render_markdown(&fields.body);
    OutgoingMail {
        account_id: state.account_id,
        to: compose::split_addresses(&fields.to),
        cc: compose::split_addresses(&fields.cc),
        subject: fields.subject.clone(),
        body_text,
        body_html: Some(body_html),
        attachments: Vec::new(),
        in_reply_to: state.in_reply_to.clone(),
        references: state.references.clone(),
    }
}

/// Converts editor Markdown to plain and HTML alternatives without accepting raw HTML or remote
/// images from the draft. Only ordinary web and mail links are preserved.
pub fn render_markdown(markdown: &str) -> (String, String) {
    let parser = Parser::new_ext(
        markdown,
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES,
    );
    let mut plain = String::with_capacity(markdown.len());
    let events: Vec<_> = parser
        .filter_map(|event| match event {
            Event::Html(_) | Event::InlineHtml(_) => None,
            Event::Start(Tag::Image { .. }) | Event::End(pulldown_cmark::TagEnd::Image) => None,
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) => {
                let url = dest_url.trim();
                let safe = ["https://", "http://", "mailto:"].iter().any(|scheme| {
                    url.get(..scheme.len())
                        .is_some_and(|s| s.eq_ignore_ascii_case(scheme))
                });
                Some(Event::Start(Tag::Link {
                    link_type,
                    dest_url: if safe { dest_url } else { "".into() },
                    title,
                    id,
                }))
            }
            other => Some(other),
        })
        .collect();

    for event in &events {
        match event {
            Event::Text(text) | Event::Code(text) => plain.push_str(text),
            Event::SoftBreak | Event::HardBreak | Event::Rule => plain.push('\n'),
            Event::End(pulldown_cmark::TagEnd::Paragraph | pulldown_cmark::TagEnd::Heading(_)) => {
                plain.push('\n');
            }
            _ => {}
        }
    }

    let mut html_body = String::with_capacity(markdown.len() + 64);
    html::push_html(&mut html_body, events.into_iter());
    let html_body = format!(
        "<div style=\"font-family:Arial,sans-serif;font-size:14px;line-height:1.5\">{html_body}</div>"
    );
    (plain.trim().to_string(), html_body)
}

/// Adds a useful Markdown starter at the end of the editor without depending on widget internals.
pub fn insert_format_template(body: &mut String, format: &str) {
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    let template = match format {
        "bold" => "**bold text**",
        "italic" => "*italic text*",
        "link" => "[link text](https://example.com)",
        "quote" => "> quoted text",
        "list" => "- list item",
        _ => return,
    };
    body.push_str(template);
}

/// Recovers the editable fields of a message that failed to send, so the user can fix and resend it.
pub fn draft_of(mail: &OutgoingMail) -> Draft {
    Draft {
        to: mail.to.join(", "),
        cc: mail.cc.join(", "),
        subject: mail.subject.clone(),
        body: mail.body_text.clone(),
        in_reply_to: mail.in_reply_to.clone(),
        references: mail.references.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ComposeState {
        ComposeState {
            account_id: 3,
            in_reply_to: Some("<a@x>".into()),
            references: Some("<a@x>".into()),
        }
    }

    #[test]
    fn outgoing_splits_recipients_and_keeps_threading() {
        let fields = Draft {
            to: "a@x.com; b@x.com".into(),
            cc: " c@x.com ".into(),
            subject: "Hi".into(),
            body: "Body".into(),
            ..Default::default()
        };
        let mail = to_outgoing(&state(), &fields);
        assert_eq!(mail.account_id, 3);
        assert_eq!(mail.to, vec!["a@x.com", "b@x.com"]);
        assert_eq!(mail.cc, vec!["c@x.com"]);
        assert_eq!(mail.in_reply_to.as_deref(), Some("<a@x>"));
    }

    #[test]
    fn failed_mail_restores_its_fields() {
        let fields = Draft {
            to: "a@x.com".into(),
            subject: "Hi".into(),
            body: "Body".into(),
            ..Default::default()
        };
        let restored = draft_of(&to_outgoing(&state(), &fields));
        assert_eq!(restored.to, "a@x.com");
        assert_eq!(restored.subject, "Hi");
        assert_eq!(restored.body, "Body");
        assert_eq!(restored.in_reply_to.as_deref(), Some("<a@x>"));
    }

    #[test]
    fn markdown_produces_rich_html_and_plain_text() {
        let (plain, html) = render_markdown("Hello **world**\n\n[site](https://example.com)");
        assert!(plain.contains("Hello world"));
        assert!(html.contains("<strong>world</strong>"));
        assert!(html.contains("href=\"https://example.com\""));
    }

    #[test]
    fn markdown_drops_raw_html_remote_images_and_unsafe_links() {
        let (_, html) = render_markdown("<script>alert(1)</script>\n\n![remote](https://x.test/a.png)\n\n[unsafe](javascript:alert(1))");
        assert!(!html.contains("<script"));
        assert!(!html.contains("<img"));
        assert!(!html.contains("javascript:"));
    }
}
