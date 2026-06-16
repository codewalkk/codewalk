//! Dynamic-dispatch synthesizers — port of `callback-synthesizer.ts` (Go channels).
//!
//! Three Go passes, in the TS order (each persisted before the next reads it):
//!  1. `goCrossFileMethodContainsEdges` — struct→method `contains` when the method
//!     is declared in a different file from its receiver type (deterministic, NOT
//!     heuristic). Go methods are commonly split across files in a package.
//!  2. `goImplementsEdges` — struct→interface `implements` when the struct's
//!     method-name set ⊇ the interface's (Go has no `implements` keyword).
//!  3. `interfaceOverrideEdges` — interface-method → concrete same-name method
//!     `calls` edge, so a call through an interface reaches the implementation.
//!
//! Channels 2 & 3 are `provenance = Heuristic` with `synthesizedBy` metadata —
//! the honesty invariant. The flow must close end-to-end (interface dispatch only
//! pays off once both `implements` and the override bridge exist).

use super::graph::Graph;
use crate::types::{Edge, EdgeKind, Language, NodeKind, Provenance};
use serde_json::json;
use std::collections::{HashMap, HashSet};

const MAX_CALLBACKS_PER_CHANNEL: usize = 40;

/// Run the Go synthesizer passes in order, mutating `graph.edges` between passes
/// (so pass 2 sees pass 1's `contains` edges). Returns the synthesized edges to
/// persist (the caller writes them to the DB).
pub fn synthesize(graph: &mut Graph) -> Vec<Edge> {
    graph.build_adjacency();

    // Pass 1: cross-file contains (deterministic) — must land before pass 2,
    // which derives method sets from contains edges.
    let contains = go_cross_file_method_contains(graph);
    graph.add_edges(contains.clone());

    // Pass 2: implicit implements.
    let implements = go_implements(graph);
    graph.add_edges(implements.clone());

    // Pass 3: interface-dispatch override bridge (reads implements edges).
    let overrides = interface_override(graph);

    let mut out = contains;
    out.extend(implements);
    out.extend(overrides);
    out
}

/// The method-NAME set of a type (its `contains`-linked `method` children).
fn method_name_set(graph: &Graph, type_id: &str) -> HashSet<String> {
    graph
        .outgoing(type_id, &[EdgeKind::Contains])
        .into_iter()
        .filter_map(|e| graph.node_by_id(&e.target))
        .filter(|n| n.kind == NodeKind::Method)
        .map(|n| n.name.clone())
        .collect()
}

fn dir_of(p: &str) -> &str {
    match p.rfind('/') {
        Some(i) => &p[..i],
        None => "",
    }
}

/// Port of `goCrossFileMethodContainsEdges`.
fn go_cross_file_method_contains(graph: &Graph) -> Vec<Edge> {
    let type_kinds = [
        NodeKind::Struct,
        NodeKind::Class,
        NodeKind::Interface,
        NodeKind::Enum,
        NodeKind::TypeAlias,
    ];
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for &mi in graph.nodes_by_kind(NodeKind::Method) {
        let m = graph.node(mi);
        if m.language != Language::Go {
            continue;
        }
        // Receiver is encoded as the qualified-name prefix `Recv::name`.
        let qn = &m.qualified_name;
        let Some(sep) = qn.rfind("::") else { continue };
        if sep == 0 {
            continue;
        }
        let receiver = &qn[..sep];
        if receiver.is_empty() {
            continue;
        }
        // Already attached to a type parent (same-file case)?
        let has_type_parent = graph
            .incoming(&m.id, &[EdgeKind::Contains])
            .into_iter()
            .any(|e| {
                graph
                    .node_by_id(&e.source)
                    .map(|s| type_kinds.contains(&s.kind))
                    .unwrap_or(false)
            });
        if has_type_parent {
            continue;
        }
        // Find the receiver type in the SAME directory (= same Go package).
        let dir = dir_of(&m.file_path);
        let owner = graph.nodes_by_name(receiver).iter().copied().find(|&oi| {
            let o = graph.node(oi);
            o.language == Language::Go && type_kinds.contains(&o.kind) && dir_of(&o.file_path) == dir
        });
        let Some(oi) = owner else { continue };
        let owner_id = graph.node(oi).id.clone();
        let key = format!("{}>{}", owner_id, m.id);
        if !seen.insert(key) {
            continue;
        }
        let mut e = Edge::new(owner_id, m.id.clone(), EdgeKind::Contains);
        e.line = Some(m.start_line);
        out.push(e);
    }
    out
}

/// Port of `goImplementsEdges`.
fn go_implements(graph: &Graph) -> Vec<Edge> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    let go_structs: Vec<usize> = graph
        .nodes_by_kind(NodeKind::Struct)
        .iter()
        .copied()
        .filter(|&i| graph.node(i).language == Language::Go)
        .collect();
    let struct_methods: HashMap<String, HashSet<String>> = go_structs
        .iter()
        .map(|&i| {
            let id = graph.node(i).id.clone();
            let set = method_name_set(graph, &id);
            (id, set)
        })
        .collect();

    for &ii in graph.nodes_by_kind(NodeKind::Interface) {
        let iface = graph.node(ii);
        if iface.language != Language::Go {
            continue;
        }
        let want = method_name_set(graph, &iface.id);
        if want.is_empty() {
            continue; // empty interface (`any`) would match everything
        }
        let mut added = 0;
        for &si in &go_structs {
            if added >= MAX_CALLBACKS_PER_CHANNEL {
                break;
            }
            let s = graph.node(si);
            let Some(have) = struct_methods.get(&s.id) else {
                continue;
            };
            if have.len() < want.len() {
                continue;
            }
            if !want.iter().all(|m| have.contains(m)) {
                continue;
            }
            let key = format!("{}>{}", s.id, iface.id);
            if !seen.insert(key) {
                continue;
            }
            let mut e = Edge::new(s.id.clone(), iface.id.clone(), EdgeKind::Implements);
            e.line = Some(s.start_line);
            e.provenance = Some(Provenance::Heuristic);
            e.metadata = Some(json!({
                "synthesizedBy": "go-implements",
                "via": iface.name,
                "registeredAt": format!("{}:{}", s.file_path, s.start_line),
            }));
            out.push(e);
            added += 1;
        }
    }
    out
}

/// Port of `interfaceOverrideEdges` (Go-relevant: concrete kind = struct).
fn interface_override(graph: &Graph) -> Vec<Edge> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let methods_of = |class_id: &str| -> Vec<usize> {
        graph
            .outgoing(class_id, &[EdgeKind::Contains])
            .into_iter()
            .filter_map(|e| graph.idx_of(&e.target))
            .filter(|&i| graph.node(i).kind == NodeKind::Method)
            .collect()
    };

    for kind in [NodeKind::Class, NodeKind::Struct] {
        for &ci in graph.nodes_by_kind(kind) {
            let cls = graph.node(ci);
            // Gate to languages with the interface-override shape (Go included).
            let impl_methods: Vec<usize> = methods_of(&cls.id)
                .into_iter()
                .filter(|&i| iface_override_lang(graph.node(i).language))
                .collect();
            if impl_methods.is_empty() {
                continue;
            }
            // Group impl methods by name (overloads).
            let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
            for &m in &impl_methods {
                by_name.entry(graph.node(m).name.clone()).or_default().push(m);
            }
            let cls_id = cls.id.clone();
            for sup in graph.outgoing(&cls_id, &[EdgeKind::Implements, EdgeKind::Extends]) {
                let Some(base) = graph.node_by_id(&sup.target) else {
                    continue;
                };
                if !iface_override_lang(base.language) || base.id == cls_id {
                    continue;
                }
                let base_id = base.id.clone();
                let mut added = 0;
                for &bmi in &methods_of(&base_id) {
                    if added >= MAX_CALLBACKS_PER_CHANNEL {
                        break;
                    }
                    let bm = graph.node(bmi);
                    let bm_id = bm.id.clone();
                    let bm_name = bm.name.clone();
                    let bm_line = bm.start_line;
                    for &mi in by_name.get(&bm_name).map(|v| v.as_slice()).unwrap_or(&[]) {
                        if added >= MAX_CALLBACKS_PER_CHANNEL {
                            break;
                        }
                        let m = graph.node(mi);
                        if bm_id == m.id {
                            continue;
                        }
                        let key = format!("{}>{}", bm_id, m.id);
                        if !seen.insert(key) {
                            continue;
                        }
                        let mut e = Edge::new(bm_id.clone(), m.id.clone(), EdgeKind::Calls);
                        e.line = Some(bm_line);
                        e.provenance = Some(Provenance::Heuristic);
                        e.metadata = Some(json!({
                            "synthesizedBy": "interface-impl",
                            "via": m.name,
                            "registeredAt": format!("{}:{}", m.file_path, m.start_line),
                        }));
                        out.push(e);
                        added += 1;
                    }
                }
            }
        }
    }
    out
}

fn iface_override_lang(l: Language) -> bool {
    matches!(
        l,
        Language::Java
            | Language::Kotlin
            | Language::Csharp
            | Language::Typescript
            | Language::Javascript
            | Language::Swift
            | Language::Scala
            | Language::Go
            | Language::Rust
    )
}
