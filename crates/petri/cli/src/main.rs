//! `petri` — the binary.
//!
//! The whole of it: the distribution's runtime, handed to the format-agnostic
//! command line. What the commands do lives in `cli`; which frontends and step
//! kinds they see is decided here, by [`petri::runtime`].

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    logging::init();
    cli::main(|mode| match mode {
        cli::RuntimeMode::Real => petri::runtime(),
        cli::RuntimeMode::DryRun => petri::dry_run_runtime(),
    })
    .await
}

/// Operator diagnostics.
///
/// Every other crate emits spans and events and nothing else. The binary is the
/// application, so it alone decides where that output goes, how much of it
/// there is, and what it looks like.
mod logging {
    use std::env;
    use std::io::{self, IsTerminal};

    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::filter::LevelFilter;

    /// The knob, in `tracing-subscriber`'s filter syntax: `PETRI_LOG=info`,
    /// `PETRI_LOG=petri_executor_docker=debug`. Deliberately not `RUST_LOG`,
    /// which is set globally for other tools and would switch petri's internals
    /// on by surprise.
    const FILTER_ENV: &str = "PETRI_LOG";

    /// Install the process-wide subscriber: warnings and errors, on stderr.
    ///
    /// stderr, not stdout: the commands' own output — `--json`, the printed
    /// graph, echoed step output — is stdout's, and diagnostics must not
    /// interleave with it.
    pub(super) fn init() {
        let configured = env::var_os(FILTER_ENV).is_some();
        let filter = EnvFilter::builder()
            .with_default_directive(LevelFilter::WARN.into())
            .with_env_var(FILTER_ENV)
            .from_env_lossy();
        let builder = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(io::stderr)
            .with_ansi(io::stderr().is_terminal());
        // Unset, a warning reads much like the line it replaces. Set, it comes
        // with the timestamp and the target an investigation wants.
        if configured {
            builder.compact().init();
        } else {
            builder.without_time().with_target(false).compact().init();
        }
    }
}
