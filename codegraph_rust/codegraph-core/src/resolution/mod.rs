//! Resolution — turn `unresolved_refs` into edges (port of `resolution/index.ts`,
//! Go path). Strategy chain per ref: built-in filter → possible-match pre-filter
//! → Go cross-package import resolution → name-matcher. Resolved refs become
//! `calls`/`references`/`imports`/`instantiates`/`extends` edges (with the TS
//! promotions: extends→implements when the target is an interface; calls→
//! instantiates when the target is a struct). Then the Go synthesizers run.

mod builtins;
pub mod graph;
mod name_matcher;
mod synth;

use crate::db::Store;
use crate::types::{
    Edge, EdgeKind, Language, Node, NodeKind, Provenance, ReferenceKind, UnresolvedReference,
};
use anyhow::Result;
use graph::Graph;
use name_matcher::Resolved;
use serde_json::json;
use std::collections::HashSet;
use std::path::Path;

/// Stats from a resolution pass (for parity reporting).
#[derive(Debug, Default, Clone)]
pub struct ResolveStats {
    pub total_refs: usize,
    pub resolved: usize,
    pub base_edges: usize,
    pub synthesized_edges: usize,
}

/// Resolve all unresolved references for an indexed repo and persist the edges.
/// Reads nodes/edges/unresolved from `store`, resolves in-memory, writes the new
/// edges, and clears the unresolved_refs table (matching the TS end state).
pub fn resolve(repo_root: &Path, store: &mut Store) -> Result<ResolveStats> {
    let repo_root = repo_root.canonicalize().unwrap_or_else(|_| repo_root.to_path_buf());
    let repo_root = repo_root.as_path();
    let nodes: Vec<Node> = store.all_nodes()?;
    let edges: Vec<Edge> = Vec::new(); // base `contains` edges are reloaded below
    let contains = store.all_contains_edges()?;
    let unresolved = store.all_unresolved()?;

    let mut g = Graph::build(nodes, edges, repo_root);
    g.add_edges(contains); // seed with extraction's contains edges
    g.build_adjacency();

    // Pre-extract import mappings for every file that has a Go ref (so the
    // resolution loop reads them immutably).
    let go_ref_files: HashSet<String> = unresolved
        .iter()
        .filter(|r| r.language == Some(Language::Go))
        .filter_map(|r| r.file_path.clone())
        .collect();
    g.prewarm_imports(go_ref_files);

    // Resolve each ref.
    let mut resolved_edges: Vec<Edge> = Vec::new();
    let mut resolved_count = 0usize;
    for r in &unresolved {
        if let Some(res) = resolve_one(&g, r) {
            resolved_count += 1;
            resolved_edges.push(build_edge(&g, r, &res));
        }
    }

    // Make resolved extends/implements visible to the synthesizers, then run them.
    g.add_edges(resolved_edges.clone());
    let synth_edges = synth::synthesize(&mut g);

    let base = resolved_edges.len();
    let synth_n = synth_edges.len();
    let mut all = resolved_edges;
    all.extend(synth_edges);
    store.insert_edges(&all)?;
    store.clear_unresolved()?;

    Ok(ResolveStats {
        total_refs: unresolved.len(),
        resolved: resolved_count,
        base_edges: base,
        synthesized_edges: synth_n,
    })
}

/// Resolve a single ref (port of `resolveOne`, Go path).
fn resolve_one(g: &Graph, r: &UnresolvedReference) -> Option<Resolved> {
    if builtins::is_builtin_or_external(g, r) {
        return None;
    }
    if !has_any_possible_match(g, &r.reference_name) {
        return None;
    }

    // Strategy: Go cross-package import resolution (`pkg.Func`) — high confidence.
    if r.language == Some(Language::Go) {
        if let Some(res) = resolve_go_cross_package(g, r) {
            return Some(res); // confidence 0.9 → accept immediately (TS behavior)
        }
    }

    // Strategy: name-matcher.
    name_matcher::match_reference(g, r)
}

/// Build the edge for a resolved ref, applying the TS promotions
/// (extends→implements, calls→instantiates).
fn build_edge(g: &Graph, r: &UnresolvedReference, res: &Resolved) -> Edge {
    let mut kind = match r.reference_kind {
        ReferenceKind::FunctionRef => EdgeKind::References,
        ReferenceKind::Edge(k) => k,
    };
    let target = g.node_by_id(&res.target_id);

    if kind == EdgeKind::Extends {
        if let Some(t) = target {
            if matches!(t.kind, NodeKind::Interface | NodeKind::Protocol) {
                if let Some(src) = g.node_by_id(&r.from_node_id) {
                    if !matches!(src.kind, NodeKind::Interface | NodeKind::Protocol) {
                        kind = EdgeKind::Implements;
                    }
                }
            }
        }
    }
    if kind == EdgeKind::Calls {
        if let Some(t) = target {
            if matches!(t.kind, NodeKind::Class | NodeKind::Struct) {
                kind = EdgeKind::Instantiates;
            }
        }
    }

    let mut e = Edge::new(r.from_node_id.clone(), res.target_id.clone(), kind);
    e.line = Some(r.line);
    e.col = Some(r.col);
    e.provenance = Some(Provenance::TreeSitter);
    e.metadata = Some(json!({ "confidence": res.confidence, "resolvedBy": res.resolved_by }));
    e
}

/// Port of `hasAnyPossibleMatch` (Go-relevant): name exists, or a dotted/`::`/path
/// segment exists.
fn has_any_possible_match(g: &Graph, name: &str) -> bool {
    if g.known_name(name) {
        return true;
    }
    if let Some(dot) = name.find('.') {
        let recv = &name[..dot];
        let member = &name[dot + 1..];
        if g.known_name(recv) || g.known_name(member) {
            return true;
        }
        let cap = {
            let mut c = recv.chars();
            c.next()
                .map(|f| f.to_ascii_uppercase().to_string() + c.as_str())
                .unwrap_or_default()
        };
        if g.known_name(&cap) {
            return true;
        }
        if let Some(ld) = name.rfind('.') {
            if ld > dot && g.known_name(&name[ld + 1..]) {
                return true;
            }
        }
    }
    if let Some(c) = name.find("::") {
        if g.known_name(&name[..c]) || g.known_name(&name[c + 2..]) {
            return true;
        }
        if let Some(lc) = name.rfind("::") {
            if lc > c && g.known_name(&name[lc + 2..]) {
                return true;
            }
        }
    }
    if let Some(slash) = name.rfind('/') {
        if g.known_name(&name[slash + 1..]) {
            return true;
        }
    }
    false
}

/// Port of `resolveGoCrossPackageReference`: `pkga.FuncX` → the exported `FuncX`
/// in the package directory `pkga`'s import path maps to.
fn resolve_go_cross_package(g: &Graph, r: &UnresolvedReference) -> Option<Resolved> {
    let modu = g.go_module()?;
    let name = &r.reference_name;
    let dot = name.find('.')?;
    if dot == 0 {
        return None;
    }
    let receiver = &name[..dot];
    let member = &name[dot + 1..];
    if member.is_empty() {
        return None;
    }
    let file = r.file_path.as_deref().unwrap_or("");
    for imp in g.cached_imports(file) {
        if imp.local_name != receiver {
            continue;
        }
        if imp.source != modu.module_path && !imp.source.starts_with(&format!("{}/", modu.module_path)) {
            continue;
        }
        let pkg_dir = if imp.source == modu.module_path {
            ""
        } else {
            &imp.source[modu.module_path.len() + 1..]
        };
        for &i in g.nodes_by_name(member) {
            let node = g.node(i);
            if node.language != Language::Go || !node.is_exported {
                continue;
            }
            let fp = node.file_path.replace('\\', "/");
            let file_dir = match fp.rfind('/') {
                Some(j) => &fp[..j],
                None => "",
            };
            if file_dir == pkg_dir {
                return Some(Resolved {
                    target_id: node.id.clone(),
                    confidence: name_matcher::ladder::IMPORT_MAP,
                    resolved_by: "import",
                });
            }
        }
    }
    None
}
