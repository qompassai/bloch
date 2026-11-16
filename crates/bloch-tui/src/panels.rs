//! Pure ratatui widget constructors (DESIGN §D2).
//!
//! Each panel is a pure function of its inputs: no terminal
//! access, no I/O. Tests render them into a `TestBackend` buffer
//! and assert on the text. Model- or file-controlled text is
//! truncated with the named bounds below.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table};

use bloch::Lane;

use crate::candidate::{CandidateState, LessonCandidate};
use crate::facade::StatusInfo;
use crate::triggers::TriggerRule;
use crate::StagedAction;

/// Candidate rule ids are truncated to this many characters.
pub const RULE_ID_PANEL_CHARS_MAX: usize = 28;
/// Candidate evidence is truncated to this many characters.
pub const EVIDENCE_PANEL_CHARS_MAX: usize = 44;
/// Candidate ids are truncated to this many characters.
pub const CANDIDATE_ID_PANEL_CHARS_MAX: usize = 30;
/// Audit lines are truncated to this many characters.
pub const AUDIT_PANEL_CHARS_MAX: usize = 110;
/// Free text lines are truncated to this many characters.
pub const TEXT_PANEL_CHARS_MAX: usize = 110;

/// Truncate `text` to `max` characters, marking truncation with
/// an ellipsis. Character-based, never byte-sliced.
pub fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

fn titled_panel(text: &str, title: &str, color: Color) -> Paragraph<'static> {
    let style = Style::default().fg(color).add_modifier(Modifier::BOLD);
    Paragraph::new(text.to_string()).block(
        Block::bordered()
            .title(Line::from(title.to_string()).style(style))
            .border_style(style),
    )
}

/// The Overview panel: member + TUI versions, tool count, host
/// lane, and the map / lesson store status.
pub fn overview_panel(status: &StatusInfo) -> Paragraph<'static> {
    let text = format!(
        "bloch {member} · bloch-tui {tui}\n\
         target: {target}\n\
         host lane: {lane} (lesson store)\n\
         MCP tools: {tools}\n\
         lesson candidates in inbox: {candidates}\n\
         recent lesson lines: {lessons}\n\
         roadmap (handoff source): {roadmap}",
        member = status.member_version,
        tui = status.tui_version,
        target = status.target,
        lane = status.host_lane.as_str(),
        tools = status.tool_count,
        candidates = status.candidate_count,
        lessons = status.lesson_count,
        roadmap = if status.has_roadmap {
            "present"
        } else {
            "absent"
        },
    );
    titled_panel(&text, "Overview", Color::Green)
}

/// The Lesson Candidates table. Quarantined rows are marked
/// `QUARANTINED` and malformed rows `MALFORMED`, in the state
/// column the operator reads before any action.
pub fn candidates_panel(
    candidates: &[LessonCandidate],
    host: Lane,
    selected: usize,
) -> Table<'static> {
    let rows: Vec<Row<'static>> = candidates
        .iter()
        .enumerate()
        .map(|(idx, c)| {
            let (state, _reason) = c.disposition(host);
            let state_text = match state {
                CandidateState::Pending => "pending",
                CandidateState::Quarantined => "QUARANTINED",
                CandidateState::Malformed => "MALFORMED",
            };
            let marker = if idx == selected { ">" } else { " " };
            Row::new(vec![
                Cell::from(marker.to_string()),
                Cell::from(truncate_chars(&c.id, CANDIDATE_ID_PANEL_CHARS_MAX)),
                Cell::from(truncate_chars(&c.rule_id, RULE_ID_PANEL_CHARS_MAX)),
                Cell::from(c.label_summary()),
                Cell::from(state_text.to_string()),
                Cell::from(truncate_chars(&c.evidence, EVIDENCE_PANEL_CHARS_MAX)),
            ])
        })
        .collect();
    Table::new(
        rows,
        [
            ratatui::layout::Constraint::Length(2),
            ratatui::layout::Constraint::Length(30),
            ratatui::layout::Constraint::Length(28),
            ratatui::layout::Constraint::Length(18),
            ratatui::layout::Constraint::Length(12),
            ratatui::layout::Constraint::Min(0),
        ],
    )
    .header(Row::new(vec![
        Cell::from(""),
        Cell::from("Candidate"),
        Cell::from("Rule"),
        Cell::from("Origin/Label"),
        Cell::from("State"),
        Cell::from("Evidence"),
    ]))
    .block(Block::bordered().title("Lesson Candidates"))
    .column_spacing(1)
}

/// The Log / Live panel: the bounded event buffer, oldest first.
pub fn log_panel(lines: &[String]) -> Paragraph<'static> {
    let text = lines
        .iter()
        .map(|l| truncate_chars(l, TEXT_PANEL_CHARS_MAX))
        .collect::<Vec<_>>()
        .join("\n");
    titled_panel(&text, "Log / Live", Color::Cyan)
}

/// The Fleet panel: declared outbound trigger rules and the
/// inbound audit tail. bloch ships no outbound rules; the panel
/// says so instead of inventing any.
pub fn fleet_panel(rules: &[TriggerRule], audit: &[String]) -> Paragraph<'static> {
    let mut text = String::new();
    if rules.is_empty() {
        text.push_str("outbound trigger rules: none (empty by design — bloch is the sink)\n");
    } else {
        text.push_str("outbound trigger rules:\n");
        for rule in rules {
            text.push_str(&format!(
                "  {} [{}] on {} -> {} ({})\n",
                rule.id,
                if rule.enabled { "on" } else { "off" },
                rule.event,
                rule.target,
                rule.mode
            ));
        }
    }
    text.push_str("inbound audit (tail):\n");
    if audit.is_empty() {
        text.push_str("  (no audit lines yet)\n");
    } else {
        for line in audit {
            text.push_str(&format!(
                "  {}\n",
                truncate_chars(line, AUDIT_PANEL_CHARS_MAX)
            ));
        }
    }
    titled_panel(&text, "Fleet", Color::Magenta)
}

/// The staging panel: the exact action approval would execute.
pub fn staged_panel(action: &StagedAction) -> Paragraph<'static> {
    titled_panel(
        &format!("STAGED: {}\napprove (a) / abort (x)", action.label),
        "Plan",
        Color::Yellow,
    )
}

/// A plain titled text panel (handoff, workflow results).
pub fn text_panel(text: &str, title: &str) -> Paragraph<'static> {
    titled_panel(text, title, Color::Cyan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bloch::ContentLabel;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render<W: ratatui::widgets::Widget>(widget: W, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| frame.render_widget(widget, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn status() -> StatusInfo {
        StatusInfo {
            member_version: "1.0.0".to_string(),
            tui_version: "0.1.0".to_string(),
            tool_count: 13,
            host_lane: Lane::General,
            target: "/tmp/fake-repo".to_string(),
            candidate_count: 2,
            lesson_count: 5,
            has_roadmap: true,
        }
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
    fn overview_panel_shows_versions_and_store_status() {
        let text = render(overview_panel(&status()), 80, 12);
        assert!(text.contains("bloch 1.0.0"), "{text}");
        assert!(text.contains("MCP tools: 13"), "{text}");
        assert!(text.contains("host lane: general"), "{text}");
        assert!(text.contains("candidates in inbox: 2"), "{text}");
    }

    #[test]
    fn candidates_panel_marks_quarantine_visibly() {
        let rows = vec![
            candidate("ok-1", Lane::General, ContentLabel::Sfw),
            candidate("bad-1", Lane::Restricted, ContentLabel::Nsfw),
        ];
        let text = render(candidates_panel(&rows, Lane::General, 0), 120, 10);
        assert!(text.contains("QUARANTINED"), "{text}");
        assert!(text.contains("pending"), "{text}");
        assert!(text.contains("restricted/nsfw"), "{text}");
    }

    #[test]
    fn fleet_panel_states_empty_rules_by_design() {
        let text = render(fleet_panel(&[], &["audit line one".to_string()]), 100, 10);
        assert!(text.contains("empty by design"), "{text}");
        assert!(text.contains("audit line one"), "{text}");
    }

    #[test]
    fn truncate_chars_is_char_safe() {
        assert_eq!(truncate_chars("abc", 5), "abc");
        assert_eq!(truncate_chars("abcdef", 4), "abc…");
        assert_eq!(truncate_chars("héllo", 3), "hé…");
    }
}
