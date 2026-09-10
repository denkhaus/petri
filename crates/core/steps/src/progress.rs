//! A step's progress channel and its two acknowledgement levels.
//!
//! Every progress event — a log line, an artifact, a step-defined payload —
//! travels one channel per attempt, in arrival order, to the driver, which
//! appends it to the execution's log as a `StepProgress` record. Two sends
//! differ only in what their return means:
//!
//! - [`ProgressSender::send`] resolves when the event is **queued**. It is in
//!   order behind everything sent before it and ahead of the attempt's outcome
//!   (the driver's completion fence drains the queue before it records the
//!   outcome), but it is not yet in the log: a crash before the append loses
//!   it, and the re-dispatched attempt emits it again.
//! - [`ProgressSender::send_acked`] resolves when the record is **durable**:
//!   the driver appended it and every registered observer with durable storage
//!   confirmed the write. A crash after the acknowledgement cannot lose the
//!   record. A write failure is the error.
//!
//! Both go through the same queue, so an acknowledged event and the plain
//! ones around it keep their order.

use std::error::Error;

use ir::StepEvent;
use tokio::sync::mpsc::error::{SendError, TrySendError};
use tokio::sync::{mpsc, oneshot};

/// Where a durable acknowledgement is answered.
pub type ProgressAck = oneshot::Sender<Result<(), ProgressError>>;

/// One progress event on its way to the driver, with the acknowledgement its
/// sender asked for, if any.
pub struct Progress {
    pub event: StepEvent,
    pub ack:   Option<ProgressAck>,
}

/// Why an acknowledged progress send did not confirm a durable record.
#[derive(Debug, thiserror::Error)]
pub enum ProgressError {
    /// The driver no longer takes progress from this attempt: the run ended,
    /// or the attempt's completion fence closed the queue.
    #[error("the driver stopped taking progress from this attempt")]
    Closed,
    /// The driver appended the record, but a durable store failed to write
    /// it. The record may still be in memory and in other stores; it is not
    /// where the acknowledgement promised it would be. The store's own error
    /// is the source.
    #[error("the progress record did not reach durable storage")]
    NotDurable {
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },
}

/// The sending half of an attempt's progress channel.
#[derive(Clone)]
pub struct ProgressSender {
    tx: mpsc::Sender<Progress>,
}

impl ProgressSender {
    /// A bounded progress channel: the sender for the step, the receiver for
    /// the driver's forwarder.
    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<Progress>) {
        let (tx, rx) = mpsc::channel(capacity);
        (Self { tx }, rx)
    }

    /// Queue an event. Resolves once it is in the queue, behind every earlier
    /// send; the record is appended later, and a crash before then loses it.
    pub async fn send(&self, event: StepEvent) -> Result<(), SendError<StepEvent>> {
        self.forward(Progress { event, ack: None })
            .await
            .map_err(|SendError(progress)| SendError(progress.event))
    }

    /// Queue an event without waiting for capacity.
    pub fn try_send(&self, event: StepEvent) -> Result<(), TrySendError<StepEvent>> {
        self.tx
            .try_send(Progress { event, ack: None })
            .map_err(|error| match error {
                TrySendError::Full(progress) => TrySendError::Full(progress.event),
                TrySendError::Closed(progress) => TrySendError::Closed(progress.event),
            })
    }

    /// Queue an event and resolve once its record is durable: appended by the
    /// driver and confirmed by every observer with durable storage. The error
    /// says whether the driver went away or a store failed to write.
    pub async fn send_acked(&self, event: StepEvent) -> Result<(), ProgressError> {
        let (ack, acked) = oneshot::channel();
        self.forward(Progress {
            event,
            ack: Some(ack),
        })
        .await
        .map_err(|_| ProgressError::Closed)?;
        acked.await.unwrap_or(Err(ProgressError::Closed))
    }

    /// Queue a progress item as it is, acknowledgement included — for a relay
    /// that sits between a step and the driver and must keep the step's
    /// acknowledgement contract intact.
    pub async fn forward(&self, progress: Progress) -> Result<(), SendError<Progress>> {
        self.tx.send(progress).await
    }
}

#[cfg(test)]
mod tests {
    use ir::LogStream;

    use super::*;

    fn line(text: &str) -> StepEvent {
        StepEvent::Log {
            stream: LogStream::Stdout,
            line:   text.to_owned(),
        }
    }

    #[tokio::test]
    async fn plain_and_acknowledged_sends_share_one_queue_in_order() {
        let (sender, mut rx) = ProgressSender::channel(4);
        sender.send(line("first")).await.expect("queued");
        let acked = tokio::spawn({
            let sender = sender.clone();
            async move { sender.send_acked(line("second")).await }
        });
        let first = rx.recv().await.expect("first");
        assert!(first.ack.is_none());
        assert_eq!(first.event, line("first"));
        let second = rx.recv().await.expect("second");
        assert_eq!(second.event, line("second"));
        second
            .ack
            .expect("an acknowledgement")
            .send(Ok(()))
            .expect("awaited");
        acked.await.expect("task").expect("acknowledged");
    }

    #[tokio::test]
    async fn a_dropped_acknowledgement_or_closed_queue_is_closed() {
        let (sender, mut rx) = ProgressSender::channel(4);
        let pending = tokio::spawn({
            let sender = sender.clone();
            async move { sender.send_acked(line("lost")).await }
        });
        let progress = rx.recv().await.expect("queued");
        drop(progress);
        let error = pending
            .await
            .expect("task")
            .expect_err("no acknowledgement came");
        assert!(matches!(error, ProgressError::Closed));

        drop(rx);
        let error = sender
            .send_acked(line("after close"))
            .await
            .expect_err("the queue is closed");
        assert!(matches!(error, ProgressError::Closed));
        let SendError(event) = sender.send(line("plain")).await.expect_err("closed");
        assert_eq!(event, line("plain"));
    }
}
