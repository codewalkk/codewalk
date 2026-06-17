# REVIEW.md — how to review a change to codewalk_rust

This repo **ports** a working TypeScript implementation (`../codewalk_kb/`) to Rust,
and every claim is **gated on parity numbers**, not vibes. Review accordingly: a
change is correct when it matches the spec's behavior *and* holds the invariants
*and* doesn't regress the benchmark — not when it merely compiles.

Use this checklist for any PR/diff, and especially before signing off a milestone.

---

## 1. Invariants that must never break (hard gates)

From `docs/rust-build-plan.md` §0.1 + `CLAUDE.md`. A diff that violates any of these
is wrong regardless of test results:

1. **`codegraph-core` stays pure** — no embeddings, no LLM, no network, no `reqwest`,
   no model downloads. Mode 1 must remain offline, deterministic, single-binary. All
   learned/LLM concerns live in `codewalk-kb` (mode 2). Grep the diff for new deps in
   `codegraph-core/Cargo.toml` and reject anything embedding/LLM/network-shaped.
2. **Typed enums, not stringly-typed** — `NodeKind`/`EdgeKind`/`Language`/`Provenance`
   stay enums. Do **not** introduce free-form `TEXT` kinds (that's CBM's weakness).
3. **Every synthesized/guessed edge is provenance-tagged** `Provenance::Heuristic`
   with `synthesizedBy` + `registeredAt` metadata. **Silent beats wrong**; partial
   coverage that bridges one boundary but not the next is *worse* than none — flows
   must connect end-to-end. Check new synthesizers/resolvers for the tag and for honest
   truncation (a chain that stops at dynamic dispatch is fine; a wrong edge is not).
4. **Agent-sufficiency over power** — the default MCP surface stays at 3–4 tools with
   success-shaped errors (`isError` only for path refusals + real malfunctions;
   not-indexed / not-found return SUCCESS + guidance). Do not add tools to the default
   `tools/list` without a benchmark justification. No raw-Cypher surface by default.
5. **Bundled SQLite + FTS5** — `rusqlite` `features=["bundled"]` (FTS5 is compiled in;
   there is no separate `fts5` feature). Don't switch to a system SQLite.
6. **PORT, don't invent** — for any change to a ported module, the reviewer opens the
   TS counterpart (reference map: `docs/rust-build-plan.md` §3) and confirms the Rust
   matches its behavior. Deviations must be deliberate and commented with the reason.
   `node_id` must stay byte-compatible with the TS `generateNodeId`.

---

## 2. Parity / quality gates (run these)

Toolchain isn't on `PATH` by default: `source ~/.cargo/env` first.

### Build + unit tests (always)
```bash
cargo build && cargo test            # must be clean; no new warnings
```

### Index parity (any extraction/resolution change)
```bash
# Re-index k8s. NOTE: a schema change requires deleting the DB first —
# CREATE TABLE IF NOT EXISTS will NOT alter an existing table.
rm -f ../k8s/.codegraph/graph.db*
cargo run --release -p codegraph-cli -- index ../k8s --stats
```
Compare `{files, nodes, edges}` and per-kind counts to the TS target
**8,898 files · 166,916 nodes · 592,479 edges** (Go-only is expected slightly lower;
report the delta and which kinds moved). A drop in resolved-edge count or
single-candidate share is a regression — explain it.

### Retrieval recall (any context/search/ranking change)
```bash
node ../codewalk_kb/bench/bench-rust.mjs ../codewalk_kb/bench/k8s/queryset.json context
```
Structural-cold recall must stay **≈0.32** (the TS gate). It scores substring presence
of each query's `expectSymbols` + `expectFiles` in the `codegraph context` markdown.

### Agent A/B (any MCP/explore change) — the hero question
```bash
node ../codewalk_kb/bench/bench-rust-agent.mjs \
    ../codewalk_kb/bench/k8s/queryset.json scheduler-cycle --with-only
```
Pass bar: **0 Read · 0 Grep**, the answer contains the expected symbols, and the work
is done via codegraph tools. ⚠️ **Always pass `--strict-mcp-config`** (the harness sets
it) — without it the user-installed `codewalk` MCP server leaks into the run and
confounds the tool counts.

### Resolution precision / camelCase FTS (resolution or FTS change)
- The confidence ladder (`name_matcher.rs` `mod ladder`) must be stored on each resolved
  edge's `metadata.confidence`. Spot-check the distribution and the single-candidate
  (≥0.75) share — it must not fall.
- camelCase FTS: `name_split` populated for multi-token names; a sub-token query (e.g.
  `filter`) must match camelCase names (`RunFilterPlugins`) it wouldn't via name-prefix.

### Spot-checks (cheap, always worth it)
```bash
cargo run --release -p codegraph-cli -- node --path ../k8s ScheduleOne     # → Scheduler::ScheduleOne in pkg/scheduler/schedule_one.go
cargo run --release -p codegraph-cli -- callees --path ../k8s ScheduleOne   # flow connects toward the framework
```

---

## 3. Per-language × framework bar (M4+)

When a new language or framework lands, apply the bar in
`../codewalk_kb/codegraph/CLAUDE.md`: small/medium/large real repos, ≥3 flow prompts,
deterministic probes + an agent A/B (≥2 runs), with a ~0 Read/Grep target inside the
explore budget. Node/edge counts within ±5% of the TS CodeGraph on a shared repo. A
new synthesizer must turn a flow question that "breaks" without it into ~0 Read/Grep,
with no regression on a control repo.

---

## 4. Code-quality review (after correctness)

- **Reads like the neighbours** — match the surrounding comment density, naming, and
  idiom. Ported modules cite their TS source (`port of <file>.ts`) and explain
  deviations.
- **No silent caps** — if a change bounds coverage (top-N, no-retry, sampling, a budget
  truncation), it must `log`/note what was dropped; silent truncation reads as "covered
  everything" when it didn't.
- **Pin versions** — tree-sitter crate ↔ grammar crate ABI skew is a known risk; new
  grammars are pinned in the workspace and smoke-tested on a known file.
- **Determinism** — mode 1 output must be reproducible; watch for hash-map iteration
  order leaking into rendered output (use the insertion-ordered `IndexMap` where the TS
  used a `Map`).
- **Errors** — MCP tool errors are success-shaped; CLI errors use `anyhow` with context.

---

## 5. Milestone sign-off

**Do not claim a milestone done without its parity gate met and the numbers in the
commit message** (`docs/rust-build-plan.md` §5/§6). The commit body should carry the
index counts, recall, and the A/B result — exactly what a reviewer would otherwise have
to re-derive. If tests fail, a step was skipped, or a gate regressed, say so plainly;
keep the milestone open.
