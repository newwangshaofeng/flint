//! Reads IDE context out of the running editor and publishes it to the bridge.
//!
//! This is the only module that touches GPUI, and it runs entirely on the
//! foreground thread. Subscriptions convert editor state into plain data and
//! hand it to [`Bridge`], which the HTTP worker threads read. Nothing here may
//! be called from a worker thread.
//!
//! Remote workspaces are excluded on purpose. A remote project's files live on
//! another host, so a local bridge that reported them would hand the CLI paths
//! it cannot read.

use crate::SharedLockFile;
use crate::state::{
    ActiveFile, Bridge, ContextSnapshot, DiagnosticRange, FileDiagnostics, IdeDiagnostic, OpenFile,
    Position, SelectionSnapshot,
};
use editor::{Editor, EditorEvent};
use gpui::{App, Window};
use language::{Buffer, BufferEvent, Diagnostic, DiagnosticSeverity};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use text::PointUtf16;
use workspace::{MultiWorkspace, Workspace};

/// Starts collecting context into `bridge`.
///
/// Subscribes to the entities that exist now and to every one created later, so
/// windows opened after startup are covered. Discovered workspace roots are
/// published through `lock_file`.
pub fn start(bridge: Bridge, lock_file: SharedLockFile, cx: &mut App) {
    // Every subscription here is detached rather than stored. They must outlive
    // this call, but keeping the handles would retain one per editor and buffer
    // ever created; GPUI already drops an entity's listeners when the entity is
    // released, so there is nothing left to clean up.
    cx.observe_new({
        let bridge = bridge.clone();
        let lock_file = lock_file.clone();
        move |_multi_workspace: &mut MultiWorkspace, _, cx| {
            refresh(&bridge, &lock_file, cx);
        }
    })
    .detach();

    // Activating a workspace, or moving between its tabs, changes what the CLI
    // should consider current.
    cx.observe_new({
        let bridge = bridge.clone();
        let lock_file = lock_file.clone();
        move |_workspace: &mut Workspace, _, cx| {
            cx.subscribe_self({
                let bridge = bridge.clone();
                let lock_file = lock_file.clone();
                move |_workspace, event: &workspace::Event, cx| {
                    if matches!(
                        event,
                        workspace::Event::ActiveItemChanged
                            | workspace::Event::ItemAdded { .. }
                            | workspace::Event::ItemRemoved { .. }
                            | workspace::Event::Activate
                    ) {
                        refresh(&bridge, &lock_file, cx);
                    }
                }
            })
            .detach();
            refresh(&bridge, &lock_file, cx);
        }
    })
    .detach();

    // Any of these can move the selection or change what the CLI should see.
    cx.observe_new({
        let bridge = bridge.clone();
        let lock_file = lock_file.clone();
        move |_editor: &mut Editor, _, cx| {
            cx.subscribe_self({
                let bridge = bridge.clone();
                let lock_file = lock_file.clone();
                move |_editor, event: &EditorEvent, cx| {
                    if matches!(
                        event,
                        EditorEvent::SelectionsChanged { .. }
                            | EditorEvent::Saved
                            | EditorEvent::DirtyChanged
                            | EditorEvent::TitleChanged
                            | EditorEvent::FileHandleChanged
                    ) {
                        refresh(&bridge, &lock_file, cx);
                    }
                }
            })
            .detach();
        }
    })
    .detach();

    // Diagnostics are attached to buffers, not editors.
    cx.observe_new({
        let bridge = bridge.clone();
        let lock_file = lock_file.clone();
        move |_buffer: &mut Buffer, _, cx| {
            cx.subscribe_self({
                let bridge = bridge.clone();
                let lock_file = lock_file.clone();
                move |_buffer, event: &BufferEvent, cx| {
                    if matches!(event, BufferEvent::DiagnosticsUpdated) {
                        refresh(&bridge, &lock_file, cx);
                    }
                }
            })
            .detach();
        }
    })
    .detach();

    refresh(&bridge, &lock_file, cx);
}

/// Recomputes the snapshot from the active window and publishes it.
///
/// With no window, or a window whose workspace is remote, the published context
/// is emptied rather than left stale.
fn refresh(bridge: &Bridge, lock_file: &SharedLockFile, cx: &mut App) {
    let Some(window) = cx.active_window() else {
        bridge.clear();
        return;
    };

    let collected = window
        .update(cx, |_root, window, cx| collect(window, cx))
        .ok()
        .flatten();

    let Some((snapshot, diagnostics)) = collected else {
        bridge.clear();
        return;
    };

    // The CLI matches its working directory against these roots, so they must
    // track what is actually open.
    if let Err(error) = lock_file
        .lock()
        .replace_workspace_folders(snapshot.workspace_folders.clone())
    {
        log::warn!("droid_mcp: failed to publish workspace folders: {error:#}");
    }

    bridge.publish(snapshot, diagnostics);
}

/// Builds the snapshot and the active file's diagnostics from one window.
///
/// Returns `None` when the window has no local workspace to describe.
fn collect(
    window: &mut Window,
    cx: &mut App,
) -> Option<(ContextSnapshot, Option<(PathBuf, FileDiagnostics)>)> {
    let workspace = Workspace::for_window(window, cx)?;

    // The workspace borrow ends before diagnostics are gathered, because that
    // step needs `&mut App` to visit every window.
    let (workspace_folders, active_file, open_files) = {
        let workspace = workspace.read(cx);

        // Remote projects are out of scope: the CLI runs locally and cannot
        // read paths that live on another host.
        if workspace.project().read(cx).remote_client().is_some() {
            return None;
        }

        let workspace_folders = workspace
            .root_paths(cx)
            .into_iter()
            .map(|path| PathBuf::from(path.as_ref()))
            .collect();

        let active_file = workspace
            .active_item(cx)
            .and_then(|item| item.act_as::<Editor>(cx))
            .and_then(|editor| active_file(&editor, cx));

        let open_files = open_files(workspace, cx);

        (workspace_folders, active_file, open_files)
    };

    let diagnostics = active_file.as_ref().and_then(|active_file| {
        let path = PathBuf::from(&active_file.path);
        diagnostics_for(&path, cx).map(|diagnostics| (path, diagnostics))
    });

    Some((
        ContextSnapshot {
            workspace_folders,
            active_file,
            open_files,
        },
        diagnostics,
    ))
}

/// Builds the active-file payload, or `None` for a non-local or unsaved buffer.
fn active_file(editor: &gpui::Entity<Editor>, cx: &App) -> Option<ActiveFile> {
    let editor = editor.read(cx);
    let multi_buffer = editor.buffer().read(cx);

    // Only single-buffer editors describe one file; a multi-buffer (a diff, a
    // search result) has no single path to report.
    let buffer = multi_buffer.as_singleton()?;
    let buffer = buffer.read(cx);
    let path = buffer.file()?.as_local()?.abs_path(cx);

    // Selections are anchored to the multi-buffer, so resolve them through its
    // snapshot to get UTF-16 coordinates.
    let snapshot = multi_buffer.snapshot(cx);
    let selection = editor.selections.newest_anchor();
    let start = snapshot.summary_for_anchor::<PointUtf16>(&selection.start);
    let end = snapshot.summary_for_anchor::<PointUtf16>(&selection.end);
    let selected_text = if start == end {
        String::new()
    } else {
        snapshot.text_for_range(start..end).collect()
    };

    Some(ActiveFile {
        path: path.to_string_lossy().into_owned(),
        file_name: file_name(&path),
        is_dirty: buffer.is_dirty(),
        line_count: snapshot.max_point().row + 1,
        selection: SelectionSnapshot {
            start_line: start.row,
            start_character: start.column,
            end_line: end.row,
            end_character: end.column,
            selected_text,
        },
    })
}

/// Lists the local files open in the workspace's panes, de-duplicated by path.
fn open_files(workspace: &Workspace, cx: &App) -> Vec<OpenFile> {
    let mut files = Vec::new();
    let mut seen = HashSet::new();

    for pane in workspace.panes() {
        for item in pane.read(cx).items() {
            let Some(editor) = item.act_as::<Editor>(cx) else {
                continue;
            };
            let editor = editor.read(cx);
            let multi_buffer = editor.buffer().read(cx);

            let Some(buffer) = multi_buffer.as_singleton() else {
                continue;
            };
            let buffer = buffer.read(cx);
            let Some(local_file) = buffer.file().and_then(|file| file.as_local()) else {
                continue;
            };
            let path = local_file.abs_path(cx);
            let path = path.to_string_lossy().into_owned();
            if !seen.insert(path.clone()) {
                continue;
            }

            files.push(OpenFile {
                file_name: file_name(Path::new(&path)),
                path,
                is_dirty: buffer.is_dirty(),
                language_id: buffer
                    .language()
                    .map(|language| language.name().0.to_string())
                    .unwrap_or_default(),
            });
        }
    }

    files
}

/// Reads diagnostics for `path` from whichever open buffer backs it.
///
/// Diagnostics live on buffers, so the buffer is located by matching absolute
/// paths rather than by maintaining a parallel registry.
fn diagnostics_for(path: &Path, cx: &mut App) -> Option<FileDiagnostics> {
    // Any project can own the buffer; the CLI may ask about a file that is open
    // in a window that is not focused.
    for window in cx.windows() {
        let found = window
            .update(cx, |_root, window, cx| {
                let workspace = Workspace::for_window(window, cx)?;
                let project = workspace.read(cx).project().clone();
                for buffer in project.read(cx).opened_buffers(cx) {
                    let buffer = buffer.read(cx);
                    let Some(local_file) = buffer.file().and_then(|file| file.as_local()) else {
                        continue;
                    };
                    if local_file.abs_path(cx).as_path() != path {
                        continue;
                    }
                    return Some(collect_buffer_diagnostics(buffer));
                }
                None
            })
            .ok()
            .flatten();

        if found.is_some() {
            return found;
        }
    }

    None
}

fn collect_buffer_diagnostics(buffer: &Buffer) -> FileDiagnostics {
    let snapshot = buffer.snapshot();
    let entries = snapshot
        .diagnostics_in_range::<PointUtf16, PointUtf16>(
            PointUtf16::zero()..snapshot.max_point_utf16(),
            false,
        )
        .map(|entry| to_ide_diagnostic(entry.diagnostic, entry.range))
        .collect::<Vec<_>>();

    FileDiagnostics {
        total_count: entries.len(),
        entries,
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Converts an LSP diagnostic into the wire form: 0-based severity and
/// zero-based UTF-16 positions.
fn to_ide_diagnostic(diagnostic: &Diagnostic, range: std::ops::Range<PointUtf16>) -> IdeDiagnostic {
    IdeDiagnostic {
        severity: severity_rank(diagnostic.severity),
        message: diagnostic.message.clone(),
        source: diagnostic.source.clone(),
        code: diagnostic.code.as_ref().map(|code| code.to_string()),
        range: DiagnosticRange {
            start: Position {
                line: range.start.row,
                character: range.start.column,
            },
            end: Position {
                line: range.end.row,
                character: range.end.column,
            },
        },
    }
}

/// Maps LSP severities (error=1 .. hint=4) onto the CLI's 0-based scale.
fn severity_rank(severity: DiagnosticSeverity) -> u8 {
    if severity == DiagnosticSeverity::ERROR {
        0
    } else if severity == DiagnosticSeverity::WARNING {
        1
    } else if severity == DiagnosticSeverity::INFORMATION {
        2
    } else {
        3
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severities_map_onto_the_zero_based_scale() {
        assert_eq!(severity_rank(DiagnosticSeverity::ERROR), 0);
        assert_eq!(severity_rank(DiagnosticSeverity::WARNING), 1);
        assert_eq!(severity_rank(DiagnosticSeverity::INFORMATION), 2);
        assert_eq!(severity_rank(DiagnosticSeverity::HINT), 3);
    }

    #[test]
    fn diagnostics_are_converted_with_utf16_positions() {
        let diagnostic = Diagnostic {
            message: "unused variable".to_string(),
            source: Some("rust-analyzer".to_string()),
            severity: DiagnosticSeverity::WARNING,
            ..Diagnostic::default()
        };
        let range = PointUtf16::new(3, 7)..PointUtf16::new(3, 9);

        let converted = to_ide_diagnostic(&diagnostic, range);
        assert_eq!(converted.severity, 1);
        assert_eq!(converted.range.start.line, 3);
        assert_eq!(converted.range.start.character, 7);
        assert_eq!(converted.range.end.character, 9);
        assert_eq!(converted.source.as_deref(), Some("rust-analyzer"));
    }
}
