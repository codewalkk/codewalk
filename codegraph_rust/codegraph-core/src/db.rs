//! Storage — `rusqlite` (`bundled`, `fts5`) port of the TS `db/` layer.
//!
//! The schema (`schema.sql`) is a verbatim port of the TS spec. The whole point
//! of the Rust rebuild is here: `features = ["bundled", "fts5"]` statically links
//! a SQLite **with FTS5 compiled in**, so `nodes_fts` always exists with no
//! Node-version dependency — the "FTS5 cliff" is gone (see the M0 test).

use crate::types::{Edge, Language, Node, NodeKind, UnresolvedReference};
use anyhow::Result;
use rusqlite::{params, Connection};
use std::path::Path;

const SCHEMA: &str = include_str!("schema.sql");

/// A handle to a `graph.db`, wrapping a rusqlite connection.
pub struct Store {
    conn: Connection,
}

/// Aggregate index statistics (subset of TS `GraphStats`) used for parity checks.
#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub file_count: i64,
    pub node_count: i64,
    pub edge_count: i64,
    pub unresolved_count: i64,
}

impl Store {
    /// Open (creating if needed) a database at `path` and apply the schema.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// Open an in-memory database (used by tests).
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        // Bulk-load friendly pragmas. FK enforcement stays OFF (SQLite default,
        // matching the TS node:sqlite backend) so node/edge insert order is free.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA temp_store = MEMORY;
             PRAGMA cache_size = -64000;",
        )?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store { conn })
    }

    /// Remove all rows (a fresh full index). Triggers keep `nodes_fts` in sync.
    pub fn clear(&self) -> Result<()> {
        self.conn.execute_batch(
            "DELETE FROM edges;
             DELETE FROM unresolved_refs;
             DELETE FROM nodes;
             DELETE FROM files;",
        )?;
        Ok(())
    }

    /// Insert many file batches in ONE transaction — the fast path used by the
    /// orchestrator. Per-file transactions cost ~one WAL fsync each, which on a
    /// repo like k8s (12k+ files) dominates wall-clock; a single transaction
    /// collapses that to one commit.
    pub fn write_all(&mut self, batches: &[WriteBatch]) -> Result<()> {
        let tx = self.conn.transaction()?;
        for batch in batches {
            Self::insert_batch(&tx, batch)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Insert a single file's batch in its own transaction (used by tests).
    pub fn write_batch(&mut self, batch: &WriteBatch) -> Result<()> {
        let tx = self.conn.transaction()?;
        Self::insert_batch(&tx, batch)?;
        tx.commit()?;
        Ok(())
    }

    fn insert_batch(tx: &rusqlite::Transaction<'_>, batch: &WriteBatch) -> Result<()> {
        {
            let mut node_stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO nodes
                 (id, kind, name, qualified_name, file_path, language,
                  start_line, end_line, start_column, end_column,
                  docstring, signature, visibility, is_exported, is_async,
                  is_static, is_abstract, decorators, type_parameters,
                  return_type, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)",
            )?;
            for n in &batch.nodes {
                let decorators = n
                    .decorators
                    .as_ref()
                    .map(|d| serde_json::to_string(d).unwrap_or_default());
                let type_params = n
                    .type_parameters
                    .as_ref()
                    .map(|d| serde_json::to_string(d).unwrap_or_default());
                node_stmt.execute(params![
                    n.id,
                    n.kind.as_str(),
                    n.name,
                    n.qualified_name,
                    n.file_path,
                    n.language.as_str(),
                    n.start_line,
                    n.end_line,
                    n.start_column,
                    n.end_column,
                    n.docstring,
                    n.signature,
                    n.visibility.map(|v| v.as_str()),
                    n.is_exported as i32,
                    n.is_async as i32,
                    n.is_static as i32,
                    n.is_abstract as i32,
                    decorators,
                    type_params,
                    n.return_type,
                    n.updated_at,
                ])?;
            }

            let mut edge_stmt = tx.prepare_cached(
                "INSERT INTO edges (source, target, kind, metadata, line, col, provenance)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
            )?;
            for e in &batch.edges {
                let metadata = e.metadata.as_ref().map(|m| m.to_string());
                edge_stmt.execute(params![
                    e.source,
                    e.target,
                    e.kind.as_str(),
                    metadata,
                    e.line,
                    e.col,
                    e.provenance.map(|p| p.as_str()),
                ])?;
            }

            let mut uref_stmt = tx.prepare_cached(
                "INSERT INTO unresolved_refs
                 (from_node_id, reference_name, reference_kind, line, col, candidates, file_path, language)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            )?;
            for r in &batch.unresolved {
                let candidates = r
                    .candidates
                    .as_ref()
                    .map(|c| serde_json::to_string(c).unwrap_or_default());
                uref_stmt.execute(params![
                    r.from_node_id,
                    r.reference_name,
                    r.reference_kind.as_str(),
                    r.line,
                    r.col,
                    candidates,
                    r.file_path.clone().unwrap_or_default(),
                    r.language.map(|l| l.as_str()).unwrap_or("unknown"),
                ])?;
            }

            let mut file_stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO files
                 (path, content_hash, language, size, modified_at, indexed_at, node_count, errors)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            )?;
            for f in &batch.files {
                file_stmt.execute(params![
                    f.path,
                    f.content_hash,
                    f.language.as_str(),
                    f.size,
                    f.modified_at,
                    f.indexed_at,
                    f.node_count,
                    f.errors,
                ])?;
            }
        }
        Ok(())
    }

    /// Aggregate counts for parity reporting.
    pub fn stats(&self) -> Result<Stats> {
        let count = |sql: &str| -> Result<i64> {
            Ok(self.conn.query_row(sql, [], |r| r.get(0))?)
        };
        Ok(Stats {
            file_count: count("SELECT COUNT(*) FROM files")?,
            node_count: count("SELECT COUNT(*) FROM nodes")?,
            edge_count: count("SELECT COUNT(*) FROM edges")?,
            unresolved_count: count("SELECT COUNT(*) FROM unresolved_refs")?,
        })
    }

    /// Node counts grouped by `kind`, descending — for parity breakdowns.
    pub fn node_counts_by_kind(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT kind, COUNT(*) c FROM nodes GROUP BY kind ORDER BY c DESC")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Exact-name lookup (TS `getNodesByName`) — the spot-check probe.
    pub fn get_nodes_by_name(&self, name: &str, limit: usize) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, name, qualified_name, file_path, language,
                    start_line, end_line, start_column, end_column, return_type
             FROM nodes WHERE name = ?1 LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![name, limit as i64], Self::row_to_node)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// FTS5 search over name/qualified_name/docstring/signature (TS `searchNodes`).
    /// `query` is matched against the `nodes_fts` index; results join back to
    /// `nodes` ordered by bm25 rank.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT n.id, n.kind, n.name, n.qualified_name, n.file_path, n.language,
                    n.start_line, n.end_line, n.start_column, n.end_column, n.return_type
             FROM nodes_fts f
             JOIN nodes n ON n.rowid = f.rowid
             WHERE nodes_fts MATCH ?1
             ORDER BY bm25(nodes_fts)
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![query, limit as i64], Self::row_to_node)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn row_to_node(r: &rusqlite::Row<'_>) -> rusqlite::Result<Node> {
        let kind: String = r.get(1)?;
        let language: String = r.get(5)?;
        Ok(Node {
            id: r.get(0)?,
            kind: parse_node_kind(&kind),
            name: r.get(2)?,
            qualified_name: r.get(3)?,
            file_path: r.get(4)?,
            language: parse_language(&language),
            start_line: r.get(6)?,
            end_line: r.get(7)?,
            start_column: r.get(8)?,
            end_column: r.get(9)?,
            return_type: r.get(10)?,
            docstring: None,
            signature: None,
            visibility: None,
            is_exported: false,
            is_async: false,
            is_static: false,
            is_abstract: false,
            decorators: None,
            type_parameters: None,
            updated_at: 0,
        })
    }
}

/// A unit of work handed to `Store::write_batch`.
#[derive(Debug, Default)]
pub struct WriteBatch {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub unresolved: Vec<UnresolvedReference>,
    pub files: Vec<FileRecord>,
}

/// A tracked source file row (`files` table).
#[derive(Debug, Clone)]
pub struct FileRecord {
    pub path: String,
    pub content_hash: String,
    pub language: Language,
    pub size: i64,
    pub modified_at: i64,
    pub indexed_at: i64,
    pub node_count: i64,
    pub errors: Option<String>,
}

fn parse_node_kind(s: &str) -> NodeKind {
    serde_json::from_value(serde_json::Value::String(s.to_string())).unwrap_or(NodeKind::File)
}

fn parse_language(s: &str) -> Language {
    serde_json::from_value(serde_json::Value::String(s.to_string())).unwrap_or(Language::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Edge, EdgeKind, Node, NodeKind};

    /// THE M0 PROOF: a stock Rust toolchain (no Node) creates the full schema —
    /// including the `nodes_fts` FTS5 virtual table — and FTS MATCH queries work.
    /// This is what the "FTS5 cliff is gone" claim rests on.
    #[test]
    fn fts5_virtual_table_works_on_stock_toolchain() {
        let store = Store::open_in_memory().expect("schema applies");

        // Prove the fts5 module is compiled in: this DDL fails outright on a
        // SQLite built without FTS5.
        store
            .conn
            .execute_batch("CREATE VIRTUAL TABLE probe USING fts5(body);")
            .expect("fts5 module available");
        store
            .conn
            .execute(
                "INSERT INTO probe(body) VALUES ('the quick brown fox')",
                [],
            )
            .unwrap();
        let hits: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM probe WHERE probe MATCH 'quick'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1, "fts5 MATCH returns the row");
    }

    #[test]
    fn write_and_search_roundtrip() {
        let mut store = Store::open_in_memory().unwrap();
        let mut n = Node::new(
            "function:abc123".into(),
            NodeKind::Function,
            "ScheduleOne".into(),
            "Scheduler::ScheduleOne".into(),
            "pkg/scheduler/schedule_one.go".into(),
            Language::Go,
        );
        n.signature = Some("(ctx context.Context)".into());
        let file_node = Node::new(
            "file:pkg/scheduler/schedule_one.go".into(),
            NodeKind::File,
            "schedule_one.go".into(),
            "pkg/scheduler/schedule_one.go".into(),
            "pkg/scheduler/schedule_one.go".into(),
            Language::Go,
        );
        let edge = Edge::new(file_node.id.clone(), n.id.clone(), EdgeKind::Contains);

        store
            .write_batch(&WriteBatch {
                nodes: vec![file_node, n],
                edges: vec![edge],
                unresolved: vec![],
                files: vec![],
            })
            .unwrap();

        let stats = store.stats().unwrap();
        assert_eq!(stats.node_count, 2);
        assert_eq!(stats.edge_count, 1);

        // FTS finds the symbol via the auto-synced nodes_fts trigger.
        let hits = store.search("ScheduleOne", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "ScheduleOne");

        // Exact-name probe.
        let by_name = store.get_nodes_by_name("ScheduleOne", 10).unwrap();
        assert_eq!(by_name.len(), 1);
        assert_eq!(by_name[0].kind, NodeKind::Function);
    }
}
