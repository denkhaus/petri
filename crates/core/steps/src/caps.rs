//! Typed, host-registered capabilities for steps.
//!
//! Host services a step may use — a blob store, a comment poster, a queue —
//! reach it through [`Capabilities`]: components define concrete handle types
//! and register values, a step asks by type, and the core never names a
//! capability. The precedent is `EnvHandle::teardown::<T>()`, which does the
//! same downcast for executor teardown records.
//!
//! The key is the **concrete type**: `Arc<dyn Any>` downcasts only to sized
//! types, so a component defines a handle (`struct Blobs(Arc<dyn BlobStore>)`)
//! and registers that. This keeps the map trivial and the trait objects on the
//! component's side of the line.
//!
//! Capabilities live entirely on the effects side, where steps already are:
//! nothing here touches the engine, the log, or determinism.

use std::any::{self, Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

use ir::FailureClass;

use crate::ctx::StepFailure;

/// The failure class when a step requires a capability its host never
/// registered. Routable, like `secret_unavailable`: a step missing its host
/// service fails its node, never the run machinery.
pub const CAPABILITY_UNAVAILABLE_CLASS: FailureClass =
    FailureClass::new_static("capability_unavailable");

/// Host services a step may use. Opaque to the core; keyed by type.
#[derive(Clone, Default)]
pub struct Capabilities {
    map: Arc<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
}

impl Capabilities {
    pub fn builder() -> CapabilitiesBuilder {
        CapabilitiesBuilder::default()
    }

    /// The registered value of this type, if any.
    pub fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.map
            .get(&TypeId::of::<T>())
            .cloned()
            .and_then(|value| value.downcast::<T>().ok())
    }

    /// Return this set plus one execution-local capability.
    ///
    /// # Panics
    ///
    /// On a duplicate type, as with [`CapabilitiesBuilder::provide`].
    #[must_use]
    pub fn with<T: Send + Sync + 'static>(&self, value: T) -> Self {
        let mut map = (*self.map).clone();
        let replaced = map.insert(TypeId::of::<T>(), Arc::new(value));
        assert!(
            replaced.is_none(),
            "a capability of type `{}` is already registered",
            any::type_name::<T>()
        );
        Self { map: Arc::new(map) }
    }

    /// The registered value, or the routable failure a step returns when its
    /// host service is missing. The message names the type.
    pub fn require<T: Send + Sync + 'static>(&self) -> Result<Arc<T>, StepFailure> {
        self.get::<T>().ok_or_else(|| StepFailure {
            class:   CAPABILITY_UNAVAILABLE_CLASS,
            message: format!(
                "no capability of type `{}` is registered for this run",
                any::type_name::<T>()
            ),
        })
    }
}

#[derive(Clone, Default)]
pub struct CapabilitiesBuilder {
    map: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl CapabilitiesBuilder {
    /// Register a value under its concrete type.
    ///
    /// # Panics
    ///
    /// On a duplicate type — registration is configuration, the same rule as
    /// `Registry::register`.
    #[must_use]
    pub fn provide<T: Send + Sync + 'static>(mut self, value: T) -> Self {
        let replaced = self.map.insert(TypeId::of::<T>(), Arc::new(value));
        assert!(
            replaced.is_none(),
            "a capability of type `{}` is already registered",
            any::type_name::<T>()
        );
        self
    }

    pub fn build(self) -> Capabilities {
        Capabilities {
            map: Arc::new(self.map),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Blobs(&'static str);
    #[derive(Debug)]
    struct Queue(u32);

    /// A `dyn`-trait service rides behind a concrete newtype — the handle-type
    /// pattern the map is designed around.
    trait Store: Send + Sync {
        fn name(&self) -> &'static str;
    }
    struct MemStore;
    impl Store for MemStore {
        fn name(&self) -> &'static str {
            "mem"
        }
    }
    struct StoreHandle(Arc<dyn Store>);

    #[test]
    fn get_finds_by_type_and_absence_is_none() {
        let caps = Capabilities::builder()
            .provide(Blobs("b"))
            .provide(Queue(7))
            .build();
        assert_eq!(caps.get::<Blobs>().expect("registered").0, "b");
        assert_eq!(caps.get::<Queue>().expect("registered").0, 7);
        assert!(caps.get::<StoreHandle>().is_none());
    }

    #[test]
    fn require_fails_routably_naming_the_type() {
        let caps = Capabilities::default();
        let failure = caps.require::<Queue>().expect_err("absent");
        assert_eq!(failure.class, CAPABILITY_UNAVAILABLE_CLASS);
        assert!(failure.message.contains("Queue"), "{}", failure.message);
    }

    #[test]
    fn a_dyn_service_rides_a_concrete_handle() {
        let caps = Capabilities::builder()
            .provide(StoreHandle(Arc::new(MemStore)))
            .build();
        assert_eq!(
            caps.get::<StoreHandle>().expect("registered").0.name(),
            "mem"
        );
    }

    #[test]
    #[should_panic(expected = "already registered")]
    fn duplicate_registration_panics() {
        let _ = Capabilities::builder().provide(Queue(1)).provide(Queue(2));
    }
}
