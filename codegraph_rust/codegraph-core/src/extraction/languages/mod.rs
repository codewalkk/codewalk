//! Per-language extractor configs (port of `extraction/languages/`).
//! Ported incrementally — Go first (k8s parity). TS/JS, Python, Rust follow at M4.

pub mod go;

use crate::extraction::extractor::LanguageExtractor;
use crate::types::Language;

/// Return the extractor for a language, or `None` if not yet ported.
/// Mirrors the TS `EXTRACTORS[language]` lookup.
pub fn extractor_for(language: Language) -> Option<&'static dyn LanguageExtractor> {
    match language {
        Language::Go => Some(&go::GoExtractor),
        _ => None,
    }
}
