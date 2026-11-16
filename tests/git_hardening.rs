//! Regression contracts for the repo-config RCE hardening (CVE-2026-129128
//! shape): a hostile repo's `diff.external` / `core.fsmonitor` must never
//! execute when bloch inspects the repo through `run_git`.
//!
//! First test: validation — the hardened wrapper still reports real data.
//! Second test: adversarial — a fixture repo whose config points both
//! vectors at marker-writing scripts. An unhardened control invocation
//! proves the fixture is live (markers appear); the same operations
//! through `run_git` must create no markers.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use bloch::run_git;

fn scratch(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("bloch-git-hardening-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn raw_git(root: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn write_script(path: &Path, body: &str) {
    fs::write(path, body).expect("script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}

/// A repo with one committed file, then a modification, whose config
/// weaponises both the external-diff and fsmonitor vectors.
fn hostile_repo(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let root = scratch(name);
    let ext_marker = root.join("extdiff-fired");
    let fsm_marker = root.join("fsmonitor-fired");
    let ext_script = root.join("ext.sh");
    let fsm_script = root.join("fsm.sh");
    write_script(
        &ext_script,
        &format!("#!/bin/sh\ntouch '{}'\n", ext_marker.display()),
    );
    // fsmonitor protocol v1: git runs `<cmd> 1 <token>`; execution is the
    // signal, so the script only records that it ran.
    write_script(
        &fsm_script,
        &format!("#!/bin/sh\ntouch '{}'\n", fsm_marker.display()),
    );

    assert!(raw_git(&root, &["init", "-q"]), "git init");
    assert!(raw_git(
        &root,
        &["config", "diff.external", ext_script.to_str().unwrap()]
    ));
    assert!(raw_git(
        &root,
        &["config", "core.fsmonitor", fsm_script.to_str().unwrap()]
    ));
    fs::write(root.join("tracked.txt"), "one\n").expect("tracked file");
    assert!(raw_git(&root, &["add", "tracked.txt"]), "git add");
    assert!(
        raw_git(
            &root,
            &[
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "user.name=test",
                "commit",
                "-qm",
                "init"
            ]
        ),
        "git commit"
    );
    fs::write(root.join("tracked.txt"), "one\ntwo\n").expect("modify tracked file");
    (root, ext_marker, fsm_marker)
}

#[test]
fn hardened_run_git_still_reports() {
    let (root, _, _) = hostile_repo("benign");
    let branch = run_git(&root, &["branch", "--show-current"]);
    assert!(!branch.is_empty(), "branch must be reported, got empty");
    let stat = run_git(&root, &["diff", "--stat", "--", "."]);
    assert!(
        stat.contains("tracked.txt"),
        "diff stat must name the file: {stat}"
    );
    let status = run_git(
        &root,
        &["status", "--porcelain=v1", "--untracked-files=normal"],
    );
    assert!(
        status.contains("tracked.txt"),
        "status must name the file: {status}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn hostile_repo_config_never_executes_through_run_git() {
    let (root, ext_marker, fsm_marker) = hostile_repo("hostile");

    // Control: unhardened git DOES fire both vectors in this fixture.
    let _ = raw_git(&root, &["status", "--porcelain"]);
    let _ = raw_git(&root, &["diff"]);
    assert!(
        fsm_marker.exists(),
        "control: fsmonitor script must fire under raw git"
    );
    assert!(
        ext_marker.exists(),
        "control: external diff must fire under raw git"
    );
    fs::remove_file(&ext_marker).expect("clear ext marker");
    fs::remove_file(&fsm_marker).expect("clear fsm marker");

    // The exact operations bloch performs on a caller-supplied root.
    let _ = run_git(
        &root,
        &["status", "--porcelain=v1", "--untracked-files=normal"],
    );
    let _ = run_git(&root, &["diff", "--stat", "--", "."]);
    let _ = run_git(&root, &["diff", "--cached", "--stat", "--", "."]);
    assert!(
        !fsm_marker.exists(),
        "fsmonitor script fired through hardened run_git"
    );
    assert!(
        !ext_marker.exists(),
        "external diff fired through hardened run_git"
    );
    let _ = fs::remove_dir_all(&root);
}
