/// Byte-to-line assembly with overwrite detection.
///
/// Handles CR/LF/CRLF, progress bar overwrites, and partial line timeouts.
pub struct LineBuffer;

/// A logical line from the byte stream.
#[derive(Debug, Clone)]
pub enum Line {
    /// Terminated by \n
    Complete(String),
    /// CR without LF (progress/spinner)
    Overwrite(String),
    /// No terminator after timeout
    Partial(String),
}
