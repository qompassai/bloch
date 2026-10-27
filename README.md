# bloch

Ranked, budgeted codebase maps for coding agents — with session handoff fidelity.
<a href="./LICENSE"><img src="https://img.shields.io/badge/License-Apache%202.0-blue.svg" alt="License: Apache 2.0"></a>

Named for the **Bloch sphere**: the geometric map of a qubit's quantum state.
Just as the Bloch sphere maps the two-dimensional Hilbert space (C²) of
single-qubit states onto a 3D unit sphere, `bloch` maps the sprawling state
of a codebase onto a compact, ranked representation. See [QUANTUM.md](QUANTUM.md)
for the full physics-to-code analogy with equations, diagrams, and published
research supporting this approach.

**Note:** `bloch` is a classical tool. It does not perform quantum computation
or simulate qubits. The Bloch sphere is a metaphor for dimensional reduction,
not a physical model. See QUANTUM.md for the explicit disclaimer.

## What it does

`bloch /path/to/repo` walks a repo's text files, extracts structure, ranks
definitions by qualified cross-file references with recency weighting, and
emits a budgeted map. Agents read the map instead of the whole tree.

**Handoff fidelity:** The output leads with a HANDOFF section containing:
- Git session state (branch, dirty files, recent commits, stashes)
- `.ai/memory/` contents (`handoff.md` first, then patterns/decisions)
- Cached test/lint status

So any AI model can pick up exactly where the last one left off.

## Language support

| Language | Method |
|----------|--------|
| Rust | Full tree-sitter extraction + qualified-reference ranking |
| Python | Tree-sitter: functions, classes, methods |
| Lua | Tree-sitter: functions |
| TypeScript / TSX | Tree-sitter: functions, classes, interfaces |
| Kotlin | Tree-sitter (`tree-sitter-kotlin-ng`): functions, classes, objects |
| Bash / Shell | Tree-sitter: functions |
| Nix | Tree-sitter: top-level attributes |
| Markdown | Tree-sitter: headers |
| Ruby | Regex: lanes, platforms, methods |
| TOML, YAML, JSON | Regex: tables, keys |
| Other text | File metadata fallback |

## SCIP integration

`bloch` can read [SCIP](https://github.com/sourcegraph/scip) indexes for
precise, compiler-accurate definitions and references instead of heuristic
tree-sitter extraction.

```sh
# Generate an index (Rust example via rust-analyzer)
rust-analyzer scip . --output index.scip

# Use it
bloch /path/to/repo --scip
```

Behavior:
- Reads `.scip/index.scip` or `index.scip` (in that order)
- Falls back to tree-sitter when no index exists, the index is stale
  (source files newer than index, detected via mtime), or parsing fails
- Reports `bloch: using SCIP index (N files)` on stderr when active

SCIP provides fully-qualified symbol names and cross-file references that
tree-sitter cannot infer. For Rust, `rust-analyzer scip` is the recommended
indexer. Other languages need their own SCIP indexers.

## Persistent memory

`bloch` maintains agent session memory in `.ai/memory/`:

```sh
# Write current map to .ai/memory/bloch-last.json and summary to .ai/memory/bloch-summary.md
bloch /path/to/repo --update-memory

# Show diff between cached map and current state
bloch /path/to/repo --diff

# Manage memory files
bloch memory list                    # list .ai/memory/ files with sizes
bloch memory show <file>             # print a memory file
```

Memory files:
- `bloch-last.json` — machine-readable cached map (written atomically)
- `bloch-summary.md` — human-readable summary
- `handoff.md` — manual session notes (surfaced first in output)

The HANDOFF section of `bloch` output includes `.ai/memory/` contents so
the next agent session starts with full context.

## Roadmap (v0.2): the longitudinal v1 tracker

`bloch init` adds a repo-kind-specialised roadmap that takes a repo from
its v0.1 baseline to a fully functional v1 — any agent can pick it up
and work it, and bloch itself keeps the tracker honest:

```sh
bloch init                       # detect kind (evidence reported), scaffold .ai/roadmap.md
bloch init --kind mcp            # or force the kind: mcp|game|app|education|tutorial
bloch task start R-101 --agent claude --confidence 80
bloch task done  R-101 --note "what I did"   # runs every verify: command; green only if all pass
bloch task fail  R-101 --error "what went wrong"
bloch task status                # % green, current phase, next open, live attempts
bloch task lessons --export      # distilled fixes from fail -> pass sequences
bloch task export --format opst  # attempts as OPST trajectory material (read-only)
```

- **Stable IDs** (`R-101`, `R-102`, …) are never renumbered; dropped
  tasks are struck through, not deleted.
- **Green is gated.** There is no override flag and no self-declared
  done: `task done` executes the task's `verify:` commands (600 s
  timeout each, output tail capped; repo-authored trusted input, the
  Makefile trust class) and flips `[x]` only on all-pass. A task with
  no `verify:` lines can never go green.
- **Attempts are evidence.** `.ai/memory/attempts.jsonl` is append-only:
  agent, timing, self-rated confidence, error/evidence per attempt.
  Failures auto-file memory inbox entries; a fail → pass sequence
  auto-files an `attempt-fix-<id>` entry for promotion into
  `.ai/memory/lessons.md`, and the HANDOFF section leads with the
  roadmap state and the most recent lessons.
- **Config/dotfiles repos are refused a roadmap** (memory-only init) —
  there is no v1 to track for a dotfiles tree.
- **Lane labels from birth.** Repos are assigned a content lane in
  the policy configuration, and every attempt/lesson record carries
  `origin_lane`, `content_label` (sfw|nsfw), and label evidence.
  Defaults fail closed (content from a restricted-lane origin without
  positive SFW evidence is labeled nsfw), and labels never change at
  transfer time.
- **Failures feed research.** Fail events carry a deduplicated failure
  signature; externally-implicating failures (dependency, upstream,
  protocol, hardware) file a research candidate in the inbox (a
  standard finding record: system, signature, source, status — one
  per signature, recurrence counted), after a check against
  the curated lessons. Curated records carry `last_verified` /
  `applies_to` staleness fields and a `disposition` slot
  (extract / reverse-engineer / adapt / clean-build).

## MCP server (v0.3): shared memory + lesson sharing

`bloch serve` is the shared, multi-repo surface over the same files.
Files stay canonical — the
server is an index, a query surface, and a write gate, never the
store; with the server down, the CLI + files carry the full workflow
and the index rebuilds identically on restart.

```sh
bloch serve --root ~/repos/a --root ~/repos/b \
  --policy policy.toml --ideas ~/register/IDEAS.md --board ~/board/TRACKER.md
```

11 tools, annotations from birth: `memory_search` / `memory_get`
(read), `memory_propose` (inbox-only; requires the repo's current
fencing generation and a live attention lease, verified
server-side from the policy file), `memory_promote` (curator gate;
proposer ≠ promoter; evidence must resolve), `tracker_state` /
`tracker_rollup`, `idea_lookup` / `idea_register` (gated),
`lessons_search` (cross-repo lessons + attempt aggregates),
`roadmap_state` / `roadmap_rollup`. Content lanes are enforced in
server code: nsfw-labeled entries are never served to callers whose
clearance covers only the general lane, and nsfw content cannot be
proposed into a general-lane repo. The operator's own private memory
directories are refused as index roots and never served. Transport is
stdio JSON-RPC, hand-rolled on serde_json (no new dependencies),
with a dual-era posture (legacy `initialize` + `server/discover`).
Access posture: declared access above matches the detected surface —
read tools are read-only, writes are inbox-only or gate-class, no
tool exceeds its annotations; license Apache-2.0.

## Skill-aware ranking

`bloch` detects active skills from `.ai/skills/` and boosts definitions in
matching languages:

```sh
# Auto-detect from .ai/skills/
bloch /path/to/repo

# Manually specify skills
bloch /path/to/repo --skill tiger-style-rust --skill docs

# Disable skill boosting
bloch /path/to/repo --no-skill-boost
```

A skill named `tiger-style-rust` boosts Rust definitions by 1.5×.
Skills are matched to languages by name (e.g., `tiger-style-python`
boosts Python). This helps agents focus on the languages relevant to
their current task.

## Install

```sh
cargo install blochs   # package name is `blochs` (`bloch` was taken); the installed binary is `bloch`
```

## Usage

```sh
# Map a repo to stdout (12,000 char budget)
bloch /path/to/repo

# Custom budget and output file
bloch /path/to/repo --budget 15000 --out .bloch.txt

# Use SCIP index if available
bloch /path/to/repo --scip

# ASCII Bloch sphere visualization of top definitions
bloch /path/to/repo --sphere --sphere-points 20
```

## As a library

```rust
use bloch::{map_repo, MapConfig};
use std::path::Path;

let root = Path::new("/path/to/repo");
let config = MapConfig { use_scip: true };
let repo_map = map_repo(root, &config)?;

println!("{} files, {} definitions", 
    repo_map.files.len(), 
    repo_map.total_definitions);
for (lang, count) in &repo_map.languages {
    println!("  {}: {}", lang, count);
}
# Ok::<(), bloch::BlochError>(())
```

The library also exposes:
- `detect_skills(root)` — find active skills in `.ai/skills/`
- `skill_boost(lang, skills)` — get boost factor for a language
- `read_scip_index(root)` — parse SCIP index directly
- `render_sphere(...)` — ASCII Bloch sphere visualization
- `write_memory_cache` / `read_memory_cache` — persistent memory

## License

This project is licensed under the [Apache License, Version 2.0](./LICENSE).

