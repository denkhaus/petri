//! The Fabro black box harness: stage a scenario, start provider twins on
//! loopback, run the shipped `petri` binary on it in an isolated environment
//! with a scripted interviewer, and read what the run left behind.
//!
//! The harness never lowers a graph, registers a step, or touches engine
//! state. It invokes public commands and reads their JSON and filesystem
//! outputs. Task 3 added the scenario staging, subprocess control, and run
//! observation modules; task 4 added the twins, the isolated launcher, and the
//! interview scripts; task 5 routed every read of a finished run through
//! `petri inspect --json`. Task 17 adds the scenario matrix and task 18 the
//! pinned Fabro adapter (`crates/fabro/acceptance/scenarios/*/fabro-reference/`
//! already holds the first capture and its `capture.sh`).

#![allow(
    dead_code,
    unused_imports,
    reason = "each test file uses the subset it needs"
)]

pub(crate) mod bundle;
pub(crate) mod compare;
pub(crate) mod evidence;
pub(crate) mod fabro_adapter;
pub(crate) mod failures;
pub(crate) mod inspect;
pub(crate) mod interview;
pub(crate) mod launch;
pub(crate) mod observe;
pub(crate) mod record;
pub(crate) mod require;
pub(crate) mod scenario;
pub(crate) mod subagents;
pub(crate) mod subprocess;
pub(crate) mod twins;

pub(crate) use bundle::Scenario;
pub(crate) use observe::{BranchEnvelope, RunObservation};
pub(crate) use subprocess::{Petri, RunOutput};
