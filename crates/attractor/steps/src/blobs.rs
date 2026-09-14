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
use std::sync::atomic::{AtomicU64, Ordering};
use std::{io, mem, process};

use ir::Value;
use smol_str::SmolStr;
use tokio::fs;

/// Fabro's offload threshold: a value whose serialized form is larger than
/// this leaves the context for the blob store.
pub const OFFLOAD_THRESHOLD: usize = 100 * 1024;

/// Petri's lower threshold for the values a fan-out multiplies: a fork
/// snapshot value is copied into every branch child's request and records,
/// and the joined `parallel.results` list is copied into every later fork's
/// snapshot. Above this size such a value leaves the context for the store so
/// that the bytes per child do not grow with the item count. The `for_each`
/// source list itself is offloaded at any size: it is O(items) by
/// definition. Readers that show Fabro's view of the context restore these
/// values through [`restore_small`].
pub const FAN_OUT_OFFLOAD_THRESHOLD: usize = 4 * 1024;

const _: () = assert!(FAN_OUT_OFFLOAD_THRESHOLD < OFFLOAD_THRESHOLD);

/// Distinguishes the partial files of concurrent writers in one process.
static PARTIALS: AtomicU64 = AtomicU64::new(0);

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

    /// The size of the bytes behind a digest, or `None` when the store has
    /// none. The default reads the blob; a store that can answer from
    /// metadata overrides it.
    async fn len(&self, digest: &str) -> Result<Option<u64>, BlobError> {
        Ok(self.get(digest).await?.map(|bytes| bytes.len() as u64))
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
        // Write beside, then rename: a reader never sees a torn blob. Two
        // writers of one digest in one process (two forks snapshotting the
        // same value at once) each get their own partial file.
        let partial = self.dir.join(format!(
            "{digest}.partial-{}-{}",
            process::id(),
            PARTIALS.fetch_add(1, Ordering::Relaxed)
        ));
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

    async fn len(&self, digest: &str) -> Result<Option<u64>, BlobError> {
        let path = self.path_of(digest);
        match fs::metadata(&path).await {
            Ok(meta) => Ok(Some(meta.len())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(BlobError::Io {
                action: "stat",
                digest: digest.to_string(),
                path,
                source,
            }),
        }
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
    is_larger_than(value, OFFLOAD_THRESHOLD)
}

/// Fabro's size test against a chosen threshold in bytes of compact JSON.
pub fn is_larger_than(value: &Value, threshold: usize) -> bool {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
        Value::String(text) => {
            if text.len().saturating_mul(6).saturating_add(2) <= threshold {
                return false;
            }
            serde_json::to_vec(value).is_ok_and(|bytes| bytes.len() > threshold)
        }
        Value::Array(_) | Value::Object(_) => {
            serde_json::to_vec(value).is_ok_and(|bytes| bytes.len() > threshold)
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
    offload_above(value, store, OFFLOAD_THRESHOLD).await
}

/// [`offload_value`] against a chosen threshold. A threshold of zero
/// offloads every string, array and object; a scalar never leaves.
pub async fn offload_above(
    value: &mut Value,
    store: &dyn BlobStore,
    threshold: usize,
) -> Option<String> {
    if !is_larger_than(value, threshold) {
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

/// Fabro's view of a context: put back inline every top-level value that is
/// a reference to a blob of at most [`OFFLOAD_THRESHOLD`] bytes. Fabro never
/// offloads a value that small, so such a reference is Petri's own (a fork
/// snapshot value, a `for_each` source list, a joined result list), and a
/// reader that renders the context as Fabro would (an agent's preamble, a
/// nested workflow's starting context) sees the value Fabro's reader sees.
/// A reference to a larger blob stays a reference, as it is in Fabro. A
/// value the store cannot find or read stays as it is.
pub async fn restore_small<'a>(values: impl Iterator<Item = &'a mut Value>, store: &dyn BlobStore) {
    for value in values {
        let Value::String(text) = &*value else {
            continue;
        };
        let Some((digest, _)) = split_ref(text) else {
            continue;
        };
        let small = match store.len(digest).await {
            Ok(Some(len)) => usize::try_from(len).is_ok_and(|len| len <= OFFLOAD_THRESHOLD),
            Ok(None) => false,
            Err(error) => {
                tracing::warn!(error = %error, "offloaded value could not be sized");
                false
            }
        };
        if small {
            let taken = mem::take(value);
            *value = hydrate(taken, store).await;
        }
    }
}

/// What an agent or prompt step renders its preamble from, as Fabro's would
/// see it: the small references in `kv` restored ([`restore_small`]), and the
/// fork-time stage records hydrated when the fork offloaded them whole (a
/// Petri-only value, so its size does not matter). References inside a
/// stage record stay references, as the preamble names them.
pub async fn restore_fabro_view(kv: &mut Value, nodes: &mut Value, store: &dyn BlobStore) {
    if let Value::Object(map) = kv {
        restore_small(map.values_mut(), store).await;
    }
    if nodes.is_string() {
        let taken = mem::take(nodes);
        *nodes = hydrate(taken, store).await;
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
    use std::{env, fs as std_fs};

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

    #[test]
    fn the_fan_out_size_test_follows_fabros_measure() {
        let list: Value = (0..300)
            .map(|i| serde_json::json!({ "name": format!("job-{i}") }))
            .collect();
        assert!(is_larger_than(&list, FAN_OUT_OFFLOAD_THRESHOLD));
        assert!(!is_large(&list));
        assert!(is_larger_than(&serde_json::json!([]), 0));
        assert!(!is_larger_than(&Value::from(7), 0));
    }

    #[tokio::test]
    async fn a_small_reference_is_restored_and_a_large_one_is_kept() {
        let dir = env::temp_dir().join(format!("petri-blobs-restore-{}", process::id()));
        let _ = std_fs::remove_dir_all(&dir);
        let store = LocalBlobStore::new(&dir);
        let list: Value = (0..300)
            .map(|i| serde_json::json!({ "name": format!("job-{i}") }))
            .collect();
        let mut small = list.clone();
        let reference = offload_above(&mut small, &store, 0)
            .await
            .expect("offloaded");
        assert!(reference.ends_with("#json"));
        let mut big = Value::String("x".repeat(OFFLOAD_THRESHOLD + 1));
        let big_ref = offload_value(&mut big, &store).await.expect("offloaded");
        let mut scalar = Value::from(3);
        assert_eq!(offload_above(&mut scalar, &store, 0).await, None);
        let mut kv = serde_json::Map::new();
        kv.insert("jobs".into(), small.clone());
        kv.insert("big".into(), big.clone());
        kv.insert("n".into(), scalar);
        restore_small(kv.values_mut(), &store).await;
        assert_eq!(kv["jobs"], list, "the fork-sized value is back inline");
        assert_eq!(
            kv["big"],
            Value::String(big_ref),
            "Fabro's own offload stays"
        );
        assert_eq!(kv["n"], Value::from(3));
        let (digest, json) = split_ref(&reference).expect("a structured reference");
        assert!(json);
        assert_eq!(
            store.len(digest).await.expect("stat"),
            Some(serde_json::to_vec(&list).expect("json").len() as u64),
            "the store sizes a blob without reading it"
        );
        let _ = std_fs::remove_dir_all(&dir);
    }
}
