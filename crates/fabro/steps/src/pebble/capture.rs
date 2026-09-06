//! Bounded head and tail capture while every process byte is drained.

use std::collections::VecDeque;

use pebble_coding_agent::tools::OutputCaptureStats;

pub(super) struct Capture {
    cap:      Option<usize>,
    head:     Vec<u8>,
    tail:     VecDeque<u8>,
    observed: usize,
}
impl Capture {
    pub(super) fn new(cap: Option<usize>) -> Self {
        Self {
            cap,
            head: Vec::new(),
            tail: VecDeque::new(),
            observed: 0,
        }
    }
    pub(super) fn push(&mut self, bytes: &[u8]) {
        self.observed = self.observed.saturating_add(bytes.len());
        let Some(cap) = self.cap else {
            self.head.extend_from_slice(bytes);
            return;
        };
        let head_length = (cap / 2).saturating_sub(self.head.len()).min(bytes.len());
        self.head.extend_from_slice(&bytes[..head_length]);
        let remaining = &bytes[head_length..];
        let tail_cap = cap - cap / 2;
        if remaining.len() >= tail_cap {
            self.tail.clear();
            self.tail.extend(&remaining[remaining.len() - tail_cap..]);
        } else {
            let discard = (self.tail.len() + remaining.len()).saturating_sub(tail_cap);
            self.tail.drain(..discard);
            self.tail.extend(remaining);
        }
    }
    pub(super) fn finish(mut self) -> (String, OutputCaptureStats) {
        self.head.extend(self.tail);
        let retained = self.head.len();
        (
            String::from_utf8_lossy(&self.head).into_owned(),
            OutputCaptureStats {
                observed_bytes: self.observed,
                retained_bytes: retained,
                omitted_bytes:  self.observed.saturating_sub(retained),
            },
        )
    }
}
