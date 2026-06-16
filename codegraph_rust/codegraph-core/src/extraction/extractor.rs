//! The `LanguageExtractor` contract — port of `tree-sitter-types.ts:80`.
//!
//! It is NOT "run tree-sitter and read the names": each implementation is a
//! per-grammar adapter encoding that grammar's idiosyncrasies (Go methods are
//! top-level and carry a receiver; `type_spec` wraps struct/interface defs; an
//! identifier is exported iff it starts uppercase). The base engine
//! (`engine.rs`) drives these hooks; per-language configs live in `languages/`.
//!
//! Only the hooks the ported languages actually need are defined; the rest of the
//! ~40-hook TS surface (preParse, classifyClassNode, visitNode, …) is added as
//! languages that need them are ported (docs/rust-build-plan.md §3).

use crate::types::{Language, NodeKind};
use tree_sitter::Node as TsNode;

/// Per-language extraction configuration + hooks. `Send + Sync` so a single
/// shared `&dyn LanguageExtractor` can drive `rayon` parallel extraction.
pub trait LanguageExtractor: Send + Sync {
    fn language(&self) -> Language;

    // --- Node-type → concept mappings (each grammar names things differently) ---
    fn function_types(&self) -> &[&str];
    fn class_types(&self) -> &[&str] {
        &[]
    }
    fn method_types(&self) -> &[&str] {
        &[]
    }
    fn interface_types(&self) -> &[&str] {
        &[]
    }
    fn struct_types(&self) -> &[&str] {
        &[]
    }
    fn enum_types(&self) -> &[&str] {
        &[]
    }
    fn enum_member_types(&self) -> &[&str] {
        &[]
    }
    fn type_alias_types(&self) -> &[&str] {
        &[]
    }
    fn import_types(&self) -> &[&str] {
        &[]
    }
    fn call_types(&self) -> &[&str] {
        &[]
    }
    fn variable_types(&self) -> &[&str] {
        &[]
    }
    fn field_types(&self) -> &[&str] {
        &[]
    }
    fn property_types(&self) -> &[&str] {
        &[]
    }

    // --- Field-name mappings ---
    fn name_field(&self) -> &str {
        "name"
    }
    fn body_field(&self) -> &str {
        "body"
    }

    // --- Config flags ---
    /// Whether methods can be top-level without an enclosing class (Go: true).
    fn methods_are_top_level(&self) -> bool {
        false
    }
    /// NodeKind for interface-like declarations (Rust: Trait). Default Interface.
    fn interface_kind(&self) -> NodeKind {
        NodeKind::Interface
    }

    // --- Hooks (return None / default to fall through) ---

    /// Receiver/owner type of a method (`func (sl *scrapeLoop) run()` → `scrapeLoop`).
    /// When present, it's woven into the method's qualified name and used for the
    /// struct→method `contains` edge.
    fn get_receiver_type(&self, _node: TsNode<'_>, _source: &str) -> Option<String> {
        None
    }
    /// Normalized return/result type name (bare class, pointer unwrapped) — stored
    /// as `Node.return_type` for chained-receiver inference (#645).
    fn get_return_type(&self, _node: TsNode<'_>, _source: &str) -> Option<String> {
        None
    }
    /// A function/method signature string for display + FTS.
    fn get_signature(&self, _node: TsNode<'_>, _source: &str) -> Option<String> {
        None
    }
    /// Whether the symbol is exported/public.
    fn is_exported(&self, _node: TsNode<'_>, _source: &str) -> bool {
        false
    }
    /// Whether a variable declaration is a constant (const vs var/let).
    fn is_const(&self, _node: TsNode<'_>) -> bool {
        false
    }
    /// Resolve the real kind of a `type_alias` declaration — Go `type_spec` wraps
    /// structs/interfaces (`type Foo struct {…}` → Struct). None keeps TypeAlias.
    fn resolve_type_alias_kind(&self, _node: TsNode<'_>, _source: &str) -> Option<NodeKind> {
        None
    }
}

// =============================================================================
// Shared helpers — port of `tree-sitter-helpers.ts`
// =============================================================================

/// Text of a syntax node. tree-sitter byte offsets are UTF-8 char boundaries.
pub fn node_text<'s>(node: TsNode<'_>, source: &'s str) -> &'s str {
    &source[node.start_byte()..node.end_byte()]
}

/// Find a child by field name (`getChildByField`).
pub fn child_by_field<'t>(node: TsNode<'t>, field: &str) -> Option<TsNode<'t>> {
    node.child_by_field_name(field)
}

/// Find the first named child whose kind is in `types` (`findChildByTypes`).
pub fn find_child_by_types<'t>(node: TsNode<'t>, types: &[&str]) -> Option<TsNode<'t>> {
    for i in 0..node.named_child_count() {
        if let Some(c) = node.named_child(i) {
            if types.contains(&c.kind()) {
                return Some(c);
            }
        }
    }
    None
}

/// Extract a symbol name (`extractName`) — the Go-relevant subset: name field
/// first, else the first identifier-like named child, else `<anonymous>`.
pub fn extract_name(node: TsNode<'_>, source: &str, extractor: &dyn LanguageExtractor) -> String {
    if let Some(name_node) = child_by_field(node, extractor.name_field()) {
        return node_text(name_node, source).to_string();
    }
    // Fall back to the first identifier-like named child.
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            if matches!(
                child.kind(),
                "identifier" | "type_identifier" | "simple_identifier" | "constant"
            ) {
                return node_text(child, source).to_string();
            }
        }
    }
    "<anonymous>".to_string()
}

/// The docstring/comment immediately preceding a node (`getPrecedingDocstring`),
/// with comment markers stripped. Walks contiguous leading comment siblings.
pub fn preceding_docstring(node: TsNode<'_>, source: &str) -> Option<String> {
    let mut sibling = node.prev_named_sibling();
    let mut comments: Vec<String> = Vec::new();
    while let Some(s) = sibling {
        match s.kind() {
            "comment" | "line_comment" | "block_comment" | "documentation_comment" => {
                comments.insert(0, node_text(s, source).to_string());
                sibling = s.prev_named_sibling();
            }
            _ => break,
        }
    }
    if comments.is_empty() {
        return None;
    }
    let cleaned: Vec<String> = comments.iter().map(|c| clean_comment_markers(c)).collect();
    Some(cleaned.join("\n").trim().to_string())
}

/// Strip comment-syntax markers (the Go-relevant subset: `//`, `///`, `/* */`).
fn clean_comment_markers(comment: &str) -> String {
    let mut c = comment.trim().to_string();
    if c.starts_with("/*") {
        c = c
            .trim_start_matches("/*")
            .trim_start_matches(['*', '!'])
            .to_string();
        c = c.trim_end_matches("*/").trim_end_matches('*').to_string();
    }
    c.lines()
        .map(|line| {
            let l = line.trim_start();
            let l = l
                .strip_prefix("///")
                .or_else(|| l.strip_prefix("//!"))
                .or_else(|| l.strip_prefix("//"))
                .unwrap_or(l);
            let l = l.strip_prefix('*').unwrap_or(l);
            l.trim()
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}
