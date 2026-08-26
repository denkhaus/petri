//! `petri` — the command line.
//!
//! `petri check <workflow>` parses, lowers and validates a workflow file — including
//! the step registry, so an unknown step kind or a bad literal config is caught here
//! — and prints every diagnostic with its span. `petri run` runs one to completion
//! on the standard runtime and writes the event log beside the workspaces.
//! `petri replay` feeds a saved log back through the engine and checks byte-identity
//! — the determinism canary, runnable from the shell.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use petri::frontend::Lowered;
use petri::ir::{Graph, RunStatus};
use petri::{RunOptions, Runtime};

#[derive(Parser)]
#[command(name = "petri", version, about = "A token-flow workflow engine")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct FileArgs {
    /// The workflow file.
    file: PathBuf,
    /// Which format the file is in (`gha` or `native`). Guessed from its path when
    /// omitted: anything under `.github/workflows/` is GitHub Actions, everything
    /// else is native.
    #[arg(long)]
    format: Option<String>,
    /// Repository root, for resolving `uses: ./local/action`. Defaults to the
    /// nearest ancestor of the file containing `.github/`, else the file's dir.
    #[arg(long)]
    repo: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Parse, lower and validate a workflow file; print diagnostics.
    Check {
        #[command(flatten)]
        target: FileArgs,
        /// Print the lowered graph in a stable text form.
        #[arg(long)]
        print_graph: bool,
        /// Print diagnostics as JSON lines instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Lower a workflow file and print the graph in a stable text form.
    PrintGraph {
        #[command(flatten)]
        target: FileArgs,
    },
    /// Run a workflow file to completion on the standard runtime.
    Run {
        #[command(flatten)]
        target: FileArgs,
        /// Where workspaces, logs and `events.json` go. Defaults to a fresh
        /// directory under the system temp dir, printed at start.
        #[arg(long)]
        run_dir: Option<PathBuf>,
        /// Do not echo step output.
        #[arg(long)]
        quiet: bool,
    },
    /// Replay a saved event log against the workflow and verify byte-identity.
    ///
    /// Lower the same file on the same machine as the original run: the graph —
    /// including the default run parameters — is the replay's input.
    Replay {
        #[command(flatten)]
        target: FileArgs,
        /// The `events.json` a `petri run` wrote.
        log: PathBuf,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Check {
            target,
            print_graph,
            json,
        } => check(&target, print_graph, json),
        Command::PrintGraph { target } => check(&target, true, false),
        Command::Run {
            target,
            run_dir,
            quiet,
        } => run(&target, run_dir, quiet).await,
        Command::Replay { target, log } => replay(&target, &log),
    }
}

/// Lower and validate, or explain why not. `Err` carries the exit code.
fn lowered_graph(rt: &Runtime, target: &FileArgs, json: bool) -> Result<Lowered, ExitCode> {
    match rt.check(
        &target.file,
        target.format.as_deref(),
        target.repo.as_deref(),
    ) {
        Ok(lowered) => {
            for d in lowered.diagnostics.iter() {
                if json {
                    println!("{}", serde_json::to_string(d).unwrap_or_default());
                } else {
                    eprintln!("{d}");
                }
            }
            Ok(lowered)
        }
        Err(message) => {
            eprintln!("error: {message}");
            Err(ExitCode::from(2))
        }
    }
}

fn check(target: &FileArgs, print_graph: bool, json: bool) -> ExitCode {
    let rt = Runtime::standard();
    let lowered = match lowered_graph(&rt, target, json) {
        Ok(lowered) => lowered,
        Err(code) => return code,
    };
    let errors = lowered.diagnostics.errors().count();
    let warnings = lowered.diagnostics.warnings().count();
    match &lowered.graph {
        Some(graph) => {
            if print_graph {
                print!("{}", petri::frontend::print_graph(graph));
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
        }
        None => {
            if !json {
                eprintln!("rejected: {errors} error(s), {warnings} warning(s)");
            }
            ExitCode::FAILURE
        }
    }
}

async fn run(target: &FileArgs, run_dir: Option<PathBuf>, quiet: bool) -> ExitCode {
    let run_dir = run_dir
        .unwrap_or_else(|| std::env::temp_dir().join(format!("petri-run-{}", std::process::id())));
    let mut options = RunOptions::new(&run_dir);
    options.echo = !quiet;
    let rt = Runtime::standard().options(options);

    let lowered = match lowered_graph(&rt, target, false) {
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
    default_params(&rt, target, &mut graph);

    eprintln!("run dir: {}", run_dir.display());
    let report = match rt.run(graph).await {
        Ok(report) => report,
        Err(mismatch) => {
            eprintln!("error: {mismatch}");
            return ExitCode::from(3);
        }
    };

    let log_path = run_dir.join("events.json");
    match serde_json::to_vec_pretty(&report.state.log) {
        Ok(bytes) => {
            if let Err(e) = std::fs::write(&log_path, bytes) {
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
    eprintln!("run: {status:?}");
    if status == RunStatus::Success {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn replay(target: &FileArgs, log_path: &Path) -> ExitCode {
    let rt = Runtime::standard();
    let lowered = match lowered_graph(&rt, target, false) {
        Ok(lowered) => lowered,
        Err(code) => return code,
    };
    let Some(mut graph) = lowered.graph else {
        eprintln!("rejected; nothing to replay");
        return ExitCode::FAILURE;
    };
    default_params(&rt, target, &mut graph);

    let text = match std::fs::read_to_string(log_path) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("error: could not read {}: {e}", log_path.display());
            return ExitCode::from(2);
        }
    };
    let log: petri::engine::EventLog = match serde_json::from_str(&text) {
        Ok(log) => log,
        Err(e) => {
            eprintln!("error: {} is not an event log: {e}", log_path.display());
            return ExitCode::from(2);
        }
    };

    match petri::engine::verify_replay(graph, &log) {
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

/// The run parameters a host owes a GHA graph: the `github`, `runner` and `vars`
/// contexts. Fixed values, so a replay lowers to the identical graph.
fn default_params(rt: &Runtime, target: &FileArgs, graph: &mut Graph) {
    let is_gha = rt
        .frontend_for(&target.file, target.format.as_deref())
        .map(|f| f.name() == "gha")
        .unwrap_or(false);
    if !is_gha {
        return;
    }
    let repo = target
        .repo
        .clone()
        .unwrap_or_else(|| Runtime::guess_repo(&target.file));
    let name = repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string());
    graph
        .params
        .entry("github".into())
        .or_insert(serde_json::json!({
            "repository": format!("local/{name}"),
            "event_name": "workflow_dispatch",
            "actor": "petri",
            "ref": "refs/heads/main",
            "ref_name": "main",
            "sha": "0000000000000000000000000000000000000000",
            "run_id": "1",
            "run_number": "1",
        }));
    graph
        .params
        .entry("runner".into())
        .or_insert(serde_json::json!({
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "name": "local",
        }));
    graph
        .params
        .entry("vars".into())
        .or_insert(serde_json::json!({}));
}
