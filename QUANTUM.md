# Bloch Sphere: The Quantum-Inspired Principle Behind `bloch`

## The Physics

A **qubit** is the quantum analogue of a classical bit. Where a classical bit
is either 0 or 1, a qubit exists in a **superposition** of both states
simultaneously. The general single-qubit state is:

```
|ψ⟩ = α|0⟩ + β|1⟩
```

where α and β are **complex probability amplitudes** satisfying the
normalization condition:

```
|α|² + |β|² = 1
```

The state lives in **C²**, a two-dimensional complex Hilbert space. This is
an important distinction: C² is the mathematical space of all (α, β) pairs,
a four-real-dimensional space. But physical states are equivalence classes
under normalization (|α|² + |β|² = 1, reducing to the 3-sphere S³) and global
phase (α, β) ~ e^(iγ)(α, β), reducing further to the 2-sphere S². The Bloch
sphere represents this physical state space — the continuum of distinct
quantum states — not the full Hilbert space C².

### The Bloch Sphere Representation

Felix Bloch (1905–1983) showed that every single-qubit pure state can be
mapped to a unique point on the surface of a **unit sphere** in
three-dimensional real space. The mapping is:

```
|ψ⟩ = cos(θ/2)|0⟩ + e^(iφ)·sin(θ/2)|1⟩
```

where:
- **θ** ∈ [0, π] is the polar angle from the +z axis
- **φ** ∈ [0, 2π) is the azimuthal angle in the xy-plane

The corresponding **Bloch vector** is:

```
r = (sin θ·cos φ,  sin θ·sin φ,  cos θ)
```

This is a unit vector (|r| = 1) pointing to a specific location on the sphere.

### Diagram: The Bloch Sphere

```
                        |0⟩ (ground state)
                         +z
                          │
                          │   r
                          │  ╱
                          │ ╱  θ = polar angle
                          │╱
              ────────────●──────────── +x
                         ╱│
                        ╱ │
                       ╱  │ φ = azimuthal
                      ╱   │     angle
                     ╱    │
                    +y    │
                          │
                         -z
                        |1⟩ (excited state)
```

**Key points on the sphere:**

| State | θ | φ | Bloch vector | Meaning |
|-------|---|---|--------------|---------|
| \|0⟩ | 0 | — | (0, 0, +1) | North pole: definite 0 |
| \|1⟩ | π | — | (0, 0, −1) | South pole: definite 1 |
| \|+⟩ = (\|0⟩+\|1⟩)/√2 | π/2 | 0 | (+1, 0, 0) | +x: equal superposition |
| \|−⟩ = (\|0⟩−\|1⟩)/√2 | π/2 | π | (−1, 0, 0) | −x: equal superposition, π phase |
| \|+i⟩ = (\|0⟩+i\|1⟩)/√2 | π/2 | π/2 | (0, +1, 0) | +y: equal superposition, π/2 phase |
| \|−i⟩ = (\|0⟩−i\|1⟩)/√2 | π/2 | 3π/2 | (0, −1, 0) | −y: equal superposition, 3π/2 phase |

**Measurement probabilities:** If you measure a qubit in state |ψ⟩ in the
computational basis, the probability of obtaining 0 is:

```
P(0) = |⟨0|ψ⟩|² = cos²(θ/2) = (1 + r_z) / 2
```

and P(1) = 1 − P(0) = sin²(θ/2) = (1 − r_z) / 2.

The closer the Bloch vector points to the north pole (+z), the more likely
you are to measure 0. The z-component of the Bloch vector directly encodes
the measurement bias.

### Why This Matters: Dimensional Reduction

The profound insight of the Bloch sphere is **dimensional reduction without
information loss** (for pure states):

- **Before:** A qubit state requires 2 complex numbers (α, β) — 4 real
  parameters, living in an abstract Hilbert space that humans cannot
  visualize.
- **After:** The same physical state is a single point (θ, φ) on a 3D
  sphere — 2 real parameters that you can point to, draw, and reason about
  geometrically.

**Quantum gates become rotations.** A Pauli-X gate (quantum NOT) is a 180°
rotation about the x-axis. A Hadamard gate is a 180° rotation about the
axis (x+z)/√2. Instead of matrix multiplication in complex vector spaces,
you can *see* what a quantum circuit does as a path traced on the sphere.

This is not an approximation or a simplification — for pure single-qubit
states, the Bloch sphere is a **faithful, invertible mapping**. Every point
on the sphere corresponds to exactly one physical quantum state (up to
global phase, which has no observable meaning).

---

## The Analogy: How `bloch` (the tool) Uses This Principle

### Important Disclaimer

`bloch` is a **classical** tool. It does not perform quantum computation,
simulate qubits, or claim any quantum advantage. The "Bloch sphere"
visualization is a **metaphor** for dimensional reduction, not a physical
model. Specifically:

- The probability `p(d)` assigned to each definition is computed by
  ordinary **probability-simplex normalization**:

  ```
  p(d) = max(score(d), 0) / Σ max(score(d′), 0)
  ```

  This ensures Σ p(d) = 1 and p(d) ≥ 0. It is **not** Bloch-vector
  normalization (|r| = 1) and does not imply any quantum-mechanical
  interpretation of the scores.

- The mapping from probability to sphere position (θ = π(1−p)) is an
  **arbitrary visualization choice**, not derived from quantum mechanics.
  High-probability definitions appear near the north pole (|0⟩); low-
  probability definitions near the south pole (|1⟩). The azimuthal angle φ
  encodes independent diagnostic dimensions (recency, reference count),
  not quantum phase.

### The Problem

A codebase, like a qubit's Hilbert space, has far more "state" than a human
(or AI agent) can hold at once:

- Thousands of files, each with dozens of definitions
- Cross-references forming a dense dependency graph
- Git history encoding *why* things are the way they are
- Uncommitted changes representing work in progress
- Test results indicating what's working and what's broken

An AI agent starting a session faces the equivalent of an unknown quantum
state — a vast, high-dimensional space of possibilities with no obvious
starting point.

### The Bloch Mapping

`bloch` performs the same kind of dimensional reduction as the Bloch sphere:

```
Codebase (high-dimensional)          Bloch map (compact, ranked)
─────────────────────────            ──────────────────────────

  All files                    →       Top-ranked definitions
  All definitions              →       Within character budget
  All cross-references         →       Qualified-reference scores
  Git history                  →       Recency-weighted ranking
  Uncommitted changes          →       Session state header
  Project memory               →       .ai/memory/ surfacing
  Test results                 →       Cached status block
```

### The Equations

**Definition scoring** (analogous to measurement probability):

Just as P(0) = (1 + r_z)/2 extracts a single meaningful number from the
full quantum state, bloch's scoring function extracts a single relevance
score from the full cross-reference graph:

```
score(d) = Σ qualified_refs(d) / document_frequency(d)  ×  recency_boost(d)
```

where:
- `qualified_refs(d)` counts only *qualified* references (`T::method`,
  `self.method`, type-position uses) — analogous to measuring in a
  specific basis
- `document_frequency(d)` penalizes common names (like IDF in information
  retrieval)
- `recency_boost(d)` weights recently-modified files higher:

```
recency_boost(d) = 1 + (max_boost − 1) × max(0, (30 − age_days) / 30)
```

This is a linear decay from `max_boost` (default 2.0) at age 0 to 1.0 at
30 days.

**Probability normalization** (simplex, not Bloch):

```
p(d) = max(score(d), 0) / Σ max(score(d′), 0)
```

This is ordinary probability normalization ensuring the values sum to 1.
It is **not** related to the Bloch vector normalization (|r| = 1).

**Budget constraint** (analogous to the unit sphere):

Just as the Bloch vector is constrained to |r| = 1 (unit sphere), bloch's
output is constrained to a character budget:

```
Σ len(line_i) ≤ budget
```

Definitions are emitted in score order until the budget is exhausted. This
isn't truncation — it's a principled projection onto the most informative
subspace, like choosing the optimal measurement basis.

### Handoff Fidelity as State Tomography

In quantum physics, **quantum state tomography** is the process of
reconstructing an unknown quantum state through a series of measurements.
You can't observe the full state directly, so you measure in different
bases and reconstruct the density matrix.

`bloch`'s handoff section performs the analogous function for AI agent
sessions:

| Tomography | Bloch handoff |
|------------|---------------|
| Measure in X basis | `git status` — what's dirty? |
| Measure in Y basis | `git log` — what was recently done? |
| Measure in Z basis | `.ai/memory/` — what was decided? |
| Reconstruct density matrix | Assemble handoff header |
| Next experimenter starts informed | Next agent starts informed |

The goal is identical: **enable the next observer to pick up exactly where
the last one left off, without re-measuring from scratch.**

---

## References

### Quantum Foundations

- Bloch, F. (1946). "Nuclear induction." *Physical Review*, 70(7-8), 460.
- Nielsen, M. A., & Chuang, I. L. (2010). *Quantum Computation and Quantum
  Information*. Cambridge University Press. (Chapter 1: Bloch sphere;
  Chapter 8: Quantum state tomography.)
- The Bloch sphere is a special case of the **Hopf fibration** mapping
  S³ → S², connecting the 3-sphere of normalized two-component complex
  vectors to the 2-sphere of physical states.

### Code Summarization for AI Agents

The following published works establish the value of compact, structured
codebase representations for AI coding agents — the problem `bloch`
addresses:

- Tawosi, V., Alamir, S., Liu, X., & Veloso, M. (2025). "Meta-RAG on Large
  Codebases Using Code Summarization." *arXiv:2508.02611*. Demonstrates that
  condensing codebases by 79.8% into structured natural-language summaries
  enables state-of-the-art bug localization (84.67% file-level accuracy on
  SWE-bench Lite). Validates the core premise: agents perform better with
  compact, ranked representations than with raw code.

- Tang, Y., Sarikayak, E., Tuncel, D., Zhang, J. M., & Runkler, T. (2026).
  "Agent4cs: A Multi-agent System for Code Summarization in Large
  Hierarchical Codebases." *arXiv:2607.01425*. Shows that hierarchical,
  bottom-up summarization with cross-folder keyword extraction improves
  semantic consistency by 8% over flat baselines. Supports `bloch`'s
  approach of preserving structural relationships (module paths, qualified
  references) rather than treating code as flat text.

### Information Retrieval Foundations

`bloch`'s scoring uses TF-IDF-inspired document frequency penalization,
a well-established technique in information retrieval:

- Salton, G., & McGill, M. J. (1983). *Introduction to Modern Information
  Retrieval*. McGraw-Hill. Foundational text on TF-IDF term weighting,
  which `bloch` adapts as document-frequency penalization for definition
  names (common names like `new` or `init` are downweighted).

- The code search literature (e.g., "Opportunities and Challenges in Code
  Search Tools," *arXiv:2011.02297*) documents TF-IDF as a standard baseline
  for ranking code by relevance, with Sourcerer as a canonical example of
  TF-IDF over source code corpora.
