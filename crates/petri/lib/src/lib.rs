//! Petri, as it ships.
//!
//! This is the distribution: the one crate a consumer depends on, and the only
//! crate in the tree that names every component. Core is `petri-runtime` and knows
//! the native format and nothing else; each component — today, GitHub Actions —
//! is its own crate and knows core. Here the two meet: [`runtime()`] is core's
//! standard runtime with every in-tree component registered on it.
//!
//! Everything else is re-export. [`ir`], [`engine`], [`driver`], [`frontend`]
//! (with [`frontend::gha`] and [`frontend::native`]), [`steps`] and [`executor`]
//! (with [`executor::host`] and [`executor::docker`]) are reachable through this
//! crate, so an external repository names one dependency and never a layer crate.
//!
//! The one thing here that is not wiring or re-export is [`host`]: the
//! standalone host's durable run dir — `graph.json`, `events.jsonl`, and the
//! run wrappers that keep a run resumable when no product store sits behind it.
//!
//! ```no_run
//! # async fn demo() -> Result<(), String> {
//! let rt = petri::runtime().options(petri::RunOptions::new("/tmp/petri-demo"));
//! let lowered = rt.check(std::path::Path::new("pipeline.yml"), None, None)?;
//! if let Some(graph) = lowered.graph {
//!     let report = rt.run(graph).await.expect("replay is byte-identical");
//!     println!("{:?}", report.status);
//! }
//! # Ok(())
//! # }
//! ```

pub use runtime::{RunOptions, Runtime, TargetExecutor};
pub use runtime::{driver, engine, ir};

pub mod host;

/// The executor interface, with the two local executors as submodules.
pub mod executor {
    pub use runtime::executor::*;
}

/// The frontend interface, with every format this distribution ships.
pub mod frontend {
    pub use frontend_gha as gha;
    pub use runtime::frontend::*;
}

/// Step kinds, with the standard registry.
pub mod steps {
    pub use runtime::steps::*;
}

/// The GitHub Actions component's run-time half: its step kinds and action source.
pub mod github {
    pub use github_actions::*;
}

/// The shipped configuration: core's standard runtime plus every in-tree component.
///
/// GitHub Actions comes with an action source that fetches from GitHub into
/// [`github::default_cache_dir`], its two step kinds, and `GITHUB_TOKEN` as a secret
/// when this machine has one (`$GITHUB_TOKEN`, else `gh auth token`).
///
/// A consumer that wants a different set builds one itself — `Runtime::standard()`
/// for core alone, `Runtime::bare()` for nothing — and registers what it wants.
pub fn runtime() -> Runtime {
    let actions: std::sync::Arc<dyn github::ActionSource> =
        std::sync::Arc::new(github::GitActionSource::new(github::default_cache_dir()));
    Runtime::standard()
        .frontend(frontend_gha::GitHubActions::with_actions(
            std::sync::Arc::clone(&actions),
        ))
        .step(github::RunStep)
        .step(github::ActionStep)
        .capability(github::ActionSourceCap(actions))
        .secrets(github_secrets())
}

/// The run's secrets: `GITHUB_TOKEN` from the environment, else from `gh auth
/// token` when `gh` is installed and logged in, else none. `github.token` in a
/// workflow resolves to it at spawn time and never enters the graph or the log.
fn github_secrets() -> executor::MapSecrets {
    let token = std::env::var("GITHUB_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty())
        .or_else(|| {
            std::process::Command::new("gh")
                .args(["auth", "token"])
                .stderr(std::process::Stdio::null())
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|t| !t.is_empty())
        });
    match token {
        Some(token) => executor::MapSecrets::from_pairs(&[(
            frontend_gha::exprs::GITHUB_TOKEN_SECRET,
            token.as_str(),
        )]),
        None => executor::MapSecrets::empty(),
    }
}
