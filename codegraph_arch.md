# codegraph — Architecture & Moat

*Reference for the Codewalk team. All paths are relative to the `codegraph/`
submodule. Line numbers are from the pinned commit in this repo; treat them as
"near here," not exact across upstream versions.*

codegraph is the **structural** half of Codewalk: it parses any supported
codebase with tree-sitter, stores symbols + edges in SQLite (FTS5), resolves
references (including dynamic ones), and serves a queryable graph to coding
agents over MCP. Extraction is **deterministic** — derived from the AST, never
LLM-summarized.

This document covers the pipeline, then makes the case for where the real value
lives: **the per-grammar extractors, the framework resolvers, and the
dynamic-dispatch synthesizers** — three layers of accumulated, real-repo-tuned
coverage that are expensive to reproduce.

---

## 1. The pipeline

```
files
  └─▶ ExtractionOrchestrator (tree-sitter, in a worker)        src/extraction/
        └─▶ DB: nodes / edges / files  (SQLite + FTS5)         src/db/
              └─▶ ReferenceResolver                            src/resolution/
                    (imports, name-matching, frameworks,
                     dynamic-dispatch synthesis)
                    └─▶ GraphTraverser / GraphQueryManager      src/graph/
                          (callers, callees, impact, paths)
                          └─▶ ContextBuilder (markdown/JSON)    src/context/
                                └─▶ MCPServer (tools)           src/mcp/
```

The public surface is the `CodeGraph` facade (`src/index.ts`) — `init`/`open`,
`indexAll`, `sync`, `searchNodes`, `getCallers`/`getCallees`, `getImpactRadius`,
`buildContext`. Library users (including Codewalk) touch only this file.

### Core data model — `src/types.ts`

Everything normalizes to two enums, so 30+ languages share one query surface:

- **`NODE_KINDS`** (`src/types.ts:18`): `file, module, class, struct, interface,
  trait, protocol, function, method, property, field, variable, constant, enum,
  enum_member, type_alias, namespace, parameter, import, export, route, component`.
- **`EdgeKind`** (`src/types.ts:48`): `contains, calls, imports, exports, extends,
  implements, references, type_of, returns, instantiates, overrides, decorates`.
- **`LANGUAGES`** (`src/types.ts:66`): 30+ entries (typescript, tsx, python, go,
  rust, java, c, cpp, csharp, php, ruby, swift, kotlin, dart, svelte, vue, astro,
  liquid, razor, pascal, scala, lua, luau, objc, r, …).

### Storage — `src/db/`

- `schema.sql` — `nodes`, `edges`, `files`, `unresolved_refs`, and the
  `nodes_fts` FTS5 virtual table. Note `edges.provenance` (`schema.sql:53`) +
  `idx_edges_provenance` (`:145`) — the column that flags synthesized edges (§4).
- `queries.ts` — prepared-statement `QueryBuilder` (node/edge CRUD, FTS search,
  graph helpers).
- `sqlite-adapter.ts` — thin wrapper over Node's built-in `node:sqlite`. *(This
  is the source of Codewalk's "FTS5 cliff": `node:sqlite` only ships FTS5 on Node
  24+. Our own `.codewalk/kb.db` avoids it by using better-sqlite3.)*

The algorithms above this line — FTS ranking, BFS/DFS traversal, impact radius —
are **commodity**. The moat is below.

---

## 2. Moat, part 1 — the language extractors

**26 extractor modules** turn 30+ grammars into the uniform node/edge model:

- **19 tree-sitter language extractors** — `src/extraction/languages/`:
  `c-cpp, csharp, dart, go, java, javascript, kotlin, lua, luau, objc, pascal,
  php, python, r, ruby, rust, scala, swift, typescript` (each file often covers
  several `LANGUAGES` entries — `typescript.ts` → ts/tsx, `c-cpp.ts` → c/cpp).
- **7 non-tree-sitter / template extractors** — `src/extraction/`:
  `svelte-extractor, vue-extractor, astro-extractor, liquid-extractor,
  razor-extractor, dfm-extractor` (Delphi forms), `mybatis-extractor` (Java XML).

### Why this is hard to copy: the `LanguageExtractor` contract

`src/extraction/tree-sitter-types.ts:80` defines ~40 hooks. It is not "run
tree-sitter and read the names" — it's a per-grammar adapter encoding that
grammar's idiosyncrasies:

```ts
// src/extraction/tree-sitter-types.ts:80
export interface LanguageExtractor {
  // Source transform to work around grammar gaps, byte-offset preserving
  preParse?: (source: string) => string;
  // Node-type → concept mappings (each grammar names things differently)
  functionTypes: string[]; classTypes: string[]; methodTypes: string[];
  interfaceTypes: string[]; structTypes: string[]; enumTypes: string[];
  typeAliasTypes: string[]; importTypes: string[]; callTypes: string[];
  variableTypes: string[]; fieldTypes?: string[]; propertyTypes?: string[];
  // Field-name mappings
  nameField: string; bodyField: string; paramsField: string; returnField?: string;
  // Disambiguation hooks for grammars that reuse one node type for many concepts
  classifyClassNode?: (n) => 'class'|'struct'|'enum'|'interface'|'trait';
  classifyMethodNode?: (n) => 'method'|'property';
  // Language-specific extraction the resolver later depends on
  getReceiverType?: (n, src) => string|undefined;   // Go: func (s *T) m()
  getReturnType?:   (n, src) => string|undefined;    // C/C++ chained-call inference
  extractModifiers?: (n) => string[]|undefined;       // Kotlin expect/actual
  visitNode?: (n, ctx) => boolean;                    // Pascal's different AST
  // … ~25 more
}
```

The value is the **edge cases each extractor encodes** — every one paid for by a
real broken parse. From the interface docs and extractors:

- **C#** `preParse` blanks conditional-compilation directive lines the grammar
  mis-parses inside enum bodies — *preserving byte offsets* so node positions
  stay correct (`tree-sitter-types.ts:82-89`).
- **Swift** reuses `class_declaration` for classes, structs, *and* enums →
  `classifyClassNode` (`:177-181`).
- **TS/JS** class fields are methods only when their value is callable
  (`onClick = () => {}`) vs a plain property (`count = 0`) → `classifyMethodNode`
  (`:183-190`).
- **Go** methods are top-level and carry a receiver → `methodsAreTopLevel: true`
  + `getReceiverType` (`languages/go.ts:52, :89`), so `func (sl *scrapeLoop) run()`
  becomes searchable as `scrapeLoop.run` and resolvable later.
- **C/C++** `getReturnType` lets resolution infer a chained receiver's type
  (`Foo::instance().bar()` → resolve `bar` on `Foo`, issue #645) (`:217-223`).
- **Dart** puts `function_body` as a sibling, not a child → `resolveBody`
  (`:192-196`). **Pascal** needs a full custom `visitNode` (`:171-175`).

Parsing is offloaded to a worker (`src/extraction/parse-worker.ts`) and recycled
to bound the WASM heap. None of that is the moat — the **30+ grammar adapters,
each carrying a tail of grammar-specific corrections**, are.

---

## 3. Moat, part 2 — the framework resolvers

After base extraction, `ReferenceResolver` (`src/resolution/index.ts`) walks the
`unresolved_refs` table through a strategy chain (exact-match → import →
qualified-name → name-matcher → **framework** → synthesis). The framework layer
is **21 resolver modules registering 24 framework integrations**
(`src/resolution/frameworks/index.ts:33`):

```ts
// src/resolution/frameworks/index.ts:33
const FRAMEWORK_RESOLVERS: FrameworkResolver[] = [
  laravelResolver, drupalResolver,                                   // PHP
  expressResolver, nestjsResolver, reactResolver, svelteResolver,    // JS/TS
  vueResolver, astroResolver,
  djangoResolver, flaskResolver, fastapiResolver,                    // Python
  railsResolver,                                                     // Ruby
  springResolver, playResolver,                                      // Java
  goResolver, rustResolver, aspnetResolver,                          // Go / Rust / C#
  swiftUIResolver, uikitResolver, vaporResolver,                     // Swift
  swiftObjcBridgeResolver,                                           // Swift ↔ ObjC
  reactNativeBridgeResolver, expoModulesResolver, fabricViewResolver,// RN
];
```

### Why this is hard to copy: the `FrameworkResolver` contract

`src/resolution/types.ts:164`:

```ts
export interface FrameworkResolver {
  name: string;
  languages?: Language[];
  detect(context): boolean;                       // project-level, once at startup
  resolve(ref, context): ResolvedRef | null;      // framework-specific patterns
  claimsReference?(name): boolean;                // opt a dynamic name past the
                                                  // name-exists pre-filter
  extract?(filePath, content): FrameworkExtractionResult; // emit route/component
                                                  // nodes + handler refs
  postExtract?(context): Node[];                  // cross-file finalization
}
```

What each resolver buys you:

- **New node kinds the AST has no concept of.** `extract()` emits `route` and
  `component` nodes (the only two `NodeKind`s that don't exist in source) and
  links them to handlers — Express/NestJS/Django/Flask/FastAPI/Rails/Spring/Play
  request→handler→view chains.
- **`claimsReference()`** lets a *dynamic* call name (Django's
  `self._iterable_class(...)`, a React effect callback) survive the
  "no symbol is named this" filter and reach `resolve()` instead of being dropped
  (`types.ts:173-180`). This is the hook that makes framework magic resolvable at
  all.
- **`postExtract()`** handles cross-file truth a per-file pass can't see — e.g.
  NestJS's `RouterModule.register([...])` setting route prefixes for controllers
  declared in other files, idempotently (`types.ts:190-203`).
- **Cross-language bridges** that no single-language parser could connect:
  `swiftObjcBridgeResolver` (mixed iOS), `reactNativeBridgeResolver`
  (JS ↔ native, legacy + TurboModules), `expoModulesResolver`
  (Function/AsyncFunction DSL on Swift/Kotlin), `fabricViewResolver` (RN Codegen
  TS spec → native component).

Each resolver is a small ecosystem study — where that framework hides control
flow, and how to recover it deterministically.

---

## 4. Moat, part 3 — the dynamic-dispatch synthesizers

This is the sharpest edge. Static tree-sitter extraction **cannot see** computed
or indirect calls — callbacks, observers, event emitters, vtable dispatch,
interface satisfaction, framework re-renders. A flow that crosses one of these
"breaks," and the agent falls back to reading files to reconstruct it. The
synthesizers bridge those holes so a flow connects **end-to-end** in the graph.

Two production sites: `src/extraction/function-ref.ts` (indirect *call-ref*
extraction at parse time, via the extractor's `functionRefProducers` hook) and
`src/resolution/callback-synthesizer.ts` (a whole-graph pass after base
resolution), plus framework `extract()` methods.

### 20 synthesis channels

Grepping `synthesizedBy` across the resolution layer enumerates the channels —
each a distinct dynamic-dispatch shape it learned to recover:

```
callback            closure-collection   event-emitter        react-render
jsx-render          flutter-build        cpp-override         go-implements
go-grpc-stub-impl   interface-impl       gin-middleware-chain sveltekit-load
vue-handler         mybatis-java-xml     pascal-form          kotlin-expect-actual
rn-cross-platform   rn-event-channel     expo-cross-platform  fabric-native-impl
```

The header of `callback-synthesizer.ts:1-24` documents the design — e.g. the
field-backed observer channel:

```
onUpdate(cb) { this.callbacks.add(cb); }            // registrar
triggerUpdate() { for (cb of this.callbacks) cb(); } // dispatcher
scene.onUpdate(this.triggerRender)                  // registration
   → synthesize  triggerUpdate → triggerRender
```

A representative implementation — the React `setState → render` channel
(`callback-synthesizer.ts:339`):

```ts
function reactRenderEdges(queries, ctx): Edge[] {
  for (const cls of queries.getNodesByKind('class')) {
    const render = childrenOf(cls).find((n) => n.name === 'render');
    if (!render) continue;
    for (const m of childrenOf(cls)) {
      if (!SETSTATE_RE.test(sliceLines(ctx.readFile(m.filePath), m.startLine, m.endLine))) continue;
      edges.push({
        source: m.id, target: render.id, kind: 'calls',
        provenance: 'heuristic',                                    // ← honest tag
        metadata: { synthesizedBy: 'react-render', via: 'setState',
                    registeredAt: `${render.filePath}:${render.startLine}` },
      });
    }
  }
}
```

### Two design decisions that *are* the moat

1. **Every synthesized edge is provenance-tagged.** `provenance: 'heuristic'` +
   `metadata.synthesizedBy` + `registeredAt` (the wiring site) — stored in the
   `edges.provenance` column (`schema.sql:53`) and surfaced inline in
   `codegraph_explore`'s output. This lets the tool be **honest**: it shows the
   synthesized hop *and* where it was inferred from, and it can choose silence
   over a wrong guess (reactive/reconciler runtimes with no static edges are left
   uncovered — "silent beats wrong").

2. **"Partial coverage is WORSE than none."** (codegraph's `CLAUDE.md`.) Bridging
   one boundary but not the next *reveals* a hop the agent then drills into and
   reads. Measured on Excalidraw, the React-render channel alone *raised* reads to
   5–7; only completing the flow (adding the jsx-child hop) dropped it to 0–1. So
   channels are co-designed to close a flow end-to-end, not piecemeal.

---

## 5. Why this is a moat (the meta-point)

The retrievable graph is only as good as its **coverage × precision**, and that
is the product of ~46 specialized modules (26 extractors + 24 framework
integrations) and 20 synthesis channels — **each one derived from a real repo
where a flow broke, then validated back on real repos.** codegraph's `CLAUDE.md`
encodes the bar every addition must clear:

> For each **language × framework**, validate on **small, medium, and large**
> real repos with **≥3 different flow prompts** each: deterministic probes
> (explore connects from→to with no break; no node explosion; synthesized-edge
> precision spot-check), then an **agent A/B** (with vs without, ≥2 runs/arm,
> measuring Read/Grep/duration). Pass bar: a flow question reaches **~0 Read/Grep**
> within the repo's explore budget, faster than without, no regression on a
> control repo.

That methodology — not any single algorithm — is the asset. Reproducing
codegraph means re-deriving the long tail of grammar quirks, framework control-
flow patterns, and dynamic-dispatch shapes, and re-validating each on real
codebases. It is **breadth of tuned coverage**, accreted over many issues and
PRs, which is exactly the kind of thing that is cheap to depend on and expensive
to rebuild.

### Implication for Codewalk

- **Reuse, don't rebuild** the extraction + resolution + synthesis layers — that
  is the moat we get for free by depending on / vendoring codegraph.
- The layers that *are* commodity (the SQLite backend, FTS, graph traversal) are
  the ones we can safely own — which is exactly the lever for the FTS5 fix
  (swap `node:sqlite` for an FTS5-bundling backend) without touching the moat.
  See [`launch-plan.md`](launch-plan.md) §3.

---

## Appendix — key files

| Concern | File(s) |
|---|---|
| Public facade | `src/index.ts` |
| Core types (NodeKind/EdgeKind/Language) | `src/types.ts` |
| Extraction orchestrator + worker | `src/extraction/index.ts`, `parse-worker.ts` |
| Extractor contract | `src/extraction/tree-sitter-types.ts:80` |
| Language extractors (19) | `src/extraction/languages/*.ts` |
| Template/non-TS extractors (7) | `src/extraction/{svelte,vue,astro,liquid,razor,dfm,mybatis}-extractor.ts` |
| Indirect call-ref extraction | `src/extraction/function-ref.ts` |
| Reference resolver | `src/resolution/index.ts` |
| Framework registry (24 integrations) | `src/resolution/frameworks/index.ts:33` |
| Framework contract | `src/resolution/types.ts:164` |
| Dynamic-dispatch synthesizers (20 channels) | `src/resolution/callback-synthesizer.ts` |
| Storage + FTS5 schema | `src/db/schema.sql`, `src/db/queries.ts`, `src/db/sqlite-adapter.ts` |
| Graph algorithms | `src/graph/traversal.ts`, `src/graph/queries.ts` |
| Context assembly | `src/context/index.ts`, `formatter.ts` |
| MCP server + agent guidance | `src/mcp/index.ts`, `tools.ts`, `server-instructions.ts` |
| Validation methodology + design notes | `CLAUDE.md`, `docs/design/callback-edge-synthesis.md`, `docs/design/dynamic-dispatch-coverage-playbook.md` |
