use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use driver::{EventObserver, ObserveError};
use engine::{EngineState, EventLog, EventRecord, InvalidRecords, LOG_VERSION};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::jsonl::clean_lines;
use crate::{CoordinatorRecord, CoordinatorState, ExecutionId};

#[async_trait::async_trait]
pub trait ExecutionObserver: Send + Sync {
    fn on_engine_record(&self, execution: ExecutionId, record: &EventRecord, state: &EngineState);

    fn on_lifecycle(&self, record: &CoordinatorRecord);

    /// The replayed coordinator state an observer attaches to on resume, so
    /// state it keeps from coordinator records (a pause, the live executions)
    /// starts where the log left it. Records already on disk are not
    /// redelivered; only records appended from here on reach `on_lifecycle`.
    /// Not called on a fresh run.
    fn on_resumed(&self, state: &CoordinatorState) {
        let _ = state;
    }
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

enum WriterMessage {
    Record(Box<EventRecord>),
    /// Answer once every record queued before this message is written: the
    /// writer is one thread over one FIFO queue, so reaching the marker means
    /// the earlier records reached the file or a write failed.
    Durable(oneshot::Sender<Result<(), ObserveError>>),
    Finish(oneshot::Sender<Result<(), ObserveError>>),
}

/// Writer for one execution's independent engine log. The durability bar is
/// flush per record and one fsync at `finish`. A dedicated thread owns the
/// file, so serialization and filesystem latency do not block the driver event
/// loop. The channel is unbounded because the observer contract is lossless;
/// its queue cannot exceed the execution's finite event log.
///
/// [`EventObserver::durable`] is the acknowledgement a step's
/// `send_acked` waits for: the record is written and flushed to the file, so a
/// process crash cannot lose it. The first write failure is answered there and
/// again at `finish`; later records are not written.
pub struct JsonlEngineLog {
    path:       PathBuf,
    high_water: u64,
    tx:         Sender<WriterMessage>,
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
        let (tx, rx) = mpsc::channel();
        let writer_path = path.clone();
        thread::spawn(move || write_engine_records(file, &writer_path, &rx));
        Self {
            path,
            high_water,
            tx,
        }
    }
}

fn write_engine_records(mut file: File, path: &Path, rx: &Receiver<WriterMessage>) {
    let mut failure: Option<String> = None;
    while let Ok(message) = rx.recv() {
        match message {
            WriterMessage::Record(record) => {
                if failure.is_some() {
                    continue;
                }
                let result = serde_json::to_vec(&record)
                    .map_err(|error| error.to_string())
                    .and_then(|mut line| {
                        line.push(b'\n');
                        file.write_all(&line)
                            .and_then(|()| file.flush())
                            .map_err(|error| error.to_string())
                    });
                if let Err(message) = result {
                    failure = Some(message);
                }
            }
            WriterMessage::Durable(reply) => {
                let result = match &failure {
                    Some(message) => Err(ObserveError::new("execution events", message.clone())),
                    None => Ok(()),
                };
                let _ = reply.send(result);
            }
            WriterMessage::Finish(reply) => {
                let result = match &failure {
                    Some(message) => Err(ObserveError::new("execution events", message.clone())),
                    None => file
                        .flush()
                        .and_then(|()| file.sync_data())
                        .map_err(|source| {
                            ObserveError::new(
                                "execution events",
                                format!("could not sync `{}`", path.display()),
                            )
                            .with_source(source)
                        }),
                };
                let _ = reply.send(result);
            }
        }
    }
}

#[async_trait::async_trait]
impl EventObserver for JsonlEngineLog {
    fn on_record(&self, record: &EventRecord, _state: &EngineState) {
        if record.seq < self.high_water {
            return;
        }
        let _ = self
            .tx
            .send(WriterMessage::Record(Box::new(record.clone())));
    }

    async fn durable(&self, _seq: u64) -> Result<(), ObserveError> {
        // The queue is FIFO: every record handed over before this call is
        // ahead of the marker, whatever its seq.
        let (reply, done) = oneshot::channel();
        self.tx
            .send(WriterMessage::Durable(reply))
            .map_err(|_| self.dead())?;
        done.await.unwrap_or_else(|_| Err(self.dead()))
    }

    async fn finish(&self) -> Result<(), ObserveError> {
        let (reply, done) = oneshot::channel();
        self.tx
            .send(WriterMessage::Finish(reply))
            .map_err(|_| self.dead())?;
        done.await.unwrap_or_else(|_| Err(self.dead()))
    }
}

impl JsonlEngineLog {
    /// The writer thread is gone: its queue closed before it answered.
    fn dead(&self) -> ObserveError {
        ObserveError::new(
            "execution events",
            format!("the writer for `{}` stopped", self.path.display()),
        )
    }
}

fn log_io(action: &'static str, path: &Path, source: io::Error) -> EngineLogError {
    EngineLogError::Io {
        action,
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use engine::{Event, EventSource};
    use ir::{CancelScopeId, Graph};
    use testkit::RunDir;

    use super::*;

    fn record(seq: u64) -> EventRecord {
        EventRecord {
            seq,
            source: EventSource::External,
            event: Event::CancelRequested {
                scope: CancelScopeId::ROOT,
            },
        }
    }

    /// `durable` answers once the record is in the file: the bytes read back
    /// decode to the record before the acknowledgement is used for anything.
    #[tokio::test]
    async fn durable_answers_once_the_record_is_in_the_file() {
        let dir = RunDir::new("engine-log-durable");
        let path = dir.path().join("events.jsonl");
        let log = JsonlEngineLog::create(&path).expect("created");
        let state = EngineState::new(Graph::new());
        log.on_record(&record(0), &state);
        log.on_record(&record(1), &state);
        log.durable(1).await.expect("both records written");

        let decoded = decode_engine_log(&fs::read(&path).expect("read")).expect("decodes");
        assert_eq!(decoded.log.records(), &[record(0), record(1)]);
        assert!(!decoded.torn);
        log.finish().await.expect("synced");
    }

    /// A write that fails is the acknowledgement's error — the step hears it
    /// — and `finish` reports it again for the run's report.
    #[tokio::test]
    async fn a_failed_write_is_the_durable_answer_and_the_finish_report() {
        let dir = RunDir::new("engine-log-write-fails");
        let path = dir.path().join("events.jsonl");
        // A read-only handle: every write fails as a full disk's would.
        drop(JsonlEngineLog::create(&path).expect("created"));
        let file = File::open(&path).expect("opened read-only");
        let log = JsonlEngineLog::over(path.clone(), file, 0);
        log.on_record(&record(0), &EngineState::new(Graph::new()));

        let error = log.durable(0).await.expect_err("the write failed");
        assert_eq!(error.observer, "execution events");
        let error = log.finish().await.expect_err("reported again at finish");
        assert_eq!(error.observer, "execution events");
        let decoded = decode_engine_log(&fs::read(&path).expect("read")).expect("decodes");
        assert!(decoded.log.is_empty(), "nothing reached the file");
    }
}
