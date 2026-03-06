//! Key expansion for dedicated PTY input.
//!
//! Expands `<key>` tokens in input strings to terminal byte sequences.
//! Only used for dedicated PTY processes (raw mode TUI apps) where the
//! kernel line discipline doesn't translate keypresses.

/// Expand `<key>` tokens in input string to terminal byte sequences.
///
/// Angle-bracket tokens are case-insensitive. Unrecognized tokens pass
/// through unchanged. Use `<<` to emit a literal `<`.
pub fn expand_keys(input: &str) -> Vec<u8> {
    let mut result = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        if bytes[i] == b'<' {
            // Check for escaped `<<` → literal `<`
            if i + 1 < len && bytes[i + 1] == b'<' {
                result.push(b'<');
                i += 2;
                continue;
            }

            // Scan for closing `>`
            if let Some(close) = bytes[i + 1..].iter().position(|&b| b == b'>') {
                let close = i + 1 + close;
                let token = &input[i + 1..close];
                if let Some(expansion) = lookup_key(token) {
                    result.extend_from_slice(expansion);
                    i = close + 1;
                    continue;
                }
            }

            // No match or no closing `>` — pass through literal `<`
            result.push(b'<');
            i += 1;
        } else {
            result.push(bytes[i]);
            i += 1;
        }
    }

    result
}

/// Look up a key token (case-insensitive) and return its byte sequence.
fn lookup_key(token: &str) -> Option<&'static [u8]> {
    let lower = token.to_ascii_lowercase();

    // Check ctrl- sequences first
    if let Some(letter) = lower.strip_prefix("ctrl-") {
        if letter.len() == 1 {
            let ch = letter.as_bytes()[0];
            if ch.is_ascii_lowercase() {
                // ctrl-a = 0x01, ctrl-z = 0x1a
                let code = ch - b'a' + 1;
                return Some(ctrl_byte(code));
            }
        }
        return None;
    }

    match lower.as_str() {
        "enter" | "cr" | "return" => Some(b"\r"),
        "tab" => Some(b"\t"),
        "esc" | "escape" => Some(b"\x1b"),
        "backspace" | "bs" => Some(b"\x7f"),
        "up" => Some(b"\x1b[A"),
        "down" => Some(b"\x1b[B"),
        "right" => Some(b"\x1b[C"),
        "left" => Some(b"\x1b[D"),
        "space" => Some(b" "),
        _ => None,
    }
}

/// Return a static slice for a ctrl byte (0x01–0x1a).
fn ctrl_byte(code: u8) -> &'static [u8] {
    const CTRL_BYTES: [u8; 26] = [
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
        0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10,
        0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18,
        0x19, 0x1a,
    ];
    &CTRL_BYTES[code as usize - 1..code as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_enter() {
        assert_eq!(expand_keys("hello<enter>"), b"hello\r");
    }

    #[test]
    fn expand_cr_alias() {
        assert_eq!(expand_keys("hello<cr>"), b"hello\r");
    }

    #[test]
    fn expand_return_alias() {
        assert_eq!(expand_keys("<return>"), b"\r");
    }

    #[test]
    fn expand_ctrl_c() {
        assert_eq!(expand_keys("<ctrl-c>"), b"\x03");
    }

    #[test]
    fn expand_ctrl_d() {
        assert_eq!(expand_keys("<ctrl-d>"), b"\x04");
    }

    #[test]
    fn expand_ctrl_a() {
        assert_eq!(expand_keys("<ctrl-a>"), b"\x01");
    }

    #[test]
    fn expand_ctrl_z() {
        assert_eq!(expand_keys("<ctrl-z>"), b"\x1a");
    }

    #[test]
    fn expand_arrows_and_enter() {
        assert_eq!(expand_keys("<up><up><enter>"), b"\x1b[A\x1b[A\r");
    }

    #[test]
    fn expand_all_arrows() {
        assert_eq!(
            expand_keys("<up><down><left><right>"),
            b"\x1b[A\x1b[B\x1b[D\x1b[C"
        );
    }

    #[test]
    fn expand_tab() {
        assert_eq!(expand_keys("cmd<tab>"), b"cmd\t");
    }

    #[test]
    fn expand_escape() {
        assert_eq!(expand_keys("<esc>"), b"\x1b");
        assert_eq!(expand_keys("<escape>"), b"\x1b");
    }

    #[test]
    fn expand_backspace() {
        assert_eq!(expand_keys("<backspace>"), b"\x7f");
        assert_eq!(expand_keys("<bs>"), b"\x7f");
    }

    #[test]
    fn expand_space() {
        assert_eq!(expand_keys("<space>"), b" ");
    }

    #[test]
    fn no_keys_passthrough() {
        assert_eq!(expand_keys("no keys here"), b"no keys here");
    }

    #[test]
    fn escaped_angle_bracket() {
        // `<<` emits literal `<`, then `angle>>` is literal text
        assert_eq!(expand_keys("<<angle>>"), b"<angle>>");
        // To get literal `<enter>` text, escape the opening `<`
        assert_eq!(expand_keys("<<enter>"), b"<enter>");
    }

    #[test]
    fn unknown_token_passthrough() {
        assert_eq!(expand_keys("<unknown>"), b"<unknown>");
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(expand_keys("<Enter>"), b"\r");
        assert_eq!(expand_keys("<ENTER>"), b"\r");
        assert_eq!(expand_keys("<Ctrl-C>"), b"\x03");
        assert_eq!(expand_keys("<TAB>"), b"\t");
    }

    #[test]
    fn mixed_text_and_keys() {
        assert_eq!(
            expand_keys("/help<enter>"),
            b"/help\r"
        );
    }

    #[test]
    fn multiple_keys_in_sequence() {
        assert_eq!(
            expand_keys("What is 2+2?<enter>"),
            b"What is 2+2?\r"
        );
    }

    #[test]
    fn unclosed_angle_bracket() {
        assert_eq!(expand_keys("hello<world"), b"hello<world");
    }

    #[test]
    fn empty_input() {
        assert_eq!(expand_keys(""), b"");
    }

    #[test]
    fn ctrl_invalid_not_letter() {
        // ctrl-1 is not a valid ctrl sequence — passes through
        assert_eq!(expand_keys("<ctrl-1>"), b"<ctrl-1>");
    }

    #[test]
    fn double_escaped_angle() {
        // `<<` emits `<`, then `<<` emits `<`, then `enter>` is literal
        assert_eq!(expand_keys("<<<<enter>"), b"<<enter>");
        // `<<` emits `<`, then `<enter>` expands
        assert_eq!(expand_keys("<<<enter>"), b"<\r");
    }
}
