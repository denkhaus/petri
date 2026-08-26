//! Petri, assembled.
//!
//! This is the one crate a consumer depends on. It does two jobs:
//!
//! - **Facade.** Every layer is reachable through it — [`ir`], [`engine`],
//!   [`driver`], [`frontend`] (with [`frontend::gha`] and [`frontend::native`]),
//!   [`steps`], and [`executor`] (with [`executor::host`] and [`executor::docker`])
//!   — so an external repository names one dependency and never a layer crate.
//! - **Assembly.** [`Runtime`] is the standard configuration: the known frontends,
//!   the built-in step kinds, and an executor per [`ir::RuntimeTarget`], wired into
//!   the driver with the replay canary on. A consumer registers its own frontends
//!   and step kinds on the same builder.
//!
//! ```no_run
//! # async fn demo() -> Result<(), String> {
//! let rt = petri::Runtime::standard()
//!     .options(petri::RunOptions::new("/tmp/petri-demo"));
//! let lowered = rt.check(std::path::Path::new("pipeline.yml"), None, None)?;
//! if let Some(graph) = lowered.graph {
//!     let report = rt.run(graph).await.expect("replay is byte-identical");
//!     println!("{:?}", report.status);
//! }
//! # Ok(())
//! # }
//! ```

pub use driver;
pub use engine;
pub use ir;

/// The executor interface, with the two local executors as submodules.
pub mod executor {
    pub use ::executor::*;
    pub use executor_docker as docker;
    pub use executor_host as host;
}

/// The frontend interface, with the two built-in formats as submodules.
pub mod frontend {
    pub use ::frontend::*;
    pub use frontend_gha as gha;
    pub use frontend_native as native;
}

/// Step kinds, with the standard registry.
pub mod steps {
    pub use ::steps::*;

    /// The built-in step kinds: `noop` and `process`.
    pub fn standard() -> Registry {
        let mut registry = Registry::new();
        registry.register(::steps::NoopStep);
        registry.register(::steps::ProcessStep);
        registry
    }
}

mod runtime;
mod target;

pub use runtime::{RunOptions, Runtime};
pub use target::TargetExecutor;
