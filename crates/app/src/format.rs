//! Pure formatting and defaulting helpers for the UI layer.
use chrono::{DateTime, Datelike, Local, TimeZone};

/// `"Alice Example <alice@example.com>, Bob <b@x.org>"` -> `"Alice Example"`.
/// Falls back to the address when there is no display name.
pub fn display_name(sender: &str) -> String {
    let s = sender.trim();
    match s.find('<') {
        Some(i) if !s[..i].trim().is_empty() => s[..i].trim().trim_matches('"').trim().to_string(),
        Some(i) => s[i + 1..].split('>').next().unwrap_or("").trim().to_string(),
        None => s.split(',').next().unwrap_or(s).trim().to_string(),
    }
}

/// Up to two letters for an avatar, from the first and last word of a display name.
pub fn initials(name: &str) -> String {
    let words: Vec<&str> = name
        .split_whitespace()
        .filter(|w| w.chars().next().is_some_and(char::is_alphanumeric))
        .collect();
    let letters: String = match words.as_slice() {
        [] => return "?".to_string(),
        [only] => only.chars().take(2).collect(),
        [first, .., last] => first.chars().next().into_iter().chain(last.chars().next()).collect(),
    };
    letters.to_uppercase()
}

const AVATAR_PALETTE: [(u8, u8, u8); 8] = [
    (0x25, 0x63, 0xEB),
    (0x0D, 0x94, 0x88),
    (0x7C, 0x3A, 0xED),
    (0xDB, 0x27, 0x77),
    (0xDC, 0x26, 0x26),
    (0xEA, 0x58, 0x0C),
    (0x0E, 0x74, 0x90),
    (0x16, 0xA3, 0x4A),
];

/// A color that is the same for the same sender name on every launch.
pub fn avatar_rgb(name: &str) -> (u8, u8, u8) {
    let hash = name.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    AVATAR_PALETTE[hash as usize % AVATAR_PALETTE.len()]
}

/// Short list timestamp: time today, "Mon D" this year, otherwise "YYYY-MM-DD". Local time.
pub fn format_list_date(ts: i64, now_ts: i64) -> String {
    format_list_date_in(&Local, ts, now_ts)
}

pub fn format_list_date_in<Tz: TimeZone>(tz: &Tz, ts: i64, now_ts: i64) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let (Some(t), Some(now)) = (DateTime::from_timestamp(ts, 0), DateTime::from_timestamp(now_ts, 0)) else {
        return String::new();
    };
    let (t, now) = (t.with_timezone(tz), now.with_timezone(tz));
    if t.date_naive() == now.date_naive() {
        t.format("%H:%M").to_string()
    } else if t.year() == now.year() {
        t.format("%b %-d").to_string()
    } else {
        t.format("%Y-%m-%d").to_string()
    }
}

/// Full timestamp for the reading pane header, in local time.
pub fn format_full_date(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|t| t.with_timezone(&Local).format("%a, %d %b %Y %H:%M").to_string())
        .unwrap_or_default()
}

/// Date bucket shown as a header in the message list (local calendar days).
pub fn date_group(ts: i64, now_ts: i64) -> &'static str {
    date_group_in(&Local, ts, now_ts)
}

pub fn date_group_in<Tz: TimeZone>(tz: &Tz, ts: i64, now_ts: i64) -> &'static str
where
    Tz::Offset: std::fmt::Display,
{
    let (Some(t), Some(now)) = (DateTime::from_timestamp(ts, 0), DateTime::from_timestamp(now_ts, 0)) else {
        return "Older";
    };
    let days = now
        .with_timezone(tz)
        .date_naive()
        .signed_duration_since(t.with_timezone(tz).date_naive())
        .num_days();
    match days {
        d if d <= 0 => "Today",
        1 => "Yesterday",
        2..=7 => "Last week",
        8..=30 => "Last month",
        31..=90 => "Last 3 months",
        91..=180 => "Last 6 months",
        _ => "Older",
    }
}

pub fn human_size(bytes: usize) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerDefaults {
    pub imap_host: String,
    pub smtp_host: String,
}

/// Guesses IMAP/SMTP hosts from an email address for well-known providers.
pub fn provider_defaults(email: &str) -> Option<ServerDefaults> {
    let domain = email.rsplit_once('@')?.1.trim().to_ascii_lowercase();
    if domain.is_empty() || !domain.contains('.') {
        return None;
    }
    let (imap, smtp) = match domain.as_str() {
        "gmail.com" | "googlemail.com" => ("imap.gmail.com".to_string(), "smtp.gmail.com".to_string()),
        "outlook.com" | "hotmail.com" | "live.com" | "msn.com" => {
            ("outlook.office365.com".to_string(), "smtp.office365.com".to_string())
        }
        "yahoo.com" | "ymail.com" => ("imap.mail.yahoo.com".to_string(), "smtp.mail.yahoo.com".to_string()),
        "icloud.com" | "me.com" => ("imap.mail.me.com".to_string(), "smtp.mail.me.com".to_string()),
        d => (format!("imap.{d}"), format!("smtp.{d}")),
    };
    Some(ServerDefaults { imap_host: imap, smtp_host: smtp })
}

/// Splits `host[:port]`; falls back to `default_port` when the port is absent or invalid.
pub fn split_host_port(s: &str, default_port: u16) -> (String, u16) {
    let s = s.trim();
    match s.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() => match p.parse::<u16>() {
            Ok(port) if port != 0 => (h.to_string(), port),
            _ => (h.to_string(), default_port),
        },
        _ => (s.to_string(), default_port),
    }
}

/// Sidebar label for a folder. Roles follow Gmail's own names (Spam, All Mail, Starred).
pub fn folder_title(name: &str, role: Option<&str>) -> String {
    match role {
        Some("inbox") => "Inbox".into(),
        Some("sent") => "Sent".into(),
        Some("drafts") => "Drafts".into(),
        Some("archive") => "All Mail".into(),
        Some("trash") => "Trash".into(),
        Some("junk") => "Spam".into(),
        Some("starred") => "Starred".into(),
        Some("important") => "Important".into(),
        _ => name.rsplit(['/', '.']).next().filter(|s| !s.is_empty()).unwrap_or(name).to_string(),
    }
}

/// Name of the sidebar icon (see `MailIcon` in `widgets.slint`) for a folder role.
pub fn folder_icon(role: Option<&str>) -> &'static str {
    match role {
        Some("inbox") => "tray",
        Some("sent") => "send",
        Some("drafts") => "doc",
        Some("archive") => "archive",
        Some("trash") => "trash",
        Some("junk") => "warning",
        Some("starred") => "star",
        Some("important") => "tag",
        _ => "folder",
    }
}

pub fn looks_like_email(s: &str) -> bool {
    let s = s.trim();
    match s.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && !s.chars().any(|c| c.is_whitespace() || c.is_control())
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn date_groups_follow_gmail_style_buckets() {
        let now = 1_700_000_000; // 2023-11-14 22:13 UTC
        let day = 86_400;
        assert_eq!(date_group_in(&Utc, now - 60, now), "Today");
        assert_eq!(date_group_in(&Utc, now - day, now), "Yesterday");
        assert_eq!(date_group_in(&Utc, now - 5 * day, now), "Last week");
        assert_eq!(date_group_in(&Utc, now - 20 * day, now), "Last month");
        assert_eq!(date_group_in(&Utc, now - 60 * day, now), "Last 3 months");
        assert_eq!(date_group_in(&Utc, now - 150 * day, now), "Last 6 months");
        assert_eq!(date_group_in(&Utc, now - 400 * day, now), "Older");
    }

    #[test]
    fn display_names() {
        assert_eq!(display_name("Alice Example <alice@example.com>"), "Alice Example");
        assert_eq!(display_name("\"Doe, John\" <j@x.org>"), "Doe, John");
        assert_eq!(display_name("A <a@x.org>, B <b@x.org>"), "A");
        assert_eq!(display_name("<bare@example.com>"), "bare@example.com");
        assert_eq!(display_name("plain@example.com, other@x.org"), "plain@example.com");
        assert_eq!(display_name(""), "");
    }

    #[test]
    fn list_dates_use_time_month_or_full_date() {
        let now = 1_793_700_000; // 2026-11-03 10:00 UTC
        assert_eq!(format_list_date_in(&Utc, now - 3600, now), "09:00");
        assert_eq!(format_list_date_in(&Utc, now - 5 * 86_400, now), "Oct 29");
        assert_eq!(format_list_date_in(&Utc, now - 400 * 86_400, now), "2025-09-29");
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn provider_guessing() {
        let g = provider_defaults("me@gmail.com").unwrap();
        assert_eq!((g.imap_host.as_str(), g.smtp_host.as_str()), ("imap.gmail.com", "smtp.gmail.com"));
        assert_eq!(provider_defaults("me@Example.ORG").unwrap().imap_host, "imap.example.org");
        assert!(provider_defaults("nonsense").is_none());
        assert!(provider_defaults("me@localhost").is_none());
    }

    #[test]
    fn host_port_and_folder_titles() {
        assert_eq!(split_host_port("imap.x.com", 993), ("imap.x.com".into(), 993));
        assert_eq!(split_host_port("imap.x.com:143", 993), ("imap.x.com".into(), 143));
        assert_eq!(split_host_port("imap.x.com:abc", 993), ("imap.x.com".into(), 993));
        assert_eq!(folder_title("[Gmail]/Sent Mail", Some("sent")), "Sent");
        assert_eq!(folder_title("Work/Clients", None), "Clients");
        assert_eq!(folder_title("Projects", None), "Projects");
    }

    #[test]
    fn email_validation() {
        assert!(looks_like_email("a@b.co"));
        assert!(!looks_like_email("a@b"));
        assert!(!looks_like_email("@b.co"));
        assert!(!looks_like_email("a b@c.de"));
        assert!(!looks_like_email("a@.com"));
    }
}
