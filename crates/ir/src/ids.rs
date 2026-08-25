//! Newtype identifiers used across the IR and the engine.

use serde::{Deserialize, Serialize};

macro_rules! id_newtype {
    ($(#[$meta:meta])* $name:ident, $repr:ty) => {
        $(#[$meta])*
        #[derive(
            Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub $repr);

        impl $name {
            pub const fn new(raw: $repr) -> Self {
                Self(raw)
            }

            pub const fn raw(self) -> $repr {
                self.0
            }

            pub const fn index(self) -> usize {
                self.0 as usize
            }
        }

        impl From<$repr> for $name {
            fn from(raw: $repr) -> Self {
                Self(raw)
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

id_newtype!(
    /// Index of a node in [`Graph::nodes`](crate::Graph::nodes).
    NodeId, u32
);
id_newtype!(
    /// Unique id of an outgoing edge. Joins count distinct incoming edge ids.
    EdgeId, u32
);
id_newtype!(
    /// Index of a resource scope in [`Graph::scopes`](crate::Graph::scopes).
    ScopeId, u32
);
id_newtype!(
    /// Index into the graph's [`ExprTable`](crate::ExprTable).
    ExprId, u32
);
id_newtype!(
    /// Unique per run: one id per node execution attempt.
    FiringId, u64
);
id_newtype!(
    /// Loop-iteration counter carried by tokens; bumped by `back` edges.
    Generation, u32
);
id_newtype!(
    /// Key into a [`StepRegistry`](crate::StepRegistry).
    StepKindId, u32
);
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
    pub const ZERO: Generation = Generation(0);

    /// Generation carried across a `back` edge.
    pub const fn next(self) -> Generation {
        Generation(self.0 + 1)
    }
}

impl Attempt {
    /// Every firing starts here. Counters never carry across firings, so a later
    /// generation retries from scratch.
    pub const FIRST: Attempt = Attempt(1);

    pub const fn next(self) -> Attempt {
        Attempt(self.0 + 1)
    }
}

impl EdgeId {
    /// Reserved: no edge declared in a graph may use this id. The engine allocates
    /// seed edges for entry nodes and expansion clones from the free id space, and
    /// keeps this one out of play as an unambiguous "not an edge" sentinel.
    pub const SEED: EdgeId = EdgeId(u32::MAX);
}

impl CancelScopeId {
    /// The run's own cancel scope. Cancelling it cancels everything.
    pub const ROOT: CancelScopeId = CancelScopeId(0);
}
