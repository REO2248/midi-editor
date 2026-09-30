// Regenerate the SMF compatibility corpus: fixtures/corpus/*.mid and
// fixtures/corpus/manifest.json.
//
//   corpus_gen [--check] [dir]     (default dir: fixtures/corpus)
//
// Every fixture is generated in-repo — no downloaded files, no licensing
// ambiguity. Each fixture emulates a quirk a real sequencer/device emits;
// the "mirrors" field names what it stands in for. Field regressions should
// be added as a minimized fixture + a manifest expectation here, then
// `corpus_gen` re-run.
//
// --check regenerates everything in memory and diffs against the committed
// files, so CI fails if the corpus and its generator drift apart.

use serde::Serialize;

#[derive(Serialize)]
struct Expect {
    /// "ok" = smf_core::parse must succeed; "error" = it must fail.
    parse: &'static str,
    /// write(parse(bytes)) == bytes (default write options).
    byte_exact: bool,
    /// write(parse(write(parse(bytes)))) is a fixpoint.
    fixpoint: bool,
    /// exact set of warning substrings the parse must report.
    warnings: &'static [&'static str],
    /// parsed track count (ignored when parse == "error").
    tracks: u16,
    /// derived doc.notes() count.
    notes: usize,
    /// exact sorted set of Document::diagnose() codes.
    diagnose: &'static [&'static str],
    /// chase_events(0) + a mid-file chase must not panic.
    chase_ok: bool,
    /// parse(doc.serialize(..)) result: "ok" or "error".
    reopen: &'static str,
}

struct Fixture {
    file: &'static str,
    /// interop class, e.g. "format-2", "smpte", "gs-sysex", "malformed"
    class: &'static str,
    /// the real-world source this fixture emulates
    mirrors: &'static str,
    build: fn() -> Vec<u8>,
    expect: Expect,
}

#[derive(Serialize)]
struct Manifest {
    note: &'static str,
    fixtures: Vec<FixtureView<'static>>,
}

#[derive(Serialize)]
struct FixtureView<'a> {
    file: &'a str,
    class: &'a str,
    mirrors: &'a str,
    expect: &'a Expect,
}

// ---- byte emitters ----

fn ev(delta: u64, body: &[u8], out: &mut Vec<u8>) {
    smf_core::write_vlq(delta, out);
    out.extend_from_slice(body);
}
fn meta(delta: u64, t: u8, data: &[u8], out: &mut Vec<u8>) {
    smf_core::write_vlq(delta, out);
    out.push(0xFF);
    out.push(t);
    smf_core::write_vlq(data.len() as u64, out);
    out.extend_from_slice(data);
}
fn tempo(delta: u64, us: u32, out: &mut Vec<u8>) {
    meta(delta, 0x51, &us.to_be_bytes()[1..], out);
}
fn eot(delta: u64, out: &mut Vec<u8>) {
    ev(delta, &[0xFF, 0x2F, 0x00], out);
}
fn on(delta: u64, ch: u8, key: u8, vel: u8, out: &mut Vec<u8>) {
    ev(delta, &[0x90 | ch, key, vel], out);
}
fn off(delta: u64, ch: u8, key: u8, out: &mut Vec<u8>) {
    ev(delta, &[0x80 | ch, key, 0], out);
}
fn cc(delta: u64, ch: u8, num: u8, val: u8, out: &mut Vec<u8>) {
    ev(delta, &[0xB0 | ch, num, val], out);
}
fn sysex(delta: u64, payload: &[u8], out: &mut Vec<u8>) {
    smf_core::write_vlq(delta, out);
    out.push(0xF0);
    smf_core::write_vlq(payload.len() as u64, out);
    out.extend_from_slice(payload);
}
fn chunk(tag: &[u8; 4], body: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(tag);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
}
fn assemble(format: u16, division_raw: u16, tracks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"MThd");
    out.extend_from_slice(&6u32.to_be_bytes());
    out.extend_from_slice(&format.to_be_bytes());
    out.extend_from_slice(&(tracks.len() as u16).to_be_bytes());
    out.extend_from_slice(&division_raw.to_be_bytes());
    for t in tracks {
        chunk(b"MTrk", t, &mut out);
    }
    out
}

/// three quarter-note-ish notes, 240 ticks apart, on `ch`
fn three_notes(ch: u8, out: &mut Vec<u8>) {
    for (i, k) in [60u8, 64, 67].iter().enumerate() {
        on(if i == 0 { 0 } else { 240 }, ch, *k, 96, out);
        off(240, ch, *k, out);
    }
}

fn conductor_track() -> Vec<u8> {
    let mut t = Vec::new();
    tempo(0, 500_000, &mut t);
    eot(0, &mut t);
    t
}

// ---- fixtures ----

fn format0_basic() -> Vec<u8> {
    let mut t = Vec::new();
    tempo(0, 500_000, &mut t);
    meta(0, 0x03, b"fmt0 demo", &mut t);
    three_notes(0, &mut t);
    eot(0, &mut t);
    assemble(0, 480, &[t])
}

fn format1_conductor() -> Vec<u8> {
    let mut t1 = Vec::new();
    meta(0, 0x03, b"melody", &mut t1);
    three_notes(0, &mut t1);
    eot(0, &mut t1);
    assemble(1, 480, &[conductor_track(), t1])
}

fn format2_patterns() -> Vec<u8> {
    let mut a = Vec::new();
    meta(0, 0x03, b"pattern A", &mut a);
    three_notes(0, &mut a);
    eot(0, &mut a);
    let mut b = Vec::new();
    meta(0, 0x03, b"pattern B", &mut b);
    on(0, 9, 36, 110, &mut b);
    off(120, 9, 36, &mut b);
    eot(0, &mut b);
    assemble(2, 480, &[a, b])
}

fn smpte_25fps() -> Vec<u8> {
    let mut t1 = Vec::new();
    meta(0, 0x03, b"smpte", &mut t1);
    // film-cue style notes 40 smpte ticks apart (25fps * 40tpf)
    for k in [60u8, 64, 67] {
        on(0, 0, k, 100, &mut t1);
        off(40, 0, k, &mut t1);
    }
    eot(0, &mut t1);
    // division 0xE728: -25fps in the top byte, 40 ticks/frame
    assemble(1, 0xE728, &[conductor_track(), t1])
}

fn karaoke_lyrics() -> Vec<u8> {
    let mut t1 = Vec::new();
    meta(0, 0x05, b"@TKaraoke demo", &mut t1);
    meta(0, 0x05, b"@LEN US", &mut t1);
    meta(0, 0x01, b"verse 1", &mut t1);
    let words: [&[u8]; 3] = [b"Hel-", b"lo ", b"world"];
    for (i, w) in words.iter().enumerate() {
        meta(0, 0x05, w, &mut t1);
        on(
            if i == 0 { 0 } else { 240 },
            0,
            [60u8, 64, 67][i],
            90,
            &mut t1,
        );
        off(240, 0, [60u8, 64, 67][i], &mut t1);
    }
    eot(0, &mut t1);
    assemble(1, 480, &[conductor_track(), t1])
}

fn shiftjis_meta() -> Vec<u8> {
    let mut t1 = Vec::new();
    // "ピアノ" in Shift-JIS
    meta(0, 0x03, &[0x83, 0x73, 0x83, 0x41, 0x83, 0x6D], &mut t1);
    // "歌詞" in Shift-JIS
    meta(0, 0x01, &[0x89, 0xCC, 0x8E, 0x8C], &mut t1);
    three_notes(0, &mut t1);
    eot(0, &mut t1);
    assemble(1, 480, &[conductor_track(), t1])
}

fn gs_reset() -> Vec<u8> {
    let mut t = Vec::new();
    meta(0, 0x03, b"GS", &mut t);
    // Roland GS Reset
    sysex(
        0,
        &[0x41, 0x10, 0x42, 0x12, 0x40, 0x00, 0x7F, 0x00, 0x41, 0xF7],
        &mut t,
    );
    three_notes(0, &mut t);
    eot(0, &mut t);
    assemble(0, 480, &[t])
}

fn xg_system_on() -> Vec<u8> {
    let mut t = Vec::new();
    meta(0, 0x03, b"XG", &mut t);
    // Yamaha XG System On
    sysex(0, &[0x43, 0x10, 0x4C, 0x00, 0x00, 0x7E, 0x00, 0xF7], &mut t);
    ev(0, &[0xC0, 0x00], &mut t); // program change 0
    three_notes(0, &mut t);
    eot(0, &mut t);
    assemble(0, 480, &[t])
}

fn rmid_wrapped() -> Vec<u8> {
    let inner = format0_basic();
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((4 + 8 + inner.len()) as u32).to_le_bytes());
    out.extend_from_slice(b"RMID");
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(inner.len() as u32).to_le_bytes());
    out.extend_from_slice(&inner);
    out
}

fn running_status() -> Vec<u8> {
    let mut body = Vec::new();
    tempo(0, 500_000, &mut body);
    // status emitted once, then running-status bodies
    smf_core::write_vlq(0, &mut body);
    body.extend_from_slice(&[0x90, 60, 96]);
    for k in [64u8, 67] {
        smf_core::write_vlq(240, &mut body);
        body.extend_from_slice(&[k, 96]);
    }
    smf_core::write_vlq(240, &mut body);
    body.extend_from_slice(&[0x80, 60, 0]); // explicit off status
    for k in [64u8, 67] {
        smf_core::write_vlq(0, &mut body);
        body.extend_from_slice(&[k, 0]);
    }
    eot(0, &mut body);
    assemble(0, 480, &[body])
}

fn dense_cc() -> Vec<u8> {
    let mut t = Vec::new();
    meta(0, 0x03, b"cc storm", &mut t);
    // controller storm at tick 0 — drawbar/filter dumps from real synths
    for i in 0..2000u32 {
        cc(0, 0, (i % 120) as u8, (i % 128) as u8, &mut t);
    }
    three_notes(0, &mut t);
    eot(0, &mut t);
    assemble(0, 480, &[t])
}

fn no_eot() -> Vec<u8> {
    let mut t = Vec::new();
    meta(0, 0x03, b"no eot", &mut t);
    three_notes(0, &mut t);
    assemble(0, 480, &[t])
}

fn trailing_garbage() -> Vec<u8> {
    let mut f = format0_basic();
    f.extend_from_slice(&[
        0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
    ]);
    f
}

fn xf_chunks() -> Vec<u8> {
    // Yamaha XF: extra XFIH/XFKM chunks around the tracks
    let mut out = Vec::new();
    out.extend_from_slice(b"MThd");
    out.extend_from_slice(&6u32.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&480u16.to_be_bytes());
    chunk(b"XFIH", &[0, 0, 0, 0], &mut out);
    let mut t = Vec::new();
    three_notes(0, &mut t);
    eot(0, &mut t);
    chunk(b"MTrk", &t, &mut out);
    chunk(b"XFKM", &[0xFF, 0xFF], &mut out);
    out
}

fn empty_track() -> Vec<u8> {
    let mut t1 = Vec::new();
    meta(0, 0x03, b"content", &mut t1);
    three_notes(0, &mut t1);
    eot(0, &mut t1);
    assemble(1, 480, &[conductor_track(), Vec::new(), t1])
}

fn vel0_offs() -> Vec<u8> {
    let mut body = Vec::new();
    meta(0, 0x03, b"vel0", &mut body);
    // note-offs as noteOn vel 0 — the classic running-status-friendly form
    smf_core::write_vlq(0, &mut body);
    body.extend_from_slice(&[0x90, 60, 96]);
    for (i, k) in [64u8, 67].iter().enumerate() {
        smf_core::write_vlq(240, &mut body);
        body.extend_from_slice(&[*k, 96]);
        let _ = i;
    }
    for k in [60u8, 64, 67] {
        smf_core::write_vlq(240, &mut body);
        body.extend_from_slice(&[k, 0]); // vel-0 off, still running 0x90
    }
    eot(0, &mut body);
    assemble(0, 480, &[body])
}

fn truncated_meta() -> Vec<u8> {
    // meta declares length 9, chunk only has 3 payload bytes left
    let mut t = Vec::new();
    meta(0, 0x03, b"cut", &mut t);
    t.extend_from_slice(&[0x00, 0xFF, 0x01, 0x09, b'a', b'b', b'c']);
    assemble(0, 480, &[t])
}

fn escape_f7() -> Vec<u8> {
    let mut t = Vec::new();
    meta(0, 0x03, b"esc", &mut t);
    // F7 escape packet: partial sysex continuation (GS parameter dump split)
    smf_core::write_vlq(0, &mut t);
    t.extend_from_slice(&[0xF7, 0x04, 0x41, 0x10, 0x42, 0x12]);
    three_notes(0, &mut t);
    eot(0, &mut t);
    assemble(0, 480, &[t])
}

fn noncanonical_vlq() -> Vec<u8> {
    // delta 96 padded to a non-canonical 4-byte VLQ
    let mut t = Vec::new();
    meta(0, 0x03, b"vlq", &mut t);
    on(0, 0, 60, 96, &mut t);
    t.extend_from_slice(&[0x80, 0x80, 0x80, 0x60]); // delta 96, over-long form
    t.extend_from_slice(&[0x80, 60, 0]); // note off
    eot(0, &mut t);
    assemble(0, 480, &[t])
}

fn pitch_rpn() -> Vec<u8> {
    let mut t = Vec::new();
    meta(0, 0x03, b"rpn", &mut t);
    // RPN 0000 = pitch bend range, data entry 5 semitones
    cc(0, 0, 101, 0, &mut t);
    cc(0, 0, 100, 0, &mut t);
    cc(0, 0, 6, 5, &mut t);
    cc(0, 0, 38, 0, &mut t);
    ev(120, &[0xE0, 0x00, 0x60], &mut t); // pitch bend +0.5
    three_notes(0, &mut t);
    eot(0, &mut t);
    assemble(0, 480, &[t])
}

fn markers_cues() -> Vec<u8> {
    let mut t0 = Vec::new();
    tempo(0, 500_000, &mut t0);
    meta(0, 0x06, b"verse", &mut t0);
    meta(480, 0x06, b"chorus", &mut t0);
    meta(0, 0x07, b"cue: lighting up", &mut t0);
    eot(0, &mut t0);
    let mut t1 = Vec::new();
    three_notes(0, &mut t1);
    eot(0, &mut t1);
    assemble(1, 480, &[t0, t1])
}

fn port_prefix() -> Vec<u8> {
    let mut t1 = Vec::new();
    meta(0, 0x21, &[0x00], &mut t1); // port 0 (Roland multi-port files)
    meta(0, 0x20, &[0x00], &mut t1); // channel prefix
    three_notes(0, &mut t1);
    eot(0, &mut t1);
    assemble(1, 480, &[conductor_track(), t1])
}

fn tempo_track2() -> Vec<u8> {
    let mut t0 = Vec::new();
    meta(0, 0x03, b"top", &mut t0);
    eot(0, &mut t0);
    let mut t1 = Vec::new();
    // tempo sitting on the content track — common in cheap exports
    tempo(0, 400_000, &mut t1);
    three_notes(0, &mut t1);
    eot(0, &mut t1);
    assemble(1, 480, &[t0, t1])
}

fn huge_sysex() -> Vec<u8> {
    let mut t = Vec::new();
    meta(0, 0x03, b"bulk", &mut t);
    // 64KB sample/patch dump
    let mut payload = vec![0x41u8, 0x10];
    payload.extend(std::iter::repeat_n(0x5Au8, 64 * 1024));
    payload.push(0xF7);
    sysex(0, &payload, &mut t);
    three_notes(0, &mut t);
    eot(0, &mut t);
    assemble(0, 480, &[t])
}

const FIXTURES: &[Fixture] = &[
    Fixture {
        file: "format0_basic.mid",
        class: "format-0",
        mirrors: "baseline single-track export",
        build: format0_basic,
        expect: Expect {
            parse: "ok",
            byte_exact: true,
            fixpoint: true,
            warnings: &[],
            tracks: 1,
            notes: 3,
            diagnose: &[],
            chase_ok: true,
            reopen: "ok",
        },
    },
    Fixture {
        file: "format1_conductor.mid",
        class: "format-1",
        mirrors: "conductor+content layout (most sequencers)",
        build: format1_conductor,
        expect: Expect {
            parse: "ok",
            byte_exact: true,
            fixpoint: true,
            warnings: &[],
            tracks: 2,
            notes: 3,
            diagnose: &[],
            chase_ok: true,
            reopen: "ok",
        },
    },
    Fixture {
        file: "format2_patterns.mid",
        class: "format-2",
        mirrors: "independent pattern tracks (drum machines, arrangers)",
        build: format2_patterns,
        expect: Expect {
            parse: "ok",
            byte_exact: true,
            fixpoint: true,
            warnings: &[],
            tracks: 2,
            notes: 4,
            diagnose: &[],
            chase_ok: true,
            reopen: "ok",
        },
    },
    Fixture {
        file: "smpte_25fps.mid",
        class: "smpte",
        mirrors: "film/video scoring files (25fps, 40tpf)",
        build: smpte_25fps,
        expect: Expect {
            parse: "ok",
            byte_exact: true,
            fixpoint: true,
            warnings: &[],
            tracks: 2,
            notes: 3,
            diagnose: &[],
            chase_ok: true,
            reopen: "ok",
        },
    },
    Fixture {
        file: "karaoke_lyrics.mid",
        class: "karaoke",
        mirrors: ".kar karaoke exports (FF05 lyric metas, @T/@L headers)",
        build: karaoke_lyrics,
        expect: Expect {
            parse: "ok",
            byte_exact: true,
            fixpoint: true,
            warnings: &[],
            tracks: 2,
            notes: 3,
            diagnose: &[],
            chase_ok: true,
            reopen: "ok",
        },
    },
    Fixture {
        file: "shiftjis_meta.mid",
        class: "shift-jis",
        mirrors: "Japanese sequencer text (SJIS track names/lyrics)",
        build: shiftjis_meta,
        expect: Expect {
            parse: "ok",
            byte_exact: true,
            fixpoint: true,
            warnings: &[],
            tracks: 2,
            notes: 3,
            diagnose: &[],
            chase_ok: true,
            reopen: "ok",
        },
    },
    Fixture {
        file: "gs_reset.mid",
        class: "gs-sysex",
        mirrors: "Roland GS device files (GS Reset + checksums)",
        build: gs_reset,
        expect: Expect {
            parse: "ok",
            byte_exact: true,
            fixpoint: true,
            warnings: &[],
            tracks: 1,
            notes: 3,
            diagnose: &[],
            chase_ok: true,
            reopen: "ok",
        },
    },
    Fixture {
        file: "xg_system_on.mid",
        class: "xg-sysex",
        mirrors: "Yamaha XG device files (XG System On)",
        build: xg_system_on,
        expect: Expect {
            parse: "ok",
            byte_exact: true,
            fixpoint: true,
            warnings: &[],
            tracks: 1,
            notes: 3,
            diagnose: &[],
            chase_ok: true,
            reopen: "ok",
        },
    },
    Fixture {
        file: "rmid_wrapped.mid",
        class: "rmid",
        mirrors: "RIFF-wrapped MIDI from game/multimedia assets",
        build: rmid_wrapped,
        expect: Expect {
            parse: "ok",
            byte_exact: false,
            fixpoint: true,
            warnings: &["RIFF/RMID container"],
            tracks: 1,
            notes: 3,
            diagnose: &[],
            chase_ok: true,
            reopen: "ok",
        },
    },
    Fixture {
        file: "running_status.mid",
        class: "running-status",
        mirrors: "status-elided streams (real-time capture, small sequencers)",
        build: running_status,
        expect: Expect {
            parse: "ok",
            byte_exact: true,
            fixpoint: true,
            warnings: &[],
            tracks: 1,
            notes: 3,
            diagnose: &[],
            chase_ok: true,
            reopen: "ok",
        },
    },
    Fixture {
        file: "dense_cc.mid",
        class: "dense-cc",
        mirrors: "controller storms (synth dumps, drawbar sweeps)",
        build: dense_cc,
        expect: Expect {
            parse: "ok",
            byte_exact: true,
            fixpoint: true,
            warnings: &[],
            tracks: 1,
            notes: 3,
            diagnose: &[],
            chase_ok: true,
            reopen: "ok",
        },
    },
    Fixture {
        file: "no_eot.mid",
        class: "malformed",
        mirrors: "files that end without End-of-Track",
        build: no_eot,
        expect: Expect {
            parse: "ok",
            byte_exact: false,
            fixpoint: true,
            warnings: &[],
            tracks: 1,
            notes: 3,
            diagnose: &["missing-eot"],
            chase_ok: true,
            reopen: "ok",
        },
    },
    Fixture {
        file: "trailing_garbage.mid",
        class: "malformed",
        mirrors: "padding/garbage appended after the last MTrk",
        build: trailing_garbage,
        expect: Expect {
            parse: "ok",
            byte_exact: false,
            fixpoint: true,
            warnings: &["strict parse failed", "non-MTrk chunk"],
            tracks: 1,
            notes: 3,
            diagnose: &[],
            chase_ok: true,
            reopen: "ok",
        },
    },
    Fixture {
        file: "xf_chunks.mid",
        class: "xf",
        mirrors: "Yamaha XF format (XFIH/XFKM chunks)",
        build: xf_chunks,
        expect: Expect {
            parse: "ok",
            byte_exact: false,
            fixpoint: true,
            warnings: &["non-MTrk chunk", "non-MTrk chunk"],
            tracks: 1,
            notes: 3,
            diagnose: &[],
            chase_ok: true,
            reopen: "ok",
        },
    },
    Fixture {
        file: "empty_track.mid",
        class: "malformed",
        mirrors: "zero-length MTrk chunks left by editors",
        build: empty_track,
        expect: Expect {
            parse: "ok",
            byte_exact: false,
            fixpoint: true,
            warnings: &[],
            tracks: 3,
            notes: 3,
            diagnose: &[],
            chase_ok: true,
            reopen: "ok",
        },
    },
    Fixture {
        file: "vel0_offs.mid",
        class: "running-status",
        mirrors: "note-offs written as vel-0 note-ons",
        build: vel0_offs,
        expect: Expect {
            parse: "ok",
            byte_exact: true,
            fixpoint: true,
            warnings: &[],
            tracks: 1,
            notes: 3,
            diagnose: &[],
            chase_ok: true,
            reopen: "ok",
        },
    },
    Fixture {
        file: "truncated_meta.mid",
        class: "malformed",
        mirrors: "meta payload length overrunning its chunk",
        build: truncated_meta,
        expect: Expect {
            parse: "ok",
            byte_exact: false,
            fixpoint: true,
            warnings: &["strict parse failed", "overruns chunk"],
            tracks: 1,
            notes: 0,
            diagnose: &["missing-eot"],
            chase_ok: true,
            reopen: "ok",
        },
    },
    Fixture {
        file: "escape_f7.mid",
        class: "sysex-escape",
        mirrors: "F7 escape packets (split/partial sysex dumps)",
        build: escape_f7,
        expect: Expect {
            parse: "ok",
            byte_exact: true,
            fixpoint: true,
            warnings: &[],
            tracks: 1,
            notes: 3,
            diagnose: &[],
            chase_ok: true,
            reopen: "ok",
        },
    },
    Fixture {
        file: "noncanonical_vlq.mid",
        class: "malformed",
        mirrors: "over-long delta-time VLQ encodings",
        build: noncanonical_vlq,
        expect: Expect {
            parse: "ok",
            byte_exact: false,
            fixpoint: true,
            warnings: &[],
            tracks: 1,
            notes: 1,
            diagnose: &[],
            chase_ok: true,
            reopen: "ok",
        },
    },
    Fixture {
        file: "pitch_rpn.mid",
        class: "rpn",
        mirrors: "RPN pitch-bend-range setup + bends",
        build: pitch_rpn,
        expect: Expect {
            parse: "ok",
            byte_exact: true,
            fixpoint: true,
            warnings: &[],
            tracks: 1,
            notes: 3,
            diagnose: &[],
            chase_ok: true,
            reopen: "ok",
        },
    },
    Fixture {
        file: "markers_cues.mid",
        class: "markers",
        mirrors: "marker/cue-point metas from DAW exports",
        build: markers_cues,
        expect: Expect {
            parse: "ok",
            byte_exact: true,
            fixpoint: true,
            warnings: &[],
            tracks: 2,
            notes: 3,
            diagnose: &[],
            chase_ok: true,
            reopen: "ok",
        },
    },
    Fixture {
        file: "port_prefix.mid",
        class: "port-prefix",
        mirrors: "Roland multi-port files (FF20/FF21 metas)",
        build: port_prefix,
        expect: Expect {
            parse: "ok",
            byte_exact: true,
            fixpoint: true,
            warnings: &[],
            tracks: 2,
            notes: 3,
            diagnose: &[],
            chase_ok: true,
            reopen: "ok",
        },
    },
    Fixture {
        file: "tempo_track2.mid",
        class: "misplaced-tempo",
        mirrors: "tempo changes on a non-conductor track",
        build: tempo_track2,
        expect: Expect {
            parse: "ok",
            byte_exact: true,
            fixpoint: true,
            warnings: &[],
            tracks: 2,
            notes: 3,
            diagnose: &["tempo-outside-conductor"],
            chase_ok: true,
            reopen: "ok",
        },
    },
    Fixture {
        file: "huge_sysex.mid",
        class: "gs-sysex",
        mirrors: "large bulk dumps (64KB+ sample/patch transfers)",
        build: huge_sysex,
        expect: Expect {
            parse: "ok",
            byte_exact: true,
            fixpoint: true,
            warnings: &[],
            tracks: 1,
            notes: 3,
            diagnose: &[],
            chase_ok: true,
            reopen: "ok",
        },
    },
];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let check = args.iter().any(|a| a == "--check");
    let dir = args
        .iter()
        .find(|a| a.as_str() != "--check")
        .cloned()
        .unwrap_or_else(|| "fixtures/corpus".into());
    let dir = std::path::Path::new(&dir);
    let mut drift = 0;

    let manifest = Manifest {
        note: "generated by crates/document/src/bin/corpus_gen.rs — regenerate with `cargo run -p document --bin corpus_gen`",
        fixtures: FIXTURES
            .iter()
            .map(|f| FixtureView { file: f.file, class: f.class, mirrors: f.mirrors, expect: &f.expect })
            .collect(),
    };
    let manifest_json = format!("{}\n", serde_json::to_string_pretty(&manifest).unwrap());

    for f in FIXTURES {
        let want = (f.build)();
        let p = dir.join(f.file);
        if check {
            match std::fs::read(&p) {
                Ok(got) if got == want => println!("{}: ok ({} bytes)", f.file, want.len()),
                Ok(got) => {
                    drift += 1;
                    println!(
                        "{}: DRIFT (disk {}B vs generator {}B)",
                        f.file,
                        got.len(),
                        want.len()
                    );
                }
                Err(e) => {
                    drift += 1;
                    println!("{}: MISSING ({e})", f.file);
                }
            }
        } else {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(&p, &want).unwrap();
            println!("{}: wrote {} bytes", f.file, want.len());
        }
    }
    let mp = dir.join("manifest.json");
    if check {
        match std::fs::read_to_string(&mp) {
            Ok(got) if got == manifest_json => println!("manifest.json: ok"),
            Ok(_) => {
                drift += 1;
                println!("manifest.json: DRIFT");
            }
            Err(e) => {
                drift += 1;
                println!("manifest.json: MISSING ({e})");
            }
        }
        println!("== corpus_gen --check: {drift} drifted");
        std::process::exit((drift > 0) as i32);
    }
    std::fs::write(&mp, manifest_json).unwrap();
    println!("manifest.json: wrote {} fixtures", FIXTURES.len());
}
