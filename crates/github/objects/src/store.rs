//! The run-scoped artifact store: content files plus one index.
//!
//! Layout under the store directory (the host puts it beside the run dir, so
//! artifacts share the run's retention):
//!
//! ```text
//! index.json          the finalized artifacts and the id high-water mark
//! <id>.zip            one artifact's content, exactly as uploaded
//! staging/<id>/…      blocks of an upload in flight
//! ```
//!
//! Artifacts are stored by numeric id, never by name: names are workflow input
//! and belong in the index, not in file paths. The index is rewritten whole on
//! every mutation, temp-then-rename, so a resumed run reopens a consistent
//! store and an upload that never finalized leaves only staging debris.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;
use std::{fs, io};

use serde::{Deserialize, Serialize};

use crate::index;

/// One finalized artifact, as the index records it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Artifact {
    pub id:         i64,
    pub name:       String,
    pub size:       u64,
    /// RFC 3339 — the JSON form of a protobuf `Timestamp`.
    pub created_at: String,
    /// The client's content hash (`sha256:<hex>`), echoed back on list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest:     Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Index {
    next_id:   i64,
    artifacts: Vec<Artifact>,
}

struct State {
    index:   Index,
    /// Uploads begun and not yet finalized, by name. In memory only: a crash
    /// mid-upload leaves staging files, and the client retries from create.
    pending: BTreeMap<String, i64>,
}

pub(crate) struct ArtifactStore {
    dir:   PathBuf,
    state: Mutex<State>,
}

impl ArtifactStore {
    /// Open (or create) the store under `dir`.
    pub(crate) fn open(dir: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&dir)?;
        let index: Index = index::read(&dir, "artifact")?;
        Ok(Self {
            dir,
            state: Mutex::new(State {
                index,
                pending: BTreeMap::new(),
            }),
        })
    }

    /// Begin an upload: allocate the artifact's id. The id high-water mark is
    /// persisted now, so a resumed run never re-issues an id.
    pub(crate) fn begin(&self, name: &str) -> io::Result<i64> {
        let mut state = self
            .state
            .lock()
            .expect("the artifact store lock is not poisoned");
        state.index.next_id += 1;
        let id = state.index.next_id;
        state.pending.insert(name.to_string(), id);
        self.persist(&state.index)?;
        Ok(id)
    }

    /// Finalize `name`: the committed content becomes the artifact. `None`
    /// when no upload for that name was begun or no content was committed.
    /// An earlier artifact with the same name is replaced, content and all.
    pub(crate) fn finalize(
        &self,
        name: &str,
        digest: Option<String>,
    ) -> io::Result<Option<Artifact>> {
        let mut state = self
            .state
            .lock()
            .expect("the artifact store lock is not poisoned");
        let Some(id) = state.pending.remove(name) else {
            return Ok(None);
        };
        let size = match fs::metadata(self.content_path(id)) {
            Ok(meta) => meta.len(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let artifact = Artifact {
            id,
            name: name.to_string(),
            size,
            created_at: humantime::format_rfc3339_seconds(SystemTime::now()).to_string(),
            digest,
        };
        if let Some(previous) = state.index.artifacts.iter().position(|a| a.name == name) {
            let old = state.index.artifacts.remove(previous);
            let _ = fs::remove_file(self.content_path(old.id));
        }
        state.index.artifacts.push(artifact.clone());
        self.persist(&state.index)?;
        Ok(Some(artifact))
    }

    /// The finalized artifacts, oldest first.
    pub(crate) fn list(&self) -> Vec<Artifact> {
        self.state
            .lock()
            .expect("the artifact store lock is not poisoned")
            .index
            .artifacts
            .clone()
    }

    pub(crate) fn find(&self, name: &str) -> Option<Artifact> {
        self.state
            .lock()
            .expect("the artifact store lock is not poisoned")
            .index
            .artifacts
            .iter()
            .find(|a| a.name == name)
            .cloned()
    }

    /// Remove `name`, content and index entry both.
    pub(crate) fn delete(&self, name: &str) -> io::Result<Option<i64>> {
        let mut state = self
            .state
            .lock()
            .expect("the artifact store lock is not poisoned");
        let Some(position) = state.index.artifacts.iter().position(|a| a.name == name) else {
            return Ok(None);
        };
        let artifact = state.index.artifacts.remove(position);
        let _ = fs::remove_file(self.content_path(artifact.id));
        self.persist(&state.index)?;
        Ok(Some(artifact.id))
    }

    /// Where an artifact's content lives.
    pub(crate) fn content_path(&self, id: i64) -> PathBuf {
        self.dir.join(format!("{id}.zip"))
    }

    /// Where an in-flight upload's blocks stage.
    pub(crate) fn staging_dir(&self, id: i64) -> PathBuf {
        self.dir.join("staging").join(id.to_string())
    }

    fn persist(&self, index: &Index) -> io::Result<()> {
        index::write(&self.dir, index)
    }
}

#[cfg(test)]
mod tests {
    use std::{env, fs, process};

    use super::*;

    fn scratch(label: &str) -> PathBuf {
        let dir = env::temp_dir()
            .join("petri-objects-store")
            .join(format!("{label}-{}", process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_lifecycle_round_trips_and_survives_reopen() {
        let dir = scratch("lifecycle");
        let store = ArtifactStore::open(dir.clone()).expect("open");
        let id = store.begin("dist").expect("begin");
        fs::write(store.content_path(id), b"zip bytes").expect("content");
        let artifact = store
            .finalize("dist", Some("sha256:abc".into()))
            .expect("finalize")
            .expect("an upload was begun");
        assert_eq!(artifact.size, 9);

        // A fresh listener over the same dir — the resume case.
        drop(store);
        let store = ArtifactStore::open(dir.clone()).expect("reopen");
        assert_eq!(store.list().len(), 1);
        assert_eq!(store.find("dist").expect("found").id, id);
        // Ids never repeat, even for a name that was deleted.
        assert!(store.begin("dist").expect("begin") > id);

        store.delete("dist").expect("delete");
        assert!(store.find("dist").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn finalize_without_content_or_begin_is_none() {
        let dir = scratch("empty");
        let store = ArtifactStore::open(dir.clone()).expect("open");
        assert!(store.finalize("ghost", None).expect("io ok").is_none());
        store.begin("ghost").expect("begin");
        // Begun, but nothing committed.
        assert!(store.finalize("ghost", None).expect("io ok").is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
