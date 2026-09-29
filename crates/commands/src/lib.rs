//! Helio-style undo: every edit is a self-contained `Transaction` holding
//! before/after values; undo = `Document::revert`, redo = replay at the
//! current revision. One editor action = one `Transaction` = one undo step.

use document::{Document, Transaction};

#[derive(Default)]
pub struct UndoStack {
    done: Vec<Transaction>,
    undone: Vec<Transaction>,
    /// max undo entries kept; oldest are dropped first
    cap: usize,
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
        self.done.push(tx);
        if self.done.len() > self.cap {
            self.done.remove(0);
        }
    }

    pub fn undo(&mut self, doc: &mut Document) -> Option<String> {
        let tx = self.done.pop()?;
        doc.revert(&tx);
        let label = tx.label.clone();
        self.undone.push(tx);
        Some(label)
    }

    pub fn redo(&mut self, doc: &mut Document) -> Option<String> {
        let tx = self.undone.pop()?;
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
                Some(label)
            }
            // keep the entry redoable — dropping it would silently skip a
            // step on the next redo (the document itself is untouched:
            // `Document::apply` is atomic)
            Err(_) => {
                self.undone.push(tx);
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
        doc.apply(tx.clone()).unwrap();
        stack.push(tx);
    }

    #[test]
    fn undo_redo_roundtrip() {
        let mut doc = note_doc();
        let mut stack = UndoStack::new(512);
        let tx = insert_tx(&mut doc, 0, 64);
        apply(&mut doc, &mut stack, tx);
        assert_eq!(doc.tracks[0].events.len(), 2);

        assert_eq!(stack.undo(&mut doc).as_deref(), Some("insert 64"));
        assert_eq!(doc.tracks[0].events.len(), 1);

        assert_eq!(stack.redo(&mut doc).as_deref(), Some("insert 64"));
        assert_eq!(doc.tracks[0].events.len(), 2);
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
    fn failed_redo_keeps_the_entry() {
        // tx1 inserts into track 1. After undoing it, an out-of-band edit
        // (MCP/another tx path) removes track 1, so the replay fails — the
        // entry must stay redoable instead of being silently dropped.
        let mut doc = Document::from_file(smf_core::File {
            format: 1,
            division: smf_core::Division::Metrical(480),
            tracks: vec![smf_core::Track { events: vec![] }, smf_core::Track { events: vec![] }],
            warnings: vec![],
        });
        let mut stack = UndoStack::new(512);
        let tx = insert_tx(&mut doc, 1, 64);
        apply(&mut doc, &mut stack, tx);
        assert_eq!(doc.tracks[1].events.len(), 1);
        stack.undo(&mut doc);
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
        assert_eq!(doc.tracks[1].events.len(), 1);
    }
}
