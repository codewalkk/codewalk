//! codewalk-kb — the optional learned-intelligence layer (mode 2).
//!
//! Lands at M5 (docs/rust-build-plan.md §6): `fastembed` MiniLM embeddings, a KB
//! store (rusqlite + cosine), RRF fusion over structural + learned hits, and
//! transcript capture/distill. It depends on `codegraph-core` and never the
//! reverse — mode 1 must stay a pure, offline, deterministic single binary.

// Placeholder until M5. Kept as a buildable member so the workspace graph and
// crate-dependency rules (arch §8) are wired from day one.
