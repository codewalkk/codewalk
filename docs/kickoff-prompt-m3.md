# New-session kickoff prompt — build `codewalk_rust` M3 onward

*Paste everything in the fenced block below as the first message of a fresh
Claude Code session opened in `/home/azureuser/cw_june/codewalk_rust/`. It is
self-contained: it points at the current Rust state, the TypeScript behavioral
spec, and the source-verified competitive study that defines the "best-of-both"
target.*

*(The earlier [`kickoff-prompt.md`](kickoff-prompt.md) drove M0–M1 and is kept as
history. This one starts from M2-done and builds M3 → M6.)*

---

```
You are continuing the Rust-native rebuild of Codewalk. M0–M2 are DONE: the Rust
core indexes Go, resolves references (cross-package import + name-match),
synthesizes a few Go edges (provenance-tagged), and traces callers/callees —
verified end-to-end. Your job now is M3 onward: make the binary AGENT-USABLE
(MCP server + buildContext), then widen languages, add incremental indexing, and
build the learned layer — folding in the "best-of-both" improvements from the
competitive study.

## Layout (siblings under /home/azureuser/cw_june/)
- /home/azureuser/cw_june/codewalk_rust/   ← YOU BUILD HERE (cargo workspace; M0–M2 done)
    · codegraph_rust/codegraph-core         ← the working engine (extraction/resolution/db/graph)
    · codegraph_rust/codegraph-cli          ← `codegraph` bin (index/search/node/callers/callees/trace)
    · crates/codewalk-kb, crates/codewalk-cli, codegraph_rust/codegraph-node  ← placeholders (M5/M6)
- /home/azureuser/cw_june/codewalk_kb/      ← the working TypeScript Codewalk = BEHAVIORAL SPEC
- /home/azureuser/cw_june/codewalk_kb/codegraph/  ← the codegraph submodule = the MOAT to port
- /home/azureuser/cw_june/codebase-mem-mcp/ ← Codebase-Memory (the C competitor) + the STUDY in docs/
- /home/azureuser/cw_june/k8s/              ← Kubernetes, indexed by the TS build (parity target: 8,898 files · 166,916 symbols · 592,479 edges)

## Read these FIRST, in order (do not skip — they are the design)
1. docs/rust-build-plan.md   — the plan: status (M0–M2 done), design invariants (§0.1), milestone map + per-milestone scope & gates (§6, now with "best-of-both inserts"), validation methodology (§5). START HERE.
2. ../codebase-mem-mcp/docs/04-rust-improvement-plan.md — WHY each best-of-both insert exists (the actionable roadmap; every item tagged CBM/CG/CW with a parity gate).
3. ../codebase-mem-mcp/docs/03-competitive-analysis.md — the head-to-head: what each system uniquely does; where WE must improve (resolution precision, incremental, staleness) and where we already win (synthesizer moat, agent ergonomics, the learned layer).
4. docs/codewalk_rust_arch.md — trait/type sketches (LanguageExtractor, FrameworkResolver, synthesizers), storage model, the two modes, napi bindings.
5. ../codegraph_arch.md       — the moat: why the 26 extractors + 24 framework resolvers + 20 synthesizers are the value, with file:line refs into the codegraph submodule.
(Deeper CBM ground truth if needed: ../codebase-mem-mcp/docs/01-codebase-memory-architecture.md.)

## First: verify the starting point
- `cargo build` and `cargo test` from the workspace root — confirm green (M0–M2: ~3 tests in codegraph-core).
- `cargo run -p codegraph-cli -- index /home/azureuser/cw_june/k8s --stats` — note current {files, nodes, edges} and the delta vs the TS target (only Go is ported, so expect lower; record which kinds are missing).

## This session's objective: M3 — MCP server (mode 1) + buildContext
The binary indexes and traces but is NOT agent-usable yet. M3 closes that. Deliver:

1. buildContext equivalent (CG) — port codegraph/src/context/index.ts → codegraph-core/src/context/.
   Commodity lexical ranking, NO vectors: identifier extraction from NL → exact-name + co-location boost → stem/prefix variants → FTS per term → test-file dampening → hub (dominant-file) boost → multi-term co-occurrence re-rank → CamelCase-boundary LIKE → import→def resolution → graph expansion (type-hierarchy + BFS depth 1) → multi-stage token budget (maxNodes, per-file diversity cap, non-production ≤15%, edge recovery) → markdown with a `## Call paths` section annotating synthesized hops inline.

2. MCP server with the CG-proven 4-tool surface (CG) — use `rmcp` (official Rust MCP SDK), stdio.
   Tools: codegraph_explore (PRIMARY, takes a precise symbol bag → finds the flow among them), codegraph_node (full body + caller/callee trail; for an ambiguous name return EVERY overload's body in one call), codegraph_search, codegraph_callers.
   - Success-shaped errors: isError=true ONLY for path refusals + real malfunctions. not-indexed / symbol-not-found return a SUCCESS-shaped response with guidance. An unindexed workspace serves an empty tools/list + a 2-line "inactive" instructions variant. (Lesson: one or two errors and the agent abandons the server.)
   - Single source of guidance: the MCP initialize instructions (port server-instructions.ts). Do NOT write a CLAUDE.md block.
   - Size-scaled explore budget: port getExploreBudget/getExploreOutputBudget — call-count and char/file budgets scale monotonically with indexed file count, held under the host's ~25K inline-result cap.
   - Indexing stance: default to CG's "indexing is the user's call" (predictable); add an opt-in `--auto-index` flag (CBM auto-indexes on connect — offer it, don't default to it).

3. [insert] Resolution polish from CBM (CBM) — in resolution/name_matcher.rs:
   - Explicit confidence ladder with these floors, stored on the edge: import_map 0.95, same_module 0.90, import_map_suffix 0.85, unique_name 0.75, suffix_match 0.55, fuzzy 0.40/0.30.
   - Candidate-count penalty (floor confidence for high-fan-out names) + test-path deprioritization.
   - A camelCase/snake pre-split scalar SQL function for FTS (CBM's cbm_camel_split) so `getUserById` tokenizes to get/user/by/id.

## M3 done-gates (do not claim done without these)
- The codewalk_kb benchmark harness (bench/bench.mjs + k8s/queryset.json) drives the Rust MCP server; structural-cold recall MATCHES the TS structural recall (~0.32).
- The k8s hero question (scheduler-cycle) is answered in ≤1 explore call with 0 Read/Grep, matching the TS A/B.
- `codegraph serve --mcp --path /home/azureuser/cw_june/k8s` runs as a single binary with no Node and no FTS5 cliff.
- Resolution precision: single-candidate call-resolution share on Go k8s ≥ current; FTS recall on camelCase queries measurably improved.

## After M3 (subsequent sessions — full detail in docs/rust-build-plan.md §6)
- M4   widen languages via a HYBRID extractor model (generic table-driven core + optional per-language hook trait = CBM breadth × CG depth); port TS/JS → Python → Rust → Java → C/C++; add the per-language TYPE-RESOLUTION pass (Go first — biggest precision lever from CBM); port synthesizers then framework resolvers, ROI-ordered and §5-gated.
- M4.5 incremental indexing (git-status + content-hash diff; scope resolution to changed files) + `notify` watcher. (content_hash column already exists, unused.)
- M5   the learned layer (codewalk binary): fastembed MiniLM, KB store, RRF fusion (never worse than structural), transcript capture/distill, install at LOCAL scope. Fix CW gaps while porting: wire markStale (reuse M4.5 hashing), miss-driven capture from kb_query_log, sqlite-vec when KB grows.
- M5.5 Leiden communities + get_architecture; MinHash/LSH SIMILAR_TO (medium priority).
- M6   napi-rs bindings + `curl | sh` single-binary distribution + CBM security discipline (path containment, shell-arg validation, signed releases + SBOM, transcript-privacy gate).

## Hard constraints / invariants (docs/rust-build-plan.md §0.1 — do not violate)
- codegraph-core stays PURE: no embeddings, no LLM, no network. The learned layer lives in codewalk-kb (mode 2), depending on core, never the reverse.
- Typed enums (NodeKind/EdgeKind), NOT stringly-typed. (CBM uses free-form TEXT — that's worse; don't copy it.)
- Every synthesized/guessed edge is tagged Provenance::Heuristic with synthesizedBy/registeredAt; flows connect END-TO-END ("partial coverage is worse than none"; silent beats wrong).
- Agent-sufficiency over power: 3–4 tools, success-shaped errors, output sufficient to STOP the read. Do NOT ship CBM's 14-tool + raw-Cypher surface as the default.
- The TS code is the SPEC — PORT, don't invent. For every Rust module read its TS counterpart first (reference map: docs/rust-build-plan.md §3). When unsure how codegraph behaves, READ the TS source; you can also `codewalk_ask` the TS codebase (the codewalk MCP server is installed in codewalk_kb).

## Working style
- Pin versions in the workspace (tree-sitter crate ↔ grammar crate ABI skew is a known risk).
- Commit per milestone with the parity numbers in the message.
- Show me the passing M3 gates (harness-driven recall + the hero-question A/B) before moving to M4.

Start by reading docs/rust-build-plan.md and ../codebase-mem-mcp/docs/04-rust-improvement-plan.md, verify the M0–M2 build is green, then do M3.
```

---

*Subsequent milestones M4–M6 are detailed in [`rust-build-plan.md`](rust-build-plan.md) §6,
with the "best-of-both" rationale in
[`../codebase-mem-mcp/docs/04-rust-improvement-plan.md`](../codebase-mem-mcp/docs/04-rust-improvement-plan.md).*
</content>
