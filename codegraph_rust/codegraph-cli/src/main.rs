//! `codegraph` — mode 1 CLI. Structural intelligence only.
//!
//! Commands (docs/rust-build-plan.md §4): `index`, `search`/`query`, and
//! (M3) `serve --mcp`. Wired up incrementally; M1 lands `index` + `search`.

use clap::{Parser, Subcommand};

mod mcp;
mod server_instructions;

#[derive(Parser)]
#[command(name = "codegraph", version, about = "Structural code-intelligence engine (mode 1)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Index a repository into <repo>/.codegraph/graph.db
    Index {
        /// Path to the repository root
        path: String,
        /// Print per-kind node counts after indexing
        #[arg(long)]
        stats: bool,
    },
    /// Full-text search the indexed graph
    Search {
        /// Repository root (defaults to current dir)
        #[arg(long, default_value = ".")]
        path: String,
        /// FTS query
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Look up nodes by exact name (the parity spot-check probe)
    Node {
        #[arg(long, default_value = ".")]
        path: String,
        name: String,
    },
    /// Who calls this symbol (incoming call/ref/instantiate edges)
    Callers {
        #[arg(long, default_value = ".")]
        path: String,
        name: String,
        #[arg(long, default_value_t = 2)]
        depth: u32,
    },
    /// What this symbol calls (outgoing edges) — the trace/explore primitive
    Callees {
        #[arg(long, default_value = ".")]
        path: String,
        name: String,
        #[arg(long, default_value_t = 2)]
        depth: u32,
    },
    /// Shortest path between two symbols (by name) — `trace from → to`
    Trace {
        #[arg(long, default_value = ".")]
        path: String,
        from: String,
        to: String,
    },
    /// Build lexical context for a natural-language query (buildContext markdown)
    Context {
        #[arg(long, default_value = ".")]
        path: String,
        query: String,
    },
    /// Run the MCP server over stdio (mode 1, the 4-tool CG surface)
    Serve {
        /// Serve over MCP (the only transport today; flag mirrors the CG CLI)
        #[arg(long)]
        mcp: bool,
        /// Repository root to serve
        #[arg(long, default_value = ".")]
        path: String,
        /// Index the repo first if it isn't indexed (CBM-style convenience; off by default)
        #[arg(long)]
        auto_index: bool,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Index { path, stats } => codegraph_cli::cmd_index(&path, stats),
        Command::Search { path, query, limit } => codegraph_cli::cmd_search(&path, &query, limit),
        Command::Node { path, name } => codegraph_cli::cmd_node(&path, &name),
        Command::Callers { path, name, depth } => codegraph_cli::cmd_callers(&path, &name, depth),
        Command::Callees { path, name, depth } => codegraph_cli::cmd_callees(&path, &name, depth),
        Command::Trace { path, from, to } => codegraph_cli::cmd_trace(&path, &from, &to),
        Command::Context { path, query } => codegraph_cli::cmd_context(&path, &query),
        Command::Serve { mcp: _, path, auto_index } => {
            use std::path::Path;
            let db = Path::new(&path).join(".codegraph").join("graph.db");
            if auto_index && !db.exists() {
                eprintln!("--auto-index: indexing {} first …", path);
                codegraph_cli::cmd_index(&path, false)?;
            }
            mcp::serve(&path)
        }
    }
}

mod codegraph_cli {
    use codegraph_core::{index_repo, resolve, Store};
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    /// `<repo>/.codegraph/graph.db`
    fn db_path(repo: &str) -> PathBuf {
        Path::new(repo).join(".codegraph").join("graph.db")
    }

    pub fn cmd_index(path: &str, stats: bool) -> anyhow::Result<()> {
        let db = db_path(path);
        let mut store = Store::open(&db)?;
        let started = Instant::now();
        eprintln!("indexing {} → {} …", path, db.display());
        let s = index_repo(Path::new(path), &mut store)?;
        eprintln!(
            "  extracted {} files · {} nodes · {} contains-edges · {} unresolved-refs",
            s.file_count, s.node_count, s.edge_count, s.unresolved_count
        );
        eprintln!("resolving references …");
        let rs = resolve(Path::new(path), &mut store)?;
        let s = store.stats()?;
        let secs = started.elapsed().as_secs_f64();
        println!(
            "indexed {} files · {} nodes · {} edges in {:.1}s",
            s.file_count, s.node_count, s.edge_count, secs
        );
        println!(
            "  resolved {}/{} refs → {} edges · {} synthesized",
            rs.resolved, rs.total_refs, rs.base_edges, rs.synthesized_edges
        );
        if stats {
            println!("\nnodes by kind:");
            for (kind, count) in store.node_counts_by_kind()? {
                println!("  {:>10}  {}", count, kind);
            }
        }
        Ok(())
    }

    pub fn cmd_search(path: &str, query: &str, limit: usize) -> anyhow::Result<()> {
        let store = Store::open(&db_path(path))?;
        let hits = store.search(query, limit)?;
        if hits.is_empty() {
            println!("no results for {:?}", query);
        }
        for n in hits {
            println!(
                "{:<10} {:<32} {}:{}",
                n.kind.as_str(),
                n.qualified_name,
                n.file_path,
                n.start_line
            );
        }
        Ok(())
    }

    pub fn cmd_node(path: &str, name: &str) -> anyhow::Result<()> {
        let store = Store::open(&db_path(path))?;
        let hits = store.get_nodes_by_name(name, 50)?;
        if hits.is_empty() {
            println!("no node named {:?}", name);
        }
        for n in hits {
            println!(
                "{:<10} {:<36} {}:{}-{}",
                n.kind.as_str(),
                n.qualified_name,
                n.file_path,
                n.start_line,
                n.end_line
            );
        }
        Ok(())
    }

    /// Resolve a name to its best node (prefer function/method), for traversal.
    fn pick_node(store: &Store, name: &str) -> anyhow::Result<Option<codegraph_core::Node>> {
        use codegraph_core::NodeKind;
        let mut hits = store.get_nodes_by_name(name, 50)?;
        if hits.is_empty() {
            return Ok(None);
        }
        hits.sort_by_key(|n| match n.kind {
            NodeKind::Method | NodeKind::Function => 0,
            NodeKind::Struct | NodeKind::Interface | NodeKind::Class => 1,
            _ => 2,
        });
        Ok(hits.into_iter().next())
    }

    pub fn cmd_callers(path: &str, name: &str, depth: u32) -> anyhow::Result<()> {
        let store = Store::open(&db_path(path))?;
        let Some(node) = pick_node(&store, name)? else {
            println!("no node named {:?}", name);
            return Ok(());
        };
        println!("callers of {} ({}:{}):", node.qualified_name, node.file_path, node.start_line);
        let hops = codegraph_core::graph::callers(&store, &node.id, depth)?;
        if hops.is_empty() {
            println!("  (none)");
        }
        for h in hops {
            println!(
                "  {}{:<8} {:<32} {}:{}",
                "  ".repeat(h.depth as usize),
                h.edge.kind.as_str(),
                h.node.qualified_name,
                h.node.file_path,
                h.node.start_line
            );
        }
        Ok(())
    }

    pub fn cmd_callees(path: &str, name: &str, depth: u32) -> anyhow::Result<()> {
        let store = Store::open(&db_path(path))?;
        let Some(node) = pick_node(&store, name)? else {
            println!("no node named {:?}", name);
            return Ok(());
        };
        println!("callees of {} ({}:{}):", node.qualified_name, node.file_path, node.start_line);
        let hops = codegraph_core::graph::callees(&store, &node.id, depth)?;
        if hops.is_empty() {
            println!("  (none)");
        }
        for h in hops {
            let synth = h
                .edge
                .provenance
                .map(|p| p.as_str() == "heuristic")
                .unwrap_or(false);
            println!(
                "  {}{:<8} {:<32} {}:{}{}",
                "  ".repeat(h.depth as usize),
                h.edge.kind.as_str(),
                h.node.qualified_name,
                h.node.file_path,
                h.node.start_line,
                if synth { "  [synthesized]" } else { "" }
            );
        }
        Ok(())
    }

    pub fn cmd_context(path: &str, query: &str) -> anyhow::Result<()> {
        let store = Store::open(&db_path(path))?;
        let builder = codegraph_core::context::ContextBuilder::new(&store, Path::new(path));
        let opts = codegraph_core::context::BuildOptions::retrieval();
        let md = builder.build_context_markdown(query, &opts)?;
        println!("{}", md);
        Ok(())
    }

    pub fn cmd_trace(path: &str, from: &str, to: &str) -> anyhow::Result<()> {
        let store = Store::open(&db_path(path))?;
        let (Some(a), Some(b)) = (pick_node(&store, from)?, pick_node(&store, to)?) else {
            println!("could not resolve both endpoints");
            return Ok(());
        };
        match codegraph_core::graph::find_path(&store, &a.id, &b.id, &[])? {
            Some(steps) => {
                println!("path {} → {} ({} hops):", from, to, steps.len().saturating_sub(1));
                for (i, s) in steps.iter().enumerate() {
                    let via = s
                        .edge
                        .as_ref()
                        .map(|e| format!(" --{}-->", e.kind.as_str()))
                        .unwrap_or_default();
                    println!("  {}{} {} ({}:{})", "  ".repeat(i), via, s.node.qualified_name, s.node.file_path, s.node.start_line);
                }
            }
            None => println!("no path {} → {}", from, to),
        }
        Ok(())
    }
}
