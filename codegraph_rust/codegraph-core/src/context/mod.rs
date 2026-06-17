//! Context builder — port of `codegraph/src/context/index.ts` + `formatter.ts`.
//!
//! Commodity lexical ranking (NO vectors): identifier extraction → exact-name +
//! co-location boost → stem/prefix variants → FTS per term → test dampening →
//! hub boost → multi-term co-occurrence re-rank → CamelCase-boundary LIKE →
//! import→def resolution → graph expansion (type hierarchy + BFS depth 1) →
//! multi-stage token budget → markdown with a `## Call paths` section.

mod budgets;
mod explore;
mod symbols;

pub use budgets::{get_explore_budget, get_explore_output_budget, ExploreOutputBudget};

use crate::db::Store;
use crate::graph::{self, Confidence, Direction, Subgraph, TraversalOptions};
use crate::search::query_utils as qu;
use crate::search::SearchResult;
use crate::types::{EdgeKind, Node, NodeKind};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub use symbols::extract_symbols_from_query;

/// Heading that leads the honest low-confidence handoff (port of `LOW_CONFIDENCE_MARKER`).
pub const LOW_CONFIDENCE_MARKER: &str = "### ⚠️ Low-confidence match";

/// High-information node kinds (port of `HIGH_VALUE_NODE_KINDS`).
const HIGH_VALUE_NODE_KINDS: &[NodeKind] = &[
    NodeKind::Function,
    NodeKind::Method,
    NodeKind::Class,
    NodeKind::Interface,
    NodeKind::TypeAlias,
    NodeKind::Struct,
    NodeKind::Trait,
    NodeKind::Component,
    NodeKind::Route,
    NodeKind::Variable,
    NodeKind::Constant,
    NodeKind::Enum,
    NodeKind::Module,
    NodeKind::Namespace,
];

const DEFINITION_KINDS: &[NodeKind] = &[
    NodeKind::Class,
    NodeKind::Interface,
    NodeKind::Struct,
    NodeKind::Trait,
    NodeKind::Protocol,
    NodeKind::Enum,
    NodeKind::TypeAlias,
];

/// Options for `build_context` (port of `BuildContextOptions` defaults; the
/// retrieval adapter overrides several — see `structuralContext`).
#[derive(Clone)]
pub struct BuildOptions {
    pub max_nodes: usize,
    pub max_code_blocks: usize,
    pub max_code_block_size: usize,
    pub include_code: bool,
    pub search_limit: usize,
    pub traversal_depth: u32,
    pub min_score: f64,
}

impl Default for BuildOptions {
    fn default() -> Self {
        BuildOptions {
            max_nodes: 20,
            max_code_blocks: 5,
            max_code_block_size: 1500,
            include_code: true,
            search_limit: 3,
            traversal_depth: 1,
            min_score: 0.3,
        }
    }
}

impl BuildOptions {
    /// The options `codegraph_explore` uses (generous gather; the explore output
    /// budget bounds the rendered size, not the subgraph).
    pub fn explore() -> Self {
        BuildOptions {
            max_nodes: 200,
            max_code_blocks: 0,
            max_code_block_size: 0,
            include_code: false,
            search_limit: 8,
            traversal_depth: 3,
            min_score: 0.2,
        }
    }

    /// The options the benchmark's `structuralContext` uses (its recall is the M3 gate).
    pub fn retrieval() -> Self {
        BuildOptions {
            max_nodes: 20,
            max_code_blocks: 8,
            max_code_block_size: 2000,
            include_code: true,
            search_limit: 8,
            traversal_depth: 2,
            min_score: 0.3,
        }
    }
}

/// The context builder — holds a borrowed store + project metadata.
pub struct ContextBuilder<'a> {
    store: &'a Store,
    project_root: PathBuf,
    project_tokens: HashSet<String>,
}

impl<'a> ContextBuilder<'a> {
    pub fn new(store: &'a Store, project_root: &Path) -> Self {
        let project_tokens = qu::derive_project_name_tokens(project_root);
        ContextBuilder {
            store,
            project_root: project_root.to_path_buf(),
            project_tokens,
        }
    }

    /// Build context for a query, rendered as markdown (port of `buildContext`).
    pub fn build_context_markdown(&self, query: &str, opts: &BuildOptions) -> Result<String> {
        let subgraph = self.find_relevant_context(query, opts)?;
        let entry_points = self.entry_points(&subgraph);
        let code_blocks = if opts.include_code {
            self.extract_code_blocks(&subgraph, opts.max_code_blocks, opts.max_code_block_size)
        } else {
            vec![]
        };
        let mut md = self.format_markdown(query, &subgraph, &entry_points, &code_blocks);
        md.push_str(&self.build_call_paths_section(&subgraph));
        if subgraph.confidence == Some(Confidence::Low) {
            md.push_str(&self.build_low_confidence_note(&entry_points));
        }
        Ok(md)
    }

    /// Find the relevant subgraph for a query (port of `findRelevantContext`).
    pub(crate) fn find_relevant_context(&self, query: &str, opts: &BuildOptions) -> Result<Subgraph> {
        let mut sg = Subgraph::default();
        if query.trim().is_empty() {
            return Ok(sg);
        }
        let node_kinds = HIGH_VALUE_NODE_KINDS.to_vec();
        let search_limit = opts.search_limit;

        // Step 1: extract candidate symbol names.
        let symbols_from_query = extract_symbols_from_query(query);

        // Step 2: exact matches for extracted symbols, with co-location boost.
        let mut exact_matches: Vec<SearchResult> = Vec::new();
        if !symbols_from_query.is_empty() {
            exact_matches = self.store.find_nodes_by_exact_name(
                &symbols_from_query,
                &node_kinds,
                (search_limit as f64 * 5.0).ceil() as usize,
            )?;
            if exact_matches.len() > 1 {
                let mut file_symbols: HashMap<String, HashSet<String>> = HashMap::new();
                for r in &exact_matches {
                    file_symbols
                        .entry(r.node.file_path.clone())
                        .or_default()
                        .insert(r.node.name.to_ascii_lowercase());
                }
                for r in exact_matches.iter_mut() {
                    let count = file_symbols.get(&r.node.file_path).map(|s| s.len()).unwrap_or(1);
                    if count > 1 {
                        r.score += (count as f64 - 1.0) * 20.0;
                    }
                }
                exact_matches.sort_by(|a, b| cmp_desc(a.score, b.score));
            }
            exact_matches.truncate((search_limit as f64 * 2.0).ceil() as usize);
        }

        // Step 2b: definition-prefix matches (with stem variants).
        if !symbols_from_query.is_empty() {
            let mut expanded: Vec<String> = symbols_from_query.clone();
            let mut seen: HashSet<String> = symbols_from_query.iter().cloned().collect();
            for sym in &symbols_from_query {
                for v in qu::get_stem_variants(sym) {
                    if seen.insert(v.clone()) {
                        expanded.push(v);
                    }
                }
            }
            for sym in &expanded {
                let title = title_case(sym);
                if title == *sym {
                    continue;
                }
                let prefix_results =
                    self.store.search_nodes(&title, DEFINITION_KINDS, 30, &self.project_tokens)?;
                let mut matched: Vec<SearchResult> = Vec::new();
                for r in prefix_results {
                    if r.node.name.to_ascii_lowercase().starts_with(&title.to_ascii_lowercase()) {
                        let brevity = (10.0
                            - (r.node.name.chars().count() as f64 - title.chars().count() as f64) / 3.0)
                            .max(0.0);
                        matched.push(SearchResult { score: r.score + 15.0 + brevity, node: r.node });
                    }
                }
                matched.sort_by(|a, b| cmp_desc(a.score, b.score));
                for r in matched.into_iter().take((search_limit as f64).ceil() as usize) {
                    if !exact_matches.iter().any(|e| e.node.id == r.node.id) {
                        exact_matches.push(r);
                    }
                }
            }
            exact_matches.sort_by(|a, b| cmp_desc(a.score, b.score));
            exact_matches.truncate((search_limit as f64 * 3.0).ceil() as usize);
        }

        // Step 3: FTS text search per term, boosting multi-term hits.
        let mut text_results: Vec<SearchResult> = Vec::new();
        let search_terms = qu::extract_search_terms(query, true);
        if !search_terms.is_empty() {
            // No explicit kind filter → exclude imports (they flood FTS).
            let search_kinds: Vec<NodeKind> = vec![
                NodeKind::File, NodeKind::Module, NodeKind::Class, NodeKind::Struct,
                NodeKind::Interface, NodeKind::Trait, NodeKind::Protocol, NodeKind::Function,
                NodeKind::Method, NodeKind::Property, NodeKind::Field, NodeKind::Variable,
                NodeKind::Constant, NodeKind::Enum, NodeKind::EnumMember, NodeKind::TypeAlias,
                NodeKind::Namespace, NodeKind::Export, NodeKind::Route, NodeKind::Component,
            ];
            let mut term_map: HashMap<String, (SearchResult, u32)> = HashMap::new();
            for term in &search_terms {
                let term_results =
                    self.store.search_nodes(term, &search_kinds, search_limit * 2, &self.project_tokens)?;
                for r in term_results {
                    if let Some(existing) = term_map.get_mut(&r.node.id) {
                        existing.1 += 1;
                        existing.0.score = existing.0.score.max(r.score);
                    } else {
                        term_map.insert(r.node.id.clone(), (r, 1));
                    }
                }
            }
            text_results = term_map
                .into_values()
                .map(|(mut r, hits)| {
                    r.score += (hits as f64 - 1.0) * 5.0;
                    r
                })
                .collect();
            text_results.sort_by(|a, b| cmp_desc(a.score, b.score));
            text_results.truncate(search_limit * 2);
        }

        // Step 4: merge channels (max score on dup).
        let mut by_id: HashMap<String, usize> = HashMap::new();
        let mut search_results: Vec<SearchResult> = Vec::new();
        for r in exact_matches.iter().chain(text_results.iter()) {
            if let Some(&idx) = by_id.get(&r.node.id) {
                let cur: &mut SearchResult = &mut search_results[idx];
                cur.score = cur.score.max(r.score);
            } else {
                by_id.insert(r.node.id.clone(), search_results.len());
                search_results.push(r.clone());
            }
        }

        let query_lower = query.to_ascii_lowercase();
        let is_test_query = query_lower.contains("test") || query_lower.contains("spec");

        // Deprioritize test files early.
        if !is_test_query {
            for r in search_results.iter_mut() {
                if qu::is_test_file(&r.node.file_path) {
                    r.score *= 0.3;
                }
            }
        }

        // Core-directory (dominant file) boost.
        if let Ok(Some((dom_path, edge_count, next_count))) = self.store.get_dominant_file() {
            if edge_count >= 3 * next_count {
                if let Some(slash) = dom_path.rfind('/') {
                    let core_dir = &dom_path[..slash + 1];
                    for r in search_results.iter_mut() {
                        if r.node.file_path.starts_with(core_dir) {
                            r.score += 25.0;
                        }
                    }
                }
            }
        }

        // Step 5a: multi-term co-occurrence re-ranking (before truncation).
        let query_terms_for_boost = qu::extract_search_terms(query, true);
        if query_terms_for_boost.len() >= 2 {
            // Group terms that are substrings of each other (stem variants = one concept).
            let mut term_groups: Vec<Vec<String>> = Vec::new();
            let mut sorted = query_terms_for_boost.clone();
            sorted.sort_by_key(|s| std::cmp::Reverse(s.len()));
            let mut assigned: HashSet<String> = HashSet::new();
            for term in &sorted {
                if assigned.contains(term) {
                    continue;
                }
                let mut group = vec![term.clone()];
                assigned.insert(term.clone());
                for other in &sorted {
                    if assigned.contains(other) {
                        continue;
                    }
                    if term.contains(other.as_str()) || other.contains(term.as_str()) {
                        group.push(other.clone());
                        assigned.insert(other.clone());
                    }
                }
                term_groups.push(group);
            }

            let exact_ids: HashSet<String> = exact_matches.iter().map(|r| r.node.id.clone()).collect();
            let distinctive_tokens: HashSet<String> = symbols_from_query
                .iter()
                .filter(|s| qu::is_distinctive_identifier(s))
                .map(|s| s.to_ascii_lowercase())
                .collect();
            let distinctive_exact_ids: HashSet<String> = exact_matches
                .iter()
                .filter(|r| distinctive_tokens.contains(&r.node.name.to_ascii_lowercase()))
                .map(|r| r.node.id.clone())
                .collect();

            for r in search_results.iter_mut() {
                let name_lower = r.node.name.to_ascii_lowercase();
                let dir_segments: Vec<String> = parent_dir(&r.node.file_path)
                    .to_ascii_lowercase()
                    .split('/')
                    .map(|s| s.to_string())
                    .collect();
                let mut match_count = 0;
                for group in &term_groups {
                    let group_matches = group.iter().any(|term| {
                        name_lower.contains(term.as_str())
                            || dir_segments.iter().any(|seg| seg == term)
                    });
                    if group_matches {
                        match_count += 1;
                    }
                }
                if match_count >= 2 {
                    r.score *= 1.0 + match_count as f64 * 0.5;
                } else if distinctive_exact_ids.contains(&r.node.id) {
                    // keep full score
                } else if exact_ids.contains(&r.node.id) {
                    r.score *= 0.3;
                } else {
                    r.score *= 0.6;
                }
            }
            search_results.sort_by(|a, b| cmp_desc(a.score, b.score));
        }

        // Step 5b: CamelCase-boundary LIKE matches.
        if !symbols_from_query.is_empty() {
            let mut camel_searched: HashSet<String> = HashSet::new();
            let search_id_set: HashSet<String> =
                search_results.iter().map(|r| r.node.id.clone()).collect();
            let mut camel_node_terms: HashMap<String, (SearchResult, u32)> = HashMap::new();
            let max_camel_per_term = ((search_limit as f64) / 2.0).ceil() as usize;

            for sym in &symbols_from_query {
                let title = title_case(sym);
                if title.chars().count() < 3 {
                    continue;
                }
                let key = title.to_ascii_lowercase();
                if !camel_searched.insert(key) {
                    continue;
                }
                let like_results =
                    self.store.find_nodes_by_name_substring(&title, DEFINITION_KINDS, 200, true)?;
                let mut term_cands: Vec<SearchResult> = Vec::new();
                for r in like_results {
                    let name = &r.node.name;
                    let Some(idx) = name.find(title.as_str()) else { continue };
                    if idx == 0 {
                        continue;
                    }
                    let before = name[..idx].chars().last().unwrap_or(' ');
                    if !before.is_ascii_alphabetic() {
                        continue;
                    }
                    if search_id_set.contains(&r.node.id) {
                        continue;
                    }
                    if qu::is_test_file(&r.node.file_path) && !is_test_query {
                        continue;
                    }
                    let path_score = qu::score_path_relevance(&r.node.file_path, query, &self.project_tokens);
                    let brevity = (6.0
                        - (name.chars().count() as f64 - title.chars().count() as f64) / 4.0)
                        .max(0.0);
                    term_cands.push(SearchResult { score: 8.0 + brevity + path_score as f64, node: r.node });
                }
                term_cands.sort_by(|a, b| cmp_desc(a.score, b.score));
                let accum = max_camel_per_term * 4;
                for r in term_cands.into_iter().take(accum) {
                    if let Some(existing) = camel_node_terms.get_mut(&r.node.id) {
                        existing.1 += 1;
                    } else {
                        camel_node_terms.insert(r.node.id.clone(), (r, 1));
                    }
                }
            }
            let mut camel_results: Vec<SearchResult> = camel_node_terms
                .into_values()
                .map(|(mut r, term_count)| {
                    r.score = r.score * (1.0 + term_count as f64) + (term_count as f64 - 1.0) * 30.0;
                    r
                })
                .collect();
            camel_results.sort_by(|a, b| cmp_desc(a.score, b.score));
            let mut id_set = search_id_set;
            for r in camel_results.into_iter().take(search_limit) {
                if id_set.insert(r.node.id.clone()) {
                    search_results.push(r);
                }
            }

            // Step 5c: compound-term matching (2+ terms anywhere in the name).
            if symbols_from_query.len() >= 2 {
                let mut compound: HashMap<String, (Node, HashSet<String>)> = HashMap::new();
                for sym in &symbols_from_query {
                    let title = title_case(sym);
                    if title.chars().count() < 3 {
                        continue;
                    }
                    let like_results = self
                        .store
                        .find_nodes_by_name_substring(&title, DEFINITION_KINDS, 200, false)?;
                    for r in like_results {
                        if id_set.contains(&r.node.id) {
                            continue;
                        }
                        if qu::is_test_file(&r.node.file_path) && !is_test_query {
                            continue;
                        }
                        let entry = compound
                            .entry(r.node.id.clone())
                            .or_insert_with(|| (r.node.clone(), HashSet::new()));
                        entry.1.insert(title.clone());
                    }
                }
                let mut compound_results: Vec<SearchResult> = Vec::new();
                for (_, (node, terms)) in compound {
                    if terms.len() >= 2 {
                        let path_score = qu::score_path_relevance(&node.file_path, query, &self.project_tokens);
                        let brevity = (6.0 - node.name.chars().count() as f64 / 8.0).max(0.0);
                        compound_results.push(SearchResult {
                            score: 10.0 + (terms.len() as f64 - 1.0) * 20.0 + path_score as f64 + brevity,
                            node,
                        });
                    }
                }
                compound_results.sort_by(|a, b| cmp_desc(a.score, b.score));
                let max_compound = ((search_limit as f64) / 2.0).ceil() as usize;
                for r in compound_results.into_iter().take(max_compound) {
                    if id_set.insert(r.node.id.clone()) {
                        search_results.push(r);
                    }
                }
            }
        }

        // Final sort + truncate.
        search_results.sort_by(|a, b| cmp_desc(a.score, b.score));
        search_results.truncate(search_limit * 3);
        let mut filtered: Vec<SearchResult> =
            search_results.into_iter().filter(|r| r.score >= opts.min_score).collect();

        // Resolve imports/exports to definitions.
        filtered = self.resolve_imports_to_definitions(filtered)?;

        // Cap entry points.
        if filtered.len() > search_limit {
            filtered.truncate(search_limit);
        }

        // Confidence signal.
        let mut confidence = Confidence::High;
        let conf_terms: Vec<String> = qu::extract_search_terms(query, false)
            .into_iter()
            .filter(|t| t.len() >= 3)
            .collect();
        if conf_terms.len() >= 2 && !filtered.is_empty() {
            let distinctive: HashSet<String> = symbols_from_query
                .iter()
                .filter(|s| qu::is_distinctive_identifier(s))
                .map(|s| s.to_ascii_lowercase())
                .collect();
            let any_strong = filtered.iter().any(|r| {
                if distinctive.contains(&r.node.name.to_ascii_lowercase()) {
                    return true;
                }
                let name_lower = r.node.name.to_ascii_lowercase();
                let dir_segs: Vec<String> = parent_dir(&r.node.file_path)
                    .to_ascii_lowercase()
                    .split('/')
                    .map(|s| s.to_string())
                    .collect();
                let mut hits = 0;
                for t in &conf_terms {
                    if name_lower.contains(t.as_str()) || dir_segs.iter().any(|s| s == t) {
                        hits += 1;
                        if hits >= 2 {
                            return true;
                        }
                    }
                }
                false
            });
            if !any_strong {
                confidence = Confidence::Low;
            }
        }

        // Seed entry points.
        for r in &filtered {
            sg.nodes.insert(r.node.id.clone(), r.node.clone());
            sg.roots.push(r.node.id.clone());
        }

        // Type-hierarchy expansion for class/interface entry points.
        let type_hierarchy_kinds: HashSet<NodeKind> = [
            NodeKind::Class, NodeKind::Interface, NodeKind::Struct, NodeKind::Trait, NodeKind::Protocol,
        ]
        .into_iter()
        .collect();
        let max_hierarchy = ((opts.max_nodes as f64) / 4.0).ceil() as usize;
        let mut hierarchy_added = 0;
        for r in &filtered {
            if hierarchy_added >= max_hierarchy {
                break;
            }
            if type_hierarchy_kinds.contains(&r.node.kind) {
                let h = graph::get_type_hierarchy(self.store, &r.node.id)?;
                for (id, node) in &h.nodes {
                    if !sg.nodes.contains_key(id) {
                        sg.nodes.insert(id.clone(), node.clone());
                        hierarchy_added += 1;
                    }
                }
                for e in h.edges {
                    if !edge_exists(&sg.edges, &e) {
                        sg.edges.push(e);
                    }
                }
            }
        }
        // Pass 2: sibling hierarchy of discovered parents.
        if hierarchy_added > 0 {
            let candidates: Vec<String> = sg
                .nodes
                .values()
                .filter(|n| type_hierarchy_kinds.contains(&n.kind) && !sg.roots.contains(&n.id))
                .map(|n| n.id.clone())
                .collect();
            for cid in candidates {
                if hierarchy_added >= max_hierarchy {
                    break;
                }
                let h = graph::get_type_hierarchy(self.store, &cid)?;
                for (id, node) in &h.nodes {
                    if !sg.nodes.contains_key(id) && hierarchy_added < max_hierarchy {
                        sg.nodes.insert(id.clone(), node.clone());
                        hierarchy_added += 1;
                    }
                }
                for e in h.edges {
                    if sg.nodes.contains_key(&e.source) && sg.nodes.contains_key(&e.target) && !edge_exists(&sg.edges, &e) {
                        sg.edges.push(e);
                    }
                }
            }
        }

        // BFS expansion from each entry point.
        let per_entry = ((opts.max_nodes as f64) / (filtered.len().max(1) as f64)).ceil() as usize;
        for r in &filtered {
            let topts = TraversalOptions {
                max_depth: opts.traversal_depth,
                edge_kinds: vec![],
                node_kinds: node_kinds.clone(),
                direction: Direction::Both,
                limit: per_entry,
            };
            let tr = graph::traverse_bfs(self.store, &r.node.id, &topts)?;
            for (id, node) in &tr.nodes {
                if !sg.nodes.contains_key(id) {
                    sg.nodes.insert(id.clone(), node.clone());
                }
            }
            for e in tr.edges {
                if !edge_exists(&sg.edges, &e) {
                    sg.edges.push(e);
                }
            }
        }

        // Trim to max nodes (prioritize entry points + neighbors).
        if sg.nodes.len() > opts.max_nodes {
            let mut priority: HashSet<String> = sg.roots.iter().cloned().collect();
            for e in &sg.edges {
                if priority.contains(&e.source) {
                    priority.insert(e.target.clone());
                }
                if priority.contains(&e.target) {
                    priority.insert(e.source.clone());
                }
            }
            let mut kept: indexmap::IndexMap<String, Node> = indexmap::IndexMap::new();
            for id in &priority {
                if kept.len() >= opts.max_nodes {
                    break;
                }
                if let Some(n) = sg.nodes.get(id) {
                    kept.insert(id.clone(), n.clone());
                }
            }
            for (id, node) in &sg.nodes {
                if kept.len() >= opts.max_nodes {
                    break;
                }
                if !kept.contains_key(id) {
                    kept.insert(id.clone(), node.clone());
                }
            }
            sg.nodes = kept;
            sg.edges.retain(|e| sg.nodes.contains_key(&e.source) && sg.nodes.contains_key(&e.target));
        }

        // Per-file diversity cap (~20%).
        let max_per_file = ((opts.max_nodes as f64 * 0.2).ceil() as usize).max(5);
        let mut file_counts: HashMap<String, Vec<String>> = HashMap::new();
        for (id, node) in &sg.nodes {
            file_counts.entry(node.file_path.clone()).or_default().push(id.clone());
        }
        let root_set: HashSet<String> = sg.roots.iter().cloned().collect();
        let mut to_remove: Vec<String> = Vec::new();
        for (_, node_ids) in file_counts.iter() {
            if node_ids.len() <= max_per_file {
                continue;
            }
            let mut sorted = node_ids.clone();
            let kind_prio = |k: NodeKind| match k {
                NodeKind::Class | NodeKind::Interface | NodeKind::Struct | NodeKind::Trait
                | NodeKind::Protocol | NodeKind::Enum => 3,
                NodeKind::Method | NodeKind::Function => 1,
                _ => 0,
            };
            sorted.sort_by_key(|id| {
                let root = if root_set.contains(id) { 10 } else { 0 };
                let kind = sg.nodes.get(id).map(|n| kind_prio(n.kind)).unwrap_or(0);
                std::cmp::Reverse(root + kind)
            });
            for id in sorted.into_iter().skip(max_per_file) {
                to_remove.push(id);
            }
        }
        for id in to_remove {
            sg.nodes.shift_remove(&id);
        }

        // Non-production cap (≤15%).
        if !is_test_query {
            let max_non_prod = ((opts.max_nodes as f64 * 0.15).ceil() as usize).max(3);
            let non_prod: Vec<String> = sg
                .nodes
                .iter()
                .filter(|(_, n)| qu::is_test_file(&n.file_path))
                .map(|(id, _)| id.clone())
                .collect();
            if non_prod.len() > max_non_prod {
                for id in non_prod.into_iter().skip(max_non_prod) {
                    sg.nodes.shift_remove(&id);
                    sg.roots.retain(|r| r != &id);
                }
            }
        }

        sg.edges.retain(|e| sg.nodes.contains_key(&e.source) && sg.nodes.contains_key(&e.target));

        // Edge recovery between kept nodes.
        let recovery_kinds = [
            EdgeKind::Calls, EdgeKind::Extends, EdgeKind::Implements, EdgeKind::References, EdgeKind::Overrides,
        ];
        let ids: Vec<String> = sg.nodes.keys().cloned().collect();
        let recovered = self.store.find_edges_between_nodes(&ids, &recovery_kinds)?;
        let mut existing_keys: HashSet<String> =
            sg.edges.iter().map(|e| edge_key(e)).collect();
        for e in recovered {
            let k = edge_key(&e);
            if existing_keys.insert(k) {
                sg.edges.push(e);
            }
        }

        sg.confidence = Some(confidence);
        Ok(sg)
    }

    /// Resolve import/export nodes to their definitions (port of `resolveImportsToDefinitions`).
    fn resolve_imports_to_definitions(&self, results: Vec<SearchResult>) -> Result<Vec<SearchResult>> {
        let mut out: Vec<SearchResult> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for r in results {
            if r.node.kind != NodeKind::Import && r.node.kind != NodeKind::Export {
                if seen.insert(r.node.id.clone()) {
                    out.push(r);
                }
                continue;
            }
            let edge_kind = if r.node.kind == NodeKind::Import { EdgeKind::Imports } else { EdgeKind::Exports };
            let edges = self.store.outgoing_edges(&r.node.id, &[edge_kind])?;
            for e in edges {
                if let Some(target) = self.store.get_node_by_id_full(&e.target)? {
                    if seen.insert(target.id.clone()) {
                        out.push(SearchResult { node: target, score: r.score });
                    }
                }
            }
        }
        Ok(out)
    }

    fn entry_points(&self, sg: &Subgraph) -> Vec<Node> {
        sg.roots.iter().filter_map(|id| sg.nodes.get(id).cloned()).collect()
    }

    /// Extract code blocks for key nodes (port of `extractCodeBlocks`).
    fn extract_code_blocks(&self, sg: &Subgraph, max_blocks: usize, max_size: usize) -> Vec<CodeBlock> {
        let mut priority: Vec<Node> = Vec::new();
        let root_set: HashSet<&String> = sg.roots.iter().collect();
        for id in &sg.roots {
            if let Some(n) = sg.nodes.get(id) {
                priority.push(n.clone());
            }
        }
        for n in sg.nodes.values() {
            if !root_set.contains(&n.id) && matches!(n.kind, NodeKind::Function | NodeKind::Method) {
                priority.push(n.clone());
            }
        }
        for n in sg.nodes.values() {
            if !root_set.contains(&n.id) && n.kind == NodeKind::Class {
                priority.push(n.clone());
            }
        }
        let mut blocks = Vec::new();
        for node in priority {
            if blocks.len() >= max_blocks {
                break;
            }
            if let Some(code) = self.extract_node_code(&node) {
                let truncated = if code.len() > max_size {
                    format!("{}\n... (truncated) ...", &code[..floor_char_boundary(&code, max_size)])
                } else {
                    code
                };
                blocks.push(CodeBlock {
                    content: truncated,
                    file_path: node.file_path.clone(),
                    start_line: node.start_line,
                    language: node.language.as_str().to_string(),
                    name: node.name.clone(),
                });
            }
        }
        blocks
    }

    /// Read a node's source lines (port of `extractNodeCode`, config-leaf guard).
    fn extract_node_code(&self, node: &Node) -> Option<String> {
        if is_config_leaf(node) {
            return Some(
                node.signature
                    .clone()
                    .unwrap_or_else(|| if node.qualified_name.is_empty() { node.name.clone() } else { node.qualified_name.clone() }),
            );
        }
        let abs = validate_path_within_root(&self.project_root, &node.file_path)?;
        let content = std::fs::read_to_string(&abs).ok()?;
        let lines: Vec<&str> = content.split('\n').collect();
        let start = (node.start_line.saturating_sub(1)) as usize;
        let end = (node.end_line as usize).min(lines.len());
        if start >= lines.len() {
            return None;
        }
        Some(lines[start..end.max(start)].join("\n"))
    }

    /// Format the context as markdown (port of `formatContextAsMarkdown`).
    fn format_markdown(
        &self,
        query: &str,
        sg: &Subgraph,
        entry_points: &[Node],
        code_blocks: &[CodeBlock],
    ) -> String {
        let mut lines: Vec<String> = Vec::new();
        lines.push("## Code Context\n".to_string());
        lines.push(format!("**Query:** {}\n", query));

        let mut ordered_entries = entry_points.to_vec();
        ordered_entries.sort_by_key(|n| qu::is_generated_file(&n.file_path));
        if !ordered_entries.is_empty() {
            lines.push("### Entry Points\n".to_string());
            for node in &ordered_entries {
                let loc = if node.start_line > 0 { format!(":{}", node.start_line) } else { String::new() };
                lines.push(format!("- **{}** ({}) - {}{}", node.name, node.kind.as_str(), node.file_path, loc));
                if let Some(sig) = &node.signature {
                    lines.push(format!("  `{}`", sig));
                }
            }
            lines.push(String::new());
        }

        // Related symbols.
        let entry_ids: HashSet<&String> = entry_points.iter().map(|n| &n.id).collect();
        let other: Vec<&Node> = sg
            .nodes
            .values()
            .filter(|n| !entry_ids.contains(&n.id))
            .filter(|n| !qu::is_generated_file(&n.file_path))
            .take(10)
            .collect();
        if !other.is_empty() {
            lines.push("### Related Symbols\n".to_string());
            let mut by_file: indexmap::IndexMap<String, Vec<&Node>> = indexmap::IndexMap::new();
            for n in &other {
                by_file.entry(n.file_path.clone()).or_default().push(n);
            }
            for (file, nodes) in &by_file {
                let list = nodes.iter().map(|n| format!("{}:{}", n.name, n.start_line)).collect::<Vec<_>>().join(", ");
                lines.push(format!("- {}: {}", file, list));
            }
            lines.push(String::new());
        }

        // Code blocks.
        if !code_blocks.is_empty() {
            let mut ordered = code_blocks.to_vec();
            ordered.sort_by_key(|b| qu::is_generated_file(&b.file_path));
            lines.push("### Code\n".to_string());
            for block in &ordered {
                lines.push(format!("#### {} ({}:{})\n", block.name, block.file_path, block.start_line));
                lines.push(format!("```{}", block.language));
                lines.push(block.content.clone());
                lines.push("```\n".to_string());
            }
        }

        lines.join("\n")
    }

    /// `## Call paths` section derived from the subgraph's `calls` edges (port of
    /// `buildCallPathsSection`). Annotates synthesized (dynamic-dispatch) hops inline.
    fn build_call_paths_section(&self, sg: &Subgraph) -> String {
        let mut adj: HashMap<String, Vec<String>> = HashMap::new();
        for e in &sg.edges {
            if e.kind != EdgeKind::Calls {
                continue;
            }
            if !sg.nodes.contains_key(&e.source) || !sg.nodes.contains_key(&e.target) {
                continue;
            }
            adj.entry(e.source.clone()).or_default().push(e.target.clone());
        }
        if adj.is_empty() {
            return String::new();
        }
        const MAX_HOPS: usize = 6;
        let mut chains: Vec<Vec<String>> = Vec::new();
        let mut budget: i64 = 2000;
        fn dfs(
            id: &str,
            path: &mut Vec<String>,
            seen: &mut HashSet<String>,
            adj: &HashMap<String, Vec<String>>,
            chains: &mut Vec<Vec<String>>,
            budget: &mut i64,
        ) {
            if *budget <= 0 {
                return;
            }
            *budget -= 1;
            let next: Vec<String> = adj
                .get(id)
                .map(|v| v.iter().filter(|t| !seen.contains(*t)).cloned().collect())
                .unwrap_or_default();
            if next.is_empty() || path.len() >= MAX_HOPS {
                if path.len() >= 3 {
                    chains.push(path.clone());
                }
                return;
            }
            for t in next {
                seen.insert(t.clone());
                path.push(t.clone());
                dfs(&t, path, seen, adj, chains, budget);
                path.pop();
                seen.remove(&t);
            }
        }
        let mut starts: Vec<String> = if !sg.roots.is_empty() {
            sg.roots.iter().filter(|id| adj.contains_key(*id)).cloned().collect()
        } else {
            adj.keys().cloned().collect()
        };
        starts.truncate(5);
        for s in starts {
            let mut path = vec![s.clone()];
            let mut seen: HashSet<String> = [s.clone()].into_iter().collect();
            dfs(&s, &mut path, &mut seen, &adj, &mut chains, &mut budget);
        }
        if chains.is_empty() {
            return String::new();
        }
        let root_set: HashSet<&String> = sg.roots.iter().collect();
        let root_count = |c: &Vec<String>| c.iter().filter(|id| root_set.contains(id)).count();
        let mut relevant: Vec<Vec<String>> = chains.into_iter().filter(|c| root_count(c) >= 2).collect();
        relevant.sort_by(|a, b| root_count(b).cmp(&root_count(a)).then(b.len().cmp(&a.len())));
        let mut kept: Vec<Vec<String>> = Vec::new();
        for c in relevant {
            let key = c.join(">");
            if kept.iter().any(|k| k.join(">").contains(&key)) {
                continue;
            }
            kept.push(c);
            if kept.len() >= 3 {
                break;
            }
        }
        if kept.is_empty() {
            return String::new();
        }
        let name = |id: &str| sg.nodes.get(id).map(|n| n.name.clone()).unwrap_or_else(|| id.to_string());

        // Synthesized-hop labels keyed by "source>target".
        let mut synth_by_pair: HashMap<String, String> = HashMap::new();
        for e in &sg.edges {
            if e.kind != EdgeKind::Calls || e.provenance != Some(crate::types::Provenance::Heuristic) {
                continue;
            }
            let Some(m) = e.metadata.as_ref() else { continue };
            let synth_by = m.get("synthesizedBy").and_then(|v| v.as_str());
            let Some(synth_by) = synth_by else { continue };
            let at = m.get("registeredAt").and_then(|v| v.as_str()).map(|s| format!(" @{}", s)).unwrap_or_default();
            let via = m.get("via").and_then(|v| v.as_str());
            let label = match synth_by {
                "callback" => format!("callback via {}{}", via.map(|v| format!("`{}`", v)).unwrap_or_else(|| "registrar".to_string()), at),
                "react-render" => format!("React re-render via setState{}", at),
                "jsx-render" => format!("renders <{}>", via.unwrap_or("child")),
                "vue-handler" => format!("Vue @{} handler", m.get("event").and_then(|v| v.as_str()).unwrap_or("event")),
                _ => format!("event {}{}", m.get("event").and_then(|v| v.as_str()).map(|e| format!("`{}`", e)).unwrap_or_default(), at),
            };
            synth_by_pair.insert(format!("{}>{}", e.source, e.target), label);
        }
        let render_chain = |c: &Vec<String>| -> String {
            let mut s = name(&c[0]);
            for i in 1..c.len() {
                let key = format!("{}>{}", c[i - 1], c[i]);
                if let Some(synth) = synth_by_pair.get(&key) {
                    s += &format!(" →[{}] {}", synth, name(&c[i]));
                } else {
                    s += &format!(" → {}", name(&c[i]));
                }
            }
            s
        };
        let has_synth = kept.iter().any(|c| (1..c.len()).any(|i| synth_by_pair.contains_key(&format!("{}>{}", c[i - 1], c[i]))));
        let mut lines: Vec<String> = vec![
            String::new(),
            "## Call paths".to_string(),
            String::new(),
            "Execution flow among the key symbols (traced through the call graph):".to_string(),
            String::new(),
        ];
        for c in &kept {
            lines.push(format!("- {}", render_chain(c)));
        }
        lines.push(String::new());
        lines.push(if has_synth {
            "_Hops marked `[callback/event …]` are dynamic dispatch bridged by codegraph (with the registration site); the rest are direct calls. codegraph_node any symbol for its body._".to_string()
        } else {
            "_codegraph_node any symbol above for its source + its own callers/callees._".to_string()
        });
        format!("\n{}\n", lines.join("\n"))
    }

    /// Honest low-confidence handoff (port of `buildLowConfidenceNote`).
    fn build_low_confidence_note(&self, entry_points: &[Node]) -> String {
        let mut dirs: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for n in entry_points {
            let dir = match n.file_path.rfind('/') {
                Some(s) if s > 0 => n.file_path[..s].to_string(),
                _ => n.file_path.clone(),
            };
            if seen.insert(dir.clone()) {
                dirs.push(dir);
            }
            if dirs.len() >= 4 {
                break;
            }
        }
        let dir_line = if dirs.is_empty() {
            String::new()
        } else {
            format!("\n- `codegraph_files` a likely area: {}", dirs.iter().map(|d| format!("`{}`", d)).collect::<Vec<_>>().join(", "))
        };
        format!(
            "\n\n{}\n\nThis query matched mostly on common words, so the entry points above may be off-target — treat them as a starting point, not a complete answer. For a reliable result:\n- `codegraph_explore` with the **exact symbol names** you are after (class / function / method names), or\n- `codegraph_search <name>` for one specific symbol{}\n\nDo not assume the list above is comprehensive.",
            LOW_CONFIDENCE_MARKER, dir_line
        )
    }
}

#[derive(Clone)]
struct CodeBlock {
    content: String,
    file_path: String,
    start_line: u32,
    language: String,
    name: String,
}

fn cmp_desc(a: f64, b: f64) -> std::cmp::Ordering {
    b.partial_cmp(&a).unwrap_or(std::cmp::Ordering::Equal)
}

fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(f) => f.to_ascii_uppercase().to_string() + &chars.as_str().to_ascii_lowercase(),
        None => String::new(),
    }
}

fn parent_dir(path: &str) -> String {
    match path.rfind('/') {
        Some(s) => path[..s].to_string(),
        None => ".".to_string(),
    }
}

fn edge_key(e: &crate::types::Edge) -> String {
    format!("{}:{}:{}", e.source, e.target, e.kind.as_str())
}

fn edge_exists(edges: &[crate::types::Edge], e: &crate::types::Edge) -> bool {
    edges.iter().any(|x| x.source == e.source && x.target == e.target && x.kind == e.kind)
}

/// Port of `isConfigLeafNode`: a constant in a config language (yaml/properties/…).
fn is_config_leaf(node: &Node) -> bool {
    node.kind == NodeKind::Constant
        && matches!(
            node.language,
            crate::types::Language::Yaml | crate::types::Language::Properties | crate::types::Language::Xml
        )
}

/// Port of `validatePathWithinRoot` (lexical + canonicalize containment).
pub fn validate_path_within_root(project_root: &Path, file_path: &str) -> Option<PathBuf> {
    let resolved = project_root.join(file_path);
    let root = project_root.canonicalize().unwrap_or_else(|_| project_root.to_path_buf());
    // Lexical check.
    let norm = normalize_lexical(&resolved);
    if !norm.starts_with(&root) && !resolved.starts_with(project_root) {
        // fall through to canonicalize check
    }
    match resolved.canonicalize() {
        Ok(real) => {
            if real.starts_with(&root) {
                Some(real)
            } else {
                None
            }
        }
        Err(_) => {
            // ENOENT: allow the lexical path if it stays within root.
            if norm.starts_with(&root) || resolved.starts_with(project_root) {
                Some(resolved)
            } else {
                None
            }
        }
    }
}

fn normalize_lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Floor a byte index to a char boundary (so slicing never panics on UTF-8).
fn floor_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut i = idx;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}
