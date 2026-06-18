//! Function-as-value capture — port of `extraction/function-ref.ts` (#756), the
//! Go/TS/JS/Python/Rust subset.
//!
//! A function name used as a VALUE — passed as a call argument (`register(handler)`),
//! assigned to a field (`Ops{Cb: target}`, `obj.cb = target`), or listed in a
//! function table — is an indirect call the static call graph misses. We capture
//! these as `function_ref` candidates; the engine gates them (name must be a
//! function/method defined in this file or imported) and resolution matches them
//! against function/method nodes ONLY, emitting `references` edges.

use crate::extraction::extractor::{child_by_field, node_text};
use crate::types::Language;
use tree_sitter::Node as TsNode;

#[derive(Clone, Copy, PartialEq)]
pub enum CaptureMode {
    Args,    // every named child is a candidate (call argument lists)
    Rhs,     // assignment right-hand side
    Value,   // the value of a keyed pair (object/struct/table initializers)
    List,    // every named child (array / positional initializer)
    VarInit, // a declarator's initializer value
}

pub struct CaptureRule {
    pub mode: CaptureMode,
    pub field: Option<&'static str>,
}

/// A captured function-value candidate (name + position).
pub struct FnRefCandidate {
    pub name: String,
    pub line: u32,
    pub col: u32,
}

const STOPLIST: &[&str] = &[
    "this", "self", "super", "null", "nil", "true", "false", "undefined", "new", "NULL", "nullptr",
    "None",
];

/// Container node type → how to pull candidate function values from it
/// (port of the `dispatch` maps in `FN_REF_SPECS`).
pub fn dispatch_rule(lang: Language, node_type: &str) -> Option<CaptureRule> {
    use CaptureMode::*;
    let r = |mode, field| Some(CaptureRule { mode, field });
    match lang {
        Language::Typescript | Language::Tsx | Language::Javascript | Language::Jsx => {
            match node_type {
                "arguments" => r(Args, None),
                "assignment_expression" => r(Rhs, Some("right")),
                "variable_declarator" => r(VarInit, Some("value")),
                "pair" => r(Value, Some("value")),
                "array" => r(List, None),
                _ => None,
            }
        }
        Language::Python => match node_type {
            "argument_list" => r(Args, None),
            "assignment" => r(Rhs, Some("right")),
            "keyword_argument" => r(Value, Some("value")), // Thread(target=worker)
            "pair" => r(Value, Some("value")),
            "list" => r(List, None),
            _ => None,
        },
        Language::Go => match node_type {
            "argument_list" => r(Args, None),
            "assignment_statement" => r(Rhs, Some("right")),
            "short_var_declaration" => r(Rhs, Some("right")),
            "var_spec" => r(VarInit, Some("value")),
            "keyed_element" => r(Value, None), // value = last child
            "literal_value" => r(List, None),
            _ => None,
        },
        Language::Rust => match node_type {
            "arguments" => r(Args, None),
            "assignment_expression" => r(Rhs, Some("right")),
            "field_initializer" => r(Value, Some("value")),
            "array_expression" => r(List, None),
            "static_item" => r(VarInit, Some("value")),
            "let_declaration" => r(VarInit, Some("value")),
            _ => None,
        },
        _ => None,
    }
}

fn is_id_type(t: &str) -> bool {
    t == "identifier"
}

/// Transparent wrapper layers (Go `literal_element`/`expression_list`).
fn is_layer(lang: Language, t: &str) -> bool {
    lang == Language::Go && matches!(t, "literal_element" | "expression_list")
}

/// Whole-node reference forms with bespoke name extraction (`this.m`, `self.m`).
fn is_special(lang: Language, t: &str) -> bool {
    match lang {
        Language::Typescript | Language::Tsx | Language::Javascript | Language::Jsx => {
            t == "member_expression"
        }
        Language::Python => t == "attribute",
        _ => false,
    }
}

/// Extract candidate names from a dispatched container (port of `captureFnRefCandidates`).
pub fn capture_candidates(
    container: TsNode<'_>,
    rule: &CaptureRule,
    lang: Language,
    source: &str,
) -> Vec<FnRefCandidate> {
    let mut value_nodes: Vec<TsNode> = Vec::new();
    match rule.mode {
        CaptureMode::Args | CaptureMode::List => {
            for i in 0..container.named_child_count() {
                if let Some(c) = container.named_child(i) {
                    value_nodes.push(c);
                }
            }
        }
        CaptureMode::Rhs => {
            let rhs = rule
                .field
                .and_then(|f| child_by_field(container, f))
                .or_else(|| container.named_child(container.named_child_count().saturating_sub(1)));
            if let Some(rhs) = rhs {
                // Param-storage skip: `this.cb = cb` / `o.cb = cb` — when the
                // assigned member name EQUALS the RHS identifier, the RHS is a
                // local/param being stored, not a function alias.
                let lhs = child_by_field(container, "left")
                    .or_else(|| child_by_field(container, "lhs"))
                    .or_else(|| {
                        if container.named_child_count() >= 2 {
                            container.named_child(0)
                        } else {
                            None
                        }
                    });
                let lhs_last = lhs
                    .map(|l| node_text(l, source))
                    .and_then(|t| t.rsplit(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$')).next().map(|s| s.to_string()))
                    .unwrap_or_default();
                let rhs_text = node_text(rhs, source).trim().to_string();
                if !lhs_last.is_empty() && lhs_last == rhs_text {
                    // skip
                } else {
                    value_nodes.push(rhs);
                }
            }
        }
        CaptureMode::Value => {
            let value = rule
                .field
                .and_then(|f| child_by_field(container, f))
                .or_else(|| container.named_child(container.named_child_count().saturating_sub(1)));
            if let Some(v) = value {
                value_nodes.push(v);
            }
        }
        CaptureMode::VarInit => {
            // Destructuring patterns extract DATA, not a function alias — skip.
            let name = child_by_field(container, "name").or_else(|| child_by_field(container, "pattern"));
            let is_pattern = name
                .map(|n| matches!(n.kind(), "object_pattern" | "array_pattern" | "tuple_pattern" | "struct_pattern"))
                .unwrap_or(false);
            if !is_pattern {
                if let Some(f) = rule.field {
                    if let Some(v) = child_by_field(container, f) {
                        value_nodes.push(v);
                    }
                }
            }
        }
    }

    let mut out = Vec::new();
    for v in value_nodes {
        for (name, node) in normalize_value(v, lang, source, 0) {
            if name.is_empty() || STOPLIST.contains(&name.as_str()) {
                continue;
            }
            out.push(FnRefCandidate {
                name,
                line: node.start_position().row as u32 + 1,
                col: node.start_position().column as u32,
            });
        }
    }
    out
}

/// Normalize one value expression to zero or more (name, node) function values.
fn normalize_value<'n>(node: TsNode<'n>, lang: Language, source: &str, depth: u32) -> Vec<(String, TsNode<'n>)> {
    if depth > 4 {
        return vec![];
    }
    let t = node.kind();
    if is_id_type(t) {
        return vec![(node_text(node, source).to_string(), node)];
    }
    if is_layer(lang, t) {
        let mut out = Vec::new();
        for i in 0..node.named_child_count() {
            if let Some(c) = node.named_child(i) {
                out.extend(normalize_value(c, lang, source, depth + 1));
            }
        }
        return out;
    }
    if is_special(lang, t) {
        return normalize_special(node, t, source);
    }
    vec![]
}

/// `this.method` (TS member_expression) → "this.method"; `self.method` (Python
/// attribute) → "method". Port of the relevant `normalizeSpecial` cases.
fn normalize_special<'n>(node: TsNode<'n>, t: &str, source: &str) -> Vec<(String, TsNode<'n>)> {
    match t {
        "member_expression" => {
            let (obj, prop) = (child_by_field(node, "object"), child_by_field(node, "property"));
            if let (Some(obj), Some(prop)) = (obj, prop) {
                if obj.kind() == "this" && prop.kind() == "property_identifier" {
                    return vec![(format!("this.{}", node_text(prop, source)), prop)];
                }
            }
            vec![]
        }
        "attribute" => {
            let (obj, attr) = (child_by_field(node, "object"), child_by_field(node, "attribute"));
            if let (Some(obj), Some(attr)) = (obj, attr) {
                if obj.kind() == "identifier" && node_text(obj, source) == "self" {
                    return vec![(node_text(attr, source).to_string(), attr)];
                }
            }
            vec![]
        }
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use crate::extraction::extract_file;
    use crate::types::{Language, ReferenceKind};

    fn fn_refs(r: &crate::types::ExtractionResult) -> Vec<&str> {
        r.unresolved_references
            .iter()
            .filter(|u| u.reference_kind == ReferenceKind::FunctionRef)
            .map(|u| u.reference_name.as_str())
            .collect()
    }

    #[test]
    fn go_callback_captured_and_gated() {
        // `myHandler` (defined here) passed as a value → function_ref; `local`
        // (a param, not a defined function) is gated out.
        let src = r#"
package main
func myHandler() {}
func register(f func()) {}
func setup(local func()) {
    register(myHandler)
    register(local)
}
"#;
        let r = extract_file("m.go", src, Language::Go);
        let refs = fn_refs(&r);
        assert!(refs.contains(&"myHandler"), "go fn_refs: {:?}", refs);
        assert!(!refs.contains(&"local"), "param should be gated out: {:?}", refs);
    }

    #[test]
    fn ts_callback_in_args_and_struct() {
        let src = r#"
function onClick() {}
function register(cb: () => void) {}
register(onClick);
const handlers = { click: onClick };
"#;
        let r = extract_file("m.ts", src, Language::Typescript);
        let refs = fn_refs(&r);
        assert!(refs.iter().filter(|n| **n == "onClick").count() >= 1, "ts fn_refs: {:?}", refs);
    }

    #[test]
    fn python_callback_captured() {
        let src = r#"
def worker():
    pass
def setup():
    run(worker)
    Thread(target=worker)
"#;
        let r = extract_file("m.py", src, Language::Python);
        let refs = fn_refs(&r);
        assert!(refs.contains(&"worker"), "py fn_refs: {:?}", refs);
    }
}
