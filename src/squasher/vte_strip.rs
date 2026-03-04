//! VTE-based ANSI stripping.
//!
//! Uses the `vte` crate state machine to parse terminal sequences and extract printable text.

use vte::{Params, Perform};

/// ANSI colors detected in output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnsiColor {
    Red,
    BrightRed,
    Yellow,
    BrightYellow,
    Green,
    BrightGreen,
    Cyan,
    BrightCyan,
    Blue,
    BrightBlue,
    Magenta,
    BrightMagenta,
    White,
    BrightWhite,
    Black,
    BrightBlack,
}

/// Metadata extracted from ANSI sequences in a line.
#[derive(Debug, Clone, Default)]
pub struct AnsiMetadata {
    pub colors: Vec<AnsiColor>,
    pub has_cursor_movement: bool,
    pub has_erase: bool,
    pub clean_text: String,
}

/// Strips ANSI escape sequences from text, extracting metadata.
pub struct VteStripper;

impl VteStripper {
    /// Strip ANSI sequences from raw bytes, returning clean text and metadata.
    pub fn strip(bytes: &[u8]) -> AnsiMetadata {
        let mut collector = StripCollector::default();
        let mut parser = vte::Parser::new();
        for &byte in bytes {
            parser.advance(&mut collector, byte);
        }
        AnsiMetadata {
            colors: collector.colors,
            has_cursor_movement: collector.has_cursor_movement,
            has_erase: collector.has_erase,
            clean_text: collector.text,
        }
    }
}

/// Strip ANSI escape sequences from a multi-line string, line by line.
///
/// Returns clean text without any terminal escape codes. Used by sh_run,
/// sh_spawn, and sh_interact to sanitize output before returning to LLMs.
pub fn strip_ansi(raw: &str) -> String {
    raw.lines()
        .map(|line| VteStripper::strip(line.as_bytes()).clean_text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Strip trailing zsh PROMPT_SP no-newline indicator lines from output.
///
/// When command output doesn't end with a newline, zsh emits PROMPT_EOL_MARK
/// (default "%") followed by spaces and a CR. This is a TTY cosmetic feature
/// that shouldn't appear in structured output.
///
/// This function removes trailing lines that match the PROMPT_SP pattern:
/// - A line that is only whitespace (empty PROMPT_EOL_MARK case)
/// - A line that is "%" or "#" followed by only whitespace (default PROMPT_EOL_MARK)
pub fn strip_prompt_sp(raw: &str) -> String {
    let lines: Vec<&str> = raw.split('\n').collect();
    let mut end = lines.len();

    while end > 0 {
        let line = lines[end - 1].replace('\r', "");
        let trimmed = line.trim();
        if trimmed.is_empty() {
            end -= 1;
        } else if (trimmed == "%" || trimmed == "#") && line.len() > 2 {
            // PROMPT_SP marker: single char + spaces (line > 2 chars = not just "%")
            end -= 1;
        } else {
            break;
        }
    }

    lines[..end].join("\n")
}

#[derive(Default)]
struct StripCollector {
    text: String,
    colors: Vec<AnsiColor>,
    has_cursor_movement: bool,
    has_erase: bool,
    bold: bool,
}

impl StripCollector {
    fn record_color(&mut self, color: AnsiColor) {
        if !self.colors.contains(&color) {
            self.colors.push(color);
        }
    }

    fn map_sgr_color(&mut self, code: u16) {
        let color = match code {
            30 => Some(if self.bold { AnsiColor::BrightBlack } else { AnsiColor::Black }),
            31 => Some(if self.bold { AnsiColor::BrightRed } else { AnsiColor::Red }),
            32 => Some(if self.bold { AnsiColor::BrightGreen } else { AnsiColor::Green }),
            33 => Some(if self.bold { AnsiColor::BrightYellow } else { AnsiColor::Yellow }),
            34 => Some(if self.bold { AnsiColor::BrightBlue } else { AnsiColor::Blue }),
            35 => Some(if self.bold { AnsiColor::BrightMagenta } else { AnsiColor::Magenta }),
            36 => Some(if self.bold { AnsiColor::BrightCyan } else { AnsiColor::Cyan }),
            37 => Some(if self.bold { AnsiColor::BrightWhite } else { AnsiColor::White }),
            90 => Some(AnsiColor::BrightBlack),
            91 => Some(AnsiColor::BrightRed),
            92 => Some(AnsiColor::BrightGreen),
            93 => Some(AnsiColor::BrightYellow),
            94 => Some(AnsiColor::BrightBlue),
            95 => Some(AnsiColor::BrightMagenta),
            96 => Some(AnsiColor::BrightCyan),
            97 => Some(AnsiColor::BrightWhite),
            _ => None,
        };
        if let Some(c) = color {
            self.record_color(c);
        }
    }
}

impl Perform for StripCollector {
    fn print(&mut self, c: char) {
        self.text.push(c);
    }

    fn execute(&mut self, _byte: u8) {
        // Control characters (like \n, \r) — we don't add them to clean text
    }

    fn csi_dispatch(&mut self, params: &Params, _intermediates: &[u8], _ignore: bool, action: char) {
        match action {
            // SGR — Select Graphic Rendition
            'm' => {
                // Two-pass: first collect bold state, then map colors
                let mut has_bold = self.bold;
                for param in params.iter() {
                    match param[0] {
                        0 => has_bold = false,
                        1 => has_bold = true,
                        _ => {}
                    }
                }
                self.bold = has_bold;
                for param in params.iter() {
                    let code = param[0];
                    match code {
                        0 | 1 => {} // already handled
                        30..=37 | 90..=97 => self.map_sgr_color(code),
                        _ => {}
                    }
                }
            }
            // Cursor movement
            'A' | 'B' | 'C' | 'D' | 'H' | 'f' => {
                self.has_cursor_movement = true;
            }
            // Erase
            'J' | 'K' => {
                self.has_erase = true;
            }
            _ => {}
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {}
    fn put(&mut self, _byte: u8) {}
    fn unhook(&mut self) {}
    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}
    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_text_passthrough() {
        let result = VteStripper::strip(b"hello world");
        assert_eq!(result.clean_text, "hello world");
        assert!(result.colors.is_empty());
        assert!(!result.has_cursor_movement);
        assert!(!result.has_erase);
    }

    #[test]
    fn test_strip_simple_color() {
        // \x1b[31m = red foreground, \x1b[0m = reset
        let input = b"\x1b[31merror: something broke\x1b[0m";
        let result = VteStripper::strip(input);
        assert_eq!(result.clean_text, "error: something broke");
        assert!(result.colors.contains(&AnsiColor::Red));
    }

    #[test]
    fn test_strip_bold_color() {
        // \x1b[1;31m = bold red
        let input = b"\x1b[1;31mFATAL ERROR\x1b[0m";
        let result = VteStripper::strip(input);
        assert_eq!(result.clean_text, "FATAL ERROR");
        assert!(result.colors.contains(&AnsiColor::BrightRed));
    }

    #[test]
    fn test_strip_multiple_colors() {
        // green "ok" then red "fail"
        let input = b"\x1b[32mok\x1b[0m \x1b[31mfail\x1b[0m";
        let result = VteStripper::strip(input);
        assert_eq!(result.clean_text, "ok fail");
        assert!(result.colors.contains(&AnsiColor::Green));
        assert!(result.colors.contains(&AnsiColor::Red));
    }

    #[test]
    fn test_detect_cursor_movement() {
        // \x1b[A = cursor up
        let input = b"\x1b[Asome text";
        let result = VteStripper::strip(input);
        assert_eq!(result.clean_text, "some text");
        assert!(result.has_cursor_movement);
    }

    #[test]
    fn test_detect_erase() {
        // \x1b[K = erase to end of line
        let input = b"content\x1b[K";
        let result = VteStripper::strip(input);
        assert_eq!(result.clean_text, "content");
        assert!(result.has_erase);
    }

    #[test]
    fn test_strip_256_color() {
        // \x1b[38;5;196m = 256-color mode, color 196 (red-ish)
        let input = b"\x1b[38;5;196mcolored text\x1b[0m";
        let result = VteStripper::strip(input);
        assert_eq!(result.clean_text, "colored text");
        // 256-color mode detected but not mapped to named colors
    }

    #[test]
    fn test_mixed_ansi_and_text() {
        // Typical cargo output: "   Compiling \x1b[32;1mserde\x1b[0m v1.0.195"
        let input = b"   Compiling \x1b[32;1mserde\x1b[0m v1.0.195";
        let result = VteStripper::strip(input);
        assert_eq!(result.clean_text, "   Compiling serde v1.0.195");
        assert!(result.colors.contains(&AnsiColor::BrightGreen));
    }

    #[test]
    fn test_empty_input() {
        let result = VteStripper::strip(b"");
        assert_eq!(result.clean_text, "");
        assert!(result.colors.is_empty());
    }

    #[test]
    fn test_detect_erase_display() {
        // \x1b[2J = erase entire display
        let input = b"\x1b[2Jrefreshed content";
        let result = VteStripper::strip(input);
        assert_eq!(result.clean_text, "refreshed content");
        assert!(result.has_erase);
    }

    // ── strip_ansi (multi-line public function) ──────────────────────

    #[test]
    fn test_strip_ansi_multiline() {
        let input = "\x1b[32mok\x1b[0m\n\x1b[31merror\x1b[0m\nplain";
        let result = strip_ansi(input);
        assert_eq!(result, "ok\nerror\nplain");
    }

    #[test]
    fn test_strip_ansi_preserves_plain() {
        let input = "hello\nworld";
        assert_eq!(strip_ansi(input), "hello\nworld");
    }

    #[test]
    fn test_strip_ansi_empty() {
        assert_eq!(strip_ansi(""), "");
    }

    // ── strip_prompt_sp ──────────────────────────────────────────────

    #[test]
    fn test_strip_prompt_sp_percent_with_spaces() {
        // Zsh PROMPT_SP: "%" followed by spaces (default PROMPT_EOL_MARK)
        let input = "hello\n%                                        ";
        let result = strip_prompt_sp(input);
        assert_eq!(result, "hello");
    }

    #[test]
    fn test_strip_prompt_sp_percent_with_spaces_and_cr() {
        // Zsh PROMPT_SP with CR: "%" + spaces + "\r"
        let input = "hello\n%                                        \r";
        let result = strip_prompt_sp(input);
        assert_eq!(result, "hello");
    }

    #[test]
    fn test_strip_prompt_sp_hash_with_spaces() {
        // Root PROMPT_SP: "#" followed by spaces
        let input = "hello\n#                                        ";
        let result = strip_prompt_sp(input);
        assert_eq!(result, "hello");
    }

    #[test]
    fn test_strip_prompt_sp_empty_eol_mark() {
        // Empty PROMPT_EOL_MARK: just spaces
        let input = "hello\n                                        ";
        let result = strip_prompt_sp(input);
        assert_eq!(result, "hello");
    }

    #[test]
    fn test_strip_prompt_sp_preserves_content() {
        // Normal output with "%" in content should be preserved
        let input = "progress: 50%\ndone";
        let result = strip_prompt_sp(input);
        assert_eq!(result, "progress: 50%\ndone");
    }

    #[test]
    fn test_strip_prompt_sp_preserves_single_percent() {
        // A bare "%" without trailing spaces (not PROMPT_SP) should be preserved
        let input = "hello\n%";
        let result = strip_prompt_sp(input);
        assert_eq!(result, "hello\n%");
    }

    #[test]
    fn test_strip_prompt_sp_empty_input() {
        assert_eq!(strip_prompt_sp(""), "");
    }

    #[test]
    fn test_strip_prompt_sp_no_prompt_sp() {
        let input = "line 1\nline 2\nline 3";
        let result = strip_prompt_sp(input);
        assert_eq!(result, "line 1\nline 2\nline 3");
    }

    #[test]
    fn test_strip_prompt_sp_multiple_trailing_empty_lines() {
        // Multiple trailing empty/whitespace lines after PROMPT_SP
        let input = "hello\n%                    \n\n";
        let result = strip_prompt_sp(input);
        assert_eq!(result, "hello");
    }
}
