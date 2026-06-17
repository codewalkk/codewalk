//! MCP server (mode 1) — the CG-proven 4-tool surface over stdio via `rmcp`.
//!
//! Tools: `codegraph_explore` (PRIMARY), `codegraph_node`, `codegraph_search`,
//! `codegraph_callers`. Success-shaped errors: not-indexed / symbol-not-found
//! return a SUCCESS response with guidance — `isError` is reserved for path
//! refusals + real malfunctions. An unindexed workspace serves an empty
//! `tools/list` + the "inactive" instructions variant.

use crate::server_instructions::{SERVER_INSTRUCTIONS, SERVER_INSTRUCTIONS_UNINDEXED};
use codegraph_core::context::{get_explore_budget, get_explore_output_budget, ContextBuilder};
use codegraph_core::{graph, NodeKind, Store};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, RoleServer, ServerHandler, ServiceExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

// ---- tool parameter structs -------------------------------------------------

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExploreArgs {
    /// Symbol names, file names, or short code terms to explore (a natural-language question works too).
    pub query: String,
    /// Maximum number of files to include source from (default: size-scaled).
    pub max_files: Option<u32>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchArgs {
    /// Symbol name or partial name (e.g. "auth", "signIn", "UserService").
    pub query: String,
    /// Filter by node kind (function, method, class, interface, struct, …).
    pub kind: Option<String>,
    /// Maximum results (default 10).
    pub limit: Option<u32>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CallersArgs {
    /// Name of the function/method/class to find callers for.
    pub symbol: String,
    /// Narrow to the definition in this file (path or suffix) when several same-named symbols exist.
    pub file: Option<String>,
    /// Maximum callers to return (default 20).
    pub limit: Option<u32>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodeArgs {
    /// Symbol to read (symbol mode). Omit and pass `file` alone to read a whole file like Read.
    pub symbol: Option<String>,
    /// Include the symbol's full body (symbol mode; default false).
    pub include_code: Option<bool>,
    /// A file path or basename. Pass it ALONE to read the file like Read; or WITH a symbol to disambiguate.
    pub file: Option<String>,
    /// File mode: 1-based start line (like Read's offset).
    pub offset: Option<u32>,
    /// File mode: max lines to return (like Read's limit).
    pub limit: Option<u32>,
    /// File mode: return just the file's symbol map + dependents.
    pub symbols_only: Option<bool>,
}

// ---- server -----------------------------------------------------------------

struct Inner {
    store: Option<Mutex<Store>>,
    project_root: PathBuf,
    file_count: usize,
    indexed: bool,
}

#[derive(Clone)]
pub struct CodegraphServer {
    inner: std::sync::Arc<Inner>,
    tool_router: ToolRouter<CodegraphServer>,
}

impl CodegraphServer {
    fn new(project_root: PathBuf, store: Option<Store>) -> Self {
        let indexed = store.is_some();
        let file_count = store
            .as_ref()
            .and_then(|s| s.stats().ok())
            .map(|s| s.file_count as usize)
            .unwrap_or(0);
        CodegraphServer {
            inner: std::sync::Arc::new(Inner {
                store: store.map(Mutex::new),
                project_root,
                file_count,
                indexed,
            }),
            tool_router: Self::tool_router(),
        }
    }

    fn text(s: String) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::success(vec![Content::text(s)]))
    }

    /// Run a closure with the locked store, or return the success-shaped
    /// "not indexed" guidance.
    fn with_store<F: FnOnce(&Store, &Path) -> String>(&self, f: F) -> Result<CallToolResult, McpError> {
        match &self.inner.store {
            Some(m) => {
                let guard = m.lock().expect("store mutex");
                let out = f(&guard, &self.inner.project_root);
                Self::text(out)
            }
            None => Self::text(
                "This workspace is not indexed (no .codegraph/). codegraph tools are inactive — \
                 use your built-in tools. The user can run `codegraph index <repo>` to enable it."
                    .to_string(),
            ),
        }
    }
}

#[tool_router]
impl CodegraphServer {
    #[tool(
        description = "PRIMARY TOOL — call FIRST for almost any question OR before an edit. Returns the verbatim source of the relevant symbols grouped by file in ONE capped call (Read-equivalent — treat the shown source as already Read), plus the call path among them. Query can be a natural-language question OR a bag of symbol/file names. Usually the ONLY call you need."
    )]
    async fn codegraph_explore(&self, Parameters(args): Parameters<ExploreArgs>) -> Result<CallToolResult, McpError> {
        let fc = self.inner.file_count;
        self.with_store(move |store, root| {
            let budget = get_explore_output_budget(fc);
            let max_files = args
                .max_files
                .map(|m| (m as usize).clamp(1, 20))
                .unwrap_or(budget.default_max_files);
            let builder = ContextBuilder::new(store, root);
            match builder.explore_markdown(&args.query, max_files, &budget) {
                Ok(mut md) => {
                    if budget.include_budget_note {
                        let calls = get_explore_budget(fc);
                        md.push_str(&format!(
                            "\n\n_(Explore budget for this project: ~{} call(s). If this didn't fully answer, refine the query — don't fall back to grep/Read.)_",
                            calls
                        ));
                    }
                    md
                }
                Err(e) => format!("explore failed: {}", e),
            }
        })
    }

    #[tool(
        description = "Quick symbol search by name. Returns locations only (no code). Use codegraph_explore to get the actual source / understand an area in one call."
    )]
    async fn codegraph_search(&self, Parameters(args): Parameters<SearchArgs>) -> Result<CallToolResult, McpError> {
        self.with_store(move |store, root| {
            let limit = args.limit.unwrap_or(10) as usize;
            let kinds: Vec<NodeKind> = args.kind.as_deref().and_then(parse_kind).into_iter().collect();
            let tokens = codegraph_core::search::query_utils::derive_project_name_tokens(root);
            match store.search_nodes(&args.query, &kinds, limit, &tokens) {
                Ok(hits) if !hits.is_empty() => {
                    let mut out = format!("{} result(s) for \"{}\":\n", hits.len(), args.query);
                    for r in hits {
                        let n = &r.node;
                        out.push_str(&format!(
                            "- {} {} ({}:{})\n",
                            n.kind.as_str(),
                            if n.qualified_name.is_empty() { &n.name } else { &n.qualified_name },
                            n.file_path,
                            n.start_line
                        ));
                    }
                    out
                }
                Ok(_) => format!(
                    "No symbol matched \"{}\". Try codegraph_explore with the concept, or a shorter/partial name.",
                    args.query
                ),
                Err(e) => format!("search failed: {}", e),
            }
        })
    }

    #[tool(
        description = "List functions that call <symbol> (every call site with file:line, including callback registrations). When several unrelated symbols share a name, reports one section per definition — pass `file` to focus one. For the full flow, use codegraph_explore."
    )]
    async fn codegraph_callers(&self, Parameters(args): Parameters<CallersArgs>) -> Result<CallToolResult, McpError> {
        self.with_store(move |store, _root| {
            let limit = args.limit.unwrap_or(20) as usize;
            run_callers(store, &args.symbol, args.file.as_deref(), limit)
        })
    }

    #[tool(
        description = "Two modes. (1) READ A FILE — pass `file` alone (no `symbol`): returns the file's current source with line numbers (`<n>\\t<line>`, safe to Edit from), narrowable with `offset`/`limit` like Read, plus which files depend on it. Use it whenever you would Read a source file. (2) ONE SYMBOL — its location, verbatim source (includeCode=true) and caller/callee trail. For an AMBIGUOUS name it returns EVERY matching definition's body in one call."
    )]
    async fn codegraph_node(&self, Parameters(args): Parameters<NodeArgs>) -> Result<CallToolResult, McpError> {
        self.with_store(move |store, root| run_node(store, root, &args))
    }
}

#[tool_handler]
impl ServerHandler for CodegraphServer {
    fn get_info(&self) -> ServerInfo {
        let instructions = if self.inner.indexed {
            SERVER_INSTRUCTIONS
        } else {
            SERVER_INSTRUCTIONS_UNINDEXED
        };
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(instructions.to_string())
    }

    /// Unindexed workspace → empty tools/list (the agent has nothing to mis-call).
    async fn list_tools(
        &self,
        _req: Option<PaginatedRequestParams>,
        _ctx: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        if !self.inner.indexed {
            return Ok(ListToolsResult::default());
        }
        Ok(ListToolsResult {
            tools: self.tool_router.list_all(),
            ..Default::default()
        })
    }
}

// ---- tool implementations ---------------------------------------------------

fn parse_kind(s: &str) -> Option<NodeKind> {
    Some(match s {
        "function" => NodeKind::Function,
        "method" => NodeKind::Method,
        "class" => NodeKind::Class,
        "interface" => NodeKind::Interface,
        "struct" => NodeKind::Struct,
        "trait" => NodeKind::Trait,
        "type" | "type_alias" => NodeKind::TypeAlias,
        "variable" => NodeKind::Variable,
        "constant" => NodeKind::Constant,
        "enum" => NodeKind::Enum,
        "route" => NodeKind::Route,
        "component" => NodeKind::Component,
        _ => return None,
    })
}

/// Pick the best node for a name (prefer function/method, then types), with an
/// optional file-suffix filter. Returns all candidates too (for ambiguity).
fn resolve_symbol(store: &Store, name: &str, file: Option<&str>) -> Vec<codegraph_core::Node> {
    let mut hits = store.get_nodes_by_name_full(name, 50).unwrap_or_default();
    if let Some(f) = file {
        let fl = f.to_ascii_lowercase();
        hits.retain(|n| n.file_path.to_ascii_lowercase().contains(&fl));
    }
    hits.sort_by_key(|n| match n.kind {
        NodeKind::Method | NodeKind::Function => 0,
        NodeKind::Struct | NodeKind::Interface | NodeKind::Class | NodeKind::Trait => 1,
        _ => 2,
    });
    hits
}

fn run_callers(store: &Store, symbol: &str, file: Option<&str>, limit: usize) -> String {
    let cands = resolve_symbol(store, symbol, file);
    if cands.is_empty() {
        return format!(
            "No symbol named \"{}\" is indexed. Try codegraph_search for the right name, or codegraph_explore for the area.",
            symbol
        );
    }
    let mut out = String::new();
    // One section per distinct definition (TS behavior).
    for def in cands.iter().take(6) {
        out.push_str(&format!(
            "## callers of {} ({}:{})\n",
            if def.qualified_name.is_empty() { &def.name } else { &def.qualified_name },
            def.file_path,
            def.start_line
        ));
        let hops = graph::callers(store, &def.id, 2).unwrap_or_default();
        if hops.is_empty() {
            out.push_str("  (no callers found — may be an entry point, or only reached by dynamic dispatch)\n\n");
            continue;
        }
        for h in hops.into_iter().take(limit) {
            let via = h
                .edge
                .provenance
                .filter(|p| p.as_str() == "heuristic")
                .and(
                    h.edge
                        .metadata
                        .as_ref()
                        .and_then(|m| m.get("via").and_then(|v| v.as_str())),
                )
                .map(|v| format!("  [via callback registration: {}]", v))
                .unwrap_or_default();
            out.push_str(&format!(
                "  {}{} {} ({}:{}){}\n",
                "  ".repeat(h.depth as usize),
                h.edge.kind.as_str(),
                if h.node.qualified_name.is_empty() { &h.node.name } else { &h.node.qualified_name },
                h.node.file_path,
                h.node.start_line,
                via
            ));
        }
        out.push('\n');
    }
    out
}

fn read_file_lines(root: &Path, rel: &str) -> Option<Vec<String>> {
    let abs = codegraph_core::context::validate_path_within_root(root, rel)?;
    let content = std::fs::read_to_string(abs).ok()?;
    Some(content.split('\n').map(|s| s.to_string()).collect())
}

fn run_node(store: &Store, root: &Path, args: &NodeArgs) -> String {
    // File mode: `file` given, no `symbol`.
    if args.symbol.is_none() {
        let Some(file) = &args.file else {
            return "codegraph_node needs a `symbol` (to read a symbol) or a `file` (to read a file like Read).".to_string();
        };
        // Resolve file (exact or suffix) via the nodes table.
        let rel = resolve_file_path(store, file);
        let Some(rel) = rel else {
            return format!("No indexed file matches \"{}\". Use Read for unindexed files (configs, docs).", file);
        };
        if args.symbols_only.unwrap_or(false) {
            return file_symbol_map(store, &rel);
        }
        let Some(lines) = read_file_lines(root, &rel) else {
            return format!("Could not read {} from disk.", rel);
        };
        let offset = args.offset.unwrap_or(1).max(1) as usize;
        let limit = args.limit.map(|l| l as usize).unwrap_or(2000);
        let start = offset - 1;
        let end = (start + limit).min(lines.len());
        let mut out = String::new();
        let deps = store.get_dependent_file_paths(&rel).unwrap_or_default();
        if !deps.is_empty() {
            out.push_str(&format!("// {} file(s) depend on {} (e.g. {})\n", deps.len(), rel, deps.iter().take(3).cloned().collect::<Vec<_>>().join(", ")));
        }
        out.push_str(&format!("// {}\n", rel));
        for (i, line) in lines[start..end.max(start)].iter().enumerate() {
            out.push_str(&format!("{}\t{}\n", offset + i, line));
        }
        return out;
    }

    // Symbol mode.
    let symbol = args.symbol.as_ref().unwrap();
    let cands = resolve_symbol(store, symbol, args.file.as_deref());
    if cands.is_empty() {
        return format!(
            "No symbol named \"{}\" is indexed. Try codegraph_search for the right name, or codegraph_explore for the area.",
            symbol
        );
    }
    let include_code = args.include_code.unwrap_or(false);
    let mut out = String::new();
    if cands.len() > 1 {
        out.push_str(&format!("{} definitions of \"{}\" (every overload shown):\n\n", cands.len(), symbol));
    }
    for def in cands.iter().take(8) {
        out.push_str(&format!(
            "### {} — {} ({}:{}-{})\n",
            def.kind.as_str(),
            if def.qualified_name.is_empty() { &def.name } else { &def.qualified_name },
            def.file_path,
            def.start_line,
            def.end_line
        ));
        if let Some(sig) = &def.signature {
            out.push_str(&format!("`{}`\n", sig));
        }
        if include_code {
            if let Some(lines) = read_file_lines(root, &def.file_path) {
                let s = (def.start_line.saturating_sub(1)) as usize;
                let e = (def.end_line as usize).min(lines.len());
                if s < lines.len() {
                    out.push_str(&format!("```{}\n", def.language.as_str()));
                    for (i, line) in lines[s..e.max(s)].iter().enumerate() {
                        out.push_str(&format!("{}\t{}\n", def.start_line as usize + i, line));
                    }
                    out.push_str("```\n");
                }
            }
        }
        // Caller/callee trail (depth 1).
        let callers = graph::callers(store, &def.id, 1).unwrap_or_default();
        let callees = graph::callees(store, &def.id, 1).unwrap_or_default();
        if !callers.is_empty() {
            let names: Vec<String> = callers.iter().take(8).map(|h| h.node.name.clone()).collect();
            out.push_str(&format!("callers: {}\n", names.join(", ")));
        }
        if !callees.is_empty() {
            let names: Vec<String> = callees.iter().take(8).map(|h| h.node.name.clone()).collect();
            out.push_str(&format!("calls: {}\n", names.join(", ")));
        }
        out.push('\n');
    }
    out
}

/// Find an indexed file whose path equals or ends with `q`.
fn resolve_file_path(store: &Store, q: &str) -> Option<String> {
    // Search the nodes table for a file_path match (cheap; one query).
    let ql = q.to_ascii_lowercase();
    let files = store.distinct_file_paths_like(&ql).unwrap_or_default();
    // Prefer exact, then suffix, then contains.
    files
        .iter()
        .find(|f| f.to_ascii_lowercase() == ql)
        .or_else(|| files.iter().find(|f| f.to_ascii_lowercase().ends_with(&ql)))
        .or_else(|| files.first())
        .cloned()
}

fn file_symbol_map(store: &Store, rel: &str) -> String {
    let syms = store.nodes_in_file(rel).unwrap_or_default();
    let mut out = format!("// symbol map for {}\n", rel);
    for n in syms {
        out.push_str(&format!("{}\t{} {} ({})\n", n.start_line, n.kind.as_str(), n.name, n.end_line));
    }
    let deps = store.get_dependent_file_paths(rel).unwrap_or_default();
    if !deps.is_empty() {
        out.push_str(&format!("// {} dependent file(s)\n", deps.len()));
    }
    out
}

/// Start the stdio MCP server for `repo`.
pub fn serve(repo: &str) -> anyhow::Result<()> {
    let root = std::fs::canonicalize(repo).unwrap_or_else(|_| PathBuf::from(repo));
    let db = root.join(".codegraph").join("graph.db");
    let store = if db.exists() { Store::open(&db).ok() } else { None };

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        let server = CodegraphServer::new(root, store);
        let service = server.serve((tokio::io::stdin(), tokio::io::stdout())).await?;
        service.waiting().await?;
        Ok::<(), anyhow::Error>(())
    })
}
