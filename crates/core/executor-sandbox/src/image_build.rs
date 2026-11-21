//! The environment image build (ADR-0042, the fabro plan
//! 2026-11-09-run-environment-image-build): an environment may declare
//! a Dockerfile; the engine builds that image at scope acquisition,
//! BEFORE the scope's sandbox is created, and the scope runs on the
//! built tag.
//!
//! Why a holder sandbox: the docker provider's image build is a
//! ONE-SHOT capability — it reads the build context out of an existing
//! sandbox's filesystem (the daemon archive API) and builds daemon-side
//! (`sandbox-driver-docker/src/one_shot.rs`). A scope's own image
//! cannot be built inside itself, so the build rides a THROWAWAY
//! holder: create (busybox), upload the packed context, extract it
//! with a one-shot, build with a one-shot (`reuse` short-circuits on a
//! tag hit), then stop and delete the holder. Everything uses driver
//! capabilities that already exist; no protocol change rides this.
//!
//! The tag is the cache: `petri-env-<hash12>` over the COOK INPUTS —
//! the `cargo chef prepare` recipe for the context plus the Dockerfile
//! text and its base image reference (work#1551) — so an unchanged
//! dependency set costs one prepare walk (seconds, no compile) and a
//! SOURCE-ONLY merge reuses the prior image outright. Concurrent
//! cooks, one process per gear (work#1463), serialize through a
//! daemon-side CAS lock (work#1551): a container CREATE under the
//! fixed name `petri-cook-lock-<tag>` is atomic on the daemon, so the
//! first cook builds and every other cook WAITS on the canonical tag
//! and reuses it at the mint — the check-then-act presence probe
//! alone let five simultaneous first cooks all build (2026-11-21,
//! load 42). A waiter that outlives the bound fails CLOSED (the run
//! parks); a lock whose winner died is reclaimed after a grace, so a
//! crashed cook never wedges the fleet's fresh-tip runs. After every
//! successful cook a mint-time eviction caps the `petri-env`
//! population at the newest few images, so concurrent gears no longer
//! mint one full layer set each and fill the Docker disk.
//!
//! The CAS lock owns serialization; the in-process guard/ledger of
//! work#1440 survives only as the probe-only arm's cache (see
//! `image_flight`'s docs). Presence is answered ONCE, at the depth
//! that owns it — the daemon's image set, through the one-shot facet —
//! with the build one-shot's own `reuse` short-circuit as a
//! daemon-side belt-and-suspenders beneath it.
//!
//! KNOWN FOLLOW-UPS (named here, not fixed here — scope discipline):
//! the `env-image-evict` feature now gates the eviction sweep, the
//! presence probe, the recipe digest, AND the cook lock, and its name
//! predates three of those; and the `petri-env-` tag prefix is shared
//! knowledge between this cook and the driver's eviction facet.
//!
//! Known residuals (accepted): the build one-shot runs a trivial
//! `/bin/sh -c 'exit 0'` from the freshly built tag, so an environment
//! image without a shell cannot be built this way; and the in-process
//! fast path trusts the ledger — an out-of-band `docker rmi` between
//! cook and use surfaces as an image-missing sandbox create, not as a
//! rebuild (the daemon-side sweep keeps its evictions in sync through
//! `forget`). The whole-tree digest arm (the bootstrap fallback) and
//! the recipe arm never alias one tag (separate domain separators),
//! so a daemon's first-ever cook pays one rebuild when the fleet moves
//! onto recipe digests — once per daemon lifetime.

use std::path::Path;
use std::process::{Command as StdCommand, id};
use std::sync::Arc;
use std::thread::Builder as ThreadBuilder;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, fs, io};

use ir::ImageCook;
use sandbox_driver::{
    Error as DriverError, ExecControls, OneShotImage, OneShotSpec, Sandbox, SandboxProvider,
    SandboxSource, SandboxSpec,
};
use tokio::runtime::Builder as RuntimeBuilder;
use tokio::task::spawn_blocking;

#[cfg(feature = "env-image-evict")]
use crate::cook_lock;
use crate::{EnvError, image_flight};

/// The holder's base: the smallest image that can run `tar` and `sh`.
/// The daemon pulls it once; it exists only to hold the context.
pub(crate) const HOLDER_IMAGE: &str = "busybox:1.36";
/// Where the packed context lands inside the holder, and where it is
/// extracted to. Both live UNDER THE HOLDER'S WORKSPACE: a one-shot
/// container sees the holder's filesystem only through the workspace
/// mount, so the extract one-shot finds the tar exactly there, and the
/// build's context read (which resolves against the holder) sees the
/// extracted tree at the same path.
const CONTEXT_TAR: &str = "/workspace/petri-env-context.tar";
const CONTEXT_DIR: &str = "/workspace/petri-env-context";
/// The recipe prepare's mini-context and output, under the holder's
/// workspace the same way (work#1551): the prepare one-shot reads the
/// manifests from the extracted mini-context and writes the recipe
/// beside them, where the holder's filesystem facet reads it back.
#[cfg_attr(
    not(feature = "env-image-evict"),
    allow(
        dead_code,
        reason = "recipe-digest machinery; the fork-only pins exercise it in tests"
    )
)]
const RECIPE_TAR: &str = "/workspace/petri-env-recipe-context.tar";
#[cfg_attr(
    not(feature = "env-image-evict"),
    allow(
        dead_code,
        reason = "recipe-digest machinery; the fork-only pins exercise it in tests"
    )
)]
const RECIPE_DIR: &str = "/workspace/petri-env-recipe-context";
#[cfg_attr(
    not(feature = "env-image-evict"),
    allow(
        dead_code,
        reason = "recipe-digest machinery; the fork-only pins exercise it in tests"
    )
)]
const RECIPE_JSON: &str = "/workspace/petri-env-recipe.json";
/// How long the prepare one-shot may run: parsing manifests is
/// seconds, but a large workspace's metadata can take its time on a
/// loaded daemon — minutes of budget, never hours.
#[cfg_attr(
    not(feature = "env-image-evict"),
    allow(
        dead_code,
        reason = "recipe-digest machinery; the fork-only pins exercise it in tests"
    )
)]
const RECIPE_BUDGET: Duration = Duration::from_secs(300);
/// The Dockerfile's name inside the context (the build reads
/// `context/Dockerfile`).
const DOCKERFILE_NAME: &str = "Dockerfile";
/// The label that marks the holder as build machinery, never a run
/// workspace a lease reconcile could mistake for a lost sandbox.
pub(crate) const ROLE_LABEL: &str = "petri.role";
pub(crate) const ROLE_VALUE: &str = "env-build";

/// What an environment's build declaration carries: the inline
/// Dockerfile text and the host-side context root (the run's bound
/// repository).
#[derive(Clone, Debug)]
pub(crate) struct EnvironmentBuild {
    pub dockerfile: String,
    pub context:    String,
}

/// What ensuring the environment image produced: the tag the scope
/// must run on, and how the cook fared. The outcome rides the scope's
/// `scope.acquired` record (`SandboxInstance::image_cook`,
/// work#1551 decision 5) — the engine-side record vocabulary already
/// carries scope acquisition, so the cook's outcome reaches the run's
/// stream without a host-side record and without shell access to the
/// host. `None` on the feature-off path (no fork facets, no observed
/// provenance) and for scopes that never cook.
#[derive(Clone, Debug)]
pub(crate) struct CookedEnvironment {
    pub tag:  String,
    pub cook: Option<ImageCook>,
}

impl CookedEnvironment {
    /// A cook whose provenance the executor cannot observe (the
    /// feature-off path).
    fn opaque(tag: String) -> Self {
        Self { tag, cook: None }
    }
}

/// Ensures the environment image exists on the daemon and returns the
/// tag the scope must run on. A failure fails the scope's acquisition
/// (a routable error; the run records `scope.failed`).
///
/// On the fork feature (`env-image-evict`) the cook is SERIALIZED
/// daemon-side (work#1551): the tag is the RECIPE digest, and the
/// build right is a container-name CAS lock — one winner builds, the
/// rest wait and reuse. Off the feature (upstream sandbox-driver, no
/// cook facets) the cook keeps the probe-only posture (work#1463):
/// single-flight per tag in-process, then the daemon presence probe,
/// then the build.
pub(crate) async fn ensure_environment_image(
    provider: &Arc<dyn SandboxProvider>,
    build: &EnvironmentBuild,
) -> Result<CookedEnvironment, EnvError> {
    #[cfg(feature = "env-image-evict")]
    {
        ensure_serialized(provider, build).await
    }
    #[cfg(not(feature = "env-image-evict"))]
    {
        ensure_probe_only(provider, build).await
    }
}

/// One holder name per cook, minted inside the cook: unique across
/// every cook of a tag (the daemon refuses duplicate container names),
/// still recognizably machinery by its `petri.role=env-build` label
/// when an operator sweeps.
fn minted_holder_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or_default();
    format!("petri-env-cook-{}-{nanos}", id())
}

/// The serialized cook (work#1551). Every step rides the holder — the
/// probe, the recipe prepare, and (for the winner) the build itself —
/// and the holder rides the cancellation guard, so an aborted cook
/// reaps its machinery instead of leaking it (the two-orphaned-holders
/// incident). The cook mints its holder's name itself: the caller
/// names nothing, the name is unique per cook (the daemon refuses
/// duplicate container names, and every cook of one tag — same process
/// or not — needs its own holder), and the winner's name rides the
/// lock's labels for the waiters' liveness lookups.
#[cfg(feature = "env-image-evict")]
async fn ensure_serialized(
    provider: &Arc<dyn SandboxProvider>,
    build: &EnvironmentBuild,
) -> Result<CookedEnvironment, EnvError> {
    let dockerfile = build.dockerfile.clone();
    let holder_name = minted_holder_name();
    let holder = create_holder(provider, &holder_name)
        .await
        .map_err(|error| backend_error(format!("creating the build holder failed: {error}")))?;
    let mut cleanup = CookCleanup::arm(holder.clone());

    // The cook-input tag (decision 4): the recipe digest when a cooked
    // environment image exists on the daemon to prepare in, the
    // whole-tree digest otherwise.
    let hex = environment_tag(&holder, &dockerfile, &build.context).await?;
    let tag = format!("{}{hex}", image_flight::ENV_TAG_PREFIX);

    let one_shot = holder
        .one_shot()
        .ok_or_else(|| backend_error("the holder lost its one-shot capability".to_owned()))?;
    // Presence, once, at the depth that owns it: the daemon's image
    // set. A hit reuses with no lock cycle at all — the common
    // sequential case never touches the CAS.
    if probe_present(one_shot, &tag).await {
        tracing::info!(%tag, "env image cook: the daemon already holds the tag — reusing");
        retire(&mut cleanup, &tag).await;
        let cook = ImageCook::Reused {
            tag: tag.clone().into(),
        };
        return Ok(CookedEnvironment {
            tag,
            cook: Some(cook),
        });
    }

    // The CAS: one winner builds per tag across every cook process on
    // the daemon. A loser waits for the winner's mint — or, when the
    // winner died without cleanup, reclaims the lock after the grace
    // and becomes the winner itself (decision 3).
    let mut took_over = false;
    loop {
        match cook_lock::try_acquire(provider, &tag, &holder_name).await {
            Ok(Some(lock)) => {
                cleanup.hold(lock.clone());
                // The mint-in-window race: the winner may have minted
                // and released between the probe above and the CAS.
                if probe_present(one_shot, &tag).await {
                    release_lock(&lock, &tag).await;
                    retire(&mut cleanup, &tag).await;
                    let cook = ImageCook::Reused {
                        tag: tag.clone().into(),
                    };
                    return Ok(CookedEnvironment {
                        tag,
                        cook: Some(cook),
                    });
                }
                tracing::info!(%tag, "env image cook: lock acquired — building");
                let outcome = build_on_holder(&holder, &dockerfile, &build.context, &tag).await;
                if outcome.is_ok() {
                    // Mint-time eviction (work#1440): rides the SAME
                    // holder — it is the only driver seam the cook
                    // holds — so it must run before the holder goes
                    // away.
                    evict_beyond_ceiling(&holder).await;
                }
                release_lock(&lock, &tag).await;
                retire(&mut cleanup, &tag).await;
                outcome?;
                let cook = if took_over {
                    ImageCook::Reclaimed {
                        tag: tag.clone().into(),
                    }
                } else {
                    ImageCook::Built {
                        tag: tag.clone().into(),
                    }
                };
                return Ok(CookedEnvironment {
                    tag,
                    cook: Some(cook),
                });
            }
            Ok(None) => {
                tracing::info!(%tag, "env image cook: another process holds the lock — waiting for its mint");
                match wait_for_mint(provider, one_shot, &tag).await {
                    Ok(MintWait::Minted) => {
                        retire(&mut cleanup, &tag).await;
                        let cook = ImageCook::Waited {
                            tag: tag.clone().into(),
                        };
                        return Ok(CookedEnvironment {
                            tag,
                            cook: Some(cook),
                        });
                    }
                    Ok(MintWait::LockAvailable) => {
                        tracing::info!(%tag, "env image cook: the lock was stale — taking it over");
                        took_over = true;
                    }
                    Err(error) => {
                        retire(&mut cleanup, &tag).await;
                        return Err(error);
                    }
                }
            }
            Err(error) => {
                let problem = backend_error(format!("acquiring the cook lock failed: {error}"));
                retire(&mut cleanup, &tag).await;
                return Err(problem);
            }
        }
    }
}

/// The wait side of the CAS (decision 2): poll the canonical tag every
/// [`cook_lock::WAIT_POLL`] until it mints — then reuse — or until
/// [`cook_lock::WAIT_BOUND`] passes, which fails CLOSED: the cook
/// returns a routable error and the run parks; it never fails open to
/// building. Between polls the lock is re-observed (decision 3): a
/// lock gone, dead, or orphaned by a dead winner past the grace is
/// taken over — [`MintWait::LockAvailable`] sends the caller back to
/// the CAS as the new winner.
#[cfg(feature = "env-image-evict")]
async fn wait_for_mint(
    provider: &Arc<dyn SandboxProvider>,
    one_shot: &dyn sandbox_driver::OneShot,
    tag: &str,
) -> Result<MintWait, EnvError> {
    let started = tokio::time::Instant::now();
    let mut stale_since: Option<tokio::time::Instant> = None;
    loop {
        if probe_present(one_shot, tag).await {
            return Ok(MintWait::Minted);
        }
        let lock = match cook_lock::observe(provider, tag).await {
            Ok(lock) => lock,
            Err(problem) => {
                // An observation failure is an anomaly (WARN is the
                // channel for those), never a reclaim license — and
                // never a reason to outlive the bound either.
                tracing::warn!(%tag, %problem, "env image cook: the lock could not be observed");
                if started.elapsed() >= cook_lock::WAIT_BOUND {
                    return Err(park_error(tag, started.elapsed()));
                }
                tokio::time::sleep(cook_lock::WAIT_POLL).await;
                continue;
            }
        };
        let winner_alive = match &lock {
            cook_lock::LockSight::Alive {
                holder: Some(holder),
                ..
            } => match cook_lock::winner_holder_alive(provider, holder).await {
                Ok(alive) => Some(alive),
                Err(problem) => {
                    // Undeterminable is NOT dead: reading a live winner
                    // out of a transient listing error would take over
                    // a lock whose cook is running. Keep waiting.
                    tracing::warn!(
                        %tag,
                        %problem,
                        "env image cook: the winner's holder could not be observed"
                    );
                    None
                }
            },
            _ => None,
        };
        let stale_for = stale_since.map(|since| since.elapsed());
        let step = cook_lock::waiter_step(false, &lock, winner_alive, stale_for, started.elapsed());
        match step {
            cook_lock::WaiterStep::Reuse => return Ok(MintWait::Minted),
            cook_lock::WaiterStep::Park => {
                return Err(park_error(tag, started.elapsed()));
            }
            cook_lock::WaiterStep::TakeOver { id } => {
                if let Some(id) = id
                    && let Err(problem) = provider
                        .delete(
                            &sandbox_driver::SandboxId::try_new(id.as_str()).map_err(|error| {
                                backend_error(format!(
                                    "the stale cook lock's id did not parse: {error}"
                                ))
                            })?,
                            None,
                        )
                        .await
                {
                    // The corpse may have vanished between observation
                    // and deletion — benign; anything else is an
                    // anomaly worth a line, but never a reason to stop
                    // reclaiming.
                    tracing::warn!(%tag, %problem, "env image cook: deleting a stale lock failed");
                }
                // The winner may have minted and cleaned up while the
                // grace ran: re-probe before spending the CAS.
                if probe_present(one_shot, tag).await {
                    return Ok(MintWait::Minted);
                }
                return Ok(MintWait::LockAvailable);
            }
            cook_lock::WaiterStep::KeepWaiting => {
                if cook_lock::stale(&lock, winner_alive) {
                    stale_since.get_or_insert_with(tokio::time::Instant::now);
                } else {
                    stale_since = None;
                }
                tokio::time::sleep(cook_lock::WAIT_POLL).await;
            }
        }
    }
}

/// The fail-closed wait outcome (decision 2): a routable error that
/// PARKS the run — the scope's acquisition fails, every firing in the
/// scope fails routably, and nothing builds concurrently.
#[cfg(feature = "env-image-evict")]
fn park_error(tag: &str, waited: Duration) -> EnvError {
    tracing::warn!(%tag, waited_secs = waited.as_secs(), "env image cook: the wait outlived its bound — parking the run");
    backend_error(format!(
        "the environment cook for {tag} waited {}s for the tag to mint under the cook lock {} \
         and no winner finished; the run parks rather than build concurrently — check the \
         docker host for a stuck cook (machinery labeled petri.role=env-build / \
         env-cook-lock) and re-fire",
        waited.as_secs(),
        cook_lock::lock_name(tag),
    ))
}

/// How a wait ended.
#[cfg(feature = "env-image-evict")]
enum MintWait {
    /// The tag minted: reuse it.
    Minted,
    /// The lock went stale and the waiter took it over: re-run the CAS.
    LockAvailable,
}

/// The daemon presence probe, mapped for the wait loop: a hit is true,
/// a miss is false, and a probe FAILURE is an anomaly (WARN) read as a
/// miss — the waiter keeps waiting and the bound still closes the
/// wait; building on a probe failure would re-open the storm class.
#[cfg(feature = "env-image-evict")]
async fn probe_present(one_shot: &dyn sandbox_driver::OneShot, tag: &str) -> bool {
    match one_shot.image_present(tag).await {
        Ok(present) => present,
        Err(problem) => {
            tracing::warn!(%tag, %problem, "env image cook: daemon presence probe unavailable");
            false
        }
    }
}

/// The cook's cancellation guard (work#1551's holder-lifecycle rider):
/// a run deleted mid-cook aborts the scope acquisition — the driver
/// drops the acquire task — and a dropped future runs no cleanup code,
/// so the holder (and the winner's lock) leaked; the incident left two
/// orphaned holders behind. The guard's Drop REAPS the machinery on a
/// private runtime thread, which works from any drop point: abort,
/// panic, or runtime teardown. A process that dies outright still
/// leaks daemon-side — waiters reclaim a dead winner's lock, and the
/// operator sweeps machinery by label (the standing posture).
#[cfg_attr(
    not(feature = "env-image-evict"),
    allow(
        dead_code,
        reason = "the serialized cook's cancellation guard; pinned in tests"
    )
)]
struct CookCleanup {
    holder: Arc<dyn Sandbox>,
    lock:   Option<Arc<dyn Sandbox>>,
    armed:  bool,
}

#[cfg_attr(
    not(feature = "env-image-evict"),
    allow(
        dead_code,
        reason = "the serialized cook's cancellation guard; pinned in tests"
    )
)]
impl CookCleanup {
    fn arm(holder: Arc<dyn Sandbox>) -> Self {
        Self {
            holder,
            lock: None,
            armed: true,
        }
    }

    /// The cook won the CAS: the lock retires with the holder.
    fn hold(&mut self, lock: Arc<dyn Sandbox>) {
        self.lock = Some(lock);
    }

    /// The cook finished and retired its own machinery: stand down.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CookCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let holder = self.holder.clone();
        let lock = self.lock.take();
        tracing::warn!("env image cook: cancelled mid-cook — reaping the holder and the lock");
        // A private thread and runtime: the guard must reap from ANY
        // drop point, including one the cook's own runtime is tearing
        // down.
        let reaped = ThreadBuilder::new()
            .name("petri-env-cook-reap".to_owned())
            .spawn(move || {
                let Ok(runtime) = RuntimeBuilder::new_current_thread().enable_all().build() else {
                    return;
                };
                runtime.block_on(async move {
                    if let Some(lock) = lock {
                        let _ = lock.stop().await;
                        let _ = lock.delete().await;
                    }
                    let _ = holder.stop().await;
                    let _ = holder.delete().await;
                });
            });
        if reaped.is_err() {
            tracing::warn!("env image cook: spawning the reap thread failed");
        }
    }
}

/// Retires the holder and stands the guard down — the normal end of a
/// cook, whatever the cook's result.
#[cfg_attr(
    not(feature = "env-image-evict"),
    allow(
        dead_code,
        reason = "the serialized cook's retirement; pinned in tests"
    )
)]
async fn retire(cleanup: &mut CookCleanup, tag: &str) {
    cleanup.disarm();
    retire_holder(&cleanup.holder, tag).await;
}

/// Releases the cook lock: the winner's build right ends with the
/// build, whatever its result, so waiters never wait on a lock whose
/// cook is done. Best-effort — the liveness reclaim covers a lock that
/// survives a failed release.
#[cfg(feature = "env-image-evict")]
async fn release_lock(lock: &Arc<dyn Sandbox>, tag: &str) {
    if let Err(problem) = lock.stop().await {
        tracing::warn!(%tag, %problem, "env image cook: stopping the lock failed");
    }
    if let Err(problem) = lock.delete().await {
        tracing::warn!(%tag, %problem, "env image cook: deleting the lock failed");
    }
}

/// The probe-only cook (work#1463, unchanged by work#1551): the
/// feature-off path against upstream sandbox-driver, whose OneShot has
/// no cook facets. Kept as its own arm so the fork's serialized cook
/// never has to compile without the facets it calls.
#[cfg_attr(
    feature = "env-image-evict",
    allow(
        dead_code,
        reason = "the serialized cook replaces it on the fork feature; kept compiling so the \
                  probe-only arm and its pins stay honest against upstream"
    )
)]
async fn ensure_probe_only(
    provider: &Arc<dyn SandboxProvider>,
    build: &EnvironmentBuild,
) -> Result<CookedEnvironment, EnvError> {
    let dockerfile = build.dockerfile.clone();
    let context = build.context.clone();
    // The digest walks the tree, not the tar: cheap enough to pay on
    // every cook, and the tar is packed only on a manifest miss.
    let digest_context = context.clone();
    let digest_dockerfile = dockerfile.clone();
    let hex = spawn_blocking(move || {
        image_flight::manifest_digest(&digest_dockerfile, Path::new(&digest_context))
    })
    .await
    .map_err(|error| backend_error(format!("the manifest digest task failed: {error}")))?
    .map_err(|message| backend_error(format!("hashing the build context failed: {message}")))?;
    let tag = format!("{}{hex}", image_flight::ENV_TAG_PREFIX);
    let holder_name = minted_holder_name();

    // The legacy single-flight seam of the probe-only arm (work#1440):
    // an in-process CACHE, not the serialization — the daemon probe
    // below is what dedupes across processes on this arm.
    let guard = image_flight::guard_for(&tag);
    let _single_flight = guard.lock().await;
    // A tag THIS process already cooked needs no holder, no pack, no
    // build one-shot — the ledger dropped it if the sweep evicted it.
    if image_flight::is_built(&tag) {
        tracing::debug!(%tag, "env image cook: in-process fast path");
        return Ok(CookedEnvironment::opaque(tag));
    }

    let holder = create_holder(provider, &holder_name)
        .await
        .map_err(|error| backend_error(format!("creating the build holder failed: {error}")))?;
    // Cross-process presence re-check (work#1463): another gear's
    // process may already have cooked this tag — the daemon is the
    // shared truth. A hit skips the pack and the build entirely.
    #[cfg(feature = "env-image-evict")]
    {
        let one_shot = holder
            .one_shot()
            .ok_or_else(|| backend_error("the holder lost its one-shot capability".to_owned()))?;
        if cook_path_after_ledger_miss(one_shot, &tag).await == CookPath::Reuse {
            retire_holder(&holder, &tag).await;
            return Ok(CookedEnvironment::opaque(tag));
        }
    }
    // Feature-off builds (upstream sandbox-driver, no image_present)
    // and probe failures fall through to the build path.

    let context_tar = spawn_blocking(move || pack_context(Path::new(&context)))
        .await
        .map_err(|error| backend_error(format!("the context pack task failed: {error}")))?
        .map_err(|message| backend_error(format!("packing the build context failed: {message}")))?;

    let outcome = build_on_holder_tar(&holder, &dockerfile, &context_tar, &tag).await;
    if outcome.is_ok() {
        image_flight::note_built(&tag);
        // Mint-time eviction (work#1440): rides the SAME holder — it is
        // the only driver seam the cook holds — so it must run before
        // the holder goes away. Gated on the fork feature: upstream
        // sandbox-driver has no `evict_lru_images`.
        #[cfg(feature = "env-image-evict")]
        evict_beyond_ceiling(&holder).await;
    }
    // The holder is machinery: it never outlives the attempt, whatever
    // the attempt's result.
    retire_holder(&holder, &tag).await;
    outcome?;
    Ok(CookedEnvironment::opaque(tag))
}

/// Stops and deletes the holder — machinery that never outlives the
/// cook attempt, whatever the attempt's result.
async fn retire_holder(holder: &Arc<dyn Sandbox>, tag: &str) {
    if let Err(problem) = holder.stop().await {
        tracing::warn!(%tag, %problem, "env image build: stopping the holder failed");
    }
    if let Err(problem) = holder.delete().await {
        tracing::warn!(%tag, %problem, "env image build: deleting the holder failed");
    }
}

/// The path a post-ledger-miss cook takes after the daemon presence
/// probe (work#1463).
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(
    not(feature = "env-image-evict"),
    allow(
        dead_code,
        reason = "driven only through the fork probe; the pins exercise it in tests"
    )
)]
pub(crate) enum CookPath {
    /// The daemon already holds the tag — reuse it, no build.
    Reuse,
    /// Build the image.
    Build,
}

/// The cross-process single-flight DECISION (work#1463), pure over the
/// probe's outcome so the fork-only pins can drive it without a
/// daemon: after the in-process ledger misses, the daemon is
/// re-checked for the tag. A presence hit means another process's
/// cook already minted it — the tag joins the fast-path ledger and
/// the cook reuses it with no pack and no build. A probe failure
/// (including providers without the facet) fails OPEN to `Build`: the
/// worst case is a duplicate build whose layers the daemon dedupes,
/// never a stuck cook.
#[cfg_attr(
    not(feature = "env-image-evict"),
    allow(
        dead_code,
        reason = "driven only through the fork probe; the pins exercise it in tests"
    )
)]
pub(crate) fn cook_path(probe: Result<bool, DriverError>, tag: &str) -> CookPath {
    match probe {
        Ok(true) => {
            tracing::info!(%tag, "env image cook: the daemon already holds the tag — reusing");
            image_flight::note_built(tag);
            CookPath::Reuse
        }
        Ok(false) => CookPath::Build,
        Err(problem) => {
            tracing::warn!(
                %tag,
                %problem,
                "env image cook: daemon presence probe unavailable — building"
            );
            CookPath::Build
        }
    }
}

/// Runs the daemon presence probe and maps its outcome through
/// [`cook_path`]. Gated on the fork feature exactly like the eviction
/// sweep: upstream sandbox-driver has no `OneShot::image_present`.
#[cfg(feature = "env-image-evict")]
async fn cook_path_after_ledger_miss(
    one_shot: &dyn sandbox_driver::OneShot,
    tag: &str,
) -> CookPath {
    cook_path(one_shot.image_present(tag).await, tag)
}

/// Caps the daemon's `petri-env` population at the newest
/// [`image_flight::ENV_IMAGE_CEILING`] images after a successful cook.
/// Eviction is a backstop the cook must survive: unsupported providers
/// and daemon errors are warnings, and the evicted references leave
/// the in-process fast path so a later cook re-adjudicates presence.
#[cfg(feature = "env-image-evict")]
async fn evict_beyond_ceiling(holder: &Arc<dyn Sandbox>) {
    let Some(one_shot) = holder.one_shot() else {
        return;
    };
    match one_shot
        .evict_lru_images(
            image_flight::ENV_TAG_PREFIX,
            image_flight::ENV_IMAGE_CEILING,
        )
        .await
    {
        Ok(evicted) if evicted.is_empty() => {}
        Ok(evicted) => {
            tracing::info!(
                count = evicted.len(),
                ?evicted,
                "env image cook: mint-time eviction"
            );
            image_flight::forget(&evicted);
        }
        Err(problem) => tracing::warn!(
            %problem,
            "env image cook: mint-time eviction unavailable"
        ),
    }
}

/// Creates the holder sandbox. Its labels mark it as build machinery;
/// nothing about it matches a run workspace label, so lease
/// reconciliation never adopts it.
async fn create_holder(
    provider: &Arc<dyn SandboxProvider>,
    name: &str,
) -> Result<Arc<dyn Sandbox>, DriverError> {
    let mut spec = SandboxSpec::new(SandboxSource::Image {
        reference: HOLDER_IMAGE.to_owned(),
    })
    .name(name.to_owned())
    .working_directory("/workspace".to_owned());
    spec.labels
        .insert(ROLE_LABEL.to_owned(), ROLE_VALUE.to_owned());
    let holder = provider.create(&spec, None).await?;
    if holder.one_shot().is_none() {
        // Free the just-created sandbox before refusing: an unusable
        // holder must not leak as a running container.
        let _ = holder.stop().await;
        let _ = holder.delete().await;
        return Err(DriverError::unsupported(
            sandbox_driver::Capability::OneShot,
        ));
    }
    Ok(holder)
}

/// The cook's tag over the COOK INPUTS (decision 4): the recipe digest
/// when the daemon holds a cooked environment image to prepare in —
/// busybox (the holder) has no cargo, while every `petri-env-*` image
/// carries cargo and cargo-chef (circular but valid: prepare only
/// parses manifests) — and the whole-tree digest otherwise: the
/// bootstrap (a daemon with no cooked image) and the non-cargo
/// context. Both arms are INFO-grade facts: the fallback is a designed
/// path, not an anomaly.
#[cfg(feature = "env-image-evict")]
async fn environment_tag(
    holder: &Arc<dyn Sandbox>,
    dockerfile: &str,
    context: &str,
) -> Result<String, EnvError> {
    match newest_recipe(holder, context).await {
        Ok(recipe) => {
            let hex = image_flight::recipe_digest(&recipe, dockerfile);
            tracing::debug!(%hex, "env image cook: recipe digest");
            Ok(hex)
        }
        Err(problem) => {
            tracing::info!(
                %problem,
                "env image cook: no recipe digest available — the whole-tree digest covers this cook"
            );
            let context = context.to_owned();
            let dockerfile = dockerfile.to_owned();
            spawn_blocking(move || image_flight::manifest_digest(&dockerfile, Path::new(&context)))
                .await
                .map_err(|error| {
                    backend_error(format!("the manifest digest task failed: {error}"))
                })?
                .map_err(|message| {
                    backend_error(format!("hashing the build context failed: {message}"))
                })
        }
    }
}

/// Runs `cargo chef prepare` for the context in the NEWEST cooked
/// environment image and returns its recipe.json bytes — the cook
/// inputs the digest hashes (decision 4). The prepare rides the holder
/// like every other cook step: a manifests-only MINI-CONTEXT is
/// uploaded and extracted there (kilobytes — every Cargo manifest at
/// its path, the lock files, the cargo configuration; chef
/// dummy-replaces sources, so a prepare over the manifests alone
/// yields the recipe a prepare over the full tree would), and the
/// one-shot runs in the newest `petri-env-*` image over it.
///
/// An error is the CALLER's fallback signal, never a cook failure: no
/// cooked image on the daemon, a context without cargo manifests, a
/// prepare that cannot parse — the whole-tree digest covers all of
/// them.
#[cfg(feature = "env-image-evict")]
pub(crate) async fn newest_recipe(
    holder: &Arc<dyn Sandbox>,
    context: &str,
) -> Result<Vec<u8>, String> {
    let Some(one_shot) = holder.one_shot() else {
        return Err("the holder has no one-shot capability".to_owned());
    };
    let newest = one_shot
        .newest_image(image_flight::ENV_TAG_PREFIX)
        .await
        .map_err(|problem| format!("listing the daemon's cooked images failed: {problem}"))?;
    let Some(image) = newest else {
        return Err("the daemon holds no cooked environment image yet".to_owned());
    };
    let context = context.to_owned();
    let mini = spawn_blocking(move || pack_manifests(Path::new(&context)))
        .await
        .map_err(|error| format!("the manifest pack task failed: {error}"))?
        .map_err(|message| format!("packing the cook manifests failed: {message}"))?;

    holder
        .fs()
        .write(RECIPE_TAR, &mini)
        .await
        .map_err(|error| format!("uploading the cook manifests failed: {error}"))?;
    let extract = OneShotSpec::new(OneShotImage::Registry {
        reference: HOLDER_IMAGE.to_owned(),
    })
    .entrypoint("/bin/sh")
    .args([
        "-c",
        &format!("mkdir -p {RECIPE_DIR} && tar -xf {RECIPE_TAR} -C {RECIPE_DIR}"),
    ]);
    run_one_shot(one_shot, &extract, "extracting the cook manifests")
        .await
        .map_err(|message| message.to_string())?;

    let prepare = OneShotSpec::new(OneShotImage::Registry { reference: image })
        .entrypoint("/bin/sh")
        .args([
            "-c",
            &format!(
                "cd {RECIPE_DIR} && cargo chef prepare --recipe-path {RECIPE_JSON} \
                 && test -s {RECIPE_JSON}"
            ),
        ])
        .timeout(RECIPE_BUDGET);
    run_one_shot(one_shot, &prepare, "preparing the cook recipe")
        .await
        .map_err(|message| message.to_string())?;

    let recipe = holder
        .fs()
        .read(RECIPE_JSON)
        .await
        .map_err(|error| format!("reading the cook recipe back failed: {error}"))?;
    Ok(recipe)
}

/// Packs the context's COOK INPUTS — every `Cargo.toml` and `Cargo.lock`
/// at their paths plus the cargo configuration under `.cargo/` — as a
/// tar over the working tree (run checkouts are clean, so this equals
/// the committed manifests). `rust-toolchain` files are deliberately
/// absent: the prepare image's toolchain is fixed, and a toolchain
/// file would steer rustup inside it instead.
#[cfg(feature = "env-image-evict")]
fn pack_manifests(root: &Path) -> Result<Vec<u8>, String> {
    let mut archive = tar::Builder::new(Vec::new());
    append_manifests(&mut archive, root, "")
        .map_err(|error| format!("packing the cook manifests failed: {error}"))?;
    let bytes = archive
        .into_inner()
        .map_err(|error| format!("finishing the manifest tar failed: {error}"))?;
    Ok(bytes)
}

/// Appends the manifest files under `dir`, sorted by name so the tar is
/// deterministic regardless of directory order.
#[cfg(feature = "env-image-evict")]
fn append_manifests(
    archive: &mut tar::Builder<Vec<u8>>,
    dir: &Path,
    prefix: &str,
) -> io::Result<()> {
    let mut names: Vec<String> = fs::read_dir(dir)?
        .filter_map(|entry| {
            entry
                .ok()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
        })
        .collect();
    names.sort();
    for name in names {
        if name == ".git" {
            continue;
        }
        let path = dir.join(&name);
        let arc = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let metadata = fs::metadata(&path)?;
        if metadata.is_dir() {
            let holds_manifests = name == ".cargo" || holds_manifest_inputs(&path);
            if holds_manifests {
                append_manifests(archive, &path, &arc)?;
            }
        } else if name == "Cargo.toml" || name == "Cargo.lock" {
            archive.append_path_with_name(&path, &arc)?;
        }
    }
    Ok(())
}

/// Whether `dir` itself holds, or recursively contains, a file the
/// recipe reads — pruning directory walks the mini-context does not
/// need (the vendor tree's sources, the target dir, node_modules).
#[cfg(feature = "env-image-evict")]
fn holds_manifest_inputs(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if name == "Cargo.toml" || name == "Cargo.lock" || name == ".cargo" {
            true
        } else {
            path.is_dir() && name != ".git" && holds_manifest_inputs(&path)
        }
    })
}

/// Uploads the full context, then runs the extract and build one-shots
/// on the holder. The build one-shot exits trivially from the built
/// tag — the image build is its side effect, which is why the tag must
/// carry a shell (v1 residual, see the module docs).
#[cfg(feature = "env-image-evict")]
async fn build_on_holder(
    holder: &Arc<dyn Sandbox>,
    dockerfile: &str,
    context: &str,
    tag: &str,
) -> Result<(), EnvError> {
    let context = context.to_owned();
    let context_tar = spawn_blocking(move || pack_context(Path::new(&context)))
        .await
        .map_err(|error| backend_error(format!("the context pack task failed: {error}")))?
        .map_err(|message| backend_error(format!("packing the build context failed: {message}")))?;
    build_on_holder_tar(holder, dockerfile, &context_tar, tag).await
}

/// The tar-carrying core of [`build_on_holder`], shared with the
/// probe-only arm.
async fn build_on_holder_tar(
    holder: &Arc<dyn Sandbox>,
    dockerfile: &str,
    context_tar: &[u8],
    tag: &str,
) -> Result<(), EnvError> {
    let one_shot = holder
        .one_shot()
        .ok_or_else(|| backend_error("the holder lost its one-shot capability".to_owned()))?;

    holder
        .fs()
        .write(CONTEXT_TAR, context_tar)
        .await
        .map_err(|error| backend_error(format!("uploading the build context failed: {error}")))?;

    let extract = OneShotSpec::new(OneShotImage::Registry {
        reference: HOLDER_IMAGE.to_owned(),
    })
    .entrypoint("/bin/sh")
    .args([
        "-c",
        &format!("mkdir -p {CONTEXT_DIR} && tar -xf {CONTEXT_TAR} -C {CONTEXT_DIR}"),
    ]);
    run_one_shot(one_shot, &extract, "extracting the build context").await?;

    holder
        .fs()
        .write(
            &format!("{CONTEXT_DIR}/{DOCKERFILE_NAME}"),
            dockerfile.as_bytes(),
        )
        .await
        .map_err(|error| backend_error(format!("writing the Dockerfile failed: {error}")))?;

    let build = OneShotSpec::new(OneShotImage::Build {
        context:    CONTEXT_DIR.to_owned(),
        dockerfile: Some(DOCKERFILE_NAME.to_owned()),
        tag:        tag.to_owned(),
        reuse:      true,
    })
    .entrypoint("/bin/sh")
    .args(["-c", "exit 0"]);
    run_one_shot(one_shot, &build, "building the environment image").await
}

/// Runs one one-shot to completion and checks it ended by exiting 0.
async fn run_one_shot(
    one_shot: &dyn sandbox_driver::OneShot,
    spec: &OneShotSpec,
    what: &str,
) -> Result<(), EnvError> {
    let result = one_shot
        .run(spec, ExecControls::default())
        .await
        .map_err(|error| backend_error(format!("{what} failed: {error}")))?;
    if result.result.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&result.result.stderr);
    Err(backend_error(format!(
        "{what} failed: {}{}",
        result.result.exit_code.map_or_else(
            || "no exit observed".to_owned(),
            |code| format!("exit {code}")
        ),
        if stderr.trim().is_empty() {
            String::new()
        } else {
            format!(" — {}", stderr.trim())
        }
    )))
}

/// Packs the context host-side: a fresh clone of the repository at
/// `root`, tarred WITHOUT `.git` (a build context wants the committed
/// tree, not the history; the checkout seed's `pack_clone` in
/// attractor/steps keeps `.git` because a workspace needs it — same
/// dance, different keep-list). Returns the tar bytes.
fn pack_context(root: &Path) -> Result<Vec<u8>, String> {
    if !root.join(".git").exists() {
        return Err(format!(
            "{} carries no .git — the build context must be a git checkout",
            root.display()
        ));
    }
    let temp = env::temp_dir().join(format!(
        "petri-env-ctx-{}-{}",
        id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_millis())
            .unwrap_or_default()
    ));
    let clone = temp.join("clone");
    let status = StdCommand::new("git")
        .args([
            "clone",
            "--quiet",
            "--no-hardlinks",
            &format!("file://{}", root.display()),
            clone.to_string_lossy().as_ref(),
        ])
        .status()
        .map_err(|error| format!("running git clone failed: {error}"))?;
    if !status.success() {
        let _ = fs::remove_dir_all(&temp);
        return Err("git clone of the context failed".to_owned());
    }

    let mut archive = tar::Builder::new(Vec::new());
    append_tree(&mut archive, &clone, "")
        .map_err(|error| format!("packing the context failed: {error}"))?;
    let bytes = archive
        .into_inner()
        .map_err(|error| format!("finishing the context tar failed: {error}"))?;
    let _ = fs::remove_dir_all(&temp);
    Ok(bytes)
}

/// Appends `dir`'s contents under `prefix`, skipping `.git` at every
/// level and `.dockerignore` at the root: the packed context is a plain
/// source tree the DAEMON builds, and the repository's own
/// `.dockerignore` (tuned for the runtime image's staged-binary
/// context) would exclude everything from an environment build — the
/// daemon applies it when it unpacks the context tar.
fn append_tree(archive: &mut tar::Builder<Vec<u8>>, dir: &Path, prefix: &str) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" || (name == ".dockerignore" && prefix.is_empty()) {
            continue;
        }
        let path = entry.path();
        let arc = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let metadata = fs::metadata(&path)?;
        if metadata.is_dir() {
            archive.append_dir(&arc, &path)?;
            append_tree(archive, &path, &arc)?;
        } else {
            archive.append_path_with_name(&path, &arc)?;
        }
    }
    Ok(())
}

/// The build's error shape: a backend error under the sandbox backend,
/// staged as `image-build` so a failed acquire names the image build,
/// not an unrelated sandbox create.
fn backend_error(message: String) -> EnvError {
    EnvError::backend(crate::BACKEND, "image-build", message)
}
