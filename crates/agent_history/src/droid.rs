//! Droid (Factory CLI) session history.
//!
//! Droid stores one JSONL transcript per session under
//! `~/.factory/sessions/<encoded-cwd>/<session-id>.jsonl`. The first record is
//! a `session_start` header carrying the session id, the working directory, and
//! a title; the remaining records are messages with RFC 3339 timestamps, plus
//! harness bookkeeping (`todo_state`, `agent_turn_outcome`, `compaction_state`,
//! `session_end`). A session's `<id>.settings.json` siblings are not part of
//! the transcript and are excluded by the `.jsonl` extension filter.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::transcript::{Classified, RawEvent, one_line, summarize_tool_input};
use crate::{
    FileIdentity, HistoryHost, HistoryKind, HistoryProvider, IndexedSession, ProviderRefresh,
};

const MAX_PROJECT_HISTORY_FILES_SCANNED: usize = 200;
const MAX_TITLE_CHARS: usize = 60;
const DEFAULT_TITLE: &str = "Droid session";

/// Titles Droid writes before it has derived a real one from the first turn.
/// Treated as absent so a usable first user message can take their place.
const PLACEHOLDER_TITLES: &[&str] = &["New Session", "Start new chat"];

/// User messages Droid synthesizes for interruptions and slash-command output.
/// They are not prompts, so they never become a title.
const SYNTHETIC_USER_PREFIXES: &[&str] = &[
    "Request cancelled by user",
    "Request interrupted by user",
    "Skill \"",
];

pub struct DroidHistoryProvider;

#[async_trait]
impl HistoryProvider for DroidHistoryProvider {
    fn kind(&self) -> HistoryKind {
        HistoryKind::Droid
    }

    async fn refresh(
        &self,
        host: &HistoryHost,
        previous: Option<&Value>,
    ) -> Result<ProviderRefresh> {
        let previous = previous
            .and_then(|value| serde_json::from_value::<DroidSourceState>(value.clone()).ok())
            .unwrap_or_default();

        let sessions_dir = host.join(&host.base_dir, "sessions")?;
        let mut files = BTreeMap::new();
        for project_dir in host.fs.read_dir(&sessions_dir).await.unwrap_or_default() {
            let mut project_files = host
                .fs
                .read_dir(&project_dir)
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|path| {
                    path.extension()
                        .is_some_and(|extension| extension == "jsonl")
                })
                .collect::<Vec<_>>();
            project_files.sort_unstable_by(|left, right| right.cmp(left));
            project_files.truncate(MAX_PROJECT_HISTORY_FILES_SCANNED);
            for path in project_files {
                let Some(key) = path.to_str().map(str::to_string) else {
                    continue;
                };
                let Some(identity) = host.fs.metadata(&path).await.ok().flatten() else {
                    continue;
                };
                let summary = match previous.files.get(&key) {
                    Some(entry) if entry.identity == identity => entry.summary.clone(),
                    _ => parse_session_summary(&host.fs.load(&path).await.unwrap_or_default()),
                };
                files.insert(key, SessionFileEntry { identity, summary });
            }
        }

        let mut sessions = Vec::new();
        for (key, entry) in &files {
            let Some(summary) = &entry.summary else {
                continue;
            };
            // A session that never recorded a message has no timestamp of its
            // own; its file mtime is when it started, which keeps a brand-new
            // session visible (and discoverable by a live thread) immediately.
            let (last_activity_secs, last_activity_nanos) =
                if summary.last_activity_secs == 0 && summary.last_activity_nanos == 0 {
                    (
                        entry.identity.modified_at_secs,
                        entry.identity.modified_at_nanos,
                    )
                } else {
                    (summary.last_activity_secs, summary.last_activity_nanos)
                };
            sessions.push(IndexedSession {
                session_id: summary.session_id.clone(),
                resolved_title: summary.title.clone(),
                fallback_title: None,
                working_dir: summary.project_root.clone(),
                last_activity_secs,
                last_activity_nanos,
                source_path: Some(key.clone()),
                source_identity: Some(entry.identity),
            });
        }

        let source_state = serde_json::to_value(DroidSourceState { files })?;
        Ok(ProviderRefresh {
            source_state,
            sessions,
        })
    }
}

#[derive(Default, Serialize, Deserialize)]
struct DroidSourceState {
    files: BTreeMap<String, SessionFileEntry>,
}

#[derive(Clone, Serialize, Deserialize)]
struct SessionFileEntry {
    identity: FileIdentity,
    /// `None` records an unreadable or headerless file so it is not reloaded
    /// until it changes.
    summary: Option<DroidSummary>,
}

#[derive(Clone, Serialize, Deserialize)]
struct DroidSummary {
    session_id: String,
    title: String,
    project_root: String,
    last_activity_secs: u64,
    last_activity_nanos: u32,
}

fn parse_session_summary(content: &str) -> Option<DroidSummary> {
    let mut session_id = None;
    let mut project_root = None;
    let mut header_title = None;
    let mut first_user_message = None;
    let mut last_activity_at = UNIX_EPOCH;

    for line in content.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(timestamp) = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp)
        {
            last_activity_at = last_activity_at.max(timestamp);
        }
        match entry.get("type").and_then(Value::as_str) {
            Some("session_start") => {
                session_id = entry
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string);
                project_root = entry.get("cwd").and_then(Value::as_str).map(str::to_string);
                header_title = entry
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            Some("message") if first_user_message.is_none() => {
                first_user_message = user_message_title(&entry);
            }
            _ => {}
        }
    }

    let session_id = session_id?;
    let project_root = project_root?;
    let (last_activity_secs, last_activity_nanos) = last_activity_at
        .duration_since(UNIX_EPOCH)
        .map(|duration| (duration.as_secs(), duration.subsec_nanos()))
        .unwrap_or((0, 0));
    Some(DroidSummary {
        session_id,
        title: header_title
            .as_deref()
            .and_then(normalize_title)
            .or(first_user_message)
            .unwrap_or_else(|| DEFAULT_TITLE.to_string()),
        project_root,
        last_activity_secs,
        last_activity_nanos,
    })
}

/// Classifies a Droid session into the shared event stream.
///
/// Droid's file is an append-only tree keyed by `id`/`parentId`, so the active
/// conversation is the parent chain from the last message back to the root;
/// abandoned branches (rewound or edited turns) are discarded. Tool calls are
/// `tool_use` blocks on assistant messages; tool results are `tool_result`
/// blocks on user messages.
///
/// Only `message` records participate in that tree: `session_start`,
/// `todo_state`, `compaction_state`, and `agent_turn_outcome` are side logs
/// with no `parentId` of their own, so they are classified in file order
/// alongside the active chain.
pub(crate) fn classify_transcript(content: &str) -> Classified {
    let mut classified = Classified::default();

    // Parse every line, preserving order and indexing by id.
    let mut entries: Vec<Value> = Vec::new();
    let mut index_by_id: BTreeMap<String, usize> = BTreeMap::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            classified.malformed_count += 1;
            continue;
        };
        if let Some(id) = value.get("id").and_then(Value::as_str) {
            index_by_id.insert(id.to_string(), entries.len());
        }
        entries.push(value);
    }

    // Mark the active branch: walk `parentId` from the last message to the root.
    let mut active: collections::HashSet<usize> = collections::HashSet::default();
    let mut current = entries
        .iter()
        .rposition(|entry| entry.get("type").and_then(Value::as_str) == Some("message"));
    let mut guard = entries.len() + 1;
    while let Some(index) = current {
        if !active.insert(index) || guard == 0 {
            break;
        }
        guard -= 1;
        current = entries[index]
            .get("parentId")
            .and_then(Value::as_str)
            .and_then(|parent| index_by_id.get(parent))
            .copied();
    }

    for (index, entry) in entries.iter().enumerate() {
        let is_message = entry.get("type").and_then(Value::as_str) == Some("message");
        if is_message && !active.contains(&index) {
            continue;
        }
        classify_entry(entry, &mut classified);
    }
    classified
}

fn classify_entry(entry: &Value, classified: &mut Classified) {
    match entry.get("type").and_then(Value::as_str) {
        Some("message") => classify_message(entry, classified),
        Some("compaction_state") => classified
            .events
            .push(RawEvent::Checkpoint("compaction".to_string())),
        Some("session_start")
        | Some("session_end")
        | Some("todo_state")
        | Some("agent_turn_outcome") => classified.events.push(RawEvent::Noise),
        _ => classified.unknown_count += 1,
    }
}

fn classify_message(entry: &Value, classified: &mut Classified) {
    let Some(message) = entry.get("message") else {
        classified.malformed_count += 1;
        return;
    };
    match message.get("role").and_then(Value::as_str) {
        Some("user") => classify_user_blocks(message, classified),
        Some("assistant") => classify_assistant_blocks(message, classified),
        _ => classified.unknown_count += 1,
    }
}

fn classify_user_blocks(message: &Value, classified: &mut Classified) {
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        classify_user_text(text, classified);
        return;
    }
    let Some(blocks) = message.get("content").and_then(Value::as_array) else {
        classified.malformed_count += 1;
        return;
    };
    let mut user_text: Vec<String> = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("tool_result") => classified.events.push(RawEvent::ToolResult {
                call_id: block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                output: tool_result_text(block.get("content")),
                is_error: block.get("is_error").and_then(Value::as_bool) == Some(true),
            }),
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    user_text.push(text.to_string());
                }
            }
            // Attached images are known and excluded.
            Some("image") => classified.events.push(RawEvent::Noise),
            _ => {}
        }
    }
    let joined = user_text.join("\n");
    if !joined.trim().is_empty() {
        classify_user_text(&joined, classified);
    }
}

fn classify_user_text(text: &str, classified: &mut Classified) {
    let text = strip_system_reminders(text);
    let text = text.trim();
    if text.is_empty() || is_synthetic_user_text(text) {
        classified.events.push(RawEvent::Noise);
        return;
    }
    classified.events.push(RawEvent::User(one_line(text)));
}

fn classify_assistant_blocks(message: &Value, classified: &mut Classified) {
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        classified
            .events
            .push(RawEvent::Assistant(text.to_string()));
        return;
    }
    let Some(blocks) = message.get("content").and_then(Value::as_array) else {
        classified.malformed_count += 1;
        return;
    };
    // Flush buffered assistant text before each tool call so ordering (text then
    // the call it introduces) is preserved.
    let mut pending_text: Vec<String> = Vec::new();
    let flush = |pending_text: &mut Vec<String>, classified: &mut Classified| {
        let joined = pending_text.join("\n");
        pending_text.clear();
        if !joined.trim().is_empty() {
            classified.events.push(RawEvent::Assistant(joined));
        }
    };
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    pending_text.push(text.to_string());
                }
            }
            Some("tool_use") => {
                flush(&mut pending_text, classified);
                classified.events.push(RawEvent::ToolCall {
                    call_id: block.get("id").and_then(Value::as_str).map(str::to_string),
                    name: block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_string(),
                    detail: summarize_tool_input(block.get("input")),
                });
            }
            // Thinking and attached images are known and excluded.
            Some("thinking") | Some("redacted_thinking") | Some("image") => {
                classified.events.push(RawEvent::Noise)
            }
            _ => classified.unknown_count += 1,
        }
    }
    flush(&mut pending_text, classified);
}

/// A `tool_result` block's `content` is a string or an array of
/// `{type:text}` (and other) blocks; join the text.
fn tool_result_text(content: Option<&Value>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    if let Some(blocks) = content.as_array() {
        return blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
    }
    content.to_string()
}

fn parse_timestamp(timestamp: &str) -> Option<SystemTime> {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|timestamp| {
            UNIX_EPOCH + Duration::from_millis(timestamp.timestamp_millis().max(0) as u64)
        })
}

/// The first real prompt in a session, used only when the header carries a
/// placeholder title. Droid prefixes most user messages with a
/// `<system-reminder>` block, which is stripped before the text is considered.
fn user_message_title(entry: &Value) -> Option<String> {
    let message = entry.get("message")?;
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return None;
    }
    let text = message_text(message)?;
    let text = strip_system_reminders(&text);
    let text = text.trim();
    if text.is_empty() || is_synthetic_user_text(text) {
        return None;
    }
    normalize_title(text)
}

/// Extracts message text from either a string `content` or a `content` array of
/// `{type:text}` blocks.
fn message_text(message: &Value) -> Option<String> {
    let content = message.get("content")?;
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }
    let blocks = content.as_array()?;
    let text = blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    Some(text)
}

/// Removes harness-injected `<system-reminder>` blocks from user text. These
/// carry runtime context rather than the user's own words, so they must never
/// become a title or a handoff turn.
fn strip_system_reminders(text: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";

    let mut remaining = text;
    let mut stripped = String::new();
    while let Some(start) = remaining.find(OPEN) {
        stripped.push_str(&remaining[..start]);
        let after_open = &remaining[start + OPEN.len()..];
        match after_open.find(CLOSE) {
            Some(end) => remaining = &after_open[end + CLOSE.len()..],
            None => {
                // Unterminated reminder: drop the rest of the text.
                remaining = "";
                break;
            }
        }
    }
    stripped.push_str(remaining);
    stripped
}

fn is_synthetic_user_text(text: &str) -> bool {
    SYNTHETIC_USER_PREFIXES
        .iter()
        .any(|prefix| text.starts_with(prefix))
}

fn normalize_title(title: &str) -> Option<String> {
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() || PLACEHOLDER_TITLES.contains(&title.as_str()) {
        return None;
    }
    if title.chars().count() <= MAX_TITLE_CHARS {
        return Some(title);
    }
    Some(format!(
        "{}…",
        title.chars().take(MAX_TITLE_CHARS).collect::<String>()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_start(id: &str, title: &str, cwd: &str) -> String {
        serde_json::json!({
            "type": "session_start",
            "id": id,
            "title": title,
            "cwd": cwd,
        })
        .to_string()
    }

    fn user_message(id: &str, parent_id: Option<&str>, timestamp: &str, text: &str) -> String {
        serde_json::json!({
            "type": "message",
            "id": id,
            "parentId": parent_id,
            "timestamp": timestamp,
            "message": { "role": "user", "content": [{ "type": "text", "text": text }] },
        })
        .to_string()
    }

    fn assistant_message(id: &str, parent_id: Option<&str>, timestamp: &str, text: &str) -> String {
        serde_json::json!({
            "type": "message",
            "id": id,
            "parentId": parent_id,
            "timestamp": timestamp,
            "message": { "role": "assistant", "content": [{ "type": "text", "text": text }] },
        })
        .to_string()
    }

    #[test]
    fn summary_uses_header_id_title_and_cwd() {
        let content = [
            session_start("session-a", "Add Droid thread sidebar", r"D:\work\project"),
            user_message("m1", None, "2026-09-12T08:59:39.516Z", "hello"),
        ]
        .join("\n");

        let summary = parse_session_summary(&content).expect("valid session header");

        assert_eq!(summary.session_id, "session-a");
        assert_eq!(summary.title, "Add Droid thread sidebar");
        assert_eq!(summary.project_root, r"D:\work\project");
        assert_eq!(
            summary.last_activity_secs,
            parse_timestamp("2026-09-12T08:59:39.516Z")
                .unwrap()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
        );
    }

    #[test]
    fn placeholder_title_falls_back_to_first_real_user_message() {
        let content = [
            session_start("session-a", "Start new chat", "/work/project"),
            user_message(
                "m1",
                None,
                "2026-09-12T08:59:39.516Z",
                "<system-reminder>\ncontext\n</system-reminder>\n优化下服务器cpa调用性能",
            ),
        ]
        .join("\n");

        let summary = parse_session_summary(&content).expect("valid session header");

        assert_eq!(summary.title, "优化下服务器cpa调用性能");
    }

    #[test]
    fn synthetic_and_reminder_only_messages_never_become_the_title() {
        let content = [
            session_start("session-a", "New Session", "/work/project"),
            user_message(
                "m1",
                None,
                "2026-09-12T08:59:39.516Z",
                "<system-reminder>\nonly context\n</system-reminder>",
            ),
            user_message(
                "m2",
                Some("m1"),
                "2026-09-12T08:59:40.516Z",
                "Request interrupted by user",
            ),
            user_message(
                "m3",
                Some("m2"),
                "2026-09-12T08:59:41.516Z",
                "Skill \"init\" activated",
            ),
        ]
        .join("\n");

        let summary = parse_session_summary(&content).expect("valid session header");

        assert_eq!(summary.title, DEFAULT_TITLE);
    }

    #[test]
    fn headerless_session_is_rejected_but_malformed_lines_are_skipped() {
        assert!(parse_session_summary("not json\n{\"type\":\"message\"}").is_none());

        let content = [
            session_start("session-a", "Title", "/work/project"),
            "{ not json".to_string(),
            user_message("m1", None, "2026-09-12T08:59:39.516Z", "still indexed"),
        ]
        .join("\n");

        let summary = parse_session_summary(&content).expect("valid session header");
        assert_eq!(summary.session_id, "session-a");
    }

    #[test]
    fn greatest_valid_timestamp_wins_and_invalid_ones_are_ignored() {
        let content = [
            session_start("session-a", "Title", "/work/project"),
            user_message("m1", None, "2026-09-12T08:00:00.000Z", "first"),
            user_message("m2", Some("m1"), "not a timestamp", "second"),
            user_message("m3", Some("m2"), "2026-09-12T09:30:00.000Z", "third"),
        ]
        .join("\n");

        let summary = parse_session_summary(&content).expect("valid session header");

        assert_eq!(
            summary.last_activity_secs,
            parse_timestamp("2026-09-12T09:30:00.000Z")
                .unwrap()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
        );
    }

    #[test]
    fn windows_and_posix_cwds_are_recorded_verbatim() {
        for cwd in [r"C:\Users\alice\project", "/home/alice/project"] {
            let content = session_start("session-a", "Title", cwd);
            let summary = parse_session_summary(&content).expect("valid session header");
            assert_eq!(summary.project_root, cwd);
        }
    }

    #[test]
    fn title_is_normalized_and_truncated_to_sixty_characters() {
        let long_title = "word ".repeat(30);
        let content = session_start("session-a", &long_title, "/work/project");

        let summary = parse_session_summary(&content).expect("valid session header");

        assert_eq!(summary.title.chars().count(), MAX_TITLE_CHARS + 1);
        assert!(summary.title.ends_with('…'));
        assert!(!summary.title.contains("  "));
    }

    #[test]
    fn walks_parent_chain_and_discards_abandoned_branch() {
        let content = [
            session_start("s", "Title", "/work/project"),
            user_message("u1", None, "2026-09-12T08:00:00.000Z", "the real question"),
            assistant_message(
                "a1",
                Some("u1"),
                "2026-09-12T08:00:01.000Z",
                "abandoned answer",
            ),
            assistant_message(
                "a2",
                Some("u1"),
                "2026-09-12T08:00:02.000Z",
                "the kept answer",
            ),
        ]
        .join("\n");

        let classified = classify_transcript(&content);

        assert_eq!(
            classified.events,
            vec![
                RawEvent::Noise, // session_start side log
                RawEvent::User("the real question".to_string()),
                RawEvent::Assistant("the kept answer".to_string()),
            ]
        );
    }

    #[test]
    fn tool_use_and_tool_result_blocks_pair_and_reminders_are_noise() {
        let content = [
            session_start("s", "Title", "/work/project"),
            user_message(
                "u1",
                None,
                "2026-09-12T08:00:00.000Z",
                "<system-reminder>context</system-reminder>\nbuild it",
            ),
            serde_json::json!({
                "type": "message",
                "id": "a1",
                "parentId": "u1",
                "timestamp": "2026-09-12T08:00:01.000Z",
                "message": { "role": "assistant", "content": [
                    { "type": "thinking", "thinking": "hidden" },
                    { "type": "text", "text": "running build" },
                    { "type": "tool_use", "id": "t1", "name": "Execute", "input": { "command": "make" } },
                ] },
            })
            .to_string(),
            serde_json::json!({
                "type": "message",
                "id": "r1",
                "parentId": "a1",
                "timestamp": "2026-09-12T08:00:02.000Z",
                "message": { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "t1", "is_error": true, "content": "make: fatal" },
                ] },
            })
            .to_string(),
        ]
        .join("\n");

        let classified = classify_transcript(&content);

        assert_eq!(
            classified.events,
            vec![
                RawEvent::Noise, // session_start side log
                RawEvent::User("build it".to_string()),
                RawEvent::Noise, // thinking
                RawEvent::Assistant("running build".to_string()),
                RawEvent::ToolCall {
                    call_id: Some("t1".to_string()),
                    name: "Execute".to_string(),
                    detail: "make".to_string(),
                },
                RawEvent::ToolResult {
                    call_id: Some("t1".to_string()),
                    output: "make: fatal".to_string(),
                    is_error: true,
                },
            ]
        );
    }

    #[test]
    fn compaction_is_a_checkpoint_and_harness_records_are_noise() {
        let content = [
            session_start("s", "Title", "/work/project"),
            serde_json::json!({ "type": "todo_state", "id": "t1" }).to_string(),
            serde_json::json!({ "type": "compaction_state", "id": "c1" }).to_string(),
            serde_json::json!({ "type": "agent_turn_outcome", "turnId": "turn" }).to_string(),
            user_message("u1", Some("t1"), "2026-09-12T08:00:00.000Z", "hi"),
            serde_json::json!({ "type": "brand_new", "id": "x1" }).to_string(),
        ]
        .join("\n");

        let classified = classify_transcript(&content);

        assert_eq!(
            classified.events,
            vec![
                RawEvent::Noise, // session header
                RawEvent::Noise, // todo_state
                RawEvent::Checkpoint("compaction".to_string()),
                RawEvent::Noise, // agent_turn_outcome
                RawEvent::User("hi".to_string()),
            ]
        );
        assert_eq!(classified.unknown_count, 1);
    }

    #[test]
    fn malformed_lines_are_counted_without_losing_valid_siblings() {
        let content = [
            session_start("s", "Title", "/work/project"),
            "{ not json".to_string(),
            user_message("u1", None, "2026-09-12T08:00:00.000Z", "keep me"),
        ]
        .join("\n");

        let classified = classify_transcript(&content);

        assert_eq!(classified.malformed_count, 1);
        assert_eq!(
            classified.events,
            vec![RawEvent::Noise, RawEvent::User("keep me".to_string()),]
        );
    }
}
