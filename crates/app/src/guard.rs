//! Centralized "Save / Don't Save / Cancel" guard. Every path that would
//! throw away unsaved work — document replacement (New, Open dialog,
//! drag/drop, Open Recent) or leaving the app (window close) — funnels
//! through `confirm_discard_or_save` so the decision, the prompt wording,
//! and the save-then-proceed ordering live in exactly one place.
//!
//! The pure decision tables (`needs_guard`, `choice_for_index`,
//! `verdict_for`) are split from the async gpui flow so the policy is
//! unit-testable without a window.

use crate::{lock_shared, t, EditorView, PendingAction};
use gpui_kit::*;

/// The three answers the discard prompt offers, in button order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum GuardChoice {
    /// save the document, then run the pending action
    Save,
    /// throw the work away and run the pending action
    DontSave,
    /// do nothing — document, playback, selection, and undo stay as they are
    Cancel,
}

/// Whether the gated action may run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum GuardVerdict {
    Proceed,
    Abort,
}

/// Result of the guard's save step.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SaveOutcome {
    /// document written; it is clean now
    Saved,
    /// the write failed — document stays open and dirty
    Failed,
    /// no backing path — the user must pick one (or bail out)
    NeedsPath,
}

/// Does anything stand to be lost? A dirty document does, and so does an
/// armed recording (its captured take has never been committed).
pub(crate) fn needs_guard(dirty: bool, rec_armed: bool) -> bool {
    dirty || rec_armed
}

/// Map a prompt button index to a choice. Buttons are always registered in
/// the order Save / Don't Save / Cancel; any out-of-range index — including
/// a dismissed dialog — is treated as Cancel so a confused prompt can never
/// destroy work.
pub(crate) fn choice_for_index(i: usize) -> GuardChoice {
    match i {
        0 => GuardChoice::Save,
        1 => GuardChoice::DontSave,
        _ => GuardChoice::Cancel,
    }
}

/// Final decision once the user answered (and the save finished, when they
/// asked for one). Save failure is an abort: the document must stay open
/// and dirty, so the pending action is not allowed to run.
pub(crate) fn verdict_for(choice: GuardChoice, save_ok: bool) -> GuardVerdict {
    match choice {
        GuardChoice::Cancel => GuardVerdict::Abort,
        GuardChoice::DontSave => GuardVerdict::Proceed,
        GuardChoice::Save if save_ok => GuardVerdict::Proceed,
        GuardChoice::Save => GuardVerdict::Abort,
    }
}

impl EditorView {
    /// Dirty for save-prompt purposes — the undo-stack save point decides
    /// (#177): undoing back to the saved content is clean again.
    pub(crate) fn is_dirty(&self) -> bool {
        lock_shared(&self.shared).is_dirty()
    }

    /// True when a document-replacing or exit action would lose work.
    pub(crate) fn needs_discard_guard(&self) -> bool {
        needs_guard(self.is_dirty(), self.rec.is_some())
    }

    /// The single gate every destructive path goes through. `window` is the
    /// editor's own window (all call sites have it; the close handler
    /// supplies it too). When nothing is at risk the action runs inline;
    /// otherwise a prompt is shown and the action runs only after an
    /// explicit proceed — Save must actually succeed, Don't Save discards,
    /// Cancel/abort leaves everything untouched.
    pub(crate) fn confirm_discard_or_save(
        &mut self,
        action: PendingAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.needs_discard_guard() {
            self.perform_pending(action, cx);
            return;
        }
        // a prompt is already up — extra triggers must not stack dialogs
        // (`window.prompt` panics on re-entrant use)
        if self.guard_active {
            return;
        }
        self.guard_active = true;
        let (msg, detail) = self.guard_strings();
        let rx = window.prompt(
            PromptLevel::Warning,
            &msg,
            Some(&detail),
            &[
                PromptButton::Ok(t("guard.save").into()),
                PromptButton::Other(t("guard.dont_save").into()),
                PromptButton::Cancel(t("guard.cancel").into()),
            ],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let idx = rx.await.unwrap_or(usize::MAX);
            let choice = choice_for_index(idx);
            let save_ok = match choice {
                GuardChoice::Save => Self::save_for_guard_async(this.clone(), cx).await,
                _ => false,
            };
            if verdict_for(choice, save_ok) == GuardVerdict::Proceed {
                Self::run_pending(this.clone(), &action, cx).await;
            }
            this.update(cx, |v, _cx| v.guard_active = false).ok();
        })
        .detach();
    }

    /// Prompt text for the current at-risk state. Recording is folded into
    /// the same prompt so one answer decides both fates.
    fn guard_strings(&self) -> (String, String) {
        let dirty = self.is_dirty();
        let armed = self.rec.is_some();
        let msg = match (dirty, armed) {
            (true, true) => t("guard.unsaved_rec"),
            (true, false) => t("guard.unsaved"),
            (false, true) => t("guard.rec_only"),
            (false, false) => t("guard.unsaved"),
        };
        (msg.to_string(), t("guard.detail").to_string())
    }

    /// The save step inside the flow, awaited from the prompt task.
    /// Returns true only when the document is clean afterwards: a write
    /// failure or a cancelled Save-As keeps it open and dirty.
    async fn save_for_guard_async(this: WeakEntity<Self>, cx: &mut AsyncWindowContext) -> bool {
        let outcome = this
            .update(cx, |v, cx| v.save_for_guard(cx))
            .unwrap_or(SaveOutcome::Failed);
        match outcome {
            SaveOutcome::Saved => true,
            SaveOutcome::Failed => false,
            SaveOutcome::NeedsPath => {
                let Some(path) = Self::pick_save_path(&this, cx).await else {
                    return false;
                };
                this.update(cx, |v, cx| v.try_save_to(&path, cx))
                    .unwrap_or(false)
            }
        }
    }

    /// The guard's Save-As pick (#158): the native filtered dialog on
    /// Windows, the gpui prompt elsewhere.
    #[cfg(windows)]
    async fn pick_save_path(
        this: &WeakEntity<EditorView>,
        cx: &mut AsyncWindowContext,
    ) -> Option<std::path::PathBuf> {
        let (dir, name) = this
            .update(cx, |v, _cx| v.save_dialog_start())
            .unwrap_or_else(|_| {
                (
                    std::env::current_dir().unwrap_or_default(),
                    "untitled.mid".into(),
                )
            });
        let hwnd = this.update(cx, |v, cx| v.dialog_hwnd(cx)).unwrap_or(0);
        let rx = crate::filedlg::save_path(hwnd, dir, name);
        crate::filedlg::result(rx, cx.background_executor().clone()).await
    }

    #[cfg(not(windows))]
    async fn pick_save_path(
        this: &WeakEntity<EditorView>,
        cx: &mut AsyncWindowContext,
    ) -> Option<std::path::PathBuf> {
        let (dir, name) = this
            .update(cx, |v, _cx| v.save_dialog_start())
            .unwrap_or_else(|_| {
                (
                    std::env::current_dir().unwrap_or_default(),
                    "untitled.mid".into(),
                )
            });
        let Ok(rx) = cx.update(|_w, app| app.prompt_for_new_path(&dir, Some(&name))) else {
            return None;
        };
        rx.await.ok().and_then(|r| r.ok()).flatten()
    }

    /// Run the gated action after the guard passed. `CloseWindow` latches
    /// `close_confirmed` first: removing the window re-enters
    /// `on_window_should_close`, which must now see a clean pass instead of
    /// re-prompting.
    async fn run_pending(
        this: WeakEntity<Self>,
        action: &PendingAction,
        cx: &mut AsyncWindowContext,
    ) {
        if let PendingAction::CloseWindow = action {
            this.update(cx, |v, _cx| v.close_confirmed = true).ok();
            cx.update(|w, _app| w.remove_window()).ok();
            return;
        }
        this.update(cx, |v, cx| v.perform_pending(action.clone(), cx))
            .ok();
    }
}

#[cfg(test)]
mod tests {
    // no `use super::*`: the parent's `gpui_kit::*` glob would shadow the
    // builtin `#[test]` attribute with `gpui::test`
    use super::{choice_for_index, needs_guard, verdict_for, GuardChoice, GuardVerdict};
    use crate::lock_shared;
    use std::path::PathBuf;

    fn shared() -> mcp_server::SharedDoc {
        let f = smf_core::File {
            format: 1,
            division: smf_core::Division::Metrical(480),
            tracks: vec![smf_core::Track { events: vec![] }],
            warnings: vec![],
        };
        mcp_server::SharedDoc::new(std::sync::Mutex::new(mcp_server::Shared::new(
            document::Document::from_file(f),
        )))
    }

    fn dirty(shared: &mcp_server::SharedDoc) {
        lock_shared(shared)
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

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("midi-guard-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn needs_guard_truth_table() {
        assert!(!needs_guard(false, false));
        assert!(needs_guard(true, false));
        assert!(needs_guard(false, true));
        assert!(needs_guard(true, true));
    }

    #[test]
    fn choice_for_index_maps_buttons_and_fallbacks_to_cancel() {
        assert_eq!(choice_for_index(0), GuardChoice::Save);
        assert_eq!(choice_for_index(1), GuardChoice::DontSave);
        assert_eq!(choice_for_index(2), GuardChoice::Cancel);
        // a dismissed dialog or an unexpected index can never destroy work
        assert_eq!(choice_for_index(usize::MAX), GuardChoice::Cancel);
    }

    #[test]
    fn verdict_for_covers_all_cases() {
        assert_eq!(verdict_for(GuardChoice::Cancel, true), GuardVerdict::Abort);
        assert_eq!(verdict_for(GuardChoice::Cancel, false), GuardVerdict::Abort);
        assert_eq!(
            verdict_for(GuardChoice::DontSave, false),
            GuardVerdict::Proceed
        );
        assert_eq!(verdict_for(GuardChoice::Save, true), GuardVerdict::Proceed);
        // a failed save must keep the document open and dirty — the pending
        // action is never allowed to run
        assert_eq!(verdict_for(GuardChoice::Save, false), GuardVerdict::Abort);
    }

    #[test]
    fn save_document_marks_clean_on_success() {
        let sh = shared();
        dirty(&sh);
        let file = tmpdir("ok").join("a.mid");
        assert!(mcp_server::service::save_document(
            &sh,
            mcp_server::service::SaveRequest {
                path: Some(&file),
                ..Default::default()
            },
        )
        .is_ok());
        assert!(file.exists());
        let g = lock_shared(&sh);
        assert_eq!(g.doc.revision(), g.saved_revision);
        // the serialized file parses back — original track + inserted one
        let bytes = std::fs::read(&file).unwrap();
        assert_eq!(smf_core::parse(&bytes).unwrap().tracks.len(), 2);
    }

    #[test]
    fn save_document_failure_stays_dirty() {
        let sh = shared();
        dirty(&sh);
        // a path inside a directory that does not exist makes write_atomic fail
        let file = tmpdir("fail").join("no-such-dir").join("a.mid");
        let rev = lock_shared(&sh).doc.revision();
        assert!(mcp_server::service::save_document(
            &sh,
            mcp_server::service::SaveRequest {
                path: Some(&file),
                ..Default::default()
            },
        )
        .is_err());
        let g = lock_shared(&sh);
        assert_eq!(g.doc.revision(), rev);
        assert_ne!(g.doc.revision(), g.saved_revision);
    }
}
