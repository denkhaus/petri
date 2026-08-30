//! Where log lines go, and where masking happens.
//!
//! Masking runs **before** the append, so the persisted log is post-mask. There
//! is no window in which a secret is on disk.

use std::path::{Path, PathBuf};

use executor::Masker;
use ir::{LogStream, Value};
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt as _;

/// Writes step output to the run directory, and optionally echoes it.
pub struct LogSink {
    dir:    PathBuf,
    masker: Masker,
    echo:   bool,
}

impl LogSink {
    pub fn new(run_dir: &Path, masker: Masker) -> Self {
        Self {
            dir: run_dir.join("logs"),
            masker,
            echo: false,
        }
    }

    /// Also write lines to this process's stdout.
    #[must_use]
    pub fn echoing(mut self, echo: bool) -> Self {
        self.echo = echo;
        self
    }

    pub fn masker(&self) -> &Masker {
        &self.masker
    }

    /// Mask a line and persist it. The masked line is what the caller should
    /// then hand to the core.
    #[expect(
        clippy::print_stdout,
        reason = "echoing step output to the user's terminal is the whole point of the \
                  `echo` option, and the driver has no other stdout path"
    )]
    pub async fn record(&self, node: &str, firing: u64, stream: LogStream, line: &str) -> String {
        let masked = self.masker.mask(line);
        let _ = fs::create_dir_all(&self.dir).await;
        let path = self.dir.join(format!("{}-{firing}.log", sanitize(node)));
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
        {
            let tag = match stream {
                LogStream::Stdout => "out",
                LogStream::Stderr => "err",
            };
            let _ = file
                .write_all(format!("[{tag}] {masked}\n").as_bytes())
                .await;
        }
        if self.echo {
            println!("{node} | {masked}");
        }
        masked
    }

    /// Mask every string in an outcome's data before the finish record is
    /// appended.
    pub fn mask_value(&self, value: &Value) -> Value {
        self.masker.mask_value(value)
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
