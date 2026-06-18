//! Name matcher — port of `resolution/name-matcher.ts` (Go-relevant strategies).
//!
//! Strategy order (matchReference): qualified-name → Go dotted-call chain
//! (`New().Method`) → method-call (`recv.method`) → exact-name. Fuzzy/lower-name
//! matching is intentionally omitted for now (lowest confidence, adds noise);
//! it's a clean follow-on if a parity gap shows it matters.

use super::graph::Graph;
use crate::types::{Language, NodeKind, UnresolvedReference};

/// Explicit confidence ladder (CBM `registry.c:625` borrow). These are the
/// floors stored on each resolved edge so the retrieval layer has a principled
/// signal. Higher = more certain the target is correct.
pub mod ladder {
    /// Cross-package import binding (`pkg.Func` → the package's exported def).
    pub const IMPORT_MAP: f64 = 0.95;
    /// Same exact qualified name, unique.
    pub const QUALIFIED_EXACT: f64 = 0.95;
    /// Same module / same-file unique name.
    pub const SAME_MODULE: f64 = 0.90;
    /// Qualified-name suffix / import-path suffix match.
    pub const IMPORT_MAP_SUFFIX: f64 = 0.85;
    /// Unique same-language name across the codebase.
    pub const UNIQUE_NAME: f64 = 0.75;
    /// Suffix / receiver-word overlap among several candidates.
    pub const SUFFIX_MATCH: f64 = 0.55;
    /// Fuzzy / best-of-many with weak proximity.
    pub const FUZZY_NEAR: f64 = 0.40;
    /// Fuzzy / cross-language best-effort.
    pub const FUZZY_FAR: f64 = 0.30;
}

/// A successful resolution: the target node id + confidence + which strategy.
pub struct Resolved {
    pub target_id: String,
    pub confidence: f64,
    pub resolved_by: &'static str,
}

fn lang(r: &UnresolvedReference) -> Language {
    r.language.unwrap_or(Language::Unknown)
}
fn file(r: &UnresolvedReference) -> &str {
    r.file_path.as_deref().unwrap_or("")
}

/// Two languages sharing a type system. Go is a singleton family (`go` only).
pub fn same_language_family(a: Language, b: Language) -> bool {
    if a == b {
        return true;
    }
    fn fam(l: Language) -> Option<&'static str> {
        match l {
            Language::Java | Language::Kotlin | Language::Scala => Some("jvm"),
            Language::Swift | Language::Objc => Some("apple"),
            Language::Typescript | Language::Tsx | Language::Javascript | Language::Jsx => {
                Some("web")
            }
            Language::C | Language::Cpp => Some("c"),
            _ => None,
        }
    }
    matches!((fam(a), fam(b)), (Some(x), Some(y)) if x == y)
}

/// Directory-segment proximity (port of `computePathProximity`): 15 pts/shared
/// leading dir segment, capped at 80.
fn path_proximity(a: &str, b: &str) -> i64 {
    let da: Vec<&str> = a.split('/').collect();
    let db: Vec<&str> = b.split('/').collect();
    let da = &da[..da.len().saturating_sub(1)];
    let db = &db[..db.len().saturating_sub(1)];
    let mut shared = 0i64;
    for i in 0..da.len().min(db.len()) {
        if da[i] == db[i] {
            shared += 1;
        } else {
            break;
        }
    }
    (shared * 15).min(80)
}

/// Top-level entry (port of `matchReference`).
pub fn match_reference(graph: &Graph, r: &UnresolvedReference) -> Option<Resolved> {
    let name = &r.reference_name;

    // 1. Qualified-name match (name contains :: or .)
    if name.contains("::") || name.contains('.') {
        if let Some(res) = match_by_qualified_name(graph, r) {
            return Some(res);
        }
    }

    // 1d. Go dotted chained static-factory / fluent call `New().Method`.
    if lang(r) == Language::Go {
        if let Some(res) = match_dotted_call_chain_go(graph, r) {
            return Some(res);
        }
    }

    // 2. Method-call pattern `obj.method` / `Class::method`.
    if let Some(res) = match_method_call(graph, r) {
        return Some(res);
    }

    // 3. Exact-name match.
    match_by_exact_name(graph, r)
}

/// Port of `matchByQualifiedName`.
fn match_by_qualified_name(graph: &Graph, r: &UnresolvedReference) -> Option<Resolved> {
    let exact = graph.nodes_by_qname(&r.reference_name);
    if exact.len() == 1 {
        return Some(Resolved {
            target_id: graph.node(exact[0]).id.clone(),
            confidence: ladder::QUALIFIED_EXACT,
            resolved_by: "qualified-name",
        });
    }
    // Partial: last segment, then any node whose qualified name ends with the ref.
    let last = r.reference_name.split([':', '.']).last()?;
    for &i in graph.nodes_by_name(last) {
        let n = graph.node(i);
        if n.qualified_name.ends_with(&r.reference_name) {
            return Some(Resolved {
                target_id: n.id.clone(),
                confidence: ladder::IMPORT_MAP_SUFFIX,
                resolved_by: "qualified-name",
            });
        }
    }
    None
}

/// The captured return type of a bare function or `Type::method` (port of
/// `lookupCalleeReturnType`, Go subset). Reads the indexed node's `return_type`.
fn lookup_callee_return_type(graph: &Graph, callee: &str, l: Language) -> Option<String> {
    let (cls, method) = if let Some(idx) = callee.rfind("::") {
        (Some(&callee[..idx]), &callee[idx + 2..])
    } else {
        (None, callee)
    };
    let cands = graph.nodes_by_name(method);
    if let Some(cls) = cls {
        let want = format!("{}::{}", cls, method);
        for &i in cands {
            let n = graph.node(i);
            if (n.kind == NodeKind::Method || n.kind == NodeKind::Function)
                && n.language == l
                && n.return_type.is_some()
                && (n.qualified_name == want
                    || n.qualified_name.ends_with(&format!("::{}", want))
                    || want.ends_with(&n.qualified_name))
            {
                return n.return_type.clone();
            }
        }
        return None;
    }
    for &i in cands {
        let n = graph.node(i);
        if n.kind == NodeKind::Function && n.language == l && n.return_type.is_some() {
            return n.return_type.clone();
        }
    }
    None
}

/// Resolve `method` on `type_name` (port of `resolveMethodOnType`, base case):
/// a method whose qualified name is `Type::method` or ends with `::Type::method`.
fn resolve_method_on_type(
    graph: &Graph,
    type_name: &str,
    method_name: &str,
    l: Language,
    confidence: f64,
    resolved_by: &'static str,
) -> Option<Resolved> {
    let want = format!("{}::{}", type_name, method_name);
    for &i in graph.nodes_by_name(method_name) {
        let n = graph.node(i);
        if n.kind != NodeKind::Method || n.language != l {
            continue;
        }
        if n.qualified_name == want || n.qualified_name.ends_with(&format!("::{}", want)) {
            return Some(Resolved {
                target_id: n.id.clone(),
                confidence,
                resolved_by,
            });
        }
    }
    None
}

/// Port of `matchDottedCallChain`, Go branch: `New().Method` encoded as
/// `New().Method`. Resolve `Method` on what `New` returns; if `New`'s return
/// type isn't recoverable, fall back to bare-name resolution of `Method`.
fn match_dotted_call_chain_go(graph: &Graph, r: &UnresolvedReference) -> Option<Resolved> {
    let name = &r.reference_name;
    let (inner, method) = split_chain(name)?;
    let last_dot = inner.rfind('.');
    if last_dot.is_none() || last_dot == Some(0) {
        // Bare package-level factory FUNCTION `New().method()`.
        if let Some(ret) = lookup_callee_return_type(graph, inner, Language::Go) {
            if let Some(res) =
                resolve_method_on_type(graph, &ret, method, Language::Go, 0.85, "instance-method")
            {
                return Some(res);
            }
        }
        // Fall back to bare-name resolution of the method (don't drop the edge).
        let bare = UnresolvedReference {
            reference_name: method.to_string(),
            ..r.clone()
        };
        return match_by_exact_name(graph, &bare).map(|m| Resolved {
            target_id: m.target_id,
            confidence: m.confidence,
            resolved_by: m.resolved_by,
        });
    }
    // `Receiver.factory().method`: factory's declared return type, then method.
    let ld = last_dot.unwrap();
    let factory_class = inner[..ld].rsplit('.').next().unwrap_or(&inner[..ld]);
    let factory_method = &inner[ld + 1..];
    if factory_class.is_empty() || factory_method.is_empty() {
        return None;
    }
    let ret = lookup_callee_return_type(
        graph,
        &format!("{}::{}", factory_class, factory_method),
        Language::Go,
    )?;
    resolve_method_on_type(graph, &ret, method, Language::Go, 0.85, "instance-method")
}

/// Split the extractor's chained-receiver encoding `<inner>().<method>`.
fn split_chain(name: &str) -> Option<(&str, &str)> {
    let idx = name.find("().")?;
    let inner = &name[..idx];
    let method = &name[idx + 3..];
    if inner.is_empty() || method.is_empty() || !is_ident(method) {
        return None;
    }
    Some((inner, method))
}

fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .next()
            .map(|c| c.is_ascii_alphabetic() || c == '_')
            .unwrap_or(false)
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Port of `matchMethodCall` (Go-relevant strategies): `obj.method` / `Class::method`.
fn match_method_call(graph: &Graph, r: &UnresolvedReference) -> Option<Resolved> {
    let name = &r.reference_name;
    let l = lang(r);
    // Parse `receiver.method` (receiver may itself be dotted) or `Class::method`.
    let (recv, method) = if let Some(idx) = name.rfind("::") {
        (&name[..idx], &name[idx + 2..])
    } else if let Some(idx) = name.rfind('.') {
        (&name[..idx], &name[idx + 1..])
    } else {
        return None;
    };
    if method.is_empty() || !is_ident(method) {
        return None;
    }
    let receiver = recv.rsplit('.').next().unwrap_or(recv);

    // Strategy 1: direct class/struct/interface name → its same-named method.
    for &i in graph.nodes_by_name(receiver) {
        let cn = graph.node(i);
        if matches!(
            cn.kind,
            NodeKind::Class | NodeKind::Struct | NodeKind::Interface
        ) && cn.language == l
        {
            if let Some(res) = resolve_method_on_type(graph, &cn.name, method, l, 0.85, "qualified-name") {
                return Some(res);
            }
        }
    }
    // Strategy 2: capitalized receiver → type.
    let cap = capitalize(receiver);
    if cap != receiver {
        for &i in graph.nodes_by_name(&cap) {
            let cn = graph.node(i);
            if matches!(
                cn.kind,
                NodeKind::Class | NodeKind::Struct | NodeKind::Interface
            ) && cn.language == l
            {
                if let Some(res) =
                    resolve_method_on_type(graph, &cn.name, method, l, 0.8, "instance-method")
                {
                    return Some(res);
                }
            }
        }
    }
    // Strategy 3: unique same-language method by name across the codebase.
    let methods: Vec<usize> = graph
        .nodes_by_name(method)
        .iter()
        .copied()
        .filter(|&i| graph.node(i).kind == NodeKind::Method)
        .collect();
    let same_lang: Vec<usize> = methods.iter().copied().filter(|&i| graph.node(i).language == l).collect();
    let pool = if same_lang.is_empty() { &methods } else { &same_lang };
    if pool.len() == 1 && graph.node(pool[0]).language == l {
        return Some(Resolved {
            target_id: graph.node(pool[0]).id.clone(),
            confidence: 0.7,
            resolved_by: "instance-method",
        });
    }
    // Multiple: score by receiver-word overlap with the class qualified name.
    if pool.len() > 1 {
        let recv_words = split_camel(receiver);
        let mut best: Option<usize> = None;
        let mut best_score = 0usize;
        for &i in pool {
            let n = graph.node(i);
            let class_words = split_camel(&n.qualified_name);
            let mut score = recv_words
                .iter()
                .filter(|w| class_words.iter().any(|cw| cw.eq_ignore_ascii_case(w)))
                .count();
            if n.language == l {
                score += 1;
            }
            if score > best_score {
                best_score = score;
                best = Some(i);
            }
        }
        if best_score >= 2 {
            if let Some(i) = best {
                return Some(Resolved {
                    target_id: graph.node(i).id.clone(),
                    confidence: 0.65,
                    resolved_by: "instance-method",
                });
            }
        }
    }
    None
}

/// Port of `matchByExactName`.
fn match_by_exact_name(graph: &Graph, r: &UnresolvedReference) -> Option<Resolved> {
    let l = lang(r);
    let cands: Vec<usize> = apply_language_gate(graph, graph.nodes_by_name(&r.reference_name), r);
    if cands.is_empty() {
        return None;
    }
    if cands.len() == 1 {
        let n = graph.node(cands[0]);
        let cross = n.language != l;
        // A single same-language candidate is the unique definition (same_module);
        // a single cross-language one is best-effort (fuzzy).
        return Some(Resolved {
            target_id: n.id.clone(),
            confidence: if cross { ladder::FUZZY_NEAR } else { ladder::SAME_MODULE },
            resolved_by: "exact-match",
        });
    }
    let best = find_best_match(graph, r, &cands)?;
    let prox = path_proximity(file(r), &graph.node(best).file_path);
    // Candidate-count penalty (CBM): a high-fan-out name (`Run`, `New`) resolved
    // by best-effort scoring is far less certain than a 2-way tie — floor it.
    let high_fan_out = cands.len() > 8;
    let confidence = match (prox >= 30, high_fan_out) {
        (true, false) => ladder::UNIQUE_NAME,   // close + few candidates
        (true, true) => ladder::SUFFIX_MATCH,   // close but many candidates
        (false, _) => ladder::FUZZY_FAR,        // far → weakest
    };
    Some(Resolved {
        target_id: graph.node(best).id.clone(),
        confidence,
        resolved_by: "exact-match",
    })
}

/// Resolve a `function_ref` (a function used as a value) — port of
/// `matchFunctionRef`. Bare identifiers in TS/JS/Python match FUNCTIONS only
/// (methods need a receiver); Go/Rust also allow methods. Same-file definition
/// wins (first by line); cross-file resolves ONLY when unambiguous (unique-or-drop).
pub fn match_function_ref(graph: &Graph, r: &UnresolvedReference) -> Option<Resolved> {
    let name = &r.reference_name;
    // `this.<member>` is class-scoped — not resolved by bare name matching.
    if name.starts_with("this.") {
        return None;
    }
    let l = lang(r);
    let bare_fn_only = matches!(
        l,
        Language::Typescript | Language::Tsx | Language::Javascript | Language::Jsx | Language::Python
    );
    let cands: Vec<usize> = graph
        .nodes_by_name(name)
        .iter()
        .copied()
        .filter(|&i| {
            let n = graph.node(i);
            let kind_ok = n.kind == NodeKind::Function || (!bare_fn_only && n.kind == NodeKind::Method);
            kind_ok && same_language_family(n.language, l) && n.id != r.from_node_id
        })
        .collect();
    if cands.is_empty() {
        return None;
    }
    // Same-file definition wins (first by start line, for determinism).
    let same_file: Vec<usize> = cands
        .iter()
        .copied()
        .filter(|&i| graph.node(i).file_path == file(r))
        .collect();
    if !same_file.is_empty() {
        let target = *same_file.iter().min_by_key(|&&i| graph.node(i).start_line).unwrap();
        return Some(Resolved {
            target_id: graph.node(target).id.clone(),
            confidence: if same_file.len() == 1 { 0.95 } else { 0.90 },
            resolved_by: "function-ref",
        });
    }
    // Cross-file: only an unambiguous match resolves.
    if cands.len() == 1 {
        return Some(Resolved {
            target_id: graph.node(cands[0]).id.clone(),
            confidence: 0.8,
            resolved_by: "function-ref",
        });
    }
    None
}

/// Port of `applyLanguageGate` (references/imports regimes; calls pass through).
fn apply_language_gate(graph: &Graph, cands: &[usize], r: &UnresolvedReference) -> Vec<usize> {
    use crate::types::{EdgeKind, ReferenceKind};
    let l = lang(r);
    match r.reference_kind {
        // A function-as-value matches function/method nodes ONLY (matchFunctionRef),
        // within the same language family.
        ReferenceKind::FunctionRef => cands
            .iter()
            .copied()
            .filter(|&i| {
                let n = graph.node(i);
                matches!(n.kind, NodeKind::Function | NodeKind::Method)
                    && same_language_family(n.language, l)
            })
            .collect(),
        ReferenceKind::Edge(EdgeKind::References) => cands
            .iter()
            .copied()
            .filter(|&i| same_language_family(graph.node(i).language, l))
            .collect(),
        _ => cands.to_vec(),
    }
}

/// Port of `findBestMatch` scoring.
fn find_best_match(graph: &Graph, r: &UnresolvedReference, cands: &[usize]) -> Option<usize> {
    use crate::types::{EdgeKind, ReferenceKind};
    let l = lang(r);
    let mut best: Option<usize> = None;
    let mut best_score = i64::MIN;
    for &i in cands {
        let c = graph.node(i);
        let mut score = 0i64;
        if c.file_path == file(r) {
            score += 100;
        }
        score += path_proximity(file(r), &c.file_path);
        if c.language == l {
            score += 50;
        } else {
            score -= 80;
        }
        match r.reference_kind {
            ReferenceKind::Edge(EdgeKind::Calls) => {
                if matches!(c.kind, NodeKind::Function | NodeKind::Method) {
                    score += 25;
                }
            }
            ReferenceKind::Edge(EdgeKind::Instantiates) => {
                if matches!(c.kind, NodeKind::Class | NodeKind::Struct | NodeKind::Interface) {
                    score += 25;
                }
            }
            _ => {}
        }
        if c.is_exported {
            score += 10;
        }
        // Line-proximity tie-break (port of findBestMatch): among same-name
        // candidates, prefer the one defined nearest the reference's line — the
        // deciding signal when two definitions share a file. Bounded 0..=20.
        let line_diff = (c.start_line as i64 - r.line as i64).abs();
        score += (20 - line_diff / 10).max(0);
        // Test-path deprioritization (CBM): a candidate in a test file is rarely
        // the real target of a non-test reference. Demote it so production defs win.
        if crate::search::query_utils::is_test_file(&c.file_path)
            && !crate::search::query_utils::is_test_file(file(r))
        {
            score -= 60;
        }
        if score > best_score {
            best_score = score;
            best = Some(i);
        }
    }
    best
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
        None => String::new(),
    }
}

/// Split camelCase/PascalCase into words (port of `splitCamelCase`). Shares the
/// boundary algorithm with FTS `name_split` via `query_utils::camel_space`.
fn split_camel(s: &str) -> Vec<String> {
    crate::search::query_utils::camel_space(s)
        .split(|ch: char| ch.is_whitespace() || "._:/\\".contains(ch))
        .filter(|w| w.len() > 1)
        .map(|w| w.to_string())
        .collect()
}
