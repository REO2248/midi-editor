// Semantic ops generators: the same functions drive GUI chips and MCP tools.
// Contract per generator: returns pure Vec<Op> (no doc mutation besides id
// minting); applying then reverting the transaction restores the document.
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
    let mut d = doc(vec![vec![
        chan(490, 0x90, 60, 100),
        chan(970, 0x80, 60, 0),
    ]]);
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
        EventKind::Meta { meta_type: 0x03, .. }
    ));
}

#[test]
fn scale_velocity_clamps() {
    let mut d = doc(vec![vec![
        chan(0, 0x90, 60, 100),
        chan(100, 0x90, 62, 10),
    ]]);
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
        EventKind::Meta { meta_type: 0x05, .. }
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
    let __ops = d.set_tempo_ops(0, 120.0);
    apply(&mut d, __ops);
    let n_tempos = |d: &Document| {
        d.tracks[0]
            .events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Meta { meta_type: 0x51, .. }))
            .count()
    };
    assert_eq!(n_tempos(&d), 1);
    let __ops = d.set_tempo_ops(0, 140.0); // same tick: replace
    apply(&mut d, __ops);
    assert_eq!(n_tempos(&d), 1);
    // 140bpm: 480 ticks = one quarter = 60e6/140 us
    assert_eq!(d.tempo_map.tick_to_us(480), 428_571);
    let __ops = d.set_tempo_ops(960, 60.0); // new tick: insert
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
    let __ops = d.set_time_sig_ops(0, 6, 8);
    apply(&mut d, __ops);
    match &d.tracks[0].events[0].kind {
        EventKind::Meta { meta_type: 0x58, data } => {
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
    let tx = apply(&mut d, vec![Op::UpdateEvent { track: 0, before: ev, after }]);
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
        assert!(n.end_tick.unwrap() - n.start_tick == 480, "length preserved");
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
    let mut d = doc(vec![vec![
        chan(0, 0x90, 60, 100),
        chan(480, 0x80, 60, 0),
    ]]);
    let ops = d.set_length_ops(0, 0, u64::MAX, 120);
    apply(&mut d, ops);
    let ops = d.set_velocity_ops(0, 0, u64::MAX, 64);
    apply(&mut d, ops);
    let n = &notes_on(&d, 0)[0];
    assert_eq!(n.end_tick, Some(120));
    assert_eq!(n.vel, 64);
}
