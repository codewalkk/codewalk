# MEMORY.md — start-here index for codewalk_rust

A pointer map for anyone (human or agent) picking up this repo. The authoritative
*behavioral spec* is the TypeScript implementation in `../codewalk_kb/`; this repo
**ports** it to Rust, gated on parity. Read `CLAUDE.md` first, then the design docs
below.

## What this is

A Cargo workspace re-implementing Codewalk's structural engine (and, later, its
learned layer) natively in Rust. Two modes → two binaries → two DBs:

- **`codegraph`** (mode 1, built) — structural intelligence only: extraction · db ·
  resolution · graph · context · MCP serve. Pure, offline, deterministic, single
  static binary. Links `codegraph-core` only.
- **`codewalk`** (mode 2, stub) — adds the learned RAG layer (embeddings + KB from
  transcripts). Links core + `codewalk-kb`. Never worse than structural on a KB miss.

## Design docs (read in this order)

1. [`docs/rust-build-plan.md`](docs/rust-build-plan.md) — **the plan**: status banner
   (M0–M3 done), design invariants (§0.1), workspace layout (§1), TS→Rust dependency
   map (§2), per-module reference map (§3), the two MCP surfaces (§4), parity/validation
   methodology (§5), phased milestones M0–M6 with best-of-both inserts (§6).
2. [`docs/codewalk_rust_arch.md`](docs/codewalk_rust_arch.md) — **what it is**: crate
   sketches, the `LanguageExtractor` / `FrameworkResolver` / synthesizer traits, the
   storage model, napi bindings.
3. [`codegraph_arch.md`](codegraph_arch.md) — **the moat**: why the 26 extractors + 24
   framework resolvers + 20 synthesizers are the value, with `file:line` refs into the
   TS spec.
4. [`CLAUDE.md`](CLAUDE.md) — project instructions + the "PORT, don't invent" rule.
5. [`REVIEW.md`](REVIEW.md) — **how to review a change here** (parity gates + invariants
   + the commands to run them). Read before reviewing or claiming a milestone done.

Best-of-both rationale (CBM vs CG vs CW comparative study) lives at
`../codebase-mem-mcp/docs/` — start with `04-rust-improvement-plan.md`.

## Where the code lives

| Concern | Path |
|---|---|
| Structural engine (the moat) | `codegraph_rust/codegraph-core/src/` |
| ├─ extraction (tree-sitter, rayon, Go) | `extraction/` |
| ├─ db (rusqlite bundled+fts5, schema) | `db.rs`, `schema.sql` |
| ├─ resolution (imports · name-match · synth) | `resolution/` |
| ├─ graph traversal (callers/callees/path/BFS) | `graph.rs` |
| ├─ context (buildContext + explore + budgets) | `context/` |
| └─ lexical ranking helpers | `search/query_utils.rs` |
| `codegraph` binary (index/search/node/callers/context/serve) | `codegraph_rust/codegraph-cli/src/` (`main.rs`, `mcp.rs`, `server_instructions.rs`) |
| Learned layer (mode 2, stub) | `crates/codewalk-kb/`, `crates/codewalk-cli/` |
| napi bindings (stub) | `codegraph_rust/codegraph-node/` |

## Current state (2026-06-17)

**M0–M3 done.** The engine indexes Go, resolves references (cross-package import +
name-match), synthesizes provenance-tagged edges, traces callers/callees, builds
lexical context, and **serves the 4-tool CG MCP surface over stdio** (`codegraph
serve --mcp --path <repo>`). k8s parity: 8,751 files · 167,101 nodes · 592,456 edges
(TS target 8,898 / 166,916 / 592,479). Structural-cold recall 0.328 (TS ~0.32). Hero
question answered with 0 Read / 0 Grep. See the `docs/rust-build-plan.md` status banner
for the full gate table.

**Next: M4** — widen languages (hybrid table-driven + per-language-hook extractor),
the Go type-resolution pass, then synthesizers/frameworks (ROI-ordered, §5-gated).

## Two SQLite DBs (both rusqlite `bundled` → FTS5 baked in)

- `<repo>/.codegraph/graph.db` — nodes, edges (+ `provenance`), files, unresolved_refs,
  `nodes_fts` (incl. the `name_split` camelCase column).
- `<repo>/.codewalk/kb.db` — learned KB (mode 2, not built yet).

Keeping them separate means re-indexing code never wipes learned knowledge.
