# Changelog

All notable changes to bloch will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- Published to crates.io as package `blochs` (the name `bloch` was taken in Oct 2026); the library and binary remain named `bloch`.

## [0.4.0] - 2026-10-27

### Changed
- `bloch serve` now runs on the rmcp SDK and speaks the modern MCP
  protocol era only (2026-07-28): first contact is `server/discover`,
  and legacy `initialize` handshakes are refused with a typed error
  naming the supported protocol versions. The 11-tool surface —
  names, input schemas, annotations, and behavior — is unchanged,
  as are all CLI commands; only the stdio framing layer moved.

## [0.3.0] - 2026-10-27

### Added
- `bloch serve`: MCP server over stdio (hand-rolled JSON-RPC on the
  existing serde_json dependency — no new dependencies) carrying the
  roadmap system's cross-repo lesson sharing. 11 tools with annotations
  from birth:
  `memory_search`, `memory_get` (read, lane-filtered), `memory_propose`
  (inbox-only write gated by fencing generation + an attention lease,
  verified server-side against policy-as-data), `memory_promote`
  (curator gate act; proposer ≠ promoter; evidence must resolve;
  gated nsfw→sfw reclassification), `tracker_state`, `tracker_rollup`,
  `idea_lookup`, `idea_register` (gated), `lessons_search`,
  `roadmap_state`, `roadmap_rollup`.
- Content-lane enforcement in server code: nsfw-labeled entries
  are never returned to callers whose clearance covers only the
  general lane; proposing nsfw content into a general-lane repo is a
  typed refusal; labels are never changed at transfer time. Policy
  (repo lanes, caller clearances, leases) is a plain data file
  supplied to the server.
- Files stay canonical: the server is an index + query + write-gate
  over the repos' `.ai/` files, rebuilt from them on demand. The
  operator's private memory directories are refused as roots and
  never indexed; secret-shaped files are excluded from the index.
- Dual-era protocol posture: legacy `initialize` negotiation plus the
  modern `server/discover` method.
- `bloch task export --format opst`: read-only export of attempts as
  OPST trajectory material (task descriptor, attempts, verified gate
  outcome, confidence trajectory, time-to-green).
- Scope/subject on every attempt, lesson, and attempt-fix record:
  `scope` (`repo_task` | `agent_task` | `action`) +
  `subject` (roadmap task ID, dispatch/task ID, or action name +
  target), so the failure → research → lesson machinery operates
  identically across scopes. Library filing API `file_scoped_event`
  enforces the action volume rule (failures + their resolving success
  only) and the agent-scope research trigger (signature fails ≥2×, or
  once when blocking a user-facing task). Existing records default to
  `repo_task` with subject = task ID.

## [0.2.0] - 2026-10-27

### Added
- `bloch init [--kind mcp|game|app|education|tutorial]`: scaffolds
  `.ai/memory/` + `.ai/skills/` and writes `.ai/roadmap.md` — a
  v0.1 → v1.0 task list specialised per repo kind (common spine plus
  kind-specific phases), seeded from kind detection that reports its
  evidence. Config/dotfiles repos are REFUSED a roadmap (memory-only
  init); ambiguous detection yields `unclassified` with
  classification as the first task. Init never overwrites existing
  files.
- Roadmap format: stable `R-<phase><seq>` task IDs (never renumbered;
  dropped tasks are struck through), per-task `verify:` commands, and an
  `evidence:` line. Parse/render is bounded and round-trip safe;
  unknown lines are preserved verbatim; duplicate IDs are refused.
- `bloch task start|done|fail|status`: the gated green path. `done`
  runs every `verify:` command (600 s timeout each, output tail
  capped) and flips a task to `[x]` only when all exit 0 — there is no
  override, and a task with no verify lines can never go green,
  including hand-edited checkboxes. Attempts are appended to
  `.ai/memory/attempts.jsonl` (alphabetical keys) with agent, duration
  (null when there was no start — never fabricated), self-rated
  confidence, and error/evidence.
- Learning loop: a failure auto-files an `attempt-fail-<id>` memory
  inbox entry; a task that goes green after failures auto-files an
  `attempt-fix-<id>` entry (errors in order, the fixing note, the
  confidence trajectory) for promotion into `.ai/memory/lessons.md`
  (new `memory promote` target). `bloch task lessons [--export]`
  prints the distilled lessons.
- Lane labels from birth: every attempt event, lesson, and
  attempt-fix record carries `origin_lane` (from the repo's lane
  stamp in the policy configuration), `content_label` (sfw|nsfw), and
  label evidence. Defaults fail closed: restricted-lane origin
  without positive SFW evidence is nsfw; general-lane origin defaults
  sfw. No relabeling at transfer time.
- Failure → research loop: fail events carry a failure
  signature (`sig_version` + command + error class + normalised first
  error line; volatile numbers normalised away). A failure implicating
  something external (dependency, upstream, protocol, hardware) files
  a deduplicated research candidate in the inbox —
  one open candidate per signature, repeats increment a recurrence
  count derived from the attempts log — after a curated-knowledge
  check against lessons/patterns. Internal failures stay lessons-only.
  Curated records carry `last_verified` + `applies_to` noise-guard
  fields and a `disposition` slot (extract / reverse-engineer / adapt
  / clean-build; unset by default, never fabricated). Candidates are
  marked resolved when their task goes green.
- HANDOFF output gains a ROADMAP section (% green, current phase,
  next open task, live attempt with elapsed time, recent lessons);
  `--update-memory` refreshes roadmap counts into `bloch-summary.md`
  and snapshots roadmap state; `--diff` reports roadmap movement
  (tasks turned green, new tasks, new failed attempts).

### Added
- `bloch memory read`: bounded session-start read of INDEX.md + current.md
- `bloch memory inbox`: timestamped inbox entries (`<epoch>-<agent>-<topic>.md`)
- `bloch memory promote`: inbox -> curated file (decisions/patterns/current) + archive
- `bloch todo`: add/list/done/drop on `.ai/todo.md`
- `bloch git status`: branch + ahead/behind + session state for agents
- `bloch todo sync`: scan repo for TODO/FIXME/HACK/XXX/BUG markers, add new
  ones as todos, mark resolved ones done (manual todos untouched)
- `bloch git wip`: per-file diff stats for uncommitted changes
- Initial release as `bloch` (renamed from `repomap`)
- Ranked, budgeted codebase maps for coding agents
- Probability normalization: `p(d) = max(score(d), 0) / Σ max(score(d′), 0)`
  (ordinary simplex normalization, not Bloch-vector normalization)
- ASCII Bloch sphere visualization (`--sphere`, `--sphere-points`)
- Persistent agent memory (`--update-memory`, `--diff`, `bloch memory` subcommands)
  - Atomic writes (temp file + rename, no corrupt cache on crash)
  - Path traversal protection
  - Typed errors for corrupt cache (`BlochError::JsonError`)
- Skill-aware ranking (`--skill`, `--no-skill-boost`)
- Tree-sitter extractors for Rust, Lua, Python, Nix, Markdown, Bash,
  TypeScript, TSX, and Kotlin
- `--scip` flag: opt-in SCIP index reading for precise definitions and references
  - Reads `.scip/index.scip` or `index.scip`
  - Falls back to tree-sitter when no index exists, index is stale,
    or parsing fails
  - Staleness detection via file mtime comparison
- Public library API: `map_repo`, `MapConfig`, `RepoMap`
- `index.scip` and `.scip/` added to `.gitignore`

### Changed
- Renamed from `repomap` to `bloch` throughout (binary, output headers, file names)
- `#[command(name = "bloch")]` with working `--version`
- Swapped abandoned `tree-sitter-markdown` for maintained `tree-sitter-md`
- Swapped `tree-sitter-kotlin` (0.3, incompatible) for `tree-sitter-kotlin-ng`
  (1.1, compatible with tree-sitter 0.25+)
- Skill language detection now covers `tsx` and `markdown`
- `MapConfig` simplified to only `use_scip` (budget/skills are CLI concerns)
- CLI now uses public `map_repo` API instead of duplicating logic

### Fixed
- SCIP file count: was reporting `# 0 files` when using index
- Kotlin extractor enabled via `tree-sitter-kotlin-ng`
- Eliminated all production `unwrap()` calls
- Sphere visualization now uses independent axes (recency + visibility)
  instead of circular centrality derived from score
