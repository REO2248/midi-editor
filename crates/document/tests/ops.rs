// Semantic ops generators: the same functions drive GUI chips and MCP tools.
// Contract per generator: returns pure Vec<Op> (no doc mutation besides id
// minting); applying then reverting the transaction restores the document.
use bytes::Bytes;
use document::*;
use smf_core::{Division, EventKind};

fn chan(tick: u64, status: u8, d0: u8, d1: u8) -> smf_core::Event {
    smf_core::Event {
        tick,
        seq: 0,
        raw_body: None,
        kind: EventKind::Channel {
            status,
            data: [d0, d1],
            len: match status & 0xF0 {
                0xC0 | 0xD0 => 1,
                _ => 2,
            },
        },
    }
}

fn doc(tracks: Vec<Vec<smf_core::Event>>) -> Document {
    Document::from_file(smf_core::File {
        format: 1,
        division: Division::Metrical(480),
        tracks: tracks
            .into_iter()
            .map(|events| smf_core::Track { events })
            .collect(),
        warnings: vec![],
    })
}

fn notes_on(d: &Document, track: usize) -> Vec<Note> {
    d.notes().into_iter().filter(|n| n.track == track).collect()
}

fn apply(d: &mut Document, ops: Vec<Op>) -> Transaction {
    let tx = Transaction {
        label: "t".into(),
        base: d.revision(),
        ops,
    };
    d.apply(tx.clone()).unwrap();
    tx
}

#[test]
fn quantize_moves_on_and_off_together() {
    // note 490..970 → grid 480: start snaps to 480, off to 960 (shift -10/-10)
    let mut d = doc(vec![vec![chan(490, 0x90, 60, 100), chan(970, 0x80, 60, 0)]]);
    let ops = d.quantize_ops(0, 0, u64::MAX, 480, 100);
    apply(&mut d, ops);
    let n = &notes_on(&d, 0)[0];
    assert_eq!((n.start_tick, n.end_tick), (480, Some(960)));
}

#[test]
fn quantize_strength_50_lands_halfway() {
    let mut d = doc(vec![vec![chan(540, 0x90, 60, 100)]]);
    let ops = d.quantize_ops(0, 0, u64::MAX, 480, 50);
    apply(&mut d, ops);
    assert_eq!(notes_on(&d, 0)[0].start_tick, 510);
}

#[test]
fn quantize_range_limited() {
    let mut d = doc(vec![vec![
        chan(490, 0x90, 60, 100),
        chan(2000, 0x90, 64, 100),
    ]]);
    let ops = d.quantize_ops(0, 0, 1000, 480, 100);
    apply(&mut d, ops);
    let notes = notes_on(&d, 0);
    assert_eq!(notes[0].start_tick, 480);
    assert_eq!(notes[1].start_tick, 2000, "out-of-range untouched");
}

#[test]
fn transpose_skips_out_of_range_and_meta() {
    let mut d = doc(vec![vec![
        smf_core::Event {
            tick: 0,
            seq: 0,
            raw_body: None,
            kind: EventKind::Meta {
                meta_type: 0x03,
                data: b"name".to_vec().into(),
            },
        },
        chan(100, 0x90, 60, 100),
        chan(200, 0x90, 127, 100),
    ]]);
    let ops = d.transpose_ops(0, 0, u64::MAX, 5);
    let tx = apply(&mut d, ops);
    let notes = notes_on(&d, 0);
    assert_eq!(notes.iter().find(|n| n.vel == 100).unwrap().key, 65);
    assert!(notes.iter().any(|n| n.key == 127), "127+5 skips, stays");
    // revert restores
    d.revert(&tx);
    assert!(notes_on(&d, 0).iter().any(|n| n.key == 60));
    // meta untouched
    assert!(matches!(
        d.tracks[0].events[0].kind,
        EventKind::Meta {
            meta_type: 0x03,
            ..
        }
    ));
}

#[test]
fn scale_velocity_clamps() {
    let mut d = doc(vec![vec![chan(0, 0x90, 60, 100), chan(100, 0x90, 62, 10)]]);
    let __ops = d.scale_velocity_ops(0, 0, u64::MAX, 2.0);
    apply(&mut d, __ops);
    assert_eq!(notes_on(&d, 0)[0].vel, 127, "100*2 clamps to 127");
    let __ops = d.scale_velocity_ops(0, 0, u64::MAX, 0.01);
    apply(&mut d, __ops);
    assert!(
        notes_on(&d, 0).iter().all(|n| n.vel >= 1),
        "never scales to vel 0 (would mean noteOff)"
    );
}

#[test]
fn note_off_velocity_and_form_are_captured() {
    // 0x80 off carrying release 42 + 0x90-vel0 off — the Note model
    // keeps the release value AND which wire form closed the note
    let d = doc(vec![vec![
        chan(0, 0x90, 60, 100),
        chan(480, 0x80, 60, 42),
        chan(0, 0x90, 64, 90),
        chan(480, 0x90, 64, 0),
    ]]);
    let ns = notes_on(&d, 0);
    assert_eq!(ns.len(), 2);
    assert!(
        !ns[0].off_via_on && ns[0].off_vel == 42,
        "0x80 off → release 42"
    );
    assert!(
        ns[1].off_via_on && ns[1].off_vel == 0,
        "0x90-vel0 off → form kept"
    );
    // dangling note-on reports a zero/0x80 default — nothing stored
    let d = doc(vec![vec![chan(0, 0x90, 60, 100)]]);
    assert!(notes_on(&d, 0)[0].off_id.is_none());
}

#[test]
fn release_velocity_survives_serialize_roundtrip() {
    // import → serialize → re-import: release velocity and the 0x80/0x90v0
    // split come through byte-for-byte
    let d = doc(vec![vec![
        chan(0, 0x90, 60, 100),
        chan(480, 0x80, 60, 42),
        chan(0, 0x90, 64, 90),
        chan(480, 0x90, 64, 0),
    ]]);
    let bytes = d.serialize(smf_core::WriteOptions {
        running_status: true,
    });
    let d2 = Document::from_file(smf_core::parse(&bytes).unwrap());
    let ns = notes_on(&d2, 0);
    assert_eq!(ns.len(), 2);
    assert!(!ns[0].off_via_on && ns[0].off_vel == 42);
    assert!(ns[1].off_via_on && ns[1].off_vel == 0);
}

#[test]
fn release_velocity_survives_structural_edits() {
    // transpose rewrites the pitch byte on BOTH ends; set_length moves the
    // off's tick — the release byte rides along untouched
    let mut d = doc(vec![vec![chan(0, 0x90, 60, 100), chan(500, 0x80, 60, 42)]]);
    let __ops = d.transpose_ops(0, 0, u64::MAX, 5);
    apply(&mut d, __ops);
    let __ops = d.set_length_ops(0, 0, u64::MAX, 960);
    apply(&mut d, __ops);
    let n = &notes_on(&d, 0)[0];
    assert_eq!(n.key, 65);
    let off = d.tracks[0]
        .events
        .iter()
        .find(|e| Some(e.id) == n.off_id)
        .unwrap();
    match &off.kind {
        EventKind::Channel { status, data, .. } => {
            assert_eq!(*status & 0xF0, 0x80);
            assert_eq!(data, &[65, 42]);
        }
        _ => panic!("off event lost its channel kind"),
    }
    assert_eq!(off.tick, 960, "off moved to start+len");
    assert_eq!(n.off_vel, 42);
}

#[test]
fn set_release_velocity_upgrades_0x90v0_to_0x80() {
    // vel>0: a real note-off is required — the 0x90v0 form has no byte
    // for release data; an existing 0x80 keeps its form
    let mut d = doc(vec![vec![
        chan(0, 0x90, 60, 100),
        chan(480, 0x80, 60, 10),
        chan(0, 0x90, 64, 90),
        chan(480, 0x90, 64, 0),
    ]]);
    let __ops = d.set_release_velocity_ops(0, 0, u64::MAX, 42);
    let tx = apply(&mut d, __ops);
    let ns = notes_on(&d, 0);
    for n in &ns {
        let off = d.tracks[0]
            .events
            .iter()
            .find(|e| Some(e.id) == n.off_id)
            .unwrap();
        match &off.kind {
            EventKind::Channel { status, data, .. } => {
                assert_eq!(*status, 0x80, "key {}: upgraded/kept as 0x80", n.key);
                assert_eq!(data[1], 42);
            }
            _ => panic!(),
        }
    }
    assert!(ns.iter().all(|n| n.off_vel == 42 && !n.off_via_on));
    // revert restores the original forms
    d.revert(&tx);
    let ns = notes_on(&d, 0);
    assert!(ns[1].off_via_on && ns[1].off_vel == 0);
}

#[test]
fn set_release_velocity_zero_preserves_form() {
    // vel=0 must NOT normalize 0x90v0 → 0x80 — both are valid "released"
    let mut d = doc(vec![vec![chan(0, 0x90, 64, 90), chan(480, 0x90, 64, 0)]]);
    let __ops = d.set_release_velocity_ops(0, 0, u64::MAX, 0);
    apply(&mut d, __ops);
    assert!(matches!(
        d.tracks[0].events[1].kind,
        EventKind::Channel { status: 0x90, .. }
    ));
    // and on an 0x80 it zeroes the release byte without changing form
    let mut d = doc(vec![vec![chan(0, 0x90, 64, 90), chan(480, 0x80, 64, 42)]]);
    let __ops = d.set_release_velocity_ops(0, 0, u64::MAX, 0);
    apply(&mut d, __ops);
    let off_id = notes_on(&d, 0)[0].off_id.unwrap();
    let off = d.tracks[0].events.iter().find(|e| e.id == off_id).unwrap();
    match &off.kind {
        EventKind::Channel { status, data, .. } => {
            assert_eq!(*status, 0x80);
            assert_eq!(data[1], 0);
        }
        _ => panic!(),
    }
}

#[test]
fn set_channel_rewrites_nibble_only_on_channel_events() {
    let mut d = doc(vec![vec![
        chan(0, 0x90, 60, 100),
        chan(50, 0xE0, 0, 64), // pitch bend
        smf_core::Event {
            tick: 60,
            seq: 0,
            raw_body: None,
            kind: EventKind::Meta {
                meta_type: 0x05,
                data: b"lyr".to_vec().into(),
            },
        },
    ]]);
    let __ops = d.set_channel_ops(0, 0, u64::MAX, 9);
    apply(&mut d, __ops);
    assert!(matches!(
        d.tracks[0].events[0].kind,
        EventKind::Channel { status: 0x99, .. }
    ));
    assert!(matches!(
        d.tracks[0].events[1].kind,
        EventKind::Channel { status: 0xE9, .. }
    ));
    assert!(matches!(
        d.tracks[0].events[2].kind,
        EventKind::Meta {
            meta_type: 0x05,
            ..
        }
    ));
}

#[test]
fn set_program_emits_bank_then_pc_in_order() {
    let mut d = doc(vec![vec![chan(100, 0x90, 60, 100)]]);
    let __ops = d.set_program_ops(0, 0, 2, 10, Some(8), Some(0));
    apply(&mut d, __ops);
    let kinds: Vec<u8> = d.tracks[0]
        .events
        .iter()
        .filter(|e| e.tick == 0)
        .filter_map(|e| match e.kind {
            EventKind::Channel { status, data, .. } => Some(match status {
                0xB2 if data[0] == 0 => 0,  // MSB
                0xB2 if data[0] == 32 => 1, // LSB
                0xC2 => 2,                  // PC
                _ => 9,
            }),
            _ => None,
        })
        .collect();
    assert_eq!(kinds, vec![0, 1, 2], "MSB -> LSB -> PC ordering");
}

#[test]
fn set_tempo_replaces_same_tick_inserts_elsewhere() {
    let mut d = doc(vec![vec![], vec![chan(0, 0x90, 60, 100)]]);
    let __ops = d.set_tempo_ops(0, 0, 120.0);
    apply(&mut d, __ops);
    let n_tempos = |d: &Document| {
        d.tracks[0]
            .events
            .iter()
            .filter(|e| {
                matches!(
                    e.kind,
                    EventKind::Meta {
                        meta_type: 0x51,
                        ..
                    }
                )
            })
            .count()
    };
    assert_eq!(n_tempos(&d), 1);
    let __ops = d.set_tempo_ops(0, 0, 140.0); // same tick: replace
    apply(&mut d, __ops);
    assert_eq!(n_tempos(&d), 1);
    // 140bpm: 480 ticks = one quarter = 60e6/140 us
    assert_eq!(d.tempo_map.tick_to_us(480), 428_571);
    let __ops = d.set_tempo_ops(0, 960, 60.0); // new tick: insert
    apply(&mut d, __ops);
    assert_eq!(n_tempos(&d), 2);
    // tempo map: 0..960 at 140bpm, 960.. at 60bpm
    let us = d.tempo_map.tick_to_us(1440);
    // 960 ticks @140bpm (2 quarters) + 480 ticks @60bpm (1 quarter)
    assert_eq!(us, 428_571 * 2 + 1_000_000);
}

#[test]
fn set_time_sig_encodes_denominator_log2() {
    let mut d = doc(vec![vec![]]);
    let __ops = d.set_time_sig_ops(0, 0, 6, 8);
    apply(&mut d, __ops);
    match &d.tracks[0].events[0].kind {
        EventKind::Meta {
            meta_type: 0x58,
            data,
        } => {
            assert_eq!(&data[..], &[6, 3, 24, 8], "6/8 → dd=3");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn duplicate_range_keeps_noteoff_beyond_range() {
    // note 100..3000 duplicated over [0,500): copy lands 3000..5900 with its off
    let mut d = doc(vec![vec![
        chan(100, 0x90, 60, 100),
        chan(3000, 0x80, 60, 0),
    ]]);
    let __ops = d.duplicate_range_ops(0, 0, 500);
    apply(&mut d, __ops);
    let notes = notes_on(&d, 0);
    assert_eq!(notes.len(), 2);
    // both notes must be closed (same-key overlap pairs LIFO — what matters
    // is that neither copy hangs)
    assert!(notes.iter().all(|n| n.end_tick.is_some()));
    let mut ends: Vec<u64> = notes.iter().filter_map(|n| n.end_tick).collect();
    ends.sort();
    assert_eq!(ends, vec![3000, 3500]);
}

#[test]
fn set_pitch_bend_encodes_14bit_centered() {
    let mut d = doc(vec![vec![]]);
    let __ops = d.set_pitch_bend_ops(0, 480, 3, 0x2000); // center
    apply(&mut d, __ops);
    match &d.tracks[0].events[0].kind {
        EventKind::Channel { status, data, .. } => {
            assert_eq!(*status, 0xE3);
            assert_eq!(*data, [0x00, 0x40], "center = lsb 0 msb 64");
        }
        other => panic!("{other:?}"),
    }
    let __ops = d.set_pitch_bend_ops(0, 480, 3, 0x3FFF); // max
    apply(&mut d, __ops);
    match &d.tracks[0].events[1].kind {
        EventKind::Channel { data, .. } => assert_eq!(*data, [0x7F, 0x7F]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn set_track_channel_writes_meta_and_field() {
    let mut d = doc(vec![vec![chan(0, 0x90, 60, 100), chan(480, 0x80, 60, 0)]]);
    let __ops = d.set_track_channel_ops(0, 9);
    let tx = apply(&mut d, __ops);
    assert_eq!(d.tracks[0].out_channel, 9);
    assert!(d.tracks[0].events.iter().any(|e| matches!(
        &e.kind,
        EventKind::Meta { meta_type: 0x20, data } if data[..] == [9]
    )));
    d.revert(&tx);
    assert_eq!(d.tracks[0].out_channel, 0);
}

#[test]
fn delete_range_removes_whole_notes() {
    let mut d = doc(vec![vec![
        chan(100, 0x90, 60, 100),
        chan(3000, 0x80, 60, 0),
        chan(500, 0xB0, 7, 100),
    ]]);
    let __ops = d.delete_range_ops(0, 0, 1000);
    apply(&mut d, __ops);
    let notes = notes_on(&d, 0);
    assert!(notes.is_empty(), "note + off + CC all gone");
    assert!(d.tracks[0].events.is_empty());
}

#[test]
fn track_ops_roundtrip_and_revert() {
    let mut d = doc(vec![vec![chan(0, 0x90, 60, 100)]]);
    let __ops = d.add_track_ops(Some("Bass"));
    let add = apply(&mut d, __ops);
    assert_eq!(d.tracks.len(), 2);
    assert_eq!(d.tracks[1].name.as_deref(), Some(b"Bass" as &[u8]));
    d.revert(&add);
    assert_eq!(d.tracks.len(), 1);

    let __ops = d.set_track_name_ops(0, "Piano");
    let ren = apply(&mut d, __ops);
    assert_eq!(d.tracks[0].name.as_deref(), Some(b"Piano" as &[u8]));
    d.revert(&ren);
    assert_eq!(d.tracks[0].name, None);

    let __ops = d.set_track_channel_ops(0, 9);
    let ch = apply(&mut d, __ops);
    assert_eq!(d.tracks[0].out_channel, 9);
    d.revert(&ch);
    assert_eq!(d.tracks[0].out_channel, 0);
}

#[test]
fn remove_track_revert_restores_contents() {
    let mut d = doc(vec![
        vec![chan(0, 0x90, 60, 100)],
        vec![chan(0, 0x91, 64, 90)],
    ]);
    let __ops = d.remove_track_ops(0);
    let tx = apply(&mut d, __ops);
    assert_eq!(d.tracks.len(), 1);
    assert_eq!(notes_on(&d, 0)[0].channel, 1);
    d.revert(&tx);
    assert_eq!(d.tracks.len(), 2);
    assert_eq!(notes_on(&d, 0)[0].channel, 0);
    assert_eq!(notes_on(&d, 1)[0].channel, 1);
}

#[test]
fn update_revert_restores_raw_body() {
    // an event parsed from bytes carries raw_body; an update then revert must
    // restore the original raw bytes, not just the kind
    let raw = smf_core::parse(&{
        let mut f = Vec::new();
        f.extend_from_slice(b"MThd\x00\x00\x00\x06\x00\x00\x00\x01\x01\xE0");
        let t = [0x00, 0x90, 0x3C, 0x64, 0x00, 0xFF, 0x2F, 0x00];
        f.extend_from_slice(b"MTrk");
        f.extend_from_slice(&(t.len() as u32).to_be_bytes());
        f.extend_from_slice(&t);
        f
    })
    .unwrap();
    let mut d = Document::from_file(raw);
    let ev = d.tracks[0].events[0].clone();
    assert!(ev.raw_body.is_some());
    let mut after = ev.clone();
    after.tick = 999;
    let tx = apply(
        &mut d,
        vec![Op::UpdateEvent {
            track: 0,
            before: ev,
            after,
        }],
    );
    d.revert(&tx);
    let back = &d.tracks[0].events[0];
    assert_eq!(back.tick, 0);
    assert_eq!(back.raw_body.as_deref(), Some(&[0x90, 0x3C, 0x64][..]));
}

#[test]
fn humanize_is_deterministic_and_bounded() {
    let mut d = doc(vec![vec![
        chan(480, 0x90, 60, 100),
        chan(960, 0x80, 60, 0),
        chan(1440, 0x90, 64, 90),
        chan(1920, 0x80, 64, 0),
    ]]);
    let ops_a = d.humanize_ops(0, 0, u64::MAX, 10, 8);
    let ops_b = d.humanize_ops(0, 0, u64::MAX, 10, 8);
    let ticks_a: Vec<u64> = ops_a
        .iter()
        .map(|o| match o {
            Op::UpdateEvent { after, .. } => after.tick,
            _ => 0,
        })
        .collect();
    let ticks_b: Vec<u64> = ops_b
        .iter()
        .map(|o| match o {
            Op::UpdateEvent { after, .. } => after.tick,
            _ => 0,
        })
        .collect();
    assert_eq!(ticks_a, ticks_b);
    apply(&mut d, ops_a);
    for n in notes_on(&d, 0) {
        assert!(
            n.end_tick.unwrap() - n.start_tick == 480,
            "length preserved"
        );
    }
}

#[test]
fn legato_extends_same_key_only() {
    // 60 at 0..240, 64 at 480..600, 60 at 720..800:
    // key60 first note → end moves to 720; key64 untouched (next same-key)
    let mut d = doc(vec![vec![
        chan(0, 0x90, 60, 100),
        chan(240, 0x80, 60, 0),
        chan(480, 0x90, 64, 100),
        chan(600, 0x80, 64, 0),
        chan(720, 0x90, 60, 100),
        chan(800, 0x80, 60, 0),
    ]]);
    let ops = d.legato_ops(0, 0, u64::MAX);
    apply(&mut d, ops);
    let ns = notes_on(&d, 0);
    let k60: Vec<&Note> = ns.iter().filter(|n| n.key == 60).collect();
    assert_eq!(k60[0].end_tick, Some(720));
    assert_eq!(ns.iter().find(|n| n.key == 64).unwrap().end_tick, Some(600));
}

#[test]
fn set_length_and_velocity() {
    let mut d = doc(vec![vec![chan(0, 0x90, 60, 100), chan(480, 0x80, 60, 0)]]);
    let ops = d.set_length_ops(0, 0, u64::MAX, 120);
    apply(&mut d, ops);
    let ops = d.set_velocity_ops(0, 0, u64::MAX, 64);
    apply(&mut d, ops);
    let n = &notes_on(&d, 0)[0];
    assert_eq!(n.end_tick, Some(120));
    assert_eq!(n.vel, 64);
}

// ---- regressions: edits must survive save + reload ----
// The writer prefers an event's raw_body over its kind, so any edit that
// changes `kind` must also drop raw_body — otherwise the edit shows in the
// UI but the saved file re-emits the original bytes.

/// format-1 file with a conductor tempo and one named, two-note track,
/// parsed strictly so every event carries a raw_body.
fn parsed_doc() -> Document {
    let mut f = Vec::new();
    f.extend_from_slice(b"MThd\x00\x00\x00\x06\x00\x01\x00\x02\x01\xE0");
    let t0 = [
        0x00, 0xFF, 0x51, 0x03, 0x07, 0xA1, 0x20, // tempo 120bpm
        0x00, 0xFF, 0x2F, 0x00,
    ];
    let t1 = [
        0x00, 0xFF, 0x03, 0x04, b'L', b'e', b'a', b'd', // track name
        0x00, 0x90, 0x3C, 0x64, // note on C4 vel 100
        0x60, 0x3C, 0x00, // running-status note off
        0x00, 0x90, 0x40, 0x40, // note on E4 vel 64
        0x60, 0x40, 0x00, 0x00, 0xFF, 0x2F, 0x00,
    ];
    for t in [&t0[..], &t1[..]] {
        f.extend_from_slice(b"MTrk");
        f.extend_from_slice(&(t.len() as u32).to_be_bytes());
        f.extend_from_slice(t);
    }
    Document::from_file(smf_core::parse(&f).unwrap())
}

fn save_reload(d: &Document) -> Document {
    let bytes = d.serialize(smf_core::WriteOptions::default());
    Document::from_file(smf_core::parse(&bytes).unwrap())
}

#[test]
fn kind_edits_survive_save_reload() {
    // transpose + set_velocity + set_channel all rewrite kind data
    let mut d = parsed_doc();
    assert!(d.tracks[1].events.iter().all(|e| e.raw_body.is_some()));

    let ops = d.transpose_ops(1, 0, u64::MAX, 12);
    apply(&mut d, ops);
    let ops = d.set_velocity_ops(1, 0, u64::MAX, 33);
    apply(&mut d, ops);
    let ops = d.set_channel_ops(1, 0, u64::MAX, 5);
    apply(&mut d, ops);

    let re = save_reload(&d);
    let mut keys: Vec<u8> = notes_on(&re, 1).iter().map(|n| n.key).collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![72, 76],
        "transposed keys must survive save+reload"
    );
    for n in notes_on(&re, 1) {
        assert_eq!(n.vel, 33, "edited velocity must survive save+reload");
        assert_eq!(n.channel, 5, "edited channel must survive save+reload");
    }
}

#[test]
fn tempo_and_name_replace_survive_save_reload() {
    let mut d = parsed_doc();
    let ops = d.set_tempo_ops(0, 0, 240.0); // replaces the tick-0 tempo
    apply(&mut d, ops);
    let ops = d.set_track_name_ops(1, "Bass");
    apply(&mut d, ops);

    let re = save_reload(&d);
    let mpq = re.tracks[0].events.iter().find_map(|e| match &e.kind {
        EventKind::Meta {
            meta_type: 0x51,
            data,
        } => Some(u32::from_be_bytes([0, data[0], data[1], data[2]])),
        _ => None,
    });
    assert_eq!(mpq, Some(250_000), "240bpm tempo must survive save+reload");
    assert_eq!(re.tracks[1].name.as_deref(), Some(b"Bass" as &[u8]));
}

#[test]
fn tick_only_edit_preserves_raw_body() {
    // a move that doesn't touch the kind keeps the verbatim body bytes —
    // only the delta VLQ is regenerated
    let mut d = parsed_doc();
    let ev = d.tracks[1].events[1].clone(); // note on with full-status body
    assert!(ev.raw_body.as_deref() == Some(&[0x90, 0x3C, 0x64][..]));
    let mut after = ev.clone();
    after.tick = 960;
    apply(
        &mut d,
        vec![Op::UpdateEvent {
            track: 1,
            before: ev,
            after,
        }],
    );
    let re = save_reload(&d);
    let moved = re.tracks[1].events.iter().find(|e| e.tick == 960).unwrap();
    assert_eq!(moved.raw_body.as_deref(), Some(&[0x90, 0x3C, 0x64][..]));
}

#[test]
fn apply_is_atomic_on_unknown_track() {
    let mut d = doc(vec![vec![chan(0, 0x90, 60, 100)]]);
    let rev = d.revision();
    let new_ev = Event {
        id: d.alloc_event_id(),
        tick: 100,
        seq: 0,
        raw_body: None,
        kind: EventKind::Channel {
            status: 0x90,
            data: [64, 90],
            len: 2,
        },
    };
    let tx = Transaction {
        label: "bad".into(),
        base: rev,
        ops: vec![
            Op::InsertEvents {
                track: 0,
                events: vec![new_ev.clone()],
            },
            Op::InsertEvents {
                track: 9,
                events: vec![new_ev.clone()],
            },
        ],
    };
    match d.apply(tx) {
        Err(ApplyError::UnknownTrack(9)) => {}
        other => panic!("expected UnknownTrack(9), got {other:?}"),
    }
    assert_eq!(d.revision(), rev, "failed apply must not bump the revision");
    assert_eq!(d.tracks[0].events.len(), 1, "op 1 must not be half-applied");
    // the id index stays consistent: a follow-up edit at the same base works
    apply(
        &mut d,
        vec![Op::InsertEvents {
            track: 0,
            events: vec![new_ev],
        }],
    );
    assert_eq!(d.tracks[0].events.len(), 2);
}

#[test]
fn duplicate_whole_track_range_does_not_overflow() {
    let mut d = doc(vec![vec![chan(100, 0x90, 60, 100)]]);
    let ops = d.duplicate_range_ops(0, 0, u64::MAX); // span = u64::MAX
    apply(&mut d, ops);
    assert_eq!(d.tracks[0].events.len(), 2, "saturates instead of wrapping");
}

#[test]
fn set_length_huge_ticks_saturates() {
    let mut d = doc(vec![vec![chan(0, 0x90, 60, 100), chan(480, 0x80, 60, 0)]]);
    let ops = d.set_length_ops(0, 0, u64::MAX, u64::MAX);
    apply(&mut d, ops);
    assert_eq!(notes_on(&d, 0)[0].end_tick, Some(u64::MAX));
}

#[test]
fn smpte_tempo_map_uses_frames_not_ppq() {
    // 30fps * 100 tpf = 3000 ticks/s → 3000 ticks = 1s
    let mut d = doc(vec![vec![]]);
    d.division = Division::Smpte {
        fps: 30,
        ticks_per_frame: 100,
    };
    d.tempo_map = TempoMap::build(&d.tracks, d.division);
    assert_eq!(d.tempo_map.tick_to_us(3000), 1_000_000);
    assert_eq!(d.tempo_map.tick_to_us(1500), 500_000);
    assert_eq!(d.tempo_map.us_to_tick(1_000_000), 3000);
}

// ---- SMPTE UI timing: every frame rate the SMF spec defines ----

/// A real SMPTE-division SMF file as parsed bytes — fixture coverage for
/// 24/25/29.97(-29)/30 fps, all representable in the format.
fn smpte_doc(fps: u8, tpf: u8) -> Document {
    let track = smf_core::Track {
        // write() appends End-of-Track itself
        events: vec![chan(0, 0x90, 60, 100), chan(10_000, 0x80, 60, 40)],
    };
    let bytes = smf_core::write(
        1,
        Division::Smpte {
            fps,
            ticks_per_frame: tpf,
        },
        &[track],
        smf_core::WriteOptions::default(),
    );
    let f = smf_core::parse(&bytes).unwrap();
    assert_eq!(
        f.division,
        Division::Smpte {
            fps,
            ticks_per_frame: tpf
        },
        "fixture must actually carry the SMPTE division"
    );
    Document::from_file(f)
}

#[test]
fn smpte_files_report_timecode_positions_not_fake_bars() {
    for (fps, tpf) in [(24u8, 100u8), (25, 40), (29, 100), (30, 100)] {
        let d = smpte_doc(fps, tpf);
        let td = d.time_display();
        assert!(td.is_smpte(), "fps {fps} must be a SMPTE UI timing mode");
        assert_eq!(d.tempo_map.ppq(), None, "SMPTE has no quarter note");
        // the coarse grid is one *displayed* second (nominal frames for
        // the -29 drop division), never 4*480 invented beats — and the
        // position label at that boundary is a round timecode second
        let sec = td.bar_ticks();
        assert_eq!(sec, td.snap_base_ticks(), "fps {fps}");
        assert_eq!(td.format_tick(sec), "00:00:01.00", "fps {fps}");
        // round trip keeps the division and event ticks byte-exact
        let bytes = d.serialize(smf_core::WriteOptions::default());
        let re = smf_core::parse(&bytes).unwrap();
        assert_eq!(
            re.division,
            Division::Smpte {
                fps,
                ticks_per_frame: tpf
            },
            "fps {fps} division must round-trip"
        );
        assert_eq!(re.tracks[0].events.len(), 3);
    }
}

/// A format-2 document: each track is an independent sequence with its
/// own tempo map — built via real SMF bytes so the header flag survives.
fn seq_doc(tracks: Vec<Vec<smf_core::Event>>) -> Document {
    let bytes = smf_core::write(
        2,
        Division::Metrical(480),
        &tracks
            .into_iter()
            .map(|events| smf_core::Track { events })
            .collect::<Vec<_>>(),
        smf_core::WriteOptions::default(),
    );
    let f = smf_core::parse(&bytes).unwrap();
    assert_eq!(f.format, 2, "fixture must actually be format 2");
    Document::from_file(f)
}

fn tempo(tick: u64, mpq: u32) -> smf_core::Event {
    smf_core::Event {
        tick,
        seq: 0,
        raw_body: None,
        kind: EventKind::Meta {
            meta_type: 0x51,
            data: Bytes::copy_from_slice(&mpq.to_be_bytes()[1..]),
        },
    }
}

#[test]
fn smpte_time_and_positions_round_trip() {
    // 25fps * 40tpf = 1000 ticks/s — the classic PAL fixture
    let d = smpte_doc(25, 40);
    assert_eq!(d.tempo_map.tick_to_us(1000), 1_000_000);
    assert_eq!(d.tempo_map.us_to_tick(1_500_000), 1500);
    let td = d.time_display();
    assert_eq!(td.format_tick(0), "00:00:00.00");
    assert_eq!(td.format_tick(1000), "00:00:01.00");
    // 308 frames = 12s + 8 frames; +20 ticks of sub-frame remainder
    assert_eq!(td.format_tick(12_320), "00:00:12.08");
    assert_eq!(td.format_tick(12_340), "00:00:12.08+20");
    // quantize/small-step quanta are the frame, not a 16th of 480
    assert_eq!(td.min_grid_ticks(), 40);
    assert_eq!(td.nudge_ticks(), 40);
}

#[test]
fn smpte_drop_frame_boundary_from_file() {
    // -29 division = 29.97 drop-frame; numbering verified against the
    // file-level TimeDisplay the UI renders
    let d = smpte_doc(29, 100);
    let td = d.time_display();
    assert_eq!(td.badge(), "29.97df");
    assert_eq!(td.format_tick(179_900), "00:00:59.29");
    assert_eq!(td.format_tick(180_000), "00:01:00.02");
    assert_eq!(td.format_tick(1_798_200), "00:10:00.00");
}

#[test]
fn format2_is_detected_and_roundtrips() {
    let d = seq_doc(vec![
        vec![
            tempo(0, 500_000),
            chan(0, 0x90, 60, 100),
            chan(480, 0x80, 60, 0),
        ],
        vec![
            tempo(0, 250_000),
            chan(0, 0x90, 64, 100),
            chan(960, 0x80, 64, 0),
        ],
    ]);
    assert!(d.is_sequential());
    assert_eq!(d.tracks.len(), 2);
    // serialize preserves the header format and each sequence's events
    let bytes = d.serialize(smf_core::WriteOptions::default());
    let re = smf_core::parse(&bytes).unwrap();
    assert_eq!(re.format, 2);
    assert_eq!(re.tracks.len(), 2);
    let t0_ticks: Vec<u64> = re.tracks[0].events.iter().map(|e| e.tick).collect();
    assert_eq!(
        t0_ticks,
        vec![0, 0, 480, 480],
        "sequence A: tempo,on,off,eot"
    );
}

#[test]
fn format2_sequences_have_independent_durations() {
    let d = seq_doc(vec![
        vec![chan(0, 0x90, 60, 100), chan(480, 0x80, 60, 0)],
        vec![
            chan(0, 0x90, 64, 100),
            chan(1920, 0x80, 64, 0),
            chan(1920, 0x90, 65, 100),
            chan(2880, 0x80, 65, 0),
        ],
    ]);
    assert_eq!(d.track_end_tick(0), 480);
    assert_eq!(d.track_end_tick(1), 2880);
    assert_eq!(d.track_end_tick(9), 0);
}

#[test]
fn format2_tempo_maps_are_per_sequence() {
    // seq A at 120bpm (500000µs/q), seq B at 240bpm — a quarter of B must
    // take half the wall time of a quarter of A, never A's tempo
    let d = seq_doc(vec![
        vec![tempo(0, 500_000), chan(0, 0x90, 60, 100)],
        vec![tempo(0, 250_000), chan(0, 0x90, 64, 100)],
    ]);
    assert_eq!(d.tempo_map_for(0).tick_to_us(480), 500_000);
    assert_eq!(d.tempo_map_for(1).tick_to_us(480), 250_000);
    // non-sequential docs keep the shared conductor map for every track
    let flat = doc(vec![vec![tempo(0, 500_000)], vec![chan(0, 0x90, 60, 100)]]);
    assert!(!flat.is_sequential());
    assert_eq!(flat.tempo_map_for(1).tick_to_us(480), 500_000);
}

#[test]
fn format2_playback_timeline_uses_each_sequence_tempo() {
    // one note at tick 480 in each sequence: seq A (120bpm) sounds it at
    // 500ms, seq B (240bpm) at 250ms — the tag lets the app pick ONE
    // sequence to play rather than all concurrently
    let d = seq_doc(vec![
        vec![tempo(0, 500_000), chan(480, 0x90, 60, 100)],
        vec![tempo(0, 250_000), chan(480, 0x90, 64, 100)],
    ]);
    let tl = d.timeline_tagged();
    let us_of = |track: usize| tl.iter().find(|e| e.1 == track).unwrap().0;
    assert_eq!(us_of(0), 500_000);
    assert_eq!(us_of(1), 250_000);
}

#[test]
fn format2_edits_stay_inside_their_sequence() {
    // edits are ordinary track-scoped transactions: quantizing sequence B
    // must not touch sequence A's events or its timeline
    let mut d = seq_doc(vec![
        vec![tempo(0, 500_000), chan(490, 0x90, 60, 100)],
        vec![tempo(0, 250_000), chan(490, 0x90, 64, 100)],
    ]);
    let ops = d.quantize_ops(1, 0, u64::MAX, 480, 100);
    apply(&mut d, ops);
    assert_eq!(d.tracks[0].events[1].tick, 490, "sequence A untouched");
    assert_eq!(d.tracks[1].events[1].tick, 480);
    let bytes = d.serialize(smf_core::WriteOptions::default());
    let re = smf_core::parse(&bytes).unwrap();
    assert_eq!(re.format, 2, "edits never demote format 2");
}

// ---- note-pairing corpus (#23): deterministic LIFO per (channel,key),
// overlapping ons are diagnosed, never silently normalized away ----

#[test]
fn pairing_is_lifo_for_overlapping_ons() {
    // on@0, on@100, off@200, off@300 — the NEWEST on takes the FIRST off
    let d = doc(vec![vec![
        chan(0, 0x90, 60, 100),
        chan(100, 0x90, 60, 80),
        chan(200, 0x80, 60, 0),
        chan(300, 0x80, 60, 0),
    ]]);
    let ns = notes_on(&d, 0);
    assert_eq!(ns.len(), 2);
    assert_eq!(
        (ns[0].start_tick, ns[0].end_tick, ns[0].vel),
        (0, Some(300), 100),
        "older on pairs with the second off"
    );
    assert_eq!(
        (ns[1].start_tick, ns[1].end_tick, ns[1].vel),
        (100, Some(200), 80),
        "newest on takes the first off (LIFO)"
    );
}

#[test]
fn overlapping_noteon_is_diagnosed_at_stable_event() {
    let d = doc(vec![vec![
        chan(0, 0x90, 60, 100),
        chan(100, 0x90, 60, 80),
        chan(200, 0x80, 60, 0),
        chan(300, 0x80, 60, 0),
    ]]);
    let overlaps: Vec<_> = d
        .diagnose()
        .into_iter()
        .filter(|d| d.code == "overlapping-noteon")
        .collect();
    assert_eq!(overlaps.len(), 1, "retrigger flagged exactly once");
    assert_eq!(overlaps[0].tick, 100);
    // the diag points at the retriggering on's stable EventId
    assert_eq!(overlaps[0].event, Some(d.tracks[0].events[1].id));
    assert!(overlaps[0].detail.contains("60"));
}

#[test]
fn overlaps_are_scoped_per_channel_and_key() {
    // same key on a different channel + a different key on the same
    // channel — neither lane overlaps
    let d = doc(vec![vec![
        chan(0, 0x90, 60, 100),
        chan(50, 0x91, 60, 90), // ch1 key60 — different channel
        chan(60, 0x90, 64, 90), // ch0 key64 — different key
        chan(200, 0x80, 60, 0),
        chan(200, 0x81, 60, 0),
        chan(200, 0x80, 64, 0),
    ]]);
    assert!(
        d.diagnose().iter().all(|d| d.code != "overlapping-noteon"),
        "cross-channel/cross-key ons must not be flagged"
    );
    assert_eq!(notes_on(&d, 0).len(), 3);
    assert!(notes_on(&d, 0).iter().all(|n| n.end_tick.is_some()));
}

#[test]
fn stacked_duplicates_diagnose_each_extra_on() {
    // three stacked ons, one off → two overlap diags, one paired note,
    // two dangling ons — every byte survives in the model
    let d = doc(vec![vec![
        chan(0, 0x90, 60, 100),
        chan(50, 0x90, 60, 90),
        chan(60, 0x90, 60, 80),
        chan(200, 0x80, 60, 0),
    ]]);
    let diags = d.diagnose();
    assert_eq!(
        diags
            .iter()
            .filter(|d| d.code == "overlapping-noteon")
            .count(),
        2,
        "2nd and 3rd stacked ons each flag"
    );
    assert_eq!(
        diags.iter().filter(|d| d.code == "dangling-noteon").count(),
        2
    );
    let ns = notes_on(&d, 0);
    assert_eq!(ns.len(), 3);
    let paired = ns.iter().find(|n| n.end_tick.is_some()).unwrap();
    assert_eq!(paired.vel, 80, "newest on pairs with the off");
}

#[test]
fn same_tick_on_off_pairs_as_zero_length() {
    let d = doc(vec![vec![
        chan(100, 0x90, 60, 100),
        chan(100, 0x80, 60, 0),
        chan(200, 0x90, 64, 90),
        chan(400, 0x80, 64, 0),
    ]]);
    let n = &notes_on(&d, 0)[0];
    assert_eq!((n.start_tick, n.end_tick), (100, Some(100)));
    assert!(d
        .diagnose()
        .iter()
        .any(|d| d.code == "zero-length-note" && d.tick == 100));
}

#[test]
fn sustain_pedal_does_not_affect_pairing() {
    // CC64 changes playback sustain but not the on/off pairing rule —
    // a retrigger under pedal is still an overlap
    let d = doc(vec![vec![
        chan(0, 0xB0, 64, 127),
        chan(0, 0x90, 60, 100),
        chan(100, 0x90, 60, 80),
        chan(150, 0xB0, 64, 0),
        chan(200, 0x80, 60, 0),
        chan(300, 0x80, 60, 0),
    ]]);
    assert_eq!(
        d.diagnose()
            .iter()
            .filter(|d| d.code == "overlapping-noteon")
            .count(),
        1
    );
    assert_eq!(notes_on(&d, 0).len(), 2);
}

#[test]
fn overlapping_ons_are_preserved_not_normalized() {
    // "fix all" resolves structural findings but MUST NOT delete
    // ambiguous performance data — the overlap stays, diagnosed
    let mut d = doc(vec![vec![
        chan(0, 0x90, 60, 100),
        chan(100, 0x90, 60, 80),
        chan(200, 0x80, 60, 0),
        chan(300, 0x80, 60, 0),
    ]]);
    let ops = d.fix_ops(&[]);
    apply(&mut d, ops);
    assert!(
        d.diagnose().iter().any(|d| d.code == "overlapping-noteon"),
        "overlap diag survives normalize"
    );
    assert_eq!(
        d.tracks[0]
            .events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Channel { .. }))
            .count(),
        4,
        "every channel event preserved"
    );
    // round-trip: the ambiguous bytes are in the file, not just the model
    let bytes = d.serialize(smf_core::WriteOptions {
        running_status: false,
    });
    let d2 = Document::from_file(smf_core::parse(&bytes).unwrap());
    assert_eq!(notes_on(&d2, 0).len(), 2);
    assert!(d2.diagnose().iter().any(|d| d.code == "overlapping-noteon"));
}

#[test]
fn channel_setup_collects_bank_and_program_before_tick() {
    let mut d = doc(vec![vec![
        chan(10, 0xB0, 0, 1),  // bank MSB = 1
        chan(20, 0xB0, 32, 5), // bank LSB = 5
        chan(30, 0xC0, 42, 0), // program 42
        chan(40, 0xB0, 0, 3),  // MSB overwritten -> 3
        chan(60, 0xB1, 0, 99), // different channel: must not leak in
        chan(60, 0xC1, 7, 0),
        chan(500, 0xC0, 99, 0), // after the query tick: ignored
    ]]);
    // MSB+LSB+PC in emit order
    assert_eq!(
        d.channel_setup(0, 0, 100),
        vec![vec![0xB0, 0, 3], vec![0xB0, 32, 5], vec![0xC0, 42]]
    );
    // before the PC/second-bank events only the first bank pair survives
    assert_eq!(
        d.channel_setup(0, 0, 25),
        vec![vec![0xB0, 0, 1], vec![0xB0, 32, 5]]
    );
    // channel with no setup state -> nothing prefixed
    assert!(d.channel_setup(0, 5, 100).is_empty());
    // before every event -> nothing
    assert!(d.channel_setup(0, 0, 5).is_empty());
    // missing track -> nothing
    assert!(d.channel_setup(9, 0, 100).is_empty());
    let _ = &mut d;
}
