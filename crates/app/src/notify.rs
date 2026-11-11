//! Desktop toast notifications for newly arrived mail. Shows sender and subject only, never the body.
use pigeonmail_net::NewMailNotice;
use notify_rust::Notification;

/// Lines shown in the toast body, newest first.
const MAX_LINES: usize = 3;

pub fn new_mail(count: usize, latest: &[NewMailNotice]) {
    let Some(newest) = latest.last() else {
        return;
    };
    let summary = if count == 1 {
        format!("New mail from {}", display_name(&newest.sender))
    } else {
        format!("{count} new messages")
    };
    let body = latest
        .iter()
        .rev()
        .take(MAX_LINES)
        .map(|m| {
            let clip = if m.has_attachment { "📎 " } else { "" };
            let subject = if m.subject.trim().is_empty() {
                "(no subject)"
            } else {
                m.subject.trim()
            };
            format!("{clip}{subject}\n{}", display_name(&m.sender))
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    // A toast failure must not affect mail sync, so the error is ignored.
    let _ = Notification::new()
        .appname("PigeonMail")
        .summary(&summary)
        .body(&body)
        .show();
}

/// Sender name without the address part, e.g. `Ana <ana@x.com>` becomes `Ana`.
fn display_name(sender: &str) -> &str {
    let name = sender.split('<').next().unwrap_or(sender).trim().trim_matches('"').trim();
    if name.is_empty() {
        sender.trim()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::display_name;

    #[test]
    fn strips_address_from_sender() {
        assert_eq!(display_name("Ana Diaz <ana@example.com>"), "Ana Diaz");
        assert_eq!(display_name("\"Ana\" <ana@example.com>"), "Ana");
        assert_eq!(display_name("ana@example.com"), "ana@example.com");
        assert_eq!(display_name("<ana@example.com>"), "<ana@example.com>");
    }
}
