//! Required assets of the black box battery: the materialized bundles, the
//! pinned Fabro binary, the Fabro corpus, and a Docker daemon.
//!
//! Every check follows the convention `PETRI_REQUIRE_DOCKER` set: an absent
//! asset skips with a visible notice on a developer machine, and fails when
//! the matching `PETRI_REQUIRE_*` variable says CI expected it. A silently
//! skipped scenario is indistinguishable from a passing one, and the
//! readiness gate never counts a skip as a pass.
//!
//! | Asset | Variable | Materialized by |
//! |---|---|---|
//! | bundles | `PETRI_REQUIRE_FABRO_BUNDLES` | `scripts/corpus-fetch-fabro-bundles.sh` |
//! | fabro binary | `PETRI_REQUIRE_FABRO_BINARY` | `scripts/fabro-binary.sh` (handed over as `PETRI_FABRO_BIN`) |
//! | corpus | `PETRI_REQUIRE_FABRO_CORPUS` | `scripts/corpus-fetch-fabro.sh` |
//! | Docker | `PETRI_REQUIRE_DOCKER` | a reachable daemon and the Docker plugin |

use std::env;
use std::path::{Path, PathBuf};

/// The skip-or-fail decision, kept pure so it can be tested without touching
/// the process environment. `required` is the value of the `PETRI_REQUIRE_*`
/// variable, `None` when unset. Returns whether the asset is usable, or the
/// failure message when it is absent and required.
pub(crate) fn decide(
    present: bool,
    required: Option<&str>,
    variable: &str,
    what: &str,
) -> Result<bool, String> {
    if present {
        return Ok(true);
    }
    match required {
        Some(value) if !value.is_empty() => Err(format!("{variable} is set, but {what}")),
        _ => Ok(false),
    }
}

/// Apply [`decide`] to the process environment, printing the skip notice.
///
/// # Panics
///
/// Panics with the failure message when the asset is absent and required.
#[expect(
    clippy::print_stderr,
    reason = "the skip notice belongs to the test runner's output, which no subscriber reads"
)]
pub(crate) fn require_or_skip(present: bool, variable: &str, what: &str) -> bool {
    let required = env::var(variable).ok();
    match decide(present, required.as_deref(), variable, what) {
        Ok(true) => true,
        Ok(false) => {
            eprintln!("skipping: {what}");
            false
        }
        Err(message) => panic!("{message}"),
    }
}

/// The workspace root, from this crate's manifest directory.
pub(crate) fn workspace_root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    root.canonicalize().unwrap_or(root)
}

/// The materialized bundle `crates/fabro/acceptance/bundles/<id>`, or `None`
/// (with a notice) when the bundle set is not fetched. The set is verified
/// as a whole: `MANIFEST.txt` exists only after every bundle passed its
/// digest check.
///
/// # Panics
///
/// Panics when `PETRI_REQUIRE_FABRO_BUNDLES` is set and the bundle is absent.
pub(crate) fn bundle(id: &str) -> Option<PathBuf> {
    let root = workspace_root().join("crates/fabro/acceptance/bundles");
    let dir = root.join(id);
    let present = root.join("MANIFEST.txt").is_file() && dir.is_dir();
    require_or_skip(
        present,
        "PETRI_REQUIRE_FABRO_BUNDLES",
        &format!(
            "the Fabro bundle `{id}` is not materialized under {} (run \
             scripts/corpus-fetch-fabro-bundles.sh)",
            root.display()
        ),
    )
    .then_some(dir)
}

/// The pinned `fabro` binary `PETRI_FABRO_BIN` names, or `None` (with a
/// notice) when it is not provided. `scripts/test-fabro-differential.sh`
/// sets the variable from `scripts/fabro-binary.sh`, which checks the
/// binary reports the pinned commit; a `fabro` on `PATH` is never used.
///
/// # Panics
///
/// Panics when `PETRI_REQUIRE_FABRO_BINARY` is set and the binary is absent.
pub(crate) fn fabro_binary() -> Option<PathBuf> {
    let path = env::var_os("PETRI_FABRO_BIN").map(PathBuf::from);
    let present = path.as_deref().is_some_and(Path::is_file);
    require_or_skip(
        present,
        "PETRI_REQUIRE_FABRO_BINARY",
        "PETRI_FABRO_BIN does not name the pinned fabro binary (run through \
         scripts/test-fabro-differential.sh, or scripts/fabro-binary.sh)",
    )
    .then(|| path.unwrap_or_default())
}

/// The fetched Fabro corpus `crates/fabro/corpus/fabro`, or `None` (with a
/// notice) when it is not fetched.
///
/// # Panics
///
/// Panics when `PETRI_REQUIRE_FABRO_CORPUS` is set and the corpus is absent.
pub(crate) fn fabro_corpus() -> Option<PathBuf> {
    let root = workspace_root().join("crates/fabro/corpus/fabro");
    require_or_skip(
        root.join(".fabro/workflows").is_dir(),
        "PETRI_REQUIRE_FABRO_CORPUS",
        "the Fabro corpus is not fetched (run scripts/corpus-fetch-fabro.sh)",
    )
    .then_some(root)
}

/// A reachable Docker daemon with the Docker plugin, under
/// `PETRI_REQUIRE_DOCKER`'s convention.
pub(crate) async fn docker() -> bool {
    testkit::is_docker_ready().await
}
