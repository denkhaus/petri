//! Invocation trees and reset-free execution successions.

mod client;
mod coordinator;
mod event;
mod id;
mod middleware;
mod observe;
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
pub use id::{
    CallSite, ExecutionId, FiringAddress, GraphDigest, InvocationId, ParentCallKey,
    SandboxAllocationKey, SandboxLeaseId,
};
pub use middleware::{
    AdmitCall, AdmitNext, DecisionAddress, FoldEvent, Middleware, MiddlewareError,
    MiddlewareFoldObserver, MiddlewarePipeline, MiddlewareState, RouteCall, RouteNext,
    initial_middleware_state, validate_middleware_state,
};
pub use observe::{
    AddressedObserver, DecodedEngineLog, EngineLogError, ExecutionObserver, JsonlEngineLog,
    decode_engine_log, read_engine_log,
};
pub use resource::{
    ProviderSandbox, ResourceError, ResourceStore, SandboxAdapter, SandboxResourceRecord,
};
pub use secret::InvocationSecrets;
pub use state::{
    CoordinatorState, ExecutionDeclaration, ExecutionState, InvocationDeclaration, InvocationState,
    StateError,
};
pub use store::{
    COORDINATOR_FILE, CoordinatorStore, DecodedCoordinatorLog, GRAPHS_DIR, INVOCATIONS_DIR,
    RESOURCES_DIR, RUN_FILE, RunMetadata, StoreError, decode_coordinator_log,
};
