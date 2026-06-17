//! Server-level instructions emitted in the MCP `initialize` response — the
//! single source of agent guidance (port of `mcp/server-instructions.ts`).
//! MCP clients surface this in the agent's system prompt. No CLAUDE.md block.

pub const SERVER_INSTRUCTIONS: &str = r#"# Codegraph — code intelligence over an indexed knowledge graph

Codegraph is a SQLite knowledge graph of every symbol, edge, and file in
the workspace — pre-computed structure you would otherwise re-derive by
reading files. Reads are sub-millisecond. Reach for it BEFORE *and* while
writing or editing code — not just for questions: one call returns the
verbatim source PLUS who calls it and what it affects, so you edit with the
blast radius in view. More accurate context, in far fewer tokens and
round-trips than reading files yourself.

## Use codegraph instead of reading files — for questions AND edits

Whether you're answering "how does X work" or implementing a change, reach
for codegraph before you Read. For understanding, answer DIRECTLY — usually
with ONE `codegraph_explore` call. `codegraph_explore` takes either a
natural-language question or a bag of symbol/file names and returns the
verbatim source of the relevant symbols grouped by file, so it is
Read-equivalent and most often the ONLY codegraph call you need. A direct
codegraph answer is typically one to a few calls; a grep/read exploration is
dozens.

## Tool selection by intent

- **Almost any question — "how does X work", architecture, a bug, "what/where is X", or surveying an area** → `codegraph_explore` (PRIMARY — call FIRST; ONE capped call returns the verbatim source of the relevant symbols grouped by file; most often the ONLY call you need)
- **"How does X reach/become Y? / the flow / the path from X to Y"** → `codegraph_explore`, naming the symbols that span the flow — it surfaces the call path among them, including dynamic-dispatch hops (callbacks) grep can't follow
- **"What is the symbol named X?" (just its location)** → `codegraph_search`
- **"What calls this?" / "What would changing this break?"** → `codegraph_callers` — EVERY call site with file:line, including callback registrations. When several UNRELATED symbols share a name, it reports one section per definition — pass `file` to focus one.
- **About to read or edit a symbol you can name** → `codegraph_node` with that `symbol` (the verbatim source plus its caller/callee trail). For an OVERLOADED name it returns EVERY matching definition's body in one call.
- **Reading a source FILE (any time you'd use Read)** → `codegraph_node` with a `file` path and no `symbol`. Returns the file's current source with line numbers (`<n>\t<line>`, safe to Edit from), narrowable with `offset`/`limit` like Read, plus which files depend on it.

## Common chains

- **Flow / "how does X reach Y"**: ONE `codegraph_explore` with the symbol names spanning the flow.
- **Onboarding / understanding any area**: ONE `codegraph_explore` is usually the whole answer.
- **Refactor planning**: `codegraph_callers` for the complete call-site list to update.

## Anti-patterns

- **Trust codegraph's results — don't re-verify them with grep.** They come from a full AST parse.
- **Don't grep first** when looking up a symbol by name — `codegraph_search` is faster.
- **Don't chain `codegraph_search` + `codegraph_node`** to understand an area — ONE `codegraph_explore` returns the relevant symbols' source together.
- **Don't reach for `Read` on an indexed source file** — `codegraph_node` with a `file` reads it for you (same `<n>\t<line>` source, faster, with its blast radius).

## Limitations

- Indexing is the user's decision — mention they can run `codegraph index <repo>` if it comes up, but don't run it yourself.
- Cross-file resolution is best-effort name matching; ambiguous calls may return multiple candidates.
- No live correctness validation — that's still the compiler / test suite / linter's job.
"#;

/// Variant sent when the workspace has NO codegraph index (`tools/list` is empty
/// in this state, so the agent has nothing to mis-call).
pub const SERVER_INSTRUCTIONS_UNINDEXED: &str = r#"# Codegraph — inactive (workspace not indexed)

This workspace has no codegraph index (no `.codegraph/` directory), so no
codegraph tools are available this session. Work with your built-in tools as
usual.

Indexing is the user's decision — do not run it yourself. If the user asks
about codegraph, they can enable it by running `codegraph index <repo>` in the
project root and starting a new session.
"#;
