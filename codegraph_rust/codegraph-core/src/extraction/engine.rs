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

/// Built-in/primitive type names that shouldn't create `references`
/// (port of TS `BUILTIN_TYPES`, the Go-relevant + shared entries).
const BUILTIN_TYPES: &[&str] = &[
    "string", "number", "boolean", "void", "null", "undefined", "never", "any", "object", "symbol",
    "bigint", "true", "false", "str", "bool", "i8", "i16", "i32", "i64", "i128", "isize", "u8",
    "u16", "u32", "u64", "u128", "usize", "f32", "f64", "char", "int", "long", "short", "byte",
    "float", "double", "int8", "int16", "int32", "int64", "uint", "uint8", "uint16", "uint32",
    "uint64", "uintptr", "float32", "float64", "complex64", "complex128", "rune", "error",
];

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
    /// Function-as-value candidates (#756): (candidate, from-node-id). Gated and
    /// flushed to `function_ref` refs after the walk, when the file is complete.
    fn_refs: Vec<(crate::extraction::fn_ref::FnRefCandidate, String)>,
    /// Generated file (path-based) — function-ref capture is skipped entirely
    /// (its candidates are noise), computed once instead of per-flush.
    is_generated: bool,
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
        fn_refs: Vec::new(),
        is_generated: crate::search::query_utils::is_generated_file(file_path),
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

        // Gate + flush function-as-value candidates while the file's nodes and
        // import refs are complete and the file scope is still on the stack.
        self.flush_fn_refs();

        self.scopes.pop();
    }

    /// Capture function-as-value candidates from a dispatched container node,
    /// attributed to the current scope.
    fn maybe_capture_fn_refs(&mut self, node: TsNode<'_>) {
        if self.is_generated {
            return; // candidates would be discarded at flush anyway
        }
        let Some(rule) = crate::extraction::fn_ref::dispatch_rule(self.language, node.kind()) else {
            return;
        };
        let Some(from) = self.scopes.last().map(|s| s.id.clone()) else {
            return;
        };
        for cand in crate::extraction::fn_ref::capture_candidates(node, &rule, self.language, self.source) {
            self.fn_refs.push((cand, from.clone()));
        }
    }

    /// Scan a value subtree the main walkers skip (object-literal initializers)
    /// for function-as-value candidates, halting at nested function defs (their
    /// bodies are captured by `extract_function`). Port of `scanFnRefSubtree`.
    fn scan_fn_ref_subtree(&mut self, node: TsNode<'_>, depth: u32) {
        if self.is_generated || depth > 12 {
            return;
        }
        if depth > 0 {
            let k = node.kind();
            if self.extractor.function_types().contains(&k)
                || matches!(k, "arrow_function" | "function_expression")
            {
                return;
            }
        }
        self.maybe_capture_fn_refs(node);
        for i in 0..node.named_child_count() {
            if let Some(c) = node.named_child(i) {
                self.scan_fn_ref_subtree(c, depth + 1);
            }
        }
    }

    /// Gate captured candidates and push survivors as `function_ref` refs: a name
    /// must be a function/method DEFINED IN THIS FILE or one this file imports
    /// (everything else — locals, params, fields — is dropped). Port of
    /// `flushFnRefCandidates` (the common gate; no C/PHP/C++ special cases).
    fn flush_fn_refs(&mut self) {
        if self.fn_refs.is_empty() {
            return;
        }
        let candidates = std::mem::take(&mut self.fn_refs);
        let defined_here: std::collections::HashSet<&str> = self
            .nodes
            .iter()
            .filter(|n| matches!(n.kind, NodeKind::Function | NodeKind::Method))
            .map(|n| n.name.as_str())
            .collect();
        let imported: std::collections::HashSet<&str> = self
            .unresolved
            .iter()
            .filter(|r| r.reference_kind == ReferenceKind::Edge(EdgeKind::Imports))
            .map(|r| r.reference_name.as_str())
            .collect();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut refs: Vec<UnresolvedReference> = Vec::new();
        for (c, from) in &candidates {
            // Gate: a name must be a function/method defined in this file or one
            // this file imports. (`this.<member>` forms can't resolve by bare-name
            // matching — no class-scoped fn-ref resolver is ported — so they fall
            // out here rather than becoming orphan refs.)
            if !defined_here.contains(c.name.as_str()) && !imported.contains(c.name.as_str()) {
                continue;
            }
            let key = format!("{}|{}", from, c.name);
            if !seen.insert(key) {
                continue;
            }
            refs.push(UnresolvedReference {
                from_node_id: from.clone(),
                reference_name: c.name.clone(),
                reference_kind: ReferenceKind::FunctionRef,
                line: c.line,
                col: c.col,
                file_path: Some(self.file_path.to_string()),
                language: Some(self.language),
                candidates: None,
            });
        }
        self.unresolved.extend(refs);
    }

    /// The main dispatch ladder (TS `visitNode`, Go-relevant branches).
    fn visit_node(&mut self, node: TsNode<'_>) {
        let kind = node.kind();
        let ex = self.extractor;
        let mut skip_children = false;

        // Function-as-value candidates in this container (args/assignment/initializer).
        self.maybe_capture_fn_refs(node);

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
            // TS/JS class fields: a function-valued field is a method, a plain
            // field is a property (classify_method_node demotes the latter).
            if ex.classify_method_node(node) == Some(NodeKind::Property) {
                self.extract_property(node);
            } else {
                self.extract_method(node);
            }
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
        } else if ex.enum_member_types().contains(&kind) && self.is_inside_enum() {
            self.extract_enum_member(node);
        } else if matches!(kind, "property_signature" | "method_signature")
            && self.is_inside_class_like()
        {
            // TS interface members carry type annotations (`foo: T`, `m(a: A): R`)
            // the interface walker would otherwise drop — emit them as `references`
            // from the interface. Children still traversed (nested signatures).
            if let Some(parent) = self.scopes.last().map(|s| s.id.clone()) {
                self.extract_type_annotations(node, &parent);
            }
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

    fn is_inside_enum(&self) -> bool {
        self.scopes.last().map(|s| s.kind) == Some(NodeKind::Enum)
    }

    /// An enum variant → an `enum_member` node (Rust `enum_variant`).
    fn extract_enum_member(&mut self, node: TsNode<'_>) {
        let name = extract_name(node, self.source, self.extractor);
        if name == "<anonymous>" || name.is_empty() {
            return;
        }
        let extra = NodeExtra {
            docstring: preceding_docstring(node, self.source),
            ..Default::default()
        };
        self.create_node(NodeKind::EnumMember, &name, node, extra);
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
        self.extract_function_named(node, None);
    }

    /// `name_override` is supplied for object-literal function members the caller
    /// resolved itself (`{ fetchUser: () => {} }` → a function named `fetchUser`).
    fn extract_function_named(&mut self, node: TsNode<'_>, name_override: Option<String>) {
        let ex = self.extractor;
        // A function_item that actually has a receiver (Rust impl) is a method.
        if name_override.is_none() && ex.get_receiver_type(node, self.source).is_some() {
            self.extract_method(node);
            return;
        }
        let mut name = name_override
            .clone()
            .unwrap_or_else(|| extract_name(node, self.source, ex));
        // Arrow/function expressions assigned to a variable take the declarator's
        // name (`export const useAuth = () => {…}` → `useAuth`).
        if name_override.is_none()
            && name == "<anonymous>"
            && matches!(node.kind(), "arrow_function" | "function_expression")
        {
            if let Some(parent) = node.parent() {
                if parent.kind() == "variable_declarator" {
                    if let Some(vn) = child_by_field(parent, "name") {
                        name = node_text(vn, self.source).to_string();
                    }
                }
            }
        }
        if name == "<anonymous>" || name.is_empty() {
            // Don't emit a node for the anonymous wrapper, but still walk its body
            // (module wrappers / IIFEs hold named inner functions + calls).
            if let Some(body) = self.get_body(node) {
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
        self.extract_type_annotations(node, &id);
        self.push_scope(id.clone(), NodeKind::Function, name);
        if let Some(body) = self.get_body(node) {
            self.visit_function_body(body);
        }
        self.scopes.pop();
    }

    /// The function/method body — `resolve_body` hook (arrow-field bodies) else
    /// the plain `body` field.
    fn get_body<'n>(&self, node: TsNode<'n>) -> Option<TsNode<'n>> {
        self.extractor
            .resolve_body(node, self.extractor.body_field())
            .or_else(|| child_by_field(node, self.extractor.body_field()))
    }

    /// A plain (non-callable) class field → a `property` node (TS/JS
    /// `public_field_definition`/`field_definition` classified as property).
    fn extract_property(&mut self, node: TsNode<'_>) {
        let ex = self.extractor;
        let name = extract_name(node, self.source, ex);
        if name == "<anonymous>" || name.is_empty() {
            return;
        }
        let extra = NodeExtra {
            docstring: preceding_docstring(node, self.source),
            is_exported: ex.is_exported(node, self.source),
            ..Default::default()
        };
        if let Some(id) = self.create_node(NodeKind::Property, &name, node, extra) {
            // Field type annotation (`private store: ITextModel`) → references.
            if let Some(ty) = child_by_field(node, "type") {
                self.extract_type_refs_from_subtree(ty, &id);
            }
        }
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

        self.extract_type_annotations(node, &id);
        self.push_scope(id.clone(), NodeKind::Method, name);
        if let Some(body) = self.get_body(node) {
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
                if let Some(id) = self.create_node(NodeKind::TypeAlias, &name, node, extra) {
                    // `type X = Foo | Bar<Baz>` → references to the RHS types.
                    if let Some(value) = child_by_field(node, "value") {
                        self.extract_type_refs_from_subtree(value, &id);
                    }
                }
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

    /// TS/JS variable declarations: `variable_declarator` children of a
    /// `lexical_declaration`/`variable_declaration`. An arrow/function value
    /// becomes a named function (`const foo = () => {}`); else a variable/constant.
    fn extract_variable_ts(&mut self, node: TsNode<'_>) {
        let ex = self.extractor;
        let kind = if ex.is_const(node) { NodeKind::Constant } else { NodeKind::Variable };
        let docstring = preceding_docstring(node, self.source);
        let is_exported = ex.is_exported(node, self.source);
        for i in 0..node.named_child_count() {
            let Some(child) = node.named_child(i) else { continue };
            if child.kind() != "variable_declarator" {
                continue;
            }
            let Some(name_node) = child_by_field(child, "name") else { continue };
            // Skip destructured patterns (`const { x, y } = …`) — ugly multi-line names.
            if matches!(name_node.kind(), "object_pattern" | "array_pattern") {
                continue;
            }
            let name = node_text(name_node, self.source).to_string();
            let value = child_by_field(child, "value");
            if let Some(v) = value {
                if matches!(v.kind(), "arrow_function" | "function_expression") {
                    self.extract_function(v);
                    continue;
                }
            }
            let signature = value.map(|v| format!("= {}", truncate(node_text(v, self.source), 100)));
            let extra = NodeExtra {
                docstring: docstring.clone(),
                signature,
                is_exported,
                ..Default::default()
            };
            let vid = self.create_node(kind, &name, child, extra);
            // Type annotation references (`const x: ITextModel = …`).
            if let Some(id) = &vid {
                if let Some(ty) = child_by_field(child, "type") {
                    self.extract_type_refs_from_subtree(ty, id);
                }
            }
            // Exported const object-of-functions: extract each function-valued
            // property as a function named by its key (handler maps, store
            // factories, the LanguageExtractor config objects). Two shapes: the
            // object as the direct value, or the object RETURNED by an initializer
            // call (Zustand/Redux/Pinia middleware-wrapped factories).
            let object_of_fns = value.and_then(|v| match v.kind() {
                "object" | "object_expression" => Some(v),
                "call_expression" => self.find_initializer_returned_object(v, 0),
                _ => None,
            });
            let extract_object_methods = is_exported && object_of_fns.is_some();

            // Walk the initializer for calls/instantiations — EXCEPT object
            // literals (handled below) and the store-factory call whose returned
            // object we extract method-by-method (walking it would re-visit the
            // method arrows and mis-attribute their calls to the file scope).
            if let Some(v) = value {
                let is_obj = matches!(v.kind(), "object" | "object_expression");
                let is_factory = extract_object_methods && v.kind() == "call_expression";
                if !is_obj && !is_factory {
                    if let Some(id) = vid {
                        self.push_scope(id, kind, name.clone());
                        self.visit_function_body(v);
                        self.scopes.pop();
                    } else {
                        self.visit_function_body(v);
                    }
                }
            }
            if let Some(obj) = object_of_fns {
                if extract_object_methods {
                    self.extract_object_literal_functions(obj);
                }
            }
            // Object-literal handler tables (`const h = { click: onClick }`): the
            // identifier function-values are fn-ref candidates the main walker
            // skipped (object isn't visit_function_body-walked, and visit_node set
            // skip_children) — scan the object subtree for them.
            if let Some(v) = value {
                if matches!(v.kind(), "object" | "object_expression") {
                    self.scan_fn_ref_subtree(v, 0);
                }
            }
        }
    }

    /// Function-valued properties of an object literal → function nodes named by
    /// their key (port of `extractObjectLiteralFunctions`). Handles `key: () => {}`
    /// / `key: function(){}` pairs and method shorthand `key() {}`.
    fn extract_object_literal_functions(&mut self, obj: TsNode<'_>) {
        for i in 0..obj.named_child_count() {
            let Some(member) = obj.named_child(i) else { continue };
            match member.kind() {
                "pair" => {
                    let (Some(key), Some(value)) =
                        (child_by_field(member, "key"), child_by_field(member, "value"))
                    else {
                        continue;
                    };
                    if matches!(value.kind(), "arrow_function" | "function_expression") {
                        let name = object_key_name(key, self.source);
                        self.extract_function_named(value, Some(name));
                    }
                }
                "method_definition" => {
                    if let Some(key) = child_by_field(member, "name") {
                        let name = object_key_name(key, self.source);
                        self.extract_function_named(member, Some(name));
                    }
                }
                _ => {}
            }
        }
    }

    /// The object literal returned by a `call_expression` initializer
    /// (`create((set, get) => ({...}))`), descending nested call args for
    /// middleware wrappers (port of `findInitializerReturnedObject`).
    fn find_initializer_returned_object<'n>(&self, call: TsNode<'n>, depth: u32) -> Option<TsNode<'n>> {
        if depth > 4 {
            return None;
        }
        let args = child_by_field(call, "arguments")?;
        for i in 0..args.named_child_count() {
            let Some(arg) = args.named_child(i) else { continue };
            match arg.kind() {
                "arrow_function" | "function_expression" => {
                    if let Some(obj) = function_returned_object(arg) {
                        return Some(obj);
                    }
                }
                "call_expression" => {
                    if let Some(obj) = self.find_initializer_returned_object(arg, depth + 1) {
                        return Some(obj);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Python module-level assignment → a variable node (`MAX = 100`). Skips
    /// `self.x = …` (attribute target), tuple/subscript targets.
    fn extract_variable_py(&mut self, node: TsNode<'_>) {
        let left = child_by_field(node, "left").or_else(|| node.named_child(0));
        let Some(left) = left else { return };
        if left.kind() != "identifier" {
            return;
        }
        let name = node_text(left, self.source).to_string();
        let right = child_by_field(node, "right").or_else(|| node.named_child(node.named_child_count().saturating_sub(1)));
        let signature = right
            .filter(|r| r.id() != left.id())
            .map(|r| format!("= {}", truncate(node_text(r, self.source), 100)));
        let extra = NodeExtra {
            docstring: preceding_docstring(node, self.source),
            signature,
            ..Default::default()
        };
        self.create_node(NodeKind::Variable, &name, node, extra);
    }

    /// Rust `const`/`static` (→ constant) and top-level `let` (→ variable).
    fn extract_variable_rust(&mut self, node: TsNode<'_>) {
        let name_node = child_by_field(node, "name").or_else(|| child_by_field(node, "pattern"));
        let Some(nn) = name_node else { return };
        if nn.kind() != "identifier" {
            return; // skip destructuring let-patterns
        }
        let name = node_text(nn, self.source).to_string();
        // The TS rust extractor defines no `isConst`, so `const`/`static`/`let`
        // all extract as `variable` — match it for parity (PORT, don't invent).
        let kind = NodeKind::Variable;
        let signature = child_by_field(node, "value")
            .map(|v| format!("= {}", truncate(node_text(v, self.source), 100)));
        let extra = NodeExtra {
            docstring: preceding_docstring(node, self.source),
            signature,
            is_exported: self.extractor.is_exported(node, self.source),
            ..Default::default()
        };
        self.create_node(kind, &name, node, extra);
    }

    /// Go variable/const declarations (var/const specs + `:=`).
    fn extract_variable(&mut self, node: TsNode<'_>) {
        if matches!(
            self.language,
            Language::Typescript | Language::Tsx | Language::Javascript | Language::Jsx
        ) {
            self.extract_variable_ts(node);
            return;
        }
        if self.language == Language::Python {
            self.extract_variable_py(node);
            return;
        }
        if self.language == Language::Rust {
            self.extract_variable_rust(node);
            return;
        }
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

    /// Imports — one `import` node + an `imports` unresolved ref. Generic path via
    /// the `import_module` hook (TS/JS `import_statement`); Go uses its multi-spec path.
    fn extract_import(&mut self, node: TsNode<'_>) {
        // Generic single-module import (TS/JS).
        if let Some((module, signature)) = self.extractor.import_module(node, self.source) {
            if module.is_empty() {
                return;
            }
            let parent_id = self.scopes.last().map(|s| s.id.clone());
            let extra = NodeExtra {
                signature: Some(signature),
                ..Default::default()
            };
            let import_id = self.create_node(NodeKind::Import, &module, node, extra);
            // A direct file→import `imports` edge (the file's module-level
            // dependency), on top of the `contains` create_node already added —
            // matches the TS graph and is the only import signal for externals.
            if let (Some(iid), Some(pid)) = (&import_id, &parent_id) {
                let mut e = Edge::new(pid.clone(), iid.clone(), EdgeKind::Imports);
                e.provenance = Some(crate::types::Provenance::TreeSitter);
                self.edges.push(e);
            }
            // Per-imported-symbol refs: each named/default import becomes an
            // `imports` ref whose target is resolved by scoping `name` to the
            // module's file (the module specifier is carried in `candidates`).
            // External modules (`fs`, `commander`) carry a non-relative specifier
            // that the import resolver declines, so they create no edge.
            if let Some(pid) = parent_id {
                let line = node.start_position().row as u32 + 1;
                let col = node.start_position().column as u32;
                for name in self.imported_symbol_names(node) {
                    self.unresolved.push(UnresolvedReference {
                        from_node_id: pid.clone(),
                        reference_name: name,
                        reference_kind: ReferenceKind::Edge(EdgeKind::Imports),
                        line,
                        col,
                        file_path: Some(self.file_path.to_string()),
                        language: Some(self.language),
                        candidates: Some(vec![module.clone()]),
                    });
                }
            }
            return;
        }
        // Python `import os, sys` / `import os.path as p` — one node per module
        // (the hook handled only `from … import`, port of the import_statement path).
        if self.language == Language::Python && node.kind() == "import_statement" {
            let parent_id = self.scopes.last().map(|s| s.id.clone());
            let sig = node_text(node, self.source).trim().to_string();
            for i in 0..node.named_child_count() {
                let Some(child) = node.named_child(i) else { continue };
                let dotted = match child.kind() {
                    "dotted_name" => Some(child),
                    "aliased_import" => find_child_by_types(child, &["dotted_name"]),
                    _ => None,
                };
                if let Some(d) = dotted {
                    let module = node_text(d, self.source).to_string();
                    let extra = NodeExtra {
                        signature: Some(sig.clone()),
                        ..Default::default()
                    };
                    self.create_node(NodeKind::Import, &module, node, extra);
                    if let Some(pid) = &parent_id {
                        self.add_unresolved(pid, &module, ReferenceKind::Edge(EdgeKind::Imports), d);
                    }
                }
            }
            return;
        }
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

    /// The exported names brought in by a TS/JS `import_statement` — named
    /// imports (`{ X, Y as Z }` → X, Y) and default imports (`import D from …`).
    /// Namespace imports (`* as ns`) are skipped (they bind no single symbol).
    fn imported_symbol_names(&self, node: TsNode<'_>) -> Vec<String> {
        let mut out = Vec::new();
        // Python `from module import a, b as c` — the imported names (the original,
        // not the alias). `module_name` is skipped; `import *` binds nothing.
        if node.kind() == "import_from_statement" {
            let module_id = child_by_field(node, "module_name").map(|m| m.id());
            for i in 0..node.named_child_count() {
                let Some(c) = node.named_child(i) else { continue };
                if Some(c.id()) == module_id {
                    continue;
                }
                let name_node = match c.kind() {
                    "dotted_name" | "identifier" => Some(c),
                    "aliased_import" => find_child_by_types(c, &["dotted_name", "identifier"]),
                    _ => None, // wildcard_import
                };
                if let Some(nn) = name_node {
                    // Last segment (`a.b` → `b`) is the bound name.
                    let t = node_text(nn, self.source).trim();
                    let last = t.rsplit('.').next().unwrap_or(t);
                    if !last.is_empty() {
                        out.push(last.to_string());
                    }
                }
            }
            return out;
        }
        let Some(clause) = find_child_by_types(node, &["import_clause"]) else {
            return out;
        };
        for i in 0..clause.named_child_count() {
            let Some(c) = clause.named_child(i) else { continue };
            match c.kind() {
                // Default import binding (`import D from './d'`).
                "identifier" => {
                    let t = node_text(c, self.source).trim();
                    if !t.is_empty() {
                        out.push(t.to_string());
                    }
                }
                "named_imports" => {
                    for j in 0..c.named_child_count() {
                        let Some(spec) = c.named_child(j) else { continue };
                        if spec.kind() != "import_specifier" {
                            continue;
                        }
                        // The `name` field is the EXPORTED name (what exists in the
                        // target file); an `alias` only renames it locally.
                        let nm = child_by_field(spec, "name").or_else(|| spec.named_child(0));
                        if let Some(nm) = nm {
                            let t = node_text(nm, self.source).trim();
                            if !t.is_empty() {
                                out.push(t.to_string());
                            }
                        }
                    }
                }
                _ => {} // namespace_import etc.
            }
        }
        out
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
                let property = child_by_field(func, "field")
                    .or_else(|| child_by_field(func, "property"))
                    .or_else(|| child_by_field(func, "attribute")); // Python `obj.method`
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

    /// Inheritance/embedding → `extends` unresolved refs (faithful port of the TS
    /// `extractInheritance` Go branches: `constraint_elem` interface embedding,
    /// `field_declaration` struct embedding without a field name).
    ///
    /// GRAMMAR-SKEW NOTE (docs/rust-build-plan.md §7): the TS build used an older
    /// tree-sitter-go that exposed struct fields as direct `field_declaration`
    /// children and interface embeddings as `constraint_elem`. The 0.25 crate nests
    /// fields under `field_declaration_list` and uses `type_elem` for interface
    /// type-sets — so descending those shapes OVER-captures vs the TS reference
    /// (extends 340%, cascading into the interface-override synthesizer → calls
    /// 110%). We deliberately keep the TS port verbatim (extends ≈ the TS shape on
    /// this grammar) for parity; richer Go embedding coverage is a separate,
    /// benchmark-gated change, not a parity port.
    fn extract_inheritance(&mut self, node: TsNode<'_>, class_id: &str) {
        for i in 0..node.named_child_count() {
            let Some(child) = node.named_child(i) else {
                continue;
            };
            match child.kind() {
                // TS/JS: `class A extends B implements C, D` and `interface I extends J`.
                "class_heritage" | "extends_clause" | "implements_clause"
                | "extends_type_clause" => {
                    self.extract_ts_heritage(child, class_id);
                }
                // Python class bases: `class Foo(Base, Mixin, metaclass=M)` — the
                // `superclasses` argument_list; positional identifiers are bases.
                "argument_list" if self.language == Language::Python => {
                    for j in 0..child.named_child_count() {
                        let Some(b) = child.named_child(j) else { continue };
                        if matches!(b.kind(), "identifier" | "attribute") {
                            if let Some(name) = ts_base_type_name(b, self.source) {
                                self.add_unresolved(class_id, &name, ReferenceKind::Edge(EdgeKind::Extends), b);
                            }
                        }
                    }
                }
                "constraint_elem" => {
                    if let Some(tid) = (0..child.named_child_count())
                        .filter_map(|j| child.named_child(j))
                        .find(|c| c.kind() == "type_identifier")
                    {
                        let name = node_text(tid, self.source).to_string();
                        self.add_unresolved(class_id, &name, ReferenceKind::Edge(EdgeKind::Extends), tid);
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
                            self.add_unresolved(class_id, &name, ReferenceKind::Edge(EdgeKind::Extends), tid);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// TS/JS class/interface heritage → `extends`/`implements` refs.
    /// `class_heritage` wraps `extends_clause` (a value expression) and
    /// `implements_clause` (types); `extends_type_clause` is interface-extends.
    fn extract_ts_heritage(&mut self, node: TsNode<'_>, class_id: &str) {
        match node.kind() {
            "class_heritage" => {
                for i in 0..node.named_child_count() {
                    if let Some(child) = node.named_child(i) {
                        self.extract_ts_heritage(child, class_id);
                    }
                }
            }
            "extends_clause" | "extends_type_clause" => {
                // `extends_clause`'s superclass is an expression (`value` field or
                // first child); `extends_type_clause` holds type(s). Both → extends.
                let targets: Vec<TsNode> = child_by_field(node, "value")
                    .map(|v| vec![v])
                    .unwrap_or_else(|| {
                        (0..node.named_child_count())
                            .filter_map(|i| node.named_child(i))
                            .filter(|c| c.kind() != "type_arguments")
                            .collect()
                    });
                for t in targets {
                    if let Some(name) = ts_base_type_name(t, self.source) {
                        self.add_unresolved(class_id, &name, ReferenceKind::Edge(EdgeKind::Extends), t);
                    }
                }
            }
            "implements_clause" => {
                for i in 0..node.named_child_count() {
                    let Some(c) = node.named_child(i) else { continue };
                    if c.kind() == "type_arguments" {
                        continue;
                    }
                    if let Some(name) = ts_base_type_name(c, self.source) {
                        self.add_unresolved(class_id, &name, ReferenceKind::Edge(EdgeKind::Implements), c);
                    }
                }
            }
            _ => {}
        }
    }

    /// Emit `references` refs for the types named in a function/method's
    /// parameter and result positions (port of `extractTypeAnnotations`). Go names
    /// the return position `result`; TS/JS names it `return_type`.
    fn extract_type_annotations(&mut self, node: TsNode<'_>, from_id: &str) {
        if let Some(params) = child_by_field(node, "parameters") {
            self.extract_type_refs_from_subtree(params, from_id);
        }
        if let Some(result) =
            child_by_field(node, "result").or_else(|| child_by_field(node, "return_type"))
        {
            self.extract_type_refs_from_subtree(result, from_id);
        }
        // Direct type annotation — class fields / property signatures
        // (`foo: Bar`), where the type isn't under params/return.
        if let Some(ty) =
            child_by_field(node, "type").or_else(|| find_child_by_types(node, &["type_annotation"]))
        {
            self.extract_type_refs_from_subtree(ty, from_id);
        }
    }

    /// Recurse a type-position subtree, emitting a `references` ref per non-builtin
    /// `type_identifier` leaf (TS `extractTypeRefsFromSubtree`).
    fn extract_type_refs_from_subtree(&mut self, node: TsNode<'_>, from_id: &str) {
        if node.kind() == "type_identifier" {
            let type_name = node_text(node, self.source);
            if !type_name.is_empty() && !BUILTIN_TYPES.contains(&type_name) {
                self.add_unresolved(
                    from_id,
                    &type_name.to_string(),
                    ReferenceKind::Edge(EdgeKind::References),
                    node,
                );
            }
            return; // type_identifier is a leaf
        }
        for i in 0..node.named_child_count() {
            if let Some(child) = node.named_child(i) {
                self.extract_type_refs_from_subtree(child, from_id);
            }
        }
    }

    /// Walk a function/method body for calls, instantiations, nested named
    /// functions, and structural decls (TS `visitFunctionBody`).
    fn visit_function_body(&mut self, body: TsNode<'_>) {
        let kind = body.kind();
        let ex = self.extractor;

        // Function-as-value candidates inside the body (callbacks passed as args, …).
        self.maybe_capture_fn_refs(body);

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

        // Body-local typed declarations (`const x: Foo = …`): the locals aren't
        // nodes, but the annotated TYPE is a real dependency of the enclosing
        // function — emit a `references` edge to it (port of the visitFunctionBody
        // `variable_declarator` path; the TS rust extractor deliberately does NOT
        // cover Rust's `let_declaration`, so neither do we — parity). Falls through
        // to recursion so the initializer's calls are still walked.
        if kind == "variable_declarator" {
            if let Some(owner) = self.scopes.last().map(|s| s.id.clone()) {
                if let Some(ty) =
                    child_by_field(body, "type").or_else(|| find_child_by_types(body, &["type_annotation"]))
                {
                    self.extract_type_refs_from_subtree(ty, &owner);
                }
            }
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

/// Base type NAME from a TS heritage target — unwrapping `generic_type`
/// (`Foo<T>` → `Foo`), taking the last segment of a qualified
/// `member_expression`/`nested_type_identifier` (`ns.Foo` → `Foo`).
fn ts_base_type_name(node: TsNode<'_>, source: &str) -> Option<String> {
    match node.kind() {
        "type_identifier" | "identifier" => {
            let t = node_text(node, source).trim();
            (!t.is_empty()).then(|| t.to_string())
        }
        "generic_type" => node.named_child(0).and_then(|c| ts_base_type_name(c, source)),
        // TS member_expression (`ns.Foo`) and Python attribute (`mod.Base`).
        "member_expression" | "nested_type_identifier" | "attribute" => {
            child_by_field(node, "property")
                .or_else(|| child_by_field(node, "attribute"))
                .or_else(|| node.named_child(node.named_child_count().saturating_sub(1)))
                .and_then(|c| ts_base_type_name(c, source))
        }
        _ => {
            // Fall back to a descendant type_identifier/identifier.
            (0..node.named_child_count())
                .filter_map(|i| node.named_child(i))
                .find_map(|c| {
                    matches!(c.kind(), "type_identifier" | "identifier")
                        .then(|| node_text(c, source).trim().to_string())
                        .filter(|s| !s.is_empty())
                })
        }
    }
}

/// Property-key text with surrounding quotes stripped (`'foo'` → `foo`).
fn object_key_name(key: TsNode<'_>, source: &str) -> String {
    node_text(key, source).trim_matches(['\'', '"', '`']).to_string()
}

/// The object literal a function expression returns — the `=> ({...})` arrow form
/// (object inside a parenthesized_expression) or `=> { return {...} }` block
/// (port of `functionReturnedObject`).
fn function_returned_object<'n>(fn_node: TsNode<'n>) -> Option<TsNode<'n>> {
    let body = child_by_field(fn_node, "body")?;
    fn as_object<'n>(n: TsNode<'n>) -> Option<TsNode<'n>> {
        if matches!(n.kind(), "object" | "object_expression") {
            return Some(n);
        }
        if n.kind() == "parenthesized_expression" {
            for i in 0..n.named_child_count() {
                if let Some(inner) = n.named_child(i).and_then(as_object) {
                    return Some(inner);
                }
            }
        }
        None
    }
    if let Some(direct) = as_object(body) {
        return Some(direct);
    }
    if body.kind() == "statement_block" {
        for i in 0..body.named_child_count() {
            let Some(stmt) = body.named_child(i) else { continue };
            if stmt.kind() != "return_statement" {
                continue;
            }
            for j in 0..stmt.named_child_count() {
                if let Some(obj) = stmt.named_child(j).and_then(as_object) {
                    return Some(obj);
                }
            }
        }
    }
    None
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
