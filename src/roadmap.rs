//! Roadmap system (bloch v0.2, SPEC `bloch-roadmap-20261027` S1–S3).
//!
//! `bloch init` scaffolds `.ai/roadmap.md` — a v0.1 → v1.0 task list with
//! stable `R-<phase><seq>` IDs — specialised per repo kind. Task completion
//! is gated: `task done` runs the task's `verify:` commands (bounded) and
//! flips the checkbox to `[x]` only when every command exits 0. There is
//! no override. Attempts are appended to `.ai/memory/attempts.jsonl` with
//! agent, duration, and self-rated confidence; a fail-then-pass sequence
//! auto-files an `attempt-fix-<id>` memory inbox entry for promotion into
//! `.ai/memory/lessons.md` (the learning loop).
//!
//! Contracts: roadmap input is bounded ([`ROADMAP_MAX_BYTES`],
//! [`ROADMAP_MAX_TASKS`], [`ROADMAP_MAX_LINES`]); unknown markdown lines
//! are preserved verbatim; verify commands are repo-authored trusted input
//! (Makefile trust class) executed via `sh -c`, bounded by
//! [`VERIFY_TIMEOUT_SECS`] and an output tail cap.

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{atomic_write, memory_inbox, BlochError, Result};

/// Maximum accepted size of `.ai/roadmap.md`, in bytes.
pub const ROADMAP_MAX_BYTES: u64 = 1_048_576;
/// Maximum number of tasks in one roadmap.
pub const ROADMAP_MAX_TASKS: usize = 500;
/// Maximum number of lines in one roadmap.
pub const ROADMAP_MAX_LINES: usize = 20_000;
/// Maximum accepted size of `.ai/memory/attempts.jsonl`, in bytes.
pub const ATTEMPTS_MAX_BYTES: u64 = 16 * 1_048_576;
/// Maximum attempt events read from the log in one pass.
pub const ATTEMPTS_MAX_EVENTS: usize = 20_000;
/// Per-command timeout for verify commands, in seconds.
pub const VERIFY_TIMEOUT_SECS: u64 = 600;
/// Maximum characters kept from a verify command's combined output tail.
pub const VERIFY_OUTPUT_TAIL: usize = 4_000;
const VERIFY_POLL_MS: u64 = 10;
const DETECTION_FILE_SCAN_MAX: usize = 3_000;
const MANIFEST_READ_MAX: u64 = 262_144;

// ---------------------------------------------------------------------------
// Repo kind + stack
// ---------------------------------------------------------------------------

/// The kind of repo a roadmap is specialised for.
///
/// `Config` and `Dotfiles` are detection outcomes only: roadmap generation
/// is refused for them (Owner ruling) and init is memory-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoKind {
    /// Application (CLI, desktop, mobile, service binary).
    App,
    /// Configuration repo — roadmap refused.
    Config,
    /// Dotfiles repo — roadmap refused.
    Dotfiles,
    /// Educational material (courses, books, curricula).
    Education,
    /// Game.
    Game,
    /// MCP server.
    Mcp,
    /// Tutorial repo (step-by-step learner material).
    Tutorial,
    /// Detection was ambiguous or found no signals.
    Unclassified,
}

impl RepoKind {
    /// Stable lowercase name used in roadmap headers and CLI values.
    pub fn as_str(&self) -> &'static str {
        match self {
            RepoKind::App => "app",
            RepoKind::Config => "config",
            RepoKind::Dotfiles => "dotfiles",
            RepoKind::Education => "education",
            RepoKind::Game => "game",
            RepoKind::Mcp => "mcp",
            RepoKind::Tutorial => "tutorial",
            RepoKind::Unclassified => "unclassified",
        }
    }

    /// Parse a kind name (case-insensitive). `None` for unknown names.
    pub fn parse(s: &str) -> Option<RepoKind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "app" => Some(RepoKind::App),
            "config" => Some(RepoKind::Config),
            "dotfiles" | "dotfile" => Some(RepoKind::Dotfiles),
            "education" => Some(RepoKind::Education),
            "game" => Some(RepoKind::Game),
            "mcp" => Some(RepoKind::Mcp),
            "tutorial" => Some(RepoKind::Tutorial),
            "unclassified" => Some(RepoKind::Unclassified),
            _ => None,
        }
    }

    /// True when the Owner's exclusion applies: no roadmap is generated.
    pub fn roadmap_refused(&self) -> bool {
        matches!(self, RepoKind::Config | RepoKind::Dotfiles)
    }
}

/// The detected build stack; parameterises template verify commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoStack {
    /// Rust / cargo.
    Cargo,
    /// Anything else; verify commands are file-presence checks.
    Generic,
    /// Godot project.
    Godot,
    /// mdBook.
    MdBook,
    /// Node / npm.
    Node,
    /// Python.
    Python,
    /// Unity project.
    Unity,
}

impl RepoStack {
    fn build_cmd(&self) -> &'static str {
        match self {
            RepoStack::Cargo => "cargo build",
            RepoStack::Generic => "test -f README.md",
            RepoStack::Godot => "godot --headless --quit",
            RepoStack::MdBook => "mdbook build",
            RepoStack::Node => "npm run build",
            RepoStack::Python => "python3 -m compileall .",
            RepoStack::Unity => "test -d Assets",
        }
    }

    fn test_cmd(&self) -> &'static str {
        match self {
            RepoStack::Cargo => "cargo test",
            RepoStack::Generic => "test -f README.md",
            RepoStack::Godot => "godot --headless --quit",
            RepoStack::MdBook => "mdbook build",
            RepoStack::Node => "npm test",
            RepoStack::Python => "python3 -m pytest",
            RepoStack::Unity => "test -f ProjectSettings/ProjectVersion.txt",
        }
    }

    fn lint_cmd(&self) -> Option<&'static str> {
        match self {
            RepoStack::Cargo => Some("cargo clippy -- -D warnings"),
            _ => None,
        }
    }

    fn release_version_cmd(&self, baseline_file: &str) -> String {
        match self {
            RepoStack::Cargo => "grep -q 'version = \"1.0.0\"' Cargo.toml".to_string(),
            RepoStack::Node => "grep -q '\"version\": \"1.0.0\"' package.json".to_string(),
            RepoStack::Python => "grep -q 'version = \"1.0.0\"' pyproject.toml".to_string(),
            _ => {
                let _ = baseline_file;
                "grep -q '1.0.0' CHANGELOG.md".to_string()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Kind detection (S1)
// ---------------------------------------------------------------------------

/// The result of repo-kind detection: the kind, the evidence that produced
/// it (every signal, labelled by the kind it supports), the detected stack,
/// the detected baseline version, and the repo name.
#[derive(Debug, Clone)]
pub struct KindDetection {
    /// Evidence lines, each prefixed with the kind it supports.
    pub evidence: Vec<String>,
    /// The detected kind (`Unclassified` when ambiguous or signal-free).
    pub kind: RepoKind,
    /// Repo directory name.
    pub repo_name: String,
    /// Detected baseline version (e.g. `0.1.0`).
    pub baseline: String,
    /// Detected build stack.
    pub stack: RepoStack,
}

fn read_capped(path: &Path, max_bytes: u64) -> String {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return String::new(),
    };
    if meta.len() > max_bytes {
        return String::new();
    }
    std::fs::read_to_string(path).unwrap_or_default()
}

fn score_add(
    scores: &mut [(RepoKind, u32, Vec<String>)],
    kind: RepoKind,
    points: u32,
    evidence: &str,
) {
    if let Some(entry) = scores.iter_mut().find(|(k, _, _)| *k == kind) {
        entry.1 += points;
        entry.2.push(evidence.to_string());
    }
}

fn score_of(scores: &[(RepoKind, u32, Vec<String>)], kind: RepoKind) -> u32 {
    scores
        .iter()
        .find(|(k, _, _)| *k == kind)
        .map(|(_, s, _)| *s)
        .unwrap_or(0)
}

fn detect_baseline(cargo: &str, package: &str, pyproject: &str) -> String {
    let from_kv = |text: &str, key: &str| -> Option<String> {
        for line in text.lines() {
            let t = line.trim();
            if let Some(rest) = t.strip_prefix(key) {
                let rest = rest.trim_start_matches(['=', ':', ' ']);
                let v = rest.trim_matches(['"', '\'', ','].as_ref());
                if !v.is_empty()
                    && v.chars()
                        .all(|c| c.is_ascii_digit() || c == '.' || c == '-')
                    && v.chars().any(|c| c.is_ascii_digit())
                {
                    return Some(v.to_string());
                }
            }
        }
        None
    };
    from_kv(cargo, "version")
        .or_else(|| from_kv(package, "\"version\""))
        .or_else(|| from_kv(pyproject, "version"))
        .unwrap_or_else(|| "0.1.0".to_string())
}

/// Detect the repo kind at `root`, reporting every signal as evidence.
///
/// Scoring is deterministic: each signal adds points to one kind. The
/// highest-scoring kind wins when it scores at least 3 and strictly beats
/// the runner-up; a tie is `Unclassified` (never a silent guess). A repo
/// with config/dotfiles signals, no build manifest, and no other kind
/// signals is `Config`/`Dotfiles`, for which roadmaps are refused.
pub fn detect_repo_kind(root: &Path) -> KindDetection {
    let repo_name = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "repo".to_string());

    let cargo = read_capped(&root.join("Cargo.toml"), MANIFEST_READ_MAX);
    let package = read_capped(&root.join("package.json"), MANIFEST_READ_MAX);
    let pyproject = read_capped(&root.join("pyproject.toml"), MANIFEST_READ_MAX);
    let has_manifest = !cargo.is_empty()
        || !package.is_empty()
        || !pyproject.is_empty()
        || root.join("go.mod").is_file();

    let mut scores: Vec<(RepoKind, u32, Vec<String>)> = vec![
        (RepoKind::Mcp, 0, Vec::new()),
        (RepoKind::Game, 0, Vec::new()),
        (RepoKind::App, 0, Vec::new()),
        (RepoKind::Education, 0, Vec::new()),
        (RepoKind::Tutorial, 0, Vec::new()),
        (RepoKind::Config, 0, Vec::new()),
        (RepoKind::Dotfiles, 0, Vec::new()),
    ];

    let cargo_lc = cargo.to_ascii_lowercase();
    let package_lc = package.to_ascii_lowercase();
    let pyproject_lc = pyproject.to_ascii_lowercase();

    // Manifest-content signals.
    if cargo_lc.contains("rmcp") || cargo_lc.contains("modelcontextprotocol") {
        score_add(
            &mut scores,
            RepoKind::Mcp,
            3,
            "Cargo.toml names an MCP SDK (rmcp/modelcontextprotocol)",
        );
    } else if cargo_lc.contains("mcp") {
        score_add(&mut scores, RepoKind::Mcp, 3, "Cargo.toml mentions mcp");
    }
    if package_lc.contains("@modelcontextprotocol") || package_lc.contains("modelcontextprotocol") {
        score_add(
            &mut scores,
            RepoKind::Mcp,
            3,
            "package.json names @modelcontextprotocol",
        );
    }
    if pyproject_lc.contains("mcp") {
        score_add(&mut scores, RepoKind::Mcp, 2, "pyproject.toml mentions mcp");
    }
    if cargo_lc.contains("bevy") {
        score_add(&mut scores, RepoKind::Game, 3, "Cargo.toml depends on bevy");
    }
    if package_lc.contains("phaser") || package_lc.contains("pixi") {
        score_add(
            &mut scores,
            RepoKind::Game,
            2,
            "package.json names a game engine (phaser/pixi)",
        );
    }
    if cargo_lc.contains("tauri") || package_lc.contains("electron") {
        score_add(
            &mut scores,
            RepoKind::App,
            3,
            "manifest names a desktop app shell (tauri/electron)",
        );
    }
    if cargo.contains("[[bin]]") || root.join("src").join("main.rs").is_file() {
        score_add(
            &mut scores,
            RepoKind::App,
            2,
            "binary crate (src/main.rs or [[bin]])",
        );
    }

    // File-layout signals (bounded scan).
    let files = crate::iter_all_files(root);
    let mut md_count = 0usize;
    let mut code_count = 0usize;
    let mut config_count = 0usize;
    let mut has_curriculum = false;
    let mut has_examples = false;
    let mut has_exercises = false;
    let mut has_solutions = false;
    let mut has_mcp_name = false;
    for path in files.iter().take(DETECTION_FILE_SCAN_MAX) {
        let rel = path
            .strip_prefix(root)
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if rel.starts_with(".ai/") || rel.starts_with(".git/") {
            continue;
        }
        let lc = rel.to_ascii_lowercase();
        match path.extension().and_then(|e| e.to_str()) {
            Some("md") => md_count += 1,
            Some("rs" | "py" | "js" | "ts" | "lua" | "gd" | "cs" | "go") => code_count += 1,
            Some("toml" | "yaml" | "yml" | "ini" | "conf" | "nix") => config_count += 1,
            _ => {}
        }
        if lc.contains("curriculum") || lc.contains("syllabus") || lc.contains("/lesson") {
            has_curriculum = true;
        }
        if lc.starts_with("examples/") || lc.contains("/examples/") {
            has_examples = true;
        }
        if lc.starts_with("exercises/") || lc.contains("/exercises/") {
            has_exercises = true;
        }
        if lc.starts_with("solutions/") || lc.contains("/solutions/") {
            has_solutions = true;
        }
        if lc.contains("mcp") {
            has_mcp_name = true;
        }
    }
    if has_mcp_name {
        score_add(&mut scores, RepoKind::Mcp, 1, "file paths mention mcp");
    }
    if root.join("project.godot").is_file() {
        score_add(
            &mut scores,
            RepoKind::Game,
            3,
            "project.godot present (Godot)",
        );
    }
    if root
        .join("ProjectSettings")
        .join("ProjectVersion.txt")
        .is_file()
        || root.join("Assets").is_dir()
    {
        score_add(
            &mut scores,
            RepoKind::Game,
            3,
            "Unity project layout (Assets/, ProjectSettings/)",
        );
    }
    if root.join("AndroidManifest.xml").is_file()
        || root.join("build.gradle").is_file()
        || root.join("settings.gradle").is_file()
    {
        score_add(
            &mut scores,
            RepoKind::App,
            2,
            "Android/Gradle build files present",
        );
    }
    if root.join("book.toml").is_file() {
        score_add(
            &mut scores,
            RepoKind::Education,
            3,
            "book.toml present (mdBook)",
        );
    }
    if has_curriculum {
        score_add(
            &mut scores,
            RepoKind::Education,
            2,
            "curriculum/syllabus/lesson files present",
        );
    }
    if md_count >= 10 && md_count > code_count {
        score_add(
            &mut scores,
            RepoKind::Education,
            2,
            "docs-dominant tree (markdown outnumbers code)",
        );
    }
    if has_exercises && has_solutions {
        score_add(
            &mut scores,
            RepoKind::Tutorial,
            3,
            "exercises/ + solutions/ structure",
        );
    } else if has_examples && has_solutions {
        score_add(
            &mut scores,
            RepoKind::Tutorial,
            3,
            "examples/ + solutions/ structure",
        );
    } else if has_exercises || has_examples {
        score_add(
            &mut scores,
            RepoKind::Tutorial,
            1,
            "examples/ or exercises/ present",
        );
    }

    // Config / dotfiles signals.
    let name_lc = repo_name.to_ascii_lowercase();
    if name_lc.contains("dotfiles") || name_lc.contains("dotfile") {
        score_add(
            &mut scores,
            RepoKind::Dotfiles,
            3,
            "repo name says dotfiles",
        );
    }
    if root.join(".config").is_dir() {
        score_add(
            &mut scores,
            RepoKind::Dotfiles,
            2,
            ".config/ directory present",
        );
    }
    for rc in [".bashrc", ".zshrc", ".tmux.conf", ".vimrc", ".gitconfig"] {
        if root.join(rc).is_file() {
            score_add(
                &mut scores,
                RepoKind::Dotfiles,
                2,
                &format!("dotfile {} present", rc),
            );
        }
    }
    if root.join("configuration.nix").is_file() || root.join("home.nix").is_file() {
        score_add(
            &mut scores,
            RepoKind::Config,
            3,
            "NixOS/home-manager configuration files present",
        );
    }
    if !has_manifest && config_count > 0 && code_count == 0 {
        score_add(
            &mut scores,
            RepoKind::Config,
            2,
            "config-dominant tree, no build manifest, no code",
        );
    }

    // Stack (independent of kind).
    let stack = if root.join("project.godot").is_file() {
        RepoStack::Godot
    } else if root
        .join("ProjectSettings")
        .join("ProjectVersion.txt")
        .is_file()
    {
        RepoStack::Unity
    } else if !cargo.is_empty() {
        RepoStack::Cargo
    } else if !package.is_empty() {
        RepoStack::Node
    } else if !pyproject.is_empty() {
        RepoStack::Python
    } else if root.join("book.toml").is_file() {
        RepoStack::MdBook
    } else {
        RepoStack::Generic
    };

    let baseline = detect_baseline(&cargo, &package, &pyproject);

    // Decision.
    let kinds = [
        RepoKind::Mcp,
        RepoKind::Game,
        RepoKind::App,
        RepoKind::Education,
        RepoKind::Tutorial,
    ];
    let other_max = kinds
        .iter()
        .map(|k| score_of(&scores, *k))
        .max()
        .unwrap_or(0);
    let config_score = score_of(&scores, RepoKind::Config);
    let dot_score = score_of(&scores, RepoKind::Dotfiles);
    let kind = if !has_manifest && other_max == 0 && (config_score > 0 || dot_score > 0) {
        if dot_score >= config_score {
            RepoKind::Dotfiles
        } else {
            RepoKind::Config
        }
    } else if other_max < 3 {
        RepoKind::Unclassified
    } else {
        let winners: Vec<RepoKind> = kinds
            .iter()
            .copied()
            .filter(|k| score_of(&scores, *k) == other_max)
            .collect();
        if winners.len() == 1 {
            winners[0]
        } else {
            RepoKind::Unclassified
        }
    };

    let mut evidence: Vec<String> = Vec::new();
    for (k, s, ev) in &scores {
        for e in ev {
            evidence.push(format!("[{} +{}] {}", k.as_str(), s, e));
        }
    }
    if kind == RepoKind::Unclassified {
        if other_max == 0 {
            evidence.push("[unclassified] no kind signals found".to_string());
        } else {
            evidence.push(format!(
                "[unclassified] ambiguous: top score {} is tied or below the confidence floor",
                other_max
            ));
        }
    }

    KindDetection {
        evidence,
        kind,
        repo_name,
        baseline,
        stack,
    }
}

// ---------------------------------------------------------------------------
// Roadmap format (S1): parse / render with stable IDs
// ---------------------------------------------------------------------------

/// Status of a roadmap task, carried by its checkbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// `[ ]` — open.
    Open,
    /// `[~]` — an attempt is live or was started.
    InProgress,
    /// `[x]` — green; set by bloch only after the verify gate passes.
    Green,
    /// Struck through by hand — dropped, never renumbered.
    Dropped,
}

impl TaskStatus {
    /// The checkbox character for this status.
    pub fn checkbox(&self) -> char {
        match self {
            TaskStatus::Open => ' ',
            TaskStatus::InProgress => '~',
            TaskStatus::Green => 'x',
            TaskStatus::Dropped => ' ',
        }
    }

    /// Stable lowercase name used in snapshots and rollups.
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Open => "open",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::Green => "green",
            TaskStatus::Dropped => "dropped",
        }
    }
}

/// One roadmap task with its stable ID, verify commands, and evidence line.
#[derive(Debug, Clone, PartialEq)]
pub struct RoadmapTask {
    /// Evidence text recorded on the task's `evidence:` sub-line.
    pub evidence: String,
    /// Stable ID, `R-<phase><seq>` (never renumbered).
    pub id: String,
    /// Index of the task's line in [`Roadmap::lines`].
    pub line_index: usize,
    /// Phase number (from the ID).
    pub phase: u32,
    /// Task status from its checkbox / strikethrough.
    pub status: TaskStatus,
    /// Task title text.
    pub title: String,
    /// `verify:` commands, in authored order. Empty means never greenable.
    pub verify: Vec<String>,
}

/// A parsed roadmap: header fields plus the raw lines, so unknown lines
/// survive a parse → render round trip verbatim.
#[derive(Debug, Clone)]
pub struct Roadmap {
    /// Baseline version from the header comment (e.g. `0.1.0`).
    pub baseline: String,
    /// Kind string from the header comment (e.g. `mcp`).
    pub kind: String,
    /// Lane stamp from the header comment (`general` | `restricted`, §9).
    pub lane: String,
    /// Raw lines of the file, in order.
    pub lines: Vec<String>,
    /// `(phase number, title)` per `## Phase` heading, in file order.
    pub phases: Vec<(u32, String)>,
    /// Repo name from the title line.
    pub repo_name: String,
    /// Target version from the header comment (e.g. `1.0.0`).
    pub target: String,
    /// Tasks in file order.
    pub tasks: Vec<RoadmapTask>,
}

/// Parse a task ID `R-<phase><seq>` into `(phase, seq)`.
///
/// The sequence is the last two digits; everything before them is the
/// phase. Returns `None` for malformed IDs (the renumbering-attack guard:
/// IDs are validated everywhere they are consumed).
pub fn parse_task_id(id: &str) -> Option<(u32, u32)> {
    let digits = id.strip_prefix("R-")?;
    if digits.len() < 3 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (phase_s, seq_s) = digits.split_at(digits.len() - 2);
    let phase: u32 = phase_s.parse().ok()?;
    let seq: u32 = seq_s.parse().ok()?;
    if phase == 0 || seq == 0 {
        return None;
    }
    Some((phase, seq))
}

fn parse_task_line(line: &str) -> Option<(TaskStatus, String, String)> {
    let t = line.trim_start();
    let rest = t.strip_prefix("- [")?;
    let mut chars = rest.chars();
    let status_ch = chars.next()?;
    if !matches!(status_ch, ' ' | '~' | 'x' | 'X') {
        return None;
    }
    let after = chars.as_str().strip_prefix("] ")?;
    let mut dropped = false;
    let after = if let Some(stripped) = after.strip_prefix("~~") {
        dropped = true;
        stripped
    } else {
        after
    };
    let (id, title) = match after.split_once(' ') {
        Some((i, t)) => (i, t),
        None => (after, ""),
    };
    let id = id.trim_end_matches('~').trim();
    parse_task_id(id)?;
    let mut title = title.trim().to_string();
    if title.ends_with("~~") {
        dropped = true;
        title = title.trim_end_matches('~').trim().to_string();
    }
    let status = if dropped {
        TaskStatus::Dropped
    } else if status_ch == 'x' || status_ch == 'X' {
        TaskStatus::Green
    } else if status_ch == '~' {
        TaskStatus::InProgress
    } else {
        TaskStatus::Open
    };
    Some((status, id.to_string(), title))
}

fn parse_phase_heading(line: &str) -> Option<(u32, String)> {
    let t = line.trim_start().strip_prefix("## Phase ")?;
    let num_s: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    let phase: u32 = num_s.parse().ok()?;
    let rest = t[num_s.len()..].trim_start_matches([' ', '-', '—']);
    Some((phase, rest.trim().to_string()))
}

/// Parse roadmap markdown into a [`Roadmap`].
///
/// Bounded: inputs over [`ROADMAP_MAX_BYTES`], [`ROADMAP_MAX_LINES`], or
/// [`ROADMAP_MAX_TASKS`] are refused with a typed error. Duplicate task
/// IDs are refused (`duplicate_task_id`). Unknown lines are preserved in
/// [`Roadmap::lines`] verbatim.
pub fn roadmap_parse(content: &str) -> Result<Roadmap> {
    if content.len() as u64 > ROADMAP_MAX_BYTES {
        return Err(BlochError::GitFailed(format!(
            "roadmap_too_large: {} bytes exceeds limit {}",
            content.len(),
            ROADMAP_MAX_BYTES
        )));
    }
    let lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    if lines.len() > ROADMAP_MAX_LINES {
        return Err(BlochError::GitFailed(format!(
            "roadmap_too_many_lines: {} exceeds limit {}",
            lines.len(),
            ROADMAP_MAX_LINES
        )));
    }

    let mut kind = "unclassified".to_string();
    let mut lane = "general".to_string();
    let mut baseline = "0.1.0".to_string();
    let mut target = "1.0.0".to_string();
    let mut repo_name = String::new();
    for line in &lines {
        if line.contains("bloch:roadmap") {
            for tok in line.split_whitespace() {
                if let Some(v) = tok.strip_prefix("kind=") {
                    kind = v.trim_end_matches(['>', '-']).to_string();
                } else if let Some(v) = tok.strip_prefix("lane=") {
                    let raw = v.trim_end_matches(['>', '-']);
                    // Normalise to the canonical lane name: legacy
                    // pre-0.3 header stamps parse through the alias
                    // arms and come back generic; unknown values are
                    // preserved verbatim.
                    lane = match Lane::parse(raw) {
                        Some(l) => l.as_str().to_string(),
                        None => raw.to_string(),
                    };
                } else if let Some(v) = tok.strip_prefix("baseline=") {
                    baseline = v.trim_end_matches(['>', '-']).to_string();
                } else if let Some(v) = tok.strip_prefix("target=") {
                    target = v.trim_end_matches(['>', '-']).to_string();
                }
            }
        }
        if repo_name.is_empty() && line.starts_with("# Roadmap") {
            if let Some(start) = line.find("— ") {
                let tail = &line[start + "— ".len()..];
                repo_name = tail.split(" (").next().unwrap_or(tail).trim().to_string();
            }
        }
    }

    let mut phases: Vec<(u32, String)> = Vec::new();
    let mut tasks: Vec<RoadmapTask> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    for (idx, line) in lines.iter().enumerate() {
        if let Some((phase, title)) = parse_phase_heading(line) {
            phases.push((phase, title));
            continue;
        }
        if let Some((status, id, title)) = parse_task_line(line) {
            if tasks.len() >= ROADMAP_MAX_TASKS {
                return Err(BlochError::GitFailed(format!(
                    "roadmap_too_many_tasks: exceeds limit {}",
                    ROADMAP_MAX_TASKS
                )));
            }
            if !seen_ids.insert(id.clone()) {
                return Err(BlochError::GitFailed(format!(
                    "duplicate_task_id: {} appears more than once",
                    id
                )));
            }
            let (phase, _) = parse_task_id(&id).unwrap_or((0, 0));
            tasks.push(RoadmapTask {
                evidence: String::new(),
                id,
                line_index: idx,
                phase,
                status,
                title,
                verify: Vec::new(),
            });
        }
    }

    // Attach verify/evidence sub-lines to their task (indented lines that
    // follow the task line, up to the next task, heading, or prose line).
    for task in tasks.iter_mut() {
        let start = task.line_index + 1;
        let mut verify: Vec<String> = Vec::new();
        let mut evidence = String::new();
        for line in lines.iter().skip(start) {
            if line.trim().is_empty() {
                break;
            }
            if parse_task_line(line).is_some() || parse_phase_heading(line).is_some() {
                break;
            }
            if !line.starts_with(char::is_whitespace) {
                break;
            }
            let t = line.trim();
            if let Some(cmd) = t.strip_prefix("- verify:") {
                verify.push(cmd.trim().to_string());
            } else if let Some(ev) = t.strip_prefix("- evidence:") {
                evidence = ev.trim().to_string();
            }
        }
        task.verify = verify;
        task.evidence = evidence;
    }

    Ok(Roadmap {
        baseline,
        kind,
        lane,
        lines,
        phases,
        repo_name,
        target,
        tasks,
    })
}

/// Render a parsed roadmap back to markdown. Idempotent:
/// `render(parse(render(parse(x)))) == render(parse(x))`.
pub fn roadmap_render(rm: &Roadmap) -> String {
    if rm.lines.is_empty() {
        return String::new();
    }
    let mut out = rm.lines.join("\n");
    out.push('\n');
    out
}

/// Path of the roadmap inside a repo.
pub fn roadmap_path(root: &Path) -> PathBuf {
    root.join(".ai").join("roadmap.md")
}

/// Load and parse `.ai/roadmap.md`; `Ok(None)` when absent.
pub fn roadmap_load(root: &Path) -> Result<Option<Roadmap>> {
    let path = roadmap_path(root);
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(BlochError::Io(e)),
    };
    if meta.len() > ROADMAP_MAX_BYTES {
        return Err(BlochError::GitFailed(format!(
            "roadmap_too_large: {} bytes exceeds limit {}",
            meta.len(),
            ROADMAP_MAX_BYTES
        )));
    }
    let content = std::fs::read_to_string(&path)?;
    Ok(Some(roadmap_parse(&content)?))
}

/// Load the roadmap or fail with `roadmap_not_found`.
pub fn roadmap_load_required(root: &Path) -> Result<Roadmap> {
    roadmap_load(root)?.ok_or_else(|| {
        BlochError::GitFailed(
            "roadmap_not_found: no .ai/roadmap.md in this repo; run `bloch init` first".to_string(),
        )
    })
}

/// Persist a roadmap atomically (temp file + rename).
pub fn roadmap_save(root: &Path, rm: &Roadmap) -> Result<()> {
    let path = roadmap_path(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write(&path, &roadmap_render(rm))
}

fn roadmap_find<'a>(rm: &'a Roadmap, id: &str) -> Result<&'a RoadmapTask> {
    rm.tasks
        .iter()
        .find(|t| t.id == id)
        .ok_or_else(|| BlochError::GitFailed(format!("task_not_found: {}", id)))
}

fn set_task_status(rm: &mut Roadmap, id: &str, status: TaskStatus) -> Result<()> {
    let pos = rm
        .tasks
        .iter()
        .position(|t| t.id == id)
        .ok_or_else(|| BlochError::GitFailed(format!("task_not_found: {}", id)))?;
    let line_index = rm.tasks[pos].line_index;
    let line = rm.lines.get(line_index).cloned().unwrap_or_default();
    if let Some(bracket) = line.find("- [") {
        let char_idx = bracket + 3;
        let mut bytes = line.into_bytes();
        if char_idx < bytes.len() {
            bytes[char_idx] = status.checkbox() as u8;
        }
        rm.lines[line_index] = String::from_utf8_lossy(&bytes).to_string();
    }
    rm.tasks[pos].status = status;
    Ok(())
}

fn set_task_evidence(rm: &mut Roadmap, id: &str, text: &str) -> Result<()> {
    let pos = rm
        .tasks
        .iter()
        .position(|t| t.id == id)
        .ok_or_else(|| BlochError::GitFailed(format!("task_not_found: {}", id)))?;
    let start = rm.tasks[pos].line_index;
    let new_line = format!("  - evidence: {}", text);
    let mut insert_at = start + 1;
    let mut replaced = false;
    let mut idx = start + 1;
    while idx < rm.lines.len() {
        let line = &rm.lines[idx];
        if line.trim().is_empty()
            || parse_task_line(line).is_some()
            || parse_phase_heading(line).is_some()
            || !line.starts_with(char::is_whitespace)
        {
            break;
        }
        insert_at = idx + 1;
        if line.trim_start().starts_with("- evidence:") {
            rm.lines[idx] = new_line.clone();
            replaced = true;
            break;
        }
        idx += 1;
    }
    if !replaced {
        rm.lines.insert(insert_at, new_line);
        for t in rm.tasks.iter_mut() {
            if t.line_index >= insert_at {
                t.line_index += 1;
            }
        }
    }
    if let Some(t) = rm.tasks.iter_mut().find(|t| t.id == id) {
        t.evidence = text.to_string();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Templates (S1): common spine + per-kind phases
// ---------------------------------------------------------------------------

struct TemplateTask {
    title: &'static str,
    verify: Vec<String>,
}

fn one(cmd: &str) -> Vec<String> {
    vec![cmd.to_string()]
}

fn major_minor(version: &str) -> String {
    let parts: Vec<&str> = version.split('.').collect();
    if parts.len() >= 2 {
        format!("{}.{}", parts[0], parts[1])
    } else {
        "0.1".to_string()
    }
}

fn template_phases(
    kind: RepoKind,
    stack: RepoStack,
) -> Vec<(u32, &'static str, Vec<TemplateTask>)> {
    let build = stack.build_cmd();
    let test = stack.test_cmd();
    let lint = stack.lint_cmd().unwrap_or(test);

    let mut phase1 = vec![
        TemplateTask {
            title: "Build is green from a clean checkout",
            verify: one(build),
        },
        TemplateTask {
            title: "Existing tests pass at the baseline",
            verify: one(test),
        },
        TemplateTask {
            title: "Repo identity, baseline version, and this roadmap are recorded",
            verify: one("test -f .ai/roadmap.md"),
        },
    ];
    if kind == RepoKind::Unclassified {
        phase1.insert(
            0,
            TemplateTask {
                title: "Classify this repo (mcp, game, app, education, or tutorial), then regenerate this roadmap with `bloch init --kind <kind>` on a branch",
                verify: one("test -f .ai/roadmap.md"),
            },
        );
    }

    let phase2: Vec<TemplateTask> = match kind {
        RepoKind::Mcp => vec![
            TemplateTask {
                title: "Tool surface is contracted: every tool has a complete schema and annotations from birth",
                verify: one(test),
            },
            TemplateTask {
                title: "Tool count stays within the declared budget (<= 25 tools)",
                verify: one(test),
            },
            TemplateTask {
                title: "Typed error and refusal behaviour is implemented in server code, never left to client hints",
                verify: one(test),
            },
        ],
        RepoKind::Game => vec![
            TemplateTask {
                title: "Core loop is playable from start to finish",
                verify: one(build),
            },
            TemplateTask {
                title: "Save and load round-trips game state",
                verify: one(test),
            },
            TemplateTask {
                title: "Settings and accessibility options persist across sessions",
                verify: one(test),
            },
        ],
        RepoKind::App => vec![
            TemplateTask {
                title: "Primary workflow runs end to end",
                verify: one(test),
            },
            TemplateTask {
                title: "Persistence and data migration round-trip without loss",
                verify: one(test),
            },
            TemplateTask {
                title: "Error and offline states are handled explicitly",
                verify: one(test),
            },
        ],
        RepoKind::Education => vec![
            TemplateTask {
                title: "Learning objectives are stated for every unit",
                verify: one(build),
            },
            TemplateTask {
                title: "Curriculum is sequenced with prerequisites in order",
                verify: one(build),
            },
            TemplateTask {
                title: "Every exercise ships with a worked solution",
                verify: one(build),
            },
        ],
        RepoKind::Tutorial => vec![
            TemplateTask {
                title: "First step takes a fresh learner from zero to running",
                verify: one(build),
            },
            TemplateTask {
                title: "Every step builds or runs at its checkpoint",
                verify: one(test),
            },
            TemplateTask {
                title: "Per-step validation tells the learner what success looks like",
                verify: one(test),
            },
        ],
        _ => vec![
            TemplateTask {
                title: "Core functionality is complete against the repo's own spec",
                verify: one(test),
            },
            TemplateTask {
                title: "Primary workflow runs end to end",
                verify: one(test),
            },
        ],
    };

    let mut phase3 = vec![
        TemplateTask {
            title: "Validation tests cover the happy path end to end (half the test split)",
            verify: one(test),
        },
        TemplateTask {
            title: "Adversarial tests cover malformed, oversized, and hostile inputs (half the test split)",
            verify: one(test),
        },
    ];
    match kind {
        RepoKind::Mcp => phase3.push(TemplateTask {
            title: "Refusal and typed-error behaviour is covered by tests, including oversized payloads",
            verify: one(test),
        }),
        RepoKind::Game => phase3.push(TemplateTask {
            title: "Structured playtest checklist is executed and its record filed",
            verify: one(test),
        }),
        RepoKind::App => phase3.push(TemplateTask {
            title: "Migration from the previous data format is tested",
            verify: one(test),
        }),
        RepoKind::Education => phase3.push(TemplateTask {
            title: "Assessment answers are checked against the marking rubric",
            verify: one(build),
        }),
        RepoKind::Tutorial => phase3.push(TemplateTask {
            title: "Every checkpoint command is executed by the test pass",
            verify: one(test),
        }),
        _ => {}
    }

    let phase4: Vec<TemplateTask> = match kind {
        RepoKind::Mcp => vec![
            TemplateTask {
                title: "MCP Inspector conformance pass is recorded as evidence",
                verify: one(test),
            },
            TemplateTask {
                title: "Client matrix is verified with at least two real clients",
                verify: one(test),
            },
            TemplateTask {
                title: "Declared-vs-detected access (EFLO) check passes: no tool exceeds its annotations",
                verify: one(lint),
            },
            TemplateTask {
                title: "Registry/release packaging builds from a clean checkout",
                verify: one(build),
            },
        ],
        RepoKind::Game => vec![
            TemplateTask {
                title: "Performance budget holds on target hardware",
                verify: one(test),
            },
            TemplateTask {
                title: "Platform builds produce runnable artefacts",
                verify: one(build),
            },
            TemplateTask {
                title: "Safety-critical options (stop/safeword-equivalents, accessibility) are never gated behind progression",
                verify: one(test),
            },
        ],
        RepoKind::App => vec![
            TemplateTask {
                title: "Install and update path is verified from a clean environment",
                verify: one(build),
            },
            TemplateTask {
                title: "Data-safety review is completed and recorded",
                verify: one(test),
            },
            TemplateTask {
                title: "Platform builds produce installable artefacts",
                verify: one(build),
            },
        ],
        RepoKind::Education => vec![
            TemplateTask {
                title: "Subject-accuracy review pass is completed by a qualified reviewer",
                verify: one(build),
            },
            TemplateTask {
                title: "Assessment items ship with a marking rubric",
                verify: one(build),
            },
            TemplateTask {
                title: "Materials meet the accessibility checklist (alt text, reading level, contrast)",
                verify: one(build),
            },
        ],
        RepoKind::Tutorial => vec![
            TemplateTask {
                title: "Solutions are kept separate from exercise text",
                verify: one(test),
            },
            TemplateTask {
                title: "Fresh-learner walkthrough gate passes with no prior context",
                verify: one(test),
            },
            TemplateTask {
                title: "Everything the learner installs is version-pinned",
                verify: one(test),
            },
        ],
        _ => vec![
            TemplateTask {
                title: "Error paths return typed, bounded failures",
                verify: one(test),
            },
            TemplateTask {
                title: "No unbounded work: inputs, outputs, retries, and time are all bounded",
                verify: one(lint),
            },
        ],
    };

    let mut phase5 = vec![
        TemplateTask {
            title: "README documents install, usage, and verification",
            verify: one("test -f README.md"),
        },
        TemplateTask {
            title: "CHANGELOG records every user-facing change",
            verify: one("test -f CHANGELOG.md"),
        },
    ];
    match kind {
        RepoKind::Mcp => phase5.push(TemplateTask {
            title: "Tool reference documents every tool, schema, annotation, and error kind",
            verify: one("test -f README.md"),
        }),
        RepoKind::Education => phase5.push(TemplateTask {
            title: "Instructor/maintainer notes document how to extend the material",
            verify: one(build),
        }),
        RepoKind::Tutorial => phase5.push(TemplateTask {
            title: "Troubleshooting section covers the failures a fresh learner actually hits",
            verify: one("test -f README.md"),
        }),
        _ => {}
    }

    let phase6 = vec![
        TemplateTask {
            title: "Version is bumped to 1.0.0 and the changelog records the release",
            verify: one(&stack.release_version_cmd("")),
        },
        TemplateTask {
            title: "Clean-environment install and run is verified",
            verify: one(build),
        },
        TemplateTask {
            title: "Release artefacts are built from a clean checkout and checksummed",
            verify: one(build),
        },
    ];

    vec![
        (1, "Baseline truth", phase1),
        (2, "Core complete", phase2),
        (3, "Tests and gates", phase3),
        (4, "Hardening", phase4),
        (5, "Docs", phase5),
        (6, "Release v1", phase6),
    ]
}

/// Render a fresh roadmap from the template for `kind`.
///
/// Task IDs are assigned `R-<phase><seq>` with `seq` counting from 01
/// within each phase; IDs are stable once written and never renumbered
/// by bloch.
pub fn roadmap_render_template(
    repo_name: &str,
    kind: RepoKind,
    baseline: &str,
    stack: RepoStack,
    lane: Lane,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Roadmap — {} ({}) — v{} → v1.0\n",
        repo_name,
        kind.as_str(),
        major_minor(baseline)
    ));
    out.push_str(&format!(
        "<!-- bloch:roadmap schema=1 kind={} lane={} baseline={} target=1.0.0 -->\n\n",
        kind.as_str(),
        lane.as_str(),
        baseline
    ));
    out.push_str(
        "How to work this roadmap (any agent): run `bloch task start R-xxx --agent <you>` \
         before working a task, `bloch task done R-xxx` when you believe it is complete — \
         done runs every `verify:` command and flips the task green only if all pass; \
         there is no override. `bloch task fail R-xxx --error \"...\"` records a failure. \
         Fixes learned the hard way land in `.ai/memory/lessons.md` via the inbox/promote \
         flow and are surfaced in every HANDOFF.\n\n",
    );
    for (phase, title, tasks) in template_phases(kind, stack) {
        out.push_str(&format!("## Phase {} — {}\n", phase, title));
        for (i, task) in tasks.iter().enumerate() {
            out.push_str(&format!("- [ ] R-{}{:02} {}\n", phase, i + 1, task.title));
            for cmd in &task.verify {
                out.push_str(&format!("  - verify: {}\n", cmd));
            }
            out.push_str("  - evidence:\n");
        }
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// Init (S1)
// ---------------------------------------------------------------------------

/// Outcome of [`bloch_init`].
#[derive(Debug, Clone)]
pub struct InitOutcome {
    /// Full detection (evidence is reported even when `--kind` overrides).
    pub detection: KindDetection,
    /// The kind the init acted on.
    pub kind: RepoKind,
    /// Human-readable summary of what happened.
    pub message: String,
    /// True when a new `.ai/roadmap.md` was written.
    pub roadmap_written: bool,
    /// True when the roadmap was refused (config/dotfiles).
    pub refused: bool,
}

/// Initialise bloch in a repo: create `.ai/memory/` (+ `inbox/`) and
/// `.ai/skills/` if absent, then write `.ai/roadmap.md` from the kind
/// template unless the kind is config/dotfiles (refused — memory-only
/// init) or a roadmap already exists (never overwritten).
pub fn bloch_init(root: &Path, kind_override: Option<RepoKind>) -> Result<InitOutcome> {
    let mem_dir = root.join(".ai").join("memory");
    std::fs::create_dir_all(mem_dir.join("inbox"))?;
    std::fs::create_dir_all(root.join(".ai").join("skills"))?;

    let detection = detect_repo_kind(root);
    let kind = kind_override.unwrap_or(detection.kind);

    if kind.roadmap_refused() {
        return Ok(InitOutcome {
            detection,
            kind,
            message: format!(
                "roadmap_refused: {} repos do not get a v1 roadmap (Owner ruling); memory-only init performed (.ai/memory/ + .ai/skills/ created)",
                kind.as_str()
            ),
            roadmap_written: false,
            refused: true,
        });
    }

    let lane = repo_lane(root);
    let lane_stamp_path = root.join(".ai").join("lane");
    if !lane_stamp_path.exists() {
        std::fs::write(&lane_stamp_path, format!("lane: {}\n", lane.as_str()))?;
    }

    let attempts_path = mem_dir.join("attempts.jsonl");
    if !attempts_path.exists() {
        std::fs::write(&attempts_path, "")?;
    }

    let path = roadmap_path(root);
    if path.exists() {
        return Ok(InitOutcome {
            detection,
            kind,
            message: format!(
                "existing .ai/roadmap.md preserved (init never overwrites); kind acted on: {}",
                kind.as_str()
            ),
            roadmap_written: false,
            refused: false,
        });
    }

    let content = roadmap_render_template(
        &detection.repo_name,
        kind,
        &detection.baseline,
        detection.stack,
        lane,
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write(&path, &content)?;
    let message = format!(
        "roadmap written for {} ({}) — v{} → v1.0",
        detection.repo_name,
        kind.as_str(),
        major_minor(&detection.baseline)
    );
    Ok(InitOutcome {
        detection,
        kind,
        message,
        roadmap_written: true,
        refused: false,
    })
}

// ---------------------------------------------------------------------------
// Lane law (SPEC §9.1): labels from birth, monotone, fail closed
// ---------------------------------------------------------------------------

/// A repo lane (provenance). Stamped from the repo's lane stamp at init
/// and never changed by a transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// The general lane.
    General,
    /// The restricted lane.
    Restricted,
}

impl Lane {
    /// Stable lowercase name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Lane::General => "general",
            Lane::Restricted => "restricted",
        }
    }

    /// Parse a lane name (case-insensitive). The pre-0.3 estate names
    /// (`qompassai`, `ntranch`) are accepted as legacy read aliases so
    /// existing records keep working; they are never written back out —
    /// everything bloch emits uses the generic names.
    pub fn parse(s: &str) -> Option<Lane> {
        match s.trim().to_ascii_lowercase().as_str() {
            "general" => Some(Lane::General),
            "restricted" => Some(Lane::Restricted),
            // Legacy aliases (read-only back-compat, pre-0.3 records).
            "qompassai" => Some(Lane::General),
            "ntranch" => Some(Lane::Restricted),
            _ => None,
        }
    }
}

/// Content label governing visibility (SPEC §9.1): visibility follows the
/// content label, never the origin lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentLabel {
    /// Safe for every caller.
    Sfw,
    /// Walled: never served to general-only callers.
    Nsfw,
}

impl ContentLabel {
    /// Stable lowercase name.
    pub fn as_str(&self) -> &'static str {
        match self {
            ContentLabel::Sfw => "sfw",
            ContentLabel::Nsfw => "nsfw",
        }
    }

    /// Parse a content label (case-insensitive).
    pub fn parse(s: &str) -> Option<ContentLabel> {
        match s.trim().to_ascii_lowercase().as_str() {
            "sfw" => Some(ContentLabel::Sfw),
            "nsfw" => Some(ContentLabel::Nsfw),
            _ => None,
        }
    }
}

/// The three label fields every attempt / lesson / attempt-fix record
/// carries from birth (SPEC §9.1).
#[derive(Debug, Clone, PartialEq)]
pub struct LabelStamp {
    /// Provenance lane of the repo the record was created in.
    pub origin_lane: Lane,
    /// Content label governing visibility.
    pub content_label: ContentLabel,
    /// Evidence for the content label (the default rule's name when the
    /// label came from the fail-closed default).
    pub evidence: String,
}

/// The repo's lane: the repo's own configured lane is the source of
/// truth — the `.ai/lane` stamp file when present, else the `lane=`
/// stamp in the `.ai/roadmap.md` header — and `general` when no lane
/// is configured. (Pre-0.3 this fell back to a path-component name
/// heuristic; that was removed when the lane values went generic —
/// a directory name is not a lane assignment.)
pub fn repo_lane(root: &Path) -> Lane {
    let stamp = read_capped(&root.join(".ai").join("lane"), 4096);
    for line in stamp.lines() {
        let t = line.trim().trim_start_matches("lane:").trim();
        if let Some(lane) = Lane::parse(t) {
            return lane;
        }
    }
    if let Ok(Some(rm)) = roadmap_load(root) {
        if let Some(lane) = Lane::parse(&rm.lane) {
            return lane;
        }
    }
    Lane::General
}

/// The fail-closed default label for a lane (SPEC §9.1): restricted-origin
/// without positive SFW evidence is nsfw; general-origin defaults sfw.
pub fn default_label(origin: Lane) -> LabelStamp {
    match origin {
        Lane::Restricted => LabelStamp {
            origin_lane: origin,
            content_label: ContentLabel::Nsfw,
            evidence: "default: restricted-origin without positive SFW evidence".to_string(),
        },
        Lane::General => LabelStamp {
            origin_lane: origin,
            content_label: ContentLabel::Sfw,
            evidence: "default: general-origin".to_string(),
        },
    }
}

/// Deterministic label decision (the OpenAPPA decide() for labels):
/// a claimed label takes effect only with cited evidence; without
/// evidence the fail-closed default supplies the label. Labels are never
/// changed by this function at transfer time — reclassification is a
/// gated promotion act, not a decide() outcome.
pub fn decide_content_label(
    origin: Lane,
    claimed: Option<ContentLabel>,
    evidence: &str,
) -> LabelStamp {
    let evidence = evidence.trim();
    match (claimed, evidence.is_empty()) {
        (Some(label), false) => LabelStamp {
            origin_lane: origin,
            content_label: label,
            evidence: evidence.to_string(),
        },
        _ => default_label(origin),
    }
}

/// The label stamp for records created in `root` right now.
pub fn repo_label_stamp(root: &Path, claimed: Option<ContentLabel>, evidence: &str) -> LabelStamp {
    decide_content_label(repo_lane(root), claimed, evidence)
}

/// Record scope (SPEC §10.4): the failure → research → lesson loop is an
/// estate-wide agent discipline; roadmap tasks are its first surface.
/// The same signature, dedup, lane-label, and staleness machinery
/// operates identically across all three scopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// A single action (an MCP tool call, a dispatch step, a command).
    /// Volume rule: only failures and their resolving success are
    /// recorded — never every successful action.
    Action,
    /// An agent task (a dispatch, a worker run) in the estate-ops root.
    AgentTask,
    /// A roadmap task in a repo (the original S2/S3 surface).
    RepoTask,
}

impl Scope {
    /// Stable lowercase name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Action => "action",
            Scope::AgentTask => "agent_task",
            Scope::RepoTask => "repo_task",
        }
    }

    /// Parse a scope name (case-insensitive); unknown → `RepoTask`
    /// (the pre-§10.4 default for existing records).
    pub fn parse(s: &str) -> Scope {
        match s.trim().to_ascii_lowercase().as_str() {
            "action" => Scope::Action,
            "agent_task" => Scope::AgentTask,
            _ => Scope::RepoTask,
        }
    }
}

/// Parse `scope:` / `subject:` fields out of a record's opening lines.
/// Defaults: `RepoTask`, and the empty subject (callers fall back to
/// the record's task ID) — the §10.4 default for existing records.
pub fn parse_record_fields(text: &str) -> (Scope, String) {
    let mut scope = Scope::RepoTask;
    let mut subject = String::new();
    for line in text.lines().take(32) {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("scope:") {
            scope = Scope::parse(v);
        } else if let Some(v) = t.strip_prefix("subject:") {
            if subject.is_empty() {
                subject = v.trim().to_string();
            }
        }
    }
    (scope, subject)
}

/// Render a label stamp as frontmatter lines for lesson / inbox records.
pub fn label_frontmatter(stamp: &LabelStamp) -> String {
    format!(
        "---\norigin_lane: {}\ncontent_label: {}\nevidence: {}\n---\n",
        stamp.origin_lane.as_str(),
        stamp.content_label.as_str(),
        stamp.evidence
    )
}

/// Parse a label stamp out of a record's frontmatter. Falls back to the
/// fail-closed default for `origin_fallback` when fields are missing —
/// an unlabelled restricted record therefore reads as nsfw.
pub fn parse_label_frontmatter(text: &str, origin_fallback: Lane) -> LabelStamp {
    let mut origin: Option<Lane> = None;
    let mut label: Option<ContentLabel> = None;
    let mut evidence = String::new();
    for line in text.lines().take(24) {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("origin_lane:") {
            origin = Lane::parse(v);
        } else if let Some(v) = t.strip_prefix("content_label:") {
            label = ContentLabel::parse(v);
        } else if let Some(v) = t.strip_prefix("evidence:") {
            if evidence.is_empty() {
                evidence = v.trim().to_string();
            }
        }
    }
    let origin = origin.unwrap_or(origin_fallback);
    match (label, evidence.is_empty()) {
        (Some(l), false) => LabelStamp {
            origin_lane: origin,
            content_label: l,
            evidence,
        },
        (Some(l), true) if origin == Lane::General => LabelStamp {
            origin_lane: origin,
            content_label: l,
            evidence: "frontmatter label without cited evidence (general-origin)".to_string(),
        },
        _ => default_label(origin),
    }
}

// ---------------------------------------------------------------------------
// Time helpers (no chrono dependency: civil-date conversion, UTC)
// ---------------------------------------------------------------------------

/// Current Unix epoch seconds.
pub fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y_adj = if m <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Format Unix epoch seconds as ISO 8601 UTC (`YYYY-MM-DDTHH:MM:SSZ`).
pub fn iso8601_utc(epoch_secs: i64) -> String {
    let days = epoch_secs.div_euclid(86_400);
    let secs = epoch_secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Parse an ISO 8601 UTC timestamp back to epoch seconds. Strict shape;
/// `None` for anything malformed.
pub fn parse_iso8601_utc(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'Z'
    {
        return None;
    }
    let num = |from: usize, len: usize| -> Option<i64> {
        let mut v: i64 = 0;
        for c in &b[from..from + len] {
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + (c - b'0') as i64;
        }
        Some(v)
    };
    let year = num(0, 4)?;
    let month = num(5, 2)? as u32;
    let day = num(8, 2)? as u32;
    let hour = num(11, 2)?;
    let min = num(14, 2)?;
    let sec = num(17, 2)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + min * 60 + sec)
}

/// Current time as ISO 8601 UTC.
pub fn now_iso() -> String {
    iso8601_utc(now_epoch())
}

/// Human elapsed duration: `45s`, `12m 05s`, `1h 02m`.
pub fn format_elapsed(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{}h {:02}m", h, m)
    } else if m > 0 {
        format!("{}m {:02}s", m, s)
    } else {
        format!("{}s", s)
    }
}

// ---------------------------------------------------------------------------
// Attempts log (S2): append-only JSONL with alphabetical keys + lane labels
// ---------------------------------------------------------------------------

/// One attempt event in `.ai/memory/attempts.jsonl`.
///
/// Key order on disk is alphabetical (SPEC §5), with the §9 label fields
/// added: `content_label`, `label_evidence`, `origin_lane`. The §5
/// `evidence` field keeps its meaning (verify evidence summary on a
/// finish event); the label's evidence travels as `label_evidence`.
#[derive(Debug, Clone, PartialEq)]
pub struct AttemptEvent {
    /// Agent/model that made the attempt.
    pub agent: String,
    /// Self-rated confidence 0–100, when declared.
    pub confidence: Option<u32>,
    /// §9 content label stamped at filing time.
    pub content_label: ContentLabel,
    /// Seconds from start to this event; `None` (null) when there was no
    /// start event — never fabricated.
    pub duration_secs: Option<u64>,
    /// Failure description (fail events only).
    pub error: String,
    /// Event kind: `start`, `finish`, or `fail`.
    pub event: String,
    /// Verify evidence summary (finish events only).
    pub evidence: String,
    /// §9 evidence for the content label.
    pub label_evidence: String,
    /// Free-text note from the agent.
    pub note: String,
    /// §9 origin lane stamped at filing time.
    pub origin_lane: Lane,
    /// Record scope (§10.4): repo_task | agent_task | action.
    pub scope: Scope,
    /// Failure signature (SPEC §10.2; fail events only, empty otherwise):
    /// failing command + error class + normalised first error line.
    pub signature: String,
    /// Subject (§10.4): roadmap task ID, dispatch/task ID, or action
    /// name + target. Defaults to the task ID for repo_task records.
    pub subject: String,
    /// Task ID (`R-###`).
    pub task: String,
    /// ISO 8601 UTC timestamp.
    pub ts: String,
}

impl AttemptEvent {
    /// A new event stamped with a repo's label stamp and the current time.
    pub fn new(task: &str, event: &str, agent: &str, stamp: &LabelStamp) -> AttemptEvent {
        AttemptEvent {
            agent: agent.to_string(),
            confidence: None,
            content_label: stamp.content_label,
            duration_secs: None,
            error: String::new(),
            event: event.to_string(),
            evidence: String::new(),
            label_evidence: stamp.evidence.clone(),
            note: String::new(),
            origin_lane: stamp.origin_lane,
            scope: Scope::RepoTask,
            signature: String::new(),
            subject: task.to_string(),
            task: task.to_string(),
            ts: now_iso(),
        }
    }
}

fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

fn opt_num<T: std::fmt::Display>(v: Option<T>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => "null".to_string(),
    }
}

/// Serialise an attempt event with keys in alphabetical order.
pub fn attempt_to_json(ev: &AttemptEvent) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(12);
    parts.push(format!("\"agent\":{}", json_str(&ev.agent)));
    parts.push(format!("\"confidence\":{}", opt_num(ev.confidence)));
    parts.push(format!(
        "\"content_label\":{}",
        json_str(ev.content_label.as_str())
    ));
    parts.push(format!("\"duration_secs\":{}", opt_num(ev.duration_secs)));
    if ev.event == "fail" {
        parts.push(format!("\"error\":{}", json_str(&ev.error)));
    }
    parts.push(format!("\"event\":{}", json_str(&ev.event)));
    if ev.event == "finish" {
        parts.push(format!("\"evidence\":{}", json_str(&ev.evidence)));
    }
    parts.push(format!(
        "\"label_evidence\":{}",
        json_str(&ev.label_evidence)
    ));
    parts.push(format!("\"note\":{}", json_str(&ev.note)));
    parts.push(format!(
        "\"origin_lane\":{}",
        json_str(ev.origin_lane.as_str())
    ));
    parts.push(format!("\"scope\":{}", json_str(ev.scope.as_str())));
    if ev.event == "fail" {
        // Alphabetical: "sig_version" sorts before "signature"
        // ('_' < 'n'), so it is emitted first.
        parts.push(format!("\"sig_version\":{}", SIGNATURE_VERSION));
        parts.push(format!("\"signature\":{}", json_str(&ev.signature)));
    }
    parts.push(format!("\"subject\":{}", json_str(&ev.subject)));
    parts.push(format!("\"task\":{}", json_str(&ev.task)));
    parts.push(format!("\"ts\":{}", json_str(&ev.ts)));
    format!("{{{}}}", parts.join(","))
}

/// Parse one attempts.jsonl line. Missing label fields fall back to the
/// fail-closed default for the recorded (or general) origin.
pub fn attempt_parse_line(line: &str) -> Option<AttemptEvent> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let get_s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let origin = Lane::parse(&get_s("origin_lane")).unwrap_or(Lane::General);
    let label = ContentLabel::parse(&get_s("content_label"));
    let label_evidence = get_s("label_evidence");
    let stamp = decide_content_label(origin, label, &label_evidence);
    Some(AttemptEvent {
        agent: get_s("agent"),
        confidence: v
            .get("confidence")
            .and_then(|x| x.as_u64())
            .and_then(|n| u32::try_from(n).ok()),
        content_label: stamp.content_label,
        duration_secs: v.get("duration_secs").and_then(|x| x.as_u64()),
        error: get_s("error"),
        event: get_s("event"),
        evidence: get_s("evidence"),
        label_evidence: stamp.evidence,
        note: get_s("note"),
        origin_lane: origin,
        scope: Scope::parse(&get_s("scope")),
        signature: get_s("signature"),
        subject: {
            let s = get_s("subject");
            if s.is_empty() {
                get_s("task")
            } else {
                s
            }
        },
        task: get_s("task"),
        ts: get_s("ts"),
    })
}

/// Path of the attempts log inside a repo.
pub fn attempts_path(root: &Path) -> PathBuf {
    root.join(".ai").join("memory").join("attempts.jsonl")
}

/// Append one event to the attempts log (creating it if needed).
/// Refuses when the log exceeds [`ATTEMPTS_MAX_BYTES`].
pub fn append_attempt(root: &Path, ev: &AttemptEvent) -> Result<()> {
    let path = attempts_path(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > ATTEMPTS_MAX_BYTES {
            return Err(BlochError::GitFailed(format!(
                "attempts_log_too_large: {} bytes exceeds limit {}",
                meta.len(),
                ATTEMPTS_MAX_BYTES
            )));
        }
    }
    let mut fh = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(fh, "{}", attempt_to_json(ev))?;
    Ok(())
}

/// Read all attempt events (malformed lines skipped, count bounded).
pub fn read_attempts(root: &Path) -> Vec<AttemptEvent> {
    let content = match std::fs::read_to_string(attempts_path(root)) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    content
        .lines()
        .take(ATTEMPTS_MAX_EVENTS)
        .filter_map(attempt_parse_line)
        .collect()
}

/// The live (started, not yet finished/failed) attempt on a task.
pub fn live_attempt(events: &[AttemptEvent], task: &str) -> Option<AttemptEvent> {
    let mut live: Option<AttemptEvent> = None;
    for ev in events.iter().filter(|e| e.task == task) {
        match ev.event.as_str() {
            "start" => live = Some(ev.clone()),
            "finish" | "fail" => live = None,
            _ => {}
        }
    }
    live
}

/// All events for one task, in log order.
pub fn attempt_history(events: &[AttemptEvent], task: &str) -> Vec<AttemptEvent> {
    events.iter().filter(|e| e.task == task).cloned().collect()
}

fn event_duration(start: &AttemptEvent) -> Option<u64> {
    let start_epoch = parse_iso8601_utc(&start.ts)?;
    let delta = now_epoch() - start_epoch;
    if delta < 0 {
        None
    } else {
        Some(delta as u64)
    }
}

fn validate_confidence(confidence: Option<u32>) -> Result<()> {
    if let Some(c) = confidence {
        if c > 100 {
            return Err(BlochError::GitFailed(format!(
                "invalid_confidence: {} exceeds 100",
                c
            )));
        }
    }
    Ok(())
}

fn validate_task_id(id: &str) -> Result<()> {
    if parse_task_id(id).is_none() {
        return Err(BlochError::GitFailed(format!(
            "invalid_task_id: '{}' is not an R-<phase><seq> id",
            id
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Verify runner (S2): bounded shell execution, repo-trusted input
// ---------------------------------------------------------------------------

/// Outcome of one verify command.
#[derive(Debug, Clone)]
pub struct VerifyRun {
    /// Process exit code; `None` when killed (timeout) or terminated by a
    /// signal. Only `Some(0)` is a pass.
    pub exit_code: Option<i32>,
    /// Combined stdout+stderr, tail-capped to [`VERIFY_OUTPUT_TAIL`].
    pub output_tail: String,
    /// True when the command was killed at the timeout.
    pub timed_out: bool,
}

fn drain_stream<R: Read + Send + 'static>(mut reader: R) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.len() > VERIFY_OUTPUT_TAIL * 2 {
                        let excess = buf.len() - VERIFY_OUTPUT_TAIL;
                        buf.drain(..excess);
                    }
                }
            }
        }
        buf
    })
}

fn tail_chars(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        s.to_string()
    } else {
        s.chars().skip(count - max_chars).collect()
    }
}

/// Run one verify command via `sh -c` in `root`, bounded by `timeout`.
///
/// Verify commands are repo-authored trusted input (Makefile trust
/// class, SPEC §5) — this is a stated tradeoff, not a hidden one.
/// Output is drained on threads (no pipe deadlock) and tail-capped.
pub fn run_verify_command(command: &str, root: &Path, timeout: Duration) -> VerifyRun {
    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return VerifyRun {
                exit_code: None,
                output_tail: format!("spawn failed: {}", e),
                timed_out: false,
            };
        }
    };
    let out_thread = child.stdout.take().map(drain_stream);
    let err_thread = child.stderr.take().map(drain_stream);
    let started = std::time::Instant::now();
    let mut exit_code: Option<i32> = None;
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit_code = status.code();
                break;
            }
            Ok(None) => {
                if started.elapsed() > timeout {
                    timed_out = true;
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(VERIFY_POLL_MS));
            }
            Err(_) => break,
        }
    }
    let mut combined: Vec<u8> = Vec::new();
    if let Some(h) = out_thread {
        if let Ok(mut v) = h.join() {
            combined.append(&mut v);
        }
    }
    if let Some(h) = err_thread {
        if let Ok(mut v) = h.join() {
            combined.append(&mut v);
        }
    }
    VerifyRun {
        exit_code,
        output_tail: tail_chars(&String::from_utf8_lossy(&combined), VERIFY_OUTPUT_TAIL),
        timed_out,
    }
}

// ---------------------------------------------------------------------------
// Task lifecycle (S2) + learning loop (S3)
// ---------------------------------------------------------------------------

fn file_inbox_best_effort(root: &Path, stamp: &LabelStamp, topic: &str, content: &str) {
    // Best-effort secondary surface: attempts.jsonl is the record of
    // truth; an inbox failure must not lose the attempt outcome.
    let labelled = format!("{}{}", label_frontmatter(stamp), content);
    let _ = memory_inbox(root, "bloch", topic, &labelled);
}

fn confidence_trajectory(events: &[AttemptEvent]) -> String {
    let mut tokens: Vec<String> = Vec::new();
    for ev in events {
        match ev.event.as_str() {
            "start" => tokens.push(
                ev.confidence
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "start".to_string()),
            ),
            "fail" => tokens.push("fail".to_string()),
            "finish" => tokens.push(
                ev.confidence
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "pass".to_string()),
            ),
            _ => {}
        }
    }
    tokens.join(" → ")
}

/// Noise-guard fields (SPEC §10.3) carried by curated-layer records from
/// birth: last-verified date + applies-to version, plus the §10.2
/// disposition slot (empty = unset, never fabricated).
fn noise_guard_fields(applies_to: &str) -> String {
    format!(
        "last_verified: {}\napplies_to: {}\ndisposition: \n",
        &now_iso()[..10],
        applies_to
    )
}

/// `scope:` / `subject:` lines for curated records (§10.4).
fn record_scope_fields(task_id: &str) -> String {
    format!("scope: repo_task\nsubject: {}\n", task_id)
}

fn file_attempt_fail_inbox(
    root: &Path,
    stamp: &LabelStamp,
    task: &RoadmapTask,
    error: &str,
    note: &str,
    applies_to: &str,
) {
    let content = format!(
        "# Attempt failed — {} {}\n\n{}{}\nerror: {}\nnote: {}\n",
        task.id,
        task.title,
        noise_guard_fields(applies_to),
        record_scope_fields(&task.id),
        error,
        note
    );
    file_inbox_best_effort(
        root,
        stamp,
        &format!("attempt-fail-{}", task.id.to_ascii_lowercase()),
        &content,
    );
}

fn file_attempt_fix_inbox(
    root: &Path,
    stamp: &LabelStamp,
    task: &RoadmapTask,
    history: &[AttemptEvent],
    fix_note: &str,
    applies_to: &str,
) {
    let errors: Vec<String> = history
        .iter()
        .filter(|e| e.event == "fail")
        .map(|e| e.error.clone())
        .collect();
    let mut content = format!("# Attempt fix — {} {}\n\n", task.id, task.title);
    content.push_str(&noise_guard_fields(applies_to));
    content.push_str(&record_scope_fields(&task.id));
    content.push('\n');
    content.push_str("failing errors, in order:\n");
    for (i, e) in errors.iter().enumerate() {
        content.push_str(&format!("{}. {}\n", i + 1, e));
    }
    content.push_str(&format!(
        "\nwhat fixed it (successful attempt's note): {}\n",
        if fix_note.trim().is_empty() {
            "(no note recorded)"
        } else {
            fix_note
        }
    ));
    content.push_str(&format!(
        "confidence trajectory: {}\n",
        confidence_trajectory(history)
    ));
    file_inbox_best_effort(
        root,
        stamp,
        &format!("attempt-fix-{}", task.id.to_ascii_lowercase()),
        &content,
    );
}

/// Start an attempt on a task: appends a `start` event and flips the
/// task to `[~]`. One live attempt per task — a second start while one
/// is open is refused (`attempt_already_open`).
pub fn task_start(
    root: &Path,
    id: &str,
    agent: &str,
    confidence: Option<u32>,
    claimed_label: Option<ContentLabel>,
    label_evidence: &str,
) -> Result<String> {
    validate_task_id(id)?;
    validate_confidence(confidence)?;
    let mut rm = roadmap_load_required(root)?;
    let task = roadmap_find(&rm, id)?.clone();
    match task.status {
        TaskStatus::Green => {
            return Err(BlochError::GitFailed(format!("task_already_green: {}", id)))
        }
        TaskStatus::Dropped => return Err(BlochError::GitFailed(format!("task_dropped: {}", id))),
        _ => {}
    }
    let events = read_attempts(root);
    if let Some(live) = live_attempt(&events, id) {
        return Err(BlochError::GitFailed(format!(
            "attempt_already_open: {} was started by {} at {}",
            id, live.agent, live.ts
        )));
    }
    let stamp = repo_label_stamp(root, claimed_label, label_evidence);
    let agent = if agent.trim().is_empty() {
        "agent"
    } else {
        agent
    };
    let mut ev = AttemptEvent::new(id, "start", agent, &stamp);
    ev.confidence = confidence;
    append_attempt(root, &ev)?;
    set_task_status(&mut rm, id, TaskStatus::InProgress)?;
    roadmap_save(root, &rm)?;
    Ok(format!(
        "started {} by {} at {} (lane {}, label {})",
        id,
        agent,
        ev.ts,
        stamp.origin_lane.as_str(),
        stamp.content_label.as_str()
    ))
}

/// Record a voluntary failure: appends a `fail` event (duration from the
/// live start, or null when there was none), files an attempt-fail inbox
/// entry, and reopens the task.
pub fn task_fail(
    root: &Path,
    id: &str,
    error: &str,
    confidence: Option<u32>,
    note: &str,
    claimed_label: Option<ContentLabel>,
    label_evidence: &str,
) -> Result<String> {
    validate_task_id(id)?;
    validate_confidence(confidence)?;
    if error.trim().is_empty() {
        return Err(BlochError::GitFailed(
            "invalid_error: --error must not be empty".to_string(),
        ));
    }
    let mut rm = roadmap_load_required(root)?;
    let task = roadmap_find(&rm, id)?.clone();
    if task.status == TaskStatus::Green {
        return Err(BlochError::GitFailed(format!("task_already_green: {}", id)));
    }
    if task.status == TaskStatus::Dropped {
        return Err(BlochError::GitFailed(format!("task_dropped: {}", id)));
    }
    let events = read_attempts(root);
    let live = live_attempt(&events, id);
    let stamp = repo_label_stamp(root, claimed_label, label_evidence);
    let agent = live
        .as_ref()
        .map(|l| l.agent.clone())
        .unwrap_or_else(|| "unknown".to_string());
    let mut ev = AttemptEvent::new(id, "fail", &agent, &stamp);
    ev.confidence = confidence;
    ev.error = error.to_string();
    ev.note = note.to_string();
    ev.duration_secs = live.as_ref().and_then(event_duration);
    ev.signature = failure_signature("(voluntary fail)", error, "");
    append_attempt(root, &ev)?;
    file_attempt_fail_inbox(root, &stamp, &task, error, note, &rm.baseline);
    // §10.2: an externally-implicating failure also enters the phaedrus
    // research loop (signature-deduped candidate, curated check first).
    let research =
        maybe_file_research_candidate(root, &stamp, &rm, &task, &ev.signature, error, note, "");
    if task.status == TaskStatus::InProgress {
        set_task_status(&mut rm, id, TaskStatus::Open)?;
        roadmap_save(root, &rm)?;
    }
    Ok(format!("failed {}: {}{}", id, error, research.describe()))
}

/// Outcome of [`task_done`]: the gate either passed (task green) or
/// failed with per-command detail (task stays open).
#[derive(Debug, Clone)]
pub struct TaskDoneOutcome {
    /// Evidence summary recorded on the finish event / task.
    pub evidence: String,
    /// `(command, exit code, output tail, timed out)` per failed command.
    pub failing: Vec<(String, Option<i32>, String, bool)>,
    /// True iff every verify command exited 0 and the task went green.
    pub passed: bool,
}

/// Attempt to complete a task with an injected verify runner (the test
/// seam). Production callers use [`task_done`].
///
/// The green gate: every `verify:` command runs, in order, bounded by
/// `timeout_secs` each. All exit 0 → `[x]` + `finish` event (+ an
/// attempt-fix inbox entry when earlier attempts failed). Any failure →
/// the task stays open, a `fail` event and attempt-fail inbox entry are
/// filed. A task with no verify lines is refused (`no_verify_defined`)
/// — including a hand-edited `[x]`, which is never honoured as verified.
#[allow(clippy::too_many_arguments)]
pub fn task_done_with_runner<F>(
    root: &Path,
    id: &str,
    confidence: Option<u32>,
    note: &str,
    claimed_label: Option<ContentLabel>,
    label_evidence: &str,
    timeout_secs: u64,
    runner: F,
) -> Result<TaskDoneOutcome>
where
    F: Fn(&str, &Path, Duration) -> VerifyRun,
{
    validate_task_id(id)?;
    validate_confidence(confidence)?;
    let mut rm = roadmap_load_required(root)?;
    let task = roadmap_find(&rm, id)?.clone();
    if task.status == TaskStatus::Dropped {
        return Err(BlochError::GitFailed(format!("task_dropped: {}", id)));
    }
    if task.verify.is_empty() {
        return Err(BlochError::GitFailed(format!(
            "no_verify_defined: task {} has no verify: lines; it can never go green",
            id
        )));
    }
    if task.status == TaskStatus::Green {
        return Err(BlochError::GitFailed(format!("task_already_green: {}", id)));
    }

    let mut failing: Vec<(String, Option<i32>, String, bool)> = Vec::new();
    for cmd in &task.verify {
        let run = runner(cmd, root, Duration::from_secs(timeout_secs));
        if run.exit_code != Some(0) {
            failing.push((cmd.clone(), run.exit_code, run.output_tail, run.timed_out));
        }
    }

    let events = read_attempts(root);
    let live = live_attempt(&events, id);
    let stamp = repo_label_stamp(root, claimed_label, label_evidence);
    let agent = live
        .as_ref()
        .map(|l| l.agent.clone())
        .unwrap_or_else(|| "unknown".to_string());

    if !failing.is_empty() {
        let (cmd, code, _tail, timed_out) = &failing[0];
        let error = if *timed_out {
            format!("{}: timed out after {}s", cmd, timeout_secs)
        } else {
            match code {
                Some(c) => format!("{}: exit code {}", cmd, c),
                None => format!("{}: terminated by signal", cmd),
            }
        };
        let mut ev = AttemptEvent::new(id, "fail", &agent, &stamp);
        ev.confidence = confidence;
        ev.error = error.clone();
        ev.note = note.to_string();
        ev.duration_secs = live.as_ref().and_then(event_duration);
        ev.signature = failure_signature(cmd, &error, &failing[0].2);
        append_attempt(root, &ev)?;
        file_attempt_fail_inbox(root, &stamp, &task, &error, note, &rm.baseline);
        // §10.2 research loop: the failing command's output tail is the
        // text that can implicate something external.
        let tail = failing[0].2.clone();
        let _research = maybe_file_research_candidate(
            root,
            &stamp,
            &rm,
            &task,
            &ev.signature,
            &error,
            note,
            &tail,
        );
        if task.status == TaskStatus::InProgress {
            set_task_status(&mut rm, id, TaskStatus::Open)?;
            roadmap_save(root, &rm)?;
        }
        return Ok(TaskDoneOutcome {
            evidence: error,
            failing,
            passed: false,
        });
    }

    let evidence = format!("{} verify commands, all exit 0", task.verify.len());
    let mut ev = AttemptEvent::new(id, "finish", &agent, &stamp);
    ev.confidence = confidence;
    ev.evidence = evidence.clone();
    ev.note = note.to_string();
    ev.duration_secs = live.as_ref().and_then(event_duration);
    append_attempt(root, &ev)?;

    let mut history = attempt_history(&events, id);
    history.push(ev.clone());
    if history.iter().any(|e| e.event == "fail") {
        file_attempt_fix_inbox(root, &stamp, &task, &history, note, &rm.baseline);
        resolve_research_candidates(root, &history);
    }

    set_task_status(&mut rm, id, TaskStatus::Green)?;
    let evidence_line = format!("{} at {}", evidence, ev.ts);
    set_task_evidence(&mut rm, id, &evidence_line)?;
    roadmap_save(root, &rm)?;
    Ok(TaskDoneOutcome {
        evidence,
        failing: Vec::new(),
        passed: true,
    })
}

/// Attempt to complete a task using the real bounded verify runner
/// ([`VERIFY_TIMEOUT_SECS`] per command). See [`task_done_with_runner`].
pub fn task_done(
    root: &Path,
    id: &str,
    confidence: Option<u32>,
    note: &str,
    claimed_label: Option<ContentLabel>,
    label_evidence: &str,
) -> Result<TaskDoneOutcome> {
    task_done_with_runner(
        root,
        id,
        confidence,
        note,
        claimed_label,
        label_evidence,
        VERIFY_TIMEOUT_SECS,
        run_verify_command,
    )
}

// ---------------------------------------------------------------------------
// Status, HANDOFF section, lessons, snapshots (S2/S3)
// ---------------------------------------------------------------------------

/// A live attempt as reported by status / `roadmap_state`.
#[derive(Debug, Clone)]
pub struct LiveAttempt {
    /// Agent holding the attempt.
    pub agent: String,
    /// Elapsed seconds since the start event, when its ts parses.
    pub elapsed_secs: Option<u64>,
    /// Start timestamp (ISO 8601 UTC).
    pub started_ts: String,
    /// Task ID.
    pub task: String,
}

/// A roadmap progress rollup for one repo.
#[derive(Debug, Clone)]
pub struct RoadmapStatus {
    /// Current phase: the lowest phase with a non-green, non-dropped task.
    pub current_phase: Option<u32>,
    /// Green task count.
    pub green: usize,
    /// In-progress task count.
    pub in_progress: usize,
    /// Roadmap kind string.
    pub kind: String,
    /// Live attempts (started, unfinished).
    pub live: Vec<LiveAttempt>,
    /// Next open task `(id, title)`.
    pub next_open: Option<(String, String)>,
    /// Open task count.
    pub open: usize,
    /// Percent green of non-dropped tasks.
    pub percent: f64,
    /// `(phase, title, green, total)` per phase (dropped excluded).
    pub phases: Vec<(u32, String, usize, usize)>,
    /// Repo name.
    pub repo_name: String,
    /// Non-dropped task total.
    pub total: usize,
}

/// Compute the roadmap status rollup for a repo.
pub fn roadmap_status(root: &Path) -> Result<RoadmapStatus> {
    let rm = roadmap_load_required(root)?;
    let events = read_attempts(root);
    let mut green = 0usize;
    let mut in_progress = 0usize;
    let mut open = 0usize;
    let mut total = 0usize;
    let mut phase_map: HashMap<u32, (usize, usize)> = HashMap::new();
    let mut next_open: Option<(String, String)> = None;
    let mut current_phase: Option<u32> = None;
    for t in &rm.tasks {
        if t.status == TaskStatus::Dropped {
            continue;
        }
        total += 1;
        let entry = phase_map.entry(t.phase).or_insert((0, 0));
        entry.1 += 1;
        match t.status {
            TaskStatus::Green => {
                green += 1;
                entry.0 += 1;
            }
            TaskStatus::InProgress => in_progress += 1,
            _ => open += 1,
        }
        if t.status != TaskStatus::Green {
            if next_open.is_none() && t.status == TaskStatus::Open {
                next_open = Some((t.id.clone(), t.title.clone()));
            }
            current_phase = Some(match current_phase {
                Some(p) => p.min(t.phase),
                None => t.phase,
            });
        }
    }
    if next_open.is_none() {
        if let Some(t) = rm.tasks.iter().find(|t| t.status == TaskStatus::InProgress) {
            next_open = Some((t.id.clone(), t.title.clone()));
        }
    }
    let mut phase_nums: Vec<u32> = phase_map.keys().copied().collect();
    phase_nums.sort();
    let phases: Vec<(u32, String, usize, usize)> = phase_nums
        .iter()
        .map(|p| {
            let (g, tot) = phase_map[p];
            let title = rm
                .phases
                .iter()
                .find(|(n, _)| n == p)
                .map(|(_, t)| t.clone())
                .unwrap_or_default();
            (*p, title, g, tot)
        })
        .collect();
    let mut live: Vec<LiveAttempt> = Vec::new();
    let mut live_tasks: Vec<String> = Vec::new();
    for t in &rm.tasks {
        if live_attempt(&events, &t.id).is_some() {
            live_tasks.push(t.id.clone());
        }
    }
    live_tasks.sort();
    for task_id in live_tasks {
        if let Some(start) = live_attempt(&events, &task_id) {
            live.push(LiveAttempt {
                agent: start.agent.clone(),
                elapsed_secs: event_duration(&start),
                started_ts: start.ts.clone(),
                task: task_id,
            });
        }
    }
    let percent = if total == 0 {
        0.0
    } else {
        green as f64 * 100.0 / total as f64
    };
    Ok(RoadmapStatus {
        current_phase,
        green,
        in_progress,
        kind: rm.kind.clone(),
        live,
        next_open,
        open,
        percent,
        phases,
        repo_name: rm.repo_name.clone(),
        total,
    })
}

/// The most recent lesson lines from `.ai/memory/lessons.md` (bounded).
pub fn recent_lessons(root: &Path, max: usize) -> Vec<String> {
    let path = root.join(".ai").join("memory").join("lessons.md");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut lines: Vec<String> = content
        .lines()
        .map(|l| l.trim())
        .filter(|l| l.starts_with("- ") || l.starts_with("## "))
        .map(|l| l.to_string())
        .collect();
    if lines.len() > max {
        lines = lines.split_off(lines.len() - max);
    }
    lines
}

/// Render a [`RoadmapStatus`] as the CLI `task status` text.
pub fn render_roadmap_status(st: &RoadmapStatus) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# roadmap status — {} ({})\n",
        st.repo_name, st.kind
    ));
    out.push_str(&format!(
        "progress: {}/{} green ({:.0}%)\n",
        st.green, st.total, st.percent
    ));
    out.push_str("phases:\n");
    for (phase, title, g, tot) in &st.phases {
        out.push_str(&format!(
            "  phase {} — {}: {}/{} green\n",
            phase, title, g, tot
        ));
    }
    match st.current_phase {
        Some(p) => {
            let title = st
                .phases
                .iter()
                .find(|(n, _, _, _)| *n == p)
                .map(|(_, t, _, _)| t.clone())
                .unwrap_or_default();
            out.push_str(&format!("current phase: {} — {}\n", p, title));
        }
        None => out.push_str("current phase: none (all green)\n"),
    }
    match &st.next_open {
        Some((id, title)) => out.push_str(&format!("next open: {} {}\n", id, title)),
        None => out.push_str("next open: none\n"),
    }
    for l in &st.live {
        let elapsed = l
            .elapsed_secs
            .map(format_elapsed)
            .unwrap_or_else(|| "?".to_string());
        out.push_str(&format!(
            "live attempt: {} by {} (started {}, elapsed {})\n",
            l.task, l.agent, l.started_ts, elapsed
        ));
    }
    out
}

/// The HANDOFF ROADMAP section for a repo (empty when no roadmap).
/// Bounded to `budget` characters.
pub fn get_roadmap_section(root: &Path, budget: usize) -> String {
    let st = match roadmap_status(root) {
        Ok(s) => s,
        Err(_) => return String::new(),
    };
    let mut lines: Vec<String> = vec!["## roadmap".to_string()];
    lines.push(format!(
        "progress: {}/{} green ({:.0}%) — kind: {}",
        st.green, st.total, st.percent, st.kind
    ));
    match st.current_phase {
        Some(p) => {
            let title = st
                .phases
                .iter()
                .find(|(n, _, _, _)| *n == p)
                .map(|(_, t, _, _)| t.clone())
                .unwrap_or_default();
            lines.push(format!("current phase: {} — {}", p, title));
        }
        None => lines.push("current phase: none (all green)".to_string()),
    }
    match &st.next_open {
        Some((id, title)) => lines.push(format!("next open: {} {}", id, title)),
        None => lines.push("next open: none".to_string()),
    }
    for l in &st.live {
        let elapsed = l
            .elapsed_secs
            .map(format_elapsed)
            .unwrap_or_else(|| "?".to_string());
        lines.push(format!(
            "live attempt: {} by {}, elapsed {}",
            l.task, l.agent, elapsed
        ));
    }
    let lessons = recent_lessons(root, 3);
    if lessons.is_empty() {
        lines.push("recent lessons: none recorded yet".to_string());
    } else {
        lines.push("recent lessons:".to_string());
        for l in lessons {
            lines.push(format!("  {}", l));
        }
    }
    let out = lines.join("\n");
    out.chars().take(budget).collect()
}

/// The roadmap summary block appended to `bloch-summary.md` by
/// `--update-memory` (SPEC §7). `None` when the repo has no roadmap.
pub fn roadmap_summary_for_map(root: &Path) -> Option<String> {
    let st = roadmap_status(root).ok()?;
    let mut out = String::from("\n## Roadmap\n\n");
    out.push_str(&format!("- kind: {}\n", st.kind));
    out.push_str(&format!(
        "- progress: {}/{} green ({:.0}%)\n",
        st.green, st.total, st.percent
    ));
    match st.current_phase {
        Some(p) => out.push_str(&format!("- current phase: {}\n", p)),
        None => out.push_str("- current phase: none (all green)\n"),
    }
    match &st.next_open {
        Some((id, title)) => out.push_str(&format!("- next open: {} {}\n", id, title)),
        None => out.push_str("- next open: none\n"),
    }
    Some(out)
}

/// Distilled lessons for `bloch task lessons [--export]` (S3): the
/// curated `.ai/memory/lessons.md` when present, else the unpromoted
/// attempt-fix inbox entries, else an explicit none-recorded message.
pub fn task_lessons(root: &Path, export: bool) -> String {
    let lessons_path = root.join(".ai").join("memory").join("lessons.md");
    if let Ok(content) = std::fs::read_to_string(&lessons_path) {
        if !content.trim().is_empty() {
            let body: String = content.chars().take(32_000).collect();
            return if export {
                format!("# bloch lessons — export\n\n{}", body)
            } else {
                body
            };
        }
    }
    let inbox = root.join(".ai").join("memory").join("inbox");
    let mut names: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&inbox) {
        for e in entries.filter_map(|e| e.ok()) {
            let name = e.file_name().to_string_lossy().to_string();
            if name.contains("attempt-fix") {
                names.push(name);
            }
        }
    }
    names.sort();
    if !names.is_empty() {
        let mut out = String::from("# bloch lessons — unpromoted inbox attempt-fix entries\n\n");
        for name in names.iter().take(50) {
            if let Ok(content) = std::fs::read_to_string(inbox.join(name)) {
                let body: String = content.chars().take(8_000).collect();
                out.push_str(&format!("## {}\n\n{}\n\n", name, body.trim()));
            }
        }
        return out;
    }
    "(no lessons recorded yet — a lesson is filed as an attempt-fix inbox entry when a task passes after a failure, and promoted to .ai/memory/lessons.md)".to_string()
}

// ---------------------------------------------------------------------------
// Roadmap snapshot + diff (S2, SPEC §7)
// ---------------------------------------------------------------------------

/// A point-in-time roadmap snapshot stored beside the map cache, used by
/// `--diff` to report roadmap movement. A separate file so the existing
/// `bloch-last.json` format is untouched.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RoadmapSnapshot {
    /// Number of fail events in the attempts log at snapshot time.
    pub fail_events: usize,
    /// Task ID → status name.
    pub tasks: HashMap<String, String>,
}

/// Take the current roadmap snapshot; `None` when no roadmap exists.
pub fn roadmap_snapshot(root: &Path) -> Option<RoadmapSnapshot> {
    let rm = roadmap_load(root).ok()??;
    let events = read_attempts(root);
    let fail_events = events.iter().filter(|e| e.event == "fail").count();
    let tasks = rm
        .tasks
        .iter()
        .map(|t| (t.id.clone(), t.status.as_str().to_string()))
        .collect();
    Some(RoadmapSnapshot { fail_events, tasks })
}

/// Persist the roadmap snapshot to `.ai/memory/bloch-roadmap-cache.json`.
pub fn write_roadmap_cache(root: &Path) -> Result<Option<PathBuf>> {
    let snap = match roadmap_snapshot(root) {
        Some(s) => s,
        None => return Ok(None),
    };
    let mem_dir = root.join(".ai").join("memory");
    std::fs::create_dir_all(&mem_dir)?;
    let path = mem_dir.join("bloch-roadmap-cache.json");
    let json = serde_json::to_string_pretty(&snap)
        .map_err(|e| BlochError::JsonError(format!("roadmap snapshot serialize failed: {}", e)))?;
    atomic_write(&path, &json)?;
    Ok(Some(path))
}

/// The `--diff` roadmap movement section: tasks turned green, newly
/// in-progress, new tasks, and failed attempts since the snapshot.
/// Empty when there is no snapshot or no roadmap.
pub fn roadmap_diff_section(root: &Path) -> String {
    let cache_path = root
        .join(".ai")
        .join("memory")
        .join("bloch-roadmap-cache.json");
    let old: RoadmapSnapshot = match std::fs::read_to_string(&cache_path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
    {
        Some(s) => s,
        None => return String::new(),
    };
    let new = match roadmap_snapshot(root) {
        Some(s) => s,
        None => return String::new(),
    };
    let mut lines: Vec<String> = vec!["## roadmap movement since last map".to_string()];
    let mut turned_green: Vec<&String> = new
        .tasks
        .iter()
        .filter(|(id, st)| {
            st.as_str() == "green" && old.tasks.get(*id).map(|o| o.as_str()) != Some("green")
        })
        .map(|(id, _)| id)
        .collect();
    turned_green.sort();
    for id in &turned_green {
        lines.push(format!("turned green: {}", id));
    }
    let mut new_tasks: Vec<&String> = new
        .tasks
        .keys()
        .filter(|id| !old.tasks.contains_key(*id))
        .collect();
    new_tasks.sort();
    for id in &new_tasks {
        lines.push(format!("new task: {}", id));
    }
    let new_fails = new.fail_events.saturating_sub(old.fail_events);
    if new_fails > 0 {
        lines.push(format!("new failed attempts: {}", new_fails));
    }
    if lines.len() == 1 {
        lines.push("no roadmap movement".to_string());
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

// ---------------------------------------------------------------------------
// Failure → phaedrus research loop (SPEC §10.2) + disposition (§10.2 step 4)
// ---------------------------------------------------------------------------

/// The §10.2 disposition vocabulary for a curated finding/lesson: what
/// the estate does with an upstream fix or prior art. `None` (unset) is
/// the default everywhere — a disposition is recorded when known, never
/// fabricated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Take patterns/design, reimplement in estate idiom.
    Adapt,
    /// Write original from the spec (nothing usable, poisoned licence,
    /// or prior art is a NO-FIT).
    CleanBuild,
    /// Take the fix/patch itself, licence permitting, with attribution.
    Extract,
    /// Behaviour-level only: measurements and spec out, no code.
    ReverseEngineer,
}

impl Disposition {
    /// Stable lowercase name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Disposition::Adapt => "adapt",
            Disposition::CleanBuild => "clean-build",
            Disposition::Extract => "extract",
            Disposition::ReverseEngineer => "reverse-engineer",
        }
    }

    /// Parse a disposition name (case-insensitive).
    pub fn parse(s: &str) -> Option<Disposition> {
        match s.trim().to_ascii_lowercase().as_str() {
            "adapt" => Some(Disposition::Adapt),
            "clean-build" | "cleanbuild" | "clean_build" => Some(Disposition::CleanBuild),
            "extract" => Some(Disposition::Extract),
            "reverse-engineer" | "reverse_engineer" | "reverse engineer" => {
                Some(Disposition::ReverseEngineer)
            }
            _ => None,
        }
    }
}

/// Signature schema version (build plan A): bumped when normalisation
/// changes, so dedup history never silently splits across versions.
/// Exact signatures only — no fuzzy/embedding dedup; a changed
/// normalisation fails loudly as a new signature, by design.
pub const SIGNATURE_VERSION: u32 = 1;

/// Substrings that mark a failure as implicating something external
/// (dependency, upstream tool/library, protocol, hardware — SPEC §10.2).
/// Matched case-insensitively against error + note + verify output tail.
pub const EXTERNAL_SIGNALS: &[&str] = &[
    "upstream",
    "dependency",
    "dependencies",
    "protocol",
    "hardware",
    "linker",
    "cannot find -l",
    "pkg-config",
    "registry",
    "version conflict",
    "incompatible",
    "network",
    "connection refused",
    "dns",
    "cuda",
    "vulkan",
    "gpu",
    "driver",
    "segmentation fault",
    "segfault",
    "library",
    "crates.io",
    "ffi",
    "abi",
];

/// Normalise an error line for signatures: lowercase, digit runs → `#`,
/// whitespace collapsed, bounded length. Volatile numbers (ports, PIDs,
/// counts) must not split one failure class into many signatures.
pub fn normalise_error_line(line: &str) -> String {
    let mut out = String::new();
    let mut in_digits = false;
    let mut last_space = true;
    for ch in line.chars() {
        if ch.is_ascii_digit() {
            if !in_digits {
                out.push('#');
                in_digits = true;
            }
            last_space = false;
        } else if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
            in_digits = false;
        } else {
            out.extend(ch.to_lowercase());
            in_digits = false;
            last_space = false;
        }
        if out.len() >= 160 {
            break;
        }
    }
    out.trim().to_string()
}

fn failure_error_class(error: &str) -> &'static str {
    let lc = error.to_ascii_lowercase();
    if lc.contains("timed out") || lc.contains("timeout") {
        "timeout"
    } else if lc.contains("exit code") {
        "exit"
    } else if lc.contains("panicked") || lc.contains("panic") {
        "panic"
    } else if lc.contains("spawn failed") {
        "spawn"
    } else {
        "error"
    }
}

/// The §10.2 failure signature: failing command + error class +
/// normalised first error line. The first error line is taken from the
/// verify output tail when there is one (the informative line), else
/// from the error text. Deterministic; the dedup key for the research
/// loop and the recurrence signal.
pub fn failure_signature(command: &str, error: &str, output_tail: &str) -> String {
    let first_line = output_tail
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .or_else(|| error.lines().map(|l| l.trim()).find(|l| !l.is_empty()))
        .unwrap_or("");
    let combined = format!("{} {}", error, output_tail);
    format!(
        "{} | {} | {}",
        command.trim(),
        failure_error_class(&combined),
        normalise_error_line(first_line)
    )
}

/// Short stable hash of a signature (FNV-1a, hex) for candidate file
/// names. Not a security primitive — a dedup key.
pub fn signature_hash(signature: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in signature.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:016x}", hash)
}

/// True when the failure implicates something external (SPEC §10.2
/// trigger). Internal-logic failures return false: lessons-only.
pub fn failure_is_external(error: &str, note: &str, output_tail: &str) -> bool {
    let hay = format!("{} {} {}", error, note, output_tail).to_ascii_lowercase();
    EXTERNAL_SIGNALS.iter().any(|sig| hay.contains(sig))
}

/// Query the curated layer first (§10.2 step 2): does `lessons.md` or
/// `patterns.md` already cover this signature's normalised error line?
/// Returns the curated file's name on a hit. (The phaedrus-side
/// `finding_list` half of the check is S4 composition; the local half
/// lives here so the S3 filing hook can run it.)
pub fn curated_known_match(root: &Path, signature: &str) -> Option<String> {
    let normalised = signature.rsplit(" | ").next().unwrap_or("");
    if normalised.len() < 8 {
        return None;
    }
    let probe: String = normalised.chars().take(80).collect();
    for name in ["lessons.md", "patterns.md"] {
        let path = root.join(".ai").join("memory").join(name);
        if let Ok(content) = std::fs::read_to_string(&path) {
            let normalised_content = normalise_error_line(&content.replace('\n', " "));
            if normalised_content.contains(&probe) {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// A research candidate in phaedrus shape (SPEC §10.2 step 3), filed in
/// the repo's memory inbox under a deterministic, signature-derived
/// name — one open candidate per signature; repeats update the
/// recurrence count instead of filing anew.
#[derive(Debug, Clone, PartialEq)]
pub struct ResearchCandidate {
    /// Applies-to version/range (noise guard, §10.3).
    pub applies_to: String,
    /// §10.2 disposition; `None` until a curated finding records one.
    pub disposition: Option<Disposition>,
    /// Last-verified date (noise guard, §10.3).
    pub last_verified: String,
    /// Recurrence count, derived from the attempts log.
    pub recurrence_count: usize,
    /// Record scope (§10.4).
    pub scope: Scope,
    /// The failure signature (dedup key).
    pub signature: String,
    /// Source ref: the attempts.jsonl record that first filed this.
    pub source_ref: String,
    /// Candidate status: `open`, or `resolved ...` once the task is green.
    pub status: String,
    /// Summary: signature + verify command + capped output tail.
    pub summary: String,
    /// System: the repo/component the failure implicates.
    pub system: String,
    /// Roadmap task that produced the failure.
    pub task: String,
}

/// File name of the candidate for a signature, inside the inbox.
pub fn research_candidate_filename(signature: &str) -> String {
    format!("research-candidate-{}.md", signature_hash(signature))
}

/// Path of the candidate for a signature.
pub fn research_candidate_path(root: &Path, signature: &str) -> PathBuf {
    root.join(".ai")
        .join("memory")
        .join("inbox")
        .join(research_candidate_filename(signature))
}

/// Render a research candidate record (labels first, phaedrus shape).
pub fn render_research_candidate(c: &ResearchCandidate, stamp: &LabelStamp) -> String {
    let mut out = label_frontmatter(stamp);
    out.push_str(&format!("# Research candidate — {}\n\n", c.signature));
    out.push_str(&format!("system: {}\n", c.system));
    out.push_str(&format!("sig_version: {}\n", SIGNATURE_VERSION));
    out.push_str(&format!("signature: {}\n", c.signature));
    out.push_str(&format!("scope: {}\n", c.scope.as_str()));
    out.push_str(&format!("subject: {}\n", c.task));
    out.push_str(&format!("source: {}\n", c.source_ref));
    out.push_str(&format!("status: {}\n", c.status));
    out.push_str(&format!("recurrence_count: {}\n", c.recurrence_count));
    out.push_str(&format!("last_verified: {}\n", c.last_verified));
    out.push_str(&format!("applies_to: {}\n", c.applies_to));
    out.push_str(&format!(
        "disposition: {}\n",
        c.disposition.map(|d| d.as_str()).unwrap_or("")
    ));
    out.push_str(&format!("\nsummary: {}\n", c.summary));
    out
}

/// Read one `key: value` field from a candidate record.
pub fn candidate_field(content: &str, key: &str) -> Option<String> {
    for line in content.lines() {
        if let Some(v) = line.trim().strip_prefix(&format!("{}:", key)) {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// What the research-loop hook did with one failure.
#[derive(Debug, Clone, PartialEq)]
pub enum ResearchOutcome {
    /// Failure is internal-logic: lessons-only, no candidate.
    NotExternal,
    /// The signature is already covered by the curated layer.
    Known(String),
    /// A candidate was filed or its recurrence count was updated.
    Filed {
        /// Signature hash (candidate file key).
        hash: String,
        /// Total fail events with this signature, from the log.
        recurrence: usize,
    },
}

impl ResearchOutcome {
    /// Short suffix for the `task fail` message (empty when not external).
    pub fn describe(&self) -> String {
        match self {
            ResearchOutcome::NotExternal => String::new(),
            ResearchOutcome::Known(src) => format!(
                "; known failure (curated match in {}) — attached, no new research filed",
                src
            ),
            ResearchOutcome::Filed { hash, recurrence } => {
                format!("; research candidate {} (recurrence {})", hash, recurrence)
            }
        }
    }
}

/// The §10.2 filing hook, run on every fail event after the event is in
/// the log. Internal failure → [`ResearchOutcome::NotExternal`]. Known
/// signature → attached, nothing filed. New/external → candidate filed
/// (or recurrence refreshed) in the inbox. No live phaedrus calls: the
/// candidate is the filing surface the S4 composition submits.
#[allow(clippy::too_many_arguments)]
pub fn maybe_file_research_candidate(
    root: &Path,
    stamp: &LabelStamp,
    rm: &Roadmap,
    task: &RoadmapTask,
    signature: &str,
    error: &str,
    note: &str,
    output_tail: &str,
) -> ResearchOutcome {
    if !failure_is_external(error, note, output_tail) {
        return ResearchOutcome::NotExternal;
    }
    if let Some(src) = curated_known_match(root, signature) {
        return ResearchOutcome::Known(src);
    }
    let system = if rm.repo_name.is_empty() {
        root.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".to_string())
    } else {
        rm.repo_name.clone()
    };
    file_candidate_record(
        root,
        stamp,
        &system,
        &rm.baseline.clone(),
        &task.id.clone(),
        Scope::RepoTask,
        signature,
        error,
        output_tail,
    )
}

/// Write (or refresh) the candidate file for an external failure whose
/// signature is not yet covered by the curated layer. Shared by the
/// roadmap fail path and the scoped filing API (§10.4).
#[allow(clippy::too_many_arguments)]
pub fn file_candidate_record(
    root: &Path,
    stamp: &LabelStamp,
    system: &str,
    applies_to: &str,
    subject: &str,
    scope: Scope,
    signature: &str,
    error: &str,
    output_tail: &str,
) -> ResearchOutcome {
    let events = read_attempts(root);
    let recurrence = events
        .iter()
        .filter(|e| e.event == "fail" && e.signature == signature)
        .count()
        .max(1);
    let tail = tail_chars(output_tail, 1_000);
    let summary = format!(
        "signature: {} | error: {} | verify output tail: {}",
        signature,
        error,
        if tail.trim().is_empty() {
            "(no verify output)"
        } else {
            tail.trim()
        }
    );
    let candidate = ResearchCandidate {
        applies_to: applies_to.to_string(),
        disposition: None,
        last_verified: now_iso()[..10].to_string(),
        recurrence_count: recurrence,
        scope,
        signature: signature.to_string(),
        source_ref: format!(
            "attempts.jsonl subject {} (fail event; see .ai/memory/attempts.jsonl)",
            subject
        ),
        status: "open".to_string(),
        summary,
        system: system.to_string(),
        task: subject.to_string(),
    };
    let path = research_candidate_path(root, signature);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // The candidate is a filing surface, not the record of truth (the
    // attempts log is); a failed write must not lose the fail outcome.
    let _ = std::fs::write(&path, render_research_candidate(&candidate, stamp));
    ResearchOutcome::Filed {
        hash: signature_hash(signature),
        recurrence,
    }
}

/// Mark candidates resolved when their task goes green (§10.2 step 5:
/// the fix files back as a lesson; the candidate closes with it).
pub fn resolve_research_candidates(root: &Path, history: &[AttemptEvent]) {
    let mut seen: HashSet<String> = HashSet::new();
    for ev in history.iter().filter(|e| e.event == "fail") {
        if ev.signature.is_empty() || !seen.insert(ev.signature.clone()) {
            continue;
        }
        let path = research_candidate_path(root, &ev.signature);
        if let Ok(content) = std::fs::read_to_string(&path) {
            let updated = content.replace(
                "status: open",
                &format!("status: resolved (task green {})", &now_iso()[..10]),
            );
            if updated != content {
                let _ = std::fs::write(&path, updated);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// OPST export (SPEC §9.2, S4/M3): attempts as on-policy trajectory material
// ---------------------------------------------------------------------------

/// File one attempt event for a non-roadmap scope into `root`'s
/// attempts log (the estate-ops root for agent/action records; S4
/// registers it with the server — this is the pre-S4 / server-down
/// filing path, same schema as roadmap attempts).
///
/// Volume discipline (§10.4): for `Scope::Action`, a `start` event is
/// refused (`action_scope_pairs_only`) and a `finish` is recorded ONLY
/// when it resolves a prior failure for the same subject
/// (`action_success_without_failure` otherwise) — never one record per
/// successful action. For `Scope::AgentTask`, start/fail/finish follow
/// the roadmap rules (one live attempt per subject; finish without
/// start records `duration_secs: null`).
///
/// Research trigger (§10.4): an external failure in a non-repo scope
/// files a research candidate when its signature has failed ≥2 times,
/// or on the first failure when `blocks_user_facing` is set; repo_task
/// scope keeps the §10.2 first-external-failure rule.
#[allow(clippy::too_many_arguments)]
pub fn file_scoped_event(
    root: &Path,
    scope: Scope,
    subject: &str,
    event: &str,
    agent: &str,
    error: &str,
    note: &str,
    confidence: Option<u32>,
    claimed_label: Option<ContentLabel>,
    label_evidence: &str,
    blocks_user_facing: bool,
) -> Result<AttemptEvent> {
    if subject.trim().is_empty() || subject.chars().count() > 200 {
        return Err(BlochError::GitFailed(
            "invalid_subject: subject must be 1..=200 chars".to_string(),
        ));
    }
    if !matches!(event, "start" | "fail" | "finish") {
        return Err(BlochError::GitFailed(format!(
            "invalid_event: {} (want start|fail|finish)",
            event
        )));
    }
    validate_confidence(confidence)?;
    let events = read_attempts(root);
    match scope {
        Scope::Action => {
            if event == "start" {
                return Err(BlochError::GitFailed(
                    "action_scope_pairs_only: action records are failures and their resolving success"
                        .to_string(),
                ));
            }
            if event == "finish" {
                // The resolving success: a fail for this subject with no
                // finish after it.
                let mut open_fail = false;
                for e in events.iter().filter(|e| e.subject == subject) {
                    match e.event.as_str() {
                        "fail" => open_fail = true,
                        "finish" => open_fail = false,
                        _ => {}
                    }
                }
                if !open_fail {
                    return Err(BlochError::GitFailed(format!(
                        "action_success_without_failure: no open failure for action {}",
                        subject
                    )));
                }
            }
        }
        Scope::AgentTask => {
            if event == "start" && live_attempt(&events, subject).is_some() {
                return Err(BlochError::GitFailed(format!(
                    "attempt_already_open: {}",
                    subject
                )));
            }
        }
        Scope::RepoTask => {}
    }
    let stamp = repo_label_stamp(root, claimed_label, label_evidence);
    let mut ev = AttemptEvent::new(subject, event, agent, &stamp);
    ev.scope = scope;
    ev.subject = subject.to_string();
    ev.confidence = confidence;
    ev.note = note.to_string();
    if event == "fail" {
        ev.error = error.to_string();
        ev.signature = failure_signature(subject, error, "");
    }
    if matches!(event, "fail" | "finish") {
        if let Some(start) = live_attempt(&events, subject) {
            ev.duration_secs = event_duration(&start);
        }
    }
    append_attempt(root, &ev)?;
    if event == "fail" && failure_is_external(error, note, "") {
        let after = read_attempts(root);
        let recurrence = after
            .iter()
            .filter(|e| e.event == "fail" && e.signature == ev.signature)
            .count();
        let trigger = scope == Scope::RepoTask || recurrence >= 2 || blocks_user_facing;
        if trigger && curated_known_match(root, &ev.signature).is_none() {
            let system = root
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "estate-ops".to_string());
            file_candidate_record(
                root,
                &stamp,
                &system,
                "unversioned",
                subject,
                scope,
                &ev.signature,
                error,
                "",
            );
        }
    }
    Ok(ev)
}

/// Export the roadmap's attempts as OPST material: one JSON object per
/// task (descriptor, attempts in order, verified gate outcome from the
/// green gate, confidence trajectory, time-to-green). Read-only.
///
/// The verified outcome is the deterministic oracle label OPST's
/// pseudo-verification lacks: the gate actually ran the verify commands.
pub fn task_export_opst(root: &Path) -> Result<String> {
    let rm = roadmap_load_required(root)?;
    let events = read_attempts(root);
    let mut out = String::new();
    for task in &rm.tasks {
        let history = attempt_history(&events, &task.id);
        let trajectory: Vec<serde_json::Value> = history
            .iter()
            .filter_map(|e| e.confidence)
            .map(|c| serde_json::json!(c))
            .collect();
        let outcome = match task.status {
            TaskStatus::Green => "green",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::Dropped => "dropped",
            TaskStatus::Open => "open",
        };
        let time_to_green: Option<u64> = if task.status == TaskStatus::Green {
            let mut sum: u64 = 0;
            let mut any = false;
            for e in &history {
                if let Some(d) = e.duration_secs {
                    sum += d;
                    any = true;
                }
                if e.event == "finish" {
                    break;
                }
            }
            if any {
                Some(sum)
            } else {
                None
            }
        } else {
            None
        };
        let attempts_json: Vec<serde_json::Value> = history
            .iter()
            .map(|e| serde_json::from_str(&attempt_to_json(e)).unwrap_or(serde_json::Value::Null))
            .collect();
        let record = serde_json::json!({
            "attempts": attempts_json,
            "confidence_trajectory": trajectory,
            "kind": rm.kind,
            "lane": repo_lane(root).as_str(),
            "scope": "repo_task",
            "subject": task.id,
            "task": {
                "id": task.id,
                "phase": task.phase,
                "title": task.title,
                "verify": task.verify,
            },
            "time_to_green_secs": time_to_green,
            "verified_outcome": outcome,
        });
        out.push_str(&serde_json::to_string(&record).unwrap_or_default());
        out.push('\n');
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests: ~50/50 validation / adversarial (repo convention)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bloch-roadmap-test-{}-{}",
            name,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_roadmap_file(root: &Path, content: &str) {
        let ai = root.join(".ai");
        fs::create_dir_all(ai.join("memory")).unwrap();
        fs::write(ai.join("roadmap.md"), content).unwrap();
    }

    const SIMPLE_ROADMAP: &str = "\
# Roadmap — demo (app) — v0.1 → v1.0
<!-- bloch:roadmap schema=1 kind=app lane=general baseline=0.1.0 target=1.0.0 -->

## Phase 1 — Baseline truth
- [ ] R-101 Build it
  - verify: true
  - evidence:
- [ ] R-102 No verify task
  - evidence:
";

    // --- validation -------------------------------------------------------

    #[test]
    fn detect_mcp_from_cargo() {
        let root = tmp_root("detect-mcp");
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.2.0\"\n\n[dependencies]\nrmcp = \"0.8\"\n",
        )
        .unwrap();
        let d = detect_repo_kind(&root);
        assert_eq!(d.kind, RepoKind::Mcp);
        assert_eq!(d.baseline, "0.2.0");
        assert!(
            d.evidence.iter().any(|e| e.contains("MCP SDK")),
            "{:?}",
            d.evidence
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_game_bevy() {
        let root = tmp_root("detect-game");
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"g\"\nversion = \"0.1.0\"\n\n[dependencies]\nbevy = \"0.14\"\n",
        )
        .unwrap();
        let d = detect_repo_kind(&root);
        assert_eq!(d.kind, RepoKind::Game);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_tutorial_layout() {
        let root = tmp_root("detect-tut");
        fs::create_dir_all(root.join("exercises")).unwrap();
        fs::create_dir_all(root.join("solutions")).unwrap();
        fs::write(root.join("exercises").join("01.md"), "# ex").unwrap();
        fs::write(root.join("solutions").join("01.md"), "# sol").unwrap();
        fs::write(root.join("README.md"), "# tut").unwrap();
        let d = detect_repo_kind(&root);
        assert_eq!(d.kind, RepoKind::Tutorial);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_education_mdbook() {
        let root = tmp_root("detect-edu");
        fs::write(root.join("book.toml"), "[book]\ntitle = \"Course\"\n").unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src").join("chapter.md"), "# ch").unwrap();
        let d = detect_repo_kind(&root);
        assert_eq!(d.kind, RepoKind::Education);
        assert_eq!(d.stack, RepoStack::MdBook);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn init_mcp_writes_roadmap_and_never_overwrites() {
        let root = tmp_root("init-mcp");
        let outcome = bloch_init(&root, Some(RepoKind::Mcp)).unwrap();
        assert!(outcome.roadmap_written);
        assert!(!outcome.refused);
        let path = roadmap_path(&root);
        assert!(path.exists());
        assert!(root.join(".ai").join("lane").exists());
        assert!(root.join(".ai").join("memory").join("inbox").is_dir());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("kind=mcp"));
        assert!(content.contains("lane=general"));
        assert!(content.contains("R-101"));
        let rm = roadmap_parse(&content).unwrap();
        assert_eq!(rm.lane, "general");
        assert!(rm.tasks.len() >= 15, "got {}", rm.tasks.len());
        assert!(rm.tasks.iter().all(|t| !t.verify.is_empty()));

        // Second init must not overwrite an edited roadmap.
        let edited = format!("{}<!-- agent edit -->\n", content);
        fs::write(&path, &edited).unwrap();
        let second = bloch_init(&root, Some(RepoKind::Mcp)).unwrap();
        assert!(!second.roadmap_written);
        assert_eq!(fs::read_to_string(&path).unwrap(), edited);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn templates_roundtrip_all_kinds() {
        for kind in [
            RepoKind::Mcp,
            RepoKind::Game,
            RepoKind::App,
            RepoKind::Education,
            RepoKind::Tutorial,
            RepoKind::Unclassified,
        ] {
            let text =
                roadmap_render_template("demo", kind, "0.1.0", RepoStack::Cargo, Lane::General);
            let rm = roadmap_parse(&text).unwrap();
            assert_eq!(rm.kind, kind.as_str());
            assert!(
                rm.tasks.iter().all(|t| !t.verify.is_empty()),
                "kind {}",
                kind.as_str()
            );
            let mut ids: Vec<&str> = rm.tasks.iter().map(|t| t.id.as_str()).collect();
            let n = ids.len();
            ids.sort();
            ids.dedup();
            assert_eq!(ids.len(), n, "duplicate ids in {}", kind.as_str());
            let rendered = roadmap_render(&rm);
            let rm2 = roadmap_parse(&rendered).unwrap();
            assert_eq!(
                roadmap_render(&rm2),
                rendered,
                "render not idempotent for {}",
                kind.as_str()
            );
            assert_eq!(rm2.tasks.len(), rm.tasks.len());
            if kind == RepoKind::Unclassified {
                assert!(
                    rm.tasks[0].title.to_ascii_lowercase().contains("classify"),
                    "unclassified first task must be classification"
                );
            }
        }
    }

    #[test]
    fn iso8601_roundtrip() {
        for epoch in [0i64, 86_400, 1_700_000_000, 1_793_000_000] {
            let s = iso8601_utc(epoch);
            assert_eq!(parse_iso8601_utc(&s), Some(epoch), "ts {}", s);
        }
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00Z");
        assert!(parse_iso8601_utc("2026-13-01T00:00:00Z").is_none());
        assert!(parse_iso8601_utc("not a date").is_none());
    }

    #[test]
    fn task_lifecycle_green_with_real_verify() {
        let root = tmp_root("lifecycle");
        write_roadmap_file(&root, SIMPLE_ROADMAP);
        let msg = task_start(&root, "R-101", "tester", Some(80), None, "").unwrap();
        assert!(msg.contains("started R-101"));
        let rm = roadmap_load(&root).unwrap().unwrap();
        assert_eq!(rm.tasks[0].status, TaskStatus::InProgress);

        let outcome = task_done(&root, "R-101", Some(90), "built it", None, "").unwrap();
        assert!(outcome.passed, "{:?}", outcome.failing);
        let rm = roadmap_load(&root).unwrap().unwrap();
        assert_eq!(rm.tasks[0].status, TaskStatus::Green);
        assert!(rm.tasks[0].evidence.contains("1 verify commands"));

        let events = read_attempts(&root);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, "start");
        assert_eq!(events[1].event, "finish");
        assert_eq!(events[1].agent, "tester");
        assert!(events[1].duration_secs.is_some());
        assert_eq!(events[1].origin_lane, Lane::General);
        assert_eq!(events[1].content_label, ContentLabel::Sfw);

        // Alphabetical keys on disk.
        let raw = fs::read_to_string(attempts_path(&root)).unwrap();
        let first = raw.lines().next().unwrap();
        let agent_pos = first.find("\"agent\"").unwrap();
        let conf_pos = first.find("\"confidence\"").unwrap();
        let label_pos = first.find("\"content_label\"").unwrap();
        let task_pos = first.find("\"task\"").unwrap();
        assert!(agent_pos < conf_pos && conf_pos < label_pos && label_pos < task_pos);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn task_status_and_handoff_section() {
        let root = tmp_root("status");
        write_roadmap_file(&root, SIMPLE_ROADMAP);
        task_start(&root, "R-101", "tester", Some(50), None, "").unwrap();
        task_done(&root, "R-101", Some(60), "ok", None, "").unwrap();
        task_start(&root, "R-102", "tester2", None, None, "").unwrap();
        let st = roadmap_status(&root).unwrap();
        assert_eq!(st.green, 1);
        assert_eq!(st.total, 2);
        assert_eq!(st.percent, 50.0);
        assert_eq!(st.live.len(), 1);
        assert_eq!(st.live[0].agent, "tester2");
        let section = get_roadmap_section(&root, 1200);
        assert!(section.contains("## roadmap"));
        assert!(section.contains("1/2 green (50%)"));
        assert!(section.contains("live attempt: R-102 by tester2"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn fail_then_pass_files_attempt_fix_and_promotes_to_lessons() {
        let root = tmp_root("learning");
        let roadmap = SIMPLE_ROADMAP.replace("verify: true", "verify: test -f marker.txt");
        write_roadmap_file(&root, &roadmap);
        task_start(&root, "R-101", "worker", Some(40), None, "").unwrap();
        let failed = task_done(&root, "R-101", Some(40), "first try", None, "").unwrap();
        assert!(!failed.passed);
        // attempt-fail inbox entry exists.
        let inbox = root.join(".ai").join("memory").join("inbox");
        let names: Vec<String> = fs::read_dir(&inbox)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
            .collect();
        assert!(
            names.iter().any(|n| n.contains("attempt-fail-r-101")),
            "{:?}",
            names
        );

        fs::write(root.join("marker.txt"), "ok").unwrap();
        task_start(&root, "R-101", "worker", Some(65), None, "").unwrap();
        let passed = task_done(&root, "R-101", Some(65), "created marker.txt", None, "").unwrap();
        assert!(passed.passed);
        let names: Vec<String> = fs::read_dir(&inbox)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
            .collect();
        let fix_name = names
            .iter()
            .find(|n| n.contains("attempt-fix-r-101"))
            .expect("attempt-fix entry filed");
        let fix_content = fs::read_to_string(inbox.join(fix_name)).unwrap();
        assert!(fix_content.contains("origin_lane: general"));
        assert!(fix_content.contains("content_label: sfw"));
        assert!(fix_content.contains("scope: repo_task"), "{}", fix_content);
        assert!(fix_content.contains("subject: R-101"), "{}", fix_content);
        assert!(fix_content.contains("created marker.txt"));
        assert!(fix_content.contains("40"), "trajectory: {}", fix_content);
        assert!(fix_content.contains("fail"));
        assert!(fix_content.contains("65"));

        // Promote through the existing flow into lessons.md (S3 target).
        let msg = crate::memory_promote(&root, fix_name, "lessons.md").unwrap();
        assert!(msg.contains("lessons.md"));
        let lessons = task_lessons(&root, true);
        assert!(lessons.contains("Attempt fix"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn opst_export_shape() {
        let root = tmp_root("opst");
        write_roadmap_file(&root, SIMPLE_ROADMAP);
        task_start(&root, "R-101", "worker", Some(70), None, "").unwrap();
        task_done(&root, "R-101", Some(85), "done", None, "").unwrap();
        let out = task_export_opst(&root).unwrap();
        let first: serde_json::Value = serde_json::from_str(out.lines().next().unwrap()).unwrap();
        assert_eq!(first["verified_outcome"], "green");
        assert_eq!(first["scope"], "repo_task");
        assert_eq!(first["subject"], "R-101");
        assert_eq!(first["task"]["id"], "R-101");
        assert_eq!(first["confidence_trajectory"], serde_json::json!([70, 85]));
        assert!(first["attempts"].as_array().unwrap().len() == 2);
        assert!(first["time_to_green_secs"].is_number());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn verify_output_tail_is_capped() {
        let root = tmp_root("tailcap");
        let run = run_verify_command(
            "for i in $(seq 1 5000); do echo line$i; done; exit 3",
            &root,
            Duration::from_secs(30),
        );
        assert_eq!(run.exit_code, Some(3));
        assert!(run.output_tail.chars().count() <= VERIFY_OUTPUT_TAIL);
        assert!(run.output_tail.contains("line5000"));
        let _ = fs::remove_dir_all(&root);
    }

    // --- adversarial ------------------------------------------------------

    #[test]
    fn init_refuses_dotfiles() {
        let root = tmp_root("init-dotfiles");
        fs::write(root.join(".bashrc"), "# rc").unwrap();
        fs::write(root.join(".zshrc"), "# rc").unwrap();
        fs::write(root.join("notes.txt"), "hi").unwrap();
        let outcome = bloch_init(&root, None).unwrap();
        assert!(outcome.refused, "{:?}", outcome);
        assert_eq!(outcome.kind, RepoKind::Dotfiles);
        assert!(outcome.message.contains("roadmap_refused"));
        assert!(!roadmap_path(&root).exists());
        // Memory-only init still happened.
        assert!(root.join(".ai").join("memory").is_dir());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn init_ambiguous_tie_is_unclassified() {
        let root = tmp_root("init-ambig");
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n\n[dependencies]\nbevy = \"0.14\"\nrmcp = \"0.8\"\n",
        )
        .unwrap();
        let d = detect_repo_kind(&root);
        assert_eq!(d.kind, RepoKind::Unclassified, "evidence: {:?}", d.evidence);
        let outcome = bloch_init(&root, None).unwrap();
        assert!(outcome.roadmap_written);
        let rm = roadmap_load(&root).unwrap().unwrap();
        assert_eq!(rm.kind, "unclassified");
        assert!(rm.tasks[0].title.to_ascii_lowercase().contains("classify"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn done_no_verify_refused_and_hand_edited_green_not_honoured() {
        let root = tmp_root("noverify");
        write_roadmap_file(&root, SIMPLE_ROADMAP);
        let err = task_done(&root, "R-102", None, "", None, "").unwrap_err();
        assert!(format!("{}", err).contains("no_verify_defined"), "{}", err);

        // Hand-edit R-102 to [x]: bloch must still refuse (no verify lines),
        // never treat the hand edit as a verified green.
        let content = fs::read_to_string(roadmap_path(&root)).unwrap();
        fs::write(
            roadmap_path(&root),
            content.replace("- [ ] R-102", "- [x] R-102"),
        )
        .unwrap();
        let err = task_done(&root, "R-102", None, "", None, "").unwrap_err();
        assert!(format!("{}", err).contains("no_verify_defined"), "{}", err);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn done_failing_verify_never_flips() {
        let root = tmp_root("failgate");
        let roadmap = SIMPLE_ROADMAP.replace("verify: true", "verify: false");
        write_roadmap_file(&root, &roadmap);
        task_start(&root, "R-101", "worker", Some(90), None, "").unwrap();
        let outcome = task_done(&root, "R-101", Some(90), "surely done", None, "").unwrap();
        assert!(!outcome.passed);
        assert_eq!(outcome.failing.len(), 1);
        assert_eq!(outcome.failing[0].1, Some(1));
        let rm = roadmap_load(&root).unwrap().unwrap();
        assert_eq!(rm.tasks[0].status, TaskStatus::Open);
        let events = read_attempts(&root);
        let fail = events
            .iter()
            .find(|e| e.event == "fail")
            .expect("fail event");
        assert!(fail.error.contains("exit code 1"), "{}", fail.error);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn double_start_refused() {
        let root = tmp_root("doublestart");
        write_roadmap_file(&root, SIMPLE_ROADMAP);
        task_start(&root, "R-101", "a", None, None, "").unwrap();
        let err = task_start(&root, "R-101", "b", None, None, "").unwrap_err();
        assert!(
            format!("{}", err).contains("attempt_already_open"),
            "{}",
            err
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn finish_without_start_duration_null() {
        let root = tmp_root("nostart");
        write_roadmap_file(&root, SIMPLE_ROADMAP);
        let outcome = task_done(&root, "R-101", Some(55), "no start", None, "").unwrap();
        assert!(outcome.passed);
        let raw = fs::read_to_string(attempts_path(&root)).unwrap();
        assert!(raw.contains("\"duration_secs\":null"), "{}", raw);
        let events = read_attempts(&root);
        assert_eq!(events[0].agent, "unknown");
        assert_eq!(events[0].duration_secs, None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn duplicate_id_renumbering_attack_refused() {
        let dup = SIMPLE_ROADMAP.replace("R-102 No verify task", "R-101 Duplicate task");
        let err = roadmap_parse(&dup).unwrap_err();
        assert!(format!("{}", err).contains("duplicate_task_id"), "{}", err);
    }

    #[test]
    fn oversized_roadmap_refused() {
        let big = "x".repeat((ROADMAP_MAX_BYTES + 1) as usize);
        let err = roadmap_parse(&big).unwrap_err();
        assert!(format!("{}", err).contains("roadmap_too_large"), "{}", err);
    }

    #[test]
    fn malformed_lines_preserved_verbatim() {
        let weird = "# Roadmap — demo (app) — v0.1 → v1.0\n<!-- bloch:roadmap schema=1 kind=app baseline=0.1.0 target=1.0.0 -->\n\nsome random prose\n- not a task\n- [ ] R-101 Real task\n  - verify: true\n  - evidence:\n- [?] bogus checkbox\ntrailing junk\n";
        let rm = roadmap_parse(weird).unwrap();
        assert_eq!(rm.tasks.len(), 1);
        let rendered = roadmap_render(&rm);
        assert!(rendered.contains("some random prose"));
        assert!(rendered.contains("- [?] bogus checkbox"));
        assert!(rendered.contains("trailing junk"));
    }

    #[test]
    fn invalid_confidence_and_unknown_task_refused() {
        let root = tmp_root("badinput");
        write_roadmap_file(&root, SIMPLE_ROADMAP);
        let err = task_start(&root, "R-101", "a", Some(101), None, "").unwrap_err();
        assert!(format!("{}", err).contains("invalid_confidence"), "{}", err);
        let err = task_start(&root, "R-999", "a", None, None, "").unwrap_err();
        assert!(format!("{}", err).contains("task_not_found"), "{}", err);
        let err = task_start(&root, "X-1", "a", None, None, "").unwrap_err();
        assert!(format!("{}", err).contains("invalid_task_id"), "{}", err);
        let err = task_fail(&root, "R-101", "", None, "", None, "").unwrap_err();
        assert!(format!("{}", err).contains("invalid_error"), "{}", err);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn verify_timeout_recorded_as_failure() {
        let root = tmp_root("timeout");
        write_roadmap_file(&root, SIMPLE_ROADMAP);
        task_start(&root, "R-101", "worker", None, None, "").unwrap();
        let outcome = task_done_with_runner(
            &root,
            "R-101",
            None,
            "",
            None,
            "",
            600,
            |_cmd, _root, _timeout| VerifyRun {
                exit_code: None,
                output_tail: String::new(),
                timed_out: true,
            },
        )
        .unwrap();
        assert!(!outcome.passed);
        assert!(outcome.failing[0].3);
        let events = read_attempts(&root);
        let fail = events.iter().find(|e| e.event == "fail").unwrap();
        assert!(fail.error.contains("timed out"), "{}", fail.error);
        let rm = roadmap_load(&root).unwrap().unwrap();
        assert_ne!(rm.tasks[0].status, TaskStatus::Green);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn verify_real_timeout_kills_long_command() {
        let root = tmp_root("realtimeout");
        let run = run_verify_command("sleep 5", &root, Duration::from_millis(200));
        assert!(run.timed_out);
        assert_eq!(run.exit_code, None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn dropped_task_preserved_and_ops_refused() {
        let root = tmp_root("dropped");
        let roadmap = SIMPLE_ROADMAP.replace(
            "- [ ] R-102 No verify task",
            "- [ ] ~~R-102 No verify task~~",
        );
        write_roadmap_file(&root, &roadmap);
        let rm = roadmap_load(&root).unwrap().unwrap();
        assert_eq!(rm.tasks[1].status, TaskStatus::Dropped);
        let rendered = roadmap_render(&rm);
        assert!(
            rendered.contains("~~R-102"),
            "dropped line preserved: {}",
            rendered
        );
        let err = task_start(&root, "R-102", "a", None, None, "").unwrap_err();
        assert!(format!("{}", err).contains("task_dropped"), "{}", err);
        let st = roadmap_status(&root).unwrap();
        assert_eq!(st.total, 1, "dropped tasks leave the denominator");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn lane_defaults_fail_closed() {
        // restricted-origin: no evidence -> nsfw; claim without evidence
        // still nsfw; claim WITH evidence takes effect.
        let d = decide_content_label(Lane::Restricted, None, "");
        assert_eq!(d.content_label, ContentLabel::Nsfw);
        let d = decide_content_label(Lane::Restricted, Some(ContentLabel::Sfw), "");
        assert_eq!(
            d.content_label,
            ContentLabel::Nsfw,
            "unevidenced sfw claim must fail closed"
        );
        let d = decide_content_label(
            Lane::Restricted,
            Some(ContentLabel::Sfw),
            "lesson text is a rust build fix; reviewed 2026-10-27",
        );
        assert_eq!(d.content_label, ContentLabel::Sfw);
        assert_eq!(
            d.origin_lane,
            Lane::Restricted,
            "origin is provenance, never relabeled"
        );
        let d = decide_content_label(Lane::General, None, "");
        assert_eq!(d.content_label, ContentLabel::Sfw);

        // Repo-level: the repo's configured lane is the source of
        // truth. A repo whose roadmap header stamps `lane=restricted`
        // stamps restricted + nsfw on its records.
        let base = tmp_root("lane");
        let root = base.join("somerepo");
        fs::create_dir_all(&root).unwrap();
        let restricted = SIMPLE_ROADMAP.replace("lane=general", "lane=restricted");
        write_roadmap_file(&root, &restricted);
        assert_eq!(repo_lane(&root), Lane::Restricted);
        task_start(&root, "R-101", "worker", None, None, "").unwrap();
        let events = read_attempts(&root);
        assert_eq!(events[0].origin_lane, Lane::Restricted);
        assert_eq!(events[0].content_label, ContentLabel::Nsfw);

        // No configured lane defaults to general — a suggestive
        // directory name is not a lane assignment (the pre-0.3 path
        // heuristic is gone).
        let unconfigured = base.join("NTRanch").join("otherrepo");
        fs::create_dir_all(&unconfigured).unwrap();
        assert_eq!(repo_lane(&unconfigured), Lane::General);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn legacy_lane_names_parse_as_aliases() {
        // Back-compat (pre-0.3 records): the legacy estate lane names
        // still parse, mapping onto the generic lanes — and everything
        // written back out is generic.
        assert_eq!(Lane::parse("qompassai"), Some(Lane::General));
        assert_eq!(Lane::parse("NTRanch"), Some(Lane::Restricted));
        assert_eq!(Lane::General.as_str(), "general");
        assert_eq!(Lane::Restricted.as_str(), "restricted");

        // A legacy roadmap header stamp normalises on parse.
        let legacy = SIMPLE_ROADMAP.replace("lane=general", "lane=qompassai");
        let rm = roadmap_parse(&legacy).unwrap();
        assert_eq!(rm.lane, "general");

        // A legacy attempt line keeps its origin through the alias.
        let line = "{\"agent\":\"w\",\"content_label\":\"nsfw\",\"event\":\"fail\",\"origin_lane\":\"ntranch\",\"task\":\"R-101\",\"ts\":\"2026-10-27T00:00:00Z\"}";
        let ev = attempt_parse_line(line).unwrap();
        assert_eq!(ev.origin_lane, Lane::Restricted);
        assert_eq!(ev.content_label, ContentLabel::Nsfw);
    }

    // --- §10: failure → research loop -------------------------------------

    fn external_fail_run(_cmd: &str, _root: &Path, _timeout: Duration) -> VerifyRun {
        VerifyRun {
            exit_code: Some(1),
            output_tail: "error: linker `cc` failed: cannot find -lcuda (port 54321, pid 9911)"
                .to_string(),
            timed_out: false,
        }
    }

    #[test]
    fn signature_normalisation_ignores_volatile_numbers() {
        let a = failure_signature(
            "cargo build",
            "cargo build: exit code 1",
            "error on port 54321, pid 9911",
        );
        let b = failure_signature(
            "cargo build",
            "cargo build: exit code 1",
            "error on port 10000, pid 42",
        );
        assert_eq!(a, b, "volatile numbers must not split a signature");
        let c = failure_signature("cargo build", "error: a completely different failure", "");
        assert_ne!(a, c);
        assert!(a.contains("exit"), "class recorded: {}", a);
    }

    #[test]
    fn external_fail_files_research_candidate() {
        let root = tmp_root("research");
        write_roadmap_file(&root, SIMPLE_ROADMAP);
        task_start(&root, "R-101", "worker", None, None, "").unwrap();
        let outcome =
            task_done_with_runner(&root, "R-101", None, "", None, "", 600, external_fail_run)
                .unwrap();
        assert!(!outcome.passed);
        let events = read_attempts(&root);
        let fail = events.iter().find(|e| e.event == "fail").unwrap();
        assert!(!fail.signature.is_empty(), "fail events carry a signature");
        assert!(fail.signature.contains("linker") || fail.signature.contains("exit"));
        let path = research_candidate_path(&root, &fail.signature);
        assert!(path.exists(), "candidate filed at {}", path.display());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("sig_version: 1"), "{}", content);
        assert!(content.contains("origin_lane: general"), "{}", content);
        assert!(content.contains("recurrence_count: 1"), "{}", content);
        assert!(content.contains("status: open"));
        assert!(
            content.contains("disposition: \n")
                || content.contains("disposition:\n")
                || content.contains("disposition: "),
            "disposition slot present, unset: {}",
            content
        );
        assert!(content.contains("last_verified: 20"), "{}", content);
        assert!(content.contains("applies_to: 0.1.0"), "{}", content);
        assert!(
            content.contains("attempts.jsonl"),
            "source ref: {}",
            content
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn internal_fail_is_lessons_only_no_candidate() {
        let root = tmp_root("internal");
        write_roadmap_file(&root, SIMPLE_ROADMAP);
        task_start(&root, "R-101", "worker", None, None, "").unwrap();
        let outcome = task_done_with_runner(
            &root,
            "R-101",
            None,
            "",
            None,
            "",
            600,
            |_cmd, _root, _t| VerifyRun {
                exit_code: Some(1),
                output_tail: "assertion failed: our parser misread the fixture".to_string(),
                timed_out: false,
            },
        )
        .unwrap();
        assert!(!outcome.passed);
        let inbox = root.join(".ai").join("memory").join("inbox");
        let names: Vec<String> = fs::read_dir(&inbox)
            .map(|rd| {
                rd.filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            !names.iter().any(|n| n.contains("research-candidate")),
            "internal failure filed a candidate: {:?}",
            names
        );
        assert!(
            names.iter().any(|n| n.contains("attempt-fail")),
            "lesson path still filed"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn repeated_external_fail_dedups_to_one_candidate_with_recurrence() {
        let root = tmp_root("dedup");
        write_roadmap_file(&root, SIMPLE_ROADMAP);
        for _ in 0..2 {
            task_start(&root, "R-101", "worker", None, None, "").unwrap();
            let outcome =
                task_done_with_runner(&root, "R-101", None, "", None, "", 600, external_fail_run)
                    .unwrap();
            assert!(!outcome.passed);
        }
        let inbox = root.join(".ai").join("memory").join("inbox");
        let candidates: Vec<String> = fs::read_dir(&inbox)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
            .filter(|n| n.contains("research-candidate"))
            .collect();
        assert_eq!(
            candidates.len(),
            1,
            "one open candidate per signature: {:?}",
            candidates
        );
        let content = fs::read_to_string(inbox.join(&candidates[0])).unwrap();
        assert!(content.contains("recurrence_count: 2"), "{}", content);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn known_signature_attaches_without_new_candidate() {
        let root = tmp_root("known");
        write_roadmap_file(&root, SIMPLE_ROADMAP);
        // Curated layer already covers this failure class: the normalised
        // error line of the composed fail error appears in lessons.md.
        let mem = root.join(".ai").join("memory");
        fs::write(
            mem.join("lessons.md"),
            "# Lessons\n\n- error: linker `cc` failed: cannot find -lcuda (port #, pid #) — known issue, fix: install the cuda toolkit.\n",
        )
        .unwrap();
        task_start(&root, "R-101", "worker", None, None, "").unwrap();
        let outcome =
            task_done_with_runner(&root, "R-101", None, "", None, "", 600, external_fail_run)
                .unwrap();
        assert!(!outcome.passed);
        let events = read_attempts(&root);
        let fail = events.iter().find(|e| e.event == "fail").unwrap();
        assert_eq!(
            curated_known_match(&root, &fail.signature).as_deref(),
            Some("lessons.md"),
            "curated check must hit before filing"
        );
        assert!(
            !research_candidate_path(&root, &fail.signature).exists(),
            "known signature must not file new research"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn disposition_vocabulary_roundtrip() {
        for (name, d) in [
            ("extract", Disposition::Extract),
            ("reverse-engineer", Disposition::ReverseEngineer),
            ("adapt", Disposition::Adapt),
            ("clean-build", Disposition::CleanBuild),
        ] {
            assert_eq!(Disposition::parse(name), Some(d));
            assert_eq!(d.as_str(), name);
        }
        assert_eq!(Disposition::parse(""), None, "unset stays unset");
        assert_eq!(Disposition::parse("copy-everything"), None);
    }

    #[test]
    fn green_after_external_fail_resolves_candidate_and_fix_has_noise_guard() {
        let root = tmp_root("resolve");
        write_roadmap_file(&root, SIMPLE_ROADMAP);
        task_start(&root, "R-101", "worker", None, None, "").unwrap();
        let outcome =
            task_done_with_runner(&root, "R-101", None, "", None, "", 600, external_fail_run)
                .unwrap();
        assert!(!outcome.passed);
        let events = read_attempts(&root);
        let sig = events
            .iter()
            .find(|e| e.event == "fail")
            .unwrap()
            .signature
            .clone();
        let cpath = research_candidate_path(&root, &sig);
        assert!(cpath.exists());
        task_start(&root, "R-101", "worker", None, None, "").unwrap();
        let outcome = task_done(&root, "R-101", None, "installed the toolkit", None, "").unwrap();
        assert!(outcome.passed);
        let content = fs::read_to_string(&cpath).unwrap();
        assert!(
            content.contains("status: resolved"),
            "candidate closes on green: {}",
            content
        );
        // The attempt-fix record carries the §10.3 fields from birth.
        let inbox = root.join(".ai").join("memory").join("inbox");
        let fix = fs::read_dir(&inbox)
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.file_name().to_string_lossy().contains("attempt-fix"))
            .expect("attempt-fix filed");
        let fix_content = fs::read_to_string(fix.path()).unwrap();
        assert!(fix_content.contains("last_verified: 20"), "{}", fix_content);
        assert!(fix_content.contains("applies_to: 0.1.0"), "{}", fix_content);
        assert!(fix_content.contains("disposition:"), "{}", fix_content);
        let _ = fs::remove_dir_all(&root);
    }

    // -- §10.4 scope/subject -------------------------------------------

    #[test]
    fn scope_subject_on_attempts_and_legacy_defaults() {
        let root = tmp_root("scope-attempts");
        write_roadmap_file(&root, SIMPLE_ROADMAP);
        task_start(&root, "R-101", "worker", Some(50), None, "").unwrap();
        let events = read_attempts(&root);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].scope, Scope::RepoTask);
        assert_eq!(events[0].subject, "R-101");
        let json = attempt_to_json(&events[0]);
        assert!(json.contains("\"scope\":\"repo_task\""), "{}", json);
        assert!(json.contains("\"subject\":\"R-101\""), "{}", json);
        // A legacy line without scope/subject parses to the §10.4
        // defaults: repo_task, subject = task ID.
        let legacy = "{\"agent\":\"w\",\"event\":\"start\",\"task\":\"R-101\",\"ts\":\"2026-01-01T00:00:00Z\"}";
        let parsed = attempt_parse_line(legacy).expect("legacy line parses");
        assert_eq!(parsed.scope, Scope::RepoTask);
        assert_eq!(parsed.subject, "R-101");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn action_scope_records_fail_pass_pairs_only() {
        let root = tmp_root("scope-action");
        // A bare success is refused: no record per successful action.
        let err = file_scoped_event(
            &root,
            Scope::Action,
            "tool:deploy target:primo",
            "finish",
            "worker",
            "",
            "all good",
            None,
            None,
            "",
            false,
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("action_success_without_failure"),
            "{}",
            err
        );
        // A start is refused too — actions have no open-attempt record.
        let err = file_scoped_event(
            &root,
            Scope::Action,
            "tool:deploy target:primo",
            "start",
            "worker",
            "",
            "",
            None,
            None,
            "",
            false,
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("action_scope_pairs_only"),
            "{}",
            err
        );
        // Fail, then the resolving success: both recorded.
        file_scoped_event(
            &root,
            Scope::Action,
            "tool:deploy target:primo",
            "fail",
            "worker",
            "exit 1: connection reset",
            "",
            None,
            None,
            "",
            false,
        )
        .unwrap();
        file_scoped_event(
            &root,
            Scope::Action,
            "tool:deploy target:primo",
            "finish",
            "worker",
            "",
            "retry worked",
            None,
            None,
            "",
            false,
        )
        .unwrap();
        let events = read_attempts(&root);
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| e.scope == Scope::Action));
        assert!(events
            .iter()
            .all(|e| e.subject == "tool:deploy target:primo"));
        // A second success with no open failure is refused again.
        let err = file_scoped_event(
            &root,
            Scope::Action,
            "tool:deploy target:primo",
            "finish",
            "worker",
            "",
            "",
            None,
            None,
            "",
            false,
        )
        .unwrap_err();
        assert!(format!("{}", err).contains("action_success_without_failure"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn agent_task_scope_and_research_trigger() {
        let root = tmp_root("scope-agent");
        let ext_err = "upstream registry unreachable: connection refused";
        // First external fail, not blocking: no candidate yet (§10.4).
        file_scoped_event(
            &root,
            Scope::AgentTask,
            "dispatch-42",
            "start",
            "worker",
            "",
            "",
            Some(55),
            None,
            "",
            false,
        )
        .unwrap();
        file_scoped_event(
            &root,
            Scope::AgentTask,
            "dispatch-42",
            "fail",
            "worker",
            ext_err,
            "",
            Some(55),
            None,
            "",
            false,
        )
        .unwrap();
        let inbox = root.join(".ai").join("memory").join("inbox");
        let candidates = |dir: &PathBuf| -> Vec<String> {
            fs::read_dir(dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .filter(|n| n.contains("research-candidate"))
                        .collect()
                })
                .unwrap_or_default()
        };
        assert!(
            candidates(&inbox).is_empty(),
            "first non-blocking agent-scope fail files no candidate"
        );
        // Second fail with the same signature: recurrence ≥2 triggers.
        file_scoped_event(
            &root,
            Scope::AgentTask,
            "dispatch-42",
            "start",
            "worker",
            "",
            "",
            Some(30),
            None,
            "",
            false,
        )
        .unwrap();
        file_scoped_event(
            &root,
            Scope::AgentTask,
            "dispatch-42",
            "fail",
            "worker",
            ext_err,
            "",
            Some(30),
            None,
            "",
            false,
        )
        .unwrap();
        let found = candidates(&inbox);
        assert_eq!(found.len(), 1, "{:?}", found);
        let content = fs::read_to_string(inbox.join(&found[0])).unwrap();
        assert!(content.contains("scope: agent_task"), "{}", content);
        assert!(content.contains("subject: dispatch-42"), "{}", content);
        assert!(content.contains("recurrence_count: 2"), "{}", content);
        // A single BLOCKING external fail triggers immediately.
        file_scoped_event(
            &root,
            Scope::AgentTask,
            "dispatch-77",
            "fail",
            "worker",
            "upstream gpu driver fault on host",
            "",
            None,
            None,
            "",
            true,
        )
        .unwrap();
        assert_eq!(candidates(&inbox).len(), 2);
        // Events carry scope/subject; finish without start → null duration.
        let events = read_attempts(&root);
        let ev77 = events.iter().find(|e| e.subject == "dispatch-77").unwrap();
        assert_eq!(ev77.scope, Scope::AgentTask);
        assert_eq!(ev77.duration_secs, None);
        let _ = fs::remove_dir_all(&root);
    }
}
