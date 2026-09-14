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
use sandbox_driver::{Sandbox, SandboxFilter, SandboxId, SandboxSource, SandboxSpec};
use sandbox_driver_docker_config::{BindMount, DockerProviderConfig};
use smol_str::SmolStr;
use tokio::fs;
use tokio::sync::Mutex;

use crate::env::OneShotRunner;
use crate::lease::{LiveSandbox, delete_sandbox};
use crate::plugin::ProviderSource;
use crate::run::{RUN_LABEL, RunIdentity, scope_dir, write_record};
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

/// Delete an action host, reconciling an uncertain create by its marker and
/// label.
pub(crate) async fn remove_recorded(
    source: &dyn ProviderSource,
    identity: &RunIdentity,
    workspace_id: &str,
    known: Option<&SandboxId>,
) -> Result<Vec<SandboxId>, EnvError> {
    let marker = marker_path(identity.run_dir(), workspace_id);
    let fingerprint = match fs::read_to_string(&marker).await {
        Ok(fingerprint) => fingerprint,
        Err(error) if error.kind() == ErrorKind::NotFound && known.is_none() => {
            return Ok(Vec::new());
        }
        Err(error) => return Err(EnvError::workspace("read", marker.display(), error)),
    };
    let (provider, _) = source.current().await?;
    if fingerprint != source.fingerprint() {
        return Err(EnvError::backend(
            BACKEND,
            "one-shot",
            "the action host's recorded Docker provider fingerprint differs from the configured provider",
        ));
    }
    let ids = if let Some(id) = known {
        vec![id.clone()]
    } else {
        let label = identity.workspace_label(workspace_id);
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
        delete_sandbox(&*provider, id)
            .await
            .map_err(|error| acquire_failed(&error))?;
    }
    let _ = fs::remove_file(marker).await;
    Ok(ids)
}

#[derive(Default)]
struct HostState {
    prepared: bool,
    released: bool,
    sandbox:  Option<LiveSandbox>,
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

    /// Sweep once per workspace in this process. A later inherited acquire
    /// shares this state and cannot sweep its caller's live action host.
    async fn prepare_locked(&self, state: &mut HostState) -> Result<(), EnvError> {
        if !state.prepared {
            remove_recorded(&*self.source, &self.identity, &self.workspace_id, None).await?;
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

    async fn create(&self) -> Result<LiveSandbox, EnvError> {
        let (provider, generation) = self.source.current().await?;
        let label = self.identity.workspace_label(&self.workspace_id);
        let run_id = self.identity.run_id();
        let prefix = self.identity.container_prefix();
        let marker = self.marker();
        write_record(marker, self.source.fingerprint().as_bytes().to_vec()).await?;
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
        let sandbox = provider
            .create(&spec, None)
            .await
            .map_err(|error| acquire_failed(&error))?;
        Ok(LiveSandbox {
            sandbox,
            generation,
        })
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
        if let Some(live) = &state.sandbox {
            let (_, generation) = self.source.current().await?;
            if live.generation == generation {
                return Ok(live.sandbox.clone());
            }
            state.sandbox = None;
            state.prepared = false;
        }
        self.prepare_locked(&mut state).await?;
        let host = self.clone();
        // The task owns the lock until create settles. Cancelling run() cannot
        // discard a late create: teardown waits for it and deletes its result.
        tokio::spawn(async move {
            let live = match host.create().await {
                Ok(live) => live,
                Err(error) => {
                    state.prepared = false;
                    return Err(error);
                }
            };
            let sandbox = live.sandbox.clone();
            state.sandbox = Some(live);
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
        let removed = remove_recorded(
            &*self.source,
            &self.identity,
            &self.workspace_id,
            state.sandbox.as_ref().map(|live| live.sandbox.id()),
        )
        .await?;
        state.sandbox = None;
        state.released = true;
        Ok(!removed.is_empty())
    }
}

/// Each scope retains its own environment, even when the action host is shared.
pub(crate) struct ActionHostRunner {
    host: Result<Arc<ActionHost>, String>,
    env:  BTreeMap<SmolStr, SmolStr>,
}

impl ActionHostRunner {
    pub(crate) fn new(host: Result<Arc<ActionHost>, EnvError>, scope: &ScopeSpec) -> Self {
        Self {
            host: host.map_err(|error| error.to_string()),
            env:  scope.env.clone(),
        }
    }

    fn host(&self) -> Result<&Arc<ActionHost>, EnvError> {
        self.host
            .as_ref()
            .map_err(|message| EnvError::backend(BACKEND, "one-shot", message.clone()))
    }
}

#[async_trait::async_trait]
impl ContainerRunner for ActionHostRunner {
    fn workspace_path(&self) -> &str {
        CONTAINER_WORKSPACE
    }

    fn host_address(&self) -> Result<&str, EnvError> {
        Ok(&self.host()?.host_address)
    }

    async fn run(&self, spec: OneShotContainer) -> Result<Box<dyn ProcessHandle>, EnvError> {
        let host = self.host()?;
        let sandbox = host.sandbox().await?;
        let runner = OneShotRunner {
            sandbox,
            workspace: CONTAINER_WORKSPACE.to_owned(),
            host_address: Some(host.host_address.clone()),
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
        Capabilities, Capability, Error as DriverError, EventContext, Isolation, ProviderKind,
        SandboxProvider, SandboxState, SandboxStatus,
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
    async fn a_changed_action_provider_preserves_the_resource_and_marker() {
        let dir = testkit::RunDir::new("action-host-changed-provider");
        let identity = RunIdentity::for_run_dir(dir.path().to_path_buf());
        let provider = Arc::new(DelayedProvider {
            kind:         ProviderKind::try_new("docker").expect("kind"),
            capabilities: Capabilities::minimal(Isolation::Container),
            creating:     Notify::new(),
            complete:     Notify::new(),
            exists:       AtomicBool::new(true),
        });
        let source = FixedProvider::new(provider.clone());
        let marker = marker_path(dir.path(), "scope-0");
        fs::create_dir_all(marker.parent().unwrap()).await.unwrap();
        fs::write(&marker, "docker:original-provider")
            .await
            .unwrap();
        let id = SandboxId::try_new("late-host").unwrap();
        for known in [None, Some(&id)] {
            let error = remove_recorded(&source, &identity, "scope-0", known)
                .await
                .expect_err("changed provider must not touch the resource");
            assert!(error.to_string().contains("fingerprint"), "{error}");
            assert!(provider.exists.load(Ordering::SeqCst));
            assert_eq!(
                fs::read_to_string(&marker).await.unwrap(),
                "docker:original-provider"
            );
        }
        fs::write(&marker, source.fingerprint()).await.unwrap();
        assert_eq!(
            remove_recorded(&source, &identity, "scope-0", None)
                .await
                .unwrap(),
            [id]
        );
        assert!(!provider.exists.load(Ordering::SeqCst));
        assert!(!marker.exists());
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
            Arc::new(RunIdentity::for_run_dir(dir.path().to_path_buf())),
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
