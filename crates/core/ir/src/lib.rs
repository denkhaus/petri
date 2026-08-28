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
pub mod desugar;
pub mod expr;
pub mod flow;
pub mod graph;
pub mod ids;
pub mod placeholder;
pub mod step;
pub mod validate;

pub use builder::{Arm, GraphBuilder};
pub use desugar::{
    CollectorExprs, LoopExprs, SequentialForEach, collector_exprs, loop_exprs, loop_exprs_over,
    normalize_loop_heads, parallel_for_each, sequential_for_each, sequential_for_each_over,
};
pub use expr::{
    BinOp, EvalEnv, EvalError, Expr, ExprTable, StaticCtx, UnOp, eval, eval_bool, truthy,
};
pub use flow::{
    Control, FailureInfo, LogStream, Metrics, NodeRecord, Outcome, RunContext, RunStatus, Status,
    StatusKind, StepEvent, Token,
};
pub use graph::{
    Backoff, Budget, Completion, Edge, Exhaustion, ExpandTarget, Expansion, ExprOrValue,
    Fallthrough, Graph, Guard, JoinPolicy, Node, RegistryCredentials, RetryOn, RetryPolicy,
    Routing, RuntimeSpec, RuntimeTarget, Scope, SelectGroup, ServiceSpec, StepRef, WorkspacePolicy,
};
pub use ids::{
    Attempt, CancelScopeId, EdgeId, ExprId, FiringId, Generation, NodeId, ScopeId, StepKindId,
};
pub use step::{Digest, StepKind, StepKinds};
pub use validate::{
    ValidationError, ValidationReport, ValidationWarning, check, check_with, validate,
    validate_plan, validate_with,
};

/// The value type carried by tokens, outcomes and step configuration.
pub type Value = serde_json::Value;
