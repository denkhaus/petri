//! GitHub Actions step kinds: what a `run:` step and a `uses:` step do at run
//! time.
//!
//! Both are the core `process` step with GitHub's runner contract around it. A
//! [`RunStep`] runs the script with the `GITHUB_*` files in place and applies
//! what the script wrote to them; an [`ActionStep`] stages a fetched JavaScript
//! action and runs its entry point with `INPUT_*` set. Neither knows where the
//! process runs: they build a `ProcessConfig` and hand it to the process step,
//! swapping the log sender so `::` workflow commands are seen on the way past.
//!
//! The frontend (`frontend_gha`) lowers steps to these kinds and resolves
//! actions to commits at load time through an [`ActionSource`];
//! [`GitActionSource`] is the one that fetches from git. The step finds the
//! same source as a capability ([`ActionSourceCap`]) to stage the tree at run
//! time.

mod action;
mod checkout;
pub mod commands;
pub mod config;
mod docker;
pub mod gate;
pub mod hashfiles;
mod results;
mod run;
pub mod session;
pub mod source;

pub use action::{ActionSourceCap, ActionStep, ActionTreeSource};
pub use checkout::CheckoutStep;
pub use docker::DockerActionStep;
pub use frontend_gha::action::{ActionRef, ActionSource, ActionSourceError, PinnedAction};
pub use frontend_gha::{
    ACTION_KIND, CHECKOUT_KIND, DOCKER_ACTION_KIND, RUN_KIND, STATE_OUTPUT_KEY,
};
pub use results::{ResultsServiceCap, ToolCacheCap};
pub use run::RunStep;
pub use source::{GitActionSource, default_cache_dir};
