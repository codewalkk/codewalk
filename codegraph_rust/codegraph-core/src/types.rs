//! Core graph types — a faithful port of the TS spec `codegraph/src/types.ts`
//! (`NODE_KINDS`:18, `EdgeKind`:48, `LANGUAGES`:66).
//!
//! Every language normalizes onto these two enums, so 30+ languages share one
//! query surface. The string forms (via `as_str` / serde `rename`) match the TS
//! values verbatim, because they are what gets written into `graph.db` and
//! compared against the TS index for parity.

use serde::{Deserialize, Serialize};

/// Types of nodes in the knowledge graph. Mirrors `NODE_KINDS` (types.ts:18).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    File,
    Module,
    Class,
    Struct,
    Interface,
    Trait,
    Protocol,
    Function,
    Method,
    Property,
    Field,
    Variable,
    Constant,
    Enum,
    EnumMember,
    TypeAlias,
    Namespace,
    Parameter,
    Import,
    Export,
    Route,
    Component,
}

impl NodeKind {
    /// The canonical TS string (used in node ids, the `kind` column, FTS).
    pub fn as_str(&self) -> &'static str {
        match self {
            NodeKind::File => "file",
            NodeKind::Module => "module",
            NodeKind::Class => "class",
            NodeKind::Struct => "struct",
            NodeKind::Interface => "interface",
            NodeKind::Trait => "trait",
            NodeKind::Protocol => "protocol",
            NodeKind::Function => "function",
            NodeKind::Method => "method",
            NodeKind::Property => "property",
            NodeKind::Field => "field",
            NodeKind::Variable => "variable",
            NodeKind::Constant => "constant",
            NodeKind::Enum => "enum",
            NodeKind::EnumMember => "enum_member",
            NodeKind::TypeAlias => "type_alias",
            NodeKind::Namespace => "namespace",
            NodeKind::Parameter => "parameter",
            NodeKind::Import => "import",
            NodeKind::Export => "export",
            NodeKind::Route => "route",
            NodeKind::Component => "component",
        }
    }
}

impl std::fmt::Display for NodeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Types of edges (relationships) between nodes. Mirrors `EdgeKind` (types.ts:48).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// Parent contains child (file→class, class→method)
    Contains,
    /// Function/method calls another
    Calls,
    /// File imports from another
    Imports,
    /// File exports a symbol
    Exports,
    /// Class/interface extends another
    Extends,
    /// Class implements interface
    Implements,
    /// Generic reference to another symbol
    References,
    /// Variable/parameter has type
    TypeOf,
    /// Function returns type
    Returns,
    /// Creates instance of class
    Instantiates,
    /// Method overrides parent method
    Overrides,
    /// Decorator applied to symbol
    Decorates,
}

impl EdgeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EdgeKind::Contains => "contains",
            EdgeKind::Calls => "calls",
            EdgeKind::Imports => "imports",
            EdgeKind::Exports => "exports",
            EdgeKind::Extends => "extends",
            EdgeKind::Implements => "implements",
            EdgeKind::References => "references",
            EdgeKind::TypeOf => "type_of",
            EdgeKind::Returns => "returns",
            EdgeKind::Instantiates => "instantiates",
            EdgeKind::Overrides => "overrides",
            EdgeKind::Decorates => "decorates",
        }
    }
}

impl std::fmt::Display for EdgeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Kinds an unresolved reference can carry. `function_ref` is internal-only — a
/// function name used as a VALUE (callback registration, #756). It never becomes
/// an edge kind: resolution maps it to a `references` edge targeting
/// function/method nodes only. Mirrors `ReferenceKind` (types.ts:289).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceKind {
    Edge(EdgeKind),
    FunctionRef,
}

impl ReferenceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReferenceKind::Edge(k) => k.as_str(),
            ReferenceKind::FunctionRef => "function_ref",
        }
    }
}

/// How an edge was created. Mirrors `Edge.provenance` (types.ts:204) and the
/// `edges.provenance` column. `Heuristic` flags every synthesized edge — the
/// honesty invariant from the arch docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provenance {
    TreeSitter,
    Scip,
    Heuristic,
}

impl Provenance {
    pub fn as_str(&self) -> &'static str {
        match self {
            Provenance::TreeSitter => "tree-sitter",
            Provenance::Scip => "scip",
            Provenance::Heuristic => "heuristic",
        }
    }
}

/// Supported programming languages. Mirrors `LANGUAGES` (types.ts:66).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Typescript,
    Javascript,
    Tsx,
    Jsx,
    Python,
    Go,
    Rust,
    Java,
    C,
    Cpp,
    Csharp,
    Razor,
    Php,
    Ruby,
    Swift,
    Kotlin,
    Dart,
    Svelte,
    Vue,
    Astro,
    Liquid,
    Pascal,
    Scala,
    Lua,
    Luau,
    Objc,
    R,
    Yaml,
    Twig,
    Xml,
    Properties,
    Unknown,
}

impl Language {
    pub fn as_str(&self) -> &'static str {
        match self {
            Language::Typescript => "typescript",
            Language::Javascript => "javascript",
            Language::Tsx => "tsx",
            Language::Jsx => "jsx",
            Language::Python => "python",
            Language::Go => "go",
            Language::Rust => "rust",
            Language::Java => "java",
            Language::C => "c",
            Language::Cpp => "cpp",
            Language::Csharp => "csharp",
            Language::Razor => "razor",
            Language::Php => "php",
            Language::Ruby => "ruby",
            Language::Swift => "swift",
            Language::Kotlin => "kotlin",
            Language::Dart => "dart",
            Language::Svelte => "svelte",
            Language::Vue => "vue",
            Language::Astro => "astro",
            Language::Liquid => "liquid",
            Language::Pascal => "pascal",
            Language::Scala => "scala",
            Language::Lua => "lua",
            Language::Luau => "luau",
            Language::Objc => "objc",
            Language::R => "r",
            Language::Yaml => "yaml",
            Language::Twig => "twig",
            Language::Xml => "xml",
            Language::Properties => "properties",
            Language::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Visibility modifier (types.ts:148).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Public,
    Private,
    Protected,
    Internal,
}

impl Visibility {
    pub fn as_str(&self) -> &'static str {
        match self {
            Visibility::Public => "public",
            Visibility::Private => "private",
            Visibility::Protected => "protected",
            Visibility::Internal => "internal",
        }
    }
}

/// A node in the knowledge graph representing a code symbol. Port of `Node`
/// (types.ts:110). Positions are 1-indexed lines / 0-indexed columns, matching
/// the TS extractor.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    /// Unique identifier: `<kind>:<hash>` (see `node_id`).
    pub id: String,
    pub kind: NodeKind,
    /// Simple name (e.g. "calculateTotal").
    pub name: String,
    /// Fully qualified name (e.g. "MathHelper::calculateTotal"). No file path.
    pub qualified_name: String,
    /// File path relative to project root.
    pub file_path: String,
    pub language: Language,
    pub start_line: u32,
    pub end_line: u32,
    pub start_column: u32,
    pub end_column: u32,
    pub docstring: Option<String>,
    pub signature: Option<String>,
    pub visibility: Option<Visibility>,
    pub is_exported: bool,
    pub is_async: bool,
    pub is_static: bool,
    pub is_abstract: bool,
    /// Decorators/annotations applied (JSON array in the DB).
    pub decorators: Option<Vec<String>>,
    pub type_parameters: Option<Vec<String>>,
    /// Normalized return/result type name — used by resolution to infer a chained
    /// receiver's type (types.ts:168, Go/C++ #645).
    pub return_type: Option<String>,
    pub updated_at: i64,
}

impl Node {
    /// Construct a node with all optional fields empty, positions/flags defaulted.
    pub fn new(
        id: String,
        kind: NodeKind,
        name: String,
        qualified_name: String,
        file_path: String,
        language: Language,
    ) -> Self {
        Node {
            id,
            kind,
            name,
            qualified_name,
            file_path,
            language,
            start_line: 1,
            end_line: 1,
            start_column: 0,
            end_column: 0,
            docstring: None,
            signature: None,
            visibility: None,
            is_exported: false,
            is_async: false,
            is_static: false,
            is_abstract: false,
            decorators: None,
            type_parameters: None,
            return_type: None,
            updated_at: 0,
        }
    }
}

/// An edge representing a relationship between two nodes. Port of `Edge`
/// (types.ts:184).
#[derive(Debug, Clone, PartialEq)]
pub struct Edge {
    pub source: String,
    pub target: String,
    pub kind: EdgeKind,
    /// Additional context (JSON object in the DB): synthesizedBy, via, registeredAt.
    pub metadata: Option<serde_json::Value>,
    pub line: Option<u32>,
    pub col: Option<u32>,
    pub provenance: Option<Provenance>,
}

impl Edge {
    pub fn new(source: String, target: String, kind: EdgeKind) -> Self {
        Edge {
            source,
            target,
            kind,
            metadata: None,
            line: None,
            col: None,
            provenance: None,
        }
    }
}

/// A reference that couldn't be resolved during extraction. Port of
/// `UnresolvedReference` (types.ts:294). `file_path`/`language` are denormalized
/// for resolver performance.
#[derive(Debug, Clone, PartialEq)]
pub struct UnresolvedReference {
    pub from_node_id: String,
    pub reference_name: String,
    pub reference_kind: ReferenceKind,
    pub line: u32,
    pub col: u32,
    pub file_path: Option<String>,
    pub language: Option<Language>,
    pub candidates: Option<Vec<String>>,
}

/// Result of extracting one source file. Port of `ExtractionResult` (types.ts:243).
#[derive(Debug, Clone, Default)]
pub struct ExtractionResult {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub unresolved_references: Vec<UnresolvedReference>,
    pub errors: Vec<String>,
}
