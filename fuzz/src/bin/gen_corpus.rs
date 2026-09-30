//! Regenerate the committed seed corpus under `fuzz/corpus/shared/`.
//! Deterministic: same code, same files. Run with:
//!     cargo run --manifest-path fuzz/Cargo.toml --bin gen_corpus
//!
//! Every file doubles as a permanent regression fixture: `cargo test`
//! replays the whole corpus through the parse/write fixpoint invariants
//! (crates/smf-core/tests/fuzz_corpus.rs).
use std::fs;
use std::path::{Path, PathBuf};

fn out_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("corpus")
        .join("shared")
}

/// Seeds that exercise a known midly panic the guarded `parse` survives —
/// kept out of `corpus/shared` so the `parse_strict` target (whose job is to
/// surface panics unfiltered) doesn't trip on it every replay. The lenient /
/// guarded targets and the corpus regression test still see it.
fn not_strict_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("corpus")
        .join("not_strict")
}

fn header(format: u16, division: u16, ntrks: u16) -> Vec<u8> {
    let mut f = b"MThd".to_vec();
    f.extend_from_slice(&6u32.to_be_bytes());
    f.extend_from_slice(&format.to_be_bytes());
    f.extend_from_slice(&ntrks.to_be_bytes());
    f.extend_from_slice(&division.to_be_bytes());
    f
}

fn mtrk(body: &[u8]) -> Vec<u8> {
    let mut t = b"MTrk".to_vec();
    t.extend_from_slice(&(body.len() as u32).to_be_bytes());
    t.extend_from_slice(body);
    t
}

fn note_on(delta: u8, ch: u8, key: u8, vel: u8) -> [u8; 4] {
    [delta, 0x90 | ch, key, vel]
}

const EOT: [u8; 4] = [0x00, 0xFF, 0x2F, 0x00];

fn fixture_sjis() -> Vec<u8> {
    // the primary in-tree fixture: format 1, SJIS track name, running
    // status, SysEx
    let mut f = header(1, 0x01E0, 2);
    let t0: &[u8] = &[
        0x00, 0xFF, 0x51, 0x03, 0x07, 0xA1, 0x20, // tempo 500000
        0x00, 0xFF, 0x58, 0x04, 0x04, 0x02, 0x18, 0x08, // 4/4
        0x00, 0xFF, 0x2F, 0x00, // EOT
    ];
    let mut t1 = Vec::new();
    t1.extend_from_slice(&[0x00, 0xFF, 0x03, 0x06]);
    t1.extend_from_slice(&[0x83, 0x65, 0x83, 0x58, 0x83, 0x67]); // "テスト" SJIS
    t1.extend_from_slice(&[0x00, 0x90, 0x3C, 0x64]);
    t1.extend_from_slice(&[0x60, 0x3C, 0x00]); // running status vel0
    t1.extend_from_slice(&[0x00, 0xF0, 0x04, 0x7E, 0x7F, 0x09, 0x01]); // GM on
    t1.extend_from_slice(&EOT);
    f.extend_from_slice(&mtrk(t0));
    f.extend_from_slice(&mtrk(&t1));
    f
}

fn tiny() -> Vec<u8> {
    let mut f = header(0, 480, 1);
    f.extend_from_slice(&mtrk(&[
        0x00, 0x90, 0x3C, 0x64, 0x00, 0xFF, 0x2F, 0x00,
    ]));
    f
}

fn smpte() -> Vec<u8> {
    let mut f = header(1, 0xE728, 1); // -25fps, 40 tpf
    f.extend_from_slice(&mtrk(&[
        0x00, 0x90, 0x3C, 0x64, 0x28, 0x80, 0x3C, 0x00, 0x00, 0xFF, 0x2F, 0x00,
    ]));
    f
}

fn smpte_degenerate() -> Vec<u8> {
    // fps byte 0x80 is not a legal SMPTE rate — exercises the lenient path
    let mut f = tiny();
    f[12] = 0x80;
    f[13] = 0x64;
    f
}

fn format0_multi() -> Vec<u8> {
    let mut t = Vec::new();
    t.extend_from_slice(&[0x00, 0xFF, 0x03, 0x04]);
    t.extend_from_slice(b"lead");
    t.extend_from_slice(&note_on(0, 0, 60, 100));
    t.extend_from_slice(&note_on(0, 9, 64, 100)); // ch10
    t.extend_from_slice(&[0x60, 0x80, 0x3C, 0x00]);
    t.extend_from_slice(&[0x00, 0x89, 0x40, 0x00]);
    t.extend_from_slice(&EOT);
    let mut f = header(0, 480, 1);
    f.extend_from_slice(&mtrk(&t));
    f
}

fn running_status() -> Vec<u8> {
    let t = [
        0x00, 0x90, 0x3C, 0x64, // NoteOn 60
        0x10, 0x40, 0x60, // +16 NoteOn 64 (running)
        0x10, 0x3C, 0x00, // +16 off via vel0 (running)
        0x10, 0x40, 0x00, // +16 off (running)
        0x00, 0xFF, 0x2F, 0x00,
    ];
    let mut f = header(0, 480, 1);
    f.extend_from_slice(&mtrk(&t));
    f
}

fn escape_sysex() -> Vec<u8> {
    let mut t = Vec::new();
    t.extend_from_slice(&[0x00, 0xF0, 0x05, 0x7E, 0x7F, 0x09, 0x01, 0xF7]);
    t.extend_from_slice(&[0x00, 0xF7, 0x03, 0x41, 0x10, 0x42]);
    t.extend_from_slice(&EOT);
    let mut f = header(0, 480, 1);
    f.extend_from_slice(&mtrk(&t));
    f
}

fn unknown_metas() -> Vec<u8> {
    let mut t = Vec::new();
    t.extend_from_slice(&[0x00, 0xFF, 0x21, 0x01, 0x02]); // port prefix
    t.extend_from_slice(&[0x00, 0xFF, 0x20, 0x01, 0x03]); // channel prefix
    t.extend_from_slice(&[0x00, 0xFF, 0x7F, 0x04, 0xDE, 0xAD, 0xBE, 0xEF]);
    t.extend_from_slice(&EOT);
    let mut f = header(1, 480, 1);
    f.extend_from_slice(&mtrk(&t));
    f
}

fn missing_eot() -> Vec<u8> {
    let t = [0x00, 0x90, 0x3C, 0x64, 0x60, 0x3C, 0x00];
    let mut f = header(0, 480, 1);
    f.extend_from_slice(&mtrk(&t));
    f
}

fn junk_chunk() -> Vec<u8> {
    let t = [0x00, 0xFF, 0x2F, 0x00];
    let mut f = header(1, 480, 2);
    f.extend_from_slice(&mtrk(&t));
    f.extend_from_slice(b"Xtra");
    f.extend_from_slice(&4u32.to_be_bytes());
    f.extend_from_slice(&[1, 2, 3, 4]);
    f.extend_from_slice(&mtrk(&t));
    f
}

fn big_vlq_delta() -> Vec<u8> {
    let t = [
        0x00, 0x90, 0x3C, 0x64, //
        0x81, 0x80, 0x80, 0x00, 0x80, 0x3C, 0x00, // off at tick 0x200000
        0x00, 0xFF, 0x2F, 0x00,
    ];
    let mut f = header(0, 480, 1);
    f.extend_from_slice(&mtrk(&t));
    f
}

fn running_across_meta() -> Vec<u8> {
    let t = [
        0x00, 0x90, 0x3C, 0x64, // NoteOn 60
        0x00, 0xFF, 0x01, 0x03, b'h', b'e', b'y', // text meta mid-status
        0x10, 0x40, 0x60, // +16 NoteOn 64 — running status ACROSS the meta
        0x00, 0xFF, 0x2F, 0x00,
    ];
    let mut f = header(0, 480, 1);
    f.extend_from_slice(&mtrk(&t));
    f
}

fn truncated_meta() -> Vec<u8> {
    let t = [
        0x00, 0x90, 0x3C, 0x64, // good noteOn
        0x00, 0xFF, 0x05, 0x7F, b'o', b'k', // lyric claims 127, has 2
    ];
    let mut f = header(0, 480, 1);
    f.extend_from_slice(&mtrk(&t));
    f
}

fn hostile_vlq10() -> Vec<u8> {
    // meta length declared as a 10-byte VLQ
    let t: Vec<u8> = [
        &[0x00u8, 0xFF, 0x01][..],
        &[0xFF; 10][..],
        &[0x41, 0x42][..],
    ]
    .concat();
    let mut f = header(0, 480, 1);
    f.extend_from_slice(&mtrk(&t));
    f
}

fn rmid() -> Vec<u8> {
    let smf = tiny();
    let mut f = Vec::new();
    f.extend_from_slice(b"RIFF");
    f.extend_from_slice(&((4 + 8 + smf.len()) as u32).to_be_bytes());
    f.extend_from_slice(b"RMID");
    f.extend_from_slice(b"data");
    f.extend_from_slice(&(smf.len() as u32).to_be_bytes());
    f.extend_from_slice(&smf);
    f
}

fn text_mix() -> Vec<u8> {
    let mut t = Vec::new();
    t.extend_from_slice(&[0x00, 0xFF, 0x03, 0x06]);
    t.extend_from_slice(&[0x83, 0x65, 0x83, 0x58, 0x83, 0x67]); // SJIS name
    t.extend_from_slice(&[0x00, 0xFF, 0x05, 0x09]);
    t.extend_from_slice("歌詞abc".as_bytes()); // UTF-8 lyric
    t.extend_from_slice(&[0x00, 0xFF, 0x06, 0x02, 0xE9, 0x20]); // Latin-1 marker
    t.extend_from_slice(&EOT);
    let mut f = header(0, 480, 1);
    f.extend_from_slice(&mtrk(&t));
    f
}

/// A plausible mid-size song: conductor track + 4 instrument tracks with
/// notes, CC, pitch bend, program changes, markers, lyrics, SysEx — the
/// "real-world shaped" bulk of the corpus (~several hundred events).
fn song() -> Vec<u8> {
    let mut xorshift = 0x9E3779B97F4A7C15u64;
    let mut rnd = move || {
        xorshift ^= xorshift << 13;
        xorshift ^= xorshift >> 7;
        xorshift ^= xorshift << 17;
        xorshift
    };

    // conductor: tempo changes, time sig, markers, EOT
    let mut t0 = Vec::new();
    t0.extend_from_slice(&[0x00, 0xFF, 0x03, 0x05]);
    t0.extend_from_slice(b"song!");
    t0.extend_from_slice(&[0x00, 0xFF, 0x51, 0x03, 0x07, 0xA1, 0x20]); // 120bpm
    t0.extend_from_slice(&[0x00, 0xFF, 0x58, 0x04, 0x04, 0x02, 0x18, 0x08]); // 4/4
    for i in 0..8u32 {
        let mut v = Vec::new();
        smf_core::write_vlq(480 * 4, &mut v); // one bar
        t0.extend_from_slice(&v);
        t0.extend_from_slice(&[0xFF, 0x06, 0x05]);
        t0.extend_from_slice(format!("bar {i}").as_bytes());
        if i == 4 {
            t0.extend_from_slice(&[0x00, 0xFF, 0x51, 0x03, 0x06, 0x8B, 0x5D]); // 140bpm
        }
    }
    t0.extend_from_slice(&EOT);

    let mut f = header(1, 480, 5);
    f.extend_from_slice(&mtrk(&t0));

    for track in 0..4u8 {
        let ch = track;
        let mut t = Vec::new();
        let mut name = Vec::new();
        name.extend_from_slice(&[0x00, 0xFF, 0x03, 0x07]);
        name.extend_from_slice(format!("track-{track}").as_bytes());
        t.extend_from_slice(&name);
        t.extend_from_slice(&[0x00, 0xC0 | ch, (track * 16 + 8) & 0x7F]); // program
        if track == 0 {
            t.extend_from_slice(&[0x00, 0xF0, 0x06, 0x7E, 0x7F, 0x09, 0x01, 0xF7]); // GM on
        }
        let mut delta_left = 0u64; // ticks owed before next event
        for i in 0..120u64 {
            let key = (36 + (rnd() % 48)) as u8;
            let vel = (40 + (rnd() % 80)) as u8;
            let dur = 60 + (rnd() % 420);
            let gap = 30 + (rnd() % 200);
            let mut d = Vec::new();
            smf_core::write_vlq(delta_left, &mut d);
            t.extend_from_slice(&d);
            t.extend_from_slice(&[0x90 | ch, key, vel]);
            let mut d = Vec::new();
            smf_core::write_vlq(dur, &mut d);
            t.extend_from_slice(&d);
            t.extend_from_slice(&[0x80 | ch, key, 0]);
            delta_left = gap;
            if i % 12 == 5 {
                let mut d = Vec::new();
                smf_core::write_vlq(delta_left, &mut d);
                t.extend_from_slice(&d);
                t.extend_from_slice(&[0xB0 | ch, 7, (rnd() % 128) as u8]); // volume CC
                delta_left = 0;
            }
            if i % 17 == 9 {
                let mut d = Vec::new();
                smf_core::write_vlq(delta_left, &mut d);
                t.extend_from_slice(&d);
                let bend = (rnd() % 16384) as u16;
                t.extend_from_slice(&[0xE0 | ch, (bend & 0x7F) as u8, (bend >> 7) as u8]);
                delta_left = 0;
            }
            if i % 31 == 13 && track == 0 {
                let mut d = Vec::new();
                smf_core::write_vlq(delta_left, &mut d);
                t.extend_from_slice(&d);
                t.extend_from_slice(&[0xFF, 0x05, 0x03]);
                t.extend_from_slice(b"ooh");
                delta_left = 0;
            }
        }
        let mut d = Vec::new();
        smf_core::write_vlq(delta_left, &mut d);
        t.extend_from_slice(&d);
        t.extend_from_slice(&EOT[1..]);
        f.extend_from_slice(&mtrk(&t));
    }
    f
}

fn truncations(base: &[u8]) -> Vec<(String, Vec<u8>)> {
    // a spread of cut points hitting header, chunk header, mid-event, payload
    let n = base.len();
    let cuts = [
        1, 4, 8, 12, 13, 14, 15, 20, 22, 26, n / 3, n / 2, n * 2 / 3, n - 8, n - 5,
        n - 3, n - 1,
    ];
    cuts.iter()
        .filter(|&&c| c < n)
        .map(|&c| (format!("trunc_{c}"), base[..c].to_vec()))
        .collect()
}

fn main() {
    let dir = out_dir();
    fs::create_dir_all(&dir).unwrap();
    let not_strict = not_strict_dir();
    fs::create_dir_all(&not_strict).unwrap();
    // division byte 0x80 -> midly negates -128 and panics (upstream bug);
    // parse() recovers via lenient. Lives in not_strict so parse_strict
    // corpus replay stays clean.
    fs::write(not_strict.join("smpte_degenerate.mid"), smpte_degenerate()).unwrap();

    let mut files: Vec<(String, Vec<u8>)> = vec![
        ("fixture_sjis".into(), fixture_sjis()),
        ("tiny".into(), tiny()),
        ("smpte".into(), smpte()),
        ("format0_multi".into(), format0_multi()),
        ("running_status".into(), running_status()),
        ("escape_sysex".into(), escape_sysex()),
        ("unknown_metas".into(), unknown_metas()),
        ("missing_eot".into(), missing_eot()),
        ("junk_chunk".into(), junk_chunk()),
        ("big_vlq_delta".into(), big_vlq_delta()),
        ("running_across_meta".into(), running_across_meta()),
        ("truncated_meta".into(), truncated_meta()),
        ("hostile_vlq10".into(), hostile_vlq10()),
        ("rmid".into(), rmid()),
        ("text_mix".into(), text_mix()),
        ("song".into(), song()),
    ];
    files.extend(truncations(&fixture_sjis()));
    files.extend(truncations(&song().iter().take(200).copied().collect::<Vec<u8>>()));

    for (name, bytes) in &files {
        fs::write(dir.join(format!("{name}.mid")), bytes).unwrap();
    }
    println!("wrote {} seed files to {}", files.len(), dir.display());
}
