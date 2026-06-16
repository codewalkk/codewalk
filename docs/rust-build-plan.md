# Codewalk-in-Rust — Build Plan

*Plan for a Rust-native re-implementation of Codewalk (codegraph core + the
learned-intelligence layer) under `../codewalk_rust/`. Written from the working
TypeScript implementation in `../codewalk_kb/`, which is the **behavioral spec**.
We start building in a fresh session — this doc + [`codewalk_rust_arch.md`](codewalk_rust_arch.md)
+ [`../codegraph_arch.md`](../codegraph_arch.md) are the entry points.*

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
tail, §6). Browser/WASM target (napi/Node first).

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

The benchmark harness is JS and only needs the `.codegraph`/`.codewalk` SQLite +
a CLI/MCP endpoint, so it can drive the Rust binaries directly.

---

## 6. Phased milestones

Each milestone is independently shippable and benchmark-gated. **Port the
highest-value coverage first (Go → the k8s benchmark), then widen.**

- **M0 · Toolchain + scaffold (½–1 day).** `rustup` install (Rust is not yet on
  this box). Cargo workspace + the 5 crate skeletons. Wire `rusqlite`
  `bundled,fts5` and **prove FTS5 works** (`CREATE VIRTUAL TABLE … USING fts5`).
  Port `schema.sql` → migration. *Done:* `cargo test` creates the schema + an FTS5
  table on a stock toolchain (no Node, any OS).
- **M1 · Extraction MVP (Go).** `LanguageExtractor` trait + Go extractor
  (`tree-sitter-go`), file walk (`ignore`), node/edge model, store, FTS search,
  `rayon` parallelism. *Done:* `codegraph index ../k8s` produces node/edge/file
  counts within tolerance of the TS index (§5.1).
- **M2 · Resolution + graph (Go).** Import resolution, name-matcher, the
  Go-relevant synthesizers (`go-implements`, `interface-impl`,
  `gin-middleware-chain`, `go-grpc-stub-impl`), graph traversal
  (callers/callees/impact/path), context builder. *Done:* explore/trace connects
  the scheduler flow end-to-end (parity with TS on the hero question).
- **M3 · MCP server, mode 1 (`codegraph` binary).** `rmcp` stdio server +
  structural tools + explore budgets + `server-instructions`. *Done:* the
  benchmark harness drives the Rust MCP; structural-cold recall matches TS;
  **single binary runs with no Node and no FTS5 cliff.**
- **M4 · More languages.** TS/JS, Python, Rust extractors + their framework
  resolvers + synthesizers, each validated by the §5 bar. *Done:* parity on at
  least one non-Go benchmark repo.
- **M5 · Learned layer, mode 2 (`codewalk` binary).** `codewalk-kb`: `fastembed`
  MiniLM, KB store (rusqlite + cosine), fusion (RRF + structural), capture
  (miner + `claude -p` distill), `codewalk_ask/learn/status` tools, `update`,
  `install` (local scope). *Done:* full retrieval + agent A/B match the TS k8s
  results (0.32→0.87; the multiplier chart).
- **M6 · TS bindings + distribution.** `codegraph-node` (napi-rs) → publish
  `@codewalk/codegraph-native` with prebuilds; optionally swap the TS Codewalk's
  `@colbymchenry/codegraph` dependency for it (retrofits the FTS5 fix + perf into
  the TS product). `curl | sh` single-binary install for both binaries.

> Realistic expectation: M0–M3 (Go-only codegraph in Rust, FTS5-clean, benchmarked)
> is the high-confidence near-term goal. M4–M5 widen coverage and add the learned
> layer; M6 is the distribution/integration payoff. Full codegraph-parity across
> all 30+ languages is a long tail — keep the TS codegraph as the reference and,
> if useful, as a fallback during the transition.

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

## 8. First-session checklist (M0)

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
