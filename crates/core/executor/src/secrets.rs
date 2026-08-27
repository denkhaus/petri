//! Secret resolution and the mask set.
//!
//! Secrets never enter the event log. A `ResolvedFiring` carries
//! `{"$secret": "NAME"}` references, and the value is fetched at spawn time and put
//! straight into the child's environment. Resolving a secret also registers its value
//! for masking, so the two cannot get out of step: anything that was resolved is
//! masked, by construction. A resolved value is a [`Secret`].

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
    #[error("a secret named `{0}` already exists")]
    Duplicate(SmolStr),
    #[error("this secret provider does not support registering secrets at runtime")]
    RegistrationUnsupported,
}

/// A resolved secret value.
///
/// The plaintext leaves only through [`Secret::expose`], so every consumer is an
/// explicit call at the boundary that needs the value — the child's environment, a
/// `Deliver` payload. `Debug` redacts and there is no `Display`, so a resolved value
/// cannot ride into an error message or a log line by accident.
#[derive(Clone)]
pub struct Secret(SmolStr);

impl Secret {
    pub fn new(value: SmolStr) -> Self {
        Self(value)
    }

    /// The plaintext, moved out of the wrapper. Call it where the value is consumed,
    /// never to build a message.
    pub fn expose(self) -> SmolStr {
        self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secret({MASK})")
    }
}

/// Where secret values come from.
pub trait SecretProvider: Send + Sync {
    /// Fetch a secret and register it for masking.
    fn resolve(&self, name: &str) -> Result<Secret, SecretError>;

    /// Register a secret at runtime, so a dynamic value — a human gate's sensitive
    /// answer — can cross as a `{"$secret": ...}` reference and stay out of the
    /// log. Registration feeds the masker, so anything resolvable is maskable by
    /// construction; the lifetime is the provider instance, i.e. the run.
    ///
    /// Duplicate names are rejected, so a runtime registration can never shadow a
    /// configured secret. The default is a typed unsupported error; providers opt
    /// in.
    fn register(&self, name: &str, value: &str) -> Result<(), SecretError> {
        let _ = (name, value);
        Err(SecretError::RegistrationUnsupported)
    }

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

/// A provider backed by a map: the run's secrets are loaded once and handed in,
/// and [`SecretProvider::register`] can add run-scoped values on top.
pub struct MapSecrets {
    secrets: RwLock<BTreeMap<SmolStr, SmolStr>>,
    masker: Masker,
}

impl MapSecrets {
    pub fn new(secrets: BTreeMap<SmolStr, SmolStr>) -> Self {
        Self {
            secrets: RwLock::new(secrets),
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
    fn resolve(&self, name: &str) -> Result<Secret, SecretError> {
        let secrets = self.secrets.read().expect("secret map is not poisoned");
        let value = secrets
            .get(name)
            .ok_or_else(|| SecretError::Unknown(SmolStr::new(name)))?;
        self.masker.register(value);
        Ok(Secret::new(value.clone()))
    }

    fn register(&self, name: &str, value: &str) -> Result<(), SecretError> {
        let mut secrets = self.secrets.write().expect("secret map is not poisoned");
        if secrets.contains_key(name) {
            return Err(SecretError::Duplicate(SmolStr::new(name)));
        }
        secrets.insert(SmolStr::new(name), SmolStr::new(value));
        // Feed the masker at registration, not first resolution: the value is
        // sensitive from the moment the provider holds it.
        self.masker.register(value);
        Ok(())
    }

    fn masker(&self) -> Masker {
        self.masker.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_debug_redacts_and_expose_reveals() {
        let secrets = MapSecrets::from_pairs(&[("TOKEN", "hunter2-hunter2")]);
        let secret = secrets.resolve("TOKEN").expect("configured");
        assert_eq!(format!("{secret:?}"), "Secret(***)");
        assert_eq!(secret.expose(), "hunter2-hunter2");
        // Resolution registered the value, so the masker knows it.
        assert_eq!(secrets.masker().mask("got hunter2-hunter2"), "got ***");
    }
}
