//! Windows session-shutdown hook (#198).
//!
//! An OS logoff/restart bypasses the window-close path entirely: gpui
//! answers `WM_QUERYENDSESSION` with TRUE unconditionally and routes
//! `WM_ENDSESSION` straight to the quit observers — no discard guard and
//! no document snapshot — so a dirty document was silently lost. This
//! module subclasses the main window with comctl32 `SetWindowSubclass`
//! and adds conservative protection ON TOP of that behavior:
//!
//! - `WM_QUERYENDSESSION`: while the document is dirty (or its state
//!   cannot be determined under a contended lock), write an emergency
//!   recovery snapshot and register a shutdown-block reason so the
//!   shutdown screen explains the brief linger. The message then falls
//!   through to the default handling — the shutdown is never vetoed;
//!   the snapshot is the protection, not a block.
//! - `WM_ENDSESSION` `wParam=TRUE`: synchronously `persist()` the
//!   view-state sidecar. `wParam=FALSE` (shutdown cancelled): destroy
//!   the block reason so it cannot outlive the attempt it described.
//! - EVERY message ends in `DefSubclassProc` and any internal failure is
//!   swallowed — the hook can only add protection, never remove it.
//!
//! The dirty/snapshot decision lives in the `plan_*` pure functions and
//! the Win32 calls sit behind the `SessionEndFx` seam, so CI exercises
//! the whole dispatch with no window at all. The proc itself — which
//! only runs on a real logoff/reboot — stays a manual verification item.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

use windows::core::HSTRING;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Shutdown::{ShutdownBlockReasonCreate, ShutdownBlockReasonDestroy};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{WM_ENDSESSION, WM_QUERYENDSESSION};

use crate::{recovery, t, EditorView};
use mcp_server::{Shared, SharedDoc};

/// Identifies this hook's subclass entry — reinstalling with the same
/// (proc, id) pair would only refresh the refdata, never stack a second
/// proc (issue #198 → 0x198).
const SUBCLASS_ID: usize = 0x198;

/// The HWND this hook is installed on, or 0 — the install-once latch.
static SUBCLASSED_HWND: AtomicUsize = AtomicUsize::new(0);

/// What the hook needs at message time, carried as the subclass refdata.
///
/// `view` is a raw pointer to the boxed `EditorView` inside gpui's
/// entity map: the box's contents stay at a stable address for the
/// app's life (the map may move the `Box`, never its pointee), and the
/// proc runs on the UI thread where no entity lease is live while the
/// OS dispatches these two messages. It is dereferenced only inside
/// `catch_unwind` — worst case the sidecar flush is skipped, which is
/// exactly the pre-hook behavior.
struct HookState {
    view: *mut EditorView,
    doc: Weak<Mutex<Shared>>,
}

/// `WM_QUERYENDSESSION` decision — see `plan_query_endsession`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum QueryPlan {
    /// Clean document — nothing to protect; gpui's default TRUE stands.
    PassThrough,
    /// Dirty (or undeterminable): emergency snapshot + block reason, and
    /// STILL allow the shutdown — never trap the user's machine.
    ProtectThenAllow,
}

/// `WM_ENDSESSION` decision — see `plan_endsession`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum EndPlan {
    /// `wParam=TRUE` — flush the view-state sidecar, then pass through
    /// so gpui's quit observers still run.
    PersistThenPass,
    /// `wParam=FALSE` — shutdown cancelled: drop the block reason.
    ClearReason,
}

/// The dirty/snapshot policy for `WM_QUERYENDSESSION`. `dirty` is an
/// `Option` because the shared-document lock is probed with `try_lock`
/// in the proc: `None` means "cannot tell". Between the two wrong
/// guesses, treating unknown as dirty costs one extra snapshot file
/// while treating it as clean could lose the user's work — conservatism
/// wins.
pub(crate) fn plan_query_endsession(dirty: Option<bool>) -> QueryPlan {
    match dirty {
        Some(false) => QueryPlan::PassThrough,
        Some(true) | None => QueryPlan::ProtectThenAllow,
    }
}

/// The `WM_ENDSESSION` policy; `ending` is `wParam != 0`.
pub(crate) fn plan_endsession(ending: bool) -> EndPlan {
    if ending {
        EndPlan::PersistThenPass
    } else {
        EndPlan::ClearReason
    }
}

/// The effects the hook performs — real Win32/disk calls in production,
/// a recorder in tests. Keeps every decision reachable without a window.
pub(crate) trait SessionEndFx {
    /// Best-effort emergency snapshot of the document.
    fn emergency_snapshot(&mut self, doc: &SharedDoc);
    /// Explain the brief linger on the shutdown screen.
    fn set_block_reason(&mut self, hwnd: usize, reason: &'static str);
    /// Called when the shutdown is cancelled (`wParam=FALSE`).
    fn clear_block_reason(&mut self, hwnd: usize);
    /// Synchronous `EditorView::persist()` (sidecar write).
    fn persist_view(&mut self, view: *mut EditorView);
}

/// Production `SessionEndFx`: the real Win32 + disk calls.
struct WinFx;

impl SessionEndFx for WinFx {
    fn emergency_snapshot(&mut self, doc: &SharedDoc) {
        let _ = recovery::write_emergency_snapshot(doc, &recovery::recovery_dir());
    }
    fn set_block_reason(&mut self, hwnd: usize, reason: &'static str) {
        unsafe {
            let _ = ShutdownBlockReasonCreate(HWND(hwnd as _), &HSTRING::from(reason));
        }
    }
    fn clear_block_reason(&mut self, hwnd: usize) {
        unsafe {
            let _ = ShutdownBlockReasonDestroy(HWND(hwnd as _));
        }
    }
    fn persist_view(&mut self, view: *mut EditorView) {
        if !view.is_null() {
            unsafe { (*view).persist() };
        }
    }
}

/// `WM_QUERYENDSESSION` work: probe the dirty flag WITHOUT blocking
/// (a contended lock counts as "cannot tell" → treated as dirty), run
/// the plan, never veto.
fn on_query_endsession(hwnd: usize, doc: &Weak<Mutex<Shared>>, fx: &mut impl SessionEndFx) {
    let dirty = doc
        .upgrade()
        .and_then(|shared| shared.try_lock().ok().map(|sh| sh.is_dirty()));
    if plan_query_endsession(dirty) == QueryPlan::ProtectThenAllow {
        if let Some(shared) = doc.upgrade() {
            fx.emergency_snapshot(&shared);
        }
        fx.set_block_reason(hwnd, t("shutdown.block_reason"));
    }
}

/// `WM_ENDSESSION` work per `wParam`.
fn on_endsession(hwnd: usize, wparam: usize, state: &HookState, fx: &mut impl SessionEndFx) {
    match plan_endsession(wparam != 0) {
        EndPlan::PersistThenPass => fx.persist_view(state.view),
        EndPlan::ClearReason => fx.clear_block_reason(hwnd),
    }
}

/// The subclass proc: handle the two session messages, then ALWAYS fall
/// through to the default handling — no early return can ever swallow
/// gpui's own session handling.
unsafe extern "system" fn session_end_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _uidsubclass: usize,
    dwrefdata: usize,
) -> LRESULT {
    if matches!(msg, WM_QUERYENDSESSION | WM_ENDSESSION) && dwrefdata != 0 {
        // A panic may not cross this FFI boundary — it would abort the
        // process mid-shutdown. A caught error falls through to the
        // default handling, identical to the pre-hook behavior.
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let state = &*(dwrefdata as *const HookState);
            let mut fx = WinFx;
            if msg == WM_QUERYENDSESSION {
                on_query_endsession(hwnd.0 as usize, &state.doc, &mut fx);
            } else {
                on_endsession(hwnd.0 as usize, wparam.0, state, &mut fx);
            }
        }));
    }
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// Install the subclass on the app's main window once its HWND is known.
/// Idempotent — a second call for the same window is a no-op and the
/// fixed (proc, `SUBCLASS_ID`) pair makes `SetWindowSubclass` itself a
/// refdata refresh rather than a stack. No unhooking is needed at exit:
/// the subclass dies with the window and the leaked `HookState` box is
/// reclaimed with the process.
pub(crate) fn install(hwnd: usize, view: &mut EditorView) -> bool {
    if hwnd == 0 || SUBCLASSED_HWND.swap(hwnd, Ordering::SeqCst) == hwnd {
        return false;
    }
    let state = Box::new(HookState {
        view,
        doc: Arc::downgrade(&view.shared),
    });
    let raw = std::boxed::Box::into_raw(state);
    let ok = unsafe {
        SetWindowSubclass(
            HWND(hwnd as _),
            Some(session_end_proc),
            SUBCLASS_ID,
            raw as usize,
        )
    };
    if ok.as_bool() {
        true
    } else {
        // leave no half-installed state: drop the box, allow a retry
        SUBCLASSED_HWND.store(0, Ordering::SeqCst);
        let _ = unsafe { std::boxed::Box::from_raw(raw) };
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Records every effect call so the dispatch can be verified with no
    /// window and no Win32.
    #[derive(Default)]
    struct RecFx {
        calls: Rc<RefCell<Vec<&'static str>>>,
    }

    impl SessionEndFx for RecFx {
        fn emergency_snapshot(&mut self, _doc: &SharedDoc) {
            self.calls.borrow_mut().push("snapshot");
        }
        fn set_block_reason(&mut self, _hwnd: usize, _reason: &'static str) {
            self.calls.borrow_mut().push("block_reason");
        }
        fn clear_block_reason(&mut self, _hwnd: usize) {
            self.calls.borrow_mut().push("clear_reason");
        }
        fn persist_view(&mut self, _view: *mut EditorView) {
            self.calls.borrow_mut().push("persist");
        }
    }

    fn shared() -> SharedDoc {
        let f = smf_core::File {
            format: 1,
            division: smf_core::Division::Metrical(480),
            tracks: vec![smf_core::Track { events: vec![] }],
            warnings: vec![],
        };
        std::sync::Arc::new(std::sync::Mutex::new(mcp_server::Shared::new(
            document::Document::from_file(f),
        )))
    }

    fn dirty_doc(doc: &SharedDoc) {
        crate::lock_shared(doc)
            .apply(
                "add track",
                vec![document::Op::InsertTrack {
                    index: 0,
                    track: document::Track {
                        name: None,
                        out_port: 0,
                        out_channel: 0,
                        events: vec![],
                    },
                }],
            )
            .unwrap();
    }

    #[test]
    fn plan_query_truth_table() {
        assert_eq!(plan_query_endsession(Some(false)), QueryPlan::PassThrough);
        assert_eq!(
            plan_query_endsession(Some(true)),
            QueryPlan::ProtectThenAllow
        );
        // "cannot tell" takes the conservative branch
        assert_eq!(plan_query_endsession(None), QueryPlan::ProtectThenAllow);
    }

    #[test]
    fn plan_end_truth_table() {
        assert_eq!(plan_endsession(true), EndPlan::PersistThenPass);
        assert_eq!(plan_endsession(false), EndPlan::ClearReason);
    }

    #[test]
    fn query_clean_doc_does_nothing() {
        let doc = shared();
        let weak = Arc::downgrade(&doc);
        let mut fx = RecFx::default();
        let calls = fx.calls.clone();
        on_query_endsession(1, &weak, &mut fx);
        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn query_dirty_doc_snapshots_and_blocks() {
        let doc = shared();
        dirty_doc(&doc);
        let weak = Arc::downgrade(&doc);
        let mut fx = RecFx::default();
        let calls = fx.calls.clone();
        on_query_endsession(1, &weak, &mut fx);
        assert_eq!(*calls.borrow(), vec!["snapshot", "block_reason"]);
    }

    #[test]
    fn query_contended_lock_treated_as_dirty() {
        let doc = shared();
        let weak = Arc::downgrade(&doc);
        // hold the lock so the proc-side try_lock fails — the hook must
        // still protect (snapshot attempts are individually best-effort)
        let _guard = crate::lock_shared(&doc);
        let mut fx = RecFx::default();
        let calls = fx.calls.clone();
        on_query_endsession(1, &weak, &mut fx);
        assert_eq!(*calls.borrow(), vec!["snapshot", "block_reason"]);
    }

    #[test]
    fn endsession_true_persists_false_clears_reason() {
        let doc = shared();
        let state = HookState {
            view: std::ptr::null_mut(),
            doc: Arc::downgrade(&doc),
        };
        let mut fx = RecFx::default();
        let calls = fx.calls.clone();
        on_endsession(1, 1, &state, &mut fx);
        assert_eq!(*calls.borrow(), vec!["persist"]);
        calls.borrow_mut().clear();
        on_endsession(1, 0, &state, &mut fx);
        assert_eq!(*calls.borrow(), vec!["clear_reason"]);
    }
}
