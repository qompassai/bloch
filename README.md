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
cargo install bloch
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

