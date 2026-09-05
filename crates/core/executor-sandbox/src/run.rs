//! The run directory: the run id minted once under it, the labels every
//! sandbox of the run carries, and the per-scope layout the host backend
//! uses.

use std::fs::{self, File};
use std::io::{self, ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::process;

use executor::{EnvError, SandboxLeaseId};
use smol_str::SmolStr;
use tokio::sync::OnceCell;
use tokio::task::spawn_blocking;

/// The file under the run dir holding the run id the labels carry; minted
/// once, read back by any executor over the same run dir.
pub const RUN_ID_FILE: &str = "sandbox-run-id";

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

/// A run's identity on the provider: the run id, read or minted once from
/// the run dir, and every label and name derived from it. The executors
/// over one run dir share one, so the reconcile key can never disagree.
pub struct RunIdentity {
    run_dir: PathBuf,
    run_id:  OnceCell<SmolStr>,
}

impl RunIdentity {
    pub fn new(run_dir: PathBuf) -> Self {
        Self {
            run_dir,
            run_id: OnceCell::new(),
        }
    }

    pub(crate) fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    /// The run id, resolved on first use from the run dir so a resuming
    /// executor reaches the crashed run's sandboxes.
    pub async fn run_id(&self) -> Result<SmolStr, EnvError> {
        self.run_id
            .get_or_try_init(|| load_or_record_run_id(&self.run_dir))
            .await
            .cloned()
    }

    /// `petri-<run id>-`: what the name of every sandbox this run owns
    /// starts with, for an operator's `docker ps` and a leak check.
    pub async fn container_prefix(&self) -> Result<String, EnvError> {
        Ok(format!("petri-{}-", self.run_id().await?))
    }

    /// The workspace label value for `workspace_id`.
    pub async fn workspace_label(&self, workspace_id: &str) -> Result<String, EnvError> {
        Ok(format!("{}/{workspace_id}", self.run_id().await?))
    }

    /// The sandbox name for a lease.
    pub async fn container_name(&self, lease: SandboxLeaseId) -> Result<String, EnvError> {
        Ok(format!(
            "{}l{}",
            self.container_prefix().await?,
            lease.raw()
        ))
    }

    /// Every label a sandbox of this run carries.
    pub async fn labels(
        &self,
        lease: SandboxLeaseId,
        workspace_id: &str,
    ) -> Result<Vec<(String, String)>, EnvError> {
        let run_id = self.run_id().await?;
        Ok(vec![
            (RUN_LABEL.to_owned(), run_id.to_string()),
            (LEASE_LABEL.to_owned(), lease.raw().to_string()),
            (
                WORKSPACE_LABEL.to_owned(),
                format!("{run_id}/{workspace_id}"),
            ),
        ])
    }
}

/// Reads the run id from the run dir, minting and recording it on first use.
/// Publication never replaces an established id, even if a cancelled
/// initializer's blocking write finishes after its replacement.
async fn load_or_record_run_id(run_dir: &Path) -> Result<SmolStr, EnvError> {
    let path = run_dir.join(RUN_ID_FILE);
    spawn_blocking(move || match read_run_id(&path) {
        Ok(recorded) => {
            // Another publisher may have linked the record but not yet synced
            // its directory. Every successful reader establishes durability.
            sync_parent(&path)?;
            Ok(recorded)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => publish_run_id(&path, &fresh_run_id()),
        Err(error) => Err(record_error("read", &path, error)),
    })
    .await
    .map_err(|error| EnvError::backend(crate::BACKEND, "record", error.to_string()))?
}

fn read_run_id(path: &Path) -> io::Result<SmolStr> {
    let recorded = fs::read_to_string(path)?;
    if recorded.trim().is_empty() {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "the recorded sandbox run id is empty",
        ));
    }
    Ok(SmolStr::new(recorded.trim()))
}

fn publish_run_id(path: &Path, minted: &str) -> Result<SmolStr, EnvError> {
    let staged = StagedRecord::new(path, minted.as_bytes())?;
    // The staged file shares the destination's filesystem. Linking makes the
    // complete, synced inode visible only if no publisher has won already.
    match fs::hard_link(&staged.0, path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => return Err(record_error("publish", path, error)),
    }
    drop(staged);
    sync_parent(path)?;
    read_run_id(path).map_err(|error| record_error("read", path, error))
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
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    use testkit::RunDir;
    use tokio::sync::oneshot;
    use tokio::task::JoinSet;
    use tokio::time::timeout;

    use super::*;

    #[tokio::test]
    async fn concurrent_executors_share_one_durable_run_id() {
        let dir = RunDir::new("concurrent-run-id");
        let mut tasks = JoinSet::new();
        for _ in 0..16 {
            let identity = RunIdentity::new(dir.path().to_path_buf());
            tasks.spawn(async move { identity.run_id().await });
        }
        let mut ids = Vec::new();
        while let Some(joined) = tasks.join_next().await {
            ids.push(
                joined
                    .expect("initializer joined")
                    .expect("run id recorded"),
            );
        }
        let recorded = read_run_id(&dir.path().join(RUN_ID_FILE)).expect("durable run id");
        assert!(ids.iter().all(|id| id == &recorded));
        assert_eq!(fs::read_dir(dir.path()).expect("run directory").count(), 1);
    }

    #[tokio::test]
    async fn a_cancelled_initializer_cannot_replace_the_winning_run_id() {
        let dir = RunDir::new("cancelled-run-id-publisher");
        let identity = Arc::new(RunIdentity::new(dir.path().to_path_buf()));
        let path = dir.path().join(RUN_ID_FILE);
        let (started, start) = oneshot::channel();
        let (resume, gate) = mpsc::channel();
        let (published, publish) = oneshot::channel();
        let cancelled_identity = identity.clone();
        let cancelled_path = path.clone();
        let initializer = tokio::spawn(async move {
            cancelled_identity
                .run_id
                .get_or_try_init(|| async move {
                    spawn_blocking(move || {
                        // Pause after minting, before publication, just as a
                        // queued blocking initializer can outlive its caller.
                        let minted = fresh_run_id();
                        let _ = started.send(());
                        gate.recv_timeout(Duration::from_secs(10))
                            .expect("resume the cancelled publisher");
                        let result = publish_run_id(&cancelled_path, &minted);
                        let _ =
                            published.send(result.as_ref().cloned().map_err(ToString::to_string));
                        result
                    })
                    .await
                    .expect("blocking initializer joined")
                })
                .await
                .cloned()
        });
        timeout(Duration::from_secs(10), start)
            .await
            .expect("initializer started")
            .expect("initializer minted its candidate");
        initializer.abort();
        assert!(matches!(initializer.await, Err(error) if error.is_cancelled()));

        let winner = identity
            .run_id()
            .await
            .expect("replacement initializer won");
        resume.send(()).expect("resume the late publisher");
        let late = timeout(Duration::from_secs(10), publish)
            .await
            .expect("late publication completed")
            .expect("late publisher reported")
            .expect("late publisher reads the winner");
        assert_eq!(late, winner);
        assert_eq!(read_run_id(&path).expect("durable run id"), winner);
        assert_eq!(identity.run_id().await.expect("cached run id"), winner);
    }

    #[tokio::test]
    async fn an_empty_run_id_is_rejected_without_replacing_it() {
        let dir = RunDir::new("empty-run-id");
        fs::create_dir_all(dir.path()).expect("run directory");
        let path = dir.path().join(RUN_ID_FILE);
        fs::write(&path, " \n").expect("empty record");
        let identity = RunIdentity::new(dir.path().to_path_buf());
        assert!(matches!(
            identity.run_id().await,
            Err(EnvError::Workspace { source, .. }) if source.kind() == ErrorKind::InvalidData
        ));
        assert_eq!(fs::read_to_string(path).expect("original record"), " \n");
    }

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
