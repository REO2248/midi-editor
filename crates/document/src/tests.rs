use super::*;

fn doc_with_note() -> Document {
    let ev = smf_core::Event {
        tick: 480,
        seq: 0,
        raw_body: None,
        kind: EventKind::Channel {
            status: 0x90,
            data: [60, 100],
            len: 2,
        },
    };
    let f = smf_core::File {
        format: 1,
        division: Division::Metrical(480),
        tracks: vec![smf_core::Track { events: vec![ev] }],
        warnings: vec![],
    };
    Document::from_file(f)
}

#[test]
fn meta_text_ops_create_update_remove() {
    let mut d = doc_with_note();
    let apply = |d: &mut Document, ops: Vec<Op>| {
        let base = d.revision();
        d.apply(Transaction {
            label: "t".into(),
            base,
            ops,
        })
        .unwrap();
    };
    // create a marker at tick 240 — UTF-8 by default
    let ops = d.set_meta_text_ops(0, 240, 0x06, 0, "Verse", None);
    apply(&mut d, ops);
    let id = d.tracks[0]
        .events
        .iter()
        .find(|e| {
            matches!(
                e.kind,
                EventKind::Meta {
                    meta_type: 0x06,
                    ..
                }
            )
        })
        .unwrap()
        .id;
    // update by id — tick stays, only the payload is rewritten
    let ops = d.set_meta_text_ops(0, 240, 0x06, id, "Chorus", None);
    apply(&mut d, ops);
    let e = d.tracks[0].events.iter().find(|e| e.id == id).unwrap();
    assert_eq!(e.tick, 240);
    assert!(
        matches!(&e.kind, EventKind::Meta { meta_type: 0x06, data } if data.as_ref() == b"Chorus")
    );
    // explicit non-UTF-8 write encodes bytes, never mutates siblings
    let ops = d.set_meta_text_ops(
        0,
        480,
        0x05,
        0,
        "héllo",
        Some(smf_core::TextEncoding::Latin1),
    );
    apply(&mut d, ops);
    let ly = d.tracks[0]
        .events
        .iter()
        .find(|e| {
            matches!(
                e.kind,
                EventKind::Meta {
                    meta_type: 0x05,
                    ..
                }
            )
        })
        .unwrap();
    assert!(matches!(&ly.kind, EventKind::Meta { data, .. } if data.as_ref() == b"h\xE9llo"));
    // sjis encodes multibyte; utf8 round-trips through decode_text
    let sj = smf_core::encode_text("歌詞", smf_core::TextEncoding::ShiftJis);
    assert_eq!(
        smf_core::decode_text(&sj, Some(smf_core::TextEncoding::ShiftJis)),
        "歌詞"
    );
    // remove only the named event
    let n = d.tracks[0].events.len();
    let ops = d.remove_meta_ops(0, id);
    apply(&mut d, ops);
    assert_eq!(d.tracks[0].events.len(), n - 1);
    assert!(d.tracks[0].events.iter().all(|e| e.id != id));
    // key sig: insert then update-in-place
    let ops = d.set_key_sig_ops(0, -3, 1);
    apply(&mut d, ops);
    let ops = d.set_key_sig_ops(0, 2, 0);
    apply(&mut d, ops);
    let ks: Vec<_> = d.tracks[0]
        .events
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                EventKind::Meta {
                    meta_type: 0x59,
                    ..
                }
            )
        })
        .collect();
    assert_eq!(ks.len(), 1);
    assert!(matches!(&ks[0].kind, EventKind::Meta { data, .. } if data.as_ref() == [2u8, 0]));
}

#[test]
fn apply_checks_revision() {
    let mut d = doc_with_note();
    let bad = Transaction {
        label: "x".into(),
        base: 99,
        ops: vec![],
    };
    assert!(matches!(
        d.apply(bad),
        Err(ApplyError::StaleRevision { .. })
    ));
}

#[test]
fn notes_pairing_and_dangling() {
    // one paired note (on 480/off 960) + one dangling NoteOn at 1440
    let mk = |tick, status, d0, d1| smf_core::Event {
        tick,
        seq: 0,
        raw_body: None,
        kind: EventKind::Channel {
            status,
            data: [d0, d1],
            len: 2,
        },
    };
    let f = smf_core::File {
        format: 1,
        division: Division::Metrical(480),
        tracks: vec![smf_core::Track {
            events: vec![
                mk(480, 0x90, 60, 100),
                mk(960, 0x80, 60, 0),
                mk(1440, 0x91, 64, 90),
            ],
        }],
        warnings: vec![],
    };
    let d = Document::from_file(f);
    let notes = d.notes();
    assert_eq!(notes.len(), 2);
    let paired = notes.iter().find(|n| n.key == 60).unwrap();
    assert_eq!((paired.start_tick, paired.end_tick), (480, Some(960)));
    let dangling = notes.iter().find(|n| n.key == 64).unwrap();
    assert_eq!((dangling.channel, dangling.end_tick), (1, None));
}

#[test]
fn apply_and_revert() {
    let mut d = doc_with_note();
    let new_ev = Event {
        id: 999,
        tick: 0,
        seq: 0,
        raw_body: None,
        kind: EventKind::Channel {
            status: 0x90,
            data: [64, 90],
            len: 2,
        },
    };
    let tx = Transaction {
        label: "ins".into(),
        base: 0,
        ops: vec![Op::InsertEvents {
            track: 0,
            events: vec![new_ev],
        }],
    };
    let applied = d.apply(tx).unwrap();
    // inserted event + the structural End-of-Track the track gains
    assert_eq!(d.tracks[0].events.len(), 3);
    d.revert(&applied.tx);
    // undo removes the minted EOT too — exact pre-edit state
    assert_eq!(d.tracks[0].events.len(), 1);
}

#[test]
fn tempo_map_basic() {
    // 120bpm default, 480ppq: tick 480 -> 500000us
    let d = doc_with_note();
    assert_eq!(d.tempo_map.tick_to_us(480), 500_000);
}

#[test]
fn tempo_map_dense_ramp_keeps_fractional_us_exact() {
    // #224: a dense tempo grid (one event every 10 ticks) truncates <1µs
    // per segment when converting ticks to µs; over thousands of ramp
    // segments that accumulates into milliseconds of drift. The
    // fractional-remainder carry keeps every breakpoint at the exact
    // rational time (floor), never more than 1µs short.
    let step = 10u64;
    let count = 3000u64;
    let mpq = 100_000u32; // µs/quarter — 1041.666…µs per tick at 96ppq
    let events: Vec<smf_core::Event> = (0..=count)
        .map(|k| smf_core::Event {
            tick: k * step,
            seq: k as u32,
            raw_body: None,
            kind: EventKind::Meta {
                meta_type: 0x51,
                data: mpq.to_be_bytes()[1..].to_vec().into(),
            },
        })
        .collect();
    let f = smf_core::File {
        format: 1,
        division: Division::Metrical(96),
        tracks: vec![smf_core::Track { events }],
        warnings: vec![],
    };
    let d = Document::from_file(f);
    // last breakpoint: 30000 ticks * 100000µs / 96 = 31_250_000 exactly —
    // the truncating accumulator landed at 31_248_000 (2ms early)
    let last = d.tempo_map.points().last().unwrap();
    assert_eq!((last.0, last.2), (count * step, 31_250_000));
    // mid-ramp breakpoint off an exact µs boundary stays within 1µs;
    // per-segment truncation was already 1000µs behind here
    let mid = d.tempo_map.points()[1501];
    let exact = 1501 * step * mpq as u64;
    assert!((mid.2 as i128 - (exact / 96) as i128).abs() < 1);
    assert_eq!(d.tempo_map.tick_to_us(mid.0), mid.2);
}

#[test]
fn tempo_map_saturates_on_hostile_tick_deltas() {
    // VLQ-scaled deltas can push a segment's µs past u64; breakpoints
    // must pin at u64::MAX ("far future") instead of wrapping
    let mk = |tick: u64, seq: u32| smf_core::Event {
        tick,
        seq,
        raw_body: None,
        kind: EventKind::Meta {
            meta_type: 0x51,
            data: 500_000u32.to_be_bytes()[1..].to_vec().into(),
        },
    };
    let f = smf_core::File {
        format: 1,
        division: Division::Metrical(1),
        tracks: vec![smf_core::Track {
            events: vec![mk(0, 0), mk(u64::MAX / 2, 1), mk(u64::MAX, 2)],
        }],
        warnings: vec![],
    };
    let d = Document::from_file(f);
    let pts = d.tempo_map.points();
    assert_eq!(pts[0].2, 0);
    assert_eq!(pts[1].2, u64::MAX);
    assert_eq!(pts[2].2, u64::MAX);
}

#[test]
fn smpte_dropframe_runs_at_30000_over_1001() {
    // #211: the SMF -29 division is 29.97 drop-frame = 30000/1001 fps.
    // Integer `29 * ticks_per_frame` math undercounted the tick rate by
    // 3.24%, dragging playback proportionally slow (~2s per minute).
    let f = smf_core::File {
        format: 1,
        division: Division::Smpte {
            fps: 29,
            ticks_per_frame: 100,
        },
        tracks: vec![smf_core::Track { events: vec![] }],
        warnings: vec![],
    };
    let d = Document::from_file(f);
    // 2997 ticks = 2997 * 1001 / 3000 seconds-worth of µs: 999_999 —
    // the old 2900 tps rate put 2997 ticks a full 1_033_448µs out
    assert_eq!(d.tempo_map.tick_to_us(2997), 999_999);
    assert_eq!(d.tempo_map.us_to_tick(999_999), 2997);
    // one real second is 3000000/1001 = 2997.003 ticks — floor
    assert_eq!(d.tempo_map.us_to_tick(1_000_000), 2997);
    // 3M ticks = 30_000 frames = exactly 1001 wall-clock seconds at
    // 30000/1001 fps (the old integer rate claimed 1034.5s — 3.4% slow)
    assert_eq!(d.tempo_map.tick_to_us(3_000_000), 1_001_000_000);
}

#[test]
fn premature_eot_collapses_on_edit_and_undo_restores() {
    // imported file whose stored EOT sits before later content: the doc
    // keeps it verbatim until a transaction touches the track, then the
    // effective tx collapses it to one terminator last — and undo puts
    // the exact pre-edit structure back
    let is_eot = |e: &Event| {
        matches!(
            e.kind,
            EventKind::Meta {
                meta_type: 0x2F,
                ..
            }
        )
    };
    let mk = |tick: u64, seq: u32, eot: bool| smf_core::Event {
        tick,
        seq,
        raw_body: None,
        kind: if eot {
            EventKind::Meta {
                meta_type: 0x2F,
                data: Bytes::new(),
            }
        } else {
            EventKind::Channel {
                status: 0x90,
                data: [64, 90],
                len: 2,
            }
        },
    };
    let f = smf_core::File {
        format: 1,
        division: Division::Metrical(480),
        tracks: vec![smf_core::Track {
            events: vec![mk(0, 0, true), mk(480, 1, false), mk(960, 2, true)],
        }],
        warnings: vec![],
    };
    let mut d = Document::from_file(f);
    // untouched: both stored EOTs ride along verbatim
    assert_eq!(d.tracks[0].events.iter().filter(|e| is_eot(e)).count(), 2);

    let new_ev = Event {
        id: d.alloc_event_id(),
        tick: 1440,
        seq: 0,
        raw_body: None,
        kind: EventKind::Channel {
            status: 0x90,
            data: [65, 90],
            len: 2,
        },
    };
    let applied = d
        .apply(Transaction {
            label: "ins".into(),
            base: d.revision(),
            ops: vec![Op::InsertEvents {
                track: 0,
                events: vec![new_ev],
            }],
        })
        .unwrap();
    // the touched track carries exactly one EOT, sorted last at the new end
    let eots: Vec<&Event> = d.tracks[0].events.iter().filter(|e| is_eot(e)).collect();
    assert_eq!(eots.len(), 1);
    let last = d.tracks[0]
        .events
        .iter()
        .max_by_key(|e| (e.tick, e.seq))
        .unwrap();
    assert!(is_eot(last));
    assert_eq!(last.tick, 1440, "terminator reticked past new content");

    d.revert(&applied.tx);
    // exact pre-edit structure: both stored EOTs back at their ticks
    let t = &d.tracks[0];
    assert_eq!(t.events.len(), 3);
    let eot_ticks: Vec<u64> = t
        .events
        .iter()
        .filter(|e| is_eot(e))
        .map(|e| e.tick)
        .collect();
    assert_eq!(eot_ticks, vec![0, 960]);
}

#[test]
fn undo_restores_event_order_among_same_tick_events() {
    // position-exact undo: an event whose (tick, seq) collides with
    // siblings must come back at its original index — a re-sort lands it
    // at an arbitrary slot among the equal keys and reorders the bytes
    let is_eot = |e: &Event| {
        matches!(
            e.kind,
            EventKind::Meta {
                meta_type: 0x2F,
                ..
            }
        )
    };
    let f = smf_core::File {
        format: 1,
        division: Division::Metrical(480),
        tracks: vec![smf_core::Track {
            events: vec![smf_core::Event {
                tick: 0,
                seq: 0,
                raw_body: None,
                kind: EventKind::Meta {
                    meta_type: 0x2F,
                    data: Bytes::new(),
                },
            }],
        }],
        warnings: vec![],
    };
    let mut d = Document::from_file(f);
    fn ins(d: &mut Document, tick: u64, key: u8) -> Applied {
        let ev = Event {
            id: d.alloc_event_id(),
            tick,
            seq: u32::MAX,
            raw_body: None,
            kind: EventKind::Channel {
                status: 0x90,
                data: [key, 100],
                len: 2,
            },
        };
        d.apply(Transaction {
            label: "ins".into(),
            base: d.revision(),
            ops: vec![Op::InsertEvents {
                track: 0,
                events: vec![ev],
            }],
        })
        .unwrap()
    }
    ins(&mut d, 0, 60);
    ins(&mut d, 0, 62); // the EOT now shares (0, MAX) with the notes
    ins(&mut d, 0, 64);
    let before = d.serialize(smf_core::WriteOptions::default());
    let eot_pos = d.tracks[0].events.iter().position(is_eot).unwrap();

    // an edit past the track's end reticks the EOT — the synthesized
    // UpdateEvent must round-trip back to the exact same slot
    let applied = ins(&mut d, 3347, 65);
    d.revert(&applied.tx);
    assert_eq!(d.serialize(smf_core::WriteOptions::default()), before);
    assert_eq!(
        d.tracks[0].events.iter().position(is_eot).unwrap(),
        eot_pos,
        "EOT must return to its original slot among the same-key events"
    );
}

// ---- chase_events ----

fn chase_doc(events: Vec<smf_core::Event>) -> Document {
    let f = smf_core::File {
        format: 1,
        division: Division::Metrical(480),
        tracks: vec![smf_core::Track { events }],
        warnings: vec![],
    };
    Document::from_file(f)
}

fn ev(tick: u64, status: u8, d0: u8, d1: u8) -> smf_core::Event {
    smf_core::Event {
        tick,
        seq: 0,
        raw_body: None,
        kind: EventKind::Channel {
            status,
            data: [d0, d1],
            len: if matches!(status & 0xF0, 0xC0 | 0xD0) {
                1
            } else {
                2
            },
        },
    }
}

fn bytes_of(chase: &[(u64, usize, Vec<u8>)]) -> Vec<Vec<u8>> {
    chase.iter().map(|(_, _, b)| b.clone()).collect()
}

#[test]
fn chase_at_zero_is_empty() {
    let d = chase_doc(vec![ev(0, 0x90, 60, 100)]);
    assert!(d.chase_events(0).is_empty());
}

#[test]
fn chase_full_state_held_and_sustained_notes() {
    let d = chase_doc(vec![
        ev(0, 0xB0, 0, 1),     // bank MSB
        ev(1, 0xB0, 32, 2),    // bank LSB
        ev(2, 0xC0, 5, 0),     // program 5
        ev(10, 0xB0, 7, 100),  // volume
        ev(20, 0xB0, 7, 90),   // later volume wins
        ev(30, 0xE0, 3, 64),   // pitch bend
        ev(15, 0xA0, 60, 40),  // poly AT
        ev(16, 0xD0, 55, 0),   // channel AT
        ev(40, 0xB0, 64, 127), // pedal down
        ev(100, 0x90, 60, 100),
        ev(200, 0x90, 64, 80),
        ev(300, 0x80, 60, 0),  // 60 released under pedal -> sustained
        ev(500, 0x90, 72, 70), // still held at the chase point
    ]);
    let chase = d.chase_events(d.tempo_map.tick_to_us(720));
    assert!(chase
        .iter()
        .all(|&(us, tr, _)| us == d.tempo_map.tick_to_us(720) && tr == 0));
    assert_eq!(
        bytes_of(&chase),
        vec![
            vec![0xB0, 0, 1],    // bank MSB before PC
            vec![0xB0, 32, 2],   // bank LSB
            vec![0xC0, 5],       // program
            vec![0xB0, 7, 90],   // CC last value
            vec![0xB0, 64, 127], // pedal down before the pairs below
            vec![0xE0, 3, 64],   // bend
            vec![0xD0, 55],      // channel AT
            vec![0xA0, 60, 40],  // poly AT
            vec![0x90, 64, 80],  // held notes (ascending key)
            vec![0x90, 72, 70],
            vec![0x90, 60, 100], // sustained note re-struck on+off
            vec![0x80, 60, 0],
        ]
    );
}

#[test]
fn chase_pedal_up_clears_sustained() {
    let d = chase_doc(vec![
        ev(40, 0xB0, 64, 127),
        ev(100, 0x90, 60, 100),
        ev(200, 0x80, 60, 0),
        ev(600, 0xB0, 64, 0),
    ]);
    let chase = d.chase_events(d.tempo_map.tick_to_us(720));
    assert_eq!(bytes_of(&chase), vec![vec![0xB0, 64, 0]]);
}

#[test]
fn chase_cc121_clears_controllers_but_not_bank_program() {
    let d = chase_doc(vec![
        ev(0, 0xB0, 0, 1),
        ev(1, 0xB0, 32, 2),
        ev(2, 0xC0, 5, 0),
        ev(10, 0xB0, 7, 90),
        ev(30, 0xE0, 3, 64),
        ev(600, 0xB0, 121, 0), // reset all controllers
    ]);
    let chase = d.chase_events(d.tempo_map.tick_to_us(720));
    assert_eq!(
        bytes_of(&chase),
        vec![vec![0xB0, 0, 1], vec![0xB0, 32, 2], vec![0xC0, 5]]
    );
}

#[test]
fn chase_mode_messages_drop_notes_and_are_not_chased() {
    // all-notes-off releases held notes (no pedal): nothing to restrike
    let d = chase_doc(vec![ev(100, 0x90, 60, 100), ev(600, 0xB0, 123, 0)]);
    assert!(d.chase_events(d.tempo_map.tick_to_us(720)).is_empty());
    // all-sound-off kills even pedal-caught notes
    let d = chase_doc(vec![
        ev(10, 0xB0, 64, 127),
        ev(100, 0x90, 60, 100),
        ev(200, 0x80, 60, 0),
        ev(600, 0xB0, 120, 0),
    ]);
    assert_eq!(
        bytes_of(&d.chase_events(d.tempo_map.tick_to_us(720))),
        vec![vec![0xB0, 64, 127]]
    );
}

#[test]
fn chase_rpn_nrpn_selector_then_data() {
    let d = chase_doc(vec![
        ev(10, 0xB0, 101, 0), // RPN 0,0 (pitch bend sensitivity)
        ev(11, 0xB0, 100, 0),
        ev(12, 0xB0, 6, 2),  // data MSB
        ev(20, 0xB0, 99, 1), // switch to NRPN 1,3
        ev(21, 0xB0, 98, 3),
        ev(22, 0xB0, 38, 5), // data LSB
    ]);
    assert_eq!(
        bytes_of(&d.chase_events(d.tempo_map.tick_to_us(720))),
        vec![
            vec![0xB0, 99, 1], // NRPN msb first, then lsb...
            vec![0xB0, 98, 3],
            vec![0xB0, 6, 2], // ...then data entry
            vec![0xB0, 38, 5],
        ]
    );
    // all-zero RPN (the common case) must still be chased
    let d = chase_doc(vec![ev(10, 0xB0, 101, 0), ev(11, 0xB0, 100, 0)]);
    assert_eq!(
        bytes_of(&d.chase_events(d.tempo_map.tick_to_us(720))),
        vec![vec![0xB0, 101, 0], vec![0xB0, 100, 0]]
    );
}

#[test]
fn chase_excludes_events_at_the_play_position() {
    let d = chase_doc(vec![
        ev(10, 0xB0, 7, 100),
        ev(30, 0xE0, 3, 64), // exactly at the start: plays as a real event
    ]);
    let start = d.tempo_map.tick_to_us(30);
    let chase = d.chase_events(start);
    assert_eq!(bytes_of(&chase), vec![vec![0xB0, 7, 100]]);
}

#[test]
fn chase_is_per_track() {
    let f = smf_core::File {
        format: 1,
        division: Division::Metrical(480),
        tracks: vec![
            smf_core::Track {
                events: vec![ev(10, 0xB0, 7, 10)],
            },
            smf_core::Track {
                events: vec![ev(10, 0xB0, 7, 20)],
            },
        ],
        warnings: vec![],
    };
    let d = Document::from_file(f);
    let chase = d.chase_events(d.tempo_map.tick_to_us(720));
    assert_eq!(
        chase,
        vec![
            (d.tempo_map.tick_to_us(720), 0, vec![0xB0, 7, 10]),
            (d.tempo_map.tick_to_us(720), 1, vec![0xB0, 7, 20]),
        ]
    );
}

#[test]
fn chase_dangling_noteon_is_held() {
    let d = chase_doc(vec![ev(100, 0x91, 64, 90)]);
    assert_eq!(
        bytes_of(&d.chase_events(d.tempo_map.tick_to_us(720))),
        vec![vec![0x91, 64, 90]]
    );
}

// ---- SysEx timeline / chase ----

fn sx(tick: u64, payload: &[u8]) -> smf_core::Event {
    smf_core::Event {
        tick,
        seq: 0,
        raw_body: None,
        kind: EventKind::SysEx(Bytes::copy_from_slice(payload)),
    }
}

fn esc(tick: u64, payload: &[u8]) -> smf_core::Event {
    smf_core::Event {
        tick,
        seq: 0,
        raw_body: None,
        kind: EventKind::Escape(Bytes::copy_from_slice(payload)),
    }
}

#[test]
fn sysex_complete_and_split_messages_join() {
    // one complete message + one split across an F0 and two F7 escapes,
    // with a channel event interleaved between the fragments
    let d = chase_doc(vec![
        sx(0, &[0x7E, 0x7F, 0x09, 0x01, 0xF7]), // GM system on
        sx(100, &[0x41, 0x10, 0x42]),           // split head, no F7
        ev(110, 0x90, 60, 100),                 // interleaved event
        esc(120, &[0x12, 0x40]),                // continuation
        esc(130, &[0x00, 0xF7]),                // final fragment
    ]);
    let t0 = d.tempo_map.tick_to_us(0);
    let t100 = d.tempo_map.tick_to_us(100);
    assert_eq!(
        d.timeline_sysex(),
        vec![
            (t0, 0, vec![0xF0, 0x7E, 0x7F, 0x09, 0x01, 0xF7]),
            (
                t100,
                0,
                vec![0xF0, 0x41, 0x10, 0x42, 0x12, 0x40, 0x00, 0xF7]
            ),
        ]
    );
}

#[test]
fn sysex_standalone_escapes_and_open_messages() {
    // standalone escapes carry arbitrary bytes — never sent
    let d = chase_doc(vec![
        esc(10, &[0x01, 0x02]),
        sx(20, &[0x7E, 0x7F]),                   // never terminated
        sx(30, &[0x7E, 0x7F, 0x09, 0x01, 0xF7]), // new F0 closes it
    ]);
    let t20 = d.tempo_map.tick_to_us(20);
    let t30 = d.tempo_map.tick_to_us(30);
    assert_eq!(
        d.timeline_sysex(),
        vec![
            (t20, 0, vec![0xF0, 0x7E, 0x7F, 0xF7]), // closed with F7
            (t30, 0, vec![0xF0, 0x7E, 0x7F, 0x09, 0x01, 0xF7]),
        ]
    );
    // open at end of track is closed too
    let d = chase_doc(vec![sx(20, &[0x41, 0x10])]);
    assert_eq!(
        d.timeline_sysex(),
        vec![(d.tempo_map.tick_to_us(20), 0, vec![0xF0, 0x41, 0x10, 0xF7])]
    );
}

#[test]
fn chase_sysex_picks_last_complete_per_track() {
    let d = chase_doc(vec![
        sx(0, &[0x7E, 0x7F, 0x09, 0x01, 0xF7]),   // GM on
        sx(100, &[0x41, 0x10, 0x12, 0x00, 0xF7]), // later message wins
        sx(200, &[0x41, 0x10, 0x40]),             // incomplete at the boundary: skipped
    ]);
    let start = d.tempo_map.tick_to_us(720);
    assert_eq!(
        d.chase_sysex(start),
        vec![(start, 0, vec![0xF0, 0x41, 0x10, 0x12, 0x00, 0xF7])]
    );
    // before any message: nothing
    assert!(d.chase_sysex(d.tempo_map.tick_to_us(0)).is_empty());
}

#[test]
fn aftertouch_ops_edit_and_chase() {
    let mut d = doc_with_note();
    // insert a channel-pressure point and a poly-pressure point through the
    // same ops the lane UI / MCP tools use
    let base = d.revision();
    let mut ops = d.set_channel_pressure_ops(0, 240, 0, 90);
    ops.extend(d.set_poly_pressure_ops(0, 480, 0, 60, 70));
    d.apply(Transaction {
        label: "aftertouch".into(),
        base,
        ops,
    })
    .unwrap();
    // inserted bytes are well-formed: 1-byte 0xD0, 2-byte 0xA0
    assert!(d.tracks[0].events.iter().any(|e| matches!(
        &e.kind,
        EventKind::Channel { status: 0xD0, data, len: 1 } if data[0] == 90
    )));
    assert!(d.tracks[0].events.iter().any(|e| matches!(
        &e.kind,
        EventKind::Channel { status: 0xA0, data, len: 2 } if data[0] == 60 && data[1] == 70
    )));
    // chased state mid-song agrees with the edited values
    let chased = d.chase_events(d.tempo_map.tick_to_us(1440) - 1);
    let bytes = bytes_of(&chased);
    assert!(bytes.contains(&vec![0xD0, 90]));
    assert!(bytes.contains(&vec![0xA0, 60, 70]));

    // remove_events_ops deletes by id
    let ids: Vec<EventId> = d.tracks[0]
        .events
        .iter()
        .filter(|e| {
            matches!(
                &e.kind,
                EventKind::Channel { status, .. } if status & 0xF0 == 0xD0
                    || status & 0xF0 == 0xA0
            )
        })
        .map(|e| e.id)
        .collect();
    assert_eq!(ids.len(), 2);
    let ops = d.remove_events_ops(&ids);
    let base = d.revision();
    d.apply(Transaction {
        label: "rm".into(),
        base,
        ops,
    })
    .unwrap();
    assert!(!d.tracks[0].events.iter().any(|e| {
        matches!(&e.kind, EventKind::Channel { status, .. } if status & 0xF0 == 0xD0
            || status & 0xF0 == 0xA0)
    }));
    // unknown ids are skipped rather than failing the whole op batch
    assert!(d.remove_events_ops(&[999_999]).is_empty());
}

// ---- RPN/NRPN ----

fn apply_ops(d: &mut Document, label: &str, ops: Vec<Op>) {
    let tx = Transaction {
        label: label.into(),
        base: d.revision(),
        ops,
    };
    d.apply(tx).unwrap();
}

fn cc_bytes(d: &Document, ti: usize) -> Vec<(u8, u8, u8)> {
    d.tracks[ti]
        .events
        .iter()
        .filter_map(|e| match e.kind {
            EventKind::Channel { status, data, .. } if status & 0xF0 == 0xB0 => {
                Some((status, data[0], data[1]))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn rpn_entries_parse_and_unusual_untouched() {
    // RPN pitch bend range 14-bit, an unrelated CC between selector and
    // data (still attaches — stateful), a null selector, then an NRPN.
    let d = chase_doc(vec![
        ev(0, 0xB0, 101, 0),
        ev(10, 0xB0, 100, 0),
        ev(20, 0xB0, 7, 100), // unrelated CC: not part of the entry
        ev(30, 0xB0, 6, 4),
        ev(40, 0xB0, 38, 1),
        ev(50, 0xB0, 101, 0x7F),
        ev(60, 0xB0, 100, 0x7F),
        ev(70, 0xB0, 99, 1),
        ev(80, 0xB0, 98, 2),
        ev(90, 0xB0, 6, 9),
    ]);
    let entries = d.rpn_entries();
    assert_eq!(entries.len(), 3);
    let e0 = &entries[0];
    assert!(!e0.nrpn && !e0.is_null());
    assert_eq!(e0.param14(), 0);
    assert_eq!(e0.param_name(), Some("Pitch Bend Range"));
    assert_eq!(e0.value(), Some((4 << 7) | 1));
    assert!(e0.is_14bit());
    assert_eq!(e0.ids().len(), 4);
    assert!(entries[1].is_null());
    assert_eq!(entries[1].value(), None);
    assert!(entries[2].nrpn);
    assert_eq!(entries[2].param14(), (1 << 7) | 2);
    assert_eq!(entries[2].value(), Some(9));
    // unrelated CC stayed a plain event: id list covers only sel+data
    assert_eq!(cc_bytes(&d, 0).len(), 10);

    // LSB-first ordering also parses (unusual hardware)
    let d2 = chase_doc(vec![
        ev(0, 0xB0, 100, 2),
        ev(10, 0xB0, 101, 0),
        ev(20, 0xB0, 6, 12),
    ]);
    let e = &d2.rpn_entries()[0];
    assert_eq!(e.param14(), 2);
    assert_eq!(e.value(), Some(12));
    assert_eq!(d2.tracks[0].events[0].id, e.sel_ids[0]); // order preserved

    // data entry with no active selector: not in the view, still raw
    let d3 = chase_doc(vec![ev(0, 0xB0, 6, 5)]);
    assert!(d3.rpn_entries().is_empty());
    assert_eq!(cc_bytes(&d3, 0).len(), 1);

    // viewing never mutates: event count and bytes are unchanged after
    let n = d.tracks[0].events.len();
    d.rpn_entries();
    assert_eq!(d.tracks[0].events.len(), n);
}

#[test]
fn rpn_ops_emit_valid_order_and_edit() {
    let mut d = chase_doc(vec![]);
    // RPN 0.0 = 14-bit (2 semitones + cents) → 101,100,6,38 in order
    let ops = d.set_rpn_ops(0, 480, 0, false, 0, 0, 12, Some(30));
    apply_ops(&mut d, "rpn", ops);
    let ccs = cc_bytes(&d, 0);
    assert_eq!(
        ccs,
        vec![
            (0xB0, 101, 0),
            (0xB0, 100, 0),
            (0xB0, 6, 12),
            (0xB0, 38, 30),
        ]
    );
    // null selector: no data attached
    let ops = d.set_rpn_ops(0, 960, 0, false, 0x7F, 0x7F, 0, None);
    apply_ops(&mut d, "null", ops);
    let entries = d.rpn_entries();
    assert_eq!(entries.len(), 2);
    assert!(entries[1].is_null());

    // NRPN order is 99,98; null selector emitted no data event
    let ops = d.set_rpn_ops(0, 1200, 0, true, 1, 2, 64, None);
    apply_ops(&mut d, "nrpn", ops);
    let ccs = cc_bytes(&d, 0);
    assert_eq!(&ccs[4..6], &[(0xB0, 101, 0x7F), (0xB0, 100, 0x7F)]);
    assert_eq!(&ccs[6..], &[(0xB0, 99, 1), (0xB0, 98, 2), (0xB0, 6, 64)]);

    // value edit: 14-bit → new bytes; then → 7-bit removes the LSB event
    let e = d.rpn_entries().remove(0);
    let ops = d.update_rpn_value_ops(&e, 5, Some(3));
    apply_ops(&mut d, "v", ops);
    let ccs = cc_bytes(&d, 0);
    assert_eq!(&ccs[2..4], &[(0xB0, 6, 5), (0xB0, 38, 3)]);
    let e = d.rpn_entries().remove(0);
    let ops = d.update_rpn_value_ops(&e, 5, None);
    apply_ops(&mut d, "v7", ops);
    let ccs = cc_bytes(&d, 0);
    assert_eq!(&ccs[2], &(0xB0, 6, 5));
    assert_eq!(ccs.len(), 8);

    // param edit rewrites only selector bytes
    let e = d.rpn_entries().remove(0);
    let ops = d.update_rpn_param_ops(&e, 0, 2);
    apply_ops(&mut d, "p", ops);
    let e = d.rpn_entries().remove(0);
    assert_eq!(e.param_name(), Some("Coarse Tuning"));
    assert_eq!(e.value(), Some(5));
}
// ---- GM/GS/XG names ----

#[test]
fn program_names_and_mode_hints() {
    let d = chase_doc(vec![
        sx(
            0,
            &[0x41, 0x10, 0x42, 0x12, 0x40, 0x00, 0x7F, 0x00, 0x41, 0xF7],
        ), // GS reset
        ev(0, 0xB0, 0, 0),   // ch1 bank msb 0
        ev(0, 0xB0, 32, 0),  // ch1 bank lsb 0
        ev(10, 0xC0, 24, 0), // PC 24 on bank 0.0
        ev(20, 0xB0, 0, 5),  // ch1 bank msb → 5
        ev(30, 0xC0, 9, 0),  // PC 9 on bank 5.0 (unknown → numeric)
        ev(40, 0xB9, 0, 0),  // ch10 bank msb 0
        ev(50, 0xC9, 0, 0),  // ch10 program → kit
        ev(60, 0x99, 36, 100),
    ]);
    assert_eq!(d.synth_mode(), Some(smf_core::ModeHint::Gs));
    let pcs = d.program_changes();
    assert_eq!(pcs.len(), 3);
    assert_eq!(
        (pcs[0].bank_msb, pcs[0].bank_lsb, pcs[0].program),
        (0, 0, 24)
    );
    assert_eq!(
        d.program_name(&pcs[0]).as_deref(),
        Some("GS: Acoustic Guitar (nylon)")
    );
    assert_eq!(pcs[1].bank_msb, 5);
    assert!(d.program_name(&pcs[1]).is_none()); // unknown bank stays numeric
    assert_eq!(pcs[2].channel, 9);
    assert_eq!(d.program_name(&pcs[2]).as_deref(), Some("Standard Kit #0"));

    // no reset SysEx → still GM names on bank 0, just unlabeled by mode
    let d2 = chase_doc(vec![ev(0, 0xC0, 0, 0)]);
    assert_eq!(d2.synth_mode(), None);
    assert_eq!(
        d2.program_name(&d2.program_changes()[0]).as_deref(),
        Some("Acoustic Grand Piano")
    );

    // XG file: drum bank 127 resolves to the XG kit label
    let d3 = chase_doc(vec![
        sx(0, &[0x43, 0x10, 0x4C, 0x00, 0x00, 0x7E, 0x00, 0xF7]), // XG on
        ev(0, 0xB9, 0, 127),
        ev(10, 0xC9, 1, 0),
    ]);
    assert_eq!(d3.synth_mode(), Some(smf_core::ModeHint::Xg));
    assert_eq!(
        d3.program_name(&d3.program_changes()[0]).as_deref(),
        Some("XG Drums #1")
    );

    // viewing never mutates
    let n = d.tracks[0].events.len();
    d.program_changes();
    d.synth_mode();
    assert_eq!(d.tracks[0].events.len(), n);
}
