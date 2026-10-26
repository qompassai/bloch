# Changelog

All notable changes to bloch will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- Published to crates.io as package `blochs` (the name `bloch` was taken in Oct 2026); the library and binary remain named `bloch`.

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
