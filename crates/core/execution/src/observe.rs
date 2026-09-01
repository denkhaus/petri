use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use driver::{EventObserver, ObserveError};
use engine::{EngineState, EventLog, EventRecord, InvalidRecords, LOG_VERSION};
use serde::{Deserialize, Serialize};

use crate::{CoordinatorRecord, ExecutionId};

#[async_trait::async_trait]
pub trait ExecutionObserver: Send + Sync {
    fn on_engine_record(&self, execution: ExecutionId, record: &EventRecord, state: &EngineState);

    fn on_lifecycle(&self, record: &CoordinatorRecord);
}

pub struct AddressedObserver {
    execution: ExecutionId,
    observer:  Arc<dyn ExecutionObserver>,
}

impl AddressedObserver {
    pub fn new(execution: ExecutionId, observer: Arc<dyn ExecutionObserver>) -> Self {
        Self {
            execution,
            observer,
        }
    }
}

#[async_trait::async_trait]
impl EventObserver for AddressedObserver {
    fn on_record(&self, record: &EventRecord, state: &EngineState) {
        self.observer
            .on_engine_record(self.execution, record, state);
    }
}

#[derive(Debug)]
pub struct DecodedEngineLog {
    pub log:       EventLog,
    pub clean_len: usize,
    pub torn:      bool,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineLogError {
    #[error("could not {action} `{path}`: {source}")]
    Io {
        action: &'static str,
        path:   PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("`{path}` has no complete engine-log header")]
    MissingHeader { path: PathBuf },
    #[error("`{path}` has an invalid engine-log header")]
    BadHeader {
        path:   PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("`{path}` has an invalid engine record on line {line}")]
    BadRecord {
        path:   PathBuf,
        line:   usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("`{path}` has an invalid engine history: {source}")]
    Invalid {
        path:   PathBuf,
        #[source]
        source: InvalidRecords,
    },
}

#[derive(Serialize, Deserialize)]
struct Header {
    version: u32,
}

pub fn decode_engine_log(path: &Path, bytes: &[u8]) -> Result<DecodedEngineLog, EngineLogError> {
    let clean_len = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |last| last + 1);
    let mut lines = bytes[..clean_len]
        .split_inclusive(|byte| *byte == b'\n')
        .map(|line| &line[..line.len() - 1]);
    let header = lines.next().ok_or_else(|| EngineLogError::MissingHeader {
        path: path.to_path_buf(),
    })?;
    let header: Header =
        serde_json::from_slice(header).map_err(|source| EngineLogError::BadHeader {
            path: path.to_path_buf(),
            source,
        })?;
    let mut records = Vec::new();
    for (index, line) in lines.enumerate() {
        records.push(
            serde_json::from_slice(line).map_err(|source| EngineLogError::BadRecord {
                path: path.to_path_buf(),
                line: index + 2,
                source,
            })?,
        );
    }
    let log = EventLog::try_from_records(header.version, records).map_err(|source| {
        EngineLogError::Invalid {
            path: path.to_path_buf(),
            source,
        }
    })?;
    Ok(DecodedEngineLog {
        log,
        clean_len,
        torn: clean_len < bytes.len(),
    })
}

pub fn read_engine_log(path: &Path) -> Result<DecodedEngineLog, EngineLogError> {
    let bytes = fs::read(path).map_err(|source| log_io("read", path, source))?;
    decode_engine_log(path, &bytes)
}

struct Writer {
    file:    File,
    failure: Option<String>,
}

/// Durable writer for one execution's independent engine log.
pub struct JsonlEngineLog {
    path:       PathBuf,
    high_water: u64,
    writer:     Mutex<Writer>,
}

impl JsonlEngineLog {
    pub fn create(path: impl Into<PathBuf>) -> Result<Self, EngineLogError> {
        let path = path.into();
        let mut file = File::create(&path).map_err(|source| log_io("create", &path, source))?;
        let mut header = serde_json::to_vec(&Header {
            version: LOG_VERSION,
        })
        .expect("an engine-log header always encodes");
        header.push(b'\n');
        file.write_all(&header)
            .and_then(|()| file.flush())
            .and_then(|()| file.sync_data())
            .map_err(|source| log_io("write", &path, source))?;
        Ok(Self::over(path, file, 0))
    }

    pub fn append(path: impl Into<PathBuf>, high_water: u64) -> Result<Self, EngineLogError> {
        let path = path.into();
        let file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|source| log_io("open", &path, source))?;
        Ok(Self::over(path, file, high_water))
    }

    fn over(path: PathBuf, file: File, high_water: u64) -> Self {
        Self {
            path,
            high_water,
            writer: Mutex::new(Writer {
                file,
                failure: None,
            }),
        }
    }
}

#[async_trait::async_trait]
impl EventObserver for JsonlEngineLog {
    fn on_record(&self, record: &EventRecord, _state: &EngineState) {
        if record.seq < self.high_water {
            return;
        }
        let mut writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        if writer.failure.is_some() {
            return;
        }
        let result = serde_json::to_vec(record)
            .map_err(|error| error.to_string())
            .and_then(|mut line| {
                line.push(b'\n');
                writer
                    .file
                    .write_all(&line)
                    .and_then(|()| writer.file.flush())
                    .and_then(|()| writer.file.sync_data())
                    .map_err(|error| error.to_string())
            });
        if let Err(message) = result {
            writer.failure = Some(message);
        }
    }

    async fn finish(&self) -> Result<(), ObserveError> {
        let mut writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(message) = &writer.failure {
            return Err(ObserveError::new("execution events", message.clone()));
        }
        writer
            .file
            .flush()
            .and_then(|()| writer.file.sync_data())
            .map_err(|source| {
                ObserveError::new(
                    "execution events",
                    format!("could not sync `{}`", self.path.display()),
                )
                .with_source(source)
            })
    }
}

fn log_io(action: &'static str, path: &Path, source: io::Error) -> EngineLogError {
    EngineLogError::Io {
        action,
        path: path.to_path_buf(),
        source,
    }
}
