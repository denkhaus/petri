/// The lease type is the executor interface's: the executor keys sandboxes by
/// it, and the coordinator allocates it.
pub use executor::SandboxLeaseId;
use ir::{Attempt, FiringId};
/// The run-level ids are the IR's, so a store can name an execution's log
/// without depending on this crate.
pub use ir::{ExecutionId, InvocationId};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
/// SHA-256 of the exact persisted graph bytes: a blob digest in the store.
pub use store::Digest as GraphDigest;
use store::RunKey;

/// The execution a step runs in, as an execution-local capability. The
/// coordinator registers one on every driver it builds, so a step that
/// hands work to something outside the run (a host tool an agent calls)
/// can name the run, invocation and execution the effect belongs to. A
/// bare driver built outside the coordinator has none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionIdentity {
    pub run:        RunKey,
    pub invocation: InvocationId,
    pub execution:  ExecutionId,
}

/// The durable idempotency key for a nested call.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ParentCallKey {
    pub parent:  ExecutionId,
    pub firing:  FiringId,
    pub attempt: Attempt,
    pub slot:    SmolStr,
}

/// What a step supplies at its own stable call site.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallSite {
    pub firing:  FiringId,
    pub attempt: Attempt,
    pub slot:    SmolStr,
}

/// Stable reconciliation key for one invocation-owned graph scope.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SandboxAllocationKey {
    pub invocation: InvocationId,
    pub scope:      engine::ScopeIdentity,
}
