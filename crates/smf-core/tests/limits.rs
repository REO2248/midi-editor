//! Pathological-input limits: oversized files, excessive track/event
//! counts, and giant event payloads must fail with `Error::LimitExceeded`
//! — a category distinct from malformed input — rather than allocating
//! unboundedly.

use smf_core::{Error, Limits};

fn header(ntrks: u16) -> Vec<u8> {
    let mut v = b"MThd\x00\x00\x00\x06\x00\x01".to_vec();
    v.extend_from_slice(&ntrks.to_be_bytes());
    v.extend_from_slice(&480u16.to_be_bytes());
    v
}

fn trk(events: &[Vec<u8>]) -> Vec<u8> {
    let body: Vec<u8> = events.concat();
    let mut v = b"MTrk".to_vec();
    v.extend_from_slice(&(body.len() as u32).to_be_bytes());
    v.extend_from_slice(&body);
    v
}

fn note_on() -> Vec<u8> {
    vec![0x00, 0x90, 60, 100] // delta 0, note on
}

fn tiny_limits() -> Limits {
    Limits {
        max_file_bytes: 128,
        max_tracks: 2,
        max_events: 4,
        max_track_events: 3,
        max_event_payload: 8,
    }
}

fn assert_limit(err: Error, name: &str) {
    match err {
        Error::LimitExceeded { limit, .. } => assert_eq!(limit, name),
        other => panic!("expected LimitExceeded({name}), got {other:?}"),
    }
}

#[test]
fn file_size_limit() {
    let raw = vec![0u8; 200];
    let err = smf_core::parse_with_limits(&raw, &tiny_limits()).unwrap_err();
    assert_limit(err, "file size");
}

#[test]
fn track_count_limit_strict_and_lenient() {
    // three valid MTrk chunks — over the two-track test limit. Strict parse
    // succeeds syntactically, so this exercises file_from_map's count check.
    let mut raw = header(3);
    for _ in 0..3 {
        raw.extend_from_slice(&trk(&[vec![0x00, 0xFF, 0x2F, 0x00]]));
    }
    let err = smf_core::parse_with_limits(&raw, &tiny_limits()).unwrap_err();
    assert_limit(err, "track count");

    // lenient path gets the same rejection
    let err = smf_core::parse_lenient_with_limits(&raw, &tiny_limits()).unwrap_err();
    assert_limit(err, "track count");
}

#[test]
fn event_count_limits() {
    // 5 events in one track: over per-track (3) and total (4) — per-track
    // trips first
    let raw = {
        let mut r = header(1);
        r.extend_from_slice(&trk(&vec![note_on(); 5]));
        r
    };
    let err = smf_core::parse_with_limits(&raw, &tiny_limits()).unwrap_err();
    assert_limit(err, "events in a track");

    // 2 events in each of 2 tracks (within per-track) but over total 4...
    // needs 5 events: 3 + 3 crosses total at 5th event before per-track's 4th
    let mut limits = tiny_limits();
    limits.max_events = 5;
    let raw = {
        let mut r = header(2);
        for _ in 0..2 {
            r.extend_from_slice(&trk(&vec![note_on(); 3]));
        }
        r
    };
    let err = smf_core::parse_with_limits(&raw, &limits).unwrap_err();
    assert_limit(err, "total events");
}

#[test]
fn event_payload_limit() {
    // a meta event whose declared payload exceeds the limit
    let big_meta = {
        let mut e = vec![0x00, 0xFF, 0x01, 0x10]; // delta 0, meta type 1, len 16
        e.extend_from_slice(&[b'x'; 16]);
        e
    };
    let raw = {
        let mut r = header(1);
        r.extend_from_slice(&trk(&[big_meta]));
        r
    };
    let err = smf_core::parse_with_limits(&raw, &tiny_limits()).unwrap_err();
    assert_limit(err, "event payload");

    // lenient path too (force it by mangling the strict path? same input
    // is clean — test the lenient entry directly)
    let err = smf_core::parse_lenient_with_limits(&raw, &tiny_limits()).unwrap_err();
    assert_limit(err, "event payload");
}

#[test]
fn unlimited_accepts_what_limits_reject() {
    let mut raw = header(3);
    for _ in 0..3 {
        raw.extend_from_slice(&trk(&vec![note_on(); 5]));
    }
    assert!(smf_core::parse_with_limits(&raw, &tiny_limits()).is_err());
    let f = smf_core::parse_with_limits(&raw, &Limits::unlimited()).unwrap();
    assert_eq!(f.tracks.len(), 3);
}

#[test]
fn limits_are_separate_from_malformed() {
    // malformed input still reports Parse, not LimitExceeded
    let err = smf_core::parse_with_limits(b"garbage", &tiny_limits()).unwrap_err();
    assert!(matches!(err, Error::Parse(_)));
}

#[test]
fn normal_file_within_limits_round_trips() {
    // a small but well-formed file passes the real default limits and
    // round-trips byte-exactly
    let mut raw = header(1);
    raw.extend_from_slice(&trk(&[
        vec![0x00, 0x90, 60, 100],
        vec![0x60, 0x80, 60, 0],
        vec![0x00, 0xFF, 0x2F, 0x00],
    ]));
    let f = smf_core::parse(&raw).unwrap();
    let out = smf_core::write(f.format, f.division, &f.tracks, Default::default());
    assert_eq!(out, raw);
}
