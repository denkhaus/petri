//! The shared line framing for the crate's append-only JSONL files.

use std::slice::SplitInclusive;

/// Split bytes into complete (newline-terminated) lines under the strict
/// torn-tail rule: only an EOF-torn final line is dropped, and `clean_len` is
/// where a resumer truncates before appending.
pub(crate) fn clean_lines(bytes: &[u8]) -> CleanLines<'_> {
    let clean_len = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |last| last + 1);
    let is_newline: fn(&u8) -> bool = |byte| *byte == b'\n';
    CleanLines {
        clean_len,
        torn: clean_len < bytes.len(),
        lines: bytes[..clean_len].split_inclusive(is_newline),
    }
}

pub(crate) struct CleanLines<'a> {
    pub clean_len: usize,
    pub torn:      bool,
    lines:         SplitInclusive<'a, u8, fn(&u8) -> bool>,
}

impl<'a> Iterator for CleanLines<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        self.lines.next().map(|line| &line[..line.len() - 1])
    }
}
