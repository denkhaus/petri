//! Step kinds: the registry the engine resolves [`StepRef::kind`](crate::StepRef::kind)
//! against. The engine never runs a step itself; it names one in a command.

use std::collections::HashMap;

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

/// The set of step kinds a graph may refer to.
#[derive(Default)]
pub struct StepRegistry {
    kinds: HashMap<StepKindId, Box<dyn StepKind>>,
}

impl StepRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, kind: Box<dyn StepKind>) -> StepKindId {
        let id = kind.id();
        self.kinds.insert(id.clone(), kind);
        id
    }

    pub fn get(&self, id: &StepKindId) -> Option<&dyn StepKind> {
        self.kinds.get(id).map(|k| k.as_ref())
    }

    pub fn contains(&self, id: &StepKindId) -> bool {
        self.kinds.contains_key(id)
    }

    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty()
    }

    pub fn len(&self) -> usize {
        self.kinds.len()
    }
}

impl std::fmt::Debug for StepRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut names: Vec<_> = self.kinds.values().map(|k| k.name()).collect();
        names.sort_unstable();
        f.debug_struct("StepRegistry")
            .field("kinds", &names)
            .finish()
    }
}
