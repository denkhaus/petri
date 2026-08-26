//! The standard configuration, as a builder.
//!
//! Every place that runs a graph used to wire the same pieces by hand: a registry
//! with `noop` and `process`, an executor, a secret provider, a `RunConfig`, a
//! driver, and the replay canary. [`Runtime`] is that wiring, written once. The CLI,
//! the acceptance harness, and an external repository all configure the same
//! builder; extension is registration, not new plumbing.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use driver::{Driver, EventObserver, ResumeError, ResumeInfo, RunConfig, RunReport};
use engine::{EventLog, ReplayMismatch};
use executor::{DEFAULT_GRACE, Executor, MapSecrets, Masker, Retention, SecretProvider};
use frontend::{DirFiles, Frontend, Lowered, Span};
use ir::Graph;

use crate::target::TargetExecutor;

/// The knobs a run gets, with the defaults the driver documents.
#[derive(Clone, Debug)]
pub struct RunOptions {
    /// Where workspaces and logs live.
    pub run_dir: PathBuf,
    /// Between `SIGTERM` and `SIGKILL`, per scope.
    pub grace: Duration,
    /// How much longer than `grace` a step gets before the driver stops waiting.
    pub hard_deadline_slack: Duration,
    /// Between the first root cancel and the `KillRequested` that ends whatever
    /// cleanup is still running.
    pub cleanup_grace: Duration,
    pub retention: Retention,
    /// Echo step output to this process's stdout.
    pub echo: bool,
    /// Replay the log after the run and fail on any divergence. The determinism
    /// canary; on by default.
    pub verify_replay: bool,
}

impl RunOptions {
    pub fn new(run_dir: impl Into<PathBuf>) -> Self {
        Self {
            run_dir: run_dir.into(),
            grace: DEFAULT_GRACE,
            hard_deadline_slack: Duration::from_secs(5),
            cleanup_grace: driver::DEFAULT_CLEANUP_GRACE,
            retention: Retention::default(),
            echo: false,
            verify_replay: true,
        }
    }
}

/// The assembled system: frontends, step kinds, executors, secrets, options.
pub struct Runtime {
    frontends: Vec<Arc<dyn Frontend>>,
    steps: ::steps::Registry,
    executor: Option<Arc<dyn Executor>>,
    secrets: Arc<dyn SecretProvider>,
    observers: Vec<Arc<dyn EventObserver>>,
    caps: ::steps::CapabilitiesBuilder,
    options: RunOptions,
}

impl Runtime {
    /// The standard configuration: the formats and step kinds core itself owns —
    /// the `native` frontend, the `noop` and `process` steps — no secrets, and,
    /// unless [`Runtime::executors`] overrides it, a host executor and a Docker
    /// executor dispatched by each scope's [`ir::RuntimeTarget`].
    ///
    /// A distribution or a consumer registers its own frontends on top with
    /// [`Runtime::frontend`]; each one goes to the front of the list, ahead of the
    /// native catch-all.
    ///
    /// The default run directory is under the system temp dir; set a real one with
    /// [`Runtime::options`].
    pub fn standard() -> Self {
        Self {
            frontends: vec![Arc::new(frontend_native::Native)],
            steps: crate::steps::standard(),
            executor: None,
            secrets: Arc::new(MapSecrets::empty()),
            observers: Vec::new(),
            caps: ::steps::Capabilities::builder(),
            options: RunOptions::new(
                std::env::temp_dir().join(format!("petri-run-{}", std::process::id())),
            ),
        }
    }

    /// No frontends, no step kinds: for a consumer that assembles everything itself.
    pub fn bare() -> Self {
        Self {
            frontends: Vec::new(),
            steps: ::steps::Registry::new(),
            executor: None,
            secrets: Arc::new(MapSecrets::empty()),
            observers: Vec::new(),
            caps: ::steps::Capabilities::builder(),
            options: RunOptions::new(
                std::env::temp_dir().join(format!("petri-run-{}", std::process::id())),
            ),
        }
    }

    /// Register a frontend. It goes to the front of the list, so a specific format
    /// is asked before the native catch-all.
    pub fn frontend(mut self, frontend: impl Frontend + 'static) -> Self {
        self.frontends.insert(0, Arc::new(frontend));
        self
    }

    /// Replace the step registry.
    pub fn steps(mut self, registry: ::steps::Registry) -> Self {
        self.steps = registry;
        self
    }

    /// Register one step kind on the current registry.
    pub fn step<S: ::steps::Step>(mut self, step: S) -> Self {
        self.steps.register(step);
        self
    }

    /// Route scopes by [`ir::RuntimeTarget`] through this dispatcher.
    pub fn executors(mut self, executors: TargetExecutor) -> Self {
        self.executor = Some(Arc::new(executors));
        self
    }

    /// Use one executor for every scope, whatever its target.
    pub fn executor(mut self, executor: impl Executor + 'static) -> Self {
        self.executor = Some(Arc::new(executor));
        self
    }

    pub fn secrets(mut self, secrets: impl SecretProvider + 'static) -> Self {
        self.secrets = Arc::new(secrets);
        self
    }

    /// Register an event observer on every driver this runtime builds: it sees
    /// every appended record, in seq order, with the post-apply state.
    pub fn observe(mut self, observer: Arc<dyn EventObserver>) -> Self {
        self.observers.push(observer);
        self
    }

    /// Register a host service for steps, keyed by its concrete type: every
    /// step this runtime's drivers run can ask for it through `StepCtx`.
    ///
    /// # Panics
    ///
    /// On a duplicate type — registration is configuration, the same rule as
    /// step registration. A host with per-run services builds its own
    /// [`::steps::Capabilities`] and calls `Driver::with_capabilities` instead.
    pub fn capability<T: Send + Sync + 'static>(mut self, value: T) -> Self {
        self.caps = self.caps.provide(value);
        self
    }

    pub fn options(mut self, options: RunOptions) -> Self {
        self.options = options;
        self
    }

    /// The options runs get, for a host wrapping this runtime — the standalone
    /// petri host reads the run directory here.
    pub fn run_options(&self) -> &RunOptions {
        &self.options
    }

    /// The step registry, for lookups (`type_known`-style lints, validation).
    pub fn registry(&self) -> &::steps::Registry {
        &self.steps
    }

    // ── Frontends ──────────────────────────────────────────────────────────

    /// The frontend for a file: by `name` when given, else the first that claims
    /// the path.
    pub fn frontend_for(&self, path: &Path, name: Option<&str>) -> Result<&dyn Frontend, String> {
        let all: Vec<&dyn Frontend> = self.frontends.iter().map(|f| f.as_ref()).collect();
        match name {
            Some(name) => frontend::by_name(&all, name).ok_or_else(|| {
                let known: Vec<&str> = all.iter().map(|f| f.name()).collect();
                format!(
                    "unknown format `{name}`; known formats: {}",
                    known.join(", ")
                )
            }),
            None => frontend::detect(&all, path)
                .ok_or_else(|| format!("no frontend claims `{}`", path.display())),
        }
    }

    /// Read and lower one file. `Err` is an IO-or-usage problem; a rejected
    /// workflow comes back as `Ok` with diagnostics and no graph.
    pub fn lower(
        &self,
        file: &Path,
        format: Option<&str>,
        repo: Option<&Path>,
    ) -> Result<Lowered, String> {
        let frontend = self.frontend_for(file, format)?;
        let repo = repo
            .map(Path::to_path_buf)
            .unwrap_or_else(|| frontend.repo_root(file));
        let text = std::fs::read_to_string(file)
            .map_err(|e| format!("could not read {}: {e}", file.display()))?;
        let name = file
            .strip_prefix(&repo)
            .unwrap_or(file)
            .to_string_lossy()
            .into_owned();
        let files = DirFiles { root: repo };
        Ok(frontend.load(&name, &text, &files))
    }

    /// [`Runtime::lower`], then validate the graph against the step registry, so an
    /// unregistered kind or a bad literal config is a diagnostic here rather than a
    /// step failure at firing time.
    pub fn check(
        &self,
        file: &Path,
        format: Option<&str>,
        repo: Option<&Path>,
    ) -> Result<Lowered, String> {
        let mut lowered = self.lower(file, format, repo)?;
        if let Some(graph) = lowered.graph.take() {
            match ir::validate_with(&graph, Some(&self.steps)) {
                Ok(()) => lowered.graph = Some(graph),
                Err(errors) => {
                    let span = Span::file(file.to_string_lossy().as_ref());
                    for error in errors {
                        let code = match &error {
                            ir::ValidationError::UnknownStepKind { .. } => "step.unknown_kind",
                            ir::ValidationError::BadStepConfig { .. } => "step.bad_config",
                            _ => "validate",
                        };
                        lowered
                            .diagnostics
                            .error(code, span.clone(), error.to_string());
                    }
                }
            }
        }
        Ok(lowered)
    }

    // ── Running ────────────────────────────────────────────────────────────

    /// A driver over this configuration, for callers that need the handle (to
    /// cancel a run in flight). [`Runtime::run`] is the plain path.
    pub fn driver(&self, graph: Graph) -> Driver {
        let driver = Driver::new(
            graph,
            self.run_executor(),
            self.steps.clone(),
            Arc::clone(&self.secrets),
            self.run_config(),
        );
        self.equip(driver)
    }

    /// A driver continuing a crashed run's log, however the host stored it —
    /// the mirror of [`Runtime::driver`] over `Driver::resume`. `ResumeInfo`
    /// comes back beside the driver, so a host installs its own execution
    /// identities for the re-dispatched firings before calling `run()`.
    pub fn resume_driver(
        &self,
        graph: Graph,
        log: EventLog,
    ) -> Result<(Driver, ResumeInfo), ResumeError> {
        let (driver, info) = Driver::resume(
            graph,
            log,
            self.run_executor(),
            self.steps.clone(),
            Arc::clone(&self.secrets),
            self.run_config(),
        )?;
        Ok((self.equip(driver), info))
    }

    fn run_config(&self) -> RunConfig {
        let mut config = RunConfig::new(&self.options.run_dir)
            .with_grace(self.options.grace)
            .with_cleanup_grace(self.options.cleanup_grace)
            .with_retention(self.options.retention)
            .echoing(self.options.echo);
        config.hard_deadline_slack = self.options.hard_deadline_slack;
        config
    }

    fn run_executor(&self) -> Arc<dyn Executor> {
        self.executor
            .clone()
            .unwrap_or_else(|| self.default_executor())
    }

    /// The registrations every driver gets, whichever way it was built.
    fn equip(&self, mut driver: Driver) -> Driver {
        driver = driver.with_capabilities(self.caps.clone().build());
        for observer in &self.observers {
            driver = driver.observe(Arc::clone(observer));
        }
        driver
    }

    /// Run a graph to completion. With `verify_replay` on (the default), the log is
    /// replayed afterwards and any divergence is the error.
    pub async fn run(&self, graph: Graph) -> Result<RunReport, ReplayMismatch> {
        self.run_verified(graph, |graph| Ok(self.driver(graph)))
            .await
    }

    /// Run the driver `build` makes over `graph` to completion, with the same
    /// verification as [`Runtime::run`]: `verify_replay` on (the default) replays
    /// the log against the graph as it was before the run, and any divergence is
    /// the error. For hosts that build their own driver — one with observers
    /// attached, or a resumed one.
    pub async fn run_verified<E: From<ReplayMismatch>>(
        &self,
        graph: Graph,
        build: impl FnOnce(Graph) -> Result<Driver, E>,
    ) -> Result<RunReport, E> {
        let original = self.options.verify_replay.then(|| graph.clone());
        let report = build(graph)?.run().await;
        if let Some(graph) = original {
            engine::verify_replay(graph, &report.state.log)?;
        }
        Ok(report)
    }

    /// The mask set of the configured `SecretProvider`, for a host that persists
    /// anything beside the run.
    pub fn masker(&self) -> Masker {
        self.secrets.masker()
    }

    /// Host and Docker executors over the run dir, dispatched by target. Both
    /// take their identity from the run dir, so a resumed run's executors reach
    /// the crashed run's environments.
    fn default_executor(&self) -> Arc<dyn Executor> {
        Arc::new(
            TargetExecutor::new()
                .host(
                    executor_host::HostExecutor::new(&self.options.run_dir)
                        .with_retention(self.options.retention),
                )
                .container(
                    executor_docker::DockerExecutor::new(&self.options.run_dir)
                        .with_retention(self.options.retention),
                ),
        )
    }
}
