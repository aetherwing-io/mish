/// Byte-to-line assembly with overwrite detection.
///
/// Handles CR/LF/CRLF, progress bar overwrites, and partial line timeouts.
use std::time::{Duration, Instant};

/// A logical line from the byte stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// Terminated by \n
    Complete(String),
    /// CR without LF (progress/spinner)
    Overwrite(String),
    /// No terminator after timeout
    Partial(String),
}

pub struct LineBuffer {
    partial: Vec<u8>,
    overwrite_mode: bool,
    last_byte_time: Instant,
    partial_timeout: Duration,
}

impl LineBuffer {
    pub fn new() -> Self {
        Self {
            partial: Vec::new(),
            overwrite_mode: false,
            last_byte_time: Instant::now(),
            partial_timeout: Duration::from_millis(500),
        }
    }

    /// Ingest a chunk of bytes and return any complete lines.
    pub fn ingest(&mut self, bytes: &[u8]) -> Vec<Line> {
        let mut lines = Vec::new();
        let mut i = 0;

        while i < bytes.len() {
            // Check for CSI erase sequences
            if let Some(skip) = is_erase_sequence(bytes, i) {
                // CSI erase acts like an overwrite — emit current partial as Overwrite
                if !self.partial.is_empty() {
                    let text = String::from_utf8_lossy(&self.partial).into_owned();
                    lines.push(Line::Overwrite(text));
                    self.partial.clear();
                }
                self.overwrite_mode = true;
                i += skip;
                continue;
            }

            let b = bytes[i];

            match b {
                b'\n' => {
                    // Check for CRLF: if partial ends with CR, strip it
                    if self.partial.last() == Some(&b'\r') {
                        self.partial.pop();
                    }
                    let text = String::from_utf8_lossy(&self.partial).into_owned();
                    lines.push(Line::Complete(text));
                    self.partial.clear();
                    self.overwrite_mode = false;
                    i += 1;
                }
                b'\r' => {
                    // Look ahead: if next byte is \n, let the \n handler deal with it
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                        // Push the CR so the \n handler can strip it
                        self.partial.push(b'\r');
                        i += 1;
                        continue;
                    }

                    // Bare CR: overwrite mode
                    if !self.partial.is_empty() {
                        let text = String::from_utf8_lossy(&self.partial).into_owned();
                        lines.push(Line::Overwrite(text));
                    }
                    self.partial.clear();
                    self.overwrite_mode = true;
                    i += 1;
                }
                _ => {
                    self.partial.push(b);
                    self.last_byte_time = Instant::now();
                    i += 1;
                }
            }
        }

        lines
    }

    /// Check if partial buffer has timed out and emit it.
    pub fn emit_partial(&mut self) -> Option<Line> {
        if !self.partial.is_empty()
            && self.last_byte_time.elapsed() >= self.partial_timeout
        {
            let text = String::from_utf8_lossy(&self.partial).into_owned();
            self.partial.clear();
            self.overwrite_mode = false;
            Some(Line::Partial(text))
        } else {
            None
        }
    }

    /// Finalize: flush any remaining bytes as lines.
    /// `remaining` is any extra bytes to process before flushing.
    pub fn finalize(&mut self, remaining: &[u8]) -> Vec<Line> {
        let mut lines = self.ingest(remaining);

        if !self.partial.is_empty() {
            let text = String::from_utf8_lossy(&self.partial).into_owned();
            if self.overwrite_mode {
                lines.push(Line::Overwrite(text));
            } else {
                lines.push(Line::Partial(text));
            }
            self.partial.clear();
            self.overwrite_mode = false;
        }

        lines
    }
}

impl Default for LineBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// Detect CSI erase sequences at position `pos` in `bytes`.
/// Returns the number of bytes consumed if a recognized sequence is found.
///
/// Recognized sequences:
/// - CSI K  (ESC [ K)       — Erase to end of line
/// - CSI 2K (ESC [ 2 K)     — Erase entire line
/// - CSI A  (ESC [ A)        — Cursor up
/// - CSI J  (ESC [ J)        — Erase display
/// - CSI nA (ESC [ <digit> A) — Cursor up n lines
fn is_erase_sequence(bytes: &[u8], pos: usize) -> Option<usize> {
    let remaining = &bytes[pos..];

    // Must start with ESC [
    if remaining.len() < 3 {
        return None;
    }
    if remaining[0] != 0x1b || remaining[1] != b'[' {
        return None;
    }

    let after_csi = &remaining[2..];

    // CSI K — erase to end of line
    if !after_csi.is_empty() && after_csi[0] == b'K' {
        return Some(3);
    }

    // CSI J — erase display
    if !after_csi.is_empty() && after_csi[0] == b'J' {
        return Some(3);
    }

    // CSI A — cursor up
    if !after_csi.is_empty() && after_csi[0] == b'A' {
        return Some(3);
    }

    // CSI <digits> <final_byte>
    // Scan for digits followed by K, A, or J
    let mut j = 0;
    while j < after_csi.len() && after_csi[j].is_ascii_digit() {
        j += 1;
    }

    if j > 0 && j < after_csi.len() {
        let final_byte = after_csi[j];
        if final_byte == b'K' || final_byte == b'A' || final_byte == b'J' {
            return Some(2 + j + 1); // ESC [ <digits> <final>
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    // Test 6: Complete line via LF
    #[test]
    fn test_complete_line_via_lf() {
        let mut buf = LineBuffer::new();
        let lines = buf.ingest(b"hello world\n");
        assert_eq!(lines, vec![Line::Complete("hello world".to_string())]);
    }

    // Test 7: Windows CRLF handling
    #[test]
    fn test_crlf_handling() {
        let mut buf = LineBuffer::new();
        let lines = buf.ingest(b"hello\r\nworld\r\n");
        assert_eq!(
            lines,
            vec![
                Line::Complete("hello".to_string()),
                Line::Complete("world".to_string()),
            ]
        );
    }

    // Test 8: Multiple lines in one chunk
    #[test]
    fn test_multiple_lines_in_one_chunk() {
        let mut buf = LineBuffer::new();
        let lines = buf.ingest(b"line1\nline2\nline3\n");
        assert_eq!(
            lines,
            vec![
                Line::Complete("line1".to_string()),
                Line::Complete("line2".to_string()),
                Line::Complete("line3".to_string()),
            ]
        );
    }

    // Test 9: Partial line timeout (after 500ms)
    #[test]
    fn test_partial_line_timeout() {
        let mut buf = LineBuffer::new();
        buf.partial_timeout = Duration::from_millis(50); // shorter for test
        let lines = buf.ingest(b"partial data");
        assert!(lines.is_empty());

        // Not yet timed out
        assert!(buf.emit_partial().is_none());

        // Wait for timeout
        thread::sleep(Duration::from_millis(60));

        let partial = buf.emit_partial();
        assert_eq!(partial, Some(Line::Partial("partial data".to_string())));
    }

    // Test 10: Overwrite via CR
    #[test]
    fn test_overwrite_via_cr() {
        let mut buf = LineBuffer::new();
        let lines = buf.ingest(b"progress 50%\rprogress 100%\n");
        assert_eq!(
            lines,
            vec![
                Line::Overwrite("progress 50%".to_string()),
                Line::Complete("progress 100%".to_string()),
            ]
        );
    }

    // Test 11: Progress bar collapse (multiple CRs, one final LF)
    #[test]
    fn test_progress_bar_collapse() {
        let mut buf = LineBuffer::new();
        let lines = buf.ingest(b"10%\r20%\r30%\r40%\rdone\n");
        assert_eq!(
            lines,
            vec![
                Line::Overwrite("10%".to_string()),
                Line::Overwrite("20%".to_string()),
                Line::Overwrite("30%".to_string()),
                Line::Overwrite("40%".to_string()),
                Line::Complete("done".to_string()),
            ]
        );
    }

    // Test 12: CR without content after
    #[test]
    fn test_cr_without_content_after() {
        let mut buf = LineBuffer::new();
        let lines = buf.ingest(b"hello\r");
        assert_eq!(lines, vec![Line::Overwrite("hello".to_string())]);
        // Partial buffer should be empty
        assert!(buf.partial.is_empty());
    }

    // Test 13: Empty line (just LF)
    #[test]
    fn test_empty_line() {
        let mut buf = LineBuffer::new();
        let lines = buf.ingest(b"\n");
        assert_eq!(lines, vec![Line::Complete("".to_string())]);
    }

    // Test 14: Finalize flushes partial
    #[test]
    fn test_finalize_flushes_partial() {
        let mut buf = LineBuffer::new();
        let lines = buf.ingest(b"no newline");
        assert!(lines.is_empty());

        let lines = buf.finalize(b"");
        assert_eq!(lines, vec![Line::Partial("no newline".to_string())]);
    }

    // Test 15: Binary data / invalid UTF-8 (lossy conversion)
    #[test]
    fn test_invalid_utf8_lossy() {
        let mut buf = LineBuffer::new();
        // Invalid UTF-8: 0xFF 0xFE are not valid UTF-8 bytes
        let lines = buf.ingest(b"hello \xff\xfe world\n");
        assert_eq!(lines.len(), 1);
        match &lines[0] {
            Line::Complete(s) => {
                assert!(s.contains("hello"));
                assert!(s.contains("world"));
                // Should contain replacement characters
                assert!(s.contains('\u{FFFD}'));
            }
            other => panic!("expected Complete, got {:?}", other),
        }
    }

    // Test 16: CSI erase-to-end-of-line detection
    #[test]
    fn test_csi_erase_to_end_of_line() {
        let mut buf = LineBuffer::new();
        // ESC [ K — erase to end of line
        let mut input = Vec::new();
        input.extend_from_slice(b"old text");
        input.extend_from_slice(b"\x1b[K");
        input.extend_from_slice(b"new text\n");
        let lines = buf.ingest(&input);
        assert_eq!(
            lines,
            vec![
                Line::Overwrite("old text".to_string()),
                Line::Complete("new text".to_string()),
            ]
        );
    }

    // Test 17: CSI cursor up detection
    #[test]
    fn test_csi_cursor_up() {
        let mut buf = LineBuffer::new();
        let mut input = Vec::new();
        input.extend_from_slice(b"line content");
        input.extend_from_slice(b"\x1b[A");
        input.extend_from_slice(b"replacement\n");
        let lines = buf.ingest(&input);
        assert_eq!(
            lines,
            vec![
                Line::Overwrite("line content".to_string()),
                Line::Complete("replacement".to_string()),
            ]
        );
    }

    // Test 18: Mixed content: normal lines + progress + CSI sequences
    #[test]
    fn test_mixed_content() {
        let mut buf = LineBuffer::new();
        let mut input = Vec::new();
        input.extend_from_slice(b"Starting...\n");
        input.extend_from_slice(b"10%\r20%\r30%\n");
        input.extend_from_slice(b"processing");
        input.extend_from_slice(b"\x1b[K");
        input.extend_from_slice(b"done\n");
        input.extend_from_slice(b"Finished!\n");

        let lines = buf.ingest(&input);
        assert_eq!(
            lines,
            vec![
                Line::Complete("Starting...".to_string()),
                Line::Overwrite("10%".to_string()),
                Line::Overwrite("20%".to_string()),
                Line::Complete("30%".to_string()),
                Line::Overwrite("processing".to_string()),
                Line::Complete("done".to_string()),
                Line::Complete("Finished!".to_string()),
            ]
        );
    }

    // Additional: CRLF split across chunks
    #[test]
    fn test_crlf_split_across_chunks() {
        let mut buf = LineBuffer::new();
        // First chunk ends with \r, second starts with \n
        let lines1 = buf.ingest(b"hello\r");
        // \r alone means overwrite
        assert_eq!(lines1, vec![Line::Overwrite("hello".to_string())]);

        // But if next byte is \n, we should get a complete line
        // Since we already emitted the overwrite, the \n produces an empty complete
        let lines2 = buf.ingest(b"\n");
        assert_eq!(lines2, vec![Line::Complete("".to_string())]);
    }

    // Finalize with remaining bytes
    #[test]
    fn test_finalize_with_remaining() {
        let mut buf = LineBuffer::new();
        buf.ingest(b"start ");
        let lines = buf.finalize(b"end");
        assert_eq!(lines, vec![Line::Partial("start end".to_string())]);
    }
}
