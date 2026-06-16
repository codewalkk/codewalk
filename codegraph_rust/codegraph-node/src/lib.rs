//! codegraph-node — Node native module (napi-rs) over codegraph-core.
//!
//! Lands at M6 (docs/rust-build-plan.md §6 / arch §5): exposes the `CodeGraph`
//! facade shape the TS side expects (`open`, `indexAll`, `searchNodes`,
//! `getCallers`/`getCallees`, `getImpactRadius`, `getNodesByName`,
//! `buildContext`, `getStats`) so the TS Codewalk can swap its codegraph
//! dependency for the native core. Plain lib placeholder until then (no napi
//! toolchain pulled in yet, to keep the M0 workspace building cleanly).
