//! MIDI I/O layer. WinMM backend via midir on Windows.
//! loopMIDI cables and the Windows MIDI Services built-in loopback appear
//! as ordinary output ports — no special-casing.

use midir::{Ignore, MidiInput, MidiOutput, MidiOutputConnection};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A stable output-destination identity — what the UI persists and MCP tools
/// name. Ports are addressed by NAME (indexes shift as devices come and go).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Destination {
    /// midir output port, resolved by name at open time
    MidiPort { port_name: String },
    /// hosted VST3 plugin instance, by bundle path
    Plugin { plugin_path: String },
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("midi init failed: {0}")]
    Init(String),
    #[error("connect failed: {0}")]
    Connect(String),
    #[error("send failed: {0}")]
    Send(String),
}

#[derive(Debug, Clone)]
pub struct PortInfo {
    pub index: usize,
    pub name: String,
}

pub fn list_outputs() -> Result<Vec<PortInfo>, Error> {
    let out = MidiOutput::new("midi-editor").map_err(|e| Error::Init(e.to_string()))?;
    Ok(out
        .ports()
        .iter()
        .enumerate()
        .map(|(i, p)| PortInfo {
            index: i,
            name: out.port_name(p).unwrap_or_else(|_| "<unknown>".into()),
        })
        .collect())
}

pub fn list_inputs() -> Result<Vec<PortInfo>, Error> {
    let mut inp = MidiInput::new("midi-editor").map_err(|e| Error::Init(e.to_string()))?;
    inp.ignore(Ignore::None);
    Ok(inp
        .ports()
        .iter()
        .enumerate()
        .map(|(i, p)| PortInfo {
            index: i,
            name: inp.port_name(p).unwrap_or_else(|_| "<unknown>".into()),
        })
        .collect())
}

/// One open output connection. `MidiOutputConnection` is `Send`; the playback
/// thread owns it.
pub struct Output {
    conn: MidiOutputConnection,
    pub name: String,
}

impl Output {
    pub fn open(index: usize) -> Result<Self, Error> {
        let out = MidiOutput::new("midi-editor").map_err(|e| Error::Init(e.to_string()))?;
        let port = out
            .ports()
            .into_iter()
            .nth(index)
            .ok_or_else(|| Error::Connect(format!("port {index} not found")))?;
        let name = out.port_name(&port).unwrap_or_else(|_| "<unknown>".into());
        let conn = out
            .connect(&port, "midi-editor-out")
            .map_err(|e| Error::Connect(e.to_string()))?;
        Ok(Self { conn, name })
    }

    /// Open the first output port whose name equals `name` — the stable way
    /// to address ports across sessions.
    pub fn open_named(name: &str) -> Result<Self, Error> {
        let out = MidiOutput::new("midi-editor").map_err(|e| Error::Init(e.to_string()))?;
        let port = out
            .ports()
            .into_iter()
            .find(|p| out.port_name(p).map(|n| n == name).unwrap_or(false))
            .ok_or_else(|| Error::Connect(format!("port '{name}' not found")))?;
        let conn = out
            .connect(&port, "midi-editor-out")
            .map_err(|e| Error::Connect(e.to_string()))?;
        Ok(Self {
            conn,
            name: name.to_string(),
        })
    }

    pub fn send(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.conn
            .send(bytes)
            .map_err(|e| Error::Send(format!("{e}")))
    }

    /// All-notes-off + reset all controllers on every channel (panic).
    pub fn panic(&mut self) {
        for ch in 0u8..16 {
            let _ = self.send(&[0xB0 | ch, 123, 0]); // All Notes Off
            let _ = self.send(&[0xB0 | ch, 121, 0]); // Reset All Controllers
            let _ = self.send(&[0xB0 | ch, 120, 0]); // All Sound Off
        }
    }
}

/// Delivery-latency counters for a recording input — the jitter that WOULD
/// have been recorded had we kept timestamping by callback delivery. `gap`
/// = arrival-stamp minus device-stamp, in µs; shared so the app can read a
/// summary when the take finishes.
#[derive(Debug)]
pub struct InputDiag {
    /// callbacks that carried a backend timestamp
    pub stamped: std::sync::atomic::AtomicU64,
    /// callbacks with no backend timestamp (arrival fallback)
    pub unstamped: std::sync::atomic::AtomicU64,
    /// worst callback delivery delay seen (µs)
    pub gap_max_us: std::sync::atomic::AtomicU64,
    /// most recent delivery delay (µs)
    pub gap_last_us: std::sync::atomic::AtomicU64,
}

impl InputDiag {
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            stamped: 0.into(),
            unstamped: 0.into(),
            gap_max_us: 0.into(),
            gap_last_us: 0.into(),
        })
    }
    fn note(&self, stamped: bool, gap_us: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        if stamped {
            self.stamped.fetch_add(1, Relaxed);
            self.gap_last_us.store(gap_us, Relaxed);
            self.gap_max_us.fetch_max(gap_us, Relaxed);
        } else {
            self.unstamped.fetch_add(1, Relaxed);
        }
    }
}

/// How to timestamp incoming messages.
#[derive(Debug, Default)]
pub struct InputOpts {
    /// subtracted from every stamped µs — manual compensation for a known
    /// input pipeline delay (keyboard/USB/driver), in µs
    pub latency_us: u64,
    /// shared jitter counters, updated per callback
    pub diag: Option<std::sync::Arc<InputDiag>>,
}

/// Maps backend device timestamps onto the "µs since open" domain used for
/// recording. midir's callback timestamp is backend-defined (µs since
/// `midiInStart` on WinMM): a device-side clock that does not shift when the
/// callback thread is scheduled late. The first timestamped message anchors
/// the map — it is stamped by arrival, keeping the recording's t=0 on the
/// same base callers already use — and later messages advance by device-time
/// deltas, so callback delivery delay no longer moves recorded placement.
/// A message with no backend stamp falls back to arrival time.
pub struct Timebase {
    t0: std::time::Instant,
    /// (first backend µs, its arrival instant)
    anchor: Option<(u64, std::time::Instant)>,
    latency_us: u64,
    diag: Option<std::sync::Arc<InputDiag>>,
}

impl Timebase {
    pub fn new(latency_us: u64, diag: Option<std::sync::Arc<InputDiag>>) -> Self {
        Self::new_at(latency_us, diag, std::time::Instant::now())
    }

    fn new_at(
        latency_us: u64,
        diag: Option<std::sync::Arc<InputDiag>>,
        t0: std::time::Instant,
    ) -> Self {
        Self {
            t0,
            anchor: None,
            latency_us,
            diag,
        }
    }

    /// `dev_us` = backend timestamp in µs (0 = backend supplied none);
    /// `now` = the instant the callback delivered the message.
    /// Returns µs since `new()`, latency-compensated, never negative.
    pub fn stamp(&mut self, dev_us: u64, now: std::time::Instant) -> u64 {
        let arrival = now
            .saturating_duration_since(self.t0)
            .as_micros() as u64;
        let raw = match (dev_us != 0, self.anchor) {
            (true, Some((d0, i0))) => {
                // anchor arrival + device-time delta: `now` enters only
                // through the anchor, so a delayed callback cannot shift it
                let base = i0.saturating_duration_since(self.t0).as_micros() as u64;
                base.saturating_add(dev_us.saturating_sub(d0))
            }
            _ => arrival,
        };
        if dev_us != 0 && self.anchor.is_none() {
            self.anchor = Some((dev_us, now));
        }
        if let Some(d) = &self.diag {
            d.note(dev_us != 0, arrival.saturating_sub(raw));
        }
        raw.saturating_sub(self.latency_us)
    }
}

/// One open input connection. Timestamps each incoming message in µs relative
/// to the moment `open` returned (not midir's platform epoch) so callers can
/// place recorded events on the playback timeline directly. When the backend
/// supplies a device timestamp (WinMM does), `Timebase` uses it so callback
/// scheduling latency is not recorded as timing error.
pub struct Input {
    // connection must stay alive to keep receiving
    _conn: midir::MidiInputConnection<()>,
    pub name: String,
}

impl Input {
    /// `cb(us_since_open, bytes)` is called on midir's callback thread.
    pub fn open<F>(index: usize, cb: F) -> Result<Self, Error>
    where
        F: FnMut(u64, &[u8]) + Send + 'static,
    {
        Self::open_opts(index, InputOpts::default(), cb)
    }

    pub fn open_opts<F>(index: usize, opts: InputOpts, cb: F) -> Result<Self, Error>
    where
        F: FnMut(u64, &[u8]) + Send + 'static,
    {
        let mut inp = MidiInput::new("midi-editor-in").map_err(|e| Error::Init(e.to_string()))?;
        inp.ignore(Ignore::None);
        let port = inp
            .ports()
            .into_iter()
            .nth(index)
            .ok_or_else(|| Error::Connect(format!("input {index} not found")))?;
        let name = inp.port_name(&port).unwrap_or_else(|_| "<unknown>".into());
        Self::connect_on(inp, port, name, opts, cb)
    }

    pub fn open_named<F>(name: &str, cb: F) -> Result<Self, Error>
    where
        F: FnMut(u64, &[u8]) + Send + 'static,
    {
        Self::open_named_opts(name, InputOpts::default(), cb)
    }

    pub fn open_named_opts<F>(name: &str, opts: InputOpts, cb: F) -> Result<Self, Error>
    where
        F: FnMut(u64, &[u8]) + Send + 'static,
    {
        let mut inp = MidiInput::new("midi-editor-in").map_err(|e| Error::Init(e.to_string()))?;
        inp.ignore(Ignore::None);
        let port = inp
            .ports()
            .into_iter()
            .find(|p| inp.port_name(p).map(|n| n == name).unwrap_or(false))
            .ok_or_else(|| Error::Connect(format!("input '{name}' not found")))?;
        let pname = inp.port_name(&port).unwrap_or_else(|_| name.to_string());
        Self::connect_on(inp, port, pname, opts, cb)
    }

    fn connect_on<F>(
        inp: MidiInput,
        port: midir::MidiInputPort,
        name: String,
        opts: InputOpts,
        mut cb: F,
    ) -> Result<Self, Error>
    where
        F: FnMut(u64, &[u8]) + Send + 'static,
    {
        let mut tb = Timebase::new(opts.latency_us, opts.diag);
        let conn = inp
            .connect(
                &port,
                "midi-editor-in",
                move |ts, bytes, _| cb(tb.stamp(ts, std::time::Instant::now()), bytes),
                (),
            )
            .map_err(|e| Error::Connect(e.to_string()))?;
        Ok(Self { _conn: conn, name })
    }
}

/// A destination a playback thread can deliver raw channel-message bytes to.
/// Implementors: `PortSink` (WinMM port) and the output crate's plugin sink.
pub trait EventSink: Send {
    /// How long before the scheduled deadline the thread should wake the sink:
    /// audio-clock destinations queue the event with a sample offset instead of
    /// firing on the wall clock. Ports use the default (send at the deadline).
    fn lead_us(&self) -> u64 {
        0
    }
    /// Deliver one event `rem_us` µs before its scheduled time.
    fn send_at(&mut self, bytes: &[u8], rem_us: u64);
    /// All-notes-off / reset — called on stop and at end of timeline.
    fn panic(&mut self);
    /// Release sounding notes without the full reset: All Notes Off on every
    /// channel, leaving controller state and release tails intact. Called at
    /// loop boundaries, where chased state follows immediately.
    fn notes_off(&mut self) {
        for ch in 0u8..16 {
            self.send_at(&[0xB0 | ch, 123, 0], 0);
        }
    }
}

/// `EventSink` over a `MidiOutputConnection`.
pub struct PortSink {
    out: Output,
    /// a port that disappeared mid-play would otherwise fail every event;
    /// one log line is enough to notice it
    warned_dead: bool,
}

impl PortSink {
    pub fn new(out: Output) -> Self {
        Self {
            out,
            warned_dead: false,
        }
    }
}

impl EventSink for PortSink {
    fn send_at(&mut self, bytes: &[u8], _rem_us: u64) {
        if let Err(e) = self.out.send(bytes) {
            if !self.warned_dead {
                self.warned_dead = true;
                tracing::warn!("midi port '{}' stopped accepting events: {e}", self.out.name);
            }
        }
    }
    fn panic(&mut self) {
        self.out.panic();
    }
}

/// Raise the OS scheduler/timer resolution for the duration of playback so
/// the 2 ms sleep granularity actually lands near 1 ms (Windows defaults to
/// ~15.6 ms). Per-process scope on Win10 2004+; released on thread end.
#[cfg(windows)]
fn set_timer_resolution(ms: u32) {
    extern "system" {
        fn timeBeginPeriod(u: u32) -> u32;
        fn timeEndPeriod(u: u32) -> u32;
    }
    unsafe {
        if ms == 0 {
            timeEndPeriod(1);
        } else {
            timeBeginPeriod(ms);
        }
    }
}
#[cfg(not(windows))]
fn set_timer_resolution(_ms: u32) {}

/// Scheduled playback on a dedicated thread.
///
/// The caller snapshots the timeline as `(absolute µs, sink index, message
/// bytes)` triples; the thread sleeps until each deadline and sends verbatim.
/// Meta events never reach a sink — filtering is the caller's job. SysEx
/// reaches sinks as complete `F0 … F7` wire messages; the caller joins SMF
/// split packets, and a port send blocks until the transmission finishes
/// (WinMM serializes long messages), which delays later events on that sink.
/// `position_us` is updated as the schedule advances so the UI can draw a
/// playhead.
pub struct Playback {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    position_us: std::sync::Arc<std::sync::atomic::AtomicU64>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Playback {
    /// `events` must be sorted by absolute µs. `start_us` seeks: events before
    /// it are skipped and the clock starts at `start_us`. Each event carries
    /// the index of the sink to deliver it to. With `loop_from_us`, reaching
    /// the end all-notes-offs every sink and restarts the schedule at that
    /// point — sinks (and VST3 audio streams) stay alive across the boundary.
    pub fn start(
        mut sinks: Vec<Box<dyn EventSink>>,
        events: Vec<(u64, usize, Vec<u8>)>,
        start_us: u64,
        loop_from_us: Option<u64>,
    ) -> Self {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let position_us = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (stop2, pos2) = (stop.clone(), position_us.clone());
        let thread = std::thread::spawn(move || {
            use std::sync::atomic::Ordering::Relaxed;
            set_timer_resolution(1);
            let mut base_us = start_us;
            let mut t0 = std::time::Instant::now();
            let mut i = events.partition_point(|(us, _, _)| *us < base_us);
            'outer: loop {
                while i < events.len() {
                    if stop2.load(Relaxed) {
                        break 'outer;
                    }
                    let (us, sink_idx, bytes) = &events[i];
                    i += 1;
                    let us = *us;
                    let Some(sink) = sinks.get_mut(*sink_idx) else { continue };
                    let target = t0 + std::time::Duration::from_micros(us - base_us)
                        - std::time::Duration::from_micros(sink.lead_us());
                    loop {
                        let now = std::time::Instant::now();
                        if now >= target {
                            break;
                        }
                        if stop2.load(Relaxed) {
                            break 'outer;
                        }
                        let rem = target - now;
                        if rem > std::time::Duration::from_millis(2) {
                            std::thread::sleep(rem.min(std::time::Duration::from_millis(2)));
                        } else {
                            std::hint::spin_loop();
                        }
                    }
                    pos2.store(us, Relaxed);
                    let deadline = t0 + std::time::Duration::from_micros(us - base_us);
                    let rem = deadline
                        .saturating_duration_since(std::time::Instant::now())
                        .as_micros() as u64;
                    sink.send_at(bytes, rem);
                }
                // loop wrap: release notes but keep tails and controller
                // state — the schedule restarts with chase events at the
                // loop point, which re-establish whatever should sound
                for s in &mut sinks {
                    s.notes_off();
                }
                match loop_from_us {
                    Some(ls) => {
                        let ni = events.partition_point(|(us, _, _)| *us < ls);
                        // nothing to replay → don't spin on panic forever
                        if ni >= events.len() {
                            break;
                        }
                        base_us = ls;
                        t0 = std::time::Instant::now();
                        i = ni;
                        pos2.store(ls, Relaxed);
                    }
                    None => break,
                }
            }
            for s in &mut sinks {
                s.panic();
            }
            set_timer_resolution(0);
        });
        Self {
            stop,
            position_us,
            thread: Some(thread),
        }
    }

    pub fn position_us(&self) -> u64 {
        self.position_us.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// True while the playback thread is alive (also true at end-of-timeline
    /// until the final panic has been sent).
    pub fn is_running(&self) -> bool {
        self.thread
            .as_ref()
            .is_some_and(|t| !t.is_finished())
    }

    pub fn stop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct RecordingSink(Arc<Mutex<Vec<Vec<u8>>>>);

    impl EventSink for RecordingSink {
        fn send_at(&mut self, bytes: &[u8], _rem_us: u64) {
            self.0.lock().unwrap().push(bytes.to_vec());
        }
        fn panic(&mut self) {
            for ch in 0u8..16 {
                for ctl in [123u8, 121, 120] {
                    self.0.lock().unwrap().push(vec![0xB0 | ch, ctl, 0]);
                }
            }
        }
    }

    #[test]
    fn loop_wrap_releases_notes_without_full_reset() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let events = vec![
            (0u64, 0usize, vec![0x90, 60, 100]),
            (5_000u64, 0usize, vec![0x80, 60, 0]),
        ];
        let mut pb = Playback::start(vec![Box::new(RecordingSink(log.clone()))], events, 0, Some(0));
        // wait for at least two passes: a wrap happened and the schedule
        // replayed through it
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let strikes = log
                .lock()
                .unwrap()
                .iter()
                .filter(|b| b == &&vec![0x90, 60, 100])
                .count();
            if strikes >= 2 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "schedule did not replay across the loop boundary"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let snapshot = log.lock().unwrap().clone();
        pb.stop();
        // the wrap cleanup is notes-off only; 121/120 belong to a full panic
        assert!(snapshot
            .iter()
            .any(|b| b.len() == 3 && b[0] == 0xB0 && b[1] == 123));
        assert!(snapshot
            .iter()
            .all(|b| !(b.len() == 3 && (b[1] == 121 || b[1] == 120))));
    }

    // --- recording timebase (backend timestamps) ---------------------------

    /// Artificially delaying callback delivery must NOT shift recorded
    /// placement when the backend supplies timestamps — the issue's core
    /// acceptance criterion. Three messages stamped 10ms apart on the device
    /// arrive 0ms, 200ms and 800ms late; the stamps keep the 10ms spacing.
    #[test]
    fn backend_timestamp_immune_to_callback_delay() {
        let t0 = std::time::Instant::now();
        let mut tb = Timebase::new_at(0, None, t0);
        let ms = std::time::Duration::from_millis;
        // first timestamped message anchors: stamped by arrival
        let s0 = tb.stamp(100_000, t0 + ms(50));
        assert_eq!(s0, 50_000);
        // same +10ms device time, delivered 200ms late
        let s1 = tb.stamp(110_000, t0 + ms(260));
        // +10ms more, delivered 800ms late
        let s2 = tb.stamp(120_000, t0 + ms(1_060));
        assert_eq!(s1 - s0, 10_000, "device delta must drive the stamp");
        assert_eq!(s2 - s1, 10_000, "device delta must drive the stamp");
        // arrival-stamping would have produced 210ms and 800ms spacings
    }

    /// Without a backend timestamp (dev_us == 0) the stamp is arrival time —
    /// recording still works on backends that can't timestamp.
    #[test]
    fn no_backend_timestamp_falls_back_to_arrival() {
        let t0 = std::time::Instant::now();
        let mut tb = Timebase::new_at(0, None, t0);
        let ms = std::time::Duration::from_millis;
        assert_eq!(tb.stamp(0, t0 + ms(30)), 30_000);
        assert_eq!(tb.stamp(0, t0 + ms(45)), 45_000);
        // a timestamped message later still anchors normally
        let s = tb.stamp(500_000, t0 + ms(60));
        assert_eq!(s, 60_000);
        let s2 = tb.stamp(505_000, t0 + ms(360));
        assert_eq!(s2 - s, 5_000);
    }

    /// Manual input-latency compensation subtracts from every stamp.
    #[test]
    fn latency_compensation_subtracts() {
        let t0 = std::time::Instant::now();
        let mut tb = Timebase::new_at(5_000, None, t0);
        let ms = std::time::Duration::from_millis;
        assert_eq!(tb.stamp(0, t0 + ms(30)), 25_000);
        // never goes negative
        assert_eq!(tb.stamp(0, t0 + ms(2)), 0);
    }

    /// The diag counters record the delivery delay that the device stamp
    /// absorbed — the jitter stat the app surfaces in debug mode.
    #[test]
    fn diag_records_delivery_gap() {
        use std::sync::atomic::Ordering::Relaxed;
        let t0 = std::time::Instant::now();
        let diag = InputDiag::new();
        let mut tb = Timebase::new_at(0, Some(diag.clone()), t0);
        let ms = std::time::Duration::from_millis;
        tb.stamp(100_000, t0 + ms(50)); // anchor: gap 0
        tb.stamp(110_000, t0 + ms(300)); // delivered 240ms late
        tb.stamp(0, t0 + ms(310)); // unstamped fallback
        assert_eq!(diag.stamped.load(Relaxed), 2);
        assert_eq!(diag.unstamped.load(Relaxed), 1);
        assert_eq!(diag.gap_max_us.load(Relaxed), 240_000);
    }

    /// Device timestamps stay monotonic for the map even if a callback is
    /// reordered: a dev_us behind the anchor still lands at the anchor base.
    #[test]
    fn timebase_never_goes_backwards_under_anchor() {
        let t0 = std::time::Instant::now();
        let mut tb = Timebase::new_at(0, None, t0);
        let ms = std::time::Duration::from_millis;
        let s0 = tb.stamp(100_000, t0 + ms(50));
        // a counter restart/rewind clamps to the anchor's stamp, not a jump back
        let s1 = tb.stamp(50_000, t0 + ms(60));
        assert_eq!(s1, s0);
    }
}
