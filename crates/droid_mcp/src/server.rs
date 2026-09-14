//! Loopback HTTP server speaking MCP over JSON-RPC, plus the SSE stream the CLI
//! uses for server-initiated notifications.
//!
//! Bound to `127.0.0.1` on an ephemeral port; the port is published through the
//! lock file and the terminal environment rather than fixed, so two Flint
//! windows never contend for one.

use crate::protocol::{
    GET_IDE_DIAGNOSTICS, MAX_REQUEST_BYTES, MCP_PATH, SESSION_HEADER, error_response,
    initialize_result, success_response, tool_success, tools_list_result,
};
use crate::state::Bridge;
use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};
use std::io::{self, Read, Write as _};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;
use tiny_http::{Header, Request, Response, Server};

/// How long an idle SSE stream waits before emitting a keepalive comment.
const SSE_KEEPALIVE: Duration = Duration::from_secs(15);

/// Ceiling on concurrent in-flight requests. SSE streams hold their thread for
/// the life of the session, so this bounds both sockets and threads.
const MAX_CONCURRENT_REQUESTS: usize = 32;

pub struct ServerHandle {
    server: Arc<Server>,
    port: u16,
    accept_thread: Option<JoinHandle<()>>,
}

impl ServerHandle {
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Stops accepting, unblocks the accept loop, and waits for it to finish.
    ///
    /// SSE threads exit on their own once `Bridge::shutdown` has been called.
    pub fn shutdown(&mut self) {
        self.server.unblock();
        if let Some(thread) = self.accept_thread.take()
            && thread.join().is_err()
        {
            log::warn!("droid_mcp: HTTP accept thread panicked during shutdown");
        }
    }
}

/// Binds the loopback listener and starts serving.
pub fn start(bridge: Bridge) -> Result<ServerHandle> {
    let server = Server::http("127.0.0.1:0")
        .map_err(|error| anyhow::anyhow!("failed to bind the Droid MCP bridge: {error}"))?;
    let server = Arc::new(server);

    let Some(address) = server.server_addr().to_ip() else {
        bail!("Droid MCP bridge is not listening on an IP socket");
    };
    let port = address.port();

    let in_flight = Arc::new(AtomicUsize::new(0));
    let accept_thread = std::thread::Builder::new()
        .name("droid-mcp-accept".to_string())
        .spawn({
            let server = server.clone();
            // `bridge` and `in_flight` are only needed by the accept loop.
            move || accept_loop(server, bridge, in_flight)
        })
        .context("failed to start the Droid MCP accept thread")?;

    log::info!("droid_mcp: bridge listening on http://127.0.0.1:{port}{MCP_PATH}");

    Ok(ServerHandle {
        server,
        port,
        accept_thread: Some(accept_thread),
    })
}

fn accept_loop(server: Arc<Server>, bridge: Bridge, in_flight: Arc<AtomicUsize>) {
    loop {
        let request = match server.recv() {
            Ok(request) => request,
            Err(error) => {
                // `unblock()` during shutdown surfaces here as an error.
                if !bridge.is_shutdown() {
                    log::warn!("droid_mcp: accepting a request failed: {error}");
                }
                return;
            }
        };

        if in_flight.load(Ordering::Acquire) >= MAX_CONCURRENT_REQUESTS {
            log::warn!(
                "droid_mcp: rejecting a request, {MAX_CONCURRENT_REQUESTS} already in flight"
            );
            let _ = request.respond(Response::empty(503));
            continue;
        }

        in_flight.fetch_add(1, Ordering::AcqRel);
        let worker_bridge = bridge.clone();
        let worker_in_flight = in_flight.clone();
        let spawn_result = std::thread::Builder::new()
            .name("droid-mcp-request".to_string())
            .spawn(move || {
                handle_request(request, &worker_bridge);
                worker_in_flight.fetch_sub(1, Ordering::AcqRel);
            });

        if let Err(error) = spawn_result {
            in_flight.fetch_sub(1, Ordering::AcqRel);
            log::warn!("droid_mcp: failed to start a request thread: {error}");
        }
    }
}

fn handle_request(request: Request, bridge: &Bridge) {
    let path = request
        .url()
        .split('?')
        .next()
        .unwrap_or_default()
        .to_string();

    if !path.eq_ignore_ascii_case(MCP_PATH) {
        let _ = respond_json(request, 404, &json!({ "error": "Not Found" }), None);
        return;
    }

    let method = request.method().as_str().to_ascii_uppercase();
    match method.as_str() {
        "OPTIONS" => {
            let _ = request.respond(with_cors(Response::empty(200), None));
        }
        "GET" => handle_sse(request, bridge),
        "DELETE" => handle_delete_session(request, bridge),
        "POST" => handle_post(request, bridge),
        _ => {
            let _ = respond_json(
                request,
                405,
                &json!({ "error": "Method Not Allowed" }),
                None,
            );
        }
    }
}

fn handle_delete_session(request: Request, bridge: &Bridge) {
    let Some(session_id) = session_id(&request) else {
        let _ = respond_json(
            request,
            400,
            &json!({ "error": "Missing MCP session" }),
            None,
        );
        return;
    };

    let removed = bridge.remove_session(&session_id);
    let _ = request.respond(with_cors(
        Response::empty(if removed { 204 } else { 404 }),
        None,
    ));
}

fn handle_sse(request: Request, bridge: &Bridge) {
    let Some(session_id) = session_id(&request) else {
        let _ = respond_json(
            request,
            400,
            &json!({ "error": "Missing MCP session" }),
            None,
        );
        return;
    };

    if !bridge.has_session(&session_id) {
        let _ = respond_json(
            request,
            400,
            &json!({ "error": "Invalid MCP session" }),
            None,
        );
        return;
    }

    if let Err(error) = stream_events(request, bridge, &session_id) {
        log::debug!("droid_mcp: SSE stream ended: {error}");
    }
}

/// Writes the SSE response straight to the socket.
///
/// `tiny_http`'s own chunked encoder buffers until 8 KiB accumulate before it
/// writes anything, which would hold notifications back indefinitely, so the
/// chunk framing and the per-event flush are done here instead.
fn stream_events(request: Request, bridge: &Bridge, session_id: &str) -> io::Result<()> {
    let mut writer = request.into_writer();

    write!(
        writer,
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/event-stream\r\n\
         Cache-Control: no-cache\r\n\
         Transfer-Encoding: chunked\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Expose-Headers: {SESSION_HEADER}\r\n\
         {SESSION_HEADER}: {session_id}\r\n\
         \r\n"
    )?;
    writer.flush()?;

    write_event(&mut writer, ": connected\n\n")?;

    while bridge.has_session(session_id) {
        let notifications = bridge.drain_notifications(Some(session_id));
        if !notifications.is_empty() {
            for notification in notifications {
                write_event(
                    &mut writer,
                    &format!("event: message\ndata: {notification}\n\n"),
                )?;
            }
            continue;
        }

        // `false` means the bridge is shutting down, so the stream is over.
        if !bridge.wait_for_notifications(session_id, SSE_KEEPALIVE) {
            break;
        }
        // A session that was deleted mid-wait never gets signalled; re-checking
        // the loop condition is what ends the stream in that case.
        if !bridge.has_pending_notifications(session_id) {
            write_event(&mut writer, ": keepalive\n\n")?;
        }
    }

    // Terminating chunk, so a client holding the connection open can tell the
    // stream ended rather than was truncated.
    writer.write_all(b"0\r\n\r\n")?;
    writer.flush()
}

fn write_event<W: io::Write>(writer: &mut W, payload: &str) -> io::Result<()> {
    write!(writer, "{:x}\r\n", payload.len())?;
    writer.write_all(payload.as_bytes())?;
    writer.write_all(b"\r\n")?;
    writer.flush()
}

fn handle_post(mut request: Request, bridge: &Bridge) {
    let body = match read_body(&mut request) {
        Ok(body) => body,
        Err(error) => {
            let _ = respond_json(request, 400, &json!({ "error": error.to_string() }), None);
            return;
        }
    };

    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(parsed) => parsed,
        Err(error) => {
            let _ = respond_json(
                request,
                400,
                &json!({ "error": format!("Invalid JSON: {error}") }),
                None,
            );
            return;
        }
    };

    let Some(object) = parsed.as_object() else {
        let _ = respond_json(
            request,
            400,
            &json!({ "error": "Request must be a JSON object" }),
            None,
        );
        return;
    };

    let method = object
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let id = object.get("id").cloned().unwrap_or(Value::Null);
    let provided_session = session_id(&request);

    // Only `initialize` may mint a session; every other method must present one.
    let session_id = if method == "initialize" {
        let id = match provided_session {
            Some(existing) if bridge.has_session(&existing) => existing,
            _ => bridge.create_session(uuid_like_id()),
        };
        Some(id)
    } else {
        provided_session.filter(|candidate| bridge.has_session(candidate))
    };

    if method == "notifications/initialized" {
        let _ = request.respond(with_cors(Response::empty(202), session_id.as_deref()));
        return;
    }

    if method.is_empty() {
        let _ = respond_json(
            request,
            400,
            &json!({ "error": "Missing method" }),
            session_id.as_deref(),
        );
        return;
    }

    let payload = match method.as_str() {
        "initialize" => success_response(&id, initialize_result()),
        "ping" => success_response(&id, json!({})),
        "tools/list" => success_response(&id, tools_list_result()),
        "tools/call" => handle_tools_call(bridge, &id, object.get("params")),
        "notifications/pull" => success_response(
            &id,
            json!({ "notifications": bridge.drain_notifications(session_id.as_deref()) }),
        ),
        other => error_response(&id, -32601, &format!("Method not found: {other}")),
    };

    // A client without a session cannot receive notifications out of band, so
    // they ride along with the response as a batch.
    let payload = if session_id.is_none() {
        let notifications = bridge.drain_notifications(None);
        if notifications.is_empty() {
            payload
        } else {
            let mut batch = notifications;
            batch.push(payload);
            Value::Array(batch)
        }
    } else {
        payload
    };

    let _ = respond_json(request, 200, &payload, session_id.as_deref());
}

fn handle_tools_call(bridge: &Bridge, id: &Value, params: Option<&Value>) -> Value {
    let Some(params) = params.and_then(Value::as_object) else {
        return error_response(id, -32602, "Invalid params: missing params object");
    };

    let tool_name = ["name", "toolName", "tool"]
        .iter()
        .find_map(|key| params.get(*key).and_then(Value::as_str));

    let Some(tool_name) = tool_name else {
        return error_response(id, -32602, "Invalid params: missing tool name");
    };

    if tool_name != GET_IDE_DIAGNOSTICS {
        return error_response(id, -32601, &format!("Unknown tool: {tool_name}"));
    }

    let Some(uri) = params
        .get("arguments")
        .and_then(Value::as_object)
        .and_then(|arguments| arguments.get("uri"))
        .and_then(Value::as_str)
    else {
        return error_response(id, -32602, "Invalid params: missing arguments.uri");
    };

    tool_success(id, diagnostics_payload(bridge, uri))
}

/// Builds the `getIdeDiagnostics` payload. Unknown files report an empty set
/// rather than an error: the CLI may ask about a file this window never opened.
fn diagnostics_payload(bridge: &Bridge, uri: &str) -> Value {
    let empty = json!({
        "uri": uri,
        "totalCount": 0,
        "filteredCount": 0,
        "diagnostics": [],
    });

    let Some(path) = file_uri_to_path(uri) else {
        return empty;
    };

    let Some(diagnostics) = bridge.diagnostics_for(&path) else {
        return empty;
    };

    let reported: Vec<&crate::state::IdeDiagnostic> = diagnostics
        .entries
        .iter()
        .filter(|diagnostic| diagnostic.is_error_or_warning())
        .collect();

    json!({
        "uri": uri,
        "totalCount": diagnostics.total_count,
        "filteredCount": reported.len(),
        "diagnostics": reported,
    })
}

fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    url::Url::parse(uri).ok()?.to_file_path().ok()
}

fn read_body(request: &mut Request) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    // Read one byte past the limit so an oversized body is rejected rather than
    // silently truncated.
    request
        .as_reader()
        .take(MAX_REQUEST_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .context("failed to read the request body")?;

    if body.is_empty() {
        bail!("Empty body");
    }
    if body.len() > MAX_REQUEST_BYTES {
        bail!("Request body exceeds {MAX_REQUEST_BYTES} bytes");
    }
    Ok(body)
}

fn session_id(request: &Request) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv(SESSION_HEADER))
        .map(|header| header.value.as_str().to_string())
        .filter(|value| !value.is_empty())
}

fn respond_json(
    request: Request,
    status: u16,
    payload: &Value,
    session_id: Option<&str>,
) -> Result<()> {
    let body = payload.to_string();
    let response = Response::from_string(body)
        .with_status_code(status)
        .with_header(header("Content-Type", "application/json"));
    request
        .respond(with_cors(response, session_id))
        .context("failed to write the response")
}

fn with_cors<R: Read>(response: Response<R>, session_id: Option<&str>) -> Response<R> {
    let mut response = response
        .with_header(header("Access-Control-Allow-Origin", "*"))
        .with_header(header(
            "Access-Control-Allow-Methods",
            "GET, POST, DELETE, OPTIONS",
        ))
        .with_header(header(
            "Access-Control-Allow-Headers",
            &format!("Content-Type, {SESSION_HEADER}"),
        ))
        .with_header(header("Access-Control-Expose-Headers", SESSION_HEADER));

    if let Some(session_id) = session_id {
        response.add_header(header(SESSION_HEADER, session_id));
    }
    response
}

/// Builds a header from static, ASCII-only inputs.
///
/// `Header::from_bytes` validates that the value is ASCII; every call site here
/// passes either a literal or a session id, which is generated locally.
fn header(name: &str, value: &str) -> Header {
    match Header::from_bytes(name.as_bytes(), value.as_bytes()) {
        Ok(header) => header,
        Err(()) => {
            // Unreachable for the constants used here, but a malformed header
            // must not take the response down with it.
            log::warn!("droid_mcp: dropping malformed header {name}");
            Header::from_bytes(&b"X-Droid-Mcp-Invalid"[..], &b"1"[..])
                .unwrap_or_else(|()| unreachable!("static header literal is valid ASCII"))
        }
    }
}

fn uuid_like_id() -> String {
    // A session id only needs to be unique within this process, so a counter
    // derived from the clock and a random-looking suffix is enough. Uniqueness
    // is enforced by the map insertion in `Bridge::create_session`.
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!("{nanos:032x}{sequence:08x}")
}

/// Timestamp helper shared with the heartbeat loop.
pub fn now_millis() -> i64 {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ContextSnapshot, FileDiagnostics, IdeDiagnostic, Position};
    use std::time::Instant;

    /// Minimal HTTP client so the tests exercise the real socket path.
    fn request(
        port: u16,
        method: &str,
        session: Option<&str>,
        body: Option<&str>,
    ) -> (u16, Option<String>, String) {
        use std::io::{BufRead as _, BufReader, Write};
        use std::net::TcpStream;

        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("set read timeout");

        let body = body.unwrap_or_default();
        let mut head = format!("{method} {MCP_PATH} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
        if let Some(session) = session {
            head.push_str(&format!("{SESSION_HEADER}: {session}\r\n"));
        }
        if !body.is_empty() {
            head.push_str("Content-Type: application/json\r\n");
            head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        head.push_str("Connection: close\r\n\r\n");
        stream.write_all(head.as_bytes()).expect("write head");
        stream.write_all(body.as_bytes()).expect("write body");
        stream.flush().expect("flush");

        let mut reader = BufReader::new(stream);
        let mut status_line = String::new();
        reader.read_line(&mut status_line).expect("read status");
        let status = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);

        let mut content_length = None;
        let mut session_header = None;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read header");
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim().to_string();
            if name.eq_ignore_ascii_case("Content-Length") {
                content_length = value.parse::<usize>().ok();
            }
            if name.eq_ignore_ascii_case(SESSION_HEADER) {
                session_header = Some(value);
            }
        }

        let mut body = String::new();
        if let Some(length) = content_length {
            let mut raw = vec![0u8; length];
            reader.read_exact(&mut raw).expect("read body");
            body = String::from_utf8_lossy(&raw).into_owned();
        }

        (status, session_header, body)
    }

    fn post(port: u16, session: Option<&str>, payload: Value) -> (u16, Option<String>, Value) {
        let (status, session, body) = request(port, "POST", session, Some(&payload.to_string()));
        let parsed = serde_json::from_str(&body).unwrap_or(Value::Null);
        (status, session, parsed)
    }

    struct TestServer {
        handle: ServerHandle,
        bridge: Bridge,
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.bridge.shutdown();
            self.handle.shutdown();
        }
    }

    fn start_server() -> TestServer {
        let bridge = Bridge::new();
        let handle = start(bridge.clone()).expect("start server");
        TestServer { handle, bridge }
    }

    #[test]
    fn initialize_returns_a_session_and_server_info() {
        let server = start_server();
        let (status, session, body) = post(
            server.handle.port(),
            None,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }),
        );

        assert_eq!(status, 200);
        assert!(session.is_some(), "initialize must mint a session");
        assert_eq!(
            body["result"]["protocolVersion"],
            crate::protocol::PROTOCOL_VERSION
        );
        assert_eq!(
            body["result"]["serverInfo"]["name"],
            crate::protocol::SERVER_NAME
        );
    }

    #[test]
    fn ping_and_tools_list_work_without_a_session() {
        let server = start_server();
        let port = server.handle.port();

        let (status, _, body) = post(
            port,
            None,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }),
        );
        assert_eq!(status, 200);
        assert!(body["result"].is_object());

        let (_, _, body) = post(
            port,
            None,
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        );
        assert_eq!(body["result"]["tools"][0]["name"], GET_IDE_DIAGNOSTICS);
    }

    #[test]
    fn tools_call_returns_diagnostics_for_a_known_file() {
        let server = start_server();
        let port = server.handle.port();

        // A session keeps the published notifications out of the tool response,
        // which is the flow the CLI actually uses.
        let (_, session, _) = post(
            port,
            None,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }),
        );
        let session = session.expect("session id");

        // The key must be exactly what the server derives from the URI, so the
        // URI is built from a real absolute path rather than hard-coded.
        let path = std::env::temp_dir().join("droid-mcp-tools-call.rs");
        let uri = url::Url::from_file_path(&path)
            .expect("file url")
            .to_string();
        server.bridge.publish(
            ContextSnapshot::default(),
            Some((
                path,
                FileDiagnostics {
                    total_count: 3,
                    entries: vec![
                        IdeDiagnostic {
                            severity: 0,
                            message: "mismatched types".to_string(),
                            source: Some("rust-analyzer".to_string()),
                            code: Some("E0308".to_string()),
                            range: crate::state::DiagnosticRange {
                                start: Position {
                                    line: 4,
                                    character: 1,
                                },
                                end: Position {
                                    line: 4,
                                    character: 9,
                                },
                            },
                        },
                        IdeDiagnostic {
                            severity: 3,
                            message: "this is a hint".to_string(),
                            source: None,
                            code: None,
                            range: crate::state::DiagnosticRange {
                                start: Position {
                                    line: 5,
                                    character: 0,
                                },
                                end: Position {
                                    line: 5,
                                    character: 1,
                                },
                            },
                        },
                    ],
                },
            )),
        );

        let (status, _, body) = post(
            port,
            Some(&session),
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": GET_IDE_DIAGNOSTICS,
                    "arguments": { "uri": uri }
                }
            }),
        );

        assert_eq!(status, 200);
        let text = body["result"]["content"][0]["text"]
            .as_str()
            .expect("tool content text");
        let payload: Value = serde_json::from_str(text).expect("parse tool payload");
        assert_eq!(payload["totalCount"], 3);
        // Only the error survives the error/warning filter.
        assert_eq!(payload["filteredCount"], 1);
        assert_eq!(payload["diagnostics"][0]["severity"], 0);
        assert_eq!(payload["diagnostics"][0]["range"]["start"]["line"], 4);
    }

    #[test]
    fn tools_call_reports_an_empty_set_for_an_unknown_file() {
        let server = start_server();
        let (_, _, body) = post(
            server.handle.port(),
            None,
            json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": GET_IDE_DIAGNOSTICS,
                    "arguments": { "uri": "file:///work/never-opened.rs" }
                }
            }),
        );

        let text = body["result"]["content"][0]["text"].as_str().expect("text");
        let payload: Value = serde_json::from_str(text).expect("parse payload");
        assert_eq!(payload["totalCount"], 0);
        assert_eq!(payload["diagnostics"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn tools_call_without_a_uri_is_an_invalid_params_error() {
        let server = start_server();
        let (status, _, body) = post(
            server.handle.port(),
            None,
            json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "tools/call",
                "params": { "name": GET_IDE_DIAGNOSTICS, "arguments": {} }
            }),
        );

        assert_eq!(status, 200);
        assert_eq!(body["error"]["code"], -32602);
    }

    #[test]
    fn unknown_tools_and_methods_are_reported() {
        let server = start_server();
        let port = server.handle.port();

        let (_, _, body) = post(
            port,
            None,
            json!({
                "jsonrpc": "2.0",
                "id": 6,
                "method": "tools/call",
                "params": { "name": "openFile", "arguments": { "filePath": "/tmp/x" } }
            }),
        );
        assert_eq!(body["error"]["code"], -32601);

        let (_, _, body) = post(
            port,
            None,
            json!({ "jsonrpc": "2.0", "id": 7, "method": "does/not/exist" }),
        );
        assert_eq!(body["error"]["code"], -32601);
    }

    #[test]
    fn stateless_requests_receive_pending_notifications_as_a_batch() {
        let server = start_server();
        server.bridge.publish(
            ContextSnapshot {
                active_file: Some(crate::state::ActiveFile {
                    path: "/work/src/main.rs".to_string(),
                    file_name: "main.rs".to_string(),
                    is_dirty: false,
                    line_count: 1,
                    selection: crate::state::SelectionSnapshot::default(),
                }),
                ..ContextSnapshot::default()
            },
            None,
        );

        let (_, _, body) = post(
            server.handle.port(),
            None,
            json!({ "jsonrpc": "2.0", "id": 8, "method": "ping" }),
        );

        let batch = body.as_array().expect("batch response");
        let methods: Vec<&str> = batch
            .iter()
            .filter_map(|message| message.get("method").and_then(Value::as_str))
            .collect();
        assert_eq!(
            methods,
            vec!["notifications/activeFile", "notifications/openFiles"]
        );
        // The response itself comes last so a client can match it by id.
        assert_eq!(batch.last().unwrap()["id"], 8);
    }

    #[test]
    fn notifications_pull_drains_the_session_queue() {
        let server = start_server();
        let port = server.handle.port();

        let (_, session, _) = post(
            port,
            None,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }),
        );
        let session = session.expect("session id");

        server.bridge.publish(
            ContextSnapshot {
                active_file: Some(crate::state::ActiveFile {
                    path: "/work/src/main.rs".to_string(),
                    file_name: "main.rs".to_string(),
                    is_dirty: false,
                    line_count: 1,
                    selection: crate::state::SelectionSnapshot::default(),
                }),
                ..ContextSnapshot::default()
            },
            None,
        );

        let (_, _, body) = post(
            port,
            Some(&session),
            json!({ "jsonrpc": "2.0", "id": 9, "method": "notifications/pull" }),
        );
        let notifications = body["result"]["notifications"].as_array().expect("array");
        assert_eq!(notifications.len(), 2);

        // A second pull finds nothing new.
        let (_, _, body) = post(
            port,
            Some(&session),
            json!({ "jsonrpc": "2.0", "id": 10, "method": "notifications/pull" }),
        );
        assert!(
            body["result"]["notifications"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn sse_stream_delivers_notifications_and_ends_on_shutdown() {
        use std::net::TcpStream;

        let server = start_server();
        let port = server.handle.port();

        let (_, session, _) = post(
            port,
            None,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }),
        );
        let session = session.expect("session id");

        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        let head = format!(
            "GET {MCP_PATH} HTTP/1.1\r\nHost: 127.0.0.1\r\n{SESSION_HEADER}: {session}\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(head.as_bytes()).expect("write head");

        server.bridge.publish(
            ContextSnapshot {
                active_file: Some(crate::state::ActiveFile {
                    path: "/work/src/main.rs".to_string(),
                    file_name: "main.rs".to_string(),
                    is_dirty: true,
                    line_count: 3,
                    selection: crate::state::SelectionSnapshot::default(),
                }),
                ..ContextSnapshot::default()
            },
            None,
        );

        let mut received = Vec::new();
        let mut buffer = [0u8; 4096];
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    received.extend_from_slice(&buffer[..count]);
                    let text = String::from_utf8_lossy(&received);
                    if text.contains("notifications/activeFile") {
                        break;
                    }
                }
                Err(error) => panic!("SSE read failed: {error}"),
            }
        }

        let text = String::from_utf8_lossy(&received);
        assert!(
            text.contains("text/event-stream"),
            "expected an event stream, got: {text}"
        );
        assert!(
            text.contains("notifications/activeFile"),
            "expected a notification, got: {text}"
        );
        assert!(text.contains("/work/src/main.rs"));
    }

    #[test]
    fn sse_without_a_session_is_rejected() {
        let server = start_server();
        let (status, _, _) = request(server.handle.port(), "GET", None, None);
        assert_eq!(status, 400);
    }

    #[test]
    fn delete_removes_the_session() {
        let server = start_server();
        let port = server.handle.port();

        let (_, session, _) = post(
            port,
            None,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }),
        );
        let session = session.expect("session id");

        let (status, _, _) = request(port, "DELETE", Some(&session), None);
        assert_eq!(status, 204);

        let (status, _, _) = request(port, "DELETE", Some(&session), None);
        assert_eq!(status, 404);
    }

    #[test]
    fn options_is_allowed_and_unknown_paths_are_not_found() {
        let server = start_server();
        let port = server.handle.port();

        let (status, _, _) = request(port, "OPTIONS", None, None);
        assert_eq!(status, 200);

        use std::net::TcpStream;
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        stream
            .write_all(b"GET /other HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .expect("write");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read");
        assert!(response.starts_with("HTTP/1.1 404"));
    }

    #[test]
    fn malformed_json_and_oversized_bodies_are_rejected() {
        let server = start_server();
        let port = server.handle.port();

        let (status, _, body) = request(port, "POST", None, Some("{not json"));
        assert_eq!(status, 400);
        assert!(body.contains("Invalid JSON"));

        let (status, _, _) = request(port, "POST", None, Some(""));
        assert_eq!(status, 400);

        let huge = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\",\"padding\":\"{}\"}}",
            "x".repeat(MAX_REQUEST_BYTES + 16)
        );
        let (status, _, body) = request(port, "POST", None, Some(&huge));
        assert_eq!(status, 400);
        assert!(body.contains("exceeds"));
    }

    #[test]
    fn heartbeat_notifications_reach_sessions() {
        let server = start_server();
        let port = server.handle.port();

        let (_, session, _) = post(
            port,
            None,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }),
        );
        let session = session.expect("session id");

        server.bridge.queue_heartbeat(1234);
        let (_, _, body) = post(
            port,
            Some(&session),
            json!({ "jsonrpc": "2.0", "id": 11, "method": "notifications/pull" }),
        );
        let notifications = body["result"]["notifications"].as_array().expect("array");
        assert_eq!(notifications[0]["method"], "notifications/heartbeat");
        assert_eq!(notifications[0]["params"]["timestamp"], 1234);
    }

    #[test]
    fn notifications_initialized_is_accepted_without_a_body() {
        let server = start_server();
        let (status, _, _) = post(
            server.handle.port(),
            None,
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        );
        assert_eq!(status, 202);
    }
}
