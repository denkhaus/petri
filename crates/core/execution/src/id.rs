use std::fmt;
use std::str::FromStr;

use ir::{Attempt, FiringId, ScopeId};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use smol_str::SmolStr;

macro_rules! run_id {
    ($name:ident) => {
        #[derive(
            Clone,
            Copy,
            Debug,
            Default,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            Serialize,
            Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(u64);

        impl $name {
            pub const fn new(raw: u64) -> Self {
                Self(raw)
            }

            pub const fn raw(self) -> u64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "{}", self.0)
            }
        }
    };
}

run_id!(InvocationId);
run_id!(ExecutionId);
run_id!(SandboxLeaseId);

impl InvocationId {
    pub const ROOT: Self = Self(0);

    /// The workspace-prefix fragment this invocation contributes to scope
    /// identities: durable lease records and the driver's workspace names
    /// both build on it, so it has exactly one spelling.
    pub fn workspace_prefix(self) -> String {
        format!("invocation-{self}")
    }
}

impl ExecutionId {
    /// The environment-prefix fragment this execution contributes to scope
    /// identities — the per-execution counterpart of
    /// [`InvocationId::workspace_prefix`].
    pub fn environment_prefix(self) -> String {
        format!("execution-{self}")
    }
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SandboxAllocationKey {
    pub invocation: InvocationId,
    pub scope:      ScopeId,
}

/// SHA-256 of the exact persisted graph bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GraphDigest([u8; 32]);

impl GraphDigest {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        ir::digest_hex(&self.0)
    }
}

impl fmt::Display for GraphDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a graph digest must be 64 lowercase hexadecimal characters")]
pub struct GraphDigestParseError;

impl FromStr for GraphDigest {
    type Err = GraphDigestParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 64 {
            return Err(GraphDigestParseError);
        }
        let mut bytes = [0_u8; 32];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            let high = hex_digit(pair[0]).ok_or(GraphDigestParseError)?;
            let low = hex_digit(pair[1]).ok_or(GraphDigestParseError)?;
            bytes[index] = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
}

const fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

impl Serialize for GraphDigest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for GraphDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(D::Error::custom)
    }
}
