//! Invocation trees and reset-free execution successions.

mod client;
mod coordinator;
mod event;
pub mod host;
mod id;
pub mod interview;
mod jsonl;
mod middleware;
mod observe;
pub mod prune;
mod resource;
mod secret;
mod state;
mod store;

pub use client::{
    CoordinatorInvocationClient, InvocationClient, InvocationHandle, InvocationRequest, InvokeError,
};
pub use coordinator::{
    Coordinator, CoordinatorError, CoordinatorHandle, CoordinatorOptions, DEFAULT_MAX_INVOCATIONS,
};
pub use event::{
    COORDINATOR_FORMAT_VERSION, CoordinatorEvent, CoordinatorRecord, InvocationResult,
    InvocationStatus, SandboxBinding, SandboxMode, SecretBinding, SecretBindings,
};
pub use interview::{
    Delivery, InterviewDispatcher, InterviewError, InterviewReceipt, InterviewRecord,
    InterviewReply, InterviewRequest, Interviewer, RECEIPT_FILE, RECEIPT_VERSION, ReplyRecord,
};
pub use id::{
    CallSite, ExecutionId, GraphDigest, InvocationId, ParentCallKey, SandboxAllocationKey,
    SandboxLeaseId,
};
pub use middleware::{
    AdmitCall, AdmitNext, DecisionAddress, FoldEvent, Middleware, MiddlewareError,
    MiddlewareFoldObserver, MiddlewarePipeline, MiddlewareState, RouteCall, RouteNext,
    initial_middleware_state, validate_middleware_state,
};
pub use observe::{
    AddressedObserver, DecodedEngineLog, EngineLogDecodeError, EngineLogError, ExecutionObserver,
    JsonlEngineLog, decode_engine_log, encode_engine_log, read_engine_log,
};
pub use resource::{
    HOST_PROVIDER, LeaseState, PendingIntent, ResourceError, ResourceLedger, ResourceStore,
    SandboxResourceRecord,
};
pub use secret::InvocationSecrets;
pub use state::{
    CoordinatorState, ExecutionDeclaration, ExecutionState, InvocationDeclaration, InvocationState,
    StateError,
};
pub use store::{
    COORDINATOR_FILE, CoordinatorStore, DecodedCoordinatorLog, GRAPHS_DIR, INVOCATIONS_DIR,
    RESOURCES_DIR, RUN_FILE, RunMetadata, StoreError, decode_coordinator_log, hold_run_lease,
};
