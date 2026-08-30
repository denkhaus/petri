//! The component rules, enforced mechanically.
//!
//! Directories do not stop a stray dependency; this does. The rules:
//!
//! 1. **Core is self-contained.** A crate under `crates/core/` depends only on
//!    crates under `crates/core/` — normal, build, and dev alike. Core never
//!    names a component, a distribution, or the binary.
//! 2. **Components see core, and only core.** A crate under
//!    `crates/<component>/` (anything that is not `core`) depends only on core
//!    and on its own component. Components never depend on each other —
//!    anything two components need is core — and never on the distribution, not
//!    even for tests.
//! 3. Only the distribution (`crates/petri/`) may depend on component crates.
//!
//! The check reads `cargo metadata`, so it sees what cargo sees, not what the
//! manifests appear to say.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

/// Which world a workspace crate lives in, by its manifest path.
#[derive(Clone, Debug, PartialEq, Eq)]
enum World {
    Core,
    Component(String),
    Distribution,
}

fn world(workspace_root: &Path, crate_dir: &Path) -> World {
    let rel = crate_dir
        .strip_prefix(workspace_root)
        .expect("workspace member outside the workspace");
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        parts.first().map(String::as_str),
        Some("crates"),
        "member outside crates/: {rel:?}"
    );
    assert!(
        parts.len() >= 3,
        "every crate lives at crates/<group>/<name>; {rel:?} is directly under crates/"
    );
    match parts[1].as_str() {
        "core" => World::Core,
        "petri" => World::Distribution,
        component => World::Component(component.to_string()),
    }
}

#[test]
fn core_never_depends_on_a_component() {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo metadata runs");
    assert!(output.status.success(), "cargo metadata failed");
    let meta: Value = serde_json::from_slice(&output.stdout).expect("metadata is JSON");

    let root = Path::new(meta["workspace_root"].as_str().unwrap());
    let members: Vec<&str> = meta["workspace_members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap())
        .collect();

    // Package id -> (name, world); workspace members only.
    let mut worlds: HashMap<&str, (String, World)> = HashMap::new();
    for pkg in meta["packages"].as_array().unwrap() {
        let id = pkg["id"].as_str().unwrap();
        if !members.contains(&id) {
            continue;
        }
        let manifest = Path::new(pkg["manifest_path"].as_str().unwrap());
        worlds.insert(
            id,
            (
                pkg["name"].as_str().unwrap().to_string(),
                world(root, manifest.parent().unwrap()),
            ),
        );
    }
    let by_name: HashMap<&str, &World> = worlds.values().map(|(n, w)| (n.as_str(), w)).collect();
    assert!(
        worlds.len() >= 16,
        "expected the whole workspace, saw {}",
        worlds.len()
    );

    let mut violations = Vec::new();
    for pkg in meta["packages"].as_array().unwrap() {
        let id = pkg["id"].as_str().unwrap();
        let Some((name, from)) = worlds.get(id) else {
            continue;
        };
        for dep in pkg["dependencies"].as_array().unwrap() {
            let dep_name = dep["name"].as_str().unwrap();
            let Some(to) = by_name.get(dep_name) else {
                continue; // not a workspace crate
            };
            let kind = dep["kind"].as_str().unwrap_or("normal");
            let ok = match (from, to) {
                // Rule 1: core reaches only core.
                (World::Core, World::Core) => true,
                (World::Core, _) => false,
                // Rule 2: a component reaches core and itself; never another
                // component, never the distribution — dev deps included.
                (World::Component(_), World::Core) => true,
                (World::Component(a), World::Component(b)) => a == b,
                (World::Component(_), World::Distribution) => false,
                // Rule 3: the distribution reaches everything.
                (World::Distribution, _) => true,
            };
            if !ok {
                violations.push(format!("{name} -> {dep_name} ({kind}): {from:?} -> {to:?}"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "component rules violated:\n  {}",
        violations.join("\n  ")
    );
}
