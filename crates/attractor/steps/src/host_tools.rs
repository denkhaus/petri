//! Host tools on the native backend: an embedding application's own tools,
//! registered on every native session beside Pebble's.
//!
//! A host that embeds Petri (Fabro's run tools, say) registers a
//! [`HostTools`] capability. Each native session built by
//! [`crate::pebble::NativeSession::open`] asks it for the node's tools with a
//! [`HostToolContext`], the Petri-defined identity of the stage the tools
//! serve, and passes the tools to Pebble's builder with `.tools(...)`. From
//! there a host tool is a tool like any other: the model sees its
//! definition, every call passes through the run's tool hooks
//! (`crate::hooks::tools::ToolHooks`, so a `pre_tool_use` hook can block
//! it), Pebble reports `ToolCallStarted` and `ToolCallCompleted` on its
//! event stream, and the node's sink records them in the `pebble` envelope
//! under the stage's node, firing and attempt. A tool marked
//! `RegisteredTool::allow_in_subagents` reaches the session's children
//! through Pebble's own inheritance; Petri adds nothing to that rule.
//!
//! The tools are built once per session open: every node, every attempt,
//! and a node that continues a retained thread each call the builders with
//! their own context, so a tool's context names the stage calling it and a
//! child's tools carry the parent node's context, as the child does the
//! parent's work. The builders run on the step's task and should return
//! quickly; a tool that needs a connection opens it when called.
//!
//! The standalone runner registers no `HostTools`, so `petri run` is
//! unchanged. The context needs the coordinator's
//! [`execution::ExecutionIdentity`]: a driver built outside the coordinator
//! has none, and a run that registered `HostTools` on such a driver fails
//! its agent nodes with `capability_unavailable` rather than run without
//! the tools the host asked for.

use std::sync::Arc;

use execution::{ExecutionId, ExecutionIdentity, InvocationId, RunKey};
use ir::{Attempt, FiringId};
use pebble_coding_agent::tools::RegisteredTool;
use smol_str::SmolStr;
use steps::{StepCtx, StepFailure};

/// The stage a host tool serves, as Petri names it: the run, the invocation
/// and execution the stage runs in, and the node, firing and attempt that
/// opened the session. Built by Petri; a host reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct HostToolContext {
    pub run:        RunKey,
    pub invocation: InvocationId,
    pub execution:  ExecutionId,
    /// The node's instance name (`build`, or `build#2` for an expansion
    /// clone).
    pub node:       SmolStr,
    pub firing:     FiringId,
    pub attempt:    Attempt,
}

/// What builds one host's tools for a stage.
pub type HostToolBuilder = Arc<dyn Fn(&HostToolContext) -> Vec<RegisteredTool> + Send + Sync>;

/// Host capability: the tools an embedding application gives every native
/// agent session, as builders called once per session with the stage's
/// [`HostToolContext`]. Register it with `Runtime::capability`; absent, a
/// session has Pebble's tools alone.
#[derive(Clone, Default)]
pub struct HostTools {
    builders: Vec<HostToolBuilder>,
}

impl HostTools {
    /// No tools yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a builder. Builders run in the order they were added; Pebble
    /// refuses a session whose tools share a visible name.
    #[must_use]
    pub fn with(
        mut self,
        builder: impl Fn(&HostToolContext) -> Vec<RegisteredTool> + Send + Sync + 'static,
    ) -> Self {
        self.builders.push(Arc::new(builder));
        self
    }

    /// Whether no builder was added.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.builders.is_empty()
    }

    /// Every builder's tools for `context`, in builder order.
    #[must_use]
    pub fn build(&self, context: &HostToolContext) -> Vec<RegisteredTool> {
        self.builders
            .iter()
            .flat_map(|builder| builder(context))
            .collect()
    }
}

/// The host's tools for the session `ctx` opens: none when the run has no
/// `HostTools`, else every builder's tools for the node's context. A run
/// with `HostTools` but no [`ExecutionIdentity`] (a driver built outside the
/// coordinator) fails the node routably.
pub(crate) fn for_node(ctx: &StepCtx) -> Result<Vec<RegisteredTool>, StepFailure> {
    let Some(tools) = ctx.capability::<HostTools>() else {
        return Ok(Vec::new());
    };
    let identity = ctx.require_capability::<ExecutionIdentity>()?;
    let context = HostToolContext {
        run:        identity.run.clone(),
        invocation: identity.invocation,
        execution:  identity.execution,
        node:       ctx.node.clone(),
        firing:     ctx.firing,
        attempt:    ctx.attempt,
    };
    Ok(tools.build(&context))
}
