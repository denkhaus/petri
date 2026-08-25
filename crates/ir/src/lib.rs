//! The engine IR: a directed graph (cycles permitted) executed with token-flow
//! semantics.
//!
//! A node fires when its [`JoinPolicy`] is satisfied by incoming tokens. On
//! completion its [`Routing`] emits tokens on outgoing edges. Routing is an
//! **AND of XORs**: each [`SelectGroup`] emits at most one token, and groups emit
//! concurrently. The default is selection — one completing node routes to exactly one
//! successor — so fan-out is never implicit. It takes writing more than one group,
//! which keeps single-path workflows analyzable and makes parallelism visible.
//!
//! This crate is data and pure functions only. Coordination lives in the `engine`
//! crate; side effects live behind traits in the host.

pub mod builder;
pub mod expr;
pub mod graph;
pub mod ids;
pub mod lower;
pub mod runtime;
pub mod step;
pub mod validate;

pub use builder::{Arm, GraphBuilder};
pub use expr::{BinOp, Context, EvalError, Expr, ExprTable, UnOp, eval, eval_bool, truthy};
pub use graph::{
    Budget, Edge, ExpandTarget, Expansion, ExprOrValue, Fallthrough, Graph, Guard, JoinPolicy,
    Node, Routing, RuntimeSpec, Scope, SelectGroup, StepRef, WorkspacePolicy,
};
pub use ids::{CancelScopeId, EdgeId, ExprId, FiringId, Generation, NodeId, ScopeId, StepKindId};
pub use lower::{
    CollectorExprs, LoopExprs, SequentialForEach, collector_exprs, loop_exprs, parallel_for_each,
    sequential_for_each,
};
pub use runtime::{
    Control, FailureInfo, LogStream, Metrics, Outcome, RunStatus, Status, StepEvent, Token,
};
pub use step::{Digest, StepKind, StepRegistry};
pub use validate::{ValidationError, validate, validate_plan, validate_with};

/// The value type carried by tokens, outcomes and step configuration.
pub type Value = serde_json::Value;
