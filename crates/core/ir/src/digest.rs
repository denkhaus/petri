//! Canonical graph encoding and content digests.

use std::fmt::Write as _;

use sha2::{Digest as _, Sha256};

use crate::Graph;

/// Encode a graph exactly as the coordinator persists it.
pub fn encode_graph(graph: &Graph) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(graph)
}

/// SHA-256 of canonical persisted graph bytes.
pub fn graph_digest_bytes(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// SHA-256 of a graph's canonical encoding.
pub fn graph_digest(graph: &Graph) -> Result<[u8; 32], serde_json::Error> {
    encode_graph(graph).map(|bytes| graph_digest_bytes(&bytes))
}

/// A lowercase hexadecimal digest.
pub fn digest_hex(bytes: &[u8; 32]) -> String {
    let mut output = String::with_capacity(64);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}
