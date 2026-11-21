//! Fork feature (work#1551): the environment cook's daemon-side CAS lock
//! and the waiter's liveness machine.
//!
//! The probe-only cross-process serialization (work#1463) is
//! check-then-act: every cook process probes the daemon for the tag
//! BEFORE any process has minted it, so simultaneous FIRST cooks all
//! miss and all build — the 2026-11-21 incident (five gears, load 42,
//! five concurrent `cargo chef cook --all-targets` pipelines).
//! Acquiring the build right is now an atomic container CREATE under
//! the fixed name `petri-cook-lock-<tag>` (decision 1): the daemon
//! refuses a second container under one name, so the collision IS the
//! arbitration — exactly one winner builds, the losers wait on the
//! canonical tag (decision 2) and reuse it at the winner's mint.
//!
//! A crashed winner must not wedge the fleet's fresh-tip runs (decision
//! 3): waiters re-observe the lock every poll. A lock that is GONE, a
//! lock container that is not RUNNING, and a lock whose winner's HOLDER
//! no longer runs each read as stale; a stale observation that persists
//! past a short grace is taken over — the corpse is deleted and the
//! waiter re-runs the CAS itself (it may become the winner). The
//! winner's holder always exists while its lock does (the cook creates
//! the holder FIRST), so a live winner never presents as stale.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use sandbox_driver::{
    Error as DriverError, Sandbox, SandboxFilter, SandboxProvider, SandboxSource, SandboxSpec,
    SandboxState,
};

use crate::image_build::{HOLDER_IMAGE, ROLE_LABEL, ROLE_VALUE};

/// The fixed lock-name prefix: `petri-cook-lock-<tag>`. Container-name
/// creation is atomic on the daemon; a second create under the name is
/// the CAS loss.
pub(crate) const LOCK_NAME_PREFIX: &str = "petri-cook-lock-";
/// The lock's `petri.role` value: cook machinery, never a run workspace
/// a lease reconcile could adopt (same posture as the holder's
/// `env-build`).
pub(crate) const LOCK_ROLE_VALUE: &str = "env-cook-lock";
/// The label naming the tag a lock serializes.
pub(crate) const LOCK_TAG_LABEL: &str = "petri.cook.tag";
/// The label naming the WINNER'S holder, so waiters can observe whether
/// the winner still lives (the holder is created before the lock).
pub(crate) const LOCK_HOLDER_LABEL: &str = "petri.cook.holder";
/// How long a waiter sleeps between observations (decision 2: 15-30s).
pub(crate) const WAIT_POLL: Duration = Duration::from_secs(20);
/// How long a waiter may wait for the mint before the cook fails CLOSED
/// (decision 2: a bound that outlives the slowest observed cold cook,
/// ~47 minutes under load — one hour with margin). Beyond it the cook
/// parks the run with a routable error; it never fails open to
/// building.
pub(crate) const WAIT_BOUND: Duration = Duration::from_secs(60 * 60);
/// How long a stale lock observation must persist before a waiter takes
/// the lock over (decision 3: "a short grace"). A winner that lives
/// never presents as stale — its holder predates its lock — so the
/// grace only has to outlast transient daemon observations.
pub(crate) const LOCK_RECLAIM_GRACE: Duration = Duration::from_secs(30);

/// The lock container's fixed name for `tag`.
pub(crate) fn lock_name(tag: &str) -> String {
    format!("{LOCK_NAME_PREFIX}{tag}")
}

/// Whether `error` is the daemon's refusal to create a second container
/// under the lock's fixed name — the CAS loss. The docker provider maps
/// a create-time name conflict to
/// `InvalidSpec { field: "name", reason: "a container with this name
/// already exists" }`; both sides of that mapping live in this fork
/// (the vendored sandbox-driver), so matching the shape is stable
/// here.
pub(crate) fn is_name_conflict(error: &DriverError) -> bool {
    matches!(
        error,
        DriverError::InvalidSpec { field, reason }
            if field == "name" && reason.contains("already exists")
    )
}

/// Attempts to acquire the build right for `tag`: one atomic container
/// CREATE under the fixed lock name. `Ok(Some(lock))` — this cook is
/// the winner and holds the lock until it deletes the container.
/// `Ok(None)` — another process holds the lock. `Err` — the create
/// itself failed (routable; never silently treated as a loss).
pub(crate) async fn try_acquire(
    provider: &Arc<dyn SandboxProvider>,
    tag: &str,
    holder_name: &str,
) -> Result<Option<Arc<dyn Sandbox>>, DriverError> {
    let mut spec = SandboxSpec::new(SandboxSource::Image {
        reference: HOLDER_IMAGE.to_owned(),
    })
    .name(lock_name(tag))
    .working_directory("/workspace".to_owned());
    spec.labels
        .insert(ROLE_LABEL.to_owned(), LOCK_ROLE_VALUE.to_owned());
    spec.labels
        .insert(LOCK_TAG_LABEL.to_owned(), tag.to_owned());
    spec.labels
        .insert(LOCK_HOLDER_LABEL.to_owned(), holder_name.to_owned());
    match provider.create(&spec, None).await {
        Ok(lock) => Ok(Some(lock)),
        Err(error) if is_name_conflict(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

/// What one observation of the lock for a tag saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LockSight {
    /// No lock container for the tag exists.
    Gone,
    /// The lock container exists but is not running: a corpse only a
    /// reclaim removes (`id` names it for deletion).
    Dead { id: String },
    /// The lock runs, held by the winner whose holder `holder` names
    /// (`None` when the lock predates the holder label — read
    /// conservatively).
    Alive {
        id:     String,
        holder: Option<String>,
    },
}

/// Observes the lock container for `tag` through the provider's own
/// label-keyed listing — the same daemon every cook shares, so no name
/// parsing and no extra driver facet. The fixed name makes two locks
/// for one tag impossible; if a listing ever disagrees, the running
/// container is the lock.
pub(crate) async fn observe(
    provider: &Arc<dyn SandboxProvider>,
    tag: &str,
) -> Result<LockSight, DriverError> {
    let mut labels = BTreeMap::new();
    labels.insert(ROLE_LABEL.to_owned(), LOCK_ROLE_VALUE.to_owned());
    labels.insert(LOCK_TAG_LABEL.to_owned(), tag.to_owned());
    let mut filter = SandboxFilter::default();
    filter.labels = labels;
    let statuses = provider.list(&filter).await?;
    let pick = statuses
        .iter()
        .find(|status| status.state == SandboxState::Running)
        .or_else(|| statuses.first());
    Ok(match pick {
        None => LockSight::Gone,
        Some(status) if status.state == SandboxState::Running => LockSight::Alive {
            id:     status.id.as_str().to_owned(),
            holder: status.labels.get(LOCK_HOLDER_LABEL).cloned(),
        },
        Some(status) => LockSight::Dead {
            id: status.id.as_str().to_owned(),
        },
    })
}

/// Whether the winner's holder (named by the lock's label) still runs.
/// The winner creates its holder BEFORE its lock, so a live winner
/// always has one to find; a holder that is gone or dead means the
/// winner died without cleanup — the stale-lock class the reclaim
/// exists for (an operator-killed cook, a run deleted mid-build).
pub(crate) async fn winner_holder_alive(
    provider: &Arc<dyn SandboxProvider>,
    holder_name: &str,
) -> Result<bool, DriverError> {
    let mut labels = BTreeMap::new();
    labels.insert(ROLE_LABEL.to_owned(), ROLE_VALUE.to_owned());
    let mut filter = SandboxFilter::default();
    filter.labels = labels;
    let statuses = provider.list(&filter).await?;
    Ok(statuses.iter().any(|status| {
        status.name.as_deref() == Some(holder_name) && status.state == SandboxState::Running
    }))
}

/// Whether a lock observation reads stale: the lock is gone, is a
/// corpse, or its winner's holder no longer runs. Pure, shared by the
/// decision below and the wait loop's clock.
pub(crate) fn stale(lock: &LockSight, winner_alive: Option<bool>) -> bool {
    match lock {
        LockSight::Gone | LockSight::Dead { .. } => true,
        LockSight::Alive { holder: None, .. } => false,
        LockSight::Alive {
            holder: Some(_), ..
        } => !matches!(winner_alive, Some(true) | None),
    }
}

/// One waiter step, pure over the observations so the pins drive it
/// with no daemon and no clock (work#1551 decisions 2 and 3).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WaiterStep {
    /// The tag is minted: reuse it and finish.
    Reuse,
    /// Nothing to do yet: sleep [`WAIT_POLL`] and observe again.
    KeepWaiting,
    /// The lock read stale past the grace: delete the stale lock (`id`,
    /// when a corpse exists) and re-attempt the CAS — the waiter may
    /// become the winner.
    TakeOver { id: Option<String> },
    /// The wait reached [`WAIT_BOUND`]: fail closed. The cook parks the
    /// run with a routable error; it never builds concurrently.
    Park,
}

/// The waiter's decision at one tick. `stale_for` says how long the
/// lock has READ stale (`None` while this is the first such
/// observation) — the wait loop owns the clock, so the bound and grace
/// behaviors stay pure and pinned.
pub(crate) fn waiter_step(
    tag_present: bool,
    lock: &LockSight,
    winner_alive: Option<bool>,
    stale_for: Option<Duration>,
    waited: Duration,
) -> WaiterStep {
    if tag_present {
        return WaiterStep::Reuse;
    }
    if waited >= WAIT_BOUND {
        return WaiterStep::Park;
    }
    if stale(lock, winner_alive) && stale_for.is_some_and(|held| held >= LOCK_RECLAIM_GRACE) {
        let id = match lock {
            LockSight::Gone => None,
            LockSight::Dead { id } | LockSight::Alive { id, .. } => Some(id.clone()),
        };
        return WaiterStep::TakeOver { id };
    }
    WaiterStep::KeepWaiting
}
