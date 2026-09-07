//! `petri run --control <FILE>`: run controls from a file the terminal (or a
//! script beside it) appends to.
//!
//! The file is tailed: petri creates it if needed, then polls it for new
//! lines while the run is live. One command per line:
//!
//! | Line | Effect |
//! |---|---|
//! | `pause` | Hold every attempt not yet admitted. Running work continues. |
//! | `unpause` | Release held and future attempts. |
//! | `steer <node> <text…>` | Deliver guidance to the named stage's live firing. An agent queues it for its session; a human gate ignores it and keeps its question open. |
//! | `cancel` | Cancel the run politely; a second `cancel` reaches the kill tier. |
//!
//! Blank lines and lines starting with `#` are ignored. A line that is not a
//! command, or names a stage that is not running, is reported on stderr and
//! skipped: control input never fails the run and never answers a question.
//! Answers travel through the interviewer (`--interactive`,
//! `--interview-script`), so a control line cannot consume a pending answer.
//!
//! An embedded host drives the same [`ControlService`] directly.

use std::io::{self, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use execution::controls::{ControlError, ControlService};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

/// How often the file is checked for new lines.
const POLL: Duration = Duration::from_millis(100);

/// One parsed control line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlLine {
    Pause,
    Unpause,
    Steer { node: String, text: String },
    Cancel,
}

impl ControlLine {
    /// Parse one line. `None` for a blank or comment line; `Err` names why a
    /// line is not a command.
    pub fn parse(line: &str) -> Result<Option<Self>, String> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return Ok(None);
        }
        let mut words = line.splitn(3, char::is_whitespace);
        let command = words.next().unwrap_or_default();
        match command {
            "pause" => Ok(Some(Self::Pause)),
            "unpause" | "resume" => Ok(Some(Self::Unpause)),
            "cancel" => Ok(Some(Self::Cancel)),
            "steer" => {
                let node = words
                    .next()
                    .filter(|node| !node.is_empty())
                    .ok_or_else(|| "`steer` needs a stage name and text".to_owned())?;
                let text = words.next().unwrap_or_default().trim();
                if text.is_empty() {
                    return Err(format!("`steer {node}` needs text to deliver"));
                }
                Ok(Some(Self::Steer {
                    node: node.to_owned(),
                    text: text.to_owned(),
                }))
            }
            other => Err(format!(
                "`{other}` is not a control; use pause, unpause, steer <node> <text>, or cancel"
            )),
        }
    }
}

/// Tail `path` and apply each line to `service` until `stop` fires. Runs on
/// its own task for the run's lifetime; the CLI cancels it once the run has
/// reported.
#[expect(
    clippy::print_stderr,
    reason = "the terminal is told what each control line did on stderr, beside the run's output"
)]
pub async fn drive(path: PathBuf, service: ControlService, stop: CancellationToken) {
    let mut file = match open(&path).await {
        Ok(file) => file,
        Err(error) => {
            eprintln!(
                "warning: could not open the control file {}: {error}",
                path.display()
            );
            return;
        }
    };
    let mut offset = 0_u64;
    let mut partial = String::new();
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            () = sleep(POLL) => {}
        }
        let mut fresh = String::new();
        if file.seek(SeekFrom::Start(offset)).await.is_err() {
            continue;
        }
        match file.read_to_string(&mut fresh).await {
            Ok(0) => continue,
            Ok(read) => offset += read as u64,
            Err(error) => {
                eprintln!(
                    "warning: could not read the control file {}: {error}",
                    path.display()
                );
                continue;
            }
        }
        partial.push_str(&fresh);
        while let Some(end) = partial.find('\n') {
            let line = partial[..end].to_owned();
            partial.drain(..=end);
            apply(&service, &line).await;
        }
    }
}

async fn open(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(path)
        .await
}

#[expect(
    clippy::print_stderr,
    reason = "the terminal is told what each control line did on stderr, beside the run's output"
)]
async fn apply(service: &ControlService, line: &str) {
    let command = match ControlLine::parse(line) {
        Ok(Some(command)) => command,
        Ok(None) => return,
        Err(problem) => {
            eprintln!("control: {problem}");
            return;
        }
    };
    let result = match &command {
        ControlLine::Pause => {
            service.pause();
            Ok(())
        }
        ControlLine::Unpause => {
            service.unpause();
            Ok(())
        }
        ControlLine::Steer { node, text } => service.steer(node, text.clone()).await,
        ControlLine::Cancel => service.cancel(),
    };
    match result {
        Ok(()) => match command {
            ControlLine::Pause => eprintln!("control: paused"),
            ControlLine::Unpause => eprintln!("control: unpaused"),
            ControlLine::Steer { node, .. } => eprintln!("control: steered {node}"),
            ControlLine::Cancel => eprintln!("control: cancel requested"),
        },
        Err(ControlError::NoSuchStage(node)) => {
            eprintln!("control: no stage named `{node}` is running");
        }
        Err(ControlError::NotLive) => eprintln!("control: the stage finished first"),
        Err(ControlError::Finished) => eprintln!("control: the run has finished"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_lines_parse() {
        assert_eq!(ControlLine::parse("pause"), Ok(Some(ControlLine::Pause)));
        assert_eq!(
            ControlLine::parse("  unpause "),
            Ok(Some(ControlLine::Unpause))
        );
        assert_eq!(ControlLine::parse("cancel"), Ok(Some(ControlLine::Cancel)));
        assert_eq!(
            ControlLine::parse("steer agent check the edge cases too"),
            Ok(Some(ControlLine::Steer {
                node: "agent".into(),
                text: "check the edge cases too".into(),
            }))
        );
        assert_eq!(ControlLine::parse(""), Ok(None));
        assert_eq!(ControlLine::parse("# note"), Ok(None));
        assert!(ControlLine::parse("steer agent").is_err());
        assert!(ControlLine::parse("dance").is_err());
    }
}
