//! One executor per kind of environment.
//!
//! The IR names the *kind* of environment a scope needs ([`RuntimeTarget`]); each
//! executor implementation provides one kind. This is the dispatcher that lets a
//! single run mix them: a graph with one host scope and one container scope routes
//! each `acquire` to the executor registered for that target.
//!
//! It lives here rather than in the `executor` crate because it must name every
//! implementation, and the interface crate names none — assembly is this crate's
//! job.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use executor::{EnvError, EnvHandle, Executor, ReleaseReport, ScopeOutcome, ScopeSpec};
use ir::{RuntimeTarget, ScopeId};
use smol_str::SmolStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Which {
    Host,
    Container,
}

/// Routes each scope to the executor for its [`RuntimeTarget`].
#[derive(Default)]
pub struct TargetExecutor {
    host: Option<Arc<dyn Executor>>,
    container: Option<Arc<dyn Executor>>,
    /// Which executor acquired each live environment, so release goes back to it.
    /// Keyed by scope and instance name; one `TargetExecutor` serves one run
    /// directory, where instance names are unique.
    routes: Mutex<HashMap<(ScopeId, SmolStr), Which>>,
}

impl TargetExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    /// The executor for [`RuntimeTarget::HostProcess`] scopes.
    pub fn host(mut self, executor: impl Executor + 'static) -> Self {
        self.host = Some(Arc::new(executor));
        self
    }

    /// The executor for [`RuntimeTarget::Container`] scopes.
    pub fn container(mut self, executor: impl Executor + 'static) -> Self {
        self.container = Some(Arc::new(executor));
        self
    }
}

#[async_trait]
impl Executor for TargetExecutor {
    async fn acquire(&self, scope: &ScopeSpec) -> Result<EnvHandle, EnvError> {
        let (which, executor, kind) = match &scope.runtime.target {
            RuntimeTarget::HostProcess => (Which::Host, &self.host, "host-process"),
            RuntimeTarget::Container { .. } => (Which::Container, &self.container, "container"),
        };
        let Some(executor) = executor else {
            return Err(EnvError::Backend {
                backend: SmolStr::new("target"),
                operation: SmolStr::new("acquire"),
                message: format!("no executor is registered for {kind} scopes"),
            });
        };
        let handle = executor.acquire(scope).await?;
        self.routes
            .lock()
            .expect("route table is not poisoned")
            .insert((scope.id, scope.instance.clone()), which);
        Ok(handle)
    }

    async fn release(&self, env: EnvHandle, outcome: ScopeOutcome) -> ReleaseReport {
        let which = self
            .routes
            .lock()
            .expect("route table is not poisoned")
            .remove(&(env.scope(), SmolStr::new(env.instance())));
        let executor = match which {
            Some(Which::Host) => self.host.as_ref(),
            Some(Which::Container) => self.container.as_ref(),
            None => None,
        };
        match executor {
            Some(executor) => executor.release(env, outcome).await,
            None => ReleaseReport::default().problem(format!(
                "no executor is recorded for scope {} instance `{}`; nothing was released",
                env.scope(),
                env.instance()
            )),
        }
    }
}
