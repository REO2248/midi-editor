//! Property tests for `UndoStack`: randomized interleaved apply/undo/redo
//! scripts over generated documents, asserting LIFO order, snapshot equality
//! at every step, redo-stack clearing, and cap eviction behavior.

use commands::UndoStack;
use document::{Document, Event, Op, Transaction};
use proptest::prelude::*;
use smf_core::{Division, Event as SmfEvent, EventKind, File, Track as SmfTrack, WriteOptions};

fn arb_file() -> impl Strategy<Value = File> {
    (prop::collection::vec(
        prop::collection::vec(
            (0u64..=4_000, 0x80u8..0xF0, any::<u8>(), any::<u8>()),
            0..16,
        ),
        1..=3,
    ))
    .prop_map(|tracks| File {
        format: 1,
        division: Division::Metrical(480),
        tracks: tracks
            .into_iter()
            .map(|mut evs| {
                evs.sort_by_key(|(tick, ..)| *tick);
                SmfTrack {
                    events: evs
                        .into_iter()
                        .enumerate()
                        .map(|(i, (tick, st, d0, d1))| SmfEvent {
                            tick,
                            seq: i as u32,
                            raw_body: None,
                            kind: EventKind::Channel {
                                status: st,
                                data: [d0 & 0x7F, d1 & 0x7F],
                                len: if (0xC0..0xE0).contains(&st) { 1 } else { 2 },
                            },
                        })
                        .collect(),
                }
            })
            .collect(),
        warnings: vec![],
    })
}

/// A mixed script of edits and stack operations.
#[derive(Debug, Clone)]
enum Action {
    /// insert a note-on event into an existing track
    Insert {
        track_idx: usize,
        tick: u64,
        key: u8,
    },
    /// delete a tick range on an existing track
    DeleteRange {
        track_idx: usize,
        from: u64,
        to: u64,
    },
    /// remove a whole track
    RemoveTrack {
        index: usize,
    },
    /// add an empty track
    AddTrack,
    Undo,
    Redo,
}

fn arb_action() -> impl Strategy<Value = Action> {
    prop_oneof![
        4 => (prop::num::usize::ANY, 0u64..=4_000, any::<u8>())
            .prop_map(|(t, tick, k)| Action::Insert { track_idx: t, tick, key: k & 0x7F }),
        3 => (prop::num::usize::ANY, 0u64..=4_000, 0u64..=4_000)
            .prop_map(|(t, a, b)| Action::DeleteRange { track_idx: t, from: a.min(b), to: a.max(b) }),
        1 => prop::num::usize::ANY.prop_map(|index| Action::RemoveTrack { index }),
        1 => Just(Action::AddTrack),
        4 => Just(Action::Undo),
        3 => Just(Action::Redo),
    ]
}

fn ser(doc: &Document) -> Vec<u8> {
    doc.serialize(WriteOptions::default())
}

/// Returns false when the ops were empty — the app never pushes a no-op edit,
/// so nothing is pushed and no snapshot is recorded.
fn commit(doc: &mut Document, stack: &mut UndoStack, ops: Vec<Op>, label: &str) -> bool {
    if ops.is_empty() {
        return false;
    }
    let tx = Transaction {
        label: label.into(),
        base: doc.revision(),
        ops,
    };
    // push the *effective* transaction — undo of synthesized normalization
    // ops is what restores the pre-edit bytes exactly
    let applied = doc.apply(tx).expect("ops built on current state");
    stack.push(applied.tx);
    true
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn undo_redo_lifo_snapshots(file in arb_file(), script in prop::collection::vec(arb_action(), 1..=48)) {
        const CAP: usize = 16;
        let mut doc = Document::from_file(file);
        let mut stack = UndoStack::new(CAP);
        // snaps[i] is the serialized state after i committed, non-evicted edits;
        // pos tracks where the document currently sits in that window.
        let mut snaps: std::collections::VecDeque<Vec<u8>> = [ser(&doc)].into();
        let mut pos = 0usize;

        for (step, act) in script.iter().enumerate() {
            let mut committed = false;
            match *act {
                Action::Insert { track_idx, tick, key } => {
                    if doc.tracks.is_empty() { continue; }
                    let t = track_idx % doc.tracks.len();
                    let ev = Event {
                        id: doc.alloc_event_id(),
                        tick,
                        seq: u32::MAX, // lands last among same-tick events
                        raw_body: None,
                        kind: EventKind::Channel {
                            status: 0x90,
                            data: [key, 100],
                            len: 2,
                        },
                    };
                    committed = commit(&mut doc, &mut stack, vec![Op::InsertEvents {
                        track: t,
                        events: vec![ev],
                    }], "insert");
                }
                Action::DeleteRange { track_idx, from, to } => {
                    if doc.tracks.is_empty() { continue; }
                    let t = track_idx % doc.tracks.len();
                    let ops = doc.delete_range_ops(t, from, to);
                    committed = commit(&mut doc, &mut stack, ops, "delete");
                }
                Action::RemoveTrack { index } => {
                    if doc.tracks.is_empty() { continue; }
                    let ops = doc.remove_track_ops(index % doc.tracks.len());
                    committed = commit(&mut doc, &mut stack, ops, "remove_track");
                }
                Action::AddTrack => {
                    let ops = doc.add_track_ops(Some("gen"), None);
                    committed = commit(&mut doc, &mut stack, ops, "add_track");
                }
                Action::Undo => {
                    match stack.undo(&mut doc) {
                        Some(_) => {
                            prop_assert!(pos > 0, "step {}: undo beyond retained history", step);
                            pos -= 1;
                        }
                        None => prop_assert_eq!(pos, 0, "step {}: undo refused with live entries", step),
                    }
                }
                Action::Redo => {
                    match stack.redo(&mut doc) {
                        Some(_) => pos += 1,
                        None => prop_assert_eq!(pos, snaps.len() - 1, "step {}: redo refused with live entries", step),
                    }
                }
            }
            // after a commit the redo tail is gone and cap evicts the oldest
            if committed {
                snaps.truncate(pos + 1);
                snaps.push_back(ser(&doc));
                pos += 1;
                if snaps.len() > CAP + 1 {
                    snaps.pop_front();
                    pos -= 1;
                }
            }
            prop_assert_eq!(ser(&doc), snaps[pos].to_vec(), "step {}: document diverged from snapshot window", step);
            // accessor invariants: len()/is_empty() mirror the undoable depth
            prop_assert_eq!(stack.len(), pos, "step {}: len() desynced", step);
            prop_assert_eq!(stack.is_empty(), pos == 0, "step {}: is_empty() desynced", step);
        }

        // unwind everything the stack still holds
        while stack.undo(&mut doc).is_some() {
            pos -= 1;
        }
        prop_assert_eq!(pos, 0);
        prop_assert_eq!(ser(&doc), snaps[0].to_vec());
        while stack.redo(&mut doc).is_some() {
            pos += 1;
        }
        prop_assert_eq!(pos, snaps.len() - 1);
        prop_assert_eq!(ser(&doc), snaps[pos].to_vec());
    }

    /// push() must clear the redo stack — a new edit invalidates it.
    #[test]
    fn new_edit_clears_redo_stack(file in arb_file(), n_undo in 1usize..=4) {
        let mut doc = Document::from_file(file);
        let mut stack = UndoStack::new(64);
        for i in 0..n_undo + 1 {
            let ops = doc.add_track_ops(Some(&format!("t{i}")), None);
            prop_assert!(commit(&mut doc, &mut stack, ops, "add"));
        }
        for _ in 0..n_undo {
            stack.undo(&mut doc).expect("all entries undoable");
        }
        let ops = doc.add_track_ops(Some("fork"), None);
        prop_assert!(commit(&mut doc, &mut stack, ops, "fork"));
        prop_assert!(stack.redo(&mut doc).is_none(), "stale redo must be cleared");
    }
}
