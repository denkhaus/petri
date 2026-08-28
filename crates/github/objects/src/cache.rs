//! The host-scoped cache store: cross-run, keyed the way the toolkit keys.
//!
//! Layout under the store's `cache/` root:
//!
//! ```text
//! index.json      every finalized entry: key, version, blob id, size, stamps
//! blobs/<id>      one entry's content (the toolkit's compressed archive)
//! staging/<id>/…  blocks of an upload in flight
//! ```
//!
//! Semantics follow GitHub where they matter and diverge where the plan says
//! so: an entry is `(key, version)` and immutable — a second reserve of the
//! same pair is refused, as GitHub refuses it; lookups match the exact key
//! first, then each restore key as a prefix, newest entry first, always within
//! the same `version` (the toolkit's hash of paths and compression, which is
//! what makes archives interchangeable). GitHub's branch scoping is
//! deliberately ignored: one machine, one store — documented in SUPPORT.md.
//!
//! Writes are temp-then-rename; the index is rewritten whole per mutation. The
//! store is shared by *processes*, not just tasks, and takes the simple
//! stance: last index writer wins, and a sweep prunes anything the index no
//! longer names. Finalize prunes to the size budget, oldest-used first.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The default size budget: 10 GiB, pruned LRU on write.
pub const DEFAULT_BUDGET: u64 = 10 * 1024 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CacheEntry {
    pub key: String,
    pub version: String,
    /// The blob's file name: hex of a digest over `(key, version)`.
    pub id: String,
    pub size: u64,
    /// Unix seconds; `created` orders "newest wins", `used` orders the LRU.
    pub created: u64,
    pub used: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct Index {
    next_entry: i64,
    entries: Vec<CacheEntry>,
}

pub struct CacheStore {
    dir: PathBuf,
    budget: u64,
    /// Serializes this process's read-modify-write of the index.
    lock: Mutex<()>,
}

impl CacheStore {
    /// Open (or create) the store under `dir`, pruning to `budget` on writes.
    pub fn open(dir: PathBuf, budget: u64) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir.join("blobs"))?;
        Ok(Self {
            dir,
            budget,
            lock: Mutex::new(()),
        })
    }

    /// The blob id for a `(key, version)` pair.
    pub fn blob_id(key: &str, version: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(key.as_bytes());
        hasher.update([0]);
        hasher.update(version.as_bytes());
        crate::token::hex(&hasher.finalize())
    }

    /// Reserve an upload. `None` when the entry already exists — immutable, as
    /// on GitHub — else the blob id the signed upload URL names.
    pub fn reserve(&self, key: &str, version: &str) -> std::io::Result<Option<String>> {
        let _guard = self.lock.lock().expect("the store lock");
        let index = self.read_index()?;
        if index
            .entries
            .iter()
            .any(|e| e.key == key && e.version == version)
        {
            return Ok(None);
        }
        Ok(Some(Self::blob_id(key, version)))
    }

    /// Record the committed upload as a live entry and prune to budget.
    /// `None` when nothing was committed for the pair.
    pub fn finalize(&self, key: &str, version: &str) -> std::io::Result<Option<i64>> {
        let id = Self::blob_id(key, version);
        let size = match std::fs::metadata(self.blob_path(&id)) {
            Ok(meta) => meta.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let _guard = self.lock.lock().expect("the store lock");
        let mut index = self.read_index()?;
        index.next_entry += 1;
        let entry_id = index.next_entry;
        let now = unix_now();
        index
            .entries
            .retain(|e| !(e.key == key && e.version == version));
        index.entries.push(CacheEntry {
            key: key.to_string(),
            version: version.to_string(),
            id,
            size,
            created: now,
            used: now,
        });
        self.prune(&mut index);
        self.write_index(&index)?;
        Ok(Some(entry_id))
    }

    /// The toolkit's lookup: exact key, then each restore key as a prefix,
    /// newest first — always within `version`. A hit stamps the LRU.
    pub fn lookup(
        &self,
        key: &str,
        restore_keys: &[String],
        version: &str,
    ) -> std::io::Result<Option<CacheEntry>> {
        let _guard = self.lock.lock().expect("the store lock");
        let mut index = self.read_index()?;
        let found = {
            let same_version: Vec<&CacheEntry> = index
                .entries
                .iter()
                .filter(|e| e.version == version)
                .collect();
            let exact = same_version.iter().find(|e| e.key == key).copied();
            let by_prefix = || {
                restore_keys.iter().find_map(|prefix| {
                    same_version
                        .iter()
                        .filter(|e| e.key.starts_with(prefix.as_str()))
                        .max_by_key(|e| e.created)
                        .copied()
                })
            };
            exact.or_else(by_prefix).cloned()
        };
        let Some(found) = found else { return Ok(None) };
        // The stamp only orders the LRU, so a repeat hit within the minute
        // doesn't earn a whole-index rewrite.
        let now = unix_now();
        if now.saturating_sub(found.used) >= 60 {
            if let Some(entry) = index.entries.iter_mut().find(|e| e.id == found.id) {
                entry.used = now;
            }
            self.write_index(&index)?;
        }
        Ok(Some(found))
    }

    /// Where an entry's content lives.
    pub fn blob_path(&self, id: &str) -> PathBuf {
        self.dir.join("blobs").join(id)
    }

    /// Where an in-flight upload's blocks stage.
    pub fn staging_dir(&self, id: &str) -> PathBuf {
        self.dir.join("staging").join(id)
    }

    /// Drop oldest-used entries until the live set fits the budget, and remove
    /// any blob the index no longer names (an orphan from a lost index race or
    /// a pruned entry).
    fn prune(&self, index: &mut Index) {
        let mut total: u64 = index.entries.iter().map(|e| e.size).sum();
        if total > self.budget {
            let mut by_age = index.entries.clone();
            by_age.sort_by_key(|e| e.used);
            let mut dropped: Vec<String> = Vec::new();
            for entry in by_age {
                if total <= self.budget {
                    break;
                }
                total -= entry.size;
                dropped.push(entry.id);
            }
            index.entries.retain(|e| !dropped.contains(&e.id));
        }
        if let Ok(blobs) = std::fs::read_dir(self.dir.join("blobs")) {
            for blob in blobs.flatten() {
                let name = blob.file_name().to_string_lossy().into_owned();
                if !index.entries.iter().any(|e| e.id == name) {
                    let _ = std::fs::remove_file(blob.path());
                }
            }
        }
    }

    fn read_index(&self) -> std::io::Result<Index> {
        crate::index::read(&self.dir, "cache")
    }

    fn write_index(&self, index: &Index) -> std::io::Result<()> {
        crate::index::write(&self.dir, index)
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(label: &str, budget: u64) -> CacheStore {
        let dir = std::env::temp_dir()
            .join("petri-cache-store")
            .join(format!("{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        CacheStore::open(dir, budget).expect("open")
    }

    fn put(store: &CacheStore, key: &str, version: &str, bytes: &[u8]) {
        let id = store.reserve(key, version).expect("io").expect("fresh");
        std::fs::write(store.blob_path(&id), bytes).expect("blob");
        store.finalize(key, version).expect("io").expect("entry");
    }

    #[test]
    fn exact_then_prefix_newest_wins_within_version() {
        let store = store("lookup", DEFAULT_BUDGET);
        put(&store, "deps-linux-abc", "v1", b"old");
        std::thread::sleep(std::time::Duration::from_millis(1100));
        put(&store, "deps-linux-def", "v1", b"new");
        put(&store, "deps-linux-def", "v2", b"other-version");

        let exact = store
            .lookup("deps-linux-abc", &[], "v1")
            .expect("io")
            .expect("hit");
        assert_eq!(exact.key, "deps-linux-abc");

        let prefixed = store
            .lookup("deps-linux-zzz", &["deps-linux-".to_string()], "v1")
            .expect("io")
            .expect("hit");
        assert_eq!(prefixed.key, "deps-linux-def", "newest of the prefix");

        assert!(
            store
                .lookup("deps-linux-abc", &[], "v9")
                .expect("io")
                .is_none(),
            "a version never crosses"
        );
    }

    #[test]
    fn entries_are_immutable_and_the_budget_prunes_lru() {
        let store = store("budget", 10);
        put(&store, "a", "v1", b"aaaaaa");
        assert!(
            store.reserve("a", "v1").expect("io").is_none(),
            "immutable, as on GitHub"
        );
        std::thread::sleep(std::time::Duration::from_millis(1100));
        put(&store, "b", "v1", b"bbbbbb");
        // Budget 10 holds one six-byte entry, not two: `a` (older) was pruned,
        // its blob with it.
        assert!(store.lookup("a", &[], "v1").expect("io").is_none());
        let b = store.lookup("b", &[], "v1").expect("io").expect("kept");
        assert!(store.blob_path(&b.id).exists());
    }
}
