//! `inspect`: print what a run directory's durable files say.
//!
//! A thin printer over [`execution::inspect::inspect_run_dir`]. The command
//! opens the run directory's store for reading, replays the coordinator log,
//! the registered graphs and every execution's engine log, and prints the
//! versioned document on stdout. It takes no runtime and no lease, starts
//! nothing, and writes nothing under the run directory, so it is safe on a
//! run another process still holds and on a machine with no provider
//! reachable.

use std::fmt::Write as _;
use std::path::Path;
use std::process::ExitCode;

use execution::inspect::{RunInspection, inspect_run_dir};

/// Exit codes: 0 for a complete run, 1 for an incomplete one, 2 when the
/// files do not support a trustworthy reconstruction.
#[expect(
    clippy::print_stdout,
    reason = "the document is the command's output; a caller reads it on stdout"
)]
#[expect(
    clippy::print_stderr,
    reason = "the CLI reports what stopped the inspection to the user on stderr"
)]
pub(crate) async fn inspect(run_dir: &Path, json: bool) -> ExitCode {
    let inspection = match inspect_run_dir(run_dir).await {
        Ok(inspection) => inspection,
        Err(error) => {
            eprintln!("error: {}", super::error_chain(&error));
            return ExitCode::from(2);
        }
    };
    if json {
        match serde_json::to_string_pretty(&inspection) {
            Ok(text) => println!("{text}"),
            Err(error) => {
                eprintln!("error: could not encode the inspection: {error}");
                return ExitCode::from(2);
            }
        }
    } else {
        print!("{}", summary(&inspection));
    }
    if inspection.complete {
        ExitCode::SUCCESS
    } else {
        for reason in &inspection.incomplete {
            eprintln!("incomplete: {reason}");
        }
        ExitCode::FAILURE
    }
}

/// The human-readable form: one line per run fact, invocation and execution.
fn summary(inspection: &RunInspection) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "run: {}{}",
        inspection.status.as_deref().unwrap_or("unfinished"),
        if inspection.complete {
            ""
        } else {
            " (incomplete)"
        }
    );
    if inspection.paused {
        let _ = writeln!(
            out,
            "paused: yes (a resume holds admission until an unpause)"
        );
    }
    if !inspection.notes.is_empty() {
        let _ = writeln!(out, "notes: {} run-level note(s)", inspection.notes.len());
    }
    let _ = writeln!(
        out,
        "root: invocation {} final execution {}",
        inspection.root.invocation,
        inspection
            .root
            .final_execution
            .map_or_else(|| "none".to_owned(), |execution| execution.to_string()),
    );
    if let Some(receipt) = &inspection.interviews {
        let _ = writeln!(
            out,
            "interviews: {} question(s), {} error(s)",
            receipt.questions.len(),
            receipt.errors.len()
        );
    }
    for invocation in &inspection.invocations {
        let _ = writeln!(
            out,
            "invocation {}: {} graph {} executions [{}]{}",
            invocation.invocation,
            invocation.status,
            invocation.graph,
            invocation
                .executions
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            invocation
                .parent
                .as_ref()
                .map_or_else(String::new, |parent| {
                    format!(
                        " parent execution {} slot {}",
                        parent.execution, parent.slot
                    )
                }),
        );
    }
    for execution in &inspection.executions {
        let _ = writeln!(
            out,
            "execution {} (invocation {}, index {}): {} log {} records replay {}",
            execution.execution,
            execution.invocation,
            execution.execution_index,
            execution.status,
            execution.log.records,
            execution.log.replay,
        );
        if let Some(engine) = &execution.engine {
            for record in &engine.history {
                let _ = writeln!(
                    out,
                    "  {} {} gen {} attempt {}",
                    record.status, record.node, record.generation, record.attempt
                );
            }
        }
    }
    out
}
