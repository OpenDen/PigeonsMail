//! MIME interpretation for display: body selection, attachment metadata and inline images.
use mail_parser::{MessageParser, PartRole};

/// Inline images larger than this are ignored to bound memory use.
const MAX_INLINE_IMAGE_BYTES: usize = 5 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentMeta {
    /// Position of the part in the message; pass to [`extract_attachment`].
    pub part_index: usize,
    pub filename: String,
    pub mime: String,
    pub size_bytes: usize,
}

#[derive(Debug, Clone, Default)]
pub struct ParsedEmail {
    /// Concatenated HTML body parts. Plain-text bodies are converted to HTML.
    pub html: Option<String>,
    pub attachments: Vec<AttachmentMeta>,
    /// `(content-id without angle brackets, bytes)` for images referenced via `cid:`.
    pub cid_images: Vec<(String, Vec<u8>)>,
}

fn mime_of(part: &mail_parser::MessagePart<'_>) -> String {
    match part.content_type() {
        Some(ct) => match ct.subtype() {
            Some(sub) => format!(
                "{}/{}",
                ct.ctype().to_ascii_lowercase(),
                sub.to_ascii_lowercase()
            ),
            None => ct.ctype().to_ascii_lowercase(),
        },
        None => "application/octet-stream".to_string(),
    }
}

fn clean_filename(name: &str) -> String {
    // Keep only the final path component and replace characters rejected by Windows filesystems.
    let leaf = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = leaf
        .chars()
        .filter_map(|c| {
            if c.is_control() {
                None
            } else if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                Some('_')
            } else {
                Some(c)
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').to_string();
    let stem = cleaned.split('.').next().unwrap_or("").to_ascii_uppercase();
    if cleaned.is_empty()
        || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix)
                .is_some_and(|suffix| suffix.len() == 1 && suffix.as_bytes()[0].is_ascii_digit())
        })
    {
        "attachment".to_string()
    } else {
        cleaned
    }
}

/// Threading headers and plain text of a received message, used to draft replies and forwards.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuoteSource {
    /// Message-ID without angle brackets.
    pub message_id: Option<String>,
    /// Ids from the `References` header, without angle brackets.
    pub references: Vec<String>,
    pub text: String,
}

/// Reads the threading headers and plain-text body of a raw RFC 822 message.
pub fn quote_source(raw: &[u8]) -> Option<QuoteSource> {
    let msg = MessageParser::default().parse(raw)?;
    Some(QuoteSource {
        message_id: msg.message_id().map(str::to_string),
        references: msg
            .references()
            .map(|l| l.iter().map(str::to_string).collect())
            .unwrap_or_default(),
        text: msg.body_text(0).map(|t| t.into_owned()).unwrap_or_default(),
    })
}

/// Parses a raw RFC 822 message.
pub fn parse_email(raw: &[u8]) -> Option<ParsedEmail> {
    let msg = MessageParser::default().parse(raw)?;

    let mut bodies: Vec<String> = msg.html_bodies().map(|b| b.into_owned()).collect();
    if bodies.is_empty() {
        bodies = msg.text_bodies().map(|t| text_to_html(&t)).collect();
    }
    let html = if bodies.is_empty() {
        None
    } else {
        Some(bodies.join("\n<hr>\n"))
    };

    let mut attachments = Vec::new();
    let mut cid_images = Vec::new();
    for (index, part) in msg.parts().enumerate() {
        if part.is_multipart() {
            continue;
        }
        let mime = mime_of(&part);
        if let Some(cid) = part.content_id() {
            if mime.starts_with("image/") {
                let cid = cid
                    .trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_string();
                if part.decoded_len() <= MAX_INLINE_IMAGE_BYTES && !cid.is_empty() {
                    cid_images.push((cid, part.decoded().into_owned()));
                }
                continue;
            }
        }
        if part.role() == PartRole::Attachment && !part.is_inline() {
            attachments.push(AttachmentMeta {
                part_index: index,
                filename: clean_filename(part.attachment_name().unwrap_or("")),
                mime,
                size_bytes: part.decoded_len(),
            });
        }
    }

    Some(ParsedEmail {
        html,
        attachments,
        cid_images,
    })
}

/// Decodes one attachment on demand, so attachment contents are never kept in memory by default.
/// Returns `(filename, mime, bytes)`.
pub fn extract_attachment(raw: &[u8], part_index: usize) -> Option<(String, String, Vec<u8>)> {
    let msg = MessageParser::default().parse(raw)?;
    let part = msg.parts().nth(part_index)?;
    if part.is_multipart() || part.role() != PartRole::Attachment || part.is_inline() {
        return None;
    }
    Some((
        clean_filename(part.attachment_name().unwrap_or("")),
        mime_of(&part),
        part.decoded().into_owned(),
    ))
}

/// Search/list data derived from a downloaded message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    /// Plain text of the body, capped, for full-text indexing.
    pub text: String,
    /// First ~160 characters with whitespace collapsed, for the message list.
    pub snippet: String,
    pub has_attachment: bool,
}

const MAX_INDEX_TEXT_BYTES: usize = 200 * 1024;

pub fn preview(raw: &[u8]) -> Option<Preview> {
    let msg = MessageParser::default().parse(raw)?;
    let mut text = String::new();
    for t in msg.text_bodies() {
        text.push_str(&t);
        text.push('\n');
        if text.len() >= MAX_INDEX_TEXT_BYTES {
            break;
        }
    }
    if text.len() > MAX_INDEX_TEXT_BYTES {
        let mut cut = MAX_INDEX_TEXT_BYTES;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    let snippet: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(160)
        .collect();
    let has_attachment = msg
        .parts()
        .any(|p| !p.is_multipart() && p.role() == PartRole::Attachment && !p.is_inline());
    Some(Preview {
        text,
        snippet,
        has_attachment,
    })
}

pub(crate) fn text_to_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 32);
    out.push_str("<div style=\"font-family: Arial, sans-serif; font-size: 14px;\">");
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => {}
            '\n' => out.push_str("<br>"),
            ' ' => out.push_str("&nbsp;"),
            c => out.push(c),
        }
    }
    out.push_str("</div>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MULTIPART: &[u8] = b"From: a@example.com\r\n\
Subject: Test\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"outer\"\r\n\r\n\
--outer\r\n\
Content-Type: multipart/related; boundary=\"inner\"\r\n\r\n\
--inner\r\n\
Content-Type: text/html; charset=utf-8\r\n\r\n\
<p>Hello <b>world</b></p><img src=\"cid:logo@x\">\r\n\
--inner\r\n\
Content-Type: image/png\r\n\
Content-ID: <logo@x>\r\n\
Content-Transfer-Encoding: base64\r\n\r\n\
iVBORw0KGgo=\r\n\
--inner--\r\n\
--outer\r\n\
Content-Type: application/pdf; name=\"../report.pdf\"\r\n\
Content-Disposition: attachment; filename=\"../report.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\r\n\
JVBERi0xLjQ=\r\n\
--outer--\r\n";

    #[test]
    fn extracts_html_attachments_and_cid_images() {
        let p = parse_email(MULTIPART).unwrap();
        assert!(p.html.as_deref().unwrap().contains("<b>world</b>"));
        assert_eq!(p.cid_images.len(), 1);
        assert_eq!(p.cid_images[0].0, "logo@x");
        assert_eq!(p.attachments.len(), 1);
        let a = &p.attachments[0];
        assert_eq!(a.filename, "report.pdf");
        assert_eq!(a.mime, "application/pdf");
        assert_eq!(a.size_bytes, 8);
    }

    #[test]
    fn extracts_attachment_bytes_on_demand() {
        let p = parse_email(MULTIPART).unwrap();
        let (name, mime, bytes) =
            extract_attachment(MULTIPART, p.attachments[0].part_index).unwrap();
        assert_eq!(
            (name.as_str(), mime.as_str()),
            ("report.pdf", "application/pdf")
        );
        assert_eq!(bytes, b"%PDF-1.4");
    }

    #[test]
    fn attachment_names_are_safe_for_user_selected_paths() {
        assert_eq!(clean_filename(r"..\report:name?.pdf"), "report_name_.pdf");
        assert_eq!(clean_filename("CON.txt"), "attachment");
    }

    #[test]
    fn plain_text_is_escaped_into_html() {
        let raw = b"Subject: x\r\nContent-Type: text/plain\r\n\r\n1 < 2 & <script>\r\nline two";
        let html = parse_email(raw).unwrap().html.unwrap();
        assert!(!html.contains("<script>"));
        assert!(html.contains("line"));
    }

    #[test]
    fn preview_has_snippet_text_and_attachment_flag() {
        let p = preview(MULTIPART).unwrap();
        assert!(p.has_attachment);
        assert!(p.snippet.starts_with("Hello world"));
        assert!(p.text.contains("Hello"));
        assert!(!p.text.contains("<b>"));
        let plain = preview(b"Subject: x\r\n\r\nshort  body\r\n\r\nwith   gaps").unwrap();
        assert_eq!(plain.snippet, "short body with gaps");
        assert!(!plain.has_attachment);
    }

    #[test]
    fn filename_sanitising() {
        assert_eq!(clean_filename("..\\..\\evil.exe"), "evil.exe");
        assert_eq!(clean_filename("  "), "attachment");
        assert_eq!(clean_filename("a\u{0}b.txt"), "ab.txt");
    }
}
