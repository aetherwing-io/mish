/// Streaming UTF-8 decoder.
///
/// Handles multi-byte characters split across buffer boundaries.

/// Decodes raw bytes into valid UTF-8 strings, carrying over incomplete
/// multi-byte sequences across buffer boundaries.
pub struct Utf8Decoder {
    carryover: Vec<u8>,
}

impl Utf8Decoder {
    pub fn new() -> Self {
        Self {
            carryover: Vec::new(),
        }
    }

    /// Decode a chunk of bytes into a UTF-8 string.
    ///
    /// Incomplete multi-byte sequences at the end are stored in `carryover`
    /// and will be completed by the next call.
    pub fn decode(&mut self, bytes: &[u8]) -> String {
        let input = if self.carryover.is_empty() {
            bytes.to_vec()
        } else {
            let mut combined = std::mem::take(&mut self.carryover);
            combined.extend_from_slice(bytes);
            combined
        };

        // Find the last point where valid UTF-8 ends.
        // Any trailing incomplete sequence goes into carryover.
        match std::str::from_utf8(&input) {
            Ok(s) => s.to_string(),
            Err(e) => {
                let valid_up_to = e.valid_up_to();
                // Check if there's an incomplete sequence at the end
                // vs an actual invalid byte in the middle
                match e.error_len() {
                    None => {
                        // Incomplete sequence at end — carry over
                        self.carryover = input[valid_up_to..].to_vec();
                        let valid = &input[..valid_up_to];
                        // SAFETY: we just determined this prefix is valid UTF-8
                        unsafe { std::str::from_utf8_unchecked(valid) }.to_string()
                    }
                    Some(_error_len) => {
                        // Invalid byte(s) in the middle — replace and continue
                        let mut result = String::new();
                        let mut pos = 0;
                        while pos < input.len() {
                            match std::str::from_utf8(&input[pos..]) {
                                Ok(s) => {
                                    result.push_str(s);
                                    pos = input.len();
                                }
                                Err(e) => {
                                    let vup = e.valid_up_to();
                                    // SAFETY: valid_up_to guarantees valid UTF-8
                                    result.push_str(unsafe {
                                        std::str::from_utf8_unchecked(&input[pos..pos + vup])
                                    });
                                    match e.error_len() {
                                        Some(elen) => {
                                            result.push('\u{FFFD}');
                                            pos = pos + vup + elen;
                                        }
                                        None => {
                                            // Incomplete at end — carry over
                                            self.carryover = input[pos + vup..].to_vec();
                                            pos = input.len();
                                        }
                                    }
                                }
                            }
                        }
                        result
                    }
                }
            }
        }
    }

    /// Flush any remaining carryover bytes as replacement characters.
    pub fn flush(&mut self) -> String {
        if self.carryover.is_empty() {
            String::new()
        } else {
            self.carryover.clear();
            "\u{FFFD}".to_string()
        }
    }
}

impl Default for Utf8Decoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pure_ascii_passthrough() {
        let mut dec = Utf8Decoder::new();
        let result = dec.decode(b"hello world");
        assert_eq!(result, "hello world");
        assert!(dec.carryover.is_empty());
    }

    #[test]
    fn test_valid_multibyte_complete() {
        let mut dec = Utf8Decoder::new();
        // "café" — é is 2 bytes: 0xC3 0xA9
        let result = dec.decode("café".as_bytes());
        assert_eq!(result, "café");
    }

    #[test]
    fn test_3byte_char_split_across_buffers() {
        let mut dec = Utf8Decoder::new();
        // "→" is U+2192: 3 bytes: 0xE2 0x86 0x92
        let bytes = "→".as_bytes();
        assert_eq!(bytes.len(), 3);

        // First chunk: just the first byte
        let r1 = dec.decode(&bytes[..1]);
        assert_eq!(r1, "");
        assert_eq!(dec.carryover.len(), 1);

        // Second chunk: remaining 2 bytes
        let r2 = dec.decode(&bytes[1..]);
        assert_eq!(r2, "→");
        assert!(dec.carryover.is_empty());
    }

    #[test]
    fn test_2byte_char_split_across_buffers() {
        let mut dec = Utf8Decoder::new();
        // é is 0xC3 0xA9
        let bytes = "é".as_bytes();
        assert_eq!(bytes.len(), 2);

        let r1 = dec.decode(&bytes[..1]);
        assert_eq!(r1, "");

        let r2 = dec.decode(&bytes[1..]);
        assert_eq!(r2, "é");
    }

    #[test]
    fn test_4byte_char_split_multiple_ways() {
        let mut dec = Utf8Decoder::new();
        // 🎉 is U+1F389: 4 bytes: 0xF0 0x9F 0x8E 0x89
        let bytes = "🎉".as_bytes();
        assert_eq!(bytes.len(), 4);

        // Split: 2 bytes, then 2 bytes
        let r1 = dec.decode(&bytes[..2]);
        assert_eq!(r1, "");
        assert_eq!(dec.carryover.len(), 2);

        let r2 = dec.decode(&bytes[2..]);
        assert_eq!(r2, "🎉");
        assert!(dec.carryover.is_empty());
    }

    #[test]
    fn test_mixed_ascii_and_split_multibyte() {
        let mut dec = Utf8Decoder::new();
        // "hello→" as bytes, split so → is incomplete
        let full = "hello→world".as_bytes();
        // "hello" is 5 bytes, "→" is 3 bytes (E2 86 92), "world" is 5 bytes
        // total = 13 bytes
        // Split at byte 6: "hello" + first byte of →
        let r1 = dec.decode(&full[..6]);
        assert_eq!(r1, "hello");

        let r2 = dec.decode(&full[6..]);
        assert_eq!(r2, "→world");
    }

    #[test]
    fn test_invalid_byte_replaced() {
        let mut dec = Utf8Decoder::new();
        // 0xFF is never valid in UTF-8
        let result = dec.decode(&[0xFF, b'a', b'b']);
        assert!(result.contains('\u{FFFD}'));
        assert!(result.contains('a'));
    }

    #[test]
    fn test_empty_input() {
        let mut dec = Utf8Decoder::new();
        let result = dec.decode(b"");
        assert_eq!(result, "");
    }

    #[test]
    fn test_flush_returns_replacement_for_incomplete() {
        let mut dec = Utf8Decoder::new();
        // Push first byte of a 3-byte sequence
        let bytes = "→".as_bytes();
        dec.decode(&bytes[..1]);
        assert_eq!(dec.carryover.len(), 1);

        // Flush should emit replacement char
        let flushed = dec.flush();
        assert_eq!(flushed, "\u{FFFD}");
        assert!(dec.carryover.is_empty());
    }

    #[test]
    fn test_flush_empty_carryover() {
        let mut dec = Utf8Decoder::new();
        let flushed = dec.flush();
        assert_eq!(flushed, "");
    }
}
