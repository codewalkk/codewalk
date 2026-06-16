//! `TreeSitterExtractor` — port of `codegraph/src/extraction/tree-sitter.ts`
//! (the Go-relevant subset). Walks the AST, dispatching each node to a
//! concept-specific extractor driven by the `LanguageExtractor` hooks, and
//! produces `nodes` + `contains` edges + `unresolved_references`.
//!
//! Resolution (turning unresolved refs into calls/imports/references edges) is
//! M2; M1 stores the refs so they're ready and reports node/file parity.

use crate::extraction::extractor::{
    child_by_field, extract_name, find_child_by_types, node_text, preceding_docstring,
    LanguageExtractor,
};
use crate::extraction::grammars::grammar_for;
use crate::node_id;
use crate::types::{
    Edge, EdgeKind, ExtractionResult, Language, Node, NodeKind, ReferenceKind, UnresolvedReference,
};
use tree_sitter::{Node as TsNode, Parser};

/// Constructor-invocation node kinds (TS `INSTANTIATION_KINDS`, Go-relevant
/// subset). Go composite literals `Widget{…}` / `pkga.Widget{…}`.
const INSTANTIATION_KINDS: &[&str] = &[
    "new_expression",
    "object_creation_expression",
    "instance_creation_expression",
    "composite_literal",
    "struct_expression",
    "instance_expression",
];

/// A lexical scope on the stack: its node id (for `contains` edges) + kind/name
/// (for qualified-name building).
struct Scope {
    id: String,
    kind: NodeKind,
    name: String,
}

/// Optional fields set when creating a node.
#[derive(Default)]
struct NodeExtra {
    docstring: Option<String>,
    signature: Option<String>,
    is_exported: bool,
    return_type: Option<String>,
    qualified_name: Option<String>,
}

struct Engine<'a> {
    file_path: &'a str,
    source: &'a str,
    language: Language,
    extractor: &'a dyn LanguageExtractor,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    unresolved: Vec<UnresolvedReference>,
    scopes: Vec<Scope>,
    updated_at: i64,
}

/// Parse and extract one source file. Returns an empty-but-ok result (with an
/// error message) when the language has no grammar/extractor yet.
pub fn extract_file(file_path: &str, source: &str, language: Language) -> ExtractionResult {
    let extractor = match crate::extraction::languages::extractor_for(language) {
        Some(e) => e,
        None => {
            return ExtractionResult {
                errors: vec![format!("no extractor for language: {}", language)],
                ..Default::default()
            }
        }
    };
    let grammar = match grammar_for(language) {
        Some(g) => g,
        None => {
            return ExtractionResult {
                errors: vec![format!("no grammar for language: {}", language)],
                ..Default::default()
            }
        }
    };

    let mut parser = Parser::new();
    if parser.set_language(&grammar).is_err() {
        return ExtractionResult {
            errors: vec![format!("failed to set grammar for {}", language)],
            ..Default::default()
        };
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => {
            return ExtractionResult {
                errors: vec!["parser returned no tree".into()],
                ..Default::default()
            }
        }
    };

    let mut engine = Engine {
        file_path,
        source,
        language,
        extractor,
        nodes: Vec::new(),
        edges: Vec::new(),
        unresolved: Vec::new(),
        scopes: Vec::new(),
        updated_at: 0,
    };
    engine.run(tree.root_node());

    ExtractionResult {
        nodes: engine.nodes,
        edges: engine.edges,
        unresolved_references: engine.unresolved,
        errors: Vec::new(),
    }
}

impl<'a> Engine<'a> {
    fn run(&mut self, root: TsNode<'_>) {
        // File node representing the source file.
        let line_count = self.source.split('\n').count() as u32;
        let file_id = format!("file:{}", self.file_path);
        let mut file_node = Node::new(
            file_id.clone(),
            NodeKind::File,
            basename(self.file_path),
            self.file_path.to_string(),
            self.file_path.to_string(),
            self.language,
        );
        file_node.end_line = line_count.max(1);
        file_node.updated_at = self.updated_at;
        self.nodes.push(file_node);
        self.scopes.push(Scope {
            id: file_id,
            kind: NodeKind::File,
            name: basename(self.file_path),
        });

        self.visit_node(root);

        self.scopes.pop();
    }

    /// The main dispatch ladder (TS `visitNode`, Go-relevant branches).
    fn visit_node(&mut self, node: TsNode<'_>) {
        let kind = node.kind();
        let ex = self.extractor;
        let mut skip_children = false;

        if ex.function_types().contains(&kind) {
            if self.is_inside_class_like() && ex.method_types().contains(&kind) {
                self.extract_method(node);
            } else {
                self.extract_function(node);
            }
            skip_children = true;
        } else if ex.class_types().contains(&kind) {
            self.extract_class(node, NodeKind::Class);
            skip_children = true;
        } else if ex.method_types().contains(&kind) {
            self.extract_method(node);
            skip_children = true;
        } else if ex.interface_types().contains(&kind) {
            self.extract_interface(node);
            skip_children = true;
        } else if ex.struct_types().contains(&kind) {
            self.extract_struct(node);
            skip_children = true;
        } else if ex.enum_types().contains(&kind) {
            self.extract_enum(node);
            skip_children = true;
        } else if ex.type_alias_types().contains(&kind) {
            skip_children = self.extract_type_alias(node);
        } else if ex.variable_types().contains(&kind) && !self.is_inside_class_like() {
            self.extract_variable(node);
            skip_children = true;
        } else if ex.import_types().contains(&kind) {
            self.extract_import(node);
        } else if ex.call_types().contains(&kind) {
            self.extract_call(node);
        } else if INSTANTIATION_KINDS.contains(&kind) {
            self.extract_instantiation(node);
        }

        if !skip_children {
            for i in 0..node.named_child_count() {
                if let Some(child) = node.named_child(i) {
                    self.visit_node(child);
                }
            }
        }
    }

    /// Create a node + a `contains` edge from the current scope. Returns its id.
    fn create_node(
        &mut self,
        kind: NodeKind,
        name: &str,
        node: TsNode<'_>,
        extra: NodeExtra,
    ) -> Option<String> {
        if name.is_empty() {
            return None;
        }
        let start_line = node.start_position().row as u32 + 1;
        let id = node_id(self.file_path, kind, name, start_line);
        let qualified_name = extra
            .qualified_name
            .unwrap_or_else(|| self.build_qualified_name(name));

        let mut new_node = Node::new(
            id.clone(),
            kind,
            name.to_string(),
            qualified_name,
            self.file_path.to_string(),
            self.language,
        );
        new_node.start_line = start_line;
        new_node.end_line = node.end_position().row as u32 + 1;
        new_node.start_column = node.start_position().column as u32;
        new_node.end_column = node.end_position().column as u32;
        new_node.docstring = extra.docstring;
        new_node.signature = extra.signature;
        new_node.is_exported = extra.is_exported;
        new_node.return_type = extra.return_type;
        new_node.updated_at = self.updated_at;
        self.nodes.push(new_node);

        if let Some(parent) = self.scopes.last() {
            self.edges
                .push(Edge::new(parent.id.clone(), id.clone(), EdgeKind::Contains));
        }
        Some(id)
    }

    /// Qualified name from the semantic hierarchy only (no file path) — joins
    /// non-file scope names with `::` and appends `name` (TS `buildQualifiedName`).
    fn build_qualified_name(&self, name: &str) -> String {
        let mut parts: Vec<&str> = self
            .scopes
            .iter()
            .filter(|s| s.kind != NodeKind::File)
            .map(|s| s.name.as_str())
            .collect();
        parts.push(name);
        parts.join("::")
    }

    fn is_inside_class_like(&self) -> bool {
        matches!(
            self.scopes.last().map(|s| s.kind),
            Some(
                NodeKind::Class
                    | NodeKind::Struct
                    | NodeKind::Interface
                    | NodeKind::Trait
                    | NodeKind::Enum
                    | NodeKind::Module
            )
        )
    }

    fn push_scope(&mut self, id: String, kind: NodeKind, name: String) {
        self.scopes.push(Scope { id, kind, name });
    }

    fn add_unresolved(&mut self, from: &str, name: &str, kind: ReferenceKind, node: TsNode<'_>) {
        if name.is_empty() {
            return;
        }
        self.unresolved.push(UnresolvedReference {
            from_node_id: from.to_string(),
            reference_name: name.to_string(),
            reference_kind: kind,
            line: node.start_position().row as u32 + 1,
            col: node.start_position().column as u32,
            file_path: Some(self.file_path.to_string()),
            language: Some(self.language),
            candidates: None,
        });
    }

    // --- Concept extractors ---

    fn extract_function(&mut self, node: TsNode<'_>) {
        let ex = self.extractor;
        // A function_item that actually has a receiver (Rust impl) is a method.
        if ex.get_receiver_type(node, self.source).is_some() {
            self.extract_method(node);
            return;
        }
        let name = extract_name(node, self.source, ex);
        if name == "<anonymous>" {
            if let Some(body) = child_by_field(node, ex.body_field()) {
                self.visit_function_body(body);
            }
            return;
        }
        let extra = NodeExtra {
            docstring: preceding_docstring(node, self.source),
            signature: ex.get_signature(node, self.source),
            is_exported: ex.is_exported(node, self.source),
            return_type: ex.get_return_type(node, self.source),
            qualified_name: None,
        };
        let Some(id) = self.create_node(NodeKind::Function, &name, node, extra) else {
            return;
        };
        self.push_scope(id.clone(), NodeKind::Function, name);
        if let Some(body) = child_by_field(node, ex.body_field()) {
            self.visit_function_body(body);
        }
        self.scopes.pop();
    }

    fn extract_method(&mut self, node: TsNode<'_>) {
        let ex = self.extractor;
        let receiver = ex.get_receiver_type(node, self.source);

        if !self.is_inside_class_like() && !ex.methods_are_top_level() && receiver.is_none() {
            self.extract_function(node);
            return;
        }
        let name = extract_name(node, self.source, ex);
        let qualified_name = receiver.as_ref().map(|r| format!("{}::{}", r, name));
        let extra = NodeExtra {
            docstring: preceding_docstring(node, self.source),
            signature: ex.get_signature(node, self.source),
            is_exported: ex.is_exported(node, self.source),
            return_type: ex.get_return_type(node, self.source),
            qualified_name,
        };
        let Some(id) = self.create_node(NodeKind::Method, &name, node, extra) else {
            return;
        };

        // Receiver method with no class-like parent on the stack (Go top-level
        // methods) — add a `contains` edge from the owning struct/type if present.
        if let Some(recv) = &receiver {
            if !self.is_inside_class_like() {
                if let Some(owner) = self.nodes.iter().find(|n| {
                    &n.name == recv
                        && n.file_path == self.file_path
                        && matches!(
                            n.kind,
                            NodeKind::Struct
                                | NodeKind::Class
                                | NodeKind::Enum
                                | NodeKind::Trait
                                | NodeKind::Interface
                        )
                }) {
                    self.edges
                        .push(Edge::new(owner.id.clone(), id.clone(), EdgeKind::Contains));
                }
            }
        }

        self.push_scope(id.clone(), NodeKind::Method, name);
        if let Some(body) = child_by_field(node, ex.body_field()) {
            self.visit_function_body(body);
        }
        self.scopes.pop();
    }

    fn extract_class(&mut self, node: TsNode<'_>, kind: NodeKind) {
        let ex = self.extractor;
        let name = extract_name(node, self.source, ex);
        let extra = NodeExtra {
            docstring: preceding_docstring(node, self.source),
            is_exported: ex.is_exported(node, self.source),
            ..Default::default()
        };
        let Some(id) = self.create_node(kind, &name, node, extra) else {
            return;
        };
        self.extract_inheritance(node, &id);
        self.push_scope(id, kind, name);
        let body = child_by_field(node, ex.body_field()).unwrap_or(node);
        self.visit_named_children(body);
        self.scopes.pop();
    }

    fn extract_interface(&mut self, node: TsNode<'_>) {
        let ex = self.extractor;
        let name = extract_name(node, self.source, ex);
        let kind = ex.interface_kind();
        let extra = NodeExtra {
            docstring: preceding_docstring(node, self.source),
            is_exported: ex.is_exported(node, self.source),
            ..Default::default()
        };
        let Some(id) = self.create_node(kind, &name, node, extra) else {
            return;
        };
        self.extract_inheritance(node, &id);
        self.push_scope(id, kind, name);
        let body = child_by_field(node, ex.body_field()).unwrap_or(node);
        self.visit_named_children(body);
        self.scopes.pop();
    }

    fn extract_struct(&mut self, node: TsNode<'_>) {
        let ex = self.extractor;
        let Some(body) = child_by_field(node, ex.body_field()) else {
            return;
        };
        let name = extract_name(node, self.source, ex);
        let extra = NodeExtra {
            docstring: preceding_docstring(node, self.source),
            is_exported: ex.is_exported(node, self.source),
            ..Default::default()
        };
        let Some(id) = self.create_node(NodeKind::Struct, &name, node, extra) else {
            return;
        };
        self.extract_inheritance(node, &id);
        self.push_scope(id, NodeKind::Struct, name);
        self.visit_named_children(body);
        self.scopes.pop();
    }

    fn extract_enum(&mut self, node: TsNode<'_>) {
        let ex = self.extractor;
        let Some(body) = child_by_field(node, ex.body_field()) else {
            return;
        };
        let name = extract_name(node, self.source, ex);
        let extra = NodeExtra {
            docstring: preceding_docstring(node, self.source),
            is_exported: ex.is_exported(node, self.source),
            ..Default::default()
        };
        let Some(id) = self.create_node(NodeKind::Enum, &name, node, extra) else {
            return;
        };
        self.push_scope(id, NodeKind::Enum, name);
        self.visit_named_children(body);
        self.scopes.pop();
    }

    /// TS `extractTypeAlias` — Go's `type_spec` wraps struct/interface defs.
    /// Returns true when children were handled (skip the default descent).
    fn extract_type_alias(&mut self, node: TsNode<'_>) -> bool {
        let ex = self.extractor;
        let name = extract_name(node, self.source, ex);
        if name == "<anonymous>" {
            return false;
        }
        let docstring = preceding_docstring(node, self.source);
        let is_exported = ex.is_exported(node, self.source);
        let resolved = ex.resolve_type_alias_kind(node, self.source);

        match resolved {
            Some(NodeKind::Struct) => {
                let extra = NodeExtra {
                    docstring,
                    is_exported,
                    ..Default::default()
                };
                let Some(id) = self.create_node(NodeKind::Struct, &name, node, extra) else {
                    return true;
                };
                self.push_scope(id.clone(), NodeKind::Struct, name);
                let type_child = child_by_field(node, "type")
                    .or_else(|| find_child_by_types(node, ex.struct_types()));
                if let Some(tc) = type_child {
                    self.extract_inheritance(tc, &id);
                    let body = child_by_field(tc, ex.body_field()).unwrap_or(tc);
                    self.visit_named_children(body);
                }
                self.scopes.pop();
                true
            }
            Some(NodeKind::Interface) => {
                let kind = ex.interface_kind();
                let extra = NodeExtra {
                    docstring,
                    is_exported,
                    ..Default::default()
                };
                let Some(id) = self.create_node(kind, &name, node, extra) else {
                    return true;
                };
                if let Some(tc) = child_by_field(node, "type") {
                    self.extract_inheritance(tc, &id);
                    if self.language == Language::Go {
                        self.extract_go_interface_methods(tc, &id, &name);
                    }
                }
                true
            }
            _ => {
                let extra = NodeExtra {
                    docstring,
                    is_exported,
                    ..Default::default()
                };
                self.create_node(NodeKind::TypeAlias, &name, node, extra);
                false
            }
        }
    }

    /// Go interface method specs (`method_elem`/`method_spec`) → `method` nodes.
    fn extract_go_interface_methods(
        &mut self,
        interface_type: TsNode<'_>,
        iface_id: &str,
        iface_name: &str,
    ) {
        self.push_scope(iface_id.to_string(), NodeKind::Interface, iface_name.to_string());
        for i in 0..interface_type.named_child_count() {
            let Some(m) = interface_type.named_child(i) else {
                continue;
            };
            if m.kind() != "method_elem" && m.kind() != "method_spec" {
                continue;
            }
            let name_node = child_by_field(m, "name").or_else(|| m.named_child(0));
            let Some(nn) = name_node else { continue };
            let mname = node_text(nn, self.source).to_string();
            if !mname.is_empty() {
                let extra = NodeExtra {
                    signature: self.extractor.get_signature(m, self.source),
                    ..Default::default()
                };
                self.create_node(NodeKind::Method, &mname, m, extra);
            }
        }
        self.scopes.pop();
    }

    /// Go variable/const declarations (var/const specs + `:=`).
    fn extract_variable(&mut self, node: TsNode<'_>) {
        let docstring = preceding_docstring(node, self.source);

        if self.language == Language::Go {
            let kind = if node.kind() == "const_declaration" {
                NodeKind::Constant
            } else {
                NodeKind::Variable
            };
            // Collect var_spec/const_spec, descending one level into a grouped
            // `var ( … )` / `const ( … )` block. tree-sitter-go wraps grouped
            // specs in a `var_spec_list`/`const_spec_list`; a single declaration
            // has the spec as a direct child. (Missing the list wrapper dropped
            // every grouped package-level var, e.g. metrics registries.)
            let mut specs: Vec<TsNode> = Vec::new();
            for i in 0..node.named_child_count() {
                let Some(child) = node.named_child(i) else {
                    continue;
                };
                match child.kind() {
                    "var_spec" | "const_spec" => specs.push(child),
                    "var_spec_list" | "const_spec_list" => {
                        for j in 0..child.named_child_count() {
                            if let Some(s) = child.named_child(j) {
                                if s.kind() == "var_spec" || s.kind() == "const_spec" {
                                    specs.push(s);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            for spec in specs {
                let Some(name_node) = spec.named_child(0) else {
                    continue;
                };
                if name_node.kind() != "identifier" {
                    continue;
                }
                let name = node_text(name_node, self.source).to_string();
                let signature = init_signature(spec, self.source);
                let extra = NodeExtra {
                    docstring: docstring.clone(),
                    signature,
                    ..Default::default()
                };
                let var_id = self.create_node(kind, &name, spec, extra);
                // Walk the initializer for calls / composite literals, attributed
                // to the declared symbol (TS #693).
                if let Some(value) = child_by_field(spec, "value") {
                    if let Some(vid) = var_id {
                        self.push_scope(vid, kind, name);
                        self.visit_function_body(value);
                        self.scopes.pop();
                    } else {
                        self.visit_function_body(value);
                    }
                }
            }

            // short_var_declaration (`x := ...`)
            if node.kind() == "short_var_declaration" {
                if let Some(left) = child_by_field(node, "left") {
                    let ids: Vec<TsNode> = if left.kind() == "expression_list" {
                        (0..left.named_child_count())
                            .filter_map(|i| left.named_child(i))
                            .filter(|c| c.kind() == "identifier")
                            .collect()
                    } else {
                        vec![left]
                    };
                    let signature = child_by_field(node, "right").map(|r| {
                        let v = node_text(r, self.source);
                        format!("= {}", truncate(v, 100))
                    });
                    for id in ids {
                        let name = node_text(id, self.source).to_string();
                        let extra = NodeExtra {
                            docstring: docstring.clone(),
                            signature: signature.clone(),
                            ..Default::default()
                        };
                        self.create_node(NodeKind::Variable, &name, node, extra);
                    }
                }
            }
        }
    }

    /// Go imports — one `import` node + an `imports` unresolved ref per spec.
    fn extract_import(&mut self, node: TsNode<'_>) {
        if self.language != Language::Go {
            return;
        }
        let parent_id = self.scopes.last().map(|s| s.id.clone());
        let specs: Vec<TsNode> = if let Some(list) =
            (0..node.named_child_count())
                .filter_map(|i| node.named_child(i))
                .find(|c| c.kind() == "import_spec_list")
        {
            (0..list.named_child_count())
                .filter_map(|i| list.named_child(i))
                .filter(|c| c.kind() == "import_spec")
                .collect()
        } else {
            (0..node.named_child_count())
                .filter_map(|i| node.named_child(i))
                .filter(|c| c.kind() == "import_spec")
                .collect()
        };
        for spec in specs {
            let Some(lit) = (0..spec.named_child_count())
                .filter_map(|i| spec.named_child(i))
                .find(|c| c.kind() == "interpreted_string_literal")
            else {
                continue;
            };
            let import_path = node_text(lit, self.source).trim_matches(['"', '\'']).to_string();
            if import_path.is_empty() {
                continue;
            }
            let extra = NodeExtra {
                signature: Some(node_text(spec, self.source).trim().to_string()),
                ..Default::default()
            };
            self.create_node(NodeKind::Import, &import_path, spec, extra);
            if let Some(pid) = &parent_id {
                self.add_unresolved(
                    &pid.clone(),
                    &import_path,
                    ReferenceKind::Edge(EdgeKind::Imports),
                    spec,
                );
            }
        }
    }

    /// Function/method call → a `calls` unresolved ref (TS `extractCall`, Go subset).
    fn extract_call(&mut self, node: TsNode<'_>) {
        let Some(caller) = self.scopes.last().map(|s| s.id.clone()) else {
            return;
        };
        let func = child_by_field(node, "function").or_else(|| node.named_child(0));
        let Some(func) = func else { return };

        let mut callee = String::new();
        match func.kind() {
            "selector_expression" | "member_expression" | "field_expression"
            | "navigation_expression" | "attribute" => {
                let property =
                    child_by_field(func, "field").or_else(|| child_by_field(func, "property"));
                if let Some(prop) = property {
                    let method = node_text(prop, self.source);
                    let receiver = child_by_field(func, "operand")
                        .or_else(|| child_by_field(func, "object"))
                        .or_else(|| func.named_child(0));
                    const SKIP: &[&str] = &["self", "this", "cls", "super"];
                    if let Some(recv) = receiver {
                        if matches!(recv.kind(), "identifier" | "field_identifier") {
                            let rname = node_text(recv, self.source);
                            callee = if SKIP.contains(&rname) {
                                method.to_string()
                            } else {
                                format!("{}.{}", rname, method)
                            };
                        } else if self.language == Language::Go
                            && recv.kind() == "call_expression"
                        {
                            // `New().Method()` — re-encode only a bare factory chain
                            // (inner callee is an identifier) so resolution can infer
                            // the receiver type from the inner call's return (#645).
                            let inner_fn = child_by_field(recv, "function");
                            if inner_fn.map(|f| f.kind()) == Some("identifier") {
                                let inner = node_text(inner_fn.unwrap(), self.source)
                                    .replace(char::is_whitespace, "");
                                callee = format!("{}().{}", inner, method);
                            } else {
                                callee = method.to_string();
                            }
                        } else {
                            callee = method.to_string();
                        }
                    } else {
                        callee = method.to_string();
                    }
                }
            }
            "scoped_identifier" | "scoped_call_expression" => {
                callee = node_text(func, self.source).to_string();
            }
            _ => {
                callee = node_text(func, self.source).to_string();
            }
        }

        // Go parenthesized type conversion `(*T)(x)` / `(T)(x)` → bare `T`.
        if let Some(c) = conv_inner(&callee) {
            callee = c;
        }
        self.add_unresolved(&caller, &callee, ReferenceKind::Edge(EdgeKind::Calls), node);
    }

    /// Constructor/composite-literal → an `instantiates` unresolved ref.
    fn extract_instantiation(&mut self, node: TsNode<'_>) {
        let Some(from) = self.scopes.last().map(|s| s.id.clone()) else {
            return;
        };
        let ctor = child_by_field(node, "constructor")
            .or_else(|| child_by_field(node, "type"))
            .or_else(|| child_by_field(node, "name"))
            .or_else(|| node.named_child(0));
        let Some(ctor) = ctor else { return };

        if node.kind() == "composite_literal" {
            // Only a directly-named struct type is a meaningful target — skip
            // slice/map/array literals. Keep the package qualifier `pkga.Widget`.
            if ctor.kind() != "type_identifier" && ctor.kind() != "qualified_type" {
                return;
            }
            let mut go_type = node_text(ctor, self.source).trim().to_string();
            if let Some(br) = go_type.find('[') {
                if br > 0 {
                    go_type = go_type[..br].trim().to_string();
                }
            }
            self.add_unresolved(
                &from,
                &go_type,
                ReferenceKind::Edge(EdgeKind::Instantiates),
                node,
            );
            return;
        }

        let mut class_name = node_text(ctor, self.source).to_string();
        if let Some(lt) = class_name.find('<') {
            if lt > 0 {
                class_name = class_name[..lt].to_string();
            }
        }
        let last_dot = class_name
            .rfind("::")
            .map(|i| i + 2)
            .or_else(|| class_name.rfind('.').map(|i| i + 1));
        if let Some(i) = last_dot {
            class_name = class_name[i..].to_string();
        }
        let class_name = class_name.trim().to_string();
        self.add_unresolved(
            &from,
            &class_name,
            ReferenceKind::Edge(EdgeKind::Instantiates),
            node,
        );
    }

    /// Inheritance/embedding → `extends`/`implements` unresolved refs (Go subset:
    /// interface embedding `constraint_elem`, struct embedding `field_declaration`
    /// without a field name).
    fn extract_inheritance(&mut self, node: TsNode<'_>, class_id: &str) {
        for i in 0..node.named_child_count() {
            let Some(child) = node.named_child(i) else {
                continue;
            };
            match child.kind() {
                "constraint_elem" => {
                    if let Some(tid) = (0..child.named_child_count())
                        .filter_map(|j| child.named_child(j))
                        .find(|c| c.kind() == "type_identifier")
                    {
                        let name = node_text(tid, self.source).to_string();
                        self.add_unresolved(
                            class_id,
                            &name,
                            ReferenceKind::Edge(EdgeKind::Extends),
                            tid,
                        );
                    }
                }
                "field_declaration" => {
                    let has_field_ident = (0..child.named_child_count())
                        .filter_map(|j| child.named_child(j))
                        .any(|c| c.kind() == "field_identifier");
                    if !has_field_ident {
                        if let Some(tid) = (0..child.named_child_count())
                            .filter_map(|j| child.named_child(j))
                            .find(|c| c.kind() == "type_identifier")
                        {
                            let name = node_text(tid, self.source).to_string();
                            self.add_unresolved(
                                class_id,
                                &name,
                                ReferenceKind::Edge(EdgeKind::Extends),
                                tid,
                            );
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Walk a function/method body for calls, instantiations, nested named
    /// functions, and structural decls (TS `visitFunctionBody`).
    fn visit_function_body(&mut self, body: TsNode<'_>) {
        let kind = body.kind();
        let ex = self.extractor;

        if ex.call_types().contains(&kind) {
            self.extract_call(body);
        } else if INSTANTIATION_KINDS.contains(&kind) {
            self.extract_instantiation(body);
        }

        // Nested NAMED functions become their own nodes (bounds the graph: no
        // anonymous explosion). extract_function walks its own body, so return.
        if ex.function_types().contains(&kind) {
            let name = extract_name(body, self.source, ex);
            if name != "<anonymous>" && !name.is_empty() {
                self.extract_function(body);
                return;
            }
        }
        // Structural decls inside a body (type decls in a func).
        if ex.type_alias_types().contains(&kind) {
            self.extract_type_alias(body);
            return;
        }
        if ex.struct_types().contains(&kind) {
            self.extract_struct(body);
            return;
        }
        if ex.interface_types().contains(&kind) {
            self.extract_interface(body);
            return;
        }

        for i in 0..body.named_child_count() {
            if let Some(child) = body.named_child(i) {
                self.visit_function_body(child);
            }
        }
    }

    fn visit_named_children(&mut self, node: TsNode<'_>) {
        for i in 0..node.named_child_count() {
            if let Some(child) = node.named_child(i) {
                self.visit_node(child);
            }
        }
    }
}

// --- small helpers ---

fn basename(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
    } else {
        s.to_string()
    }
}

/// `= <first 100 chars of initializer>` signature for a Go var/const spec.
fn init_signature(spec: TsNode<'_>, source: &str) -> Option<String> {
    let n = spec.named_child_count();
    if n <= 1 {
        return None;
    }
    let value = spec.named_child(n - 1)?;
    let v = node_text(value, source);
    Some(format!("= {}", truncate(v, 100)))
}

/// `(*T)` / `( T )` → `T` for Go parenthesized type conversions.
fn conv_inner(callee: &str) -> Option<String> {
    let t = callee.trim();
    let inner = t.strip_prefix('(')?.strip_suffix(')')?.trim();
    let inner = inner.strip_prefix('*').unwrap_or(inner).trim();
    if !inner.is_empty()
        && inner
            .chars()
            .next()
            .map(|c| c.is_ascii_alphabetic() || c == '_')
            .unwrap_or(false)
        && inner
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
    {
        Some(inner.to_string())
    } else {
        None
    }
}
