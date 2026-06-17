//! Python extractor — port of `extraction/languages/python.ts`.

use crate::extraction::extractor::{child_by_field, node_text, LanguageExtractor};
use crate::types::Language;
use tree_sitter::Node as TsNode;

pub struct PythonExtractor;

impl LanguageExtractor for PythonExtractor {
    fn language(&self) -> Language {
        Language::Python
    }
    fn function_types(&self) -> &[&str] {
        &["function_definition"]
    }
    fn class_types(&self) -> &[&str] {
        &["class_definition"]
    }
    fn method_types(&self) -> &[&str] {
        // Methods are function_definitions inside a class.
        &["function_definition"]
    }
    fn import_types(&self) -> &[&str] {
        &["import_statement", "import_from_statement"]
    }
    fn call_types(&self) -> &[&str] {
        &["call"]
    }
    fn variable_types(&self) -> &[&str] {
        &["assignment"]
    }

    /// `from module import …` → the module name (per-symbol refs are emitted by
    /// the engine). `import os, sys` returns None → the engine's Python
    /// import_statement path creates one node per module (port of `extractImport`).
    fn import_module(&self, node: TsNode<'_>, source: &str) -> Option<(String, String)> {
        if node.kind() == "import_from_statement" {
            let m = child_by_field(node, "module_name")?;
            let module = node_text(m, source).to_string();
            if module.is_empty() {
                return None;
            }
            return Some((module, node_text(node, source).trim().to_string()));
        }
        None
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
}

#[cfg(test)]
mod tests {
    use crate::extraction::extract_file;
    use crate::types::{Language, NodeKind};

    #[test]
    fn py_extractor_shapes() {
        let src = r#"
import os
from .models import User, Order
from typing import List

MAX = 100

def top_level(x: int) -> str:
    return helper(x)

class Service:
    def handle(self, req):
        self.log()
        return process(req)

async def fetch():
    await get_data()
"#;
        let r = extract_file("svc.py", src, Language::Python);
        let by = |k: NodeKind| {
            r.nodes.iter().filter(|n| n.kind == k).map(|n| n.name.as_str()).collect::<Vec<_>>()
        };
        let fns = by(NodeKind::Function);
        assert!(fns.contains(&"top_level"), "fns: {:?}", fns);
        assert!(fns.contains(&"fetch"), "fns: {:?}", fns);
        assert!(by(NodeKind::Class).contains(&"Service"), "classes: {:?}", by(NodeKind::Class));
        assert!(by(NodeKind::Method).contains(&"handle"), "methods: {:?}", by(NodeKind::Method));
        assert!(by(NodeKind::Variable).contains(&"MAX"), "vars: {:?}", by(NodeKind::Variable));
        assert!(by(NodeKind::Import).contains(&"os"), "imports: {:?}", by(NodeKind::Import));
        assert!(by(NodeKind::Import).contains(&".models"), "imports: {:?}", by(NodeKind::Import));
        // method-call name extraction via the `attribute` field (`self.log()` → log).
        let calls: Vec<&str> = r.unresolved_references.iter().map(|u| u.reference_name.as_str()).collect();
        assert!(calls.contains(&"helper"), "calls: {:?}", calls);
        assert!(calls.contains(&"log"), "calls: {:?}", calls);
        assert!(calls.contains(&"process"), "calls: {:?}", calls);
    }
}
