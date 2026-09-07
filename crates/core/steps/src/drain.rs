//! The post-exit output drain every process-running step kind shares.
//!
//! A step forwards its process's output lines on a task of its own, so the
//! forwarding keeps going through cancellation. Once the process is gone the
//! step waits for that task, and the wait is bounded by **silence**, not by
//! a clock started at the exit: output that keeps arriving, however slowly
//! the log consumer takes it, re-arms the limit, so a loaded run never loses
//! the tail of a process that already exited. A forwarder that passes
//! nothing on for the whole limit (a pipe a backgrounded grandchild holds
//! open, a consumer that never reads) is stopped, and the step reports the
//! output as possibly incomplete.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio::time;

/// How long a post-exit drain waits for the next forwarded line before it
/// gives up.
pub const DRAIN_IDLE_LIMIT: Duration = Duration::from_secs(5);

/// The count of lines a [`Forwarder`] has passed on. The forwarding future
/// bumps it once per line; the drain reads it to tell arriving output from
/// silence.
pub type Forwarded = Arc<AtomicU64>;

/// How a forwarder's drain ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drain {
    /// The forwarder finished: the process's output stream closed and every
    /// line was passed on.
    Complete,
    /// Nothing was forwarded for [`DRAIN_IDLE_LIMIT`] (or the limit given)
    /// after the process was gone: the forwarder was stopped, and the output
    /// it passed on may be incomplete.
    Silent,
}

/// A spawned task forwarding one process's output, with the count of lines
/// it has passed on.
#[must_use = "call `finish` once the process is gone, so the forwarded output is drained"]
pub struct Forwarder {
    task:     JoinHandle<()>,
    progress: Forwarded,
}

impl Forwarder {
    /// Spawn the forwarding future. It receives the progress counter and
    /// bumps it once per line it passes on; the count is what lets
    /// [`Self::finish`] tell a slow consumer from a silent stream.
    pub fn spawn<F, Fut>(forward: F) -> Self
    where
        F: FnOnce(Forwarded) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let progress = Forwarded::default();
        let task = tokio::spawn(forward(Arc::clone(&progress)));
        Self { task, progress }
    }

    /// Wait for the forwarder after its process is gone. Returns
    /// [`Drain::Complete`] when the task ends on its own, however long that
    /// takes while lines keep arriving; [`Drain::Silent`] once `idle`
    /// passes with no line forwarded, in which case the task is stopped
    /// first.
    pub async fn finish(mut self, idle: Duration) -> Drain {
        let mut seen = self.progress.load(Ordering::Relaxed);
        loop {
            if time::timeout(idle, &mut self.task).await.is_ok() {
                return Drain::Complete;
            }
            let now = self.progress.load(Ordering::Relaxed);
            if now == seen {
                self.task.abort();
                let _ = (&mut self.task).await;
                return Drain::Silent;
            }
            seen = now;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Instant;

    use tokio::sync::mpsc;

    use super::*;

    /// Modelled on sandbox-driver's
    /// `a_slow_consumer_receives_every_byte_after_the_process_exits`: the
    /// stream already holds every line when the drain starts, the consumer
    /// takes far longer than the idle limit to absorb them all, and none is
    /// lost.
    #[tokio::test]
    async fn a_slow_consumer_receives_every_line_after_the_process_exits() {
        let idle = Duration::from_millis(100);
        let total: u64 = 40;
        let (tx, mut rx) = mpsc::channel(usize::try_from(total).expect("fits"));
        for n in 1..=total {
            tx.send(n).await.expect("room for every line");
        }
        drop(tx);
        let received = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        let forwarder = Forwarder::spawn(move |progress| async move {
            while let Some(n) = rx.recv().await {
                // Each line takes a fifth of the idle limit; the whole stream
                // takes eight times the limit.
                time::sleep(Duration::from_millis(20)).await;
                sink.lock().expect("not poisoned").push(n);
                progress.fetch_add(1, Ordering::Relaxed);
            }
        });
        let started = Instant::now();
        assert_eq!(forwarder.finish(idle).await, Drain::Complete);
        assert!(started.elapsed() > idle * 4, "the drain outlived the limit");
        let received = received.lock().expect("not poisoned").clone();
        assert_eq!(received, (1..=total).collect::<Vec<_>>());
    }

    /// A stream held open with nothing arriving ends the drain after the
    /// idle limit and reports it.
    #[tokio::test]
    async fn a_silent_open_stream_ends_the_drain_after_the_limit() {
        let idle = Duration::from_millis(100);
        let (tx, mut rx) = mpsc::channel::<u64>(1);
        let forwarder = Forwarder::spawn(move |progress| async move {
            while rx.recv().await.is_some() {
                progress.fetch_add(1, Ordering::Relaxed);
            }
        });
        let started = Instant::now();
        assert_eq!(forwarder.finish(idle).await, Drain::Silent);
        let elapsed = started.elapsed();
        assert!(elapsed >= idle && elapsed < idle * 10, "{elapsed:?}");
        // The sender outlived the drain: the stream really was still open.
        drop(tx);
    }

    /// A stream that goes quiet after forwarding some lines is drained for
    /// one more idle period after the last line, then reported silent.
    #[tokio::test]
    async fn output_then_silence_re_arms_once_and_then_ends() {
        let idle = Duration::from_millis(100);
        let (tx, mut rx) = mpsc::channel::<u64>(4);
        let forwarder = Forwarder::spawn(move |progress| async move {
            while rx.recv().await.is_some() {
                progress.fetch_add(1, Ordering::Relaxed);
            }
        });
        let feeder = tokio::spawn(async move {
            time::sleep(Duration::from_millis(60)).await;
            let _ = tx.send(1).await;
            // Keep the stream open, silently.
            time::sleep(Duration::from_secs(5)).await;
            drop(tx);
        });
        let started = Instant::now();
        assert_eq!(forwarder.finish(idle).await, Drain::Silent);
        let elapsed = started.elapsed();
        assert!(elapsed >= idle * 2 && elapsed < idle * 10, "{elapsed:?}");
        feeder.abort();
    }
}
