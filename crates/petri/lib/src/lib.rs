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
//! [`executor`] (with [`executor::sandbox`], the executors) are
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
//! let inputs = petri::frontend::CompileInputs::new();
//! let lowered = rt.check(std::path::Path::new("pipeline.yml"), None, None, &inputs)?;
//! if let Some(graph) = lowered.graph {
//!     let report = rt.run(graph).await?;
//!     println!("{}", report.status);
//! }
//! # Ok(())
//! # }
//! ```

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use std::{env, fs, io, thread};

pub use execution::{self, host};
use fabro_steps::pebble::PebbleClient;
use frontend_gha::exprs::GITHUB_TOKEN_SECRET;
use lithos_llm::catalog::{Catalog, CatalogError};
use lithos_llm::client::ClientBuildError;
use lithos_llm::credentials::EnvironmentCredentials;
pub use runtime::{
    DaytonaResources, DaytonaSandboxKind, RunOptions, Runtime, SandboxBackend, SandboxOptions,
    driver, engine, ir,
};

/// The executor interface, with the sandbox-driver adapter as a submodule.
pub mod executor {
    pub use runtime::executor::*;
}

/// The frontend interface, with every format this distribution ships.
pub mod frontend {
    pub use frontend_fabro as fabro;
    pub use frontend_gha as gha;
    pub use runtime::frontend::*;
}

/// The Fabro component's run-time half: its step kinds and the stub registry.
pub mod fabro {
    pub use fabro_steps::*;
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
    assemble(fabro_steps::register)
}

/// [`runtime`] with the Fabro step kinds simulated — `petri run --dry-run`:
/// every Fabro stage succeeds and a human gate takes its first choice, as
/// Fabro's own `--dry-run` does. The GitHub Actions kinds have no simulation
/// and run for real.
pub fn dry_run_runtime() -> Runtime {
    assemble(fabro_steps::register_stubs)
}

fn assemble(fabro: fn(Runtime) -> Runtime) -> Runtime {
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
    let runtime = Runtime::standard()
        .frontend(frontend_fabro::Fabro::new())
        .frontend(
            frontend_gha::GitHubActions::with_actions(manifests.clone())
                .with_runners(runners)
                .with_checkout_substitution(substitute_checkout),
        );
    let runtime = match llm_client() {
        Ok(client) => runtime.capability(PebbleClient(client)),
        Err(error) => {
            tracing::warn!(error = %error, "native Pebble client unavailable");
            runtime
        }
    };
    github::register(fabro(runtime))
        .capability(github::ActionSourceCap(trees))
        .capability(github::ActionManifestSourceCap(manifests))
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

/// The environment variable naming extra catalog layers for the model client:
/// one or more paths to `lithos-llm` catalog TOML files, separated by the
/// platform's path separator, layered over the built-in catalog in order. A
/// layer redirects a provider (`[providers.openai] base_url = "http://…"`),
/// adds models, or changes their metadata. A path that cannot be read or does
/// not parse is an error: the client is not built and every native agent node
/// fails with `pebble_unconfigured`, rather than reaching a live provider.
pub const LLM_CATALOG_ENV: &str = "PETRI_LLM_CATALOG";

/// The environment variable naming which providers the model client may
/// route to: provider ids separated by commas. Unset, every built-in provider
/// with credentials is available. Set, a model on any other provider is
/// unavailable, so a test that redirects `openai` and `anthropic` to loopback
/// cannot reach a third provider by accident.
pub const LLM_PROVIDERS_ENV: &str = "PETRI_LLM_PROVIDERS";

/// Why the model client could not be built.
#[derive(Debug, thiserror::Error)]
pub enum LlmClientError {
    #[error("could not read the {LLM_CATALOG_ENV} layer `{path}`")]
    ReadLayer {
        path:   PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("the model catalog is invalid")]
    Catalog(#[source] CatalogError),
    #[error("the model client could not be built")]
    Client(#[source] ClientBuildError),
}

/// The model client native Pebble sessions use, configured the way this
/// distribution documents.
///
/// Credentials come from `lithos-llm`'s conventional environment variables
/// (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, and so on), read per request, never
/// at build time. Endpoints come from the built-in catalog, with
/// [`LLM_CATALOG_ENV`] layers on top, and [`LLM_PROVIDERS_ENV`] narrows which
/// providers are available at all. Both are read from this process's
/// environment only; a harness sets them on the child it launches and leaves
/// the developer's shell alone.
pub fn llm_client() -> Result<lithos_llm::Client, LlmClientError> {
    let mut catalog = Catalog::builder().with_builtin();
    if let Some(layers) = env::var_os(LLM_CATALOG_ENV) {
        for path in env::split_paths(&layers).filter(|p| !p.as_os_str().is_empty()) {
            let text = fs::read_to_string(&path).map_err(|source| LlmClientError::ReadLayer {
                path: path.clone(),
                source,
            })?;
            catalog = catalog
                .toml_layer(path.display().to_string(), &text)
                .map_err(LlmClientError::Catalog)?;
        }
    }
    let catalog = catalog.build().map_err(LlmClientError::Catalog)?;
    let mut builder = lithos_llm::Client::builder()
        .catalog(catalog)
        .credentials(EnvironmentCredentials::conventional());
    if let Ok(providers) = env::var(LLM_PROVIDERS_ENV) {
        let enabled: Vec<String> = providers
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect();
        builder = builder.enabled_providers(enabled);
    }
    let build = builder.build().map_err(LlmClientError::Client)?;
    for issue in &build.issues {
        tracing::debug!(provider = %issue.provider, cause = %issue.cause, "provider unavailable");
    }
    Ok(build.client)
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
