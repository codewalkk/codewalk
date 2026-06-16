● Both docs are in place and every cross-reference + TS-spec path resolves. Here's what I created in codewalk_rust/docs/.

  rust-build-plan.md — how/when to build it

  - Why Rust: the FTS5 cliff (solved by rusqlite with bundled,fts5 — static SQLite, no Node-version dependency), single-binary distribution, perf, and TS bindings — tied back to
  ../codewalk_kb/docs/launch-plan.md.
  - The two modes baked into the crate design: codegraph binary (structural only) and codewalk binary (codegraph + LLM learned intelligence).
  - Cargo workspace layout (codegraph-core, codegraph-cli, codewalk-kb, codewalk-cli, codegraph-node).
  - Dependency map (TS → Rust): tree-sitter native, rusqlite, fastembed (MiniLM-L6-v2, 384-d), rmcp, clap, ignore, notify, rayon, napi-rs, claude -p for distillation.
  - Reference map: a per-module table pointing each Rust module at its exact TS spec file (codegraph/src/... and codewalk_kb/src/...), so the next session reads the spec before porting.
  - Validation/parity methodology: gate every module against the TS index (8,898 files · 167k symbols · 592k edges) and the existing bench/k8s/queryset.json recall (0.32→0.87).
  - Phased milestones M0–M6 (toolchain → Go extraction → resolution/graph → MCP mode 1 → more languages → learned layer mode 2 → napi bindings + distribution), with the honest note that full
  codegraph parity is a long tail and M0–M3 (Go-only, FTS5-clean, benchmarked) is the near-term goal.
  - First-session checklist including rustup (Rust isn't installed yet) and the FTS5 proof test.

  codewalk_rust_arch.md — what it is

  - The two-layer / two-mode / TS-binding picture, crate dependency rules (codegraph-core knows nothing about embeddings/LLMs; codewalk-kb depends on core, never the reverse).
  - Sketched Rust traits/types: NodeKind/EdgeKind enums, the LanguageExtractor trait (the ~40-hook port), FrameworkResolver trait, the 20 synthesizer channels with provenance=Heuristic.
  - Storage model (two rusqlite DBs, FTS5 baked in), the codewalk-kb layer (fastembed, KB store, RRF fusion, transcript capture), the per-mode MCP surface, and the codegraph-node napi surface
  (incl. the payoff: retrofit the FTS5 fix into the TS product by swapping its codegraph dependency).
  - A dedicated section on why this removes the FTS5 cliff and, importantly, what Rust does not change — the moat still has to be ported faithfully.

  Both lean on ../codegraph_arch.md (the moat) and treat ../codewalk_kb/ as the behavioral spec. Net: a new session can start at the M0 checklist and build with the TS source as the reference at
  every step.

  The one thing flagged for the build session: Rust isn't installed on this machine — M0 starts with rustup.
