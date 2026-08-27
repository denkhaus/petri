//! Core, assembled.
//!
//! Every other core crate is one layer and names no other implementation: the IR
//! knows no executor, the executor interface knows no backend, the frontend trait
//! knows no format. This crate is the one place those pieces are wired together —
//! [`Runtime`] holds a frontend list, a step registry, an executor per
//! [`ir::RuntimeTarget`], secrets and options, and drives a graph with the replay
//! canary on. It is also the facade: [`ir`], [`engine`], [`driver`], [`frontend`],
//! [`steps`] and [`executor`] are reachable through it, so one dependency reaches
//! all of core.
//!
//! The wiring goes one way. A component — another workflow format, another step
//! kind — *registers onto* this builder with [`Runtime::frontend`] and
//! [`Runtime::step`]; nothing registers *into* it, and no crate under `core` names
//! a component. The distribution that ships is assembled a layer up, in `petri`,
//! which is the crate that names them all.

pub use driver;
pub use engine;
pub use ir;

/// The executor interface, with the two local executors as submodules.
pub mod executor {
    pub use ::executor::*;
    pub use executor_docker as docker;
    pub use executor_host as host;
}

/// The frontend interface, with the native format as a submodule.
pub mod frontend {
    pub use ::frontend::*;
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

mod local;
mod runtime;

pub use local::LocalExecutor;
pub use runtime::{RunOptions, Runtime};
