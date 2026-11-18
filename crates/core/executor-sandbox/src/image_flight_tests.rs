//! Fork-only presence and behavior pins for the environment cook's
//! single-flight machinery (work#1440). A `just vendor import` from a
//! wrong fork state rewrites `vendor/PINS.toml` cleanly and would
//! silently drop the feature; these tests keep it observable.

use std::path::PathBuf;
use std::sync::Arc;

use super::image_flight;

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
