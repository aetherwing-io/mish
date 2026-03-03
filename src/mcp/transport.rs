use serde::Serialize;
use std::fmt;
use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex as TokioMutex;

use crate::mcp::types::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, ERR_INVALID_REQUEST, ERR_PARSE_ERROR};

// ----- TransportError -----

/// Errors that can occur during MCP transport operations.
#[derive(Debug)]
pub enum TransportError {
    /// An I/O error occurred reading from or writing to the transport.
    IoError(std::io::Error),
    /// The received data could not be parsed as valid JSON or JSON-RPC.
    ParseError(String),
    /// The transport reached end-of-file (client disconnected).
    Eof,
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::IoError(e) => write!(f, "transport I/O error: {}", e),
            TransportError::ParseError(msg) => write!(f, "transport parse error: {}", msg),
            TransportError::Eof => write!(f, "transport EOF"),
        }
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TransportError::IoError(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for TransportError {
    fn from(err: std::io::Error) -> Self {
        TransportError::IoError(err)
    }
}

// ----- JSON-RPC Notification -----

/// A JSON-RPC 2.0 notification (no `id` field).
#[derive(Debug, Clone, Serialize)]
struct JsonRpcNotification {
    jsonrpc: String,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<serde_json::Value>,
}

// ----- StdioTransport -----

/// MCP transport over stdio using newline-delimited JSON.
///
/// Each JSON-RPC message is a single line of JSON terminated by `\n`.
/// The transport reads requests from an async reader and writes responses
/// to an async writer.
pub struct StdioTransport<R, W> {
    reader: R,
    writer: W,
}

impl StdioTransport<tokio::io::BufReader<tokio::io::Stdin>, tokio::io::Stdout> {
    /// Create a new transport using real stdin/stdout.
    pub fn new() -> Self {
        StdioTransport {
            reader: tokio::io::BufReader::new(tokio::io::stdin()),
            writer: tokio::io::stdout(),
        }
    }
}

impl<R, W> StdioTransport<R, W> {
    /// Create a transport with custom reader/writer (for testing).
    pub fn with_io(reader: R, writer: W) -> Self {
        StdioTransport { reader, writer }
    }

    /// Consume the transport and return the underlying reader and writer.
    pub fn into_parts(self) -> (R, W) {
        (self.reader, self.writer)
    }
}

impl<R, W> StdioTransport<R, W>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Read the next JSON-RPC request from the input.
    ///
    /// Returns `Ok(None)` on EOF (client disconnect).
    /// Returns `Err(TransportError)` on I/O or parse errors.
    ///
    /// If the line is invalid JSON, this method writes a JSON-RPC parse error
    /// response (code -32700) to the output and continues reading the next line.
    ///
    /// If the JSON is valid but the `jsonrpc` field is not `"2.0"`, this method
    /// writes a JSON-RPC invalid request error (code -32600) and continues.
    pub async fn read_request(&mut self) -> Result<Option<JsonRpcRequest>, TransportError> {
        loop {
            let mut line = String::new();
            let bytes_read = self.reader.read_line(&mut line).await?;

            if bytes_read == 0 {
                return Ok(None);
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            // Try to parse as generic JSON first to extract `id` for error responses.
            let raw_value: serde_json::Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    // Invalid JSON: send parse error with null id.
                    let error_resp = JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: serde_json::Value::Null,
                        result: None,
                        error: Some(JsonRpcError {
                            code: ERR_PARSE_ERROR,
                            message: format!("Parse error: {}", e),
                            data: None,
                        }),
                    };
                    self.write_response(error_resp).await?;
                    continue;
                }
            };

            // Validate jsonrpc field.
            let jsonrpc_field = raw_value.get("jsonrpc");
            let id_value = raw_value
                .get("id")
                .cloned()
                .unwrap_or(serde_json::Value::Null);

            match jsonrpc_field {
                Some(v) if v == "2.0" => {}
                _ => {
                    let error_resp = JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: id_value,
                        result: None,
                        error: Some(JsonRpcError {
                            code: ERR_INVALID_REQUEST,
                            message: "Invalid Request: jsonrpc field must be \"2.0\"".to_string(),
                            data: None,
                        }),
                    };
                    self.write_response(error_resp).await?;
                    continue;
                }
            }

            // Now deserialize into the full JsonRpcRequest.
            match serde_json::from_value::<JsonRpcRequest>(raw_value) {
                Ok(req) => return Ok(Some(req)),
                Err(e) => {
                    let error_resp = JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: id_value,
                        result: None,
                        error: Some(JsonRpcError {
                            code: ERR_INVALID_REQUEST,
                            message: format!("Invalid Request: {}", e),
                            data: None,
                        }),
                    };
                    self.write_response(error_resp).await?;
                    continue;
                }
            }
        }
    }

    /// Write a JSON-RPC response to the output.
    ///
    /// The response is serialized as a single line of JSON followed by `\n`.
    pub async fn write_response(&mut self, response: JsonRpcResponse) -> Result<(), TransportError> {
        let json = serde_json::to_string(&response)
            .map_err(|e| TransportError::ParseError(format!("Failed to serialize response: {}", e)))?;
        self.writer.write_all(json.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Write a JSON-RPC notification to the output (no `id` field).
    ///
    /// Notifications are one-way messages that do not expect a response.
    pub async fn write_notification(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<(), TransportError> {
        let notification = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params: if params.is_null() { None } else { Some(params) },
        };
        let json = serde_json::to_string(&notification)
            .map_err(|e| TransportError::ParseError(format!("Failed to serialize notification: {}", e)))?;
        self.writer.write_all(json.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        self.writer.flush().await?;
        Ok(())
    }
}

// ----- SharedWriter -----

/// Clone-able writer wrapping `Arc<TokioMutex<W>>`.
///
/// Ensures no interleaved bytes on the underlying writer when multiple
/// tasks write concurrently — each write acquires the mutex, serializes,
/// writes, and flushes before releasing.
pub struct SharedWriter<W> {
    inner: Arc<TokioMutex<W>>,
}

impl<W> Clone for SharedWriter<W> {
    fn clone(&self) -> Self {
        SharedWriter {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<W> SharedWriter<W>
where
    W: AsyncWrite + Unpin,
{
    /// Create a new SharedWriter wrapping the given writer.
    pub fn new(writer: W) -> Self {
        SharedWriter {
            inner: Arc::new(TokioMutex::new(writer)),
        }
    }

    /// Write a JSON-RPC response. Acquires lock, serializes, writes, flushes.
    pub async fn write_response(&self, response: JsonRpcResponse) -> Result<(), TransportError> {
        let json = serde_json::to_string(&response)
            .map_err(|e| TransportError::ParseError(format!("Failed to serialize response: {}", e)))?;
        let mut w = self.inner.lock().await;
        w.write_all(json.as_bytes()).await?;
        w.write_all(b"\n").await?;
        w.flush().await?;
        Ok(())
    }

    /// Write a JSON-RPC notification. Acquires lock, serializes, writes, flushes.
    pub async fn write_notification(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<(), TransportError> {
        let notification = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params: if params.is_null() { None } else { Some(params) },
        };
        let json = serde_json::to_string(&notification)
            .map_err(|e| TransportError::ParseError(format!("Failed to serialize notification: {}", e)))?;
        let mut w = self.inner.lock().await;
        w.write_all(json.as_bytes()).await?;
        w.write_all(b"\n").await?;
        w.flush().await?;
        Ok(())
    }
}

// ----- TransportReader -----

/// Reader half of a split transport. Owns the reader + a SharedWriter clone
/// for writing inline error responses (parse errors, invalid requests).
pub struct TransportReader<R, W> {
    reader: R,
    writer: SharedWriter<W>,
}

impl<R, W> TransportReader<R, W>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Read the next JSON-RPC request from the input.
    ///
    /// Same logic as `StdioTransport::read_request()` — writes inline error
    /// responses for parse errors and invalid requests via the SharedWriter.
    pub async fn read_request(&mut self) -> Result<Option<JsonRpcRequest>, TransportError> {
        loop {
            let mut line = String::new();
            let bytes_read = self.reader.read_line(&mut line).await?;

            if bytes_read == 0 {
                return Ok(None);
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let raw_value: serde_json::Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    let error_resp = JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: serde_json::Value::Null,
                        result: None,
                        error: Some(JsonRpcError {
                            code: ERR_PARSE_ERROR,
                            message: format!("Parse error: {}", e),
                            data: None,
                        }),
                    };
                    self.writer.write_response(error_resp).await?;
                    continue;
                }
            };

            let jsonrpc_field = raw_value.get("jsonrpc");
            let id_value = raw_value
                .get("id")
                .cloned()
                .unwrap_or(serde_json::Value::Null);

            match jsonrpc_field {
                Some(v) if v == "2.0" => {}
                _ => {
                    let error_resp = JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: id_value,
                        result: None,
                        error: Some(JsonRpcError {
                            code: ERR_INVALID_REQUEST,
                            message: "Invalid Request: jsonrpc field must be \"2.0\"".to_string(),
                            data: None,
                        }),
                    };
                    self.writer.write_response(error_resp).await?;
                    continue;
                }
            }

            match serde_json::from_value::<JsonRpcRequest>(raw_value) {
                Ok(req) => return Ok(Some(req)),
                Err(e) => {
                    let error_resp = JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: id_value,
                        result: None,
                        error: Some(JsonRpcError {
                            code: ERR_INVALID_REQUEST,
                            message: format!("Invalid Request: {}", e),
                            data: None,
                        }),
                    };
                    self.writer.write_response(error_resp).await?;
                    continue;
                }
            }
        }
    }
}

// ----- into_split -----

impl<R, W> StdioTransport<R, W>
where
    W: AsyncWrite + Unpin,
{
    /// Consume the transport and return a `(TransportReader, SharedWriter)` pair.
    ///
    /// The `SharedWriter` is `Clone`-able so it can be shared across concurrent
    /// tasks. The `TransportReader` owns the reader half and a writer clone for
    /// inline error responses.
    pub fn into_split(self) -> (TransportReader<R, W>, SharedWriter<W>) {
        let shared = SharedWriter::new(self.writer);
        let reader = TransportReader {
            reader: self.reader,
            writer: shared.clone(),
        };
        (reader, shared)
    }
}

// ----- Tests -----

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Helper: create a transport with in-memory reader/writer.
    fn make_transport(
        input: &str,
    ) -> StdioTransport<
        tokio::io::BufReader<std::io::Cursor<Vec<u8>>>,
        Vec<u8>,
    > {
        let reader = tokio::io::BufReader::new(std::io::Cursor::new(input.as_bytes().to_vec()));
        let writer = Vec::new();
        StdioTransport::with_io(reader, writer)
    }

    /// Helper: get what was written to the output.
    fn output_string<R>(transport: &StdioTransport<R, Vec<u8>>) -> String {
        String::from_utf8(transport.writer.clone()).unwrap()
    }

    // ---- Test 1: Read valid JSON-RPC request from simulated stdin ----

    #[tokio::test]
    async fn read_valid_request() {
        let input = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"sh_run"}}"#;
        let input_with_newline = format!("{}\n", input);
        let mut transport = make_transport(&input_with_newline);

        let req = transport.read_request().await.unwrap().unwrap();
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.id, json!(1));
        assert_eq!(req.method, "tools/call");
        assert!(req.params.is_some());
    }

    // ---- Test 2: Write JSON-RPC response to captured stdout ----

    #[tokio::test]
    async fn write_response_to_output() {
        let mut transport = make_transport("");
        let response = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: json!(1),
            result: Some(json!({"exit_code": 0})),
            error: None,
        };

        transport.write_response(response).await.unwrap();

        let output = output_string(&transport);
        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 1);
        assert_eq!(parsed["result"]["exit_code"], 0);
    }

    // ---- Test 3: EOF on stdin returns None ----

    #[tokio::test]
    async fn eof_returns_none() {
        let mut transport = make_transport("");
        let result = transport.read_request().await.unwrap();
        assert!(result.is_none());
    }

    // ---- Test 4: Invalid JSON returns parse error ----

    #[tokio::test]
    async fn invalid_json_returns_parse_error() {
        // Send invalid JSON followed by EOF.
        let input = "this is not json\n";
        let mut transport = make_transport(input);

        // read_request should write a parse error and then return None on EOF.
        let result = transport.read_request().await.unwrap();
        assert!(result.is_none());

        // Check the error response that was written.
        let output = output_string(&transport);
        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert!(parsed["id"].is_null());
        assert_eq!(parsed["error"]["code"], ERR_PARSE_ERROR);
        assert!(parsed["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("Parse error:"));
    }

    // ---- Test 5: Missing jsonrpc field returns invalid request error ----

    #[tokio::test]
    async fn missing_jsonrpc_field_returns_invalid_request() {
        let input = r#"{"id":1,"method":"test"}"#;
        let input_with_newline = format!("{}\n", input);
        let mut transport = make_transport(&input_with_newline);

        let result = transport.read_request().await.unwrap();
        assert!(result.is_none());

        let output = output_string(&transport);
        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 1);
        assert_eq!(parsed["error"]["code"], ERR_INVALID_REQUEST);
        assert!(parsed["error"]["message"]
            .as_str()
            .unwrap()
            .contains("jsonrpc"));
    }

    // ---- Test 6: Request with string id, response echoes same string id ----

    #[tokio::test]
    async fn string_id_preserved() {
        let input = r#"{"jsonrpc":"2.0","id":"abc-123","method":"initialize"}"#;
        let input_with_newline = format!("{}\n", input);
        let mut transport = make_transport(&input_with_newline);

        let req = transport.read_request().await.unwrap().unwrap();
        assert_eq!(req.id, json!("abc-123"));

        // Write response with same id.
        let response = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: req.id.clone(),
            result: Some(json!({})),
            error: None,
        };
        transport.write_response(response).await.unwrap();

        let output = output_string(&transport);
        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(parsed["id"], "abc-123");
    }

    // ---- Test 7: Request with numeric id, response echoes same numeric id ----

    #[tokio::test]
    async fn numeric_id_preserved() {
        let input = r#"{"jsonrpc":"2.0","id":42,"method":"test"}"#;
        let input_with_newline = format!("{}\n", input);
        let mut transport = make_transport(&input_with_newline);

        let req = transport.read_request().await.unwrap().unwrap();
        assert_eq!(req.id, json!(42));

        let response = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: req.id.clone(),
            result: Some(json!({"ok": true})),
            error: None,
        };
        transport.write_response(response).await.unwrap();

        let output = output_string(&transport);
        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(parsed["id"], 42);
    }

    // ---- Test 8: Multiple requests read sequentially ----

    #[tokio::test]
    async fn multiple_requests_sequential() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#, "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#, "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"sh_run"}}"#, "\n",
        );
        let mut transport = make_transport(input);

        let req1 = transport.read_request().await.unwrap().unwrap();
        assert_eq!(req1.id, json!(1));
        assert_eq!(req1.method, "initialize");

        let req2 = transport.read_request().await.unwrap().unwrap();
        assert_eq!(req2.id, json!(2));
        assert_eq!(req2.method, "tools/list");

        let req3 = transport.read_request().await.unwrap().unwrap();
        assert_eq!(req3.id, json!(3));
        assert_eq!(req3.method, "tools/call");

        // Next read should be EOF.
        let req4 = transport.read_request().await.unwrap();
        assert!(req4.is_none());
    }

    // ---- Test 9: Response is a single line (no embedded newlines) ----

    #[tokio::test]
    async fn response_is_single_line() {
        let mut transport = make_transport("");

        // Response with a value containing embedded newlines in a string.
        let response = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: json!(1),
            result: Some(json!({"output": "line1\nline2\nline3"})),
            error: None,
        };
        transport.write_response(response).await.unwrap();

        let output = output_string(&transport);
        // The JSON serialization should escape newlines within strings,
        // so the entire response is on one line.
        let lines: Vec<&str> = output.trim().split('\n').collect();
        assert_eq!(lines.len(), 1, "Response must be a single line, got: {:?}", lines);
    }

    // ---- Test 10: Write notification ----

    #[tokio::test]
    async fn write_notification_no_id() {
        let mut transport = make_transport("");

        transport
            .write_notification("notifications/initialized", json!({}))
            .await
            .unwrap();

        let output = output_string(&transport);
        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["method"], "notifications/initialized");
        // Notifications must NOT have an `id` field.
        assert!(parsed.get("id").is_none());
    }

    // ---- Test 11: Notification with null params omits params ----

    #[tokio::test]
    async fn notification_null_params_omitted() {
        let mut transport = make_transport("");

        transport
            .write_notification("test/event", serde_json::Value::Null)
            .await
            .unwrap();

        let output = output_string(&transport);
        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert!(parsed.get("params").is_none());
    }

    // ---- Test 12: Wrong jsonrpc version returns invalid request ----

    #[tokio::test]
    async fn wrong_jsonrpc_version_returns_invalid_request() {
        let input = r#"{"jsonrpc":"1.0","id":5,"method":"test"}"#;
        let input_with_newline = format!("{}\n", input);
        let mut transport = make_transport(&input_with_newline);

        let result = transport.read_request().await.unwrap();
        assert!(result.is_none());

        let output = output_string(&transport);
        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(parsed["error"]["code"], ERR_INVALID_REQUEST);
        assert_eq!(parsed["id"], 5);
    }

    // ---- Test 13: Empty lines are skipped ----

    #[tokio::test]
    async fn empty_lines_skipped() {
        let input = concat!(
            "\n",
            "\n",
            r#"{"jsonrpc":"2.0","id":1,"method":"test"}"#, "\n",
        );
        let mut transport = make_transport(input);

        let req = transport.read_request().await.unwrap().unwrap();
        assert_eq!(req.id, json!(1));
        assert_eq!(req.method, "test");
    }

    // ---- Test 14: Invalid JSON followed by valid request ----

    #[tokio::test]
    async fn invalid_json_then_valid_request() {
        let input = concat!(
            "not json\n",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#, "\n",
        );
        let mut transport = make_transport(input);

        // Should skip the invalid line (writing error) and return the valid request.
        let req = transport.read_request().await.unwrap().unwrap();
        assert_eq!(req.id, json!(1));
        assert_eq!(req.method, "tools/list");

        // The error response for the invalid JSON was written.
        let output = output_string(&transport);
        let first_line = output.lines().next().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(first_line).unwrap();
        assert_eq!(parsed["error"]["code"], ERR_PARSE_ERROR);
    }

    // ---- Test 15: TransportError Display ----

    #[test]
    fn transport_error_display() {
        let io_err = TransportError::IoError(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "pipe broke",
        ));
        assert!(format!("{}", io_err).contains("pipe broke"));

        let parse_err = TransportError::ParseError("bad json".to_string());
        assert!(format!("{}", parse_err).contains("bad json"));

        let eof_err = TransportError::Eof;
        assert!(format!("{}", eof_err).contains("EOF"));
    }

    // ---- Test 16: TransportError From<io::Error> ----

    #[test]
    fn transport_error_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        let transport_err: TransportError = io_err.into();
        match transport_err {
            TransportError::IoError(e) => assert_eq!(e.kind(), std::io::ErrorKind::ConnectionReset),
            _ => panic!("Expected IoError variant"),
        }
    }

    // ---- Test 17: SharedWriter concurrent writes produce valid JSON lines ----

    #[tokio::test]
    async fn shared_writer_concurrent_writes() {
        let buf: Vec<u8> = Vec::new();
        let writer = SharedWriter::new(buf);

        let mut handles = Vec::new();
        for i in 0..10u32 {
            let w = writer.clone();
            handles.push(tokio::spawn(async move {
                let resp = JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: json!(i),
                    result: Some(json!({"task": i})),
                    error: None,
                };
                w.write_response(resp).await.unwrap();
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        // Read back all written data
        let data = writer.inner.lock().await;
        let text = String::from_utf8(data.clone()).unwrap();
        let lines: Vec<&str> = text.trim().lines().collect();
        assert_eq!(lines.len(), 10, "Expected 10 JSON lines, got {}", lines.len());

        // Each line must be valid JSON with correct structure
        for line in &lines {
            let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(parsed["jsonrpc"], "2.0");
            assert!(parsed["result"]["task"].is_number());
        }
    }

    // ---- Test 18: into_split reader works ----

    #[tokio::test]
    async fn into_split_reader_works() {
        let input = r#"{"jsonrpc":"2.0","id":1,"method":"test"}"#;
        let input_with_newline = format!("{}\n", input);
        let transport = make_transport(&input_with_newline);
        let (mut reader, _writer) = transport.into_split();

        let req = reader.read_request().await.unwrap().unwrap();
        assert_eq!(req.id, json!(1));
        assert_eq!(req.method, "test");
    }

    // ---- Test 19: into_split writer works ----

    #[tokio::test]
    async fn into_split_writer_works() {
        let transport = make_transport("");
        let (_reader, writer) = transport.into_split();

        let resp = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: json!(42),
            result: Some(json!({"ok": true})),
            error: None,
        };
        writer.write_response(resp).await.unwrap();

        let data = writer.inner.lock().await;
        let text = String::from_utf8(data.clone()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(parsed["id"], 42);
        assert_eq!(parsed["result"]["ok"], true);
    }
}
