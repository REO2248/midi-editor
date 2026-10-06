//! Helio-style undo: every edit is a self-contained `Transaction` holding
//! before/after values; undo = `Document::revert`, redo = replay at the
//! current revision. One editor action = one `Transaction` = one undo step.

use document::{Document, Transaction};

#[derive(Default)]
pub struct UndoStack {
    done: Vec<Transaction>,
    undone: Vec<Transaction>,
    /// ids parallel to `done` — assigned on push, stable through undo/redo,
    /// so the done list's identity (not just its length) can be compared
    done_ids: Vec<u64>,
    /// ids parallel to `undone`
    undone_ids: Vec<u64>,
    next_id: u64,
    /// max undo entries kept; oldest are dropped first
    cap: usize,
    /// transactions dropped from `done`'s front by the cap — permanently
    /// applied, no longer undoable
    evicted: usize,
    /// The stack shape at the last verified save: (evicted, done.len(),
    /// top id). Content is a pure function of `base` plus the done list —
    /// `undone` holds only reverted transactions — so matching this triple
    /// reproduces the saved bytes exactly, even though the revision kept
    /// climbing (#177). The id is what distinguishes "undo + fresh edit of
    /// the same length" from the real saved shape. `None` = no known save
    /// point (snapshot restore): the caller falls back to comparing
    /// revisions. Cap eviction after a save raises `evicted` past the
    /// recorded value, which (conservatively) reports dirty — the saved
    /// shape is no longer reachable.
    saved: Option<(usize, usize, u64)>,
}

impl UndoStack {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            ..Default::default()
        }
    }

    pub fn push(&mut self, tx: Transaction) {
        self.undone.clear();
        self.undone_ids.clear();
        let id = self.next_id;
        self.next_id += 1;
        self.done.push(tx);
        self.done_ids.push(id);
        if self.done.len() > self.cap {
            self.done.remove(0);
            self.done_ids.remove(0);
            self.evicted += 1;
        }
    }

    /// Record the current stack shape as the saved state — called after a
    /// verified save and when a fresh document is installed (#177).
    pub fn mark_saved(&mut self) {
        self.saved = Some(self.shape());
    }

    /// Drop the save marker (a save committed at a different revision than
    /// the live document — the caller's revision comparison decides).
    pub fn clear_saved(&mut self) {
        self.saved = None;
    }

    pub fn has_saved_point(&self) -> bool {
        self.saved.is_some()
    }

    /// True when the document content equals the last saved state: the
    /// stack shape matches the recorded save point.
    pub fn is_at_saved_point(&self) -> bool {
        self.saved == Some(self.shape())
    }

    fn shape(&self) -> (usize, usize, u64) {
        (
            self.evicted,
            self.done.len(),
            self.done_ids.last().copied().unwrap_or(0),
        )
    }

    /// The transaction `undo()` would revert — lets callers summarize it
    /// (e.g. a history/changes feed) before popping.
    pub fn peek_done(&self) -> Option<&Transaction> {
        self.done.last()
    }

    /// The transaction `redo()` would replay.
    pub fn peek_undone(&self) -> Option<&Transaction> {
        self.undone.last()
    }

    pub fn undo(&mut self, doc: &mut Document) -> Option<String> {
        let tx = self.done.pop()?;
        let id = self.done_ids.pop().unwrap_or_default();
        doc.revert(&tx);
        let label = tx.label.clone();
        self.undone.push(tx);
        self.undone_ids.push(id);
        Some(label)
    }

    pub fn redo(&mut self, doc: &mut Document) -> Option<String> {
        let tx = self.undone.pop()?;
        let id = self.undone_ids.pop().unwrap_or_default();
        let label = tx.label.clone();
        // replay ops at current revision
        let replay = Transaction {
            label: tx.label.clone(),
            base: doc.revision(),
            ops: tx.ops.clone(),
        };
        match doc.apply(replay) {
            Ok(_) => {
                self.done.push(tx);
                self.done_ids.push(id);
                Some(label)
            }
            // keep the entry redoable — dropping it would silently skip a
            // step on the next redo (the document itself is untouched:
            // `Document::apply` is atomic)
            Err(_) => {
                self.undone.push(tx);
                self.undone_ids.push(id);
                None
            }
        }
    }

    pub fn len(&self) -> usize {
        self.done.len()
    }

    pub fn is_empty(&self) -> bool {
        self.done.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use document::Op;
    use smf_core::EventKind;

    fn note_doc() -> Document {
        Document::from_file(smf_core::File {
            format: 1,
            division: smf_core::Division::Metrical(480),
            tracks: vec![smf_core::Track {
                events: vec![smf_core::Event {
                    tick: 0,
                    seq: 0,
                    raw_body: None,
                    kind: EventKind::Channel {
                        status: 0x90,
                        data: [60, 100],
                        len: 2,
                    },
                }],
            }],
            warnings: vec![],
        })
    }

    fn insert_tx(doc: &mut Document, track: usize, key: u8) -> Transaction {
        let ev = document::Event {
            id: doc.alloc_event_id(),
            tick: 0,
            seq: 0,
            raw_body: None,
            kind: EventKind::Channel {
                status: 0x90,
                data: [key, 100],
                len: 2,
            },
        };
        Transaction {
            label: format!("insert {key}"),
            base: doc.revision(),
            ops: vec![Op::InsertEvents {
                track,
                events: vec![ev],
            }],
        }
    }

    fn apply(doc: &mut Document, stack: &mut UndoStack, tx: Transaction) {
        // undo needs the *effective* transaction — including any
        // normalization ops the document synthesized — to restore the
        // exact pre-edit state
        let applied = doc.apply(tx).unwrap();
        stack.push(applied.tx);
    }

    #[test]
    fn undo_redo_roundtrip() {
        let mut doc = note_doc();
        let mut stack = UndoStack::new(512);
        let tx = insert_tx(&mut doc, 0, 64);
        apply(&mut doc, &mut stack, tx);
        // +1 for the structural End-of-Track every track gains on first edit
        assert_eq!(doc.tracks[0].events.len(), 3);

        assert_eq!(stack.undo(&mut doc).as_deref(), Some("insert 64"));
        // undo removes the synthesized EOT too — the pre-edit state is
        // restored byte-for-byte
        assert_eq!(doc.tracks[0].events.len(), 1);

        assert_eq!(stack.redo(&mut doc).as_deref(), Some("insert 64"));
        assert_eq!(doc.tracks[0].events.len(), 3);
    }

    #[test]
    fn new_edit_clears_redo() {
        let mut doc = note_doc();
        let mut stack = UndoStack::new(512);
        let tx = insert_tx(&mut doc, 0, 64);
        apply(&mut doc, &mut stack, tx);
        stack.undo(&mut doc);
        let tx = insert_tx(&mut doc, 0, 67);
        apply(&mut doc, &mut stack, tx);
        assert!(stack.redo(&mut doc).is_none(), "redo cleared by new edit");
    }

    #[test]
    fn cap_evicts_oldest() {
        let mut doc = note_doc();
        let mut stack = UndoStack::new(2);
        let tx = insert_tx(&mut doc, 0, 60);
        apply(&mut doc, &mut stack, tx);
        let tx = insert_tx(&mut doc, 0, 61);
        apply(&mut doc, &mut stack, tx);
        let tx = insert_tx(&mut doc, 0, 62);
        apply(&mut doc, &mut stack, tx);
        assert_eq!(stack.len(), 2);
        assert_eq!(stack.undo(&mut doc).as_deref(), Some("insert 62"));
        assert_eq!(stack.undo(&mut doc).as_deref(), Some("insert 61"));
        assert_eq!(stack.undo(&mut doc), None, "oldest was evicted");
    }

    #[test]
    fn undo_back_to_saved_point_is_clean() {
        // #177: save after one edit, make a second edit, undo it — the
        // content is byte-identical to the saved state, so the save marker
        // must say clean even though the revision kept climbing
        let mut doc = note_doc();
        let mut stack = UndoStack::new(512);
        stack.mark_saved(); // fresh document is clean
        let tx = insert_tx(&mut doc, 0, 64);
        apply(&mut doc, &mut stack, tx);
        assert!(!stack.is_at_saved_point(), "edit 1 is dirty");
        stack.mark_saved(); // verified save after edit 1
        let tx = insert_tx(&mut doc, 0, 67);
        apply(&mut doc, &mut stack, tx);
        assert!(!stack.is_at_saved_point(), "edit 2 is dirty");
        stack.undo(&mut doc);
        assert!(stack.is_at_saved_point(), "undo back to save is clean");
        // redoing the edit marks dirty again
        stack.redo(&mut doc);
        assert!(!stack.is_at_saved_point(), "redo is dirty");
        // undo again lands on the save point once more
        stack.undo(&mut doc);
        assert!(stack.is_at_saved_point());
    }

    #[test]
    fn new_edit_after_undo_is_never_mistaken_for_saved() {
        // the trap #177's fix must avoid: undo + a fresh edit leaves the
        // stack at the same length as the save point but with a different
        // transaction — the id comparison must catch it
        let mut doc = note_doc();
        let mut stack = UndoStack::new(512);
        let tx = insert_tx(&mut doc, 0, 64);
        apply(&mut doc, &mut stack, tx);
        stack.mark_saved(); // saved content = base + edit 64
        stack.undo(&mut doc);
        assert!(!stack.is_at_saved_point(), "undoing past the save is dirty");
        let tx = insert_tx(&mut doc, 0, 67);
        apply(&mut doc, &mut stack, tx);
        assert!(
            !stack.is_at_saved_point(),
            "a fresh edit at the same depth is still dirty"
        );
        assert!(stack.redo(&mut doc).is_none(), "redo cleared by new edit");
    }

    #[test]
    fn eviction_after_save_reports_dirty_conservatively() {
        // cap=2: save with one entry, then overflow the stack — the saved
        // stack shape is no longer reachable, so the marker must stop
        // claiming clean (falling back to the caller's revision check)
        let mut doc = note_doc();
        let mut stack = UndoStack::new(2);
        let tx = insert_tx(&mut doc, 0, 60);
        apply(&mut doc, &mut stack, tx);
        stack.mark_saved();
        assert!(stack.is_at_saved_point(), "marked at the save point");
        let tx = insert_tx(&mut doc, 0, 61);
        apply(&mut doc, &mut stack, tx);
        assert!(!stack.is_at_saved_point(), "edit after save is dirty");
        let tx = insert_tx(&mut doc, 0, 62);
        apply(&mut doc, &mut stack, tx);
        assert!(!stack.is_at_saved_point(), "evicted save shape stays dirty");
        // undoing everything still cannot reach the evicted save shape
        stack.undo(&mut doc);
        stack.undo(&mut doc);
        assert!(!stack.is_at_saved_point());
    }

    #[test]
    fn failed_redo_keeps_the_entry() {
        // tx1 inserts into track 1. After undoing it, an out-of-band edit
        // (MCP/another tx path) removes track 1, so the replay fails — the
        // entry must stay redoable instead of being silently dropped.
        let mut doc = Document::from_file(smf_core::File {
            format: 1,
            division: smf_core::Division::Metrical(480),
            tracks: vec![
                smf_core::Track { events: vec![] },
                smf_core::Track { events: vec![] },
            ],
            warnings: vec![],
        });
        let mut stack = UndoStack::new(512);
        let tx = insert_tx(&mut doc, 1, 64);
        apply(&mut doc, &mut stack, tx);
        // +1 for the structural End-of-Track every track gains on first edit
        assert_eq!(doc.tracks[1].events.len(), 2);
        stack.undo(&mut doc);
        // undo removes the synthesized EOT too — empty again
        assert_eq!(doc.tracks[1].events.len(), 0);

        // out-of-band removal (not pushed onto this stack)
        doc.apply(Transaction {
            label: "out-of-band".into(),
            base: doc.revision(),
            ops: vec![Op::RemoveTrack {
                index: 1,
                track: document::Track {
                    name: None,
                    out_port: 0,
                    out_channel: 0,
                    events: vec![],
                },
            }],
        })
        .unwrap();
        assert_eq!(doc.tracks.len(), 1);

        assert_eq!(stack.redo(&mut doc), None, "replay hits unknown track");
        assert_eq!(doc.tracks.len(), 1, "document untouched by failed redo");

        // once the track is back, the kept entry redoes normally
        doc.apply(Transaction {
            label: "re-add".into(),
            base: doc.revision(),
            ops: vec![Op::InsertTrack {
                index: 1,
                track: document::Track {
                    name: None,
                    out_port: 0,
                    out_channel: 0,
                    events: vec![],
                },
            }],
        })
        .unwrap();
        assert_eq!(stack.redo(&mut doc).as_deref(), Some("insert 64"));
        assert_eq!(doc.tracks[1].events.len(), 2);
    }
}
