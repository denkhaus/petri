//! `petri` — the command line.
//!
//! `petri check <workflow>` parses, lowers and validates a workflow file and prints
//! every diagnostic with its span. Exit status is non-zero on any Error. This is the
//! fastest feedback loop there is, and the first thing a user will run.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use frontend::{Lowered, Severity};

#[derive(Parser)]
#[command(name = "petri", version, about = "A token-flow workflow engine")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Parse, lower and validate a workflow file; print diagnostics.
    Check {
        /// The workflow file.
        file: PathBuf,
        /// Which format the file is in. Guessed from its path when omitted: anything
        /// under `.github/workflows/` is GitHub Actions, everything else is native.
        #[arg(long, value_enum)]
        format: Option<Format>,
        /// Repository root, for resolving `uses: ./local/action`. Defaults to the
        /// nearest ancestor of the file containing `.github/`, else the file's dir.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Print the lowered graph in a stable text form.
        #[arg(long)]
        print_graph: bool,
        /// Print diagnostics as JSON lines instead of text.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Format {
    Gha,
    Native,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Check {
            file,
            format,
            repo,
            print_graph,
            json,
        } => check(&file, format, repo, print_graph, json),
    }
}

/// Where the repository root is, for a workflow file.
fn guess_repo(file: &Path) -> PathBuf {
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

fn guess_format(file: &Path) -> Format {
    let text = file.to_string_lossy();
    if text.contains(".github/workflows") || text.contains(".github\\workflows") {
        Format::Gha
    } else {
        Format::Native
    }
}

/// Lower one file with the given format. Shared with the corpus harness.
pub fn lower_file(file: &Path, format: Format, repo: &Path) -> Result<Lowered, String> {
    let text = std::fs::read_to_string(file)
        .map_err(|e| format!("could not read {}: {e}", file.display()))?;
    let name = file
        .strip_prefix(repo)
        .unwrap_or(file)
        .to_string_lossy()
        .into_owned();
    Ok(match format {
        Format::Gha => frontend_gha::load(
            &name,
            &text,
            &frontend_gha::DirFiles {
                root: repo.to_path_buf(),
            },
        ),
        Format::Native => frontend_native::load(&name, &text),
    })
}

fn check(
    file: &Path,
    format: Option<Format>,
    repo: Option<PathBuf>,
    print_graph: bool,
    json: bool,
) -> ExitCode {
    let format = format.unwrap_or_else(|| guess_format(file));
    let repo = repo.unwrap_or_else(|| guess_repo(file));
    let lowered = match lower_file(file, format, &repo) {
        Ok(l) => l,
        Err(message) => {
            eprintln!("error: {message}");
            return ExitCode::from(2);
        }
    };

    for d in lowered.diagnostics.iter() {
        if json {
            println!("{}", serde_json::to_string(d).unwrap_or_default());
        } else {
            eprintln!("{d}");
        }
    }

    let errors = lowered.diagnostics.errors().count();
    let warnings = lowered.diagnostics.warnings().count();
    match &lowered.graph {
        Some(graph) => {
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
        }
        None => {
            if !json {
                eprintln!("rejected: {errors} error(s), {warnings} warning(s)");
            }
            ExitCode::FAILURE
        }
    }
}

#[allow(dead_code)]
fn _severity(_: Severity) {}
