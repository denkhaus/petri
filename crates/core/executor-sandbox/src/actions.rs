//! One-shot containers for host-process scopes.
//!
//! Scopes sharing a workspace share one action host. Its sandbox binds the
//! host workspace at `/workspace`; each scope's runner supplies its own env.
//! The action host ends with its owning lease, or with a standalone scope.
//! A marker lets the first acquire after a crash sweep old action hosts
//! without starting a plugin for workspaces that never ran an action.

use std::collections::BTreeMap;
use std::env;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use executor::{ContainerRunner, EnvError, OneShotContainer, ProcessHandle, ScopeSpec};
use sandbox_driver::{
    Error as DriverError, Sandbox, SandboxFilter, SandboxId, SandboxSource, SandboxSpec,
};
use sandbox_driver_docker_config::{BindMount, DockerProviderConfig};
use smol_str::SmolStr;
use tokio::fs;
use tokio::sync::Mutex;

use crate::env::OneShotRunner;
use crate::plugin::ProviderSource;
use crate::run::{RUN_LABEL, RunIdentity, scope_dir};
use crate::{BACKEND, CONTAINER_WORKSPACE, DOCKER_HOST_ALIAS, acquire_failed};

pub(crate) const ACTIONS_LABEL: &str = "petri.actions";
const IMAGE_VAR: &str = "PETRI_SANDBOX_ACTION_HOST_IMAGE";
const DEFAULT_IMAGE: &str = "alpine:3.20";
const MARKER: &str = "action-host";

fn marker_path(run_dir: &Path, workspace_id: &str) -> PathBuf {
    scope_dir(run_dir, workspace_id).join(MARKER)
}

fn host_name(prefix: &str, workspace_id: &str) -> String {
    let safe: String = workspace_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("{prefix}a-{safe}")
}

#[derive(Default)]
struct HostState {
    prepared: bool,
    released: bool,
    sandbox:  Option<Arc<dyn Sandbox>>,
}

/// The shared action host, independent of any scope's environment variables.
pub(crate) struct ActionHost {
    source:       Arc<dyn ProviderSource>,
    identity:     Arc<RunIdentity>,
    workspace:    PathBuf,
    workspace_id: String,
    host_address: String,
    state:        Arc<Mutex<HostState>>,
}

impl ActionHost {
    pub(crate) fn new(
        source: Arc<dyn ProviderSource>,
        identity: Arc<RunIdentity>,
        workspace: PathBuf,
        workspace_id: String,
        host_address: String,
    ) -> Self {
        Self {
            source,
            identity,
            workspace,
            workspace_id,
            host_address,
            state: Arc::new(Mutex::new(HostState::default())),
        }
    }

    fn marker(&self) -> PathBuf {
        marker_path(self.identity.run_dir(), &self.workspace_id)
    }

    /// Delete a known host, or reconcile an uncertain create by its label.
    /// The marker avoids contacting a provider when no create was attempted.
    async fn remove(&self, known: Option<&SandboxId>) -> Result<bool, EnvError> {
        let marker = self.marker();
        if known.is_none() {
            match fs::read(&marker).await {
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(EnvError::workspace("read", marker.display(), error)),
            }
        }
        let (provider, _) = self.source.current().await?;
        let ids = if let Some(id) = known {
            vec![id.clone()]
        } else {
            let label = self.identity.workspace_label(&self.workspace_id).await?;
            let mut filter = SandboxFilter::default();
            filter.labels.insert(ACTIONS_LABEL.to_owned(), label);
            provider
                .list(&filter)
                .await
                .map_err(|error| acquire_failed(&error))?
                .into_iter()
                .map(|status| status.id)
                .collect()
        };
        for id in &ids {
            match provider.delete(id, None).await {
                Ok(()) | Err(DriverError::NotFound { .. }) => {}
                Err(error) => return Err(acquire_failed(&error)),
            }
        }
        let _ = fs::remove_file(marker).await;
        Ok(!ids.is_empty())
    }

    /// Sweep once per workspace in this process. A later inherited acquire
    /// shares this state and cannot sweep its caller's live action host.
    async fn prepare_locked(&self, state: &mut HostState) -> Result<(), EnvError> {
        if !state.prepared {
            self.remove(None).await?;
            state.prepared = true;
        }
        Ok(())
    }

    pub(crate) async fn prepare(&self) {
        let mut state = self.state.lock().await;
        if let Err(error) = self.prepare_locked(&mut state).await {
            tracing::warn!(error = %error, "sweeping stale action hosts failed");
        }
    }

    async fn create(&self) -> Result<Arc<dyn Sandbox>, EnvError> {
        let (provider, _) = self.source.current().await?;
        let label = self.identity.workspace_label(&self.workspace_id).await?;
        let run_id = self.identity.run_id().await?;
        let prefix = self.identity.container_prefix().await?;
        let marker = self.marker();
        if let Some(parent) = marker.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|error| EnvError::workspace("create", parent.display(), error))?;
        }
        fs::write(&marker, b"")
            .await
            .map_err(|error| EnvError::workspace("write", marker.display(), error))?;
        let image = env::var(IMAGE_VAR)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_IMAGE.to_owned());
        let config = DockerProviderConfig {
            init: true,
            binds: vec![BindMount {
                host:      self.workspace.display().to_string(),
                container: CONTAINER_WORKSPACE.to_owned(),
                mode:      None,
            }],
            extra_hosts: vec![format!("{DOCKER_HOST_ALIAS}:host-gateway")],
            ..DockerProviderConfig::default()
        };
        let spec = SandboxSpec::new(SandboxSource::Image { reference: image })
            .name(host_name(&prefix, &self.workspace_id))
            .working_directory(CONTAINER_WORKSPACE)
            .provider_config(config.into_value())
            .label(RUN_LABEL, run_id.to_string())
            .label(ACTIONS_LABEL, label);
        provider
            .create(&spec, None)
            .await
            .map_err(|error| acquire_failed(&error))
    }

    async fn sandbox(self: &Arc<Self>) -> Result<Arc<dyn Sandbox>, EnvError> {
        let mut state = self.state.clone().lock_owned().await;
        if state.released {
            return Err(EnvError::backend(
                BACKEND,
                "one-shot",
                "the action host was released",
            ));
        }
        if let Some(sandbox) = &state.sandbox {
            return Ok(sandbox.clone());
        }
        self.prepare_locked(&mut state).await?;
        let host = self.clone();
        // The task owns the lock until create settles. Cancelling run() cannot
        // discard a late create: teardown waits for it and deletes its result.
        tokio::spawn(async move {
            let sandbox = match host.create().await {
                Ok(sandbox) => sandbox,
                Err(error) => {
                    state.prepared = false;
                    return Err(error);
                }
            };
            state.sandbox = Some(sandbox.clone());
            Ok::<_, EnvError>(sandbox)
        })
        .await
        .map_err(|error| EnvError::backend(BACKEND, "one-shot", error.to_string()))?
    }

    pub(crate) async fn teardown(&self) -> Result<bool, EnvError> {
        let mut state = self.state.lock().await;
        if state.released {
            return Ok(false);
        }
        let removed = self
            .remove(state.sandbox.as_ref().map(|sandbox| sandbox.id()))
            .await?;
        state.sandbox = None;
        state.released = true;
        Ok(removed)
    }
}

/// Each scope retains its own environment, even when the action host is shared.
pub(crate) struct ActionHostRunner {
    host: Arc<ActionHost>,
    env:  BTreeMap<SmolStr, SmolStr>,
}

impl ActionHostRunner {
    pub(crate) fn new(host: Arc<ActionHost>, scope: &ScopeSpec) -> Self {
        Self {
            host,
            env: scope.env.clone(),
        }
    }
}

#[async_trait::async_trait]
impl ContainerRunner for ActionHostRunner {
    fn workspace_path(&self) -> &str {
        CONTAINER_WORKSPACE
    }

    fn host_address(&self) -> Result<&str, EnvError> {
        Ok(&self.host.host_address)
    }

    async fn run(&self, spec: OneShotContainer) -> Result<Box<dyn ProcessHandle>, EnvError> {
        let sandbox = self.host.sandbox().await?;
        let runner = OneShotRunner {
            sandbox,
            workspace: CONTAINER_WORKSPACE.to_owned(),
            host_address: Some(self.host.host_address.clone()),
            env: self.env.clone(),
        };
        runner.run(spec).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use sandbox_driver::{
        Capabilities, Capability, EventContext, Isolation, ProviderKind, SandboxProvider,
        SandboxState, SandboxStatus,
    };
    use tokio::sync::Notify;
    use tokio::time::timeout;

    use super::*;
    use crate::FixedProvider;

    struct DelayedProvider {
        kind:         ProviderKind,
        capabilities: Capabilities,
        creating:     Notify,
        complete:     Notify,
        exists:       AtomicBool,
    }

    #[async_trait::async_trait]
    impl SandboxProvider for DelayedProvider {
        fn kind(&self) -> &ProviderKind {
            &self.kind
        }
        fn capabilities(&self) -> &Capabilities {
            &self.capabilities
        }

        async fn create(
            &self,
            _: &SandboxSpec,
            _: Option<EventContext>,
        ) -> sandbox_driver::Result<Arc<dyn Sandbox>> {
            self.creating.notify_one();
            self.complete.notified().await;
            self.exists.store(true, Ordering::SeqCst);
            // An ambiguous failure: the resource exists, but no handle arrived.
            Err(DriverError::unsupported(Capability::LifecycleUndelete))
        }

        async fn attach(
            &self,
            _: &SandboxId,
            _: Option<EventContext>,
        ) -> sandbox_driver::Result<Arc<dyn Sandbox>> {
            Err(DriverError::unsupported(Capability::LifecycleUndelete))
        }

        async fn delete(
            &self,
            _: &SandboxId,
            _: Option<EventContext>,
        ) -> sandbox_driver::Result<()> {
            self.exists.store(false, Ordering::SeqCst);
            Ok(())
        }

        async fn list(&self, _: &SandboxFilter) -> sandbox_driver::Result<Vec<SandboxStatus>> {
            Ok(if self.exists.load(Ordering::SeqCst) {
                vec![SandboxStatus::new(
                    SandboxId::try_new("late-host").expect("id"),
                    SandboxState::Running,
                )]
            } else {
                Vec::new()
            })
        }
    }

    #[tokio::test]
    async fn cancelled_creation_settles_before_teardown() {
        let dir = testkit::RunDir::new("action-host-cancelled-create");
        let provider = Arc::new(DelayedProvider {
            kind:         ProviderKind::try_new("docker").expect("kind"),
            capabilities: Capabilities::minimal(Isolation::Container),
            creating:     Notify::new(),
            complete:     Notify::new(),
            exists:       AtomicBool::new(false),
        });
        let host = Arc::new(ActionHost::new(
            Arc::new(FixedProvider::new(provider.clone())),
            Arc::new(RunIdentity::new(dir.path().to_path_buf())),
            dir.workspace(),
            "scope-0".to_owned(),
            DOCKER_HOST_ALIAS.to_owned(),
        ));
        let creating_host = host.clone();
        let creating = tokio::spawn(async move { creating_host.sandbox().await });
        provider.creating.notified().await;
        creating.abort();
        assert!(matches!(creating.await, Err(error) if error.is_cancelled()));
        let ending_host = host.clone();
        let mut ending = tokio::spawn(async move { ending_host.teardown().await });
        assert!(
            timeout(Duration::from_millis(20), &mut ending)
                .await
                .is_err()
        );
        provider.complete.notify_one();
        assert!(ending.await.expect("teardown task").expect("teardown"));
        assert!(!provider.exists.load(Ordering::SeqCst));
        assert!(
            host.sandbox().await.is_err(),
            "released host rejects new work"
        );
    }
}
