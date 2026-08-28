//! One `index.json` convention for both stores: read-or-default on the way
//! in, whole-file temp-then-rename on the way out — the crash-safety story
//! lives here, once.

use std::io;
use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;

const INDEX_FILE: &str = "index.json";

/// Read `dir/index.json`; a store that has never written one is empty, not
/// broken. `what` names the store in the corrupt-index error.
pub(crate) fn read<T: Default + DeserializeOwned>(dir: &Path, what: &str) -> io::Result<T> {
    match std::fs::read(dir.join(INDEX_FILE)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| io::Error::other(format!("the {what} index is corrupt: {e}"))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e),
    }
}

/// Rewrite `dir/index.json` whole, temp-then-rename, so a reader (or a
/// resumed run) never sees a half-written index.
pub(crate) fn write<T: Serialize>(dir: &Path, index: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(index).expect("the index encodes");
    let temp = dir.join(format!("{INDEX_FILE}.tmp"));
    std::fs::write(&temp, bytes)?;
    std::fs::rename(&temp, dir.join(INDEX_FILE))
}
