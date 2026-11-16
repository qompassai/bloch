//! Lesson candidates: the shape other MCPs' triggers produce
//! (DESIGN §D4) and the lane law that governs their acceptance.
//!
//! A candidate is a draft lesson offered to bloch's lesson store. It
//! carries its birth metadata from the start — `origin_lane`,
//! `content_label`, and `evidence` — because bloch's write law
//! (D-1030) refuses unlabelled records at every gate, and this TUI
//! holds the same line: a candidate whose labels are missing or
//! unparseable is *malformed*, and one whose labels would violate
//! the lane law for the host repo is *quarantined*. Neither state
//! can be accepted from the TUI; the accept path refuses before any
//! call into bloch's machinery.
//!
//! Lane law for this host (bloch lane: qompass / sfw, a general-lane
//! store): visibility and admission follow the *content label*,
//! never the origin alone (bloch SPEC §9.1). An nsfw-labelled
//! candidate may never enter a general-lane store — the same wall
//! the MCP server enforces as `lane_wall_refusal`. An sfw-labelled
//! candidate of restricted origin *may* enter, when it carries its
//! evidence: SFW lessons flow into the general lane; nsfw-pure
//! lessons never reach qompass.

use bloch::{ContentLabel, Lane};
use serde_json::Value;

use crate::{
    TuiError, CANDIDATE_ID_CHARS_MAX, EVIDENCE_CHARS_MAX, PAYLOAD_BYTES_MAX, RULE_ID_CHARS_MAX,
};

/// Disposition of one candidate for one host lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateState {
    /// Labelled, lawful for the host, and shape-valid: acceptable.
    Pending,
    /// Labelled, but the lane law walls it off from this host.
    Quarantined,
    /// Birth metadata missing/unparseable, or shape-invalid.
    Malformed,
}

/// One incoming lesson candidate.
///
/// `origin_lane` / `content_label` are `Option` because malformed
/// real-world input exists (legacy inbox files without frontmatter);
/// the type represents it honestly instead of defaulting a label —
/// defaulting at intake is exactly what bloch's write law forbids.
#[derive(Debug, Clone, PartialEq)]
pub struct LessonCandidate {
    /// Stable id (the inbox file name for file-backed candidates).
    pub id: String,
    /// Id of the trigger rule that produced the candidate, or the
    /// proposing agent for inbox files.
    pub rule_id: String,
    /// Provenance lane stamped at birth, when parseable.
    pub origin_lane: Option<Lane>,
    /// Content label stamped at birth, when parseable.
    pub content_label: Option<ContentLabel>,
    /// Evidence summary for the content label (never empty on a
    /// well-formed candidate).
    pub evidence: String,
    /// The lesson payload (the draft lesson text).
    pub payload: String,
    /// Inbox file name when this candidate is backed by a file in
    /// `.ai/memory/inbox/`; `None` for trigger-delivered candidates
    /// held in memory until accepted.
    pub source_file: Option<String>,
}

impl LessonCandidate {
    /// The lane-law refusal for `host`, when the candidate's labels
    /// are present and parseable. `None` means the labels admit the
    /// candidate to the host (shape is checked separately).
    pub fn lane_law_refusal(&self, host: Lane) -> Option<String> {
        let label = self.content_label?;
        if host == Lane::General && label == ContentLabel::Nsfw {
            return Some(
                "lane_wall_refusal: nsfw-labelled content may not enter a \
                 general-lane (qompass/sfw) lesson store"
                    .to_string(),
            );
        }
        None
    }

    /// Why this candidate is not acceptable, in gate order: missing
    /// birth metadata first (malformed), then the lane wall
    /// (quarantined). Returns the state and the reason together so
    /// the UI can show exactly what the gate saw.
    pub fn disposition(&self, host: Lane) -> (CandidateState, Option<String>) {
        let mut missing: Vec<&str> = Vec::new();
        if self.origin_lane.is_none() {
            missing.push("origin_lane");
        }
        if self.content_label.is_none() {
            missing.push("content_label");
        }
        if self.evidence.trim().is_empty() {
            missing.push("evidence");
        }
        if !missing.is_empty() {
            return (
                CandidateState::Malformed,
                Some(format!(
                    "unlabelled_candidate: missing birth metadata: {}",
                    missing.join(", ")
                )),
            );
        }
        if let Some(reason) = self.lane_law_refusal(host) {
            return (CandidateState::Quarantined, Some(reason));
        }
        match self.validate_shape() {
            Ok(()) => (CandidateState::Pending, None),
            Err(err) => (CandidateState::Malformed, Some(err.to_string())),
        }
    }

    /// True when the candidate may be accepted into `host`.
    pub fn acceptable(&self, host: Lane) -> bool {
        self.disposition(host).0 == CandidateState::Pending
    }

    /// Shape bounds, independent of labels: ids and evidence are
    /// bounded, the payload is non-empty and within
    /// [`PAYLOAD_BYTES_MAX`]. Checked before any allocation-heavy
    /// work downstream and again at the facade boundary.
    pub fn validate_shape(&self) -> Result<(), TuiError> {
        if self.id.trim().is_empty() || self.id.chars().count() > CANDIDATE_ID_CHARS_MAX {
            return Err(TuiError::Invalid(format!(
                "candidate id must be 1..={CANDIDATE_ID_CHARS_MAX} chars"
            )));
        }
        if self.rule_id.trim().is_empty() || self.rule_id.chars().count() > RULE_ID_CHARS_MAX {
            return Err(TuiError::Invalid(format!(
                "rule id must be 1..={RULE_ID_CHARS_MAX} chars"
            )));
        }
        if self.evidence.chars().count() > EVIDENCE_CHARS_MAX {
            return Err(TuiError::Invalid(format!(
                "evidence exceeds {EVIDENCE_CHARS_MAX} chars"
            )));
        }
        if self.payload.trim().is_empty() {
            return Err(TuiError::Invalid(
                "candidate payload must not be empty".to_string(),
            ));
        }
        if self.payload.len() > PAYLOAD_BYTES_MAX {
            return Err(TuiError::Invalid(format!(
                "payload {} bytes exceeds PAYLOAD_BYTES_MAX {PAYLOAD_BYTES_MAX}",
                self.payload.len()
            )));
        }
        Ok(())
    }

    /// Build a candidate from one trigger-delivered JSON object
    /// (DESIGN §D4: `rule id, origin_lane, content_label, evidence
    /// summary, payload`). Intake refuses unlabelled objects —
    /// nothing is defaulted here, mirroring bloch's outcome intake.
    pub fn from_json(value: &Value) -> Result<LessonCandidate, TuiError> {
        let obj = value.as_object().ok_or_else(|| {
            TuiError::Invalid("candidate intake must be a JSON object".to_string())
        })?;
        let field = |key: &str| -> Result<String, TuiError> {
            obj.get(key)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .ok_or_else(|| {
                    TuiError::Invalid(format!(
                        "unlabelled_candidate: missing or non-string '{key}'; candidates \
                         carry rule_id + origin_lane + content_label + evidence from birth"
                    ))
                })
        };
        let rule_id = field("rule_id")?;
        let origin_raw = field("origin_lane")?;
        let label_raw = field("content_label")?;
        let evidence = field("evidence")?;
        let payload = field("payload")?;
        let origin_lane = Lane::parse(&origin_raw).ok_or_else(|| {
            TuiError::Invalid(format!(
                "unlabelled_candidate: origin_lane '{origin_raw}' is not general|restricted"
            ))
        })?;
        let content_label = ContentLabel::parse(&label_raw).ok_or_else(|| {
            TuiError::Invalid(format!(
                "unlabelled_candidate: content_label '{label_raw}' is not sfw|nsfw"
            ))
        })?;
        let id = obj
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("{rule_id}-candidate"));
        let candidate = LessonCandidate {
            id,
            rule_id,
            origin_lane: Some(origin_lane),
            content_label: Some(content_label),
            evidence,
            payload,
            source_file: None,
        };
        candidate.validate_shape()?;
        if candidate.evidence.trim().is_empty() {
            return Err(TuiError::Invalid(
                "unlabelled_candidate: evidence is empty; a claimed label needs its evidence"
                    .to_string(),
            ));
        }
        Ok(candidate)
    }

    /// Parse one `.ai/memory/inbox/` file into a candidate. The
    /// frontmatter scan is the same bounded opening window bloch's
    /// own label parser uses (24 lines). Missing or unparseable
    /// labels stay `None`: the candidate renders as malformed and
    /// the accept path refuses it — reads never invent labels.
    pub fn from_inbox_file(file_name: &str, text: &str) -> LessonCandidate {
        let mut origin_lane: Option<Lane> = None;
        let mut content_label: Option<ContentLabel> = None;
        let mut evidence = String::new();
        let mut rule_id = String::new();
        for line in text.lines().take(24) {
            let t = line.trim();
            if let Some(v) = t.strip_prefix("origin_lane:") {
                origin_lane = Lane::parse(v.trim());
            } else if let Some(v) = t.strip_prefix("content_label:") {
                content_label = ContentLabel::parse(v.trim());
            } else if let Some(v) = t.strip_prefix("evidence:") {
                if evidence.is_empty() {
                    evidence = v.trim().to_string();
                }
            } else if let Some(v) = t.strip_prefix("rule_id:") {
                if rule_id.is_empty() {
                    rule_id = v.trim().to_string();
                }
            } else if let Some(rest) = t.strip_prefix("<!-- bloch:proposer ") {
                if rule_id.is_empty() {
                    if let Some(name) = rest.strip_suffix(" -->") {
                        rule_id = name.trim().to_string();
                    }
                }
            }
        }
        if rule_id.is_empty() {
            rule_id = "inbox".to_string();
        }
        LessonCandidate {
            id: file_name.to_string(),
            rule_id,
            origin_lane,
            content_label,
            evidence,
            payload: text.to_string(),
            source_file: Some(file_name.to_string()),
        }
    }

    /// Short lane/label rendering for list views, e.g.
    /// `restricted/nsfw`. Unparseable fields render as `?`.
    pub fn label_summary(&self) -> String {
        let origin = self.origin_lane.map(|l| l.as_str()).unwrap_or("?");
        let label = self.content_label.map(|l| l.as_str()).unwrap_or("?");
        format!("{origin}/{label}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(origin: Lane, label: ContentLabel) -> LessonCandidate {
        LessonCandidate {
            id: "cand-1".to_string(),
            rule_id: "theodolite-diag-to-bloch-lesson".to_string(),
            origin_lane: Some(origin),
            content_label: Some(label),
            evidence: "diagnostics run 2026-11-16, tool-checked".to_string(),
            payload: "lesson: pin the witness file before reading counts".to_string(),
            source_file: None,
        }
    }

    #[test]
    fn sfw_general_candidate_is_pending_for_general_host() {
        let c = candidate(Lane::General, ContentLabel::Sfw);
        assert_eq!(
            c.disposition(Lane::General),
            (CandidateState::Pending, None)
        );
        assert!(c.acceptable(Lane::General));
    }

    #[test]
    fn sfw_restricted_candidate_flows_into_general_host() {
        // SFW lessons may flow into the general lane (bloch lane law:
        // visibility follows the content label, not the origin).
        let c = candidate(Lane::Restricted, ContentLabel::Sfw);
        assert_eq!(c.disposition(Lane::General).0, CandidateState::Pending);
    }

    #[test]
    fn nsfw_candidate_is_quarantined_for_general_host() {
        let c = candidate(Lane::Restricted, ContentLabel::Nsfw);
        let (state, reason) = c.disposition(Lane::General);
        assert_eq!(state, CandidateState::Quarantined);
        assert!(reason.unwrap().contains("lane_wall_refusal"));
        assert!(!c.acceptable(Lane::General));
    }

    #[test]
    fn nsfw_from_general_origin_is_still_quarantined() {
        // The wall follows the content label, never the origin.
        let c = candidate(Lane::General, ContentLabel::Nsfw);
        assert_eq!(c.disposition(Lane::General).0, CandidateState::Quarantined);
    }

    #[test]
    fn nsfw_candidate_is_pending_for_restricted_host() {
        let c = candidate(Lane::Restricted, ContentLabel::Nsfw);
        assert_eq!(c.disposition(Lane::Restricted).0, CandidateState::Pending);
    }

    #[test]
    fn unlabelled_candidate_is_malformed() {
        let mut c = candidate(Lane::General, ContentLabel::Sfw);
        c.origin_lane = None;
        let (state, reason) = c.disposition(Lane::General);
        assert_eq!(state, CandidateState::Malformed);
        assert!(reason.unwrap().contains("origin_lane"));
    }

    #[test]
    fn empty_evidence_is_malformed() {
        let mut c = candidate(Lane::General, ContentLabel::Sfw);
        c.evidence = "   ".to_string();
        assert_eq!(c.disposition(Lane::General).0, CandidateState::Malformed);
    }

    #[test]
    fn oversized_payload_fails_shape() {
        let mut c = candidate(Lane::General, ContentLabel::Sfw);
        c.payload = "x".repeat(PAYLOAD_BYTES_MAX + 1);
        assert!(c.validate_shape().is_err());
        assert_eq!(c.disposition(Lane::General).0, CandidateState::Malformed);
    }

    #[test]
    fn exact_payload_bound_is_accepted() {
        let mut c = candidate(Lane::General, ContentLabel::Sfw);
        c.payload = "x".repeat(PAYLOAD_BYTES_MAX);
        assert!(c.validate_shape().is_ok());
    }

    #[test]
    fn from_json_happy_path() {
        let v = serde_json::json!({
            "id": "trig-7",
            "rule_id": "lumen-gen-to-bloch",
            "origin_lane": "general",
            "content_label": "sfw",
            "evidence": "conversion gate passed",
            "payload": "lesson: pin the scaler for evidence frames"
        });
        let c = LessonCandidate::from_json(&v).unwrap();
        assert_eq!(c.id, "trig-7");
        assert_eq!(c.origin_lane, Some(Lane::General));
        assert!(c.acceptable(Lane::General));
    }

    #[test]
    fn from_json_refuses_missing_labels() {
        let v = serde_json::json!({
            "rule_id": "r1", "content_label": "sfw",
            "evidence": "e", "payload": "p"
        });
        let err = LessonCandidate::from_json(&v).unwrap_err();
        assert!(err.to_string().contains("unlabelled_candidate"));
    }

    #[test]
    fn from_json_refuses_unknown_lane_spelling() {
        let v = serde_json::json!({
            "rule_id": "r1", "origin_lane": "somewhere",
            "content_label": "sfw", "evidence": "e", "payload": "p"
        });
        assert!(LessonCandidate::from_json(&v).is_err());
    }

    #[test]
    fn from_json_refuses_oversized_payload() {
        let v = serde_json::json!({
            "rule_id": "r1", "origin_lane": "general",
            "content_label": "sfw", "evidence": "e",
            "payload": "y".repeat(PAYLOAD_BYTES_MAX + 1)
        });
        assert!(LessonCandidate::from_json(&v).is_err());
    }

    #[test]
    fn inbox_file_with_frontmatter_parses() {
        let text = "---\norigin_lane: restricted\ncontent_label: sfw\n\
                    evidence: lesson text is a rust build fix; reviewed\n---\n\
                    <!-- bloch:proposer azoth -->\nlesson body\n";
        let c = LessonCandidate::from_inbox_file("1-azoth-fix.md", text);
        assert_eq!(c.origin_lane, Some(Lane::Restricted));
        assert_eq!(c.content_label, Some(ContentLabel::Sfw));
        assert_eq!(c.rule_id, "azoth");
        assert_eq!(c.disposition(Lane::General).0, CandidateState::Pending);
    }

    #[test]
    fn inbox_file_without_frontmatter_is_malformed() {
        let c = LessonCandidate::from_inbox_file("x.md", "# mystery lesson\n");
        assert_eq!(c.disposition(Lane::General).0, CandidateState::Malformed);
        assert!(!c.acceptable(Lane::General));
    }
}
