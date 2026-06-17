//! Grammar loading + language detection — port of `extraction/grammars.ts`
//! (the Go-relevant subset). Native tree-sitter (no WASM): the grammar is
//! statically linked via the `tree-sitter-go` crate.
//!
//! `EXTENSION_MAP` is the single source of truth for "should we index this file",
//! so parser support and selection never drift (TS `isSourceFile`).

use crate::types::Language;
use std::path::Path;
use tree_sitter::Language as TsLanguage;

/// Map a file extension to a `Language`. Subset of the TS `EXTENSION_MAP`,
/// covering the languages whose extractors are ported (Go) plus the extensions
/// we recognize for file-count parity. Unported languages map to a real
/// `Language` so the file is *counted* but yields no symbols yet.
pub fn detect_language(file_path: &str) -> Language {
    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("go") => Language::Go,
        Some("ts") | Some("mts") | Some("cts") => Language::Typescript,
        Some("tsx") => Language::Tsx,
        Some("js") | Some("mjs") | Some("cjs") => Language::Javascript,
        Some("jsx") => Language::Jsx,
        Some("py") | Some("pyw") => Language::Python,
        Some("rs") => Language::Rust,
        Some("java") => Language::Java,
        Some("c") | Some("h") => Language::C,
        Some("cpp") | Some("cc") | Some("cxx") | Some("hpp") | Some("hxx") => Language::Cpp,
        Some("cs") => Language::Csharp,
        Some("php") => Language::Php,
        Some("rb") | Some("rake") => Language::Ruby,
        Some("swift") => Language::Swift,
        Some("kt") | Some("kts") => Language::Kotlin,
        Some("dart") => Language::Dart,
        Some("scala") | Some("sc") => Language::Scala,
        Some("lua") => Language::Lua,
        Some("r") => Language::R,
        Some("m") | Some("mm") => Language::Objc,
        _ => Language::Unknown,
    }
}

/// Whether codegraph indexes this file at all (TS `isSourceFile`). Derived from
/// `detect_language` so it never drifts from parser selection.
pub fn is_source_file(file_path: &str) -> bool {
    detect_language(file_path) != Language::Unknown
}

/// Whether a ported tree-sitter extractor exists for this language (so parsing
/// yields symbols, not just a file record).
pub fn has_extractor(language: Language) -> bool {
    crate::extraction::languages::extractor_for(language).is_some()
}

/// The tree-sitter grammar for a language, if one is linked.
pub fn grammar_for(language: Language) -> Option<TsLanguage> {
    match language {
        Language::Go => Some(tree_sitter_go::LANGUAGE.into()),
        Language::Typescript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        Language::Tsx => Some(tree_sitter_typescript::LANGUAGE_TSX.into()),
        Language::Javascript | Language::Jsx => Some(tree_sitter_javascript::LANGUAGE.into()),
        Language::Python => Some(tree_sitter_python::LANGUAGE.into()),
        Language::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    /// ABI smoke test: the TS grammar (0.23) loads + parses under tree-sitter 0.25.
    #[test]
    fn ts_grammar_loads_and_parses() {
        for (lang, src, want) in [
            (Language::Typescript, "export function foo(x: number): number { return x; }", "function_declaration"),
            (Language::Tsx, "const App = () => <div/>;", "lexical_declaration"),
            (Language::Javascript, "class C { m() { return 1; } }", "class_declaration"),
        ] {
            let grammar = grammar_for(lang).expect("grammar linked");
            let mut p = Parser::new();
            p.set_language(&grammar).expect("set_language (ABI compatible)");
            let tree = p.parse(src, None).expect("parses");
            let sexp = tree.root_node().to_sexp();
            assert!(sexp.contains(want), "{:?}: expected {} in {}", lang, want, sexp);
        }
    }
}
