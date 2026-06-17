# Codewalk-in-Rust — Build Plan

*Plan for a Rust-native re-implementation of Codewalk (codegraph core + the
learned-intelligence layer) under `../codewalk_rust/`. Written from the working
TypeScript implementation in `../codewalk_kb/`, which is the **behavioral spec**.
This doc + [`codewalk_rust_arch.md`](codewalk_rust_arch.md)
+ [`../codegraph_arch.md`](../codegraph_arch.md) are the entry points.*

> **Status (2026-06-16): M0–M3 are DONE.** The Rust core indexes Go, resolves
> references (cross-package import + name-match), synthesizes Go edges, traces
> callers/callees, builds lexical context, and **serves the 4-tool CG MCP surface
> over stdio (mode 1)** — verified end-to-end against the k8s benchmark. **Next up
> is M4 onward.**
>
> **M3 parity (k8s, Go-only):** index 8,751 files · 167,101 nodes · 592,456 edges
> (TS target 8,898 · 166,916 · 592,479 — at parity). Structural-cold recall
> **0.328** (TS gate ~0.32). Hero question (`scheduler-cycle`): WITH codegraph
> **0 Read · 0 Grep**, 5/5 expected symbols, via `codegraph_explore` only — vs
> 6 Reads for the no-tool baseline. camelCase FTS recall via the `name_split`
> column roughly doubles sub-token matches (e.g. `filter` 162→545, `informer`
> 1081→2296). `codegraph serve --mcp` runs as a single static binary, no Node, no
> FTS5 cliff. The resolution confidence ladder is stored on every resolved edge.
>
> **This plan now folds in the "best-of-both" improvement plan** derived from a
> source-verified study of **Codebase-Memory** (the Tree-Sitter-in-C MCP
> competitor) vs our CodeGraph/Codewalk stack. The full study lives at
> [`../codebase-mem-mcp/docs/`](../codebase-mem-mcp/docs/) — read
> [`04-rust-improvement-plan.md`](../codebase-mem-mcp/docs/04-rust-improvement-plan.md)
> for the rationale behind every "best-of-both insert" below. Each insert is
> tagged with its source of truth: **CBM** (Codebase-Memory), **CG** (CodeGraph),
> **CW** (Codewalk).

---

## 0. Why Rust, and what we're building

The TS prototype works and is proven on Kubernetes (recall 0.32→0.87; one
`/codewalk` call = 5× fewer tool calls, ~2.9× cheaper — see
`../codewalk_kb/bench/k8s/`). Three things pushed us to a native core (all flagged
in `../codewalk_kb/docs/launch-plan.md`):

1. **FTS5 cliff** — the TS structural store uses Node's `node:sqlite`, whose
   bundled SQLite only ships FTS5 on Node 24+. In Rust, `rusqlite` with
   `features = ["bundled", "fts5"]` statically links a SQLite **with FTS5 baked
   in** — the cliff disappears, on any platform, with no runtime dependency.
2. **Distribution** — a single static binary (`curl | sh`, no Node, no native
   addon matrix) collapses the biggest install-friction items into one artifact.
3. **Performance** — native tree-sitter (no WASM) + real parallelism turns the
   ~34-min Kubernetes index into (target) single-digit minutes.

**Two modes** (a hard requirement):
- **`codegraph-only`** — structural intelligence only (AST/symbol graph, FTS,
  callers/callees/impact, context). 100% local, deterministic, no model, no LLM.
  Binary: `codegraph`.
- **`codegraph + llm learned intelligence`** — adds the learned-knowledge RAG
  layer (local embeddings + KB distilled from transcripts). Binary: `codewalk`.

**TS bindings** — `codegraph-core` also ships as a Node native module
(napi-rs), so (a) the existing TS Codewalk can swap its codegraph dependency for
the native core (killing the FTS5 cliff in the TS product too) and (b) anyone can
build a JS/TS app on `codegraph_rust`.

**Non-goals (for now):** day-one parity with codegraph's full 30+ language /
24-framework / 20-synthesizer coverage (that's the moat — see
[`../codegraph_arch.md`](../codegraph_arch.md) — and it's a long, benchmark-gated
tail, §6). Browser/WASM target (napi/Node first). The CBM-style **direct B-tree
page-writer flush** (kernel-in-3-min) — transactional bulk INSERT with
indexes-dropped is enough until kernel-scale indexing is a real blocker.

### 0.1 Design invariants (do not violate)

From the comparative study — these are load-bearing and must hold across every
milestone:

1. **`codegraph-core` stays pure** — no embeddings, no LLM, no network. The
   learned layer is a *separate crate/binary/DB* (`codewalk-kb`). *(Honored.)*
2. **Two modes, two binaries, two DBs** — `codegraph` (`.codegraph/graph.db`) and
   `codewalk` (`.codewalk/kb.db`). *(Scaffolded; mode-2 empty.)*
3. **Typed enums, not stringly-typed** — keep `NodeKind`/`EdgeKind`. CBM uses
   free-form `TEXT` for labels/edge-types; that is *worse* — do **not** copy it.
4. **Every synthesized/guessed edge is provenance-tagged** — `Provenance::Heuristic`
   + `synthesizedBy`/`registeredAt`; **silent beats wrong**. *(Honored in
   `synth.rs`.)* CBM has no provenance — this is a CG win we keep.
5. **Agent-sufficiency over power** — 3–4 tools, success-shaped errors, output
   sufficient to stop the read. Do **not** ship CBM's 14-tool + raw-Cypher
   surface as the default. (An optional, hidden Cypher-like capability for power
   use is fine later — just don't lead with it.)
6. **Bundled SQLite + FTS5** via `rusqlite` — own the SQLite build. *(Proven by
   the M0 test; CBM independently validates the bet.)*

---

## 1. Workspace layout (Cargo)

`codewalk_rust/` is the Cargo **workspace** root. Proposed crates (the existing
empty `codegraph_rust/` folder becomes the core crate or a nested folder of the
`codegraph-*` crates):

```
codewalk_rust/
├─ Cargo.toml                 # [workspace] members
├─ crates/
│  ├─ codegraph-core/         # extraction · db · resolution · graph · context  (lib)
│  ├─ codegraph-cli/          # `codegraph` binary: index/query/serve --mcp (mode 1)
│  ├─ codewalk-kb/            # embeddings · kb store · fusion · capture/distill (lib)
│  ├─ codewalk-cli/           # `codewalk` binary: full mode + install (mode 2)
│  └─ codegraph-node/         # napi-rs bindings → npm @codewalk/codegraph-native
└─ docs/
```

Why this split:
- `codegraph-core` knows nothing about embeddings or LLMs — it's the reusable,
  mode-agnostic moat. `codewalk-kb` depends on it; never the reverse.
- The two binaries map 1:1 to the two modes. `codegraph` links only the core;
  `codewalk` links core + kb.
- `codegraph-node` exposes the **core** to TS first (the KB layer can follow).

---

## 2. Dependency map (TS → Rust)

The whole point is to keep the *design* from the TS spec and swap the runtime.

| Concern | TS (spec) | Rust crate | Notes |
|---|---|---|---|
| AST parsing | web-tree-sitter (WASM) | `tree-sitter` + `tree-sitter-{go,python,javascript,typescript,rust,java,c,cpp,…}` | Native, no WASM. Watch grammar/ABI version skew. |
| Storage + FTS | `node:sqlite` / better-sqlite3 | **`rusqlite`** `features=["bundled","fts5"]` | **Kills the FTS5 cliff** — static SQLite with FTS5. |
| Embeddings | transformers.js (`Xenova/all-MiniLM-L6-v2`, 384-d) | **`fastembed`** (`AllMiniLML6V2`) | Same model/dims; wraps `ort`, downloads/caches weights. Alt: `candle` + `tokenizers`. |
| Vector search | brute-force cosine over Float32 blobs | plain slices / `ndarray`; later `sqlite-vec` (rusqlite `load_extension`) or `usearch`/`hnsw_rs` | MVP brute-force matches TS. |
| MCP server | `@modelcontextprotocol/sdk` | **`rmcp`** (official Rust MCP SDK) | stdio transport. |
| LLM distillation | `claude -p` (subprocess) | `std::process::Command` (`claude -p`); opt. `reqwest`+Anthropic API | Default `claude -p` for parity / no API key. |
| CLI | commander | `clap` | |
| Gitignore-aware walk | `ignore` (npm) | `ignore` (ripgrep crate) | Port `DEFAULT_IGNORE_DIRS` (`codegraph/src/extraction/index.ts:117`). |
| File watching (sync) | codegraph FileWatcher | `notify` | Incremental re-index. |
| Parallelism | worker threads | `rayon` (CPU) / `tokio` (MCP async) | Parallel extraction. |
| JSON / types | TS interfaces | `serde` / `serde_json` | |
| TS bindings | — | **`napi` + `napi-derive`** (napi-rs) | `.node` addon + prebuilds (napi CI templates). |

---

## 3. Reference map — TS spec → Rust module

When implementing a Rust module, read its TS counterpart first. **codegraph is
the moat; port behavior faithfully, validate against it (§5).**

### Structural core (`codegraph-core`) — spec is `../codewalk_kb/codegraph/src/`
| Rust module | TS spec file | Port notes |
|---|---|---|
| `db` (schema, queries) | `db/schema.sql`, `db/queries.ts`, `db/sqlite-adapter.ts` | Port `schema.sql` to a rusqlite migration verbatim (nodes/edges/files/unresolved_refs/nodes_fts + `edges.provenance`). Use rusqlite, **not** node:sqlite. |
| `types` (Node/Edge/Kind enums) | `types.ts` (`NODE_KINDS:18`, `EdgeKind:48`, `LANGUAGES:66`) | Rust enums + serde. |
| `extraction` (orchestrator, walk) | `extraction/index.ts` (orchestrator; `DEFAULT_IGNORE_DIRS:117`), `parse-worker.ts` | `ignore` crate + `rayon`. |
| `extraction::LanguageExtractor` trait | `extraction/tree-sitter-types.ts:80` | The ~40-hook contract → a Rust trait (see arch doc). |
| per-language extractors | `extraction/languages/*.ts` (19) + `*-extractor.ts` (7) | Port incrementally (Go first). |
| indirect call-refs | `extraction/function-ref.ts` | `functionRefProducers` hook. |
| `resolution` | `resolution/index.ts`, `name-matcher.ts`, `import-resolver.ts`, `path-aliases.ts` | Strategy chain. |
| `resolution::FrameworkResolver` trait | `resolution/types.ts:164`; registry `frameworks/index.ts:33` (24 integrations) | detect/resolve/claimsReference/extract/postExtract. |
| `resolution::synth` (20 channels) | `resolution/callback-synthesizer.ts` | `provenance:'heuristic'` + `synthesizedBy` + `registeredAt`. **"Partial coverage is worse than none"** — close flows end-to-end. |
| `graph` (traversal) | `graph/traversal.ts`, `graph/queries.ts` | callers/callees/impact/path. |
| `context` (markdown assembler) | `context/index.ts`, `context/formatter.ts` | `buildContext` output. |
| MCP tools + budgets | `mcp/tools.ts` (`getExploreBudget`/`getExploreOutputBudget`), `mcp/server-instructions.ts` | Scale explore output with file count; keep the "never tell the agent to Read" rule. |

### Learned layer (`codewalk-kb`) — spec is `../codewalk_kb/src/`
| Rust module | TS spec file | Port notes |
|---|---|---|
| `embeddings` | `embeddings/embedder.ts` | MiniLM-L6-v2, 384-d, mean-pool + L2-norm; cache `~/.codewalk/models`. → `fastembed`. |
| `store` (KB) | `kb/store.ts`, `kb/schema.sql`, `kb/types.ts` | rusqlite + `kb_fts` + Float32 embedding blobs; `searchVector` (cosine), `searchFts` (stopword-filtered). |
| `retrieval` (fusion) | `retrieval/fuse.ts`, `core.ts` | RRF (`RRF_K=60`), `MIN_KB_SCORE≈0.45`, `DISPLAY_SCORE≈0.3`, dedupe-merge `≥0.9`. |
| `capture` (miner+distill) | `capture/miner.ts`, `capture/distill.ts` | Transcript dir = `~/.claude/projects/<cwd→all-nonalnum-to-'-'>`; episodes by user-prompt→resolution; discovery-cost ranking; `claude -p` distill + heuristic fallback; watermark. |

### Facade / CLI / install — spec `../codewalk_kb/src/`
| Rust | TS spec | Notes |
|---|---|---|
| `codewalk-cli` core (ask/learn/update/status) | `core.ts` | The fused facade. |
| installer | `install/claude.ts` | **Install at LOCAL scope by default** (the hard-won lesson — project `.mcp.json` sits "pending approval"; local scope auto-trusts). |
| MCP server | `mcp/server.ts`, `mcp/server-instructions.ts` | Tool set per mode (§4). |

### Validation & context
- Benchmark harness to reuse/compare against: `../codewalk_kb/bench/` (`bench.mjs`,
  `charts.mjs`, `k8s/queryset.json`, `k8s/results.html`).
- Methodology: `../codewalk_kb/codegraph/CLAUDE.md` (the language×framework
  validation bar) + `codegraph/docs/design/*`.
- Strategy context: `../codewalk_kb/docs/launch-plan.md`, `../codegraph_arch.md`.

---

## 4. The two modes (MCP surface)

| | `codegraph` binary (mode 1) | `codewalk` binary (mode 2) |
|---|---|---|
| Links | `codegraph-core` | core + `codewalk-kb` |
| Needs | nothing (single binary) | + embedding model; + `claude`/API for distillation |
| MCP tools | structural: `search`, `node`, `explore`/`trace`, `context` (port `codegraph/src/mcp/tools.ts`) | adds `codewalk_ask` (fused), `codewalk_learn`, `codewalk_status` (port `codewalk_kb/src/mcp/server.ts`) |
| On a KB miss | n/a | degrades to structural — never worse than codegraph |

Both expose `serve --mcp --path <repo>` over stdio (rmcp). `codewalk` also has
`update` (mine transcripts) and `install` (wire into Claude Code, local scope).

---

## 5. Validation / parity methodology

Every Rust module is gated against the TS implementation **and** the benchmark —
we never guess at parity:

1. **Index parity** — index `../k8s` with the Rust core; compare
   `{fileCount, nodeCount, edgeCount}` and spot-checked `searchNodes`/`getNodesByName`
   results to the TS index (target: 8,898 files · 166,916 symbols · 592,479 edges,
   within tolerance as extractors are ported).
2. **Retrieval parity** — run the same `bench/k8s/queryset.json` recall eval; match
   structural-cold recall (~0.32) and, once the KB lands, fused recall (~0.87).
3. **Agent A/B** — `bench.mjs agent` against the Rust MCP server vs the TS one on
   the hero question; expect the same tool-call/latency/cost reduction.
4. **codegraph's own bar** (`CLAUDE.md`) for each new language×framework: small/
   medium/large real repos, ≥3 flow prompts, deterministic probes + agent A/B
   (≥2 runs), pass bar ~0 Read/Grep within the explore budget.
5. **Resolution precision** (new, from the CBM study) — track resolved/total refs
   and **single-candidate share** on Go k8s; the type-resolution pass (M4) must
   raise single-candidate share vs the name-matching baseline, with a manual
   spot-check of ~20 ambiguous receivers resolving correctly. FTS recall on
   camelCase queries must improve after the pre-split (M3).

The benchmark harness is JS and only needs the `.codegraph`/`.codewalk` SQLite +
a CLI/MCP endpoint, so it can drive the Rust binaries directly.

---

## 6. Phased milestones

Each milestone is independently shippable and benchmark-gated. **Port the
highest-value coverage first (Go → the k8s benchmark), then widen.** Items marked
**[insert]** are best-of-both additions from the competitive study, tagged with
their source (CBM / CG / CW).

### Milestone map at a glance

| Milestone | Status | Existing scope | Best-of-both inserts |
|---|---|---|---|
| M0–M2 | ✅ done | scaffold + FTS5 proof; Go extract+index+search; resolution + graph + synth | *(already mirrors CBM's extract→build→resolve spine with rayon)* |
| **M3 — MCP serve (mode 1)** | ✅ done | `rmcp` server, 4-tool surface, `buildContext`, explore budgets | camelCase FTS pre-split (CBM); explicit confidence ladder + candidate-count penalty in resolution (CBM) |
| **M4 — widen languages** | 🔨 in progress | **TS/JS extractors DONE** (Python, Rust next) + frameworks + synthesizers | hybrid table-driven + per-language-hook extractor model (CBM breadth × CG depth); **per-language type-resolution pass, Go first** (CBM precision) |
| **M4.5 — incremental + watch** | ⬜ [insert] | — | git-status + content-hash incremental indexing (CBM/CG); `notify` watcher (CG) |
| **M5 — learned layer (mode 2)** | ⬜ | `fastembed`, KB, RRF fusion, capture/distill, install | wire `markStale` on changed `ref_files` (fixes CW's inert staleness, reuses M4.5 hashing); miss-driven capture from `kb_query_log` (CW gap) |
| **M5.5 — overview/similarity** | ⬜ [insert] | — | Leiden communities + `get_architecture` (CBM); MinHash/LSH `SIMILAR_TO` (CBM) |
| **M6 — bindings + distribution** | ⬜ | napi-rs TS bindings; `curl \| sh` | single static binary + signed releases + SBOM + path containment + shell-arg validation (CBM security discipline) |

### M0 · Toolchain + scaffold ✅ DONE
Cargo workspace + 5 crate skeletons; `rusqlite` `bundled` with **FTS5 proven**
(`CREATE VIRTUAL TABLE … USING fts5` test passes on a stock toolchain);
`schema.sql` ported verbatim (nodes/edges/files/unresolved_refs/nodes_fts +
triggers + `edges.provenance`).

### M1 · Extraction MVP (Go) ✅ DONE
`LanguageExtractor` trait (partial) + Go extractor (`tree-sitter-go`), `ignore`
walk with `DEFAULT_IGNORE_DIRS`, node/edge model, store, FTS `bm25` search,
`rayon` parallel extraction. `node_id()` byte-compatible with the TS scheme.

### M2 · Resolution + graph (Go) ✅ DONE
Go cross-package import resolution (0.9) → name-matcher → 3 synthesizer passes
(cross-file `contains`, implicit `implements`, interface override —
`Heuristic`-tagged). `callers`/`callees`/`find_path` traversal. Verified
end-to-end on a live Go repo.

### M3 · MCP server, mode 1 (`codegraph` binary) ✅ DONE — the binary is agent-usable
The single highest-leverage step: the binary now serves an MCP surface and builds
context. `context/` (buildContext + explore), `search/query_utils` (lexical
ranking), the `name_split` FTS column + confidence ladder in resolution, and
`mcp.rs` (rmcp stdio server, 4 tools) all landed and are k8s-gated (see the status
banner above).
- **`buildContext` equivalent (CG)** — port `codegraph/src/context/index.ts` →
  `codegraph-core/src/context/`. Commodity lexical ranking (no vectors): identifier
  extraction → exact-name + co-location boost → stem/prefix variants → FTS per
  term → test-file dampening → hub (dominant-file) boost → multi-term co-occurrence
  re-rank → CamelCase-boundary LIKE → import→def resolution → graph expansion
  (type-hierarchy + BFS depth 1) → multi-stage token budget (maxNodes, per-file
  diversity cap, non-production ≤15%, edge recovery) → markdown with a `## Call
  paths` section annotating synthesized hops inline.
- **`rmcp` stdio server, 4-tool CG surface (CG)** — `codegraph_explore` (PRIMARY,
  symbol-bag→flow), `codegraph_node` (full body + caller/callee trail, every
  overload in one call), `codegraph_search`, `codegraph_callers`.
  **Success-shaped errors** (`isError` only for path refusals/real malfunctions);
  unindexed workspace → empty `tools/list` + "inactive" instructions;
  single-source guidance in MCP `initialize`; **size-scaled explore budget**
  (call-count + char/file monotonic with file count, under ~25K inline cap).
  *Decision:* default to CG's "indexing is the user's call" (not CBM auto-index);
  add opt-in `--auto-index`.
- **[insert] Resolution polish (CBM)** — in `name_matcher.rs`: explicit confidence
  ladder (import_map 0.95, same_module 0.90, import_map_suffix 0.85, unique_name
  0.75, suffix_match 0.55, fuzzy 0.40/0.30) stored on the edge; candidate-count
  penalty for high-fan-out names + test-path deprioritization; a **camelCase/snake
  pre-split scalar function** for FTS so `getUserById` matches `get`/`user`/`by`/`id`.
- *Done:* benchmark harness drives the Rust MCP; **structural-cold recall matches
  TS**; the k8s hero question (`scheduler-cycle`) answered in ≤1 explore call, 0
  Read/Grep; single binary runs with no Node and no FTS5 cliff.

### M4 · More languages (breadth without losing depth) 🔨
> **M4a done (2026-06-17): TypeScript + JavaScript extractors.** The hybrid model
> proved out — the engine's hook-driven dispatch needed only generalizing (arrow-
> from-declarator naming, `resolve_body`/`resolve_name`/`classify_method_node`/
> `import_module` hooks, object-of-functions extraction, generic TS variable +
> type-annotation + class-heritage paths). New `languages/typescript.rs` +
> `javascript.rs` (TSX/JSX reuse them). Grammars: `tree-sitter-typescript` 0.23 +
> `tree-sitter-javascript` 0.25 (ABI-compatible with the 0.25 runtime, smoke-tested).
> **Parity on `codewalk_kb/codegraph/src` (135 TS files): nodes EXACT 2,925 = 2,925**
> (every kind within ±2: function 877✓, method 583✓, import 510✓, constant 366✓,
> interface 120✓, class 55✓). Edges 8,796 vs 10,740 (82%) — `contains` 2,790✓,
> `calls` 4,278/4,284 (99.9%), `extends`/`implements` exact. The edge gap is **TS
> import resolution** (per-symbol named imports, −959) + fuller type-reference
> coverage (−974) — the resolution follow-on, not the extractor. **Go re-verified
> byte-identical** (8,751 / 167,101 / 592,456), so the generalization was non-regressing.
> Next: TS import resolution to close the edge gap, then Python → Rust.

- **Hybrid extractor model (CBM breadth × CG depth)** — a generic
  node-type-table-driven core (`extraction/engine.rs`, exists) driven by
  per-language `LanguageSpec` data tables (cheap breadth, CBM-style: a quirk-free
  language = one data table) + an optional per-language `LanguageExtractor` trait
  widened toward CG's ~40 hooks (`pre_parse`, `classify_class_node`,
  `get_receiver_type`, `get_return_type`, `classify_method_node`, `visit_node`).
- Port order by ROI: **TS/JS → Python → Rust → Java → C/C++**.
- **[insert] Per-language type-resolution pass (CBM — biggest precision lever).**
  After base extraction, a bounded per-language type pass resolving what
  name-matching can't: **Go first** (we already infer receiver types — build a
  `(receiver_type, method_name)→def` index), then C/C++ (pointer indirection,
  implicit `this`) and TS (typed receivers, overloads). Model on CBM's
  `CBMTypeRegistry`/`score_overload_match` but scoped — start with receiver +
  overload resolution (most of the gain); admit type-resolved edges ahead of the
  name cascade (CBM's 0.6 confidence floor), provenance-tagged.
- **Framework resolvers + synthesizers (CG moat)** — port incrementally,
  ROI-ordered, each gated by the §5 bar. Synthesizers first (they connect flows
  that break): interface-impl, event-emitter, react-render, jsx-render,
  flutter-build, cpp-override. Then frameworks (Express/NestJS, Django/Flask/
  FastAPI, Rails, Spring) emitting `route`/`component` nodes + `claimsReference()`.
- *Done:* per-language node/edge counts within ±5% of TS CodeGraph on a shared
  repo; the `add-lang` skill's 3-repo validation passes; Go single-candidate
  call-resolution share rises vs the name-matching baseline.

### M4.5 · Incremental indexing + watch ⬜ [insert] (CBM/CG)
Today the port does full re-index only (`index_repo` always `clear()`s;
`files.content_hash` exists but is unused). Table stakes for "stays installed."
- **Change detection** — `git status --porcelain` + `git rev-parse HEAD` (CBM's
  cheap portable approach) and/or per-file **sha256+mtime+size** diff vs the
  `files` table; re-extract only changed files; **scope resolution to refs from
  changed files** (CG's `getUnresolvedReferencesByFiles`).
- **Watcher** — the `notify` crate, debounced ~2s (CG), off by default.
- **Staleness banner** in MCP responses for pending files (CG).
- *Done:* editing 1 file in k8s re-indexes in <2s; full-index result unchanged vs
  from-scratch.

### M5 · Learned layer, mode 2 (`codewalk` binary) ⬜ — the half CBM lacks
`codewalk-kb`: `fastembed` MiniLM-L6-v2 (384-d, pre-warmed), KB store
(`.codewalk/kb.db`, schema ported verbatim), fusion (`searchVector(8)` ⊕
`searchFts(8)` → RRF k=60 + 0.1·confidence → always also fetch structural
`buildContext` → miss <0.45 → render `## Learned knowledge` + `## Structural
context`, **never worse than structural**), capture (transcript miner +
discovery-cost ranking + `claude -p` distill + heuristic fallback + dedupe ≥0.9 +
watermark), `codewalk_ask`/`learn`/`status` tools, `update`, `install`
(**local scope** by default — the hard-won lesson).
- **[insert] Fix CW's known gaps while porting:** wire **`markStale`** (CW's is
  inert) — on incremental re-index (M4.5), diff a KB entry's `ref_files` mtimes vs
  `created_at`, set `stale=1`, surface the "⚠️ may be outdated" flag (already
  rendered); **miss-driven capture** — consult `kb_query_log` misses in `update()`
  to prioritize mining; swap brute-force cosine for **`sqlite-vec`** once the KB
  grows past low-thousands.
- *Done:* reproduce the k8s result from the Rust binary — structural ~0.32 → fused
  ~0.87; hero question 5× fewer tool calls.

### M5.5 · Overview & similarity ⬜ [insert] (CBM) — *medium priority*
Powers onboarding / "explain this subsystem" / "where are the duplicates" answers
our stack can't currently give; well-bounded ports.
- **Leiden community detection** (CBM uses Leiden, better than the paper's Louvain)
  over the CALLS graph → community nodes + a `get_architecture`-style overview
  tool; cap ~8000 nodes for interactivity.
- **MinHash + LSH** over normalized AST leaf trigrams (XXH3 via `xxhash-rust`),
  Jaccard ≥0.95 → `SIMILAR_TO` edges for clone detection.
- *Done:* `get_architecture` on k8s yields coherent subsystem clusters; MinHash
  finds known duplicate handlers.

### M6 · TS bindings + distribution ⬜ (CBM security discipline)
`codegraph-node` (napi-rs) → publish `@codewalk/codegraph-native` with prebuilds;
optionally swap the TS Codewalk's `@colbymchenry/codegraph` dependency for it.
`curl | sh` single-binary install for both binaries.
- **[insert] Security discipline to adopt from CBM** (cheap, high-trust):
  `canonicalize` **path containment** on the code-snippet tool (refuse outside
  project root); **shell-arg validation** before any `claude -p`/git shell-out
  (reject metacharacters — matters since CW reads `~/.claude` transcripts);
  **signed releases** (cosign/sigstore) + **SBOM** (CycloneDX) + checksums;
  **transcript-privacy gate** (opt-in mining, secret redaction, local-only).

> **Prioritized "do this next" (by leverage):** M3 (MCP + buildContext) → M4 TS/JS
> + Python → M4.5 incremental → M5 learned layer → M4 Go type-resolution pass →
> M4 synthesizer/framework port → M6 distribution+security → M5.5 Leiden+MinHash.
> M0–M2 are done; the high-confidence near-term goal is **M3 (Go-only codegraph in
> Rust, FTS5-clean, agent-usable, benchmarked)**.

---

## 7. Risks & mitigations

| Risk | Mitigation |
|---|---|
| **Porting the moat is huge** (26 extractors + 24 frameworks + 20 synthesizers). | Phase by value (Go→TS→Python…), gate each on the benchmark, keep TS codegraph as the spec + fallback. Don't big-bang. |
| **tree-sitter grammar/ABI skew** across the `tree-sitter` crate + grammar crates. | Pin versions in the workspace; smoke-test each grammar on a known file in M1. |
| **Embedding model download/size** (~90 MB) — same friction as TS. | `fastembed` caches under `~/.codewalk/models`; consider bundling for offline; mode 1 needs no model at all. |
| **napi prebuild matrix** (per OS/arch). | Use napi-rs CI templates; ship prebuilds; build-from-source fallback. |
| **LLM distillation availability** (`claude -p` vs API). | Default `claude -p` (no key, parity with TS); `reqwest`+API as opt-in; heuristic fallback always. |
| **Behavioral drift from codegraph** as we port. | The §5 parity gates + the `CLAUDE.md` methodology catch regressions against real numbers. |

---

## 8. First-session checklist (M0) — ✅ HISTORICAL (done)

*This was the M0 bootstrap checklist; M0–M2 are complete. To start the **next**
session (M3 onward), use [`kickoff-prompt-m3.md`](kickoff-prompt-m3.md) instead.*

1. Install Rust: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
   (Rust is **not** installed on this machine yet).
2. `cargo new --lib` the workspace + 5 crates per §1; root `Cargo.toml [workspace]`.
3. Add `rusqlite = { version = "*", features = ["bundled", "fts5"] }` to
   `codegraph-core`; write a test that creates the schema (port
   `../codewalk_kb/codegraph/src/db/schema.sql`) and an FTS5 virtual table — this
   is the proof the cliff is gone.
4. Add `tree-sitter` + `tree-sitter-go`; parse one k8s file; assert it yields
   function/struct nodes.
5. Read [`codewalk_rust_arch.md`](codewalk_rust_arch.md) for the trait/type shapes,
   and [`../codegraph_arch.md`](../codegraph_arch.md) for what each layer must cover.
