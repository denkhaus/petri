//! Newtype identifiers used across the IR and the engine.
//!
//! Graph-structure ids (`NodeId`, `EdgeId`, `ScopeId`, `ExprId`) carry an
//! id-space marker: [`Live`] for the run's live graph — the default everywhere,
//! so ordinary call sites never name it — and [`Local`] for ids inside a
//! [`GraphFragment`](crate::GraphFragment), which are meaningless against the
//! live graph until the splice remapper converts them. The marker is how the
//! type system refuses a mixed-space id without a parallel family of mirror
//! types. Runtime ids (`FiringId`, `Generation`, `Attempt`, `CancelScopeId`)
//! exist only at run time, so they have no space.

use std::fmt;
use std::marker::PhantomData;

use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

/// The live run graph's id space. The default for every spaced id, so existing
/// call sites read and write `NodeId` and mean this.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Live;

/// A fragment's local id space: ids in a
/// [`GraphFragment`](crate::GraphFragment) index the fragment's own tables and
/// mean nothing against the live graph. Only the splice remapper converts them,
/// by allocating fresh [`Live`] ids.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Local;

macro_rules! impl_id_api {
    ($name:ident [$($impl_generics:tt)*] [$($type_generics:tt)*], $repr:ty, [$($extra:expr),*]) => {
        impl $($impl_generics)* $name $($type_generics)* {
            pub const fn new(raw: $repr) -> Self {
                Self(raw $(, $extra)*)
            }

            pub const fn raw(self) -> $repr {
                self.0
            }

            #[allow(
                clippy::cast_possible_truncation,
                reason = "every id that indexes a table is `u32` and widens cleanly; the \
                          one `u64` id is the firing counter, which identifies a firing \
                          rather than addressing a table"
            )]
            pub const fn index(self) -> usize {
                self.0 as usize
            }
        }

        impl $($impl_generics)* From<$repr> for $name $($type_generics)* {
            fn from(raw: $repr) -> Self {
                Self::new(raw)
            }
        }

        impl $($impl_generics)* std::fmt::Debug for $name $($type_generics)* {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0)
            }
        }

        impl $($impl_generics)* std::fmt::Display for $name $($type_generics)* {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

/// An id reads from a JSON number, or from the string a JSON object key
/// spells one as. A record that holds a map keyed by ids (`prior_firings`)
/// is read through serde's buffered content when the record is an internally
/// tagged enum, and the buffer keeps every object key as a string; without
/// the string form those maps would not read back.
macro_rules! impl_id_deserialize {
    ($name:ident [$($impl_generics:tt)*] [$($type_generics:tt)*], $repr:ty, [$($extra:expr),*]) => {
        impl $($impl_generics)* Deserialize<'de> for $name $($type_generics)* {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct IdVisitor;

                impl serde::de::Visitor<'_> for IdVisitor {
                    type Value = $repr;

                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        write!(f, concat!("a ", stringify!($name), " as a number"))
                    }

                    fn visit_u64<E: serde::de::Error>(self, raw: u64) -> Result<$repr, E> {
                        <$repr>::try_from(raw).map_err(|_| E::custom(format!(
                            "{raw} is out of range for a {}",
                            stringify!($name)
                        )))
                    }

                    fn visit_i64<E: serde::de::Error>(self, raw: i64) -> Result<$repr, E> {
                        u64::try_from(raw)
                            .ok()
                            .and_then(|raw| <$repr>::try_from(raw).ok())
                            .ok_or_else(|| E::custom(format!(
                                "{raw} is out of range for a {}",
                                stringify!($name)
                            )))
                    }

                    fn visit_str<E: serde::de::Error>(self, raw: &str) -> Result<$repr, E> {
                        raw.parse::<$repr>().map_err(|_| E::custom(format!(
                            "`{raw}` is not a {}",
                            stringify!($name)
                        )))
                    }
                }

                deserializer.deserialize_any(IdVisitor).map(|raw| Self(raw $(, $extra)*))
            }
        }
    };
}

macro_rules! id_newtype {
    ($(#[$meta:meta])* $name:ident<S = $default:ty>, $repr:ty) => {
        $(#[$meta])*
        #[derive(Serialize)]
        #[serde(transparent)]
        pub struct $name<S = $default>(pub $repr, PhantomData<S>);

        impl_id_api!($name [<S>] [<S>], $repr, [PhantomData]);
        impl_id_deserialize!($name [<'de, S>] [<S>], $repr, [PhantomData]);

        // Manual impls rather than derives: a derive would demand the same trait of
        // the space marker, and the marker is phantom — the id is a `$repr` whatever
        // the space says.
        impl<S> Clone for $name<S> {
            fn clone(&self) -> Self {
                *self
            }
        }

        impl<S> Copy for $name<S> {}

        impl<S> PartialEq for $name<S> {
            fn eq(&self, other: &Self) -> bool {
                self.0 == other.0
            }
        }

        impl<S> Eq for $name<S> {}

        impl<S> PartialOrd for $name<S> {
            fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
                Some(self.cmp(other))
            }
        }

        impl<S> Ord for $name<S> {
            fn cmp(&self, other: &Self) -> std::cmp::Ordering {
                self.0.cmp(&other.0)
            }
        }

        impl<S> std::hash::Hash for $name<S> {
            fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
                self.0.hash(state);
            }
        }
    };
    ($(#[$meta:meta])* $name:ident, $repr:ty) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(pub $repr);

        impl_id_api!($name [] [], $repr, []);
        impl_id_deserialize!($name [<'de>] [], $repr, []);
    };
}

id_newtype!(
    /// Index of a node in [`Graph::nodes`](crate::Graph::nodes).
    NodeId<S = Live>, u32
);
id_newtype!(
    /// Unique id of an outgoing edge. Joins count distinct incoming edge ids.
    EdgeId<S = Live>, u32
);
id_newtype!(
    /// Index of a resource scope in [`Graph::scopes`](crate::Graph::scopes).
    ScopeId<S = Live>, u32
);
id_newtype!(
    /// Index into the graph's [`ExprTable`](crate::ExprTable).
    ExprId<S = Live>, u32
);
id_newtype!(
    /// Unique per run: one id per node execution attempt.
    FiringId, u64
);
id_newtype!(
    /// Loop-iteration counter carried by tokens; bumped by `back` edges.
    Generation, u32
);
/// The name of a step kind: `process`, `noop`, or a namespaced `vendor/kind`
/// from another repository.
///
/// A name, not a number, so a serialized graph says what each node runs and two
/// repositories never have to agree on a `u32`. Built-in kinds use bare names;
/// kinds defined elsewhere use `<vendor>/<kind>`, and a registry rejects a
/// duplicate. Names are what a step registry is keyed by.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StepKindId(SmolStr);

impl StepKindId {
    pub fn new(name: &str) -> Self {
        Self(SmolStr::new(name))
    }

    /// For constants: `const PROCESS: StepKindId =
    /// StepKindId::new_static("process")`.
    pub const fn new_static(name: &'static str) -> Self {
        Self(SmolStr::new_static(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for StepKindId {
    fn from(name: &str) -> Self {
        Self::new(name)
    }
}

impl From<String> for StepKindId {
    fn from(name: String) -> Self {
        Self(SmolStr::from(name))
    }
}

impl AsRef<str> for StepKindId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for StepKindId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StepKindId({:?})", self.as_str())
    }
}

impl fmt::Display for StepKindId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
id_newtype!(
    /// Which try this is, 1-based. A retry is **not** a loop iteration: it never
    /// touches [`Generation`]. The full identity of an execution attempt is
    /// `(NodeId, Generation, Attempt)`.
    Attempt, u32
);
id_newtype!(
    /// A dynamic set of firings that can be cancelled as a unit.
    CancelScopeId, u32
);

impl Generation {
    pub const ZERO: Self = Self(0);

    /// Generation carried across a `back` edge.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl Attempt {
    /// Every firing starts here. Counters never carry across firings, so a
    /// later generation retries from scratch.
    pub const FIRST: Self = Self(1);

    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl<S> EdgeId<S> {
    /// Reserved: no edge declared in a graph may use this id. The engine
    /// allocates seed edges for entry nodes and expansion clones from the
    /// free id space, and keeps this one out of play as an unambiguous "not
    /// an edge" sentinel.
    pub const SEED: Self = Self::new(u32::MAX);
}

impl CancelScopeId {
    /// The run's own cancel scope. Cancelling it cancels everything.
    pub const ROOT: Self = Self(0);
}
