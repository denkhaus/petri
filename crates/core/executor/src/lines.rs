//! Line-buffered capture of a process's output.
//!
//! Every executor pipes a child's stdout and stderr through [`pump`], so the
//! line cap and the truncation marker are decided once, here, and a step's log
//! looks the same whichever environment ran it.

use tokio::io::{AsyncBufReadExt as _, AsyncRead, BufReader};
use tokio::sync::mpsc;

use crate::env::LogLine;

/// Lines longer than this are cut, with a marker.
pub(crate) const LINE_CAP: usize = 64 * 1024;

/// Capacity of the per-process line channel every executor strings between its
/// [`pump`] tasks and the step's reader. One decision, decided here: enough
/// slack that a chatty child keeps streaming while the reader is busy, small
/// enough that a stalled reader backpressures the pumps instead of buffering
/// the log in memory.
pub const LINE_CHANNEL_CAPACITY: usize = 256;

const TRUNCATION_MARKER: &str = " …[line truncated]";

/// Read one stream line by line, capping each line, and forward in arrival
/// order.
pub async fn pump<R: AsyncRead + Unpin + Send + 'static>(
    reader: R,
    stream: ir::LogStream,
    tx: mpsc::Sender<LogLine>,
) {
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        // Only the stream's last line can lack the newline: `read_until`
        // returns without one at end of input alone.
        let terminated = buf.last() == Some(&b'\n');
        while buf.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
            buf.pop();
        }
        let truncated = buf.len() > LINE_CAP;
        if truncated {
            buf.truncate(LINE_CAP);
        }
        let mut line = String::from_utf8_lossy(&buf).into_owned();
        if truncated {
            line.push_str(TRUNCATION_MARKER);
        }
        if tx
            .send(LogLine {
                stream,
                line,
                truncated,
                terminated,
            })
            .await
            .is_err()
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn lines_of(input: &'static [u8]) -> Vec<LogLine> {
        let (tx, mut rx) = mpsc::channel(LINE_CHANNEL_CAPACITY);
        pump(input, ir::LogStream::Stdout, tx).await;
        let mut lines = Vec::new();
        while let Some(line) = rx.recv().await {
            lines.push(line);
        }
        lines
    }

    /// The pump records whether each line ended with a newline, so a
    /// consumer that rejoins the lines can restore the exact bytes: only a
    /// final line the process did not terminate is unterminated.
    #[tokio::test]
    async fn the_last_line_records_whether_the_process_terminated_it() {
        let lines = lines_of(b"a\nb").await;
        assert_eq!(
            lines
                .iter()
                .map(|line| (line.line.as_str(), line.terminated))
                .collect::<Vec<_>>(),
            [("a", true), ("b", false)]
        );
        let lines = lines_of(b"a\r\nb\n").await;
        assert_eq!(
            lines
                .iter()
                .map(|line| (line.line.as_str(), line.terminated))
                .collect::<Vec<_>>(),
            [("a", true), ("b", true)]
        );
        assert!(lines_of(b"").await.is_empty());
    }
}
