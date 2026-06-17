//! JavaScript extractor — port of `extraction/languages/javascript.ts`.
//! Reuses the shared TS/JS class-member helpers in `typescript.rs`.

use super::typescript::{
    classify_ts_class_member, import_module_ts, is_const_ts, is_exported_ts, resolve_field_body,
};
use crate::extraction::extractor::{child_by_field, node_text, LanguageExtractor};
use crate::types::{Language, NodeKind};
use tree_sitter::Node as TsNode;

pub struct JavascriptExtractor;

impl LanguageExtractor for JavascriptExtractor {
    fn language(&self) -> Language {
        Language::Javascript
    }
    fn function_types(&self) -> &[&str] {
        &["function_declaration", "arrow_function", "function_expression"]
    }
    fn class_types(&self) -> &[&str] {
        &["class_declaration"]
    }
    fn method_types(&self) -> &[&str] {
        // JS `field_definition` ≙ TS `public_field_definition` (#808).
        &["method_definition", "field_definition"]
    }
    fn import_types(&self) -> &[&str] {
        &["import_statement"]
    }
    fn call_types(&self) -> &[&str] {
        &["call_expression"]
    }
    fn variable_types(&self) -> &[&str] {
        &["lexical_declaration", "variable_declaration"]
    }

    fn classify_method_node(&self, node: TsNode<'_>) -> Option<NodeKind> {
        match classify_ts_class_member(node) {
            NodeKind::Property => Some(NodeKind::Property),
            _ => None,
        }
    }
    fn resolve_body<'t>(&self, node: TsNode<'t>, body_field: &str) -> Option<TsNode<'t>> {
        resolve_field_body(node, "field_definition", body_field)
    }
    /// JS `field_definition` names its key the `property` field (not `name`).
    fn resolve_name(&self, node: TsNode<'_>, source: &str) -> Option<String> {
        if node.kind() == "field_definition" {
            return child_by_field(node, "property").map(|p| node_text(p, source).to_string());
        }
        None
    }
    fn is_exported(&self, node: TsNode<'_>, _source: &str) -> bool {
        is_exported_ts(node)
    }
    fn is_const(&self, node: TsNode<'_>) -> bool {
        is_const_ts(node)
    }
    fn import_module(&self, node: TsNode<'_>, source: &str) -> Option<(String, String)> {
        import_module_ts(node, source)
    }
    fn get_signature(&self, node: TsNode<'_>, source: &str) -> Option<String> {
        child_by_field(node, "parameters").map(|p| node_text(p, source).to_string())
    }
}
