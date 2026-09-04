//! A Petri [`Executor`] over the sandbox-driver provider family.
//!
//! One `SandboxExecutor` realizes every scope on one backend — host or
//! Docker — by mapping a [`ScopeSpec`] to a `SandboxSpec`, creating the
//! sandbox, and handing steps a [`ExecEnv`] over it. It replaces the
//! bespoke host and Docker executors: the provider owns process execution,
//! the filesystem, and teardown, and this crate is the thin adapter that
//! speaks the executor interface on top.

mod backend;
mod env;

use std::fmt;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use executor::{
    AcquireContext, EnvError, EnvHandle, Executor, ReleaseReport, Retention, ScopeOutcome,
    ScopeSpec,
};
use ir::RuntimeTarget;
use sandbox_driver::{Sandbox, SandboxProvider, SandboxSource, SandboxSpec};
use smol_str::SmolStr;
use tokio::fs;

pub use crate::backend::BackendKind;
use crate::env::SandboxEnv;

/// The container path every scope's workspace is mounted or created at.
const CONTAINER_WORKSPACE: &str = "/workspace";
/// The alias a container reaches the driver's machine through.
const DOCKER_HOST_ALIAS: &str = "host.docker.internal";

/// An [`Executor`] backed by one sandbox-driver provider.
pub struct SandboxExecutor {
    provider:  Arc<dyn SandboxProvider>,
    backend:   BackendKind,
    run_dir:   PathBuf,
    retention: Retention,
}

impl SandboxExecutor {
    /// Builds an executor over `provider`. The `backend` selects how scopes
    /// map onto specs; `run_dir` roots every workspace under `scopes/`.
    pub fn new(provider: Arc<dyn SandboxProvider>, backend: BackendKind, run_dir: PathBuf) -> Self {
        Self {
            provider,
            backend,
            run_dir,
            retention: Retention::default(),
        }
    }

    #[must_use]
    pub fn with_retention(mut self, retention: Retention) -> Self {
        self.retention = retention;
        self
    }

    /// The host directory a scope's workspace lives in, on both backends: a
    /// designated directory for host, and a bind-mount source for Docker.
    fn workspace_host_path(&self, workspace_id: &str) -> PathBuf {
        self.run_dir.join("scopes").join(workspace_id).join("work")
    }

    async fn acquire_inner(
        &self,
        scope: &ScopeSpec,
        ctx: &AcquireContext,
    ) -> Result<EnvHandle, EnvError> {
        let workspace_host = self.workspace_host_path(scope.workspace_id.as_str());
        fs::create_dir_all(&workspace_host)
            .await
            .map_err(|error| EnvError::workspace("create", workspace_host.display(), error))?;
        let workspace_host_str = workspace_host.display().to_string();

        let spec = self.build_spec(scope, &workspace_host_str, ctx)?;
        let sandbox = self
            .provider
            .create(&spec, None)
            .await
            .map_err(|error| self.acquire_failed(&error))?;

        let (workspace_path, host_address) = match self.backend {
            BackendKind::Host => (workspace_host_str.clone(), "127.0.0.1".to_owned()),
            BackendKind::Docker => (CONTAINER_WORKSPACE.to_owned(), DOCKER_HOST_ALIAS.to_owned()),
        };
        let ambient = sandbox.environment().await.unwrap_or_default();

        let env = SandboxEnv::new(
            sandbox.clone(),
            self.backend,
            workspace_path,
            host_address,
            ambient,
            scope.grace,
        );
        let teardown = SandboxTeardown {
            sandbox,
            backend: self.backend,
            retention: self.retention,
            workspace_host: workspace_host.clone(),
        };
        Ok(EnvHandle::new(
            scope.id,
            SmolStr::new(scope.environment.as_str()),
            Arc::new(env),
            teardown,
        ))
    }

    /// Maps a scope to a `SandboxSpec` for this backend.
    fn build_spec(
        &self,
        scope: &ScopeSpec,
        workspace_host: &str,
        ctx: &AcquireContext,
    ) -> Result<SandboxSpec, EnvError> {
        let mut spec = match (self.backend, &scope.runtime.target) {
            (BackendKind::Host, RuntimeTarget::HostProcess) => {
                SandboxSpec::new(SandboxSource::HostDirectory).working_directory(workspace_host)
            }
            (BackendKind::Docker, RuntimeTarget::Container { image, .. }) => {
                let provider_config = docker_provider_config(workspace_host, None);
                SandboxSpec::new(SandboxSource::Image {
                    reference: image.to_string(),
                })
                .working_directory(CONTAINER_WORKSPACE)
                .provider_config(provider_config)
            }
            (BackendKind::Docker, RuntimeTarget::HostProcess) => {
                // A host-process scope on the Docker backend runs in the
                // runner image the runtime selected and recorded on the spec.
                return Err(EnvError::Backend {
                    backend:   SmolStr::new("docker"),
                    operation: SmolStr::new("acquire"),
                    message:   "a host-process scope needs a runner image on the docker backend; \
                              the runtime must set a container target"
                        .into(),
                });
            }
            (BackendKind::Host, RuntimeTarget::Container { .. }) => {
                return Err(EnvError::Backend {
                    backend:   SmolStr::new("host"),
                    operation: SmolStr::new("acquire"),
                    message:
                        "the host backend cannot run a container scope; use the docker backend"
                            .into(),
                });
            }
        };

        // Scope env is the trusted channel on both backends.
        for (key, value) in &scope.env {
            spec = spec.env_var(key.as_str(), value.as_str());
        }
        let _ = ctx;
        Ok(spec)
    }

    fn acquire_failed(&self, error: &sandbox_driver::Error) -> EnvError {
        EnvError::Backend {
            backend:   SmolStr::new(self.backend.as_str()),
            operation: SmolStr::new("acquire"),
            message:   error.to_string(),
        }
    }
}

/// The Docker `provider_config` for a scope container: the workspace is a
/// host bind mount, an init process reaps zombies, and the host alias
/// resolves to the gateway.
fn docker_provider_config(
    workspace_host: &str,
    registry_auth: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut config = serde_json::json!({
        "init": true,
        "binds": [{ "host": workspace_host, "container": CONTAINER_WORKSPACE }],
        "extra_hosts": [format!("{DOCKER_HOST_ALIAS}:host-gateway")],
    });
    if let Some(auth) = registry_auth {
        config["registry_auth"] = auth;
    }
    config
}

/// What release needs: the sandbox to tear down, whether to keep a failed
/// workspace, and where that workspace lives on the host.
struct SandboxTeardown {
    sandbox:        Arc<dyn Sandbox>,
    backend:        BackendKind,
    retention:      Retention,
    workspace_host: PathBuf,
}

impl fmt::Debug for SandboxTeardown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SandboxTeardown")
            .field("sandbox", &self.sandbox.id())
            .field("backend", &self.backend)
            .field("retention", &self.retention)
            .field("workspace_host", &self.workspace_host)
            .finish()
    }
}

#[async_trait]
impl Executor for SandboxExecutor {
    #[tracing::instrument(
        name = "scope.acquire",
        skip_all,
        fields(scope = scope.id.raw(), backend = self.backend.as_str())
    )]
    async fn acquire(
        &self,
        scope: &ScopeSpec,
        ctx: &AcquireContext,
    ) -> Result<EnvHandle, EnvError> {
        match self.acquire_inner(scope, ctx).await {
            Ok(handle) => {
                tracing::info!("scope environment acquired");
                Ok(handle)
            }
            Err(error) => {
                tracing::error!(error = ?error, "scope environment acquire failed");
                Err(error)
            }
        }
    }

    #[tracing::instrument(
        name = "scope.release",
        skip_all,
        fields(scope = env.scope().raw(), outcome = ?outcome)
    )]
    async fn release(&self, env: EnvHandle, outcome: ScopeOutcome) -> ReleaseReport {
        let mut report = ReleaseReport::default();
        let Some(teardown) = env.teardown::<SandboxTeardown>() else {
            return report.problem("sandbox executor was handed a foreign environment");
        };
        let sandbox = teardown.sandbox.clone();
        let backend = teardown.backend;
        let retention = teardown.retention;
        let workspace_host = teardown.workspace_host.clone();
        drop(env);

        // On host and Docker the workspace is a host directory, so the
        // sandbox itself holds nothing worth keeping: delete it always.
        match sandbox.delete().await {
            Ok(()) => report = report.released("sandbox"),
            Err(error) => {
                tracing::warn!(error = ?error, "sandbox delete failed");
                report = report.problem(format!("sandbox delete failed: {error}"));
            }
        }

        // The host directory is the workspace on both backends; keep it on a
        // failure the retention policy says to keep.
        let workspace = format!("workspace {}", workspace_host.display());
        let _ = backend;
        if retention.keeps(outcome) {
            return report.kept(workspace);
        }
        match fs::remove_dir_all(&workspace_host).await {
            Ok(()) => report.released(workspace),
            Err(error) if error.kind() == ErrorKind::NotFound => {
                report.released(workspace)
            }
            Err(error) => {
                tracing::warn!(error = ?error, "workspace removal failed");
                report.problem(format!(
                    "could not remove {}: {error}",
                    workspace_host.display()
                ))
            }
        }
    }
}
