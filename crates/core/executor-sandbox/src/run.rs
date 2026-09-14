//! The run directory and the run's identity on its providers: the run key
//! every label carries, and the per-scope layout the host backend uses.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process;

use executor::{EnvError, SandboxLeaseId};
use sha2::{Digest as _, Sha256};
use smol_str::SmolStr;
use tokio::task::spawn_blocking;

/// The label naming a sandbox's workspace: `<run id>/<workspace id>`. It
/// is the reconcile key: recovery lists the provider by it and attaches the
/// one match.
pub const WORKSPACE_LABEL: &str = "petri.workspace";
/// The label naming the run a sandbox belongs to, for prune.
pub const RUN_LABEL: &str = "petri.run";
/// The label naming the lease a sandbox belongs to.
pub const LEASE_LABEL: &str = "petri.lease";

/// The directory a host scope's state lives under: `scopes/<id>`.
pub(crate) fn scope_dir(run_dir: &Path, id: &str) -> PathBuf {
    run_dir.join("scopes").join(id)
}

/// The workspace of `workspace_id` for the host backend.
pub(crate) fn workspace_dir(run_dir: &Path, workspace_id: &str) -> PathBuf {
    scope_dir(run_dir, workspace_id).join("work")
}

/// A run's identity on the provider: the run id every label and name
/// derives from, and the run directory host scopes live under. The run id
/// is the run's store key; the coordinator hands it down so the reconcile
/// key can never disagree between a run and its resume. A driver with no
/// coordinator names its run after its run directory
/// ([`RunIdentity::for_run_dir`]), so a fresh executor over the same
/// directory still reaches what an earlier one left.
pub struct RunIdentity {
    run_dir: PathBuf,
    run_id:  SmolStr,
}

impl RunIdentity {
    pub fn new(run_dir: PathBuf, run_id: impl Into<SmolStr>) -> Self {
        Self {
            run_dir,
            run_id: run_id.into(),
        }
    }

    /// The identity a driver with no coordinator uses: a run id derived
    /// from the run directory's path, the same for every executor over that
    /// directory, in this process or the next.
    pub fn for_run_dir(run_dir: PathBuf) -> Self {
        let canonical = fs::canonicalize(&run_dir).unwrap_or_else(|_| run_dir.clone());
        let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
        let mut id = String::with_capacity(16);
        for byte in &digest[..8] {
            let _ = write!(id, "{byte:02x}");
        }
        Self::new(run_dir, id)
    }

    pub(crate) fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    /// The run id the labels carry.
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// `petri-<run id>-`: what the name of every sandbox this run owns
    /// starts with, for an operator's `docker ps` and a leak check.
    pub fn container_prefix(&self) -> String {
        format!("petri-{}-", self.run_id)
    }

    /// The workspace label value for `workspace_id`.
    pub fn workspace_label(&self, workspace_id: &str) -> String {
        format!("{}/{workspace_id}", self.run_id)
    }

    /// The sandbox name for a lease.
    pub fn container_name(&self, lease: SandboxLeaseId) -> String {
        format!("{}l{}", self.container_prefix(), lease.raw())
    }

    /// Every label a sandbox of this run carries.
    pub fn labels(&self, lease: SandboxLeaseId, workspace_id: &str) -> Vec<(String, String)> {
        vec![
            (RUN_LABEL.to_owned(), self.run_id.to_string()),
            (LEASE_LABEL.to_owned(), lease.raw().to_string()),
            (
                WORKSPACE_LABEL.to_owned(),
                self.workspace_label(workspace_id),
            ),
        ]
    }
}

/// Persist recovery metadata before creating a provider resource. A reader
/// sees a complete record, and both its content and directory entry survive
/// a crash after this returns.
pub(crate) async fn write_record(path: PathBuf, bytes: Vec<u8>) -> Result<(), EnvError> {
    spawn_blocking(move || {
        let staged = StagedRecord::new(&path, &bytes)?;
        fs::rename(&staged.0, &path).map_err(|error| record_error("rename", &path, error))?;
        sync_parent(&path)
    })
    .await
    .map_err(|error| EnvError::backend(crate::BACKEND, "record", error.to_string()))?
}

struct StagedRecord(PathBuf);

impl StagedRecord {
    fn new(path: &Path, bytes: &[u8]) -> Result<Self, EnvError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| record_error("create", parent, error))?;
        }
        let staged = path.with_extension(format!("{}.tmp", fresh_run_id()));
        let mut file =
            File::create_new(&staged).map_err(|error| record_error("create", &staged, error))?;
        let staged = Self(staged);
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| record_error("write", &staged.0, error))?;
        Ok(staged)
    }
}

impl Drop for StagedRecord {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn sync_parent(path: &Path) -> Result<(), EnvError> {
    if let Some(parent) = path.parent() {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| record_error("sync", parent, error))?;
    }
    Ok(())
}

fn record_error(action: &'static str, path: &Path, error: io::Error) -> EnvError {
    EnvError::workspace(action, path.display(), error)
}

/// Unique across processes and time.
fn fresh_run_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "{nanos:x}-{}-{}",
        process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use testkit::RunDir;
    use tokio::task::JoinSet;

    use super::*;

    #[tokio::test]
    async fn concurrent_marker_writes_publish_complete_records() {
        let dir = RunDir::new("concurrent-marker-writes");
        let path = dir.path().join("marker");
        let mut tasks = JoinSet::new();
        for index in 0u8..16 {
            tasks.spawn(write_record(path.clone(), vec![index; 16 * 1024]));
        }
        while let Some(joined) = tasks.join_next().await {
            joined
                .expect("writer joined")
                .expect("complete record published");
        }
        let recorded = fs::read(path).expect("published marker");
        assert_eq!(recorded.len(), 16 * 1024);
        assert!(recorded.iter().all(|byte| *byte == recorded[0]));
        assert_eq!(fs::read_dir(dir.path()).expect("run directory").count(), 1);
    }
}
