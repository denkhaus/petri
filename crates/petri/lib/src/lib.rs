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

/// The shipped configuration: core's standard runtime plus every in-tree component.
///
/// A consumer that wants a different set builds one itself — `Runtime::standard()`
/// for core alone, `Runtime::bare()` for nothing — and registers what it wants.
pub fn runtime() -> Runtime {
    Runtime::standard().frontend(frontend_gha::GitHubActions)
}
