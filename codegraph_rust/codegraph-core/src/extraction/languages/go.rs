//! Go extractor — port of `codegraph/src/extraction/languages/go.ts`.
//!
//! Go's quirks the engine depends on: methods are top-level and carry a receiver
//! (`func (sl *scrapeLoop) run()` → searchable as `scrapeLoop::run`); `type_spec`
//! is the named wrapper for structs/interfaces; a symbol is exported iff its name
//! starts uppercase. Ported FIRST because k8s — the parity target — is Go.

use crate::extraction::extractor::{child_by_field, node_text, LanguageExtractor};
use crate::types::{Language, NodeKind};
use regex::Regex;
use std::sync::OnceLock;
use tree_sitter::Node as TsNode;

pub struct GoExtractor;

impl LanguageExtractor for GoExtractor {
    fn language(&self) -> Language {
        Language::Go
    }

    fn function_types(&self) -> &[&str] {
        &["function_declaration"]
    }
    fn method_types(&self) -> &[&str] {
        &["method_declaration"]
    }
    // interfaces & structs are handled via type_spec → resolve_type_alias_kind.
    fn type_alias_types(&self) -> &[&str] {
        &["type_spec"]
    }
    fn import_types(&self) -> &[&str] {
        &["import_declaration"]
    }
    fn call_types(&self) -> &[&str] {
        &["call_expression"]
    }
    fn variable_types(&self) -> &[&str] {
        &["var_declaration", "short_var_declaration", "const_declaration"]
    }

    fn methods_are_top_level(&self) -> bool {
        true
    }

    fn name_field(&self) -> &str {
        "name"
    }
    fn body_field(&self) -> &str {
        "body"
    }

    /// `type Foo struct {…}` / `type Bar interface {…}` — the inner type is in the
    /// `type` field of the `type_spec`.
    fn resolve_type_alias_kind(&self, node: TsNode<'_>, _source: &str) -> Option<NodeKind> {
        let type_child = child_by_field(node, "type")?;
        match type_child.kind() {
            "struct_type" => Some(NodeKind::Struct),
            "interface_type" => Some(NodeKind::Interface),
            _ => None,
        }
    }

    /// Exported when the name's first letter is uppercase (A–Z).
    fn is_exported(&self, node: TsNode<'_>, source: &str) -> bool {
        if let Some(name_node) = child_by_field(node, "name") {
            if let Some(first) = node_text(name_node, source).chars().next() {
                return first.is_ascii_uppercase();
            }
        }
        false
    }

    fn get_signature(&self, node: TsNode<'_>, source: &str) -> Option<String> {
        let params = child_by_field(node, "parameters")?;
        let mut sig = node_text(params, source).to_string();
        if let Some(result) = child_by_field(node, "result") {
            sig.push(' ');
            sig.push_str(node_text(result, source));
        }
        Some(sig)
    }

    /// Receiver type from a `method_declaration` — `(sl *scrapeLoop)`,
    /// `(s *Stack[T])`, `(*Type)`, `(Type)`. Anchored on `(`, skips an optional
    /// receiver var name, an optional `*`, then captures the type ident (#583).
    fn get_receiver_type(&self, node: TsNode<'_>, source: &str) -> Option<String> {
        let receiver = child_by_field(node, "receiver")?;
        let text = node_text(receiver, source);
        static RE: OnceLock<Regex> = OnceLock::new();
        let re = RE.get_or_init(|| {
            Regex::new(r"\(\s*(?:[A-Za-z_]\w*\s+)?\*?\s*([A-Za-z_]\w*)").unwrap()
        });
        re.captures(text)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
    }

    /// Declared return type normalized to the bare type a chained `New().Method()`
    /// could be called on (#645). Reads `result`: multi-return `(T, error)` → first
    /// result; pointer `*Foo` → `Foo`; qualified `pkg.Foo` → `Foo`; generics stripped.
    fn get_return_type(&self, node: TsNode<'_>, source: &str) -> Option<String> {
        let mut result = child_by_field(node, "result")?;

        // Multi-return `(T, error)` → the first parameter_declaration's type.
        if result.kind() == "parameter_list" {
            let first = (0..result.named_child_count())
                .filter_map(|i| result.named_child(i))
                .find(|c| c.kind() == "parameter_declaration")?;
            result = child_by_field(first, "type").unwrap_or(first);
        }
        // Unwrap a pointer `*Foo` → `Foo`.
        if result.kind() == "pointer_type" {
            if let Some(inner) = (0..result.named_child_count())
                .filter_map(|i| result.named_child(i))
                .find(|c| matches!(c.kind(), "type_identifier" | "qualified_type" | "generic_type"))
            {
                result = inner;
            }
        }
        let raw = node_text(result, source).trim();
        // Strip leading `*`, generic args `<…>` / `[…]`.
        static CLEAN: OnceLock<Regex> = OnceLock::new();
        let clean = CLEAN.get_or_init(|| Regex::new(r"<[^>]*>|\[[^\]]*\]").unwrap());
        let stripped = clean.replace_all(raw.trim_start_matches('*'), "");
        let last = stripped.split('.').next_back()?.trim();
        static IDENT: OnceLock<Regex> = OnceLock::new();
        let ident = IDENT.get_or_init(|| Regex::new(r"^[A-Za-z_]\w*$").unwrap());
        if last.is_empty() || !ident.is_match(last) {
            return None;
        }
        Some(last.to_string())
    }
}
