//! Guard coverage for the chunk-level walkers: non-MTrk chunks produce a
//! warning (the data is dropped on write, so silence would corrupt), stray
//! trailing bytes are reported, bad header lengths are rejected, and the
//! SMPTE division form decodes its negated fps byte.

use smf_core::{parse, parse_lenient, Division};

fn header(fmt: u16, ntrks: u16, division: u16) -> Vec<u8> {
    let mut b = b"MThd".to_vec();
    b.extend_from_slice(&6u32.to_be_bytes());
    b.extend_from_slice(&fmt.to_be_bytes());
    b.extend_from_slice(&ntrks.to_be_bytes());
    b.extend_from_slice(&division.to_be_bytes());
    b
}

fn mtrk(body: &[u8]) -> Vec<u8> {
    let mut b = b"MTrk".to_vec();
    b.extend_from_slice(&(body.len() as u32).to_be_bytes());
    b.extend_from_slice(body);
    b
}

#[test]
fn non_mtrk_chunks_and_trailing_bytes_warn() {
    let mut f = header(0, 1, 480);
    f.extend_from_slice(b"JNK!");
    f.extend_from_slice(&4u32.to_be_bytes());
    f.extend_from_slice(b"abcd");
    f.extend_from_slice(&mtrk(&[0x00, 0xFF, 0x2F, 0x00]));
    f.extend_from_slice(b"tail");

    let file = parse_lenient(&f).expect("lenient walk must survive a junk chunk");
    assert!(
        file.warnings.iter().any(|w| w.contains("non-MTrk chunk")),
        "expected a non-MTrk warning: {:?}",
        file.warnings
    );
    assert!(
        file.warnings.iter().any(|w| w.contains("trailing bytes")),
        "expected a trailing-bytes warning: {:?}",
        file.warnings
    );
    assert_eq!(file.tracks.len(), 1);
}

#[test]
fn clean_file_has_no_extra_chunk_warnings() {
    let mut f = header(0, 1, 480);
    f.extend_from_slice(&mtrk(&[0x00, 0xFF, 0x2F, 0x00]));
    let file = parse_lenient(&f).unwrap();
    assert!(
        file.warnings
            .iter()
            .all(|w| !w.contains("non-MTrk") && !w.contains("trailing")),
        "unexpected warnings: {:?}",
        file.warnings
    );
}

#[test]
fn bad_mthd_lengths_are_rejected() {
    // declared header shorter than the mandatory 6 bytes
    let mut f = b"MThd".to_vec();
    f.extend_from_slice(&5u32.to_be_bytes());
    f.extend_from_slice(&[0; 5]);
    assert!(parse_lenient(&f).is_err());

    // declared header overruns the actual file
    let mut f = b"MThd".to_vec();
    f.extend_from_slice(&6u32.to_be_bytes());
    f.extend_from_slice(&[0; 3]);
    assert!(parse_lenient(&f).is_err());

    // no header at all
    assert!(parse_lenient(b"NOMThd").is_err());
}

#[test]
fn smpte_division_decodes() {
    // raw[12] = 0xE7 = -25 fps, raw[13] = 40 ticks/frame
    let mut f = b"MThd".to_vec();
    f.extend_from_slice(&6u32.to_be_bytes());
    f.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0xE7, 40]);
    f.extend_from_slice(&mtrk(&[0x00, 0xFF, 0x2F, 0x00]));
    let file = parse_lenient(&f).unwrap();
    assert_eq!(
        file.division,
        Division::Smpte {
            fps: 25,
            ticks_per_frame: 40
        }
    );
}

#[test]
fn metrical_division_decodes() {
    let mut f = header(0, 1, 480);
    f.extend_from_slice(&mtrk(&[0x00, 0xFF, 0x2F, 0x00]));
    let file = parse_lenient(&f).unwrap();
    assert_eq!(file.division, Division::Metrical(480));
}

#[test]
fn riff_wrapper_is_flagged() {
    // Minimal RMID-shaped head: the container must produce the unwrap
    // warning so a round trip doesn't silently lose it.
    let mut f = b"RIFF".to_vec();
    f.extend_from_slice(&20u32.to_be_bytes());
    f.extend_from_slice(b"RMIDdata");
    f.extend_from_slice(&header(0, 1, 480)[..]);
    f.extend_from_slice(&mtrk(&[0x00, 0xFF, 0x2F, 0x00]));
    let res = parse(&f);
    if let Ok(file) = res {
        assert!(
            file.warnings.iter().any(|w| w.contains("RIFF")),
            "expected an RMID unwrap warning: {:?}",
            file.warnings
        );
    }
    // a rejected RMID is also acceptable — midly decides; only a silent
    // acceptance would be wrong
}
