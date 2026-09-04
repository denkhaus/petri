//! One-shot containers for a host-process scope.
//!
//! A host scope has no sandbox, and a one-shot container is a provider
//! operation on a sandbox. So the first Docker action a host scope runs
//! creates one small **action host** sandbox on the local daemon — an
//! Alpine container that binds the scope's host workspace at `/workspace`
//! and does nothing else — and every action of the scope runs as a one-shot
//! beside it, sharing that bind and its network. The sandbox goes with the
//! scope's release. A crash leaves one behind, and its one-shots with it:
//! the **action-host marker** under the scope's directory says one was
//! created, so the next acquire of the same workspace sweeps it by label
//! (and its one-shots with it) before any step runs, without touching a
//! daemon for a scope that never ran an action.
//!
//! This only makes sense on a daemon that shares Petri's filesystem, which
//! is what a host scope's bind mount already assumes.

use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use executor::{ContainerRunner, EnvError, OneShotContainer, ProcessHandle, ScopeSpec};
use sandbox_driver::{Sandbox, SandboxFilter, SandboxSource, SandboxSpec};
use sandbox_driver_docker_config::{BindMount, DockerProviderConfig};
use smol_str::SmolStr;
use tokio::fs;
use tokio::sync::OnceCell;

use crate::env::OneShotRunner;
use crate::plugin::ProviderSource;
use crate::run::{RUN_LABEL, RunIdentity, scope_dir};
use crate::{BACKEND, CONTAINER_WORKSPACE, DOCKER_HOST_ALIAS, acquire_failed};

/// The label naming the host workspace an action host serves.
pub(crate) const ACTIONS_LABEL: &str = "petri.actions";
/// Overrides the action host image.
const IMAGE_VAR: &str = "PETRI_SANDBOX_ACTION_HOST_IMAGE";
const DEFAULT_IMAGE: &str = "alpine:3.20";
/// The marker under a scope dir saying an action host was created.
const MARKER: &str = "action-host";

/// The marker path for `workspace_id`'s scope dir.
fn marker_path(run_dir: &Path, workspace_id: &str) -> PathBuf {
    scope_dir(run_dir, workspace_id).join(MARKER)
}

/// The action host's name: the run's prefix, `a`, and the workspace, with
/// every character Docker would refuse in a name replaced.
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

/// Removes every action host of `workspace_id` left by a dead process,
/// when the marker says one may exist. Called at acquire; a failure to
/// reach the daemon is logged, not fatal — the scope's steps do not need
/// it, and its first action will report the daemon's state itself.
pub(crate) async fn sweep_stale(
    source: &dyn ProviderSource,
    identity: &RunIdentity,
    workspace_id: &str,
) {
    let marker = marker_path(identity.run_dir(), workspace_id);
    if !marker.exists() {
        return;
    }
    let outcome = async {
        let (provider, _) = source.current().await?;
        let label = identity.workspace_label(workspace_id).await?;
        let mut filter = SandboxFilter::default();
        filter.labels.insert(ACTIONS_LABEL.to_owned(), label);
        for stale in provider
            .list(&filter)
            .await
            .map_err(|error| acquire_failed(&error))?
        {
            provider
                .delete(&stale.id, None)
                .await
                .map_err(|error| acquire_failed(&error))?;
            tracing::info!(sandbox = %stale.id, "removed a stale action host");
        }
        Ok::<(), EnvError>(())
    }
    .await;
    match outcome {
        Ok(()) => {
            let _ = fs::remove_file(&marker).await;
        }
        Err(error) => tracing::warn!(error = %error, "sweeping stale action hosts failed"),
    }
}

/// A host scope's runner: the action host is created on first use.
pub(crate) struct ActionHostRunner {
    source:       Arc<dyn ProviderSource>,
    identity:     Arc<RunIdentity>,
    workspace:    PathBuf,
    workspace_id: String,
    env:          BTreeMap<SmolStr, SmolStr>,
    host_address: String,
    sandbox:      OnceCell<Arc<dyn Sandbox>>,
}

impl ActionHostRunner {
    pub(crate) fn new(
        source: Arc<dyn ProviderSource>,
        identity: Arc<RunIdentity>,
        workspace: PathBuf,
        scope: &ScopeSpec,
        host_address: String,
    ) -> Self {
        Self {
            source,
            identity,
            workspace,
            workspace_id: scope.workspace_id.as_str().to_owned(),
            env: scope.env.clone(),
            host_address,
            sandbox: OnceCell::new(),
        }
    }

    async fn label(&self) -> Result<String, EnvError> {
        self.identity.workspace_label(&self.workspace_id).await
    }

    /// The action host, creating it once. A leftover from a crashed process
    /// carries the same label and is removed first.
    async fn sandbox(&self) -> Result<Arc<dyn Sandbox>, EnvError> {
        self.sandbox
            .get_or_try_init(|| async {
                let (provider, _) = self.source.current().await?;
                let label = self.label().await?;
                let mut filter = SandboxFilter::default();
                filter
                    .labels
                    .insert(ACTIONS_LABEL.to_owned(), label.clone());
                for stale in provider
                    .list(&filter)
                    .await
                    .map_err(|error| acquire_failed(&error))?
                {
                    if let Err(error) = provider.delete(&stale.id, None).await {
                        tracing::warn!(error = %error, "removing a stale action host failed");
                    }
                }
                let run_id = self.identity.run_id().await?;
                let prefix = self.identity.container_prefix().await?;
                // The marker lands before the create, so a crash between
                // the two still leaves the next acquire something to sweep.
                let marker = marker_path(self.identity.run_dir(), &self.workspace_id);
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
            })
            .await
            .cloned()
    }

    /// Ends the action host: `true` when one was created and is now gone.
    pub(crate) async fn teardown(&self) -> Result<bool, EnvError> {
        let Some(sandbox) = self.sandbox.get() else {
            return Ok(false);
        };
        sandbox
            .delete()
            .await
            .map_err(|error| EnvError::backend(BACKEND, "release", error.to_string()))?;
        let _ = fs::remove_file(marker_path(self.identity.run_dir(), &self.workspace_id)).await;
        Ok(true)
    }
}

#[async_trait::async_trait]
impl ContainerRunner for ActionHostRunner {
    fn workspace_path(&self) -> &str {
        CONTAINER_WORKSPACE
    }

    fn host_address(&self) -> &str {
        &self.host_address
    }

    async fn run(&self, spec: OneShotContainer) -> Result<Box<dyn ProcessHandle>, EnvError> {
        let sandbox = self.sandbox().await?;
        let runner = OneShotRunner {
            sandbox,
            workspace: CONTAINER_WORKSPACE.to_owned(),
            host_address: self.host_address.clone(),
            env: self.env.clone(),
        };
        runner.run(spec).await
    }
}
