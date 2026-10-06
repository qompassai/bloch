//! bloch: ranked, budgeted codebase maps for coding agents.
use bloch::{
    detect_skills, diff_maps, get_file_recency_boost, get_file_why, get_memory_context,
    get_session_state, get_test_status, git_ahead_behind, git_wip_detail, map_repo, memory_inbox,
    memory_promote, memory_read, read_memory_cache, render_sphere, skill_boost, todo_add,
    todo_done, todo_drop, todo_list, write_memory_cache, write_memory_summary, CachedMap,
    Definition, MapConfig, RefKey, RefKind,
};
use clap::Parser;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "bloch")]
#[command(version)]
#[command(about = "Ranked, budgeted codebase maps for coding agents")]
struct Args {
    /// Root directory of the repo to map
    root: Option<PathBuf>,
    /// Character budget for the output
    #[arg(long, default_value_t = 12000)]
    budget: usize,
    /// Output file (- for stdout)
    #[arg(long, default_value = "-")]
    out: String,
    /// Render Bloch sphere visualization of top definitions
    #[arg(long)]
    sphere: bool,
    /// Max points on the sphere (default 15)
    #[arg(long, default_value_t = 15)]
    sphere_points: usize,
    /// Write summary to .ai/memory/bloch-summary.md and cache to bloch-last.json
    #[arg(long)]
    update_memory: bool,
    /// Show diff against cached .ai/memory/bloch-last.json instead of full map
    #[arg(long)]
    diff: bool,
    /// Manually specify active skills (repeatable). Auto-detected from .ai/skills/ otherwise.
    /// Boosts definitions in languages matching the skill (e.g. tiger-style-rust boosts Rust).
    #[arg(long)]
    skill: Vec<String>,
    /// Disable skill-aware ranking boost
    #[arg(long)]
    no_skill_boost: bool,
    /// Use SCIP index (.scip/index.scip) for precise definitions/references if available.
    /// Falls back to tree-sitter when no index exists or it is stale.
    #[arg(long)]
    scip: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Agent persistent memory operations (read/write .ai/memory/ files)
    Memory {
        /// Root of the repository (defaults to current directory)
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[command(subcommand)]
        op: MemoryOp,
    },
    /// Todo list operations (.ai/todo.md)
    Todo {
        /// Root of the repository (defaults to current directory)
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[command(subcommand)]
        op: TodoOp,
    },
    /// Git repository helpers for agents
    Git {
        /// Root of the repository (defaults to current directory)
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[command(subcommand)]
        op: GitOp,
    },
}

#[derive(clap::Subcommand)]
enum TodoOp {
    /// Add a todo item
    Add {
        /// Item text
        text: String,
    },
    /// List all todo items
    List,
    /// Mark a todo item done
    Done {
        /// Item ID
        id: usize,
    },
    /// Remove a todo item
    Drop {
        /// Item ID
        id: usize,
    },
}

#[derive(clap::Subcommand)]
enum GitOp {
    /// Agent-friendly git status (branch, ahead/behind, dirty files)
    Status,
    /// Detailed WIP: per-file diff stats for uncommitted changes
    Wip,
}

#[derive(clap::Subcommand)]
enum MemoryOp {
    /// Read a memory file (e.g. `bloch memory get handoff.md`)
    Get {
        /// Memory file name (relative to .ai/memory/)
        file: String,
    },
    /// Write a memory file, overwriting existing content
    Set {
        /// Memory file name (relative to .ai/memory/)
        file: String,
        /// Content to write
        content: String,
    },
    /// Append content to a memory file
    Append {
        /// Memory file name (relative to .ai/memory/)
        file: String,
        /// Content to append
        content: String,
    },
    /// List all memory files with sizes
    List,
    /// Read INDEX.md and current.md (session-start helper, bounded)
    Read,
    /// Write a timestamped inbox entry
    Inbox {
        /// Topic slug for the file name
        topic: String,
        /// Content to write
        content: String,
        /// Agent name for the file name (default: agent)
        #[arg(long, default_value = "agent")]
        agent: String,
    },
    /// Promote an inbox entry to a curated memory file
    Promote {
        /// Inbox file name (e.g. 1234567890-agent-topic.md)
        file: String,
        /// Target: decisions.md, patterns.md, or current.md
        #[arg(long)]
        to: String,
    },
}

fn main() {
    let args = Args::parse();

    // Skill-aware ranking: detect active skills and boost matching languages
    let detect_root = args.root.clone().unwrap_or(PathBuf::from("."));
    let active_skills: Vec<String> = if args.no_skill_boost {
        Vec::new()
    } else if args.skill.is_empty() {
        detect_skills(&detect_root)
    } else {
        args.skill.clone()
    };
    if !active_skills.is_empty() {
        eprintln!("bloch: active skills: {}", active_skills.join(", "));
    }

    // Memory subcommand: handle and exit early (does not need map root)
    if let Some(Command::Memory { root, op }) = &args.command {
        let root = root.canonicalize().unwrap_or(root.clone());
        let mem_dir = root.join(".ai").join("memory");
        match op {
            MemoryOp::Get { file } => {
                let path = mem_dir.join(file);
                match std::fs::read_to_string(&path) {
                    Ok(content) => print!("{}", content),
                    Err(e) => {
                        eprintln!("bloch: cannot read {}: {}", path.display(), e);
                        std::process::exit(1);
                    }
                }
            }
            MemoryOp::Set { file, content } => {
                if let Err(e) = std::fs::create_dir_all(&mem_dir) {
                    eprintln!("bloch: cannot create {}: {}", mem_dir.display(), e);
                    std::process::exit(1);
                }
                let path = mem_dir.join(file);
                match std::fs::write(&path, content) {
                    Ok(()) => eprintln!("bloch: wrote {}", path.display()),
                    Err(e) => {
                        eprintln!("bloch: cannot write {}: {}", path.display(), e);
                        std::process::exit(1);
                    }
                }
            }
            MemoryOp::Append { file, content } => {
                if let Err(e) = std::fs::create_dir_all(&mem_dir) {
                    eprintln!("bloch: cannot create {}: {}", mem_dir.display(), e);
                    std::process::exit(1);
                }
                let path = mem_dir.join(file);
                let existing = std::fs::read_to_string(&path).unwrap_or_default();
                let new_content = if existing.is_empty() {
                    content.clone()
                } else {
                    format!("{}\n{}", existing.trim_end(), content)
                };
                match std::fs::write(&path, new_content) {
                    Ok(()) => eprintln!("bloch: appended to {}", path.display()),
                    Err(e) => {
                        eprintln!("bloch: cannot write {}: {}", path.display(), e);
                        std::process::exit(1);
                    }
                }
            }
            MemoryOp::List => match std::fs::read_dir(&mem_dir) {
                Ok(entries) => {
                    let mut files: Vec<(String, u64)> = entries
                        .filter_map(|e| e.ok())
                        .filter_map(|e| {
                            let name = e.file_name().to_str()?.to_string();
                            let size = e.metadata().ok()?.len();
                            Some((name, size))
                        })
                        .collect();
                    files.sort();
                    if files.is_empty() {
                        println!("(no memory files in {})", mem_dir.display());
                    } else {
                        for (name, size) in files {
                            println!("{:>8}  {}", size, name);
                        }
                    }
                }
                Err(_) => {
                    println!("(no memory directory at {})", mem_dir.display());
                }
            },
            MemoryOp::Read => {
                print!("{}", memory_read(&root));
            }
            MemoryOp::Inbox {
                topic,
                content,
                agent,
            } => match memory_inbox(&root, agent, topic, content) {
                Ok(fname) => eprintln!("bloch: inbox entry written: {}", fname),
                Err(e) => {
                    eprintln!("bloch: error: {}", e);
                    std::process::exit(1);
                }
            },
            MemoryOp::Promote { file, to } => match memory_promote(&root, file, to) {
                Ok(msg) => eprintln!("bloch: {}", msg),
                Err(e) => {
                    eprintln!("bloch: error: {}", e);
                    std::process::exit(1);
                }
            },
        }
        return;
    }

    // Todo subcommand: handle and exit early
    if let Some(Command::Todo { root, op }) = &args.command {
        let root = root.canonicalize().unwrap_or(root.clone());
        match op {
            TodoOp::Add { text } => match todo_add(&root, text) {
                Ok(id) => eprintln!("bloch: todo #{} added", id),
                Err(e) => {
                    eprintln!("bloch: error: {}", e);
                    std::process::exit(1);
                }
            },
            TodoOp::List => {
                let items = todo_list(&root);
                if items.is_empty() {
                    println!("(no todos)");
                } else {
                    for it in items {
                        println!(
                            "#{} [{}] {}",
                            it.id,
                            if it.done { "x" } else { " " },
                            it.text
                        );
                    }
                }
            }
            TodoOp::Done { id } => match todo_done(&root, *id) {
                Ok(()) => eprintln!("bloch: todo #{} done", id),
                Err(e) => {
                    eprintln!("bloch: error: {}", e);
                    std::process::exit(1);
                }
            },
            TodoOp::Drop { id } => match todo_drop(&root, *id) {
                Ok(()) => eprintln!("bloch: todo #{} dropped", id),
                Err(e) => {
                    eprintln!("bloch: error: {}", e);
                    std::process::exit(1);
                }
            },
        }
        return;
    }

    // Git subcommand: handle and exit early
    if let Some(Command::Git { root, op }) = &args.command {
        let root = root.canonicalize().unwrap_or(root.clone());
        match op {
            GitOp::Status => {
                let (ahead, behind) = git_ahead_behind(&root);
                let branch = bloch::run_git(&root, &["branch", "--show-current"]);
                if !branch.is_empty() {
                    println!("branch: {}", branch);
                }
                if ahead > 0 || behind > 0 {
                    println!("ahead {}, behind {}", ahead, behind);
                }
                print!("{}", get_session_state(&root));
            }
            GitOp::Wip => {
                print!("{}", git_wip_detail(&root));
            }
        }
        return;
    }

    let root_arg = args.root.clone().unwrap_or_else(|| {
        eprintln!("bloch: error: <ROOT> is required when not using a subcommand");
        std::process::exit(2);
    });
    let root = root_arg.canonicalize().unwrap_or(root_arg.clone());

    // Use the public API for mapping (was duplicated logic)
    let config = MapConfig {
        use_scip: args.scip,
    };
    let repo_map = match map_repo(&root, &config) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("bloch: error: {}", e);
            std::process::exit(1);
        }
    };
    if repo_map.used_scip {
        eprintln!("bloch: using SCIP index ({} files)", repo_map.files.len());
    }
    let per_file = repo_map.files;
    let files: Vec<PathBuf> = per_file.keys().map(|rel| root.join(rel)).collect();

    // Aggregate reference index (Rust only)
    let mut name_df: HashMap<String, usize> = HashMap::new();
    for data in per_file.values() {
        if data.lang == "rust" {
            for d in &data.defs {
                *name_df.entry(d.name.clone()).or_insert(0) += 1;
            }
        }
    }

    let refs_outside = |home_rel: &str, kind: RefKind, key: &RefKey| -> usize {
        per_file
            .iter()
            .filter(|(rel, _)| *rel != home_rel)
            .map(|(_, data)| data.refs.get(kind, key))
            .sum()
    };

    // Rank definitions
    let mut ranked: Vec<(f64, bool, String, Definition)> = Vec::new();
    for (rel, data) in &per_file {
        if data.lang != "rust" {
            continue;
        }
        for d in &data.defs {
            let df = *name_df.get(&d.name).unwrap_or(&1) as f64;
            let score = match (d.kind.as_str(), &d.impl_target) {
                ("fn", Some(t)) => {
                    let q = refs_outside(
                        rel,
                        RefKind::Call,
                        &RefKey::Call(d.name.clone(), Some(t.clone())),
                    ) + refs_outside(
                        rel,
                        RefKind::Call,
                        &RefKey::Call(d.name.clone(), Some("self".to_string())),
                    ) + refs_outside(
                        rel,
                        RefKind::Call,
                        &RefKey::Call(d.name.clone(), Some("Self".to_string())),
                    );
                    let bare = refs_outside(rel, RefKind::Call, &RefKey::Call(d.name.clone(), None))
                        as f64;
                    (q as f64 + bare) / df
                }
                ("fn", None) => {
                    let last_seg = data.modpath.last().cloned();
                    let q = last_seg
                        .map(|s| {
                            refs_outside(rel, RefKind::Call, &RefKey::Call(d.name.clone(), Some(s)))
                                as f64
                        })
                        .unwrap_or(0.0);
                    let bare = refs_outside(rel, RefKind::Call, &RefKey::Call(d.name.clone(), None))
                        as f64;
                    (q + bare) / df
                }
                ("struct" | "enum" | "trait", _) => {
                    refs_outside(rel, RefKind::Type, &RefKey::Single(d.name.clone())) as f64 / df
                }
                ("type", _) => {
                    let w = if d.impl_target.is_some() { 0.1 } else { 1.0 };
                    w * refs_outside(rel, RefKind::Type, &RefKey::Single(d.name.clone())) as f64
                        / df
                }
                ("mod", _) => {
                    refs_outside(rel, RefKind::Path, &RefKey::Single(d.name.clone())) as f64 / df
                }
                _ => refs_outside(rel, RefKind::Ident, &RefKey::Single(d.name.clone())) as f64 / df,
            };
            // Recency boost for handoff fidelity
            let boost = get_file_recency_boost(&root, rel, 2.0);
            // Skill boost: 1.5x for languages matching active skills
            let sboost = skill_boost(&data.lang, &active_skills);
            ranked.push((score * boost * sboost, d.is_pub, rel.clone(), d.clone()));
        }
    }

    ranked.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.1.cmp(&a.1))
    });

    // Bloch normalization: convert raw scores to a probability distribution.
    // Like the Bloch vector constraint |r| = 1, the probabilities sum to 1.0.
    // P(d) = score(d) / Σ score(d') — the probability that definition d is
    // "where to look", analogous to quantum measurement probabilities.
    let total_score: f64 = ranked.iter().map(|(s, _, _, _)| s.max(0.0)).sum();
    let ranked_probs: Vec<(f64, f64, bool, String, Definition)> = ranked
        .iter()
        .map(|(score, is_pub, rel, d)| {
            let prob = if total_score > 0.0 {
                score.max(0.0) / total_score
            } else {
                0.0
            };
            (*score, prob, *is_pub, rel.clone(), d.clone())
        })
        .collect();

    // Language summary
    let mut lang_counts: HashMap<&str, usize> = HashMap::new();
    for data in per_file.values() {
        *lang_counts.entry(data.lang.as_str()).or_insert(0) += 1;
    }
    let mut lang_vec: Vec<(&str, usize)> = lang_counts.into_iter().collect();
    lang_vec.sort_by_key(|(l, _)| *l);
    let lang_summary: Vec<String> = lang_vec
        .iter()
        .map(|(l, c)| format!("{}:{}", l, c))
        .collect();

    let n_defs: usize = per_file.values().map(|d| d.defs.len()).sum();
    let n_rust_defs: usize = per_file
        .values()
        .filter(|d| d.lang == "rust")
        .map(|d| d.defs.len())
        .sum();

    let mut lines: Vec<String> = Vec::new();

    // Handoff section first
    let session = get_session_state(&root);
    let memory = get_memory_context(&root, 3000);
    let test_status = get_test_status(&root, 800);

    if !session.is_empty() || !memory.is_empty() || !test_status.is_empty() {
        lines.push(format!("# {}", "=".repeat(60)));
        lines.push("# HANDOFF: where the last session left off".to_string());
        lines.push(format!("# {}", "=".repeat(60)));
        if !session.is_empty() {
            lines.push(String::new());
            lines.extend(session.lines().map(|l| l.to_string()));
        }
        if !memory.is_empty() {
            lines.push(String::new());
            lines.extend(memory.lines().map(|l| l.to_string()));
        }
        if !test_status.is_empty() {
            lines.push(String::new());
            lines.extend(test_status.lines().map(|l| l.to_string()));
        }
        lines.push(String::new());
        lines.push(format!("# {}", "=".repeat(60)));
        lines.push("# CODE MAP".to_string());
        lines.push(format!("# {}", "=".repeat(60)));
        lines.push(String::new());
    }

    lines.push(format!("# bloch: {}", root.display()));
    lines.push(format!(
        "# {} files, {} definitions ({} rust), budget {} chars",
        files.len(),
        n_defs,
        n_rust_defs,
        args.budget
    ));
    lines.push(format!("# languages: {}", lang_summary.join(", ")));
    lines.push(String::new());

    let mut used: usize = lines.iter().map(|l| l.len() + 1).sum();
    let mut emitted: HashSet<String> = HashSet::new();
    let mut why_cache: HashMap<String, String> = HashMap::new();

    for (score, prob, is_pub, rel, d) in &ranked_probs {
        if *score <= 0.0 && !emitted.is_empty() {
            break;
        }
        let pub_s = if *is_pub { "pub " } else { "" };
        let impl_s = d
            .impl_target
            .as_ref()
            .map(|t| format!(" [impl {}]", t))
            .unwrap_or_default();
        let why = why_cache
            .entry(rel.clone())
            .or_insert_with(|| get_file_why(&root, rel, 160))
            .clone();
        let why_s = if why.is_empty() {
            String::new()
        } else {
            format!("  # why: {}", why)
        };
        let line = format!(
            "{}: {}{} {}{} :: {}  (p={:.3}, score={:.1}){}",
            rel, pub_s, d.kind, d.name, impl_s, d.sig, prob, score, why_s
        );
        if used + line.len() + 1 > args.budget && !emitted.is_empty() {
            break;
        }
        lines.push(line.clone());
        used += line.len() + 1;
        emitted.insert(rel.clone());
    }

    // Non-Rust highlights
    lines.push(String::new());
    lines.push("# non-rust structure:".to_string());
    let mut sorted_rels: Vec<&String> = per_file.keys().collect();
    sorted_rels.sort();
    'outer: for rel in sorted_rels {
        let data = &per_file[rel];
        if data.lang == "rust" || data.defs.is_empty() {
            continue;
        }
        for d in data.defs.iter().take(5) {
            let line = format!("{}: [{}] {} {} :: {}", rel, d.lang, d.kind, d.name, d.sig);
            if used + line.len() + 1 > args.budget {
                break 'outer;
            }
            lines.push(line.clone());
            used += line.len() + 1;
        }
    }

    lines.push(String::new());
    lines.push(format!("# table of contents: {} files", files.len()));
    let mut sorted_rels: Vec<&String> = per_file.keys().collect();
    sorted_rels.sort();
    for rel in sorted_rels {
        let data = &per_file[rel];
        lines.push(format!(
            "#   {} [{}] ({} defs)",
            rel,
            data.lang,
            data.defs.len()
        ));
    }

    // Diff mode: compare against cached map and output only changes
    if args.diff {
        // Build current state as CachedMap
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| {
                // Simple ISO-ish format from epoch seconds
                format!("{}", d.as_secs())
            })
            .unwrap_or_default();
        let git_head = bloch::run_git(&root, &["rev-parse", "HEAD"]);
        let mut files_map = HashMap::new();
        for (rel, data) in &per_file {
            let names: Vec<String> = data.defs.iter().map(|d| d.name.clone()).collect();
            files_map.insert(rel.clone(), names);
        }
        let top_defs: Vec<(String, String, String, f64)> = ranked_probs
            .iter()
            .take(20)
            .map(|(_, prob, _, rel, d)| (rel.clone(), d.name.clone(), d.kind.clone(), *prob))
            .collect();
        let mut languages = HashMap::new();
        for data in per_file.values() {
            *languages.entry(data.lang.clone()).or_insert(0) += 1;
        }
        let current = CachedMap {
            timestamp: timestamp.clone(),
            git_head: git_head.clone(),
            files: files_map,
            top_defs,
            languages,
        };
        match read_memory_cache(&root) {
            Ok(Some(old)) => {
                let diff = diff_maps(&old, &current);
                if args.out == "-" {
                    print!("{}", diff);
                } else {
                    fs::write(&args.out, &diff).expect("write output");
                }
            }
            Ok(None) => {
                eprintln!("bloch: no cached map found at .ai/memory/bloch-last.json");
                eprintln!("bloch: run with --update-memory first to create one");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("bloch: error reading cache: {}", e);
                eprintln!("bloch: cache may be corrupt; delete .ai/memory/bloch-last.json and re-run with --update-memory");
                std::process::exit(1);
            }
        }
        return;
    }

    // Bloch sphere visualization (if requested)
    if args.sphere {
        let mut recency_map = HashMap::new();
        let mut visibility_map = HashMap::new();
        for (_score, _prob, _pub, rel, d) in &ranked_probs {
            let key = format!("{}::{}", rel, d.name);
            let boost = get_file_recency_boost(&root, rel, 2.0);
            let rec_norm = ((boost - 1.0) / 1.0).clamp(0.0, 1.0);
            recency_map.insert(key.clone(), rec_norm);
            // Visibility (public=1.0, private=0.0) is independent of score,
            // unlike the old centrality which was just normalized score
            visibility_map.insert(key, if d.is_pub { 1.0 } else { 0.0 });
        }
        let sphere = render_sphere(
            &ranked_probs,
            &recency_map,
            &visibility_map,
            args.sphere_points,
        );
        lines.push(String::new());
        lines.extend(sphere.lines().map(|l| l.to_string()));
    }

    let out = lines.join("\n") + "\n";
    if args.out == "-" {
        print!("{}", out);
    } else {
        fs::write(&args.out, &out).expect("write output");
    }

    // Update memory: write summary and cache
    if args.update_memory {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| format!("{}", d.as_secs()))
            .unwrap_or_default();
        let git_head = bloch::run_git(&root, &["rev-parse", "HEAD"]);
        let mut files_map = HashMap::new();
        for (rel, data) in &per_file {
            let names: Vec<String> = data.defs.iter().map(|d| d.name.clone()).collect();
            files_map.insert(rel.clone(), names);
        }
        let top_defs: Vec<(String, String, String, f64)> = ranked_probs
            .iter()
            .take(20)
            .map(|(_, prob, _, rel, d)| (rel.clone(), d.name.clone(), d.kind.clone(), *prob))
            .collect();
        let mut languages = HashMap::new();
        for data in per_file.values() {
            *languages.entry(data.lang.clone()).or_insert(0) += 1;
        }
        let cached = CachedMap {
            timestamp,
            git_head,
            files: files_map,
            top_defs,
            languages,
        };
        match write_memory_summary(&root, &cached) {
            Ok(p) => eprintln!("bloch: wrote {}", p.display()),
            Err(e) => eprintln!("bloch: failed to write summary: {}", e),
        }
        match write_memory_cache(&root, &cached) {
            Ok(p) => eprintln!("bloch: wrote {}", p.display()),
            Err(e) => eprintln!("bloch: failed to write cache: {}", e),
        }
    }
    eprintln!(
        "bloch: {} files, {} defs, {} rust files in budget, {} chars",
        files.len(),
        n_defs,
        emitted.len(),
        out.len()
    );
}
