//! Compose glue: converts between the compose window's fields and outgoing mail. Holds no UI calls.
use mailthin_core::compose::{self, Draft, Original};
use mailthin_core::{AccountId, OutgoingMail};
use pulldown_cmark::{html, CodeBlockKind, CowStr, Event, Options, Parser, Tag, TagEnd};
use std::sync::OnceLock;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::html::highlighted_html_for_string;
use syntect::parsing::{SyntaxReference, SyntaxSet};

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
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS,
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

    let events = highlight_code_blocks(events);
    let mut html_body = String::with_capacity(markdown.len() + 64);
    html::push_html(&mut html_body, events.into_iter());
    let html_body = format!(
        "<div style=\"font-family:Arial,sans-serif;font-size:14px;line-height:1.5\">{html_body}</div>"
    );
    (plain.trim().to_string(), html_body)
}

/// Syntax definitions and a light theme, loaded once. The theme suits the white mail background.
fn highlighter() -> &'static (SyntaxSet, Theme) {
    static HIGHLIGHTER: OnceLock<(SyntaxSet, Theme)> = OnceLock::new();
    HIGHLIGHTER.get_or_init(|| {
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let mut themes = ThemeSet::load_defaults();
        let theme = themes
            .themes
            .remove("InspiredGitHub")
            .unwrap_or_default();
        (syntaxes, theme)
    })
}

/// Replaces fenced code blocks in a known language with syntax-colored HTML. The highlighter
/// escapes the code, so it cannot inject markup. Other blocks pass through unchanged.
fn highlight_code_blocks(events: Vec<Event<'_>>) -> Vec<Event<'_>> {
    let mut out = Vec::with_capacity(events.len());
    let mut iter = events.into_iter();
    while let Some(event) = iter.next() {
        let language = match &event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(lang))) => lang
                .split_whitespace()
                .next()
                .map(str::to_string)
                .filter(|lang| !lang.is_empty()),
            _ => None,
        };
        let syntax = language.and_then(|lang| {
            let (syntaxes, _) = highlighter();
            syntaxes.find_syntax_by_token(&lang).cloned()
        });
        let Some(syntax) = syntax else {
            out.push(event);
            continue;
        };
        let mut code = String::new();
        let mut closed = false;
        for inner in iter.by_ref() {
            match inner {
                Event::Text(text) => code.push_str(&text),
                Event::End(TagEnd::CodeBlock) => {
                    closed = true;
                    break;
                }
                _ => {}
            }
        }
        if !closed {
            break;
        }
        out.push(Event::Html(CowStr::from(highlight_html(&code, &syntax))));
    }
    out
}

fn highlight_html(code: &str, syntax: &SyntaxReference) -> String {
    let (syntaxes, theme) = highlighter();
    highlighted_html_for_string(code, syntaxes, syntax, theme)
        .unwrap_or_else(|_| format!("<pre>{}</pre>", html_escape(code)))
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Applies a Markdown formatting command to the selection `anchor..cursor` (UTF-8 byte offsets from
/// the editor). Returns the new body and the selection to restore. Without a selection, inline
/// commands insert a placeholder and select it; line commands act on the current line.
pub fn apply_format(body: &str, anchor: usize, cursor: usize, format: &str) -> (String, usize, usize) {
    let start = char_boundary(body, anchor.min(cursor));
    let end = char_boundary(body, anchor.max(cursor));
    match format {
        "bold" => wrap_selection(body, start, end, "**", "**", "bold text"),
        "italic" => wrap_selection(body, start, end, "*", "*", "italic text"),
        "strike" => wrap_selection(body, start, end, "~~", "~~", "struck text"),
        "code" => wrap_selection(body, start, end, "`", "`", "code"),
        "link" => insert_link(body, start, end),
        "quote" => prefix_lines(body, start, end, "> "),
        "bullet" => prefix_lines(body, start, end, "- "),
        "numbered" => map_lines(body, start, end, |index, line| format!("{}. {line}", index + 1)),
        _ => (body.to_string(), start, end),
    }
}

fn char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn wrap_selection(
    body: &str,
    start: usize,
    end: usize,
    open: &str,
    close: &str,
    placeholder: &str,
) -> (String, usize, usize) {
    let inner = if start == end { placeholder } else { &body[start..end] };
    let mut out = String::with_capacity(body.len() + open.len() + close.len() + inner.len());
    out.push_str(&body[..start]);
    out.push_str(open);
    out.push_str(inner);
    out.push_str(close);
    out.push_str(&body[end..]);
    let inner_start = start + open.len();
    (out, inner_start, inner_start + inner.len())
}

/// Inserts `[text](https://)` and selects the URL so it can be typed over immediately.
fn insert_link(body: &str, start: usize, end: usize) -> (String, usize, usize) {
    const URL: &str = "https://";
    let inner = if start == end { "link text" } else { &body[start..end] };
    let mut out = String::with_capacity(body.len() + inner.len() + URL.len() + 4);
    out.push_str(&body[..start]);
    out.push('[');
    out.push_str(inner);
    out.push_str("](");
    out.push_str(URL);
    out.push(')');
    out.push_str(&body[end..]);
    let url_start = start + inner.len() + 3;
    (out, url_start, url_start + URL.len())
}

/// Adds `prefix` to every selected line, or removes it when all of them already have it.
fn prefix_lines(body: &str, start: usize, end: usize, prefix: &str) -> (String, usize, usize) {
    let (first, last) = line_bounds(body, start, end);
    let all_marked = body[first..last].split('\n').all(|line| line.starts_with(prefix));
    map_lines(body, start, end, |_, line| {
        if all_marked {
            line.strip_prefix(prefix).unwrap_or(line).to_string()
        } else {
            format!("{prefix}{line}")
        }
    })
}

fn line_bounds(body: &str, start: usize, end: usize) -> (usize, usize) {
    let first = body[..start].rfind('\n').map_or(0, |i| i + 1);
    let last = body[end..].find('\n').map_or(body.len(), |i| end + i);
    (first, last)
}

fn map_lines(
    body: &str,
    start: usize,
    end: usize,
    transform: impl Fn(usize, &str) -> String,
) -> (String, usize, usize) {
    let (first, last) = line_bounds(body, start, end);
    let replaced = body[first..last]
        .split('\n')
        .enumerate()
        .map(|(index, line)| transform(index, line))
        .collect::<Vec<_>>()
        .join("\n");
    let mut out = String::with_capacity(body.len() + replaced.len());
    out.push_str(&body[..first]);
    out.push_str(&replaced);
    out.push_str(&body[last..]);
    (out, first, first + replaced.len())
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

    #[test]
    fn known_code_language_is_highlighted_and_escaped() {
        let (plain, html) = render_markdown("```rust\nfn main() { let x = 1 < 2; }\n```");
        assert!(html.contains("<span style="));
        assert!(html.contains("&lt;"));
        assert!(plain.contains("fn main()"));
    }

    #[test]
    fn code_block_content_cannot_inject_markup() {
        let (_, html) = render_markdown("```html\n<script>alert(1)</script>\n```");
        assert!(!html.contains("<script>"));
    }

    #[test]
    fn unknown_or_missing_language_keeps_plain_code_block() {
        let (plain, html) = render_markdown("```nosuchlang\nlet a = 1;\n```\n\n```\nplain\n```");
        assert!(html.contains("<pre><code"));
        assert!(plain.contains("let a = 1;"));
        assert!(plain.contains("plain"));
    }

    #[test]
    fn bold_wraps_selection_and_keeps_inner_text_selected() {
        let (body, start, end) = apply_format("hello world", 6, 11, "bold");
        assert_eq!(body, "hello **world**");
        assert_eq!(&body[start..end], "world");
    }

    #[test]
    fn inline_format_without_selection_inserts_selected_placeholder() {
        let (body, start, end) = apply_format("", 0, 0, "italic");
        assert_eq!(body, "*italic text*");
        assert_eq!(&body[start..end], "italic text");
    }

    #[test]
    fn link_selects_url_for_typing_over() {
        let (body, start, end) = apply_format("see docs", 4, 8, "link");
        assert_eq!(body, "see [docs](https://)");
        assert_eq!(&body[start..end], "https://");
    }

    #[test]
    fn bullet_adds_then_removes_prefix_on_selected_lines() {
        let (body, _, _) = apply_format("one\ntwo", 0, 7, "bullet");
        assert_eq!(body, "- one\n- two");
        let (body, _, _) = apply_format(&body, 0, body.len(), "bullet");
        assert_eq!(body, "one\ntwo");
    }

    #[test]
    fn numbered_list_numbers_each_selected_line() {
        let (body, _, _) = apply_format("a\nb\nc", 0, 3, "numbered");
        assert_eq!(body, "1. a\n2. b\nc");
    }

    #[test]
    fn multibyte_offsets_do_not_panic() {
        let text = "héllo wörld";
        // Offset 2 falls inside the two-byte 'é'; it snaps back to the start of 'é'.
        let (body, _, _) = apply_format(text, 2, 7, "code");
        assert_eq!(body, "h`éllo `wörld");
    }
}
