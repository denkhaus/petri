//! Secret resolution and the mask set.
//!
//! Secrets never enter the event log. A `ResolvedFiring` carries
//! `{"$secret": "NAME"}` references, and the value is fetched at spawn time and put
//! straight into the child's environment. Resolving a secret also registers its value
//! for masking, so the two cannot get out of step: anything that was resolved is
//! masked, by construction.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use smol_str::SmolStr;

/// The key marking a secret reference in a step config.
///
/// Re-exported from `ir` so there is one definition: the constructor that permits the
/// form and the step kind that resolves it must agree.
pub use ir::placeholder::SECRET_REF_KEY;

/// Values shorter than this are not masked: masking `1` or `true` would
/// turn every log into asterisks.
pub const MIN_MASK_LENGTH: usize = 6;

pub const MASK: &str = "***";

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SecretError {
    #[error("no secret named `{0}`")]
    Unknown(SmolStr),
}

/// Where secret values come from.
pub trait SecretProvider: Send + Sync {
    /// Fetch a secret and register it for masking.
    fn resolve(&self, name: &str) -> Result<SmolStr, SecretError>;

    /// The mask set, shared with the log sink.
    fn masker(&self) -> Masker;
}

/// The set of resolved secret values, and the masking itself.
///
/// Shared by handle: the provider adds to it as secrets are resolved, and the log
/// sink reads it on every line.
#[derive(Clone, Debug, Default)]
pub struct Masker {
    values: Arc<RwLock<Vec<String>>>,
}

impl Masker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a value. Short values are ignored, and multi-line secrets are
    /// registered line by line as well as whole, so a secret that spans lines is
    /// still masked in line-buffered output.
    pub fn register(&self, value: &str) {
        let mut values = self.values.write().expect("mask set is not poisoned");
        let mut add = |candidate: &str| {
            if candidate.len() >= MIN_MASK_LENGTH && !values.iter().any(|v| v == candidate) {
                values.push(candidate.to_string());
            }
        };
        add(value);
        if value.contains('\n') {
            for line in value.lines() {
                add(line);
            }
        }
        // Longest first, so a secret containing another is masked whole.
        values.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    }

    /// Replace every registered value with `***`.
    pub fn mask(&self, text: &str) -> String {
        let values = self.values.read().expect("mask set is not poisoned");
        let mut out = text.to_string();
        for value in values.iter() {
            if out.contains(value.as_str()) {
                out = out.replace(value.as_str(), MASK);
            }
        }
        out
    }

    /// Mask every string inside a JSON value, however deeply nested.
    pub fn mask_value(&self, value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::String(s) => serde_json::Value::String(self.mask(s)),
            serde_json::Value::Array(items) => {
                serde_json::Value::Array(items.iter().map(|i| self.mask_value(i)).collect())
            }
            serde_json::Value::Object(map) => serde_json::Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), self.mask_value(v)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    pub fn len(&self) -> usize {
        self.values.read().expect("mask set is not poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A provider backed by a fixed map. The run's secrets are loaded once and handed in.
pub struct MapSecrets {
    secrets: BTreeMap<SmolStr, SmolStr>,
    masker: Masker,
}

impl MapSecrets {
    pub fn new(secrets: BTreeMap<SmolStr, SmolStr>) -> Self {
        Self {
            secrets,
            masker: Masker::new(),
        }
    }

    pub fn from_pairs(pairs: &[(&str, &str)]) -> Self {
        Self::new(
            pairs
                .iter()
                .map(|(k, v)| (SmolStr::new(*k), SmolStr::new(*v)))
                .collect(),
        )
    }

    pub fn empty() -> Self {
        Self::new(BTreeMap::new())
    }
}

impl SecretProvider for MapSecrets {
    fn resolve(&self, name: &str) -> Result<SmolStr, SecretError> {
        let value = self
            .secrets
            .get(name)
            .ok_or_else(|| SecretError::Unknown(SmolStr::new(name)))?;
        self.masker.register(value);
        Ok(value.clone())
    }

    fn masker(&self) -> Masker {
        self.masker.clone()
    }
}
