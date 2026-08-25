//! A pure, sans-IO token-flow state machine.
//!
//! The engine executes an [`ir::Graph`] with token-flow semantics: a node fires when
//! its join policy is satisfied by incoming tokens, and on completion its routing
//! policy emits tokens on outgoing edges. All coordination state lives in
//! [`EngineState`]; side effects happen in the host, behind the [`Command`] /
//! [`Event`] boundary.
//!
//! ```no_run
//! # use engine::{EngineState, Event, apply};
//! # let graph = ir::Graph::new();
//! let state = EngineState::new(graph);
//! let (state, commands) = apply(state, Event::RunStarted);
//! // Run the commands, then feed each result back as an event.
//! # let _ = (state, commands);
//! ```

pub mod apply;
pub mod context;
pub mod event;
pub mod log;
pub mod replay;
pub mod state;

pub use apply::apply;
pub use event::{
    BoundaryViolation, Command, Event, ResolvedFiring, SpliceClone, SubgraphSplice,
    UnresolvedConfig,
};
pub use log::{EventLog, EventRecord, EventSource, LOG_VERSION, UnsupportedLogVersion};
pub use replay::{ReplayMismatch, replay, verify_replay};
pub use state::{CancelScope, EngineState, Firing, FiringRecord, RunError, Splice};
