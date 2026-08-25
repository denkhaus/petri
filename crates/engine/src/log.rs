//! The event log. Versioned from day one so replay and resume can land later
//! without a format migration.

use serde::{Deserialize, Serialize};

use crate::event::Event;

/// Bumped whenever the on-disk shape of a record changes.
pub const LOG_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    /// Position in the log, starting at 0.
    pub seq: u64,
    pub event: Event,
}

/// An append-only list of every event the run has applied, in order.
///
/// The core appends here before applying, including for the events it emits itself
/// while routing. Replaying the log through `apply` from a fresh state reproduces the
/// run exactly, because `apply` has no other inputs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventLog {
    pub version: u32,
    records: Vec<EventRecord>,
}

impl Default for EventLog {
    fn default() -> Self {
        Self {
            version: LOG_VERSION,
            records: Vec::new(),
        }
    }
}

impl EventLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn append(&mut self, event: Event) -> u64 {
        let seq = self.records.len() as u64;
        self.records.push(EventRecord { seq, event });
        seq
    }

    pub fn records(&self) -> &[EventRecord] {
        &self.records
    }

    pub fn events(&self) -> impl Iterator<Item = &Event> {
        self.records.iter().map(|r| &r.event)
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}
