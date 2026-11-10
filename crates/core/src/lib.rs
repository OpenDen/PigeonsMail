//! Synchronous domain models and rules engine. No network or database dependencies.
use serde::{Deserialize, Serialize};

pub type AccountId = i64;
pub type FolderId = i64;
pub type MessageId = i64;
pub type LabelId = i64;

/// Bit flags stored in `messages.flags`.
pub mod flags {
    pub const SEEN: u32 = 1;
    pub const FLAGGED: u32 = 1 << 1;
    pub const ANSWERED: u32 = 1 << 2;
    pub const DRAFT: u32 = 1 << 3;
    pub const DELETED: u32 = 1 << 4;
    pub const HAS_ATTACHMENT: u32 = 1 << 5;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageSummary {
    pub id: MessageId,
    pub account_id: AccountId,
    pub folder_id: FolderId,
    pub uid: u32,
    pub from: String,
    pub to: String,
    pub subject: String,
    pub date_ts: i64,
    pub snippet: String,
    pub is_read: bool,
    pub is_starred: bool,
    pub has_attachment: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutgoingMail {
    pub account_id: AccountId,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub body_text: String,
    pub body_html: Option<String>,
    /// (filename, mime, bytes)
    pub attachments: Vec<(String, String, Vec<u8>)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub id: i64,
    pub name: String,
    pub enabled: bool,
    pub priority: i32,
    pub match_all: bool,
    pub conditions: Vec<Condition>,
    pub actions: Vec<RuleAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Condition {
    FromContains(String),
    ToContains(String),
    SubjectContains(String),
    HasAttachment,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuleAction {
    MoveToFolder(FolderId),
    AddLabel(LabelId),
    MarkRead,
    Star,
    Delete,
    Archive,
    StopProcessing,
}

impl Condition {
    fn eval(&self, msg: &MessageSummary) -> bool {
        match self {
            Condition::FromContains(s) => contains_ci(&msg.from, s),
            Condition::ToContains(s) => contains_ci(&msg.to, s),
            Condition::SubjectContains(s) => contains_ci(&msg.subject, s),
            Condition::HasAttachment => msg.has_attachment,
        }
    }
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

impl Rule {
    pub fn matches(&self, msg: &MessageSummary) -> bool {
        if !self.enabled || self.conditions.is_empty() {
            return false;
        }
        if self.match_all {
            self.conditions.iter().all(|c| c.eval(msg))
        } else {
            self.conditions.iter().any(|c| c.eval(msg))
        }
    }
}

/// Evaluates rules in ascending `priority` order (stable for ties) and returns the
/// triggered actions. Processing stops after a `StopProcessing` action.
pub fn evaluate_rules(rules: &[Rule], msg: &MessageSummary) -> Vec<RuleAction> {
    let mut ordered: Vec<&Rule> = rules.iter().collect();
    ordered.sort_by_key(|r| r.priority);

    let mut triggered = Vec::new();
    for rule in ordered {
        if rule.matches(msg) {
            for action in &rule.actions {
                triggered.push(action.clone());
                if matches!(action, RuleAction::StopProcessing) {
                    return triggered;
                }
            }
        }
    }
    triggered
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg() -> MessageSummary {
        MessageSummary {
            id: 1,
            account_id: 1,
            folder_id: 1,
            uid: 10,
            from: "Alice <Alice@Example.com>".into(),
            to: "me@example.org".into(),
            subject: "Quarterly Report".into(),
            date_ts: 0,
            snippet: String::new(),
            is_read: false,
            is_starred: false,
            has_attachment: false,
        }
    }

    fn rule(priority: i32, match_all: bool, conditions: Vec<Condition>, actions: Vec<RuleAction>) -> Rule {
        Rule { id: priority as i64, name: "r".into(), enabled: true, priority, match_all, conditions, actions }
    }

    #[test]
    fn matches_case_insensitively() {
        let r = rule(0, true, vec![Condition::FromContains("alice@example".into())], vec![RuleAction::Star]);
        assert!(r.matches(&msg()));
    }

    #[test]
    fn match_all_requires_every_condition() {
        let conds = vec![Condition::SubjectContains("report".into()), Condition::HasAttachment];
        assert!(!rule(0, true, conds.clone(), vec![]).matches(&msg()));
        assert!(rule(0, false, conds, vec![]).matches(&msg()));
    }

    #[test]
    fn disabled_or_empty_rules_never_match() {
        let mut r = rule(0, true, vec![Condition::SubjectContains("report".into())], vec![]);
        r.enabled = false;
        assert!(!r.matches(&msg()));
        assert!(!rule(0, true, vec![], vec![]).matches(&msg()));
    }

    #[test]
    fn stop_processing_halts_later_rules() {
        let c = vec![Condition::SubjectContains("report".into())];
        let rules = vec![
            rule(2, true, c.clone(), vec![RuleAction::Delete]),
            rule(1, true, c.clone(), vec![RuleAction::MarkRead, RuleAction::StopProcessing]),
        ];
        assert_eq!(
            evaluate_rules(&rules, &msg()),
            vec![RuleAction::MarkRead, RuleAction::StopProcessing]
        );
    }

    #[test]
    fn rules_run_in_priority_order() {
        let c = vec![Condition::ToContains("example.org".into())];
        let rules = vec![
            rule(5, true, c.clone(), vec![RuleAction::Archive]),
            rule(1, true, c, vec![RuleAction::Star]),
        ];
        assert_eq!(evaluate_rules(&rules, &msg()), vec![RuleAction::Star, RuleAction::Archive]);
    }
}
