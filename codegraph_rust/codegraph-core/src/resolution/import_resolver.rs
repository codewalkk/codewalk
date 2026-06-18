//! Import resolution — port of the TS/JS-relevant subset of
//! `resolution/import-resolver.ts`. Resolves a relative module specifier to an
//! indexed file, then resolves an imported symbol name to that file's definition.
//!
//! External (bare/npm) specifiers are declined — they have no in-repo target.

use super::graph::Graph;
use super::name_matcher::{ladder, Resolved};
use crate::types::{Language, UnresolvedReference};

/// Extension-resolution order per language (port of `EXTENSION_RESOLUTION`).
fn extensions_for(lang: Language) -> &'static [&'static str] {
    match lang {
        Language::Typescript => &[".ts", ".tsx", ".d.ts", ".js", ".jsx", "/index.ts", "/index.tsx", "/index.js"],
        Language::Tsx => &[".tsx", ".ts", ".d.ts", ".js", ".jsx", "/index.tsx", "/index.ts", "/index.js"],
        Language::Javascript => &[".js", ".jsx", ".mjs", ".cjs", "/index.js", "/index.jsx"],
        Language::Jsx => &[".jsx", ".js", "/index.jsx", "/index.js"],
        Language::Python => &[".py", "/__init__.py"],
        _ => &[],
    }
}

/// Translate a Python dotted-relative module (`.models`, `..pkg.mod`) into a
/// repo-relative base path. Leading dots are package levels (1 = current dir);
/// the remainder is a dotted submodule (`sub.mod` → `sub/mod`). Port of the
/// Python branch of `resolveRelativeImport`. Returns `None` for absolute imports.
fn resolve_python_base(module: &str, from_dir: &str) -> Option<String> {
    if !module.starts_with('.') {
        return None; // absolute import — needs the package root; skipped
    }
    let dots = module.len() - module.trim_start_matches('.').len();
    let rest = module[dots..].replace('.', "/");
    let mut parts: Vec<&str> = from_dir.split('/').filter(|s| !s.is_empty()).collect();
    // 1 dot = current dir; each extra dot pops one package level.
    for _ in 0..dots.saturating_sub(1) {
        parts.pop();
    }
    for seg in rest.split('/').filter(|s| !s.is_empty()) {
        parts.push(seg);
    }
    Some(parts.join("/"))
}

/// Lexically resolve `module` (a `.`/`..`-relative specifier) against `from_dir`
/// (the importing file's directory, repo-relative) into a repo-relative base path,
/// normalizing `.`/`..` segments. Returns `None` for non-relative specifiers.
fn resolve_relative_base(module: &str, from_dir: &str) -> Option<String> {
    if !module.starts_with('.') {
        return None;
    }
    let mut parts: Vec<&str> = if from_dir.is_empty() {
        Vec::new()
    } else {
        from_dir.split('/').filter(|s| !s.is_empty()).collect()
    };
    for seg in module.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

/// Resolve a relative module specifier to an indexed file (port of
/// `resolveRelativeImport`, TS/JS path): try `base+ext` for each extension,
/// then `base` as-is.
fn resolve_relative_import(module: &str, from_dir: &str, lang: Language, g: &Graph) -> Option<String> {
    let base = if lang == Language::Python {
        resolve_python_base(module, from_dir)?
    } else {
        resolve_relative_base(module, from_dir)?
    };
    for ext in extensions_for(lang) {
        let cand = format!("{}{}", base, ext);
        if g.file_indexed(&cand) {
            return Some(cand);
        }
    }
    if !base.is_empty() && g.file_indexed(&base) {
        return Some(base);
    }
    None
}

fn dirname(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

/// Resolve a TS/JS `imports` ref (`reference_name` = the imported symbol,
/// `candidates[0]` = the module specifier) to the symbol's definition in the
/// resolved module file. Returns `None` for external modules or unindexed targets.
pub fn resolve_ts_import(g: &Graph, r: &UnresolvedReference) -> Option<Resolved> {
    let lang = r.language?;
    if !matches!(
        lang,
        Language::Typescript
            | Language::Tsx
            | Language::Javascript
            | Language::Jsx
            | Language::Python
    ) {
        return None;
    }
    let module = r.candidates.as_ref().and_then(|c| c.first())?;
    let from_dir = dirname(r.file_path.as_deref().unwrap_or(""));

    // Python submodule import: `from . import events` / `from .pkg import sub`
    // binds a SUBMODULE, not a symbol — resolve `module.name` to its file and
    // target that file node (TS's file→file import edges).
    if lang == Language::Python {
        let submod = if module.ends_with('.') {
            format!("{}{}", module, r.reference_name)
        } else {
            format!("{}.{}", module, r.reference_name)
        };
        if let Some(file) = resolve_relative_import(&submod, from_dir, lang, g) {
            let file_id = format!("file:{}", file);
            if g.node_by_id(&file_id).is_some() {
                return Some(Resolved {
                    target_id: file_id,
                    confidence: ladder::IMPORT_MAP,
                    resolved_by: "import",
                });
            }
        }
    }

    let target_file = resolve_relative_import(module, from_dir, lang, g)?;
    // Find a node named `reference_name` in the resolved file; prefer an exported one.
    let mut fallback: Option<&str> = None;
    for &i in g.nodes_by_name(&r.reference_name) {
        let n = g.node(i);
        if n.file_path != target_file {
            continue;
        }
        if n.is_exported {
            return Some(Resolved {
                target_id: n.id.clone(),
                confidence: ladder::IMPORT_MAP,
                resolved_by: "import",
            });
        }
        fallback.get_or_insert(n.id.as_str());
    }
    fallback.map(|id| Resolved {
        target_id: id.to_string(),
        confidence: ladder::IMPORT_MAP_SUFFIX,
        resolved_by: "import",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_base_normalizes() {
        assert_eq!(resolve_relative_base("./foo", "src/a"), Some("src/a/foo".into()));
        assert_eq!(resolve_relative_base("../bar", "src/a"), Some("src/bar".into()));
        assert_eq!(resolve_relative_base("../../x/y", "a/b/c"), Some("a/x/y".into()));
        assert_eq!(resolve_relative_base("commander", "src"), None);
    }
}
