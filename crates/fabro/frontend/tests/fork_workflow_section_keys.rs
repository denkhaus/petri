//! Fork presence pin (denkhaus/work#1509): the `[workflow]` section keys
//! the gear fleet declares — `terminal_cleanup` among them (work#1478) —
//! sit in the frontend validator's allow-list, so run-create accepts the
//! bundles instead of refusing every workflow that carries the key. A
//! `just vendor import` from a wrong fork state rewrites the tree cleanly
//! and would silently drop the allow-list entry — this file is the pin
//! that fails when that happens.

use std::collections::BTreeMap;

use frontend::{CompileInputs, MapFiles};
use frontend_fabro::load;

/// The `[workflow]` table the gear fleet carries once the adoption lands
/// (the five tomls mirror develop's landed shape): every key the
/// validator's allow-list must accept, in one file.
const GEAR_FLEET_SECTION: &str = r#"
_version = 1

[workflow]
name = "gear"
description = "the gear graph"
graph = "workflow.fabro"
metadata = { lane = "develop" }
engine = "petri"
terminal_cleanup = "nu .fabro/scripts/run-cleanup.nu"
"#;

#[test]
fn the_gear_fleets_workflow_section_keys_validate() {
    let files = MapFiles(BTreeMap::from([(
        "workflow.toml".to_string(),
        GEAR_FLEET_SECTION.to_string(),
    )]));
    let text =
        "digraph T {\n  start [shape=Mdiamond]\n  exit [shape=Msquare]\n  start -> exit\n}\n";
    let lowered = load("w.fabro", text, &files, &CompileInputs::new());
    let refusals: Vec<String> = lowered
        .diagnostics
        .iter()
        .filter(|d| d.code == "unsupported.workflow_toml.key")
        .map(ToString::to_string)
        .collect();
    assert!(
        refusals.is_empty(),
        "the gear fleet's `[workflow]` keys must validate, refused: {refusals:?}"
    );
    assert!(
        lowered.graph.is_some(),
        "the bundle must lower: {:?}",
        lowered.diagnostics
    );
}
