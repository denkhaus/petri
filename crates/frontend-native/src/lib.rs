//! The native workflow format: a faithful YAML surface over the engine's own model.
//!
//! Where the GHA frontend exercises the degenerate subset, this is where the whole
//! engine is reachable — cycles, `Any` and `Quorum` joins, multi-arm selection,
//! `for_each` both ways. Two rules hold it honest to the spec:
//!
//! - **Fan-out is explicit.** `next:` produces one select group; `parallel:` produces
//!   several. There is no shape of document that fans out by accident.
//! - **Normalize, don't relax.** `Quorum{1}` on a loop head is rewritten to `Any`
//!   in lowering; an `All` loop head is reported with the corollary as its hint.
//!
//! See `FORMAT.md` for the reference, with one worked example per construct.

mod duration;
mod lower;
mod model;

pub use lower::lower;
pub use model::KNOWN_BINDINGS;

use frontend::{Diagnostics, Lowered};

/// Parse and lower a native-format document. Pure: text in, graph and diagnostics
/// out.
pub fn load(file: &str, text: &str) -> Lowered {
    let mut diags = Diagnostics::new();
    let Some(doc) = frontend::yaml::Document::parse(file, text, &mut diags) else {
        return Lowered::rejected(diags);
    };
    lower(&doc, diags)
}
