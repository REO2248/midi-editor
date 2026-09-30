//! Property tests for the transaction layer: random documents plus
//! randomized transaction sequences, asserting apply→revert restores both
//! serialized and semantic state, failed applies are atomic, and stale
//! revisions are rejected. Extreme tick values are exercised throughout.

use bytes::Bytes;
use document::{Document, Event as DocEvent, Note, Op, Track as DocTrack, Transaction};
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
    Quantize {
        track: usize,
        from: u64,
        to: u64,
        grid: u64,
        strength: u32,
    },
    Transpose {
        track: usize,
        from: u64,
        to: u64,
        semitones: i32,
    },
    ScaleVelocity {
        track: usize,
        from: u64,
        to: u64,
        factor: f64,
    },
    Humanize {
        track: usize,
        from: u64,
        to: u64,
        timing: i64,
        vel: i32,
    },
    Legato {
        track: usize,
        from: u64,
        to: u64,
    },
    SetLength {
        track: usize,
        from: u64,
        to: u64,
        ticks: u64,
    },
    SetVelocity {
        track: usize,
        from: u64,
        to: u64,
        vel: u8,
    },
    SetChannel {
        track: usize,
        from: u64,
        to: u64,
        channel: u8,
    },
    SetProgram {
        track: usize,
        tick: u64,
        channel: u8,
        program: u8,
        msb: Option<u8>,
        lsb: Option<u8>,
    },
    SetCc {
        track: usize,
        tick: u64,
        channel: u8,
        cc: u8,
        value: u8,
    },
    SetPitchBend {
        track: usize,
        tick: u64,
        channel: u8,
        value: u16,
    },
    SetTempo {
        tick: u64,
        bpm: f64,
    },
    SetTimeSig {
        tick: u64,
        num: u8,
        den: u8,
    },
    SetTrackChannel {
        track: usize,
        channel: u8,
    },
    DuplicateRange {
        track: usize,
        from: u64,
        to: u64,
    },
    DeleteRange {
        track: usize,
        from: u64,
        to: u64,
    },
    AddTrack {
        name: Option<String>,
    },
    RemoveTrack {
        index: usize,
    },
    SetTrackName {
        track: usize,
        name: String,
    },
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
            OpSpec::SetTempo { tick, bpm } => doc.set_tempo_ops(0, tick, bpm),
            OpSpec::SetTimeSig { tick, num, den } => doc.set_time_sig_ops(0, tick, num, den),
            OpSpec::AddTrack { ref name } => doc.add_track_ops(name.as_deref()),
            _ => vec![],
        };
    }
    match *spec {
        OpSpec::Quantize {
            track,
            from,
            to,
            grid,
            strength,
        } => doc.quantize_ops(track % n, from, to, grid, strength),
        OpSpec::Transpose {
            track,
            from,
            to,
            semitones,
        } => doc.transpose_ops(track % n, from, to, semitones),
        OpSpec::ScaleVelocity {
            track,
            from,
            to,
            factor,
        } => doc.scale_velocity_ops(track % n, from, to, factor),
        OpSpec::Humanize {
            track,
            from,
            to,
            timing,
            vel,
        } => doc.humanize_ops(
            track % n,
            from,
            to,
            timing,
            vel,
            timing as u64 ^ ((vel as u64) << 32),
        ),
        OpSpec::Legato { track, from, to } => doc.legato_ops(track % n, from, to, 0),
        OpSpec::SetLength {
            track,
            from,
            to,
            ticks,
        } => doc.set_length_ops(track % n, from, to, ticks),
        OpSpec::SetVelocity {
            track,
            from,
            to,
            vel,
        } => doc.set_velocity_ops(track % n, from, to, vel),
        OpSpec::SetChannel {
            track,
            from,
            to,
            channel,
        } => doc.set_channel_ops(track % n, from, to, channel),
        OpSpec::SetProgram {
            track,
            tick,
            channel,
            program,
            msb,
            lsb,
        } => doc.set_program_ops(track % n, tick, channel, program, msb, lsb),
        OpSpec::SetCc {
            track,
            tick,
            channel,
            cc,
            value,
        } => doc.set_cc_ops(track % n, tick, channel, cc, value),
        OpSpec::SetPitchBend {
            track,
            tick,
            channel,
            value,
        } => doc.set_pitch_bend_ops(track % n, tick, channel, value),
        OpSpec::SetTempo { tick, bpm } => doc.set_tempo_ops(0, tick, bpm),
        OpSpec::SetTimeSig { tick, num, den } => doc.set_time_sig_ops(0, tick, num, den),
        OpSpec::SetTrackChannel { track, channel } => doc.set_track_channel_ops(track % n, channel),
        OpSpec::DuplicateRange { track, from, to } => doc.duplicate_range_ops(track % n, from, to),
        OpSpec::DeleteRange { track, from, to } => doc.delete_range_ops(track % n, from, to),
        OpSpec::AddTrack { ref name } => doc.add_track_ops(name.as_deref()),
        OpSpec::RemoveTrack { index } => doc.remove_track_ops(index % n),
        OpSpec::SetTrackName { track, ref name } => doc.set_track_name_ops(track % n, name),
    }
}

/// Semantic snapshot: serialized bytes, the derived note list, cached track
/// names, and the encoding hint (ids excluded — they can legitimately differ
/// after redo cycles).
#[allow(clippy::type_complexity)]
fn snapshot(
    doc: &Document,
) -> (
    Vec<u8>,
    Vec<(usize, u8, u8, u8, u64, Option<u64>)>,
    Vec<Option<Vec<u8>>>,
    Option<smf_core::TextEncoding>,
) {
    let notes: Vec<_> = doc
        .notes()
        .into_iter()
        .map(|n: Note| (n.track, n.channel, n.key, n.vel, n.start_tick, n.end_tick))
        .collect();
    let names: Vec<_> = doc
        .tracks
        .iter()
        .map(|t| t.name.as_ref().map(|b| b.to_vec()))
        .collect();
    (
        doc.serialize(WriteOptions::default()),
        notes,
        names,
        doc.text_encoding_hint(),
    )
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
            let pre = snapshot(&doc);
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
            // event ids stay unique — the by_id index depends on it
            let mut ids = std::collections::HashSet::new();
            prop_assert!(
                doc.tracks
                    .iter()
                    .flat_map(|t| t.events.iter().map(|e| e.id))
                    .all(|id| ids.insert(id)),
                "duplicate event ids after apply"
            );
            // the playback timeline covers every channel event, sorted by µs
            let channel_events: usize = doc
                .tracks
                .iter()
                .flat_map(|t| t.events.iter())
                .filter(|e| matches!(e.kind, EventKind::Channel { .. }))
                .count();
            let tagged = doc.timeline_tagged();
            prop_assert_eq!(tagged.len(), channel_events);
            prop_assert_eq!(doc.timeline().len(), channel_events);
            prop_assert!(tagged.windows(2).all(|w| w[0].0 <= w[1].0));
            doc.revert(&tx);
            // revert is itself a state change — revision stays monotonic
            prop_assert_eq!(doc.revision(), pre_rev + 2);
            prop_assert_eq!(snapshot(&doc), pre, "revert must restore the full snapshot");
        }
    }

    /// A whole sequence applied then reverted in reverse order returns the
    /// document to its original state.
    #[test]
    fn sequence_reverts_to_origin(file in arb_file(), specs in prop::collection::vec(arb_opspec(), 1..=10)) {
        let mut doc = Document::from_file(file);
        let origin = snapshot(&doc);
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
        prop_assert_eq!(snapshot(&doc), origin);
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
        let pre = snapshot(&doc);
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
        prop_assert_eq!(snapshot(&doc), pre, "failed apply must not change the snapshot");
        prop_assert_eq!(doc.revision(), pre_rev, "failed apply must not bump revision");
    }

    /// base must equal the current revision — anything else is rejected and
    /// leaves the document untouched.
    #[test]
    fn stale_revision_is_rejected(file in arb_file(), delta in 1u64..=4) {
        let mut doc = Document::from_file(file);
        let (pre_bytes, ..) = snapshot(&doc);
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

    /// Track-index ops at or past the end of the track list are in-range for
    /// clamping (InsertTrack) or silent skips (RemoveTrack) — never a panic.
    /// This pins the bounds checks so a `<`/`<=` slip is caught.
    #[test]
    fn out_of_range_track_ops_do_not_panic(file in arb_file(), over in 0usize..=16) {
        let mut doc = Document::from_file(file);
        let n = doc.tracks.len();
        let over_idx = n + over;

        // RemoveTrack past the end: rejected as UnknownTrack, doc untouched.
        let tx = Transaction {
            label: "oob-remove".into(),
            base: doc.revision(),
            ops: vec![Op::RemoveTrack {
                index: over_idx,
                track: DocTrack {
                    name: None,
                    out_port: 0,
                    out_channel: 0,
                    events: vec![],
                },
            }],
        };
        let unknown = matches!(doc.apply(tx), Err(document::ApplyError::UnknownTrack(_)));
        prop_assert!(unknown, "expected UnknownTrack for RemoveTrack past end");
        prop_assert_eq!(doc.tracks.len(), n, "rejected apply must not change tracks");

        // InsertTrack past the end: clamps to the end of the list.
        let tx = Transaction {
            label: "oob-insert".into(),
            base: doc.revision(),
            ops: vec![Op::InsertTrack {
                index: over_idx,
                track: DocTrack {
                    name: Some(Bytes::from_static(b"appended")),
                    out_port: 0,
                    out_channel: 0,
                    events: vec![],
                },
            }],
        };
        doc.apply(tx.clone()).expect("InsertTrack past end must clamp");
        prop_assert_eq!(doc.tracks.len(), n + 1);
        doc.revert(&tx);
        prop_assert_eq!(doc.tracks.len(), n, "revert of clamped insert must remove it");
    }
}

/// Corrupt channel data (data byte with the top bit set) can arrive via
/// UpdateEvent even though the parser never emits it — the key-range guards
/// in notes()/chase_events must skip it, not index a 128-entry table.
#[test]
fn corrupt_channel_data_is_filtered() {
    let file = File {
        format: 0,
        division: Division::Metrical(480),
        warnings: vec![],
        tracks: vec![SmfTrack {
            events: vec![mk_ev(
                EventKind::Channel {
                    status: 0x90,
                    data: [60, 100],
                    len: 2,
                },
                0,
            )],
        }],
    };
    let mut doc = Document::from_file(file);
    let before = doc.tracks[0].events[0].clone();
    let mut after = before.clone();
    if let EventKind::Channel { data, .. } = &mut after.kind {
        data[0] = 0xFF;
    }
    let tx = Transaction {
        label: "corrupt".into(),
        base: doc.revision(),
        ops: vec![Op::UpdateEvent {
            track: 0,
            before,
            after,
        }],
    };
    doc.apply(tx).unwrap();
    // must not panic and must not report the corrupt key as a note
    assert!(doc.notes().iter().all(|n| n.key < 0x80));
    let _ = doc.chase_events(0);
    let _ = doc.chase_events(u64::MAX);
}

/// Conventional metas (FF 03 name / FF 21 port / FF 20 channel / FF 09
/// charset) are picked up into the cached track fields — the guards decide
/// what sticks: first name wins, port/channel need non-empty payloads, and
/// "JP" in the charset marker selects Shift-JIS.
#[test]
fn conventional_metas_are_cached() {
    let file = File {
        format: 1,
        division: Division::Metrical(480),
        warnings: vec![],
        tracks: vec![
            SmfTrack {
                events: vec![
                    mk_ev(
                        EventKind::Meta {
                            meta_type: 0x03,
                            data: Bytes::from_static(b"Piano"),
                        },
                        0,
                    ),
                    mk_ev(
                        EventKind::Meta {
                            meta_type: 0x03,
                            data: Bytes::from_static(b"Ignored"),
                        },
                        0,
                    ),
                    mk_ev(
                        EventKind::Meta {
                            meta_type: 0x21,
                            data: Bytes::from_static(&[3]),
                        },
                        0,
                    ),
                    mk_ev(
                        EventKind::Meta {
                            meta_type: 0x20,
                            data: Bytes::from_static(&[9]),
                        },
                        0,
                    ),
                    mk_ev(
                        EventKind::Meta {
                            meta_type: 0x09,
                            data: Bytes::from_static(b"JP"),
                        },
                        0,
                    ),
                ],
            },
            SmfTrack {
                events: vec![mk_ev(
                    EventKind::Meta {
                        meta_type: 0x21,
                        data: Bytes::new(), // empty payload — must be ignored
                    },
                    0,
                )],
            },
        ],
    };
    let doc = Document::from_file(file);
    assert_eq!(doc.tracks[0].name.as_deref(), Some(b"Piano".as_slice()));
    assert_eq!(doc.tracks[0].out_port, 3);
    assert_eq!(doc.tracks[0].out_channel, 9);
    assert!(matches!(
        doc.text_encoding_hint(),
        Some(smf_core::TextEncoding::ShiftJis)
    ));
    assert_eq!(
        doc.tracks[1].out_port, 0,
        "empty FF 21 payload must be ignored"
    );
}

/// `diagnose` must find each documented class of import problem, and
/// `fix_ops` must emit ops that actually clear them — this pins the codes,
/// the per-arm match structure, and the event bookkeeping.
fn mk_ev(kind: EventKind, tick: u64) -> SmfEvent {
    SmfEvent {
        tick,
        seq: 0,
        raw_body: None,
        kind,
    }
}

#[test]
fn diagnose_and_fix_ops_cover_each_finding() {
    let file = File {
        format: 1,
        division: Division::Metrical(480),
        warnings: vec![],
        tracks: vec![
            // conductor track: a clean note pair + EOT
            SmfTrack {
                events: vec![
                    mk_ev(
                        EventKind::Channel {
                            status: 0x90,
                            data: [60, 100],
                            len: 2,
                        },
                        0,
                    ),
                    mk_ev(
                        EventKind::Channel {
                            status: 0x80,
                            data: [60, 0],
                            len: 2,
                        },
                        480,
                    ),
                    mk_ev(
                        EventKind::Meta {
                            meta_type: 0x2F,
                            data: Bytes::new(),
                        },
                        480,
                    ),
                ],
            },
            // problem track: tempo in the wrong place, a dangling note-on,
            // a zero-length note, and no End-of-Track
            SmfTrack {
                events: vec![
                    mk_ev(
                        EventKind::Meta {
                            meta_type: 0x51,
                            data: Bytes::from_static(&[0x07, 0xA1, 0x20]),
                        },
                        0,
                    ),
                    mk_ev(
                        EventKind::Channel {
                            status: 0x91,
                            data: [64, 90],
                            len: 2,
                        },
                        0,
                    ),
                    mk_ev(
                        EventKind::Channel {
                            status: 0x91,
                            data: [66, 90],
                            len: 2,
                        },
                        240,
                    ),
                    mk_ev(
                        EventKind::Channel {
                            status: 0x81,
                            data: [66, 0],
                            len: 2,
                        },
                        240,
                    ),
                ],
            },
        ],
    };
    let mut doc = Document::from_file(file);

    let codes: Vec<&str> = doc.diagnose().iter().map(|d| d.code).collect();
    for want in [
        "tempo-outside-conductor",
        "dangling-noteon",
        "zero-length-note",
        "missing-eot",
    ] {
        assert!(
            codes.contains(&want),
            "missing diagnostic {want}: {codes:?}"
        );
    }

    // filtered fix_ops only emits ops for the selected codes
    let only_eot = doc.fix_ops(&["missing-eot"]);
    assert!(!only_eot.is_empty());
    assert!(matches!(only_eot[0], Op::InsertEvents { track: 1, .. }));

    // fixing everything clears every finding
    let ops = doc.fix_ops(&[]);
    assert!(ops.len() >= 4, "expected >=4 fix ops, got {}", ops.len());
    let tx = Transaction {
        label: "fix".into(),
        base: doc.revision(),
        ops,
    };
    doc.apply(tx).unwrap();
    assert!(
        doc.diagnose().is_empty(),
        "fix_ops must clear all findings: {:?}",
        doc.diagnose().iter().map(|d| d.code).collect::<Vec<_>>()
    );
}

fn mk_note_pair(key: u8, vel: u8, start: u64, end: u64) -> Vec<SmfEvent> {
    vec![
        mk_ev(
            EventKind::Channel {
                status: 0x90,
                data: [key, vel],
                len: 2,
            },
            start,
        ),
        mk_ev(
            EventKind::Channel {
                status: 0x80,
                data: [key, 0],
                len: 2,
            },
            end,
        ),
    ]
}

/// The note transforms only touch notes that START inside [from,to) — the
/// boundary comparisons are the contract, and this test pins them plus the
/// grid arithmetic and the out-of-range-key skip.
#[test]
fn transforms_respect_range_boundaries() {
    let mut events = Vec::new();
    events.extend(mk_note_pair(60, 30, 60, 200)); // start 60: off-grid, in range
    events.extend(mk_note_pair(62, 30, 480, 700)); // start 480: in range
    events.extend(mk_note_pair(62, 30, 960, 1200)); // start 960: in range, same key as the 480 note
    events.extend(mk_note_pair(66, 30, 1440, 1600)); // start 1440: == to, out
    events.extend(mk_note_pair(127, 30, 1000, 1200)); // key 127: transpose must skip
    events.push(mk_ev(
        EventKind::Meta {
            meta_type: 0x2F,
            data: Bytes::new(),
        },
        1600,
    ));
    let file = File {
        format: 0,
        division: Division::Metrical(480),
        warnings: vec![],
        tracks: vec![SmfTrack { events }],
    };
    let mut doc = Document::from_file(file);

    // quantize [0,960) grid=240 @100%: only the 60→0 note moves (2 events);
    // 480 is on-grid (delta 0 → skipped), 960/1440 are out of range.
    let ops = doc.quantize_ops(0, 0, 960, 240, 100);
    assert_eq!(ops.len(), 2, "quantize must move exactly the off-grid note");
    for op in &ops {
        let Op::UpdateEvent { before, after, .. } = op else {
            panic!("quantize must emit UpdateEvent ops");
        };
        // the whole note shifts by its snap delta (-60): on 60→0, off 200→140
        assert_eq!(after.tick as i64 - before.tick as i64, -60);
    }

    // transpose [480,1440) +12: notes at 480/960 move, the 1440/60/127-key
    // notes don't. 2 notes × (on+off) = 4 ops.
    let ops = doc.transpose_ops(0, 480, 1440, 12);
    assert_eq!(
        ops.len(),
        4,
        "transpose must hit exactly 2 in-range movable notes"
    );
    for op in &ops {
        let Op::UpdateEvent { before, after, .. } = op else {
            panic!("transpose must emit UpdateEvent ops");
        };
        let (EventKind::Channel { data: bd, .. }, EventKind::Channel { data: ad, .. }) =
            (&before.kind, &after.kind)
        else {
            panic!("channel events only");
        };
        assert!(ad[0] <= 127, "transposed key must stay in range");
        assert_eq!(ad[0], bd[0] + 12);
    }

    // scale_velocity [480,1440) ×2: the three in-range on-events (incl. the
    // key-127 note — velocity scaling has no key limit) = 3 ops.
    let ops = doc.scale_velocity_ops(0, 480, 1440, 2.0);
    assert_eq!(ops.len(), 3, "scale_velocity edits noteOn events only");
    for op in &ops {
        let Op::UpdateEvent { before, after, .. } = op else {
            panic!("scale_velocity must emit UpdateEvent ops");
        };
        let (EventKind::Channel { data: bd, .. }, EventKind::Channel { data: ad, .. }) =
            (&before.kind, &after.kind)
        else {
            panic!("channel events only");
        };
        assert_eq!(ad[1], (bd[1] * 2).clamp(1, 127));
    }

    // set_velocity [480,1440) →42: the three in-range on-events only.
    let ops = doc.set_velocity_ops(0, 480, 1440, 42);
    assert_eq!(ops.len(), 3);
    for op in &ops {
        let Op::UpdateEvent { after, .. } = op else {
            panic!()
        };
        let EventKind::Channel { data, .. } = &after.kind else {
            panic!()
        };
        assert_eq!(data[1], 42);
    }

    // set_length [480,1440) →120: the three in-range off-events move to
    // start+120.
    let ops = doc.set_length_ops(0, 480, 1440, 120);
    assert_eq!(ops.len(), 3);
    for op in &ops {
        let Op::UpdateEvent { before, after, .. } = op else {
            panic!()
        };
        assert!(matches!(before.kind, EventKind::Channel { status, .. } if status & 0xF0 == 0x80));
        assert!(matches!(before.tick, 700 | 1200));
        let _ = after;
    }

    // legato [480,1440): the key-62 pair (480→960) is the only adjacent
    // same-key pair — its off moves to the next start.
    let ops = doc.legato_ops(0, 480, 1440, 0);
    assert_eq!(ops.len(), 1);
    let Op::UpdateEvent { before, after, .. } = &ops[0] else {
        panic!()
    };
    assert_eq!(before.tick, 700);
    assert_eq!(after.tick, 960);

    // set_channel [480,1440) →ch5: every in-range channel event's status
    // low-nibble becomes 5, high nibble preserved.
    let ops = doc.set_channel_ops(0, 480, 1440, 5);
    assert_eq!(
        ops.len(),
        6,
        "set_channel covers every in-range channel event"
    );
    for op in &ops {
        let Op::UpdateEvent { before, after, .. } = op else {
            panic!()
        };
        let (EventKind::Channel { status: bs, .. }, EventKind::Channel { status: a_s, .. }) =
            (&before.kind, &after.kind)
        else {
            panic!()
        };
        assert_eq!(*a_s, (*bs & 0xF0) | 0x05);
        assert!(before.tick >= 480 && before.tick < 1440);
    }

    // set_program ch9 prog7: one program-change event, status 0xC9.
    let ops = doc.set_program_ops(0, 0, 9, 7, None, None);
    let Op::InsertEvents { events, .. } = &ops[0] else {
        panic!()
    };
    assert_eq!(events.len(), 1);
    assert!(matches!(
        events[0].kind,
        EventKind::Channel {
            status: 0xC9,
            data: [7, 0],
            len: 1
        }
    ));

    // set_track_channel →ch9: inserts an FF 20 meta holding the channel
    // nibble.
    let ops = doc.set_track_channel_ops(0, 9);
    let Op::InsertEvents { events, .. } = &ops[0] else {
        panic!()
    };
    assert!(matches!(
        &events[0].kind,
        EventKind::Meta {
            meta_type: 0x20,
            data
        } if data[..] == [9]
    ));

    // delete_range [480,1440): one RemoveEvents op per event — on+off for
    // the three notes = 6 removals; meta events and the notes at 60/1440
    // survive.
    let ops = doc.delete_range_ops(0, 480, 1440);
    assert_eq!(ops.len(), 6);
    for op in &ops {
        let Op::RemoveEvents { removed, .. } = op else {
            panic!()
        };
        assert_eq!(removed.len(), 1);
        assert!(matches!(removed[0].1.kind, EventKind::Channel { .. }));
    }

    // duplicate_range [480,1440): clones the 6 in-range channel events, all
    // shifted to [1440, 2400) — notes keep their offs.
    let ops = doc.duplicate_range_ops(0, 480, 1440);
    assert_eq!(ops.len(), 1);
    let Op::InsertEvents { events, .. } = &ops[0] else {
        panic!()
    };
    assert_eq!(events.len(), 6);
    for e in events {
        assert!(
            e.tick >= 1440 && e.tick < 2400,
            "dup tick {} out of range",
            e.tick
        );
    }
}
