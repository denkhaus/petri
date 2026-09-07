//! The command line.
//!
//! `check <workflow>` parses, lowers and validates a workflow file — including
//! the step registry, so an unknown step kind or a bad literal config is caught
//! here — and prints every diagnostic with its span. `run` runs one to
//! completion and writes the event log beside the workspaces. `replay` feeds a
//! saved log back through the engine and checks byte-identity — the determinism
//! canary, runnable from the shell.
//!
//! This crate names no format. It is the command line over whatever
//! [`Runtime`] it is handed: [`main`] takes a factory and calls it once per
//! command, so the binary that ships — and any other binary — decides which
//! frontends and step kinds are registered. Anything a format needs from its
//! host comes from the frontend itself — where its repository root is, and the
//! run parameters a run would otherwise have to hard-code — so no command here
//! has a special case for one format.

pub mod answer;
pub mod control;
mod inspect;

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{self, ExitCode};
use std::sync::Arc;
use std::{env, fs};

use answer::{AutoApproveInterviewer, ScriptedInterviewer, TerminalInterviewer};
use clap::{Args, Parser, Subcommand, ValueEnum};
use execution::controls::ControlService;
use execution::host::HostRun;
use execution::watchdog::StallWatchdog;
use execution::{
    CoordinatorHandle, InterviewDispatcher, InterviewReceipt, Interviewer, LeaseState,
    RECEIPT_FILE, ResourceStore, host, prune as sandbox_prune,
};
use runtime::engine::{self, EventLog};
use runtime::executor::Retention;
use runtime::frontend::{self, CompileInputs, Lowered, WorkspaceRetention};
use runtime::ir::{Graph, RunStatus};
use runtime::{
    DaytonaResources, DaytonaSandboxKind, LoadError, RunOptions, Runtime, SandboxBackend,
    SandboxOptions,
};
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tracing::field::{Empty, display};

#[derive(Parser)]
#[command(name = "petri", version, about = "A token-flow workflow engine")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct FileArgs {
    /// The workflow file.
    file:        PathBuf,
    /// Which format the file is in. Guessed from its path when omitted; the
    /// last frontend asked claims everything, so an unrecognized path is
    /// native.
    #[arg(long)]
    format:      Option<String>,
    /// Repository root, for resolving a format's local includes. Defaults to
    /// wherever the file's own format says its repository root is.
    #[arg(long)]
    repo:        Option<PathBuf>,
    /// A run input the format renders before lowering, as `KEY=VALUE`. The
    /// value is read as JSON when it parses as JSON, else as a string.
    /// Repeatable; later values win.
    #[arg(long = "input", value_name = "KEY=VALUE")]
    inputs:      Vec<String>,
    /// A JSON file of run inputs: either `{"inputs": {...}, "vars": {...}}`
    /// or a flat object of inputs. `--input` values land on top of it.
    #[arg(long)]
    inputs_file: Option<PathBuf>,
}

impl FileArgs {
    /// The compile inputs these arguments describe. `Err` is a usage error.
    fn compile_inputs(&self) -> Result<CompileInputs, String> {
        let mut inputs = CompileInputs::new();
        if let Some(path) = &self.inputs_file {
            let text = fs::read_to_string(path)
                .map_err(|e| format!("could not read {}: {e}", path.display()))?;
            let value: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| format!("{} is not JSON: {e}", path.display()))?;
            let serde_json::Value::Object(mut map) = value else {
                return Err(format!("{} must hold a JSON object", path.display()));
            };
            let has_sections = map.contains_key("inputs") || map.contains_key("vars");
            if has_sections {
                if let Some(unexpected) = map
                    .keys()
                    .find(|key| !matches!(key.as_str(), "inputs" | "vars"))
                {
                    return Err(format!(
                        "{} has unexpected top-level key `{unexpected}`; a sectioned input file permits only `inputs` and `vars`",
                        path.display()
                    ));
                }
                for (section, target) in
                    [("inputs", &mut inputs.inputs), ("vars", &mut inputs.vars)]
                {
                    let Some(value) = map.remove(section) else {
                        continue;
                    };
                    let serde_json::Value::Object(values) = value else {
                        return Err(format!(
                            "{} field `{section}` must hold a JSON object",
                            path.display()
                        ));
                    };
                    if values.contains_key("") {
                        return Err(format!(
                            "{} field `{section}` contains an empty key",
                            path.display()
                        ));
                    }
                    target.extend(values.into_iter().map(|(k, v)| (k.into(), v)));
                }
            } else {
                if map.contains_key("") {
                    return Err(format!("{} contains an empty input key", path.display()));
                }
                inputs
                    .inputs
                    .extend(map.into_iter().map(|(k, v)| (k.into(), v)));
            }
        }
        for pair in &self.inputs {
            let Some((key, value)) = pair.split_once('=') else {
                return Err(format!("`--input {pair}` is not `KEY=VALUE`"));
            };
            if key.is_empty() {
                return Err("`--input` keys cannot be empty".into());
            }
            let value = serde_json::from_str(value)
                .unwrap_or_else(|_| serde_json::Value::String(value.to_string()));
            inputs.inputs.insert(key.into(), value);
        }
        Ok(inputs)
    }
}

#[derive(Args)]
struct ProviderArgs {
    /// Where workflow processes run: host, docker, or daytona.
    #[arg(long, default_value = "host")]
    backend:            SandboxBackend,
    /// Allow plugins without a pinned checksum. Debug builds allow them by
    /// default.
    #[arg(long)]
    sandbox_plugin_dev: bool,
}

impl ProviderArgs {
    fn options(self) -> SandboxOptions {
        SandboxOptions {
            backend: self.backend,
            plugin_dev: self.sandbox_plugin_dev.then_some(true),
            ..Default::default()
        }
    }
}

#[derive(Args)]
struct RunnerArgs {
    /// Daytona offering for the runner: vm or container.
    #[arg(long, default_value = "vm")]
    daytona_kind:      DaytonaSandboxKind,
    /// Override a placement label's runner image. Repeatable; later values win.
    #[arg(long = "runner-image", value_name = "LABEL=IMAGE", value_parser = runner_image)]
    images:            Vec<(String, String)>,
    /// CPUs in a Daytona runner snapshot (minimum 2).
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u32).range(2..))]
    daytona_cpus:      u32,
    /// Memory in a Daytona runner snapshot, in MiB (minimum 4096).
    #[arg(long, default_value_t = 4096, value_parser = clap::value_parser!(u64).range(4096..))]
    daytona_memory_mb: u64,
    /// Disk in a Daytona runner snapshot, in MiB (minimum 4096).
    #[arg(long, default_value_t = 20480, value_parser = clap::value_parser!(u64).range(4096..))]
    daytona_disk_mb:   u64,
}

fn runner_image(value: &str) -> Result<(String, String), String> {
    match value.split_once('=') {
        Some((label, image)) if !label.trim().is_empty() && !image.trim().is_empty() => {
            Ok((label.to_owned(), image.to_owned()))
        }
        _ => Err("expected LABEL=IMAGE with a nonempty label and image".to_owned()),
    }
}

#[derive(Subcommand)]
enum Command {
    /// Parse, lower and validate a workflow file; print diagnostics.
    Check {
        #[command(flatten)]
        target:      FileArgs,
        /// Print the lowered graph in a stable text form.
        #[arg(long)]
        print_graph: bool,
        /// Print diagnostics as JSON lines instead of text.
        #[arg(long)]
        json:        bool,
    },
    /// Lower a workflow file and print the graph in a stable text form.
    PrintGraph {
        #[command(flatten)]
        target: FileArgs,
    },
    /// Run a workflow file to completion.
    Run {
        #[command(flatten)]
        target:           FileArgs,
        /// Where workspaces, logs and `events.json` go. Defaults to a fresh
        /// directory under the system temp dir, printed at start.
        #[arg(long)]
        run_dir:          Option<PathBuf>,
        /// Do not echo step output.
        #[arg(long)]
        quiet:            bool,
        /// Answer a step's question — a human gate — from the terminal:
        /// the question is printed and one line is read from stdin.
        #[arg(long, conflicts_with_all = ["auto_approve", "interview_script"])]
        interactive:      bool,
        /// Answer every step's question with its default choice.
        #[arg(long, conflicts_with = "interview_script")]
        auto_approve:     bool,
        /// Answer every step's question from a JSON interview script, and
        /// fail the run when a question matches no entry or a required entry
        /// goes unused. See `cli::answer` for the format.
        #[arg(long, value_name = "FILE")]
        interview_script: Option<PathBuf>,
        /// When to keep the run's workspaces: always, on-failure, or never.
        /// Defaults to what the workflow's format declares (Fabro: always;
        /// other formats: on-failure).
        #[arg(long, value_name = "POLICY")]
        retain:           Option<Retain>,
        /// Read run controls from this file while the run is live: one
        /// `pause`, `unpause`, `steer <node> <text>` or `cancel` per appended
        /// line. See `cli::control` for the format.
        #[arg(long, value_name = "FILE")]
        control:          Option<PathBuf>,
        /// Simulate the step kinds that offer it (Fabro's stages) instead of
        /// running them: every stage succeeds, a human gate takes its first
        /// choice.
        #[arg(long)]
        dry_run:          bool,
        #[command(flatten)]
        provider:         ProviderArgs,
        #[command(flatten)]
        runner:           RunnerArgs,
    },
    /// Reconstruct a run from its run directory's durable files and print
    /// the result. Read-only: nothing starts, nothing is written.
    Inspect {
        /// The run's directory.
        #[arg(long)]
        run_dir: PathBuf,
        /// Print the versioned JSON document instead of a summary.
        #[arg(long)]
        json:    bool,
    },
    /// Sandboxes a run holds on its provider.
    #[command(subcommand)]
    Sandbox(SandboxCommand),
    /// Replay a saved event log against the workflow and verify byte-identity.
    ///
    /// Lower the same file on the same machine — and the same checkout state,
    /// since a format's default run parameters may read it (GHA reads HEAD) —
    /// as the original run: the graph, including the default run parameters,
    /// is the replay's input. A host that persists the graph itself (the run
    /// dir's `graphs/<digest>.json`) has no such constraint.
    Replay {
        #[command(flatten)]
        target: FileArgs,
        /// The `events.json` a run wrote.
        log:    PathBuf,
    },
}

#[derive(Subcommand)]
enum SandboxCommand {
    /// Delete every sandbox a finished or abandoned run still holds on its
    /// provider, and record each as gone. Refuses a run a live process
    /// holds. Deleting a sandbox also deletes its managed workspace.
    Prune {
        /// The run's directory.
        #[arg(long)]
        run_dir:  PathBuf,
        #[command(flatten)]
        provider: ProviderArgs,
    },
}

/// `--retain`: when a run's workspaces survive it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Retain {
    /// Keep every workspace: after success, failure and cancellation.
    Always,
    /// Keep a failed run's workspaces; delete a successful run's.
    OnFailure,
    /// Delete every workspace.
    Never,
}

impl From<Retain> for Retention {
    fn from(retain: Retain) -> Self {
        match retain {
            Retain::Always => Self::Always,
            Retain::OnFailure => Self::OnFailure,
            Retain::Never => Self::Never,
        }
    }
}

impl From<WorkspaceRetention> for Retain {
    fn from(retention: WorkspaceRetention) -> Self {
        match retention {
            WorkspaceRetention::Always => Self::Always,
            WorkspaceRetention::OnFailure => Self::OnFailure,
            WorkspaceRetention::Never => Self::Never,
        }
    }
}

/// How `petri run` answers questions.
enum Answers {
    Interactive,
    AutoApprove,
    Scripted(PathBuf),
}

/// Which runtime a command wants from the factory it is handed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeMode {
    /// The real step kinds.
    Real,
    /// `run --dry-run`: the distribution's simulated step kinds, for formats
    /// that have them — a workflow runs end to end with no model, shell or
    /// person behind its stages.
    DryRun,
}

/// Parse the arguments and run the command, on a runtime from `make`. One
/// command, one runtime.
pub async fn main(make: impl Fn(RuntimeMode) -> Runtime) -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Check {
            target,
            print_graph,
            json,
        } => check(&make(RuntimeMode::Real), &target, print_graph, json),
        Command::PrintGraph { target } => check(&make(RuntimeMode::Real), &target, true, false),
        Command::Run {
            target,
            run_dir,
            quiet,
            interactive,
            auto_approve,
            interview_script,
            retain,
            control,
            dry_run,
            provider,
            runner,
        } => {
            let run_dir = run_dir
                .unwrap_or_else(|| env::temp_dir().join(format!("petri-run-{}", process::id())));
            let runtime_mode = if dry_run {
                RuntimeMode::DryRun
            } else {
                RuntimeMode::Real
            };
            let rt = make(runtime_mode);
            // One control service for the terminal's control file and an
            // embedded host alike; its pause hook wraps the hooks the
            // distribution installed, so both run at admission.
            let controls = ControlService::new();
            let hooks = controls.hooks(rt.installed_hooks());
            let rt = rt.hooks(hooks);
            let mut options = RunOptions::new(&run_dir);
            options.echo = !quiet;
            options.retention = retain
                .or_else(|| {
                    rt.frontend_for(&target.file, target.format.as_deref())
                        .ok()
                        .map(|frontend| frontend.default_retention().into())
                })
                .map_or(options.retention, Retention::from);
            options.sandbox = provider.options();
            options.sandbox.runner_images = runner.images.into_iter().collect();
            options.sandbox.daytona_kind = runner.daytona_kind;
            options.sandbox.daytona_resources = DaytonaResources {
                cpu_cores: runner.daytona_cpus,
                memory_mb: runner.daytona_memory_mb,
                disk_mb:   runner.daytona_disk_mb,
            };
            let answers = match (interactive, auto_approve, interview_script) {
                (_, _, Some(script)) => Some(Answers::Scripted(script)),
                (true, _, None) => Some(Answers::Interactive),
                (_, true, None) => Some(Answers::AutoApprove),
                _ => None,
            };
            Box::pin(run(
                &rt.options(options),
                &target,
                &run_dir,
                answers,
                controls,
                control,
            ))
            .await
        }
        Command::Replay { target, log } => replay(&make(RuntimeMode::Real), &target, &log),
        Command::Inspect { run_dir, json } => inspect::inspect(&run_dir, json),
        Command::Sandbox(SandboxCommand::Prune { run_dir, provider }) => {
            let mut options = RunOptions::new(&run_dir);
            options.sandbox = provider.options();
            prune(&make(RuntimeMode::Real).options(options)).await
        }
    }
}

/// `sandbox prune`: one line per lease, and a failure exit when any lease
/// could not be pruned, so a script can retry.
#[expect(
    clippy::print_stderr,
    reason = "the command's report is its output; there is no subscriber to route it to"
)]
async fn prune(rt: &Runtime) -> ExitCode {
    let report = match sandbox_prune::prune(rt).await {
        Ok(report) => report,
        Err(error) => {
            eprintln!("error: {}", error_chain(&error));
            return ExitCode::from(3);
        }
    };
    for (lease, ids) in &report.deleted {
        eprintln!("lease {lease}: deleted {}", ids.join(", "));
    }
    for lease in &report.clean {
        eprintln!("lease {lease}: nothing to prune");
    }
    for (lease, problem) in &report.problems {
        eprintln!("lease {lease}: {problem}");
    }
    if report.is_clean() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// An error and its whole `source()` chain on one line, so a typed cause
/// (the io error under a failed read, say) actually reaches the user.
fn error_chain(error: &dyn Error) -> String {
    let mut out = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
}

/// Lower and validate, or explain why not. `Err` carries the exit code.
#[expect(
    clippy::print_stdout,
    reason = "`--json` makes the diagnostics the command's output; a caller reads them on stdout"
)]
#[expect(
    clippy::print_stderr,
    reason = "the CLI reports diagnostics to the user on stderr, clear of the command's own output"
)]
fn lowered_graph(
    rt: &Runtime,
    target: &FileArgs,
    json: bool,
    validate_only: bool,
) -> Result<Lowered, ExitCode> {
    let mut inputs = match target.compile_inputs() {
        Ok(inputs) => inputs,
        Err(message) => {
            eprintln!("error: {message}");
            return Err(ExitCode::from(2));
        }
    };
    // A check given no inputs validates the file as written: an input it
    // reads is a warning, not a reason to stop. A run always needs them.
    if validate_only && target.inputs.is_empty() && target.inputs_file.is_none() {
        inputs = inputs.with_unbound_as_warning();
    }
    match rt.check(
        &target.file,
        target.format.as_deref(),
        target.repo.as_deref(),
        &inputs,
    ) {
        Ok(lowered) => {
            for d in lowered.diagnostics.iter() {
                if json {
                    match serde_json::to_string(d) {
                        Ok(line) => println!("{line}"),
                        Err(e) => eprintln!("error: could not serialize a diagnostic: {e}"),
                    }
                } else {
                    eprintln!("{d}");
                }
            }
            Ok(lowered)
        }
        Err(error) => {
            eprintln!("error: {}", error_chain(&error));
            Err(ExitCode::from(2))
        }
    }
}

#[expect(
    clippy::print_stdout,
    reason = "the printed graph is this command's output; stdout is the CLI's channel for it"
)]
#[expect(
    clippy::print_stderr,
    reason = "the summary is for the user, on stderr so stdout carries only the graph"
)]
#[tracing::instrument(
    name = "cli.check",
    skip_all,
    fields(
        workflow_file = %target.file.display(),
        format = target.format.as_deref(),
        error_count = Empty,
        warning_count = Empty,
    )
)]
fn check(rt: &Runtime, target: &FileArgs, print_graph: bool, json: bool) -> ExitCode {
    let lowered = match lowered_graph(rt, target, json, true) {
        Ok(lowered) => lowered,
        Err(code) => return code,
    };
    let errors = lowered.diagnostics.errors().count();
    let warnings = lowered.diagnostics.warnings().count();
    let span = tracing::Span::current();
    span.record("error_count", errors);
    span.record("warning_count", warnings);
    if let Some(graph) = &lowered.graph {
        if print_graph {
            print!("{}", frontend::print_graph(graph));
        }
        if !json {
            eprintln!(
                "ok: {} node(s), {} scope(s){}",
                graph.nodes.len(),
                graph.scopes.len(),
                if warnings > 0 {
                    format!(", {warnings} warning(s)")
                } else {
                    String::new()
                }
            );
        }
        ExitCode::SUCCESS
    } else {
        if !json {
            eprintln!("rejected: {errors} error(s), {warnings} warning(s)");
        }
        ExitCode::FAILURE
    }
}

#[expect(
    clippy::print_stderr,
    reason = "the CLI reports the run dir, each step and the final status to the user on stderr"
)]
// The run dir is the user's first line of output; it is not repeated in the
// span.
#[tracing::instrument(
    name = "cli.run",
    skip_all,
    fields(workflow_file = %target.file.display(), status = Empty)
)]
async fn run(
    rt: &Runtime,
    target: &FileArgs,
    run_dir: &Path,
    answers: Option<Answers>,
    controls: ControlService,
    control: Option<PathBuf>,
) -> ExitCode {
    let interviewer: Option<Arc<dyn Interviewer>> = match answers {
        None => None,
        Some(Answers::AutoApprove) => Some(Arc::new(AutoApproveInterviewer)),
        Some(Answers::Interactive) => match TerminalInterviewer::start() {
            Ok(terminal) => Some(Arc::new(terminal)),
            Err(error) => {
                eprintln!("error: could not read the terminal: {error}");
                return ExitCode::from(2);
            }
        },
        Some(Answers::Scripted(path)) => match ScriptedInterviewer::load(&path) {
            Ok(script) => Some(Arc::new(script)),
            Err(error) => {
                eprintln!("error: {}", error_chain(&error));
                return ExitCode::from(2);
            }
        },
    };
    let lowered = match lowered_graph(rt, target, false, false) {
        Ok(lowered) => lowered,
        Err(code) => return code,
    };
    let Some(mut graph) = lowered.graph else {
        eprintln!(
            "rejected: {} error(s); nothing to run",
            lowered.diagnostics.errors().count()
        );
        return ExitCode::FAILURE;
    };
    default_params(rt, target, &mut graph);

    eprintln!("run dir: {}", run_dir.display());
    let mut ctrl_c = None;
    let mut control_task = None;
    let stop_controls = CancellationToken::new();
    let mut host_run = HostRun::new(graph)
        .with_children(lowered.children)
        .observe(Arc::new(controls.clone()));
    // The stall watchdog, when the graph declares a budget.
    let watchdog = host_run.graph.policy.stall_timeout.map(StallWatchdog::new);
    let mut watchdog_task = None;
    if let Some(watchdog) = &watchdog {
        host_run = host_run.observe(Arc::new(watchdog.clone()));
    }
    let dispatcher = interviewer.map(InterviewDispatcher::new);
    if let Some(dispatcher) = &dispatcher {
        host_run = host_run.observe(Arc::new(dispatcher.clone()));
    }
    let outcome = host::run_configured(rt, host_run, |handle, secrets| {
        if let Some(dispatcher) = &dispatcher {
            dispatcher.wire(handle.clone(), secrets);
        }
        controls.wire(handle.clone());
        if let Some(watchdog) = &watchdog {
            watchdog_task = Some(watchdog.start(handle.clone()));
        }
        if let Some(path) = control {
            control_task = Some(tokio::spawn(control::drive(
                path,
                controls.clone(),
                stop_controls.clone(),
            )));
        }
        ctrl_c = Some(tokio::spawn(cancel_on_ctrl_c(handle)));
    })
    .await;
    if let Some(task) = ctrl_c {
        task.abort();
    }
    stop_controls.cancel();
    if let Some(task) = control_task {
        let _ = task.await;
    }
    if let Some(task) = watchdog_task {
        task.stop().await;
    }
    if let Some(stall) = watchdog.as_ref().and_then(StallWatchdog::tripped) {
        eprintln!(
            "stall watchdog: no execution activity for {} s (stall_timeout {} s); the run was \
             cancelled",
            stall.idle_ms / 1000,
            stall.stall_timeout_ms / 1000
        );
    }
    // The receipt is written whatever the run did: a failed run's interviews
    // are evidence too.
    let receipt = match &dispatcher {
        Some(dispatcher) => Some(dispatcher.shutdown().await),
        None => None,
    };
    if let Some(receipt) = &receipt {
        write_receipt(run_dir, receipt);
    }
    let report = match outcome {
        Ok(report) => report,
        Err(error) => {
            eprintln!("error: {}", error_chain(&error));
            return ExitCode::from(3);
        }
    };

    let log_path = run_dir.join("events.json");
    match serde_json::to_vec_pretty(&report.state.log) {
        Ok(bytes) => {
            if let Err(e) = fs::write(&log_path, bytes) {
                eprintln!("warning: could not write {}: {e}", log_path.display());
            } else {
                eprintln!("event log: {}", log_path.display());
            }
        }
        Err(e) => eprintln!("warning: could not encode the event log: {e}"),
    }

    for record in report.state.history() {
        eprintln!("  {} {}", record.outcome.status.tag(), record.name);
    }
    report_workspaces(run_dir, rt.run_options().retention);
    let status = report.status;
    tracing::Span::current().record("status", display(status));
    eprintln!("run: {status}");
    if let Some(receipt) = &receipt
        && !receipt.is_clean()
    {
        // The engine's status stands as persisted; the interview is what
        // failed, and the exit code says so.
        eprintln!(
            "interview verification failed: {} problem(s); see {}",
            receipt.errors.len(),
            run_dir.join(RECEIPT_FILE).display()
        );
        for error in &receipt.errors {
            eprintln!("  {error}");
        }
        return ExitCode::from(4);
    }
    if status == RunStatus::Success {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Persist the interview receipt beside the run.
#[expect(
    clippy::print_stderr,
    reason = "the CLI reports where the receipt went, and a failure to write it, on stderr"
)]
fn write_receipt(run_dir: &Path, receipt: &InterviewReceipt) {
    let path = run_dir.join(RECEIPT_FILE);
    match serde_json::to_vec_pretty(receipt) {
        Ok(bytes) => {
            if let Err(error) = fs::write(&path, bytes) {
                eprintln!("warning: could not write {}: {error}", path.display());
            } else if !receipt.questions.is_empty() || !receipt.errors.is_empty() {
                eprintln!("interviews: {}", path.display());
            }
        }
        Err(error) => eprintln!("warning: could not encode the interview receipt: {error}"),
    }
}

/// Say where the retained workspaces are, or how to reach them. A host
/// workspace is a directory under the run dir; a container's lives on its
/// provider and is reached through the sandbox, or deleted with
/// `petri sandbox prune`.
#[expect(
    clippy::print_stderr,
    reason = "the retained workspace path is the run's result for the user, on stderr"
)]
fn report_workspaces(run_dir: &Path, retention: Retention) {
    let store = match ResourceStore::load(run_dir.join(execution::RESOURCES_DIR)) {
        Ok(store) => store,
        Err(error) => {
            tracing::debug!(error = %error, "no sandbox resource records to report");
            return;
        }
    };
    for record in store.records() {
        match record.state {
            LeaseState::Deleted => {}
            LeaseState::Allocating | LeaseState::Live | LeaseState::Stopped => {
                if record.provider == execution::HOST_PROVIDER {
                    let path = run_dir
                        .join("scopes")
                        .join(record.workspace.as_str())
                        .join("work");
                    eprintln!("workspace: {}", path.display());
                } else {
                    eprintln!(
                        "workspace: {} on {} sandbox {} (delete with `petri sandbox prune \
                         --run-dir {}`)",
                        record.workspace,
                        record.provider,
                        record.resource_id.as_deref().unwrap_or("?"),
                        run_dir.display()
                    );
                }
            }
        }
    }
    if retention == Retention::Never {
        tracing::debug!("workspaces deleted by retention policy");
    }
}

/// Map Ctrl-C onto the run's two-tier stop: the first cancels the run —
/// cleanup steps and release still happen — and any further Ctrl-C reaches
/// the drivers' kill tier. The task holds no cleanup-sensitive state; the run
/// aborts it once the report is in.
#[expect(
    clippy::print_stderr,
    reason = "the CLI tells the user what each Ctrl-C did on stderr"
)]
async fn cancel_on_ctrl_c(handle: CoordinatorHandle) {
    let mut cancelled = false;
    loop {
        if signal::ctrl_c().await.is_err() {
            return;
        }
        if cancelled {
            eprintln!("killing the run");
        } else {
            eprintln!("cancelling the run; Ctrl-C again to kill");
            cancelled = true;
        }
        handle.cancel_root();
    }
}

#[expect(
    clippy::print_stderr,
    reason = "the replay verdict is this command's output, and the user reads it on stderr"
)]
#[tracing::instrument(
    name = "cli.replay",
    skip_all,
    fields(workflow_file = %target.file.display(), event_log = %log_path.display())
)]
fn replay(rt: &Runtime, target: &FileArgs, log_path: &Path) -> ExitCode {
    let lowered = match lowered_graph(rt, target, false, false) {
        Ok(lowered) => lowered,
        Err(code) => return code,
    };
    let Some(mut graph) = lowered.graph else {
        eprintln!("rejected; nothing to replay");
        return ExitCode::FAILURE;
    };
    default_params(rt, target, &mut graph);

    let text = match fs::read_to_string(log_path) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("error: could not read {}: {e}", log_path.display());
            return ExitCode::from(2);
        }
    };
    let log: EventLog = match serde_json::from_str(&text) {
        Ok(log) => log,
        Err(e) => {
            eprintln!(
                "error: {} is not an event log: {}",
                log_path.display(),
                error_chain(&e)
            );
            return ExitCode::from(2);
        }
    };

    match engine::verify_replay(graph, &log) {
        Ok(state) => {
            eprintln!(
                "replay is byte-identical: {} record(s), status {:?}",
                state.log.len(),
                state.folded_status()
            );
            ExitCode::SUCCESS
        }
        Err(mismatch) => {
            eprintln!("error: {mismatch}");
            ExitCode::FAILURE
        }
    }
}

/// Fill in what the file's own format says a host owes it, without overwriting
/// a parameter the graph already carries.
#[expect(
    clippy::print_stderr,
    reason = "a mistyped `--format` must reach the user instead of being swallowed"
)]
fn default_params(rt: &Runtime, target: &FileArgs, graph: &mut Graph) {
    let frontend = match rt.frontend_for(&target.file, target.format.as_deref()) {
        Ok(frontend) => frontend,
        // A named format that does not exist is a usage error worth saying,
        // even from this backstop; a path no frontend claims stays quiet —
        // there are simply no defaults to fill in.
        Err(error @ LoadError::UnknownFormat { .. }) => {
            eprintln!("warning: {error}");
            return;
        }
        Err(_) => return,
    };
    let repo = target
        .repo
        .clone()
        .unwrap_or_else(|| frontend.repo_root(&target.file));
    for (key, value) in frontend.default_params(&repo) {
        graph.params.entry(key).or_insert(value);
    }
}
