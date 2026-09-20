//! Live delivery: the [`EventProjector`] observer, its bounded queue and
//! pump, and the sinks it delivers to.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use engine::{EngineState, EventRecord};
use serde::{Deserialize, Serialize};
use store::{Access, RunLogs};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use super::replay::{ReplayError, project_run};
use super::{EVENT_CONTRACT_VERSION, EventId, EventSource, Projection, RunEvent};
use crate::{CoordinatorRecord, ExecutionId, ExecutionObserver, open_run_dir};

/// Where projected events go. `deliver` is awaited per event, in order: a
/// slow sink applies backpressure to the bounded queue behind it, never to
/// the driver. An error stops the pump. A call that outlasts the projector's
/// stall budget ([`ProjectorOptions::stall_timeout`]) is dropped and counts
/// as a failure, so an implementation tolerates a cancelled `deliver`.
#[async_trait::async_trait]
pub trait RunEventSink: Send + Sync {
    async fn deliver(&self, event: RunEvent) -> Result<(), SinkError>;

    /// Called once after the last event, before the receipt.
    async fn finish(&self) -> Result<(), SinkError> {
        Ok(())
    }
}

/// The queue capacity of an [`EventProjector`] unless [`ProjectorOptions`]
/// says otherwise.
pub const DEFAULT_QUEUE_CAPACITY: usize = 1024;

/// How long an [`EventProjector`] waits for one sink call unless
/// [`ProjectorOptions`] says otherwise.
pub const DEFAULT_STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// How an [`EventProjector`] bounds the memory and the time a host's sink can
/// cost it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProjectorOptions {
    /// Events the queue holds between the observer callback and the pump:
    /// the most the projector keeps in memory. The callback never waits for
    /// room; an event projected while the queue is full is left to the
    /// durable log and counted as `overflowed` in the receipt.
    pub capacity:      usize,
    /// How long one `deliver` (or `finish`) may take. A call that outlasts it
    /// is dropped, the sink counts as failed from then on, and the receipt
    /// names the event it stalled on.
    pub stall_timeout: Duration,
}

impl Default for ProjectorOptions {
    fn default() -> Self {
        Self {
            capacity:      DEFAULT_QUEUE_CAPACITY,
            stall_timeout: DEFAULT_STALL_TIMEOUT,
        }
    }
}

/// A sink refused or lost an event.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct SinkError {
    pub message: String,
}

impl SinkError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// What the projector did over a run. `projected` is every event derived;
/// `delivered + undelivered` equals it. A receipt that is not clean means
/// the sink does not hold the whole stream and `replay_run` completes it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionReceipt {
    pub version:     u32,
    pub projected:   u64,
    pub delivered:   u64,
    /// Events the sink never received: they found the queue full, or the
    /// sink had failed or stalled before their turn.
    pub undelivered: u64,
    /// Of `undelivered`, the events that found the queue full and were left
    /// to the durable log. Live delivery went on with the next event that
    /// found room, so the sink saw each source in order, with gaps.
    #[serde(default)]
    pub overflowed:  u64,
    /// Why delivery stopped, when it did: the sink's error, or the stall the
    /// pump gave up on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure:     Option<String>,
}

impl ProjectionReceipt {
    pub fn is_clean(&self) -> bool {
        self.failure.is_none() && self.undelivered == 0
    }
}

/// The sender side of the pump's queue and what never got onto it.
struct Queue {
    /// `None` once `shutdown` closed the stream.
    tx:          Option<mpsc::Sender<Box<RunEvent>>>,
    /// Events that found the queue full.
    overflowed:  u64,
    /// Events projected after the stream was closed.
    after_close: u64,
}

/// The live consumption path: an [`ExecutionObserver`] that projects each
/// record and queues the result, without waiting, for a pump task that
/// awaits the sink per event. The queue is bounded ([`ProjectorOptions`]),
/// so the projector holds at most `capacity` events in memory and the run
/// keeps its pace whatever the sink does; an event that finds no room, and
/// every event after the sink fails or stalls, stays in the durable log for
/// [`replay_run`]. The receipt says exactly what the sink did not get.
pub struct EventProjector {
    projection: Mutex<Projection>,
    queue:      Mutex<Queue>,
    pump:       Mutex<Option<JoinHandle<ProjectionReceipt>>>,
}

impl EventProjector {
    /// A projector for a fresh run, with the default options.
    pub fn new(sink: Arc<dyn RunEventSink>) -> Arc<Self> {
        Self::with_options(sink, ProjectorOptions::default())
    }

    /// A projector for a fresh run, with the given queue capacity and stall
    /// budget.
    pub fn with_options(sink: Arc<dyn RunEventSink>, options: ProjectorOptions) -> Arc<Self> {
        Self::with_projection(sink, Projection::new(), options)
    }

    /// A projector for a run being resumed: the stored records are folded
    /// into its state first, and nothing is delivered for them. The resumed
    /// driver then delivers the regenerated suffix and every new record
    /// with the identities a fresh run would have given them.
    ///
    /// # Errors
    ///
    /// The run's logs do not decode or replay.
    pub async fn primed(
        sink: Arc<dyn RunEventSink>,
        logs: &dyn RunLogs,
    ) -> Result<Arc<Self>, ReplayError> {
        Self::primed_with_options(sink, logs, ProjectorOptions::default()).await
    }

    /// [`Self::primed`] over the run directory at `run_dir`.
    pub async fn primed_run_dir(
        sink: Arc<dyn RunEventSink>,
        run_dir: &Path,
    ) -> Result<Arc<Self>, ReplayError> {
        let logs = open_run_dir(run_dir, Access::Read).await?;
        Self::primed(sink, &*logs).await
    }

    /// [`Self::primed`] with the given queue capacity and stall budget.
    ///
    /// # Errors
    ///
    /// The run's logs do not decode or replay.
    pub async fn primed_with_options(
        sink: Arc<dyn RunEventSink>,
        logs: &dyn RunLogs,
        options: ProjectorOptions,
    ) -> Result<Arc<Self>, ReplayError> {
        let mut projection = Projection::new();
        project_run(logs, &mut projection).await?;
        Ok(Self::with_projection(sink, projection, options))
    }

    fn with_projection(
        sink: Arc<dyn RunEventSink>,
        projection: Projection,
        options: ProjectorOptions,
    ) -> Arc<Self> {
        let (tx, rx) = mpsc::channel::<Box<RunEvent>>(options.capacity.max(1));
        let pump = tokio::spawn(pump(sink, rx, options.stall_timeout));
        Arc::new(Self {
            projection: Mutex::new(projection),
            queue:      Mutex::new(Queue {
                tx:          Some(tx),
                overflowed:  0,
                after_close: 0,
            }),
            pump:       Mutex::new(Some(pump)),
        })
    }

    fn projection(&self) -> MutexGuard<'_, Projection> {
        self.projection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn queue(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queue what one record derived. Never waits: an event that finds the
    /// queue full is left to the durable log and counted.
    fn push(&self, events: Vec<RunEvent>) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|d| u64::try_from(d.as_millis()).ok());
        let mut queue = self.queue();
        for mut event in events {
            event.observed_at = now;
            let Some(tx) = queue.tx.as_ref() else {
                queue.after_close += 1;
                continue;
            };
            match tx.try_send(Box::new(event)) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(event)) => {
                    queue.overflowed += 1;
                    if queue.overflowed == 1 {
                        tracing::warn!(
                            event = %event_label(&event.id),
                            "the event sink fell behind; events that find the queue full are left to the log for replay"
                        );
                    }
                }
                // The pump is gone (it panicked); the receipt says so.
                Err(mpsc::error::TrySendError::Closed(_)) => queue.after_close += 1,
            }
        }
    }

    /// End the stream, await the sink's remaining deliveries and `finish`,
    /// and report. Call once, after the run. Bounded: the pump waits at most
    /// one stall budget for any sink call, and delivers nothing more once a
    /// call stalled or failed.
    pub async fn shutdown(&self) -> ProjectionReceipt {
        // Closing the stream: the pump drains what is queued, then finishes.
        let tx = self.queue().tx.take();
        drop(tx);
        let pump = self
            .pump
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let mut receipt = match pump {
            Some(pump) => pump.await.unwrap_or_else(|error| ProjectionReceipt {
                version: EVENT_CONTRACT_VERSION,
                failure: Some(format!("the event pump failed: {error}")),
                ..ProjectionReceipt::default()
            }),
            None => {
                return ProjectionReceipt {
                    version: EVENT_CONTRACT_VERSION,
                    failure: Some("shutdown was called twice".to_owned()),
                    ..ProjectionReceipt::default()
                };
            }
        };
        let (overflowed, after_close) = {
            let queue = self.queue();
            (queue.overflowed, queue.after_close)
        };
        receipt.projected += overflowed + after_close;
        receipt.undelivered += overflowed + after_close;
        receipt.overflowed = overflowed;
        receipt
    }
}

/// The pump: deliver each queued event to the sink, in order, each call
/// bounded by `stall`; after a failure or a stall, count the rest as
/// undelivered; once the queue closes, `finish` the sink.
async fn pump(
    sink: Arc<dyn RunEventSink>,
    mut rx: mpsc::Receiver<Box<RunEvent>>,
    stall: Duration,
) -> ProjectionReceipt {
    let mut receipt = ProjectionReceipt {
        version: EVENT_CONTRACT_VERSION,
        ..ProjectionReceipt::default()
    };
    while let Some(event) = rx.recv().await {
        receipt.projected += 1;
        if receipt.failure.is_some() {
            receipt.undelivered += 1;
            continue;
        }
        let id = event.id;
        match timeout(stall, sink.deliver(*event)).await {
            Ok(Ok(())) => receipt.delivered += 1,
            Ok(Err(error)) => {
                receipt.undelivered += 1;
                receipt.failure = Some(error.message);
            }
            Err(_) => {
                receipt.undelivered += 1;
                let message = format!(
                    "the sink stalled: {} was not accepted within {}ms, and delivery stopped there",
                    event_label(&id),
                    stall.as_millis()
                );
                tracing::warn!("{message}");
                receipt.failure = Some(message);
            }
        }
    }
    if receipt.failure.is_none() {
        match timeout(stall, sink.finish()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => receipt.failure = Some(error.message),
            Err(_) => {
                receipt.failure = Some(format!(
                    "the sink stalled: `finish` did not return within {}ms",
                    stall.as_millis()
                ));
            }
        }
    }
    receipt
}

/// An event identity as a receipt or a log line names it.
fn event_label(id: &EventId) -> String {
    match id.source {
        EventSource::Coordinator => format!("coordinator record {} event {}", id.seq, id.index),
        EventSource::Execution { execution } => format!(
            "execution {} record {} event {}",
            execution.raw(),
            id.seq,
            id.index
        ),
    }
}

impl ExecutionObserver for EventProjector {
    fn on_engine_record(
        &self,
        execution: ExecutionId,
        record: &EventRecord,
        recorded_at: u64,
        state: &EngineState,
    ) {
        let events = self
            .projection()
            .engine(execution, record, recorded_at, state);
        self.push(events);
    }

    fn on_lifecycle(&self, record: &CoordinatorRecord) {
        let events = self.projection().lifecycle(record);
        self.push(events);
    }
}

/// Collects every event it is given, for tests and small hosts.
#[derive(Default)]
pub struct CollectingSink {
    events: Mutex<Vec<RunEvent>>,
}

impl CollectingSink {
    pub fn events(&self) -> Vec<RunEvent> {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait::async_trait]
impl RunEventSink for CollectingSink {
    async fn deliver(&self, event: RunEvent) -> Result<(), SinkError> {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event);
        Ok(())
    }
}
