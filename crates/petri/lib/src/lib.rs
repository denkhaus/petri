//! Petri, as it ships.
//!
//! This is the distribution: the one crate a consumer depends on, and the only
//! crate in the tree that names every component. Core is `petri-runtime` and
//! knows the native format and nothing else; each component — today, GitHub
//! Actions — is its own crate and knows core. Here the two meet: [`runtime()`]
//! is core's standard runtime with every in-tree component registered on it.
//!
//! Everything else is re-export. [`ir`], [`engine`], [`driver`], [`frontend`]
//! (with [`frontend::gha`] and [`frontend::native`]), [`steps`] and
//! [`executor`] (with [`executor::host`] and [`executor::docker`]) are
//! reachable through this crate, so an external repository names one dependency
//! and never a layer crate.
//!
//! The one thing here that is not wiring or re-export is [`host`]: the
//! standalone host's durable coordinator layout — content-addressed graphs,
//! one engine log per execution, and the run wrappers that keep it resumable
//! when no product store sits behind it.
//!
//! ```no_run
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! let rt = petri::runtime().options(petri::RunOptions::new("/tmp/petri-demo"));
//! let lowered = rt.check(std::path::Path::new("pipeline.yml"), None, None)?;
//! if let Some(graph) = lowered.graph {
//!     let report = rt.run(graph).await?;
//!     println!("{}", report.status);
//! }
//! # Ok(())
//! # }
//! ```

use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

pub use execution;
use frontend_gha::exprs::GITHUB_TOKEN_SECRET;
pub use runtime::{LocalExecutor, RunOptions, Runtime, driver, engine, ir};

pub mod host;

/// The executor interface, with the two local executors as submodules.
pub mod executor {
    pub use runtime::executor::*;
}

/// The frontend interface, with every format this distribution ships.
pub mod frontend {
    pub use frontend_gha as gha;
    pub use runtime::frontend::*;
}

/// Step kinds, with the standard registry.
pub mod steps {
    pub use runtime::steps::*;
}

/// The GitHub Actions component's run-time half: its step kinds and action
/// source.
pub mod github {
    pub use github_actions::*;
}

/// The shipped configuration: core's standard runtime plus every in-tree
/// component.
///
/// GitHub Actions comes with an action source that fetches from GitHub into
/// [`github::default_cache_dir`], its step kinds, and `GITHUB_TOKEN` as a
/// secret: this machine's (`$GITHUB_TOKEN`, else `gh auth token`), or the empty
/// string with a warning when it has none — actions then run anonymously. Its
/// runner map knows the `ubuntu-*` labels; `PETRI_RUNNER_LABELS` (labels
/// separated by commas or whitespace) adds third-party or self-hosted labels
/// that name Linux environments this machine can stand in for.
///
/// Every run also gets the per-run **ObjectService** — the local stand-in for
/// GitHub's results backend — started beside the run dir (artifacts under
/// `<run_dir>/artifacts`, released with the run) and handed to action steps as
/// `ACTIONS_RESULTS_URL`/`ACTIONS_RUNTIME_TOKEN`. When it cannot start, the
/// run proceeds and only the steps that need the backend fail, routably.
///
/// A consumer that wants a different set builds one itself —
/// `Runtime::standard()` for core alone, `Runtime::bare()` for nothing — and
/// registers what it wants.
pub fn runtime() -> Runtime {
    let secrets = GithubSecrets::new();
    // The provisioner registers the results token with this run's mask set, so
    // it is built ahead of the closure that captures its masker.
    let masker = executor::SecretProvider::masker(&secrets);
    let actions = Arc::new(github::GitActionSource::new(github::default_cache_dir()));
    let manifests: Arc<dyn github::ActionSource> = actions.clone();
    let trees: Arc<dyn github::ActionTreeSource> = actions;
    let runners = frontend_gha::RunnerMap::builtin()
        .allow_list(&env::var("PETRI_RUNNER_LABELS").unwrap_or_default());
    // `PETRI_REAL_CHECKOUT` (non-empty) turns the local-checkout substitution
    // off: every `actions/checkout` stays the real action, credentials,
    // network and all.
    let substitute_checkout = !env::var("PETRI_REAL_CHECKOUT").is_ok_and(|v| !v.is_empty());
    // The persistent store: cache entries and the per-OS tool cache under one
    // root ($PETRI_STORE overrides). Created now so the tool-cache prologue's
    // existence probe finds it on the host.
    let store = github_objects::default_store_dir();
    let tool_cache = github_objects::tool_cache_dir(&store);
    if let Err(error) = fs::create_dir_all(&tool_cache) {
        // The prologue's probe then finds nothing, and every `setup-*` action
        // reinstalls its tool on every run.
        tracing::warn!(
            path = %tool_cache.display(),
            error = ?error,
            "tool cache directory could not be created"
        );
    }
    Runtime::standard()
        .frontend(
            frontend_gha::GitHubActions::with_actions(manifests)
                .with_runners(runners)
                .with_checkout_substitution(substitute_checkout),
        )
        .step(github::RunStep)
        .step(github::ActionStep)
        .step(github::DockerActionStep)
        .step(github::CheckoutStep)
        .step(github::BackgroundStartStep)
        .step(github::BackgroundCompleteStep)
        .step(github::BackgroundPublishStep)
        .step(github::BackgroundWaitStep)
        .capability(github::ActionSourceCap(trees))
        .capability(github::ToolCacheCap(tool_cache))
        .run_services(move |run_dir, caps| {
            let cache = github_objects::cache_dir(&store);
            match github_objects::ObjectService::start(run_dir.join("artifacts"), cache) {
                Ok(service) => {
                    // `ACTIONS_RUNTIME_TOKEN` is a bearer credential for a
                    // beyond-loopback listener, injected into every action's
                    // env; registering it here is what makes the deny-list
                    // claim in `.ai/decisions/observability-tracing.md`
                    // ("object service tokens" are never loggable in the
                    // clear) hold for the step logs and the event log too.
                    masker.register(service.token());
                    let cap = github::ResultsServiceCap {
                        port:  service.port(),
                        token: service.token().into(),
                    };
                    (
                        caps.provide(cap),
                        Some(Box::new(ObjectServiceGuard(service)) as _),
                    )
                }
                Err(error) => {
                    tracing::warn!(error = ?error, "results service unavailable");
                    (caps, None)
                }
            }
        })
        .secrets(secrets)
}

/// The ObjectService as the driver's run guard: teardown is the service's
/// explicit async shutdown, so joining its thread never blocks a Tokio worker.
struct ObjectServiceGuard(github_objects::ObjectService);

#[async_trait::async_trait]
impl driver::RunGuard for ObjectServiceGuard {
    async fn teardown(self: Box<Self>) {
        self.0.shutdown().await;
    }
}

/// A runtime-registerable secret map plus a lazily loaded `GITHUB_TOKEN`.
struct GithubSecrets {
    registered: executor::MapSecrets,
    token:      OnceLock<Option<executor::Secret>>,
}

impl GithubSecrets {
    fn new() -> Self {
        Self {
            registered: executor::MapSecrets::empty(),
            token:      OnceLock::new(),
        }
    }

    /// The machine's token, or — with none configured — the empty string, said
    /// once. `github.token` always exists on GitHub; locally, empty is the
    /// honest analog the toolkit is built for: actions send no auth header and
    /// their API calls go anonymous (public reads work, rate-limited; writes
    /// fail with the API's own error). A missing-secret failure would instead
    /// stop every action that merely *names* the token, setup-* included.
    fn load_token() -> Option<executor::Secret> {
        let token = env::var("GITHUB_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty())
            .map(|t| executor::Secret::new(t.into()))
            .or_else(gh_auth_token);
        if token.is_none() {
            tracing::warn!(
                "no GITHUB_TOKEN (set it, or log in with `gh`); actions that use `github.token` \
                 run anonymously"
            );
        }
        token
    }
}

impl executor::SecretProvider for GithubSecrets {
    fn resolve(&self, name: &str) -> Result<executor::Secret, executor::SecretError> {
        match self.registered.resolve(name) {
            Ok(secret) => return Ok(secret),
            Err(executor::SecretError::Unknown(_)) => {}
            Err(error) => return Err(error),
        }
        if name != GITHUB_TOKEN_SECRET {
            return Err(executor::SecretError::Unknown(name.into()));
        }
        let value = self
            .token
            .get_or_init(Self::load_token)
            .clone()
            .map_or_else(Default::default, executor::Secret::expose);
        // An empty value never enters the mask set (the masker refuses short
        // values), so this is a no-op for the token-less case by construction.
        self.registered.masker().register(&value);
        Ok(executor::Secret::new(value))
    }

    fn register(&self, name: &str, value: &str) -> Result<(), executor::SecretError> {
        self.registered.register(name, value)
    }

    fn masker(&self) -> executor::Masker {
        self.registered.masker()
    }
}

/// Ask `gh` for a token without letting a broken credential helper block
/// forever.
fn gh_auth_token() -> Option<executor::Secret> {
    let mut child = Command::new("gh")
        .args(["auth", "token"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let output = child.wait_with_output().ok()?;
                return output
                    .status
                    .success()
                    .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
                    .filter(|token| !token.is_empty())
                    .map(|token| executor::Secret::new(token.into()));
            }
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}
