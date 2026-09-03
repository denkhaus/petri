//! The content digest of a lowered graph.
//!
//! A nested-workflow step names its child graph by digest, and the
//! coordinator registers graphs under the same digest, so the two must agree
//! on one formula: SHA-256 over the graph's compact JSON encoding. This is
//! that formula, in the one crate both a frontend and the execution layer can
//! reach.

use std::fmt::Write as _;

use sha2::{Digest as _, Sha256};

/// The lowercase hexadecimal SHA-256 of `graph`'s compact JSON encoding —
/// the key the coordinator's graph registry uses.
pub fn graph_digest(graph: &ir::Graph) -> String {
    let encoded = serde_json::to_vec(graph).expect("a graph always encodes as JSON");
    let bytes = Sha256::digest(&encoded);
    let mut out = String::with_capacity(64);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}
