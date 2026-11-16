//! Trigger rules (DESIGN §D4): data, not code.
//!
//! Rules are declared in a member's `triggers.toml`, loaded at
//! startup, and listed in the Fleet panel — a rule that is not
//! listed does not exist. bloch ships **no outbound rules**: it is
//! the fleet's lesson *sink*, so its Fleet panel honestly shows an
//! empty rule list (empty-by-design) and tails the *inbound* audit
//! (attempts / outcome activity) instead of inventing rules.
//!
//! This module owns the rule shape and a bounded parser for the
//! `[[rule]]` subset the Fleet panel needs to display. Evaluation
//! and firing live with the member that observes the event, never
//! here; the parser therefore reads display fields only and ignores
//! unknown keys.

use std::path::Path;

/// Maximum trigger rules loaded from one file.
pub const RULES_MAX: usize = 64;
/// Maximum bytes read from one `triggers.toml`.
pub const TRIGGERS_FILE_BYTES_MAX: u64 = 64_000;

/// One declared trigger rule, display shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerRule {
    /// Rule id, e.g. `theodolite-diag-to-bloch-lesson`.
    pub id: String,
    /// Origin lane stamped at birth (bloch clause, D-1030).
    pub origin_lane: String,
    /// Content label stamped at birth.
    pub content_label: String,
    /// Event name the rule observes, e.g. `diagnostics.completed`.
    pub event: String,
    /// Target member + tool, e.g. `bloch.lesson_candidate`.
    pub target: String,
    /// `auto` or `plan` (plan = surface for approval).
    pub mode: String,
    /// Whether the rule is currently enabled in the panel.
    pub enabled: bool,
}

/// Parse the `[[rule]]` blocks of a `triggers.toml` text. Malformed
/// lines are ignored (policy is data; a bad line must not invent a
/// rule), blocks without an `id` are dropped, and the result is
/// capped at [`RULES_MAX`].
pub fn parse_triggers(text: &str) -> Vec<TriggerRule> {
    let mut rules: Vec<TriggerRule> = Vec::new();
    let mut current: Option<TriggerRule> = None;
    let flush = |current: &mut Option<TriggerRule>, rules: &mut Vec<TriggerRule>| {
        if let Some(rule) = current.take() {
            if !rule.id.is_empty() && rules.len() < RULES_MAX {
                rules.push(rule);
            }
        }
    };
    for line in text.lines() {
        let t = line.trim();
        if t == "[[rule]]" {
            flush(&mut current, &mut rules);
            current = Some(TriggerRule {
                id: String::new(),
                origin_lane: String::new(),
                content_label: String::new(),
                event: String::new(),
                target: String::new(),
                mode: "plan".to_string(),
                enabled: true,
            });
            continue;
        }
        let Some(rule) = current.as_mut() else {
            continue;
        };
        let Some((key, raw)) = t.split_once('=') else {
            continue;
        };
        let value = raw.trim().trim_matches('"').to_string();
        match key.trim() {
            "id" => rule.id = value,
            "origin_lane" => rule.origin_lane = value,
            "content_label" => rule.content_label = value,
            "event" => rule.event = value,
            "mode" => rule.mode = value,
            "enabled" => rule.enabled = value != "false",
            "mcp" => rule.target = value,
            "tool" => {
                if rule.target.is_empty() {
                    rule.target = value;
                } else {
                    rule.target = format!("{}.{}", rule.target, value);
                }
            }
            _ => {}
        }
        // The D4 shape nests the target as a table on one line:
        // then = { mcp = "bloch", tool = "lesson_candidate", ... }
        if key.trim() == "then" {
            for part in raw.split(',') {
                let Some((k, v)) = part.split_once('=') else {
                    continue;
                };
                let v = v.trim().trim_matches(['"', '{', '}', ' ']).to_string();
                match k.trim() {
                    "mcp" => rule.target = v,
                    "tool" => {
                        if rule.target.is_empty() {
                            rule.target = v;
                        } else {
                            rule.target = format!("{}.{}", rule.target, v);
                        }
                    }
                    _ => {}
                }
            }
        }
        if key.trim() == "on" {
            for part in raw.split(',') {
                if let Some((k, v)) = part.split_once('=') {
                    if k.trim() == "event" {
                        rule.event = v.trim().trim_matches(['"', '{', '}', ' ']).to_string();
                    }
                }
            }
        }
    }
    flush(&mut current, &mut rules);
    rules
}

/// Load rules from `path`. A missing file is an empty rule list —
/// for bloch that is the designed state, not an error.
pub fn load_triggers(path: &Path) -> Vec<TriggerRule> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return Vec::new(),
    };
    if meta.len() > TRIGGERS_FILE_BYTES_MAX {
        return Vec::new();
    }
    match std::fs::read_to_string(path) {
        Ok(text) => parse_triggers(&text),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_yields_no_rules() {
        assert!(parse_triggers("").is_empty());
        assert!(parse_triggers("# only a comment\n").is_empty());
    }

    #[test]
    fn d4_example_rule_parses() {
        let text = "[[rule]]\n\
                    id = \"theodolite-diag-to-bloch-lesson\"\n\
                    origin_lane = \"general\"\n\
                    content_label = \"sfw\"\n\
                    mode = \"auto\"\n";
        let rules = parse_triggers(text);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, "theodolite-diag-to-bloch-lesson");
        assert_eq!(rules[0].origin_lane, "general");
        assert_eq!(rules[0].mode, "auto");
        assert!(rules[0].enabled);
    }

    #[test]
    fn block_without_id_is_dropped() {
        let text = "[[rule]]\nmode = \"auto\"\n[[rule]]\nid = \"kept\"\n";
        let rules = parse_triggers(text);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, "kept");
    }

    #[test]
    fn garbage_lines_never_invent_rules() {
        let text = "id = \"orphan\"\n[[[rule]]]\n= =\n[[rule]]\nid = \n";
        assert!(parse_triggers(text).is_empty());
    }

    #[test]
    fn rules_are_capped() {
        let mut text = String::new();
        for i in 0..(RULES_MAX + 5) {
            text.push_str(&format!("[[rule]]\nid = \"r{i}\"\n"));
        }
        assert_eq!(parse_triggers(&text).len(), RULES_MAX);
    }
}
