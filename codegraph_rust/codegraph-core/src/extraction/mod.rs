//! Extraction — tree-sitter → uniform node/edge model + the indexing orchestrator.
//!
//! Pipeline: gitignore-aware walk (`ignore`) → parallel parse/extract (`rayon`,
//! native tree-sitter, no WASM) → `Store::write_batch`. The base engine
//! (`engine::extract_file`) is the port of `tree-sitter.ts`, driven by
//! per-language `LanguageExtractor` configs (`languages/`).

pub mod engine;
pub mod extractor;
pub mod fn_ref;
pub mod grammars;
pub mod languages;

use crate::db::{FileRecord, Stats, Store, WriteBatch};
use crate::types::Language;
use anyhow::Result;
use ignore::WalkBuilder;
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use std::path::Path;

pub use engine::extract_file;
pub use extractor::LanguageExtractor;
pub use grammars::{detect_language, is_source_file};

/// Files larger than this are skipped (TS `MAX_FILE_SIZE`).
const MAX_FILE_SIZE: u64 = 1024 * 1024;

/// Directory names that are dependency/build/cache/tooling output — port of
/// `DEFAULT_IGNORE_DIRS` (extraction/index.ts:117). Excluded by default so the
/// graph reflects your code, not third-party noise, even without a `.gitignore`.
/// Notably includes `vendor` (Go) — k8s vendors a large tree.
const DEFAULT_IGNORE_DIRS: &[&str] = &[
    // JS / TS deps
    "node_modules", "bower_components", "jspm_packages", "web_modules", ".yarn", ".pnpm-store",
    // JS / TS build / cache / deploy
    ".next", ".nuxt", ".svelte-kit", ".turbo", ".vite", ".parcel-cache", ".angular",
    ".docusaurus", "storybook-static", ".vinxi", ".nitro", "out-tsc", ".vercel", ".netlify",
    ".wrangler",
    // Build output
    "dist", "build", "out", ".output",
    // Test / coverage
    "coverage", ".nyc_output",
    // Python
    "__pycache__", "__pypackages__", ".venv", "venv", ".pixi", ".pdm-build", ".mypy_cache",
    ".pytest_cache", ".ruff_cache", ".tox", ".nox", ".hypothesis", ".ipynb_checkpoints", ".eggs",
    // Rust / JVM
    "target", ".gradle",
    // .NET
    "obj",
    // Vendored deps (Go, PHP/Composer, Ruby/Bundler)
    "vendor",
    // Swift / iOS
    ".build", "Pods", "Carthage", "DerivedData", ".swiftpm",
    // Dart / Flutter
    ".dart_tool", ".pub-cache",
    // Native
    ".cxx", ".externalNativeBuild", "vcpkg_installed",
    // Scala tooling
    ".bloop", ".metals",
    // Lua
    "lua_modules", ".luarocks",
    // Delphi IDE backups
    "__history", "__recovery",
    // Generic cache
    ".cache",
];

fn is_ignored_dir(name: &str) -> bool {
    DEFAULT_IGNORE_DIRS.contains(&name)
}

/// The per-file extraction product, ready to write to the store.
struct FileOutput {
    batch: WriteBatch,
}

/// Index a repository: walk → extract (parallel) → store. Returns final `Stats`.
/// File paths are stored relative to `repo_root` (matching the TS index).
pub fn index_repo(repo_root: &Path, store: &mut Store) -> Result<Stats> {
    let repo_root = repo_root.canonicalize()?;

    // 1. Walk (gitignore-aware + DEFAULT_IGNORE_DIRS pruning).
    let mut builder = WalkBuilder::new(&repo_root);
    builder
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .hidden(false) // we prune dot-dirs explicitly via DEFAULT_IGNORE_DIRS
        .filter_entry(|entry| {
            // Prune ignored directories (don't descend into vendor/, node_modules/…).
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                if let Some(name) = entry.file_name().to_str() {
                    return !is_ignored_dir(name);
                }
            }
            true
        });

    let mut paths: Vec<String> = Vec::new();
    for entry in builder.build() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        let rel = match path.strip_prefix(&repo_root) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        if !is_source_file(&rel) {
            continue;
        }
        paths.push(rel);
    }

    // 2. Extract in parallel (native tree-sitter + rayon). Each file is
    //    independent; the grammar is statically linked and the extractor is a
    //    shared `&'static dyn` (Send + Sync), so this is embarrassingly parallel.
    let outputs: Vec<FileOutput> = paths
        .par_iter()
        .filter_map(|rel| extract_one(&repo_root, rel))
        .collect();

    // 3. Store (rusqlite Connection isn't Sync — write after the parallel parse).
    //    One transaction for the whole repo: on k8s this turns ~12k WAL fsyncs
    //    into a single commit.
    store.clear()?;
    let batches: Vec<WriteBatch> = outputs.into_iter().map(|o| o.batch).collect();
    store.write_all(&batches)?;

    store.stats()
}

fn extract_one(repo_root: &Path, rel: &str) -> Option<FileOutput> {
    let abs = repo_root.join(rel);
    let meta = std::fs::metadata(&abs).ok()?;
    if meta.len() > MAX_FILE_SIZE {
        return None;
    }
    let bytes = std::fs::read(&abs).ok()?;
    let source = String::from_utf8(bytes).ok()?; // skip non-UTF8 files
    let language = detect_language(rel);

    let content_hash = {
        let mut h = Sha256::new();
        h.update(source.as_bytes());
        format!("{:x}", h.finalize())
    };
    let modified_at = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let mut batch = WriteBatch::default();
    let result = if grammars::has_extractor(language) {
        extract_file(rel, &source, language)
    } else {
        // File-record-only: counted as indexed, no symbols (matches TS for
        // languages whose extractor isn't ported yet).
        crate::types::ExtractionResult::default()
    };

    let node_count = result.nodes.len() as i64;
    batch.nodes = result.nodes;
    batch.edges = result.edges;
    batch.unresolved = result.unresolved_references;
    batch.files.push(FileRecord {
        path: rel.to_string(),
        content_hash,
        language,
        size: meta.len() as i64,
        modified_at,
        indexed_at: 0,
        node_count,
        errors: None,
    });

    Some(FileOutput { batch })
}

/// Languages with a ported extractor (for status/reporting).
pub fn ported_languages() -> &'static [Language] {
    &[
        Language::Go,
        Language::Typescript,
        Language::Tsx,
        Language::Javascript,
        Language::Jsx,
        Language::Python,
        Language::Rust,
    ]
}
