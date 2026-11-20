//! Fork feature (work#1440): the environment-image cook's
//! single-flight registry and in-process reuse ledger.
//!
//! `ensure_environment_image` deduplicates CONCURRENTLY through a
//! process-wide per-tag guard: the first caller of a tag builds, every
//! later caller of the same tag blocks on the guard and then reuses
//! the built image — through the in-process fast path when this
//! process cooked the tag, or through the tag-presence `reuse` arm of
//! the build one-shot otherwise. The guard and the ledger are
//! PROCESS-LOCAL (work#1463): the cooks run one process per gear, so
//! they alone never dedupe across gears — the post-ledger-miss path
//! closes that gap by re-checking the daemon for the tag
//! (`image_build::cook_path_after_ledger_miss`, work#1463): a hit
//! reuses without a build, a miss builds.
//!
//! The mint-time CEILING is enforced daemon-side (the driver's
//! `evict_lru_images`, creation-time ordering): that bounds the
//! `petri-env` population even across process restarts, which no
//! in-process ledger can. The ledger here only serves the fast path —
//! a tag this process built needs no holder, no context pack, and no
//! build one-shot — and drops the references the daemon-side sweep
//! evicted, so a later cook of an evicted tag takes the full path and
//! the `reuse` arm re-adjudicates presence.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io::Read as _;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};
use std::{fs, io};

use sha2::{Digest as _, Sha256};
use tokio::sync::Mutex as AsyncMutex;

/// The reference prefix every cooked environment image carries; the
/// daemon-side eviction sweep keys on it.
pub(crate) const ENV_TAG_PREFIX: &str = "petri-env-";
/// How many newest cooked environment images survive mint-time
/// eviction (work#1440: "keep last ~6 or ~60 GB env-image bytes" — the
/// count form; each tip is a fresh image, so creation-time order and
/// use-time order agree in practice). Read only on the
/// `env-image-evict` feature; the fork-only presence test pins it in
/// every build.
#[cfg_attr(
    not(feature = "env-image-evict"),
    allow(
        dead_code,
        reason = "read only on the eviction feature; the presence test pins it"
    )
)]
pub(crate) const ENV_IMAGE_CEILING: usize = 6;

/// References of environment images cooked by THIS process: the
/// fast-path cache. Entries leave through `forget` when the
/// daemon-side sweep evicts them.
static BUILT: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// One async guard per tag: the single-flight seam. Guards accumulate
/// for the process's life — one mutex handle per cooked tag is a
/// bounded, deliberate cost.
static GUARDS: LazyLock<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The shared build guard for `tag`: concurrent first cooks of the
/// same tag serialize on it.
pub(crate) fn guard_for(tag: &str) -> Arc<AsyncMutex<()>> {
    let mut guards = GUARDS.lock().expect("env cook guard registry poisoned");
    guards.entry(tag.to_owned()).or_default().clone()
}

/// Whether THIS process cooked `tag` (and nothing evicted it since):
/// the cook may skip the holder lifecycle entirely.
pub(crate) fn is_built(tag: &str) -> bool {
    BUILT
        .lock()
        .expect("env cook ledger poisoned")
        .contains(tag)
}

/// Records `tag` as cooked by this process.
pub(crate) fn note_built(tag: &str) {
    BUILT
        .lock()
        .expect("env cook ledger poisoned")
        .insert(tag.to_owned());
}

/// Drops every reference in `tags` from the fast-path ledger — the
/// daemon-side sweep evicted them. Called only on the
/// `env-image-evict` feature; the fork-only presence test pins it in
/// every build.
#[cfg_attr(
    not(feature = "env-image-evict"),
    allow(
        dead_code,
        reason = "read only on the eviction feature; the presence test pins it"
    )
)]
pub(crate) fn forget(tags: &[String]) {
    let mut built = BUILT.lock().expect("env cook ledger poisoned");
    for tag in tags {
        built.remove(tag);
    }
}

/// Computes the environment's content digest over the Dockerfile text
/// and a MANIFEST of the context tree — each file's path, size, and
/// CONTENT (streamed in 64 KiB chunks) — instead of the packed tar
/// (work#1440 rider: the tar is packed only when the cook must build).
/// The skip rules mirror `pack_context`'s: `.git` at every level,
/// `.dockerignore` at the root only.
///
/// The manifest is CLONE-STABLE (work#1454): an earlier mtime-based
/// manifest minted a different `petri-env-<digest12>` tag per fresh
/// run checkout of the SAME trunk tip, so the tag-keyed single-flight
/// guard never grouped concurrent cooks — seven distinct tags for one
/// tip within thirteen minutes, four near-simultaneous duplicate
/// builds (fabro container logs, 2026-11-18). The price of content is
/// a full READ of the context tree on every cook: ~250 MB for the
/// fabro checkout, a couple of seconds, paid once per tip change —
/// cheap against a duplicate multi-gigabyte image build.
///
/// Rejected alternative (work#1454): `git ls-files -s` blob shas are
/// faster, but they index the STAGED state, so unstaged working-tree
/// edits would diverge from the packed tar.
///
/// A dirty working tree now hashes to its TRUE content — the old
/// "known approximation" (a dirty checkout aliasing the committed
/// tree whenever its edits left every path/size/mtime identical) is
/// gone. The digest walks the working tree while `pack_context` tars
/// a fresh clone of HEAD, so digest and tar agree exactly for a clean
/// checkout — run checkouts are clean (the checkout pipeline) — and a
/// dirty tree now mints its own tag rather than silently reusing the
/// committed tree's image.
pub(crate) fn manifest_digest(dockerfile: &str, root: &Path) -> Result<String, String> {
    if !root.join(".git").exists() {
        return Err(format!(
            "{} carries no .git — the build context must be a git checkout",
            root.display()
        ));
    }
    let mut hash = Sha256::new();
    hash.update(dockerfile.as_bytes());
    append_manifest(&mut hash, root, "")?;
    let digest = hash.finalize();
    let mut hex = String::with_capacity(12);
    for byte in &digest[..6] {
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

/// Appends `dir`'s manifest entries under `prefix`, sorted by name so
/// the digest is deterministic regardless of directory order. Each
/// regular file contributes path, size, and STREAMED CONTENT — the
/// content read is what makes the digest clone-stable (work#1454).
fn append_manifest(hash: &mut Sha256, dir: &Path, prefix: &str) -> Result<(), String> {
    let io = |error: io::Error| {
        format!(
            "walking the build context at {} failed: {error}",
            dir.display()
        )
    };
    let mut names: Vec<String> = fs::read_dir(dir)
        .map_err(io)?
        .filter_map(|entry| {
            entry
                .ok()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
        })
        .collect();
    names.sort();
    for name in names {
        if name == ".git" || (name == ".dockerignore" && prefix.is_empty()) {
            continue;
        }
        let path = dir.join(&name);
        let arc = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let metadata = fs::metadata(&path).map_err(io)?;
        hash.update(arc.as_bytes());
        if metadata.is_dir() {
            append_manifest(hash, &path, &arc)?;
        } else {
            hash.update(metadata.len().to_le_bytes().as_slice());
            let mut file = fs::File::open(&path).map_err(io)?;
            let mut chunk = [0u8; 64 * 1024];
            loop {
                let read = file.read(&mut chunk).map_err(io)?;
                if read == 0 {
                    break;
                }
                hash.update(&chunk[..read]);
            }
        }
    }
    Ok(())
}
