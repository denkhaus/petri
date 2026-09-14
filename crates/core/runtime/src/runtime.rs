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
    Driver, EventObserver, ExecutionHooks, ExecutionReport, ResumeError, ResumeInfo, RunConfig,
    RunGuard, SandboxAssignment,
};
use engine::{EngineStart, EventLog, ReplayMismatch};
use executor::{
    DEFAULT_GRACE, Executor, MapSecrets, Masker, ProgressSink, Retention, SecretProvider,
};
use executor_sandbox::{LeaseLedger, RoutingExecutor, SandboxOptions};
use frontend::{CompileInputs, DirFiles, Frontend, Lowered, REPOSITORY_VAR, Span};
use ir::Graph;
use serde_json::Value;
use smol_str::SmolStr;
use store::{Access, OwnerId, RunDirStore, RunKey, RunLogs, RunStore, StoreError};
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
    /// Echo step output to this process's stderr, prefixed by node and
    /// firing.
    pub echo:                bool,
    /// Replay the log after the run and fail on any divergence. The determinism
    /// canary; on by default.
    pub verify_replay:       bool,
    pub sandbox:             SandboxOptions,
    /// The run's identity in its store and on its sandbox providers. A host
    /// with a run id of its own passes it; a resume of a stored run finds
    /// the stored one; otherwise Petri mints one when the run is prepared.
    pub run_key:             Option<RunKey>,
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
            run_key:             None,
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
    /// The store runs live in; `None` is the run directory under
    /// `RunOptions::run_dir`.
    store:        Option<Arc<dyn RunStore>>,
    secrets:      Arc<dyn SecretProvider>,
    observers:    Vec<Arc<dyn EventObserver>>,
    progress:     Option<Arc<dyn ProgressSink>>,
    hooks:        Option<Arc<dyn ExecutionHooks>>,
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
            store:        None,
            secrets:      Arc::new(MapSecrets::empty()),
            observers:    Vec::new(),
            progress:     None,
            hooks:        None,
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
            store:        None,
            secrets:      Arc::new(MapSecrets::empty()),
            observers:    Vec::new(),
            progress:     None,
            hooks:        None,
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

    /// Keep every run's durable record in `store` instead of the run
    /// directory: a host's database, or [`store::MemoryRunStore`] for a run
    /// that leaves no files of record. The run directory still holds what
    /// is a file by nature (workspaces, step output). A resume through a
    /// host store needs `RunOptions::run_key`, since the directory no
    /// longer names the run.
    #[must_use]
    pub fn store(mut self, store: Arc<dyn RunStore>) -> Self {
        self.store = Some(store);
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

    /// Install the host's awaited extension points
    /// ([`driver::lifecycle`]) on every driver this runtime builds. Without
    /// them every driver takes the unchanged fast path.
    #[must_use]
    pub fn hooks(mut self, hooks: Arc<dyn ExecutionHooks>) -> Self {
        self.hooks = Some(hooks);
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

    /// The awaited extension points installed with [`Runtime::hooks`], for a
    /// host that wraps them (a control service holding admission while paused
    /// delegates every other point to these).
    pub fn installed_hooks(&self) -> Option<Arc<dyn ExecutionHooks>> {
        self.hooks.clone()
    }

    /// The step registry, for lookups (`type_known`-style lints, validation).
    pub fn registry(&self) -> &::steps::Registry {
        &self.steps
    }

    // ── Frontends ──────────────────────────────────────────────────────────

    /// The frontend for a file: by `name` when given, else the first that
    /// claims the path.
    /// The frontend that recognizes `graph` as its own lowering, if any: how
    /// a host that holds only a stored graph (a resume) finds the format's
    /// launch settings and defaults.
    pub fn frontend_for_graph(&self, graph: &Graph) -> Option<&dyn Frontend> {
        self.frontends
            .iter()
            .map(AsRef::as_ref)
            .find(|frontend| frontend.claims_graph(graph))
    }

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
        // The repository root, absolute, for a format whose runs check it
        // out. A caller that bound the variable itself keeps its value.
        let mut inputs = inputs.clone();
        if !inputs.vars.contains_key(REPOSITORY_VAR) {
            // A relative file's root can be the empty path, the current
            // directory; canonicalize needs a name for it.
            let base = if repo.as_os_str().is_empty() {
                Path::new(".")
            } else {
                repo.as_path()
            };
            let absolute = fs::canonicalize(base).unwrap_or_else(|_| base.to_path_buf());
            inputs.vars.insert(
                SmolStr::new(REPOSITORY_VAR),
                Value::String(absolute.to_string_lossy().into_owned()),
            );
        }
        let files = DirFiles { root: repo };
        let lowered = frontend.load(&name, &text, &files, &inputs);
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
        let run = self.prepare_run(&self.options.run_dir);
        let driver = Driver::new(
            graph,
            run.executor.clone(),
            self.steps.clone(),
            self.secrets.clone(),
            self.run_config(),
        );
        run.equip_standalone(driver)
    }

    /// Prepare resources that are shared by every execution in one root run.
    pub fn prepare_run(&self, run_dir: impl Into<PathBuf>) -> RunRuntime {
        let run_dir = run_dir.into();
        let key = self.run_key_for(&run_dir);
        let (executor, router) = self.executor_for_run(&run_dir, &key);
        self.provision_run(run_dir, key, executor, router)
    }

    /// The run's key: the one the options name, else the one the run
    /// directory already stores (a resume over the run-directory store),
    /// else a fresh one.
    pub fn run_key_for(&self, run_dir: &Path) -> RunKey {
        self.options
            .run_key
            .clone()
            .or_else(|| {
                self.store
                    .is_none()
                    .then(|| RunDirStore::new(run_dir).stored_key().ok().flatten())
                    .flatten()
            })
            .unwrap_or_else(RunKey::mint)
    }

    /// The store runs under `run_dir` live in: the installed one, else the
    /// run directory itself.
    pub fn store_for(&self, run_dir: &Path) -> Arc<dyn RunStore> {
        self.store
            .clone()
            .unwrap_or_else(|| Arc::new(RunDirStore::new(run_dir)))
    }

    /// Open the run under `run_dir` in the runtime's store: what a host does
    /// to inspect, replay or prune a run without preparing it.
    pub async fn open_run(
        &self,
        run_dir: &Path,
        access: Access,
    ) -> Result<Arc<dyn RunLogs>, StoreError> {
        let key = self.run_key_for(run_dir);
        self.store_for(run_dir).open(&key, access).await
    }

    fn executor_for_run(
        &self,
        run_dir: &Path,
        key: &RunKey,
    ) -> (Arc<dyn Executor>, Option<Arc<RoutingExecutor>>) {
        // The standard router is kept by its own type too: the coordinator
        // hands it the lease ledger and releases leases through it. A
        // caller-supplied executor manages its own sandboxes.
        if let Some(executor) = self.executor.clone() {
            (executor, None)
        } else {
            let router = self.default_router_for(run_dir, key);
            (router.clone(), Some(router))
        }
    }

    fn provision_run(
        &self,
        run_dir: PathBuf,
        key: RunKey,
        executor: Arc<dyn Executor>,
        router: Option<Arc<RoutingExecutor>>,
    ) -> RunRuntime {
        let (caps, guards) = self.provision(&run_dir);
        RunRuntime {
            store: self.store_for(&run_dir),
            run_dir,
            key,
            owner: OwnerId::mint(),
            options: self.options.clone(),
            executor,
            router,
            steps: self.steps.clone(),
            secrets: self.secrets.clone(),
            observers: self.observers.clone(),
            progress: self.progress.clone(),
            hooks: self.hooks.clone(),
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
        let key = self.run_key_for(&self.options.run_dir);
        let (executor, router) = self.executor_for_run(&self.options.run_dir, &key);
        let (driver, info) = Driver::resume(
            graph,
            log,
            executor.clone(),
            self.steps.clone(),
            self.secrets.clone(),
            self.run_config(),
        )?;
        let run = self.provision_run(self.options.run_dir.clone(), key, executor, router);
        Ok((run.equip_standalone(driver), info))
    }

    fn run_config(&self) -> RunConfig {
        base_run_config(&self.options, self.options.run_dir.clone())
            .with_retention(self.options.retention)
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

    /// Build the standard sandbox router for maintenance without starting
    /// run services. A caller-supplied executor owns its own resources.
    pub fn sandbox_router_for(&self, run_dir: &Path) -> Option<Arc<RoutingExecutor>> {
        self.executor
            .is_none()
            .then(|| self.default_router_for(run_dir, &self.run_key_for(run_dir)))
    }

    fn default_router_for(&self, run_dir: &Path, key: &RunKey) -> Arc<RoutingExecutor> {
        Arc::new(
            RoutingExecutor::with_options(
                run_dir,
                self.options.retention,
                self.options.sandbox.clone(),
            )
            .with_run_id(key.as_str()),
        )
    }
}

/// How a [`RunRuntime`] opens its run: the access modes of
/// [`store::Access`] under the run's own key and owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunAccess {
    Create,
    Write,
    Read,
}

/// Shared runtime services and configuration for all executions in one run.
pub struct RunRuntime {
    /// The store the run's durable record lives in.
    store:     Arc<dyn RunStore>,
    run_dir:   PathBuf,
    /// The run's identity in its store and on its providers.
    key:       RunKey,
    /// This coordinator instance, for the store's writer lease.
    owner:     OwnerId,
    options:   RunOptions,
    executor:  Arc<dyn Executor>,
    /// The standard router, when the executor is one.
    router:    Option<Arc<RoutingExecutor>>,
    steps:     ::steps::Registry,
    secrets:   Arc<dyn SecretProvider>,
    observers: Vec<Arc<dyn EventObserver>>,
    progress:  Option<Arc<dyn ProgressSink>>,
    hooks:     Option<Arc<dyn ExecutionHooks>>,
    caps:      ::steps::Capabilities,
    guards:    Vec<RunServiceGuard>,
}

impl RunRuntime {
    /// A standalone driver's completion owns this run's service teardown.
    fn equip_standalone(self, driver: Driver) -> Driver {
        let driver = attach(
            driver.with_capabilities(self.caps.clone()),
            &self.observers,
            self.progress.as_ref(),
            self.hooks.as_ref(),
        );
        driver.with_run_guard(Box::new(self))
    }

    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    /// The run's key: its identity in its store, and the run id every
    /// sandbox of the run is labelled with.
    pub fn run_key(&self) -> &RunKey {
        &self.key
    }

    /// This coordinator instance's owner id, minted when the run was
    /// prepared: what the store's writer lease is taken for.
    pub fn owner(&self) -> &OwnerId {
        &self.owner
    }

    /// The store the run's durable record lives in.
    pub fn store(&self) -> &Arc<dyn RunStore> {
        &self.store
    }

    /// Open this run in its store: `Create` for a fresh run, `Write` to
    /// continue one, both under this instance's owner; `Read` takes no
    /// lease.
    pub async fn open(&self, access: RunAccess) -> Result<Arc<dyn RunLogs>, StoreError> {
        let access = match access {
            RunAccess::Create => Access::Create {
                owner: self.owner.clone(),
            },
            RunAccess::Write => Access::Write {
                owner: self.owner.clone(),
            },
            RunAccess::Read => Access::Read,
        };
        self.store.open(&self.key, access).await
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

    /// Reconcile the run's recorded leases with their providers before any
    /// create: the takeover step of a resume. `host` names the leases on the
    /// host provider and `container` the rest. A no-op under a
    /// caller-supplied executor.
    pub async fn reconcile_leases(
        &self,
        host: &[executor_sandbox::RecordedLease],
        container: &[executor_sandbox::RecordedLease],
    ) -> executor_sandbox::ReconcileReport {
        match &self.router {
            Some(router) => router.reconcile(host, container).await,
            None => executor_sandbox::ReconcileReport::default(),
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
        attach(
            driver,
            &self.observers,
            self.progress.as_ref(),
            self.hooks.as_ref(),
        )
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
            attach(
                driver,
                &self.observers,
                self.progress.as_ref(),
                self.hooks.as_ref(),
            ),
            info,
        ))
    }

    pub fn secret_provider(&self) -> Arc<dyn SecretProvider> {
        self.secrets.clone()
    }

    pub fn masker(&self) -> Masker {
        self.secrets.masker()
    }

    /// Tear down run services and plugins after each lease applies retention.
    pub async fn finish(mut self) {
        for guard in mem::take(&mut self.guards) {
            guard.teardown().await;
        }
        if let Some(router) = &self.router {
            router.shutdown().await;
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
        // retention therefore belongs to the lease's release, not to an
        // individual driver release.
        base_run_config(&self.options, execution_dir)
            .with_retention(Retention::Always)
            .with_scope_identities(environment_prefix, workspace_prefix)
            .with_sandbox_assignment(sandbox)
    }
}

#[async_trait::async_trait]
impl RunGuard for RunRuntime {
    async fn teardown(self: Box<Self>) {
        self.finish().await;
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

/// Attach the runtime-registered observers, progress sink and hooks to a
/// driver.
fn attach(
    mut driver: Driver,
    observers: &[Arc<dyn EventObserver>],
    progress: Option<&Arc<dyn ProgressSink>>,
    hooks: Option<&Arc<dyn ExecutionHooks>>,
) -> Driver {
    for observer in observers {
        driver = driver.observe(observer.clone());
    }
    if let Some(progress) = progress {
        driver = driver.with_progress(progress.clone());
    }
    if let Some(hooks) = hooks {
        driver = driver.with_hooks(hooks.clone());
    }
    driver
}
