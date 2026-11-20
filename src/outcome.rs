//! Outcome-record intake (BL-1, plan INDEX §2.3): one schema for every
//! server's finished-run record, with lane labels enforced at write.
//!
//! A record states what was asked, what was produced (with hashes),
//! what was verified and how strongly, what it cost, which skills were
//! used, and its lane labels (`origin_lane` + `content_label` +
//! `evidence`, carried from birth per bloch law, SPEC §9.1).
//!
//! Write law, stricter than the read side on purpose: an intake record
//! that does not carry its labels is REFUSED (`unlabelled_outcome`) —
//! the fail-closed defaults in [`crate::roadmap`] govern how legacy
//! unlabelled records are *read*, never what bloch will *accept*.
//! An outcome without a verification-strength label is likewise
//! refused (BL-K3), and one without a result is refused too
//! (`missing_result`, BL-3): verification says how strongly a run's
//! end was checked, never what that end was — a tool-checked failure
//! and a tool-checked success are different records. Visibility
//! follows the content label: an
//! nsfw-labelled record is invisible to a general-only caller; the
//! server enforces that through `Clearance::can_see` over
//! [`OutcomeRecord::content_label`].
//!
//! The store is an append-only JSONL file (files stay canonical, as
//! with the idea register): one record per line, written only after
//! validation, so a rejected intake leaves the store untouched.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::roadmap::{ContentLabel, Lane};
use crate::McpRefusal;

/// Maximum bytes read back from one outcome store file.
pub const OUTCOMES_FILE_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// Maximum outcome records read back from one store file.
pub const OUTCOMES_MAX: usize = 10_000;
/// Maximum characters accepted for the `asked` summary.
pub const ASKED_MAX: usize = 4_000;
/// Maximum artifacts listed on one outcome record.
pub const PRODUCED_MAX: usize = 64;

/// Verification strength (INDEX §2.3 ladder, weakest first):
/// self-reported < tool-checked < independently cross-checked
/// < Owner-judged. Declaration order is the rank order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStrength {
    /// The producing agent's own report, unchecked.
    SelfReported,
    /// A deterministic tool gate checked the result.
    ToolChecked,
    /// A separate checker re-derived or cross-checked the result.
    IndependentlyCrossChecked,
    /// The Owner judged the result personally.
    OwnerJudged,
}

impl VerificationStrength {
    /// Stable lowercase name (the wire / store spelling).
    pub fn as_str(&self) -> &'static str {
        match self {
            VerificationStrength::SelfReported => "self_reported",
            VerificationStrength::ToolChecked => "tool_checked",
            VerificationStrength::IndependentlyCrossChecked => "independently_cross_checked",
            VerificationStrength::OwnerJudged => "owner_judged",
        }
    }

    /// Parse a strength name (case-insensitive); the hyphenated
    /// spellings used in prose (`self-reported`, …) are accepted as
    /// read aliases and never written back out.
    pub fn parse(s: &str) -> Option<VerificationStrength> {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "self_reported" => Some(VerificationStrength::SelfReported),
            "tool_checked" => Some(VerificationStrength::ToolChecked),
            "independently_cross_checked" => Some(VerificationStrength::IndependentlyCrossChecked),
            "owner_judged" => Some(VerificationStrength::OwnerJudged),
            _ => None,
        }
    }

    /// True when the outcome is verified strongly enough to be
    /// eligible as BL-2 lesson-card material: only verified outcomes
    /// become cards, and a self-report is not verification.
    pub fn is_verified(&self) -> bool {
        *self >= VerificationStrength::ToolChecked
    }
}

/// What the finished run came to (BL-3): the run completed its ask,
/// it failed, or it was abandoned (killed, timed out, superseded).
/// Distinct from verification strength on purpose: strength says how
/// strongly the end was checked, result says what the end was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeResult {
    /// The run completed what was asked.
    Ok,
    /// The run ended in failure (its checks or its task failed).
    Failed,
    /// The run was stopped before an end state (killed, timed out,
    /// superseded); its record exists so the attempt is not invisible.
    Abandoned,
}

impl OutcomeResult {
    /// Stable lowercase name (the wire / store spelling).
    pub fn as_str(&self) -> &'static str {
        match self {
            OutcomeResult::Ok => "ok",
            OutcomeResult::Failed => "failed",
            OutcomeResult::Abandoned => "abandoned",
        }
    }

    /// Parse a result name (case-insensitive); anything else is not a
    /// result and refuses at intake rather than defaulting.
    pub fn parse(s: &str) -> Option<OutcomeResult> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ok" => Some(OutcomeResult::Ok),
            "failed" => Some(OutcomeResult::Failed),
            "abandoned" => Some(OutcomeResult::Abandoned),
            _ => None,
        }
    }
}

/// One produced artifact: a path plus the SHA-256 of its content, so a
/// consumer can check the claim without trusting the producer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProducedArtifact {
    /// Artifact path (as the producing server names it).
    pub path: String,
    /// Lowercase hex SHA-256 of the artifact's content (64 chars).
    pub sha256: String,
}

/// Cost of the finished run (INDEX §2.3). Zero means "not metered",
/// never "free".
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct OutcomeCost {
    /// Wall-clock seconds the run took.
    #[serde(default)]
    pub wall_secs: u64,
    /// Tokens the run consumed.
    #[serde(default)]
    pub tokens: u64,
    /// Disk bytes the run produced or consumed.
    #[serde(default)]
    pub disk_bytes: u64,
}

fn ser_lane<S: serde::Serializer>(lane: &Lane, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(lane.as_str())
}

fn de_lane<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Lane, D::Error> {
    let raw = String::deserialize(d)?;
    Lane::parse(&raw).ok_or_else(|| serde::de::Error::custom(format!("unknown origin_lane: {raw}")))
}

fn ser_label<S: serde::Serializer>(label: &ContentLabel, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(label.as_str())
}

fn de_label<'de, D: serde::Deserializer<'de>>(d: D) -> Result<ContentLabel, D::Error> {
    let raw = String::deserialize(d)?;
    ContentLabel::parse(&raw)
        .ok_or_else(|| serde::de::Error::custom(format!("unknown content_label: {raw}")))
}

/// One finished run's outcome record (INDEX §2.3). Every field is
/// required at intake; the lane fields are enforced by
/// [`OutcomeRecord::from_intake_json`], the single write gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeRecord {
    /// Unique id of this outcome (producer-assigned, never reused).
    pub outcome_id: String,
    /// Producing server (e.g. `phlow`, `fathom`).
    pub server: String,
    /// What was asked (bounded summary, see [`ASKED_MAX`]).
    pub asked: String,
    /// What was produced, with content hashes. May be empty when the
    /// run produced only a verdict.
    #[serde(default)]
    pub produced: Vec<ProducedArtifact>,
    /// What the run came to (BL-3): ok, failed, or abandoned.
    pub result: OutcomeResult,
    /// How strongly the result was verified (the ladder).
    #[serde(rename = "verification")]
    pub verification: VerificationStrength,
    /// What the run cost.
    #[serde(default)]
    pub cost: OutcomeCost,
    /// Skill ids used by the run (skill trust is computed from these).
    #[serde(default)]
    pub skills: Vec<String>,
    /// Provenance lane, from birth.
    #[serde(serialize_with = "ser_lane", deserialize_with = "de_lane")]
    pub origin_lane: Lane,
    /// Content label governing visibility, from birth.
    #[serde(serialize_with = "ser_label", deserialize_with = "de_label")]
    pub content_label: ContentLabel,
    /// Evidence for the content label (never empty at intake).
    pub evidence: String,
    /// When the outcome was recorded, Unix epoch seconds. The server
    /// stamps this when the producer omits it.
    #[serde(default)]
    pub recorded_at_epoch: i64,
}

impl OutcomeRecord {
    /// True when a general-only (sfw-scoped) caller may see this
    /// record. The server still routes the decision through
    /// `Clearance::can_see`; this is the record-side statement of the
    /// same law for non-server consumers.
    pub fn visible_to_general(&self) -> bool {
        self.content_label == ContentLabel::Sfw
    }

    /// True when this outcome may become BL-2 lesson-card material:
    /// verified at tool-checked strength or stronger.
    pub fn lesson_eligible(&self) -> bool {
        self.verification.is_verified()
    }

    /// Validate a fully-built record (the checks intake and any other
    /// constructor share). Errors are typed strings (`kind: detail`).
    pub fn validate(&self) -> Result<(), String> {
        if self.outcome_id.trim().is_empty() {
            return Err(String::from(McpRefusal::InvalidOutcome(
                "outcome_id is empty".to_string(),
            )));
        }
        if self.server.trim().is_empty() {
            return Err(String::from(McpRefusal::InvalidOutcome(
                "server is empty".to_string(),
            )));
        }
        if self.asked.trim().is_empty() {
            return Err(String::from(McpRefusal::InvalidOutcome(
                "asked is empty".to_string(),
            )));
        }
        if self.asked.chars().count() > ASKED_MAX {
            return Err(String::from(McpRefusal::InvalidOutcome(format!(
                "asked exceeds {ASKED_MAX} chars"
            ))));
        }
        if self.evidence.trim().is_empty() {
            return Err(String::from(McpRefusal::UnlabelledOutcome(
                "evidence is empty; a claimed label needs its evidence".to_string(),
            )));
        }
        if self.produced.len() > PRODUCED_MAX {
            return Err(String::from(McpRefusal::InvalidOutcome(format!(
                "produced exceeds {PRODUCED_MAX} artifacts"
            ))));
        }
        for artifact in &self.produced {
            if artifact.path.trim().is_empty() {
                return Err(String::from(McpRefusal::InvalidOutcome(
                    "produced artifact with empty path".to_string(),
                )));
            }
            if !is_sha256_hex(&artifact.sha256) {
                return Err(String::from(McpRefusal::InvalidOutcome(format!(
                    "artifact '{}' sha256 is not 64 lowercase hex chars",
                    artifact.path
                ))));
            }
        }
        Ok(())
    }

    /// The BL-1 write gate: build a record from one intake JSON
    /// object, refusing anything unlabelled. Unlike the read-side
    /// label parsers, NOTHING is defaulted here — a missing or
    /// unparseable `origin_lane`, `content_label`, or `evidence`
    /// refuses the write (`unlabelled_outcome`), a missing or
    /// unknown `verification` refuses it too (BL-K3: an outcome
    /// without a verification-strength label is refused), and a
    /// missing or unknown `result` refuses it as well (BL-3).
    pub fn from_intake_json(value: &serde_json::Value) -> Result<OutcomeRecord, String> {
        let obj = value.as_object().ok_or_else(|| {
            String::from(McpRefusal::InvalidOutcome(
                "intake must be a JSON object".to_string(),
            ))
        })?;
        for field in ["origin_lane", "content_label", "evidence"] {
            match obj.get(field).and_then(|v| v.as_str()) {
                Some(s) if !s.trim().is_empty() => {}
                _ => {
                    return Err(String::from(McpRefusal::UnlabelledOutcome(format!(
                        "missing or empty '{field}'; \
                         records carry origin_lane + content_label + evidence from birth"
                    ))));
                }
            }
        }
        if Lane::parse(obj["origin_lane"].as_str().unwrap_or_default()).is_none() {
            return Err(String::from(McpRefusal::UnlabelledOutcome(
                "origin_lane is not general|restricted".to_string(),
            )));
        }
        if ContentLabel::parse(obj["content_label"].as_str().unwrap_or_default()).is_none() {
            return Err(String::from(McpRefusal::UnlabelledOutcome(
                "content_label is not sfw|nsfw".to_string(),
            )));
        }
        match obj.get("verification").and_then(|v| v.as_str()) {
            Some(s) if VerificationStrength::parse(s).is_some() => {}
            _ => {
                return Err(String::from(McpRefusal::MissingVerification(
                    "verification must be one of \
                     self_reported|tool_checked|independently_cross_checked|owner_judged"
                        .to_string(),
                )));
            }
        }
        match obj.get("result").and_then(|v| v.as_str()) {
            Some(s) if OutcomeResult::parse(s).is_some() => {}
            _ => {
                return Err(String::from(McpRefusal::MissingResult(
                    "result must be one of ok|failed|abandoned".to_string(),
                )));
            }
        }
        let record: OutcomeRecord = serde_json::from_value(value.clone())
            .map_err(|e| String::from(McpRefusal::InvalidOutcome(e.to_string())))?;
        record.validate()?;
        Ok(record)
    }
}

/// True for a 64-char lowercase hex SHA-256 spelling.
fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Append one validated record to the JSONL store (creating the file
/// and its parent directories). Callers validate first; the append
/// re-validates so no path can write an unlabelled record.
pub fn append_outcome(path: &Path, record: &OutcomeRecord) -> Result<(), String> {
    record.validate()?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| String::from(McpRefusal::WriteFailed(e.to_string())))?;
        }
    }
    let line = serde_json::to_string(record)
        .map_err(|e| String::from(McpRefusal::InvalidOutcome(e.to_string())))?;
    let mut fh = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| String::from(McpRefusal::WriteFailed(e.to_string())))?;
    fh.write_all(line.as_bytes())
        .and_then(|()| fh.write_all(b"\n"))
        .map_err(|e| String::from(McpRefusal::WriteFailed(e.to_string())))?;
    Ok(())
}

/// Read the store back. A missing store reads as empty (no run has
/// finished yet); a corrupt line is a typed error, never a silent
/// skip — the store is evidence. Bounded by
/// [`OUTCOMES_FILE_MAX_BYTES`] and [`OUTCOMES_MAX`].
pub fn read_outcomes(path: &Path) -> Result<Vec<OutcomeRecord>, String> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(String::from(McpRefusal::ReadFailed(e.to_string()))),
    };
    if meta.len() > OUTCOMES_FILE_MAX_BYTES {
        return Err(String::from(McpRefusal::ReadFailed(format!(
            "outcome store exceeds {OUTCOMES_FILE_MAX_BYTES} bytes"
        ))));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| String::from(McpRefusal::ReadFailed(e.to_string())))?;
    let mut out = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if out.len() >= OUTCOMES_MAX {
            return Err(String::from(McpRefusal::ReadFailed(format!(
                "outcome store exceeds {OUTCOMES_MAX} records"
            ))));
        }
        let record: OutcomeRecord = serde_json::from_str(t).map_err(|e| {
            String::from(McpRefusal::CorruptOutcomeStore(format!(
                "line {}: {e}",
                idx + 1
            )))
        })?;
        out.push(record);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash() -> String {
        "a".repeat(64)
    }

    fn intake() -> serde_json::Value {
        serde_json::json!({
            "outcome_id": "out-1",
            "server": "phlow",
            "asked": "run the S1 gate",
            "produced": [{"path": "gate/report.json", "sha256": hash()}],
            "result": "ok",
            "verification": "tool_checked",
            "cost": {"wall_secs": 42, "tokens": 1200, "disk_bytes": 2048},
            "skills": ["tiger-style-rust"],
            "origin_lane": "general",
            "content_label": "sfw",
            "evidence": "gate log gate/report.json signed by the test runner",
            "recorded_at_epoch": 1791300092
        })
    }

    #[test]
    fn intake_accepts_a_labelled_record() {
        let rec = OutcomeRecord::from_intake_json(&intake()).unwrap();
        assert_eq!(rec.origin_lane, Lane::General);
        assert_eq!(rec.content_label, ContentLabel::Sfw);
        assert_eq!(rec.result, OutcomeResult::Ok);
        assert_eq!(rec.verification, VerificationStrength::ToolChecked);
        assert!(rec.lesson_eligible());
        assert!(rec.visible_to_general());
    }

    #[test]
    fn intake_refuses_unlabelled_records() {
        // The BL-1 gate: each missing label field refuses the write.
        for field in ["origin_lane", "content_label", "evidence"] {
            let mut v = intake();
            v.as_object_mut().unwrap().remove(field);
            let err = OutcomeRecord::from_intake_json(&v).unwrap_err();
            assert!(err.starts_with("unlabelled_outcome"), "{field}: {err}");
        }
        // An unparseable label is unlabelled, not defaulted.
        let mut v = intake();
        v["content_label"] = serde_json::json!("maybe");
        let err = OutcomeRecord::from_intake_json(&v).unwrap_err();
        assert!(err.starts_with("unlabelled_outcome"), "{err}");
        // Empty evidence cannot carry a claimed label.
        let mut v = intake();
        v["evidence"] = serde_json::json!("  ");
        let err = OutcomeRecord::from_intake_json(&v).unwrap_err();
        assert!(err.starts_with("unlabelled_outcome"), "{err}");
    }

    #[test]
    fn intake_refuses_missing_or_unknown_verification() {
        // BL-K3: an outcome without a verification-strength label is refused.
        let mut v = intake();
        v.as_object_mut().unwrap().remove("verification");
        let err = OutcomeRecord::from_intake_json(&v).unwrap_err();
        assert!(err.starts_with("missing_verification"), "{err}");
        let mut v = intake();
        v["verification"] = serde_json::json!("vibes");
        let err = OutcomeRecord::from_intake_json(&v).unwrap_err();
        assert!(err.starts_with("missing_verification"), "{err}");
    }

    #[test]
    fn intake_records_each_result_distinctly() {
        // BL-3 validation: all three results land, and a tool-checked
        // failure is a different record from a tool-checked success —
        // same strength, different result, both stored as stated.
        for (spelling, want) in [
            ("ok", OutcomeResult::Ok),
            ("failed", OutcomeResult::Failed),
            ("abandoned", OutcomeResult::Abandoned),
        ] {
            let mut v = intake();
            v["result"] = serde_json::json!(spelling);
            let rec = OutcomeRecord::from_intake_json(&v).unwrap();
            assert_eq!(rec.result, want);
            assert_eq!(rec.verification, VerificationStrength::ToolChecked);
        }
        let mut v = intake();
        v["result"] = serde_json::json!("failed");
        let failed = OutcomeRecord::from_intake_json(&v).unwrap();
        let ok = OutcomeRecord::from_intake_json(&intake()).unwrap();
        assert_ne!(failed, ok);
        assert_eq!(failed.result.as_str(), "failed");
    }

    #[test]
    fn intake_refuses_missing_or_unknown_result() {
        // BL-3 adversarial: no result, or a word that is not a result,
        // refuses the write — a result is never defaulted to ok.
        let mut v = intake();
        v.as_object_mut().unwrap().remove("result");
        let err = OutcomeRecord::from_intake_json(&v).unwrap_err();
        assert!(err.starts_with("missing_result"), "{err}");
        let mut v = intake();
        v["result"] = serde_json::json!("passed");
        let err = OutcomeRecord::from_intake_json(&v).unwrap_err();
        assert!(err.starts_with("missing_result"), "{err}");
    }

    #[test]
    fn intake_refuses_a_bad_artifact_hash() {
        let mut v = intake();
        v["produced"][0]["sha256"] = serde_json::json!("not-a-hash");
        let err = OutcomeRecord::from_intake_json(&v).unwrap_err();
        assert!(err.starts_with("invalid_outcome"), "{err}");
    }

    #[test]
    fn verification_ladder_orders_and_gates_lessons() {
        assert!(VerificationStrength::SelfReported < VerificationStrength::ToolChecked);
        assert!(
            VerificationStrength::ToolChecked < VerificationStrength::IndependentlyCrossChecked
        );
        assert!(
            VerificationStrength::IndependentlyCrossChecked < VerificationStrength::OwnerJudged
        );
        assert!(!VerificationStrength::SelfReported.is_verified());
        assert!(VerificationStrength::ToolChecked.is_verified());
        assert!(VerificationStrength::OwnerJudged.is_verified());
        // Hyphenated prose spellings parse as read aliases.
        assert_eq!(
            VerificationStrength::parse("owner-judged"),
            Some(VerificationStrength::OwnerJudged)
        );
    }

    #[test]
    fn nsfw_records_are_invisible_to_general_callers() {
        // The BL-1 visibility gate, record side: nsfw never reads as
        // general-visible; legacy origin aliases still parse.
        let mut v = intake();
        v["origin_lane"] = serde_json::json!("ntranch");
        v["content_label"] = serde_json::json!("nsfw");
        let rec = OutcomeRecord::from_intake_json(&v).unwrap();
        assert_eq!(rec.origin_lane, Lane::Restricted);
        assert!(!rec.visible_to_general());
    }

    #[test]
    fn store_round_trips_and_missing_store_is_empty() {
        let dir = std::env::temp_dir().join(format!("bloch-outcome-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("outcomes.jsonl");
        assert!(read_outcomes(&path).unwrap().is_empty());
        let rec = OutcomeRecord::from_intake_json(&intake()).unwrap();
        append_outcome(&path, &rec).unwrap();
        append_outcome(&path, &rec).unwrap();
        let back = read_outcomes(&path).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0], rec);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
