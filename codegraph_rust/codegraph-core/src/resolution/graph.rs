//! In-memory resolution graph — the Rust analog of the TS `ResolutionContext`
//! (`resolution/index.ts` `createContext` + warmed caches). The TS resolver keeps
//! SQLite-indexed lookups + LRU caches; here we load every node once and build
//! hash indices, which is simpler and fits k8s (~167k nodes) in memory.

use crate::types::{Edge, EdgeKind, Node, NodeKind};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// A Go import binding: local name (alias or package basename) → import path.
#[derive(Debug, Clone)]
pub struct ImportMapping {
    pub local_name: String,
    pub source: String,
}

/// go.mod module path + root (port of `go-module.ts`).
#[derive(Debug, Clone)]
pub struct GoModule {
    pub module_path: String,
}

/// The resolution graph: all nodes + indices + accumulating edges + per-file
/// caches. Built once from the DB, queried immutably during resolution, then
/// mutated only to append resolved/synthesized edges before persisting.
pub struct Graph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    by_id: HashMap<String, usize>,
    by_name: HashMap<String, Vec<usize>>,
    by_qname: HashMap<String, Vec<usize>>,
    by_kind: HashMap<NodeKind, Vec<usize>>,
    by_file: HashMap<String, Vec<usize>>,
    known_names: HashSet<String>,
    repo_root: PathBuf,
    go_module: Option<GoModule>,
    file_cache: HashMap<String, Option<String>>,
    import_cache: HashMap<String, Vec<ImportMapping>>,
    // Edge adjacency by node id — built lazily for the synthesizer pass.
    out_adj: HashMap<String, Vec<usize>>,
    in_adj: HashMap<String, Vec<usize>>,
}

impl Graph {
    pub fn build(nodes: Vec<Node>, edges: Vec<Edge>, repo_root: &Path) -> Self {
        let mut by_id = HashMap::with_capacity(nodes.len());
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_qname: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_kind: HashMap<NodeKind, Vec<usize>> = HashMap::new();
        let mut by_file: HashMap<String, Vec<usize>> = HashMap::new();
        let mut known_names = HashSet::new();
        for (i, n) in nodes.iter().enumerate() {
            by_id.insert(n.id.clone(), i);
            by_name.entry(n.name.clone()).or_default().push(i);
            by_qname.entry(n.qualified_name.clone()).or_default().push(i);
            by_kind.entry(n.kind).or_default().push(i);
            by_file.entry(n.file_path.clone()).or_default().push(i);
            known_names.insert(n.name.clone());
        }
        let go_module = load_go_module(repo_root);
        Graph {
            nodes,
            edges,
            by_id,
            by_name,
            by_qname,
            by_kind,
            by_file,
            known_names,
            repo_root: repo_root.to_path_buf(),
            go_module,
            file_cache: HashMap::new(),
            import_cache: HashMap::new(),
            out_adj: HashMap::new(),
            in_adj: HashMap::new(),
        }
    }

    // --- node lookups (return index slices; use `node(i)` to deref) ---
    pub fn node(&self, i: usize) -> &Node {
        &self.nodes[i]
    }
    pub fn nodes_by_name(&self, name: &str) -> &[usize] {
        self.by_name.get(name).map(|v| v.as_slice()).unwrap_or(&[])
    }
    pub fn nodes_by_qname(&self, qn: &str) -> &[usize] {
        self.by_qname.get(qn).map(|v| v.as_slice()).unwrap_or(&[])
    }
    pub fn nodes_by_kind(&self, kind: NodeKind) -> &[usize] {
        self.by_kind.get(&kind).map(|v| v.as_slice()).unwrap_or(&[])
    }
    pub fn nodes_in_file(&self, path: &str) -> &[usize] {
        self.by_file.get(path).map(|v| v.as_slice()).unwrap_or(&[])
    }
    pub fn idx_of(&self, id: &str) -> Option<usize> {
        self.by_id.get(id).copied()
    }
    pub fn node_by_id(&self, id: &str) -> Option<&Node> {
        self.idx_of(id).map(|i| &self.nodes[i])
    }
    pub fn known_name(&self, name: &str) -> bool {
        self.known_names.contains(name)
    }
    pub fn go_module(&self) -> Option<&GoModule> {
        self.go_module.as_ref()
    }

    /// Read a repo-relative file (cached). Used for Go import-mapping extraction.
    pub fn read_file(&mut self, rel: &str) -> Option<String> {
        if let Some(c) = self.file_cache.get(rel) {
            return c.clone();
        }
        let content = std::fs::read_to_string(self.repo_root.join(rel)).ok();
        self.file_cache.insert(rel.to_string(), content.clone());
        content
    }

    /// Go import mappings for a file (cached). Port of `extractGoImports`.
    pub fn go_imports(&mut self, rel: &str) -> Vec<ImportMapping> {
        if let Some(m) = self.import_cache.get(rel) {
            return m.clone();
        }
        let mappings = self
            .read_file(rel)
            .map(|c| extract_go_imports(&c))
            .unwrap_or_default();
        self.import_cache.insert(rel.to_string(), mappings.clone());
        mappings
    }

    /// Pre-extract import mappings for a set of files so the resolution loop can
    /// read them immutably via `cached_imports`. Parses each file's imports once.
    pub fn prewarm_imports<I: IntoIterator<Item = String>>(&mut self, files: I) {
        for f in files {
            if !self.import_cache.contains_key(&f) {
                let mappings = self
                    .read_file(&f)
                    .map(|c| extract_go_imports(&c))
                    .unwrap_or_default();
                self.import_cache.insert(f, mappings);
            }
        }
    }
    pub fn cached_imports(&self, rel: &str) -> &[ImportMapping] {
        self.import_cache.get(rel).map(|v| v.as_slice()).unwrap_or(&[])
    }

    // --- edge adjacency (built once for the synthesizer pass) ---
    pub fn build_adjacency(&mut self) {
        self.out_adj.clear();
        self.in_adj.clear();
        for (i, e) in self.edges.iter().enumerate() {
            self.out_adj.entry(e.source.clone()).or_default().push(i);
            self.in_adj.entry(e.target.clone()).or_default().push(i);
        }
    }
    /// Outgoing edges of `id` matching any of `kinds`.
    pub fn outgoing(&self, id: &str, kinds: &[EdgeKind]) -> Vec<&Edge> {
        self.out_adj
            .get(id)
            .map(|idxs| {
                idxs.iter()
                    .map(|&i| &self.edges[i])
                    .filter(|e| kinds.contains(&e.kind))
                    .collect()
            })
            .unwrap_or_default()
    }
    pub fn incoming(&self, id: &str, kinds: &[EdgeKind]) -> Vec<&Edge> {
        self.in_adj
            .get(id)
            .map(|idxs| {
                idxs.iter()
                    .map(|&i| &self.edges[i])
                    .filter(|e| kinds.contains(&e.kind))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Append edges and keep adjacency in sync (so a later synthesizer pass sees
    /// edges an earlier one added, as the TS persists-between-passes flow does).
    pub fn add_edges(&mut self, new_edges: Vec<Edge>) {
        for e in new_edges {
            let i = self.edges.len();
            self.out_adj.entry(e.source.clone()).or_default().push(i);
            self.in_adj.entry(e.target.clone()).or_default().push(i);
            self.edges.push(e);
        }
    }
}

/// Port of `loadGoModule`: read the root `go.mod`'s `module` directive.
fn load_go_module(repo_root: &Path) -> Option<GoModule> {
    let content = std::fs::read_to_string(repo_root.join("go.mod")).ok()?;
    let stripped: String = content
        .lines()
        .map(|l| l.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?m)^\s*module\s+(\S+)\s*$").unwrap());
    let m = re.captures(&stripped)?;
    let module_path = m.get(1)?.as_str().trim_matches(['"', '\'']).to_string();
    if module_path.is_empty() {
        return None;
    }
    Some(GoModule { module_path })
}

/// Port of `extractGoImports`: single `import [alias] "path"` and block
/// `import ( ... )` forms. Returns (local_name, source) mappings.
fn extract_go_imports(content: &str) -> Vec<ImportMapping> {
    let mut out = Vec::new();
    static SINGLE: OnceLock<Regex> = OnceLock::new();
    static BLOCK: OnceLock<Regex> = OnceLock::new();
    static LINE: OnceLock<Regex> = OnceLock::new();
    let single = SINGLE.get_or_init(|| Regex::new(r#"import\s+(?:(\w+)\s+)?["']([^"']+)["']"#).unwrap());
    let block = BLOCK.get_or_init(|| Regex::new(r"(?s)import\s*\(\s*([^)]+)\s*\)").unwrap());
    let line = LINE.get_or_init(|| Regex::new(r#"(?:(\w+)\s+)?["']([^"']+)["']"#).unwrap());

    let push = |out: &mut Vec<ImportMapping>, alias: Option<&str>, source: &str| {
        let pkg = source.rsplit('/').next().unwrap_or(source);
        out.push(ImportMapping {
            local_name: alias.unwrap_or(pkg).to_string(),
            source: source.to_string(),
        });
    };
    for cap in single.captures_iter(content) {
        push(&mut out, cap.get(1).map(|m| m.as_str()), &cap[2]);
    }
    for blk in block.captures_iter(content) {
        for cap in line.captures_iter(&blk[1]) {
            push(&mut out, cap.get(1).map(|m| m.as_str()), &cap[2]);
        }
    }
    out
}
