use std::sync::Arc;

use executor::{Masker, Secret, SecretError, SecretProvider};
use smol_str::SmolStr;

use crate::{SecretBinding, SecretBindings};

/// A child invocation's name-only view of its parent's provider.
pub struct InvocationSecrets {
    parent:   Arc<dyn SecretProvider>,
    bindings: SecretBindings,
}

impl InvocationSecrets {
    pub fn new(parent: Arc<dyn SecretProvider>, bindings: SecretBindings) -> Self {
        Self { parent, bindings }
    }
}

impl SecretProvider for InvocationSecrets {
    fn resolve(&self, name: &str) -> Result<Secret, SecretError> {
        match &self.bindings {
            SecretBindings::None => Err(SecretError::Unknown(SmolStr::new(name))),
            SecretBindings::Inherit => self.parent.resolve(name),
            SecretBindings::Explicit(bindings) => match bindings.get(name) {
                Some(SecretBinding::Parent(parent)) => self.parent.resolve(parent),
                Some(SecretBinding::Empty) => Ok(Secret::new(SmolStr::new(""))),
                None => Err(SecretError::Unknown(SmolStr::new(name))),
            },
        }
    }

    fn masker(&self) -> Masker {
        self.parent.masker()
    }
}
