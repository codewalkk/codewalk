//! `codegraph` — mode 1 CLI. Structural intelligence only.
//!
//! Commands (docs/rust-build-plan.md §4): `index`, `search`/`query`, and
//! (M3) `serve --mcp`. Wired up incrementally; M1 lands `index` + `search`.

use clap::{Parser, Subcommand};

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
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Index { path, stats } => codegraph_cli::cmd_index(&path, stats),
        Command::Search { path, query, limit } => codegraph_cli::cmd_search(&path, &query, limit),
        Command::Node { path, name } => codegraph_cli::cmd_node(&path, &name),
    }
}

mod codegraph_cli {
    use codegraph_core::{index_repo, Store};
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
        let secs = started.elapsed().as_secs_f64();
        println!(
            "indexed {} files · {} nodes · {} edges · {} unresolved-refs in {:.1}s",
            s.file_count, s.node_count, s.edge_count, s.unresolved_count, secs
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
}
