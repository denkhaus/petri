//! Petri, as it ships.
//!
//! This is the distribution: the one crate a consumer depends on, and the only
//! crate in the tree that names every component. Core is `petri-runtime` and knows
//! the native format and nothing else; each component — today, GitHub Actions —
//! is its own crate and knows core. Here the two meet: [`runtime()`] is core's
//! standard runtime with every in-tree component registered on it.
//!
//! Everything else is re-export. [`ir`], [`engine`], [`driver`], [`frontend`]
//! (with [`frontend::gha`] and [`frontend::native`]), [`steps`] and [`executor`]
//! (with [`executor::host`] and [`executor::docker`]) are reachable through this
//! crate, so an external repository names one dependency and never a layer crate.
//!
//! The one thing here that is not wiring or re-export is [`host`]: the
//! standalone host's durable run dir — `graph.json`, `events.jsonl`, and the
//! run wrappers that keep a run resumable when no product store sits behind it.
//!
//! ```no_run
//! # async fn demo() -> Result<(), String> {
//! let rt = petri::runtime().options(petri::RunOptions::new("/tmp/petri-demo"));
//! let lowered = rt.check(std::path::Path::new("pipeline.yml"), None, None)?;
//! if let Some(graph) = lowered.graph {
//!     let report = rt.run(graph).await.expect("replay is byte-identical");
//!     println!("{:?}", report.status);
//! }
//! # Ok(())
//! # }
//! ```

pub use runtime::{LocalExecutor, RunOptions, Runtime};
pub use runtime::{driver, engine, ir};

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

/// The GitHub Actions component's run-time half: its step kinds and action source.
pub mod github {
    pub use github_actions::*;
}

/// The shipped configuration: core's standard runtime plus every in-tree component.
///
/// GitHub Actions comes with an action source that fetches from GitHub into
/// [`github::default_cache_dir`], its step kinds, and `GITHUB_TOKEN` as a secret
/// when this machine has one (`$GITHUB_TOKEN`, else `gh auth token`). Its runner
/// map knows the `ubuntu-*` labels; `PETRI_RUNNER_LABELS` (labels separated by
/// commas or whitespace) adds third-party or self-hosted labels that name Linux
/// environments this machine can stand in for.
///
/// Every run also gets the per-run **ObjectService** — the local stand-in for
/// GitHub's results backend — started beside the run dir (artifacts under
/// `<run_dir>/artifacts`, released with the run) and handed to action steps as
/// `ACTIONS_RESULTS_URL`/`ACTIONS_RUNTIME_TOKEN`. When it cannot start, the
/// run proceeds and only the steps that need the backend fail, routably.
///
/// A consumer that wants a different set builds one itself — `Runtime::standard()`
/// for core alone, `Runtime::bare()` for nothing — and registers what it wants.
pub fn runtime() -> Runtime {
    let actions = std::sync::Arc::new(github::GitActionSource::new(github::default_cache_dir()));
    let manifests: std::sync::Arc<dyn github::ActionSource> = actions.clone();
    let trees: std::sync::Arc<dyn github::ActionTreeSource> = actions;
    let runners = frontend_gha::RunnerMap::builtin()
        .allow_list(&std::env::var("PETRI_RUNNER_LABELS").unwrap_or_default());
    // `PETRI_REAL_CHECKOUT` (non-empty) turns the local-checkout substitution
    // off: every `actions/checkout` stays the real action, credentials,
    // network and all.
    let substitute_checkout = !std::env::var("PETRI_REAL_CHECKOUT").is_ok_and(|v| !v.is_empty());
    // The persistent store: cache entries and the per-OS tool cache under one
    // root ($PETRI_STORE overrides). Created now so the tool-cache prologue's
    // existence probe finds it on the host.
    let store = github_objects::default_store_dir();
    let tool_cache = github_objects::tool_cache_dir(&store);
    let _ = std::fs::create_dir_all(&tool_cache);
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
        .capability(github::ActionSourceCap(trees))
        .capability(github::ToolCacheCap(tool_cache))
        .run_services(move |run_dir, caps| {
            let cache = github_objects::cache_dir(&store);
            match github_objects::ObjectService::start(run_dir.join("artifacts"), cache) {
                Ok(service) => {
                    let cap = github::ResultsServiceCap {
                        port: service.port(),
                        token: service.token().into(),
                    };
                    (caps.provide(cap), Some(Box::new(service) as _))
                }
                Err(error) => {
                    eprintln!("warning: no results service for this run: {error}");
                    (caps, None)
                }
            }
        })
        .secrets(GithubSecrets::new())
}

/// A runtime-registerable secret map plus a lazily loaded `GITHUB_TOKEN`.
struct GithubSecrets {
    registered: executor::MapSecrets,
    token: std::sync::OnceLock<Option<String>>,
}

impl GithubSecrets {
    fn new() -> Self {
        Self {
            registered: executor::MapSecrets::empty(),
            token: std::sync::OnceLock::new(),
        }
    }

    fn load_token() -> Option<String> {
        std::env::var("GITHUB_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty())
            .or_else(gh_auth_token)
    }
}

impl executor::SecretProvider for GithubSecrets {
    fn resolve(&self, name: &str) -> Result<executor::Secret, executor::SecretError> {
        match self.registered.resolve(name) {
            Ok(secret) => return Ok(secret),
            Err(executor::SecretError::Unknown(_)) => {}
            Err(error) => return Err(error),
        }
        if name != frontend_gha::exprs::GITHUB_TOKEN_SECRET {
            return Err(executor::SecretError::Unknown(name.into()));
        }
        let value = self
            .token
            .get_or_init(Self::load_token)
            .as_deref()
            .ok_or_else(|| executor::SecretError::Unknown(name.into()))?;
        self.registered.masker().register(value);
        Ok(executor::Secret::new(value.into()))
    }

    fn register(&self, name: &str, value: &str) -> Result<(), executor::SecretError> {
        self.registered.register(name, value)
    }

    fn masker(&self) -> executor::Masker {
        self.registered.masker()
    }
}

/// Ask `gh` for a token without letting a broken credential helper block forever.
fn gh_auth_token() -> Option<String> {
    let mut child = std::process::Command::new("gh")
        .args(["auth", "token"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let output = child.wait_with_output().ok()?;
                return output
                    .status
                    .success()
                    .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
                    .filter(|token| !token.is_empty());
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}
