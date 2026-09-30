#![no_main]
use arbitrary::Arbitrary;
use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use smf_core::{Division, Event, EventKind, Track, WriteOptions};

// Structured writer fuzzing: arbitrary tracks/events -> write -> parse.
// Counts and payload sizes are bounded so iterations stay fast; channel
// status/data bytes are masked to the valid range the API contract uses.
#[derive(Arbitrary, Debug)]
struct Input {
    format: u16,
    metrical: bool,
    division: u16,
    tracks: Vec<TrackIn>,
}

#[derive(Arbitrary, Debug)]
struct TrackIn {
    events: Vec<Ev>,
}

#[derive(Arbitrary, Debug)]
struct Ev {
    tick: u64,
    seq: u32,
    kind: KindIn,
}

#[derive(Arbitrary, Debug)]
enum KindIn {
    Channel { status: u8, d0: u8, d1: u8, len1: bool },
    Meta { ty: u8, data: Vec<u8> },
    SysEx(Vec<u8>),
    Escape(Vec<u8>),
}

fuzz_target!(|input: Input| {
    let tracks: Vec<Track> = input
        .tracks
        .iter()
        .take(8)
        .map(|t| Track {
            events: t
                .events
                .iter()
                .take(512)
                .map(|e| Event {
                    tick: e.tick,
                    seq: e.seq,
                    raw_body: None,
                    kind: match &e.kind {
                        KindIn::Channel { status, d0, d1, len1 } => EventKind::Channel {
                            status: 0x80 + (status % 0x70),
                            data: [d0 & 0x7F, d1 & 0x7F],
                            len: if *len1 { 1 } else { 2 },
                        },
                        KindIn::Meta { ty, data } => EventKind::Meta {
                            meta_type: *ty,
                            data: Bytes::copy_from_slice(&data[..data.len().min(256)]),
                        },
                        KindIn::SysEx(data) => EventKind::SysEx(Bytes::copy_from_slice(
                            &data[..data.len().min(256)],
                        )),
                        KindIn::Escape(data) => EventKind::Escape(Bytes::copy_from_slice(
                            &data[..data.len().min(256)],
                        )),
                    },
                })
                .collect(),
        })
        .collect();
    let division = if input.metrical {
        Division::Metrical(input.division)
    } else {
        // keep fps spec-valid so the strict-parse invariant below holds for
        // every generated file (24/25/29/30 are the only legal rates)
        let fps = [24u8, 25, 29, 30][(input.division >> 8) as usize % 4];
        Division::Smpte {
            fps,
            ticks_per_frame: input.division as u8,
        }
    };
    let opts = WriteOptions::default();
    // format is spec-bounded (0-2); out-of-range values are the caller's
    // contract violation, not a writer bug
    let out = smf_core::write(input.format % 3, division, &tracks, opts);

    match smf_core::parse_strict(&out) {
        Ok(parsed) => {
            // strict-valid output must round-trip byte-exact
            let out2 = smf_core::write(parsed.format, parsed.division, &parsed.tracks, opts);
            assert_eq!(out, out2, "writer output must be a fixpoint; input={input:?}");
        }
        Err(_) => {
            // inputs the writer re-emits without validation (e.g. EOT meta
            // carrying a payload) may not strict-parse — but the guarded
            // pipeline must still accept its own output without panic
            smf_core::parse(&out).expect("writer output must parse through the guarded path");
        }
    }
});
