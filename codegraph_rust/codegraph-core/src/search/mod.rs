//! Search — lexical query utilities + the `SearchResult` type shared by the DB
//! search layer and the context builder. Commodity lexical ranking, no vectors.

pub mod query_utils;

use crate::types::Node;

/// A scored search hit (TS `SearchResult`).
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub node: Node,
    pub score: f64,
}
