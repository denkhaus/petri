//! A pure, sans-IO token-flow state machine.
//!
//! The engine executes an [`ir::Graph`] with token-flow semantics: a node fires
//! when its join policy is satisfied by incoming tokens, and on completion its
//! routing policy emits tokens on outgoing edges. All coordination state lives
//! in [`EngineState`]; side effects happen in the host, behind the [`Command`]
//! / [`Event`] boundary.
//!
//! ```
//! # use engine::{EngineState, Event, apply};
//! # let graph = ir::Graph::new();
//! let state = EngineState::new(graph);
//! let (state, commands) = apply(state, Event::RunStarted);
//! // Run the commands, then feed each result back as an event.
//! # let _ = (state, commands);
//! ```

mod apply;
mod context;
mod event;
mod log;
mod replay;
mod splice;
mod state;

pub use apply::{FIRING_ENV_CLASS, apply};
pub use event::{
    Admission, AdmitPoint, BoundaryViolation, Command, DEFAULT_MAX_EXECUTIONS, DecisionId,
    DecisionPoint, EngineExit, EngineStart, EntryPoint, Event, GroupDecision, Intervention,
    MiddlewareKey, ResolvedFiring, RouteApplied, RouteDecision, RoutingCandidate, RoutingProposal,
    SpliceClone, SubgraphSplice, UnresolvedConfig, WeightedDraw,
};
pub use log::{
    CANCEL_ESCALATION_KEY, EventLog, EventRecord, EventSource, InvalidRecords, LOG_VERSION,
    UnsupportedLogVersion,
};
pub use replay::{ReplayMismatch, ResumePoint, replay, resume, verify_replay};
pub use splice::{INVALID_SPLICE_CLASS, reject_splices};
pub use state::{
    AdmissionKey, AppliedSplice, BatchPolicy, CancelScope, EngineState, Firing, FiringRecord,
    PendingAdmission, PendingRouting, RunError, SpliceBatchId, SpliceEffect, SpliceOrigin,
};
