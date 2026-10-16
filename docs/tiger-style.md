<p class="kicker">Bloch · Engineering</p>

# Tiger Style in bloch

bloch's reader is a machine. A coding agent takes the map bloch emits as
its picture of a codebase and acts on it, so bloch's worst failure mode is
specific: a map that is confidently wrong, silently incomplete, or too
large to use. The choices in this chapter exist to make that failure hard.

They follow **Tiger Style**, the engineering discipline published by
TigerBeetle, which orders its goals explicitly — safety first, performance
second, developer experience third — and behind TigerStyle stands NASA
JPL's **Power of Ten**, Gerard Holzmann's 2006 rules for safety-critical
code. bloch is not flight software, and this chapter does not pretend
otherwise; it borrows the rules that transfer, says where each one lives
in the source, and links the evidence behind it. Where a rule does not
transfer, or a measurement has not been taken yet, the chapter says that
too.

<details>
<summary>1 · The order of goals is the first decision</summary>

TigerStyle's goals are not a list; they are an ordering. When safety and
performance disagree, safety wins, and the argument is over before it
starts. bloch spends its complexity budget accordingly — on not
misleading its reader. Extraction failures surface as named errors or
labeled omissions, budgets are hard limits rather than targets, and
hostile input (a symlink loop, a truncated index, a path trying to escape
the repo) is treated as a design case, not an edge case.

The lineage is deliberate. TigerBeetle wrote their style down in
[TIGER_STYLE.md](https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md)
and credit the Power of Ten behind it; Holzmann's rules were written for
code that cannot be patched after launch
([*The Power of 10*, IEEE Computer, 2006](https://doi.org/10.1109/MC.2006.212)).
A codebase map is not a spacecraft, but an agent that trusts a bad map
fails the same way: quietly, downstream, and at scale.

</details>

<details>
<summary>2 · Errors are values, and there is exactly one error type</summary>

Every fallible operation in bloch returns `Result<T>` — the crate's alias
for `Result<T, BlochError>` — and `BlochError` is a single enum, defined
with `thiserror`, whose variants name the failure precisely:
`ParseFailed` carries the path and the parser's message, `InvalidPath`
says the caller asked for something outside the repo, `ScipParseFailed`
blames the index rather than the code, and `Io`, `GitFailed`, `Regex`,
`LanguageError`, and `JsonError` cover the remaining sources. A caller
can match on the variant and behave differently per failure, which is
the entire point of returning errors instead of printing them.

Panics are reserved for broken internal contracts — conditions that mean
the program itself is wrong, not that the input was. That split is Rust's
own: [Chapter 9 of the Rust Book](https://doc.rust-lang.org/book/ch09-00-error-handling.html)
separates recoverable errors (`Result`) from unrecoverable ones
(`panic!`), and the
[Rust API Guidelines](https://rust-lang.github.io/api-guidelines/documentation.html)
ask that error conditions be documented as part of an item's contract.
It is also the Power of Ten's seventh rule in modern dress: check every
return value, and validate every parameter — in Rust, the type system
checks the return values for you, provided you give it an error type
worth checking.

</details>

<details>
<summary>3 · Everything is bounded, and the bound is in the signature</summary>

bloch never reads an unbounded amount of anything. The bounds are named
constants and parameters, not folklore: files over `MAX_FILE_SIZE`
(1 MiB) are skipped rather than read; memory reads stop at
`MEMORY_READ_BUDGET` (8,000 characters); signatures truncate at `SIG_TRUNC`
(220 characters). The packing functions take the budget as an argument —
`get_memory_context(root, budget)` — so the limit is part of each
function's contract, and the CLI exposes it as `--budget`, defaulting to
12,000 characters for the whole map. When content does not fit, the map
says so in its own output — `### {name} (omitted: over memory budget)` —
because a labeled omission is information and a silent truncation is a
lie of omission.

The bound is not merely tidiness. Liu and colleagues showed that language
models use the beginning and end of a long context far better than its
middle — performance degrades as relevant material moves inward
([*Lost in the Middle*, arXiv:2307.03172](https://arxiv.org/abs/2307.03172)).
An unbounded dump therefore fails twice: it costs tokens, and it buries
the important definitions where they are least likely to be used. A
ranked, budgeted map is the corrective. Holzmann's second rule — every
loop gets a fixed, provable upper bound — is the same instinct applied to
control flow: state the bound, then enforce it.

</details>

<details>
<summary>4 · Ranking is an explicit comparator, not an accident</summary>

A map's order is a claim about importance, so bloch computes it rather
than inheriting it from directory traversal. Definitions are scored by
qualified cross-file references with recency weighting, then sorted by a
comparator written in one place: score descending, and on equal scores,
public definitions before private ones — visibility is a relevance signal
an agent can act on. The supporting lists are equally explicit: markers
come out in (file, line) order, languages in key order.

The evidence that this ordering matters is SWE-bench (Jimenez et al.,
[arXiv:2310.06770](https://arxiv.org/abs/2310.06770), ICLR 2024), which
measures models on resolving real GitHub issues and finds that the work
routinely requires understanding and coordinating changes across multiple
functions, classes, and files. That cross-file context is precisely what
an agent cannot assemble by reading files in alphabetical order — and
precisely what a ranked map front-loads, inside the budget where the
model will actually attend to it.

</details>

<details>
<summary>5 · Contracts are asserted where they are made</summary>

bloch's invariants are checked at the point they are established:
`lib.rs` carries 74 `assert!` invocations, and values that must hold by
construction say so — a regular expression compiled from a hardcoded
literal is bound with `expect` and a message naming the contract
("hardcoded … regex"), so if it ever fails, the failure reads as a bug
report against the code, not a mystery about the input. The Power of
Ten's fifth rule asks safety-critical C for an assertion density of at
least two per function; bloch's proportionate version is that internal
assumptions are never left implicit where a reader — human or agent —
would have to guess them.

</details>

<details>
<summary>6 · No unsafe code, anywhere in the crate</summary>

There is not one `unsafe` block in bloch's sources. That is a deliberate
forfeiture of expressive power in exchange for a guarantee someone else
has already proven: RustBelt (Jung, Jourdan, Krebbers, and Dreyer,
[POPL 2018](https://doi.org/10.1145/3158154)) gave the first formal,
machine-checked safety proof for a realistic subset of Rust, and showed
what a library must verify to extend that proof when it does use
`unsafe` internally. bloch needs no such extension argument. Its memory
safety is inherited from the proven fragment of the language by staying
entirely inside it — the cheapest correctness argument is the one you
never have to make.

</details>

<details>
<summary>7 · Documentation lives in doc comments — nowhere else</summary>

As of this writing, bloch's sources carry 185 lines of doc comments
(`//!` at the crate root, `///` on items) and no plain comments at all.
The doc comments are the documentation: they are what `cargo doc`
renders, they sit on the items they describe, and they follow the Rust
API Guidelines' convention of `///` for items and `//!` for modules and
crates. A plain comment, by contrast, is a note to whoever happens to
read that file, invisible to the rendered documentation and free to rot
where it stands. Anything worth saying about the code is worth saying
where the documentation is built from — so that is the only place it is
said.

</details>

<details>
<summary>8 · Half the test suite is adversarial</summary>

bloch has 42 tests, and roughly half of them feed the code the inputs a
hostile — or merely messy — repository actually contains:
`memory_rejects_path_traversal` refuses paths that escape the repo;
`map_repo_handles_symlink_loop` walks a tree designed to loop forever;
`extract_handles_malformed_utf8`, `extract_handles_nul_bytes`,
`extract_handles_binary_file`, and `extract_handles_empty_file` push
broken bytes through the extractors;
`read_scip_index_handles_malformed_protobuf`,
`read_scip_index_handles_truncated_file`, and
`read_scip_index_handles_empty_file` do the same to the index reader;
`skill_boost_handles_hostile_names` treats names as attack surface; and
the memory tests — `read_memory_cache_distinguishes_missing_vs_corrupt`,
`memory_atomic_write_no_corrupt`, `memory_inbox_sanitizes_names` —
protect the handoff, because a corrupted memory file is a corrupted next
session. The other half validates the happy paths those attacks must not
disturb. A test suite that only demonstrates the software working is a
demo; this one is also an argument that it fails safely.

</details>

<details>
<summary>9 · What this chapter does not claim</summary>

bloch has no benchmark suite yet — there is no `benches/` directory, and
this book publishes no latency or throughput numbers, because none have
been measured. What can be stated today is the gate record: 42 of 42
tests passing, `cargo fmt --check` clean, `cargo doc` building, and a
successful `cargo publish --dry-run` (4 October 2026); clippy reports a
single pre-existing warning, an unused import in the test module.
Performance is Tiger Style's second goal, ahead of developer experience
— and it will be served with measurements or not at all. When benchmarks
exist, this chapter will cite them the way it cites everything else: by
name, with numbers that can be re-run.

</details>

## Sources

- TigerBeetle — [TIGER_STYLE.md](https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md)
- G. J. Holzmann — [*The Power of 10: Rules for Developing Safety-Critical Code*](https://doi.org/10.1109/MC.2006.212), IEEE Computer 39(6), 2006
- R. Jung, J.-H. Jourdan, R. Krebbers, D. Dreyer — [*RustBelt: Securing the Foundations of the Rust Programming Language*](https://doi.org/10.1145/3158154), POPL 2018
- N. F. Liu et al. — [*Lost in the Middle: How Language Models Use Long Contexts*](https://arxiv.org/abs/2307.03172), arXiv:2307.03172 (TACL 2024)
- C. E. Jimenez et al. — [*SWE-bench: Can Language Models Resolve Real-World GitHub Issues?*](https://arxiv.org/abs/2310.06770), arXiv:2310.06770 (ICLR 2024)
- The Rust Programming Language — [Chapter 9: Error Handling](https://doc.rust-lang.org/book/ch09-00-error-handling.html)
- Rust API Guidelines — [Documentation](https://rust-lang.github.io/api-guidelines/documentation.html)
