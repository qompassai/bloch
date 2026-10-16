# Bloch landing — 2026-10-15

Landed as commit 59561db on main (pushed, ls-remote verified
59561db22060c7bd861a14342c93eb6901dea1e7), on Matt's word.

Contents of the single commit:
- Matt's in-flight feature work (src/lib.rs, src/main.rs, CHANGELOG.md),
  snapshotted before editing at ~/wip-snapshots/bloch-20261015-prestrip/.
- Comment strip: 247 plain comments removed (224 lib.rs, 23 main.rs);
  all 185 doc-comment lines preserved byte-identical. Documentation
  lives in doc comments only.
- mdBook docs: book.toml + docs/ (qompass theme, DaddyTime Mono Nerd
  Font) with a Tiger Style chapter grounded in this codebase;
  book/ output gitignored.

Base note: origin/main had moved past the local checkout (two
license-normalization commits, LICENSE + README only); the landing
commit was rebased onto b5cffdf cleanly before pushing.

Gates on the committed tree: cargo fmt --check clean;
cargo test 42 passed / 0 failed (pre-strip baseline).

Open nit (not in this commit, Matt's call): the //! crate doc in
src/lib.rs still says "shallow regex for other languages" — stale;
the other seven languages use tree-sitter extractors.
