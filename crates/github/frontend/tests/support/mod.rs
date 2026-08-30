//! Lowering helpers: text in, graph or diagnostics out. Nothing here runs a
//! graph — the run harness lives with the acceptance battery.

#![allow(dead_code)]

use std::collections::BTreeMap;

use frontend::{Diagnostic, FileSource, MapFiles, NoFiles};
use frontend_gha::load;
use ir::Graph;

pub fn files(pairs: &[(&str, &str)]) -> MapFiles {
    MapFiles(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<BTreeMap<_, _>>(),
    )
}

pub fn lower_ok(text: &str) -> Graph {
    lower_ok_with(text, &NoFiles)
}

pub fn lower_ok_with(text: &str, files: &dyn FileSource) -> Graph {
    let lowered = load(".github/workflows/test.yml", text, files);
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    lowered.graph.expect("expected a graph")
}

pub fn diagnostics(text: &str) -> Vec<Diagnostic> {
    diagnostics_with(text, &NoFiles)
}

pub fn diagnostics_with(text: &str, files: &dyn FileSource) -> Vec<Diagnostic> {
    load(".github/workflows/test.yml", text, files)
        .diagnostics
        .into_vec()
}
