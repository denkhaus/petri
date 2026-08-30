//! One `index.json` convention for both stores: read-or-default on the way
//! in, whole-file temp-then-rename on the way out — the crash-safety story
//! lives here, once.

use std::error::Error;
use std::path::Path;
use std::{fmt, fs, io};

use serde::Serialize;
use serde::de::DeserializeOwned;

const INDEX_FILE: &str = "index.json";

/// A store's `index.json` did not parse. The parse failure stays reachable as
/// the source, so a caller that renders the chain sees both the store and the
/// offending byte.
#[derive(Debug)]
struct Corrupt {
    what:   &'static str,
    source: serde_json::Error,
}

impl fmt::Display for Corrupt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the {} index is corrupt", self.what)
    }
}

impl Error for Corrupt {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// Read `dir/index.json`; a store that has never written one is empty, not
/// broken. `what` names the store in the corrupt-index error.
pub(crate) fn read<T: Default + DeserializeOwned>(dir: &Path, what: &'static str) -> io::Result<T> {
    match fs::read(dir.join(INDEX_FILE)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|source| io::Error::new(io::ErrorKind::InvalidData, Corrupt { what, source })),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e),
    }
}

/// Rewrite `dir/index.json` whole, temp-then-rename, so a reader (or a
/// resumed run) never sees a half-written index.
pub(crate) fn write<T: Serialize>(dir: &Path, index: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(index).expect("the index encodes");
    let temp = dir.join(format!("{INDEX_FILE}.tmp"));
    fs::write(&temp, bytes)?;
    fs::rename(&temp, dir.join(INDEX_FILE))
}
