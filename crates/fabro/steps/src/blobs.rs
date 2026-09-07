//! Output-reference storage: Fabro's blob offload, as a Petri capability.
//!
//! A stage value above [`OFFLOAD_THRESHOLD`] does not stay inline in the run
//! context or the event log. The step writes it to the run's [`BlobStore`]
//! and records the durable reference `blob://sha256/<hex>` in its place, as
//! Fabro does. A later step that reads the value (a `stdin_source`, a
//! prompt's preamble) hydrates the reference back through the same store.
//!
//! The store is a host capability, [`OutputStore`]. The standalone runner
//! installs [`LocalBlobStore`] under `<run_dir>/blobs`; an embedding host
//! that already has platform storage registers its own store before the
//! run and the default steps aside.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{io, process};

use ir::Value;
use smol_str::SmolStr;
use tokio::fs;

/// Fabro's offload threshold: a value whose serialized form is larger than
/// this leaves the context for the blob store.
pub const OFFLOAD_THRESHOLD: usize = 100 * 1024;

/// The reference prefix, Fabro's spelling.
pub const BLOB_REF_PREFIX: &str = "blob://sha256/";

/// Content-addressed storage for offloaded stage values.
#[async_trait::async_trait]
pub trait BlobStore: Send + Sync {
    /// Store `bytes` and return the lowercase hex SHA-256 that names them.
    async fn put(&self, bytes: &[u8]) -> Result<String, BlobError>;

    /// The bytes behind a digest, or `None` when the store has none.
    async fn get(&self, digest: &str) -> Result<Option<Vec<u8>>, BlobError>;

    /// A path on the driver's machine that holds the bytes, when the store
    /// can offer one: Fabro's execution-local materialization. The default
    /// has none, and a reader that needs a file writes one itself.
    async fn materialize(&self, digest: &str) -> Result<Option<PathBuf>, BlobError> {
        let _ = digest;
        Ok(None)
    }
}

/// The host capability a step looks up: the run's blob store.
#[derive(Clone)]
pub struct OutputStore(pub Arc<dyn BlobStore>);

#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("could not {action} blob {digest} at `{path}`")]
    Io {
        action: &'static str,
        digest: String,
        path:   PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("the blob store refused `{digest}`: {message}")]
    Store { digest: String, message: String },
}

/// A store of one file per digest under a directory: `<dir>/<hex>`.
#[derive(Clone, Debug)]
pub struct LocalBlobStore {
    dir: PathBuf,
}

impl LocalBlobStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path_of(&self, digest: &str) -> PathBuf {
        self.dir.join(digest)
    }
}

#[async_trait::async_trait]
impl BlobStore for LocalBlobStore {
    async fn put(&self, bytes: &[u8]) -> Result<String, BlobError> {
        let digest = ir::digest_hex(&ir::graph_digest_bytes(bytes));
        let path = self.path_of(&digest);
        if fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(digest);
        }
        fs::create_dir_all(&self.dir)
            .await
            .map_err(|source| BlobError::Io {
                action: "create the directory for",
                digest: digest.clone(),
                path: self.dir.clone(),
                source,
            })?;
        // Write beside, then rename: a reader never sees a torn blob.
        let partial = self.dir.join(format!("{digest}.partial-{}", process::id()));
        fs::write(&partial, bytes)
            .await
            .map_err(|source| BlobError::Io {
                action: "write",
                digest: digest.clone(),
                path: partial.clone(),
                source,
            })?;
        fs::rename(&partial, &path)
            .await
            .map_err(|source| BlobError::Io {
                action: "rename",
                digest: digest.clone(),
                path: path.clone(),
                source,
            })?;
        Ok(digest)
    }

    async fn get(&self, digest: &str) -> Result<Option<Vec<u8>>, BlobError> {
        let path = self.path_of(digest);
        match fs::read(&path).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(BlobError::Io {
                action: "read",
                digest: digest.to_string(),
                path,
                source,
            }),
        }
    }

    async fn materialize(&self, digest: &str) -> Result<Option<PathBuf>, BlobError> {
        let path = self.path_of(digest);
        Ok(fs::try_exists(&path).await.unwrap_or(false).then_some(path))
    }
}

/// The durable reference for a digest.
pub fn blob_ref(digest: &str) -> String {
    format!("{BLOB_REF_PREFIX}{digest}")
}

/// The digest a reference names, when `text` is one.
pub fn parse_blob_ref(text: &str) -> Option<&str> {
    let digest = text.strip_prefix(BLOB_REF_PREFIX)?;
    (digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())).then_some(digest)
}

/// Whether a value is large enough to leave the context, by Fabro's test: a
/// scalar never is; a string is measured by its worst-case JSON size first,
/// so a short string costs no serialization; anything else by its compact
/// JSON size.
pub fn is_large(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
        Value::String(text) => {
            if text.len().saturating_mul(6).saturating_add(2) <= OFFLOAD_THRESHOLD {
                return false;
            }
            serde_json::to_vec(value).is_ok_and(|bytes| bytes.len() > OFFLOAD_THRESHOLD)
        }
        Value::Array(_) | Value::Object(_) => {
            serde_json::to_vec(value).is_ok_and(|bytes| bytes.len() > OFFLOAD_THRESHOLD)
        }
    }
}

/// The bytes a value is stored as: a string as itself, anything else as
/// compact JSON. A string round-trips as text; a structured value round-trips
/// through JSON.
fn encode(value: &Value) -> (Vec<u8>, bool) {
    match value {
        Value::String(text) => (text.clone().into_bytes(), true),
        other => (serde_json::to_vec(other).unwrap_or_default(), false),
    }
}

/// Replace one large value by its reference. A value that is not large, or
/// that the store refuses, stays inline: offloading is best effort, as in
/// Fabro, and the logical value is never lost.
pub async fn offload_value(value: &mut Value, store: &dyn BlobStore) -> Option<String> {
    if !is_large(value) {
        return None;
    }
    let (bytes, is_text) = encode(value);
    let digest = match store.put(&bytes).await {
        Ok(digest) => digest,
        Err(error) => {
            tracing::warn!(error = %error, "large stage value stays inline");
            return None;
        }
    };
    // A structured value keeps a `.json` marker in its reference so hydration
    // knows to parse it back; Fabro stores every blob as JSON.
    let reference = if is_text {
        blob_ref(&digest)
    } else {
        format!("{}#json", blob_ref(&digest))
    };
    *value = Value::String(reference.clone());
    Some(reference)
}

/// Offload every large value in a stage's context updates.
pub async fn offload_updates(updates: &mut BTreeMap<SmolStr, Value>, store: &dyn BlobStore) {
    for value in updates.values_mut() {
        offload_value(value, store).await;
    }
}

/// The digest and JSON marker a stored reference carries.
fn split_ref(text: &str) -> Option<(&str, bool)> {
    let (body, json) = match text.strip_suffix("#json") {
        Some(body) => (body, true),
        None => (text, false),
    };
    parse_blob_ref(body).map(|digest| (digest, json))
}

/// Hydrate a reference back to its logical value. A value that is not a
/// reference, or one the store cannot find, is returned as it is; the
/// caller then sees the reference text, as Fabro's readers do.
pub async fn hydrate(value: Value, store: &dyn BlobStore) -> Value {
    match value {
        Value::String(text) => {
            let Some((digest, json)) = split_ref(&text) else {
                return Value::String(text);
            };
            match store.get(digest).await {
                Ok(Some(bytes)) if json => {
                    serde_json::from_slice(&bytes).unwrap_or(Value::String(text))
                }
                Ok(Some(bytes)) => Value::String(String::from_utf8_lossy(&bytes).into_owned()),
                Ok(None) => Value::String(text),
                Err(error) => {
                    tracing::warn!(error = %error, "offloaded value could not be read");
                    Value::String(text)
                }
            }
        }
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(Box::pin(hydrate(item, store)).await);
            }
            Value::Array(out)
        }
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (key, item) in map {
                out.insert(key, Box::pin(hydrate(item, store)).await);
            }
            Value::Object(out)
        }
        other => other,
    }
}

/// Whether a value is, or contains, a stored reference.
pub fn holds_ref(value: &Value) -> bool {
    match value {
        Value::String(text) => split_ref(text).is_some(),
        Value::Array(items) => items.iter().any(holds_ref),
        Value::Object(map) => map.values().any(holds_ref),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_test_follows_fabro() {
        assert!(!is_large(&Value::from(7)));
        // A string's JSON form adds its two quotes.
        assert!(!is_large(&Value::String("x".repeat(OFFLOAD_THRESHOLD - 2))));
        assert!(is_large(&Value::String("x".repeat(OFFLOAD_THRESHOLD - 1))));
        assert!(!is_large(&serde_json::json!({ "a": 1 })));
    }

    #[test]
    fn references_parse_and_reject_junk() {
        let digest = "a".repeat(64);
        assert_eq!(parse_blob_ref(&blob_ref(&digest)), Some(digest.as_str()));
        assert_eq!(parse_blob_ref("blob://sha256/short"), None);
        assert_eq!(parse_blob_ref("file:///x"), None);
    }
}
