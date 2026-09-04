//! The run directory: the run id minted once under it, the labels every
//! sandbox of the run carries, and the per-scope layout the host backend
//! uses.

use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::process;

use executor::{EnvError, SandboxLeaseId};
use smol_str::SmolStr;
use tokio::fs;
use tokio::sync::OnceCell;

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
    pub(crate) fn new(run_dir: PathBuf) -> Self {
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
/// Write-then-rename, so a reader sees the whole id or none.
async fn load_or_record_run_id(run_dir: &Path) -> Result<SmolStr, EnvError> {
    let path = run_dir.join(RUN_ID_FILE);
    let io_error =
        |action, path: &Path, error: io::Error| EnvError::workspace(action, path.display(), error);
    match fs::read_to_string(&path).await {
        Ok(recorded) if !recorded.trim().is_empty() => {
            return Ok(SmolStr::new(recorded.trim()));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(io_error("read", &path, error)),
    }
    let minted = fresh_run_id();
    fs::create_dir_all(run_dir)
        .await
        .map_err(|error| io_error("create", run_dir, error))?;
    let staged = run_dir.join(format!("{RUN_ID_FILE}.tmp"));
    fs::write(&staged, minted.as_bytes())
        .await
        .map_err(|error| io_error("write", &staged, error))?;
    fs::rename(&staged, &path)
        .await
        .map_err(|error| io_error("rename", &path, error))?;
    Ok(SmolStr::new(minted))
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
