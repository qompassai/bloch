//! MCP server (bloch v0.3, SPEC §8 / LINKING Part 2): the fleet-facing
//! surface over the repos' `.ai/` files.
//!
//! Law (LINKING §2.2, SPEC §9): files stay canonical — the server is an
//! index + query + write-gate, never the store. The index is rebuilt from
//! the files, so dropping it and rebuilding yields identical answers.
//! Pax's private memory (`~/MEMORY.md`, `~/memory/`) is never indexed or
//! served; roots pointing at it are refused at startup. No secrets are
//! indexed (secret-shaped file names are excluded). Lane law is enforced
//! here, in code: nsfw-labeled entries are never returned to
//! general-only callers, and proposing nsfw content into a
//! general-lane repo is a typed refusal.
//!
//! Transport: stdio JSON-RPC on the rmcp SDK behind the era guard
//! ([`crate::guard`]), modern era only (2026-07-28) since v0.4 —
//! D-113 supersedes the dual-era posture of LINKING §2.3. The SDK
//! owns framing; the domain logic in this module (index, lane law,
//! gated writes) is unchanged from the hand-rolled era.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities,
    ServerConfig as RmcpServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};

use crate::roadmap::{
    attempt_history, decide_content_label, label_frontmatter, now_epoch, parse_label_frontmatter,
    read_attempts, repo_lane, roadmap_status, ContentLabel, LabelStamp, Lane, LiveAttempt,
    TaskStatus,
};
use crate::{memory_inbox, memory_promote, run_git, BlochError, Result};

/// Maximum bytes accepted for one JSON-RPC line. The bound itself
/// is owned by the era guard since v0.4; this name is kept as the
/// server's public handle on it.
pub const RPC_MAX_LINE_BYTES: usize = crate::guard::MAX_LINE_BYTES;
/// Maximum characters accepted for a search query.
pub const SEARCH_QUERY_MAX: usize = 256;
/// Maximum results returned by one search call.
pub const SEARCH_LIMIT_MAX: usize = 50;
/// Maximum characters accepted for one proposed memory entry.
pub const PROPOSE_CONTENT_MAX: usize = 64_000;
const ENTRY_TEXT_MAX: usize = 64_000;
const SNIPPET_MAX: usize = 300;
const WALK_DEPTH_MAX: usize = 5;
const WALK_FILES_MAX: usize = 2_000;

/// Caller clearance for lane filtering (SPEC §9.1): visibility is governed
/// by the entry's content label, never by its origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clearance {
    /// Sees sfw-labeled entries only (the fail-closed default).
    GeneralOnly,
    /// Sees every label (restricted-cleared callers).
    All,
}

impl Clearance {
    /// True when an entry with `label` may be returned to this caller.
    pub fn can_see(&self, label: ContentLabel) -> bool {
        match label {
            ContentLabel::Sfw => true,
            ContentLabel::Nsfw => *self == Clearance::All,
        }
    }
}

/// One attention lease record (policy-as-data; MATE is the issuer,
/// SPEC §9.2). The server verifies leases; it does not issue them.
#[derive(Debug, Clone)]
pub struct LeaseRecord {
    /// Lease expiry, Unix epoch seconds.
    pub expires_epoch: i64,
    /// Fencing generation the lease was awarded at.
    pub generation: u64,
    /// Lease holder (agent name).
    pub holder: String,
    /// Repo name the lease covers.
    pub repo: String,
}

/// Policy-as-data (appa.toml-style, SPEC §9.1 point 4): lane assignments,
/// caller clearances, and attention leases live in a text file beside
/// the server config — never in client assertions.
///
/// Line grammar (one statement per line, `#` comments):
/// `repo <name> lane <general|restricted>`,
/// `clearance <caller> <general|restricted|all>`,
/// `lease <repo> <holder> <generation> <expires_epoch>`.
#[derive(Debug, Clone, Default)]
pub struct AppaPolicy {
    /// Caller name → clearance.
    pub clearances: HashMap<String, Clearance>,
    /// Attention leases presented for verification.
    pub leases: Vec<LeaseRecord>,
    /// Repo name → lane override.
    pub repo_lanes: HashMap<String, Lane>,
}

/// Parse policy text (see [`AppaPolicy`] for the grammar). Malformed
/// lines are ignored — policy is data, and a bad line must fail closed,
/// which the defaults (general lane, general-only clearance) do.
pub fn parse_policy(text: &str) -> AppaPolicy {
    let mut policy = AppaPolicy::default();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let toks: Vec<&str> = t.split_whitespace().collect();
        match toks.as_slice() {
            ["repo", name, "lane", lane] => {
                if let Some(l) = Lane::parse(lane) {
                    policy.repo_lanes.insert(name.to_string(), l);
                }
            }
            ["clearance", caller, level] => {
                let clearance = match level.to_ascii_lowercase().as_str() {
                    "restricted" | "all" => Clearance::All,
                    // Legacy alias (read-only back-compat, pre-0.3 policy files).
                    "ntranch" => Clearance::All,
                    _ => Clearance::GeneralOnly,
                };
                policy.clearances.insert(caller.to_string(), clearance);
            }
            ["lease", repo, holder, generation, expires] => {
                if let (Ok(g), Ok(e)) = (generation.parse::<u64>(), expires.parse::<i64>()) {
                    policy.leases.push(LeaseRecord {
                        expires_epoch: e,
                        generation: g,
                        holder: holder.to_string(),
                        repo: repo.to_string(),
                    });
                }
            }
            _ => {}
        }
    }
    policy
}

/// Server configuration: the registered repo roots plus optional
/// policy / ideas-register / board files.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Progress-board file (TRACKER.md) used to order rollups.
    pub board_path: Option<PathBuf>,
    /// Idea register file (IDEAS.md) for idea_lookup / idea_register.
    pub ideas_path: Option<PathBuf>,
    /// Extra file with `lease ...` lines (the board's lease export).
    pub leases_path: Option<PathBuf>,
    /// appa.toml-style policy file.
    pub policy_path: Option<PathBuf>,
    /// Registered repo roots.
    pub roots: Vec<PathBuf>,
}

/// Refuse roots that would expose Pax's private memory (SPEC §8 law):
/// the home directory itself, and anything under `~/memory/`.
pub fn guard_root(root: &Path) -> Result<PathBuf> {
    let canonical = root.canonicalize().map_err(BlochError::Io)?;
    if let Some(home_raw) = std::env::var_os("HOME") {
        let home = PathBuf::from(home_raw);
        let home_canonical = home.canonicalize().unwrap_or(home);
        if canonical == home_canonical || canonical.starts_with(home_canonical.join("memory")) {
            return Err(BlochError::GitFailed(
                "private_memory_refused: Pax's private memory (~/MEMORY.md, ~/memory/) is never indexed or served"
                    .to_string(),
            ));
        }
    }
    Ok(canonical)
}

fn repo_name_of(root: &Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "repo".to_string())
}

fn is_secret_shaped(name: &str) -> bool {
    let lc = name.to_ascii_lowercase();
    [
        "secret",
        "credential",
        ".env",
        "token",
        "password",
        "passwd",
    ]
    .iter()
    .any(|pat| lc.contains(pat))
}

// ---------------------------------------------------------------------------
// Index (M1): a rebuildable projection of the files
// ---------------------------------------------------------------------------

/// One indexed memory entry.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryEntry {
    /// Entry ID: `<repo>:<rel path>`.
    pub id: String,
    /// Label stamp (frontmatter, else the fail-closed lane default).
    pub label: LabelStamp,
    /// Path relative to the repo root.
    pub rel: String,
    /// Owning repo name.
    pub repo: String,
    /// Record scope (§10.4; parsed from the record, default repo_task).
    pub scope: crate::roadmap::Scope,
    /// Promotion state: `inbox`, `promoted`, or `archive`.
    pub state: String,
    /// Record subject (§10.4; empty when the record carries none).
    pub subject: String,
    /// Entry text (bounded).
    pub text: String,
    /// First heading, or the file name.
    pub title: String,
}

/// One indexed lesson (lessons.md section or an attempt-fix record).
#[derive(Debug, Clone, PartialEq)]
pub struct LessonEntry {
    /// Label stamp.
    pub label: LabelStamp,
    /// Owning repo name.
    pub repo: String,
    /// Record scope (§10.4; default repo_task).
    pub scope: crate::roadmap::Scope,
    /// Source rel path.
    pub source: String,
    /// Record subject (§10.4; falls back to the task ID).
    pub subject: String,
    /// Roadmap task the lesson belongs to, when known.
    pub task: Option<String>,
    /// Lesson text (bounded).
    pub text: String,
}

/// Per-section tracker counts from CHECKLIST.md.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackerSection {
    /// Items marked `[x]`.
    pub done: usize,
    /// Items marked `[~]`.
    pub doing: usize,
    /// Section heading.
    pub name: String,
    /// Items marked `[ ]`.
    pub open: usize,
}

/// A repo's tracker state, computed from its CHECKLIST.md.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackerData {
    /// Fencing generation from the checklist frontmatter.
    pub generation: u64,
    /// `[x]` items citing a memory entry that is missing or unpromoted.
    pub overclaims: Vec<String>,
    /// Per-section counts.
    pub sections: Vec<TrackerSection>,
    /// `[x]` items with no memory citation at all.
    pub uncited_done: usize,
}

/// One registered repo in the index.
#[derive(Debug, Clone)]
pub struct FleetRepo {
    /// Fencing generation (CHECKLIST frontmatter, default 1).
    pub generation: u64,
    /// Lane (policy override, else the repo's lane stamp).
    pub lane: Lane,
    /// Repo directory name.
    pub name: String,
    /// Roadmap rollup, when the repo has a roadmap.
    pub roadmap: Option<RoadmapRollup>,
    /// Canonical root path.
    pub root: PathBuf,
    /// Tracker state, when the repo has a CHECKLIST.md.
    pub tracker: Option<TrackerData>,
}

/// Roadmap numbers for one repo (roadmap_state / roadmap_rollup).
#[derive(Debug, Clone)]
pub struct RoadmapRollup {
    /// Current phase.
    pub current_phase: Option<u32>,
    /// Green count.
    pub green: usize,
    /// Roadmap kind.
    pub kind: String,
    /// Live attempts.
    pub live: Vec<LiveAttempt>,
    /// Next open task `(id, title)`.
    pub next_open: Option<(String, String)>,
    /// Percent green.
    pub percent: f64,
    /// `(phase, title, green, total)` per phase.
    pub phases: Vec<(u32, String, usize, usize)>,
    /// Non-dropped total.
    pub total: usize,
}

/// One idea-register entry.
#[derive(Debug, Clone, PartialEq)]
pub struct IdeaEntry {
    /// Aliases (anti-duplication surface).
    pub aliases: Vec<String>,
    /// The idea's one home.
    pub home: String,
    /// Register ID (`IDEA-####`).
    pub id: String,
    /// Lifecycle state.
    pub state: String,
    /// One-line statement.
    pub statement: String,
}

/// The fleet index: a pure projection of the files, rebuilt wholesale.
/// Two builds over the same files produce identical query answers.
#[derive(Debug, Clone)]
pub struct FleetIndex {
    /// Indexed memory entries.
    pub entries: Vec<MemoryEntry>,
    /// Indexed ideas.
    pub ideas: Vec<IdeaEntry>,
    /// Indexed lessons.
    pub lessons: Vec<LessonEntry>,
    /// Registered repos.
    pub repos: Vec<FleetRepo>,
}

fn walk_md_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > WALK_DEPTH_MAX || out.len() >= WALK_FILES_MAX {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    for path in paths {
        if out.len() >= WALK_FILES_MAX {
            return;
        }
        if path.is_dir() {
            walk_md_files(&path, depth + 1, out);
        } else if path.extension().map(|e| e == "md").unwrap_or(false) {
            out.push(path);
        }
    }
}

fn entry_title(text: &str, fallback: &str) -> String {
    for line in text.lines() {
        let t = line.trim();
        if let Some(h) = t.strip_prefix("# ") {
            return h.trim().to_string();
        }
    }
    fallback.to_string()
}

fn parse_checklist(text: &str, entries: &[MemoryEntry], repo: &str) -> TrackerData {
    let mut generation: u64 = 1;
    let mut sections: Vec<TrackerSection> = Vec::new();
    let mut overclaims: Vec<String> = Vec::new();
    let mut uncited_done = 0usize;
    let mut in_frontmatter = false;
    let mut frontmatter_done = false;
    for line in text.lines() {
        let t = line.trim();
        if !frontmatter_done && t == "---" {
            if in_frontmatter {
                frontmatter_done = true;
            }
            in_frontmatter = !in_frontmatter;
            continue;
        }
        if in_frontmatter {
            if let Some(v) = t.strip_prefix("tracker_generation:") {
                generation = v.trim().parse().unwrap_or(1);
            }
            continue;
        }
        if let Some(h) = t.strip_prefix("## ") {
            sections.push(TrackerSection {
                done: 0,
                doing: 0,
                name: h.trim().to_string(),
                open: 0,
            });
            continue;
        }
        let status = if t.starts_with("- [x]") || t.starts_with("- [X]") {
            Some('x')
        } else if t.starts_with("- [~]") {
            Some('~')
        } else if t.starts_with("- [ ]") {
            Some(' ')
        } else {
            None
        };
        if let Some(st) = status {
            if sections.is_empty() {
                sections.push(TrackerSection {
                    done: 0,
                    doing: 0,
                    name: "(top)".to_string(),
                    open: 0,
                });
            }
            let sec = sections.last_mut().unwrap();
            match st {
                'x' => sec.done += 1,
                '~' => sec.doing += 1,
                _ => sec.open += 1,
            }
            if st == 'x' {
                if let Some(pos) = t.find("memory:") {
                    let cite: String = t[pos + "memory:".len()..]
                        .chars()
                        .take_while(|c| !c.is_whitespace() && *c != ')' && *c != ']')
                        .collect();
                    let resolved = entries.iter().any(|e| {
                        e.repo == repo
                            && e.state == "promoted"
                            && (e.rel.ends_with(&cite) || e.id.ends_with(&cite))
                    });
                    if !resolved && overclaims.len() < 20 {
                        overclaims.push(t.chars().take(160).collect());
                    }
                } else {
                    uncited_done += 1;
                }
            }
        }
    }
    TrackerData {
        generation,
        overclaims,
        sections,
        uncited_done,
    }
}

/// Parse the idea register's markdown table rows.
pub fn parse_ideas(text: &str) -> Vec<IdeaEntry> {
    let mut ideas = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if !t.starts_with('|') || !t.contains("IDEA-") {
            continue;
        }
        let cells: Vec<String> = t
            .split('|')
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .collect();
        if cells.len() < 4 || !cells[0].starts_with("IDEA-") {
            continue;
        }
        let aliases = if cells.len() >= 5 {
            cells[4]
                .split(',')
                .map(|a| a.trim().to_string())
                .filter(|a| !a.is_empty())
                .collect()
        } else {
            Vec::new()
        };
        ideas.push(IdeaEntry {
            aliases,
            home: cells[2].clone(),
            id: cells[0].clone(),
            state: cells[3].clone(),
            statement: cells[1].clone(),
        });
    }
    ideas
}

impl FleetIndex {
    /// Build the index from the files under the configured roots.
    /// Pure projection: no state survives a rebuild, by design.
    pub fn build(config: &ServerConfig, policy: &AppaPolicy) -> Result<FleetIndex> {
        let mut repos: Vec<FleetRepo> = Vec::new();
        let mut entries: Vec<MemoryEntry> = Vec::new();
        let mut lessons: Vec<LessonEntry> = Vec::new();

        for root_raw in &config.roots {
            let root = guard_root(root_raw)?;
            let name = repo_name_of(&root);
            let lane = policy
                .repo_lanes
                .get(&name)
                .copied()
                .unwrap_or_else(|| repo_lane(&root));

            // Memory entries.
            let mem_dir = root.join(".ai").join("memory");
            let mut files: Vec<PathBuf> = Vec::new();
            if mem_dir.is_dir() {
                walk_md_files(&mem_dir, 0, &mut files);
            }
            let mut repo_entries: Vec<MemoryEntry> = Vec::new();
            for path in &files {
                let fname = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                if is_secret_shaped(&fname) {
                    continue;
                }
                let rel = path
                    .strip_prefix(&root)
                    .map(|r| r.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
                let raw = match std::fs::read_to_string(path) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let text: String = raw.chars().take(ENTRY_TEXT_MAX).collect();
                let state = if rel.contains("/inbox/") {
                    "inbox"
                } else if rel.contains("/archive/") {
                    "archive"
                } else {
                    "promoted"
                };
                let (scope, subject) = crate::roadmap::parse_record_fields(&text);
                repo_entries.push(MemoryEntry {
                    id: format!("{}:{}", name, rel),
                    label: parse_label_frontmatter(&text, lane),
                    rel,
                    repo: name.clone(),
                    scope,
                    state: state.to_string(),
                    subject,
                    title: entry_title(&text, &fname),
                    text,
                });
            }

            // Lessons: lessons.md + attempt-fix records (inbox + archive).
            let lessons_rel = ".ai/memory/lessons.md".to_string();
            for e in &repo_entries {
                if e.rel == lessons_rel {
                    lessons.push(LessonEntry {
                        label: e.label.clone(),
                        repo: name.clone(),
                        scope: e.scope,
                        source: e.rel.clone(),
                        subject: e.subject.clone(),
                        task: None,
                        text: e.text.clone(),
                    });
                } else if e.rel.contains("attempt-fix") {
                    let task = e
                        .rel
                        .split("attempt-fix-")
                        .nth(1)
                        .map(|s| s.trim_end_matches(".md").to_uppercase());
                    let subject = if e.subject.is_empty() {
                        task.clone().unwrap_or_default()
                    } else {
                        e.subject.clone()
                    };
                    lessons.push(LessonEntry {
                        label: e.label.clone(),
                        repo: name.clone(),
                        scope: e.scope,
                        source: e.rel.clone(),
                        subject,
                        task,
                        text: e.text.clone(),
                    });
                }
            }
            entries.extend(repo_entries.iter().cloned());

            // Tracker.
            let tracker = std::fs::read_to_string(root.join("CHECKLIST.md"))
                .ok()
                .map(|text| parse_checklist(&text, &repo_entries, &name));
            let generation = tracker.as_ref().map(|t| t.generation).unwrap_or(1);

            // Roadmap rollup.
            let roadmap = roadmap_status(&root).ok().map(|st| RoadmapRollup {
                current_phase: st.current_phase,
                green: st.green,
                kind: st.kind.clone(),
                live: st.live.clone(),
                next_open: st.next_open.clone(),
                percent: st.percent,
                phases: st.phases.clone(),
                total: st.total,
            });

            repos.push(FleetRepo {
                generation,
                lane,
                name,
                roadmap,
                root,
                tracker,
            });
        }

        let ideas = config
            .ideas_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|text| parse_ideas(&text))
            .unwrap_or_default();

        repos.sort_by(|a, b| a.name.cmp(&b.name));
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        lessons.sort_by(|a, b| {
            (a.repo.clone(), a.source.clone()).cmp(&(b.repo.clone(), b.source.clone()))
        });
        Ok(FleetIndex {
            entries,
            ideas,
            lessons,
            repos,
        })
    }

    /// Find a registered repo by name.
    pub fn repo(&self, name: &str) -> Option<&FleetRepo> {
        self.repos.iter().find(|r| r.name == name)
    }
}

fn query_tokens(query: &str) -> Vec<String> {
    query
        .to_ascii_lowercase()
        .split_whitespace()
        .map(|t| t.to_string())
        .collect()
}

fn text_matches(haystack: &str, tokens: &[String]) -> bool {
    let lc = haystack.to_ascii_lowercase();
    tokens.iter().all(|t| lc.contains(t.as_str()))
}

fn snippet(text: &str, tokens: &[String]) -> String {
    let lc = text.to_ascii_lowercase();
    let mut start = 0usize;
    for t in tokens {
        if let Some(pos) = lc.find(t.as_str()) {
            start = pos.saturating_sub(60);
            break;
        }
    }
    let window: String = text.chars().skip(start).take(SNIPPET_MAX).collect();
    window.replace('\n', " ")
}

// ---------------------------------------------------------------------------
// Tool surface (11 tools, annotations from birth)
// ---------------------------------------------------------------------------

/// The 11 tool definitions with full schemas and MCP annotations.
/// Read tools are `readOnlyHint: true`; the two gated writes and the
/// inbox write declare their own access class in their descriptions.
pub fn tool_definitions() -> serde_json::Value {
    let read = serde_json::json!({
        "readOnlyHint": true, "destructiveHint": false,
        "idempotentHint": true, "openWorldHint": false
    });
    let write_inbox = serde_json::json!({
        "readOnlyHint": false, "destructiveHint": false,
        "idempotentHint": false, "openWorldHint": false
    });
    let gated = serde_json::json!({
        "readOnlyHint": false, "destructiveHint": false,
        "idempotentHint": false, "openWorldHint": false
    });
    let obj = |props: serde_json::Value, required: Vec<&str>| {
        serde_json::json!({
            "type": "object", "properties": props, "required": required,
            "additionalProperties": false
        })
    };
    let s = |desc: &str| serde_json::json!({"type": "string", "description": desc});
    serde_json::json!([
        {
            "name": "memory_search",
            "description": "Read. Cross-repo search over indexed .ai/memory entries. Lane-filtered server-side: nsfw-labeled entries are never returned to general-only callers.",
            "inputSchema": obj(serde_json::json!({
                "query": s("Search text (all tokens must match)"),
                "repo": s("Optional repo name filter"),
                "state": s("Optional promotion state filter: inbox|promoted|archive"),
                "caller": s("Caller name for policy clearance lookup"),
                "clearance": s("Caller clearance lane: general (default) or restricted"),
                "limit": {"type": "integer", "minimum": 1, "maximum": 50}
            }), vec!["query"]),
            "annotations": read
        },
        {
            "name": "memory_get",
            "description": "Read. One indexed memory entry by id (<repo>:<rel path>), with label, promotion state, and text. Lane-filtered server-side.",
            "inputSchema": obj(serde_json::json!({
                "id": s("Entry id from memory_search"),
                "caller": s("Caller name for policy clearance lookup"),
                "clearance": s("Caller clearance lane: general (default) or restricted")
            }), vec!["id"]),
            "annotations": read
        },
        {
            "name": "memory_propose",
            "description": "Write, inbox only. File a memory entry into a repo's .ai/memory/inbox. Requires the repo's current fencing generation and a live attention lease (MATE-issued, policy-as-data) held by the agent. Cannot promote, cannot touch promoted entries or CHECKLIST.md. Nsfw-labeled content into a general-lane repo is refused (lane wall).",
            "inputSchema": obj(serde_json::json!({
                "repo": s("Target repo name"),
                "agent": s("Proposing agent (lease holder)"),
                "topic": s("Topic slug for the inbox file name"),
                "content": s("Entry content (markdown)"),
                "generation": {"type": "integer", "minimum": 0, "description": "The repo's current fencing generation"},
                "content_label": s("Optional claimed content label: sfw|nsfw (takes effect only with label_evidence)"),
                "label_evidence": s("Evidence for the claimed content label"),
                "source_repo": s("Optional source repo for a cross-repo transfer; its lane stamps the entry's origin_lane")
            }), vec!["repo", "agent", "topic", "content", "generation"]),
            "annotations": write_inbox
        },
        {
            "name": "memory_promote",
            "description": "Gated write. Promote an inbox entry to a curated memory file (decisions.md|patterns.md|current.md|lessons.md). Gate act: promoter must be pax or owner, never the proposer; cited evidence must resolve (repo-relative file or commit:<sha>). An nsfw->sfw reclassification is possible only here, with reclassify_evidence.",
            "inputSchema": obj(serde_json::json!({
                "repo": s("Repo name"),
                "file": s("Inbox file name"),
                "to": s("Target curated file"),
                "promoter": s("Promoting actor: pax or owner"),
                "evidence": s("Evidence pointer: repo-relative path or commit:<sha>"),
                "reclassify_to": s("Optional gated reclassification: sfw|nsfw"),
                "reclassify_evidence": s("Evidence for the reclassification (required with reclassify_to)")
            }), vec!["repo", "file", "to", "promoter", "evidence"]),
            "annotations": gated
        },
        {
            "name": "tracker_state",
            "description": "Read. One repo's tracker rollup from CHECKLIST.md: per-section counts, fencing generation, overclaims (done items whose cited evidence is missing or unpromoted).",
            "inputSchema": obj(serde_json::json!({"repo": s("Repo name")}), vec!["repo"]),
            "annotations": read
        },
        {
            "name": "tracker_rollup",
            "description": "Read. The fleet view of tracker_state for every registered repo, in progress-board order when a board file is configured.",
            "inputSchema": obj(serde_json::json!({}), vec![]),
            "annotations": read
        },
        {
            "name": "idea_lookup",
            "description": "Read. Anti-duplication pre-flight: query the idea register by ID, statement text, or alias. Returns the one entry or an explicit no-match.",
            "inputSchema": obj(serde_json::json!({"query": s("IDEA id, statement fragment, or alias")}), vec!["query"]),
            "annotations": read
        },
        {
            "name": "idea_register",
            "description": "Gated write. Register a new idea (server assigns the next sequential ID, never reused). Gate act: registrar must be pax or owner. A proposed alias colliding with an existing entry is refused as a suspected duplicate.",
            "inputSchema": obj(serde_json::json!({
                "statement": s("One-line idea statement"),
                "home": s("The idea's one home (repo/program)"),
                "aliases": {"type": "array", "items": {"type": "string"}},
                "registrar": s("Registering actor: pax or owner")
            }), vec!["statement", "home", "registrar"]),
            "annotations": gated
        },
        {
            "name": "lessons_search",
            "description": "Read. Cross-repo search over lessons.md, promoted attempt-fix entries, and attempt aggregates (confidence trajectories, mean time-to-green by repo kind): what another agent learned the hard way. Lane-filtered server-side.",
            "inputSchema": obj(serde_json::json!({
                "query": s("Search text (all tokens must match)"),
                "caller": s("Caller name for policy clearance lookup"),
                "clearance": s("Caller clearance lane: general (default) or restricted"),
                "limit": {"type": "integer", "minimum": 1, "maximum": 50}
            }), vec!["query"]),
            "annotations": read
        },
        {
            "name": "roadmap_state",
            "description": "Read. One repo's roadmap rollup from .ai/roadmap.md: percent green per phase, next open task, live attempts with elapsed time (a live attempt is an attention claim), recent lessons.",
            "inputSchema": obj(serde_json::json!({"repo": s("Repo name")}), vec!["repo"]),
            "annotations": read
        },
        {
            "name": "roadmap_rollup",
            "description": "Read. The fleet view of roadmap_state: every bloch-initialised repo's v1 progress in one call.",
            "inputSchema": obj(serde_json::json!({}), vec![]),
            "annotations": read
        }
    ])
}

// ---------------------------------------------------------------------------
// Server + tool dispatch
// ---------------------------------------------------------------------------

/// The MCP server: config + policy + a rebuildable index over the files.
pub struct McpServer {
    /// Server configuration.
    pub config: ServerConfig,
    /// The current index projection.
    pub index: FleetIndex,
    /// Policy-as-data (lanes, clearances, leases).
    pub policy: AppaPolicy,
}

type ToolResult = std::result::Result<serde_json::Value, String>;

fn arg_str(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn arg_limit(args: &serde_json::Value) -> usize {
    args.get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| (n as usize).clamp(1, SEARCH_LIMIT_MAX))
        .unwrap_or(20)
}

impl McpServer {
    /// Build a server: guard every root, load policy, build the index.
    pub fn new(config: ServerConfig) -> Result<McpServer> {
        let mut policy = AppaPolicy::default();
        if let Some(path) = &config.policy_path {
            if let Ok(text) = std::fs::read_to_string(path) {
                policy = parse_policy(&text);
            }
        }
        if let Some(path) = &config.leases_path {
            if let Ok(text) = std::fs::read_to_string(path) {
                let extra = parse_policy(&text);
                policy.leases.extend(extra.leases);
                for (k, v) in extra.clearances {
                    policy.clearances.insert(k, v);
                }
                for (k, v) in extra.repo_lanes {
                    policy.repo_lanes.insert(k, v);
                }
            }
        }
        let index = FleetIndex::build(&config, &policy)?;
        Ok(McpServer {
            config,
            index,
            policy,
        })
    }

    /// Rebuild the index from the files (M1 rebuild proof + post-write
    /// refresh). Identical files → identical answers.
    pub fn rebuild(&mut self) -> Result<()> {
        self.index = FleetIndex::build(&self.config, &self.policy)?;
        Ok(())
    }

    fn clearance_for(&self, args: &serde_json::Value) -> Clearance {
        if let Some(caller) = arg_str(args, "caller") {
            if let Some(c) = self.policy.clearances.get(&caller) {
                return *c;
            }
        }
        match arg_str(args, "clearance").as_deref() {
            Some("restricted") | Some("all") => Clearance::All,
            // Legacy alias (read-only back-compat, pre-0.3 callers).
            Some("ntranch") => Clearance::All,
            _ => Clearance::GeneralOnly,
        }
    }

    fn repo_required(&self, args: &serde_json::Value) -> std::result::Result<&FleetRepo, String> {
        let name = arg_str(args, "repo")
            .ok_or_else(|| "invalid_params: missing required argument 'repo'".to_string())?;
        self.index
            .repo(&name)
            .ok_or_else(|| format!("repo_not_found: {}", name))
    }

    /// Dispatch one tool call. Errors are typed strings
    /// (`kind: detail`) returned as MCP `isError` results by the RPC layer.
    pub fn call_tool(&mut self, name: &str, args: &serde_json::Value) -> ToolResult {
        match name {
            "memory_search" => self.tool_memory_search(args),
            "memory_get" => self.tool_memory_get(args),
            "memory_propose" => self.tool_memory_propose(args),
            "memory_promote" => self.tool_memory_promote(args),
            "tracker_state" => self.tool_tracker_state(args),
            "tracker_rollup" => self.tool_tracker_rollup(args),
            "idea_lookup" => self.tool_idea_lookup(args),
            "idea_register" => self.tool_idea_register(args),
            "lessons_search" => self.tool_lessons_search(args),
            "roadmap_state" => self.tool_roadmap_state(args),
            "roadmap_rollup" => self.tool_roadmap_rollup(args),
            _ => Err(format!("unknown_tool: {}", name)),
        }
    }

    fn tool_memory_search(&mut self, args: &serde_json::Value) -> ToolResult {
        let query = arg_str(args, "query")
            .ok_or_else(|| "invalid_params: missing required argument 'query'".to_string())?;
        if query.chars().count() > SEARCH_QUERY_MAX {
            return Err(format!(
                "query_too_long: {} chars exceeds limit {}",
                query.chars().count(),
                SEARCH_QUERY_MAX
            ));
        }
        let clearance = self.clearance_for(args);
        let tokens = query_tokens(&query);
        let repo_filter = arg_str(args, "repo");
        let state_filter = arg_str(args, "state");
        let limit = arg_limit(args);
        let mut results = Vec::new();
        for e in &self.index.entries {
            if !clearance.can_see(e.label.content_label) {
                continue;
            }
            if let Some(r) = &repo_filter {
                if &e.repo != r {
                    continue;
                }
            }
            if let Some(s) = &state_filter {
                if &e.state != s {
                    continue;
                }
            }
            let hay = format!("{} {} {} {}", e.title, e.repo, e.rel, e.text);
            if !text_matches(&hay, &tokens) {
                continue;
            }
            results.push(serde_json::json!({
                "content_label": e.label.content_label.as_str(),
                "id": e.id,
                "origin_lane": e.label.origin_lane.as_str(),
                "repo": e.repo,
                "rel": e.rel,
                "scope": e.scope.as_str(),
                "snippet": snippet(&e.text, &tokens),
                "state": e.state,
                "subject": e.subject,
                "title": e.title,
            }));
            if results.len() >= limit {
                break;
            }
        }
        Ok(serde_json::json!({"results": results}))
    }

    fn tool_memory_get(&mut self, args: &serde_json::Value) -> ToolResult {
        let id = arg_str(args, "id")
            .ok_or_else(|| "invalid_params: missing required argument 'id'".to_string())?;
        let clearance = self.clearance_for(args);
        let entry = self
            .index
            .entries
            .iter()
            .find(|e| e.id == id)
            .ok_or_else(|| format!("entry_not_found: {}", id))?;
        if !clearance.can_see(entry.label.content_label) {
            return Err(format!(
                "lane_forbidden: entry {} is nsfw-labeled; caller clearance is general-only",
                id
            ));
        }
        Ok(serde_json::json!({
            "content_label": entry.label.content_label.as_str(),
            "evidence": entry.label.evidence,
            "id": entry.id,
            "origin_lane": entry.label.origin_lane.as_str(),
            "rel": entry.rel,
            "repo": entry.repo,
            "scope": entry.scope.as_str(),
            "state": entry.state,
            "subject": entry.subject,
            "text": entry.text,
            "title": entry.title,
        }))
    }

    fn tool_memory_propose(&mut self, args: &serde_json::Value) -> ToolResult {
        let repo = self.repo_required(args)?.clone();
        let agent = arg_str(args, "agent")
            .ok_or_else(|| "invalid_params: missing required argument 'agent'".to_string())?;
        let topic = arg_str(args, "topic")
            .ok_or_else(|| "invalid_params: missing required argument 'topic'".to_string())?;
        let content = arg_str(args, "content")
            .ok_or_else(|| "invalid_params: missing required argument 'content'".to_string())?;
        if content.trim().is_empty() {
            return Err("invalid_params: content must not be empty".to_string());
        }
        if content.chars().count() > PROPOSE_CONTENT_MAX {
            return Err(format!(
                "payload_too_large: content {} chars exceeds limit {}",
                content.chars().count(),
                PROPOSE_CONTENT_MAX
            ));
        }
        let generation = args
            .get("generation")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "invalid_params: missing required argument 'generation'".to_string())?;
        if generation != repo.generation {
            return Err(format!(
                "stale_generation: presented {}, current {} for repo {}",
                generation, repo.generation, repo.name
            ));
        }
        let now = now_epoch();
        let lease = self
            .policy
            .leases
            .iter()
            .find(|l| l.repo == repo.name && l.holder == agent);
        match lease {
            None => {
                return Err(format!(
                    "lease_required: no attention lease for {} on repo {}",
                    agent, repo.name
                ))
            }
            Some(l) if l.expires_epoch <= now => {
                return Err(format!(
                    "lease_expired: lease for {} on repo {} expired at {}",
                    agent, repo.name, l.expires_epoch
                ))
            }
            Some(l) if l.generation != generation => {
                return Err(format!(
                    "stale_generation: lease was awarded at generation {}, presented {}",
                    l.generation, generation
                ))
            }
            _ => {}
        }
        let claimed = arg_str(args, "content_label").and_then(|s| ContentLabel::parse(&s));
        let label_evidence = arg_str(args, "label_evidence").unwrap_or_default();
        // Origin is the source repo's lane for transfers (§9.1: provenance
        // is stamped at birth and never relabeled at handoff). A claimed
        // sfw label from a restricted origin without evidence fails closed
        // to nsfw in decide_content_label, and the wall check below then
        // refuses the transfer.
        let origin_lane = match arg_str(args, "source_repo") {
            Some(src) => self
                .index
                .repo(&src)
                .map(|r| r.lane)
                .ok_or_else(|| format!("repo_not_found: {}", src))?,
            None => repo.lane,
        };
        let stamp = decide_content_label(origin_lane, claimed, &label_evidence);
        if repo.lane == Lane::General && stamp.content_label == ContentLabel::Nsfw {
            return Err(
                "lane_wall_refusal: nsfw-labeled content may not be proposed into a general-lane repo"
                    .to_string(),
            );
        }
        let labelled = format!(
            "{}{}<!-- bloch:proposer {} -->\n{}",
            label_frontmatter(&stamp),
            "",
            agent,
            content
        );
        let fname = memory_inbox(&repo.root, &agent, &topic, &labelled)
            .map_err(|e| format!("write_failed: {}", e))?;
        let result = serde_json::json!({
            "content_label": stamp.content_label.as_str(),
            "file": fname,
            "generation": repo.generation,
            "origin_lane": stamp.origin_lane.as_str(),
            "repo": repo.name,
        });
        let _ = self.rebuild();
        Ok(result)
    }

    fn tool_memory_promote(&mut self, args: &serde_json::Value) -> ToolResult {
        let repo = self.repo_required(args)?.clone();
        let file = arg_str(args, "file")
            .ok_or_else(|| "invalid_params: missing required argument 'file'".to_string())?;
        let to = arg_str(args, "to")
            .ok_or_else(|| "invalid_params: missing required argument 'to'".to_string())?;
        let promoter = arg_str(args, "promoter")
            .ok_or_else(|| "invalid_params: missing required argument 'promoter'".to_string())?;
        let evidence = arg_str(args, "evidence").unwrap_or_default();
        if !matches!(promoter.to_ascii_lowercase().as_str(), "pax" | "owner") {
            return Err(format!(
                "promoter_not_gatekeeper: promotion is a Pax/Owner gate act, got '{}'",
                promoter
            ));
        }
        let inbox_path = repo
            .root
            .join(".ai")
            .join("memory")
            .join("inbox")
            .join(&file);
        let content = std::fs::read_to_string(&inbox_path)
            .map_err(|_| format!("entry_not_found: inbox file {}", file))?;
        let proposer = content
            .lines()
            .find_map(|l| {
                l.trim()
                    .strip_prefix("<!-- bloch:proposer ")
                    .and_then(|rest| rest.strip_suffix(" -->"))
                    .map(|s| s.trim().to_string())
            })
            .or_else(|| file.split('-').nth(1).map(|s| s.to_string()))
            .unwrap_or_default();
        if !proposer.is_empty() && proposer.eq_ignore_ascii_case(&promoter) {
            return Err(
                "proposer_is_promoter: the proposing agent may never promote its own entry"
                    .to_string(),
            );
        }
        if evidence.trim().is_empty() || !evidence_resolves(&repo.root, &evidence) {
            return Err(format!(
                "evidence_unresolved: cited evidence '{}' does not resolve",
                evidence
            ));
        }
        if let Some(target) = arg_str(args, "reclassify_to") {
            let target_label = ContentLabel::parse(&target)
                .ok_or_else(|| format!("invalid_params: bad reclassify_to '{}'", target))?;
            let re_evidence = arg_str(args, "reclassify_evidence").unwrap_or_default();
            if re_evidence.trim().is_empty() {
                return Err(
                    "reclassification_requires_evidence: nsfw->sfw reclassification is a gated act and must cite evidence"
                        .to_string(),
                );
            }
            let stamp = parse_label_frontmatter(&content, repo.lane);
            let new_stamp = LabelStamp {
                origin_lane: stamp.origin_lane,
                content_label: target_label,
                evidence: re_evidence.clone(),
            };
            let body = strip_frontmatter(&content);
            let rewritten = format!(
                "{}<!-- bloch:reclassified_by {} evidence: {} -->\n{}",
                label_frontmatter(&new_stamp),
                promoter,
                re_evidence,
                body
            );
            std::fs::write(&inbox_path, rewritten).map_err(|e| format!("write_failed: {}", e))?;
        }
        let msg =
            memory_promote(&repo.root, &file, &to).map_err(|e| format!("promote_failed: {}", e))?;
        let result = serde_json::json!({"promoted": msg, "repo": repo.name});
        let _ = self.rebuild();
        Ok(result)
    }

    fn tracker_json(&self, repo: &FleetRepo) -> serde_json::Value {
        match &repo.tracker {
            None => serde_json::json!({
                "generation": repo.generation, "has_tracker": false, "repo": repo.name
            }),
            Some(t) => {
                let sections: Vec<serde_json::Value> = t
                    .sections
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "done": s.done, "doing": s.doing, "name": s.name, "open": s.open
                        })
                    })
                    .collect();
                let done: usize = t.sections.iter().map(|s| s.done).sum();
                let total: usize = t.sections.iter().map(|s| s.done + s.doing + s.open).sum();
                serde_json::json!({
                    "done": done,
                    "gated_percent": if total == 0 { 0.0 } else { done as f64 * 100.0 / total as f64 },
                    "generation": t.generation,
                    "has_tracker": true,
                    "overclaims": t.overclaims,
                    "repo": repo.name,
                    "sections": sections,
                    "total": total,
                    "uncited_done": t.uncited_done,
                })
            }
        }
    }

    fn tool_tracker_state(&mut self, args: &serde_json::Value) -> ToolResult {
        let repo = self.repo_required(args)?.clone();
        Ok(self.tracker_json(&repo))
    }

    fn tool_tracker_rollup(&mut self, _args: &serde_json::Value) -> ToolResult {
        let mut repos: Vec<&FleetRepo> = self.index.repos.iter().collect();
        if let Some(board) = &self.config.board_path {
            if let Ok(text) = std::fs::read_to_string(board) {
                repos.sort_by_key(|r| text.find(&r.name).unwrap_or(usize::MAX));
            }
        }
        let rollup: Vec<serde_json::Value> = repos.iter().map(|r| self.tracker_json(r)).collect();
        Ok(serde_json::json!({"repos": rollup}))
    }

    fn tool_idea_lookup(&mut self, args: &serde_json::Value) -> ToolResult {
        let query = arg_str(args, "query")
            .ok_or_else(|| "invalid_params: missing required argument 'query'".to_string())?;
        let q = query.to_ascii_lowercase();
        let found = self.index.ideas.iter().find(|i| {
            i.id.to_ascii_lowercase() == q
                || i.statement.to_ascii_lowercase().contains(&q)
                || i.aliases.iter().any(|a| a.to_ascii_lowercase() == q)
        });
        match found {
            Some(i) => Ok(serde_json::json!({
                "found": true,
                "idea": {
                    "aliases": i.aliases, "home": i.home, "id": i.id,
                    "state": i.state, "statement": i.statement
                }
            })),
            None => Ok(serde_json::json!({"found": false, "query": query})),
        }
    }

    fn tool_idea_register(&mut self, args: &serde_json::Value) -> ToolResult {
        let registrar = arg_str(args, "registrar").unwrap_or_default();
        if !matches!(registrar.to_ascii_lowercase().as_str(), "pax" | "owner") {
            return Err(format!(
                "registrar_not_gatekeeper: idea registration is a Pax/Owner gate act, got '{}'",
                registrar
            ));
        }
        let ideas_path =
            self.config.ideas_path.clone().ok_or_else(|| {
                "ideas_not_configured: no idea register file configured".to_string()
            })?;
        let statement = arg_str(args, "statement")
            .ok_or_else(|| "invalid_params: missing required argument 'statement'".to_string())?;
        let home = arg_str(args, "home")
            .ok_or_else(|| "invalid_params: missing required argument 'home'".to_string())?;
        let aliases: Vec<String> = args
            .get("aliases")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        for idea in &self.index.ideas {
            for a in &aliases {
                if idea.aliases.iter().any(|x| x.eq_ignore_ascii_case(a))
                    || idea.statement.eq_ignore_ascii_case(a)
                {
                    return Err(format!(
                        "suspected_duplicate: alias '{}' collides with {} ({})",
                        a, idea.id, idea.statement
                    ));
                }
            }
            if idea.statement.eq_ignore_ascii_case(&statement) {
                return Err(format!(
                    "suspected_duplicate: statement matches {} ({})",
                    idea.id, idea.statement
                ));
            }
        }
        let next = self
            .index
            .ideas
            .iter()
            .filter_map(|i| i.id.strip_prefix("IDEA-"))
            .filter_map(|n| n.parse::<u32>().ok())
            .max()
            .unwrap_or(0)
            + 1;
        let id = format!("IDEA-{:04}", next);
        let clean = |s: &str| s.replace('|', "/");
        let row = format!(
            "| {} | {} | {} | registered | {} |\n",
            id,
            clean(&statement),
            clean(&home),
            aliases.join(", ")
        );
        let mut fh = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&ideas_path)
            .map_err(|e| format!("write_failed: {}", e))?;
        use std::io::Write as _;
        fh.write_all(row.as_bytes())
            .map_err(|e| format!("write_failed: {}", e))?;
        let result = serde_json::json!({"home": home, "id": id, "statement": statement});
        let _ = self.rebuild();
        Ok(result)
    }

    fn tool_lessons_search(&mut self, args: &serde_json::Value) -> ToolResult {
        let query = arg_str(args, "query")
            .ok_or_else(|| "invalid_params: missing required argument 'query'".to_string())?;
        if query.chars().count() > SEARCH_QUERY_MAX {
            return Err(format!(
                "query_too_long: {} chars exceeds limit {}",
                query.chars().count(),
                SEARCH_QUERY_MAX
            ));
        }
        let clearance = self.clearance_for(args);
        let tokens = query_tokens(&query);
        let limit = arg_limit(args);
        let mut matches = Vec::new();
        for l in &self.index.lessons {
            if !clearance.can_see(l.label.content_label) {
                continue;
            }
            let hay = format!("{} {} {}", l.repo, l.source, l.text);
            if !text_matches(&hay, &tokens) {
                continue;
            }
            matches.push(serde_json::json!({
                "content_label": l.label.content_label.as_str(),
                "id": format!("{}:{}", l.repo, l.source),
                "origin_lane": l.label.origin_lane.as_str(),
                "repo": l.repo,
                "scope": l.scope.as_str(),
                "snippet": snippet(&l.text, &tokens),
                "source": l.source,
                "subject": l.subject,
                "task": l.task,
            }));
            if matches.len() >= limit {
                break;
            }
        }
        // Aggregates: mean time-to-green by repo kind + failure totals.
        let mut by_kind: HashMap<String, Vec<u64>> = HashMap::new();
        let mut attempts_total = 0usize;
        let mut fails_total = 0usize;
        for repo in &self.index.repos {
            let events = read_attempts(&repo.root);
            attempts_total += events.len();
            fails_total += events.iter().filter(|e| e.event == "fail").count();
            if let Some(rollup) = &repo.roadmap {
                if let Ok(Some(rm)) = crate::roadmap::roadmap_load(&repo.root) {
                    {
                        for task in &rm.tasks {
                            if task.status != TaskStatus::Green {
                                continue;
                            }
                            let history = attempt_history(&events, &task.id);
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
                                by_kind.entry(rollup.kind.clone()).or_default().push(sum);
                            }
                        }
                    }
                }
            }
        }
        let means: Vec<serde_json::Value> = by_kind
            .iter()
            .map(|(kind, vals)| {
                let mean = if vals.is_empty() {
                    0.0
                } else {
                    vals.iter().sum::<u64>() as f64 / vals.len() as f64
                };
                serde_json::json!({"kind": kind, "mean_time_to_green_secs": mean, "tasks": vals.len()})
            })
            .collect();
        Ok(serde_json::json!({
            "aggregates": {
                "attempts_total": attempts_total,
                "fails_total": fails_total,
                "mean_time_to_green_by_kind": means,
            },
            "matches": matches,
        }))
    }

    fn roadmap_json(&self, repo: &FleetRepo) -> ToolResult {
        let rollup = repo.roadmap.as_ref().ok_or_else(|| {
            format!(
                "roadmap_not_found: repo {} has no .ai/roadmap.md",
                repo.name
            )
        })?;
        let phases: Vec<serde_json::Value> = rollup
            .phases
            .iter()
            .map(|(p, t, g, tot)| {
                serde_json::json!({"green": g, "phase": p, "title": t, "total": tot})
            })
            .collect();
        let live: Vec<serde_json::Value> = rollup
            .live
            .iter()
            .map(|l| {
                serde_json::json!({
                    "agent": l.agent, "elapsed_secs": l.elapsed_secs,
                    "started_ts": l.started_ts, "task": l.task
                })
            })
            .collect();
        let lessons = crate::roadmap::recent_lessons(&repo.root, 3);
        Ok(serde_json::json!({
            "current_phase": rollup.current_phase,
            "green": rollup.green,
            "kind": rollup.kind,
            "lane": repo.lane.as_str(),
            "live_attempts": live,
            "next_open": rollup.next_open.as_ref().map(|(id, t)| {
                serde_json::json!({"id": id, "title": t})
            }),
            "percent_green": rollup.percent,
            "phases": phases,
            "recent_lessons": lessons,
            "repo": repo.name,
            "total": rollup.total,
        }))
    }

    fn tool_roadmap_state(&mut self, args: &serde_json::Value) -> ToolResult {
        let repo = self.repo_required(args)?.clone();
        self.roadmap_json(&repo)
    }

    fn tool_roadmap_rollup(&mut self, _args: &serde_json::Value) -> ToolResult {
        let mut repos = Vec::new();
        for repo in &self.index.repos {
            if repo.roadmap.is_some() {
                if let Ok(v) = self.roadmap_json(repo) {
                    repos.push(v);
                }
            }
        }
        Ok(serde_json::json!({"repos": repos}))
    }
}

fn strip_frontmatter(content: &str) -> String {
    let mut lines = content.lines();
    if lines.next().map(|l| l.trim()) == Some("---") {
        let mut rest: Vec<&str> = Vec::new();
        let mut closed = false;
        for line in lines {
            if !closed && line.trim() == "---" {
                closed = true;
                continue;
            }
            if closed {
                rest.push(line);
            }
        }
        if closed {
            return rest.join("\n");
        }
    }
    content.to_string()
}

fn evidence_resolves(root: &Path, evidence: &str) -> bool {
    if let Some(sha) = evidence.strip_prefix("commit:") {
        let out = run_git(root, &["cat-file", "-t", sha.trim()]);
        return out == "commit";
    }
    if evidence.contains('/') || evidence.contains('\\') || evidence.starts_with('.') {
        // Repo-relative path evidence must stay inside the repo.
        let candidate = root.join(evidence);
        return candidate.is_file();
    }
    // Bare file name: resolve inside .ai/memory/.
    root.join(".ai").join("memory").join(evidence).is_file()
}

// ---------------------------------------------------------------------------
// JSON-RPC transport (stdio, hand-rolled, dual-era)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// rmcp handler (v0.4): framing only. The domain logic above — index,
// lane law, gated writes, `call_tool` — is untouched by the SDK swap.
// ---------------------------------------------------------------------------

/// The rmcp handler: a thin framing shell over [`McpServer`].
///
/// The domain server is stateful (`&mut self`: the index rebuilds,
/// gated writes mutate files) and the hand-rolled transport served
/// strictly sequentially; a mutex preserves exactly that execution
/// contract under the SDK's concurrent dispatch. A poisoned lock is
/// recovered rather than propagated: the panic that poisoned it has
/// already been reported by the panicking call, and the domain
/// server's state is file-backed and rebuildable.
pub struct BlochHandler {
    server: Arc<Mutex<McpServer>>,
}

impl BlochHandler {
    /// Wrap a built domain server for SDK serving.
    pub fn new(server: McpServer) -> BlochHandler {
        BlochHandler {
            server: Arc::new(Mutex::new(server)),
        }
    }
}

/// The served tool surface as rmcp [`Tool`] values, converted from
/// the hand-written definitions so the wire payload cannot drift
/// from [`tool_definitions`]. Conversion failure is a build-time
/// class bug (the definitions are compiled in), surfaced as an
/// internal error, never a silent drop.
fn rmcp_tools() -> std::result::Result<Vec<Tool>, String> {
    let definitions = tool_definitions();
    let mut tools = Vec::new();
    for def in definitions.as_array().cloned().unwrap_or_default() {
        let tool: Tool =
            serde_json::from_value(def).map_err(|e| format!("tool definition: {e}"))?;
        tools.push(tool);
    }
    Ok(tools)
}

impl ServerHandler for BlochHandler {
    /// Modern era only: `server/discover` advertises exactly this
    /// one version and the SDK validates every request's `_meta`
    /// against it.
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(&[ProtocolVersion::V_2026_07_28])
    }

    fn get_info(&self) -> RmcpServerConfig {
        RmcpServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("bloch", env!("CARGO_PKG_VERSION")))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, ErrorData> {
        let tools = rmcp_tools()
            .map_err(|e| ErrorData::internal_error(format!("internal_error: {e}"), None))?;
        Ok(ListToolsResult {
            tools,
            ..Default::default()
        })
    }

    /// Tool failures stay MCP `isError` results (never RPC errors),
    /// with the same payloads the hand-rolled transport produced:
    /// pretty-printed JSON text + `structuredContent` on success,
    /// the typed refusal message on failure. The one addition is
    /// the modern spec's `resultType` discriminator, set by the
    /// SDK constructors.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, ErrorData> {
        let args = request
            .arguments
            .map(serde_json::Value::Object)
            .unwrap_or_else(|| serde_json::json!({}));
        let outcome = {
            let mut server = self
                .server
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            server.call_tool(&request.name, &args)
        };
        match outcome {
            Ok(value) => {
                let text = serde_json::to_string_pretty(&value).unwrap_or_default();
                let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
                result.structured_content = Some(value);
                Ok(result.into())
            }
            Err(msg) => Ok(CallToolResult::error(vec![ContentBlock::text(msg)]).into()),
        }
    }
}

/// Duplex buffer between the era guard and the SDK service. The
/// SDK consumes incrementally, so this only needs to cover
/// request/response bursts, not whole payloads.
const DUPLEX_BUFFER_BYTES: usize = 1024 * 1024;

/// Serve until stdin EOF: the era guard owns stdio, the SDK
/// service sits behind an in-process duplex, and a pump copies
/// its responses back to stdout through the same lock the guard
/// writes with. The `RunningService` is kept alive on
/// `waiting()` for the whole session — dropping it would close
/// the server after the first request.
async fn serve_modern(handler: BlochHandler) -> std::io::Result<()> {
    let (client_side, server_side) = tokio::io::duplex(DUPLEX_BUFFER_BYTES);
    let (server_read, server_write) = tokio::io::split(server_side);
    let (client_read, client_write) = tokio::io::split(client_side);
    let stdout: crate::guard::SharedStdout = Arc::new(tokio::sync::Mutex::new(tokio::io::stdout()));

    let ordering = crate::guard::Ordering::new();
    let serve = tokio::spawn(async move {
        let transport = AsyncRwTransport::new_server(server_read, server_write);
        match handler.serve(transport).await {
            Ok(running) => running
                .waiting()
                .await
                .map(|_| ())
                .map_err(|e| std::io::Error::other(e.to_string())),
            // The client closed the connection before selecting a
            // lifecycle (a probe, a spawn check, a client that only
            // ever wanted the refusal): bare EOF is a clean end of
            // session, as it was when the transport was hand-rolled.
            Err(rmcp::service::ServerInitializeError::ConnectionClosed(_)) => Ok(()),
            Err(e) => Err(std::io::Error::other(e.to_string())),
        }
    });
    let mut pump = tokio::spawn(crate::guard::pump(
        client_read,
        Arc::clone(&stdout),
        Arc::clone(&ordering),
    ));

    // When stdin reaches EOF the guard shuts the duplex write side
    // down, which ends the SDK service; the pump ends when the
    // service's transport drops. The pump wait is bounded as a
    // backstop: a response still in flight when the service ends
    // gets its chance to drain, but a wedged pump cannot hang exit.
    let guard_result = crate::guard::run(tokio::io::stdin(), client_write, stdout, ordering).await;
    let serve_result = match serve.await {
        Ok(result) => result,
        Err(e) => Err(std::io::Error::other(e.to_string())),
    };
    if tokio::time::timeout(std::time::Duration::from_secs(2), &mut pump)
        .await
        .is_err()
    {
        pump.abort();
    }
    guard_result.and(serve_result)
}

/// Run the stdio MCP server until EOF on stdin.
///
/// stdout is the protocol channel — diagnostics go to stderr only.
/// Lines over [`RPC_MAX_LINE_BYTES`] are refused by the era guard
/// with a typed error and the server keeps serving. The domain
/// server is built synchronously (it indexes the roots); only the
/// framing runs on the async runtime, so the CLI's synchronous
/// command paths are untouched.
pub fn serve_stdio(config: ServerConfig) -> Result<()> {
    let server = McpServer::new(config)?;
    eprintln!(
        "bloch: MCP server ready ({} repos, {} entries, {} lessons)",
        server.index.repos.len(),
        server.index.entries.len(),
        server.index.lessons.len()
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(serve_modern(BlochHandler::new(server)))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests: ~50/50 validation / adversarial (repo convention)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_root(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("bloch-server-test-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    struct Fixture {
        base: PathBuf,
        repo_a: PathBuf,
        repo_b: PathBuf,
        ideas: PathBuf,
    }

    fn fixture() -> Fixture {
        let base = tmp_root("fleet");
        let repo_a = base.join("repo_a");
        let repo_b = base.join("repo_b");
        // repo_a: general lane, tracker generation 3, curated memory.
        let mem_a = repo_a.join(".ai").join("memory");
        fs::create_dir_all(mem_a.join("inbox")).unwrap();
        fs::write(
            mem_a.join("decisions.md"),
            "# Decisions\n\nThe widget sprocket decision: use sprockets everywhere.\n",
        )
        .unwrap();
        fs::write(mem_a.join("secret-notes.md"), "the password is hunter2\n").unwrap();
        fs::create_dir_all(repo_a.join("evidence")).unwrap();
        fs::write(
            repo_a.join("evidence").join("gate-run.txt"),
            "gate evidence",
        )
        .unwrap();
        fs::write(
            repo_a.join("CHECKLIST.md"),
            "---\ntracker_generation: 3\n---\n# Checklist\n\n## Core\n- [x] Sprockets wired memory:decisions.md\n- [x] Ghost item memory:missing.md\n- [ ] Open item\n- [~] Doing item\n",
        )
        .unwrap();
        crate::roadmap::bloch_init(&repo_a, Some(crate::roadmap::RepoKind::App)).unwrap();

        // repo_b: restricted lane, an nsfw lesson, an sfw attempt-fix lesson,
        // and an unlabelled attempt-fix (must default to nsfw).
        let mem_b = repo_b.join(".ai").join("memory");
        fs::create_dir_all(mem_b.join("inbox")).unwrap();
        fs::write(repo_b.join(".ai").join("lane"), "lane: restricted\n").unwrap();
        fs::write(
            mem_b.join("lessons.md"),
            "---\norigin_lane: restricted\ncontent_label: nsfw\nevidence: owner review 2026-10-27\n---\n# Lessons\n\n- adult scene pacing lesson: hold the beat two frames longer.\n",
        )
        .unwrap();
        fs::write(
            mem_b.join("inbox").join("1700000000-worker-attempt-fix-r-201.md"),
            "---\norigin_lane: restricted\ncontent_label: sfw\nevidence: lesson text is a rust build fix; reviewed\n---\n# Attempt fix — R-201\n\ncargo generics fix lesson: annotate the turbofish explicitly.\n",
        )
        .unwrap();
        fs::write(
            mem_b
                .join("inbox")
                .join("1700000001-worker-attempt-fix-r-999.md"),
            "# Attempt fix — R-999\n\nunlabelled mystery lesson about wombats.\n",
        )
        .unwrap();

        let ideas = base.join("IDEAS.md");
        fs::write(
            &ideas,
            "# Ideas\n\n| ID | Statement | Home | State | Aliases |\n|---|---|---|---|---|\n| IDEA-0001 | Central memory server | bloch | commissioned | bloch-mcp, memory-server |\n| IDEA-0002 | Widget toolkit | widgets | ideation | |\n",
        )
        .unwrap();
        Fixture {
            base,
            repo_a,
            repo_b,
            ideas,
        }
    }

    fn policy_text() -> String {
        let future = now_epoch() + 3600;
        format!(
            "lease repo_a worker-1 3 {}\nlease repo_a pax 3 {}\nlease repo_a worker-2 3 1\n",
            future, future
        )
    }

    fn server(fx: &Fixture) -> McpServer {
        let policy_path = fx.base.join("appa.toml");
        fs::write(&policy_path, policy_text()).unwrap();
        McpServer::new(ServerConfig {
            board_path: None,
            ideas_path: Some(fx.ideas.clone()),
            leases_path: None,
            policy_path: Some(policy_path),
            roots: vec![fx.repo_a.clone(), fx.repo_b.clone()],
        })
        .unwrap()
    }

    /// Drive one tool call and shape the outcome exactly as the
    /// wire presents it (text content + structuredContent on
    /// success, isError text on failure), so the domain assertions
    /// below read unchanged from the hand-rolled era. Wire framing
    /// itself is gated in `crate::guard` tests and `tests/era.rs`.
    fn call(server: &mut McpServer, name: &str, args: serde_json::Value) -> serde_json::Value {
        match server.call_tool(name, &args) {
            Ok(value) => serde_json::json!({
                "result": {
                    "content": [{
                        "text": serde_json::to_string_pretty(&value).unwrap_or_default(),
                        "type": "text"
                    }],
                    "isError": false,
                    "structuredContent": value
                }
            }),
            Err(msg) => serde_json::json!({
                "result": {
                    "content": [{"text": msg, "type": "text"}],
                    "isError": true
                }
            }),
        }
    }

    fn result_text(resp: &serde_json::Value) -> String {
        resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }

    fn is_error(resp: &serde_json::Value) -> bool {
        resp["result"]["isError"].as_bool().unwrap_or(false)
    }

    // --- validation -------------------------------------------------------

    #[test]
    fn tool_definitions_surface() {
        // The served surface, asserted on the definitions the SDK
        // handler converts from. Era negotiation (initialize
        // refusal, discover) is wire framing: gated in
        // `crate::guard` tests and `tests/era.rs`.
        let tools = tool_definitions();
        let tools = tools.as_array().unwrap();
        assert_eq!(tools.len(), 11);
        for t in tools {
            assert!(
                t["annotations"]["readOnlyHint"].is_boolean(),
                "tool {}",
                t["name"]
            );
            assert!(t["inputSchema"]["properties"].is_object());
        }
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for expected in [
            "memory_search",
            "memory_get",
            "memory_propose",
            "memory_promote",
            "tracker_state",
            "tracker_rollup",
            "idea_lookup",
            "idea_register",
            "lessons_search",
            "roadmap_state",
            "roadmap_rollup",
        ] {
            assert!(names.contains(&expected), "missing {}", expected);
        }
    }

    /// Canonical JSON (object keys sorted recursively) for
    /// order-insensitive payload comparison.
    fn canonical(value: &serde_json::Value) -> String {
        fn sort(value: &serde_json::Value) -> serde_json::Value {
            match value {
                serde_json::Value::Object(map) => {
                    let mut pairs: Vec<(&String, &serde_json::Value)> = map.iter().collect();
                    pairs.sort_by(|a, b| a.0.cmp(b.0));
                    let mut out = serde_json::Map::new();
                    for (k, v) in pairs {
                        out.insert(k.clone(), sort(v));
                    }
                    serde_json::Value::Object(out)
                }
                serde_json::Value::Array(items) => {
                    serde_json::Value::Array(items.iter().map(sort).collect())
                }
                other => other.clone(),
            }
        }
        serde_json::to_string(&sort(value)).unwrap()
    }

    #[test]
    fn rmcp_tools_match_definitions() {
        // The SDK-served surface must be the definitions, exactly:
        // same tools, same order, same schemas and annotations
        // under canonical comparison. This is the in-suite half of
        // the migration parity gate (the wire half is the probe).
        let served = serde_json::to_value(rmcp_tools().unwrap()).unwrap();
        assert_eq!(canonical(&served), canonical(&tool_definitions()));
    }

    #[test]
    fn memory_search_and_get() {
        let fx = fixture();
        let mut srv = server(&fx);
        let resp = call(
            &mut srv,
            "memory_search",
            serde_json::json!({"query": "widget sprocket"}),
        );
        assert!(!is_error(&resp));
        let text = result_text(&resp);
        assert!(text.contains("decisions.md"), "{}", text);
        let structured = &resp["result"]["structuredContent"];
        let id = structured["results"][0]["id"].as_str().unwrap().to_string();
        let resp = call(&mut srv, "memory_get", serde_json::json!({"id": id}));
        assert!(!is_error(&resp));
        assert!(result_text(&resp).contains("use sprockets everywhere"));
        let _ = fs::remove_dir_all(&fx.base);
    }

    #[test]
    fn rebuild_from_files_identical_answers() {
        let fx = fixture();
        let mut srv = server(&fx);
        let q = serde_json::json!({"query": "sprocket"});
        let before = call(&mut srv, "memory_search", q.clone());
        srv.rebuild().unwrap();
        let after = call(&mut srv, "memory_search", q);
        assert_eq!(before, after, "rebuild must not change answers");
        let _ = fs::remove_dir_all(&fx.base);
    }

    #[test]
    fn tracker_state_counts_and_overclaim() {
        let fx = fixture();
        let mut srv = server(&fx);
        let resp = call(
            &mut srv,
            "tracker_state",
            serde_json::json!({"repo": "repo_a"}),
        );
        assert!(!is_error(&resp));
        let v = &resp["result"]["structuredContent"];
        assert_eq!(v["generation"], 3);
        assert_eq!(v["done"], 2);
        assert_eq!(v["total"], 4);
        assert_eq!(
            v["overclaims"].as_array().unwrap().len(),
            1,
            "ghost citation is an overclaim"
        );
        let resp = call(&mut srv, "tracker_rollup", serde_json::json!({}));
        assert_eq!(
            resp["result"]["structuredContent"]["repos"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let _ = fs::remove_dir_all(&fx.base);
    }

    #[test]
    fn roadmap_state_and_rollup() {
        let fx = fixture();
        let mut srv = server(&fx);
        let resp = call(
            &mut srv,
            "roadmap_state",
            serde_json::json!({"repo": "repo_a"}),
        );
        assert!(!is_error(&resp), "{}", result_text(&resp));
        let v = &resp["result"]["structuredContent"];
        assert_eq!(v["kind"], "app");
        assert!(v["total"].as_u64().unwrap() > 10);
        assert!(v["next_open"]["id"].as_str().unwrap().starts_with("R-"));
        let resp = call(&mut srv, "roadmap_rollup", serde_json::json!({}));
        let repos = resp["result"]["structuredContent"]["repos"]
            .as_array()
            .unwrap();
        assert_eq!(repos.len(), 1, "only repo_a has a roadmap");
        let _ = fs::remove_dir_all(&fx.base);
    }

    #[test]
    fn idea_lookup_by_alias_and_register() {
        let fx = fixture();
        let mut srv = server(&fx);
        let resp = call(
            &mut srv,
            "idea_lookup",
            serde_json::json!({"query": "bloch-mcp"}),
        );
        let v = &resp["result"]["structuredContent"];
        assert_eq!(v["found"], true);
        assert_eq!(v["idea"]["id"], "IDEA-0001");
        let resp = call(
            &mut srv,
            "idea_register",
            serde_json::json!({"statement": "Fresh idea", "home": "bloch", "aliases": ["fresh"], "registrar": "pax"}),
        );
        assert!(!is_error(&resp), "{}", result_text(&resp));
        assert_eq!(resp["result"]["structuredContent"]["id"], "IDEA-0003");
        let _ = fs::remove_dir_all(&fx.base);
    }

    #[test]
    fn propose_then_promote_happy_path() {
        let fx = fixture();
        let mut srv = server(&fx);
        let resp = call(
            &mut srv,
            "memory_propose",
            serde_json::json!({
                "repo": "repo_a", "agent": "worker-1", "topic": "gate-finding",
                "content": "the sprocket gate passed at generation 3",
                "generation": 3
            }),
        );
        assert!(!is_error(&resp), "{}", result_text(&resp));
        let fname = resp["result"]["structuredContent"]["file"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(fx
            .repo_a
            .join(".ai")
            .join("memory")
            .join("inbox")
            .join(&fname)
            .exists());
        let resp = call(
            &mut srv,
            "memory_promote",
            serde_json::json!({
                "repo": "repo_a", "file": fname, "to": "patterns.md",
                "promoter": "pax", "evidence": "evidence/gate-run.txt"
            }),
        );
        assert!(!is_error(&resp), "{}", result_text(&resp));
        let patterns =
            fs::read_to_string(fx.repo_a.join(".ai").join("memory").join("patterns.md")).unwrap();
        assert!(patterns.contains("sprocket gate passed"));
        let _ = fs::remove_dir_all(&fx.base);
    }

    #[test]
    fn lessons_search_sfw_restricted_lesson_visible_to_general() {
        let fx = fixture();
        let mut srv = server(&fx);
        // Default clearance is general-only; an SFW lesson of restricted
        // origin transfers by content label (§9.1).
        let resp = call(
            &mut srv,
            "lessons_search",
            serde_json::json!({"query": "cargo generics"}),
        );
        assert!(!is_error(&resp));
        let v = &resp["result"]["structuredContent"];
        let matches = v["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1, "{}", v);
        assert_eq!(matches[0]["origin_lane"], "restricted");
        assert_eq!(matches[0]["content_label"], "sfw");
        // §10.4 fields ride along; a record without explicit lines
        // defaults to repo_task with the task ID as subject.
        assert_eq!(matches[0]["scope"], "repo_task");
        assert_eq!(matches[0]["subject"], "R-201");
        let _ = fs::remove_dir_all(&fx.base);
    }

    // --- adversarial ------------------------------------------------------

    #[test]
    fn nsfw_lesson_never_leaks_to_general_clearance() {
        let fx = fixture();
        let mut srv = server(&fx);
        let resp = call(
            &mut srv,
            "lessons_search",
            serde_json::json!({"query": "adult scene pacing"}),
        );
        let v = &resp["result"]["structuredContent"];
        assert!(
            v["matches"].as_array().unwrap().is_empty(),
            "nsfw lesson leaked: {}",
            v
        );
        // memory_search is walled the same way.
        let resp = call(
            &mut srv,
            "memory_search",
            serde_json::json!({"query": "adult scene pacing"}),
        );
        let v = &resp["result"]["structuredContent"];
        assert!(v["results"].as_array().unwrap().is_empty());
        // memory_get on the nsfw entry id is refused, not filtered silently.
        let resp = call(
            &mut srv,
            "memory_get",
            serde_json::json!({"id": "repo_b:.ai/memory/lessons.md"}),
        );
        assert!(is_error(&resp));
        assert!(
            result_text(&resp).contains("lane_forbidden"),
            "{}",
            result_text(&resp)
        );
        // A restricted-cleared caller CAN see it.
        let resp = call(
            &mut srv,
            "lessons_search",
            serde_json::json!({"query": "adult scene pacing", "clearance": "restricted"}),
        );
        let v = &resp["result"]["structuredContent"];
        assert_eq!(v["matches"].as_array().unwrap().len(), 1);
        let _ = fs::remove_dir_all(&fx.base);
    }

    #[test]
    fn unlabelled_restricted_lesson_defaults_nsfw() {
        let fx = fixture();
        let mut srv = server(&fx);
        let resp = call(
            &mut srv,
            "lessons_search",
            serde_json::json!({"query": "wombats"}),
        );
        let v = &resp["result"]["structuredContent"];
        assert!(
            v["matches"].as_array().unwrap().is_empty(),
            "unlabelled restricted lesson must default to nsfw and be walled: {}",
            v
        );
        let resp = call(
            &mut srv,
            "lessons_search",
            serde_json::json!({"query": "wombats", "clearance": "restricted"}),
        );
        let v = &resp["result"]["structuredContent"];
        assert_eq!(v["matches"].as_array().unwrap().len(), 1);
        assert_eq!(v["matches"][0]["content_label"], "nsfw");
        let _ = fs::remove_dir_all(&fx.base);
    }

    #[test]
    fn legacy_lane_names_still_work_on_read() {
        // Back-compat (pre-0.3 records and callers): the legacy estate
        // lane names keep working everywhere they are READ — the
        // per-call clearance argument and policy-as-data — while
        // everything the server emits is generic (asserted by the
        // canonical tests above).
        let fx = fixture();
        let mut srv = server(&fx);
        let resp = call(
            &mut srv,
            "lessons_search",
            serde_json::json!({"query": "adult scene pacing", "clearance": "ntranch"}),
        );
        let v = &resp["result"]["structuredContent"];
        assert_eq!(v["matches"].as_array().unwrap().len(), 1);
        assert_eq!(v["matches"][0]["origin_lane"], "restricted");
        let _ = fs::remove_dir_all(&fx.base);

        let policy =
            parse_policy("repo x lane qompassai\nrepo y lane ntranch\nclearance bob ntranch\n");
        assert_eq!(policy.repo_lanes["x"], Lane::General);
        assert_eq!(policy.repo_lanes["y"], Lane::Restricted);
        assert_eq!(policy.clearances["bob"], Clearance::All);
    }

    #[test]
    fn propose_stale_generation_and_leaseless_and_expired() {
        let fx = fixture();
        let mut srv = server(&fx);
        let base_args = serde_json::json!({
            "repo": "repo_a", "agent": "worker-1", "topic": "t",
            "content": "finding", "generation": 2
        });
        let resp = call(&mut srv, "memory_propose", base_args);
        assert!(is_error(&resp));
        assert!(
            result_text(&resp).contains("stale_generation"),
            "{}",
            result_text(&resp)
        );

        let resp = call(
            &mut srv,
            "memory_propose",
            serde_json::json!({
                "repo": "repo_a", "agent": "stranger", "topic": "t",
                "content": "finding", "generation": 3
            }),
        );
        assert!(
            result_text(&resp).contains("lease_required"),
            "{}",
            result_text(&resp)
        );

        let resp = call(
            &mut srv,
            "memory_propose",
            serde_json::json!({
                "repo": "repo_a", "agent": "worker-2", "topic": "t",
                "content": "finding", "generation": 3
            }),
        );
        assert!(
            result_text(&resp).contains("lease_expired"),
            "{}",
            result_text(&resp)
        );
        let _ = fs::remove_dir_all(&fx.base);
    }

    #[test]
    fn proposer_cannot_promote_own_and_promote_gates() {
        let fx = fixture();
        let mut srv = server(&fx);
        // pax proposes (pax holds a lease in the fixture policy).
        let resp = call(
            &mut srv,
            "memory_propose",
            serde_json::json!({
                "repo": "repo_a", "agent": "pax", "topic": "self-dealing",
                "content": "pax finding", "generation": 3
            }),
        );
        assert!(!is_error(&resp), "{}", result_text(&resp));
        let fname = resp["result"]["structuredContent"]["file"]
            .as_str()
            .unwrap()
            .to_string();
        // Non-gatekeeper promoter refused.
        let resp = call(
            &mut srv,
            "memory_promote",
            serde_json::json!({
                "repo": "repo_a", "file": fname, "to": "patterns.md",
                "promoter": "worker-9", "evidence": "evidence/gate-run.txt"
            }),
        );
        assert!(
            result_text(&resp).contains("promoter_not_gatekeeper"),
            "{}",
            result_text(&resp)
        );
        // Proposer == promoter refused (checker separation).
        let resp = call(
            &mut srv,
            "memory_promote",
            serde_json::json!({
                "repo": "repo_a", "file": fname, "to": "patterns.md",
                "promoter": "pax", "evidence": "evidence/gate-run.txt"
            }),
        );
        assert!(
            result_text(&resp).contains("proposer_is_promoter"),
            "{}",
            result_text(&resp)
        );
        // Unresolvable evidence refused (owner promoting worker's entry,
        // but the evidence pointer is bogus).
        let resp = call(
            &mut srv,
            "memory_propose",
            serde_json::json!({
                "repo": "repo_a", "agent": "worker-1", "topic": "ev-test",
                "content": "finding", "generation": 3
            }),
        );
        let fname2 = resp["result"]["structuredContent"]["file"]
            .as_str()
            .unwrap()
            .to_string();
        let resp = call(
            &mut srv,
            "memory_promote",
            serde_json::json!({
                "repo": "repo_a", "file": fname2, "to": "patterns.md",
                "promoter": "owner", "evidence": "nope/missing.md"
            }),
        );
        assert!(
            result_text(&resp).contains("evidence_unresolved"),
            "{}",
            result_text(&resp)
        );
        let _ = fs::remove_dir_all(&fx.base);
    }

    #[test]
    fn nsfw_propose_into_general_refused_and_relabel_attempt_refused() {
        let fx = fixture();
        let mut srv = server(&fx);
        // Direct nsfw propose into a general-lane repo.
        let resp = call(
            &mut srv,
            "memory_propose",
            serde_json::json!({
                "repo": "repo_a", "agent": "worker-1", "topic": "adult",
                "content": "adult content finding", "generation": 3,
                "content_label": "nsfw", "label_evidence": "owner review"
            }),
        );
        assert!(is_error(&resp));
        assert!(
            result_text(&resp).contains("lane_wall_refusal"),
            "{}",
            result_text(&resp)
        );
        // Transfer-time relabel attempt: restricted-origin content claimed
        // sfw WITHOUT evidence fails closed to nsfw -> same refusal.
        let resp = call(
            &mut srv,
            "memory_propose",
            serde_json::json!({
                "repo": "repo_a", "agent": "worker-1", "topic": "transfer",
                "content": "adult-lane finding", "generation": 3,
                "source_repo": "repo_b", "content_label": "sfw"
            }),
        );
        assert!(is_error(&resp));
        assert!(
            result_text(&resp).contains("lane_wall_refusal"),
            "{}",
            result_text(&resp)
        );
        // The same transfer WITH positive SFW evidence is allowed, and
        // the origin stays restricted in the filed record.
        let resp = call(
            &mut srv,
            "memory_propose",
            serde_json::json!({
                "repo": "repo_a", "agent": "worker-1", "topic": "transfer-ok",
                "content": "a rust build fix learned in the other lane",
                "generation": 3, "source_repo": "repo_b",
                "content_label": "sfw",
                "label_evidence": "lesson text is a compiler fix; reviewed 2026-10-27"
            }),
        );
        assert!(!is_error(&resp), "{}", result_text(&resp));
        let v = &resp["result"]["structuredContent"];
        assert_eq!(v["origin_lane"], "restricted");
        assert_eq!(v["content_label"], "sfw");
        let fname = v["file"].as_str().unwrap().to_string();
        let filed = fs::read_to_string(
            fx.repo_a
                .join(".ai")
                .join("memory")
                .join("inbox")
                .join(fname),
        )
        .unwrap();
        assert!(filed.contains("origin_lane: restricted"), "{}", filed);
        let _ = fs::remove_dir_all(&fx.base);
    }

    #[test]
    fn unknown_tool_and_missing_entry_errors() {
        // The wire-framing half of the pre-migration
        // `malformed_rpc_and_unknown_things` (non-object frames,
        // batches, unknown methods) moved with the framing: guard
        // tests + `tests/era.rs`. What remains here is the domain
        // half: unknown tools and missing entries are typed
        // isError results, not crashes.
        let fx = fixture();
        let mut srv = server(&fx);
        let resp = call(&mut srv, "memory_frobnicate", serde_json::json!({}));
        assert!(is_error(&resp));
        assert!(result_text(&resp).contains("unknown_tool"));
        let resp = call(
            &mut srv,
            "memory_get",
            serde_json::json!({"id": "repo_a:nope.md"}),
        );
        assert!(result_text(&resp).contains("entry_not_found"));
        let _ = fs::remove_dir_all(&fx.base);
    }

    #[test]
    fn oversized_payloads_refused() {
        let fx = fixture();
        let mut srv = server(&fx);
        let long_query = "x".repeat(SEARCH_QUERY_MAX + 1);
        let resp = call(
            &mut srv,
            "memory_search",
            serde_json::json!({"query": long_query}),
        );
        assert!(
            result_text(&resp).contains("query_too_long"),
            "{}",
            result_text(&resp)
        );
        let big_content = "y".repeat(PROPOSE_CONTENT_MAX + 1);
        let resp = call(
            &mut srv,
            "memory_propose",
            serde_json::json!({
                "repo": "repo_a", "agent": "worker-1", "topic": "big",
                "content": big_content, "generation": 3
            }),
        );
        assert!(
            result_text(&resp).contains("payload_too_large"),
            "{}",
            result_text(&resp)
        );
        let _ = fs::remove_dir_all(&fx.base);
    }

    #[test]
    fn secret_shaped_files_never_indexed() {
        let fx = fixture();
        let mut srv = server(&fx);
        let resp = call(
            &mut srv,
            "memory_search",
            serde_json::json!({"query": "hunter2"}),
        );
        let v = &resp["result"]["structuredContent"];
        assert!(
            v["results"].as_array().unwrap().is_empty(),
            "secret leaked: {}",
            v
        );
        let _ = fs::remove_dir_all(&fx.base);
    }

    #[test]
    fn private_memory_root_refused() {
        if let Some(home) = std::env::var_os("HOME") {
            let home_path = PathBuf::from(home);
            assert!(guard_root(&home_path).is_err(), "home root must be refused");
            assert!(guard_root(&home_path.join("memory")).is_err());
        }
    }

    #[test]
    fn idea_register_duplicate_alias_refused() {
        let fx = fixture();
        let mut srv = server(&fx);
        let resp = call(
            &mut srv,
            "idea_register",
            serde_json::json!({
                "statement": "Another memory server", "home": "elsewhere",
                "aliases": ["bloch-mcp"], "registrar": "owner"
            }),
        );
        assert!(is_error(&resp));
        assert!(
            result_text(&resp).contains("suspected_duplicate"),
            "{}",
            result_text(&resp)
        );
        let resp = call(
            &mut srv,
            "idea_register",
            serde_json::json!({
                "statement": "Sneaky", "home": "x", "aliases": [], "registrar": "worker-1"
            }),
        );
        assert!(result_text(&resp).contains("registrar_not_gatekeeper"));
        let _ = fs::remove_dir_all(&fx.base);
    }
}
