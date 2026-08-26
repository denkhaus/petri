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

use driver::{Driver, RunConfig, RunReport};
use engine::ReplayMismatch;
use executor::{DEFAULT_GRACE, Executor, MapSecrets, Retention, SecretProvider};
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
    options: RunOptions,
}

impl Runtime {
    /// The standard configuration: the `gha` and `native` frontends, the `noop` and
    /// `process` step kinds, no secrets, and — unless [`Runtime::executors`]
    /// overrides it — a host executor and a Docker executor dispatched by each
    /// scope's [`ir::RuntimeTarget`].
    ///
    /// The default run directory is under the system temp dir; set a real one with
    /// [`Runtime::options`].
    pub fn standard() -> Self {
        Self {
            frontends: vec![
                Arc::new(frontend_gha::Gha),
                Arc::new(frontend_native::Native),
            ],
            steps: crate::steps::standard(),
            executor: None,
            secrets: Arc::new(MapSecrets::empty()),
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

    pub fn options(mut self, options: RunOptions) -> Self {
        self.options = options;
        self
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

    /// Where the repository root is, for a workflow file: the nearest ancestor with
    /// a `.github` directory, else the file's own directory.
    pub fn guess_repo(file: &Path) -> PathBuf {
        let mut dir = file
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let start = dir.clone();
        loop {
            if dir.join(".github").is_dir() {
                return dir;
            }
            match dir.parent() {
                Some(parent) => dir = parent.to_path_buf(),
                None => return start,
            }
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
            .unwrap_or_else(|| Self::guess_repo(file));
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
        let mut config = RunConfig::new(&self.options.run_dir)
            .with_grace(self.options.grace)
            .with_retention(self.options.retention)
            .echoing(self.options.echo);
        config.hard_deadline_slack = self.options.hard_deadline_slack;
        let executor = self
            .executor
            .clone()
            .unwrap_or_else(|| self.default_executor());
        Driver::new(
            graph,
            executor,
            self.steps.clone(),
            Arc::clone(&self.secrets),
            config,
        )
    }

    /// Run a graph to completion. With `verify_replay` on (the default), the log is
    /// replayed afterwards and any divergence is the error.
    pub async fn run(&self, graph: Graph) -> Result<RunReport, ReplayMismatch> {
        let original = self.options.verify_replay.then(|| graph.clone());
        let report = self.driver(graph).run().await;
        if let Some(graph) = original {
            engine::verify_replay(graph, &report.state.log)?;
        }
        Ok(report)
    }

    /// Host and Docker executors over the run dir, dispatched by target.
    fn default_executor(&self) -> Arc<dyn Executor> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let run_id = format!(
            "{}x{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        Arc::new(
            TargetExecutor::new()
                .host(
                    executor_host::HostExecutor::new(&self.options.run_dir)
                        .with_retention(self.options.retention),
                )
                .container(
                    executor_docker::DockerExecutor::new(&self.options.run_dir, &run_id)
                        .with_retention(self.options.retention),
                ),
        )
    }
}
