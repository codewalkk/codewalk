//! codegraph-core — the structural code-intelligence engine.
//!
//! Pipeline (docs/codewalk_rust_arch.md §2):
//!   files → extraction (tree-sitter, rayon) → db (nodes/edges/files, FTS5)
//!         → resolution → graph → context
//!
//! This crate is the reusable moat and stays PURE: no embeddings, no LLM, no
//! network. Those concerns live in `codewalk-kb` (mode 2), which depends on this
//! crate, never the reverse.

pub mod context;
pub mod db;
pub mod extraction;
pub mod graph;
pub mod resolution;
pub mod search;
pub mod types;

use sha2::{Digest, Sha256};

pub use db::{FileRecord, Stats, Store, WriteBatch};
pub use extraction::{extract_file, index_repo, is_source_file, detect_language};
pub use resolution::{resolve, ResolveStats};
pub use types::{
    Edge, EdgeKind, ExtractionResult, Language, Node, NodeKind, Provenance, ReferenceKind,
    UnresolvedReference, Visibility,
};

/// Generate a unique node id — port of `generateNodeId` (tree-sitter-helpers.ts:18).
///
/// 128-bit (32 hex chars) SHA-256 prefix of `filePath:kind:name:line`, prefixed
/// with the kind: `"<kind>:<hash>"`. Must match the TS algorithm byte-for-byte so
/// ids are stable across the two implementations.
pub fn node_id(file_path: &str, kind: NodeKind, name: &str, line: u32) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{}:{}:{}:{}", file_path, kind.as_str(), name, line).as_bytes());
    let hex = hex_encode(&hasher.finalize());
    format!("{}:{}", kind.as_str(), &hex[..32])
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_matches_ts_shape() {
        // `<kind>:<32 hex chars>` and deterministic.
        let a = node_id("pkg/scheduler/schedule_one.go", NodeKind::Function, "ScheduleOne", 42);
        let b = node_id("pkg/scheduler/schedule_one.go", NodeKind::Function, "ScheduleOne", 42);
        assert_eq!(a, b);
        assert!(a.starts_with("function:"));
        assert_eq!(a.len(), "function:".len() + 32);
    }
}
