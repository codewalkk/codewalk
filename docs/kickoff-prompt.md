# New-session kickoff prompt — build `codegraph_rust`

*Paste everything in the fenced block below as the first message of a fresh
Claude Code session opened in `/home/azureuser/cw_june/codewalk_rust/`. It is
self-contained and points at the TypeScript repo as the behavioral spec.*

---

```
You are starting the Rust-native rebuild of Codewalk. Your job this session is to
stand up `codegraph_rust` — the structural code-intelligence engine — in this
workspace, using the existing TypeScript implementation as the behavioral spec.

## Layout (siblings under /home/azureuser/cw_june/)
- /home/azureuser/cw_june/codewalk_rust/   ← YOU BUILD HERE (cargo workspace root; `docs/`, empty `codegraph_rust/`)
- /home/azureuser/cw_june/codewalk_kb/     ← the working TypeScript Codewalk = the BEHAVIORAL SPEC
- /home/azureuser/cw_june/codewalk_kb/codegraph/  ← the codegraph submodule = the MOAT to port
- /home/azureuser/cw_june/k8s/             ← Kubernetes, already indexed by the TS build (parity target)

## Read these FIRST, in order (do not skip — they are the design)
1. docs/rust-build-plan.md        — how/when: crate layout, dependency map (TS→Rust), reference map, phased milestones M0–M6, validation gates, first-session checklist.
2. docs/codewalk_rust_arch.md     — what it is: crates, trait/type sketches (LanguageExtractor, FrameworkResolver, synthesizers), storage model, the two modes, napi bindings.
3. codegraph_arch.md              — the moat: why the 26 extractors + 24 framework resolvers + 20 dynamic-dispatch synthesizers are the value, with file:line refs into the codegraph submodule.

## What we're building (recap)
A Rust-native Codewalk with TWO modes:
- `codegraph` binary  = MODE 1: structural intelligence only (AST/symbol graph, FTS, callers/callees/impact, context). No model, no LLM, single static binary.
- `codewalk` binary   = MODE 2: codegraph + LLM learned intelligence (local embeddings + a KB distilled from Claude Code transcripts; fused retrieval).
Plus `codegraph-node` (napi-rs) so the core is callable from TS/JS.

## Why Rust (the motivating wins — keep these true)
- FTS5 cliff is GONE: use `rusqlite = { features = ["bundled", "fts5"] }` — SQLite with FTS5 is statically linked, no Node-version dependency. PROVE this in M0.
- Single static binary distribution; native tree-sitter (no WASM); real parallelism (`rayon`).

## The spec is authoritative — PORT, don't invent
codegraph is a hard-won moat (per-grammar extractor edge cases, framework control-flow recovery, dynamic-dispatch synthesis). For every Rust module, READ its TS counterpart first; the reference map is in docs/rust-build-plan.md §3. Anchor files:
- Schema:        codewalk_kb/codegraph/src/db/schema.sql           → port verbatim to a rusqlite migration
- Types:         codewalk_kb/codegraph/src/types.ts                → NodeKind/EdgeKind/Language enums
- Extractor API: codewalk_kb/codegraph/src/extraction/tree-sitter-types.ts:80   → LanguageExtractor trait
- Go extractor:  codewalk_kb/codegraph/src/extraction/languages/go.ts (k8s is Go — port this FIRST)
- Default ignores: codewalk_kb/codegraph/src/extraction/index.ts:117
- Resolver API:  codewalk_kb/codegraph/src/resolution/types.ts:164 + frameworks/index.ts:33 (24 integrations)
- Synthesizers:  codewalk_kb/codegraph/src/resolution/callback-synthesizer.ts   (20 channels; provenance:'heuristic')
- Graph:         codewalk_kb/codegraph/src/graph/traversal.ts
- Context:       codewalk_kb/codegraph/src/context/
- MCP tools/budgets: codewalk_kb/codegraph/src/mcp/tools.ts (getExploreBudget/getExploreOutputBudget), server-instructions.ts
- Validation methodology: codewalk_kb/codegraph/CLAUDE.md  (the language×framework pass bar)
(Learned layer, for MODE 2 later: codewalk_kb/src/{embeddings/embedder.ts, kb/store.ts, retrieval/fuse.ts, core.ts, capture/*.ts, install/claude.ts, mcp/server.ts}.)

## This session's objective: M0 → M1 (Go-only, FTS5-clean, index parity)
1. M0 — toolchain + scaffold:
   - Rust is NOT installed on this machine. Install it: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh` (then source the env).
   - Create the cargo workspace + crate skeletons per docs/rust-build-plan.md §1 (codegraph-core, codegraph-cli, codewalk-kb, codewalk-cli, codegraph-node). The existing empty `codegraph_rust/` folder can host codegraph-core.
   - Add `rusqlite` with `features=["bundled","fts5"]` to codegraph-core; port schema.sql to a migration; write a test that creates the schema AND a `CREATE VIRTUAL TABLE … USING fts5` table. This passing test is the proof the cliff is gone.
2. M1 — Go extraction MVP:
   - `LanguageExtractor` trait (port tree-sitter-types.ts:80) + a Go extractor (`tree-sitter` + `tree-sitter-go`), gitignore-aware file walk (`ignore` crate), node/edge model, store into rusqlite, FTS search, parallelize with `rayon`.
   - Index /home/azureuser/cw_june/k8s and compare counts to the TS index.

## Validation gates (do not claim done without these)
- FTS5 proof: the M0 test creates an fts5 table on a stock toolchain (no Node).
- Index parity: `codegraph index /home/azureuser/cw_june/k8s` yields {files, symbols(nodes), edges} within tolerance of the TS index — TARGET: 8,898 files · 166,916 symbols · 592,479 edges (it's fine to be lower while only Go + partial resolution is ported; report the delta and which node/edge kinds are missing).
- Spot-check: a few `getNodesByName` lookups (e.g. ScheduleOne, RunFilterPlugins) resolve to the right pkg/scheduler/*.go files (compare to the TS structural search).
- The benchmark harness at codewalk_kb/bench/ (bench.mjs + k8s/queryset.json) and codegraph/CLAUDE.md describe the fuller methodology for later milestones.

## Hard constraints / lessons already learned
- Keep `codegraph-core` free of any embedding/LLM/network code — MODE 1 must stay a pure, offline, deterministic single binary. The learned layer lives in `codewalk-kb` (MODE 2), depending on core, never the reverse.
- Synthesized edges must be tagged `provenance = Heuristic` with `synthesizedBy`/`registeredAt`, and flows must connect END-TO-END ("partial coverage is worse than none").
- (For the MODE 2 installer later) register the MCP server at LOCAL scope by default — a project `.mcp.json` server sits in Claude Code as "⏸ pending approval"; local scope is auto-trusted.
- Don't big-bang the moat: port Go first (k8s), gate on the benchmark, widen languages after. Keep the TS codegraph as the reference (and fallback) during the transition.

## Working style
- Pin versions in the workspace (tree-sitter crate ↔ grammar crate ABI skew is a known risk; smoke-test each grammar on a real file).
- Commit per milestone with the parity numbers in the message.
- When unsure how codegraph behaves, READ the TS source (it's the spec) rather than guessing. If the codewalk MCP server is available (it's installed in codewalk_kb), you can also `codewalk_ask` the TS codebase.

Start by reading the three docs, then do M0. Show me the workspace skeleton and the passing FTS5 test before moving to M1.
```

---

*After this session, continue with M2–M6 from `docs/rust-build-plan.md` §6.*
