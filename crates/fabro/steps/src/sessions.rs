//! Retained native sessions: the run-scoped map from a resolved Fabro thread
//! to the conversation a later node may continue.
//!
//! A node at effective `full` fidelity with a resolved thread takes the
//! thread's retained session, continues it, and puts it back when it
//! succeeds. Any other node starts fresh. A failed node discards the
//! session, as Fabro does ("on error, discard the session"). Pebble owns the
//! conversation: what is retained is its warm export
//! (`CodingAgentExport`), taken after the node's last prompt and before the
//! predecessor agent shut down, so nothing live outlives the step that
//! opened it and the workspace can be released without a teardown here.
//! The successor rebinds its event sink, human input, tool middleware and
//! metrics when it resumes from the export. ACP never reuses threads.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use pebble_coding_agent::CodingAgentExport;

/// One retained conversation.
pub struct Retained {
    pub export:   CodingAgentExport,
    /// The node that last used it, for events.
    pub node:     String,
    /// The route the export runs on, `provider/model`, for the warning when
    /// a later node names another model.
    pub selector: String,
    /// How many nodes have used this thread.
    pub uses:     u32,
}

/// The run's retained sessions by thread id. Registered as a capability by
/// [`crate::register`] through a per-run provisioner.
#[derive(Default)]
pub struct SessionService {
    retained: Mutex<HashMap<String, Retained>>,
}

impl SessionService {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take the thread's retained session. The caller owns it until it
    /// puts it back with [`SessionService::retain`]; a caller that fails
    /// simply does not, which disposes of it.
    pub fn take(&self, thread: &str) -> Option<Retained> {
        self.retained
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(thread)
    }

    /// Keep the session for the next node on this thread.
    pub fn retain(&self, thread: &str, retained: Retained) {
        self.retained
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(thread.to_owned(), retained);
    }

    /// The threads with a retained session, for inspection.
    pub fn threads(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .retained
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .cloned()
            .collect();
        out.sort();
        out
    }
}
