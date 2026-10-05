# AGENTS.md — bloch

Agent instructions for working in this repository. Human documentation lives in
[README.md](README.md); the physics analogy lives in [QUANTUM.md](QUANTUM.md).

## What this is

`bloch` is a Rust CLI + library that produces ranked, budgeted codebase maps
for coding agents, with session handoff fidelity. Binary: `bloch`. Library:
`bloch::{map_repo, MapConfig}`.

## Build, test, lint

```sh
cargo build          # debug build
cargo test           # 32 tests, must all pass
cargo clippy -- -D warnings   # zero warnings before commit
cargo fmt --check    # must be clean before commit
```

## Conventions

- **Tiger Style** (see `~/workspace/skills/tiger-style-rust/SKILL.md` on the
  dev machine): safety first, explicit contracts, bounded work, no hidden
  behavior. Every public function documents its contract.
- Quantum connection stays **conceptual** — no quantum/PQC dependencies.
  The Bloch sphere is a framework for thinking about dimensional reduction
  (see QUANTUM.md). Do not add qsim, Kyber, or similar.
- Binary stays lean: no new dependencies without a demonstrated need.
- `CHANGELOG.md` follows Keep a Changelog; update it with every user-facing
  change.

## Testing

- Tests live alongside the code (`#[cfg(test)]` modules).
- Maintain the ~50/50 split: half validation (happy path works), half
  adversarial (garbage input, oversized files, corrupt caches, path
  traversal attempts).
- `cargo test` must pass before any commit.

## What not to do

- Do not add dependencies for functionality the standard library covers.
- Do not change the `.ai/memory/` file formats without updating
  `write_memory_cache` / `read_memory_cache` and their tests together.
- Do not weaken the path traversal protection in memory commands.
