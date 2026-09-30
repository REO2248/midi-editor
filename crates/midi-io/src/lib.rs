//! MIDI I/O layer. WinMM backend via midir on Windows.
//! loopMIDI cables and the Windows MIDI Services built-in loopback appear
//! as ordinary output ports — no special-casing.

use midir::{Ignore, MidiInput, MidiOutput, MidiOutputConnection};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A stable output-destination identity — what the UI persists and MCP tools
/// name. Ports are addressed by NAME (indexes shift as devices come and go);
/// `ord` disambiguates same-name devices — the Nth enumerated port with that
/// name (0 = first), which is the most specific handle WinMM exposes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Destination {
    /// midir output port, resolved by name at open time
    MidiPort {
        port_name: String,
        /// None/missing in old sidecars = 0 (first match, as before)
        #[serde(default)]
        ord: usize,
    },
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
    /// raw enumeration slot — unstable across hotplug
    pub index: usize,
    pub name: String,
    /// how many earlier ports share this exact name (0 = first) — the
    /// disambiguator for same-name devices
    pub ord: usize,
}

/// Assign each name an `ord` — its position among same-named siblings — so
/// `name` + `ord` identifies a specific device even when a system reports
/// identical names for several units.
fn ord_assign(names: Vec<String>) -> Vec<(String, usize)> {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    names
        .into_iter()
        .map(|name| {
            let e = seen.entry(name.clone()).or_insert(0);
            let o = *e;
            *e += 1;
            (name, o)
        })
        .collect()
}

pub fn list_outputs() -> Result<Vec<PortInfo>, Error> {
    let out = MidiOutput::new("midi-editor").map_err(|e| Error::Init(e.to_string()))?;
    let names: Vec<String> = out
        .ports()
        .iter()
        .map(|p| out.port_name(p).unwrap_or_else(|_| "<unknown>".into()))
        .collect();
    Ok(ord_assign(names)
        .into_iter()
        .enumerate()
        .map(|(index, (name, ord))| PortInfo { index, name, ord })
        .collect())
}

pub fn list_inputs() -> Result<Vec<PortInfo>, Error> {
    let mut inp = MidiInput::new("midi-editor").map_err(|e| Error::Init(e.to_string()))?;
    inp.ignore(Ignore::None);
    let names: Vec<String> = inp
        .ports()
        .iter()
        .map(|p| inp.port_name(p).unwrap_or_else(|_| "<unknown>".into()))
        .collect();
    Ok(ord_assign(names)
        .into_iter()
        .enumerate()
        .map(|(index, (name, ord))| PortInfo { index, name, ord })
        .collect())
}

/// One open output connection. `MidiOutputConnection` is `Send`; the playback
/// thread owns it.
pub struct Output {
    conn: MidiOutputConnection,
    pub name: String,
    /// same-name ordinal of the port this connection is bound to
    pub ord: usize,
}

impl Output {
    pub fn open(index: usize) -> Result<Self, Error> {
        let out = MidiOutput::new("midi-editor").map_err(|e| Error::Init(e.to_string()))?;
        let ports = out.ports();
        let port = ports
            .into_iter()
            .nth(index)
            .ok_or_else(|| Error::Connect(format!("port {index} not found")))?;
        let name = out.port_name(&port).unwrap_or_else(|_| "<unknown>".into());
        let ord = out
            .ports()
            .iter()
            .take(index)
            .filter(|p| out.port_name(p).map(|n| n == name).unwrap_or(false))
            .count();
        let conn = out
            .connect(&port, "midi-editor-out")
            .map_err(|e| Error::Connect(e.to_string()))?;
        Ok(Self { conn, name, ord })
    }

    /// Open the first output port whose name equals `name` — the stable way
    /// to address ports across sessions.
    pub fn open_named(name: &str) -> Result<Self, Error> {
        Self::open_ord(name, 0)
    }

    /// Open the `ord`-th output port with this exact name — same-name
    /// devices keep separate identities so a reconnect never silently
    /// lands on a different unit with the same label.
    pub fn open_ord(name: &str, ord: usize) -> Result<Self, Error> {
        let out = MidiOutput::new("midi-editor").map_err(|e| Error::Init(e.to_string()))?;
        let port = out
            .ports()
            .into_iter()
            .filter(|p| out.port_name(p).map(|n| n == name).unwrap_or(false))
            .nth(ord)
            .ok_or_else(|| Error::Connect(format!("port '{name}' #{ord} not found")))?;
        let conn = out
            .connect(&port, "midi-editor-out")
            .map_err(|e| Error::Connect(e.to_string()))?;
        Ok(Self {
            conn,
            name: name.to_string(),
            ord,
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
    /// same-name ordinal of the port this connection is bound to —
    /// reopening with `(name, ord)` retargets the exact same endpoint
    /// after an unplug/replug
    pub ord: usize,
}

impl Input {
    /// `cb(us_since_open, bytes)` is called on midir's callback thread.
    pub fn open<F>(index: usize, cb: F) -> Result<Self, Error>
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
        let ord = inp
            .ports()
            .iter()
            .take(index)
            .filter(|p| inp.port_name(p).map(|n| n == name).unwrap_or(false))
            .count();
        Self::connect_on(inp, port, name, ord, cb)
    }

    pub fn open_named<F>(name: &str, cb: F) -> Result<Self, Error>
    where
        F: FnMut(u64, &[u8]) + Send + 'static,
    {
        Self::open_ord(name, 0, cb)
    }

    /// Open the `ord`-th input port with this exact name — same-name
    /// devices stay distinct so a reconnect binds the original endpoint.
    pub fn open_ord<F>(name: &str, ord: usize, cb: F) -> Result<Self, Error>
    where
        F: FnMut(u64, &[u8]) + Send + 'static,
    {
        let mut inp = MidiInput::new("midi-editor-in").map_err(|e| Error::Init(e.to_string()))?;
        inp.ignore(Ignore::None);
        let port = inp
            .ports()
            .into_iter()
            .filter(|p| inp.port_name(p).map(|n| n == name).unwrap_or(false))
            .nth(ord)
            .ok_or_else(|| Error::Connect(format!("input '{name}' #{ord} not found")))?;
        let pname = inp.port_name(&port).unwrap_or_else(|_| name.to_string());
        Self::connect_on(inp, port, pname, ord, cb)
    }

    fn connect_on<F>(
        inp: MidiInput,
        port: midir::MidiInputPort,
        name: String,
        ord: usize,
        mut cb: F,
    ) -> Result<Self, Error>
    where
        F: FnMut(u64, &[u8]) + Send + 'static,
    {
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
            name,
            ord,
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

/// `EventSink` over a `MidiOutputConnection`. When the port disappears,
/// the connection is dropped (one warning, not one per event) and sends are
/// skipped until a rate-limited reopen finds the exact same `name`+`ord`
/// endpoint again — unplug/replug recovers without restarting playback.
pub struct PortSink {
    /// `None` while the port is dead
    out: Option<Output>,
    name: String,
    ord: usize,
    dead: bool,
    /// next allowed reconnect attempt — opening a port enumerates devices,
    /// so it is throttled rather than run per dropped event
    next_retry: std::time::Instant,
}

impl PortSink {
    pub fn new(out: Output) -> Self {
        Self {
            name: out.name.clone(),
            ord: out.ord,
            out: Some(out),
            dead: false,
            next_retry: std::time::Instant::now(),
        }
    }

    fn try_reconnect(&mut self) -> bool {
        let now = std::time::Instant::now();
        if now < self.next_retry {
            return false;
        }
        self.next_retry = now + std::time::Duration::from_millis(500);
        match Output::open_ord(&self.name, self.ord) {
            Ok(mut o) => {
                // the reconnected unit may hold stale ringing notes — clean
                // its state before fresh events stream in
                o.panic();
                self.out = Some(o);
                self.dead = false;
                tracing::info!("midi port '{}' reconnected", self.name);
                true
            }
            Err(_) => false,
        }
    }
}

impl EventSink for PortSink {
    fn send_at(&mut self, bytes: &[u8], _rem_us: u64) {
        if self.out.is_none() && !self.try_reconnect() {
            return;
        }
        let Some(out) = &mut self.out else { return };
        if let Err(e) = out.send(bytes) {
            // drop the dead connection: repeated sends would each fail the
            // same way, and local held-note bookkeeping resets with it
            self.out = None;
            if !self.dead {
                self.dead = true;
                tracing::warn!("midi port '{}' stopped accepting events: {e}", self.name);
            }
        }
    }
    fn panic(&mut self) {
        if let Some(out) = &mut self.out {
            out.panic();
        }
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
    fn ord_assign_disambiguates_same_name_devices() {
        let out = ord_assign(vec![
            "UM-1".to_string(),
            "loopMIDI".to_string(),
            "UM-1".to_string(),
            "UM-1".to_string(),
        ]);
        assert_eq!(
            out,
            vec![
                ("UM-1".to_string(), 0),
                ("loopMIDI".to_string(), 0),
                ("UM-1".to_string(), 1),
                ("UM-1".to_string(), 2),
            ]
        );
    }

    /// Same-name devices at different enumeration positions must compare
    /// unequal — `ord` is part of destination identity.
    #[test]
    fn destination_ord_distinguishes_same_name_devices() {
        assert_eq!(
            Destination::MidiPort {
                port_name: "X".into(),
                ord: 0
            },
            Destination::MidiPort {
                port_name: "X".into(),
                ord: 0
            }
        );
        assert_ne!(
            Destination::MidiPort {
                port_name: "X".into(),
                ord: 0
            },
            Destination::MidiPort {
                port_name: "X".into(),
                ord: 1
            }
        );
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
}
