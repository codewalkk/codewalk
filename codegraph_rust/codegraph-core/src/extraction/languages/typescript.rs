//! TypeScript extractor — port of `extraction/languages/typescript.ts`.
//! JavaScript reuses the shared class-member helpers (`javascript.rs`).

use crate::extraction::extractor::{child_by_field, node_text, LanguageExtractor};
use crate::types::{Language, NodeKind};
use tree_sitter::Node as TsNode;

/// A TS/JS class field is a METHOD only when its value is callable (an arrow, a
/// function expression, or a HOF call wrapping one — `onScroll = throttle(() =>
/// {…})`); everything else is a PROPERTY (port of `classifyTsClassMember`, #808).
pub(super) fn classify_ts_class_member(node: TsNode<'_>) -> NodeKind {
    if node.kind() != "public_field_definition" && node.kind() != "field_definition" {
        return NodeKind::Method; // method_definition, getters/setters
    }
    for i in 0..node.named_child_count() {
        let Some(child) = node.named_child(i) else { continue };
        if matches!(child.kind(), "arrow_function" | "function_expression") {
            return NodeKind::Method;
        }
        if child.kind() == "call_expression" {
            if let Some(args) = child_by_field(child, "arguments") {
                for j in 0..args.named_child_count() {
                    if let Some(arg) = args.named_child(j) {
                        if matches!(arg.kind(), "arrow_function" | "function_expression") {
                            return NodeKind::Method;
                        }
                    }
                }
            }
        }
    }
    NodeKind::Property
}

/// The body of an arrow-function class field (`field = () => {…}` or
/// `field = throttle(() => {…})`). Port of the shared `resolveBody`.
pub(super) fn resolve_field_body<'t>(node: TsNode<'t>, field_kind: &str, body_field: &str) -> Option<TsNode<'t>> {
    if node.kind() != field_kind {
        return None;
    }
    for i in 0..node.named_child_count() {
        let child = node.named_child(i)?;
        if matches!(child.kind(), "arrow_function" | "function_expression") {
            return child_by_field(child, body_field);
        }
        if child.kind() == "call_expression" {
            if let Some(args) = child_by_field(child, "arguments") {
                for j in 0..args.named_child_count() {
                    if let Some(arg) = args.named_child(j) {
                        if matches!(arg.kind(), "arrow_function" | "function_expression") {
                            return child_by_field(arg, body_field);
                        }
                    }
                }
            }
        }
    }
    None
}

/// Walk the parent chain for an `export_statement` ancestor (port of `isExported`).
pub(super) fn is_exported_ts(node: TsNode<'_>) -> bool {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if n.kind() == "export_statement" {
            return true;
        }
        cur = n.parent();
    }
    false
}

/// `const` (vs `let`/`var`) detector for `lexical_declaration` (port of `isConst`).
pub(super) fn is_const_ts(node: TsNode<'_>) -> bool {
    if node.kind() == "lexical_declaration" {
        for i in 0..node.child_count() {
            if node.child(i).map(|c| c.kind()) == Some("const") {
                return true;
            }
        }
    }
    false
}

/// `import_statement` → (module, signature) from the `source` field (port of `extractImport`).
pub(super) fn import_module_ts(node: TsNode<'_>, source: &str) -> Option<(String, String)> {
    let src = child_by_field(node, "source")?;
    let module = node_text(src, source).trim_matches(['"', '\'']).to_string();
    if module.is_empty() {
        return None;
    }
    Some((module, node_text(node, source).trim().to_string()))
}

pub struct TypescriptExtractor;

impl LanguageExtractor for TypescriptExtractor {
    fn language(&self) -> Language {
        Language::Typescript
    }
    fn function_types(&self) -> &[&str] {
        &["function_declaration", "arrow_function", "function_expression"]
    }
    fn class_types(&self) -> &[&str] {
        &["class_declaration", "abstract_class_declaration"]
    }
    fn method_types(&self) -> &[&str] {
        &["method_definition", "public_field_definition"]
    }
    fn interface_types(&self) -> &[&str] {
        &["interface_declaration"]
    }
    fn enum_types(&self) -> &[&str] {
        &["enum_declaration"]
    }
    fn type_alias_types(&self) -> &[&str] {
        &["type_alias_declaration"]
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
        resolve_field_body(node, "public_field_definition", body_field)
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
        let params = child_by_field(node, "parameters")?;
        let mut sig = node_text(params, source).to_string();
        if let Some(rt) = child_by_field(node, "return_type") {
            let rt_text = node_text(rt, source);
            let trimmed = rt_text.trim_start_matches(':').trim_start();
            sig.push_str(": ");
            sig.push_str(trimmed);
        }
        Some(sig)
    }
}

#[cfg(test)]
mod tests {
    use crate::extraction::extract_file;
    use crate::types::{Language, NodeKind};

    #[test]
    fn ts_extractor_shapes() {
        let src = r#"
import { foo } from './foo';
export interface Widget { id: number; }
export class Service {
  count = 0;
  onScroll = () => { this.helper(); };
  helper(): void { bar(); }
}
export const useAuth = () => { login(); };
export function plain(x: number): number { return x; }
export enum Color { Red, Green }
export type Alias = Widget;
const internal = 5;
"#;
        let r = extract_file("svc.ts", src, Language::Typescript);
        let by = |k: NodeKind| r.nodes.iter().filter(|n| n.kind == k).map(|n| n.name.as_str()).collect::<Vec<_>>();
        assert!(by(NodeKind::Interface).contains(&"Widget"), "interface: {:?}", by(NodeKind::Interface));
        assert!(by(NodeKind::Class).contains(&"Service"), "class: {:?}", by(NodeKind::Class));
        assert!(by(NodeKind::Enum).contains(&"Color"));
        assert!(by(NodeKind::TypeAlias).contains(&"Alias"));
        assert!(by(NodeKind::Import).contains(&"./foo"));
        // `count = 0` is a property; `onScroll = () => {}` and `helper()` are methods.
        assert!(by(NodeKind::Property).contains(&"count"), "props: {:?}", by(NodeKind::Property));
        let methods = by(NodeKind::Method);
        assert!(methods.contains(&"onScroll"), "methods: {:?}", methods);
        assert!(methods.contains(&"helper"), "methods: {:?}", methods);
        // arrow-const → function named `useAuth`; `plain` is a function.
        let fns = by(NodeKind::Function);
        assert!(fns.contains(&"useAuth"), "fns: {:?}", fns);
        assert!(fns.contains(&"plain"), "fns: {:?}", fns);
        // a call edge ref to `login` should be staged.
        assert!(r.unresolved_references.iter().any(|u| u.reference_name == "login"), "calls: {:?}", r.unresolved_references.iter().map(|u| &u.reference_name).collect::<Vec<_>>());
    }

    #[test]
    fn ts_heritage_and_type_refs() {
        let src = r#"
interface Base {}
interface Other {}
export class Impl extends Parent implements Base, Other {
  private store: TextModel;
  handle(req: Request): Response { return new Response(); }
}
const config: AppConfig = load();
"#;
        let r = extract_file("h.ts", src, Language::Typescript);
        let refs: Vec<(&str, &str)> = r
            .unresolved_references
            .iter()
            .map(|u| (u.reference_kind.as_str(), u.reference_name.as_str()))
            .collect();
        assert!(refs.contains(&("extends", "Parent")), "refs: {:?}", refs);
        assert!(refs.contains(&("implements", "Base")), "refs: {:?}", refs);
        assert!(refs.contains(&("implements", "Other")), "refs: {:?}", refs);
        // type-annotation references: field type, param/return types, var type.
        assert!(refs.contains(&("references", "TextModel")), "refs: {:?}", refs);
        assert!(refs.contains(&("references", "Request")), "refs: {:?}", refs);
        assert!(refs.contains(&("references", "Response")), "refs: {:?}", refs);
        assert!(refs.contains(&("references", "AppConfig")), "refs: {:?}", refs);
    }

    #[test]
    fn js_object_of_functions() {
        // Exported const object-of-functions → each fn extracted by key (#808 store/handler maps).
        let src = r#"
export const handlers = {
  onSave: () => { persist(); },
  onLoad() { fetchData(); },
};
"#;
        let r = extract_file("h.js", src, Language::Javascript);
        let fns: Vec<&str> = r.nodes.iter().filter(|n| n.kind == NodeKind::Function).map(|n| n.name.as_str()).collect();
        assert!(fns.contains(&"onSave"), "fns: {:?}", fns);
        assert!(fns.contains(&"onLoad"), "fns: {:?}", fns);
    }
}
