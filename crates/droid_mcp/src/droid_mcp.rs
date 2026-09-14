//! Exposes Flint's IDE context to Factory Droid.
//!
//! Droid's CLI discovers a local IDE bridge through
//! `~/.factory/ide/<port>.lock` and `FACTORY_VSCODE_MCP_PORT`, then speaks MCP
//! over loopback HTTP. This crate supplies both halves: the lock file and the
//! server, plus the GPUI-side collector that keeps the served context current.
//!
//! Only read-only context is exposed (workspace roots, the active file and
//! selection, open files, and diagnostics). Remote projects are excluded: the
//! CLI runs locally and cannot read paths that live on another host.

mod context;
mod discovery;
mod protocol;
mod server;
mod state;

pub use discovery::{IDE_NAME, LockFile};
pub use protocol::MCP_PATH;
pub use state::{Bridge, IdeDiagnostic};

use anyhow::Result;
use gpui::{App, AppContext as _, Global, Task};
use parking_lot::Mutex;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

/// How often the lock file is refreshed and a heartbeat is published.
///
/// Refreshing rewrites the file, which repairs it if it was removed or edited
/// while Flint is running.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

/// The lock file, shared between the foreground collector (which updates the
/// workspace list) and the background heartbeat (which repairs the file).
pub(crate) type SharedLockFile = Arc<Mutex<LockFile>>;

/// A running bridge, kept alive by the app for as long as it should serve.
struct RunningBridge {
    bridge: Bridge,
    server: server::ServerHandle,
    lock_file: SharedLockFile,
    /// Held only for its lifetime: dropping the task stops the heartbeat.
    _heartbeat: Task<()>,
}

impl Drop for RunningBridge {
    fn drop(&mut self) {
        // Wake SSE threads and waiters first so they stop touching the bridge,
        // then stop accepting new work.
        self.bridge.shutdown();
        self.server.shutdown();
        self.lock_file.lock().remove();
    }
}

/// Keeps the bridge alive for the lifetime of the app.
struct BridgeHandle {
    /// Never read; owning the bridge is the whole point.
    _running: Rc<RunningBridge>,
}
impl Global for BridgeHandle {}

/// Starts the IDE bridge for the real application.
///
/// Must be called only from the application entry point, never from a test
/// harness: it binds a real socket and writes to the user's home directory.
/// Failure is logged and swallowed, because a missing IDE bridge must not stop
/// Flint from starting.
pub fn init(cx: &mut App) {
    match start_bridge(cx) {
        Ok(running) => {
            let port = running.server.port();
            // Publishing the port is what makes local terminals hand it to the
            // CLI. Remote terminals ignore it, since the port is loopback-only.
            cx.set_global(terminal::DroidMcpPort(port));
            log::info!("droid_mcp: IDE context bridge ready on port {port}");
            cx.set_global(BridgeHandle {
                _running: Rc::new(running),
            });
        }
        Err(error) => {
            log::error!("droid_mcp: failed to start the IDE context bridge: {error:#}");
        }
    }
}

fn start_bridge(cx: &mut App) -> Result<RunningBridge> {
    let bridge = Bridge::new();
    let server = server::start(bridge.clone())?;

    let lock_file = LockFile::new(server.port());
    // Publish the lock file before any workspace is known so a CLI that is
    // already running can discover the port; folders follow as collection runs.
    lock_file.write()?;
    let lock_file = Arc::new(Mutex::new(lock_file));

    context::start(bridge.clone(), lock_file.clone(), cx);

    let heartbeat_bridge = bridge.clone();
    let heartbeat_lock = lock_file.clone();
    let executor = cx.background_executor().clone();
    let _heartbeat = cx.background_spawn(async move {
        loop {
            executor.timer(HEARTBEAT_INTERVAL).await;
            heartbeat_bridge.queue_heartbeat(server::now_millis());
            if let Err(error) = heartbeat_lock.lock().write() {
                log::warn!("droid_mcp: failed to refresh the discovery lock file: {error:#}");
            }
        }
    });

    Ok(RunningBridge {
        bridge,
        server,
        lock_file,
        _heartbeat,
    })
}
