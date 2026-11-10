//! Drafting for replies and forwards. Pure functions over the original message's fields; no I/O.

/// Quoted text is capped so a huge message cannot bloat the compose window.
const QUOTE_LIMIT_CHARS: usize = 20_000;

/// The parts of a received message that a reply or forward needs.
pub struct Original<'a> {
    pub sender: &'a str,
    pub recipients: &'a str,
    pub subject: &'a str,
    pub body_text: &'a str,
    /// Message-ID without angle brackets.
    pub message_id: Option<&'a str>,
    /// Ids from the original `References` header, without angle brackets.
    pub references: &'a [String],
}

/// An editable outgoing message. Recipient fields are comma-separated text, as the user types them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Draft {
    pub to: String,
    pub cc: String,
    pub subject: String,
    pub body: String,
    pub in_reply_to: Option<String>,
    pub references: Option<String>,
}

/// Drafts a reply. With `reply_all`, the other recipients are kept, except `me`.
pub fn reply(orig: &Original<'_>, me: &str, reply_all: bool) -> Draft {
    let to = if reply_all {
        let mut seen = Vec::<String>::new();
        let mut list = Vec::new();
        for addr in std::iter::once(orig.sender)
            .chain(split_addresses(orig.recipients).iter().map(String::as_str))
        {
            let key = address_of(addr);
            if key.is_empty() || key == address_of(me) || seen.contains(&key) {
                continue;
            }
            seen.push(key);
            list.push(addr.to_string());
        }
        list.join(", ")
    } else {
        orig.sender.to_string()
    };

    let mut ids: Vec<String> = orig.references.iter().map(|r| format!("<{r}>")).collect();
    if let Some(id) = orig.message_id {
        ids.push(format!("<{id}>"));
    }

    Draft {
        to,
        cc: String::new(),
        subject: prefixed(orig.subject, "Re:", &["re:"]),
        body: format!("\n\n{} wrote:\n{}", orig.sender, quote(orig.body_text)),
        in_reply_to: orig.message_id.map(|id| format!("<{id}>")),
        references: if ids.is_empty() {
            None
        } else {
            Some(ids.join(" "))
        },
    }
}

/// Drafts a forward. Forwards start a new thread, so no threading headers are set.
pub fn forward(orig: &Original<'_>) -> Draft {
    let body: String = orig.body_text.chars().take(QUOTE_LIMIT_CHARS).collect();
    Draft {
        to: String::new(),
        cc: String::new(),
        subject: prefixed(orig.subject, "Fwd:", &["fwd:", "fw:"]),
        body: format!(
            "\n\n---------- Forwarded message ----------\nFrom: {}\nSubject: {}\n\n{}",
            orig.sender, orig.subject, body
        ),
        in_reply_to: None,
        references: None,
    }
}

/// Splits a comma- or semicolon-separated address list, dropping empty entries.
pub fn split_addresses(text: &str) -> Vec<String> {
    text.split([',', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// The bare address of a `Name <addr>` or plain `addr` entry, lowercased for comparison.
fn address_of(entry: &str) -> String {
    let inner = match (entry.find('<'), entry.rfind('>')) {
        (Some(start), Some(end)) if start < end => &entry[start + 1..end],
        _ => entry,
    };
    inner.trim().to_ascii_lowercase()
}

/// Adds `prefix` unless the subject already starts with one of `known` (case-insensitive).
fn prefixed(subject: &str, prefix: &str, known: &[&str]) -> String {
    let lower = subject.trim_start().to_ascii_lowercase();
    if known.iter().any(|k| lower.starts_with(k)) {
        subject.trim_start().to_string()
    } else {
        format!("{prefix} {}", subject.trim_start())
    }
}

/// Quotes plain text with `> `, truncated to the quote limit.
fn quote(text: &str) -> String {
    let limited: String = text.chars().take(QUOTE_LIMIT_CHARS).collect();
    limited
        .lines()
        .map(|l| format!("> {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn original() -> Original<'static> {
        Original {
            sender: "Alice <alice@example.com>",
            recipients: "me@example.com, Bob <bob@example.com>",
            subject: "Re: Lunch",
            body_text: "Noon works?",
            message_id: Some("m2@example.com"),
            references: &[],
        }
    }

    #[test]
    fn reply_does_not_double_the_subject_prefix() {
        assert_eq!(
            reply(&original(), "me@example.com", false).subject,
            "Re: Lunch"
        );
    }

    #[test]
    fn reply_sets_threading_headers() {
        let d = reply(&original(), "me@example.com", false);
        assert_eq!(d.in_reply_to.as_deref(), Some("<m2@example.com>"));
        assert_eq!(d.references.as_deref(), Some("<m2@example.com>"));
        assert_eq!(d.to, "Alice <alice@example.com>");
    }

    #[test]
    fn reply_all_drops_me_and_duplicates() {
        let mut o = original();
        o.recipients = "ME@example.com, alice@example.com, Bob <bob@example.com>";
        let d = reply(&o, "me@example.com", true);
        assert_eq!(d.to, "Alice <alice@example.com>, Bob <bob@example.com>");
    }

    #[test]
    fn reply_quotes_the_original_text() {
        let d = reply(&original(), "me@example.com", false);
        assert!(d.body.contains("> Noon works?"));
    }

    #[test]
    fn references_extend_the_existing_chain() {
        let refs = vec!["root@example.com".to_string()];
        let mut o = original();
        o.references = &refs;
        let d = reply(&o, "me@example.com", false);
        assert_eq!(
            d.references.as_deref(),
            Some("<root@example.com> <m2@example.com>")
        );
    }

    #[test]
    fn forward_adds_fwd_once_and_no_threading() {
        let mut o = original();
        o.subject = "Lunch";
        let d = forward(&o);
        assert_eq!(d.subject, "Fwd: Lunch");
        assert!(d.body.contains("Forwarded message"));
        assert!(d.in_reply_to.is_none());
        o.subject = "fw: Lunch";
        assert_eq!(forward(&o).subject, "fw: Lunch");
    }

    #[test]
    fn splits_address_lists() {
        assert_eq!(
            split_addresses(" a@x.com ; b@x.com,, "),
            vec!["a@x.com", "b@x.com"]
        );
    }
}
