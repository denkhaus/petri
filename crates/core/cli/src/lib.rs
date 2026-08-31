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

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{self, ExitCode};
use std::{env, fs};

use clap::{Args, Parser, Subcommand};
use runtime::driver::RunHandle;
use runtime::engine::{self, EventLog};
use runtime::frontend::{self, Lowered};
use runtime::ir::{CancelScopeId, Graph, RunStatus};
use runtime::{LoadError, RunOptions, Runtime};
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
    file:   PathBuf,
    /// Which format the file is in. Guessed from its path when omitted; the
    /// last frontend asked claims everything, so an unrecognized path is
    /// native.
    #[arg(long)]
    format: Option<String>,
    /// Repository root, for resolving a format's local includes. Defaults to
    /// wherever the file's own format says its repository root is.
    #[arg(long)]
    repo:   Option<PathBuf>,
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
        target:  FileArgs,
        /// Where workspaces, logs and `events.json` go. Defaults to a fresh
        /// directory under the system temp dir, printed at start.
        #[arg(long)]
        run_dir: Option<PathBuf>,
        /// Do not echo step output.
        #[arg(long)]
        quiet:   bool,
    },
    /// Replay a saved event log against the workflow and verify byte-identity.
    ///
    /// Lower the same file on the same machine — and the same checkout state,
    /// since a format's default run parameters may read it (GHA reads HEAD) —
    /// as the original run: the graph, including the default run parameters,
    /// is the replay's input. A host that persists the graph itself (the run
    /// dir's `graph.json`) has no such constraint.
    Replay {
        #[command(flatten)]
        target: FileArgs,
        /// The `events.json` a run wrote.
        log:    PathBuf,
    },
}

/// Parse the arguments and run the command, on a runtime from `make`. One
/// command, one runtime.
pub async fn main(make: impl Fn() -> Runtime) -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Check {
            target,
            print_graph,
            json,
        } => check(&make(), &target, print_graph, json),
        Command::PrintGraph { target } => check(&make(), &target, true, false),
        Command::Run {
            target,
            run_dir,
            quiet,
        } => {
            let run_dir = run_dir
                .unwrap_or_else(|| env::temp_dir().join(format!("petri-run-{}", process::id())));
            let mut options = RunOptions::new(&run_dir);
            options.echo = !quiet;
            run(&make().options(options), &target, &run_dir).await
        }
        Command::Replay { target, log } => replay(&make(), &target, &log),
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
fn lowered_graph(rt: &Runtime, target: &FileArgs, json: bool) -> Result<Lowered, ExitCode> {
    match rt.check(
        &target.file,
        target.format.as_deref(),
        target.repo.as_deref(),
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
    let lowered = match lowered_graph(rt, target, json) {
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
// The run dir is `driver.run`'s field, and the user's first line of output. It
// is not repeated here.
#[tracing::instrument(
    name = "cli.run",
    skip_all,
    fields(workflow_file = %target.file.display(), status = Empty)
)]
async fn run(rt: &Runtime, target: &FileArgs, run_dir: &Path) -> ExitCode {
    let lowered = match lowered_graph(rt, target, false) {
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
    let outcome = rt
        .run_verified(graph, |graph| {
            let driver = rt.driver(graph);
            ctrl_c = Some(tokio::spawn(cancel_on_ctrl_c(driver.handle())));
            Ok::<_, engine::ReplayMismatch>(driver)
        })
        .await;
    if let Some(task) = ctrl_c {
        task.abort();
    }
    let report = match outcome {
        Ok(report) => report,
        Err(mismatch) => {
            eprintln!("error: {mismatch}");
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
    let status = report.status;
    tracing::Span::current().record("status", display(status));
    eprintln!("run: {status}");
    if status == RunStatus::Success {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Map Ctrl-C onto the driver's two-tier stop: the first cancels the run —
/// cleanup steps and release still happen — and any further Ctrl-C feeds the
/// kill tier. The task holds no cleanup-sensitive state; the run aborts it
/// once the report is in.
#[expect(
    clippy::print_stderr,
    reason = "the CLI tells the user what each Ctrl-C did on stderr"
)]
async fn cancel_on_ctrl_c(handle: RunHandle) {
    let mut cancelled = false;
    loop {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        if cancelled {
            eprintln!("killing the run");
        } else {
            eprintln!("cancelling the run; Ctrl-C again to kill");
            cancelled = true;
        }
        handle.cancel(CancelScopeId::ROOT).await;
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
    let lowered = match lowered_graph(rt, target, false) {
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
