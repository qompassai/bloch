---
name: bloch
description: Generate ranked, budgeted codebase maps for AI agents using the bloch CLI — structure extraction, cross-file reference ranking, session handoff memory, and skill-aware boosting.
---

# bloch

`bloch` maps a codebase onto a compact, ranked representation that an agent can
read instead of the whole tree. Named for the Bloch sphere: just as the sphere
maps a qubit's state onto a 3D surface, `bloch` maps a repo's sprawling state
onto a budgeted map. It is a classical tool — no quantum computation involved.

## When to use

- Starting work in an unfamiliar repository: get the lay of the land first.
- Handing off between agent sessions: `--update-memory` + HANDOFF section.
- Before planning a multi-file change: see what references what.

## Quick start

```sh
# Map a repo to stdout (12,000 char budget)
bloch /path/to/repo

# Custom budget, write to file
bloch /path/to/repo --budget 15000 --out .bloch.txt

# ASCII Bloch sphere visualization of top definitions
bloch /path/to/repo --sphere --sphere-points 20
```

## Session memory (handoff fidelity)

```sh
# Snapshot current map into .ai/memory/
bloch /path/to/repo --update-memory
# → .ai/memory/bloch-last.json (machine-readable, atomic write)
# → .ai/memory/bloch-summary.md (human-readable)

# Diff working tree against the snapshot
bloch /path/to/repo --diff

# Inspect memory directly
bloch memory list
bloch memory show <file>
```

The HANDOFF section at the top of `bloch` output includes git state (branch,
dirty files, recent commits, stashes) plus `.ai/memory/` contents — so the
next session starts with full context.

## Skill-aware ranking

`bloch` auto-detects skills in `.ai/skills/` and boosts definitions in
matching languages (e.g. `tiger-style-rust` boosts Rust 1.5×):

```sh
bloch /path/to/repo                                # auto-detect
bloch /path/to/repo --skill tiger-style-rust       # manual
bloch /path/to/repo --no-skill-boost               # disable
```

## SCIP indexes (precise references)

```sh
rust-analyzer scip . --output index.scip   # generate (Rust)
bloch /path/to/repo --scip                 # use; falls back to tree-sitter
```

Reads `.scip/index.scip` or `index.scip`; falls back to tree-sitter when the
index is missing, stale (mtime check), or unparseable.

## Language coverage

Full tree-sitter extraction + qualified-reference ranking for Rust; function/
class extraction for Python, Lua, TypeScript/TSX, Kotlin, Bash, Nix; headers
for Markdown; regex fallback for Ruby, TOML, YAML, JSON.

## As a library

```rust
use bloch::{map_repo, MapConfig};
let repo_map = map_repo(Path::new("/path/to/repo"), &MapConfig { use_scip: true })?;
```

Also exposes `detect_skills`, `skill_boost`, `read_scip_index`,
`render_sphere`, `write_memory_cache` / `read_memory_cache`.
