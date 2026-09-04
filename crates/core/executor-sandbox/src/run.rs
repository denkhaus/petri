//! The run directory: the run id minted once under it, every container name
//! derived from that id, and the per-scope layout the backends share.

use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::process;

use executor::EnvError;
use smol_str::SmolStr;
use tokio::fs;
use tokio::sync::OnceCell;

use crate::oneshot::ContainerPrefix;

/// The file under the run dir holding the run id the environment label
/// carries; minted once, read back by any executor over the same run dir.
pub const RUN_ID_FILE: &str = "sandbox-run-id";

/// The directory a scope's state lives under: `scopes/<id>`. A workspace is
/// keyed by its workspace id, which two environments may share (a nested
/// invocation inheriting its parent's sandbox); everything else a scope
/// leaves behind is keyed by its environment id.
pub(crate) fn scope_dir(run_dir: &Path, id: &str) -> PathBuf {
    run_dir.join("scopes").join(id)
}

/// The workspace of `workspace_id`: a host scope's working directory, and a
/// container scope's bind-mount source.
pub(crate) fn workspace_dir(run_dir: &Path, workspace_id: &str) -> PathBuf {
    scope_dir(run_dir, workspace_id).join("work")
}

/// A run's identity on the daemon: the run id, read or minted once from the
/// run dir, and every container name derived from it. The executors over one
/// run dir share one, so the fence key, the job container name, and the
/// one-shot sweep prefix can never disagree.
pub(crate) struct RunIdentity {
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
    /// executor reaches the crashed run's containers.
    pub(crate) async fn run_id(&self) -> Result<SmolStr, EnvError> {
        self.run_id
            .get_or_try_init(|| load_or_record_run_id(&self.run_dir))
            .await
            .cloned()
    }

    /// `petri-<run id>-`: what the name of every container this run owns
    /// starts with, for a leak check after release.
    pub(crate) async fn container_prefix(&self) -> Result<String, EnvError> {
        Ok(format!("petri-{}-", self.run_id().await?))
    }

    /// The environment label value for `instance`: the run id and the scope's
    /// environment id, globally unique because the run id is minted once per
    /// run directory.
    pub(crate) async fn environment_label(&self, instance: &str) -> Result<String, EnvError> {
        Ok(format!("{}/{instance}", self.run_id().await?))
    }

    /// The job container name for `instance`.
    pub(crate) async fn container_name(&self, instance: &str) -> Result<String, EnvError> {
        Ok(container_name(&self.environment_label(instance).await?))
    }

    /// The prefix `instance`'s one-shot action containers are named under:
    /// the job container's name plus `-s`.
    pub(crate) async fn one_shot_prefix(
        &self,
        instance: &str,
    ) -> Result<ContainerPrefix, EnvError> {
        Ok(ContainerPrefix::new(format!(
            "{}-s",
            self.container_name(instance).await?
        )))
    }
}

/// A deterministic container name from an environment label, so a re-acquire
/// targets the same container. Non-name characters become hyphens.
pub(crate) fn container_name(label: &str) -> String {
    let sanitized: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("petri-{sanitized}")
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
