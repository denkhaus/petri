//! The Fabro black box harness: stage a scenario, run the shipped `petri`
//! binary on it, and read what the run left behind.
//!
//! The harness never lowers a graph, registers a step, or touches engine
//! state. It invokes public commands and reads their JSON and filesystem
//! outputs. Later tasks extend it: task 4 adds the twin lifecycle and the
//! scripted interviewer, task 17 the scenario matrix, task 18 the pinned
//! Fabro adapter (`crates/fabro/acceptance/scenarios/*/fabro-reference/`
//! already holds the first capture and its `capture.sh`).

pub(crate) mod bundle;
pub(crate) mod observe;
pub(crate) mod subprocess;

pub(crate) use bundle::Scenario;
pub(crate) use observe::{BranchEnvelope, RunObservation};
pub(crate) use subprocess::{Petri, RunOutput};
