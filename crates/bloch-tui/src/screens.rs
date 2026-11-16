//! The member's screens as plain text lines.
//!
//! One rendering of each screen, shared by the full-screen draw
//! (which wraps the lines in a bordered panel) and the line mode
//! (which prints them). Screen content is a pure function of the
//! [`App`] state plus facade reads the caller performs first
//! (`refresh` for Overview / Inbox).

use crate::app::{App, Screen};
use crate::candidate::CandidateState;
use crate::facade::BlochFacade;
use crate::panels::truncate_chars;
use crate::{AUDIT_TAIL_MAX, BUDGET_CHARS_MAX, BUDGET_CHARS_MIN};

/// Lines for the app's current screen.
pub fn current_lines<F: BlochFacade>(app: &mut App<F>) -> Vec<String> {
    match app.screen {
        Screen::Overview => overview_lines(app),
        Screen::Workflows => workflows_lines(app),
        Screen::Inbox => inbox_lines(app),
        Screen::Log => app.log.clone(),
        Screen::Fleet => fleet_lines(app),
    }
}

/// Overview: versions, store status, staged action if any.
pub fn overview_lines<F: BlochFacade>(app: &App<F>) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    match &app.status {
        Some(status) => {
            lines.push(format!(
                "bloch {} · bloch-tui {}",
                status.member_version, status.tui_version
            ));
            lines.push(format!("target: {}", status.target));
            lines.push(format!(
                "host lane: {} (lesson store)",
                status.host_lane.as_str()
            ));
            lines.push(format!("MCP tools: {}", status.tool_count));
            lines.push(format!(
                "lesson candidates in inbox: {}",
                status.candidate_count
            ));
            lines.push(format!("recent lesson lines: {}", status.lesson_count));
            lines.push(format!(
                "roadmap (handoff source): {}",
                if status.has_roadmap {
                    "present"
                } else {
                    "absent"
                }
            ));
        }
        None => lines.push("status: not fetched yet (run `status`)".to_string()),
    }
    lines.push(format!(
        "mode: {}",
        if app.mode == crate::app::Mode::Plan {
            "plan"
        } else {
            "auto"
        }
    ));
    if let Some(staged) = &app.staged {
        lines.push(format!("STAGED: {} (approve / abort)", staged.label));
    }
    lines
}

/// Workflows: the map build (target picker + budget controls),
/// the handoff view, and their last results.
pub fn workflows_lines<F: BlochFacade>(app: &App<F>) -> Vec<String> {
    let mut lines = vec![
        "workflows:".to_string(),
        format!(
            "  map: target {} · budget {} chars (bounds {}..={}) · scip {}",
            app.target.display(),
            app.budget_chars,
            BUDGET_CHARS_MIN,
            BUDGET_CHARS_MAX,
            if app.use_scip { "on" } else { "off" }
        ),
        "  handoff: session state + memory + test status + roadmap".to_string(),
        "  provider: n/a — bloch makes no model calls (control absent, not faked)".to_string(),
    ];
    if let Some(summary) = &app.last_map {
        lines.push(format!(
            "last map: {} files, {} definitions, {} chars (budget {}, {})",
            summary.files,
            summary.definitions,
            summary.total_chars,
            summary.budget_chars,
            if summary.truncated {
                "truncated to budget"
            } else {
                "within budget"
            }
        ));
        for (lang, count) in &summary.languages {
            lines.push(format!("  {lang}: {count} files"));
        }
    }
    if !app.handoff_text.is_empty() {
        lines.push("handoff:".to_string());
        for line in app.handoff_text.lines().take(20) {
            lines.push(format!("  {}", truncate_chars(line, 110)));
        }
    }
    if let Some(staged) = &app.staged {
        lines.push(format!("STAGED: {} (approve / abort)", staged.label));
    }
    lines
}

/// The Lesson Candidates inbox: every candidate with its birth
/// labels, its disposition for this host, and — for quarantined
/// or malformed rows — the exact reason the accept path refuses.
pub fn inbox_lines<F: BlochFacade>(app: &App<F>) -> Vec<String> {
    let host = app.host_lane();
    let mut lines = vec![format!("lesson candidates (host lane: {}):", host.as_str())];
    if app.candidates.is_empty() {
        lines.push("  (inbox empty)".to_string());
        return lines;
    }
    for (idx, candidate) in app.candidates.iter().enumerate() {
        let marker = if idx == app.selected { ">" } else { " " };
        let (state, reason) = candidate.disposition(host);
        let state_text = match state {
            CandidateState::Pending => "pending",
            CandidateState::Quarantined => "QUARANTINED",
            CandidateState::Malformed => "MALFORMED",
        };
        lines.push(format!(
            "{marker} {} [{}] {} — {}",
            candidate.id,
            candidate.label_summary(),
            state_text,
            truncate_chars(&candidate.rule_id, 40)
        ));
        lines.push(format!(
            "    evidence: {}",
            truncate_chars(&candidate.evidence, 90)
        ));
        if let Some(reason) = reason {
            lines.push(format!("    refused: {reason}"));
        }
    }
    if let Some(staged) = &app.staged {
        lines.push(format!("STAGED: {} (approve / abort)", staged.label));
    }
    lines
}

/// Fleet: outbound rules (empty by design for the sink) and the
/// inbound audit tail.
pub fn fleet_lines<F: BlochFacade>(app: &mut App<F>) -> Vec<String> {
    let rules = app.facade.trigger_rules();
    let audit = app.facade.audit_tail(AUDIT_TAIL_MAX);
    let mut lines: Vec<String> = Vec::new();
    if rules.is_empty() {
        lines
            .push("outbound trigger rules: none (empty by design — bloch is the sink)".to_string());
    } else {
        lines.push("outbound trigger rules:".to_string());
        for rule in &rules {
            lines.push(format!(
                "  {} [{}] on {} -> {} ({})",
                rule.id,
                if rule.enabled { "on" } else { "off" },
                rule.event,
                rule.target,
                rule.mode
            ));
        }
    }
    lines.push("inbound audit (tail):".to_string());
    if audit.is_empty() {
        lines.push("  (no audit lines yet)".to_string());
    } else {
        for line in &audit {
            lines.push(format!("  {}", truncate_chars(line, 110)));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Mode;
    use crate::candidate::LessonCandidate;
    use crate::facade::FakeFacade;
    use bloch::{ContentLabel, Lane};
    use std::path::PathBuf;

    fn app_with(candidates: Vec<LessonCandidate>) -> App<FakeFacade> {
        let mut facade = FakeFacade::new();
        facade.candidates = candidates;
        let mut app = App::new(facade, PathBuf::from("/tmp/fake-repo"));
        app.refresh().unwrap();
        app
    }

    fn candidate(id: &str, origin: Lane, label: ContentLabel) -> LessonCandidate {
        LessonCandidate {
            id: id.to_string(),
            rule_id: "rule-1".to_string(),
            origin_lane: Some(origin),
            content_label: Some(label),
            evidence: "gate output".to_string(),
            payload: "lesson".to_string(),
            source_file: None,
        }
    }

    #[test]
    fn inbox_lines_show_quarantine_and_reason() {
        let app = app_with(vec![candidate(
            "bad-1",
            Lane::Restricted,
            ContentLabel::Nsfw,
        )]);
        let text = inbox_lines(&app).join("\n");
        assert!(text.contains("QUARANTINED"), "{text}");
        assert!(text.contains("lane_wall_refusal"), "{text}");
    }

    #[test]
    fn overview_lines_show_versions() {
        let app = app_with(vec![]);
        let text = overview_lines(&app).join("\n");
        assert!(text.contains("bloch 1.0.0"), "{text}");
        assert!(text.contains("host lane: general"), "{text}");
    }

    #[test]
    fn workflows_lines_state_provider_honestly_absent() {
        let app = app_with(vec![]);
        let text = workflows_lines(&app).join("\n");
        assert!(text.contains("provider: n/a"), "{text}");
        assert!(text.contains("budget 12000 chars"), "{text}");
    }

    #[test]
    fn fleet_lines_empty_by_design() {
        let mut app = app_with(vec![]);
        app.mode = Mode::Plan;
        let text = fleet_lines(&mut app).join("\n");
        assert!(text.contains("empty by design"), "{text}");
    }
}
