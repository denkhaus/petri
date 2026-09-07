//! Host policies a Fabro graph declares: the stall watchdog, the failure
//! circuit breaker, and which side enforces each node's timeout.
//!
//! These attributes never change routing. They become [`ir::RunPolicy`] on
//! the graph and [`ir::TimeoutPolicy`] on each node's budget, and the host
//! (the driver, the coordinator) enforces them. Fabro's defaults apply: a
//! 30 minute stall budget, three repeats of one failure signature.

use std::num::NonZeroU32;
use std::time::Duration;

use frontend::Diagnostics;
use ir::{RunPolicy, TimeoutPolicy};

use super::Kind;
use crate::model::{NodeDecl, Workflow};

/// Fabro's default `stall_timeout`.
pub const DEFAULT_STALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Fabro's default `loop_restart_signature_limit`.
pub const DEFAULT_SIGNATURE_LIMIT: u32 = 3;

/// The graph's run policy from its attributes. `stall_timeout=0` disables
/// the watchdog; a `loop_restart_signature_limit` below 1 is refused.
pub(super) fn run_policy(workflow: &Workflow, diags: &mut Diagnostics) -> RunPolicy {
    let stall_timeout = match workflow.attrs.duration("stall_timeout", diags) {
        Some(zero) if zero.is_zero() => None,
        Some(explicit) => Some(explicit),
        None if workflow.attrs.contains("stall_timeout") => None,
        None => Some(DEFAULT_STALL_TIMEOUT),
    };
    let limit = match workflow.attrs.int("loop_restart_signature_limit", diags) {
        Some(limit) => {
            let parsed = u32::try_from(limit).ok().and_then(NonZeroU32::new);
            if parsed.is_none() {
                diags.error(
                    "fabro.bad_signature_limit",
                    workflow
                        .attrs
                        .span_of("loop_restart_signature_limit", &workflow.span),
                    format!("`loop_restart_signature_limit={limit}` must be at least 1"),
                );
            }
            parsed
        }
        None if workflow.attrs.contains("loop_restart_signature_limit") => None,
        None => NonZeroU32::new(DEFAULT_SIGNATURE_LIMIT),
    };
    RunPolicy {
        stall_timeout,
        loop_restart_signature_limit: limit,
    }
}

/// Who enforces a node's `timeout`, as Fabro's handlers declare it: a
/// command sends its deadline to the sandbox, a human gate's timeout is its
/// answer deadline, an ACP agent hands the deadline to its turn; every other
/// node (a native API agent, a prompt on the native backend, a wait, a nested
/// workflow) leaves the driver's interview-aware timer in charge.
pub(super) fn timeout_policy(kind: Kind, node: &NodeDecl, workflow: &Workflow) -> TimeoutPolicy {
    match kind {
        Kind::Command | Kind::Human => TimeoutPolicy::HandlerManaged,
        Kind::Agent | Kind::Prompt => {
            let backend = node
                .attrs
                .text("backend")
                .or_else(|| workflow.attrs.text("backend"));
            match backend.as_deref() {
                Some("api") => TimeoutPolicy::ExecutorEnforced,
                _ => TimeoutPolicy::HandlerManaged,
            }
        }
        Kind::Start
        | Kind::Exit
        | Kind::Conditional
        | Kind::Parallel
        | Kind::FanIn
        | Kind::Wait
        | Kind::ManagerLoop => TimeoutPolicy::ExecutorEnforced,
    }
}
