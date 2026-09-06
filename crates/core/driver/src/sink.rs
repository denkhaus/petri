//! Where log lines go, and where masking happens.
//!
//! Masking runs **before** the append, so the persisted log is post-mask. There
//! is no window in which a secret is on disk.

use std::io;
use std::path::{Path, PathBuf};

use executor::Masker;
use ir::{LogStream, Value};
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt as _;

/// Writes step output to the run directory, and optionally echoes it.
pub(crate) struct LogSink {
    dir:    PathBuf,
    masker: Masker,
    echo:   bool,
}

impl LogSink {
    pub(crate) fn new(run_dir: &Path, masker: Masker) -> Self {
        Self {
            dir: run_dir.join("logs"),
            masker,
            echo: false,
        }
    }

    /// Also write lines to this process's stderr, each prefixed with the
    /// node instance and firing that produced it, so interleaved output from
    /// parallel stages stays attributable.
    #[must_use]
    pub(crate) fn with_echo(mut self, echo: bool) -> Self {
        self.echo = echo;
        self
    }

    pub(crate) fn masker(&self) -> &Masker {
        &self.masker
    }

    /// Mask a line and persist it. The masked line is what the caller should
    /// then hand to the core.
    #[expect(
        clippy::print_stderr,
        reason = "echoing step output to the user's terminal is the whole point of the \
                  `echo` option; stderr keeps it clear of a command's own stdout"
    )]
    pub(crate) async fn record(
        &self,
        node: &str,
        firing: u64,
        stream: LogStream,
        line: &str,
    ) -> String {
        let masked = self.masker.mask(line);
        let path = self.dir.join(format!("{}-{firing}.log", sanitize(node)));
        if let Err(err) = self.append(&path, stream, &masked).await {
            tracing::warn!(
                path = %path.display(),
                node,
                firing,
                error = ?err,
                "step log write failed"
            );
        }
        if self.echo {
            eprintln!("[{node}#{firing}] {}", bounded(&masked));
        }
        masked
    }

    /// The write itself. Its result is the sink's only failure signal: without
    /// it a full disk loses the whole step log while the run reports success.
    /// The line never leaves this function.
    async fn append(&self, path: &Path, stream: LogStream, masked: &str) -> io::Result<()> {
        fs::create_dir_all(&self.dir).await?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        let tag = match stream {
            LogStream::Stdout => "out",
            LogStream::Stderr => "err",
        };
        file.write_all(format!("[{tag}] {masked}\n").as_bytes())
            .await
    }

    /// Mask every string in an outcome's data before the finish record is
    /// appended.
    pub(crate) fn mask_value(&self, value: &Value) -> Value {
        self.masker.mask_value(value)
    }
}

/// How much of one echoed line reaches the terminal. The log keeps the
/// whole line; the terminal gets a bounded prefix and a marker.
const ECHO_LINE_LIMIT: usize = 4096;

/// A line cut to the echo limit at a character boundary.
fn bounded(line: &str) -> String {
    match line.char_indices().nth(ECHO_LINE_LIMIT) {
        Some((end, _)) => format!("{}… [line truncated]", &line[..end]),
        None => line.to_owned(),
    }
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
