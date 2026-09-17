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
//!    even for tests. The one exception is a declared layer ([`LAYERED`]):
//!    `fabro` is built on `attractor`, since the Fabro frontend wraps the
//!    Attractor frontend and registers its step kinds
//!    (`.ai/plans/attractor-split.md`). The layer runs one way; `attractor`
//!    never names `fabro`.
//! 3. Only the distribution (`crates/petri/`) may depend on component crates.
//! 4. **Fabro's dependency set stands alone.** Fabro depends on the six crates
//!    in [`FABRO_DEPENDENCIES`], not on the distribution or on the GitHub
//!    component. The build closure of those six reaches only core, `attractor`
//!    and `fabro` (`.ai/plans/fabro-integration.md`, P1.6).
//!
//! The check reads `cargo metadata`, so it sees what cargo sees, not what the
//! manifests appear to say.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::process::Command;

use serde_json::Value;

/// Component pairs `(upper, lower)` where `upper` may depend on `lower`: the
/// layers rule 2 allows. Every other component pair is a violation.
const LAYERED: &[(&str, &str)] = &[("fabro", "attractor")];

/// The packages Fabro pins: `Runtime::standard()` comes from `petri-runtime`
/// and the Attractor registration from `petri-attractor-steps`, so a host
/// assembles a Fabro runtime from these alone (`crates/fabro/HANDOFF.md`).
const FABRO_DEPENDENCIES: &[&str] = &[
    "petri-runtime",
    "petri-execution",
    "petri-store",
    "petri-attractor-steps",
    "petri-frontend-attractor",
    "petri-frontend-fabro",
];

/// The components Fabro's build closure may reach.
const FABRO_COMPONENTS: &[&str] = &["attractor", "fabro"];

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
        "member outside crates/: {}",
        rel.display()
    );
    assert!(
        parts.len() >= 3,
        "every crate lives at crates/<group>/<name>; {} is directly under crates/",
        rel.display()
    );
    match parts[1].as_str() {
        "core" => World::Core,
        "petri" => World::Distribution,
        component => World::Component(component.to_string()),
    }
}

fn metadata() -> Value {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo metadata runs");
    assert!(output.status.success(), "cargo metadata failed");
    serde_json::from_slice(&output.stdout).expect("metadata is JSON")
}

#[test]
fn core_never_depends_on_a_component() {
    let meta = metadata();

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
            #[expect(
                clippy::match_same_arms,
                reason = "one arm per rule from the module docs, in rule order; merging arms \
                          by body would mix the three rules and hide which rule decides a pair"
            )]
            let ok = match (from, to) {
                // Rule 1: core reaches only core.
                (World::Core, World::Core) => true,
                (World::Core, _) => false,
                // Rule 2: a component reaches core, itself, and the component
                // it is declared to sit on; never another component, never
                // the distribution — dev deps included.
                (World::Component(_), World::Core) => true,
                (World::Component(a), World::Component(b)) => {
                    a == b || LAYERED.contains(&(a.as_str(), b.as_str()))
                }
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

/// Rule 4: from the six packages Fabro pins, follow every normal and build
/// edge of the resolve graph. Nothing reached is the distribution or a
/// component outside `attractor` and `fabro` — in particular, no GitHub
/// crate. Dev edges stay out: a test dependency is not what a host links.
#[test]
fn fabro_dependencies_reach_neither_github_nor_the_distribution() {
    let meta = metadata();
    let root = Path::new(meta["workspace_root"].as_str().unwrap());
    let packages = meta["packages"].as_array().unwrap();

    // Package id -> (name, world), for every package cargo resolved; a
    // non-workspace package has no world.
    let mut names: HashMap<&str, &str> = HashMap::new();
    let mut worlds: HashMap<&str, World> = HashMap::new();
    let members: Vec<&str> = meta["workspace_members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap())
        .collect();
    for pkg in packages {
        let id = pkg["id"].as_str().unwrap();
        names.insert(id, pkg["name"].as_str().unwrap());
        if members.contains(&id) {
            let manifest = Path::new(pkg["manifest_path"].as_str().unwrap());
            worlds.insert(id, world(root, manifest.parent().unwrap()));
        }
    }

    let roots: Vec<&str> = FABRO_DEPENDENCIES
        .iter()
        .map(|wanted| {
            *names
                .iter()
                .find(|(id, name)| **name == *wanted && members.contains(id))
                .map_or_else(
                    || panic!("{wanted} is not a workspace member"),
                    |(id, _)| id,
                )
        })
        .collect();

    // Edges of the resolve graph, normal and build kinds only.
    let mut edges: HashMap<&str, Vec<&str>> = HashMap::new();
    for node in meta["resolve"]["nodes"].as_array().unwrap() {
        let from = node["id"].as_str().unwrap();
        for dep in node["deps"].as_array().unwrap() {
            let linked = dep["dep_kinds"]
                .as_array()
                .unwrap()
                .iter()
                .any(|k| matches!(k["kind"].as_str(), None | Some("normal" | "build")));
            if linked {
                edges
                    .entry(from)
                    .or_default()
                    .push(dep["pkg"].as_str().unwrap());
            }
        }
    }

    let mut seen: HashSet<&str> = HashSet::new();
    let mut stack = roots.clone();
    while let Some(id) = stack.pop() {
        if seen.insert(id) {
            stack.extend(edges.get(id).into_iter().flatten().copied());
        }
    }

    let reached: BTreeSet<&str> = seen
        .iter()
        .filter(|id| worlds.contains_key(*id))
        .map(|id| names[id])
        .collect();
    assert!(
        FABRO_DEPENDENCIES.iter().all(|d| reached.contains(d)),
        "the closure misses one of its own roots: {reached:?}"
    );

    let violations: Vec<String> = seen
        .iter()
        .filter_map(|id| {
            let ok = match worlds.get(id)? {
                World::Core => true,
                World::Component(c) => FABRO_COMPONENTS.contains(&c.as_str()),
                World::Distribution => false,
            };
            (!ok).then(|| format!("{} ({:?})", names[id], worlds[id]))
        })
        .collect();
    assert!(
        violations.is_empty(),
        "Fabro's dependency closure reaches outside core, attractor and fabro:\n  {}\nreached: \
         {reached:?}",
        violations.join("\n  ")
    );
}
