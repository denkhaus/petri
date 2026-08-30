//! The run token: an unsigned JWT the toolkit decodes without verifying.
//!
//! `getBackendIdsFromToken` in the toolkit splits the `scp` claim on spaces
//! and colons looking for `Actions.Results:<wfRunBackendId>:<jobRunBackendId>`
//! — those ids key artifact names per run, and one token serves every job
//! because `ListArtifacts` filters by the run id. The signature is never
//! checked, so minting costs nothing; the `nonce` claim carries the real
//! randomness that makes the exact string a credential.

use std::fmt::Write;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// The backend ids this runner mints into every token. Fixed: the store is
/// per-run already, so the ids only need to be self-consistent (and free of
/// the `:` and space the toolkit splits on).
pub(crate) const WORKFLOW_RUN_BACKEND_ID: &str = "petri-wf-run";
pub(crate) const JOB_RUN_BACKEND_ID: &str = "petri-job";

/// `N` bytes from the operating system's generator.
pub(crate) fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).expect("the OS random source answers");
    bytes
}

/// Lowercase hex — the one spelling of the codec for the whole crate.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Mint the run's token: `header.payload.` with no signature.
pub(crate) fn mint() -> String {
    let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\",\"typ\":\"JWT\"}");
    let payload = format!(
        "{{\"scp\":\"Actions.Results:{WORKFLOW_RUN_BACKEND_ID}:{JOB_RUN_BACKEND_ID}\",\"nonce\":\"{}\"}}",
        hex(&random::<16>())
    );
    format!("{header}.{}.", URL_SAFE_NO_PAD.encode(payload.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_decodes_the_way_the_toolkit_reads_it() {
        let token = mint();
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert!(parts[2].is_empty(), "unsigned");
        let payload = URL_SAFE_NO_PAD.decode(parts[1]).expect("base64url");
        let claims: serde_json::Value = serde_json::from_slice(&payload).expect("JSON claims");
        let scp = claims["scp"].as_str().expect("an scp claim");
        let scope = scp
            .split(' ')
            .find(|s| s.starts_with("Actions.Results:"))
            .expect("the results scope");
        let parts: Vec<&str> = scope.split(':').collect();
        assert_eq!(parts, vec![
            "Actions.Results",
            WORKFLOW_RUN_BACKEND_ID,
            JOB_RUN_BACKEND_ID
        ]);
    }

    #[test]
    fn two_tokens_never_collide() {
        assert_ne!(mint(), mint());
    }
}
