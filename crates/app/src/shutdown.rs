//! Deterministic shutdown coordinator (issue #7).
//!
//! One owner, `Shutdown`, tracks every worker handle the app spawns and
//! runs the teardown sequence exactly once (window-close and app-quit both
//! call `run`; the `done` latch makes the second call a no-op).
//!
//! Order matters:
//! 1. transport: stop playback → panic/All-Notes-Off to every warm output,
//!    armed recording finishes (commits the take — same as an explicit
//!    Stop), playback thread joined.
//! 2. the standalone plugin-editor window closes BEFORE helper teardown —
//!    it hosts an in-process plugin instance that must outlive nothing.
//! 3. sidecar + global prefs flush (the same synchronous writes the
//!    normal persist paths use).
//! 4. MCP: signal `serve_http` → it stops accepting connections and drains
//!    in-flight requests (axum graceful shutdown); join the runtime thread
//!    bounded.
//! 5. plugin host: Clear + Shutdown → worker drops all instances (helper
//!    subprocesses die with their `PluginOutput`s); join bounded.
//! 6. plugin-scan thread: bounded join; a mid-flight scan is CPU/disk only
//!    and dies with the process anyway.
//!
//! Every join is bounded: `JoinHandle` has no timed join, so completion is
//! relayed through a channel and the caller gives up after its budget —
//! a wedged plugin or device can never hang process exit.

use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::EditorView;

/// Budget per teardown phase. Total worst case stays well under 10s even
/// if every worker is wedged.
const MCP_JOIN_BUDGET: Duration = Duration::from_secs(2);
const HOST_JOIN_BUDGET: Duration = Duration::from_secs(4);
const SCAN_JOIN_BUDGET: Duration = Duration::from_secs(1);

/// Join `h` but wait at most `d`; returns true if the thread finished.
/// The relay thread keeps the handle if it never completes — acceptable:
/// we only do this once, at exit.
fn join_bounded(h: JoinHandle<()>, d: Duration) -> bool {
    let (tx, rx) = std_mpsc::channel();
    std::thread::spawn(move || {
        let _ = h.join();
        let _ = tx.send(());
    });
    rx.recv_timeout(d).is_ok()
}

/// Owns the handles of every spawned worker + the once-only latch.
#[derive(Default)]
pub(crate) struct Shutdown {
    /// fires `serve_http`'s graceful-shutdown future
    mcp_stop: Option<tokio::sync::oneshot::Sender<()>>,
    mcp_thread: Option<JoinHandle<()>>,
    host_thread: Option<JoinHandle<()>>,
    scan_thread: Option<JoinHandle<()>>,
    done: bool,
}

impl Shutdown {
    pub(crate) fn track_mcp(
        &mut self,
        stop: tokio::sync::oneshot::Sender<()>,
        thread: JoinHandle<()>,
    ) {
        self.mcp_stop = Some(stop);
        self.mcp_thread = Some(thread);
    }

    pub(crate) fn track_host(&mut self, thread: JoinHandle<()>) {
        self.host_thread = Some(thread);
    }

    /// A rescan replaces the previous handle — join the old one
    /// opportunistically (it exits on its own anyway).
    pub(crate) fn track_scan(&mut self, thread: JoinHandle<()>) {
        if let Some(old) = self.scan_thread.replace(thread) {
            let _ = join_bounded(old, Duration::from_millis(50));
        }
    }

    /// Ordered teardown; idempotent — a second call (window-close then
    /// app-quit, or a re-fired hook) does nothing.
    pub(crate) fn run(&mut self, v: &mut EditorView) {
        if self.done {
            return;
        }
        self.done = true;

        // 1. transport: panic/All-Notes-Off on every warm output before any
        //    destination closes; playback thread joined inside stop();
        //    an armed recording is committed by finish_record — an
        //    intentional choice equal to pressing Stop first.
        v.stop_playback();

        // 2. plugin editor window before helper teardown
        if let Some(mut pw) = v.plugin_window.take() {
            pw.close();
        }
        v.editor_plugin = None;

        // 3. flush sidecar + global prefs (same sync writes the normal
        //    persist paths use — cheap no-ops when nothing changed)
        v.persist();
        v.save_global();

        // 4. MCP + scan workers (bounded joins)
        self.teardown_workers();

        // 5. plugin host: unload everything, then exit the worker. On
        //    timeout the helpers are our children — process exit reaps
        //    them; we do NOT taskkill by image name (that would hit other
        //    instances' helpers).
        let _ = v.plugin_req.send(output::PluginReq::Clear);
        let _ = v.plugin_req.send(output::PluginReq::Shutdown);
        if let Some(h) = self.host_thread.take() {
            if !join_bounded(h, HOST_JOIN_BUDGET) {
                eprintln!("shutdown: plugin host did not stop in {HOST_JOIN_BUDGET:?}");
            }
        }
        v.plugin_slots.clear();
        v.plugin_state.clear();
    }

    /// Steps that don't need the view: signal the MCP server and join its
    /// runtime + the scan worker, each with a time budget.
    fn teardown_workers(&mut self) {
        // MCP: stop accepting, drain in-flight, stop the runtime thread
        if let Some(tx) = self.mcp_stop.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.mcp_thread.take() {
            if !join_bounded(h, MCP_JOIN_BUDGET) {
                eprintln!("shutdown: mcp runtime did not stop in {MCP_JOIN_BUDGET:?}");
            }
        }
        // plugin-scan worker: bounded attempt; dies with the process.
        if let Some(h) = self.scan_thread.take() {
            let _ = join_bounded(h, SCAN_JOIN_BUDGET);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_bounded_returns_true_for_finished_thread() {
        let h = std::thread::spawn(|| {});
        assert!(join_bounded(h, Duration::from_secs(2)));
    }

    #[test]
    fn join_bounded_gives_up_on_stuck_thread() {
        let (_keep, block) = std_mpsc::channel::<()>();
        let h = std::thread::spawn(move || {
            let _ = block.recv();
        });
        let t0 = std::time::Instant::now();
        assert!(!join_bounded(h, Duration::from_millis(60)));
        assert!(t0.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn teardown_workers_signals_and_joins() {
        let mut s = Shutdown::default();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let (done, wait) = std_mpsc::channel::<()>();
        let worker = std::thread::spawn(move || {
            let _ = rx.blocking_recv();
            let _ = done.send(());
        });
        s.track_mcp(tx, worker);
        s.track_scan(std::thread::spawn(|| {}));
        s.teardown_workers();
        // graceful signal reached the fake runtime and its thread joined
        assert!(wait.recv_timeout(Duration::from_secs(2)).is_ok());
        assert!(s.mcp_stop.is_none() && s.mcp_thread.is_none() && s.scan_thread.is_none());
    }

    #[test]
    fn teardown_workers_survives_wedged_thread() {
        let mut s = Shutdown::default();
        // worker that never exits — teardown must not hang on it
        let (_keep, block) = std_mpsc::channel::<()>();
        s.scan_thread = Some(std::thread::spawn(move || {
            let _ = block.recv();
        }));
        let t0 = std::time::Instant::now();
        s.teardown_workers();
        assert!(t0.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn track_scan_replaces_older_handle() {
        let mut s = Shutdown::default();
        let h1 = std::thread::spawn(|| {});
        let h2 = std::thread::spawn(|| {});
        s.track_scan(h1);
        s.track_scan(h2);
        assert!(s.scan_thread.is_some());
    }
}
