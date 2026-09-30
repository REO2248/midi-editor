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

/// One open input connection. Timestamps each incoming message in µs relative
/// to the moment `open` returned (not midir's platform epoch) so callers can
/// place recorded events on the playback timeline directly.
pub struct Input {
    // connection must stay alive to keep receiving
    _conn: midir::MidiInputConnection<()>,
    pub name: String,
}

impl Input {
    /// `cb(us_since_open, bytes)` is called on midir's callback thread.
    pub fn open<F>(index: usize, mut cb: F) -> Result<Self, Error>
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
        let t0 = std::time::Instant::now();
        let conn = inp
            .connect(
                &port,
                "midi-editor-in",
                move |_ts, bytes, _| cb(t0.elapsed().as_micros() as u64, bytes),
                (),
            )
            .map_err(|e| Error::Connect(e.to_string()))?;
        Ok(Self { _conn: conn, name })
    }

    pub fn open_named<F>(name: &str, mut cb: F) -> Result<Self, Error>
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
        let t0 = std::time::Instant::now();
        let conn = inp
            .connect(
                &port,
                "midi-editor-in",
                move |_ts, bytes, _| cb(t0.elapsed().as_micros() as u64, bytes),
                (),
            )
            .map_err(|e| Error::Connect(e.to_string()))?;
        Ok(Self {
            _conn: conn,
            name: pname,
        })
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

/// Time source the playback schedule runs against. `SystemClock` drives real
/// playback; tests substitute a manual clock so sequencing is exercised
/// deterministically — no wall-clock sleeps, no timing tolerances.
pub trait Clock {
    /// Current time in µs. The epoch is arbitrary — only differences matter.
    fn now_us(&self) -> u64;
    /// Block until `target_us` (same epoch as `now_us`) or until `stop`
    /// flips. Returns true if the target was reached; false on abort.
    fn wait_until_us(&mut self, target_us: u64, stop: &std::sync::atomic::AtomicBool) -> bool;
}

/// Wall-clock `Clock`: `Instant` + 2 ms sleep/spin hybrid — the policy the
/// playback thread always used, now behind an interface.
pub struct SystemClock {
    t0: std::time::Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            t0: std::time::Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn now_us(&self) -> u64 {
        self.t0.elapsed().as_micros() as u64
    }
    fn wait_until_us(&mut self, target_us: u64, stop: &std::sync::atomic::AtomicBool) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        let target = self.t0 + std::time::Duration::from_micros(target_us);
        loop {
            let now = std::time::Instant::now();
            if now >= target {
                return true;
            }
            if stop.load(Relaxed) {
                return false;
            }
            let rem = target - now;
            if rem > std::time::Duration::from_millis(2) {
                std::thread::sleep(rem.min(std::time::Duration::from_millis(2)));
            } else {
                std::hint::spin_loop();
            }
        }
    }
}

/// The scheduling core of `Playback`, generic over `Clock` so tests can run
/// it synchronously on a fake clock. See `Playback::start` for the contract.
///
/// `events` must be sorted by absolute µs; `start_us` seeks (earlier events
/// skipped, clock base = `start_us`); `loop_from_us` notes-offs every sink at
/// the end of each pass and restarts the schedule at that point. Every exit
/// path — stop, end of timeline, or a loop with nothing left to replay —
/// ends with `panic()` on every sink. `pos` is updated as the schedule
/// advances so the UI can draw a playhead.
pub fn run_schedule(
    clock: &mut impl Clock,
    sinks: &mut [Box<dyn EventSink>],
    events: &[(u64, usize, Vec<u8>)],
    start_us: u64,
    loop_from_us: Option<u64>,
    stop: &std::sync::atomic::AtomicBool,
    pos: &std::sync::atomic::AtomicU64,
) {
    use std::sync::atomic::Ordering::Relaxed;
    let mut base_us = start_us;
    let mut epoch0_us = clock.now_us();
    let mut i = events.partition_point(|(us, _, _)| *us < base_us);
    'outer: loop {
        while i < events.len() {
            if stop.load(Relaxed) {
                break 'outer;
            }
            let (us, sink_idx, bytes) = &events[i];
            i += 1;
            let us = *us;
            let Some(sink) = sinks.get_mut(*sink_idx) else {
                continue;
            };
            let wake_us = (epoch0_us + (us - base_us)).saturating_sub(sink.lead_us());
            if !clock.wait_until_us(wake_us, stop) {
                break 'outer;
            }
            pos.store(us, Relaxed);
            let deadline_us = epoch0_us + (us - base_us);
            let rem = deadline_us.saturating_sub(clock.now_us());
            sink.send_at(bytes, rem);
        }
        // loop wrap: release notes but keep tails and controller
        // state — the schedule restarts with chase events at the
        // loop point, which re-establish whatever should sound
        for s in sinks.iter_mut() {
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
                epoch0_us = clock.now_us();
                i = ni;
                pos.store(ls, Relaxed);
            }
            None => break,
        }
    }
    for s in sinks.iter_mut() {
        s.panic();
    }
}

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
            set_timer_resolution(1);
            let mut clock = SystemClock::default();
            run_schedule(
                &mut clock,
                &mut sinks,
                &events,
                start_us,
                loop_from_us,
                &stop2,
                &pos2,
            );
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
    use std::sync::atomic::Ordering::Relaxed;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::sync::{Arc, Mutex};

    /// Manual clock: `wait_until_us` jumps straight to the target — the
    /// schedule runs synchronously, so assertions see exact µs values.
    struct FakeClock {
        now: u64,
    }

    impl Clock for FakeClock {
        fn now_us(&self) -> u64 {
            self.now
        }
        fn wait_until_us(&mut self, target_us: u64, _stop: &AtomicBool) -> bool {
            self.now = target_us;
            true
        }
    }

    /// Fake sink recording every message verbatim; sets `stop` once it has
    /// delivered `stop_after` sends, simulating a mid-playback transport stop.
    struct RecordingSink {
        log: Arc<Mutex<Vec<Vec<u8>>>>,
        stop: Arc<AtomicBool>,
        stop_after: usize,
        sends: usize,
    }

    impl EventSink for RecordingSink {
        fn send_at(&mut self, bytes: &[u8], _rem_us: u64) {
            self.log.lock().unwrap().push(bytes.to_vec());
            self.sends += 1;
            if self.sends == self.stop_after {
                self.stop.store(true, Relaxed);
            }
        }
        /// Same payload as the trait default, recorded without counting
        /// against `stop_after` — wrap cleanup must not fire the stop.
        fn notes_off(&mut self) {
            for ch in 0u8..16 {
                self.log.lock().unwrap().push(vec![0xB0 | ch, 123, 0]);
            }
        }
        fn panic(&mut self) {
            for ch in 0u8..16 {
                for ctl in [123u8, 121, 120] {
                    self.log.lock().unwrap().push(vec![0xB0 | ch, ctl, 0]);
                }
            }
        }
    }

    #[test]
    fn loop_wrap_releases_notes_without_full_reset() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let pos = AtomicU64::new(0);
        let events = vec![
            (0u64, 0usize, vec![0x90, 60, 100]),
            (5_000u64, 0usize, vec![0x80, 60, 0]),
        ];
        let mut sinks: Vec<Box<dyn EventSink>> = vec![Box::new(RecordingSink {
            log: log.clone(),
            stop: stop.clone(),
            // stop right after the replayed note-on → the note-off of pass 2
            // must be skipped and the run must end in panic()
            stop_after: 3,
            sends: 0,
        })];
        let mut clock = FakeClock { now: 0 };
        run_schedule(&mut clock, &mut sinks, &events, 0, Some(0), &stop, &pos);
        let snapshot = log.lock().unwrap().clone();
        // note-on played twice → the schedule wrapped and replayed
        assert_eq!(
            snapshot.iter().filter(|b| *b == &vec![0x90, 60, 100]).count(),
            2
        );
        // the wrap cleanup is notes-off only; 121/120 belong to a full panic
        let notes_off_pos = snapshot
            .iter()
            .position(|b| b.len() == 3 && b[0] & 0xF0 == 0xB0 && b[1] == 123)
            .expect("notes-off at loop wrap");
        assert!(snapshot[..notes_off_pos]
            .iter()
            .all(|b| !(b.len() == 3 && b[0] & 0xF0 == 0xB0 && (b[1] == 121 || b[1] == 120))));
        // panic() ran at the end — CC 121/120 appear, after the wrap cleanup
        assert!(snapshot[notes_off_pos..]
            .iter()
            .any(|b| b.len() == 3 && b[0] & 0xF0 == 0xB0 && b[1] == 121));
    }
}
