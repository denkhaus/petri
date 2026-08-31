//! The native workflow format: a faithful YAML surface over the engine's own
//! model.
//!
//! Where the GHA frontend exercises the degenerate subset, this is where the
//! whole engine is reachable — cycles, `Any` and `Quorum` joins, multi-arm
//! selection, `for_each` both ways. Two rules hold it honest to the spec:
//!
//! - **Fan-out is explicit.** `next:` produces one select group; `parallel:`
//!   produces several. There is no shape of document that fans out by accident.
//! - **Normalize, don't relax.** `Quorum{1}` on a loop head is rewritten to
//!   `Any` in lowering; an `All` loop head is reported with the corollary as
//!   its hint.
//!
//! See `FORMAT.md` for the reference, with one worked example per construct.

mod duration;
mod lower;
mod model;

use std::path::Path;

use frontend::yaml::Document;
use frontend::{Diagnostics, FileSource, Frontend, Lowered};
pub use lower::lower;

/// Parse and lower a native-format document. Pure: text in, graph and
/// diagnostics out.
pub fn load(file: &str, text: &str) -> Lowered {
    let mut diags = Diagnostics::new();
    let Some(doc) = Document::parse(file, text, &mut diags) else {
        return Lowered::rejected(diags);
    };
    lower(&doc, diags)
}

/// The native format, as a [`Frontend`]. It claims every path, so it goes last
/// in any list of frontends and acts as the default.
pub struct Native;

impl Frontend for Native {
    #[expect(
        clippy::unnecessary_literal_bound,
        reason = "the trait fixes this signature; an impl cannot widen the returned lifetime"
    )]
    fn name(&self) -> &str {
        "native"
    }

    fn claims(&self, _path: &Path) -> bool {
        true
    }

    fn load(&self, file: &str, text: &str, _files: &dyn FileSource) -> Lowered {
        load(file, text)
    }
}
