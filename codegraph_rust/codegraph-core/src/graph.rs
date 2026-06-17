//! Graph traversal — port of `graph/traversal.ts` (callers/callees/path).
//!
//! Queries the DB per call (interactive use), matching the TS `GraphTraverser`.
//! `instantiates` counts as a call edge in both directions (#774) so constructing
//! a type is "calling" it and trace can cross the function→type→method boundary.

use crate::db::Store;
use crate::types::{Edge, EdgeKind, Node};
use anyhow::Result;
use indexmap::IndexMap;
use std::collections::{HashSet, VecDeque};

/// Retrieval confidence signal for the context handoff footer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    High,
    Low,
}

/// A subgraph of relevant nodes + edges (port of TS `Subgraph`). `nodes` is
/// insertion-ordered (mirrors the TS `Map`) so output is deterministic.
#[derive(Debug, Default)]
pub struct Subgraph {
    pub nodes: IndexMap<String, Node>,
    pub edges: Vec<Edge>,
    pub roots: Vec<String>,
    pub confidence: Option<Confidence>,
}

/// Direction for `traverse_bfs`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Outgoing,
    Incoming,
    Both,
}

/// Options for `traverse_bfs` (subset of TS `TraversalOptions`).
pub struct TraversalOptions {
    pub max_depth: u32,
    pub edge_kinds: Vec<EdgeKind>,
    pub node_kinds: Vec<crate::types::NodeKind>,
    pub direction: Direction,
    pub limit: usize,
}

impl Default for TraversalOptions {
    fn default() -> Self {
        TraversalOptions {
            max_depth: 1,
            edge_kinds: vec![],
            node_kinds: vec![],
            direction: Direction::Outgoing,
            limit: 1000,
        }
    }
}

fn adjacent_edges(store: &Store, node_id: &str, dir: Direction, kinds: &[EdgeKind]) -> Result<Vec<Edge>> {
    Ok(match dir {
        Direction::Outgoing => store.outgoing_edges(node_id, kinds)?,
        Direction::Incoming => store.incoming_edges(node_id, kinds)?,
        Direction::Both => {
            let mut o = store.outgoing_edges(node_id, kinds)?;
            o.extend(store.incoming_edges(node_id, kinds)?);
            o
        }
    })
}

/// BFS traversal from `start_id` (port of `GraphTraverser.traverseBFS`).
/// Structural edges (contains, calls) are visited before references.
pub fn traverse_bfs(store: &Store, start_id: &str, opts: &TraversalOptions) -> Result<Subgraph> {
    let Some(start) = store.get_node_by_id_full(start_id)? else {
        return Ok(Subgraph::default());
    };
    let mut nodes: IndexMap<String, Node> = IndexMap::new();
    let mut edges: Vec<Edge> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    nodes.insert(start.id.clone(), start.clone());
    let mut queue: VecDeque<(Node, Option<Edge>, u32)> = VecDeque::new();
    queue.push_back((start, None, 0));

    while let Some((node, edge, depth)) = queue.pop_front() {
        if nodes.len() >= opts.limit {
            break;
        }
        if visited.contains(&node.id) {
            continue;
        }
        visited.insert(node.id.clone());
        if let Some(e) = edge {
            edges.push(e);
        }
        if depth >= opts.max_depth {
            continue;
        }
        let mut adj = adjacent_edges(store, &node.id, opts.direction, &opts.edge_kinds)?;
        adj.sort_by_key(|e| match e.kind {
            EdgeKind::Contains => 0,
            EdgeKind::Calls => 1,
            _ => 2,
        });
        let want_ids: Vec<String> = adj
            .iter()
            .map(|e| if e.source == node.id { e.target.clone() } else { e.source.clone() })
            .filter(|id| !visited.contains(id))
            .collect();
        let neighbors = store.get_nodes_by_ids(&want_ids)?;
        for adj_edge in adj {
            let next_id = if adj_edge.source == node.id { &adj_edge.target } else { &adj_edge.source };
            if visited.contains(next_id) {
                continue;
            }
            let Some(next) = neighbors.get(next_id) else { continue };
            if !opts.node_kinds.is_empty() && !opts.node_kinds.contains(&next.kind) {
                continue;
            }
            nodes.insert(next.id.clone(), next.clone());
            queue.push_back((next.clone(), Some(adj_edge.clone()), depth + 1));
        }
    }
    Ok(Subgraph { nodes, edges, roots: vec![start_id.to_string()], confidence: None })
}

/// Type hierarchy (ancestors + descendants via extends/implements) — port of
/// `getTypeHierarchy`.
pub fn get_type_hierarchy(store: &Store, node_id: &str) -> Result<Subgraph> {
    let Some(focal) = store.get_node_by_id_full(node_id)? else {
        return Ok(Subgraph::default());
    };
    let mut nodes: IndexMap<String, Node> = IndexMap::new();
    let mut edges: Vec<Edge> = Vec::new();
    nodes.insert(focal.id.clone(), focal);
    let mut visited: HashSet<String> = HashSet::new();
    type_ancestors(store, node_id, &mut nodes, &mut edges, &mut visited)?;
    visited.clear();
    type_descendants(store, node_id, &mut nodes, &mut edges, &mut visited)?;
    Ok(Subgraph { nodes, edges, roots: vec![node_id.to_string()], confidence: None })
}

fn type_ancestors(
    store: &Store,
    node_id: &str,
    nodes: &mut IndexMap<String, Node>,
    edges: &mut Vec<Edge>,
    visited: &mut HashSet<String>,
) -> Result<()> {
    if !visited.insert(node_id.to_string()) {
        return Ok(());
    }
    let out = store.outgoing_edges(node_id, &[EdgeKind::Extends, EdgeKind::Implements])?;
    for e in out {
        if !nodes.contains_key(&e.target) {
            if let Some(parent) = store.get_node_by_id_full(&e.target)? {
                let pid = parent.id.clone();
                nodes.insert(pid.clone(), parent);
                edges.push(e);
                type_ancestors(store, &pid, nodes, edges, visited)?;
            }
        }
    }
    Ok(())
}

fn type_descendants(
    store: &Store,
    node_id: &str,
    nodes: &mut IndexMap<String, Node>,
    edges: &mut Vec<Edge>,
    visited: &mut HashSet<String>,
) -> Result<()> {
    if !visited.insert(node_id.to_string()) {
        return Ok(());
    }
    let inc = store.incoming_edges(node_id, &[EdgeKind::Extends, EdgeKind::Implements])?;
    for e in inc {
        if !nodes.contains_key(&e.source) {
            if let Some(child) = store.get_node_by_id_full(&e.source)? {
                let cid = child.id.clone();
                nodes.insert(cid.clone(), child);
                edges.push(e);
                type_descendants(store, &cid, nodes, edges, visited)?;
            }
        }
    }
    Ok(())
}

/// Edge kinds traversed for callers/callees (TS uses this exact set).
const CALL_EDGE_KINDS: &[EdgeKind] = &[
    EdgeKind::Calls,
    EdgeKind::References,
    EdgeKind::Imports,
    EdgeKind::Instantiates,
];

/// A traversal hop: the reached node and the edge taken to get there.
pub struct Hop {
    pub node: Node,
    pub edge: Edge,
    pub depth: u32,
}

/// Callers of `node_id` (incoming call/ref/import/instantiate edges), to `max_depth`.
pub fn callers(store: &Store, node_id: &str, max_depth: u32) -> Result<Vec<Hop>> {
    let mut out = Vec::new();
    let mut visited = HashSet::new();
    callers_rec(store, node_id, max_depth, 0, &mut out, &mut visited)?;
    Ok(out)
}

fn callers_rec(
    store: &Store,
    node_id: &str,
    max_depth: u32,
    depth: u32,
    out: &mut Vec<Hop>,
    visited: &mut HashSet<String>,
) -> Result<()> {
    if depth >= max_depth || visited.contains(node_id) {
        return Ok(());
    }
    visited.insert(node_id.to_string());
    for edge in store.incoming_edges(node_id, CALL_EDGE_KINDS)? {
        if visited.contains(&edge.source) {
            continue;
        }
        if let Some(node) = store.node_by_id(&edge.source)? {
            let src = node.id.clone();
            out.push(Hop { node, edge, depth });
            callers_rec(store, &src, max_depth, depth + 1, out, visited)?;
        }
    }
    Ok(())
}

/// Callees of `node_id` (outgoing call/ref/import/instantiate edges), to `max_depth`.
pub fn callees(store: &Store, node_id: &str, max_depth: u32) -> Result<Vec<Hop>> {
    let mut out = Vec::new();
    let mut visited = HashSet::new();
    callees_rec(store, node_id, max_depth, 0, &mut out, &mut visited)?;
    Ok(out)
}

fn callees_rec(
    store: &Store,
    node_id: &str,
    max_depth: u32,
    depth: u32,
    out: &mut Vec<Hop>,
    visited: &mut HashSet<String>,
) -> Result<()> {
    if depth >= max_depth || visited.contains(node_id) {
        return Ok(());
    }
    visited.insert(node_id.to_string());
    for edge in store.outgoing_edges(node_id, CALL_EDGE_KINDS)? {
        if visited.contains(&edge.target) {
            continue;
        }
        if let Some(node) = store.node_by_id(&edge.target)? {
            let tgt = node.id.clone();
            out.push(Hop { node, edge, depth });
            callees_rec(store, &tgt, max_depth, depth + 1, out, visited)?;
        }
    }
    Ok(())
}

/// A node + the edge taken to reach it (edge is None for the path's start).
pub struct PathStep {
    pub node: Node,
    pub edge: Option<Edge>,
}

/// Shortest path `from_id → to_id` via outgoing edges (BFS). Port of `findPath`.
pub fn find_path(
    store: &Store,
    from_id: &str,
    to_id: &str,
    edge_kinds: &[EdgeKind],
) -> Result<Option<Vec<PathStep>>> {
    let Some(from) = store.node_by_id(from_id)? else {
        return Ok(None);
    };
    if store.node_by_id(to_id)?.is_none() {
        return Ok(None);
    }
    let mut visited = HashSet::new();
    // Each queue entry carries the full path so far (ids + the edge taken).
    let mut queue: std::collections::VecDeque<(String, Vec<(String, Option<Edge>)>)> =
        std::collections::VecDeque::new();
    queue.push_back((from_id.to_string(), vec![(from.id.clone(), None)]));

    while let Some((id, path)) = queue.pop_front() {
        if id == to_id {
            // Materialize node objects for the path.
            let mut steps = Vec::with_capacity(path.len());
            for (nid, edge) in path {
                if let Some(n) = store.node_by_id(&nid)? {
                    steps.push(PathStep { node: n, edge });
                }
            }
            return Ok(Some(steps));
        }
        if visited.contains(&id) {
            continue;
        }
        visited.insert(id.clone());
        for edge in store.outgoing_edges(&id, edge_kinds)? {
            if !visited.contains(&edge.target) {
                let mut next = path.clone();
                next.push((edge.target.clone(), Some(edge.clone())));
                queue.push_back((edge.target.clone(), next));
            }
        }
    }
    Ok(None)
}
