//! The resolved hook definitions a run carries in
//! `Graph.params["attractor.hooks"]` and on its stage steps, and the events
//! they fire on.
//!
//! Every hook names one transport: a command (`sh -c` on the host or
//! `bash -c` in the sandbox), an HTTP POST, a one-turn model call, or an
//! agent with the coding tools. Where the definitions come from is the
//! settings layer's business: the Fabro frontend reads `[[run.hooks]]` from
//! its settings files and merges the layers into the list on
//! [`RunSettings::hooks`](crate::RunSettings). The steps read only the
//! resolved shape here.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Where the resolved list lives in `Graph.params`.
pub const PARAM: &str = "attractor.hooks";

/// The default timeout for command, HTTP and agent hooks.
pub const DEFAULT_TIMEOUT_MS: u64 = 60_000;
/// The default timeout for prompt hooks.
pub const PROMPT_TIMEOUT_MS: u64 = 30_000;
/// The default model for prompt and agent hooks, Fabro's `haiku` alias.
pub const DEFAULT_MODEL: &str = "haiku";
/// The default tool-round limit for agent hooks.
pub const DEFAULT_MAX_TOOL_ROUNDS: u32 = 50;

/// The sixteen events Fabro's configuration accepts. Petri dispatches
/// thirteen; `checkpoint_saved` is a Fabro platform event that warns and
/// never runs, and `run_failed` and `sandbox_cleanup` wait for an awaited
/// run-end and scope-release point in the engine (a hook on them warns).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    RunStart,
    RunComplete,
    RunFailed,
    StageStart,
    StageComplete,
    StageFailed,
    StageRetrying,
    EdgeSelected,
    ParallelStart,
    ParallelComplete,
    SandboxReady,
    SandboxCleanup,
    CheckpointSaved,
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
}

impl HookEvent {
    pub const ALL: &'static [Self] = &[
        Self::RunStart,
        Self::RunComplete,
        Self::RunFailed,
        Self::StageStart,
        Self::StageComplete,
        Self::StageFailed,
        Self::StageRetrying,
        Self::EdgeSelected,
        Self::ParallelStart,
        Self::ParallelComplete,
        Self::SandboxReady,
        Self::SandboxCleanup,
        Self::CheckpointSaved,
        Self::PreToolUse,
        Self::PostToolUse,
        Self::PostToolUseFailure,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RunStart => "run_start",
            Self::RunComplete => "run_complete",
            Self::RunFailed => "run_failed",
            Self::StageStart => "stage_start",
            Self::StageComplete => "stage_complete",
            Self::StageFailed => "stage_failed",
            Self::StageRetrying => "stage_retrying",
            Self::EdgeSelected => "edge_selected",
            Self::ParallelStart => "parallel_start",
            Self::ParallelComplete => "parallel_complete",
            Self::SandboxReady => "sandbox_ready",
            Self::SandboxCleanup => "sandbox_cleanup",
            Self::CheckpointSaved => "checkpoint_saved",
            Self::PreToolUse => "pre_tool_use",
            Self::PostToolUse => "post_tool_use",
            Self::PostToolUseFailure => "post_tool_use_failure",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|e| e.as_str() == text)
    }

    /// Fabro's decision points: the events whose hooks block by default.
    pub const fn blocking_by_default(self) -> bool {
        matches!(
            self,
            Self::RunStart
                | Self::StageStart
                | Self::EdgeSelected
                | Self::PreToolUse
                | Self::SandboxReady
        )
    }

    /// The three events that fire at an agent's tool boundary.
    pub const fn is_tool_event(self) -> bool {
        matches!(
            self,
            Self::PreToolUse | Self::PostToolUse | Self::PostToolUseFailure
        )
    }
}

impl fmt::Display for HookEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Fabro's TLS modes for HTTP hooks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    #[default]
    Verify,
    NoVerify,
    Off,
}

/// What a hook does when it fires.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HookKind {
    /// A shell command, `sh -c` on the host or `bash -c` in the sandbox.
    Command { command: String },
    /// A POST of the event context to `url`.
    Http {
        url:     String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
        #[serde(default)]
        tls:     TlsMode,
    },
    /// One model turn that answers `{"ok": bool, "reason": ...}`.
    Prompt {
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model:  Option<String>,
    },
    /// An agent with the coding tools that answers the same JSON.
    Agent {
        prompt:          String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model:           Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_tool_rounds: Option<u32>,
    },
}

impl HookKind {
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Command { .. } => "command",
            Self::Http { .. } => "http",
            Self::Prompt { .. } => "prompt",
            Self::Agent { .. } => "agent",
        }
    }
}

/// One resolved hook, as the run carries it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookDefinition {
    /// The display name: `name`, else `id`, else generated from the event and
    /// the transport as Fabro generates it.
    pub name:       String,
    /// The merge identity across layers, when the entry gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id:         Option<String>,
    pub event:      HookEvent,
    #[serde(flatten)]
    pub kind:       HookKind,
    /// A regex tested against the event's node id, handler type, edge ends
    /// and tool name; absent, the hook fires on every occurrence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher:    Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocking:   Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Command hooks only: run in the sandbox (the default) or on the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox:    Option<bool>,
    /// Which layer the entry came from, for diagnostics and receipts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source:     Option<String>,
}

impl HookDefinition {
    pub fn is_blocking(&self) -> bool {
        self.blocking
            .unwrap_or_else(|| self.event.blocking_by_default())
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.unwrap_or(match self.kind {
            HookKind::Prompt { .. } => PROMPT_TIMEOUT_MS,
            _ => DEFAULT_TIMEOUT_MS,
        }))
    }

    /// Command hooks run in the sandbox unless told otherwise. Every other
    /// kind runs from the host.
    pub fn runs_in_sandbox(&self) -> bool {
        matches!(self.kind, HookKind::Command { .. }) && self.sandbox.unwrap_or(true)
    }
}
