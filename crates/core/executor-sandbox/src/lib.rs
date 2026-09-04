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
mod routing;

use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{fmt, process};

use async_trait::async_trait;
use executor::{
    AcquireContext, EnvError, EnvHandle, Executor, ReleaseReport, Retention, ScopeOutcome,
    ScopeSpec,
};
use ir::RuntimeTarget;
use sandbox_driver::{Sandbox, SandboxFilter, SandboxProvider, SandboxSource, SandboxSpec};
use smol_str::SmolStr;
use tokio::fs;
use tokio::sync::OnceCell;

pub use crate::backend::BackendKind;
use crate::env::SandboxEnv;
pub use crate::routing::RoutingExecutor;

/// The container path every scope's workspace is mounted or created at.
const CONTAINER_WORKSPACE: &str = "/workspace";
/// The alias a container reaches the driver's machine through.
const DOCKER_HOST_ALIAS: &str = "host.docker.internal";
/// The label naming a sandbox's environment, for the reconcile fence. Its
/// value is unique per run directory and scope, so `acquire` finds and ends
/// exactly the crashed predecessors of this environment.
const ENVIRONMENT_LABEL: &str = "petri.environment";
/// The file under the run dir holding the run id the environment label
/// carries; minted once, read back by any executor over the same run dir.
const RUN_ID_FILE: &str = "sandbox-run-id";

/// An [`Executor`] backed by one sandbox-driver provider.
pub struct SandboxExecutor {
    provider:  Arc<dyn SandboxProvider>,
    backend:   BackendKind,
    run_dir:   PathBuf,
    retention: Retention,
    /// The run id the environment label carries, resolved on first use from
    /// the run dir so a resuming executor reaches the crashed run's sandboxes.
    run_id:    OnceCell<SmolStr>,
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
            run_id: OnceCell::new(),
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

        // The environment label is the reconcile key. On the Docker backend a
        // matching sandbox is a crashed predecessor: end it before creating a
        // fresh one. The host provider's registry is in-process, so its label
        // query is empty after a restart and the fence is a no-op there.
        let label = match self.backend {
            BackendKind::Docker => Some(self.environment_label(scope).await?),
            BackendKind::Host => None,
        };
        if let Some(label) = &label {
            self.fence_by_label(label).await?;
        }

        let spec = self.build_spec(scope, &workspace_host_str, label.as_deref(), ctx)?;
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

    /// Maps a scope to a `SandboxSpec` for this backend. `label`, when
    /// present, names the environment for the reconcile fence and pins a
    /// deterministic sandbox name so a re-acquire targets the same one.
    fn build_spec(
        &self,
        scope: &ScopeSpec,
        workspace_host: &str,
        label: Option<&str>,
        ctx: &AcquireContext,
    ) -> Result<SandboxSpec, EnvError> {
        let mut spec = match (self.backend, &scope.runtime.target) {
            (BackendKind::Host, RuntimeTarget::HostProcess) => {
                SandboxSpec::new(SandboxSource::HostDirectory).working_directory(workspace_host)
            }
            (
                BackendKind::Docker,
                RuntimeTarget::Container {
                    image, credentials, ..
                },
            ) => {
                let registry_auth = self.registry_auth(credentials.as_ref(), ctx)?;
                let sidecars = self.sidecars(scope, ctx)?;
                let provider_config =
                    docker_provider_config(workspace_host, registry_auth, sidecars);
                let mut spec = SandboxSpec::new(SandboxSource::Image {
                    reference: image.to_string(),
                })
                .working_directory(CONTAINER_WORKSPACE)
                .provider_config(provider_config);
                if let Some(label) = label {
                    spec = spec
                        .name(container_name(label))
                        .label(ENVIRONMENT_LABEL, label);
                }
                spec
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
        Ok(spec)
    }

    /// Resolves image pull credentials to a `registry_auth` value, or `None`
    /// when the scope declares none.
    fn registry_auth(
        &self,
        credentials: Option<&ir::RegistryCredentials>,
        ctx: &AcquireContext,
    ) -> Result<Option<serde_json::Value>, EnvError> {
        let Some(credentials) = credentials else {
            return Ok(None);
        };
        let password = self.resolve_secret(&credentials.password_secret, ctx)?;
        Ok(Some(serde_json::json!({
            "username": credentials.username.as_str(),
            "password": password,
        })))
    }

    /// Maps the scope's services to Docker sidecars. Ports are dropped: a
    /// containerized job reaches a service by its network alias, not a
    /// published port.
    fn sidecars(
        &self,
        scope: &ScopeSpec,
        ctx: &AcquireContext,
    ) -> Result<Vec<serde_json::Value>, EnvError> {
        let mut sidecars = Vec::with_capacity(scope.services.len());
        for service in &scope.services {
            let env: serde_json::Map<String, serde_json::Value> = service
                .env
                .iter()
                .map(|(key, value)| {
                    (
                        key.as_str().to_owned(),
                        serde_json::Value::String(value.to_string()),
                    )
                })
                .collect();
            let mut sidecar = serde_json::json!({
                "name": service.name.as_str(),
                "image": service.image.as_str(),
                "env": env,
            });
            if let Some(credentials) = &service.credentials {
                let password = self.resolve_secret(&credentials.password_secret, ctx)?;
                sidecar["registry_auth"] = serde_json::json!({
                    "username": credentials.username.as_str(),
                    "password": password,
                });
            }
            sidecars.push(sidecar);
        }
        Ok(sidecars)
    }

    /// Resolves a secret by name inside acquire, registering it for masking.
    fn resolve_secret(&self, name: &str, ctx: &AcquireContext) -> Result<String, EnvError> {
        ctx.secrets()
            .resolve(name)
            .map(|secret| secret.expose().to_string())
            .map_err(|error| EnvError::Backend {
                backend:   SmolStr::new(self.backend.as_str()),
                operation: SmolStr::new("acquire"),
                message:   format!("resolving secret `{name}`: {error}"),
            })
    }

    fn acquire_failed(&self, error: &sandbox_driver::Error) -> EnvError {
        EnvError::Backend {
            backend:   SmolStr::new(self.backend.as_str()),
            operation: SmolStr::new("acquire"),
            message:   error.to_string(),
        }
    }

    /// The environment label value for a scope: the run id and the scope's
    /// environment id, globally unique because the run id is minted once per
    /// run directory.
    async fn environment_label(&self, scope: &ScopeSpec) -> Result<String, EnvError> {
        let run_id = self.resolve_run_id().await?;
        Ok(format!("{run_id}/{}", scope.environment.as_str()))
    }

    async fn resolve_run_id(&self) -> Result<SmolStr, EnvError> {
        self.run_id
            .get_or_try_init(|| load_or_record_run_id(&self.run_dir))
            .await
            .cloned()
    }

    /// Ends every sandbox that carries `label`: a crashed predecessor of this
    /// environment. Removing it makes its status unreadable and frees the
    /// deterministic name for the fresh create.
    async fn fence_by_label(&self, label: &str) -> Result<(), EnvError> {
        let mut filter = SandboxFilter::default();
        filter
            .labels
            .insert(ENVIRONMENT_LABEL.to_owned(), label.to_owned());
        let matches = self
            .provider
            .list(&filter)
            .await
            .map_err(|error| self.acquire_failed(&error))?;
        for status in matches {
            match self.provider.attach(&status.id, None).await {
                Ok(sandbox) => {
                    if let Err(error) = sandbox.delete().await {
                        tracing::warn!(error = ?error, "fencing a prior sandbox failed");
                    }
                }
                Err(error) => {
                    tracing::warn!(error = ?error, "attaching a prior sandbox for the fence failed");
                }
            }
        }
        Ok(())
    }
}

/// A deterministic sandbox name from an environment label, so a re-acquire
/// targets the same container. Non-name characters become hyphens.
fn container_name(label: &str) -> String {
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
async fn load_or_record_run_id(run_dir: &PathBuf) -> Result<SmolStr, EnvError> {
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

/// The Docker `provider_config` for a scope container: the workspace is a
/// host bind mount, an init process reaps zombies, and the host alias
/// resolves to the gateway.
fn docker_provider_config(
    workspace_host: &str,
    registry_auth: Option<serde_json::Value>,
    sidecars: Vec<serde_json::Value>,
) -> serde_json::Value {
    let mut config = serde_json::json!({
        "init": true,
        "binds": [{ "host": workspace_host, "container": CONTAINER_WORKSPACE }],
        "extra_hosts": [format!("{DOCKER_HOST_ALIAS}:host-gateway")],
    });
    if let Some(auth) = registry_auth {
        config["registry_auth"] = auth;
    }
    if !sidecars.is_empty() {
        config["sidecars"] = serde_json::Value::Array(sidecars);
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
            Err(error) if error.kind() == ErrorKind::NotFound => report.released(workspace),
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
