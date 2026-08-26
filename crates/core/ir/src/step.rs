//! Step kinds, as validation sees them.
//!
//! The engine never runs a step itself; it names one in a command. What runs steps
//! lives in the `steps` crate; this is the load-time face — enough for
//! [`validate_with`](crate::validate_with) to check that every
//! [`StepRef::kind`](crate::StepRef::kind) is known and every literal config is
//! well-formed before a run starts.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use smol_str::SmolStr;

use crate::ids::StepKindId;

/// A content digest of a step's inputs.
///
/// Reserved seam: content caching (v2) keys on this. v1 never computes one.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Digest(pub SmolStr);

impl Digest {
    pub fn new(s: &str) -> Self {
        Self(SmolStr::new(s))
    }
}

/// What a kind of step is, independent of any one node using it.
pub trait StepKind: Send + Sync {
    fn id(&self) -> StepKindId;

    fn name(&self) -> &str;

    /// Validate a node's `config` at load time.
    fn validate_config(&self, _config: &Value) -> Result<(), String> {
        Ok(())
    }

    /// Reserved seam for content caching: `None` means "never reuse a result".
    fn fingerprint(&self, _config: &Value) -> Option<Digest> {
        None
    }
}

/// A set of step kinds, looked up by id.
///
/// The one registry lives in the `steps` crate and implements this; validation only
/// needs the lookup, so this crate carries the trait and no store.
pub trait StepKinds {
    fn get(&self, id: &StepKindId) -> Option<&dyn StepKind>;
}
