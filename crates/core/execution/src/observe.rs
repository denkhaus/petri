use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use driver::{EventObserver, ObserveError};
use engine::{EngineState, EventLog, EventRecord, InvalidRecords, LOG_VERSION};
use serde::{Deserialize, Serialize};

use crate::jsonl::clean_lines;
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

/// Why engine-log bytes could not become an [`EventLog`], with no file
/// identity attached — callers that read from a path wrap this in
/// [`EngineLogError`].
///
/// The torn-line rule is strict: a final line is *torn* only when EOF arrives
/// before its terminating newline, and only then is it dropped. A
/// newline-terminated line that fails to decode refuses the load — corruption
/// or tampering must not be silently accepted as a crash prefix.
#[derive(Debug, thiserror::Error)]
pub enum EngineLogDecodeError {
    #[error("no complete header line")]
    MissingHeader,
    #[error("the header line is not `{{\"version\": N}}`")]
    BadHeader(#[source] serde_json::Error),
    #[error("line {line} is not an event record")]
    BadRecord {
        line:   usize,
        #[source]
        source: serde_json::Error,
    },
    #[error(transparent)]
    Invalid(#[from] InvalidRecords),
}

/// An engine log file that could not be read or decoded.
#[derive(Debug, thiserror::Error)]
pub enum EngineLogError {
    #[error("could not {action} `{path}`: {source}")]
    Io {
        action: &'static str,
        path:   PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("`{path}`: {source}")]
    Decode {
        path:   PathBuf,
        #[source]
        source: EngineLogDecodeError,
    },
}

/// The first line of an `events.jsonl` file.
#[derive(Serialize, Deserialize)]
struct Header {
    version: u32,
}

/// Decode `events.jsonl` bytes: header, records, strict torn-line rule.
pub fn decode_engine_log(bytes: &[u8]) -> Result<DecodedEngineLog, EngineLogDecodeError> {
    let mut lines = clean_lines(bytes);
    let (clean_len, torn) = (lines.clean_len, lines.torn);
    let header = lines.next().ok_or(EngineLogDecodeError::MissingHeader)?;
    let header: Header = serde_json::from_slice(header).map_err(EngineLogDecodeError::BadHeader)?;
    let mut records = Vec::new();
    for (index, line) in lines.enumerate() {
        records.push(serde_json::from_slice(line).map_err(|source| {
            EngineLogDecodeError::BadRecord {
                line: index + 2,
                source,
            }
        })?);
    }
    let log = EventLog::try_from_records(header.version, records)?;
    Ok(DecodedEngineLog {
        log,
        clean_len,
        torn,
    })
}

/// Render a log in the `events.jsonl` framing: what [`JsonlEngineLog`] writes
/// incrementally, produced in one piece.
pub fn encode_engine_log(log: &EventLog) -> Vec<u8> {
    let mut out = serde_json::to_vec(&Header {
        version: log.version(),
    })
    .expect("a header always encodes");
    out.push(b'\n');
    for record in log.records() {
        out.extend(serde_json::to_vec(record).expect("a record always encodes"));
        out.push(b'\n');
    }
    out
}

/// Read and decode one execution's `events.jsonl`.
pub fn read_engine_log(path: &Path) -> Result<DecodedEngineLog, EngineLogError> {
    let bytes = fs::read(path).map_err(|source| log_io("read", path, source))?;
    decode_engine_log(&bytes).map_err(|source| EngineLogError::Decode {
        path: path.to_path_buf(),
        source,
    })
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
