//! Fork feature (work#1440): the environment-image cook's
//! single-flight registry and in-process reuse ledger.
//!
//! `ensure_environment_image` deduplicates CONCURRENTLY through a
//! process-wide per-tag guard: the first caller of a tag builds, every
//! later caller of the same tag blocks on the guard and then reuses
//! the built image — through the in-process fast path when this
//! process cooked the tag, or through the tag-presence `reuse` arm of
//! the build one-shot otherwise. All gears cook in the same
//! fabro-server process, so an in-process lock is sufficient; no
//! cross-process coordination rides this.
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
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;
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
/// mtime — instead of the packed tar (work#1440 rider: the tar is
/// packed only when the cook must build). The skip rules mirror
/// `pack_context`'s: `.git` at every level, `.dockerignore` at the
/// root only.
///
/// Known approximation: the manifest walks the working tree while the
/// packed tar comes from a fresh clone of HEAD, so a DIRTY checkout
/// whose edits leave every path/size/mtime identical would hash to the
/// tag of the committed tree. Run checkouts are clean (the checkout
/// pipeline), and a divergence costs a stale-but-valid image, not a
/// broken one.
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
/// the digest is deterministic regardless of directory order.
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
            let mtime = metadata
                .modified()
                .map_err(io)?
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|since| since.as_nanos())
                .unwrap_or_default();
            hash.update(metadata.len().to_le_bytes().as_slice());
            hash.update(mtime.to_le_bytes().as_slice());
        }
    }
    Ok(())
}
