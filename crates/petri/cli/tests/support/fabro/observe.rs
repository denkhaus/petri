//! Observations of a finished run, read through `petri inspect --json`.

use std::path::Path;

use serde_json::Value;

use super::inspect;

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

/// The public record of one finished run: the `petri inspect` document.
pub(crate) struct RunObservation {
    document: Value,
}

impl RunObservation {
    /// Inspect a finished run's directory through the shipped binary.
    ///
    /// # Panics
    ///
    /// Panics when the run did not finish or the command fails, which the
    /// caller asserts first.
    pub(crate) fn load(run_dir: &Path) -> Self {
        let document = inspect::inspect(run_dir);
        assert!(
            document["complete"].as_bool() == Some(true),
            "the run is complete: {}",
            document["incomplete"]
        );
        Self { document }
    }

    /// The output of the named fan-in node: one envelope per branch, in the
    /// order the fan-in recorded them.
    pub(crate) fn fan_in_output(&self, node: &str) -> Option<Vec<BranchEnvelope>> {
        inspect::root_nodes(&self.document)[node]["output"]
            .as_array()
            .map(|items| items.iter().map(BranchEnvelope::from_value).collect())
    }

    /// The root invocation's final context.
    pub(crate) fn final_context(&self) -> &Value {
        inspect::root_context(&self.document)
    }

    /// The run's recorded status: `success`, `failed` or `cancelled`.
    pub(crate) fn final_status(&self) -> Option<&str> {
        self.document["status"].as_str()
    }
}
