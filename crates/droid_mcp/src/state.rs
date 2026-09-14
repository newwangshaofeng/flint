//! State shared between the GPUI foreground thread (which produces it) and the
//! HTTP worker threads (which serve it).
//!
//! The foreground thread owns all GPUI access. It converts what it sees into
//! plain data and hands it over here; worker threads only ever read that data.
//! Nothing in this module may touch a GPUI entity.

use crate::protocol::{NotificationQueue, Session};
use parking_lot::{Condvar, Mutex};
use serde::Serialize;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Zero-based UTF-16 position, matching the LSP convention the CLI expects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DiagnosticRange {
    pub start: Position,
    pub end: Position,
}

/// A diagnostic already normalized for the wire: severity uses the CLI's
/// 0-based scale and positions are zero-based UTF-16.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct IdeDiagnostic {
    pub severity: u8,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub range: DiagnosticRange,
}

impl IdeDiagnostic {
    /// True for errors and warnings, which is what the tool reports.
    pub fn is_error_or_warning(&self) -> bool {
        self.severity <= 1
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveFile {
    pub path: String,
    pub file_name: String,
    pub is_dirty: bool,
    pub line_count: u32,
    pub selection: SelectionSnapshot,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectionSnapshot {
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
    pub selected_text: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenFile {
    pub path: String,
    pub file_name: String,
    pub is_dirty: bool,
    pub language_id: String,
}

/// Everything the bridge knows about the IDE at one moment.
#[derive(Clone, Debug, Default)]
pub struct ContextSnapshot {
    /// Top-most workspace roots. Nested roots are dropped by the collector.
    pub workspace_folders: Vec<PathBuf>,
    /// `None` when no local editor is focused, in which case the CLI is told the
    /// context is empty rather than stale.
    pub active_file: Option<ActiveFile>,
    pub open_files: Vec<OpenFile>,
}

/// Diagnostics for one file, as last published by the collector.
#[derive(Clone, Debug, Default)]
pub struct FileDiagnostics {
    pub total_count: usize,
    pub entries: Vec<IdeDiagnostic>,
}

/// The mutex-protected core. Held only for short, non-blocking reads and writes.
#[derive(Default)]
struct BridgeState {
    snapshot: ContextSnapshot,
    diagnostics: BTreeMap<PathBuf, FileDiagnostics>,
    sessions: HashMap<String, Session>,
    /// Queue used when a client has no session yet (stateless requests).
    anonymous: NotificationQueue,
    /// Set once the server stops; parked SSE threads wake and exit.
    shutdown: bool,
}

/// Shared handle. Cloning is cheap and every clone sees the same state.
#[derive(Clone, Default)]
pub struct Bridge {
    state: Arc<Mutex<BridgeState>>,
    /// Signalled whenever notifications are queued or shutdown begins, so SSE
    /// threads do not busy-wait.
    wakeup: Arc<Condvar>,
}

impl Bridge {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the snapshot and queues the notifications it implies.
    ///
    /// `diagnostics` is the full set for the active file, used both for the
    /// notification and for later `getIdeDiagnostics` calls.
    pub fn publish(
        &self,
        snapshot: ContextSnapshot,
        active_file_diagnostics: Option<(PathBuf, FileDiagnostics)>,
    ) {
        let mut state = self.state.lock();

        let active_file_notification = snapshot
            .active_file
            .as_ref()
            .map(|active_file| json!(active_file));

        let open_files_notification = json!({ "files": snapshot.open_files });

        let diagnostics_notification = match (&snapshot.active_file, &active_file_diagnostics) {
            (Some(active_file), Some((_, diagnostics))) => Some(json!({
                "filePath": active_file.path,
                "diagnostics": diagnostics
                    .entries
                    .iter()
                    .filter(|diagnostic| diagnostic.is_error_or_warning())
                    .collect::<Vec<_>>(),
            })),
            _ => None,
        };

        if let Some((path, diagnostics)) = active_file_diagnostics {
            state.diagnostics.insert(path, diagnostics);
        }
        state.snapshot = snapshot;

        // A client with no active editor still needs to be told the context went
        // away, otherwise it keeps using the last file it saw.
        let active_file_notification = active_file_notification.unwrap_or_else(|| json!(null));
        queue_all(
            &mut state,
            "notifications/activeFile",
            active_file_notification,
        );
        queue_all(
            &mut state,
            "notifications/openFiles",
            open_files_notification,
        );
        if let Some(params) = diagnostics_notification {
            queue_all(&mut state, "notifications/diagnostics", params);
        }

        drop(state);
        self.wakeup.notify_all();
    }

    /// Drops everything known about the IDE, without tearing down sessions.
    ///
    /// Used when the last local workspace goes away.
    pub fn clear(&self) {
        let mut state = self.state.lock();
        state.snapshot = ContextSnapshot::default();
        state.diagnostics.clear();
        queue_all(&mut state, "notifications/activeFile", json!(null));
        queue_all(
            &mut state,
            "notifications/openFiles",
            json!({ "files": [] }),
        );
        drop(state);
        self.wakeup.notify_all();
    }

    pub fn snapshot(&self) -> ContextSnapshot {
        self.state.lock().snapshot.clone()
    }

    pub fn diagnostics_for(&self, path: &Path) -> Option<FileDiagnostics> {
        self.state.lock().diagnostics.get(path).cloned()
    }

    pub fn queue_heartbeat(&self, timestamp_millis: i64) {
        let mut state = self.state.lock();
        queue_all(
            &mut state,
            "notifications/heartbeat",
            json!({ "timestamp": timestamp_millis }),
        );
        drop(state);
        self.wakeup.notify_all();
    }

    /// Creates a session, returning its id.
    pub fn create_session(&self, id: String) -> String {
        let mut state = self.state.lock();
        state.sessions.insert(id.clone(), Session::default());
        id
    }

    pub fn has_session(&self, id: &str) -> bool {
        self.state.lock().sessions.contains_key(id)
    }

    pub fn remove_session(&self, id: &str) -> bool {
        self.state.lock().sessions.remove(id).is_some()
    }

    pub fn session_count(&self) -> usize {
        self.state.lock().sessions.len()
    }

    /// Takes the notifications waiting for `session_id`, or the anonymous queue
    /// when the client has no session.
    pub fn drain_notifications(&self, session_id: Option<&str>) -> Vec<serde_json::Value> {
        let mut state = self.state.lock();
        match session_id {
            Some(id) => state
                .sessions
                .get_mut(id)
                .map(|session| session.queue.drain())
                .unwrap_or_default(),
            None => state.anonymous.drain(),
        }
    }

    pub fn has_pending_notifications(&self, session_id: &str) -> bool {
        self.state
            .lock()
            .sessions
            .get(session_id)
            .is_some_and(|session| !session.queue.is_empty())
    }

    /// Blocks until something is queued for `session_id`, the wait elapses, or
    /// the server shuts down. Returns `true` if the caller should re-check.
    pub fn wait_for_notifications(&self, session_id: &str, timeout: std::time::Duration) -> bool {
        let mut state = self.state.lock();
        if state.shutdown {
            return false;
        }
        if state
            .sessions
            .get(session_id)
            .is_some_and(|session| !session.queue.is_empty())
        {
            return true;
        }
        self.wakeup.wait_for(&mut state, timeout);
        !state.shutdown
    }

    /// Wakes every parked SSE thread and marks the bridge as stopped.
    pub fn shutdown(&self) {
        self.state.lock().shutdown = true;
        self.wakeup.notify_all();
    }

    pub fn is_shutdown(&self) -> bool {
        self.state.lock().shutdown
    }
}

fn queue_all(state: &mut BridgeState, method: &str, params: serde_json::Value) {
    state.anonymous.queue(method, params.clone());
    for session in state.sessions.values_mut() {
        session.queue.queue(method, params.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active_file(path: &str) -> ActiveFile {
        ActiveFile {
            path: path.to_string(),
            file_name: "main.rs".to_string(),
            is_dirty: true,
            line_count: 12,
            selection: SelectionSnapshot {
                start_line: 1,
                start_character: 2,
                end_line: 1,
                end_character: 5,
                selected_text: "abc".to_string(),
            },
        }
    }

    fn error_diagnostic() -> IdeDiagnostic {
        IdeDiagnostic {
            severity: 0,
            message: "unused variable".to_string(),
            source: Some("rust-analyzer".to_string()),
            code: Some("E0001".to_string()),
            range: DiagnosticRange {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 3,
                },
            },
        }
    }

    #[test]
    fn publish_queues_active_file_open_files_and_diagnostics() {
        let bridge = Bridge::new();
        bridge.publish(
            ContextSnapshot {
                workspace_folders: vec![PathBuf::from("/work")],
                active_file: Some(active_file("/work/src/main.rs")),
                open_files: vec![OpenFile {
                    path: "/work/src/main.rs".to_string(),
                    file_name: "main.rs".to_string(),
                    is_dirty: true,
                    language_id: "Rust".to_string(),
                }],
            },
            Some((
                PathBuf::from("/work/src/main.rs"),
                FileDiagnostics {
                    total_count: 1,
                    entries: vec![error_diagnostic()],
                },
            )),
        );

        let drained = bridge.drain_notifications(None);
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
        assert_eq!(drained[0]["params"]["path"], "/work/src/main.rs");
        assert_eq!(drained[0]["params"]["selection"]["selectedText"], "abc");
        assert_eq!(drained[2]["params"]["diagnostics"][0]["severity"], 0);
    }

    #[test]
    fn diagnostics_stay_available_for_later_tool_calls() {
        let bridge = Bridge::new();
        bridge.publish(
            ContextSnapshot::default(),
            Some((
                PathBuf::from("/work/src/main.rs"),
                FileDiagnostics {
                    total_count: 1,
                    entries: vec![error_diagnostic()],
                },
            )),
        );

        let stored = bridge
            .diagnostics_for(Path::new("/work/src/main.rs"))
            .expect("diagnostics retained");
        assert_eq!(stored.total_count, 1);
        assert_eq!(stored.entries.len(), 1);
    }

    #[test]
    fn missing_active_file_publishes_an_empty_context() {
        let bridge = Bridge::new();
        bridge.publish(ContextSnapshot::default(), None);

        let drained = bridge.drain_notifications(None);
        assert_eq!(drained[0]["params"], serde_json::Value::Null);
        assert_eq!(drained[1]["params"]["files"].as_array().unwrap().len(), 0);
        assert_eq!(drained.len(), 2);
    }

    #[test]
    fn sessions_receive_notifications_independently() {
        let bridge = Bridge::new();
        bridge.create_session("session-a".to_string());

        bridge.publish(
            ContextSnapshot {
                active_file: Some(active_file("/work/src/main.rs")),
                ..ContextSnapshot::default()
            },
            None,
        );

        assert!(bridge.has_pending_notifications("session-a"));
        assert_eq!(bridge.drain_notifications(Some("session-a")).len(), 2);
        assert!(!bridge.has_pending_notifications("session-a"));
        // The anonymous queue is drained separately.
        assert_eq!(bridge.drain_notifications(None).len(), 2);
    }

    #[test]
    fn removing_a_session_is_reported_once() {
        let bridge = Bridge::new();
        bridge.create_session("session-a".to_string());
        assert_eq!(bridge.session_count(), 1);
        assert!(bridge.remove_session("session-a"));
        assert!(!bridge.remove_session("session-a"));
        assert_eq!(bridge.session_count(), 0);
    }

    #[test]
    fn shutdown_wakes_waiters() {
        let bridge = Bridge::new();
        bridge.create_session("session-a".to_string());
        let waiter = bridge.clone();
        let handle = std::thread::spawn(move || {
            waiter.wait_for_notifications("session-a", std::time::Duration::from_secs(30))
        });

        std::thread::sleep(std::time::Duration::from_millis(50));
        bridge.shutdown();
        assert!(!handle.join().expect("waiter finished"));
    }
}
