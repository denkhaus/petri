//! Line-buffered capture of a process's output.
//!
//! Every executor pipes a child's stdout and stderr through [`pump`], so the
//! line cap and the truncation marker are decided once, here, and a step's log
//! looks the same whichever environment ran it.
//!
//! The rule for output is that no byte is lost silently: a line up to
//! [`LINE_CAP`] reaches the step whole, and a longer one is cut at the cap
//! with a marker that states how many bytes were dropped and a
//! [`LogLine::dropped`] count the step records on its outcome.

use tokio::io::{AsyncBufReadExt as _, AsyncRead, BufReader};
use tokio::sync::mpsc;

use crate::env::LogLine;

/// Lines longer than this are cut, with a marker naming the bytes dropped.
/// Sized so a long line (a one-line JSON document, an encoded payload) is
/// kept whole and reaches a step's own output rules, while one line stays a
/// bounded log record.
pub const LINE_CAP: usize = 1024 * 1024;

/// Capacity of the per-process line channel every executor strings between its
/// [`pump`] tasks and the step's reader. One decision, decided here: enough
/// slack that a chatty child keeps streaming while the reader is busy, small
/// enough that a stalled reader backpressures the pumps instead of buffering
/// the log in memory.
pub const LINE_CHANNEL_CAPACITY: usize = 256;

/// The marker appended to a cut line, with the count of bytes dropped.
fn truncation_marker(dropped: usize) -> String {
    format!(" …[line truncated: {dropped} bytes dropped]")
}

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
        let dropped = buf.len().saturating_sub(LINE_CAP);
        if dropped > 0 {
            buf.truncate(LINE_CAP);
        }
        let mut line = String::from_utf8_lossy(&buf).into_owned();
        if dropped > 0 {
            line.push_str(&truncation_marker(dropped));
        }
        if tx
            .send(LogLine {
                stream,
                line,
                dropped,
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

    /// A line at the cap is kept whole; one past it is cut at the cap, and
    /// both the marker and `dropped` say how many bytes were lost.
    #[tokio::test]
    async fn a_line_past_the_cap_is_cut_and_the_loss_is_counted() {
        let whole: &'static [u8] = Box::leak(vec![b'x'; LINE_CAP].into_boxed_slice());
        let lines = lines_of(whole).await;
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].line.len(), LINE_CAP);
        assert_eq!(lines[0].dropped, 0);

        let mut long = vec![b'y'; LINE_CAP + 10];
        long.push(b'\n');
        long.extend_from_slice(b"after\n");
        let lines = lines_of(Box::leak(long.into_boxed_slice())).await;
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].dropped, 10);
        assert!(lines[0].terminated);
        assert_eq!(
            &lines[0].line[LINE_CAP..],
            " …[line truncated: 10 bytes dropped]"
        );
        assert!(lines[0].line[..LINE_CAP].bytes().all(|b| b == b'y'));
        assert_eq!((lines[1].line.as_str(), lines[1].dropped), ("after", 0));
    }
}
