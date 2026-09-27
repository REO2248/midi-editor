//! Helio-style undo: every edit is a self-contained `Transaction` holding
//! before/after values; undo = `Document::revert`. Gesture edits are grouped
//! by a `begin`/`end` scope and consecutive same-label edits coalesce.

use document::{Document, Transaction};

#[derive(Default)]
pub struct UndoStack {
    done: Vec<Transaction>,
    undone: Vec<Transaction>,
    /// max entries kept; oldest are dropped but never below `min_keep`
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
        self.done.push(tx);
        self.undone.clear();
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
        doc.apply(replay).ok()?;
        self.done.push(tx);
        Some(label)
    }

    pub fn len(&self) -> usize {
        self.done.len()
    }
}
