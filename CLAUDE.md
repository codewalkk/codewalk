# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Status: pre-implementation

This repo is **planning/spec stage**. It currently contains only design docs — there is
**no Rust code yet**: `codegraph_rust/` is empty, there is no Cargo workspace, and Rust
is **not installed** on this machine. The first task is M0 of the build plan: install the
toolchain and scaffold the workspace.

Read these three docs before doing anything (they *are* the design):

1. `docs/rust-build-plan.md` — how/when: crate layout, TS→Rust dependency map, per-module
   reference map, phased milestones M0–M6, validation gates, first-session checklist.
2. `docs/codewalk_rust_arch.md` — what it is: crates, trait/type sketches
   (`LanguageExtractor`, `FrameworkResolver`, synthesizers), storage model, the two modes.
3. `codegraph_arch.md` — the moat: why the 26 extractors + 24 framework resolvers + 20
   dynamic-dispatch synthesizers are the value, with `file:line` refs into the TS spec.

`docs/kickoff-prompt.md` is the self-contained brief for the build session.

## The behavioral spec is authoritative — PORT, don't invent

The working TypeScript implementation in `../codewalk_kb/` is the **behavioral spec**, and
`../codewalk_kb/codegraph/` is the codegraph submodule = the moat being ported. For every
Rust module, **read its TS counterpart first** (reference map: `docs/rust-build-plan.md` §3).
When unsure how codegraph behaves, read the TS source rather than guessing.

Sibling layout under `/home/azureuser/cw_june/`:
- `codewalk_rust/` — this repo (you build here)
- `codewalk_kb/` — the TS Codewalk (behavioral spec); `codewalk_kb/codegraph/` is the moat
- `k8s/` — Kubernetes checkout, already indexed by the TS build (the parity target)

Key anchor files in the spec (see `docs/rust-build-plan.md` §3 for the full table):
- Schema → `codewalk_kb/codegraph/src/db/schema.sql` (port verbatim to a rusqlite migration)
- Types → `codewalk_kb/codegraph/src/types.ts` (`NODE_KINDS`, `EdgeKind`, `LANGUAGES`)
- Extractor contract → `codewalk_kb/codegraph/src/extraction/tree-sitter-types.ts:80`
- Go extractor → `codewalk_kb/codegraph/src/extraction/languages/go.ts` (port FIRST — k8s is Go)
- Resolver contract → `codewalk_kb/codegraph/src/resolution/types.ts:164` + `frameworks/index.ts:33`
- Synthesizers → `codewalk_kb/codegraph/src/resolution/callback-synthesizer.ts` (20 channels)
- Validation methodology → `codewalk_kb/codegraph/CLAUDE.md` (the language×framework pass bar)

## Architecture (target)

A Cargo workspace with two binaries mapping 1:1 to two modes, plus TS bindings:

- **`codegraph-core`** (lib) — the structural engine and reusable moat: extraction · db ·
  resolution · graph · context. Pipeline: `files → extraction (tree-sitter, rayon) → db
  (nodes/edges/files, FTS5) → resolution (imports · name-match · frameworks · synthesizers)
  → graph (callers/callees/impact/path) → context (markdown/json)`. Knows **nothing** about
  embeddings or LLMs.
- **`codewalk-kb`** (lib) — optional learned-intelligence layer: embeddings (fastembed
  MiniLM-L6-v2, 384-d) · KB store · RRF fusion · transcript capture/distill. **Depends on
  core, never the reverse.**
- **`codegraph-cli`** → `codegraph` binary (Mode 1): links core only. Structural MCP tools,
  single static binary, no model/LLM/network.
- **`codewalk-cli`** → `codewalk` binary (Mode 2): links core + kb. Adds learned MCP tools;
  on a KB miss degrades to structural (never worse than codegraph).
- **`codegraph-node`** — napi-rs bindings exposing core to TS/JS (`@codewalk/codegraph-native`).

Two SQLite databases, both `rusqlite` with `features=["bundled","fts5"]`:
- `<repo>/.codegraph/graph.db` — nodes, edges (+ `provenance`), files, unresolved_refs, `nodes_fts`
- `<repo>/.codewalk/kb.db` — `kb_entries` (+ embedding blobs), `kb_fts`, `kb_meta`

Keeping two DBs means re-indexing code never wipes learned knowledge.

## Non-negotiable constraints

- **`codegraph-core` stays pure**: no embedding/LLM/network code. Mode 1 must remain a pure,
  offline, deterministic single binary. All learned/LLM concerns live in `codewalk-kb`.
- **FTS5 via `rusqlite` `bundled,fts5`** — statically linked SQLite-with-FTS5 is the whole
  point (it removes the "FTS5 cliff" the TS build hit with `node:sqlite`). Prove it works in
  M0 with a `CREATE VIRTUAL TABLE … USING fts5` test on a stock toolchain (no Node).
- **Synthesized edges are honest**: tag every one `provenance = Heuristic` with
  `synthesizedBy` + `registeredAt`, and **close flows end-to-end** — "partial coverage is
  worse than none" (bridging one boundary but not the next *raises* the agent's read count).
- **MCP installer registers at LOCAL scope by default** (Mode 2) — a project `.mcp.json`
  server sits in Claude Code as "⏸ pending approval"; local scope is auto-trusted.
- **Don't big-bang the moat**: port Go first (→ k8s benchmark), gate on the benchmark, widen
  languages after. Keep TS codegraph as reference and fallback during the transition.

## Build & validation (once code exists)

Toolchain is not installed. M0 starts with:
`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh` (then source the env).

Target binaries/commands (per `docs/rust-build-plan.md` §4):
- `codegraph index <repo>` · `codegraph query`/`search` · `codegraph serve --mcp --path <repo>`
- `codewalk` adds `update` (mine transcripts) and `install` (wire into Claude Code, local scope)

**Do not claim a milestone done without its parity gate** (`docs/rust-build-plan.md` §5):
- **Index parity** — `codegraph index ../k8s` `{files, nodes, edges}` within tolerance of the
  TS index (target: 8,898 files · 166,916 symbols · 592,479 edges; expect lower while only Go
  + partial resolution is ported — report the delta and which node/edge kinds are missing).
- **Spot-check** — `getNodesByName` (e.g. `ScheduleOne`, `RunFilterPlugins`) resolves to the
  right `pkg/scheduler/*.go` files, compared to the TS structural search.
- **Retrieval / agent A/B** — reuse the JS harness at `../codewalk_kb/bench/` (`bench.mjs`,
  `k8s/queryset.json`); it drives the Rust binaries directly via the SQLite + MCP endpoint.
- **Per language×framework** — the bar in `../codewalk_kb/codegraph/CLAUDE.md`: small/medium/
  large real repos, ≥3 flow prompts, deterministic probes + agent A/B, ~0 Read/Grep target.

Commit per milestone with the parity numbers in the message.
