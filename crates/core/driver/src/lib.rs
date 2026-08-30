//! The IO loop between the pure core and real processes.
//!
//! The driver owns all IO scheduling and makes no policy decisions: it
//! translates [`Command`](engine::Command)s into effects and effects back into
//! [`Event`](engine::Event)s. Every event it produces is
//! [`EventSource::External`](engine::EventSource::External), and every one of
//! them passes through a single channel, so **arrival order is the total
//! order** and the log makes it canonical. That is what keeps replay
//! byte-identical over real processes whose completion order is a wall-clock
//! accident.

pub mod jitter;
pub mod observe;
pub mod run;
pub mod sink;

pub use observe::{EventObserver, ObserveError};
pub use run::{
    CANCEL_FORCED, CANCELLED_BEFORE_RESUME, CONTROL_CHANNEL_CAPACITY, DEFAULT_CLEANUP_GRACE,
    DeliverDisposition, Driver, KILLED_BEFORE_RESUME, ResumeError, ResumeInfo, RunConfig,
    RunHandle, RunReport,
};
pub use sink::LogSink;
