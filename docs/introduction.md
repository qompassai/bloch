<p class="kicker">Bloch · Documentation</p>

# bloch

bloch builds **ranked, budgeted codebase maps for coding agents**, with
session handoff fidelity. Point it at a repository and it emits a compact
map — the definitions that matter, in the order they matter, inside a
character budget the caller sets — so an agent can start work from the map
instead of reading the whole tree.

The map leads with a HANDOFF section: the git session state (branch, dirty
files, recent commits, stashes), the contents of the repo's `.ai/memory/`
(`handoff.md` first, then patterns and decisions), and cached test and lint
status. Any model can pick up exactly where the last one left off.

<details>
<summary>How a map is built</summary>

1. **Walk.** bloch traverses the repo's text files, skipping known binary
   and vendored directories (`SKIP_DIRS`) and extensions (`SKIP_EXTS`), and
   refusing files over `MAX_FILE_SIZE` (1 MiB) rather than reading them.
2. **Extract.** Tree-sitter parsers pull definitions out of each file —
   functions, classes, methods, interfaces, attributes, headers — per
   language, below.
3. **Index.** When a SCIP index is present, bloch reads it for qualified
   cross-file references, and checks its staleness against the sources.
4. **Mark.** The walk collects the markers a human would want flagged —
   `BUG`, `FIXME`, `HACK`, `TODO`, `XXX` — and emits them in (file, line)
   order.
5. **Rank.** Definitions are scored by qualified cross-file references with
   recency weighting, and ordered by an explicit comparator: score
   descending, public definitions first on ties.
6. **Pack.** Memory context, test status, and the ranked definitions are
   fitted into the caller's character budget — the CLI's `--budget` flag,
   12,000 characters by default. Anything that does not fit is omitted
   *and labeled as omitted*, never silently dropped.

</details>

<details>
<summary>Language support</summary>

| Language | Method |
| --- | --- |
| Rust | Full tree-sitter extraction + qualified-reference ranking |
| Python | Tree-sitter: functions, classes, methods |
| Lua | Tree-sitter: functions |
| TypeScript / TSX | Tree-sitter: functions, classes, interfaces |
| Kotlin | Tree-sitter (`tree-sitter-kotlin-ng`): functions, classes, objects |
| Bash / Shell | Tree-sitter: functions |
| Nix | Tree-sitter: top-level attributes |
| Markdown | Tree-sitter: headers |

</details>

<details>
<summary>The name</summary>

bloch is named for the **Bloch sphere**, Felix Bloch's geometric map of a
qubit's state: a two-dimensional complex state space drawn on a unit sphere
you can hold in your head. bloch the tool does the same reduction for a
codebase — the sprawling state of a repository, mapped onto a compact,
ranked representation. The analogy, its equations, and its limits are
written up in the repo's `QUANTUM.md`. bloch is a classical tool; it
performs no quantum computation. The sphere is a metaphor for dimensional
reduction, and an honest one: a map is useful because of what it leaves
out, provided it says what it left out.

</details>

## The engineering chapter

The [Tiger Style](tiger-style.md) chapter explains the choices behind the
code above — why errors are values in a single enum, why every input is
bounded and the bound sits in the function signature, why half the test
suite is adversarial — and links the published evidence each choice rests
on.
