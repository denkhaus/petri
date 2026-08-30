//! Lowering helpers: text in, graph or diagnostics out. Nothing here runs a
//! graph — the run harness lives with the acceptance battery.

#![allow(
    dead_code,
    reason = "every test binary compiles this module whole, and no one of them uses every helper"
)]

use std::collections::BTreeMap;

use frontend::{Diagnostic, FileSource, MapFiles, NoFiles};
use frontend_gha::load;
use ir::Graph;

pub(crate) fn files(pairs: &[(&str, &str)]) -> MapFiles {
    MapFiles(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<BTreeMap<_, _>>(),
    )
}

pub(crate) fn lower_ok(text: &str) -> Graph {
    lower_ok_with(text, &NoFiles)
}

#[expect(
    clippy::print_stderr,
    reason = "the helper echoes the lowering's diagnostics so a graph that failed to lower \
              explains itself in the test output"
)]
pub(crate) fn lower_ok_with(text: &str, files: &dyn FileSource) -> Graph {
    let lowered = load(".github/workflows/test.yml", text, files);
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    lowered.graph.expect("expected a graph")
}

pub(crate) fn diagnostics(text: &str) -> Vec<Diagnostic> {
    diagnostics_with(text, &NoFiles)
}

pub(crate) fn diagnostics_with(text: &str, files: &dyn FileSource) -> Vec<Diagnostic> {
    load(".github/workflows/test.yml", text, files)
        .diagnostics
        .into_vec()
}
