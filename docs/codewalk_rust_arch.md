# codewalk_rust — Architecture

*The target architecture for the Rust-native Codewalk. Companion to
[`rust-build-plan.md`](rust-build-plan.md) (how/when) and
[`../codegraph_arch.md`](../codegraph_arch.md) (the moat we're porting). The
TypeScript implementation in `../codewalk_kb/` is the behavioral spec — paths
below point at it.*

---

## 1. The shape in one picture

```
                          ┌───────────────────────── codewalk_rust (cargo workspace) ─────────────────────────┐
                          │                                                                                    │
   Claude Code ──MCP──▶  codegraph (bin, MODE 1)            codewalk (bin, MODE 2) ◀──MCP── Claude Code        │
                          │      │                                  │                                          │
                          │      ▼                                  ▼                                          │
                          │  ┌───────────────┐              ┌───────────────┐                                 │
                          │  │ codegraph-core│◀─────────────│  codewalk-kb  │   (kb depends on core,           │
                          │  │   (lib)       │              │    (lib)      │    never the reverse)            │
                          │  └──────┬────────┘              └──────┬────────┘                                 │
                          │   extraction·db·resolution·       embeddings·kb-store·fusion·capture               │
                          │   graph·context                                                                    │
                          │      │                                                                             │
                          │      └─▶ codegraph-node (napi-rs) ─▶ @codewalk/codegraph-native (npm) ─▶ TS apps   │
                          └────────────────────────────────────────────────────────────────────────────────────┘
                                     │                                  │
                              .codegraph/graph.db                 .codewalk/kb.db
                              (rusqlite + FTS5)                   (rusqlite + FTS5 + vector blobs)
```

- **`codegraph-core`** is the structural engine and the reusable moat. It has no
  knowledge of embeddings or LLMs.
- **`codewalk-kb`** is the optional learned-intelligence layer; it depends on
  core, adds embeddings + a knowledge base + fusion + transcript capture.
- **Mode 1 (`codegraph`)** links only core → structural MCP tools, single binary,
  no model, no LLM, no network.
- **Mode 2 (`codewalk`)** links core + kb → adds the learned tools; on a KB miss
  it degrades to structural (never worse than codegraph).
- **`codegraph-node`** exposes core to TS/JS via napi-rs.

Storage is two SQLite databases, both `rusqlite` with `features=["bundled","fts5"]`
— FTS5 is statically linked, so there is **no Node-version FTS5 cliff** (§7).

---

## 2. `codegraph-core` — the structural engine

Mirrors codegraph's layered pipeline ([`../codegraph_arch.md`](../codegraph_arch.md) §1).
Spec: `../codewalk_kb/codegraph/src/`.

```
files ─▶ extraction (tree-sitter, rayon) ─▶ db (nodes/edges/files, FTS5)
            └─▶ resolution (imports · name-match · frameworks · synthesizers)
                  └─▶ graph (callers/callees/impact/path)
                        └─▶ context (markdown/json assembler)
```

### 2.1 Types (`core::types`) — spec `codegraph/src/types.ts`
Port the two enums verbatim — every language normalizes onto them:

```rust
pub enum NodeKind { File, Module, Class, Struct, Interface, Trait, Protocol,
    Function, Method, Property, Field, Variable, Constant, Enum, EnumMember,
    TypeAlias, Namespace, Parameter, Import, Export, Route, Component }

pub enum EdgeKind { Contains, Calls, Imports, Exports, Extends, Implements,
    References, TypeOf, Returns, Instantiates, Overrides, Decorates }

pub struct Node { pub id: String, pub kind: NodeKind, pub name: String,
    pub qualified_name: String, pub file_path: String, pub language: Language,
    pub start_line: u32, pub end_line: u32, /* signature, docstring, flags… */ }

pub struct Edge { pub source: String, pub target: String, pub kind: EdgeKind,
    pub provenance: Option<Provenance>,   // Some(Heuristic) for synthesized edges
    pub metadata: Option<serde_json::Value> /* synthesizedBy, registeredAt … */ }
```

### 2.2 Storage (`core::db`) — spec `codegraph/src/db/`
- `rusqlite` (`bundled`, `fts5`). Port `db/schema.sql` to a migration verbatim:
  `nodes`, `edges` (incl. `provenance` column + `idx_edges_provenance`), `files`,
  `unresolved_refs`, and the `nodes_fts` FTS5 virtual table.
- A `Store` type wrapping prepared statements (the `QueryBuilder` analog from
  `db/queries.ts`): node/edge CRUD, FTS `search`, graph helpers.
- DB lives at `<repo>/.codegraph/graph.db`.

### 2.3 Extraction (`core::extraction`) — spec `codegraph/src/extraction/`
The `LanguageExtractor` trait is the port of `tree-sitter-types.ts:80` (~40
hooks). Sketch:

```rust
pub trait LanguageExtractor {
    fn language(&self) -> Language;
    fn pre_parse(&self, src: &str) -> Option<String> { None }   // byte-offset-preserving fixups (C#)
    // node-type → concept mappings
    fn function_types(&self) -> &[&str];
    fn class_types(&self) -> &[&str];
    fn method_types(&self) -> &[&str];
    fn call_types(&self) -> &[&str];
    // … import/variable/field/enum/type-alias types, field-name mappings …
    // disambiguation + language-specific hooks (return None to fall through):
    fn classify_class_node(&self, _n: Node<'_>) -> Option<ClassKind> { None } // Swift
    fn classify_method_node(&self, _n: Node<'_>) -> Option<MemberKind> { None } // TS fields
    fn get_receiver_type(&self, _n: Node<'_>, _src: &str) -> Option<String> { None } // Go
    fn get_return_type(&self, _n: Node<'_>, _src: &str) -> Option<String> { None }   // C/C++ chained calls
    fn visit_node(&self, _n: Node<'_>, _ctx: &mut Ctx) -> bool { false }             // Pascal
}
```

- Orchestrator (`extraction/index.ts` analog): `ignore`-crate walk respecting
  `.gitignore` + a ported `DEFAULT_IGNORE_DIRS` (`extraction/index.ts:117`),
  parse with `tree-sitter` (+ grammar crates), extract into nodes/edges +
  `unresolved_refs`. Parallelize files with `rayon`.
- Per-language extractors live in `core::extraction::languages::{go, typescript,
  python, rust, …}` (port `languages/*.ts`); template extractors
  (`*-extractor.ts`) follow.
- Indirect call-refs: a `function_ref` producer hook (`function-ref.ts`).

### 2.4 Resolution (`core::resolution`) — spec `codegraph/src/resolution/`
- `ReferenceResolver`: strategy chain (exact → import → qualified → name-match →
  **framework** → **synthesis**). Spec: `resolution/index.ts`, `name-matcher.ts`,
  `import-resolver.ts`, `path-aliases.ts`.
- `FrameworkResolver` trait — port `resolution/types.ts:164` + the 24-integration
  registry (`frameworks/index.ts:33`):

```rust
pub trait FrameworkResolver {
    fn name(&self) -> &str;
    fn languages(&self) -> Option<&[Language]> { None }
    fn detect(&self, ctx: &ResolutionContext) -> bool;
    fn resolve(&self, r: &UnresolvedRef, ctx: &ResolutionContext) -> Option<ResolvedRef>;
    fn claims_reference(&self, _name: &str) -> bool { false }     // let dynamic names through
    fn extract(&self, _path: &str, _content: &str) -> Option<FrameworkExtraction> { None } // route/component nodes
    fn post_extract(&self, _ctx: &ResolutionContext) -> Vec<Node> { vec![] }                // cross-file finalize
}
```

- **Synthesizers** (`core::resolution::synth`) — port the 20 channels from
  `callback-synthesizer.ts`. Each is a whole-graph pass emitting edges with
  `provenance = Heuristic` and `metadata { synthesizedBy, via, registeredAt }`.
  Keep the two invariants: provenance-tag every synthesized edge, and **close
  flows end-to-end** ("partial coverage is worse than none").

### 2.5 Graph (`core::graph`) & Context (`core::context`)
- Traversal (BFS/DFS, callers/callees, impact radius, path) — `graph/traversal.ts`.
- Context assembler producing the markdown/json the agent consumes —
  `context/index.ts`, `context/formatter.ts`.

---

## 3. `codewalk-kb` — the learned-intelligence layer

Optional layer on top of core. Spec: `../codewalk_kb/src/`.

- **`embeddings`** (`embedder.ts`) → `fastembed` `AllMiniLML6V2` (384-d,
  mean-pool + L2-norm, cosine == dot). Cache `~/.codewalk/models`. Lazy-load +
  pre-warm at server start.
- **`store`** (`kb/store.ts`, `kb/schema.sql`, `kb/types.ts`) → `rusqlite` KB at
  `<repo>/.codewalk/kb.db`: `kb_entries` (topic, body, provenance, `ref_*`,
  confidence, **embedding BLOB** Float32, usage stats), `kb_fts` (stopword-filtered
  lexical), `kb_meta` (watermark). `search_vector` (brute-force cosine for MVP),
  `search_fts`, `record_hit`, dedupe-merge.
- **`retrieval`** (`retrieval/fuse.ts` + `core.ts`) → reciprocal-rank fusion of
  vector + FTS hits (`RRF_K=60`), ranked with confidence + usage; below
  `MIN_KB_SCORE≈0.45` (and no strong FTS) = a miss → structural-only answer.
  Render `## Learned knowledge` + `## Structural context`.
- **`capture`** (`capture/miner.ts`, `capture/distill.ts`) → mine
  `~/.claude/projects/<cwd-with-every-nonalnum→'-'>/**/*.jsonl`, segment into
  query→resolution episodes, rank by discovery cost, distill via `claude -p`
  (subprocess; heuristic fallback), dedupe (embedding ≥0.9 merge), advance a
  watermark (idempotent).

`KbEntry` mirrors `kb/types.ts` (id, type ∈ {qa,insight,deepresearch,flow}, topic,
body, ref_files/symbols/node_ids, confidence, embedding, embed_model, usage).

---

## 4. Binaries & modes

### `codegraph` (Mode 1) — `crates/codegraph-cli`
Links `codegraph-core` only. Commands: `index`, `query`/`search`, `serve --mcp`.
MCP tools (port `codegraph/src/mcp/tools.ts`): structural `search`, `node`,
`explore`/`trace`, `context` — with the explore budgets
(`getExploreBudget`/`getExploreOutputBudget`) scaled by file count, and the
"never tell the agent to Read" output rule. No model, no LLM, no network.

### `codewalk` (Mode 2) — `crates/codewalk-cli`
Links core + `codewalk-kb`. The fused facade (`core.ts` analog: `ask`, `learn`,
`update`, `status`). Commands add `update` (mine transcripts) and `install`.
MCP tools add `codewalk_ask` (fused), `codewalk_learn`, `codewalk_status`
(port `codewalk_kb/src/mcp/server.ts` + `server-instructions.ts`).

**Installer** (`install/claude.ts` analog): register the MCP server at **local
scope by default** (the lesson from the TS build — a project `.mcp.json` sits in
Claude Code as "⏸ pending approval"; local scope is auto-trusted). `--project`
and `--global` are opt-ins.

Both use `rmcp` over stdio. Consider a pre-warmed/daemonized serve so the first
tool call isn't blocked on index-open + model-warm.

---

## 5. `codegraph-node` — TS/JS bindings (napi-rs)

`napi` + `napi-derive` expose `codegraph-core` as a Node native module
(`@codewalk/codegraph-native`), surfacing the `CodeGraph` facade shape the TS
side already expects (`../codewalk_kb/src/structural/codegraph-adapter.ts` is the
consumer contract): `open`, `indexAll`, `searchNodes`, `getCallers`/`getCallees`,
`getImpactRadius`, `getNodesByName`, `buildContext`, `getStats`.

Two payoffs:
1. The existing **TS Codewalk** can swap its `@colbymchenry/codegraph` dependency
   for this native core — killing the FTS5 cliff and the WASM-parse cost in the
   TS product while keeping the TS learned layer.
2. Anyone can **build a JS/TS app** on `codegraph_rust`.

(The `codewalk-kb` layer can get napi bindings later; core comes first. WASM/
browser is a possible third target but secondary — SQLite-in-WASM is the wrinkle.)

---

## 6. Storage model summary

| DB | Path | Backend | Holds |
|---|---|---|---|
| structural | `<repo>/.codegraph/graph.db` | rusqlite (bundled, fts5) | nodes, edges (+ provenance), files, unresolved_refs, `nodes_fts` |
| learned | `<repo>/.codewalk/kb.db` | rusqlite (bundled, fts5) | `kb_entries` (+ embedding blobs), `kb_fts`, `kb_meta` |

Both schemas are direct ports of the TS `schema.sql` files. Keeping two DBs
(structural vs learned) means re-indexing code never wipes learned knowledge —
same separation as the TS build.

---

## 7. Why this removes the FTS5 cliff (and what it doesn't change)

The cliff was never about the language — it was `node:sqlite` linking whatever
SQLite Node shipped (FTS5 only on Node 24+). `rusqlite` with `bundled,fts5`
**compiles SQLite with FTS5 into the binary**, so FTS5 is always present, on any
platform, with zero runtime dependency. This is the same reason the TS KB never
hit the cliff (it uses better-sqlite3). The Rust core inherits that property for
the *structural* store too — and ships as one static binary.

What Rust does **not** change: the moat is still the extractors + framework
resolvers + synthesizers (`../codegraph_arch.md`). Rust gives us a clean SQLite
build, a single binary, speed, and TS bindings — but the intelligence still has
to be ported, faithfully and benchmark-gated (see
[`rust-build-plan.md`](rust-build-plan.md) §5–6).

---

## 8. Crate dependency rules

- `codegraph-core` depends on no other workspace crate. It is the moat and the
  reusable unit.
- `codewalk-kb` → depends on `codegraph-core`. Never the reverse.
- `codegraph-cli` → core. `codewalk-cli` → core + kb.
- `codegraph-node` → core (+ napi). KB napi bindings optional/later.
- Keep LLM/embedding/network concerns **out** of core so Mode 1 stays a pure,
  offline, deterministic single binary.
