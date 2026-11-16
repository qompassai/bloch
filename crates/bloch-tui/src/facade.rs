//! The narrow surface the UI programs against (DESIGN §D2).
//!
//! [`BlochFacade`] exposes exactly what the screens need: status,
//! the member's own tool list, a budgeted map build, the session
//! handoff, the lesson-candidate inbox with its gated accept /
//! reject, the trigger-rule list, and the inbound audit tail.
//! Two implementations:
//!
//! - [`LibFacade`]: the real one. It prefers bloch's **library
//!   crate** (the D2 rule): maps come from `bloch::map_repo`, the
//!   handoff from bloch's session/memory/test helpers, and — the
//!   centrepiece — candidate acceptance goes through bloch's real
//!   lesson machinery, `bloch::memory_promote`, whose D-1030
//!   birth-metadata gate re-checks every candidate at the write.
//!   Trigger-delivered candidates are first filed through
//!   `bloch::memory_inbox` with their birth frontmatter, then
//!   promoted by the same gate, so no TUI path bypasses it.
//! - [`FakeFacade`]: in-memory, scripted, call-recording; every
//!   test programs against it.
//!
//! Seam (named, per the rung brief): bloch has **no dedicated
//! `lesson_candidate` MCP tool** and no reject verb in its library.
//! The candidate type therefore lives in this crate
//! ([`crate::candidate`]); `submit_candidate` is the intake seam a
//! §D4 trigger engine will call, and rejection is a TUI-local
//! disposition (archive the inbox file + append an audit line)
//! until bloch grows a first-class reject. Neither is faked as a
//! server tool.

use std::path::{Path, PathBuf};

use bloch::{ContentLabel, Lane};

use crate::candidate::{CandidateState, LessonCandidate};
use crate::triggers::{load_triggers, TriggerRule};
use crate::{
    validate_budget, TuiError, AUDIT_TAIL_MAX, CANDIDATES_MAX, CANDIDATE_FILE_BYTES_MAX,
    HANDOFF_CHARS_MAX,
};

/// Overview facts for one target repo.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusInfo {
    /// The member crate version this TUI was built against.
    pub member_version: String,
    /// This TUI crate's own version.
    pub tui_version: String,
    /// Number of tools on the member's MCP surface.
    pub tool_count: usize,
    /// Host repo lane (`general` = qompass/sfw for bloch's store).
    pub host_lane: Lane,
    /// Target repo root, display form.
    pub target: String,
    /// Candidates currently in the inbox.
    pub candidate_count: usize,
    /// Recent lesson lines visible in the store (bounded sample).
    pub lesson_count: usize,
    /// Whether the target has a roadmap (handoff source).
    pub has_roadmap: bool,
}

/// The bounded result of one map build.
#[derive(Debug, Clone, PartialEq)]
pub struct MapSummary {
    /// Target directory, display form.
    pub target: String,
    /// Files mapped.
    pub files: usize,
    /// Definitions extracted.
    pub definitions: usize,
    /// `(language, file count)`, sorted by count descending.
    pub languages: Vec<(String, usize)>,
    /// Characters the full map would render.
    pub total_chars: usize,
    /// The budget the build ran under.
    pub budget_chars: usize,
    /// True when the full map exceeds the budget (the ranked map
    /// is cut to the budget — the product's core behaviour).
    pub truncated: bool,
    /// Whether the SCIP index supplied the map.
    pub used_scip: bool,
}

/// One typed event from a workflow run; the same stream the Log
/// screen renders and a trigger engine would consume (DESIGN §D4).
#[derive(Debug, Clone, PartialEq)]
pub enum RunEvent {
    /// A run started; carries the workflow label.
    Started(String),
    /// Progress note from inside the run.
    Progress(String),
    /// The run finished; carries a one-line summary.
    Done(String),
    /// The run failed; carries the typed reason.
    Failed(String),
}

impl RunEvent {
    /// One-line rendering for the Log screen.
    pub fn line(&self) -> String {
        match self {
            RunEvent::Started(label) => format!("started: {label}"),
            RunEvent::Progress(note) => format!("progress: {note}"),
            RunEvent::Done(summary) => format!("done: {summary}"),
            RunEvent::Failed(reason) => format!("failed: {reason}"),
        }
    }
}

/// The operations the bloch console needs. All fallible operations
/// return typed errors; nothing here panics on external data.
pub trait BlochFacade {
    /// Overview facts for the current target.
    fn status(&mut self) -> Result<StatusInfo, TuiError>;
    /// The member's own MCP tool names, in surface order.
    fn tools(&mut self) -> Vec<String>;
    /// Build a ranked map of `target` under `budget_chars`.
    fn build_map(
        &mut self,
        target: &Path,
        budget_chars: usize,
        use_scip: bool,
    ) -> Result<(MapSummary, Vec<RunEvent>), TuiError>;
    /// The session-handoff view for `target`, bounded.
    fn handoff(&mut self, target: &Path) -> Result<String, TuiError>;
    /// All inbox candidates (file-backed + trigger-held), bounded
    /// by [`CANDIDATES_MAX`].
    fn list_candidates(&mut self) -> Result<Vec<LessonCandidate>, TuiError>;
    /// File one trigger-delivered candidate into the inbox (the
    /// §D4 intake seam). Shape-validated here; the lane law is
    /// enforced at accept time and again by bloch's promote gate.
    fn submit_candidate(&mut self, candidate: LessonCandidate) -> Result<(), TuiError>;
    /// Accept one candidate into the lesson store through bloch's
    /// gated machinery. Quarantined / malformed candidates refuse
    /// here even if a caller skipped the UI check (second belt).
    fn accept_candidate(&mut self, id: &str) -> Result<String, TuiError>;
    /// Reject one candidate with a recorded reason.
    fn reject_candidate(&mut self, id: &str, reason: &str) -> Result<String, TuiError>;
    /// Declared outbound trigger rules (bloch: empty by design).
    fn trigger_rules(&mut self) -> Vec<TriggerRule>;
    /// Inbound audit tail (attempts + TUI dispositions), bounded.
    fn audit_tail(&mut self, max: usize) -> Vec<String>;
}

// ---------------------------------------------------------------------------
// Real facade (library-backed)
// ---------------------------------------------------------------------------

/// The member version this TUI was built against, read bounded
/// from the member manifest two directories up (this crate lives
/// in `crates/bloch-tui/` inside the member repo). Falls back to
/// `unknown` — never to a guessed number.
fn member_version() -> String {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("Cargo.toml");
    let text = match std::fs::read_to_string(&manifest) {
        Ok(t) => t,
        Err(_) => return "unknown".to_string(),
    };
    for line in text.lines().take(40) {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("version") {
            let v = rest
                .trim_start_matches(['=', ' '])
                .trim_matches('"')
                .to_string();
            if !v.is_empty() {
                return v;
            }
        }
    }
    "unknown".to_string()
}

/// The library-backed facade over one target repo.
#[derive(Debug)]
pub struct LibFacade {
    target: PathBuf,
    pending: Vec<LessonCandidate>,
}

impl LibFacade {
    /// A facade rooted at `target` (the repo the console operates
    /// on: maps are built for it, its inbox is the lesson inbox).
    pub fn new(target: PathBuf) -> LibFacade {
        LibFacade {
            target,
            pending: Vec::new(),
        }
    }

    /// The current target root.
    pub fn target(&self) -> &Path {
        &self.target
    }

    /// Retarget the console (the Workflows target picker).
    pub fn set_target(&mut self, target: PathBuf) {
        self.target = target;
    }

    fn host_lane(&self) -> Lane {
        bloch::repo_lane(&self.target)
    }

    fn inbox_dir(&self) -> PathBuf {
        self.target.join(".ai").join("memory").join("inbox")
    }

    fn audit_path(&self) -> PathBuf {
        self.target
            .join(".ai")
            .join("memory")
            .join("bloch-tui-audit.jsonl")
    }

    fn append_audit(&self, action: &str, id: &str, detail: &str) {
        let line = serde_json::json!({
            "action": action,
            "candidate": id,
            "detail": detail,
            "via": "bloch-tui",
        })
        .to_string();
        if let Some(parent) = self.audit_path().parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Best-effort: the audit is a tail for the operator, and a
        // failed append must not block the gated act it records
        // (the act itself is already committed by bloch's write).
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.audit_path())
            .and_then(|mut fh| {
                use std::io::Write as _;
                fh.write_all(line.as_bytes())
                    .and_then(|()| fh.write_all(b"\n"))
            });
    }

    fn find_candidate(&self, id: &str) -> Result<LessonCandidate, TuiError> {
        // list through a fresh scan so accept acts on current files.
        let mut all = self.scan_inbox()?;
        all.extend(self.pending.iter().cloned());
        all.into_iter()
            .find(|c| c.id == id)
            .ok_or_else(|| TuiError::NotFound(format!("candidate '{id}'")))
    }

    fn scan_inbox(&self) -> Result<Vec<LessonCandidate>, TuiError> {
        let dir = self.inbox_dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(TuiError::Io(err)),
        };
        let mut names: Vec<String> = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".md") {
                names.push(name);
            }
        }
        names.sort();
        let mut out: Vec<LessonCandidate> = Vec::new();
        for name in names.iter().take(CANDIDATES_MAX) {
            let path = dir.join(name);
            let meta = std::fs::metadata(&path)?;
            if meta.len() > CANDIDATE_FILE_BYTES_MAX {
                // Oversized file: surface it as malformed rather
                // than reading unbounded bytes into the console.
                out.push(LessonCandidate {
                    id: name.clone(),
                    rule_id: "inbox".to_string(),
                    origin_lane: None,
                    content_label: None,
                    evidence: String::new(),
                    payload: String::new(),
                    source_file: Some(name.clone()),
                });
                continue;
            }
            let text = std::fs::read_to_string(&path)?;
            out.push(LessonCandidate::from_inbox_file(name, &text));
        }
        Ok(out)
    }
}

impl BlochFacade for LibFacade {
    fn status(&mut self) -> Result<StatusInfo, TuiError> {
        let candidates = self.list_candidates().unwrap_or_default();
        Ok(StatusInfo {
            member_version: member_version(),
            tui_version: env!("CARGO_PKG_VERSION").to_string(),
            tool_count: bloch::server::tool_definitions()
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0),
            host_lane: self.host_lane(),
            target: self.target.display().to_string(),
            candidate_count: candidates.len(),
            lesson_count: bloch::recent_lessons(&self.target, AUDIT_TAIL_MAX).len(),
            has_roadmap: bloch::roadmap_status(&self.target).is_ok(),
        })
    }

    fn tools(&mut self) -> Vec<String> {
        bloch::server::tool_definitions()
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t["name"].as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn build_map(
        &mut self,
        target: &Path,
        budget_chars: usize,
        use_scip: bool,
    ) -> Result<(MapSummary, Vec<RunEvent>), TuiError> {
        validate_budget(budget_chars)?;
        if !target.is_dir() {
            return Err(TuiError::Invalid(format!(
                "map target '{}' is not a directory",
                target.display()
            )));
        }
        let label = format!("map {}", target.display());
        let mut events = vec![RunEvent::Started(label.clone())];
        let repo_map =
            bloch::map_repo(target, &bloch::MapConfig { use_scip }).map_err(TuiError::from)?;
        events.push(RunEvent::Progress(format!(
            "{} files, {} definitions",
            repo_map.files.len(),
            repo_map.total_definitions
        )));
        let mut languages: Vec<(String, usize)> = repo_map
            .languages
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        languages.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let total_chars = repo_map.total_chars();
        let summary = MapSummary {
            target: target.display().to_string(),
            files: repo_map.files.len(),
            definitions: repo_map.total_definitions,
            languages,
            total_chars,
            budget_chars,
            truncated: total_chars > budget_chars,
            used_scip: repo_map.used_scip,
        };
        events.push(RunEvent::Done(format!(
            "map: {} files, {} definitions, {} chars (budget {})",
            summary.files, summary.definitions, summary.total_chars, budget_chars
        )));
        Ok((summary, events))
    }

    fn handoff(&mut self, target: &Path) -> Result<String, TuiError> {
        if !target.is_dir() {
            return Err(TuiError::Invalid(format!(
                "handoff target '{}' is not a directory",
                target.display()
            )));
        }
        let mut out = String::new();
        out.push_str(&bloch::get_session_state(target));
        out.push_str(&bloch::get_memory_context(target, 4_000));
        out.push_str(&bloch::get_test_status(target, 2_000));
        match bloch::roadmap_status(target) {
            Ok(st) => {
                out.push_str(&format!(
                    "roadmap: {} — {:.0}% green ({}/{}), next open: {}\n",
                    st.repo_name,
                    st.percent,
                    st.green,
                    st.total,
                    st.next_open
                        .map(|(id, title)| format!("{id} {title}"))
                        .unwrap_or_else(|| "none".to_string())
                ));
            }
            Err(_) => out.push_str("roadmap: none\n"),
        }
        let lessons = bloch::recent_lessons(target, 3);
        if lessons.is_empty() {
            out.push_str("recent lessons: none recorded yet\n");
        } else {
            out.push_str("recent lessons:\n");
            for lesson in lessons {
                out.push_str(&format!("  {lesson}\n"));
            }
        }
        if out.chars().count() > HANDOFF_CHARS_MAX {
            out = out.chars().take(HANDOFF_CHARS_MAX).collect();
            out.push_str("\n(truncated at HANDOFF_CHARS_MAX)\n");
        }
        Ok(out)
    }

    fn list_candidates(&mut self) -> Result<Vec<LessonCandidate>, TuiError> {
        let mut all = self.scan_inbox()?;
        all.extend(self.pending.iter().cloned());
        all.truncate(CANDIDATES_MAX);
        Ok(all)
    }

    fn submit_candidate(&mut self, candidate: LessonCandidate) -> Result<(), TuiError> {
        candidate.validate_shape()?;
        if self.pending.len() >= CANDIDATES_MAX {
            return Err(TuiError::Invalid(format!(
                "candidate inbox is full ({CANDIDATES_MAX})"
            )));
        }
        if self.pending.iter().any(|c| c.id == candidate.id) {
            return Err(TuiError::Invalid(format!(
                "duplicate candidate id '{}'",
                candidate.id
            )));
        }
        self.pending.push(candidate);
        Ok(())
    }

    fn accept_candidate(&mut self, id: &str) -> Result<String, TuiError> {
        let candidate = self.find_candidate(id)?;
        let host = self.host_lane();
        match candidate.disposition(host) {
            (CandidateState::Pending, _) => {}
            (CandidateState::Quarantined, reason) => {
                return Err(TuiError::Quarantined(reason.unwrap_or_default()));
            }
            (CandidateState::Malformed, reason) => {
                return Err(TuiError::Invalid(reason.unwrap_or_default()));
            }
        }
        candidate.validate_shape()?;
        let message = if let Some(file) = &candidate.source_file {
            // File-backed: bloch's own promote gate (D-1030) is the
            // write path; it re-checks the birth metadata itself.
            bloch::memory_promote(&self.target, file, "lessons.md").map_err(TuiError::from)?
        } else {
            // Trigger-held: file it through the inbox with its
            // birth frontmatter, then promote through the same gate.
            let stamp = bloch::LabelStamp {
                origin_lane: candidate.origin_lane.unwrap_or(host),
                content_label: candidate.content_label.unwrap_or(ContentLabel::Sfw),
                evidence: candidate.evidence.clone(),
            };
            let content = format!("{}{}", bloch::label_frontmatter(&stamp), candidate.payload);
            let file = bloch::memory_inbox(&self.target, "bloch-tui", &candidate.rule_id, &content)
                .map_err(TuiError::from)?;
            bloch::memory_promote(&self.target, &file, "lessons.md").map_err(TuiError::from)?
        };
        self.pending.retain(|c| c.id != id);
        self.append_audit("accept", id, &message);
        Ok(message)
    }

    fn reject_candidate(&mut self, id: &str, reason: &str) -> Result<String, TuiError> {
        let candidate = self.find_candidate(id)?;
        // Rejection is always lawful (it moves nothing into the
        // store), including for quarantined candidates.
        if let Some(file) = &candidate.source_file {
            let mem = self.target.join(".ai").join("memory");
            let archive = mem.join("archive");
            std::fs::create_dir_all(&archive)?;
            let src = mem.join("inbox").join(file);
            std::fs::rename(&src, archive.join(file))?;
        }
        self.pending.retain(|c| c.id != id);
        self.append_audit("reject", id, reason);
        Ok(format!("rejected {id}: {reason}"))
    }

    fn trigger_rules(&mut self) -> Vec<TriggerRule> {
        load_triggers(&self.target.join("triggers.toml"))
    }

    fn audit_tail(&mut self, max: usize) -> Vec<String> {
        let cap = max.min(AUDIT_TAIL_MAX);
        let mut lines: Vec<String> = Vec::new();
        for path in [
            self.target
                .join(".ai")
                .join("memory")
                .join("attempts.jsonl"),
            self.audit_path(),
        ] {
            if let Ok(text) = std::fs::read_to_string(&path) {
                for line in text.lines().filter(|l| !l.trim().is_empty()) {
                    lines.push(line.to_string());
                }
            }
        }
        if lines.len() > cap {
            lines = lines.split_off(lines.len() - cap);
        }
        lines
    }
}

// ---------------------------------------------------------------------------
// Fake facade (tests)
// ---------------------------------------------------------------------------

/// An in-memory [`BlochFacade`] for tests: scripted results,
/// recorded calls. Accept enforces the same lane law as the real
/// facade, so UI tests exercise the real disposition logic.
#[derive(Debug, Default)]
pub struct FakeFacade {
    /// Status returned by [`BlochFacade::status`].
    pub status_result: Option<StatusInfo>,
    /// Tool names returned by [`BlochFacade::tools`].
    pub tools_result: Vec<String>,
    /// Candidates served by the fake inbox.
    pub candidates: Vec<LessonCandidate>,
    /// Summary returned by the next map build.
    pub map_summary: Option<MapSummary>,
    /// Handoff text returned by [`BlochFacade::handoff`].
    pub handoff_result: String,
    /// Rules returned by [`BlochFacade::trigger_rules`].
    pub rules_result: Vec<TriggerRule>,
    /// Audit lines returned by [`BlochFacade::audit_tail`].
    pub audit_result: Vec<String>,
    /// When set, every fallible call fails with this message.
    pub fail_with: Option<String>,
    /// Recorded calls, in order (`accept:<id>`, `map:<budget>`, …).
    pub calls: Vec<String>,
}

impl FakeFacade {
    /// A fake with a general-lane status and no candidates.
    pub fn new() -> FakeFacade {
        FakeFacade {
            status_result: Some(StatusInfo {
                member_version: "1.0.0".to_string(),
                tui_version: "0.1.0".to_string(),
                tool_count: 13,
                host_lane: Lane::General,
                target: "/tmp/fake-repo".to_string(),
                candidate_count: 0,
                lesson_count: 0,
                has_roadmap: true,
            }),
            tools_result: vec!["memory_search".to_string(), "lessons_search".to_string()],
            handoff_result: "handoff: fake session state".to_string(),
            ..FakeFacade::default()
        }
    }

    fn maybe_fail(&self) -> Result<(), TuiError> {
        match &self.fail_with {
            Some(msg) => Err(TuiError::Backend(msg.clone())),
            None => Ok(()),
        }
    }

    fn host_lane(&self) -> Lane {
        self.status_result
            .as_ref()
            .map(|s| s.host_lane)
            .unwrap_or(Lane::General)
    }
}

impl BlochFacade for FakeFacade {
    fn status(&mut self) -> Result<StatusInfo, TuiError> {
        self.maybe_fail()?;
        let mut status = self.status_result.clone().unwrap_or_else(|| StatusInfo {
            member_version: "1.0.0".to_string(),
            tui_version: "0.1.0".to_string(),
            tool_count: 0,
            host_lane: Lane::General,
            target: "/tmp/fake-repo".to_string(),
            candidate_count: 0,
            lesson_count: 0,
            has_roadmap: false,
        });
        status.candidate_count = self.candidates.len();
        Ok(status)
    }

    fn tools(&mut self) -> Vec<String> {
        self.tools_result.clone()
    }

    fn build_map(
        &mut self,
        target: &Path,
        budget_chars: usize,
        _use_scip: bool,
    ) -> Result<(MapSummary, Vec<RunEvent>), TuiError> {
        validate_budget(budget_chars)?;
        self.calls.push(format!("map:{budget_chars}"));
        self.maybe_fail()?;
        let summary = self.map_summary.clone().unwrap_or(MapSummary {
            target: target.display().to_string(),
            files: 3,
            definitions: 42,
            languages: vec![("rust".to_string(), 3)],
            total_chars: 900,
            budget_chars,
            truncated: false,
            used_scip: false,
        });
        let events = vec![
            RunEvent::Started(format!("map {}", target.display())),
            RunEvent::Done(format!(
                "map: {} files, {} definitions",
                summary.files, summary.definitions
            )),
        ];
        Ok((summary, events))
    }

    fn handoff(&mut self, _target: &Path) -> Result<String, TuiError> {
        self.calls.push("handoff".to_string());
        self.maybe_fail()?;
        Ok(self.handoff_result.clone())
    }

    fn list_candidates(&mut self) -> Result<Vec<LessonCandidate>, TuiError> {
        self.maybe_fail()?;
        Ok(self.candidates.clone())
    }

    fn submit_candidate(&mut self, candidate: LessonCandidate) -> Result<(), TuiError> {
        candidate.validate_shape()?;
        self.calls.push(format!("submit:{}", candidate.id));
        self.candidates.push(candidate);
        Ok(())
    }

    fn accept_candidate(&mut self, id: &str) -> Result<String, TuiError> {
        self.calls.push(format!("accept:{id}"));
        self.maybe_fail()?;
        let host = self.host_lane();
        let pos = self
            .candidates
            .iter()
            .position(|c| c.id == id)
            .ok_or_else(|| TuiError::NotFound(format!("candidate '{id}'")))?;
        match self.candidates[pos].disposition(host) {
            (CandidateState::Pending, _) => {}
            (CandidateState::Quarantined, reason) => {
                return Err(TuiError::Quarantined(reason.unwrap_or_default()));
            }
            (CandidateState::Malformed, reason) => {
                return Err(TuiError::Invalid(reason.unwrap_or_default()));
            }
        }
        self.candidates.remove(pos);
        Ok(format!("promoted {id} -> lessons.md"))
    }

    fn reject_candidate(&mut self, id: &str, reason: &str) -> Result<String, TuiError> {
        self.calls.push(format!("reject:{id}"));
        self.maybe_fail()?;
        let pos = self
            .candidates
            .iter()
            .position(|c| c.id == id)
            .ok_or_else(|| TuiError::NotFound(format!("candidate '{id}'")))?;
        self.candidates.remove(pos);
        Ok(format!("rejected {id}: {reason}"))
    }

    fn trigger_rules(&mut self) -> Vec<TriggerRule> {
        self.rules_result.clone()
    }

    fn audit_tail(&mut self, max: usize) -> Vec<String> {
        let cap = max.min(AUDIT_TAIL_MAX);
        let mut lines = self.audit_result.clone();
        if lines.len() > cap {
            lines = lines.split_off(lines.len() - cap);
        }
        lines
    }
}
