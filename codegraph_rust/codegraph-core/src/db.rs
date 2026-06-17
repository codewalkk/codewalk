//! Storage — `rusqlite` (`bundled`, `fts5`) port of the TS `db/` layer.
//!
//! The schema (`schema.sql`) is a verbatim port of the TS spec. The whole point
//! of the Rust rebuild is here: `features = ["bundled", "fts5"]` statically links
//! a SQLite **with FTS5 compiled in**, so `nodes_fts` always exists with no
//! Node-version dependency — the "FTS5 cliff" is gone (see the M0 test).

use crate::types::{Edge, EdgeKind, Language, Node, NodeKind, Provenance, UnresolvedReference};
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
                  return_type, name_split, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)",
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
                let name_split = {
                    let s = crate::search::query_utils::camel_split(&n.name);
                    if s.is_empty() { None } else { Some(s) }
                };
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
                    name_split,
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

    /// Fetch a single node by id.
    pub fn node_by_id(&self, id: &str) -> Result<Option<Node>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, kind, name, qualified_name, file_path, language,
                    start_line, end_line, start_column, end_column, return_type
             FROM nodes WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![id], Self::row_to_node)?;
        match rows.next() {
            Some(n) => Ok(Some(n?)),
            None => Ok(None),
        }
    }

    /// Outgoing edges from `source`, optionally filtered to `kinds`.
    pub fn outgoing_edges(&self, source: &str, kinds: &[EdgeKind]) -> Result<Vec<Edge>> {
        self.edges_for("source", source, kinds)
    }
    /// Incoming edges to `target`, optionally filtered to `kinds`.
    pub fn incoming_edges(&self, target: &str, kinds: &[EdgeKind]) -> Result<Vec<Edge>> {
        self.edges_for("target", target, kinds)
    }

    fn edges_for(&self, col: &str, id: &str, kinds: &[EdgeKind]) -> Result<Vec<Edge>> {
        let sql = format!(
            "SELECT source, target, kind, metadata, line, col, provenance FROM edges WHERE {} = ?1",
            col
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt
            .query_map(params![id], |r| {
                let kind: String = r.get(2)?;
                let prov: Option<String> = r.get(6)?;
                let meta: Option<String> = r.get(3)?;
                Ok(Edge {
                    source: r.get(0)?,
                    target: r.get(1)?,
                    kind: parse_edge_kind(&kind),
                    metadata: meta.and_then(|m| serde_json::from_str(&m).ok()),
                    line: r.get(4)?,
                    col: r.get(5)?,
                    provenance: prov.as_deref().map(parse_provenance),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(if kinds.is_empty() {
            rows
        } else {
            rows.into_iter().filter(|e| kinds.contains(&e.kind)).collect()
        })
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

    /// Load every node with all fields needed by resolution (decorators,
    /// is_exported, signature, return_type, qualified_name). k8s is ~167k nodes —
    /// comfortably in-memory, matching the TS resolver's warmed indices.
    pub fn all_nodes(&self) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, name, qualified_name, file_path, language,
                    start_line, end_line, start_column, end_column, return_type,
                    is_exported, signature, decorators
             FROM nodes",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let kind: String = r.get(1)?;
                let language: String = r.get(5)?;
                let decorators: Option<String> = r.get(13)?;
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
                    is_exported: r.get::<_, i64>(11)? != 0,
                    signature: r.get(12)?,
                    decorators: decorators
                        .and_then(|d| serde_json::from_str::<Vec<String>>(&d).ok()),
                    docstring: None,
                    visibility: None,
                    is_async: false,
                    is_static: false,
                    is_abstract: false,
                    type_parameters: None,
                    updated_at: 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Load every unresolved reference (for the resolution pass).
    pub fn all_unresolved(&self) -> Result<Vec<UnresolvedReference>> {
        let mut stmt = self.conn.prepare(
            "SELECT from_node_id, reference_name, reference_kind, line, col, file_path, language
             FROM unresolved_refs",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let rk: String = r.get(2)?;
                let lang: String = r.get(6)?;
                Ok(UnresolvedReference {
                    from_node_id: r.get(0)?,
                    reference_name: r.get(1)?,
                    reference_kind: parse_reference_kind(&rk),
                    line: r.get(3)?,
                    col: r.get(4)?,
                    file_path: r.get::<_, Option<String>>(5)?,
                    language: Some(parse_language(&lang)),
                    candidates: None,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Load all `contains` edges (the only kind present after extraction) for the
    /// resolution graph's adjacency. source/target/kind only — metadata not needed.
    pub fn all_contains_edges(&self) -> Result<Vec<Edge>> {
        let mut stmt = self
            .conn
            .prepare("SELECT source, target FROM edges WHERE kind = 'contains'")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Edge::new(r.get(0)?, r.get(1)?, crate::types::EdgeKind::Contains))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Bulk-insert resolved/synthesized edges in one transaction.
    pub fn insert_edges(&mut self, edges: &[Edge]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO edges (source, target, kind, metadata, line, col, provenance)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
            )?;
            for e in edges {
                let metadata = e.metadata.as_ref().map(|m| m.to_string());
                stmt.execute(params![
                    e.source,
                    e.target,
                    e.kind.as_str(),
                    metadata,
                    e.line,
                    e.col,
                    e.provenance.map(|p| p.as_str()),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Empty the unresolved_refs table (resolved → edges; the rest are dropped,
    /// matching the TS resolver's end state of 0 unresolved rows).
    pub fn clear_unresolved(&self) -> Result<()> {
        self.conn.execute("DELETE FROM unresolved_refs", [])?;
        Ok(())
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

/// Full column list for loading a `Node` with all fields the context builder and
/// MCP layer need (signature, docstring, flags). Keep in sync with `row_to_full`.
const NODE_COLS: &str = "id, kind, name, qualified_name, file_path, language, \
    start_line, end_line, start_column, end_column, docstring, signature, \
    visibility, is_exported, is_async, is_static, is_abstract, decorators, \
    type_parameters, return_type";

/// A scored search hit (re-exported via `crate::search::SearchResult`).
pub use crate::search::SearchResult;

impl Store {
    /// Materialize a `Node` from a row selected with `NODE_COLS`.
    fn row_to_full(r: &rusqlite::Row<'_>) -> rusqlite::Result<Node> {
        let kind: String = r.get(1)?;
        let language: String = r.get(5)?;
        let visibility: Option<String> = r.get(12)?;
        let decorators: Option<String> = r.get(17)?;
        let type_params: Option<String> = r.get(18)?;
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
            docstring: r.get(10)?,
            signature: r.get(11)?,
            visibility: visibility.as_deref().and_then(parse_visibility),
            is_exported: r.get::<_, i64>(13)? != 0,
            is_async: r.get::<_, i64>(14)? != 0,
            is_static: r.get::<_, i64>(15)? != 0,
            is_abstract: r.get::<_, i64>(16)? != 0,
            decorators: decorators.and_then(|d| serde_json::from_str(&d).ok()),
            type_parameters: type_params.and_then(|d| serde_json::from_str(&d).ok()),
            return_type: r.get(19)?,
            updated_at: 0,
        })
    }

    /// Fetch one node by id with all fields populated.
    pub fn get_node_by_id_full(&self, id: &str) -> Result<Option<Node>> {
        let sql = format!("SELECT {} FROM nodes WHERE id = ?1", NODE_COLS);
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let mut rows = stmt.query_map(params![id], Self::row_to_full)?;
        match rows.next() {
            Some(n) => Ok(Some(n?)),
            None => Ok(None),
        }
    }

    /// Exact-name lookup returning fully-populated nodes (for explore seeding).
    pub fn get_nodes_by_name_full(&self, name: &str, limit: usize) -> Result<Vec<Node>> {
        let sql = format!("SELECT {} FROM nodes WHERE name = ?1 LIMIT ?2", NODE_COLS);
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt
            .query_map(params![name, limit as i64], Self::row_to_full)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Batch-fetch nodes by id → map (port of `getNodesByIds`).
    pub fn get_nodes_by_ids(&self, ids: &[String]) -> Result<std::collections::HashMap<String, Node>> {
        let mut out = std::collections::HashMap::new();
        if ids.is_empty() {
            return Ok(out);
        }
        // Chunk to stay under SQLite's variable limit.
        for chunk in ids.chunks(500) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!("SELECT {} FROM nodes WHERE id IN ({})", NODE_COLS, placeholders);
            let mut stmt = self.conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> =
                chunk.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
            let rows = stmt
                .query_map(params.as_slice(), Self::row_to_full)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for n in rows {
                out.insert(n.id.clone(), n);
            }
        }
        Ok(out)
    }

    /// Outgoing edges with full fields (kinds filter optional). Wraps `edges_for`.
    pub fn get_outgoing_edges(&self, source: &str, kinds: &[EdgeKind]) -> Result<Vec<Edge>> {
        self.outgoing_edges(source, kinds)
    }
    /// Incoming edges with full fields (kinds filter optional).
    pub fn get_incoming_edges(&self, target: &str, kinds: &[EdgeKind]) -> Result<Vec<Edge>> {
        self.incoming_edges(target, kinds)
    }

    /// FTS5 + LIKE search with multi-signal rescoring (port of `searchNodes`).
    /// `kinds` empty = no kind filter. Scoring mirrors the TS: bm25 (or LIKE
    /// score) + kind_bonus + path_relevance + name_match_bonus.
    pub fn search_nodes(
        &self,
        query: &str,
        kinds: &[NodeKind],
        limit: usize,
        project_tokens: &std::collections::HashSet<String>,
    ) -> Result<Vec<SearchResult>> {
        use crate::search::query_utils::{kind_bonus, name_match_bonus, score_path_relevance};
        let mut results = self.search_fts(query, kinds, limit)?;
        if results.is_empty() && query.len() >= 2 {
            results = self.search_like(query, kinds, limit)?;
        }
        // Supplement: ensure exact name matches are candidates (bm25 can bury them).
        if !results.is_empty() && !query.is_empty() {
            let mut existing: std::collections::HashSet<String> =
                results.iter().map(|r| r.node.id.clone()).collect();
            let max_score = results.iter().map(|r| r.score).fold(0.0_f64, f64::max);
            for term in query.split_whitespace().filter(|t| t.len() >= 2) {
                let mut sql = format!(
                    "SELECT {} FROM nodes WHERE name = ?1 COLLATE NOCASE",
                    NODE_COLS
                );
                if !kinds.is_empty() {
                    sql += &format!(
                        " AND kind IN ({})",
                        kinds.iter().map(|_| "?").collect::<Vec<_>>().join(",")
                    );
                }
                sql += " LIMIT 20";
                let mut stmt = self.conn.prepare(&sql)?;
                let mut p: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(term.to_string())];
                for k in kinds {
                    p.push(Box::new(k.as_str().to_string()));
                }
                let pr: Vec<&dyn rusqlite::ToSql> = p.iter().map(|b| b.as_ref()).collect();
                let rows = stmt
                    .query_map(pr.as_slice(), Self::row_to_full)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for node in rows {
                    if existing.insert(node.id.clone()) {
                        results.push(SearchResult { node, score: max_score });
                    }
                }
            }
        }
        // Multi-signal rescore.
        if !results.is_empty() && !query.is_empty() {
            for r in results.iter_mut() {
                r.score += kind_bonus(r.node.kind) as f64
                    + score_path_relevance(&r.node.file_path, query, project_tokens) as f64
                    + name_match_bonus(&r.node.name, query) as f64;
            }
            results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.node.id.cmp(&b.node.id)));
            results.truncate(limit);
        }
        Ok(results)
    }

    /// FTS5 prefix search (port of `searchNodesFTS`). bm25 weights:
    /// id=0, name=20, qualified_name=5, docstring=1, signature=2, name_split=8.
    fn search_fts(&self, query: &str, kinds: &[NodeKind], limit: usize) -> Result<Vec<SearchResult>> {
        let cleaned: String = query.replace("::", " ");
        let terms: Vec<String> = cleaned
            .split_whitespace()
            .map(|t| t.chars().filter(|c| !"'\"*():^".contains(*c)).collect::<String>())
            .filter(|t| !t.is_empty())
            .filter(|t| !matches!(t.to_ascii_uppercase().as_str(), "AND" | "OR" | "NOT" | "NEAR"))
            .map(|t| format!("\"{}\"*", t))
            .collect();
        if terms.is_empty() {
            return Ok(vec![]);
        }
        let fts_query = terms.join(" OR ");
        let fts_limit = (limit * 5).max(100);
        let mut sql = format!(
            "SELECT {}, bm25(nodes_fts, 0, 20, 5, 1, 2, 8) AS score \
             FROM nodes_fts JOIN nodes ON nodes_fts.id = nodes.id \
             WHERE nodes_fts MATCH ?1",
            NODE_COLS
                .split(", ")
                .map(|c| format!("nodes.{}", c))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let mut p: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(fts_query)];
        if !kinds.is_empty() {
            sql += &format!(
                " AND nodes.kind IN ({})",
                kinds.iter().map(|_| "?").collect::<Vec<_>>().join(",")
            );
            for k in kinds {
                p.push(Box::new(k.as_str().to_string()));
            }
        }
        // `nodes.id` tiebreak makes the LIMIT cut deterministic on score ties.
        sql += " ORDER BY score, nodes.id LIMIT ?";
        p.push(Box::new(fts_limit as i64));
        let mut stmt = match self.conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return Ok(vec![]),
        };
        let pr: Vec<&dyn rusqlite::ToSql> = p.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(pr.as_slice(), |r| {
            let node = Self::row_to_full(r)?;
            let score: f64 = r.get(20)?;
            Ok(SearchResult { node, score: score.abs() })
        });
        match rows {
            Ok(it) => Ok(it.collect::<rusqlite::Result<Vec<_>>>()?),
            Err(_) => Ok(vec![]),
        }
    }

    /// LIKE substring fallback (port of `searchNodesLike`).
    fn search_like(&self, query: &str, kinds: &[NodeKind], limit: usize) -> Result<Vec<SearchResult>> {
        let starts_with = format!("{}%", query);
        let contains = format!("%{}%", query);
        let mut sql = format!(
            "SELECT {}, CASE \
               WHEN name = ?1 THEN 1.0 \
               WHEN name LIKE ?2 THEN 0.9 \
               WHEN name LIKE ?3 THEN 0.8 \
               WHEN qualified_name LIKE ?4 THEN 0.7 \
               ELSE 0.5 END AS score \
             FROM nodes WHERE (name LIKE ?5 OR qualified_name LIKE ?6 OR name LIKE ?7)",
            NODE_COLS
        );
        let mut p: Vec<Box<dyn rusqlite::ToSql>> = vec![
            Box::new(query.to_string()),
            Box::new(starts_with.clone()),
            Box::new(contains.clone()),
            Box::new(contains.clone()),
            Box::new(contains.clone()),
            Box::new(contains.clone()),
            Box::new(starts_with.clone()),
        ];
        if !kinds.is_empty() {
            sql += &format!(
                " AND kind IN ({})",
                kinds.iter().map(|_| "?").collect::<Vec<_>>().join(",")
            );
            for k in kinds {
                p.push(Box::new(k.as_str().to_string()));
            }
        }
        sql += " ORDER BY score DESC, length(name) ASC, id LIMIT ?";
        p.push(Box::new(limit as i64));
        let mut stmt = self.conn.prepare(&sql)?;
        let pr: Vec<&dyn rusqlite::ToSql> = p.iter().map(|b| b.as_ref()).collect();
        let rows = stmt
            .query_map(pr.as_slice(), |r| {
                let node = Self::row_to_full(r)?;
                let score: f64 = r.get(20)?;
                Ok(SearchResult { node, score })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Exact-name lookup with co-location boosting (port of `findNodesByExactName`).
    pub fn find_nodes_by_exact_name(
        &self,
        names: &[String],
        kinds: &[NodeKind],
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        if names.is_empty() {
            return Ok(vec![]);
        }
        // Pass 1: files containing each name; distinctive names hit <10 files.
        let mut distinctive_files: std::collections::HashSet<String> = std::collections::HashSet::new();
        for name in names {
            let mut sql = "SELECT DISTINCT file_path FROM nodes WHERE name COLLATE NOCASE = ?1".to_string();
            if !kinds.is_empty() {
                sql += &format!(" AND kind IN ({})", kinds.iter().map(|_| "?").collect::<Vec<_>>().join(","));
            }
            sql += " LIMIT 100";
            let mut stmt = self.conn.prepare(&sql)?;
            let mut p: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(name.clone())];
            for k in kinds {
                p.push(Box::new(k.as_str().to_string()));
            }
            let pr: Vec<&dyn rusqlite::ToSql> = p.iter().map(|b| b.as_ref()).collect();
            let files: Vec<String> = stmt
                .query_map(pr.as_slice(), |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if !files.is_empty() && files.len() < 10 {
                for f in files {
                    distinctive_files.insert(f);
                }
            }
        }
        // Pass 2: per-name query, score by co-location.
        let per_name_limit = (limit as f64 / names.len() as f64).ceil().max(8.0) as usize;
        let mut all: Vec<SearchResult> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for name in names {
            let mut sql = format!("SELECT {} FROM nodes WHERE name COLLATE NOCASE = ?1", NODE_COLS);
            if !kinds.is_empty() {
                sql += &format!(" AND kind IN ({})", kinds.iter().map(|_| "?").collect::<Vec<_>>().join(","));
            }
            sql += " LIMIT ?";
            let fetch = (per_name_limit * 3).max(50);
            let mut stmt = self.conn.prepare(&sql)?;
            let mut p: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(name.clone())];
            for k in kinds {
                p.push(Box::new(k.as_str().to_string()));
            }
            p.push(Box::new(fetch as i64));
            let pr: Vec<&dyn rusqlite::ToSql> = p.iter().map(|b| b.as_ref()).collect();
            let rows = stmt
                .query_map(pr.as_slice(), Self::row_to_full)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut name_results: Vec<SearchResult> = Vec::new();
            for node in rows {
                if seen.contains(&node.id) {
                    continue;
                }
                let boost = if distinctive_files.contains(&node.file_path) { 20.0 } else { 0.0 };
                name_results.push(SearchResult { node, score: 1.0 + boost });
            }
            name_results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.node.id.cmp(&b.node.id)));
            for r in name_results.into_iter().take(per_name_limit) {
                seen.insert(r.node.id.clone());
                all.push(r);
            }
        }
        all.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.node.id.cmp(&b.node.id)));
        all.truncate(limit);
        Ok(all)
    }

    /// Substring (LIKE) name search (port of `findNodesByNameSubstring`).
    pub fn find_nodes_by_name_substring(
        &self,
        substring: &str,
        kinds: &[NodeKind],
        limit: usize,
        exclude_prefix: bool,
    ) -> Result<Vec<SearchResult>> {
        let mut sql = format!("SELECT {} FROM nodes WHERE name LIKE ?1", NODE_COLS);
        let mut p: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(format!("%{}%", substring))];
        if exclude_prefix {
            sql += " AND name NOT LIKE ?";
            p.push(Box::new(format!("{}%", substring)));
        }
        if !kinds.is_empty() {
            sql += &format!(" AND kind IN ({})", kinds.iter().map(|_| "?").collect::<Vec<_>>().join(","));
            for k in kinds {
                p.push(Box::new(k.as_str().to_string()));
            }
        }
        sql += " ORDER BY length(name) ASC, id LIMIT ?";
        p.push(Box::new(limit as i64));
        let mut stmt = self.conn.prepare(&sql)?;
        let pr: Vec<&dyn rusqlite::ToSql> = p.iter().map(|b| b.as_ref()).collect();
        let rows = stmt
            .query_map(pr.as_slice(), |r| Ok(SearchResult { node: Self::row_to_full(r)?, score: 1.0 }))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The file holding the densest concentration of in-file edges (port of
    /// `getDominantFile`). Returns (path, edge_count, next_edge_count) or None.
    pub fn get_dominant_file(&self) -> Result<Option<(String, i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT n.file_path AS fp, COUNT(*) AS c \
             FROM edges e JOIN nodes n ON e.source = n.id JOIN nodes m ON e.target = m.id \
             WHERE n.file_path = m.file_path \
             GROUP BY n.file_path ORDER BY c DESC LIMIT 20",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let filtered: Vec<(String, i64)> = rows
            .into_iter()
            .filter(|(p, _)| !crate::search::query_utils::is_low_value_file(p))
            .collect();
        if filtered.is_empty() || filtered[0].1 < 20 {
            return Ok(None);
        }
        let next = filtered.get(1).map(|(_, c)| *c).unwrap_or(0);
        Ok(Some((filtered[0].0.clone(), filtered[0].1, next)))
    }

    /// All edges where both endpoints are in `node_ids` (port of `findEdgesBetweenNodes`).
    pub fn find_edges_between_nodes(&self, node_ids: &[String], kinds: &[EdgeKind]) -> Result<Vec<Edge>> {
        if node_ids.is_empty() {
            return Ok(vec![]);
        }
        let id_set: std::collections::HashSet<&str> = node_ids.iter().map(|s| s.as_str()).collect();
        // Pull candidate edges by source-in-set via chunked IN, then filter target.
        let mut out = Vec::new();
        for chunk in node_ids.chunks(400) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let mut sql = format!(
                "SELECT source, target, kind, metadata, line, col, provenance \
                 FROM edges WHERE source IN ({})",
                placeholders
            );
            if !kinds.is_empty() {
                sql += &format!(
                    " AND kind IN ({})",
                    kinds.iter().map(|_| "?").collect::<Vec<_>>().join(",")
                );
            }
            let mut stmt = self.conn.prepare(&sql)?;
            let mut p: Vec<Box<dyn rusqlite::ToSql>> =
                chunk.iter().map(|s| Box::new(s.clone()) as Box<dyn rusqlite::ToSql>).collect();
            for k in kinds {
                p.push(Box::new(k.as_str().to_string()));
            }
            let pr: Vec<&dyn rusqlite::ToSql> = p.iter().map(|b| b.as_ref()).collect();
            let rows = stmt
                .query_map(pr.as_slice(), |r| {
                    let kind: String = r.get(2)?;
                    let prov: Option<String> = r.get(6)?;
                    let meta: Option<String> = r.get(3)?;
                    Ok(Edge {
                        source: r.get(0)?,
                        target: r.get(1)?,
                        kind: parse_edge_kind(&kind),
                        metadata: meta.and_then(|m| serde_json::from_str(&m).ok()),
                        line: r.get(4)?,
                        col: r.get(5)?,
                        provenance: prov.as_deref().map(parse_provenance),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for e in rows {
                if id_set.contains(e.target.as_str()) {
                    out.push(e);
                }
            }
        }
        Ok(out)
    }

    /// Distinct indexed file paths matching (case-insensitive) `q` exactly, by
    /// suffix, or as a substring — for `codegraph_node` file resolution.
    pub fn distinct_file_paths_like(&self, q: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT file_path FROM nodes WHERE lower(file_path) LIKE ?1 LIMIT 50")?;
        let pat = format!("%{}%", q);
        let rows = stmt
            .query_map(params![pat], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// All symbols defined in `file_path`, ordered by start line (for the
    /// `codegraph_node` symbols-only file map).
    pub fn nodes_in_file(&self, file_path: &str) -> Result<Vec<Node>> {
        let sql = format!(
            "SELECT {} FROM nodes WHERE file_path = ?1 AND kind != 'file' ORDER BY start_line",
            NODE_COLS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![file_path], Self::row_to_full)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Distinct file paths that depend on `file_path` (port of `getDependentFilePaths`).
    pub fn get_dependent_file_paths(&self, file_path: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT src.file_path AS fp \
             FROM edges e JOIN nodes tgt ON tgt.id = e.target JOIN nodes src ON src.id = e.source \
             WHERE tgt.file_path = ?1 AND e.kind != 'contains' AND src.file_path != ?1",
        )?;
        let rows = stmt
            .query_map(params![file_path], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
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

fn parse_edge_kind(s: &str) -> EdgeKind {
    match s {
        "calls" => EdgeKind::Calls,
        "imports" => EdgeKind::Imports,
        "exports" => EdgeKind::Exports,
        "extends" => EdgeKind::Extends,
        "implements" => EdgeKind::Implements,
        "references" => EdgeKind::References,
        "type_of" => EdgeKind::TypeOf,
        "returns" => EdgeKind::Returns,
        "instantiates" => EdgeKind::Instantiates,
        "overrides" => EdgeKind::Overrides,
        "decorates" => EdgeKind::Decorates,
        _ => EdgeKind::Contains,
    }
}

fn parse_visibility(s: &str) -> Option<crate::types::Visibility> {
    use crate::types::Visibility;
    match s {
        "public" => Some(Visibility::Public),
        "private" => Some(Visibility::Private),
        "protected" => Some(Visibility::Protected),
        "internal" => Some(Visibility::Internal),
        _ => None,
    }
}

fn parse_provenance(s: &str) -> Provenance {
    match s {
        "heuristic" => Provenance::Heuristic,
        "scip" => Provenance::Scip,
        _ => Provenance::TreeSitter,
    }
}

fn parse_reference_kind(s: &str) -> crate::types::ReferenceKind {
    use crate::types::{EdgeKind, ReferenceKind};
    match s {
        "function_ref" => ReferenceKind::FunctionRef,
        "contains" => ReferenceKind::Edge(EdgeKind::Contains),
        "calls" => ReferenceKind::Edge(EdgeKind::Calls),
        "imports" => ReferenceKind::Edge(EdgeKind::Imports),
        "exports" => ReferenceKind::Edge(EdgeKind::Exports),
        "extends" => ReferenceKind::Edge(EdgeKind::Extends),
        "implements" => ReferenceKind::Edge(EdgeKind::Implements),
        "type_of" => ReferenceKind::Edge(EdgeKind::TypeOf),
        "returns" => ReferenceKind::Edge(EdgeKind::Returns),
        "instantiates" => ReferenceKind::Edge(EdgeKind::Instantiates),
        "overrides" => ReferenceKind::Edge(EdgeKind::Overrides),
        "decorates" => ReferenceKind::Edge(EdgeKind::Decorates),
        _ => ReferenceKind::Edge(EdgeKind::References),
    }
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
