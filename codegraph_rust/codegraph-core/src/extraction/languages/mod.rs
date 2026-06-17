//! Per-language extractor configs (port of `extraction/languages/`).
//! Ported incrementally — Go first (k8s parity). TS/JS, Python, Rust follow at M4.

pub mod go;
pub mod javascript;
pub mod python;
pub mod rust;
pub mod typescript;

use crate::extraction::extractor::LanguageExtractor;
use crate::types::Language;

/// Return the extractor for a language, or `None` if not yet ported.
/// Mirrors the TS `EXTRACTORS[language]` lookup. TSX/JSX reuse the TS/JS
/// extractors (the TSX/JS grammars already parse the embedded JSX).
pub fn extractor_for(language: Language) -> Option<&'static dyn LanguageExtractor> {
    match language {
        Language::Go => Some(&go::GoExtractor),
        Language::Typescript | Language::Tsx => Some(&typescript::TypescriptExtractor),
        Language::Javascript | Language::Jsx => Some(&javascript::JavascriptExtractor),
        Language::Python => Some(&python::PythonExtractor),
        Language::Rust => Some(&rust::RustExtractor),
        _ => None,
    }
}
