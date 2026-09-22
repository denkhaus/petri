//! Built-in providers linked into the embedding process.
//!
//! An embedder that links sandbox-driver's Host, Docker, or Daytona provider
//! hands Petri a [`ProviderFactory`] per kind in [`InProcessProviders`]. The
//! standard router then reaches those providers directly instead of
//! launching their plugins. Everything else is unchanged: the lease manager
//! still records a verified fingerprint before any resource exists, fences a
//! recovered sandbox once, and applies retention; prune still deletes
//! through the durable records.
//!
//! A router with in-process providers never launches a provider plugin: a
//! kind with no factory fails routably at acquire. A router without them
//! reaches every provider through its plugin, as before.
//!
//! `InProcessSource` is one factory's provider for one run: connected on
//! first use and health-checked before it serves, like a plugin's first
//! generation.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use executor::EnvError;
use sandbox_driver::{HealthStatus, SandboxProvider};
use smol_str::SmolStr;
use tokio::sync::OnceCell;
use tokio::time::timeout;

use crate::fingerprint;
use crate::plugin::{ProviderSource, docker_host_is_local, infer_host_address};

/// How long connecting and the first health check may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Builds one kind's provider for a run.
///
/// Every method but [`ProviderFactory::connect`] is called while the router
/// is configured and must not contact the backend: a Host-only run never
/// reaches Docker or Daytona. `connect` runs once per run, on the first
/// scope that needs the provider, and its provider's health is checked
/// before any lease uses it.
#[async_trait]
pub trait ProviderFactory: Send + Sync {
    /// The provider kind leases record: `host`, `docker`, or `daytona`.
    fn kind(&self) -> &str;

    /// The non-secret namespace seed, built with the [`fingerprint`]
    /// helpers from the same inputs `connect` uses, so a lease recorded
    /// through the kind's plugin is recoverable here.
    fn fingerprint_seed(&self, context: &ProviderContext) -> String;

    /// Where sandboxes are placed (a Daytona target), when configured.
    fn region(&self) -> Option<&str> {
        None
    }

    /// How a sandbox reaches services on Petri's machine.
    fn network(&self) -> ProviderNetwork;

    /// Connects the provider. A Host factory opens the context's
    /// [`ProviderContext::host_registry`]; it must not share that registry
    /// with another provider instance.
    async fn connect(
        &self,
        context: &ProviderContext,
    ) -> sandbox_driver::Result<Arc<dyn SandboxProvider>>;
}

/// What a factory knows about the run it builds a provider for.
#[derive(Clone, Debug)]
pub struct ProviderContext {
    run_id:        SmolStr,
    run_dir:       PathBuf,
    host_registry: Option<PathBuf>,
}

impl ProviderContext {
    pub(crate) fn new(run_id: &str, run_dir: &Path, host_registry: Option<PathBuf>) -> Self {
        Self {
            run_id: SmolStr::new(run_id),
            run_dir: run_dir.to_path_buf(),
            host_registry,
        }
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    /// The run's canonical Host registry, `<run dir>/host-registry`. Set
    /// for the Host factory only; the router creates it first.
    pub fn host_registry(&self) -> Option<&Path> {
        self.host_registry.as_deref()
    }
}

/// How sandboxes of one provider reach services on Petri's machine.
#[derive(Clone, Debug)]
pub struct ProviderNetwork {
    host_address:            Result<Option<String>, String>,
    supports_host_workspace: bool,
}

impl ProviderNetwork {
    /// Processes on this machine reach it on the loopback.
    pub fn host() -> Self {
        Self::unmounted("host")
    }

    /// A Docker daemon at `docker_host` (`None` for the local default).
    /// A local daemon reaches this machine through its gateway alias and
    /// can mount a Host workspace; a remote one needs `host_address`, and
    /// mounts nothing from here.
    pub fn docker(docker_host: Option<&str>, host_address: Option<&str>) -> Self {
        let docker_host = docker_host.unwrap_or_default();
        Self {
            host_address:            infer_host_address("docker", host_address, docker_host)
                .map_err(|error| error.to_string()),
            supports_host_workspace: docker_host_is_local(docker_host),
        }
    }

    /// A provider with no route back to this machine: Daytona.
    pub fn none() -> Self {
        Self::unmounted("daytona")
    }

    /// `kind`'s default route back to this machine, mounting no Host
    /// workspace.
    fn unmounted(kind: &str) -> Self {
        Self {
            host_address:            infer_host_address(kind, None, "")
                .map_err(|error| error.to_string()),
            supports_host_workspace: false,
        }
    }

    pub(crate) fn host_address(&self) -> Result<Option<String>, String> {
        self.host_address.clone()
    }

    pub(crate) fn supports_host_workspace(&self) -> bool {
        self.supports_host_workspace
    }
}

/// The built-in providers an embedder links, at most one factory per kind.
///
/// Host-process scopes use the `host` factory; Docker-backend scopes, and
/// container targets and Docker actions under the Host backend, use the
/// `docker` factory; Daytona-backend scopes use the `daytona` factory.
#[derive(Clone, Default)]
pub struct InProcessProviders {
    factories: BTreeMap<String, Arc<dyn ProviderFactory>>,
}

impl fmt::Debug for InProcessProviders {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_set()
            .entries(self.factories.keys())
            .finish()
    }
}

impl InProcessProviders {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reach `factory`'s kind through it, replacing any earlier factory
    /// for that kind.
    #[must_use]
    pub fn with(mut self, factory: Arc<dyn ProviderFactory>) -> Self {
        self.factories.insert(factory.kind().to_owned(), factory);
        self
    }

    /// The factory for `kind`.
    pub(crate) fn factory(&self, kind: &str) -> Result<Arc<dyn ProviderFactory>, String> {
        self.factories.get(kind).cloned().ok_or_else(|| {
            format!(
                "the {kind} provider is not configured in this process, and no provider plugin \
                 is launched when built-in providers are linked in"
            )
        })
    }
}

/// One factory's provider for one run, as a manager's [`ProviderSource`].
///
/// The first [`ProviderSource::current`] connects, checks the provider's
/// kind and health, and fixes the fingerprint; concurrent callers share that
/// one attempt, and a failed attempt is retried by the next call because no
/// resource has been touched. The provider then serves the run as one
/// generation: nothing in this process replaces it.
/// [`ProviderSource::shutdown`] closes the source; it never serves again.
pub(crate) struct InProcessSource {
    factory:   Arc<dyn ProviderFactory>,
    context:   ProviderContext,
    seed:      String,
    /// The provider and the fingerprint its first health report verified.
    connected: OnceCell<(Arc<dyn SandboxProvider>, String)>,
    closed:    AtomicBool,
}

impl InProcessSource {
    pub(crate) fn new(factory: Arc<dyn ProviderFactory>, context: ProviderContext) -> Self {
        let seed = factory.fingerprint_seed(&context);
        Self {
            factory,
            context,
            seed,
            connected: OnceCell::new(),
            closed: AtomicBool::new(false),
        }
    }

    fn error(&self, operation: &str, message: impl fmt::Display) -> EnvError {
        EnvError::backend(self.factory.kind(), operation, message.to_string())
    }

    fn closed_error(&self) -> EnvError {
        self.error("connect", "the run has finished; its provider is closed")
    }

    async fn connect(&self) -> Result<(Arc<dyn SandboxProvider>, String), EnvError> {
        let provider = self
            .factory
            .connect(&self.context)
            .await
            .map_err(|error| self.error("connect", error))?;
        if provider.kind().as_str() != self.factory.kind() {
            return Err(self.error(
                "connect",
                format!(
                    "the {} factory connected a `{}` provider",
                    self.factory.kind(),
                    provider.kind()
                ),
            ));
        }
        let health = provider
            .health()
            .await
            .map_err(|error| self.error("health", format!("the health check failed: {error}")))?;
        if !matches!(health.status, HealthStatus::Ok | HealthStatus::Unknown) {
            let mut detail = health
                .message
                .clone()
                .unwrap_or_else(|| "no detail".to_owned());
            if !health.missing_permissions.is_empty() {
                let _ = write!(
                    detail,
                    " (missing permissions: {})",
                    health.missing_permissions.join(", ")
                );
            }
            return Err(self.error(
                "health",
                format!(
                    "the provider reports its backend {:?}: {detail}",
                    health.status
                ),
            ));
        }
        let verified = fingerprint::verified(self.factory.kind(), &self.seed, &health)
            .map_err(|error| self.error("health", error))?;
        Ok((provider, verified))
    }
}

#[async_trait]
impl ProviderSource for InProcessSource {
    async fn current(&self) -> Result<(Arc<dyn SandboxProvider>, u64), EnvError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(self.closed_error());
        }
        let (provider, _) = self
            .connected
            .get_or_try_init(|| async {
                timeout(CONNECT_TIMEOUT, self.connect())
                    .await
                    .map_err(|_| {
                        self.error(
                            "connect",
                            format!(
                                "the provider did not connect within {}s",
                                CONNECT_TIMEOUT.as_secs()
                            ),
                        )
                    })?
            })
            .await?;
        // A shutdown that raced the connection wins: nothing is served
        // after the source closes.
        if self.closed.load(Ordering::Acquire) {
            return Err(self.closed_error());
        }
        Ok((Arc::clone(provider), 1))
    }

    async fn shutdown(&self) {
        self.closed.store(true, Ordering::Release);
    }

    fn fingerprint(&self) -> &str {
        self.connected
            .get()
            .map_or(&self.seed, |(_, verified)| verified)
    }

    fn kind(&self) -> &str {
        self.factory.kind()
    }

    fn region(&self) -> Option<&str> {
        self.factory.region().filter(|region| !region.is_empty())
    }
}
