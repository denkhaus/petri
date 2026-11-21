//! Fork-only presence and behavior pins for the environment cook's
//! single-flight machinery (work#1440, work#1454). A `just vendor
//! import` from a wrong fork state rewrites `vendor/PINS.toml` cleanly
//! and would silently drop the feature; these tests keep it observable.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use super::image_flight;

/// 2020-01-01T00:00:00Z: an mtime no fresh clone would carry.
fn aged_mtime() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_577_836_800)
}

/// Stamps every regular file under `root` with `mtime`, so a fixture
/// can masquerade as a fresh clone or an aged checkout.
fn stamp_mtimes(dir: &Path, mtime: SystemTime) {
    for entry in std::fs::read_dir(dir).expect("read fixture dir") {
        let path = entry.expect("fixture entry").path();
        if path.is_dir() {
            stamp_mtimes(&path, mtime);
        } else {
            std::fs::File::open(&path)
                .expect("open fixture file")
                .set_modified(mtime)
                .expect("stamp fixture mtime");
        }
    }
}

/// A throwaway git checkout fixture: a directory with `.git`, one
/// tracked-looking file, a nested file, and the two skip-rule
/// candidates (a root `.dockerignore` and a nested `.git` directory).
fn context_fixture(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "petri-image-flight-{}-{}",
        label,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("nested/.git")).expect("create nested fixture dirs");
    std::fs::create_dir_all(root.join(".git")).expect("create root .git");
    std::fs::write(root.join("nested/keep.txt"), "kept").expect("write nested file");
    std::fs::write(root.join(".dockerignore"), "ignored").expect("write dockerignore");
    std::fs::write(root.join("nested/.git/objects"), "ignored").expect("write nested git file");
    root
}

/// The manifest digest is stable for an unchanged tree and skips what
/// `pack_context` skips: `.git` at every level and a root
/// `.dockerignore` do not move the digest, but a file's content (via
/// size) does.
#[test]
fn manifest_digest_is_stable_and_skips_the_pack_skip_list() {
    let root = context_fixture("digest");
    let one = image_flight::manifest_digest("FROM busybox", &root).expect("first digest");
    let two = image_flight::manifest_digest("FROM busybox", &root).expect("second digest");
    assert_eq!(one, two, "an unchanged tree must digest identically");

    // The Dockerfile text participates.
    let other = image_flight::manifest_digest("FROM alpine", &root).expect("alpine digest");
    assert_ne!(one, other, "a changed Dockerfile must move the digest");

    // Skip rules: mutating excluded entries never moves the digest.
    std::fs::write(root.join(".dockerignore"), "changed").expect("rewrite dockerignore");
    std::fs::write(root.join("nested/.git/objects"), "changed").expect("rewrite nested git");
    let skipped = image_flight::manifest_digest("FROM busybox", &root).expect("skip digest");
    assert_eq!(one, skipped, "excluded entries must not move the digest");

    // A size change in a kept file does.
    std::fs::write(root.join("nested/keep.txt"), "kept longer").expect("grow kept file");
    let grown = image_flight::manifest_digest("FROM busybox", &root).expect("grown digest");
    assert_ne!(one, grown, "a changed file must move the digest");

    let _ = std::fs::remove_dir_all(&root);
}

/// THE clone-stability pin (work#1454): every run checkout is a FRESH
/// clone/pack, so the same trunk tip arrives under a different set of
/// mtimes in every root. Two roots carrying IDENTICAL content but
/// different mtimes MUST digest equal — under the mtime manifest they
/// did not, so the tag-keyed single-flight guard never grouped
/// concurrent cooks (seven distinct `petri-env-` tags for one trunk
/// tip in 13 minutes, fabro container logs 2026-11-18). There is no
/// end-to-end concurrent-cook harness in this suite — the cook path
/// runs only against a real docker provider — so "same content, one
/// build" rests on this pin composed with `guard_for_is_per_tag`:
/// equal digests mint the equal tag that maps to one guard, hence one
/// build.
#[test]
fn manifest_digest_is_clone_stable_across_different_mtimes() {
    let aged = context_fixture("clone-aged");
    let fresh = context_fixture("clone-fresh");
    stamp_mtimes(&aged, aged_mtime());
    stamp_mtimes(&fresh, SystemTime::now());
    let one = image_flight::manifest_digest("FROM busybox", &aged).expect("aged digest");
    let two = image_flight::manifest_digest("FROM busybox", &fresh).expect("fresh digest");
    assert_eq!(
        one, two,
        "identical content must digest equal across different mtimes"
    );
    let _ = std::fs::remove_dir_all(&aged);
    let _ = std::fs::remove_dir_all(&fresh);
}

/// Content-sensitivity pin (work#1454): two roots identical except ONE
/// file's content — SAME size, SAME mtimes — MUST digest differently.
/// This is the over-normalization guard: it proves content itself
/// moves the digest, not merely size or time, and under the mtime
/// manifest such an equal-size edit aliased into the other root's tag.
#[test]
fn manifest_digest_tracks_content_beyond_size_and_mtime() {
    let one_root = context_fixture("content-a");
    let other_root = context_fixture("content-b");
    // "kept" -> "kepT": four bytes either way, mtimes frozen equal.
    std::fs::write(other_root.join("nested/keep.txt"), "kepT").expect("swap content");
    stamp_mtimes(&one_root, aged_mtime());
    stamp_mtimes(&other_root, aged_mtime());
    let one = image_flight::manifest_digest("FROM busybox", &one_root).expect("first digest");
    let two = image_flight::manifest_digest("FROM busybox", &other_root).expect("second digest");
    assert_ne!(one, two, "an equal-size content edit must move the digest");
    let _ = std::fs::remove_dir_all(&one_root);
    let _ = std::fs::remove_dir_all(&other_root);
}

/// A directory without `.git` is refused: the context must be a git
/// checkout, exactly as `pack_context` demands.
#[test]
fn manifest_digest_refuses_a_non_checkout() {
    let root = std::env::temp_dir().join(format!("petri-image-flight-bare-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create bare fixture");
    let verdict = image_flight::manifest_digest("FROM busybox", &root)
        .expect_err("a bare directory must be refused");
    assert!(
        verdict.contains("no .git"),
        "the refusal must name the cause: {verdict}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The single-flight registry hands every caller of a tag the SAME
/// guard, and different tags different guards.
#[test]
fn guard_for_is_per_tag() {
    let a1 = image_flight::guard_for("petri-env-flight-a");
    let a2 = image_flight::guard_for("petri-env-flight-a");
    let b = image_flight::guard_for("petri-env-flight-b");
    assert!(Arc::ptr_eq(&a1, &a2), "one tag, one guard");
    assert!(!Arc::ptr_eq(&a1, &b), "distinct tags hold distinct guards");
}

/// The fast-path ledger records built tags and drops exactly what the
/// daemon-side sweep evicts. The ledger is process-global, so the
/// test rides unique tags and leaves them evicted.
#[tokio::test]
async fn ledger_round_trips_build_and_eviction() {
    let tag = "petri-env-flight-ledger";
    image_flight::note_built(tag);
    assert!(
        image_flight::is_built(tag),
        "a cooked tag is on the fast path"
    );
    image_flight::forget(&[tag.to_owned(), "petri-env-flight-never".to_owned()]);
    assert!(
        !image_flight::is_built(tag),
        "an evicted tag leaves the fast path"
    );
}

/// Presence pin: the ceiling and prefix exist with the decided shape
/// (work#1440: count-form ceiling, `petri-env-` prefix keyed by the
/// daemon-side sweep).
#[test]
fn ceiling_and_prefix_keep_their_decided_shape() {
    assert_eq!(image_flight::ENV_TAG_PREFIX, "petri-env-");
    assert_eq!(image_flight::ENV_IMAGE_CEILING, 6);
}

// Cross-process single-flight pins (work#1463). The cooks run one
// process per gear, so the in-process guard/ledger see only THIS
// process's cooks; the daemon presence probe is the cross-process
// half. No end-to-end concurrent-cook harness exists in this suite
// (the cook path runs only against a real docker provider), so the
// pins drive `image_build::cook_path` — the exact decision the ensure
// path composes with its ledger miss and the probe — the pure seam
// the item named for the simulation.
mod cross_process {
    use sandbox_driver::Error;

    use crate::image_build::{CookPath, cook_path};
    use crate::image_flight;

    /// Pin (a) of work#1463, red under the old code: two cooks from
    /// two "processes" — the second's ledger is empty (a fresh process
    /// static) — MUST let the second reuse WITHOUT a build when the
    /// daemon already holds the tag. Observed before the fix: every
    /// process held its own empty ledger and guard registry and built
    /// (three mints of one tag, 2026-11-19 fabro container logs).
    #[test]
    fn a_daemon_present_second_process_reuses_without_a_build() {
        let tag = "petri-env-flight-cross-present";
        image_flight::forget(&[tag.to_owned()]); // the ledger starts EMPTY (fresh process)
        assert!(
            !image_flight::is_built(tag),
            "the pin must start from a ledger miss, as a fresh process does"
        );
        assert_eq!(
            cook_path(Ok(true), tag),
            CookPath::Reuse,
            "a presence hit must reuse"
        );
        assert!(
            image_flight::is_built(tag),
            "the reused tag joins the fast-path ledger"
        );
        image_flight::forget(&[tag.to_owned()]);
    }

    /// A daemon miss builds, and does NOT pre-seed the ledger: only a
    /// successful cook may (the build path's own `note_built`).
    #[test]
    fn a_daemon_absent_cook_builds_without_touching_the_ledger() {
        let tag = "petri-env-flight-cross-absent";
        image_flight::forget(&[tag.to_owned()]);
        assert_eq!(cook_path(Ok(false), tag), CookPath::Build);
        assert!(
            !image_flight::is_built(tag),
            "the decision alone must not mark the tag built"
        );
    }

    /// A probe failure fails OPEN to Build — a duplicate build whose
    /// layers the daemon dedupes, never a stuck cook.
    #[test]
    fn an_unavailable_probe_fails_open_to_build() {
        let tag = "petri-env-flight-cross-unavail";
        image_flight::forget(&[tag.to_owned()]);
        let probe = Err(Error::unsupported(sandbox_driver::Capability::OneShot));
        assert_eq!(cook_path(probe, tag), CookPath::Build);
        assert!(!image_flight::is_built(tag));
    }
}

// The cook lock's pure machinery (work#1551): the waiter's decision,
// the CAS-loss classification, and the decided constants. No daemon
// needed — the docker-gated storm pin below drives the live path.
mod cook_lock_pins {
    use std::time::Duration;

    use sandbox_driver::Error as DriverError;

    use crate::cook_lock::{
        self, LockSight, WaiterStep, is_name_conflict, lock_name, stale, waiter_step,
    };

    fn alive(holder: Option<&str>) -> LockSight {
        LockSight::Alive {
            id:     "lock-1".to_owned(),
            holder: holder.map(str::to_owned),
        }
    }

    /// The decided constants keep their shape (decision 1-3): the fixed
    /// lock name, a poll inside the 15-30s band, a bound that outlives
    /// the slowest observed cold cook (~47 min), a short grace.
    #[test]
    fn the_decided_shapes_hold() {
        assert_eq!(cook_lock::LOCK_NAME_PREFIX, "petri-cook-lock-");
        assert_eq!(lock_name("petri-env-abc"), "petri-cook-lock-petri-env-abc");
        assert_eq!(cook_lock::LOCK_ROLE_VALUE, "env-cook-lock");
        assert_eq!(cook_lock::LOCK_TAG_LABEL, "petri.cook.tag");
        assert_eq!(cook_lock::LOCK_HOLDER_LABEL, "petri.cook.holder");
        assert!(cook_lock::WAIT_POLL >= Duration::from_secs(15));
        assert!(cook_lock::WAIT_POLL <= Duration::from_secs(30));
        assert!(cook_lock::WAIT_BOUND >= Duration::from_secs(60 * 60));
        assert!(cook_lock::LOCK_RECLAIM_GRACE >= Duration::from_secs(10));
        assert!(cook_lock::LOCK_RECLAIM_GRACE <= Duration::from_secs(120));
    }

    /// Only the daemon's name-conflict refusal reads as a CAS loss;
    /// every other InvalidSpec and every other error is a create
    /// failure the cook must surface, not interpret.
    #[test]
    fn only_the_name_conflict_reads_as_a_cas_loss() {
        let conflict =
            DriverError::invalid_spec("name", "a container with this name already exists");
        assert!(is_name_conflict(&conflict));
        let other_spec = DriverError::invalid_spec("name", "a sandbox with sidecars must be named");
        assert!(
            !is_name_conflict(&other_spec),
            "a different name fault is not a loss"
        );
        let transport =
            DriverError::Transport(sandbox_driver::TransportError::new("the plugin died"));
        assert!(!is_name_conflict(&transport));
    }

    /// The mint ends the wait wherever the lock stands (decision 2's
    /// happy path), and the bound parks fail-closed before any build
    /// could start.
    #[test]
    fn the_waiter_reuses_at_the_mint_and_parks_at_the_bound() {
        assert_eq!(
            waiter_step(
                true,
                &alive(Some("holder-1")),
                Some(true),
                None,
                Duration::ZERO
            ),
            WaiterStep::Reuse,
            "a minted tag ends the wait immediately"
        );
        assert_eq!(
            waiter_step(
                false,
                &alive(Some("holder-1")),
                Some(true),
                None,
                cook_lock::WAIT_BOUND,
            ),
            WaiterStep::Park,
            "past the bound the cook fails closed"
        );
        // Park outranks a stale lock: the bound is hard.
        assert_eq!(
            waiter_step(
                false,
                &LockSight::Gone,
                None,
                Some(cook_lock::LOCK_RECLAIM_GRACE),
                cook_lock::WAIT_BOUND,
            ),
            WaiterStep::Park
        );
    }

    /// A live winner means keep waiting, whatever the grace clock says;
    /// a winner that cannot be observed is treated as alive (reading a
    /// live winner out of a transient listing error would take over a
    /// running cook).
    #[test]
    fn a_live_or_unobservable_winner_keeps_the_waiter_waiting() {
        assert_eq!(
            waiter_step(
                false,
                &alive(Some("holder-1")),
                Some(true),
                Some(cook_lock::LOCK_RECLAIM_GRACE * 10),
                Duration::ZERO,
            ),
            WaiterStep::KeepWaiting
        );
        assert_eq!(
            waiter_step(
                false,
                &alive(Some("holder-1")),
                None,
                Some(cook_lock::LOCK_RECLAIM_GRACE * 10),
                Duration::ZERO,
            ),
            WaiterStep::KeepWaiting,
            "an undeterminable winner must not read as dead"
        );
    }

    /// A stale lock — gone, dead, or held by a winner whose holder no
    /// longer runs — is taken over only past the grace, and the take
    /// over names the corpse for deletion (decision 3).
    #[test]
    fn a_stale_lock_is_taken_over_past_the_grace() {
        for (lock, id) in [
            (LockSight::Gone, None),
            (
                LockSight::Dead {
                    id: "corpse".to_owned(),
                },
                Some("corpse"),
            ),
        ] {
            assert_eq!(
                waiter_step(false, &lock, None, None, Duration::ZERO),
                WaiterStep::KeepWaiting,
                "a fresh stale observation waits out the grace"
            );
            let step = waiter_step(
                false,
                &lock,
                None,
                Some(cook_lock::LOCK_RECLAIM_GRACE),
                Duration::ZERO,
            );
            assert_eq!(
                step,
                WaiterStep::TakeOver {
                    id: id.map(str::to_owned),
                },
                "past the grace the waiter takes the lock over"
            );
        }
        // A running lock whose winner's holder died is equally stale.
        assert!(stale(&alive(Some("holder-1")), Some(false)));
        assert_eq!(
            waiter_step(
                false,
                &alive(Some("holder-1")),
                Some(false),
                Some(cook_lock::LOCK_RECLAIM_GRACE),
                Duration::ZERO,
            ),
            WaiterStep::TakeOver {
                id: Some("lock-1".to_owned()),
            }
        );
    }

    /// A lock without the holder label predates the liveness contract:
    /// conservative, it never reads as stale on its own.
    #[test]
    fn an_unlabeled_lock_reads_conservatively() {
        assert!(!stale(&alive(None), Some(false)));
        assert_eq!(
            waiter_step(
                false,
                &alive(None),
                Some(false),
                Some(cook_lock::LOCK_RECLAIM_GRACE),
                Duration::ZERO,
            ),
            WaiterStep::KeepWaiting
        );
    }
}

// The cook-input digest's pure half (work#1551 decision 4): what moves
// the recipe digest and what must never alias onto it.
mod recipe_digest_pins {
    use ir::{ImageCook, SandboxInstance};

    use crate::image_flight;

    const DOCKERFILE: &str = "FROM ubuntu:24.04 AS base\nRUN echo toolchain\n";

    /// The digest inputs are exactly the COOK inputs: the recipe, the
    /// Dockerfile text, and the base image reference. A source-only
    /// change moves none of them — the same recipe bytes mint the same
    /// tag — while a recipe change, a Dockerfile change, and a base
    /// move each mint a different tag.
    #[test]
    fn the_digest_tracks_the_cook_inputs_alone() {
        let one = image_flight::recipe_digest(b"{\"recipe\":1}", DOCKERFILE);
        assert_eq!(
            one,
            image_flight::recipe_digest(b"{\"recipe\":1}", DOCKERFILE),
            "identical cook inputs digest identically"
        );
        assert_ne!(
            one,
            image_flight::recipe_digest(b"{\"recipe\":2}", DOCKERFILE),
            "a recipe change must move the digest"
        );
        assert_ne!(
            one,
            image_flight::recipe_digest(b"{\"recipe\":1}", "FROM ubuntu:24.04\nRUN echo other\n"),
            "a Dockerfile change must move the digest"
        );
        assert_ne!(
            one,
            image_flight::recipe_digest(
                b"{\"recipe\":1}",
                "FROM ubuntu:26.04 AS base\nRUN echo toolchain\n"
            ),
            "a base image move must move the digest"
        );
        // The tag shape matches the whole-tree arm's.
        assert_eq!(one.len(), 12, "the digest stays a 12-hex tag");
        assert!(
            one.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "the digest is hex: {one}"
        );
    }

    /// A recipe digest and a whole-tree manifest digest never alias
    /// onto one tag: the domain separator keeps the bootstrap arm and
    /// the recipe arm of one context from colliding — a same-input
    /// collision would need a sha256 break, and this pin holds the
    /// two arms to their separation on a real fixture.
    #[test]
    fn the_recipe_and_manifest_digests_never_alias() {
        let root = super::context_fixture("recipe-alias");
        let dockerfile = "FROM busybox\n";
        let manifest =
            image_flight::manifest_digest(dockerfile, &root).expect("the manifest arm digests");
        let recipe = image_flight::recipe_digest(dockerfile.as_bytes(), dockerfile);
        assert_ne!(
            manifest, recipe,
            "the two digest arms must not mint one tag"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The base reference parser reads the first FROM as written:
    /// flags skipped, `AS name` ignored, case-insensitive verb,
    /// comments and ARGs before FROM tolerated literally.
    #[test]
    fn the_base_reference_reads_the_first_from() {
        assert_eq!(
            image_flight::base_image_reference("FROM ubuntu:24.04\n"),
            Some("ubuntu:24.04")
        );
        assert_eq!(
            image_flight::base_image_reference("from ubuntu:24.04 AS base\n"),
            Some("ubuntu:24.04")
        );
        assert_eq!(
            image_flight::base_image_reference(
                "# comment\nARG VER=1\nFROM --platform=linux/amd64 ghcr.io/fabro-sh/base:3\n"
            ),
            Some("ghcr.io/fabro-sh/base:3")
        );
        assert_eq!(
            image_flight::base_image_reference("RUN nothing\n"),
            None,
            "no FROM, no reference"
        );
    }

    /// The cook outcome rides the `scope.acquired` record's sandbox
    /// instance (decision 5): every variant serializes snake_case, an
    /// absent outcome stays absent on the wire, and an old record
    /// without the field still deserializes.
    #[test]
    fn the_cook_outcome_serializes_onto_the_sandbox_record() {
        let instance = SandboxInstance {
            provider:          "docker".into(),
            instance:          "abc".into(),
            image:             Some("petri-env-abc".into()),
            snapshot:          None,
            working_directory: "/workspace".into(),
            image_cook:        Some(ImageCook::Waited {
                tag: "petri-env-abc".into(),
            }),
        };
        let json = serde_json::to_value(&instance).expect("serializes");
        assert_eq!(
            json["image_cook"],
            serde_json::json!({ "waited": { "tag": "petri-env-abc" } }),
            "the outcome carries its snake_case tag: {json}"
        );
        let back: SandboxInstance = serde_json::from_value(json).expect("round-trips");
        assert_eq!(back.image_cook, instance.image_cook);

        let legacy = serde_json::json!({
            "provider": "docker",
            "instance": "abc",
            "working_directory": "/workspace",
        });
        let old: SandboxInstance =
            serde_json::from_value(legacy).expect("a record from before the field reads");
        assert_eq!(old.image_cook, None);
    }
}

/// THE STORM PIN (work#1551): N concurrent FIRST cooks of the same fresh
/// tip against a real Docker daemon must yield exactly ONE build and N-1
/// reuses. Every earlier fix looked green because no test ever drove
/// concurrent cooks — the in-process guard serializes same-process cooks,
/// so the pin spawns REAL CHILD PROCESSES (the gears run one cook process
/// each, work#1463) and counts builds on the daemon's own event stream:
/// every cook that runs the build one-shot starts one exit-0 container
/// FROM the cooked tag, so `container create` events carrying
/// `image == <tag>` count the builds that ran, whatever the tree under
/// test reports about itself.
///
/// Requires a reachable Docker daemon; the pin passes trivially without
/// one, as the conformance run does. Runs only on the fork feature
/// (`env-image-evict`): the cook path it drives is the fork's.
#[cfg(feature = "env-image-evict")]
mod storm {
    use std::collections::HashMap;
    use std::env::current_exe;
    use std::path::PathBuf;
    use std::process;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use bollard::Docker;
    use bollard::image::RemoveImageOptions;
    use bollard::system::EventsOptions;
    use futures_util::StreamExt;
    use sandbox_driver::SandboxProvider as _;
    use sandbox_driver_docker::DockerProvider;

    use crate::image_build::{EnvironmentBuild, ensure_environment_image};

    /// How many concurrent first cooks the storm drives (the incident:
    /// five gears, load 42; the decided pin floor is three).
    const LANES: usize = 4;
    /// The build's slow layer, widening the first-probe window so every
    /// lane probes before any mint — the storm the incident observed.
    const BUILD_SLEEP_SECS: u64 = 6;
    /// The child lane's environment: the context root, the Dockerfile
    /// text, and the result file to write. The cook mints its own
    /// holder name (work#1551); the lane names nothing.
    const CHILD_FIXTURE: &str = "PETRI_STORM_FIXTURE";
    const CHILD_DOCKERFILE: &str = "PETRI_STORM_DOCKERFILE";
    const CHILD_RESULT: &str = "PETRI_STORM_RESULT";

    fn read_dockerfile(fixture: &std::path::Path) -> String {
        std::fs::read_to_string(fixture.join("Dockerfile.toolchain"))
            .expect("the fixture's dockerfile reads back")
    }

    fn nonce() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the clock is past the epoch")
            .as_nanos()
    }

    /// One throwaway git checkout with a manifest and a source file: a
    /// fresh tip good enough to cook — a REAL committed repository, as
    /// `pack_context` clones it. The Dockerfile carries the run's nonce
    /// in a LABEL — Dockerfile text is a digest input in every digest
    /// mode, so each pin run mints its own tag and cannot collide with
    /// a leftover from an earlier run.
    fn storm_fixture(run: u128) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("petri-image-flight-storm-{run}-{}", process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).expect("create fixture dirs");
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"storm-pin\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .expect("write fixture manifest");
        std::fs::write(root.join("src/lib.rs"), "pub fn probe() {}\n")
            .expect("write fixture source");
        std::fs::write(root.join("src/only-source.txt"), "source-only content\n")
            .expect("write fixture source-only file");
        std::fs::write(
            root.join("Dockerfile.toolchain"),
            format!(
                "FROM busybox:1.36\n\
                 LABEL petri.storm-nonce=\"{run}\"\n\
                 RUN sleep {BUILD_SLEEP_SECS}\n\
                 COPY . /src\n"
            ),
        )
        .expect("write fixture dockerfile");
        let git = |args: &[&str]| {
            let status = process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args([
                    "-c",
                    "user.email=storm@example.invalid",
                    "-c",
                    "user.name=storm pin",
                ])
                .args(args)
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} must succeed: {status}");
        };
        git(&["init", "--quiet"]);
        git(&["add", "-A"]);
        git(&["commit", "--quiet", "-m", "storm fixture"]);
        root
    }

    /// One cook lane, run exactly as a gear's process runs it. Started by
    /// the parent through the test binary's own name; without the
    /// environment it is a no-op, so an ordinary test run never fires it.
    #[tokio::test(flavor = "multi_thread")]
    async fn storm_child_lane() {
        let (Ok(fixture), Ok(dockerfile), Ok(result)) = (
            std::env::var(CHILD_FIXTURE),
            std::env::var(CHILD_DOCKERFILE),
            std::env::var(CHILD_RESULT),
        ) else {
            return;
        };
        let provider: Arc<dyn sandbox_driver::SandboxProvider> = Arc::new(
            DockerProvider::connect()
                .await
                .expect("the parent only spawns lanes against a live daemon"),
        );
        let cooked = ensure_environment_image(&provider, &EnvironmentBuild {
            dockerfile,
            context: fixture,
        })
        .await;
        let outcome = cooked
            .as_ref()
            .ok()
            .and_then(|cooked| cooked.cook.as_ref())
            .map(|cook| match cook {
                ir::ImageCook::Built { .. } => "built",
                ir::ImageCook::Reused { .. } => "reused",
                ir::ImageCook::Waited { .. } => "waited",
                ir::ImageCook::Reclaimed { .. } => "reclaimed",
            });
        let report = match &cooked {
            Ok(cooked) => serde_json::json!({
                "tag": cooked.tag,
                "outcome": outcome,
            }),
            Err(error) => serde_json::json!({ "error": error.to_string() }),
        };
        std::fs::write(&result, report.to_string()).expect("write the lane's result");
        cooked.expect("the lane's cook must succeed");
    }

    /// Drives LANES concurrent first cooks of one fresh tip, one cook
    /// process per lane, then asserts the daemon saw exactly one build.
    /// RED against the probe-only tree (work#1463): every lane probes
    /// before the first mint, every lane builds, and the count is LANES,
    /// not 1 (the 2026-11-21 incident: five builds, load 42).
    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_first_cooks_of_one_tip_build_exactly_once() {
        if DockerProvider::connect().await.is_err() {
            return; // no daemon: trivially green, as the conformance run is
        }
        let Ok(docker) = Docker::connect_with_local_defaults() else {
            return;
        };
        let run = nonce();
        let fixture = storm_fixture(run);
        let scratch = std::env::temp_dir().join(format!(
            "petri-image-flight-storm-results-{run}-{}",
            process::id()
        ));
        std::fs::create_dir_all(&scratch).expect("create result dir");

        let since = unix_now();
        let mut lanes = Vec::new();
        for lane in 0..LANES {
            lanes.push(
                process::Command::new(current_exe().expect("the test binary"))
                    .args(["image_flight_tests::storm::storm_child_lane", "--exact"])
                    .env(CHILD_FIXTURE, fixture.to_string_lossy().as_ref())
                    .env(CHILD_DOCKERFILE, read_dockerfile(&fixture))
                    .env(
                        CHILD_RESULT,
                        scratch
                            .join(format!("lane-{lane}.json"))
                            .to_string_lossy()
                            .as_ref(),
                    )
                    .spawn()
                    .expect("spawn a cook lane"),
            );
        }
        for mut lane in lanes {
            let status = lane.wait().expect("wait for the cook lane");
            assert!(status.success(), "a cook lane failed: {status}");
        }
        let until = unix_now();

        let mut tags = Vec::new();
        let mut outcomes = Vec::new();
        for lane in 0..LANES {
            let report = std::fs::read_to_string(scratch.join(format!("lane-{lane}.json")))
                .expect("every lane writes its result");
            let report: serde_json::Value =
                serde_json::from_str(&report).expect("a lane's result is JSON");
            assert!(
                report["error"].is_null(),
                "lane {lane} failed its cook: {report}"
            );
            tags.push(
                report["tag"]
                    .as_str()
                    .expect("a lane names its tag")
                    .to_owned(),
            );
            outcomes.push(
                report["outcome"]
                    .as_str()
                    .expect("a lane names its cook outcome")
                    .to_owned(),
            );
        }
        let tag = tags[0].clone();
        assert!(
            tags.iter().all(|other| other == &tag),
            "one tip must mint one tag across lanes: {tags:?}"
        );
        // The cook's own account, secondary to the daemon's: exactly
        // one lane built, every other lane reused or waited.
        let built = outcomes
            .iter()
            .filter(|outcome| outcome.as_str() == "built" || outcome.as_str() == "reclaimed")
            .count();
        let reused = outcomes
            .iter()
            .filter(|outcome| outcome.as_str() == "reused" || outcome.as_str() == "waited")
            .count();
        assert_eq!(
            built + reused,
            LANES,
            "every lane must observe its cook: {outcomes:?}"
        );

        // The daemon's own account of how many builds ran: one exit-0
        // container from the tag per build one-shot.
        let builds = count_build_containers(&docker, &tag, since, until).await;

        let _ = std::fs::remove_dir_all(&scratch);
        let _ = std::fs::remove_dir_all(&fixture);
        let _ = docker
            .remove_container(
                &format!("petri-cook-lock-{tag}"),
                None::<bollard::container::RemoveContainerOptions>,
            )
            .await;
        let _ = docker
            .remove_image(
                &tag,
                Some(RemoveImageOptions {
                    force: true,
                    ..Default::default()
                }),
                None,
            )
            .await;

        assert_eq!(
            builds, 1,
            "exactly one build must serve {LANES} concurrent first cooks of one tip \
             (observed {builds} build containers; the 2026-11-21 storm class, work#1551)"
        );
        assert_eq!(
            built, 1,
            "exactly one lane may observe itself the builder: {outcomes:?}"
        );
    }

    /// Counts `container create` events whose image is `tag` between the
    /// two unix timestamps: the daemon-side build counter.
    async fn count_build_containers(docker: &Docker, tag: &str, since: i64, until: i64) -> usize {
        let mut filters = HashMap::new();
        filters.insert("type".to_owned(), vec!["container".to_owned()]);
        filters.insert("event".to_owned(), vec!["create".to_owned()]);
        let options = EventsOptions::<String> {
            since: Some(since.to_string()),
            until: Some(until.to_string()),
            filters,
        };
        let mut stream = docker.events(Some(options));
        let mut builds = 0;
        while let Some(event) = stream.next().await {
            let event = event.expect("the daemon's event stream must read");
            if event
                .actor
                .as_ref()
                .and_then(|actor| actor.attributes.as_ref())
                .and_then(|attributes| attributes.get("image"))
                .is_some_and(|image| image == tag)
            {
                builds += 1;
            }
        }
        builds
    }

    fn unix_now() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the clock is past the epoch")
            .as_secs() as i64
    }

    /// THE recipe-digest property pin (work#1551 decision 4): a
    /// source-only change to the checkout leaves the prepare output —
    /// and therefore the tag — IDENTICAL, while a manifest change
    /// moves it. This is the property that makes every later run of a
    /// source-only merged tip a probe hit with zero builds. Runs the
    /// real `cargo chef prepare` in the daemon's newest cooked
    /// environment image; skips when the daemon holds none (the
    /// bootstrap case, where the whole-tree digest arm governs) or no
    /// daemon answers.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_source_only_merge_prepares_the_same_recipe() {
        let Ok(provider) = DockerProvider::connect().await else {
            return;
        };
        let Ok(holder) = provider
            .create(
                &sandbox_driver::SandboxSpec::new(sandbox_driver::SandboxSource::Image {
                    reference: "busybox:1.36".to_owned(),
                })
                .name(format!("petri-recipe-pin-{}-env-build", nonce()))
                .working_directory("/workspace".to_owned()),
                None,
            )
            .await
        else {
            return;
        };

        let run = nonce();
        let fixture = storm_fixture(run);
        let dockerfile = read_dockerfile(&fixture);
        let first =
            match crate::image_build::newest_recipe(&holder, &fixture.to_string_lossy()).await {
                Ok(recipe) => recipe,
                Err(problem) => {
                    // No cooked image on this daemon (or no cargo
                    // manifests in the fixture's line): the bootstrap arm
                    // governs and this pin has nothing to observe.
                    tracing::debug!(%problem, "recipe pin skipped");
                    let _ = holder.stop().await;
                    let _ = holder.delete().await;
                    let _ = std::fs::remove_dir_all(&fixture);
                    return;
                }
            };

        // A source-only change: new file, same manifests.
        std::fs::write(fixture.join("src/more-source.rs"), "pub fn more() {}\n")
            .expect("write a source-only file");
        let source_only = crate::image_build::newest_recipe(&holder, &fixture.to_string_lossy())
            .await
            .expect("the source-only prepare runs");
        assert_eq!(
            source_only, first,
            "a source-only change must leave the recipe — and the tag — identical"
        );

        // A manifest change: the recipe moves, and so must the tag.
        std::fs::write(
            fixture.join("Cargo.toml"),
            "[package]\nname = \"storm-pin\"\nversion = \"0.0.1\"\nedition = \"2021\"\n",
        )
        .expect("bump the manifest");
        let manifest_moved = crate::image_build::newest_recipe(&holder, &fixture.to_string_lossy())
            .await
            .expect("the manifest-bumped prepare runs");
        assert_ne!(
            manifest_moved, first,
            "a manifest change must move the recipe"
        );
        assert_ne!(
            crate::image_flight::recipe_digest(&manifest_moved, &dockerfile),
            crate::image_flight::recipe_digest(&first, &dockerfile),
            "a moved recipe must mint a different tag"
        );

        let _ = holder.stop().await;
        let _ = holder.delete().await;
        let _ = std::fs::remove_dir_all(&fixture);
    }
}
