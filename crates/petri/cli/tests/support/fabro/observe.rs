//! Observations of a finished run, read from the run directory.
//!
//! Everything here reads durable files a run wrote: `events.json` (the root
//! execution's engine log) and `invocations/*/invocation.json` (the
//! coordinator's record with the final context). Task 2 adds
//! `petri inspect --run-dir <dir> --json`; [`RunObservation::final_context`]
//! and [`RunObservation::fan_in_outputs`] are the seams that command replaces.

use std::fs;
use std::path::Path;

use serde_json::Value;

/// One branch entry of a fan-in's output, in the workflow-visible shape.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BranchEnvelope {
    pub(crate) id:              String,
    pub(crate) index:           Option<u64>,
    pub(crate) item_label:      Option<String>,
    pub(crate) status:          Option<String>,
    pub(crate) context_updates: Option<Value>,
    /// Petri's current envelope carries the raw step output here.
    pub(crate) output:          Option<Value>,
}

impl BranchEnvelope {
    pub(crate) fn from_value(value: &Value) -> Self {
        Self {
            id:              value["id"].as_str().unwrap_or_default().to_string(),
            index:           value.get("index").and_then(Value::as_u64),
            item_label:      value
                .get("item_label")
                .and_then(Value::as_str)
                .map(str::to_string),
            status:          value
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_string),
            context_updates: value.get("context_updates").cloned(),
            output:          value.get("output").cloned(),
        }
    }
}

/// The durable records of one run.
pub(crate) struct RunObservation {
    events:     Value,
    invocation: Value,
}

impl RunObservation {
    /// Read a finished run's directory.
    ///
    /// # Panics
    ///
    /// Panics when the run wrote no `events.json` or no root invocation
    /// record: the run did not finish, which the caller asserts first.
    pub(crate) fn load(run_dir: &Path) -> Self {
        let events = read_json(&run_dir.join("events.json"));
        let invocation = read_json(&run_dir.join("invocations/0000000000000000/invocation.json"));
        Self { events, invocation }
    }

    /// Every `StepFinished` outcome in the root execution, in log order.
    pub(crate) fn step_outcomes(&self) -> Vec<&Value> {
        self.events["records"]
            .as_array()
            .map(|records| {
                records
                    .iter()
                    .filter_map(|record| record.pointer("/event/StepFinished/outcome"))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The outputs of fan-in nodes: the step outcomes whose output is a list
    /// of branch envelopes. Seam for task 2's `petri inspect`, which will name
    /// the node instead of matching the shape.
    pub(crate) fn fan_in_outputs(&self) -> Vec<Vec<BranchEnvelope>> {
        self.step_outcomes()
            .into_iter()
            .filter_map(|outcome| outcome["output"].as_array())
            .filter(|items| items.iter().all(|item| item.get("id").is_some()))
            .map(|items| items.iter().map(BranchEnvelope::from_value).collect())
            .collect()
    }

    /// The fan-in output whose branch ids are exactly `ids`, in order.
    pub(crate) fn fan_in_output(&self, ids: &[&str]) -> Option<Vec<BranchEnvelope>> {
        self.fan_in_outputs().into_iter().find(|envelopes| {
            envelopes.len() == ids.len()
                && envelopes
                    .iter()
                    .zip(ids)
                    .all(|(envelope, id)| envelope.id == *id)
        })
    }

    /// The root invocation's final context. Seam for task 2's `petri inspect`.
    pub(crate) fn final_context(&self) -> &Value {
        &self.invocation["result"]["context"]
    }

    /// The root invocation's recorded status.
    pub(crate) fn final_status(&self) -> Option<&str> {
        self.invocation["result"]["status"].as_str()
    }
}

fn read_json(path: &Path) -> Value {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("the run wrote {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{} is JSON: {e}", path.display()))
}
