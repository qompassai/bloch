//! Integration tests for the library-backed facade: real bloch
//! machinery (inbox scan, the D-1030 promote gate, map building)
//! against throwaway repos on disk. Half validation, half
//! adversarial, per the member repo's testing convention.

use std::fs;
use std::path::PathBuf;

use bloch_tui::candidate::LessonCandidate;
use bloch_tui::{BlochFacade, LibFacade, TuiError};

fn temp_repo(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bloch-tui-it-{}-{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join(".ai").join("memory").join("inbox")).unwrap();
    fs::write(
        dir.join(".ai").join("memory").join("lessons.md"),
        "# Lessons\n",
    )
    .unwrap();
    dir
}

fn inbox_file(dir: &std::path::Path, name: &str, text: &str) {
    fs::write(
        dir.join(".ai").join("memory").join("inbox").join(name),
        text,
    )
    .unwrap();
}

const SFW_FILE: &str = "---\norigin_lane: general\ncontent_label: sfw\n\
    evidence: gate output reviewed\n---\n\
    <!-- bloch:proposer tester -->\nlesson: always pin the witness file\n";

const NSFW_FILE: &str = "---\norigin_lane: restricted\ncontent_label: nsfw\n\
    evidence: adult scene pacing notes\n---\n\
    <!-- bloch:proposer tester -->\nlesson: hold the beat two frames longer\n";

#[test]
fn list_candidates_reads_inbox_with_labels() {
    let dir = temp_repo("list");
    inbox_file(&dir, "1-tester-sfw.md", SFW_FILE);
    let mut facade = LibFacade::new(dir.clone());
    let candidates = facade.list_candidates().unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].id, "1-tester-sfw.md");
    assert!(candidates[0].acceptable(bloch::Lane::General));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn accept_sfw_candidate_promotes_through_real_gate() {
    let dir = temp_repo("accept");
    inbox_file(&dir, "1-tester-sfw.md", SFW_FILE);
    let mut facade = LibFacade::new(dir.clone());
    let message = facade.accept_candidate("1-tester-sfw.md").unwrap();
    assert!(message.contains("lessons.md"), "{message}");
    let lessons = fs::read_to_string(dir.join(".ai").join("memory").join("lessons.md")).unwrap();
    assert!(lessons.contains("pin the witness file"), "{lessons}");
    // The inbox file moved to the archive, per bloch's promote.
    assert!(!dir
        .join(".ai")
        .join("memory")
        .join("inbox")
        .join("1-tester-sfw.md")
        .exists());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn accept_nsfw_candidate_refuses_and_leaves_store_untouched() {
    let dir = temp_repo("wall");
    inbox_file(&dir, "2-tester-nsfw.md", NSFW_FILE);
    let mut facade = LibFacade::new(dir.clone());
    let err = facade.accept_candidate("2-tester-nsfw.md").unwrap_err();
    assert!(
        matches!(err, TuiError::Quarantined(_)),
        "expected quarantine refusal, got {err}"
    );
    // Nothing moved: the candidate stays in the inbox and the
    // lesson store is byte-identical.
    assert!(dir
        .join(".ai")
        .join("memory")
        .join("inbox")
        .join("2-tester-nsfw.md")
        .exists());
    let lessons = fs::read_to_string(dir.join(".ai").join("memory").join("lessons.md")).unwrap();
    assert_eq!(lessons, "# Lessons\n");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn accept_unlabelled_candidate_refuses_at_facade() {
    let dir = temp_repo("unlabelled");
    inbox_file(&dir, "3-mystery.md", "# unlabelled lesson\n");
    let mut facade = LibFacade::new(dir.clone());
    let err = facade.accept_candidate("3-mystery.md").unwrap_err();
    assert!(err.to_string().contains("unlabelled_candidate"), "{err}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn submit_then_accept_trigger_candidate_lands_in_lessons() {
    let dir = temp_repo("trigger");
    let candidate = LessonCandidate::from_json(&serde_json::json!({
        "id": "trig-1",
        "rule_id": "theodolite-diag-to-bloch-lesson",
        "origin_lane": "general",
        "content_label": "sfw",
        "evidence": "diagnostics run, tool-checked",
        "payload": "lesson: read counts only after the witness settles"
    }))
    .unwrap();
    let mut facade = LibFacade::new(dir.clone());
    facade.submit_candidate(candidate).unwrap();
    let message = facade.accept_candidate("trig-1").unwrap();
    assert!(message.contains("lessons.md"), "{message}");
    let lessons = fs::read_to_string(dir.join(".ai").join("memory").join("lessons.md")).unwrap();
    assert!(lessons.contains("witness settles"), "{lessons}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn reject_archives_candidate_and_audits() {
    let dir = temp_repo("reject");
    inbox_file(&dir, "1-tester-sfw.md", SFW_FILE);
    let mut facade = LibFacade::new(dir.clone());
    let message = facade
        .reject_candidate("1-tester-sfw.md", "duplicate of an existing lesson")
        .unwrap();
    assert!(message.contains("rejected"), "{message}");
    assert!(dir
        .join(".ai")
        .join("memory")
        .join("archive")
        .join("1-tester-sfw.md")
        .exists());
    let audit = facade.audit_tail(10);
    assert!(
        audit.iter().any(|l| l.contains("reject")),
        "audit tail must record the rejection: {audit:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn build_map_over_temp_repo_with_budget() {
    let dir = temp_repo("map");
    fs::write(dir.join("lib.rs"), "pub fn alpha() {}\npub fn beta() {}\n").unwrap();
    let mut facade = LibFacade::new(dir.clone());
    let (summary, events) = facade.build_map(&dir, 12_000, false).unwrap();
    assert!(summary.files >= 1);
    assert!(summary.definitions >= 2, "{summary:?}");
    assert_eq!(summary.budget_chars, 12_000);
    assert!(events.len() >= 2);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn build_map_rejects_bad_budget_and_missing_dir() {
    let dir = temp_repo("mapbad");
    let mut facade = LibFacade::new(dir.clone());
    assert!(facade.build_map(&dir, 10, false).is_err());
    assert!(facade.build_map(&dir, 10_000_000, false).is_err());
    let missing = dir.join("does-not-exist");
    assert!(facade.build_map(&missing, 12_000, false).is_err());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn status_reports_member_tools_and_lane() {
    let dir = temp_repo("status");
    let mut facade = LibFacade::new(dir.clone());
    let status = facade.status().unwrap();
    assert_eq!(status.tool_count, 13, "bloch 1.0.0 serves 13 tools");
    assert_eq!(status.host_lane, bloch::Lane::General);
    let tools = facade.tools();
    assert_eq!(tools.len(), 13);
    assert!(tools.iter().any(|t| t == "lessons_search"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn trigger_rules_empty_by_design_for_bloch() {
    let dir = temp_repo("rules");
    let mut facade = LibFacade::new(dir.clone());
    assert!(facade.trigger_rules().is_empty());
    let _ = fs::remove_dir_all(&dir);
}
