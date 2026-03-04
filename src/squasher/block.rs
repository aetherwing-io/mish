//! Block compressor — collapses multi-line diagnostic blocks into single digest lines.
//!
//! Pipeline position: VTE strip -> progress -> **block compress** -> dedup -> Oreo
//!
//! Compiler diagnostics (rustc, gcc, clang) use multi-line human-readable formats
//! with ASCII art, pipe decorators, and context lines. An 8-line rustc warning
//! contains ~1 line of LLM-useful information. This stage compresses each block
//! into a single dense line before dedup runs, so dedup can correctly group
//! identical diagnostics.

use crate::core::grammar::BlockRule;

/// State of the block compressor state machine.
enum State {
    /// Looking for a line matching a block start pattern.
    Scanning,
    /// Accumulating lines within a matched block.
    Collecting,
}

/// Block compressor: processes lines and collapses diagnostic blocks into digests.
pub struct BlockCompressor {
    rules: Vec<BlockRule>,
    state: State,
    buffer: Vec<String>,
    active_rule_idx: usize,
    pub blocks_compressed: u64,
}

impl BlockCompressor {
    pub fn new(rules: Vec<BlockRule>) -> Self {
        Self {
            rules,
            state: State::Scanning,
            buffer: Vec::new(),
            active_rule_idx: 0,
            blocks_compressed: 0,
        }
    }

    /// Process one line. Returns lines to emit (0 or more).
    pub fn feed(&mut self, line: String) -> Vec<String> {
        if self.rules.is_empty() {
            return vec![line];
        }

        match self.state {
            State::Scanning => self.feed_scanning(line),
            State::Collecting => self.feed_collecting(line),
        }
    }

    /// Flush any in-progress block at end of stream.
    pub fn flush(&mut self) -> Vec<String> {
        if self.buffer.is_empty() {
            return vec![];
        }
        self.compress_buffer()
    }

    fn feed_scanning(&mut self, line: String) -> Vec<String> {
        // Check if line matches any block start pattern
        for (idx, rule) in self.rules.iter().enumerate() {
            if rule.start.is_match(&line) {
                self.state = State::Collecting;
                self.active_rule_idx = idx;
                self.buffer.clear();
                self.buffer.push(line);
                return vec![];
            }
        }
        // No match — passthrough
        vec![line]
    }

    fn feed_collecting(&mut self, line: String) -> Vec<String> {
        let rule = &self.rules[self.active_rule_idx];

        // Check if line matches the end pattern (not consumed — emitted after digest)
        if rule.end.is_match(&line) {
            let mut result = self.compress_buffer();
            // End line is not consumed — emit it too
            result.push(line);
            return result;
        }

        // Check if line matches a different rule's start (new block interrupts current)
        for (idx, r) in self.rules.iter().enumerate() {
            if r.start.is_match(&line) {
                let result = self.compress_buffer();
                // Start new block
                self.state = State::Collecting;
                self.active_rule_idx = idx;
                self.buffer.clear();
                self.buffer.push(line);
                return result;
            }
        }

        // Normal line — buffer it
        self.buffer.push(line);
        vec![]
    }

    /// Compress the buffered block into a digest line (or fallback to original lines).
    fn compress_buffer(&mut self) -> Vec<String> {
        self.state = State::Scanning;

        if self.buffer.is_empty() {
            return vec![];
        }

        let joined = self.buffer.join("\n");
        let rule = &self.rules[self.active_rule_idx];

        if let Some(caps) = rule.extract.captures(&joined) {
            // Build digest by replacing {name} placeholders with captured values
            let digest = render_digest(&rule.digest, &caps);
            self.blocks_compressed += 1;
            self.buffer.clear();
            vec![digest]
        } else {
            // Extract regex didn't match — emit original lines unchanged (graceful fallback)
            std::mem::take(&mut self.buffer)
        }
    }
}

/// Render a digest template by replacing `{name}` with named capture values.
/// Handles `[{name}]` specially: if the capture is empty/missing, the brackets
/// are omitted entirely.
fn render_digest(template: &str, caps: &regex::Captures<'_>) -> String {
    let mut result = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '[' {
            // Look ahead for [{name}] pattern
            let rest: String = chars.clone().collect();
            if let Some(close_bracket) = rest.find(']') {
                let inner = &rest[..close_bracket];
                if inner.starts_with('{') && inner.ends_with('}') {
                    let name = &inner[1..inner.len() - 1];
                    let value = caps.name(name).map(|m| m.as_str()).unwrap_or("");
                    if !value.is_empty() {
                        result.push('[');
                        result.push_str(value);
                        result.push(']');
                    }
                    // Advance past the content + ']'
                    for _ in 0..=close_bracket {
                        chars.next();
                    }
                    continue;
                }
            }
            result.push(ch);
        } else if ch == '{' {
            // Look for {name} pattern
            let rest: String = chars.clone().collect();
            if let Some(close_brace) = rest.find('}') {
                let name = &rest[..close_brace];
                if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    let value = caps.name(name).map(|m| m.as_str()).unwrap_or("");
                    result.push_str(value);
                    // Advance past the content + '}'
                    for _ in 0..=close_brace {
                        chars.next();
                    }
                    continue;
                }
            }
            result.push(ch);
        } else {
            result.push(ch);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::grammar::load_grammar_from_str;

    fn rustc_block_rules() -> Vec<BlockRule> {
        let toml = r#"
[tool]
name = "test"
detect = ["test"]

[[block]]
start   = '^(warning|error)(\[.+\])?:\s+'
end     = '^\s*$'
extract = '(?P<level>warning|error)(?:\[(?P<code>.+?)\])?:\s+(?P<message>.+)\n\s+-->\s+(?P<file>[^\s:]+):(?P<line>\d+)'
digest  = "{level}[{code}]: {message} → {file}:{line}"
"#;
        let grammar = load_grammar_from_str(toml).unwrap();
        grammar.block
    }

    #[test]
    fn test_passthrough_no_rules() {
        let mut bc = BlockCompressor::new(vec![]);
        assert_eq!(bc.feed("hello".into()), vec!["hello"]);
        assert_eq!(bc.feed("world".into()), vec!["world"]);
        assert!(bc.flush().is_empty());
        assert_eq!(bc.blocks_compressed, 0);
    }

    #[test]
    fn test_passthrough_non_matching_lines() {
        let rules = rustc_block_rules();
        let mut bc = BlockCompressor::new(rules);
        assert_eq!(bc.feed("Compiling mish v0.1.0".into()), vec!["Compiling mish v0.1.0"]);
        assert_eq!(bc.feed("Finished dev profile".into()), vec!["Finished dev profile"]);
        assert_eq!(bc.blocks_compressed, 0);
    }

    #[test]
    fn test_single_block_compression() {
        let rules = rustc_block_rules();
        let mut bc = BlockCompressor::new(rules);

        // Feed a rustc warning block
        let lines = vec![
            "warning[dead_code]: field `config` is never read",
            "  --> src/mcp/server.rs:71:5",
            "   |",
            "67 | pub struct McpServer {",
            "   |            --------- field in this struct",
            "...",
            "71 |     config: Arc<MishConfig>,",
            "   |     ^^^^^^",
            "   = note: `#[warn(dead_code)]` on by default",
        ];

        let mut output = Vec::new();
        for line in lines {
            output.extend(bc.feed(line.to_string()));
        }
        // Block is still buffered (no end pattern hit yet)
        assert!(output.is_empty());

        // Feed blank line (end pattern)
        output.extend(bc.feed("".to_string()));

        assert_eq!(output.len(), 2); // digest + blank line
        assert_eq!(output[0], "warning[dead_code]: field `config` is never read → src/mcp/server.rs:71");
        assert_eq!(output[1], ""); // blank line passthrough
        assert_eq!(bc.blocks_compressed, 1);
    }

    #[test]
    fn test_multiple_sequential_blocks() {
        let rules = rustc_block_rules();
        let mut bc = BlockCompressor::new(rules);

        let input = vec![
            "warning[dead_code]: field `config` is never read",
            "  --> src/mcp/server.rs:71:5",
            "   |",
            "71 |     config: Arc<MishConfig>,",
            "   |     ^^^^^^",
            "",
            "warning[dead_code]: field `logger` is never read",
            "  --> src/mcp/server.rs:72:5",
            "   |",
            "72 |     logger: Logger,",
            "   |     ^^^^^^",
            "",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }

        // Should have 2 digests + 2 blank lines
        assert_eq!(output.len(), 4);
        assert!(output[0].contains("field `config` is never read"));
        assert!(output[0].contains("server.rs:71"));
        assert_eq!(output[1], "");
        assert!(output[2].contains("field `logger` is never read"));
        assert!(output[2].contains("server.rs:72"));
        assert_eq!(output[3], "");
        assert_eq!(bc.blocks_compressed, 2);
    }

    #[test]
    fn test_missing_optional_captures() {
        let rules = rustc_block_rules();
        let mut bc = BlockCompressor::new(rules);

        // Warning without a code in brackets
        let input = vec![
            "warning: unused variable `x`",
            "  --> src/main.rs:10:5",
            "   |",
            "10 |     let x = 42;",
            "   |         ^",
            "",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }

        assert_eq!(output.len(), 2);
        // [code] should be omitted when empty
        assert_eq!(output[0], "warning: unused variable `x` → src/main.rs:10");
        assert_eq!(bc.blocks_compressed, 1);
    }

    #[test]
    fn test_extract_no_match_graceful_fallback() {
        let rules = rustc_block_rules();
        let mut bc = BlockCompressor::new(rules);

        // Start matches but extract won't match (no --> line)
        let input = vec![
            "warning: some unusual format",
            "  no arrow line here",
            "  just some text",
            "",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }

        // Should fall back to original lines
        assert_eq!(output.len(), 4);
        assert_eq!(output[0], "warning: some unusual format");
        assert_eq!(output[1], "  no arrow line here");
        assert_eq!(output[2], "  just some text");
        assert_eq!(output[3], "");
        assert_eq!(bc.blocks_compressed, 0);
    }

    #[test]
    fn test_block_interrupted_by_new_block_start() {
        let rules = rustc_block_rules();
        let mut bc = BlockCompressor::new(rules);

        // First block starts but before end, a new block starts
        let input = vec![
            "warning[dead_code]: field `a` is never read",
            "  --> src/lib.rs:1:5",
            "   |",
            "1  |     a: u32,",
            "   |     ^",
            // No blank line — new block starts immediately
            "error[E0425]: cannot find value `x`",
            "  --> src/lib.rs:10:5",
            "   |",
            "10 |     println!(\"{}\", x);",
            "   |                     ^",
            "",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }

        assert_eq!(output.len(), 3); // first digest + second digest + blank
        assert!(output[0].contains("field `a` is never read"));
        assert!(output[1].contains("cannot find value `x`"));
        assert_eq!(output[2], "");
        assert_eq!(bc.blocks_compressed, 2);
    }

    #[test]
    fn test_flush_at_end_of_stream() {
        let rules = rustc_block_rules();
        let mut bc = BlockCompressor::new(rules);

        // Block without a trailing blank line
        let input = vec![
            "warning[dead_code]: field `x` is never read",
            "  --> src/main.rs:5:5",
            "   |",
            "5  |     x: u32,",
            "   |     ^",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }
        assert!(output.is_empty()); // still buffered

        // Flush at end of stream
        output.extend(bc.flush());
        assert_eq!(output.len(), 1);
        assert!(output[0].contains("field `x` is never read"));
        assert!(output[0].contains("src/main.rs:5"));
        assert_eq!(bc.blocks_compressed, 1);
    }

    #[test]
    fn test_empty_block_start_then_immediate_end() {
        let rules = rustc_block_rules();
        let mut bc = BlockCompressor::new(rules);

        // Start line immediately followed by end (blank line)
        let input = vec![
            "warning: something",
            "",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }

        // Extract won't match (no --> line) — falls back to original
        assert_eq!(output.len(), 2);
        assert_eq!(output[0], "warning: something");
        assert_eq!(output[1], "");
        assert_eq!(bc.blocks_compressed, 0);
    }

    #[test]
    fn test_mixed_blocks_and_normal_lines() {
        let rules = rustc_block_rules();
        let mut bc = BlockCompressor::new(rules);

        let input = vec![
            "Compiling mish v0.1.0",
            "warning[dead_code]: unused field `x`",
            "  --> src/lib.rs:5:5",
            "   |",
            "5  |     x: u32,",
            "   |     ^",
            "",
            "Finished dev profile in 2.5s",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }
        output.extend(bc.flush());

        assert_eq!(output.len(), 4);
        assert_eq!(output[0], "Compiling mish v0.1.0");
        assert!(output[1].contains("unused field `x`"));
        assert!(output[1].contains("src/lib.rs:5"));
        assert_eq!(output[2], "");
        assert_eq!(output[3], "Finished dev profile in 2.5s");
    }

    // ---- GCC block compression tests ----

    fn gcc_block_rules() -> Vec<BlockRule> {
        let toml = r#"
[tool]
name = "test"
detect = ["test"]

[[block]]
start   = '^\S+:\d+:\d+:\s+(warning|error|fatal error):'
end     = '^\s*$'
extract = '(?P<file>[^\s:]+):(?P<line>\d+):\d+:\s+(?P<level>warning|error|fatal error):\s+(?P<message>[^\n]+?)(?:\s+\[(?P<code>-W[^\]]+)\])?\n'
digest  = "{level}[{code}]: {message} → {file}:{line}"
"#;
        let grammar = load_grammar_from_str(toml).unwrap();
        grammar.block
    }

    #[test]
    fn test_gcc_warning_block_with_code() {
        let rules = gcc_block_rules();
        let mut bc = BlockCompressor::new(rules);

        let input = vec![
            "main.c:15:5: warning: implicit declaration of function 'gets' [-Wimplicit-function-declaration]",
            "   15 |     gets(buffer);",
            "      |     ^~~~",
            "",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }

        assert_eq!(output.len(), 2); // digest + blank
        assert_eq!(
            output[0],
            "warning[-Wimplicit-function-declaration]: implicit declaration of function 'gets' \u{2192} main.c:15"
        );
        assert_eq!(output[1], "");
        assert_eq!(bc.blocks_compressed, 1);
    }

    #[test]
    fn test_gcc_warning_without_code() {
        let rules = gcc_block_rules();
        let mut bc = BlockCompressor::new(rules);

        let input = vec![
            "main.c:23:12: warning: unused variable 'count' [-Wunused-variable]",
            "   23 |     int    count = 0;",
            "      |            ^~~~~",
            "",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }

        assert_eq!(output.len(), 2);
        assert!(output[0].starts_with("warning[-Wunused-variable]:"));
        assert!(output[0].contains("main.c:23"));
        assert_eq!(bc.blocks_compressed, 1);
    }

    #[test]
    fn test_gcc_error_block() {
        let rules = gcc_block_rules();
        let mut bc = BlockCompressor::new(rules);

        let input = vec![
            "main.c:10:5: error: use of undeclared identifier 'foo'",
            "   10 |     foo();",
            "      |     ^",
            "",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }

        assert_eq!(output.len(), 2);
        assert_eq!(
            output[0],
            "error: use of undeclared identifier 'foo' \u{2192} main.c:10"
        );
        assert_eq!(bc.blocks_compressed, 1);
    }

    #[test]
    fn test_gcc_fatal_error_block() {
        let rules = gcc_block_rules();
        let mut bc = BlockCompressor::new(rules);

        let input = vec![
            "main.c:3:10: fatal error: 'missing.h' file not found",
            "    3 | #include \"missing.h\"",
            "      |          ^~~~~~~~~~~",
            "",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }

        assert_eq!(output.len(), 2);
        assert_eq!(
            output[0],
            "fatal error: 'missing.h' file not found \u{2192} main.c:3"
        );
        assert_eq!(bc.blocks_compressed, 1);
    }

    #[test]
    fn test_gcc_multiple_blocks() {
        let rules = gcc_block_rules();
        let mut bc = BlockCompressor::new(rules);

        let input = vec![
            "main.c:15:5: warning: implicit declaration of function 'gets' [-Wimplicit-function-declaration]",
            "   15 |     gets(buffer);",
            "      |     ^~~~",
            "",
            "main.c:23:12: warning: unused variable 'count' [-Wunused-variable]",
            "   23 |     int    count = 0;",
            "      |            ^~~~~",
            "",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }

        assert_eq!(output.len(), 4); // 2 digests + 2 blanks
        assert!(output[0].contains("implicit declaration"));
        assert!(output[0].contains("main.c:15"));
        assert!(output[2].contains("unused variable"));
        assert!(output[2].contains("main.c:23"));
        assert_eq!(bc.blocks_compressed, 2);
    }

    #[test]
    fn test_gcc_flush_without_trailing_blank() {
        let rules = gcc_block_rules();
        let mut bc = BlockCompressor::new(rules);

        let input = vec![
            "main.c:10:5: error: use of undeclared identifier 'foo'",
            "   10 |     foo();",
            "      |     ^",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }
        assert!(output.is_empty());

        output.extend(bc.flush());
        assert_eq!(output.len(), 1);
        assert!(output[0].contains("use of undeclared identifier"));
        assert!(output[0].contains("main.c:10"));
        assert_eq!(bc.blocks_compressed, 1);
    }

    // ---- Webpack block compression tests ----

    fn webpack_block_rules() -> Vec<BlockRule> {
        let toml = r#"
[tool]
name = "test"
detect = ["test"]

[[block]]
start   = '^ERROR in '
end     = '^\s*$'
extract = 'ERROR in (?P<file>\S+)(?:\s+\d+:\d+-\d+)?\n(?P<message>.+?)(?:\n|$)'
digest  = "error: {message} → {file}"

[[block]]
start   = '^WARNING in '
end     = '^\s*$'
extract = 'WARNING in (?P<file>\S+)\n(?P<message>.+?)(?:\n|$)'
digest  = "warning: {message} → {file}"
"#;
        let grammar = load_grammar_from_str(toml).unwrap();
        grammar.block
    }

    #[test]
    fn test_webpack_error_module_not_found() {
        let rules = webpack_block_rules();
        let mut bc = BlockCompressor::new(rules);

        let input = vec![
            "ERROR in ./src/index.ts 5:0-30",
            "Module not found: Error: Can't resolve './utils' in '/app/src'",
            "",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }

        assert_eq!(output.len(), 2); // digest + blank
        assert_eq!(
            output[0],
            "error: Module not found: Error: Can't resolve './utils' in '/app/src' \u{2192} ./src/index.ts"
        );
        assert_eq!(output[1], "");
        assert_eq!(bc.blocks_compressed, 1);
    }

    #[test]
    fn test_webpack_error_without_location() {
        let rules = webpack_block_rules();
        let mut bc = BlockCompressor::new(rules);

        let input = vec![
            "ERROR in ./src/app.tsx",
            "Module build failed (from ./node_modules/ts-loader/dist/cjs.js):",
            "SyntaxError: Unexpected token (12:3)",
            "",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }

        assert_eq!(output.len(), 2);
        assert_eq!(
            output[0],
            "error: Module build failed (from ./node_modules/ts-loader/dist/cjs.js): \u{2192} ./src/app.tsx"
        );
        assert_eq!(output[1], "");
        assert_eq!(bc.blocks_compressed, 1);
    }

    #[test]
    fn test_webpack_warning_block() {
        let rules = webpack_block_rules();
        let mut bc = BlockCompressor::new(rules);

        let input = vec![
            "WARNING in ./src/legacy.js",
            "Module Warning (from ./node_modules/eslint-loader/dist/cjs.js):",
            "",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }

        assert_eq!(output.len(), 2);
        assert_eq!(
            output[0],
            "warning: Module Warning (from ./node_modules/eslint-loader/dist/cjs.js): \u{2192} ./src/legacy.js"
        );
        assert_eq!(output[1], "");
        assert_eq!(bc.blocks_compressed, 1);
    }

    #[test]
    fn test_webpack_multiple_error_blocks() {
        let rules = webpack_block_rules();
        let mut bc = BlockCompressor::new(rules);

        let input = vec![
            "ERROR in ./src/index.ts 5:0-30",
            "Module not found: Error: Can't resolve './utils' in '/app/src'",
            "",
            "ERROR in ./src/app.ts 12:3-15",
            "Module not found: Error: Can't resolve 'lodash' in '/app/src'",
            "",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }

        assert_eq!(output.len(), 4); // 2 digests + 2 blanks
        assert!(output[0].contains("Can't resolve './utils'"));
        assert!(output[0].contains("./src/index.ts"));
        assert!(output[2].contains("Can't resolve 'lodash'"));
        assert!(output[2].contains("./src/app.ts"));
        assert_eq!(bc.blocks_compressed, 2);
    }

    #[test]
    fn test_webpack_error_flush_without_trailing_blank() {
        let rules = webpack_block_rules();
        let mut bc = BlockCompressor::new(rules);

        let input = vec![
            "ERROR in ./src/broken.js",
            "Module parse failed: Unexpected token (1:0)",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }
        assert!(output.is_empty());

        output.extend(bc.flush());
        assert_eq!(output.len(), 1);
        assert!(output[0].contains("Module parse failed"));
        assert!(output[0].contains("./src/broken.js"));
        assert_eq!(bc.blocks_compressed, 1);
    }

    // ---- Docker block compression tests ----

    fn docker_block_rules() -> Vec<BlockRule> {
        let toml = r#"
[tool]
name = "test"
detect = ["test"]

[[block]]
start   = '^-{4,}$'
end     = '^-{4,}$'
extract = '>\s+\[(?P<stage>[^\]]+)\]\s+RUN\s+(?P<cmd>[^:]+):\n.*?(?:ERR!|[Ee]rror|ERROR)[:\s]\s*(?P<message>[^\n]+)'
digest  = "error[{stage}]: {cmd} — {message}"

[[block]]
start   = '^ERROR\s+'
end     = '^\s*$'
extract = 'ERROR\s+(?P<message>failed to solve).*?exit code:\s*(?P<code>\d+)'
digest  = "error: {message} (exit {code})"
"#;
        let grammar = load_grammar_from_str(toml).unwrap();
        grammar.block
    }

    #[test]
    fn test_docker_buildkit_delimited_block() {
        let rules = docker_block_rules();
        let mut bc = BlockCompressor::new(rules);

        let input = vec![
            "------",
            " > [build 4/7] RUN npm install:",
            "0.543 npm ERR! code ERESOLVE",
            "0.544 npm ERR! ERESOLVE unable to resolve dependency tree",
            "------",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }

        // Digest + the closing "------" line (end line is emitted)
        assert_eq!(output.len(), 2);
        assert_eq!(output[0], "error[build 4/7]: npm install \u{2014} code ERESOLVE");
        assert_eq!(output[1], "------");
        assert_eq!(bc.blocks_compressed, 1);
    }

    #[test]
    fn test_docker_error_failed_to_solve() {
        let rules = docker_block_rules();
        let mut bc = BlockCompressor::new(rules);

        let input = vec![
            "ERROR failed to solve: process \"/bin/sh -c npm install --production\" did not complete successfully: exit code: 1",
            "",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }

        assert_eq!(output.len(), 2); // digest + blank
        assert_eq!(output[0], "error: failed to solve (exit 1)");
        assert_eq!(output[1], "");
        assert_eq!(bc.blocks_compressed, 1);
    }

    #[test]
    fn test_docker_buildkit_block_with_error_keyword() {
        let rules = docker_block_rules();
        let mut bc = BlockCompressor::new(rules);

        let input = vec![
            "------",
            " > [stage-2 3/5] RUN pip install -r requirements.txt:",
            "1.234 ERROR: Could not find a version that satisfies the requirement numpy==99.0",
            "1.235 ERROR: No matching distribution found for numpy==99.0",
            "------",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }

        assert_eq!(output.len(), 2);
        assert!(output[0].contains("stage-2 3/5"));
        assert!(output[0].contains("pip install -r requirements.txt"));
        assert!(output[0].contains("Could not find a version that satisfies the requirement numpy==99.0"));
        assert_eq!(output[1], "------");
        assert_eq!(bc.blocks_compressed, 1);
    }

    #[test]
    fn test_docker_buildkit_fallback_no_error_keyword() {
        let rules = docker_block_rules();
        let mut bc = BlockCompressor::new(rules);

        // A delimited block without ERR!/error/Error keywords — extract won't match
        let input = vec![
            "------",
            " > [build 2/7] RUN echo hello:",
            "0.100 hello",
            "------",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }

        // Graceful fallback — original lines + closing delimiter
        assert_eq!(output.len(), 4);
        assert_eq!(output[0], "------");
        assert_eq!(output[1], " > [build 2/7] RUN echo hello:");
        assert_eq!(output[2], "0.100 hello");
        assert_eq!(output[3], "------");
        assert_eq!(bc.blocks_compressed, 0);
    }

    #[test]
    fn test_docker_error_flush_without_blank() {
        let rules = docker_block_rules();
        let mut bc = BlockCompressor::new(rules);

        let input = vec![
            "ERROR failed to solve: process \"/bin/sh -c make build\" did not complete successfully: exit code: 2",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }
        assert!(output.is_empty());

        output.extend(bc.flush());
        assert_eq!(output.len(), 1);
        assert_eq!(output[0], "error: failed to solve (exit 2)");
        assert_eq!(bc.blocks_compressed, 1);
    }

    // ---- jest block compression tests ----

    fn jest_block_rules() -> Vec<BlockRule> {
        let toml = r#"
[tool]
name = "test"
detect = ["test"]

[[block]]
start   = '^\s+●\s+'
end     = '^\s*$'
extract = '●\s+(?P<test>[^\n]+)\n\s+(?P<message>[^\n]+)\n(?:[^\n]*\n)*?\s+at\s+\S+\s+\((?P<file>[^:]+):(?P<line>\d+)'
digest  = "FAIL {test}: {message} → {file}:{line}"
"#;
        let grammar = load_grammar_from_str(toml).unwrap();
        grammar.block
    }

    #[test]
    fn test_jest_graceful_fallback_internal_blank() {
        let rules = jest_block_rules();
        let mut bc = BlockCompressor::new(rules);

        // Jest output has blank lines within failure blocks. With end='^\s*$',
        // the first internal blank terminates the block early. Extract can't
        // match a single-line block, so it falls back gracefully.
        let input = vec![
            "  ● math utils › adds two numbers",
            "",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }

        assert_eq!(output.len(), 2);
        assert!(output[0].contains("math utils"));
        assert_eq!(output[1], "");
        assert_eq!(bc.blocks_compressed, 0);
    }

    #[test]
    fn test_jest_compact_error_block() {
        let rules = jest_block_rules();
        let mut bc = BlockCompressor::new(rules);

        // Compact jest failure block without internal blank lines
        let input = vec![
            "  ● api service › fetches data",
            "    TypeError: fetch is not a function",
            "      at fetchData (src/api.js:6:23)",
            "",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }

        assert_eq!(output.len(), 2);
        assert!(output[0].starts_with("FAIL api service"));
        assert!(output[0].contains("TypeError: fetch is not a function"));
        assert!(output[0].contains("src/api.js:6"));
        assert_eq!(output[1], "");
        assert_eq!(bc.blocks_compressed, 1);
    }

    #[test]
    fn test_jest_block_flush_at_end() {
        let rules = jest_block_rules();
        let mut bc = BlockCompressor::new(rules);

        let input = vec![
            "  ● suite › test name",
            "    ReferenceError: x is not defined",
            "      at Object.<anonymous> (src/t.test.js:2:5)",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }
        assert!(output.is_empty());

        output.extend(bc.flush());
        assert_eq!(output.len(), 1);
        assert!(output[0].contains("test name"));
        assert!(output[0].contains("ReferenceError"));
        assert!(output[0].contains("src/t.test.js:2"));
        assert_eq!(bc.blocks_compressed, 1);
    }

    #[test]
    fn test_jest_multiple_compact_blocks() {
        let rules = jest_block_rules();
        let mut bc = BlockCompressor::new(rules);

        let input = vec![
            "  ● suite › test one",
            "    Error: timeout",
            "      at Object.<anonymous> (src/a.test.js:3:18)",
            "",
            "  ● suite › test two",
            "    Error: not found",
            "      at Object.<anonymous> (src/b.test.js:7:21)",
            "",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }

        assert_eq!(output.len(), 4);
        assert!(output[0].contains("test one"));
        assert!(output[0].contains("src/a.test.js:3"));
        assert_eq!(output[1], "");
        assert!(output[2].contains("test two"));
        assert!(output[2].contains("src/b.test.js:7"));
        assert_eq!(output[3], "");
        assert_eq!(bc.blocks_compressed, 2);
    }

    // ---- pytest block compression tests ----

    fn pytest_block_rules() -> Vec<BlockRule> {
        let toml = r#"
[tool]
name = "test"
detect = ["test"]

[[block]]
start   = '^_{3,}\s+\S+\s+_{3,}$'
end     = '^\s*$'
extract = '_{3,}\s+(?P<test>\S+)\s+_{3,}\n(?:[^\n]*\n)*?>\s+(?P<assertion>[^\n]+)\nE\s+(?P<error>[^\n]+)\n(?:[^\n]*\n)*?(?P<file>[^\s:]+):(?P<line>\d+):\s*(?P<exc>\w+)'
digest  = "FAIL {test}: {error} ({exc}) → {file}:{line}"

[[block]]
start   = '^FAILED\s+'
end     = '^\s*$'
extract = 'FAILED\s+(?P<file>[^\s:]+)::(?P<test>\S+)\s+-\s+(?P<error>[^\n]+)'
digest  = "FAIL {file}::{test}: {error}"
"#;
        let grammar = load_grammar_from_str(toml).unwrap();
        grammar.block
    }

    #[test]
    fn test_pytest_assertion_block() {
        let rules = pytest_block_rules();
        let mut bc = BlockCompressor::new(rules);

        let input = vec![
            "______________________________ test_addition ______________________________",
            "    def test_addition():",
            ">       assert 1 + 1 == 3",
            "E       AssertionError: assert 2 == 3",
            "E        +  where 2 = 1 + 1",
            "test_math.py:4: AssertionError",
            "",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }

        assert_eq!(output.len(), 2);
        assert!(output[0].starts_with("FAIL test_addition"));
        assert!(output[0].contains("AssertionError: assert 2 == 3"));
        assert!(output[0].contains("test_math.py:4"));
        assert_eq!(output[1], "");
        assert_eq!(bc.blocks_compressed, 1);
    }

    #[test]
    fn test_pytest_multiple_failure_blocks() {
        let rules = pytest_block_rules();
        let mut bc = BlockCompressor::new(rules);

        let input = vec![
            "______________________________ test_sub ______________________________",
            "    def test_sub():",
            ">       assert 5 - 3 == 1",
            "E       AssertionError: assert 2 == 1",
            "test_math.py:8: AssertionError",
            "",
            "______________________________ test_mul ______________________________",
            "    def test_mul():",
            ">       assert 2 * 3 == 7",
            "E       AssertionError: assert 6 == 7",
            "test_math.py:12: AssertionError",
            "",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }

        assert_eq!(output.len(), 4);
        assert!(output[0].contains("test_sub"));
        assert!(output[0].contains("assert 2 == 1"));
        assert!(output[0].contains("test_math.py:8"));
        assert_eq!(output[1], "");
        assert!(output[2].contains("test_mul"));
        assert!(output[2].contains("assert 6 == 7"));
        assert!(output[2].contains("test_math.py:12"));
        assert_eq!(output[3], "");
        assert_eq!(bc.blocks_compressed, 2);
    }

    #[test]
    fn test_pytest_short_summary_failed_line() {
        let rules = pytest_block_rules();
        let mut bc = BlockCompressor::new(rules);

        let input = vec![
            "FAILED test_math.py::test_addition - assert 2 == 3",
            "",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }

        assert_eq!(output.len(), 2);
        assert_eq!(output[0], "FAIL test_math.py::test_addition: assert 2 == 3");
        assert_eq!(output[1], "");
        assert_eq!(bc.blocks_compressed, 1);
    }

    #[test]
    fn test_pytest_block_flush_at_end() {
        let rules = pytest_block_rules();
        let mut bc = BlockCompressor::new(rules);

        let input = vec![
            "______________________________ test_neg ______________________________",
            "    def test_neg():",
            ">       assert -1 == 1",
            "E       AssertionError: assert -1 == 1",
            "test_math.py:20: AssertionError",
        ];

        let mut output = Vec::new();
        for line in input {
            output.extend(bc.feed(line.to_string()));
        }
        assert!(output.is_empty());

        output.extend(bc.flush());
        assert_eq!(output.len(), 1);
        assert!(output[0].starts_with("FAIL test_neg"));
        assert!(output[0].contains("assert -1 == 1"));
        assert!(output[0].contains("test_math.py:20"));
        assert_eq!(bc.blocks_compressed, 1);
    }
}
