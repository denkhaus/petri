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
    },
    RunFinished {
        status: RunStatus,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CoordinatorRecord {
    pub seq:   u64,
    pub event: CoordinatorEvent,
}
