//! GitHub Actions step kinds: what a `run:` step and a `uses:` step do at run
//! time.
//!
//! Both are the core `process` step with GitHub's runner contract around it. A
//! [`RunStep`] runs the script with the `GITHUB_*` files in place and applies
//! what the script wrote to them; an [`ActionStep`] stages a fetched JavaScript
//! action and runs its entry point with `INPUT_*` set. Neither knows where the
//! process runs: they build the session's `ResolvedProcess` carrier and hand
//! its resolved parts to the shared process-step machinery, swapping the log
//! sender so `::` workflow commands are seen on the way past.
//!
//! The frontend (`frontend_gha`) pins remote actions at load time through an
//! [`ActionSource`]. A deferred resolver reads every manifest when the action
//! is reached, then appends its executable nodes. [`GitActionSource`] fetches
//! from git. Action steps find the same source through [`ActionSourceCap`] to
//! stage the pinned tree at run time.

mod action;
mod background;
mod checkout;
mod commands;
mod config;
mod deferred;
mod docker;
mod gate;
mod hashfiles;
mod results;
mod run;
mod session;
mod source;

pub use action::{ActionSourceCap, ActionStep, ActionTreeSource};
pub use background::{
    BackgroundCompleteStep, BackgroundPublishStep, BackgroundStartStep, BackgroundWaitStep,
};
pub use checkout::CheckoutStep;
pub use deferred::{
    ActionManifestSourceCap, DeferredActionPostStep, DeferredActionPublishStep,
    DeferredActionResultStep, DeferredActionStep,
};
pub use docker::DockerActionStep;
pub use frontend_gha::action::{ActionRef, ActionSourceError, PinnedAction};
pub use frontend_gha::{
    ACTION_KIND, ActionSource, BACKGROUND_COMPLETE_KIND, BACKGROUND_PUBLISH_KIND,
    BACKGROUND_START_KIND, BACKGROUND_WAIT_KIND, CHECKOUT_KIND, DEFERRED_ACTION_KIND,
    DEFERRED_ACTION_POST_KIND, DEFERRED_ACTION_PUBLISH_KIND, DEFERRED_ACTION_RESULT_KIND,
    DOCKER_ACTION_KIND, RUN_KIND, STATE_OUTPUT_KEY,
};
pub use results::{ResultsServiceCap, ToolCacheCap};
pub use run::RunStep;
pub use source::{GitActionSource, default_cache_dir};
