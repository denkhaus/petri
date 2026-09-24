//! Built-in provider factories, linked the way an embedding application
//! links them, for tests of the in-process provider path.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use executor_sandbox::{
    InProcessProviders, ProviderContext, ProviderFactory, ProviderNetwork, fingerprint,
};
use sandbox_driver::SandboxProvider;
use sandbox_driver_host::HostProvider;

/// sandbox-driver's Host provider over the run's own registry, counting
/// how many times a run connected it.
#[derive(Default)]
pub struct HostFactory {
    connects: AtomicUsize,
}

impl HostFactory {
    pub fn connects(&self) -> usize {
        self.connects.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ProviderFactory for HostFactory {
    fn kind(&self) -> &'static str {
        "host"
    }

    fn fingerprint_seed(&self, context: &ProviderContext) -> String {
        fingerprint::host(
            context
                .host_registry()
                .expect("the router names the Host registry"),
        )
    }

    fn network(&self) -> ProviderNetwork {
        ProviderNetwork::host()
    }

    async fn connect(
        &self,
        context: &ProviderContext,
    ) -> sandbox_driver::Result<Arc<dyn SandboxProvider>> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        let registry = context
            .host_registry()
            .expect("the router names the Host registry");
        Ok(Arc::new(HostProvider::with_registry(registry).await?))
    }
}

/// A factory for `kind` that must never be connected: it counts the
/// attempt and refuses it.
pub struct UnusedFactory {
    kind:     &'static str,
    connects: AtomicUsize,
}

impl UnusedFactory {
    pub fn new(kind: &'static str) -> Self {
        Self {
            kind,
            connects: AtomicUsize::new(0),
        }
    }

    pub fn connects(&self) -> usize {
        self.connects.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ProviderFactory for UnusedFactory {
    fn kind(&self) -> &str {
        self.kind
    }

    fn fingerprint_seed(&self, _: &ProviderContext) -> String {
        format!("{}:unused", self.kind)
    }

    fn network(&self) -> ProviderNetwork {
        ProviderNetwork::docker(None, None)
    }

    async fn connect(
        &self,
        _: &ProviderContext,
    ) -> sandbox_driver::Result<Arc<dyn SandboxProvider>> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        Err(sandbox_driver::Error::invalid_spec(
            "provider",
            "this test's run must not connect this provider",
        ))
    }
}

/// Built-in Host, with a Docker factory that must stay unconnected.
pub fn host_providers() -> (InProcessProviders, Arc<HostFactory>, Arc<UnusedFactory>) {
    let host = Arc::new(HostFactory::default());
    let docker = Arc::new(UnusedFactory::new("docker"));
    let providers = InProcessProviders::new()
        .with(host.clone())
        .with(docker.clone());
    (providers, host, docker)
}
