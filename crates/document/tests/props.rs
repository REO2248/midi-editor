//! Property tests for the transaction layer: random documents plus
//! randomized transaction sequences, asserting apply→revert restores both
//! serialized and semantic state, failed applies are atomic, and stale
//! revisions are rejected. Extreme tick values are exercised throughout.

use bytes::Bytes;
use document::{Document, Event as DocEvent, Note, Op, Transaction};
use proptest::prelude::*;
use smf_core::{Division, Event as SmfEvent, EventKind, File, Track as SmfTrack, WriteOptions};

fn arb_tick() -> impl Strategy<Value = u64> {
    prop_oneof![
        // ordinary musical ticks
        5 => 0u64..=960,
        3 => 961u64..=1_000_000,
        // hostile VLQ-scale values
        2 => (u64::MAX - 1_000_000)..=u64::MAX,
        1 => prop::num::u64::ANY,
    ]
}

fn arb_kind() -> impl Strategy<Value = EventKind> {
    prop_oneof![
        5 => (0x80u8..0xF0, any::<u8>(), any::<u8>()).prop_map(|(st, d0, d1)| {
            EventKind::Channel {
                status: st,
                data: [d0 & 0x7F, d1 & 0x7F],
                len: if (0xC0..0xE0).contains(&st) { 1 } else { 2 },
            }
        }),
        3 => (any::<u8>(), prop::collection::vec(any::<u8>(), 0..16))
            .prop_map(|(ty, d)| EventKind::Meta {
                meta_type: ty,
                data: Bytes::from(d),
            }),
        1 => prop::collection::vec(any::<u8>(), 0..16)
            .prop_map(|d| EventKind::SysEx(Bytes::from(d))),
        1 => prop::collection::vec(any::<u8>(), 0..16)
            .prop_map(|d| EventKind::Escape(Bytes::from(d))),
    ]
}

pub fn arb_file() -> impl Strategy<Value = File> {
    (
        0u16..=2,
        prop_oneof![
            3 => (1u16..=960).prop_map(Division::Metrical),
            1 => (prop::sample::select(vec![24u8, 25, 29, 30]), any::<u8>())
                .prop_map(|(fps, tpf)| Division::Smpte { fps, ticks_per_frame: tpf }),
        ],
        prop::collection::vec(
            prop::collection::vec((arb_tick(), arb_kind()), 0..24),
            1..=4,
        ),
    )
        .prop_map(|(format, division, tracks)| File {
            format,
            division,
            tracks: tracks
                .into_iter()
                .map(|mut evs| {
                    evs.sort_by_key(|(tick, _)| *tick);
                    SmfTrack {
                        events: evs
                            .into_iter()
                            .enumerate()
                            .map(|(i, (tick, kind))| SmfEvent {
                                tick,
                                seq: i as u32,
                                raw_body: None,
                                kind,
                            })
                            .collect(),
                    }
                })
                .collect(),
            warnings: vec![],
        })
}

/// One editor action applied through the real ops-builders.
#[derive(Debug, Clone)]
pub enum OpSpec {
    Quantize { track: usize, from: u64, to: u64, grid: u64, strength: u32 },
    Transpose { track: usize, from: u64, to: u64, semitones: i32 },
    ScaleVelocity { track: usize, from: u64, to: u64, factor: f64 },
    Humanize { track: usize, from: u64, to: u64, timing: i64, vel: i32 },
    Legato { track: usize, from: u64, to: u64 },
    SetLength { track: usize, from: u64, to: u64, ticks: u64 },
    SetVelocity { track: usize, from: u64, to: u64, vel: u8 },
    SetChannel { track: usize, from: u64, to: u64, channel: u8 },
    SetProgram { track: usize, tick: u64, channel: u8, program: u8, msb: Option<u8>, lsb: Option<u8> },
    SetCc { track: usize, tick: u64, channel: u8, cc: u8, value: u8 },
    SetPitchBend { track: usize, tick: u64, channel: u8, value: u16 },
    SetTempo { tick: u64, bpm: f64 },
    SetTimeSig { tick: u64, num: u8, den: u8 },
    SetTrackChannel { track: usize, channel: u8 },
    DuplicateRange { track: usize, from: u64, to: u64 },
    DeleteRange { track: usize, from: u64, to: u64 },
    AddTrack { name: Option<String> },
    RemoveTrack { index: usize },
    SetTrackName { track: usize, name: String },
}

pub fn arb_opspec() -> impl Strategy<Value = OpSpec> {
    let range = || (prop::num::usize::ANY, arb_tick(), arb_tick());
    prop_oneof![
        2 => (range(), arb_tick(), 0u32..=200).prop_map(|((track, a, b), grid, strength)| OpSpec::Quantize {
            track, from: a.min(b), to: a.max(b), grid, strength,
        }),
        2 => (range(), -48i32..=48).prop_map(|((track, a, b), semitones)| OpSpec::Transpose {
            track, from: a.min(b), to: a.max(b), semitones,
        }),
        1 => (range(), 0.0f64..4.0).prop_map(|((track, a, b), factor)| OpSpec::ScaleVelocity {
            track, from: a.min(b), to: a.max(b), factor,
        }),
        1 => (range(), any::<i64>(), any::<i32>()).prop_map(|((track, a, b), timing, vel)| OpSpec::Humanize {
            track, from: a.min(b), to: a.max(b), timing, vel,
        }),
        1 => range().prop_map(|(track, a, b)| OpSpec::Legato { track, from: a.min(b), to: a.max(b) }),
        1 => (range(), arb_tick()).prop_map(|((track, a, b), ticks)| OpSpec::SetLength {
            track, from: a.min(b), to: a.max(b), ticks,
        }),
        1 => (range(), any::<u8>()).prop_map(|((track, a, b), vel)| OpSpec::SetVelocity {
            track, from: a.min(b), to: a.max(b), vel,
        }),
        1 => (range(), any::<u8>()).prop_map(|((track, a, b), channel)| OpSpec::SetChannel {
            track, from: a.min(b), to: a.max(b), channel,
        }),
        1 => (prop::num::usize::ANY, arb_tick(), any::<u8>(), any::<u8>(), any::<Option<u8>>(), any::<Option<u8>>())
            .prop_map(|(track, tick, channel, program, msb, lsb)| OpSpec::SetProgram {
                track, tick, channel, program, msb, lsb,
            }),
        1 => (prop::num::usize::ANY, arb_tick(), any::<u8>(), any::<u8>(), any::<u8>())
            .prop_map(|(track, tick, channel, cc, value)| OpSpec::SetCc { track, tick, channel, cc, value }),
        1 => (prop::num::usize::ANY, arb_tick(), any::<u8>(), any::<u16>())
            .prop_map(|(track, tick, channel, value)| OpSpec::SetPitchBend { track, tick, channel, value }),
        1 => (arb_tick(), 0.0f64..1000.0).prop_map(|(tick, bpm)| OpSpec::SetTempo { tick, bpm }),
        1 => (arb_tick(), any::<u8>(), any::<u8>())
            .prop_map(|(tick, num, den)| OpSpec::SetTimeSig { tick, num, den }),
        1 => (prop::num::usize::ANY, any::<u8>())
            .prop_map(|(track, channel)| OpSpec::SetTrackChannel { track, channel }),
        1 => range().prop_map(|(track, a, b)| OpSpec::DuplicateRange { track, from: a.min(b), to: a.max(b) }),
        1 => range().prop_map(|(track, a, b)| OpSpec::DeleteRange { track, from: a.min(b), to: a.max(b) }),
        1 => prop::option::of(prop::string::string_regex("[a-zA-Z0-9 ]{0,16}").unwrap())
            .prop_map(|name| OpSpec::AddTrack { name }),
        1 => prop::num::usize::ANY.prop_map(|index| OpSpec::RemoveTrack { index }),
        1 => (prop::num::usize::ANY, prop::string::string_regex("[a-zA-Z0-9 ]{0,16}").unwrap())
            .prop_map(|(track, name)| OpSpec::SetTrackName { track, name }),
    ]
}

/// Build real ops against the document's current state — the same path the
/// GUI and MCP take.
pub fn build_ops(doc: &mut Document, spec: &OpSpec) -> Vec<Op> {
    // callers (GUI/MCP) only ever address existing tracks — normalize so
    // generated indexes stay in range for the current document
    let n = doc.tracks.len();
    if n == 0 {
        // no tracks to address — only track-free ops still produce work
        return match *spec {
            OpSpec::SetTempo { tick, bpm } => doc.set_tempo_ops(tick, bpm),
            OpSpec::SetTimeSig { tick, num, den } => doc.set_time_sig_ops(tick, num, den),
            OpSpec::AddTrack { ref name } => doc.add_track_ops(name.as_deref()),
            _ => vec![],
        };
    }
    match *spec {
        OpSpec::Quantize { track, from, to, grid, strength } => {
            doc.quantize_ops(track % n, from, to, grid, strength)
        }
        OpSpec::Transpose { track, from, to, semitones } => {
            doc.transpose_ops(track % n, from, to, semitones)
        }
        OpSpec::ScaleVelocity { track, from, to, factor } => {
            doc.scale_velocity_ops(track % n, from, to, factor)
        }
        OpSpec::Humanize { track, from, to, timing, vel } => {
            doc.humanize_ops(track % n, from, to, timing, vel)
        }
        OpSpec::Legato { track, from, to } => doc.legato_ops(track % n, from, to),
        OpSpec::SetLength { track, from, to, ticks } => doc.set_length_ops(track % n, from, to, ticks),
        OpSpec::SetVelocity { track, from, to, vel } => doc.set_velocity_ops(track % n, from, to, vel),
        OpSpec::SetChannel { track, from, to, channel } => {
            doc.set_channel_ops(track % n, from, to, channel)
        }
        OpSpec::SetProgram { track, tick, channel, program, msb, lsb } => {
            doc.set_program_ops(track % n, tick, channel, program, msb, lsb)
        }
        OpSpec::SetCc { track, tick, channel, cc, value } => {
            doc.set_cc_ops(track % n, tick, channel, cc, value)
        }
        OpSpec::SetPitchBend { track, tick, channel, value } => {
            doc.set_pitch_bend_ops(track % n, tick, channel, value)
        }
        OpSpec::SetTempo { tick, bpm } => doc.set_tempo_ops(tick, bpm),
        OpSpec::SetTimeSig { tick, num, den } => doc.set_time_sig_ops(tick, num, den),
        OpSpec::SetTrackChannel { track, channel } => doc.set_track_channel_ops(track % n, channel),
        OpSpec::DuplicateRange { track, from, to } => doc.duplicate_range_ops(track % n, from, to),
        OpSpec::DeleteRange { track, from, to } => doc.delete_range_ops(track % n, from, to),
        OpSpec::AddTrack { ref name } => doc.add_track_ops(name.as_deref()),
        OpSpec::RemoveTrack { index } => doc.remove_track_ops(index % n),
        OpSpec::SetTrackName { track, ref name } => doc.set_track_name_ops(track % n, name),
    }
}

/// Semantic snapshot: serialized bytes plus the derived note list (ids
/// excluded — they can legitimately differ after redo cycles).
fn snapshot(doc: &Document) -> (Vec<u8>, Vec<(usize, u8, u8, u8, u64, Option<u64>)>) {
    let notes: Vec<_> = doc
        .notes()
        .into_iter()
        .map(|n: Note| (n.track, n.channel, n.key, n.vel, n.start_tick, n.end_tick))
        .collect();
    (doc.serialize(WriteOptions::default()), notes)
}

fn mint_event(doc: &mut Document) -> DocEvent {
    DocEvent {
        id: doc.alloc_event_id(),
        tick: 0,
        seq: 0,
        raw_body: None,
        kind: EventKind::Meta {
            meta_type: 0x01,
            data: Bytes::from_static(b"x"),
        },
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    /// apply→revert restores both the serialized bytes and the semantic note
    /// state, across arbitrary sequences of real editor ops.
    #[test]
    fn apply_revert_restores_state(file in arb_file(), specs in prop::collection::vec(arb_opspec(), 1..=6)) {
        let mut doc = Document::from_file(file);
        for (i, spec) in specs.iter().enumerate() {
            let (pre_bytes, pre_notes) = snapshot(&doc);
            let pre_rev = doc.revision();
            let ops = build_ops(&mut doc, spec);
            let tx = Transaction {
                label: format!("op{i}"),
                base: doc.revision(),
                ops,
            };
            doc.apply(tx.clone()).expect("builder ops must apply cleanly");
            prop_assert_eq!(doc.revision(), pre_rev + 1);
            // serialized output must always re-parse — writers never emit
            // something our own reader rejects
            prop_assert!(smf_core::parse(&doc.serialize(WriteOptions::default())).is_ok());
            doc.revert(&tx);
            let (post_bytes, post_notes) = snapshot(&doc);
            prop_assert_eq!(post_bytes, pre_bytes, "revert must restore serialized bytes");
            prop_assert_eq!(post_notes, pre_notes, "revert must restore derived notes");
        }
    }

    /// A whole sequence applied then reverted in reverse order returns the
    /// document to its original state.
    #[test]
    fn sequence_reverts_to_origin(file in arb_file(), specs in prop::collection::vec(arb_opspec(), 1..=10)) {
        let mut doc = Document::from_file(file);
        let (origin_bytes, origin_notes) = snapshot(&doc);
        let mut applied = Vec::new();
        for (i, spec) in specs.iter().enumerate() {
            let ops = build_ops(&mut doc, spec);
            let tx = Transaction {
                label: format!("op{i}"),
                base: doc.revision(),
                ops,
            };
            doc.apply(tx.clone()).unwrap();
            applied.push(tx);
        }
        for tx in applied.iter().rev() {
            doc.revert(tx);
        }
        let (bytes, notes) = snapshot(&doc);
        prop_assert_eq!(bytes, origin_bytes);
        prop_assert_eq!(notes, origin_notes);
    }

    /// A transaction that fails mid-way leaves the document byte-identical —
    /// revision, tracks, and the id index untouched.
    #[test]
    fn failed_apply_is_atomic(
        file in arb_file(),
        specs in prop::collection::vec(arb_opspec(), 0..=3),
        bad_track_delta in 1usize..=8,
    ) {
        let mut doc = Document::from_file(file);
        let (pre_bytes, pre_notes) = snapshot(&doc);
        let pre_rev = doc.revision();

        // interleave real (valid) ops with an op targeting a missing track —
        // if any op errors the whole transaction must roll back
        let mut ops = Vec::new();
        for spec in &specs {
            ops.extend(build_ops(&mut doc, spec));
        }
        let bogus_track = doc.tracks.len() + bad_track_delta;
        let bad = Op::InsertEvents {
            track: bogus_track,
            events: vec![mint_event(&mut doc)],
        };
        if ops.is_empty() {
            ops.push(bad);
        } else {
            ops.insert(ops.len() / 2, bad);
        }
        let tx = Transaction {
            label: "poisoned".into(),
            base: doc.revision(),
            ops,
        };
        prop_assert!(doc.apply(tx).is_err());
        let (post_bytes, post_notes) = snapshot(&doc);
        prop_assert_eq!(post_bytes, pre_bytes, "failed apply must not change bytes");
        prop_assert_eq!(post_notes, pre_notes, "failed apply must not change notes");
        prop_assert_eq!(doc.revision(), pre_rev, "failed apply must not bump revision");
    }

    /// base must equal the current revision — anything else is rejected and
    /// leaves the document untouched.
    #[test]
    fn stale_revision_is_rejected(file in arb_file(), delta in 1u64..=4) {
        let mut doc = Document::from_file(file);
        let (pre_bytes, _) = snapshot(&doc);
        let tx = Transaction {
            label: "stale".into(),
            base: doc.revision() + delta,
            ops: vec![],
        };
        let stale = matches!(doc.apply(tx), Err(document::ApplyError::StaleRevision { .. }));
        prop_assert!(stale, "expected StaleRevision");
        prop_assert_eq!(doc.serialize(WriteOptions::default()), pre_bytes);
    }

    /// A transaction carrying an op against a track that doesn't exist is
    /// rejected wholesale — not applied partially.
    #[test]
    fn unknown_track_errors(file in arb_file(), extra in 1usize..=64) {
        let mut doc = Document::from_file(file);
        let tx = Transaction {
            label: "bad".into(),
            base: doc.revision(),
            ops: vec![Op::RemoveEvents {
                track: doc.tracks.len() + extra,
                removed: vec![],
            }],
        };
        let unknown = matches!(doc.apply(tx), Err(document::ApplyError::UnknownTrack(_)));
        prop_assert!(unknown, "expected UnknownTrack");
    }
}
