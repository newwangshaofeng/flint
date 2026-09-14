//! Wire format for the MCP bridge: JSON-RPC framing, session bookkeeping, the
//! tool surface, and the notification queue.
//!
//! Everything here is free of GPUI types so it can be exercised without an app
//! context.

use serde_json::{Value, json};
use std::collections::HashMap;

/// The only path the bridge serves. Factory's IDE bridge always appends this.
pub const MCP_PATH: &str = "/mcp";

/// Header carrying the MCP session id, in the casing the spec uses.
pub const SESSION_HEADER: &str = "Mcp-Session-Id";

pub const PROTOCOL_VERSION: &str = "2025-03-26";
pub const SERVER_NAME: &str = "flint-droid-mcp-server";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Upper bound on a request body. Tool calls carry a file URI, so anything
/// larger is a malformed or hostile client.
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;

pub const GET_IDE_DIAGNOSTICS: &str = "getIdeDiagnostics";

/// Notifications are delivered in this order. Diagnostics come last so a client
/// that reads them in order already knows which file and selection they
/// describe.
pub const NOTIFICATION_ORDER: &[&str] = &[
    "notifications/activeFile",
    "notifications/openFiles",
    "notifications/diagnostics",
];

fn notification_rank(method: &str) -> usize {
    NOTIFICATION_ORDER
        .iter()
        .position(|candidate| *candidate == method)
        .unwrap_or(NOTIFICATION_ORDER.len())
}

/// Pending notifications for one client, deduplicated by method.
///
/// Only the newest payload per method is kept: a client that has not drained
/// yet does not need the intermediate states of a file it has already moved
/// past.
#[derive(Default)]
pub struct NotificationQueue {
    pending: HashMap<String, Value>,
}

impl NotificationQueue {
    pub fn queue(&mut self, method: &str, params: Value) {
        self.pending.insert(
            method.to_string(),
            json!({ "jsonrpc": "2.0", "method": method, "params": params }),
        );
    }

    pub fn drain(&mut self) -> Vec<Value> {
        let mut drained: Vec<(String, Value)> = self.pending.drain().collect();
        drained.sort_by_key(|(method, _)| notification_rank(method));
        drained.into_iter().map(|(_, message)| message).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

/// One initialized MCP session.
///
/// The id itself lives in the map key that holds this session; only the queue
/// needs to travel with it.
#[derive(Default)]
pub struct Session {
    pub queue: NotificationQueue,
}

pub fn tools_list_result() -> Value {
    json!({
        "tools": [{
            "name": GET_IDE_DIAGNOSTICS,
            "description": "Get language diagnostics (errors and warnings) from Flint for a specific file",
            "inputSchema": {
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "properties": {
                    "uri": {
                        "type": "string",
                        "description": "File URI to get diagnostics for (required)"
                    }
                },
                "required": ["uri"],
                "additionalProperties": false
            }
        }]
    })
}

pub fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
        "capabilities": { "tools": {} }
    })
}

pub fn success_response(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Wraps a tool payload the way `tools/call` results are expected to look.
pub fn tool_success(id: &Value, payload: Value) -> Value {
    success_response(
        id,
        json!({ "content": [{ "type": "text", "text": payload.to_string() }] }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifications_are_deduplicated_by_method() {
        let mut queue = NotificationQueue::default();
        queue.queue("notifications/activeFile", json!({ "path": "first" }));
        queue.queue("notifications/activeFile", json!({ "path": "second" }));

        let drained = queue.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0]["params"]["path"], "second");
        assert!(queue.is_empty());
    }

    #[test]
    fn notifications_drain_in_declared_order() {
        let mut queue = NotificationQueue::default();
        // Queue in reverse so ordering cannot come from insertion order.
        queue.queue("notifications/diagnostics", json!({}));
        queue.queue("notifications/openFiles", json!({}));
        queue.queue("notifications/activeFile", json!({}));

        let drained = queue.drain();
        let methods: Vec<&str> = drained
            .iter()
            .map(|message| message["method"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(
            methods,
            vec![
                "notifications/activeFile",
                "notifications/openFiles",
                "notifications/diagnostics"
            ]
        );
    }

    #[test]
    fn unknown_notifications_sort_after_known_ones() {
        let mut queue = NotificationQueue::default();
        queue.queue("notifications/heartbeat", json!({}));
        queue.queue("notifications/activeFile", json!({}));

        let drained = queue.drain();
        let methods: Vec<&str> = drained
            .iter()
            .map(|message| message["method"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(
            methods,
            vec!["notifications/activeFile", "notifications/heartbeat"]
        );
    }

    #[test]
    fn tools_list_exposes_only_read_only_context() {
        let result = tools_list_result();
        let tools = result["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], GET_IDE_DIAGNOSTICS);
        assert_eq!(tools[0]["inputSchema"]["required"][0], "uri");
    }
}
