//! Rust extractor — port of `extraction/languages/rust.ts`.
//!
//! Rust has no classes; methods live in `impl` blocks and take their receiver
//! type from the enclosing `impl` (resolved via `get_receiver_type`). Traits are
//! the interface-like kind.

use crate::extraction::extractor::{child_by_field, node_text, LanguageExtractor};
use crate::types::{Language, NodeKind};
use tree_sitter::Node as TsNode;

pub struct RustExtractor;

impl LanguageExtractor for RustExtractor {
    fn language(&self) -> Language {
        Language::Rust
    }
    fn function_types(&self) -> &[&str] {
        // `function_signature_item` is a trait method declaration (`fn f(&self);`).
        &["function_item", "function_signature_item"]
    }
    fn method_types(&self) -> &[&str] {
        &["function_item", "function_signature_item"]
    }
    fn interface_types(&self) -> &[&str] {
        &["trait_item"]
    }
    fn struct_types(&self) -> &[&str] {
        &["struct_item"]
    }
    fn enum_types(&self) -> &[&str] {
        &["enum_item"]
    }
    fn enum_member_types(&self) -> &[&str] {
        &["enum_variant"]
    }
    fn type_alias_types(&self) -> &[&str] {
        &["type_item"]
    }
    fn import_types(&self) -> &[&str] {
        &["use_declaration"]
    }
    fn call_types(&self) -> &[&str] {
        &["call_expression"]
    }
    fn variable_types(&self) -> &[&str] {
        &["let_declaration", "const_item", "static_item"]
    }
    fn interface_kind(&self) -> NodeKind {
        NodeKind::Trait
    }

    fn is_exported(&self, node: TsNode<'_>, _source: &str) -> bool {
        // `pub` (any form) on the item itself.
        for i in 0..node.child_count() {
            if node.child(i).map(|c| c.kind()) == Some("visibility_modifier") {
                return true;
            }
        }
        false
    }

    /// Receiver type for an `impl` method — the implementing type of the enclosing
    /// `impl` block (`impl Foo`/`impl Trait for Foo` → `Foo`, the last
    /// type_identifier; generics unwrapped). Port of `getReceiverType`.
    fn get_receiver_type(&self, node: TsNode<'_>, source: &str) -> Option<String> {
        let mut parent = node.parent();
        while let Some(p) = parent {
            if p.kind() == "impl_item" {
                // Last direct type_identifier is the implementing type.
                let mut last_type: Option<TsNode> = None;
                for i in 0..p.named_child_count() {
                    if let Some(c) = p.named_child(i) {
                        if c.kind() == "type_identifier" {
                            last_type = Some(c);
                        }
                    }
                }
                if let Some(t) = last_type {
                    return Some(node_text(t, source).to_string());
                }
                // `impl<T> MyStruct<T>` — the generic_type's inner type_identifier.
                for i in 0..p.named_child_count() {
                    if let Some(c) = p.named_child(i) {
                        if c.kind() == "generic_type" {
                            for j in 0..c.named_child_count() {
                                if let Some(inner) = c.named_child(j) {
                                    if inner.kind() == "type_identifier" {
                                        return Some(node_text(inner, source).to_string());
                                    }
                                }
                            }
                        }
                    }
                }
                return None;
            }
            parent = p.parent();
        }
        None
    }

    /// Declared return type, normalized to the bare type for chained-receiver
    /// inference (`-> Self` → `self`, `-> &Foo` → `Foo`, `-> Vec<Foo>` → `Vec`).
    /// Port of `extractRustReturnType`.
    fn get_return_type(&self, node: TsNode<'_>, source: &str) -> Option<String> {
        let mut rt = child_by_field(node, "return_type")?;
        if rt.kind() == "reference_type" {
            for i in 0..rt.named_child_count() {
                if let Some(c) = rt.named_child(i) {
                    if matches!(c.kind(), "type_identifier" | "scoped_type_identifier" | "generic_type") {
                        rt = c;
                        break;
                    }
                }
            }
        }
        if matches!(rt.kind(), "primitive_type" | "unit_type" | "tuple_type") {
            return None;
        }
        // Strip generic args, take the last `::` segment.
        let text = node_text(rt, source).trim().to_string();
        let no_generics = strip_generics(&text);
        let last = no_generics.split("::").last()?.trim();
        if last.is_empty() || !is_ident(last) {
            return None;
        }
        Some(if last == "Self" { "self".to_string() } else { last.to_string() })
    }

    fn get_signature(&self, node: TsNode<'_>, source: &str) -> Option<String> {
        let params = child_by_field(node, "parameters")?;
        let mut sig = node_text(params, source).to_string();
        if let Some(rt) = child_by_field(node, "return_type") {
            sig.push_str(" -> ");
            sig.push_str(node_text(rt, source));
        }
        Some(sig)
    }

    /// `use a::b::{c, d}` → the root crate/module (`a`). Port of `extractImport`.
    fn import_module(&self, node: TsNode<'_>, source: &str) -> Option<(String, String)> {
        let use_arg = (0..node.named_child_count()).filter_map(|i| node.named_child(i)).find(|c| {
            matches!(c.kind(), "scoped_use_list" | "scoped_identifier" | "use_list" | "identifier" | "use_wildcard")
        })?;
        let root = root_module(use_arg, source);
        if root.is_empty() {
            return None;
        }
        Some((root, node_text(node, source).trim().to_string()))
    }
}

/// The root crate/module of a `use` path (`crate::a::b` → `crate`, `a::b` → `a`).
fn root_module(node: TsNode<'_>, source: &str) -> String {
    let Some(first) = node.named_child(0) else {
        return node_text(node, source).to_string();
    };
    match first.kind() {
        "identifier" | "crate" | "super" | "self" => node_text(first, source).to_string(),
        "scoped_identifier" => root_module(first, source),
        _ => node_text(first, source).to_string(),
    }
}

fn strip_generics(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = (depth - 1).max(0),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

fn is_ident(s: &str) -> bool {
    let mut ch = s.chars();
    matches!(ch.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use crate::extraction::extract_file;
    use crate::types::{Language, NodeKind};

    #[test]
    fn rust_extractor_shapes() {
        let src = r#"
use std::collections::HashMap;
use crate::types::{Node, Edge};

pub const MAX: usize = 10;
static GLOBAL: i32 = 0;

pub struct Widget { id: u32 }

pub enum Color { Red, Green, Blue }

pub trait Render {
    fn render(&self) -> String;
}

impl Widget {
    pub fn new() -> Widget { Widget { id: 0 } }
    fn helper(&self) { compute(); }
}

impl Render for Widget {
    fn render(&self) -> String { format!("{}", self.id) }
}

pub fn free_fn(x: u32) -> u32 { x }
"#;
        let r = extract_file("w.rs", src, Language::Rust);
        let by = |k: NodeKind| r.nodes.iter().filter(|n| n.kind == k).map(|n| n.name.as_str()).collect::<Vec<_>>();
        assert!(by(NodeKind::Struct).contains(&"Widget"), "structs: {:?}", by(NodeKind::Struct));
        assert!(by(NodeKind::Enum).contains(&"Color"));
        assert!(by(NodeKind::EnumMember).contains(&"Red"), "variants: {:?}", by(NodeKind::EnumMember));
        assert!(by(NodeKind::Trait).contains(&"Render"), "traits: {:?}", by(NodeKind::Trait));
        // const/static extract as `variable` (matches the TS rust extractor).
        assert!(by(NodeKind::Variable).contains(&"MAX"), "vars: {:?}", by(NodeKind::Variable));
        assert!(by(NodeKind::Variable).contains(&"GLOBAL"));
        assert!(by(NodeKind::Function).contains(&"free_fn"), "fns: {:?}", by(NodeKind::Function));
        // impl methods become methods with the impl's type as receiver.
        let methods = by(NodeKind::Method);
        assert!(methods.contains(&"new"), "methods: {:?}", methods);
        assert!(methods.contains(&"helper"));
        assert!(methods.contains(&"render"));
        // `new`'s qualified name carries the receiver type.
        assert!(r.nodes.iter().any(|n| n.kind == NodeKind::Method && n.name == "new" && n.qualified_name.contains("Widget")), "qn");
        // method-call `compute()` staged.
        assert!(r.unresolved_references.iter().any(|u| u.reference_name == "compute"));
    }

    #[test]
    fn rust_use_bindings_full_paths() {
        use crate::types::{EdgeKind, ReferenceKind};
        let src = r#"
use crate::types::{Node, Edge};
use crate::db::Store;
use std::collections::HashMap;
use super::graph::Graph;
"#;
        let r = extract_file("src/resolution/mod.rs", src, Language::Rust);
        let imps: Vec<&str> = r
            .unresolved_references
            .iter()
            .filter(|u| u.reference_kind == ReferenceKind::Edge(EdgeKind::Imports))
            .map(|u| u.reference_name.as_str())
            .collect();
        // Each use-leaf becomes an `imports` ref carrying its full scoped path.
        assert!(imps.contains(&"crate::types::Node"), "use refs: {:?}", imps);
        assert!(imps.contains(&"crate::types::Edge"), "use refs: {:?}", imps);
        assert!(imps.contains(&"crate::db::Store"), "use refs: {:?}", imps);
        assert!(imps.contains(&"super::graph::Graph"), "use refs: {:?}", imps);
    }
}
