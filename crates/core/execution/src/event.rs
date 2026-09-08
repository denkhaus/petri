use std::collections::BTreeMap;

use engine::{EngineExit, EngineStart, MiddlewareKey};
use ir::{FailureInfo, RunStatus, ScopeId, Value};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

use crate::{ExecutionId, GraphDigest, InvocationId, ParentCallKey, SandboxLeaseId};

/// Version 2 records stable dynamic scope identities and their runtime and
/// execution provenance in the resource ledger.
pub const COORDINATOR_FORMAT_VERSION: u32 = 2;

/// Name-only child secret bindings. Plaintext is not representable here.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SecretBindings {
    #[default]
    None,
    Inherit,
    Explicit(BTreeMap<SmolStr, SecretBinding>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SecretBinding {
    Parent(SmolStr),
    Empty,
}

/// What a caller requests before declaration resolves the binding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxMode {
    /// Share the caller's sandbox. The caller names its own scope — the step
    /// knows where it runs — so the coordinator resolves the binding without
    /// reading the parent's engine log.
    Inherit { scope: ScopeId },
    #[default]
    Isolated,
}

/// The durable sandbox binding on an invocation declaration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxBinding {
    Inherited {
        lease: SandboxLeaseId,
    },
    #[default]
    Isolated,
}

/// Bounded concurrency for a fork's child invocations: the invocations one
/// parent execution declares under the gate `gate` share `max_parallel`
/// slots. A child's engine starts only on a free slot, in declaration order,
/// keeps the slot while its attempts run and between them, and releases it
/// when the engine ends or when a retry backoff begins. So at most
/// `max_parallel` children are live at once, plus any waiting out a backoff.
/// The gate is shared by every invocation the same parent execution declares
/// under the same name, so a fork's branches share one limit while a repeated
/// fork visit or a nested fork gets its own.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptAdmission {
    pub gate:         SmolStr,
    pub max_parallel: u32,
}

/// Why a cancel was requested, when the requester said so.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CancelReason {
    /// The stall watchdog: no execution activity for the run's budget.
    StallTimeout {
        stall_timeout_ms: u64,
        idle_ms:          u64,
    },
    /// An interrupt from the terminal (Ctrl-C).
    Interrupt,
    /// A run control: the control service, a control file, an embedding
    /// host.
    Control,
}

/// One cancel request on the coordinator's channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CancelRequest {
    pub invocation: InvocationId,
    pub reason:     Option<CancelReason>,
}

/// The one durable result returned by an invocation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InvocationResult {
    pub status:          RunStatus,
    pub failure:         Option<FailureInfo>,
    pub final_execution: ExecutionId,
    pub output:          Value,
    pub context:         BTreeMap<SmolStr, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum InvocationStatus {
    Declared,
    Running { execution: ExecutionId },
    Finished(InvocationResult),
}

/// Relationships and lifecycle facts that span engine logs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CoordinatorEvent {
    RunStarted {
        format_version:   u32,
        root:             InvocationId,
        middleware_chain: Vec<MiddlewareKey>,
    },
    GraphRegistered {
        digest: GraphDigest,
    },
    InvocationDeclared {
        invocation:      InvocationId,
        call:            Option<ParentCallKey>,
        graph:           GraphDigest,
        context:         BTreeMap<SmolStr, Value>,
        secret_bindings: SecretBindings,
        sandbox:         SandboxBinding,
        /// Bounded attempt concurrency, when the caller asked for it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        admission:       Option<AttemptAdmission>,
    },
    ExecutionDeclared {
        execution:        ExecutionId,
        invocation:       InvocationId,
        predecessor:      Option<ExecutionId>,
        start:            EngineStart,
        middleware_state: BTreeMap<MiddlewareKey, (u32, Value)>,
    },
    ExecutionFinished {
        execution: ExecutionId,
        exit:      EngineExit,
    },
    InvocationFinished {
        invocation: InvocationId,
        result:     InvocationResult,
    },
    InvocationCancelRequested {
        invocation: InvocationId,
        /// Why, when the requester said. Absent for a plain cancel.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason:     Option<CancelReason>,
    },
    /// A run control held every attempt not yet admitted. Additive since
    /// format version 2: a log without it replays as before, and a resume
    /// starts paused when this is the last control recorded.
    RunPaused,
    /// A run control released held and future attempts. Additive since
    /// format version 2.
    RunUnpaused,
    RunFinished {
        status: RunStatus,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CoordinatorRecord {
    pub seq:   u64,
    pub event: CoordinatorEvent,
}
