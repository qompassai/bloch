//! bloch: ranked, budgeted codebase maps for coding agents.
//!
//! Named for the Bloch sphere — the geometric map of a qubit's quantum state.
//! Just as the Bloch sphere maps the infinite-dimensional Hilbert space of
//! single-qubit states onto a 3D unit sphere, bloch maps the sprawling state
//! of a codebase onto a compact, ranked representation.
//!
//! Walks a repo's text files, extracts structure (tree-sitter for Rust,
//! Lua, Python, Nix, Markdown, Kotlin, Bash, and TypeScript; line patterns
//! for Ruby/Fastlane, Starlark, TOML, and YAML), ranks definitions by
//! qualified reference counting with recency weighting, and emits a
//! budgeted map.
//!
//! Handoff fidelity: output leads with git session state, .ai/memory/
//! contents, and cached test status so any AI model can pick up where the
//! last one left off.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use thiserror::Error;
use tree_sitter::{Node, Parser as TsParser};
use walkdir::WalkDir;

/// Errors that can occur during repository mapping.
#[derive(Error, Debug)]
pub enum BlochError {
    /// I/O error reading files or directories.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Failed to parse a Rust file with tree-sitter.
    #[error("Rust parse failed for {path}: {msg}")]
    ParseFailed { path: String, msg: String },

    /// Git command failed or git is not available.
    #[error("Git operation failed: {0}")]
    GitFailed(String),

    /// Invalid regular expression (internal error, should not happen).
    #[error("Regex error: {0}")]
    Regex(#[from] regex::Error),

    /// Failed to set tree-sitter language (version incompatibility).
    #[error("Tree-sitter language error: {0}")]
    LanguageError(String),

    /// Failed to parse SCIP index.
    #[error("SCIP parse failed for {path}: {msg}")]
    ScipParseFailed { path: String, msg: String },

    /// Invalid path (traversal attempt or outside expected bounds).
    #[error("Invalid path: {0}")]
    InvalidPath(String),

    /// JSON serialization/deserialization failed.
    #[error("JSON error: {0}")]
    JsonError(String),
}

/// Result type for bloch operations.
pub type Result<T> = std::result::Result<T, BlochError>;

/// Era guard (v0.4): the stdio front door of the MCP server —
/// modern-era admission in front of the rmcp SDK.
pub mod guard;
/// Roadmap system (v0.2): init, templates, gated task lifecycle, lessons.
pub mod roadmap;
/// MCP server (v0.3, SPEC §8): fleet memory/roadmap/lessons over stdio.
pub mod server;

pub use roadmap::*;

const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "__pycache__",
    ".hg",
    ".svn",
    ".venv",
    "venv",
    "dist",
    "build",
    ".next",
    ".nuxt",
    "vendor",
];

const SKIP_EXTS: &[&str] = &[
    ".png", ".jpg", ".jpeg", ".gif", ".webp", ".ico", ".bmp", ".svg", ".pdf", ".zip", ".tar",
    ".gz", ".bz2", ".xz", ".7z", ".mp3", ".mp4", ".wav", ".ogg", ".webm", ".ttf", ".otf", ".woff",
    ".woff2", ".eot", ".exe", ".dll", ".so", ".dylib", ".a", ".o", ".pyc", ".pyo", ".class", ".db",
    ".sqlite", ".sqlite3",
];

const MAX_FILE_SIZE: u64 = 1024 * 1024;
const SIG_TRUNC: usize = 220;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Definition {
    pub name: String,
    pub kind: String,
    pub is_pub: bool,
    pub sig: String,
    pub impl_target: Option<String>,
    pub lang: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefKind {
    Call,
    Type,
    Path,
    Ident,
}

#[derive(Debug, Default, Clone)]
pub struct Refs {
    call: HashMap<(String, Option<String>), usize>,
    type_refs: HashMap<String, usize>,
    path_refs: HashMap<String, usize>,
    ident_refs: HashMap<String, usize>,
}

/// Per-file data extracted from a SCIP index: (definitions, references, language).
pub type ScipFileData = (Vec<Definition>, Refs, String);

impl Refs {
    pub fn get(&self, kind: RefKind, key: &RefKey) -> usize {
        match (kind, key) {
            (RefKind::Call, RefKey::Call(n, q)) => {
                *self.call.get(&(n.clone(), q.clone())).unwrap_or(&0)
            }
            (RefKind::Type, RefKey::Single(n)) => *self.type_refs.get(n).unwrap_or(&0),
            (RefKind::Path, RefKey::Single(n)) => *self.path_refs.get(n).unwrap_or(&0),
            (RefKind::Ident, RefKey::Single(n)) => *self.ident_refs.get(n).unwrap_or(&0),
            _ => 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RefKey {
    Call(String, Option<String>),
    Single(String),
}

#[derive(Debug, Clone)]
pub struct FileData {
    pub defs: Vec<Definition>,
    pub refs: Refs,
    pub modpath: Vec<String>,
    pub lang: String,
}

/// Cached map data for diffing between sessions.
/// Written to `.ai/memory/bloch-last.json` by `--update-memory`,
/// read by `--diff` to show what changed.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CachedMap {
    /// ISO 8601 timestamp of when the map was generated.
    pub timestamp: String,
    /// Git HEAD SHA at map time (empty if not a git repo).
    pub git_head: String,
    /// Map of relative path -> list of definition names.
    pub files: std::collections::HashMap<String, Vec<String>>,
    /// Top definitions by probability: (rel_path, name, kind, probability).
    pub top_defs: Vec<(String, String, String, f64)>,
    /// Language breakdown: lang -> file count.
    pub languages: std::collections::HashMap<String, usize>,
}

/// Write a human-readable summary to `.ai/memory/bloch-summary.md`.
/// Returns the path written, or an error.
/// Get the memory directory, validating that root doesn't contain path traversal.
/// Returns an error if the canonicalized path would escape the intended location.
fn memory_dir(root: &std::path::Path) -> Result<std::path::PathBuf> {
    for component in root.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(BlochError::InvalidPath(
                "root path contains parent directory (..) components".to_string(),
            ));
        }
    }
    let mem_dir = root.join(".ai").join("memory");
    std::fs::create_dir_all(&mem_dir)?;

    let canonical = mem_dir.canonicalize().map_err(BlochError::Io)?;
    let root_canonical = root.canonicalize().map_err(BlochError::Io)?;
    if !canonical.starts_with(&root_canonical) {
        return Err(BlochError::InvalidPath(
            "memory directory escapes root".to_string(),
        ));
    }
    Ok(mem_dir)
}

/// Write content atomically: write to temp file, then rename.
/// Prevents corrupt files if the process crashes mid-write.
fn atomic_write(path: &std::path::Path, content: &str) -> Result<()> {
    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, content)?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

pub fn write_memory_summary(
    root: &std::path::Path,
    cached: &CachedMap,
) -> Result<std::path::PathBuf> {
    let mem_dir = memory_dir(root)?;
    let path = mem_dir.join("bloch-summary.md");

    let mut out = String::new();
    out.push_str(
        "# bloch summary

",
    );
    out.push_str(&format!(
        "Generated: {}
",
        cached.timestamp
    ));
    if !cached.git_head.is_empty() {
        out.push_str(&format!(
            "Git HEAD: {}
",
            cached.git_head
        ));
    }
    out.push_str(&format!(
        "
## Files: {} total

",
        cached.files.len()
    ));
    out.push_str(
        "## Languages

",
    );
    let mut langs: Vec<(&String, &usize)> = cached.languages.iter().collect();
    langs.sort_by(|a, b| b.1.cmp(a.1));
    for (lang, count) in langs {
        out.push_str(&format!(
            "- {}: {} files
",
            lang, count
        ));
    }
    out.push_str(
        "
## Top definitions by probability

",
    );
    for (i, (rel, name, kind, prob)) in cached.top_defs.iter().enumerate() {
        out.push_str(&format!(
            "{}. `{}::{}::{}::{}::{}` (p={:.3})
",
            i + 1,
            rel,
            name,
            kind,
            prob,
            "",
            prob
        ));

        let _ = (rel, name, kind);
    }

    let mut top_section = String::from(
        "
## Top definitions by probability

",
    );
    for (i, (rel, name, kind, prob)) in cached.top_defs.iter().enumerate() {
        top_section.push_str(&format!(
            "{}. {} `{}::{}` (p={:.3})
",
            i + 1,
            kind,
            rel,
            name,
            prob
        ));
    }

    if let Some(idx) = out.find(
        "
## Top definitions by probability

",
    ) {
        out.truncate(idx);
        out.push_str(&top_section);
    }

    // Roadmap summary (v0.2, SPEC §7): --update-memory also refreshes the
    // roadmap counts into the summary when the repo has a roadmap.
    if let Some(roadmap_block) = roadmap_summary_for_map(root) {
        out.push_str(&roadmap_block);
    }

    atomic_write(&path, &out)?;
    Ok(path)
}

/// Write machine-readable cache to `.ai/memory/bloch-last.json`.
pub fn write_memory_cache(
    root: &std::path::Path,
    cached: &CachedMap,
) -> Result<std::path::PathBuf> {
    let mem_dir = memory_dir(root)?;
    let path = mem_dir.join("bloch-last.json");
    let json = serde_json::to_string_pretty(cached)
        .map_err(|e| BlochError::JsonError(format!("JSON serialize failed: {}", e)))?;
    atomic_write(&path, &json)?;
    Ok(path)
}

/// Read the cached map from `.ai/memory/bloch-last.json`, if it exists.
pub fn read_memory_cache(root: &std::path::Path) -> Result<Option<CachedMap>> {
    let path = root.join(".ai").join("memory").join("bloch-last.json");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(BlochError::Io(e)),
    };
    let map: CachedMap = serde_json::from_str(&content).map_err(|e| {
        BlochError::JsonError(format!("corrupt cache at {}: {}", path.display(), e))
    })?;
    Ok(Some(map))
}

/// Compute a diff between a cached map and the current state.
/// Returns a human-readable diff string.
pub fn diff_maps(old: &CachedMap, new: &CachedMap) -> String {
    let mut out = String::new();
    out.push_str(
        "# bloch diff: changes since last map

",
    );
    out.push_str(&format!(
        "Previous: {} ({})
",
        old.timestamp, old.git_head
    ));
    out.push_str(&format!(
        "Current:  {} ({})

",
        new.timestamp, new.git_head
    ));

    let old_files: std::collections::HashSet<&String> = old.files.keys().collect();
    let new_files: std::collections::HashSet<&String> = new.files.keys().collect();
    let added: Vec<&&String> = new_files.difference(&old_files).collect();
    let removed: Vec<&&String> = old_files.difference(&new_files).collect();

    if !added.is_empty() {
        out.push_str(&format!(
            "## New files ({})

",
            added.len()
        ));
        let mut sorted: Vec<&String> = added.iter().map(|s| **s).collect();
        sorted.sort();
        for f in sorted.iter().take(20) {
            out.push_str(&format!(
                "+ {}
",
                f
            ));
        }
        if sorted.len() > 20 {
            out.push_str(&format!(
                "  ... and {} more
",
                sorted.len() - 20
            ));
        }
        out.push('\n');
    }

    if !removed.is_empty() {
        out.push_str(&format!(
            "## Removed files ({})

",
            removed.len()
        ));
        let mut sorted: Vec<&String> = removed.iter().map(|s| **s).collect();
        sorted.sort();
        for f in sorted.iter().take(20) {
            out.push_str(&format!(
                "- {}
",
                f
            ));
        }
        if sorted.len() > 20 {
            out.push_str(&format!(
                "  ... and {} more
",
                sorted.len() - 20
            ));
        }
        out.push('\n');
    }

    let mut def_changes = Vec::new();
    for (path, new_defs) in &new.files {
        if let Some(old_defs) = old.files.get(path) {
            let old_set: std::collections::HashSet<&String> = old_defs.iter().collect();
            let new_set: std::collections::HashSet<&String> = new_defs.iter().collect();
            for d in new_set.difference(&old_set) {
                def_changes.push(format!("+ {}::{}", path, d));
            }
            for d in old_set.difference(&new_set) {
                def_changes.push(format!("- {}::{}", path, d));
            }
        }
    }
    if !def_changes.is_empty() {
        out.push_str(&format!(
            "## Definition changes ({})

",
            def_changes.len()
        ));
        def_changes.sort();
        for c in def_changes.iter().take(30) {
            out.push_str(&format!(
                "{}
",
                c
            ));
        }
        if def_changes.len() > 30 {
            out.push_str(&format!(
                "  ... and {} more
",
                def_changes.len() - 30
            ));
        }
        out.push('\n');
    }

    let old_probs: std::collections::HashMap<String, f64> = old
        .top_defs
        .iter()
        .map(|(r, n, _, p)| (format!("{}::{}", r, n), *p))
        .collect();
    let mut shifts = Vec::new();
    for (rel, name, kind, new_p) in &new.top_defs {
        let key = format!("{}::{}", rel, name);
        if let Some(old_p) = old_probs.get(&key) {
            let delta = new_p - old_p;
            if delta.abs() > 0.02 {
                shifts.push((
                    delta,
                    format!("{} `{}::{}` {:.3} → {:.3}", kind, rel, name, old_p, new_p),
                ));
            }
        }
    }
    if !shifts.is_empty() {
        shifts.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        out.push_str(&format!(
            "## Significant probability shifts ({})

",
            shifts.len()
        ));
        for (_, desc) in shifts.iter().take(15) {
            out.push_str(&format!(
                "{}
",
                desc
            ));
        }
        out.push('\n');
    }

    if added.is_empty() && removed.is_empty() && def_changes.is_empty() && shifts.is_empty() {
        out.push_str(
            "No significant changes since last map.
",
        );
    }

    out
}

pub fn run_git(root: &Path, args: &[&str]) -> String {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

pub fn get_session_state(root: &Path) -> String {
    let mut lines = vec!["## session state".to_string()];

    let branch = run_git(root, &["branch", "--show-current"]);
    if !branch.is_empty() {
        lines.push(format!("branch: {}", branch));
    }

    let status = run_git(
        root,
        &["status", "--porcelain=v1", "--untracked-files=normal"],
    );
    if !status.is_empty() {
        let dirty: Vec<&str> = status.lines().filter(|l| !l.trim().is_empty()).collect();
        lines.push(format!("dirty files ({}):", dirty.len()));
        for l in dirty.iter().take(20) {
            lines.push(format!("  {}", l));
        }
        if dirty.len() > 20 {
            lines.push(format!("  ... and {} more", dirty.len() - 20));
        }
    } else {
        lines.push("tree: clean".to_string());
    }

    let log = run_git(root, &["log", "--oneline", "-8", "--no-decorate"]);
    if !log.is_empty() {
        lines.push("recent commits:".to_string());
        for l in log.lines().take(8) {
            lines.push(format!("  {}", l));
        }
    }

    let stash = run_git(root, &["stash", "list"]);
    if !stash.is_empty() {
        lines.push("stashes:".to_string());
        for l in stash.lines().take(5) {
            lines.push(format!("  {}", l));
        }
    }

    lines.join("\n")
}

pub fn get_memory_context(root: &Path, budget: usize) -> String {
    let mem_dir = root.join(".ai").join("memory");
    if !mem_dir.is_dir() {
        return String::new();
    }

    let mut lines = vec!["## memory (from .ai/memory/)".to_string()];
    let mut used: usize = lines.iter().map(|l| l.len() + 1).sum();

    let priority = ["handoff.md", "active.md", "patterns.md", "decisions.md"];
    let mut ordered: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for p in &priority {
        let fp = mem_dir.join(p);
        if fp.is_file() {
            ordered.push(fp);
            seen.insert(p.to_string());
        }
    }

    if let Ok(entries) = fs::read_dir(&mem_dir) {
        let mut rest: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension().map(|e| e == "md").unwrap_or(false)
                    && p.file_name()
                        .map(|n| !seen.contains(&n.to_string_lossy().to_string()))
                        .unwrap_or(false)
            })
            .collect();
        rest.sort();
        ordered.extend(rest);
    }

    for fp in ordered {
        let content = fs::read_to_string(&fp).unwrap_or_default();
        let content = content.trim();
        if content.is_empty() {
            continue;
        }
        let name = match fp.file_name() {
            Some(n) => n.to_string_lossy().to_string(),
            None => continue,
        };
        let header = format!("### {}", name);
        let max_file = 1200;
        let truncated = if content.len() > max_file {
            format!("{}\n... (truncated)", &content[..max_file])
        } else {
            content.to_string()
        };
        let block = format!("{}\n{}", header, truncated);
        if used + block.len() + 1 > budget {
            lines.push(format!("### {} (omitted: over memory budget)", name));
            continue;
        }
        used += block.len() + 1;
        lines.push(block);
    }

    if lines.len() == 1 {
        return String::new();
    }
    lines.join("\n\n")
}

pub fn get_test_status(root: &Path, budget: usize) -> String {
    let candidates = [
        ".ai/memory/test-status.json",
        ".ai/test-status.json",
        "target/.test-status.json",
    ];

    for c in &candidates {
        let fp = root.join(c);
        if !fp.is_file() {
            continue;
        }
        let content = match fs::read_to_string(&fp) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let data: serde_json::Value = match serde_json::from_str(&content) {
            Ok(d) => d,
            Err(_) => continue,
        };

        let mut lines = vec!["## last test/lint run".to_string()];
        for k in &[
            "command",
            "passed",
            "failed",
            "failing_tests",
            "warnings",
            "timestamp",
        ] {
            if let Some(v) = data.get(*k) {
                match v {
                    serde_json::Value::Array(arr) => {
                        lines.push(format!("{}:", k));
                        for item in arr.iter().take(10) {
                            lines.push(format!("  - {}", item));
                        }
                    }
                    _ => lines.push(format!("{}: {}", k, v)),
                }
            }
        }
        let out = lines.join("\n");
        return out.chars().take(budget).collect();
    }
    String::new()
}

pub fn get_file_recency_boost(root: &Path, rel: &str, max_boost: f64) -> f64 {
    let ts = run_git(root, &["log", "-1", "--format=%ct", "--", rel]);
    let ts: i64 = match ts.parse() {
        Ok(n) => n,
        Err(_) => return 1.0,
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(ts);
    let age_days = (now - ts) as f64 / 86400.0;

    if age_days > 30.0 {
        return 1.0;
    }
    if age_days < 0.0 {
        return max_boost;
    }
    1.0 + (max_boost - 1.0) * ((30.0 - age_days) / 30.0).max(0.0)
}

pub fn get_file_why(root: &Path, rel: &str, max_chars: usize) -> String {
    let msg = run_git(root, &["log", "-1", "--format=%s", "--", rel]);
    if msg.is_empty() {
        return String::new();
    }
    let first = msg.split('.').next().unwrap_or("").trim();
    if first.len() > max_chars {
        format!("{}…", &first[..max_chars.saturating_sub(1)])
    } else {
        first.to_string()
    }
}

fn is_text_file(path: &Path) -> bool {
    use std::io::Read;
    let mut fh = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut buf = [0u8; 8192];
    let n = match fh.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return false,
    };
    !buf[..n].contains(&0)
}

pub fn iter_all_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();

    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            if e.file_type().is_dir() {
                if name.starts_with('.') {
                    return false;
                }
                return !SKIP_DIRS.contains(&name.as_ref());
            }
            true
        });

    for entry in walker.filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let name = match path.file_name() {
            Some(n) => n.to_string_lossy(),
            None => continue,
        };

        if name.starts_with('.') && name != ".env.example" {
            if name == ".DS_Store" || name == ".gitkeep" {
                continue;
            }

            if ![".gitignore", ".bloch.txt"].contains(&name.as_ref()) {
                continue;
            }
        }

        if let Some(ext) = path.extension() {
            let ext = format!(".{}", ext.to_string_lossy().to_lowercase());
            if SKIP_EXTS.contains(&ext.as_str()) {
                continue;
            }
        }

        if let Ok(meta) = fs::metadata(path) {
            if meta.len() > MAX_FILE_SIZE {
                continue;
            }
        }

        if is_text_file(path) {
            out.push(path.to_path_buf());
        }
    }

    out.sort();
    out
}

fn node_text<'a>(node: &Node, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}

fn is_pub(node: &Node) -> bool {
    let mut cursor = node.walk();
    let found = node
        .children(&mut cursor)
        .any(|c| c.kind() == "visibility_modifier");
    found
}

fn def_name(node: &Node, src: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let k = child.kind();
        if k == "identifier" || k == "type_identifier" {
            return Some(node_text(&child, src).to_string());
        }
    }
    None
}

fn impl_target(node: &Node, src: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    let cands: Vec<Node> = node
        .children(&mut cursor)
        .filter(|c| {
            matches!(
                c.kind(),
                "type_identifier" | "generic_type" | "scoped_type_identifier"
            )
        })
        .collect();
    let last = cands.last()?;
    match last.kind() {
        "generic_type" => last
            .child_by_field_name("name")
            .map(|n| node_text(&n, src).to_string()),
        "scoped_type_identifier" => last
            .child_by_field_name("name")
            .map(|n| node_text(&n, src).to_string())
            .or_else(|| Some(node_text(last, src).to_string())),
        _ => Some(node_text(last, src).to_string()),
    }
}

fn signature(node: &Node, src: &[u8]) -> String {
    let mut end = node.end_byte();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "block" {
            end = child.start_byte();
            break;
        }
    }
    let sig: String = String::from_utf8_lossy(&src[node.start_byte()..end])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if sig.len() > SIG_TRUNC {
        format!("{}…", &sig[..SIG_TRUNC - 1])
    } else {
        sig
    }
}

pub fn mod_segments(rel: &str) -> Vec<String> {
    let parts: Vec<&str> = rel.split('/').collect();
    if let Some(i) = parts.iter().position(|&p| p == "src") {
        if i > 0 {
            let krate = parts[i - 1].to_string();
            let mut segs: Vec<String> = parts[i + 1..]
                .iter()
                .filter(|p| p.ends_with(".rs"))
                .map(|p| p.trim_end_matches(".rs").to_string())
                .filter(|s| !["lib", "main", "mod"].contains(&s.as_str()))
                .collect();
            let mut out = vec![krate];
            out.append(&mut segs);
            return out;
        }
    }
    vec![Path::new(parts.last().unwrap_or(&""))
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string()]
}

fn extract_rust(src: &[u8]) -> Result<(Vec<Definition>, Refs)> {
    let mut parser = TsParser::new();
    let lang = tree_sitter::Language::new(tree_sitter_rust::LANGUAGE);
    parser
        .set_language(&lang)
        .map_err(|e| BlochError::LanguageError(format!("rust: {e}")))?;
    let tree = match parser.parse(src, None) {
        Some(t) => t,
        None => return Ok((Vec::new(), Refs::default())),
    };

    let mut defs = Vec::new();
    let mut refs = Refs::default();

    fn visit_def(node: &Node, src: &[u8], in_impl: Option<String>, defs: &mut Vec<Definition>) {
        let kind = node.kind();
        match kind {
            "function_item" | "struct_item" | "enum_item" | "trait_item" | "type_item"
            | "const_item" | "static_item" | "mod_item" => {
                if let Some(name) = def_name(node, src) {
                    let kind_str = if kind == "function_item" {
                        "fn".to_string()
                    } else {
                        kind.trim_end_matches("_item").to_string()
                    };
                    defs.push(Definition {
                        name,
                        kind: kind_str,
                        is_pub: is_pub(node),
                        sig: signature(node, src),
                        impl_target: in_impl.clone(),
                        lang: "rust".to_string(),
                    });
                }
            }
            "impl_item" => {
                let target = impl_target(node, src);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    visit_def(&child, src, target.clone(), defs);
                }
                return;
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            visit_def(&child, src, in_impl.clone(), defs);
        }
    }

    fn visit_ref(node: &Node, src: &[u8], refs: &mut Refs) {
        match node.kind() {
            "call_expression" => {
                if let Some(callee) = node.child_by_field_name("function") {
                    match callee.kind() {
                        "identifier" => {
                            let name = node_text(&callee, src).to_string();
                            *refs.call.entry((name, None)).or_insert(0) += 1;
                        }
                        "scoped_identifier" => {
                            let nm = callee
                                .child_by_field_name("name")
                                .map(|n| node_text(&n, src).to_string());
                            let ph = callee
                                .child_by_field_name("path")
                                .map(|n| node_text(&n, src).to_string());
                            if let Some(n) = nm {
                                *refs.call.entry((n, ph)).or_insert(0) += 1;
                            }
                        }
                        "field_expression" => {
                            let f = callee.child_by_field_name("field");
                            let v = callee.child_by_field_name("value");
                            if let (Some(f), Some(v)) = (f, v) {
                                let fname = node_text(&f, src).to_string();
                                let vtext = node_text(&v, src).to_string();
                                let q = if v.kind() == "identifier" {
                                    Some(vtext)
                                } else {
                                    None
                                };
                                *refs.call.entry((fname, q)).or_insert(0) += 1;
                            }
                        }
                        _ => {}
                    }
                }
            }
            "type_identifier" => {
                let name = node_text(node, src).to_string();
                *refs.type_refs.entry(name).or_insert(0) += 1;
            }
            "scoped_identifier" | "scoped_type_identifier" => {
                if let Some(ph) = node.child_by_field_name("path") {
                    let ptext = node_text(&ph, src);
                    for seg in ptext.split("::") {
                        *refs.path_refs.entry(seg.to_string()).or_insert(0) += 1;
                    }
                }
            }
            "identifier" => {
                let name = node_text(node, src).to_string();
                *refs.ident_refs.entry(name).or_insert(0) += 1;
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            visit_ref(&child, src, refs);
        }
    }

    visit_def(&tree.root_node(), src, None, &mut defs);
    visit_ref(&tree.root_node(), src, &mut refs);
    Ok((defs, refs))
}

fn extract_lua(src: &[u8]) -> Result<(Vec<Definition>, Refs)> {
    let mut parser = TsParser::new();
    let lang = tree_sitter::Language::new(tree_sitter_lua::LANGUAGE);
    parser
        .set_language(&lang)
        .map_err(|e| BlochError::LanguageError(format!("lua: {e}")))?;
    let tree = match parser.parse(src, None) {
        Some(t) => t,
        None => return Ok((Vec::new(), Refs::default())),
    };

    let mut defs = Vec::new();
    let mut refs = Refs::default();

    fn name_text(node: &Node, src: &[u8]) -> String {
        node_text(node, src).to_string()
    }

    fn visit_def(node: &Node, src: &[u8], defs: &mut Vec<Definition>) {
        match node.kind() {
            "function_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = name_text(&name_node, src);
                    defs.push(Definition {
                        name: name.clone(),
                        kind: "function".to_string(),
                        is_pub: !name.starts_with("local"),
                        sig: format!("function {}", name),
                        impl_target: None,
                        lang: "lua".to_string(),
                    });
                }
            }
            "variable_declaration" => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "assignment_statement" {
                        let mut ac = child.walk();
                        let kids: Vec<Node> = child.children(&mut ac).collect();
                        if kids.len() >= 2 {
                            let lhs = name_text(&kids[0], src);
                            if kids[1].kind() == "function_definition" {
                                defs.push(Definition {
                                    name: lhs.clone(),
                                    kind: "function".to_string(),
                                    is_pub: false,
                                    sig: format!("local function {}", lhs),
                                    impl_target: None,
                                    lang: "lua".to_string(),
                                });
                            } else {
                                defs.push(Definition {
                                    name: lhs.clone(),
                                    kind: "variable".to_string(),
                                    is_pub: false,
                                    sig: format!("local {}", lhs),
                                    impl_target: None,
                                    lang: "lua".to_string(),
                                });
                            }
                        }
                    }
                }
            }
            "assignment_statement" => {
                let mut cursor = node.walk();
                let kids: Vec<Node> = node.children(&mut cursor).collect();
                if kids.len() >= 2 && kids[1].kind() == "function_definition" {
                    let name = name_text(&kids[0], src);
                    defs.push(Definition {
                        name: name.clone(),
                        kind: "function".to_string(),
                        is_pub: true,
                        sig: format!("{} = function", name),
                        impl_target: None,
                        lang: "lua".to_string(),
                    });
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            visit_def(&child, src, defs);
        }
    }

    fn visit_ref(node: &Node, src: &[u8], refs: &mut Refs) {
        match node.kind() {
            "function_call" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = name_text(&name_node, src);

                    if name == "require" {
                        if let Some(args) = node.child_by_field_name("arguments") {
                            let mut ac = args.walk();
                            for arg in args.children(&mut ac) {
                                if arg.kind() == "string" {
                                    let mod_name = name_text(&arg, src)
                                        .trim_matches('"')
                                        .trim_matches('\'')
                                        .to_string();
                                    *refs.path_refs.entry(mod_name).or_insert(0) += 1;
                                }
                            }
                        }
                    } else {
                        *refs.call.entry((name, None)).or_insert(0) += 1;
                    }
                }
            }
            "identifier" => {
                let name = name_text(node, src);
                *refs.ident_refs.entry(name).or_insert(0) += 1;
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            visit_ref(&child, src, refs);
        }
    }

    visit_def(&tree.root_node(), src, &mut defs);
    visit_ref(&tree.root_node(), src, &mut refs);
    Ok((defs, refs))
}

fn extract_python(src: &[u8]) -> Result<(Vec<Definition>, Refs)> {
    let mut parser = TsParser::new();
    let lang = tree_sitter::Language::new(tree_sitter_python::LANGUAGE);
    parser
        .set_language(&lang)
        .map_err(|e| BlochError::LanguageError(format!("python: {e}")))?;
    let tree = match parser.parse(src, None) {
        Some(t) => t,
        None => return Ok((Vec::new(), Refs::default())),
    };

    let mut defs = Vec::new();
    let mut refs = Refs::default();

    fn name_text(node: &Node, src: &[u8]) -> String {
        node_text(node, src).to_string()
    }

    fn visit_def(node: &Node, src: &[u8], in_class: Option<String>, defs: &mut Vec<Definition>) {
        match node.kind() {
            "function_definition" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = name_text(&name_node, src);
                    defs.push(Definition {
                        name: name.clone(),
                        kind: "function".to_string(),
                        is_pub: !name.starts_with('_'),
                        sig: format!("def {}", name),
                        impl_target: in_class.clone(),
                        lang: "python".to_string(),
                    });
                }
            }
            "class_definition" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = name_text(&name_node, src);
                    defs.push(Definition {
                        name: name.clone(),
                        kind: "class".to_string(),
                        is_pub: !name.starts_with('_'),
                        sig: format!("class {}", name),
                        impl_target: None,
                        lang: "python".to_string(),
                    });

                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        visit_def(&child, src, Some(name.clone()), defs);
                    }
                    return;
                }
            }
            "decorated_definition" => {
                if let Some(def_node) = node.child_by_field_name("definition") {
                    visit_def(&def_node, src, in_class.clone(), defs);
                    return;
                }
            }
            "assignment" => {
                if let Some(left) = node.child_by_field_name("left") {
                    if left.kind() == "identifier" {
                        let name = name_text(&left, src);
                        defs.push(Definition {
                            name: name.clone(),
                            kind: "variable".to_string(),
                            is_pub: !name.starts_with('_'),
                            sig: name.clone(),
                            impl_target: in_class.clone(),
                            lang: "python".to_string(),
                        });
                    }
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            visit_def(&child, src, in_class.clone(), defs);
        }
    }

    fn visit_ref(node: &Node, src: &[u8], refs: &mut Refs) {
        match node.kind() {
            "call" => {
                if let Some(func) = node.child_by_field_name("function") {
                    match func.kind() {
                        "identifier" => {
                            let name = name_text(&func, src);
                            *refs.call.entry((name, None)).or_insert(0) += 1;
                        }
                        "attribute" => {
                            let attr = func.child_by_field_name("attribute");
                            let obj = func.child_by_field_name("object");
                            if let (Some(a), Some(o)) = (attr, obj) {
                                let aname = name_text(&a, src);
                                let oname = name_text(&o, src);
                                let qualifier = if o.kind() == "identifier" {
                                    Some(oname)
                                } else {
                                    None
                                };
                                *refs.call.entry((aname, qualifier)).or_insert(0) += 1;
                            }
                        }
                        _ => {}
                    }
                }
            }
            "import_statement" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = name_text(&name_node, src);
                    *refs.path_refs.entry(name).or_insert(0) += 1;
                }
            }
            "import_from_statement" => {
                if let Some(mod_node) = node.child_by_field_name("module_name") {
                    let name = name_text(&mod_node, src);
                    *refs.path_refs.entry(name).or_insert(0) += 1;
                }
            }
            "identifier" => {
                let name = name_text(node, src);
                *refs.ident_refs.entry(name).or_insert(0) += 1;
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            visit_ref(&child, src, refs);
        }
    }

    visit_def(&tree.root_node(), src, None, &mut defs);
    visit_ref(&tree.root_node(), src, &mut refs);
    Ok((defs, refs))
}

fn extract_nix(src: &[u8]) -> Result<(Vec<Definition>, Refs)> {
    let mut parser = TsParser::new();
    let lang = tree_sitter::Language::new(tree_sitter_nix::LANGUAGE);
    parser
        .set_language(&lang)
        .map_err(|e| BlochError::LanguageError(format!("nix: {e}")))?;
    let tree = match parser.parse(src, None) {
        Some(t) => t,
        None => return Ok((Vec::new(), Refs::default())),
    };

    let mut defs = Vec::new();
    let mut refs = Refs::default();

    fn name_text(node: &Node, src: &[u8]) -> String {
        node_text(node, src).to_string()
    }

    fn attrpath_text(node: &Node, src: &[u8]) -> String {
        let mut parts = Vec::new();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "identifier" {
                parts.push(name_text(&child, src));
            }
        }
        parts.join(".")
    }

    fn visit_def(node: &Node, src: &[u8], defs: &mut Vec<Definition>) {
        match node.kind() {
            "binding" => {
                if let Some(attrpath) = node.child_by_field_name("attrpath") {
                    let name = attrpath_text(&attrpath, src);
                    if !name.is_empty() && name != "inherit" {
                        let is_func = node
                            .child_by_field_name("expression")
                            .map(|e| e.kind() == "function_expression")
                            .unwrap_or(false);
                        defs.push(Definition {
                            name: name.clone(),
                            kind: if is_func {
                                "function".to_string()
                            } else {
                                "attr".to_string()
                            },
                            is_pub: true,
                            sig: name.clone(),
                            impl_target: None,
                            lang: "nix".to_string(),
                        });
                    }
                }
            }
            "function_expression" => {
                if let Some(formals) = node.child_by_field_name("formals") {
                    let mut params = Vec::new();
                    let mut cursor = formals.walk();
                    for child in formals.children(&mut cursor) {
                        if child.kind() == "formal" {
                            if let Some(name_node) = child.child_by_field_name("name") {
                                params.push(name_text(&name_node, src));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            visit_def(&child, src, defs);
        }
    }

    fn visit_ref(node: &Node, src: &[u8], refs: &mut Refs) {
        match node.kind() {
            "variable_expression" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = name_text(&name_node, src);

                    if name != "import" {
                        *refs.ident_refs.entry(name).or_insert(0) += 1;
                    }
                }
            }
            "apply_expression" => {
                if let Some(func) = node.child_by_field_name("function") {
                    if func.kind() == "variable_expression" {
                        if let Some(name_node) = func.child_by_field_name("name") {
                            if name_text(&name_node, src) == "import" {
                                if let Some(arg) = node.child_by_field_name("argument") {
                                    if arg.kind() == "path_expression" {
                                        let path = name_text(&arg, src);
                                        *refs.path_refs.entry(path).or_insert(0) += 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            "select_expression" => {
                if let Some(attrpath) = node.child_by_field_name("attrpath") {
                    let name = attrpath_text(&attrpath, src);
                    if !name.is_empty() {
                        *refs.ident_refs.entry(name).or_insert(0) += 1;
                    }
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            visit_ref(&child, src, refs);
        }
    }

    visit_def(&tree.root_node(), src, &mut defs);
    visit_ref(&tree.root_node(), src, &mut refs);
    Ok((defs, refs))
}

fn extract_markdown_ts(src: &[u8]) -> Result<(Vec<Definition>, Refs)> {
    let mut parser = TsParser::new();
    let lang = tree_sitter::Language::new(tree_sitter_md::LANGUAGE);
    parser
        .set_language(&lang)
        .map_err(|e| BlochError::LanguageError(format!("markdown: {e}")))?;
    let tree = match parser.parse(src, None) {
        Some(t) => t,
        None => return Ok((Vec::new(), Refs::default())),
    };

    let mut defs = Vec::new();
    let refs = Refs::default();

    fn name_text(node: &Node, src: &[u8]) -> String {
        node_text(node, src).to_string()
    }

    fn visit(node: &Node, src: &[u8], defs: &mut Vec<Definition>) {
        match node.kind() {
            "atx_heading" => {
                let mut level = 0;
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    let k = child.kind();
                    if k.starts_with("atx_h") && k.ends_with("_marker") {
                        if let Some(d) = k.chars().nth(5) {
                            level = d.to_digit(10).unwrap_or(0) as usize;
                        }
                    }
                }
                if (1..=6).contains(&level) {
                    if let Some(content) = node.child_by_field_name("heading_content") {
                        let title = name_text(&content, src).trim().to_string();
                        if !title.is_empty() {
                            defs.push(Definition {
                                name: title.clone(),
                                kind: format!("h{}", level),
                                is_pub: true,
                                sig: format!("{} {}", "#".repeat(level), title),
                                impl_target: None,
                                lang: "markdown".to_string(),
                            });
                        }
                    }
                }
            }
            "setext_heading" => {
                let mut level = 2;
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "setext_h1_underline" {
                        level = 1;
                    }
                }
                if let Some(content) = node.child_by_field_name("heading_content") {
                    let title = name_text(&content, src).trim().to_string();
                    if !title.is_empty() {
                        defs.push(Definition {
                            name: title.clone(),
                            kind: format!("h{}", level),
                            is_pub: true,
                            sig: title.clone(),
                            impl_target: None,
                            lang: "markdown".to_string(),
                        });
                    }
                }
            }
            "fenced_code_block" => {
                let mut lang_tag = String::new();
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "info_string" {
                        let mut ic = child.walk();
                        for info_child in child.children(&mut ic) {
                            if info_child.kind() == "language" {
                                lang_tag = name_text(&info_child, src);
                            }
                        }
                    }
                }
                let name = if lang_tag.is_empty() {
                    "code block".to_string()
                } else {
                    format!("{} code", lang_tag)
                };
                defs.push(Definition {
                    name: name.clone(),
                    kind: "code".to_string(),
                    is_pub: true,
                    sig: name,
                    impl_target: None,
                    lang: "markdown".to_string(),
                });
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            visit(&child, src, defs);
        }
    }

    visit(&tree.root_node(), src, &mut defs);
    Ok((defs, refs))
}

fn extract_kotlin(src: &[u8]) -> Result<(Vec<Definition>, Refs)> {
    let mut parser = TsParser::new();
    let lang = tree_sitter_kotlin_ng::LANGUAGE.into();
    parser
        .set_language(&lang)
        .map_err(|e| BlochError::LanguageError(e.to_string()))?;
    let tree = match parser.parse(src, None) {
        Some(t) => t,
        None => return Ok((Vec::new(), Refs::default())),
    };

    let mut defs = Vec::new();
    let mut refs = Refs::default();

    fn name_text(node: &Node, src: &[u8]) -> String {
        node_text(node, src).to_string()
    }

    fn first_child_of<'a>(node: &Node<'a>, kind: &str) -> Option<Node<'a>> {
        let mut cursor = node.walk();

        let mut children = node.children(&mut cursor);
        children.find(|child| child.kind() == kind)
    }

    fn visit_def(node: &Node, src: &[u8], defs: &mut Vec<Definition>) {
        match node.kind() {
            "function_declaration" => {
                if let Some(name_node) = first_child_of(node, "identifier") {
                    let name = name_text(&name_node, src);
                    defs.push(Definition {
                        name: name.clone(),
                        kind: "function".to_string(),
                        is_pub: true,
                        sig: format!("fun {}", name),
                        impl_target: None,
                        lang: "kotlin".to_string(),
                    });
                }
            }
            "class_declaration" | "object_declaration" => {
                if let Some(name_node) = first_child_of(node, "identifier") {
                    let name = name_text(&name_node, src);
                    let kind = if node.kind() == "class_declaration" {
                        "class"
                    } else {
                        "object"
                    };
                    defs.push(Definition {
                        name: name.clone(),
                        kind: kind.to_string(),
                        is_pub: true,
                        sig: format!("{} {}", kind, name),
                        impl_target: None,
                        lang: "kotlin".to_string(),
                    });
                }
            }
            "property_declaration" => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "variable_declaration" {
                        if let Some(name_node) = first_child_of(&child, "identifier") {
                            let name = name_text(&name_node, src);
                            defs.push(Definition {
                                name: name.clone(),
                                kind: "property".to_string(),
                                is_pub: true,
                                sig: format!("val/var {}", name),
                                impl_target: None,
                                lang: "kotlin".to_string(),
                            });
                        }
                    }
                }
            }
            "type_alias" => {
                if let Some(name_node) = first_child_of(node, "identifier") {
                    let name = name_text(&name_node, src);
                    defs.push(Definition {
                        name: name.clone(),
                        kind: "typealias".to_string(),
                        is_pub: true,
                        sig: format!("typealias {}", name),
                        impl_target: None,
                        lang: "kotlin".to_string(),
                    });
                }
            }
            "enum_entry" => {
                if let Some(name_node) = first_child_of(node, "identifier") {
                    let name = name_text(&name_node, src);
                    defs.push(Definition {
                        name: name.clone(),
                        kind: "enum_entry".to_string(),
                        is_pub: true,
                        sig: name.clone(),
                        impl_target: None,
                        lang: "kotlin".to_string(),
                    });
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            visit_def(&child, src, defs);
        }
    }

    fn visit_ref(node: &Node, src: &[u8], refs: &mut Refs) {
        match node.kind() {
            "call_expression" => {
                if let Some(name_node) = first_child_of(node, "identifier") {
                    let name = name_text(&name_node, src);
                    *refs.call.entry((name, None)).or_insert(0) += 1;
                }
            }
            "navigation_expression" => {
                let text = name_text(node, src);
                *refs.ident_refs.entry(text).or_insert(0) += 1;
            }
            "identifier" => {
                let name = name_text(node, src);

                if ![
                    "val", "var", "fun", "class", "object", "if", "else", "when", "for", "while",
                    "return",
                ]
                .contains(&name.as_str())
                {
                    *refs.ident_refs.entry(name).or_insert(0) += 1;
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            visit_ref(&child, src, refs);
        }
    }

    visit_def(&tree.root_node(), src, &mut defs);
    visit_ref(&tree.root_node(), src, &mut refs);
    Ok((defs, refs))
}

fn extract_bash(src: &[u8]) -> Result<(Vec<Definition>, Refs)> {
    let mut parser = TsParser::new();
    let lang = tree_sitter::Language::new(tree_sitter_bash::LANGUAGE);
    parser
        .set_language(&lang)
        .map_err(|e| BlochError::LanguageError(format!("bash: {e}")))?;
    let tree = match parser.parse(src, None) {
        Some(t) => t,
        None => return Ok((Vec::new(), Refs::default())),
    };

    let mut defs = Vec::new();
    let mut refs = Refs::default();

    fn name_text(node: &Node, src: &[u8]) -> String {
        node_text(node, src).to_string()
    }

    fn visit_def(node: &Node, src: &[u8], defs: &mut Vec<Definition>) {
        match node.kind() {
            "function_definition" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = name_text(&name_node, src);
                    defs.push(Definition {
                        name: name.clone(),
                        kind: "function".to_string(),
                        is_pub: true,
                        sig: format!("{}()", name),
                        impl_target: None,
                        lang: "bash".to_string(),
                    });
                }
            }
            "variable_assignment" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = name_text(&name_node, src);
                    defs.push(Definition {
                        name: name.clone(),
                        kind: "variable".to_string(),
                        is_pub: false,
                        sig: name.clone(),
                        impl_target: None,
                        lang: "bash".to_string(),
                    });
                }
            }
            "declaration_command" => {
                let text = name_text(node, src);

                let parts: Vec<&str> = text.split_whitespace().collect();
                if parts.len() >= 2 {
                    for part in &parts[1..] {
                        if !part.starts_with('-') && part.contains('=') {
                            if let Some(name) = part.split('=').next() {
                                defs.push(Definition {
                                    name: name.to_string(),
                                    kind: "variable".to_string(),
                                    is_pub: parts[0] == "export",
                                    sig: part.to_string(),
                                    impl_target: None,
                                    lang: "bash".to_string(),
                                });
                                break;
                            }
                        } else if !part.starts_with('-') {
                            defs.push(Definition {
                                name: part.to_string(),
                                kind: "variable".to_string(),
                                is_pub: parts[0] == "export",
                                sig: part.to_string(),
                                impl_target: None,
                                lang: "bash".to_string(),
                            });
                            break;
                        }
                    }
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            visit_def(&child, src, defs);
        }
    }

    fn visit_ref(node: &Node, src: &[u8], refs: &mut Refs) {
        match node.kind() {
            "command" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = name_text(&name_node, src);

                    if name == "source" || name == "." {
                        let mut cursor = node.walk();
                        for child in node.children(&mut cursor) {
                            if child.kind() == "word" || child.kind() == "string" {
                                let path = name_text(&child, src)
                                    .trim_matches('"')
                                    .trim_matches('\'')
                                    .to_string();
                                if !path.is_empty() && path != "source" && path != "." {
                                    *refs.path_refs.entry(path).or_insert(0) += 1;
                                }
                            }
                        }
                    } else {
                        *refs.call.entry((name, None)).or_insert(0) += 1;
                    }
                }
            }
            "simple_expansion" | "expansion" => {
                let text = name_text(node, src);
                let var = text
                    .trim_start_matches('$')
                    .trim_start_matches('{')
                    .trim_end_matches('}')
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .next()
                    .unwrap_or("")
                    .to_string();
                if !var.is_empty() {
                    *refs.ident_refs.entry(var).or_insert(0) += 1;
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            visit_ref(&child, src, refs);
        }
    }

    visit_def(&tree.root_node(), src, &mut defs);
    visit_ref(&tree.root_node(), src, &mut refs);
    Ok((defs, refs))
}

fn extract_typescript(src: &[u8], is_tsx: bool) -> Result<(Vec<Definition>, Refs)> {
    let mut parser = TsParser::new();
    let lang = tree_sitter::Language::new(if is_tsx {
        tree_sitter_typescript::LANGUAGE_TSX
    } else {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT
    });
    parser
        .set_language(&lang)
        .map_err(|e| BlochError::LanguageError(format!("typescript: {e}")))?;
    let tree = match parser.parse(src, None) {
        Some(t) => t,
        None => return Ok((Vec::new(), Refs::default())),
    };

    let lang_name = if is_tsx { "tsx" } else { "typescript" };
    let mut defs = Vec::new();
    let mut refs = Refs::default();

    fn name_text(node: &Node, src: &[u8]) -> String {
        node_text(node, src).to_string()
    }

    fn visit_def(node: &Node, src: &[u8], lang_name: &str, defs: &mut Vec<Definition>) {
        match node.kind() {
            "function_declaration" | "generator_function_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = name_text(&name_node, src);
                    defs.push(Definition {
                        name: name.clone(),
                        kind: "function".to_string(),
                        is_pub: true,
                        sig: format!("function {}", name),
                        impl_target: None,
                        lang: lang_name.to_string(),
                    });
                }
            }
            "class_declaration" | "abstract_class_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = name_text(&name_node, src);
                    defs.push(Definition {
                        name: name.clone(),
                        kind: "class".to_string(),
                        is_pub: true,
                        sig: format!("class {}", name),
                        impl_target: None,
                        lang: lang_name.to_string(),
                    });
                }
            }
            "interface_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = name_text(&name_node, src);
                    defs.push(Definition {
                        name: name.clone(),
                        kind: "interface".to_string(),
                        is_pub: true,
                        sig: format!("interface {}", name),
                        impl_target: None,
                        lang: lang_name.to_string(),
                    });
                }
            }
            "type_alias_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = name_text(&name_node, src);
                    defs.push(Definition {
                        name: name.clone(),
                        kind: "type".to_string(),
                        is_pub: true,
                        sig: format!("type {}", name),
                        impl_target: None,
                        lang: lang_name.to_string(),
                    });
                }
            }
            "enum_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = name_text(&name_node, src);
                    defs.push(Definition {
                        name: name.clone(),
                        kind: "enum".to_string(),
                        is_pub: true,
                        sig: format!("enum {}", name),
                        impl_target: None,
                        lang: lang_name.to_string(),
                    });
                }
            }
            "method_definition" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = name_text(&name_node, src);
                    defs.push(Definition {
                        name: name.clone(),
                        kind: "method".to_string(),
                        is_pub: true,
                        sig: format!("method {}", name),
                        impl_target: None,
                        lang: lang_name.to_string(),
                    });
                }
            }
            "variable_declarator" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = name_text(&name_node, src);

                    let is_func = node
                        .child_by_field_name("value")
                        .map(|v| v.kind() == "arrow_function" || v.kind() == "function_expression")
                        .unwrap_or(false);
                    defs.push(Definition {
                        name: name.clone(),
                        kind: if is_func {
                            "function".to_string()
                        } else {
                            "variable".to_string()
                        },
                        is_pub: true,
                        sig: name.clone(),
                        impl_target: None,
                        lang: lang_name.to_string(),
                    });
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            visit_def(&child, src, lang_name, defs);
        }
    }

    fn visit_ref(node: &Node, src: &[u8], refs: &mut Refs) {
        match node.kind() {
            "call_expression" => {
                if let Some(func) = node.child_by_field_name("function") {
                    match func.kind() {
                        "identifier" => {
                            let name = name_text(&func, src);
                            *refs.call.entry((name, None)).or_insert(0) += 1;
                        }
                        "member_expression" => {
                            let obj = func.child_by_field_name("object");
                            let prop = func.child_by_field_name("property");
                            if let (Some(o), Some(p)) = (obj, prop) {
                                let pname = name_text(&p, src);
                                let oname = name_text(&o, src);
                                let qualifier = if o.kind() == "identifier" {
                                    Some(oname)
                                } else {
                                    None
                                };
                                *refs.call.entry((pname, qualifier)).or_insert(0) += 1;
                            }
                        }
                        _ => {}
                    }
                }
            }
            "new_expression" => {
                if let Some(ctor) = node.child_by_field_name("constructor") {
                    let name = name_text(&ctor, src);
                    *refs.call.entry((name, None)).or_insert(0) += 1;
                }
            }
            "import_statement" => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "string" {
                        let mod_name = name_text(&child, src)
                            .trim_matches('"')
                            .trim_matches('\'')
                            .trim_matches('`')
                            .to_string();
                        *refs.path_refs.entry(mod_name).or_insert(0) += 1;
                    }
                }
            }
            "identifier" => {
                let name = name_text(node, src);
                *refs.ident_refs.entry(name).or_insert(0) += 1;
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            visit_ref(&child, src, refs);
        }
    }

    visit_def(&tree.root_node(), src, lang_name, &mut defs);
    visit_ref(&tree.root_node(), src, &mut refs);
    Ok((defs, refs))
}

/// Find SCIP index file in repo root.
/// Checks: .scip/index.scip, index.scip
fn find_scip_index(root: &Path) -> Option<PathBuf> {
    let candidate = "index.scip";
    let path = root.join(".scip").join(candidate);
    if path.is_file() {
        return Some(path);
    }
    let path = root.join(candidate);
    if path.is_file() {
        return Some(path);
    }
    None
}

/// Check if SCIP index is stale (older than any source file).
/// Returns true if stale or if check fails (conservative: prefer tree-sitter).
fn is_scip_stale(index_path: &Path, root: &Path) -> bool {
    let index_mtime = match std::fs::metadata(index_path).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return true,
    };

    let extensions = ["rs", "py", "lua", "ts", "tsx", "kt", "sh", "nix", "md"];
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    if extensions.contains(&ext) {
                        if let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) {
                            if mtime > index_mtime {
                                return true;
                            }
                        }
                    }
                }
            }

            if path.is_dir() {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if ["src", "lib", "lua", "app"].contains(&name) {
                    if let Ok(sub_entries) = std::fs::read_dir(&path) {
                        for sub_entry in sub_entries.flatten() {
                            let sub_path = sub_entry.path();
                            if sub_path.is_file() {
                                if let Some(ext) = sub_path.extension().and_then(|e| e.to_str()) {
                                    if extensions.contains(&ext) {
                                        if let Ok(mtime) =
                                            sub_entry.metadata().and_then(|m| m.modified())
                                        {
                                            if mtime > index_mtime {
                                                return true;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    false
}

/// Parse SCIP symbol string into (name, kind).
/// SCIP symbols look like: `rust-analyzer cargo bloch 0.1.0 Args#`, `... main().`
/// Returns None for package symbols (e.g. `crate/`) which should be skipped.
fn scip_symbol_parse(symbol: &str) -> Option<(String, String)> {
    let descriptors = symbol.split_whitespace().last()?;

    if descriptors == "crate/" || descriptors.ends_with('/') {
        return None;
    }

    if descriptors.ends_with("().") {
        let name = descriptors.trim_end_matches("().").to_string();

        let name = if let Some(pos) = name.rfind('#') {
            name[pos + 1..].to_string()
        } else {
            name
        };
        if name.is_empty() {
            return None;
        }
        return Some((name, "function".to_string()));
    }

    if descriptors.ends_with('#') && !descriptors.contains("().") {
        let name = descriptors.trim_end_matches('#').to_string();

        let name = if let Some(pos) = name.rfind('#') {
            name[pos + 1..].to_string()
        } else {
            name
        };
        if name.is_empty() {
            return None;
        }
        return Some((name, "type".to_string()));
    }

    if descriptors.ends_with('.') {
        let without_dot = descriptors.trim_end_matches('.');

        let name = if let Some(pos) = without_dot.rfind('#') {
            without_dot[pos + 1..].to_string()
        } else {
            without_dot.to_string()
        };

        if name.is_empty() || name.contains('#') || name.contains('(') {
            return None;
        }
        return Some((name, "field".to_string()));
    }

    None
}

/// Legacy wrapper for compatibility
fn scip_symbol_name(symbol: &str) -> String {
    scip_symbol_parse(symbol)
        .map(|(n, _)| n)
        .unwrap_or_default()
}

/// Read SCIP index and extract definitions/references per file.
/// Returns None if no index found, index is stale, or parse fails.
/// The HashMap keys are relative file paths, values are (definitions, refs, language).
pub fn read_scip_index(root: &Path) -> Option<HashMap<String, ScipFileData>> {
    let index_path = find_scip_index(root)?;

    if is_scip_stale(&index_path, root) {
        eprintln!("bloch: SCIP index is stale, falling back to tree-sitter");
        return None;
    }

    let data = std::fs::read(&index_path).ok()?;
    let index: scip::types::Index = protobuf::Message::parse_from_bytes(&data).ok()?;

    let mut per_file: std::collections::HashMap<String, (Vec<Definition>, Refs, String)> =
        std::collections::HashMap::new();

    let mut symbol_defs: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();

    for document in &index.documents {
        for symbol_info in &document.symbols {
            let symbol = symbol_info.symbol.clone();
            if let Some((name, kind)) = scip_symbol_parse(&symbol) {
                symbol_defs.insert(symbol, (name, kind));
            }
        }
    }

    for document in &index.documents {
        let rel_path = document.relative_path.clone();

        let lang = Path::new(&rel_path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| match e {
                "rs" => "rust",
                "py" => "python",
                "lua" => "lua",
                "ts" => "typescript",
                "tsx" => "tsx",
                "kt" | "kts" => "kotlin",
                "sh" | "bash" => "bash",
                "nix" => "nix",
                "md" => "markdown",
                _ => "unknown",
            })
            .unwrap_or("unknown")
            .to_string();

        let mut defs = Vec::new();
        let mut refs = Refs::default();

        for symbol_info in &document.symbols {
            let symbol = symbol_info.symbol.clone();
            if let Some((name, kind)) = symbol_defs.get(&symbol) {
                defs.push(Definition {
                    name: name.clone(),
                    kind: kind.clone(),
                    is_pub: true,
                    sig: format!("{} (from SCIP)", name),
                    impl_target: None,
                    lang: lang.clone(),
                });
            }
        }

        for occurrence in &document.occurrences {
            let symbol = occurrence.symbol.clone();
            if symbol.is_empty() {
                continue;
            }
            let is_definition =
                (occurrence.symbol_roles & scip::types::SymbolRole::Definition as i32) != 0;
            if !is_definition {
                if let Some((name, _)) = symbol_defs.get(&symbol) {
                    *refs.ident_refs.entry(name.clone()).or_insert(0) += 1;
                } else {
                    let name = scip_symbol_name(&symbol);
                    if !name.is_empty() {
                        *refs.ident_refs.entry(name).or_insert(0) += 1;
                    }
                }
            }
        }

        if !defs.is_empty() || !refs.ident_refs.is_empty() {
            per_file.insert(rel_path, (defs, refs, lang));
        }
    }

    if per_file.is_empty() {
        return None;
    }

    Some(per_file)
}

fn extract_ruby_fastlane(text: &str) -> Vec<Definition> {
    let lane_re =
        Regex::new(r"(?:private_)?lane\s+:([a-zA-Z_][a-zA-Z0-9_]*)").expect("hardcoded lane regex");
    let plat_re =
        Regex::new(r"platform\s+:([a-zA-Z_][a-zA-Z0-9_]*)").expect("hardcoded platform regex");
    let def_re = Regex::new(r"def\s+([a-zA-Z_][a-zA-Z0-9_?!]*)").expect("hardcoded def regex");

    let mut defs = Vec::new();
    for line in text.lines() {
        let stripped = line.trim();
        if let Some(caps) = lane_re.captures(stripped) {
            let sig: String = stripped.chars().take(100).collect();
            defs.push(Definition {
                name: caps[1].to_string(),
                kind: "lane".to_string(),
                is_pub: !stripped.starts_with("private_"),
                sig,
                impl_target: None,
                lang: "ruby".to_string(),
            });
        }
        if let Some(caps) = plat_re.captures(stripped) {
            let sig: String = stripped.chars().take(100).collect();
            defs.push(Definition {
                name: caps[1].to_string(),
                kind: "platform".to_string(),
                is_pub: true,
                sig,
                impl_target: None,
                lang: "ruby".to_string(),
            });
        }
        if let Some(caps) = def_re.captures(stripped) {
            let sig: String = stripped.chars().take(100).collect();
            defs.push(Definition {
                name: caps[1].to_string(),
                kind: "fn".to_string(),
                is_pub: true,
                sig,
                impl_target: None,
                lang: "ruby".to_string(),
            });
        }
    }
    defs.truncate(50);
    defs
}

fn extract_toml(text: &str) -> Vec<Definition> {
    let re = Regex::new(r"^\[([^\]]+)\]").expect("hardcoded bracket regex");
    let mut defs = Vec::new();
    for line in text.lines() {
        let stripped = line.trim();
        if let Some(caps) = re.captures(stripped) {
            let sig: String = stripped.chars().take(100).collect();
            defs.push(Definition {
                name: caps[1].to_string(),
                kind: "table".to_string(),
                is_pub: true,
                sig,
                impl_target: None,
                lang: "toml".to_string(),
            });
        }
    }
    defs.truncate(30);
    defs
}

fn extract_yaml(text: &str) -> Vec<Definition> {
    let re = Regex::new(r"^([a-zA-Z_][a-zA-Z0-9_\-]*)\s*:").expect("hardcoded key regex");
    let mut defs = Vec::new();
    for line in text.lines() {
        if let Some(caps) = re.captures(line) {
            let sig: String = line.trim().chars().take(100).collect();
            defs.push(Definition {
                name: caps[1].to_string(),
                kind: "key".to_string(),
                is_pub: true,
                sig,
                impl_target: None,
                lang: "yaml".to_string(),
            });
        }
    }
    defs.truncate(30);
    defs
}

fn extract_generic(text: &str, lang: &str, rel: &str) -> Vec<Definition> {
    let lines: Vec<&str> = text.lines().collect();
    let non_empty = lines.iter().filter(|l| !l.trim().is_empty()).count();
    let name = Path::new(rel)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    vec![Definition {
        name,
        kind: "file".to_string(),
        is_pub: true,
        sig: format!("{} lines, {} non-empty", lines.len(), non_empty),
        impl_target: None,
        lang: lang.to_string(),
    }]
}

pub fn extract(path: &Path, rel: &str) -> Result<(Vec<Definition>, Refs, String)> {
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
        .unwrap_or_default();
    let basename = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    let src = match fs::read(path) {
        Ok(s) => s,
        Err(e) => return Err(BlochError::Io(e)),
    };
    let text = String::from_utf8_lossy(&src).to_string();
    let empty = Refs::default();

    if ext == ".rs" {
        let (defs, refs) = extract_rust(&src)?;
        return Ok((defs, refs, "rust".to_string()));
    }

    if [".md", ".markdown", ".mdown"].contains(&ext.as_str()) {
        let (defs, refs) = extract_markdown_ts(&src)?;
        return Ok((defs, refs, "markdown".to_string()));
    }

    if ext == ".nix" {
        let (defs, refs) = extract_nix(&src)?;
        return Ok((defs, refs, "nix".to_string()));
    }

    if ext == ".py" || ext == ".pyi" {
        let (defs, refs) = extract_python(&src)?;
        return Ok((defs, refs, "python".to_string()));
    }

    if ext == ".lua" {
        let (defs, refs) = extract_lua(&src)?;
        return Ok((defs, refs, "lua".to_string()));
    }

    if ext == ".rb"
        || ext == ".rake"
        || [
            "fastfile",
            "appfile",
            "deliverfile",
            "gymfile",
            "matchfile",
            "scanfile",
            "snapfile",
        ]
        .contains(&basename.as_str())
    {
        return Ok((extract_ruby_fastlane(&text), empty, "ruby".to_string()));
    }

    if ["build", "workspace"].contains(&basename.as_str())
        || [".bzl", ".bazel"].contains(&ext.as_str())
    {
        let re = Regex::new(r#"\s*name\s*=\s*["']([^"']+)["']"#).expect("hardcoded name regex");
        let mut defs = Vec::new();
        for line in text.lines() {
            if let Some(caps) = re.captures(line) {
                let sig: String = line.trim().chars().take(100).collect();
                defs.push(Definition {
                    name: caps[1].to_string(),
                    kind: "target".to_string(),
                    is_pub: true,
                    sig,
                    impl_target: None,
                    lang: "starlark".to_string(),
                });
            }
        }
        defs.truncate(30);
        if defs.is_empty() {
            return Ok((
                extract_generic(&text, "starlark", rel),
                empty,
                "starlark".to_string(),
            ));
        }
        return Ok((defs, empty, "starlark".to_string()));
    }

    if ext == ".toml" {
        let defs = extract_toml(&text);
        if defs.is_empty() {
            return Ok((
                extract_generic(&text, "toml", rel),
                empty,
                "toml".to_string(),
            ));
        }
        return Ok((defs, empty, "toml".to_string()));
    }

    if ext == ".yaml" || ext == ".yml" {
        let defs = extract_yaml(&text);
        if defs.is_empty() {
            return Ok((
                extract_generic(&text, "yaml", rel),
                empty,
                "yaml".to_string(),
            ));
        }
        return Ok((defs, empty, "yaml".to_string()));
    }

    if ext == ".json" && text.len() < 50000 {
        if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(&text)
        {
            let defs: Vec<Definition> = map
                .keys()
                .take(30)
                .map(|k| Definition {
                    name: k.clone(),
                    kind: "key".to_string(),
                    is_pub: true,
                    sig: format!("\"{}\": ...", k),
                    impl_target: None,
                    lang: "json".to_string(),
                })
                .collect();
            return Ok((defs, empty, "json".to_string()));
        }
        return Ok((
            extract_generic(&text, "json", rel),
            empty,
            "json".to_string(),
        ));
    }

    if [".sh", ".bash", ".zsh"].contains(&ext.as_str())
        || basename.starts_with("run")
        || basename.ends_with(".sh")
    {
        let (defs, refs) = extract_bash(&src)?;
        return Ok((defs, refs, "bash".to_string()));
    }

    if [".kt", ".kts"].contains(&ext.as_str()) {
        let (defs, refs) = extract_kotlin(&src)?;
        return Ok((defs, refs, "kotlin".to_string()));
    }

    if [".ts", ".mts", ".cts"].contains(&ext.as_str()) {
        let (defs, refs) = extract_typescript(&src, false)?;
        return Ok((defs, refs, "typescript".to_string()));
    }
    if ext == ".tsx" {
        let (defs, refs) = extract_typescript(&src, true)?;
        return Ok((defs, refs, "tsx".to_string()));
    }

    let lang = if ext.is_empty() {
        "text"
    } else {
        ext.trim_start_matches('.')
    };
    Ok((extract_generic(&text, lang, rel), empty, lang.to_string()))
}

/// Render top definitions as points on an ASCII Bloch sphere.
///
/// Each definition is mapped to (θ, φ):
/// - θ (polar from +z): derived from normalized probability.
///   p=1 → north pole (measure here first), p=0 → south pole.
/// - φ (azimuthal): derived from recency (x-axis) and centrality (y-axis).
///   East (+x) = recently modified, North (+y) = highly connected.
///
/// This gives a 3D view that a ranked list cannot: you see at a glance
/// which definitions are important AND fresh AND central, vs. important
/// but stale, or fresh but peripheral.
#[allow(clippy::needless_range_loop)]
pub fn render_sphere(
    ranked: &[(f64, f64, bool, String, Definition)],
    recency_scores: &std::collections::HashMap<String, f64>,
    visibility_scores: &std::collections::HashMap<String, f64>,
    max_points: usize,
) -> String {
    const WIDTH: usize = 50;
    const HEIGHT: usize = 25;
    const R: f64 = 11.0;

    let points: Vec<(f64, f64, String)> = ranked
        .iter()
        .take(max_points)
        .enumerate()
        .map(|(i, (_score, prob, _pub, rel, d))| {
            let theta = std::f64::consts::PI * (1.0 - prob.clamp(0.0, 1.0));

            let key = format!("{}::{}", rel, d.name);
            let rec = recency_scores.get(&key).copied().unwrap_or(0.5);
            let vis = visibility_scores.get(&key).copied().unwrap_or(0.5);
            let phi = (vis - 0.5).atan2(rec - 0.5);

            let label = format!("{}", (i + 1));
            let _ = label;
            (theta, phi, d.name.chars().take(12).collect::<String>())
        })
        .collect();

    let mut grid = vec![vec![' '; WIDTH]; HEIGHT];
    let cx = WIDTH as f64 / 2.0;
    let cy = HEIGHT as f64 / 2.0;

    for angle in 0..360 {
        let a = (angle as f64).to_radians();
        let x = (cx + R * 1.8 * a.cos()) as usize;
        let y = (cy + R * 0.9 * a.sin()) as usize;
        if x < WIDTH && y < HEIGHT && grid[y][x] == ' ' {
            grid[y][x] = '·';
        }
    }

    for y in 0..HEIGHT {
        let x = cx as usize;
        if x < WIDTH && grid[y][x] == ' ' {
            grid[y][x] = '│';
        }
    }

    let ey = cy as usize;
    for x in 0..WIDTH {
        if grid[ey][x] == ' ' {
            grid[ey][x] = '─';
        }
    }

    if (cx as usize) < WIDTH && (cy as usize) < HEIGHT {
        grid[cy as usize][cx as usize] = '┼';
    }

    for (i, (theta, phi, _name)) in points.iter().enumerate() {
        let x3d = theta.sin() * phi.cos();
        let y3d = theta.sin() * phi.sin();
        let z3d = theta.cos();

        let sx = cx + x3d * R * 1.8;
        let sy = cy - (z3d * 0.7 + y3d * 0.3) * R * 0.9;
        let px = sx as usize;
        let py = sy as usize;
        if px < WIDTH && py < HEIGHT {
            let ch = if i < 9 {
                (b'1' + i as u8) as char
            } else if i < 35 {
                (b'a' + (i - 9) as u8) as char
            } else {
                '*'
            };
            grid[py][px] = ch;
        }
    }

    let mut out = String::new();
    out.push_str("# Bloch sphere: top definitions by (relevance, recency, centrality)\n");
    out.push_str("#   +z (north pole) = highest probability — measure here first\n");
    out.push_str("#   +x (east) = recently modified    |  +y = highly connected\n");
    out.push_str("#\n");
    for row in grid {
        out.push_str("#   ");
        out.push_str(&row.iter().collect::<String>());
        out.push('\n');
    }
    out.push_str("#\n# Legend:\n");
    for (i, (_theta, _phi, name)) in points.iter().enumerate() {
        let ch = if i < 9 {
            ((b'1' + i as u8) as char).to_string()
        } else if i < 35 {
            ((b'a' + (i - 9) as u8) as char).to_string()
        } else {
            "*".to_string()
        };

        if let Some((_, prob, _, rel, _)) = ranked.get(i) {
            out.push_str(&format!("#   {} = {} (p={:.3})\n", ch, name, prob));
            let _ = rel;
        }
    }
    out
}

/// Detect active skills from `.ai/skills/` directory and skill references.
/// Returns a list of skill names.
pub fn detect_skills(root: &std::path::Path) -> Vec<String> {
    let mut skills = Vec::new();

    let skills_dir = root.join(".ai").join("skills");
    if let Ok(entries) = std::fs::read_dir(&skills_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            if let Some(name) = entry.file_name().to_str() {
                skills.push(name.to_string());
            }
        }
    }

    let workspace_skills = root.join("..").join("skills");
    if let Ok(entries) = std::fs::read_dir(&workspace_skills) {
        for entry in entries.filter_map(|e| e.ok()) {
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    if !skills.contains(&name.to_string()) {
                        skills.push(name.to_string());
                    }
                }
            }
        }
    }

    skills
}

/// Map a skill name to programming languages it covers.
/// E.g., "tiger-style-rust" -> ["rust"], "tiger-style-lua" -> ["lua"].
pub fn skill_languages(skill: &str) -> Vec<String> {
    let skill_lower = skill.to_lowercase();
    let mut langs = Vec::new();

    for lang in &[
        "rust",
        "python",
        "lua",
        "typescript",
        "tsx",
        "javascript",
        "go",
        "ruby",
        "nix",
        "bash",
        "shell",
        "c",
        "cpp",
        "zig",
        "kotlin",
        "markdown",
    ] {
        if skill_lower.contains(*lang) {
            langs.push(lang.to_string());
        }
    }

    if (skill_lower.contains("nvim") || skill_lower.contains("neovim"))
        && !langs.iter().any(|l| l == "lua")
    {
        langs.push("lua".to_string());
    }

    langs
}

/// Get skill boost factor for a definition's language.
/// Returns 1.5 if the language matches an active skill, 1.0 otherwise.
pub fn skill_boost(lang: &str, active_skills: &[String]) -> f64 {
    for skill in active_skills {
        let skill_langs = skill_languages(skill);
        if skill_langs.iter().any(|l| l == lang) {
            return 1.5;
        }
    }
    1.0
}

/// Configuration for mapping a repository.
///
/// Controls budget, skill-aware ranking, and SCIP integration.
#[derive(Debug, Default, Clone)]
pub struct MapConfig {
    /// Use SCIP index for precise definitions/references if available.
    /// Falls back to tree-sitter when no index exists or it is stale.
    pub use_scip: bool,
}

/// A ranked, budgeted map of a repository.
///
/// Produced by [`map_repo`]. Contains per-file definitions and references,
/// ready for ranking and rendering.
#[derive(Debug)]
pub struct RepoMap {
    /// Relative path -> file data (definitions, references, language).
    pub files: HashMap<String, FileData>,
    /// Total number of definitions across all files.
    pub total_definitions: usize,
    /// Languages present, with file counts.
    pub languages: HashMap<String, usize>,
    /// Whether SCIP index was used (vs tree-sitter fallback).
    pub used_scip: bool,
}

impl RepoMap {
    /// Total character count if all definitions were rendered.
    pub fn total_chars(&self) -> usize {
        self.files
            .values()
            .flat_map(|f| f.defs.iter())
            .map(|d| d.sig.len())
            .sum()
    }
}

/// Map a repository with the given configuration.
///
/// This is the primary entry point for library users. It handles:
/// - SCIP index reading (if `config.use_scip` and index available)
/// - Tree-sitter extraction per file (fallback)
/// - Skill detection and language mapping
///
/// Returns a [`RepoMap`] with per-file data ready for ranking.
pub fn map_repo(root: &Path, config: &MapConfig) -> Result<RepoMap> {
    let mut files: HashMap<String, FileData> = HashMap::new();

    let scip_result = if config.use_scip {
        read_scip_index(root)
    } else {
        None
    };

    let used_scip = scip_result.is_some();
    if let Some(scip_data) = scip_result {
        for (rel, (defs, refs, lang)) in scip_data {
            let modpath = if lang == "rust" {
                mod_segments(&rel)
            } else {
                Vec::new()
            };
            files.insert(
                rel,
                FileData {
                    defs,
                    refs,
                    modpath,
                    lang,
                },
            );
        }
    } else {
        for p in iter_all_files(root) {
            let rel = p
                .strip_prefix(root)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            let (defs, refs, lang) = extract(&p, &rel)?;
            let modpath = if lang == "rust" {
                mod_segments(&rel)
            } else {
                Vec::new()
            };
            files.insert(
                rel,
                FileData {
                    defs,
                    refs,
                    modpath,
                    lang,
                },
            );
        }
    }

    let total_definitions = files.values().map(|f| f.defs.len()).sum();
    let mut languages: HashMap<String, usize> = HashMap::new();
    for f in files.values() {
        *languages.entry(f.lang.clone()).or_insert(0) += 1;
    }

    Ok(RepoMap {
        files,
        total_definitions,
        languages,
        used_scip,
    })
}

/// Maximum characters for a single memory file read (bounded).
const MEMORY_READ_BUDGET: usize = 8000;

/// Read INDEX.md and current.md from .ai/memory/ in one bounded call.
/// Returns empty string if .ai/memory/ does not exist.
/// Contract: never reads more than MEMORY_READ_BUDGET chars per file.
pub fn memory_read(root: &Path) -> String {
    let mem_dir = root.join(".ai").join("memory");
    if !mem_dir.is_dir() {
        return String::new();
    }
    let mut out = String::new();
    for name in ["INDEX.md", "current.md"] {
        let path = mem_dir.join(name);
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let truncated: String = content.chars().take(MEMORY_READ_BUDGET).collect();
                out.push_str(&format!("## {}\n{}\n\n", name, truncated));
                if content.chars().count() > MEMORY_READ_BUDGET {
                    out.push_str("(truncated)\n\n");
                }
            }
            Err(_) => {
                out.push_str(&format!("## {} (missing)\n\n", name));
            }
        }
    }
    out
}

/// Write a timestamped inbox entry to .ai/memory/inbox/.
/// File name: <epoch>-<agent>-<topic>.md (sanitized).
/// Returns the created file name on success.
/// Contract: topic sanitized to [a-z0-9-]; agent defaults to "agent".
pub fn memory_inbox(root: &Path, agent: &str, topic: &str, content: &str) -> Result<String> {
    let inbox_dir = root.join(".ai").join("memory").join("inbox");
    std::fs::create_dir_all(&inbox_dir)?;

    let safe_topic: String = topic
        .to_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let safe_topic = if safe_topic.is_empty() {
        "note"
    } else {
        &safe_topic
    };
    let safe_agent: String = agent
        .to_lowercase()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-')
        .collect();
    let safe_agent = if safe_agent.is_empty() {
        "agent"
    } else {
        &safe_agent
    };

    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let fname = format!("{}-{}-{}.md", epoch, safe_agent, safe_topic);
    let path = inbox_dir.join(&fname);
    std::fs::write(&path, content)?;
    Ok(fname)
}

/// Promote an inbox entry to a curated memory file.
/// Appends inbox/<file> content to .ai/memory/<target>, then moves the
/// inbox file to .ai/memory/archive/.
/// Contract: target must be decisions.md, patterns.md, current.md, or
/// lessons.md (the roadmap learning-loop target, v0.2).
/// Rejects path traversal in file name.
pub fn memory_promote(root: &Path, file: &str, target: &str) -> Result<String> {
    const ALLOWED: &[&str] = &["decisions.md", "patterns.md", "current.md", "lessons.md"];
    if !ALLOWED.contains(&target) {
        return Err(BlochError::GitFailed(format!(
            "invalid target '{}': must be one of {}",
            target,
            ALLOWED.join(", ")
        )));
    }
    if file.contains('/') || file.contains('\\') || file.starts_with('.') {
        return Err(BlochError::InvalidPath(format!(
            "invalid inbox file name '{}'",
            file
        )));
    }
    let mem_dir = root.join(".ai").join("memory");
    let src = mem_dir.join("inbox").join(file);
    let content = std::fs::read_to_string(&src)?;

    let dest = mem_dir.join(target);
    let existing = std::fs::read_to_string(&dest).unwrap_or_default();
    let merged = if existing.is_empty() {
        content.clone()
    } else {
        format!("{}\n\n---\n\n{}", existing.trim_end(), content.trim())
    };
    std::fs::write(&dest, merged)?;

    let archive_dir = mem_dir.join("archive");
    std::fs::create_dir_all(&archive_dir)?;
    std::fs::rename(&src, archive_dir.join(file))?;
    Ok(format!("promoted {} -> {}", file, target))
}

/// A single todo item. IDs are 1-based positional.
#[derive(Debug, Clone, PartialEq)]
pub struct TodoItem {
    pub id: usize,
    pub text: String,
    pub done: bool,
}

/// Parse .ai/todo.md into items. Format: `- [ ] text` / `- [x] text`.
/// Non-matching lines are ignored.
pub fn todo_parse(content: &str) -> Vec<TodoItem> {
    content
        .lines()
        .filter_map(|line| {
            let t = line.trim();
            if let Some(rest) = t.strip_prefix("- [ ] ") {
                Some((false, rest))
            } else if let Some(rest) = t.strip_prefix("- [x] ") {
                Some((true, rest))
            } else {
                None
            }
        })
        .enumerate()
        .map(|(i, (done, text))| TodoItem {
            id: i + 1,
            text: text.to_string(),
            done,
        })
        .collect()
}

/// Render todo items to .ai/todo.md format.
pub fn todo_render(items: &[TodoItem]) -> String {
    let mut out: String = items
        .iter()
        .map(|it| format!("- [{}] {}", if it.done { "x" } else { " " }, it.text))
        .collect::<Vec<_>>()
        .join("\n");
    out.push('\n');
    out
}

fn todo_path(root: &Path) -> std::path::PathBuf {
    root.join(".ai").join("todo.md")
}

fn todo_load(root: &Path) -> Vec<TodoItem> {
    std::fs::read_to_string(todo_path(root))
        .map(|c| todo_parse(&c))
        .unwrap_or_default()
}

fn todo_save(root: &Path, items: &[TodoItem]) -> Result<()> {
    let path = todo_path(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, todo_render(items))?;
    Ok(())
}

/// Add a todo item. Returns the new item's ID.
pub fn todo_add(root: &Path, text: &str) -> Result<usize> {
    let mut items = todo_load(root);
    let id = items.len() + 1;
    items.push(TodoItem {
        id,
        text: text.to_string(),
        done: false,
    });
    todo_save(root, &items)?;
    Ok(id)
}

/// List all todo items.
pub fn todo_list(root: &Path) -> Vec<TodoItem> {
    todo_load(root)
}

/// Mark a todo item done. Errors if the ID does not exist.
pub fn todo_done(root: &Path, id: usize) -> Result<()> {
    let mut items = todo_load(root);
    let item = items
        .iter_mut()
        .find(|it| it.id == id)
        .ok_or_else(|| BlochError::GitFailed(format!("no todo with id {}", id)))?;
    item.done = true;
    todo_save(root, &items)
}

/// Remove a todo item and renumber the rest (IDs stay dense, positional).
pub fn todo_drop(root: &Path, id: usize) -> Result<()> {
    let mut items = todo_load(root);
    let pos = items
        .iter()
        .position(|it| it.id == id)
        .ok_or_else(|| BlochError::GitFailed(format!("no todo with id {}", id)))?;
    items.remove(pos);
    for (i, it) in items.iter_mut().enumerate() {
        it.id = i + 1;
    }
    todo_save(root, &items)
}

/// A TODO/FIXME/etc marker found in source code.
#[derive(Debug, Clone, PartialEq)]
pub struct CodeMarker {
    /// Relative path from repo root
    pub file: String,
    /// 1-based line number
    pub line: usize,
    /// Marker kind: TODO, FIXME, HACK, XXX, BUG
    pub kind: String,
    /// Text after the marker
    pub text: String,
}

/// Marker kinds to scan for (alphabetical).
const MARKER_KINDS: &[&str] = &["BUG", "FIXME", "HACK", "TODO", "XXX"];

/// Scan the repo for TODO/FIXME/HACK/XXX/BUG markers in comments.
/// Returns markers sorted by (file, line). Skips binary files, target/,
/// .git/, node_modules/, and files over 1MB.
/// Contract: bounded — max 5000 markers returned.
pub fn todo_scan(root: &Path) -> Vec<CodeMarker> {
    const MAX_MARKERS: usize = 5000;
    let mut markers = Vec::new();

    for path in iter_all_files(root) {
        if markers.len() >= MAX_MARKERS {
            break;
        }
        let rel = match path.strip_prefix(root) {
            Ok(r) => r.to_string_lossy().to_string(),
            Err(_) => continue,
        };

        if rel.starts_with(".ai/") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for (i, line) in content.lines().enumerate() {
            for kind in MARKER_KINDS {
                let upper = line.to_uppercase();
                if let Some(pos) = upper.find(kind) {
                    let before_ok = pos == 0
                        || line[..pos]
                            .chars()
                            .last()
                            .is_none_or(|ch| !ch.is_alphanumeric());
                    let after = &line[pos + kind.len()..];
                    let after_ok = after.is_empty()
                        || [':', ' ', '(', '[', '-'].contains(&after.chars().next().unwrap());
                    if before_ok && after_ok {
                        let text = after
                            .trim_start_matches([':', ' ', '(', '[', '-'])
                            .trim()
                            .to_string();
                        markers.push(CodeMarker {
                            file: rel.clone(),
                            line: i + 1,
                            kind: kind.to_string(),
                            text: if text.is_empty() {
                                "(no description)".to_string()
                            } else {
                                text.chars().take(120).collect()
                            },
                        });
                        break;
                    }
                }
            }
        }
    }

    markers.sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
    markers
}

/// Extract the [file] source ref from a synced todo's text.
/// Synced todos have format: `[path/to/file.rs] KIND: description`
fn todo_source_ref(text: &str) -> Option<String> {
    let t = text.trim();
    if !t.starts_with('[') {
        return None;
    }
    t.find(']').map(|end| t[1..end].to_string())
}

/// Reconcile .ai/todo.md with markers found in the repo.
/// - Adds new markers as todos (format: `[file] KIND: text`)
/// - Marks done any synced todo whose marker no longer exists
/// - Leaves manual todos (no source ref) untouched
///
/// Returns `(added, completed)` counts.
///
/// Contract: never deletes todos; only adds or marks done.
pub fn todo_sync(root: &Path) -> Result<(usize, usize)> {
    let markers = todo_scan(root);
    let mut items = todo_load(root);

    let mut added = 0;
    let mut completed = 0;

    for m in &markers {
        let key = format!("[{}]", m.file);
        let already = items.iter().any(|it| {
            it.text.starts_with(&key)
                && it.text.contains(&m.kind)
                && it
                    .text
                    .contains(m.text.chars().take(40).collect::<String>().as_str())
        });
        if !already {
            items.push(TodoItem {
                id: 0,
                text: format!("[{}] {}: {}", m.file, m.kind, m.text),
                done: false,
            });
            added += 1;
        }
    }

    for it in items.iter_mut() {
        if it.done {
            continue;
        }
        if let Some(src_file) = todo_source_ref(&it.text) {
            let still_exists = markers.iter().any(|m| {
                m.file == src_file
                    && it.text.contains(&m.kind)
                    && it
                        .text
                        .contains(m.text.chars().take(40).collect::<String>().as_str())
            });
            if !still_exists {
                let full = root.join(&src_file);
                if full.exists() {
                    it.done = true;
                    completed += 1;
                }
            }
        }
    }

    for (i, it) in items.iter_mut().enumerate() {
        it.id = i + 1;
    }

    todo_save(root, &items)?;
    Ok((added, completed))
}

/// Ahead/behind counts vs the upstream tracking branch.
/// Returns (0, 0) if no upstream or not a git repo.
pub fn git_ahead_behind(root: &Path) -> (usize, usize) {
    let out = run_git(
        root,
        &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"],
    );
    let mut parts = out.split_whitespace();
    let ahead = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let behind = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (ahead, behind)
}

/// Detailed WIP: per-file diff stats for uncommitted changes.
/// Bounded to 30 stat lines per section, 20 untracked files.
pub fn git_wip_detail(root: &Path) -> String {
    let mut lines = vec!["## wip detail".to_string()];
    let stat = run_git(root, &["diff", "--stat", "--", "."]);
    if !stat.is_empty() {
        lines.push("unstaged:".to_string());
        for l in stat.lines().take(30) {
            lines.push(format!("  {}", l));
        }
    }
    let stat_cached = run_git(root, &["diff", "--cached", "--stat", "--", "."]);
    if !stat_cached.is_empty() {
        lines.push("staged:".to_string());
        for l in stat_cached.lines().take(30) {
            lines.push(format!("  {}", l));
        }
    }
    let untracked = run_git(root, &["ls-files", "--others", "--exclude-standard"]);
    let untracked: Vec<&str> = untracked.lines().filter(|l| !l.is_empty()).collect();
    if !untracked.is_empty() {
        lines.push(format!("untracked ({}):", untracked.len()));
        for l in untracked.iter().take(20) {
            lines.push(format!("  {}", l));
        }
        if untracked.len() > 20 {
            lines.push(format!("  ... and {} more", untracked.len() - 20));
        }
    }
    if lines.len() == 1 {
        lines.push("tree: clean".to_string());
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_root(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("bloch-test-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn todo_parse_roundtrip() {
        let text = "- [ ] write tests\n- [x] fix bug\nnot a todo\n- [ ] ship it\n";
        let items = todo_parse(text);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].id, 1);
        assert!(!items[0].done);
        assert_eq!(items[0].text, "write tests");
        assert!(items[1].done);
        assert_eq!(items[2].id, 3);
        let rendered = todo_render(&items);
        assert_eq!(
            rendered,
            "- [ ] write tests\n- [x] fix bug\n- [ ] ship it\n"
        );
    }

    #[test]
    fn todo_crud() {
        let root = tmp_root("todo");
        let id1 = todo_add(&root, "first task").unwrap();
        let id2 = todo_add(&root, "second task").unwrap();
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);

        let items = todo_list(&root);
        assert_eq!(items.len(), 2);

        todo_done(&root, 1).unwrap();
        let items = todo_list(&root);
        assert!(items[0].done);
        assert!(!items[1].done);

        assert!(todo_done(&root, 99).is_err());

        todo_drop(&root, 1).unwrap();
        let items = todo_list(&root);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, 1);
        assert_eq!(items[0].text, "second task");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn memory_inbox_sanitizes_names() {
        let root = tmp_root("inbox");
        let fname = memory_inbox(&root, "Test Agent!", "My Topic: Here?", "content").unwrap();
        assert!(
            fname.ends_with("-testagent-my-topic-here.md"),
            "got {}",
            fname
        );
        assert!(!fname.contains(' '));
        assert!(!fname.contains(':'));
        assert!(!fname.contains('?'));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn memory_inbox_empty_topic_defaults() {
        let root = tmp_root("inbox2");
        let fname = memory_inbox(&root, "", "!!!", "content").unwrap();
        assert!(fname.contains("-agent-note.md"), "got {}", fname);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn memory_promote_moves_to_archive() {
        let root = tmp_root("promote");
        let mem_dir = root.join(".ai").join("memory");
        fs::create_dir_all(mem_dir.join("inbox")).unwrap();
        fs::write(
            mem_dir.join("inbox").join("123-agent-test.md"),
            "new insight",
        )
        .unwrap();
        fs::write(mem_dir.join("decisions.md"), "old decision").unwrap();

        let msg = memory_promote(&root, "123-agent-test.md", "decisions.md").unwrap();
        assert!(msg.contains("promoted"));

        let decisions = fs::read_to_string(mem_dir.join("decisions.md")).unwrap();
        assert!(decisions.contains("old decision"));
        assert!(decisions.contains("new insight"));

        assert!(mem_dir.join("archive").join("123-agent-test.md").exists());
        assert!(!mem_dir.join("inbox").join("123-agent-test.md").exists());

        assert!(memory_promote(&root, "x.md", "evil.md").is_err());

        assert!(memory_promote(&root, "../evil.md", "decisions.md").is_err());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn memory_read_bounded() {
        let root = tmp_root("memread");
        assert_eq!(memory_read(&root), "");

        let mem_dir = root.join(".ai").join("memory");
        fs::create_dir_all(&mem_dir).unwrap();
        fs::write(mem_dir.join("INDEX.md"), "# Index").unwrap();

        let out = memory_read(&root);
        assert!(out.contains("## INDEX.md"));
        assert!(out.contains("# Index"));
        assert!(out.contains("## current.md (missing)"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn todo_scan_finds_markers() {
        let root = tmp_root("scan");
        fs::write(
            root.join("a.rs"),
            "// TODO: fix this\n// FIXME: and this\nlet x = 1;\n",
        )
        .unwrap();
        fs::write(
            root.join("b.py"),
            "# HACK: workaround\n# not a marker: METHODODO\n",
        )
        .unwrap();
        let markers = todo_scan(&root);

        assert_eq!(markers.len(), 3, "got {:?}", markers);
        assert_eq!(markers[0].file, "a.rs");
        assert_eq!(markers[0].kind, "TODO");
        assert_eq!(markers[0].line, 1);
        assert!(markers[0].text.contains("fix this"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn todo_sync_adds_and_completes() {
        let root = tmp_root("sync");
        fs::write(root.join("a.rs"), "// TODO: new task\n").unwrap();

        let (added, completed) = todo_sync(&root).unwrap();
        assert_eq!(added, 1);
        assert_eq!(completed, 0);
        let items = todo_list(&root);
        assert_eq!(items.len(), 1);
        assert!(items[0].text.contains("[a.rs]"));
        assert!(items[0].text.contains("TODO"));

        let (added, completed) = todo_sync(&root).unwrap();
        assert_eq!(added, 0);
        assert_eq!(completed, 0);

        fs::write(root.join("a.rs"), "let x = 1;\n").unwrap();
        let (added, completed) = todo_sync(&root).unwrap();
        assert_eq!(added, 0);
        assert_eq!(completed, 1);
        let items = todo_list(&root);
        assert!(items[0].done);

        todo_add(&root, "manual task").unwrap();
        fs::write(root.join("b.rs"), "// FIXME: another\n").unwrap();
        let (added, completed) = todo_sync(&root).unwrap();
        assert_eq!(added, 1);
        assert_eq!(completed, 0);
        let items = todo_list(&root);
        let manual = items.iter().find(|i| i.text == "manual task").unwrap();
        assert!(!manual.done);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn git_ahead_behind_non_repo_is_zero() {
        let root = tmp_root("git-nonrepo");
        assert_eq!(git_ahead_behind(&root), (0, 0));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn git_wip_detail_clean_tree() {
        let root = tmp_root("wip");

        let out = git_wip_detail(&root);
        assert!(out.contains("tree: clean"), "got {}", out);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn markdown_extracts_headers() {
        let text = "# Title\n## Section\n### Sub\n\nBody text";
        let (defs, _) = extract_markdown_ts(text.as_bytes()).unwrap();
        assert_eq!(defs.len(), 3);
        assert_eq!(defs[0].name, "Title");
        assert_eq!(defs[0].kind, "h1");
        assert_eq!(defs[1].kind, "h2");
        assert_eq!(defs[2].kind, "h3");
        assert!(defs.iter().all(|d| d.lang == "markdown"));
    }

    #[test]
    fn markdown_strips_formatting() {
        let text = "# **Bold** and `code` title";
        let (defs, _) = extract_markdown_ts(text.as_bytes()).unwrap();
        assert_eq!(defs.len(), 1);

        assert!(defs[0].name.contains("Bold"));
    }

    #[test]
    fn nix_extracts_top_level_attrs() {
        let text = "{ pkgs, lib }:\n{\n  hello = pkgs.hello;\n  version = \"1.0\";\n}";
        let (defs, _) = extract_nix(text.as_bytes()).unwrap();
        assert!(defs.iter().any(|d| d.name == "hello"));
        assert!(defs.iter().any(|d| d.name == "version"));
        assert!(defs.iter().all(|d| d.lang == "nix"));
    }

    #[test]
    fn python_extracts_functions_and_classes() {
        let text = "def hello():\n    pass\n\nclass World:\n    pass\n\ndef _private():\n    pass";
        let (defs, _) = extract_python(text.as_bytes()).unwrap();
        let hello = defs.iter().find(|d| d.name == "hello").unwrap();
        assert_eq!(hello.kind, "function");
        assert!(hello.is_pub);
        let world = defs.iter().find(|d| d.name == "World").unwrap();
        assert_eq!(world.kind, "class");
        let priv_fn = defs.iter().find(|d| d.name == "_private").unwrap();
        assert!(!priv_fn.is_pub);
    }

    #[test]
    fn lua_extracts_functions() {
        let text = "function foo()\nend\n\nfunction bar.baz()\nend";
        let (defs, _) = extract_lua(text.as_bytes()).unwrap();
        assert!(defs.iter().any(|d| d.name == "foo"));
        assert!(defs.iter().any(|d| d.name == "bar.baz"));
        assert!(defs.iter().all(|d| d.lang == "lua"));
    }

    #[test]
    fn rust_extracts_struct_and_impl() {
        let src =
            b"pub struct Foo { x: i32 }\nimpl Foo {\n    pub fn new() -> Self { Foo { x: 0 } }\n}";
        let (defs, _refs) = extract_rust(src).unwrap();
        let foo = defs
            .iter()
            .find(|d| d.name == "Foo" && d.kind == "struct")
            .unwrap();
        assert!(foo.is_pub);
        let new_fn = defs.iter().find(|d| d.name == "new").unwrap();
        assert_eq!(new_fn.kind, "fn");
        assert_eq!(new_fn.impl_target.as_deref(), Some("Foo"));
    }

    #[test]
    fn rust_counts_references() {
        let src = b"struct Foo;\nfn bar() { let _ = Foo; bar(); }";
        let (_defs, refs) = extract_rust(src).unwrap();

        assert!(refs.type_refs.get("Foo").unwrap_or(&0) >= &1);

        let bare_calls: usize = refs
            .call
            .iter()
            .filter(|((n, q), _)| n == "bar" && q.is_none())
            .map(|(_, c)| *c)
            .sum();
        assert!(bare_calls >= 1);
    }

    #[test]
    fn toml_extracts_tables() {
        let text = "[package]\nname = \"foo\"\n\n[dependencies]\nserde = \"1\"";
        let defs = extract_toml(text);
        assert!(defs.iter().all(|d| d.kind == "table" && d.lang == "toml"));
        assert!(defs.iter().any(|d| d.name == "package"));
        assert!(defs.iter().any(|d| d.name == "dependencies"));
    }

    #[test]
    fn yaml_extracts_top_level_keys() {
        let text = "name: foo\nversion: 1.0\n  nested: should_not_match";
        let defs = extract_yaml(text);
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"name"));
        assert!(names.contains(&"version"));
        assert!(!names.contains(&"nested"));
    }

    #[test]
    fn shell_extracts_functions() {
        let text = "#!/bin/bash\nhello() {\n  echo hi\n}\n\nworld() {\n  echo bye\n}";
        let (defs, _refs) = extract_bash(text.as_bytes()).unwrap();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"hello"));
        assert!(names.contains(&"world"));
    }

    #[test]
    fn generic_fallback_counts_lines() {
        let text = "line1\n\nline3\nline4";
        let defs = extract_generic(text, "text", "foo.txt");
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].kind, "file");
        assert!(defs[0].sig.contains("4 lines"));
        assert!(defs[0].sig.contains("3 non-empty"));
    }

    #[test]
    fn mod_segments_extracts_crate_path() {
        let segs = mod_segments("crates/foo/src/bar/baz.rs");
        assert_eq!(segs, vec!["foo", "baz"]);
    }

    #[test]
    fn mod_segments_skips_lib_main_mod() {
        let segs = mod_segments("crates/foo/src/lib.rs");
        assert_eq!(segs, vec!["foo"]);
    }

    #[test]
    fn bloch_error_displays() {
        let e = BlochError::GitFailed("no repo".to_string());
        assert_eq!(format!("{}", e), "Git operation failed: no repo");
        let e = BlochError::ParseFailed {
            path: "foo.rs".to_string(),
            msg: "bad syntax".to_string(),
        };
        assert!(format!("{}", e).contains("foo.rs"));
    }

    #[test]
    fn scip_staleness_detects_newer_source() {
        use std::fs::{self, FileTimes};
        use std::time::{Duration, SystemTime};

        let tmp = std::env::temp_dir().join(format!("bloch-scip-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("src")).unwrap();
        fs::create_dir_all(tmp.join(".scip")).unwrap();

        let index_path = tmp.join(".scip").join("index.scip");
        let src_path = tmp.join("src").join("main.rs");

        fs::write(&index_path, b"fake").unwrap();
        fs::write(&src_path, b"fn main() {}").unwrap();

        let old_time = SystemTime::now() - Duration::from_secs(3600);
        let new_time = SystemTime::now();
        fs::set_times(&index_path, FileTimes::new().set_modified(old_time)).unwrap();
        fs::set_times(&src_path, FileTimes::new().set_modified(new_time)).unwrap();

        assert!(is_scip_stale(&index_path, &tmp));

        fs::set_times(&src_path, FileTimes::new().set_modified(old_time)).unwrap();
        fs::set_times(&index_path, FileTimes::new().set_modified(new_time)).unwrap();
        assert!(!is_scip_stale(&index_path, &tmp));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn memory_rejects_path_traversal() {
        use std::path::PathBuf;

        let evil_root = PathBuf::from("/tmp/foo/../../etc");
        let cached = CachedMap {
            timestamp: "test".to_string(),
            git_head: String::new(),
            files: std::collections::HashMap::new(),
            top_defs: Vec::new(),
            languages: std::collections::HashMap::new(),
        };
        let result = write_memory_cache(&evil_root, &cached);
        assert!(result.is_err());
        match result {
            Err(BlochError::InvalidPath(_)) => {}
            _ => panic!("expected InvalidPath error"),
        }
    }

    #[test]
    fn memory_atomic_write_no_corrupt() {
        use std::fs;

        let tmp = std::env::temp_dir().join(format!("bloch-atomic-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let cached = CachedMap {
            timestamp: "2026-10-04T00:00:00Z".to_string(),
            git_head: "abc123".to_string(),
            files: std::collections::HashMap::new(),
            top_defs: Vec::new(),
            languages: std::collections::HashMap::new(),
        };

        let path = write_memory_cache(&tmp, &cached).unwrap();
        assert!(path.exists());

        let tmp_file = path.with_extension("tmp");
        assert!(!tmp_file.exists());

        let content = fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["timestamp"], "2026-10-04T00:00:00Z");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn extract_handles_malformed_utf8() {
        use std::fs;
        let tmp = std::env::temp_dir().join(format!("bloch-utf8-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("bad.rs");

        fs::write(&path, b"fn main() {\xff\xfe invalid }").unwrap();
        let result = extract(&path, "bad.rs");
        assert!(result.is_ok());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn extract_handles_nul_bytes() {
        use std::fs;
        let tmp = std::env::temp_dir().join(format!("bloch-nul-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("nul.py");
        fs::write(&path, b"def hello():\x00\x00\n    pass").unwrap();
        let result = extract(&path, "nul.py");
        assert!(result.is_ok());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn extract_handles_binary_file() {
        use std::fs;
        let tmp = std::env::temp_dir().join(format!("bloch-bin-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("data.bin");

        let data: Vec<u8> = (0..1024).map(|i| (i * 7 % 256) as u8).collect();
        fs::write(&path, &data).unwrap();
        let result = extract(&path, "data.bin");
        assert!(result.is_ok());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn extract_handles_empty_file() {
        use std::fs;
        let tmp = std::env::temp_dir().join(format!("bloch-empty-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("empty.rs");
        fs::write(&path, b"").unwrap();
        let result = extract(&path, "empty.rs");
        assert!(result.is_ok());
        let (defs, _, _) = result.unwrap();
        assert!(defs.is_empty());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn map_repo_handles_symlink_loop() {
        use std::fs;
        use std::os::unix::fs::symlink;
        let tmp = std::env::temp_dir().join(format!("bloch-symlink-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("src")).unwrap();
        fs::write(tmp.join("src").join("main.rs"), b"fn main() {}").unwrap();

        let _ = symlink("..", tmp.join("src").join("loop"));
        let config = MapConfig::default();
        let result = map_repo(&tmp, &config);

        assert!(result.is_ok());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn map_repo_handles_unicode_paths() {
        use std::fs;
        let tmp = std::env::temp_dir().join(format!("bloch-unicode-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        fs::write(tmp.join("文件.rs"), "fn 主() {}".as_bytes()).unwrap();
        let config = MapConfig::default();
        let result = map_repo(&tmp, &config);
        assert!(result.is_ok());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn map_repo_handles_empty_repo() {
        use std::fs;
        let tmp = std::env::temp_dir().join(format!("bloch-empty-repo-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let config = MapConfig::default();
        let result = map_repo(&tmp, &config);
        assert!(result.is_ok());
        let map = result.unwrap();
        assert!(map.files.is_empty());
        assert_eq!(map.total_definitions, 0);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn map_repo_handles_nonexistent_root() {
        use std::path::PathBuf;
        let config = MapConfig::default();
        let result = map_repo(&PathBuf::from("/nonexistent/path/12345"), &config);

        let _ = result;
    }

    #[test]
    fn read_scip_index_handles_malformed_protobuf() {
        use std::fs;
        let tmp = std::env::temp_dir().join(format!("bloch-scip-bad-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join(".scip")).unwrap();

        fs::write(
            tmp.join(".scip").join("index.scip"),
            b"\xff\xfe\x00 garbage not protobuf",
        )
        .unwrap();

        let result = read_scip_index(&tmp);
        assert!(result.is_none());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn read_scip_index_handles_truncated_file() {
        use std::fs;
        let tmp = std::env::temp_dir().join(format!("bloch-scip-trunc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join(".scip")).unwrap();

        fs::write(tmp.join(".scip").join("index.scip"), b"\x0a\x10\x0a\x0c").unwrap();
        let result = read_scip_index(&tmp);

        let _ = result;
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn read_scip_index_handles_empty_file() {
        use std::fs;
        let tmp = std::env::temp_dir().join(format!("bloch-scip-empty-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join(".scip")).unwrap();
        fs::write(tmp.join(".scip").join("index.scip"), b"").unwrap();
        let result = read_scip_index(&tmp);
        let _ = result;
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn skill_boost_handles_hostile_names() {
        assert_eq!(skill_boost("rust", &["".to_string()]), 1.0);

        assert_eq!(skill_boost("rust", &["../../etc".to_string()]), 1.0);

        assert_eq!(skill_boost("rust", &["tiger\x00style".to_string()]), 1.0);

        let long = "a".repeat(10000);
        assert_eq!(skill_boost("rust", &[long]), 1.0);

        assert!(skill_boost("rust", &["tiger-style-rust".to_string()]) > 1.0);
    }

    #[test]
    fn detect_skills_handles_missing_dir() {
        use std::path::PathBuf;

        let result = detect_skills(&PathBuf::from("/nonexistent/12345"));
        assert!(result.is_empty());
    }

    #[test]
    fn kotlin_extracts_functions_and_classes() {
        let code = b"
fun hello(name: String): String {
    return \"Hello, $name\"
}

class Greeter(val greeting: String) {
    fun greet() = println(greeting)
}

object Singleton {
    const val VERSION = \"1.0\"
}
";
        let (defs, _) = extract_kotlin(code).unwrap();
        assert!(defs
            .iter()
            .any(|d| d.name == "hello" && d.kind == "function"));
        assert!(defs
            .iter()
            .any(|d| d.name == "Greeter" && d.kind == "class"));
        assert!(defs
            .iter()
            .any(|d| d.name == "Singleton" && d.kind == "object"));
        assert!(defs.iter().all(|d| d.lang == "kotlin"));
    }

    #[test]
    fn read_memory_cache_distinguishes_missing_vs_corrupt() {
        use std::fs;
        let tmp = std::env::temp_dir().join(format!("bloch-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let result = read_memory_cache(&tmp).unwrap();
        assert!(result.is_none());

        let mem_dir = tmp.join(".ai").join("memory");
        fs::create_dir_all(&mem_dir).unwrap();
        fs::write(mem_dir.join("bloch-last.json"), b"{ invalid json !!!").unwrap();
        let result = read_memory_cache(&tmp);
        assert!(result.is_err());
        match result {
            Err(BlochError::JsonError(msg)) => assert!(msg.contains("corrupt cache")),
            _ => panic!("expected JsonError"),
        }

        let cached = CachedMap {
            timestamp: "2026-10-04T00:00:00Z".to_string(),
            git_head: String::new(),
            files: std::collections::HashMap::new(),
            top_defs: Vec::new(),
            languages: std::collections::HashMap::new(),
        };
        write_memory_cache(&tmp, &cached).unwrap();
        let result = read_memory_cache(&tmp).unwrap();
        assert!(result.is_some());

        let _ = fs::remove_dir_all(&tmp);
    }
}
