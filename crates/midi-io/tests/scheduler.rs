//! Deterministic playback-scheduler coverage.
//!
//! The whole event loop runs synchronously on `FakeClock` — `wait_until_us`
//! jumps to the requested target — and fake sinks record the exact send
//! order, the requested relative offsets (`rem_us`), and the schedule µs each
//! send landed. No wall-clock sleeps, no timing tolerances.

use midi_io::{run_schedule, Clock, EventSink, Playback, PortSink};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex};

/// Manual clock: `wait_until_us` advances `now` straight to the target.
/// `overshoot` simulates wake jitter (a late wake lands past the target);
/// `stop_on_wait` pretends a transport stop during the Nth wait call.
struct FakeClock {
    now: Arc<AtomicU64>,
    overshoot: u64,
    stop_on_wait: Option<usize>,
    /// every requested wake target, in order
    waits: Vec<u64>,
}

impl FakeClock {
    fn new() -> Self {
        Self {
            now: Arc::new(AtomicU64::new(0)),
            overshoot: 0,
            stop_on_wait: None,
            waits: Vec::new(),
        }
    }
}

impl Clock for FakeClock {
    fn now_us(&self) -> u64 {
        self.now.load(Relaxed)
    }
    fn wait_until_us(&mut self, target_us: u64, stop: &AtomicBool) -> bool {
        self.waits.push(target_us);
        if self.stop_on_wait == Some(self.waits.len()) || stop.load(Relaxed) {
            stop.store(true, Relaxed);
            return false;
        }
        self.now.store(target_us + self.overshoot, Relaxed);
        true
    }
}

/// What one fake sink observed, in order.
#[derive(Debug, Clone, PartialEq)]
enum Entry {
    /// (sink index, message bytes, rem_us handed to send_at, schedule µs it ran)
    Send(usize, Vec<u8>, u64, u64),
    NotesOff(usize),
    Panic(usize),
}

/// Fake sink recording each callback verbatim. `lead` exercises the
/// lead_us() wake-early contract; `default_notes_off` selects the
/// trait-default CC payload (byte assertions) vs a marker (ordering tests).
struct RecordingSink {
    idx: usize,
    log: Arc<Mutex<Vec<Entry>>>,
    now: Arc<AtomicU64>,
    lead: u64,
    default_notes_off: bool,
}

impl RecordingSink {
    fn new(idx: usize, log: Arc<Mutex<Vec<Entry>>>, now: Arc<AtomicU64>) -> Self {
        Self {
            idx,
            log,
            now,
            lead: 0,
            default_notes_off: false,
        }
    }
}

impl EventSink for RecordingSink {
    fn lead_us(&self) -> u64 {
        self.lead
    }
    fn send_at(&mut self, bytes: &[u8], rem_us: u64) {
        self.log.lock().unwrap().push(Entry::Send(
            self.idx,
            bytes.to_vec(),
            rem_us,
            self.now.load(Relaxed),
        ));
    }
    fn panic(&mut self) {
        self.log.lock().unwrap().push(Entry::Panic(self.idx));
    }
    fn notes_off(&mut self) {
        if self.default_notes_off {
            for ch in 0u8..16 {
                self.send_at(&[0xB0 | ch, 123, 0], 0);
            }
        } else {
            self.log.lock().unwrap().push(Entry::NotesOff(self.idx));
        }
    }
}

struct Run {
    log: Arc<Mutex<Vec<Entry>>>,
    clock: FakeClock,
    pos: Arc<AtomicU64>,
}

/// Run the schedule to completion on a fake clock.
fn run(
    events: Vec<(u64, usize, Vec<u8>)>,
    start_us: u64,
    loop_from_us: Option<u64>,
    sink_count: usize,
    configure: impl Fn(&mut FakeClock, &mut Vec<RecordingSink>),
) -> Run {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut clock = FakeClock::new();
    let mut recorders: Vec<RecordingSink> = (0..sink_count)
        .map(|i| RecordingSink::new(i, log.clone(), clock.now.clone()))
        .collect();
    configure(&mut clock, &mut recorders);
    let mut sinks: Vec<Box<dyn EventSink>> = recorders
        .into_iter()
        .map(|s| Box::new(s) as Box<dyn EventSink>)
        .collect();
    let stop = AtomicBool::new(false);
    let pos = Arc::new(AtomicU64::new(0));
    run_schedule(
        &mut clock,
        &mut sinks,
        &events,
        start_us,
        loop_from_us,
        &stop,
        &pos,
    );
    Run { log, clock, pos }
}

fn sends(log: &[Entry]) -> Vec<(usize, Vec<u8>, u64, u64)> {
    log.iter()
        .filter_map(|e| match e {
            Entry::Send(s, b, r, a) => Some((*s, b.clone(), *r, *a)),
            _ => None,
        })
        .collect()
}

fn on(key: u8) -> Vec<u8> {
    vec![0x90, key, 100]
}

#[test]
fn events_send_in_order_at_their_deadlines() {
    let r = run(
        vec![(100, 0, on(60)), (200, 0, on(62)), (400, 0, on(64))],
        0,
        None,
        1,
        |_, _| {},
    );
    let got = sends(&r.log.lock().unwrap());
    assert_eq!(
        got,
        vec![
            (0, on(60), 0, 100),
            (0, on(62), 0, 200),
            (0, on(64), 0, 400),
        ],
        "each event must land at its own deadline with rem=0"
    );
    // the clock was asked to wake at exactly those points
    assert_eq!(r.clock.waits, vec![100, 200, 400]);
}

#[test]
fn same_tick_events_keep_file_order() {
    let r = run(
        vec![
            (100, 0, on(60)),
            (100, 0, vec![0xB0, 1, 127]),
            (100, 0, on(62)),
            (200, 0, on(64)),
        ],
        0,
        None,
        1,
        |_, _| {},
    );
    let got: Vec<Vec<u8>> = sends(&r.log.lock().unwrap())
        .into_iter()
        .map(|(_, b, _, _)| b)
        .collect();
    assert_eq!(got, vec![on(60), vec![0xB0, 1, 127], on(62), on(64)]);
}

#[test]
fn seek_skips_events_before_start_us() {
    let r = run(
        vec![(100, 0, on(60)), (200, 0, on(62)), (300, 0, on(64))],
        200,
        None,
        1,
        |_, _| {},
    );
    // `at` is fake-clock µs from the run epoch: seeking to 200 re-bases the
    // clock, so the 200µs event fires at clock 0 and the 300µs at clock 100
    let got = sends(&r.log.lock().unwrap());
    assert_eq!(got, vec![(0, on(62), 0, 0), (0, on(64), 0, 100)]);
    // position reports timeline time, not clock time
    assert_eq!(r.pos.load(Relaxed), 300);
}

#[test]
fn seek_past_end_sends_nothing() {
    let r = run(vec![(100, 0, on(60))], 9_999, None, 1, |_, _| {});
    let log = r.log.lock().unwrap();
    assert!(sends(&log).is_empty());
    // every exit still ends in panic()
    assert_eq!(log.last(), Some(&Entry::Panic(0)));
}

#[test]
fn stop_before_first_event_sends_nothing_but_panics() {
    let r = run(vec![(100, 0, on(60))], 0, None, 1, |c, _| {
        c.stop_on_wait = Some(1);
    });
    let log = r.log.lock().unwrap();
    assert!(sends(&log).is_empty());
    assert_eq!(log.last(), Some(&Entry::Panic(0)));
}

#[test]
fn stop_mid_schedule_skips_rest_and_panics() {
    let r = run(
        vec![(100, 0, on(60)), (200, 0, on(62)), (300, 0, on(64))],
        0,
        None,
        1,
        |c, _| c.stop_on_wait = Some(3),
    );
    let log = r.log.lock().unwrap();
    assert_eq!(sends(&log), vec![(0, on(60), 0, 100), (0, on(62), 0, 200)]);
    assert_eq!(log.last(), Some(&Entry::Panic(0)));
}

#[test]
fn loop_wrap_notes_off_then_replays_from_loop_point() {
    // events at 0 and 400, looping from 200: the first pass sends both, then
    // each wrap replays only the ≥200 tail, re-anchored to the wrap time.
    let r = run(
        vec![(0, 0, on(60)), (400, 0, on(62))],
        0,
        Some(200),
        1,
        |c, _| c.stop_on_wait = Some(4),
    );
    let log = r.log.lock().unwrap();
    assert_eq!(
        *log,
        vec![
            Entry::Send(0, on(60), 0, 0),
            Entry::Send(0, on(62), 0, 400),
            Entry::NotesOff(0),
            // pass 2: epoch re-anchors to 400, base = 200 → the 400µs event
            // fires at 400 + (400-200) = 600
            Entry::Send(0, on(62), 0, 600),
            Entry::NotesOff(0),
            Entry::Panic(0),
        ]
    );
    assert_eq!(r.clock.waits, vec![0, 400, 600, 800]);
    // the aborted pass-3 wait left the playhead at the loop point
    assert_eq!(r.pos.load(Relaxed), 200);
}

#[test]
fn loop_wrap_releases_notes_with_cc123_only() {
    // default notes_off payload: All Notes Off on every channel, and never
    // Reset All Controllers / All Sound Off (those belong to panic)
    let r = run(
        vec![(0, 0, on(60)), (400, 0, on(62))],
        0,
        Some(0),
        1,
        |c, sinks| {
            c.stop_on_wait = Some(3);
            for s in sinks.iter_mut() {
                s.default_notes_off = true;
            }
        },
    );
    let log = r.log.lock().unwrap();
    let sent = sends(&log);
    let notes_off: Vec<&Vec<u8>> = sent
        .iter()
        .map(|(_, b, _, _)| b)
        .filter(|b| b.len() == 3 && b[0] & 0xF0 == 0xB0 && b[1] == 123)
        .collect();
    assert_eq!(notes_off.len(), 16, "one All Notes Off per channel");
    assert!(sent.iter().all(|(_, b, _, _)| {
        !(b.len() == 3 && b[0] & 0xF0 == 0xB0 && (b[1] == 120 || b[1] == 121))
    }));
}

#[test]
fn end_of_timeline_notes_off_then_panic() {
    let r = run(vec![(0, 0, on(60))], 0, None, 1, |_, _| {});
    let log = r.log.lock().unwrap();
    assert_eq!(
        *log,
        vec![
            Entry::Send(0, on(60), 0, 0),
            Entry::NotesOff(0),
            Entry::Panic(0),
        ]
    );
}

#[test]
fn every_sink_gets_notes_off_and_panic() {
    let r = run(vec![(0, 1, on(60))], 0, None, 2, |_, _| {});
    let log = r.log.lock().unwrap();
    assert_eq!(
        *log,
        vec![
            Entry::Send(1, on(60), 0, 0),
            Entry::NotesOff(0),
            Entry::NotesOff(1),
            Entry::Panic(0),
            Entry::Panic(1),
        ]
    );
}

#[test]
fn events_route_to_their_own_sink() {
    let r = run(
        vec![
            (100, 0, on(60)),
            (100, 1, on(64)),
            (200, 1, on(65)),
            (200, 0, on(62)),
        ],
        0,
        None,
        2,
        |_, _| {},
    );
    let got = sends(&r.log.lock().unwrap());
    assert_eq!(
        got,
        vec![
            (0, on(60), 0, 100),
            (1, on(64), 0, 100),
            (1, on(65), 0, 200),
            (0, on(62), 0, 200),
        ]
    );
}

#[test]
fn events_for_missing_sinks_are_skipped() {
    let r = run(
        vec![(100, 9, on(60)), (200, 0, on(62))],
        0,
        None,
        1,
        |_, _| {},
    );
    let got = sends(&r.log.lock().unwrap());
    assert_eq!(got, vec![(0, on(62), 0, 200)]);
}

#[test]
fn sink_lead_time_wakes_early_and_reports_rem() {
    // lead_us() destinations are woken `lead` early; send_at receives the
    // remaining µs so the sink can offset its own clock (plugin sinks turn
    // it into a sample offset).
    let r = run(
        vec![(100_000, 0, on(60)), (100_000, 1, on(62))],
        0,
        None,
        2,
        |_, sinks| sinks[0].lead = 50_000,
    );
    let got = sends(&r.log.lock().unwrap());
    assert_eq!(
        got,
        vec![
            (0, on(60), 50_000, 50_000), // woke 50ms early, rem = lead
            (1, on(62), 0, 100_000),
        ]
    );
    assert_eq!(r.clock.waits, vec![50_000, 100_000]);
}

#[test]
fn wake_jitter_sends_late_with_zero_rem() {
    // the clock lands 30µs past the wake target — jitter is visible in the
    // recorded send time while rem floors at 0 instead of going negative
    let r = run(vec![(100, 0, on(60))], 0, None, 1, |c, _| {
        c.overshoot = 30;
    });
    let got = sends(&r.log.lock().unwrap());
    assert_eq!(got, vec![(0, on(60), 0, 130)]);
}

#[test]
fn long_sysex_messages_send_verbatim_in_one_send() {
    // a >1KiB SysEx: no splitting or reordering at the scheduler — port-level
    // serialization of long messages is the sink's business
    let mut msg = vec![0xF0, 0x7E, 0x7F, 0x09, 0x01];
    msg.extend(std::iter::repeat(0x5Au8).take(1024));
    msg.push(0xF7);
    let len = msg.len();
    let r = run(
        vec![(100, 0, msg.clone()), (200, 0, on(60))],
        0,
        None,
        1,
        |_, _| {},
    );
    let got = sends(&r.log.lock().unwrap());
    assert_eq!(got[0].1, msg);
    assert_eq!(got[0].1.len(), len);
    assert_eq!(got[1].1, on(60));
}

#[test]
fn position_us_tracks_schedule_and_wrap() {
    let r = run(
        vec![(0, 0, on(60)), (400, 0, on(62))],
        0,
        Some(0),
        1,
        |c, _| c.stop_on_wait = Some(4),
    );
    // pass 2 sent the 0µs event then aborts waiting for the next → pos = 0
    assert_eq!(r.pos.load(Relaxed), 0);
}

#[test]
fn loop_past_last_event_exits_instead_of_spinning() {
    let r = run(vec![(0, 0, on(60))], 0, Some(500), 1, |_, _| {});
    let log = r.log.lock().unwrap();
    assert_eq!(
        *log,
        vec![
            Entry::Send(0, on(60), 0, 0),
            Entry::NotesOff(0),
            Entry::Panic(0),
        ]
    );
}

/// Thin end-to-end check of the threaded `Playback` API — timing assertions
/// live in the fake-clock tests; this only proves start/stop plumb through.
#[test]
fn playback_start_sends_and_stops() {
    let log = Arc::new(Mutex::new(Vec::new()));
    struct S(Arc<Mutex<Vec<Vec<u8>>>>);
    impl EventSink for S {
        fn send_at(&mut self, bytes: &[u8], _rem: u64) {
            self.0.lock().unwrap().push(bytes.to_vec());
        }
        fn panic(&mut self) {
            self.0.lock().unwrap().push(vec![0xFF]);
        }
    }
    let mut pb = Playback::start(
        vec![Box::new(S(log.clone()))],
        vec![(0, 0, on(60))],
        0,
        None,
    );
    // wait for the send — stop() landing before the thread's first pass
    // would race the event; the schedule drains on its own so this returns
    // promptly (timeout only guards a genuinely stuck thread)
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !log.lock().unwrap().iter().any(|b| b == &on(60)) {
        assert!(
            std::time::Instant::now() < deadline,
            "playback thread never delivered the 0µs event"
        );
        std::thread::yield_now();
    }
    pb.stop(); // joins the thread — the schedule has already drained
    let got = log.lock().unwrap().clone();
    assert_eq!(got.last(), Some(&vec![0xFF]));
}

/// Optional hardware test — NOT part of CI. Sends through a real port and
/// measures wake jitter against wall time. Run explicitly with:
///   set MIDI_TEST_PORT=<loopback port name>
///   cargo test -p midi-io --test scheduler -- --ignored
#[test]
#[ignore = "requires a real MIDI port; set MIDI_TEST_PORT"]
fn hardware_wake_jitter() {
    let Ok(name) = std::env::var("MIDI_TEST_PORT") else {
        eprintln!("MIDI_TEST_PORT unset — skipping hardware test");
        return;
    };
    let out = midi_io::Output::open_named(&name).expect("open MIDI_TEST_PORT");
    let log = Arc::new(Mutex::new(Vec::new()));
    struct JitterSink {
        out: PortSink,
        t0: std::time::Instant,
        log: Arc<Mutex<Vec<i64>>>,
    }
    impl EventSink for JitterSink {
        fn send_at(&mut self, bytes: &[u8], rem: u64) {
            self.log
                .lock()
                .unwrap()
                .push(self.t0.elapsed().as_micros() as i64);
            self.out.send_at(bytes, rem);
        }
        fn panic(&mut self) {
            self.out.panic();
        }
    }
    let t0 = std::time::Instant::now();
    // a note every 100ms for 2s, off halfway between
    let events: Vec<(u64, usize, Vec<u8>)> = (0..20u64)
        .flat_map(|i| {
            vec![
                (i * 100_000, 0usize, on(60)),
                (i * 100_000 + 50_000, 0, vec![0x80, 60, 0]),
            ]
        })
        .collect();
    let mut pb = Playback::start(
        vec![Box::new(JitterSink {
            out: PortSink::new(out),
            t0,
            log: log.clone(),
        })],
        events,
        0,
        None,
    );
    pb.stop();
    let log = log.lock().unwrap();
    eprintln!("send times (µs): {:?}", *log);
    // loose bound: sends fire within 5ms of their deadline. The first send's
    // epoch is the Playback::start call, ~µs after t0 — folded into the bound.
    for (i, &got) in log.iter().enumerate() {
        let want = (i as i64 / 2) * 100_000 + (i as i64 % 2) * 50_000;
        assert!(
            (got - want).abs() < 5_000,
            "send {i}: {got}µs vs expected {want}µs"
        );
    }
}
