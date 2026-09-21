//! The simulated provider: what a dry run acquires its scopes on.
//!
//! A dry run touches no provider. Its step kinds are stubs that run no
//! process and read no file, so a scope needs nothing behind it. This
//! in-process provider hands out sandboxes that are a name, a working
//! directory string and labels, and nothing else: no plugin is launched,
//! no process runs, no directory is created. The lease manager, the ledger
//! and the run's scope records work unchanged over it, so `scope.acquired`
//! names the provider `simulated`, `scope.released` records the lease's
//! end, and a resume or a prune finds records it knows to leave alone.
//!
//! A simulated sandbox lives as long as the process that created it. A
//! resumed dry run finds none on the provider and creates fresh ones
//! ([`crate::LostSandbox::Replace`]), which is what a sandbox with no state
//! amounts to.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use sandbox_driver::{
    Capabilities, DirEntry, Error, EventContext, Exec, ExecControls, ExecResult, ExecSpec,
    ExecStreamingResult, FileMetadata, Filesystem, Isolation, PlatformInfo, ProviderError,
    ProviderKind, ResourceKind, Sandbox, SandboxFilter, SandboxId, SandboxProvider, SandboxSpec,
    SandboxState, SandboxStatus,
};

/// The provider kind a dry run's leases and scope records name.
pub const SIMULATED_KIND: &str = "simulated";

/// The in-process provider of sandboxes that hold nothing and run nothing.
pub struct SimulatedProvider {
    kind:         ProviderKind,
    capabilities: Capabilities,
    sandboxes:    Mutex<BTreeMap<String, Arc<SimulatedSandbox>>>,
    unnamed:      AtomicU64,
}

impl Default for SimulatedProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl SimulatedProvider {
    pub fn new() -> Self {
        Self {
            kind:         ProviderKind::try_new(SIMULATED_KIND)
                .expect("the simulated provider kind is a valid kind name"),
            // No isolation is claimed: there is nothing to isolate. Nothing
            // optional is declared, so every gated facet fails preflight.
            capabilities: Capabilities::minimal(Isolation::None),
            sandboxes:    Mutex::new(BTreeMap::new()),
            unnamed:      AtomicU64::new(0),
        }
    }
}

#[async_trait]
impl SandboxProvider for SimulatedProvider {
    fn kind(&self) -> &ProviderKind {
        &self.kind
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    /// A sandbox named as the spec asks, at the working directory it asks
    /// for, carrying its labels. A name already in use is taken over: a
    /// simulated sandbox has no state to lose.
    async fn create(
        &self,
        spec: &SandboxSpec,
        _events: Option<EventContext>,
    ) -> sandbox_driver::Result<Arc<dyn Sandbox>> {
        let name = spec.name.clone().unwrap_or_else(|| {
            format!(
                "{SIMULATED_KIND}-{}",
                self.unnamed.fetch_add(1, Ordering::Relaxed)
            )
        });
        let id = SandboxId::try_new(name.as_str())
            .map_err(|error| Error::invalid_spec("name", error.to_string()))?;
        let sandbox = Arc::new(SimulatedSandbox {
            id,
            name: name.clone(),
            labels: spec.labels.clone(),
            working_directory: spec
                .working_directory
                .clone()
                .unwrap_or_else(|| format!("/{SIMULATED_KIND}")),
            capabilities: self.capabilities.clone(),
            state: Mutex::new(SandboxState::Running),
            exec: SimulatedExec {
                kind: self.kind.clone(),
            },
            fs: SimulatedFilesystem {
                kind: self.kind.clone(),
            },
        });
        self.sandboxes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name, sandbox.clone());
        Ok(sandbox)
    }

    async fn attach(
        &self,
        id: &SandboxId,
        _events: Option<EventContext>,
    ) -> sandbox_driver::Result<Arc<dyn Sandbox>> {
        let sandbox = self
            .sandboxes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id.as_str())
            .filter(|sandbox| sandbox.state() != SandboxState::Deleted)
            .cloned();
        sandbox
            .map(|sandbox| sandbox as Arc<dyn Sandbox>)
            .ok_or_else(|| Error::NotFound {
                resource: ResourceKind::Sandbox,
                id:       id.as_str().to_owned(),
            })
    }

    async fn list(&self, filter: &SandboxFilter) -> sandbox_driver::Result<Vec<SandboxStatus>> {
        let sandboxes: Vec<Arc<SimulatedSandbox>> = self
            .sandboxes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .filter(|sandbox| sandbox.state() != SandboxState::Deleted)
            .filter(|sandbox| {
                filter
                    .labels
                    .iter()
                    .all(|(key, value)| sandbox.labels.get(key) == Some(value))
            })
            .cloned()
            .collect();
        Ok(sandboxes.iter().map(|sandbox| sandbox.status()).collect())
    }
}

/// A sandbox that is a name and a state. Its facets refuse every process
/// and hold no file.
struct SimulatedSandbox {
    id:                SandboxId,
    name:              String,
    labels:            BTreeMap<String, String>,
    working_directory: String,
    capabilities:      Capabilities,
    state:             Mutex<SandboxState>,
    exec:              SimulatedExec,
    fs:                SimulatedFilesystem,
}

impl SimulatedSandbox {
    fn state(&self) -> SandboxState {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set_state(&self, state: SandboxState) {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) = state;
    }

    fn status(&self) -> SandboxStatus {
        let mut status = SandboxStatus::new(self.id.clone(), self.state());
        status.name = Some(self.name.clone());
        status.labels = self.labels.clone();
        status
    }
}

#[async_trait]
impl Sandbox for SimulatedSandbox {
    fn id(&self) -> &SandboxId {
        &self.id
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn describe(&self) -> sandbox_driver::Result<SandboxStatus> {
        Ok(self.status())
    }

    fn working_directory(&self) -> &str {
        &self.working_directory
    }

    /// Empty: no process starts here, so no variable is in force.
    async fn environment(&self) -> sandbox_driver::Result<BTreeMap<String, String>> {
        Ok(BTreeMap::new())
    }

    async fn platform_info(&self) -> sandbox_driver::Result<PlatformInfo> {
        Err(self.exec.refusal("a simulated sandbox has no platform"))
    }

    async fn start(&self) -> sandbox_driver::Result<()> {
        self.set_state(SandboxState::Running);
        Ok(())
    }

    async fn stop(&self) -> sandbox_driver::Result<()> {
        self.set_state(SandboxState::Stopped);
        Ok(())
    }

    async fn delete(&self) -> sandbox_driver::Result<()> {
        self.set_state(SandboxState::Deleted);
        Ok(())
    }

    fn exec(&self) -> &dyn Exec {
        &self.exec
    }

    fn fs(&self) -> &dyn Filesystem {
        &self.fs
    }
}

/// The exec facet: every command is refused.
struct SimulatedExec {
    kind: ProviderKind,
}

impl SimulatedExec {
    fn refusal(&self, message: &str) -> Error {
        Error::Provider(ProviderError::new(self.kind.clone(), message))
    }
}

#[async_trait]
impl Exec for SimulatedExec {
    async fn run(&self, _spec: &ExecSpec) -> sandbox_driver::Result<ExecResult> {
        Err(self.refusal("a simulated sandbox runs no process"))
    }

    async fn run_streaming(
        &self,
        _spec: &ExecSpec,
        _controls: ExecControls,
    ) -> sandbox_driver::Result<ExecStreamingResult> {
        Err(self.refusal("a simulated sandbox runs no process"))
    }
}

/// The filesystem facet: nothing exists, and nothing can be written.
struct SimulatedFilesystem {
    kind: ProviderKind,
}

impl SimulatedFilesystem {
    fn refusal(&self) -> Error {
        Error::Provider(ProviderError::new(
            self.kind.clone(),
            "a simulated sandbox holds no files",
        ))
    }

    fn not_found(path: &str) -> Error {
        Error::NotFound {
            resource: ResourceKind::File,
            id:       path.to_owned(),
        }
    }
}

#[async_trait]
impl Filesystem for SimulatedFilesystem {
    async fn read(&self, path: &str) -> sandbox_driver::Result<Vec<u8>> {
        Err(Self::not_found(path))
    }

    async fn write(&self, _path: &str, _content: &[u8]) -> sandbox_driver::Result<()> {
        Err(self.refusal())
    }

    /// Idempotent by contract: a path that does not exist is deleted.
    async fn delete(&self, _path: &str, _recursive: bool) -> sandbox_driver::Result<()> {
        Ok(())
    }

    async fn exists(&self, _path: &str) -> sandbox_driver::Result<bool> {
        Ok(false)
    }

    async fn metadata(&self, path: &str) -> sandbox_driver::Result<FileMetadata> {
        Err(Self::not_found(path))
    }

    async fn list_dir(&self, _path: &str, _depth: usize) -> sandbox_driver::Result<Vec<DirEntry>> {
        Ok(Vec::new())
    }

    async fn create_dir(&self, _path: &str) -> sandbox_driver::Result<()> {
        Err(self.refusal())
    }

    async fn rename(&self, from: &str, _to: &str) -> sandbox_driver::Result<()> {
        Err(Self::not_found(from))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, workspace: &str) -> SandboxSpec {
        SandboxSpec::new(sandbox_driver::SandboxSource::HostDirectory)
            .name(name)
            .working_directory(format!("/{SIMULATED_KIND}/{workspace}"))
            .label(crate::WORKSPACE_LABEL, workspace)
    }

    #[tokio::test]
    async fn a_created_sandbox_is_listed_by_label_and_attached_by_id_until_deleted() {
        let provider = SimulatedProvider::new();
        let sandbox = provider
            .create(&spec("petri-run-l1", "run/ws"), None)
            .await
            .expect("create");
        assert_eq!(sandbox.id().as_str(), "petri-run-l1");
        assert_eq!(sandbox.working_directory(), "/simulated/run/ws");
        let mut filter = SandboxFilter::default();
        filter
            .labels
            .insert(crate::WORKSPACE_LABEL.to_owned(), "run/ws".to_owned());
        let listed = provider.list(&filter).await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].state, SandboxState::Running);
        let attached = provider.attach(sandbox.id(), None).await.expect("attach");
        attached.stop().await.expect("stop");
        assert_eq!(
            sandbox.describe().await.expect("describe").state,
            SandboxState::Stopped
        );
        sandbox.delete().await.expect("delete");
        assert!(provider.list(&filter).await.expect("list").is_empty());
        assert!(matches!(
            provider.attach(sandbox.id(), None).await,
            Err(Error::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn the_facets_run_nothing_and_hold_nothing() {
        let provider = SimulatedProvider::new();
        let sandbox = provider
            .create(&spec("petri-run-l2", "run/other"), None)
            .await
            .expect("create");
        let run = sandbox.exec().run(&ExecSpec::new("true")).await;
        assert!(
            run.as_ref()
                .is_err_and(|error| error.to_string().contains("runs no process")),
            "{run:?}"
        );
        assert!(matches!(
            sandbox.fs().read("a.txt").await,
            Err(Error::NotFound { .. })
        ));
        assert!(!sandbox.fs().exists("a.txt").await.expect("exists"));
        let write = sandbox.fs().write("a.txt", b"x").await;
        assert!(
            write
                .as_ref()
                .is_err_and(|error| error.to_string().contains("holds no files")),
            "{write:?}"
        );
        assert!(sandbox.environment().await.expect("environment").is_empty());
    }
}
