//! The export check: the public stream's `record` values are the stored
//! logs.
//!
//! A record's own event carries the stored line unchanged, so a host that
//! keeps `record` values keeps the logs. [`verify_export`] proves it over a
//! run's store: project the run, take the records out of the stream, and check
//! that they equal the stored records as JSON values, that they reload
//! through the log readers into the same logs, and that the external
//! records replay to the stored logs, a complete log exactly and a crash
//! prefix as a prefix. Read-only projection publishes only the stored
//! prefix, so an incomplete log exports exactly what it holds, with the
//! original recording times.

use std::path::Path;

use engine::{EventLog, EventRecord, LOG_VERSION};
use serde_json::Value;
use store::{Access, RunLogs};

use super::replay::{load_run, project_loaded};
use super::{EventSource, Projection, Record, ReplayError};
use crate::{
    CoordinatorRecord, DecodedEngineLog, ExecutionId, StoredEngineRecord,
    decode_coordinator_records, decode_engine_records, encode_engine_record, encode_record,
    open_run_dir,
};

/// Why a run's public stream does not export its logs.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error(transparent)]
    Replay(#[from] ReplayError),
    #[error("a record does not encode: {0}")]
    Encode(#[from] serde_json::Error),
    #[error(
        "the exported coordinator records differ from the stored log{}",
        at_seq(*seq)
    )]
    Coordinator { seq: Option<u64> },
    #[error(
        "the exported records of execution {execution} differ from the stored log{}",
        at_seq(*seq)
    )]
    Engine {
        execution: ExecutionId,
        seq:       Option<u64>,
    },
    #[error("the exported coordinator records do not reload: {0}")]
    CoordinatorReload(#[source] crate::StoreError),
    #[error("the exported records of execution {execution} do not reload: {source}")]
    EngineReload {
        execution: ExecutionId,
        #[source]
        source:    crate::EngineLogDecodeError,
    },
    #[error(
        "replaying the stored external records of execution {execution} does not regenerate the \
         stored log{}",
        at_seq(*seq)
    )]
    Regenerated {
        execution: ExecutionId,
        seq:       Option<u64>,
    },
    #[error(
        "execution {execution} finished, but its stored log is a prefix of the regenerated one \
         ({stored} of {regenerated} records)"
    )]
    Incomplete {
        execution:   ExecutionId,
        stored:      usize,
        regenerated: usize,
    },
}

fn at_seq(seq: Option<u64>) -> String {
    seq.map(|seq| format!(" at seq {seq}")).unwrap_or_default()
}

/// Prove that a run's public stream exports its logs. See the module docs
/// for what is checked.
pub async fn verify_export(logs: &dyn RunLogs) -> Result<(), ExportError> {
    let loaded = load_run(logs).await?;
    let events = project_loaded(&loaded, &mut Projection::new());

    // The coordinator log: every stored record, once, in order, unchanged.
    let exported: Vec<&CoordinatorRecord> = events
        .iter()
        .filter(|event| event.id.source == EventSource::Coordinator)
        .filter_map(|event| match &event.record {
            Some(Record::Coordinator(record)) => Some(record),
            _ => None,
        })
        .collect();
    if let Some(difference) = first_difference(&exported, &loaded.coordinator)? {
        return Err(ExportError::Coordinator {
            seq: difference.seq(),
        });
    }
    let stored = exported
        .iter()
        .map(|record| encode_record(record))
        .collect::<Result<Vec<_>, _>>()
        .map_err(ExportError::CoordinatorReload)?;
    let reloaded = decode_coordinator_records(&stored).map_err(ExportError::CoordinatorReload)?;
    if reloaded != loaded.coordinator {
        return Err(ExportError::Coordinator { seq: None });
    }

    for execution in &loaded.executions {
        let id = execution.execution;
        let exported: Vec<&StoredEngineRecord> = events
            .iter()
            .filter(|event| event.id.source == (EventSource::Execution { execution: id }))
            .filter_map(|event| match &event.record {
                Some(Record::Engine(record)) => Some(record),
                _ => None,
            })
            .collect();
        let stored: Vec<StoredEngineRecord> = stored_records(&execution.log);
        if let Some(difference) = first_difference(&exported, &stored)? {
            return Err(ExportError::Engine {
                execution: id,
                seq:       difference.seq(),
            });
        }
        let reloaded = reload(&exported).map_err(|source| ExportError::EngineReload {
            execution: id,
            source,
        })?;
        if reloaded.log != execution.log.log || reloaded.recorded_at != execution.log.recorded_at {
            return Err(ExportError::Engine {
                execution: id,
                seq:       None,
            });
        }
        // Replay: the stored log is what the external records regenerate,
        // whole when the execution finished, else a prefix.
        let regenerated = engine::replay(execution.graph.clone(), &execution.log.log).log;
        for (stored, regenerated) in execution
            .log
            .log
            .records()
            .iter()
            .zip(regenerated.records())
        {
            if stored != regenerated {
                return Err(ExportError::Regenerated {
                    execution: id,
                    seq:       Some(stored.seq),
                });
            }
        }
        if regenerated.len() < execution.log.log.len() {
            return Err(ExportError::Regenerated {
                execution: id,
                seq:       None,
            });
        }
        if execution.finished && regenerated.len() != execution.log.log.len() {
            return Err(ExportError::Incomplete {
                execution:   id,
                stored:      execution.log.log.len(),
                regenerated: regenerated.len(),
            });
        }
    }
    Ok(())
}

/// The stored lines of one engine log, as the stream carries them.
fn stored_records(log: &DecodedEngineLog) -> Vec<StoredEngineRecord> {
    log.log
        .records()
        .iter()
        .zip(&log.recorded_at)
        .map(|(record, at)| StoredEngineRecord::new(record, *at))
        .collect()
}

/// Exported engine lines back through the log reader.
fn reload(
    exported: &[&StoredEngineRecord],
) -> Result<DecodedEngineLog, crate::EngineLogDecodeError> {
    let (records, times): (Vec<EventRecord>, Vec<u64>) = exported
        .iter()
        .map(|record| (*record).clone().into_parts())
        .unzip();
    let log = EventLog::try_from_records(LOG_VERSION, records)?;
    let stored = log
        .records()
        .iter()
        .zip(&times)
        .map(|(record, at)| encode_engine_record(record, *at))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| crate::EngineLogDecodeError::BadRecord { line: 0, source })?;
    decode_engine_records(&stored)
}

/// [`verify_export`] over the run directory at `run_dir`.
pub async fn verify_export_run_dir(run_dir: &Path) -> Result<(), ExportError> {
    let logs = open_run_dir(run_dir, Access::Read)
        .await
        .map_err(ReplayError::from)?;
    verify_export(&*logs).await
}

/// Where two record sequences part.
enum Difference {
    /// At the record with this seq.
    At(u64),
    /// Past a common prefix: one is longer.
    Length,
}

impl Difference {
    fn seq(&self) -> Option<u64> {
        match self {
            Self::At(seq) => Some(*seq),
            Self::Length => None,
        }
    }
}

/// Where two record sequences first differ as JSON values, if they do.
fn first_difference<T: serde::Serialize + Seq>(
    exported: &[&T],
    stored: &[T],
) -> Result<Option<Difference>, serde_json::Error> {
    for (a, b) in exported.iter().zip(stored) {
        let a: Value = serde_json::to_value(a)?;
        let b_value: Value = serde_json::to_value(b)?;
        if a != b_value {
            return Ok(Some(Difference::At(b.seq())));
        }
    }
    Ok((exported.len() != stored.len()).then_some(Difference::Length))
}

trait Seq {
    fn seq(&self) -> u64;
}

impl Seq for CoordinatorRecord {
    fn seq(&self) -> u64 {
        self.seq
    }
}

impl Seq for StoredEngineRecord {
    fn seq(&self) -> u64 {
        self.seq
    }
}
