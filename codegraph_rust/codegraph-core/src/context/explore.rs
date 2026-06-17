//! `codegraph_explore` rendering — port of the `handleExplore` core (mcp/tools.ts).
//!
//! Find the relevant subgraph, add graph-aware glue + named-symbol seeds, group
//! by file, rank files by Random-Walk-with-Restart connectivity (the signal text
//! search lacks), and render contiguous, line-numbered source within the
//! size-scaled output budget. This is what makes one call Read-equivalent.

use super::budgets::ExploreOutputBudget;
use super::{BuildOptions, ContextBuilder};
use crate::graph;
use crate::search::query_utils as qu;
use crate::types::{Edge, EdgeKind, Node, NodeKind};
use anyhow::Result;
use std::collections::{HashMap, HashSet};

const CALLABLE: &[NodeKind] = &[NodeKind::Method, NodeKind::Function, NodeKind::Component];

impl<'a> ContextBuilder<'a> {
    /// Build the explore response markdown.
    pub fn explore_markdown(
        &self,
        query: &str,
        max_files: usize,
        budget: &ExploreOutputBudget,
    ) -> Result<String> {
        let mut sg = self.find_relevant_context(query, &BuildOptions::explore())?;
        if sg.nodes.is_empty() {
            return Ok(format!("No relevant code found for \"{}\"", query));
        }

        // --- Graph-aware glue: pull callers/callees of roots that live in files
        // the subgraph already surfaces (add wiring without dragging in new files).
        let subgraph_files: HashSet<String> = sg.nodes.values().map(|n| n.file_path.clone()).collect();
        let mut glue = 0;
        const GLUE_CAP: usize = 60;
        let roots = sg.roots.clone();
        for root in &roots {
            if glue >= GLUE_CAP {
                break;
            }
            let mut neighbors: Vec<Node> = Vec::new();
            if let Ok(hops) = graph::callers(self.store, root, 1) {
                neighbors.extend(hops.into_iter().map(|h| h.node));
            }
            if let Ok(hops) = graph::callees(self.store, root, 1) {
                neighbors.extend(hops.into_iter().map(|h| h.node));
            }
            for nb in neighbors {
                if glue >= GLUE_CAP {
                    break;
                }
                if sg.nodes.contains_key(&nb.id) || !subgraph_files.contains(&nb.file_path) {
                    continue;
                }
                sg.nodes.insert(nb.id.clone(), nb);
                glue += 1;
            }
        }

        // --- Named-symbol seeding: ensure every identifier the agent named has its
        // substantive definition (and thus its file) in the subgraph.
        let mut named_seed_ids: HashSet<String> = HashSet::new();
        // Order-preserving dedup (the TS uses an insertion-ordered Set): a plain
        // HashSet would make which 16 tokens survive — and thus the surfaced
        // files — nondeterministic across runs.
        let tokens: Vec<String> = {
            let mut seen = HashSet::new();
            query
                .split(|c: char| c.is_whitespace() || "(),[]".contains(c))
                .map(|t| t.trim_end_matches(|c: char| "?.,:;".contains(c)).to_string())
                .filter(|t| t.len() >= 3 && is_ident_token(t))
                .filter(|t| seen.insert(t.clone()))
                .take(16)
                .collect()
        };
        // PascalCase tokens in the query are type/file disambiguators: when the
        // agent writes "DataRequest task validate", the `task`/`validate` it wants
        // are DataRequest's, not same-named overloads elsewhere. Bias overloaded
        // names toward the file/class the query also names (port of explore's
        // typeTokens/inNamedContext); otherwise a common name seeds the wrong def.
        let type_tokens: Vec<String> =
            tokens.iter().filter(|t| is_pascal(t)).map(|t| t.to_ascii_lowercase()).collect();
        for t in &tokens {
            let raw = self.store.get_nodes_by_name_full(t, 60)?;
            let mut cands: Vec<Node> = raw
                .into_iter()
                .filter(|n| CALLABLE.contains(&n.kind) && !qu::is_test_file(&n.file_path))
                .collect();
            cands.sort_by(|a, b| {
                body_lines(b).cmp(&body_lines(a)).then_with(|| a.id.cmp(&b.id))
            });
            let picks: Vec<Node> = if cands.len() <= 3 {
                cands
            } else {
                let in_ctx: Vec<Node> = cands
                    .iter()
                    .filter(|n| {
                        let fp = n.file_path.to_ascii_lowercase();
                        let qn = n.qualified_name.to_ascii_lowercase();
                        type_tokens.iter().any(|ct| fp.contains(ct.as_str()) || qn.contains(ct.as_str()))
                    })
                    .cloned()
                    .collect();
                if !in_ctx.is_empty() {
                    in_ctx.into_iter().take(4).collect()
                } else {
                    cands.into_iter().take(1).collect()
                }
            };
            for n in picks {
                named_seed_ids.insert(n.id.clone());
                sg.nodes.entry(n.id.clone()).or_insert(n);
            }
        }

        // --- Group nodes by file, score by relevance.
        let entry_ids: HashSet<String> =
            sg.roots.iter().cloned().chain(named_seed_ids.iter().cloned()).collect();
        let mut connected: HashSet<String> = HashSet::new();
        for e in &sg.edges {
            if entry_ids.contains(&e.source) {
                connected.insert(e.target.clone());
            }
            if entry_ids.contains(&e.target) {
                connected.insert(e.source.clone());
            }
        }
        let mut groups: HashMap<String, (Vec<Node>, f64)> = HashMap::new();
        for node in sg.nodes.values() {
            if matches!(node.kind, NodeKind::Import | NodeKind::Export) {
                continue;
            }
            let entry = groups.entry(node.file_path.clone()).or_insert((Vec::new(), 0.0));
            entry.0.push(node.clone());
            entry.1 += if named_seed_ids.contains(&node.id) {
                50.0
            } else if entry_ids.contains(&node.id) {
                10.0
            } else if connected.contains(&node.id) {
                3.0
            } else {
                1.0
            };
        }
        let mut relevant: Vec<(String, (Vec<Node>, f64))> =
            groups.into_iter().filter(|(_, g)| g.1 >= 3.0).collect();

        // Hard-exclude test/spec files unless the query is about tests (and ≥2 remain).
        let query_mentions_tests = {
            let q = query.to_ascii_lowercase();
            ["test", "tests", "testing", "spec", "verify", "verifies"].iter().any(|w| q.contains(w))
        };
        if !query_mentions_tests {
            let non_low: Vec<_> = relevant.iter().filter(|(p, _)| !is_low_value(p)).cloned().collect();
            if non_low.len() >= 2 {
                relevant = non_low;
            }
        }

        // Term hits per file.
        let unique_terms: Vec<String> = {
            let mut s: Vec<String> = query
                .to_ascii_lowercase()
                .split_whitespace()
                .filter(|t| t.len() >= 3)
                .map(|t| t.to_string())
                .collect();
            s.sort();
            s.dedup();
            s
        };
        let mut term_hits: HashMap<String, usize> = HashMap::new();
        for (fp, group) in &relevant {
            let hay = format!(
                "{} {}",
                fp.to_ascii_lowercase(),
                group.0.iter().map(|n| n.name.to_ascii_lowercase()).collect::<Vec<_>>().join(" ")
            );
            let hits = unique_terms.iter().filter(|t| hay.contains(t.as_str())).count();
            term_hits.insert(fp.clone(), hits);
        }

        // RWR graph relevance.
        let node_ids: Vec<String> = sg.nodes.keys().cloned().collect();
        let node_rwr = compute_graph_relevance(&node_ids, &sg.edges, &entry_ids);
        let mut file_graph: HashMap<String, f64> = HashMap::new();
        for node in sg.nodes.values() {
            *file_graph.entry(node.file_path.clone()).or_insert(0.0) +=
                node_rwr.get(&node.id).copied().unwrap_or(0.0);
        }
        let max_graph = file_graph.values().copied().fold(0.0_f64, f64::max);

        // Central files: top-2 graph-central that also match a term.
        let mut central_sorted: Vec<(&String, &f64)> = file_graph
            .iter()
            .filter(|(fp, g)| **g > 0.0 && term_hits.get(*fp).copied().unwrap_or(0) >= 1)
            .collect();
        // Tiebreak on path so equal-score files pick the same 2 every run
        // (file_graph is a HashMap — iteration order is otherwise nondeterministic).
        central_sorted.sort_by(|a, b| {
            b.1.partial_cmp(a.1).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.0.cmp(b.0))
        });
        let central: HashSet<String> = central_sorted.iter().take(2).map(|(f, _)| (*f).clone()).collect();

        // Files defining a named/root symbol.
        let entry_files: HashSet<String> = entry_ids
            .iter()
            .filter_map(|id| sg.nodes.get(id).map(|n| n.file_path.clone()))
            .collect();
        let named_seed_files: HashSet<String> = named_seed_ids
            .iter()
            .filter_map(|id| sg.nodes.get(id).map(|n| n.file_path.clone()))
            .collect();

        // Relevance gate.
        if max_graph > 0.0 {
            let gated: Vec<_> = relevant
                .iter()
                .filter(|(fp, _)| {
                    file_graph.get(fp).copied().unwrap_or(0.0) >= max_graph * 0.06
                        || central.contains(fp)
                        || entry_files.contains(fp)
                        || term_hits.get(fp).copied().unwrap_or(0) >= 2
                })
                .cloned()
                .collect();
            if gated.len() >= 2 {
                relevant = gated;
            }
        }

        // Sort files: named first, graph connectivity, term hits, low-value, generated, score.
        relevant.sort_by(|a, b| {
            let a_named = named_seed_files.contains(&a.0);
            let b_named = named_seed_files.contains(&b.0);
            if a_named != b_named {
                return b_named.cmp(&a_named);
            }
            let ag = file_graph.get(&a.0).copied().unwrap_or(0.0);
            let bg = file_graph.get(&b.0).copied().unwrap_or(0.0);
            if (ag - bg).abs() > max_graph * 0.01 {
                return bg.partial_cmp(&ag).unwrap_or(std::cmp::Ordering::Equal);
            }
            let ah = term_hits.get(&a.0).copied().unwrap_or(0);
            let bh = term_hits.get(&b.0).copied().unwrap_or(0);
            if ah != bh {
                return bh.cmp(&ah);
            }
            let al = is_low_value(&a.0);
            let bl = is_low_value(&b.0);
            if al != bl {
                return al.cmp(&bl);
            }
            let age = qu::is_generated_file(&a.0);
            let bge = qu::is_generated_file(&b.0);
            if age != bge {
                return age.cmp(&bge);
            }
            // Final tiebreak on path: `groups` is a HashMap, so without a total
            // order the rendered file order would vary run-to-run on ties.
            b.1 .1
                .partial_cmp(&a.1 .1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });

        // --- Render.
        let mut lines: Vec<String> = vec![
            format!("## Exploration: {}", query),
            String::new(),
            format!("Found {} symbols across {} files.", sg.nodes.len(), relevant.len()),
            String::new(),
        ];

        // Call paths (flow among the key symbols).
        let call_paths = self.build_call_paths_section(&sg);
        if !call_paths.trim().is_empty() {
            lines.push(call_paths.trim_end().to_string());
            lines.push(String::new());
        }

        // Relationships.
        if budget.include_relationships {
            let significant: Vec<&Edge> = sg.edges.iter().filter(|e| e.kind != EdgeKind::Contains).collect();
            if !significant.is_empty() {
                lines.push("### Relationships".to_string());
                lines.push(String::new());
                let mut by_kind: indexmap::IndexMap<&str, Vec<(String, String)>> = indexmap::IndexMap::new();
                for e in significant {
                    let (Some(s), Some(t)) = (sg.nodes.get(&e.source), sg.nodes.get(&e.target)) else {
                        continue;
                    };
                    by_kind.entry(e.kind.as_str()).or_default().push((s.name.clone(), t.name.clone()));
                }
                for (kind, edges) in by_kind {
                    lines.push(format!("**{}:**", kind));
                    for (s, t) in edges.iter().take(budget.max_edges_per_relationship_kind) {
                        lines.push(format!("- {} → {}", s, t));
                    }
                    if edges.len() > budget.max_edges_per_relationship_kind {
                        lines.push(format!("- ... and {} more", edges.len() - budget.max_edges_per_relationship_kind));
                    }
                    lines.push(String::new());
                }
            }
        }

        lines.push("### Source Code".to_string());
        lines.push(String::new());
        lines.push("> The code below is the **verbatim, current on-disk source** of these files, line-numbered, byte-for-byte identical to what the Read tool returns. Treat each block as a Read you have already performed: do not Read a file shown here.".to_string());
        lines.push(String::new());

        let mut total_chars: usize = lines.iter().map(|l| l.len() + 1).sum();
        let mut files_included = 0;

        for (file_path, group) in &relevant {
            if files_included >= max_files {
                break;
            }
            // Hard ceiling: never exceed the host's inline cap (a bigger response
            // is externalized to a file the agent must Read back — defeating the point).
            if total_chars >= budget.max_output_chars {
                break;
            }
            let necessary = group.0.iter().any(|n| entry_ids.contains(&n.id));
            if !necessary && total_chars as f64 > budget.max_output_chars as f64 * 0.9 {
                continue;
            }
            let Some(abs) = super::validate_path_within_root(&self.project_root, file_path) else {
                continue;
            };
            let Ok(content) = std::fs::read_to_string(&abs) else { continue };
            let file_lines: Vec<&str> = content.split('\n').collect();
            let lang = group.0.first().map(|n| n.language.as_str()).unwrap_or("");
            let names: Vec<String> = {
                let mut v: Vec<String> = group
                    .0
                    .iter()
                    .filter(|n| !matches!(n.kind, NodeKind::Import | NodeKind::Export))
                    .map(|n| format!("{}({})", n.name, n.kind.as_str()))
                    .collect();
                v.dedup();
                v.truncate(budget.max_symbols_in_file_header);
                v
            };

            // Whole-file rule: a small file comes back entire (the agent would Read
            // it whole anyway). Bounded by the per-file char cap so one god-file
            // can't blow the budget — larger files fall through to clustering.
            let whole_max_lines = 220usize;
            let whole_max_chars = budget.max_chars_per_file;
            if file_lines.len() <= whole_max_lines && content.len() <= whole_max_chars {
                let body = content.trim_end_matches('\n');
                let section = number_lines(body, 1);
                if total_chars + section.len() + 200 > budget.max_output_chars {
                    continue;
                }
                lines.push(format!("#### {} — {}", file_path, names.join(", ")));
                lines.push(String::new());
                lines.push(format!("```{}", lang));
                lines.push(section.clone());
                lines.push("```".to_string());
                lines.push(String::new());
                total_chars += section.len() + 200;
                files_included += 1;
                continue;
            }

            // Cluster nearby symbols into windows.
            let mut symbols: Vec<&Node> = group
                .0
                .iter()
                .filter(|n| n.start_line > 0 && !matches!(n.kind, NodeKind::Import | NodeKind::Export))
                .collect();
            symbols.sort_by_key(|n| n.start_line);
            if symbols.is_empty() {
                continue;
            }
            let mut clusters: Vec<(u32, u32)> = Vec::new();
            for n in &symbols {
                let s = n.start_line;
                let e = n.end_line.max(n.start_line);
                if let Some(last) = clusters.last_mut() {
                    if s <= last.1 + budget.gap_threshold {
                        last.1 = last.1.max(e);
                        continue;
                    }
                }
                clusters.push((s, e));
            }
            let mut rendered: Vec<String> = Vec::new();
            let mut file_chars = 0usize;
            // Per-file cap, never more than the budget remaining.
            let file_cap = budget.max_chars_per_file.min(budget.max_output_chars.saturating_sub(total_chars));
            for (s, e) in clusters {
                if file_chars >= file_cap {
                    break;
                }
                let start = (s.saturating_sub(1)) as usize;
                let end = (e as usize).min(file_lines.len());
                if start >= file_lines.len() {
                    continue;
                }
                let slice = file_lines[start..end.max(start)].join("\n");
                let numbered = number_lines(&slice, s);
                file_chars += numbered.len();
                rendered.push(numbered);
            }
            if rendered.is_empty() {
                continue;
            }
            let section = rendered.join("\n\n…\n\n");
            if total_chars + section.len() + 200 > budget.max_output_chars && files_included > 0 {
                continue;
            }
            lines.push(format!("#### {} — {}", file_path, names.join(", ")));
            lines.push(String::new());
            lines.push(format!("```{}", lang));
            lines.push(section.clone());
            lines.push("```".to_string());
            lines.push(String::new());
            total_chars += section.len() + 200;
            files_included += 1;
        }

        if budget.include_completeness_signal {
            lines.push("_Complete source of the relevant symbols is included above (line-numbered, same as Read). codegraph_node any symbol for its full body + callers/callees._".to_string());
        }

        Ok(lines.join("\n"))
    }
}

fn is_ident_token(t: &str) -> bool {
    let mut chars = t.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {}
        _ => return false,
    }
    t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == ':' || c == '.')
}

fn body_lines(n: &Node) -> u32 {
    n.end_line.saturating_sub(n.start_line)
}

/// A PascalCase type/file disambiguator: starts uppercase, ≥4 chars, has a
/// lowercase letter (so it's not a bare acronym).
fn is_pascal(t: &str) -> bool {
    let mut chars = t.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_uppercase())
        && t.chars().count() >= 4
        && t.chars().any(|c| c.is_ascii_lowercase())
}

fn number_lines(slice: &str, first: u32) -> String {
    slice
        .split('\n')
        .enumerate()
        .map(|(i, l)| format!("{}\t{}", first as usize + i, l))
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_low_value(p: &str) -> bool {
    qu::is_test_file(p) || qu::is_generated_file(p)
}

/// Random-Walk-with-Restart (personalized PageRank) over the subgraph
/// (port of `computeGraphRelevance`). Undirected, α=0.25, 25 iters.
fn compute_graph_relevance(
    node_ids: &[String],
    edges: &[Edge],
    seed_ids: &HashSet<String>,
) -> HashMap<String, f64> {
    let mut out = HashMap::new();
    let n = node_ids.len();
    if n == 0 {
        return out;
    }
    let idx: HashMap<&str, usize> = node_ids.iter().enumerate().map(|(i, s)| (s.as_str(), i)).collect();
    let rank_edges = [
        EdgeKind::Calls, EdgeKind::References, EdgeKind::Extends, EdgeKind::Implements,
        EdgeKind::Overrides, EdgeKind::Instantiates, EdgeKind::Returns, EdgeKind::TypeOf, EdgeKind::Imports,
    ];
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for e in edges {
        if !rank_edges.contains(&e.kind) {
            continue;
        }
        let (Some(&i), Some(&j)) = (idx.get(e.source.as_str()), idx.get(e.target.as_str())) else {
            continue;
        };
        if i == j {
            continue;
        }
        adj[i].push(j);
        adj[j].push(i);
    }
    let mut r = vec![0.0_f64; n];
    let mut rsum = 0.0;
    for id in seed_ids {
        if let Some(&i) = idx.get(id.as_str()) {
            r[i] = 1.0;
            rsum += 1.0;
        }
    }
    if rsum == 0.0 {
        for v in r.iter_mut() {
            *v = 1.0;
        }
        rsum = n as f64;
    }
    for v in r.iter_mut() {
        *v /= rsum;
    }
    let alpha = 0.25;
    let mut s = r.clone();
    for _ in 0..25 {
        let mut next = vec![0.0_f64; n];
        for i in 0..n {
            let si = s[i];
            if si == 0.0 {
                continue;
            }
            let d = adj[i].len();
            if d == 0 {
                next[i] += si;
                continue;
            }
            let share = si / d as f64;
            for &j in &adj[i] {
                next[j] += share;
            }
        }
        for i in 0..n {
            s[i] = (1.0 - alpha) * next[i] + alpha * r[i];
        }
    }
    for i in 0..n {
        out.insert(node_ids[i].clone(), s[i]);
    }
    out
}
