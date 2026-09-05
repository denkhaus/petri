//! The standard configuration, as a builder.
//!
//! Every place that runs a graph used to wire the same pieces by hand: a
//! registry with `noop` and `process`, an executor, a secret provider, a
//! `RunConfig`, a driver, and the replay canary. [`Runtime`] is that wiring,
//! written once. The CLI, the acceptance harness, and an external repository
//! all configure the same builder; extension is registration, not new plumbing.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use std::{env, fs, io, mem, process};

use driver::{
    Driver, EventObserver, ExecutionReport, ResumeError, ResumeInfo, RunConfig, RunGuard,
    SandboxAssignment,
};
use engine::{EngineStart, EventLog, ReplayMismatch};
use executor::{
    DEFAULT_GRACE, Executor, MapSecrets, Masker, ProgressSink, Retention, SecretProvider,
};
use executor_sandbox::{LeaseLedger, RoutingExecutor, SandboxOptions};
use frontend::{CompileInputs, DirFiles, Frontend, Lowered, Span};
use ir::{Graph, RunStatus};
use tracing::field::Empty;

/// The knobs a run gets, with the defaults the driver documents.
#[derive(Clone, Debug)]
pub struct RunOptions {
    /// Where workspaces and logs live.
    pub run_dir:             PathBuf,
    /// Between `SIGTERM` and `SIGKILL`, per scope.
    pub grace:               Duration,
    /// How much longer than `grace` a step gets before the driver stops
    /// waiting.
    pub hard_deadline_slack: Duration,
    /// Between the first root cancel and the `KillRequested` that ends whatever
    /// cleanup is still running.
    pub cleanup_grace:       Duration,
    pub retention:           Retention,
    /// Echo step output to this process's stdout.
    pub echo:                bool,
    /// Replay the log after the run and fail on any divergence. The determinism
    /// canary; on by default.
    pub verify_replay:       bool,
    pub sandbox:             SandboxOptions,
}

impl RunOptions {
    pub fn new(run_dir: impl Into<PathBuf>) -> Self {
        Self {
            run_dir:             run_dir.into(),
            grace:               DEFAULT_GRACE,
            hard_deadline_slack: Duration::from_secs(5),
            cleanup_grace:       driver::DEFAULT_CLEANUP_GRACE,
            retention:           Retention::default(),
            echo:                false,
            verify_replay:       true,
            sandbox:             SandboxOptions::default(),
        }
    }
}

/// Why a workflow file could not be loaded at all: an IO-or-usage problem, as
/// opposed to a rejected workflow, which comes back as diagnostics.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// `--format` (or its API equivalent) named a format no registered
    /// frontend answers to.
    #[error("unknown format `{name}`; known formats: {}", known.join(", "))]
    UnknownFormat { name: String, known: Vec<String> },
    /// No `--format` was given and no registered frontend claims the path.
    #[error("no frontend claims `{}`", path.display())]
    NoFrontend { path: PathBuf },
    #[error("could not read {}", path.display())]
    Read {
        path:   PathBuf,
        #[source]
        source: io::Error,
    },
}

/// What a per-run service provisioner returns beside the capabilities: the
/// running service, opaque to the runtime. The driver holds it for the run's
/// lifetime and awaits its [`RunGuard::teardown`] when the run ends; drop is
/// the fallback for a run that never finishes.
pub type RunServiceGuard = Box<dyn RunGuard>;

/// A per-run service provisioner — see [`Runtime::run_services`].
type RunProvisioner = Arc<
    dyn Fn(
            &Path,
            ::steps::CapabilitiesBuilder,
        ) -> (::steps::CapabilitiesBuilder, Option<RunServiceGuard>)
        + Send
        + Sync,
>;

/// The assembled system: frontends, step kinds, executors, secrets, options.
pub struct Runtime {
    frontends:    Vec<Box<dyn Frontend>>,
    steps:        ::steps::Registry,
    executor:     Option<Arc<dyn Executor>>,
    secrets:      Arc<dyn SecretProvider>,
    observers:    Vec<Arc<dyn EventObserver>>,
    progress:     Option<Arc<dyn ProgressSink>>,
    caps:         ::steps::CapabilitiesBuilder,
    provisioners: Vec<RunProvisioner>,
    options:      RunOptions,
}

impl Runtime {
    /// The standard configuration: the formats and step kinds core itself owns
    /// — the `native` frontend, the `noop` and `process` steps — no
    /// secrets, and, unless [`Runtime::executor`] overrides it, the
    /// [`RoutingExecutor`] dispatching by each scope's
    /// [`ir::RuntimeTarget`].
    ///
    /// A distribution or a consumer registers its own frontends on top with
    /// [`Runtime::frontend`]; each one goes to the front of the list, ahead of
    /// the native catch-all.
    ///
    /// The default run directory is under the system temp dir; set a real one
    /// with [`Runtime::options`].
    #[expect(
        clippy::absolute_paths,
        reason = "this crate's own `steps` module and the external `steps` crate differ only \
                  by the leading `::`; spelling the local one in full keeps the two apart \
                  beside the `::steps::` uses a few lines below"
    )]
    pub fn standard() -> Self {
        Self {
            frontends:    vec![Box::new(frontend_native::Native)],
            steps:        crate::steps::standard(),
            executor:     None,
            secrets:      Arc::new(MapSecrets::empty()),
            observers:    Vec::new(),
            progress:     None,
            caps:         ::steps::Capabilities::builder(),
            provisioners: Vec::new(),
            options:      RunOptions::new(
                env::temp_dir().join(format!("petri-run-{}", process::id())),
            ),
        }
    }

    /// No frontends, no step kinds: for a consumer that assembles everything
    /// itself.
    pub fn bare() -> Self {
        Self {
            frontends:    Vec::new(),
            steps:        ::steps::Registry::new(),
            executor:     None,
            secrets:      Arc::new(MapSecrets::empty()),
            observers:    Vec::new(),
            progress:     None,
            caps:         ::steps::Capabilities::builder(),
            provisioners: Vec::new(),
            options:      RunOptions::new(
                env::temp_dir().join(format!("petri-run-{}", process::id())),
            ),
        }
    }

    /// Register a frontend. It goes to the front of the list, so a specific
    /// format is asked before the native catch-all.
    #[must_use]
    pub fn frontend(mut self, frontend: impl Frontend + 'static) -> Self {
        self.frontends.insert(0, Box::new(frontend));
        self
    }

    /// Replace the step registry.
    #[must_use]
    pub fn steps(mut self, registry: ::steps::Registry) -> Self {
        self.steps = registry;
        self
    }

    /// Register one step kind on the current registry.
    #[must_use]
    pub fn step<S: ::steps::Step>(mut self, step: S) -> Self {
        self.steps.register(step);
        self
    }

    /// Use one executor for every scope, whatever its target — the
    /// [`RoutingExecutor`] included.
    #[must_use]
    pub fn executor(mut self, executor: impl Executor + 'static) -> Self {
        self.executor = Some(Arc::new(executor));
        self
    }

    #[must_use]
    pub fn secrets(mut self, secrets: impl SecretProvider + 'static) -> Self {
        self.secrets = Arc::new(secrets);
        self
    }

    /// Register an event observer on every driver this runtime builds: it sees
    /// every appended record, in seq order, with the post-apply state.
    #[must_use]
    pub fn observe(mut self, observer: Arc<dyn EventObserver>) -> Self {
        self.observers.push(observer);
        self
    }

    /// Register a progress sink on every driver this runtime builds: live
    /// acquisition events — image pulls, service health — which never enter
    /// the replay log.
    #[must_use]
    pub fn progress(mut self, sink: Arc<dyn ProgressSink>) -> Self {
        self.progress = Some(sink);
        self
    }

    /// Register a host service for steps, keyed by its concrete type: every
    /// step this runtime's drivers run can ask for it through `StepCtx`.
    ///
    /// # Panics
    ///
    /// On a duplicate type — registration is configuration, the same rule as
    /// step registration. A service with *run* lifetime registers a
    /// [`Runtime::run_services`] provisioner instead; a host assembling its
    /// own driver can also build its own [`::steps::Capabilities`] and call
    /// `Driver::with_capabilities` directly.
    #[must_use]
    pub fn capability<T: Send + Sync + 'static>(mut self, value: T) -> Self {
        self.caps = self.caps.provide(value);
        self
    }

    /// Register a per-run service provisioner: called once per driver this
    /// runtime builds — fresh and resumed runs alike — with the run directory,
    /// to stand a run-scoped host service up and hand its capability to the
    /// run's steps. The guard it returns rides the driver; dropping the driver
    /// is the teardown. A provisioner that cannot start its service returns
    /// the capabilities unchanged and no guard — the steps that need it then
    /// fail routably (`capability_unavailable`), never the run.
    #[must_use]
    pub fn run_services<F>(mut self, provision: F) -> Self
    where
        F: Fn(
                &Path,
                ::steps::CapabilitiesBuilder,
            ) -> (::steps::CapabilitiesBuilder, Option<RunServiceGuard>)
            + Send
            + Sync
            + 'static,
    {
        self.provisioners.push(Arc::new(provision));
        self
    }

    #[must_use]
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

    /// The frontend for a file: by `name` when given, else the first that
    /// claims the path.
    pub fn frontend_for(
        &self,
        path: &Path,
        name: Option<&str>,
    ) -> Result<&dyn Frontend, LoadError> {
        let all: Vec<&dyn Frontend> = self.frontends.iter().map(AsRef::as_ref).collect();
        match name {
            Some(name) => frontend::by_name(&all, name).ok_or_else(|| LoadError::UnknownFormat {
                name:  name.to_string(),
                known: all.iter().map(|f| f.name().to_string()).collect(),
            }),
            None => frontend::detect(&all, path).ok_or_else(|| LoadError::NoFrontend {
                path: path.to_path_buf(),
            }),
        }
    }

    /// Read and lower one file. `Err` is an IO-or-usage problem; a rejected
    /// workflow comes back as `Ok` with diagnostics and no graph.
    ///
    /// The span is the only place that knows which frontend claimed the file;
    /// the frontends themselves stay free of tracing, with diagnostics as their
    /// one output channel.
    #[tracing::instrument(
        name = "runtime.lower",
        level = "debug",
        skip_all,
        fields(
            workflow_file = %file.display(),
            frontend = Empty,
            node_count = Empty,
        )
    )]
    pub fn lower(
        &self,
        file: &Path,
        format: Option<&str>,
        repo: Option<&Path>,
        inputs: &CompileInputs,
    ) -> Result<Lowered, LoadError> {
        let frontend = self.frontend_for(file, format)?;
        let span = tracing::Span::current();
        span.record("frontend", frontend.name());
        let repo = repo.map_or_else(|| frontend.repo_root(file), Path::to_path_buf);
        let text = fs::read_to_string(file).map_err(|e| LoadError::Read {
            path:   file.to_path_buf(),
            source: e,
        })?;
        let name = file
            .strip_prefix(&repo)
            .unwrap_or(file)
            .to_string_lossy()
            .into_owned();
        let files = DirFiles { root: repo };
        let lowered = frontend.load(&name, &text, &files, inputs);
        if let Some(graph) = &lowered.graph {
            span.record("node_count", graph.nodes.len());
        }
        Ok(lowered)
    }

    /// [`Runtime::lower`], then validate the graph — and every pre-lowered
    /// child graph — against the step registry, so an unregistered kind or a
    /// bad literal config is a diagnostic here rather than a step failure at
    /// firing time. Only the registry pass runs: the frontend already ran the
    /// structural passes when it lowered.
    pub fn check(
        &self,
        file: &Path,
        format: Option<&str>,
        repo: Option<&Path>,
        inputs: &CompileInputs,
    ) -> Result<Lowered, LoadError> {
        let mut lowered = self.lower(file, format, repo, inputs)?;
        let span = Span::file(file.to_string_lossy().as_ref());
        let mut errors = Vec::new();
        if let Some(graph) = &lowered.graph {
            errors.extend(
                ir::validate_step_kinds(graph, &self.steps)
                    .err()
                    .into_iter()
                    .flatten(),
            );
        }
        for child in &lowered.children {
            errors.extend(
                ir::validate_step_kinds(child, &self.steps)
                    .err()
                    .into_iter()
                    .flatten(),
            );
        }
        if !errors.is_empty() {
            lowered.graph = None;
            lowered.children.clear();
            for error in errors {
                lowered
                    .diagnostics
                    .error(error.code(), span.clone(), error.to_string());
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
            self.secrets.clone(),
            self.run_config(),
        );
        self.equip(driver)
    }

    /// Prepare resources that are shared by every execution in one root run.
    pub fn prepare_run(&self, run_dir: impl Into<PathBuf>) -> RunRuntime {
        let run_dir = run_dir.into();
        // The standard router is kept by its own type too: the coordinator
        // hands it the lease ledger and releases leases through it. A
        // caller-supplied executor manages its own sandboxes.
        let (executor, router): (Arc<dyn Executor>, Option<Arc<RoutingExecutor>>) =
            if let Some(executor) = self.executor.clone() {
                (executor, None)
            } else {
                let router = self.default_router_for(&run_dir);
                (router.clone(), Some(router))
            };
        let (caps, guards) = self.provision(&run_dir);
        RunRuntime {
            run_dir,
            options: self.options.clone(),
            executor,
            router,
            steps: self.steps.clone(),
            secrets: self.secrets.clone(),
            observers: self.observers.clone(),
            progress: self.progress.clone(),
            caps,
            guards,
        }
    }

    /// Run every registered per-run service provisioner once, and collect the
    /// capabilities and guards it leaves behind.
    fn provision(&self, run_dir: &Path) -> (::steps::Capabilities, Vec<RunServiceGuard>) {
        let mut caps = self.caps.clone();
        let mut guards = Vec::new();
        for provision in &self.provisioners {
            let (next, guard) = provision(run_dir, caps);
            caps = next;
            guards.extend(guard);
        }
        (caps.build(), guards)
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
            self.secrets.clone(),
            self.run_config(),
        )?;
        Ok((self.equip(driver), info))
    }

    fn run_config(&self) -> RunConfig {
        base_run_config(&self.options, self.options.run_dir.clone())
            .with_retention(self.options.retention)
    }

    fn run_executor(&self) -> Arc<dyn Executor> {
        self.executor
            .clone()
            .unwrap_or_else(|| self.default_executor())
    }

    /// The registrations every driver gets, whichever way it was built.
    fn equip(&self, mut driver: Driver) -> Driver {
        let (caps, guards) = self.provision(&self.options.run_dir);
        for guard in guards {
            driver = driver.with_run_guard(guard);
        }
        driver = driver.with_capabilities(caps);
        attach(driver, &self.observers, self.progress.as_ref())
    }

    /// Run a graph to completion. With `verify_replay` on (the default), the
    /// log is replayed afterwards and any divergence is the error.
    pub async fn run(&self, graph: Graph) -> Result<ExecutionReport, ReplayMismatch> {
        self.run_verified(graph, |graph| Ok(self.driver(graph)))
            .await
    }

    /// Run the driver `build` makes over `graph` to completion, with the same
    /// verification as [`Runtime::run`]: `verify_replay` on (the default)
    /// replays the log against the graph as it was before the run, and any
    /// divergence is the error. For hosts that build their own driver — one
    /// with observers attached, or a resumed one.
    pub async fn run_verified<E: From<ReplayMismatch>>(
        &self,
        graph: Graph,
        build: impl FnOnce(Graph) -> Result<Driver, E>,
    ) -> Result<ExecutionReport, E> {
        let original = self.options.verify_replay.then(|| graph.clone());
        let report = build(graph)?.run().await;
        if let Some(graph) = original {
            // The engine itself emits nothing, precisely because replay would
            // say it all a second time; the check gets a span here instead.
            let span = tracing::debug_span!(
                "runtime.verify_replay",
                record_count = report.state.log.len()
            );
            let _entered = span.enter();
            engine::verify_replay(graph, &report.state.log)?;
        }
        Ok(report)
    }

    /// The mask set of the configured `SecretProvider`, for a host that
    /// persists anything beside the run.
    pub fn masker(&self) -> Masker {
        self.secrets.masker()
    }

    /// The plugin router selected by the runtime's sandbox options. It takes
    /// its identity from the run dir, so a resumed run reaches the crashed
    /// run's sandboxes.
    fn default_executor(&self) -> Arc<dyn Executor> {
        self.default_executor_for(&self.options.run_dir)
    }

    fn default_executor_for(&self, run_dir: &Path) -> Arc<dyn Executor> {
        self.default_router_for(run_dir)
    }

    /// Build the standard sandbox router for maintenance without starting
    /// run services. A caller-supplied executor owns its own resources.
    pub fn sandbox_router_for(&self, run_dir: &Path) -> Option<Arc<RoutingExecutor>> {
        self.executor
            .is_none()
            .then(|| self.default_router_for(run_dir))
    }

    fn default_router_for(&self, run_dir: &Path) -> Arc<RoutingExecutor> {
        Arc::new(RoutingExecutor::with_options(
            run_dir,
            self.options.retention,
            self.options.sandbox.clone(),
        ))
    }
}

/// Shared runtime services and configuration for all executions in one run.
pub struct RunRuntime {
    run_dir:   PathBuf,
    options:   RunOptions,
    executor:  Arc<dyn Executor>,
    /// The standard router, when the executor is one.
    router:    Option<Arc<RoutingExecutor>>,
    steps:     ::steps::Registry,
    secrets:   Arc<dyn SecretProvider>,
    observers: Vec<Arc<dyn EventObserver>>,
    progress:  Option<Arc<dyn ProgressSink>>,
    caps:      ::steps::Capabilities,
    guards:    Vec<RunServiceGuard>,
}

impl RunRuntime {
    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    /// The standard routing executor, when this run uses it: the host that
    /// prunes sandboxes reaches the run's lease manager through it.
    pub fn sandbox_router(&self) -> Option<&Arc<RoutingExecutor>> {
        self.router.as_ref()
    }

    /// Hand the router the durable record of sandbox leases. Every
    /// container scope acquired after this is recorded there; before it, or
    /// under a caller-supplied executor, leases live in memory.
    pub fn attach_lease_ledger(&self, ledger: Arc<dyn LeaseLedger>) {
        if let Some(router) = &self.router {
            router.set_ledger(ledger);
        }
    }

    /// End a lease's sandbox: stop it, then keep or delete it by this run's
    /// retention for `outcome`. A no-op under a caller-supplied executor.
    pub async fn release_lease(
        &self,
        lease: executor::SandboxLeaseId,
        outcome: executor::ScopeOutcome,
    ) -> executor::ReleaseReport {
        match &self.router {
            Some(router) => router.release_lease(lease, outcome).await,
            None => executor::ReleaseReport::default(),
        }
    }

    /// Build one execution driver without provisioning run services again.
    pub fn driver(
        &self,
        graph: Graph,
        start: EngineStart,
        execution_dir: impl Into<PathBuf>,
        environment_prefix: impl Into<smol_str::SmolStr>,
        workspace_prefix: impl Into<smol_str::SmolStr>,
        sandbox: SandboxAssignment,
        secrets: Arc<dyn SecretProvider>,
    ) -> Driver {
        let driver = Driver::new(
            graph,
            self.executor.clone(),
            self.steps.clone(),
            secrets,
            self.execution_config(
                execution_dir.into(),
                environment_prefix.into(),
                workspace_prefix.into(),
                sandbox,
            ),
        )
        .with_engine_start(start)
        .with_capabilities(self.caps.clone());
        attach(driver, &self.observers, self.progress.as_ref())
    }

    /// The resume counterpart of [`RunRuntime::driver`].
    pub fn resume_driver(
        &self,
        graph: Graph,
        log: EventLog,
        execution_dir: impl Into<PathBuf>,
        environment_prefix: impl Into<smol_str::SmolStr>,
        workspace_prefix: impl Into<smol_str::SmolStr>,
        sandbox: SandboxAssignment,
        secrets: Arc<dyn SecretProvider>,
    ) -> Result<(Driver, ResumeInfo), ResumeError> {
        let (driver, info) = Driver::resume(
            graph,
            log,
            self.executor.clone(),
            self.steps.clone(),
            secrets,
            self.execution_config(
                execution_dir.into(),
                environment_prefix.into(),
                workspace_prefix.into(),
                sandbox,
            ),
        )?;
        let driver = driver.with_capabilities(self.caps.clone());
        Ok((
            attach(driver, &self.observers, self.progress.as_ref()),
            info,
        ))
    }

    pub fn secret_provider(&self) -> Arc<dyn SecretProvider> {
        self.secrets.clone()
    }

    pub fn masker(&self) -> Masker {
        self.secrets.masker()
    }

    pub async fn finish_with_status(mut self, status: RunStatus) {
        for guard in mem::take(&mut self.guards) {
            guard.teardown().await;
        }
        let outcome = if status == RunStatus::Success {
            executor::ScopeOutcome::Succeeded
        } else {
            executor::ScopeOutcome::Failed
        };
        // `scopes/` holds the host backend's workspaces and nothing else: a
        // container scope's workspace lives in its sandbox, and its lease's
        // release applied retention to it already.
        if !self.options.retention.keeps(outcome) {
            let _ = fs::remove_dir_all(self.run_dir.join("scopes"));
        }
    }

    fn execution_config(
        &self,
        execution_dir: PathBuf,
        environment_prefix: smol_str::SmolStr,
        workspace_prefix: smol_str::SmolStr,
        sandbox: SandboxAssignment,
    ) -> RunConfig {
        // An execution ends before its invocation can restart. Workspace
        // retention therefore belongs to `finish_with_status` and to the
        // lease's release, not to an individual driver release.
        base_run_config(&self.options, execution_dir)
            .with_retention(Retention::Always)
            .with_scope_identities(environment_prefix, workspace_prefix)
            .with_sandbox_assignment(sandbox)
    }
}

/// The `RunConfig` fields every driver takes straight from [`RunOptions`];
/// retention and scope identities stay with each caller.
fn base_run_config(options: &RunOptions, run_dir: PathBuf) -> RunConfig {
    let mut config = RunConfig::new(run_dir)
        .with_grace(options.grace)
        .with_cleanup_grace(options.cleanup_grace)
        .with_echo(options.echo);
    config.hard_deadline_slack = options.hard_deadline_slack;
    config
}

/// Attach the runtime-registered observers and progress sink to a driver.
fn attach(
    mut driver: Driver,
    observers: &[Arc<dyn EventObserver>],
    progress: Option<&Arc<dyn ProgressSink>>,
) -> Driver {
    for observer in observers {
        driver = driver.observe(observer.clone());
    }
    if let Some(progress) = progress {
        driver = driver.with_progress(progress.clone());
    }
    driver
}
