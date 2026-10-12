//! Incremental line framing for newline-delimited JSON streams.
//!
//! Both providers speak newline-delimited JSON on stdio. Reading it needs
//! careful framing: a single JSON object can arrive across many reads, and a
//! pathological writer must not be able to exhaust memory.
//!
//! This is the piece the Swift original needed 60 lines of hand-rolled buffer
//! for. In Rust it is a `Vec` and a scan.

/// The largest single line accepted, in bytes.
///
/// Codex's safety limit. Beyond it the stream is not something FlowX can
/// render, and continuing would mean buffering without bound.
pub const DEFAULT_MAXIMUM_LINE_BYTES: usize = 64 * 1024 * 1024;

/// What went wrong while reading a stream.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("a single JSON message exceeded the {limit} byte safety limit")]
    TooLarge { limit: usize },
}

/// Reassembles a byte stream into complete lines.
#[derive(Debug)]
pub struct LineFramer {
    buffer: Vec<u8>,
    /// How far through `buffer` the newline search has already been.
    ///
    /// Without this, a large line arriving in many chunks is rescanned from the
    /// start on every chunk — quadratic in the line length, which for a 64 MB
    /// message is billions of byte comparisons. The Swift original carried the
    /// same field for the same reason.
    scanned: usize,
    maximum_line_bytes: usize,
}

impl LineFramer {
    pub fn new() -> Self {
        Self::with_limit(DEFAULT_MAXIMUM_LINE_BYTES)
    }

    pub fn with_limit(maximum_line_bytes: usize) -> Self {
        Self {
            buffer: Vec::new(),
            scanned: 0,
            maximum_line_bytes: maximum_line_bytes.max(1),
        }
    }

    /// Feed bytes and take every complete line out of the buffer.
    ///
    /// The trailing partial line is kept for the next call, so a message split
    /// across four reads is reassembled rather than dropped.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, FrameError> {
        self.buffer.extend_from_slice(bytes);

        // Take the complete lines first, so a single huge read that does
        // contain terminators is not refused for the bytes after them.
        let mut lines = Vec::new();
        let mut cursor = self.scanned.min(self.buffer.len());
        let mut start = 0usize;
        let mut terminated = false;
        while let Some(offset) = self.buffer[cursor..]
            .iter()
            .position(|b| *b == b'\n')
            .map(|found| cursor + found)
        {
            let mut line = &self.buffer[start..offset];
            // Drop a carriage return from CRLF output.
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            if !line.is_empty() {
                lines.push(String::from_utf8_lossy(line).into_owned());
            }
            start = offset + 1;
            cursor = start;
            terminated = true;
        }

        if terminated {
            self.buffer.drain(..start);
            // What is left is partial-line bytes, none of it scanned yet.
            self.scanned = 0;
        } else {
            // No newline: everything buffered has now been checked once, so the
            // next call resumes at the end rather than at the front. This is the
            // line that keeps a large line linear instead of quadratic.
            self.scanned = self.buffer.len();
        }

        // The remainder is one unterminated line. Refuse it rather than buffer
        // without bound, which is what a writer that never sends a newline would
        // otherwise achieve.
        if self.buffer.len() > self.maximum_line_bytes {
            self.buffer.clear();
            self.scanned = 0;
            return Err(FrameError::TooLarge {
                limit: self.maximum_line_bytes,
            });
        }

        Ok(lines)
    }

    /// Take whatever is buffered, for a stream that ended without a newline.
    pub fn finish(&mut self) -> Option<String> {
        if self.buffer.is_empty() {
            return None;
        }
        let line = String::from_utf8_lossy(&self.buffer).into_owned();
        self.buffer.clear();
        self.scanned = 0;
        Some(line).filter(|l| !l.is_empty())
    }

    /// Bytes currently held.
    pub fn pending(&self) -> usize {
        self.buffer.len()
    }
}

impl Default for LineFramer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_whole_line_comes_back_immediately() {
        let mut f = LineFramer::new();
        assert_eq!(f.push(b"hello\n").unwrap(), vec!["hello"]);
    }

    #[test]
    fn several_lines_in_one_read_all_come_back() {
        let mut f = LineFramer::new();
        assert_eq!(f.push(b"a\nb\nc\n").unwrap(), vec!["a", "b", "c"]);
    }

    #[test]
    fn a_partial_line_is_held_until_it_completes() {
        let mut f = LineFramer::new();
        assert!(f.push(b"{\"a\":").unwrap().is_empty());
        assert!(f.pending() > 0);
        assert_eq!(f.push(b"1}\n").unwrap(), vec!["{\"a\":1}"]);
    }

    #[test]
    fn a_message_split_across_many_reads_is_reassembled() {
        let mut f = LineFramer::new();
        let whole = r#"{"type":"session_meta","payload":{"id":"abc"}}"#;
        let mut collected = Vec::new();
        for chunk in whole.as_bytes().chunks(7) {
            collected.extend(f.push(chunk).unwrap());
        }
        // The terminator is a separate read, which is the realistic case.
        assert!(
            collected.is_empty(),
            "the line completed before its newline"
        );
        collected.extend(f.push(b"\n").unwrap());
        assert_eq!(collected, vec![whole.to_string()]);
    }

    #[test]
    fn carriage_returns_are_stripped() {
        let mut f = LineFramer::new();
        assert_eq!(f.push(b"windows\r\n").unwrap(), vec!["windows"]);
    }

    #[test]
    fn empty_lines_are_dropped() {
        // Providers emit blank lines as keepalives; they are not messages.
        let mut f = LineFramer::new();
        assert_eq!(f.push(b"\n\n\n").unwrap(), Vec::<String>::new());
        assert_eq!(f.push(b"a\n\nb\n").unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn an_oversized_line_is_refused_rather_than_buffered() {
        let mut f = LineFramer::with_limit(32);
        let result = f.push(&[b'x'; 128]);
        assert_eq!(result, Err(FrameError::TooLarge { limit: 32 }));
        assert_eq!(f.pending(), 0, "the buffer was not cleared");
    }

    #[test]
    fn a_large_line_split_across_reads_is_also_refused() {
        let mut f = LineFramer::with_limit(32);
        assert!(f.push(&[b'x'; 20]).is_ok());
        let result = f.push(&[b'x'; 20]);
        assert_eq!(result, Err(FrameError::TooLarge { limit: 32 }));
    }

    #[test]
    fn a_line_just_under_the_limit_is_accepted() {
        let mut f = LineFramer::with_limit(16);
        assert_eq!(f.push(b"123456789\n").unwrap(), vec!["123456789"]);
    }

    #[test]
    fn finish_takes_a_trailing_unterminated_line() {
        let mut f = LineFramer::new();
        f.push(b"no newline here").unwrap();
        assert_eq!(f.finish().as_deref(), Some("no newline here"));
        assert_eq!(f.finish(), None, "finishing twice is harmless");
    }

    #[test]
    fn a_stream_ending_cleanly_yields_nothing_from_finish() {
        let mut f = LineFramer::new();
        f.push(b"complete\n").unwrap();
        assert_eq!(f.finish(), None);
    }

    #[test]
    fn a_large_line_arriving_in_many_chunks_is_fast() {
        // The regression this guards: rescanning the buffer on every chunk makes
        // this quadratic. A megabyte in 1 KB chunks must not take seconds.
        let mut f = LineFramer::with_limit(4 * 1024 * 1024);
        let chunk = vec![b'x'; 1024];
        let start = std::time::Instant::now();
        for _ in 0..1024 {
            assert!(f.push(&chunk).unwrap().is_empty());
        }
        let lines = f.push(b"\n").unwrap();
        let elapsed = start.elapsed();
        assert_eq!(lines.len(), 1, "the line was not reassembled");
        // Quadratic framing of a 1 MB line takes tens of seconds; linear takes
        // well under a millisecond. The bound is loose enough not to be flaky on
        // a loaded machine and tight enough to catch the regression.
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "a 1 MB line took {elapsed:?}, which means the buffer is being rescanned"
        );
    }

    #[test]
    fn a_huge_read_with_terminators_is_not_refused() {
        // The limit applies to one line, not to a whole read.
        let mut f = LineFramer::with_limit(64);
        let mut payload = Vec::new();
        for _ in 0..100 {
            payload.extend_from_slice(b"small line\n");
        }
        assert_eq!(f.push(&payload).unwrap().len(), 100);
    }

    #[test]
    fn invalid_utf8_does_not_panic() {
        let mut f = LineFramer::new();
        let lines = f.push(&[0xff, 0xfe, b'\n']).unwrap();
        assert_eq!(lines.len(), 1, "a lossy replacement was emitted");
    }
}
