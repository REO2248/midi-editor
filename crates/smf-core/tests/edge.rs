// Edge-case corpus: weird-but-real SMF inputs (SMPTE timing, format 0/2,
// running status, escape packets, unknown meta, missing EOT, junk chunks).
// The contract: never panic; byte-exact on clean input; a converging
// normalize on dirty input (write(parse(out)) == out).
use smf_core::*;

fn header(format: u16, division: u16, ntrks: u16) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(b"MThd");
    f.extend_from_slice(&6u32.to_be_bytes());
    f.extend_from_slice(&format.to_be_bytes());
    f.extend_from_slice(&ntrks.to_be_bytes());
    f.extend_from_slice(&division.to_be_bytes());
    f
}

fn mtrk(body: &[u8]) -> Vec<u8> {
    let mut t = Vec::new();
    t.extend_from_slice(b"MTrk");
    t.extend_from_slice(&(body.len() as u32).to_be_bytes());
    t.extend_from_slice(body);
    t
}

fn rt(parsed: &File) -> Vec<u8> {
    write(
        parsed.format,
        parsed.division,
        &parsed.tracks,
        WriteOptions::default(),
    )
}

fn converges(bytes: &[u8]) {
    let f = parse(bytes).unwrap();
    let out = rt(&f);
    let f2 = parse(&out).unwrap();
    let out2 = rt(&f2);
    assert_eq!(out, out2, "pipeline must reach a fixpoint");
}

#[test]
fn smpte_division_roundtrip() {
    // 0xE7 = -25 fps, 0x28 = 40 ticks/frame
    let mut f = header(1, 0xE728, 1);
    f.extend(mtrk(&[
        0x00, 0x90, 0x3C, 0x64, 0x28, 0x80, 0x3C, 0x00, 0x00, 0xFF, 0x2F, 0x00,
    ]));
    let parsed = parse(&f).unwrap();
    assert!(matches!(
        parsed.division,
        Division::Smpte {
            fps: 25,
            ticks_per_frame: 40
        }
    ));
    assert_eq!(rt(&parsed), f, "SMPTE file must round-trip byte-exact");
}

#[test]
fn format0_multichannel() {
    // everything in one track, channels interleaved
    let mut t = Vec::new();
    t.extend_from_slice(&[0x00, 0xFF, 0x03, 0x04]);
    t.extend_from_slice(b"lead");
    t.extend_from_slice(&[0x00, 0x90, 0x3C, 0x64]); // ch1 note
    t.extend_from_slice(&[0x00, 0x99, 0x40, 0x64]); // ch10 note
    t.extend_from_slice(&[0x60, 0x80, 0x3C, 0x00]);
    t.extend_from_slice(&[0x00, 0x89, 0x40, 0x00]);
    t.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]);
    let mut f = header(0, 480, 1);
    f.extend(mtrk(&t));
    let parsed = parse(&f).unwrap();
    assert_eq!(parsed.format, 0);
    assert_eq!(rt(&parsed), f);
}

#[test]
fn running_status_input_normalized_to_fixpoint() {
    // second noteOn omits status — legal running status
    let t = [
        0x00, 0x90, 0x3C, 0x64, // NoteOn 60
        0x10, 0x40, 0x60, // +16 NoteOn 64 (running)
        0x10, 0x3C, 0x00, // +16 note60 off via vel0 (running)
        0x10, 0x40, 0x00, // +16 note64 off (running)
        0x00, 0xFF, 0x2F, 0x00,
    ];
    let mut f = header(0, 480, 1);
    f.extend(mtrk(&t));
    let parsed = parse(&f).unwrap();
    // writer may emit explicit status — that is a normalization, must converge
    converges(&f);
    let notes: Vec<_> = parsed.tracks[0]
        .events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::Channel { status, .. } if status & 0xF0 == 0x90))
        .collect();
    assert_eq!(notes.len(), 4, "running-status events must all decode");
}

#[test]
fn escape_and_sysex_preserved() {
    let mut t = Vec::new();
    // F0 sysex (GM system on)
    t.extend_from_slice(&[0x00, 0xF0, 0x05, 0x7E, 0x7F, 0x09, 0x01, 0xF7]);
    // F7 escape packet (continuation / arbitrary)
    t.extend_from_slice(&[0x00, 0xF7, 0x03, 0x41, 0x10, 0x42]);
    t.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]);
    let mut f = header(0, 480, 1);
    f.extend(mtrk(&t));
    let parsed = parse(&f).unwrap();
    assert_eq!(rt(&parsed), f, "sysex+escape must round-trip byte-exact");
}

#[test]
fn unknown_meta_and_port_prefix_preserved() {
    let mut t = Vec::new();
    t.extend_from_slice(&[0x00, 0xFF, 0x21, 0x01, 0x02]); // port prefix 2
    t.extend_from_slice(&[0x00, 0xFF, 0x20, 0x01, 0x03]); // channel prefix 3
    t.extend_from_slice(&[0x00, 0xFF, 0x7F, 0x04, 0xDE, 0xAD, 0xBE, 0xEF]); // sequencer-specific
    t.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]);
    let mut f = header(1, 480, 1);
    f.extend(mtrk(&t));
    let parsed = parse(&f).unwrap();
    assert_eq!(rt(&parsed), f);
}

#[test]
fn missing_eot_gets_added_and_converges() {
    let t = [0x00, 0x90, 0x3C, 0x64, 0x60, 0x3C, 0x00]; // no EOT
    let mut f = header(0, 480, 1);
    f.extend(mtrk(&t));
    let parsed = parse(&f).unwrap();
    let out = rt(&parsed);
    assert_ne!(out, f, "writer should append EOT");
    converges(&f);
    // re-parse finds the EOT
    let f2 = parse(&out).unwrap();
    let last = f2.tracks[0]
        .events
        .iter()
        .max_by_key(|e| (e.tick, e.seq))
        .unwrap();
    assert!(matches!(
        last.kind,
        EventKind::Meta {
            meta_type: 0x2F,
            ..
        }
    ));
}

#[test]
fn duplicate_eot_untouched_is_byte_exact() {
    // two End-of-Track metas in one stream — unusual but parseable; an
    // untouched save must not collapse them
    let t = [
        0x00, 0x90, 0x3C, 0x64, // note on
        0x60, 0x80, 0x3C, 0x00, // note off
        0x00, 0xFF, 0x2F, 0x00, // EOT
        0x40, 0xFF, 0x2F, 0x00, // second EOT 64 ticks later
    ];
    let mut f = header(0, 480, 1);
    f.extend(mtrk(&t));
    let parsed = parse(&f).unwrap();
    assert_eq!(rt(&parsed), f, "duplicate EOT must survive untouched");
}

#[test]
fn premature_eot_untouched_is_byte_exact() {
    // events continue after an early End-of-Track — unusual but real;
    // an untouched save must not move or re-emit the terminator
    let t = [
        0x00, 0x90, 0x3C, 0x64, // note on
        0x00, 0xFF, 0x2F, 0x00, // premature EOT at tick 0
        0x60, 0x80, 0x3C, 0x00, // note off AFTER the EOT
        0x00, 0xFF, 0x2F, 0x00, // real EOT
    ];
    let mut f = header(0, 480, 1);
    f.extend(mtrk(&t));
    let parsed = parse(&f).unwrap();
    assert_eq!(
        rt(&parsed),
        f,
        "premature EOT + trailing events must survive untouched"
    );
    converges(&f);
}

#[test]
fn junk_chunk_warns_but_survives() {
    let t = [0x00, 0xFF, 0x2F, 0x00];
    let mut f = header(1, 480, 2);
    f.extend(mtrk(&t));
    // a non-MTrk chunk between tracks (e.g. sequencer metadata)
    let mut junk = Vec::new();
    junk.extend_from_slice(b"Xtra");
    junk.extend_from_slice(&4u32.to_be_bytes());
    junk.extend_from_slice(&[1, 2, 3, 4]);
    f.extend(junk);
    f.extend(mtrk(&t));
    let parsed = parse(&f).unwrap();
    assert!(
        parsed.warnings.iter().any(|w| w.contains("non-MTrk")),
        "warning expected for Xtra chunk, got {:?}",
        parsed.warnings
    );
    converges(&f);
}

#[test]
fn truncated_file_never_panics() {
    let mut t = Vec::new();
    t.extend_from_slice(&[
        0x00, 0x90, 0x3C, 0x64, 0x60, 0x80, 0x3C, 0x00, 0x00, 0xFF, 0x2F, 0x00,
    ]);
    let mut f = header(0, 480, 1);
    f.extend(mtrk(&t));
    for cut in (1..f.len()).rev() {
        let _ = parse(&f[..cut]); // may err, must not panic
    }
}

#[test]
fn big_vlq_delta_roundtrips() {
    // delta 0x200000 (2M ticks) encoded as 0x83 0xC0 0x80 0x00
    let t = [
        0x00, 0x90, 0x3C, 0x64, //
        0x81, 0x80, 0x80, 0x00, 0x80, 0x3C, 0x00, // off at tick 2097152
        0x00, 0xFF, 0x2F, 0x00,
    ];
    let mut f = header(0, 480, 1);
    f.extend(mtrk(&t));
    let parsed = parse(&f).unwrap();
    assert_eq!(parsed.tracks[0].events[1].tick, 0x200000);
    assert_eq!(rt(&parsed), f);
}

#[test]
fn running_status_across_meta_recovers_leniently() {
    // illegal per spec (meta should clear running status) but common in old
    // sequencer files: strict parse fails, lenient fallback must recover the
    // events and converge to clean output
    let t = [
        0x00, 0x90, 0x3C, 0x64, // NoteOn 60
        0x00, 0xFF, 0x01, 0x03, b'h', b'e', b'y', // text meta mid-status
        0x10, 0x40, 0x60, // +16 NoteOn 64 — running status ACROSS the meta
        0x00, 0xFF, 0x2F, 0x00,
    ];
    let mut f = header(0, 480, 1);
    f.extend(mtrk(&t));
    let parsed = parse(&f).unwrap();
    assert!(
        parsed.warnings.iter().any(|w| w.contains("lenient")),
        "expected lenient warning, got {:?}",
        parsed.warnings
    );
    let ons: Vec<_> = parsed.tracks[0]
        .events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::Channel { status, .. } if status & 0xF0 == 0x90))
        .collect();
    assert_eq!(ons.len(), 2, "both noteOns must be recovered");
    converges(&f);
}

#[test]
fn truncated_track_tail_keeps_good_events() {
    // meta payload length overruns the chunk — recover events up to it
    let t = [
        0x00, 0x90, 0x3C, 0x64, // good noteOn
        0x00, 0xFF, 0x05, 0x7F, b'o', b'k', // lyric claims 127 bytes, only 2 present
    ];
    let mut f = header(0, 480, 1);
    f.extend(mtrk(&t));
    let parsed = parse(&f).unwrap();
    assert!(!parsed.tracks[0].events.is_empty());
    converges(&f);
}

#[test]
fn fuzz_bytes_never_panic() {
    // xorshift PRNG, fixed seed: deterministic fuzz over arbitrary garbage
    let mut s: u64 = 0x9E3779B97F4A7C15;
    let mut rand = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    for i in 0..2000 {
        let len = (rand() % 512) as usize;
        let mut buf = Vec::with_capacity(len);
        for _ in 0..len {
            buf.push(rand() as u8);
        }
        // half the cases get a real MThd so we reach the track walker
        if i % 2 == 0 && buf.len() >= 14 {
            buf[0..4].copy_from_slice(b"MThd");
            buf[4..8].copy_from_slice(&6u32.to_be_bytes());
        }
        let _ = parse(&buf); // may Err, must not panic
    }
}

#[test]
fn text_encodings_survive_through_fixpoint() {
    // SJIS track name + UTF-8 lyric + Latin-1 marker in one track
    let mut t = Vec::new();
    let (sjis, _, _) = encoding_rs::SHIFT_JIS.encode("テスト");
    t.extend_from_slice(&[0x00, 0xFF, 0x03]);
    smf_core::write_vlq(sjis.len() as u64, &mut t);
    t.extend_from_slice(&sjis);
    t.extend_from_slice(&[0x00, 0xFF, 0x05, 0x09]);
    t.extend_from_slice("歌詞abc".as_bytes()); // UTF-8 lyric (9 bytes)
    t.extend_from_slice(&[0x00, 0xFF, 0x06, 0x02, 0xE9, 0x20]); // Latin-1 marker
    t.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]);
    let mut f = header(0, 480, 1);
    f.extend(mtrk(&t));
    let parsed = parse(&f).unwrap();
    assert_eq!(rt(&parsed), f);
    assert_eq!(
        decode_text(
            &match &parsed.tracks[0].events[0].kind {
                EventKind::Meta { data, .. } => data.clone(),
                _ => panic!(),
            },
            Some(TextEncoding::ShiftJis)
        ),
        "テスト"
    );
}
