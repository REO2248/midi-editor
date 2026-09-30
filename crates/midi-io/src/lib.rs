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
        let ord = inp
            .ports()
            .iter()
            .take(index)
            .filter(|p| inp.port_name(p).map(|n| n == name).unwrap_or(false))
            .count();
        Self::connect_on(inp, port, name, ord, opts, cb)
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
        Self::open_ord_opts(name, 0, opts, cb)
    }

    /// Open the `ord`-th input port with this exact name — same-name
    /// devices stay distinct so a reconnect binds the original endpoint.
    pub fn open_ord<F>(name: &str, ord: usize, cb: F) -> Result<Self, Error>
    where
        F: FnMut(u64, &[u8]) + Send + 'static,
    {
        Self::open_ord_opts(name, ord, InputOpts::default(), cb)
    }

    /// `open_ord` with recording options (latency compensation, diagnostics).
    pub fn open_ord_opts<F>(name: &str, ord: usize, opts: InputOpts, cb: F) -> Result<Self, Error>
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
        Self::connect_on(inp, port, pname, ord, opts, cb)
    }

    fn connect_on<F>(
        inp: MidiInput,
        port: midir::MidiInputPort,
        name: String,
        ord: usize,
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
        Ok(Self {
            _conn: conn,
            name,
            ord,
        })
    }
}

/// Policy for sending long SysEx messages during playback.
///
/// WinMM serializes a `midiOutLongMsg` transmission: `Output::send` on a
/// multi-kilobyte dump blocks until the device drains it, and every event
/// scheduled behind it on that sink is shifted by the whole dump time.
/// The policy is per-sink; anything `SysexConfig::inline_max` bytes or
/// smaller always goes inline so small setup SysEx (GM/GS/XG resets, patch
/// dumps) keeps deterministic ordering ahead of same-tick channel events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SysexPolicy {
    /// Send everything on the playback thread (default, deterministic):
    /// a long dump delays the events scheduled after it on that sink.
    Serialize,
    /// Long messages move to a per-sink worker thread holding a second
    /// connection to the same port; channel timing stays predictable.
    /// Ordering between a deferred dump and later channel events is not
    /// preserved — that is the point of the mode. Falls back to
    /// `Serialize` when the backend refuses a second connection.
    Background,
    /// Drop messages over `inline_max` during playback, with a diagnostic —
    /// for rigs where a dump must never stall channel playback at all.
    Skip,
}

impl SysexPolicy {
    /// Menu/serialization label (round-trips through `from_label`).
    pub fn label(self) -> &'static str {
        match self {
            Self::Serialize => "serialize",
            Self::Background => "background",
            Self::Skip => "skip",
        }
    }
    pub fn from_label(s: &str) -> Option<Self> {
        match s {
            "serialize" => Some(Self::Serialize),
            "background" => Some(Self::Background),
            "skip" => Some(Self::Skip),
            _ => None,
        }
    }
    pub fn cycle(self) -> Self {
        match self {
            Self::Serialize => Self::Background,
            Self::Background => Self::Skip,
            Self::Skip => Self::Serialize,
        }
    }
}

/// Tunables bounding long-message transmission: message size, queue memory.
#[derive(Debug, Clone, Copy)]
pub struct SysexConfig {
    pub policy: SysexPolicy,
    /// At or under this size a message always sends inline — small setup
    /// SysEx keeps deterministic ordering before same-tick channel events.
    pub inline_max: usize,
    /// Hard cap on one message; larger dumps are dropped under every policy.
    pub max_bytes: usize,
    /// Bound on bytes parked in the background lane of one sink; further
    /// messages are dropped with a diagnostic instead of growing memory.
    pub max_queue_bytes: usize,
}

impl Default for SysexConfig {
    fn default() -> Self {
        Self {
            policy: SysexPolicy::Serialize,
            inline_max: 256,
            max_bytes: 1 << 20,
            max_queue_bytes: 4 << 20,
        }
    }
}

/// What `SysexConfig` decided for one message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    /// Send now, on the playback thread.
    Inline,
    /// Hand to the background lane.
    Defer,
    /// Discard (oversized, policy-skip, or a full lane).
    Drop,
}

fn gate(cfg: &SysexConfig, len: usize, has_lane: bool) -> Gate {
    if len <= cfg.inline_max {
        return Gate::Inline;
    }
    if len > cfg.max_bytes {
        return Gate::Drop;
    }
    match cfg.policy {
        SysexPolicy::Skip => Gate::Drop,
        SysexPolicy::Background if has_lane => Gate::Defer,
        // Serialize — or Background that fell back when no lane could open
        SysexPolicy::Serialize | SysexPolicy::Background => Gate::Inline,
    }
}

/// Per-sink measurements of long-message transmission, shared via `Arc` so
/// the UI can surface them after playback stops.
#[derive(Debug, Default)]
pub struct SysexStats {
    /// messages deferred to the background lane
    pub deferred: std::sync::atomic::AtomicU64,
    /// messages dropped by policy/bounds
    pub dropped: std::sync::atomic::AtomicU64,
    /// messages sent inline
    pub inline: std::sync::atomic::AtomicU64,
    /// duration of the most recent long-message send (worker or inline)
    pub last_send_us: std::sync::atomic::AtomicU64,
    /// worst long-message send duration seen
    pub max_send_us: std::sync::atomic::AtomicU64,
}

impl SysexStats {
    fn note_send(&self, dur: std::time::Duration) {
        use std::sync::atomic::Ordering::Relaxed;
        let us = dur.as_micros() as u64;
        self.last_send_us.store(us, Relaxed);
        self.max_send_us.fetch_max(us, Relaxed);
    }
    pub fn snapshot(&self) -> (u64, u64, u64, u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            self.inline.load(Relaxed),
            self.deferred.load(Relaxed),
            self.dropped.load(Relaxed),
            self.last_send_us.load(Relaxed),
            self.max_send_us.load(Relaxed),
        )
    }
}

/// Bounded background lane for long SysEx: a worker thread draining a
/// sync_channel through a caller-supplied sender — a second port connection
/// for `PortSink`, a fake in tests. Memory is capped twice: a 64-message
/// channel and a byte budget in `queued` charged before enqueue.
struct SysexLane {
    tx: std::sync::mpsc::SyncSender<Vec<u8>>,
    queued: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// held so the worker's lifetime is tied to the lane; never joined
    /// (drop detaches it — a mid-send worker exits after the port drains)
    _thread: std::thread::JoinHandle<()>,
}

impl SysexLane {
    fn spawn<F>(mut send: F, stats: std::sync::Arc<SysexStats>) -> Self
    where
        F: FnMut(&[u8]) + Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(64);
        let queued = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let q2 = queued.clone();
        let thread = std::thread::spawn(move || {
            use std::sync::atomic::Ordering::Relaxed;
            while let Ok(msg) = rx.recv() {
                let t0 = std::time::Instant::now();
                send(&msg);
                stats.note_send(t0.elapsed());
                q2.fetch_sub(msg.len(), Relaxed);
            }
        });
        Self {
            tx,
            queued,
            _thread: thread,
        }
    }

    /// Charge `max_queue_bytes` then try the channel. Full = drop, never
    /// block — a lane that can't keep up must not stall playback either.
    fn try_enqueue(&self, msg: Vec<u8>, max_queue_bytes: usize) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        let len = msg.len();
        let prior = self.queued.fetch_add(len, Relaxed);
        if prior + len > max_queue_bytes {
            self.queued.fetch_sub(len, Relaxed);
            return false;
        }
        match self.tx.try_send(msg) {
            Ok(()) => true,
            Err(_) => {
                self.queued.fetch_sub(len, Relaxed);
                false
            }
        }
    }
}

// dropping the lane drops its SyncSender — the worker drains what it
// already holds then exits on its own; we never join, since an in-flight
// dump may still be transmitting on a real-time caller

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
    /// long-message policy: Serialize | Background (via `lane`) | Skip
    cfg: SysexConfig,
    lane: Option<SysexLane>,
    stats: std::sync::Arc<SysexStats>,
    /// one "dropped by policy" warning is enough
    warned_drop: bool,
}

impl PortSink {
    /// Serialized sends (today's behavior).
    pub fn new(out: Output) -> Self {
        Self::with_config(out, SysexConfig::default())
    }

    /// `cfg.policy` picks how messages over `inline_max` leave the sink.
    /// `Background` opens a second connection to the same port for the
    /// worker lane; when the backend refuses, the sink stays serialized
    /// and reports `has_lane() == false`.
    pub fn with_config(out: Output, cfg: SysexConfig) -> Self {
        let stats = std::sync::Arc::new(SysexStats::default());
        let lane = if cfg.policy == SysexPolicy::Background {
            match Output::open_named(&out.name) {
                Ok(lane_out) => {
                    let stats2 = stats.clone();
                    let mut lane_out = Some(lane_out);
                    Some(SysexLane::spawn(
                        move |b: &[u8]| {
                            if let Some(o) = lane_out.as_mut() {
                                if let Err(e) = o.send(b) {
                                    tracing::warn!("sysex lane on '{}': {e}", o.name);
                                    lane_out = None;
                                }
                            }
                        },
                        stats2,
                    ))
                }
                Err(e) => {
                    tracing::warn!(
                        "sysex background lane on '{}' unavailable ({e}) — serializing",
                        out.name
                    );
                    None
                }
            }
        } else {
            None
        };
        Self {
            name: out.name.clone(),
            ord: out.ord,
            out: Some(out),
            dead: false,
            next_retry: std::time::Instant::now(),
            cfg,
            lane,
            stats,
            warned_drop: false,
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

    /// True when the background SysEx lane is live (policy honored);
    /// false means `Background` fell back to serialized sends.
    pub fn has_lane(&self) -> bool {
        self.lane.is_some()
    }

    /// Long-message counters — read after playback for a diagnostic summary.
    pub fn stats(&self) -> std::sync::Arc<SysexStats> {
        self.stats.clone()
    }

    fn send_or_mark_dead(&mut self, bytes: &[u8]) {
        let send_err = if let Some(out) = &mut self.out {
            out.send(bytes).err()
        } else {
            None
        };
        if let Some(e) = send_err {
            // drop the dead connection: repeated sends would each fail the
            // same way, and local held-note bookkeeping resets with it
            self.out = None;
            if !self.dead {
                self.dead = true;
                tracing::warn!("midi port '{}' stopped accepting events: {e}", self.name);
            }
        }
    }
}

impl EventSink for PortSink {
    fn send_at(&mut self, bytes: &[u8], _rem_us: u64) {
        if self.out.is_none() && !self.try_reconnect() {
            return;
        }
        if bytes.first() == Some(&0xF0) {
            match gate(&self.cfg, bytes.len(), self.lane.is_some()) {
                Gate::Drop => {
                    self.stats
                        .dropped
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if !self.warned_drop {
                        self.warned_drop = true;
                        tracing::warn!(
                            "sysex ({} bytes) on '{}' dropped by policy/bounds",
                            bytes.len(),
                            self.name
                        );
                    }
                    return;
                }
                Gate::Defer => {
                    let lane = self.lane.as_ref().expect("gate defers only with a lane");
                    if lane.try_enqueue(bytes.to_vec(), self.cfg.max_queue_bytes) {
                        self.stats
                            .deferred
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    } else {
                        self.stats
                            .dropped
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if !self.warned_drop {
                            self.warned_drop = true;
                            tracing::warn!(
                                "sysex ({} bytes) on '{}' dropped — background lane full",
                                bytes.len(),
                                self.name
                            );
                        }
                    }
                    return;
                }
                Gate::Inline => {
                    self.stats
                        .inline
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let t0 = std::time::Instant::now();
                    self.send_or_mark_dead(bytes);
                    self.stats.note_send(t0.elapsed());
                    return;
                }
            }
        }
        self.send_or_mark_dead(bytes);
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
                    let Some(sink) = sinks.get_mut(*sink_idx) else {
                        continue;
                    };
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
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
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
        let mut pb = Playback::start(
            vec![Box::new(RecordingSink(log.clone()))],
            events,
            0,
            Some(0),
        );
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

    // --- SysEx long-message policy ----------------------------------------

    fn cfg(policy: SysexPolicy) -> SysexConfig {
        SysexConfig {
            policy,
            inline_max: 256,
            max_bytes: 1 << 20,
            max_queue_bytes: 4096,
        }
    }

    /// Small setup SysEx always sends inline under every policy — the
    /// deterministic ordering before same-tick channel events is preserved.
    #[test]
    fn small_sysex_stays_inline_under_every_policy() {
        for policy in [
            SysexPolicy::Serialize,
            SysexPolicy::Background,
            SysexPolicy::Skip,
        ] {
            assert_eq!(gate(&cfg(policy), 8, true), Gate::Inline);
            assert_eq!(gate(&cfg(policy), 256, true), Gate::Inline);
        }
    }

    #[test]
    fn gate_decisions_by_policy() {
        let big = 4 * 1024; // a multi-kilobyte dump
        assert_eq!(gate(&cfg(SysexPolicy::Serialize), big, false), Gate::Inline);
        assert_eq!(gate(&cfg(SysexPolicy::Skip), big, false), Gate::Drop);
        assert_eq!(gate(&cfg(SysexPolicy::Background), big, true), Gate::Defer);
        // a Background sink whose lane failed to open serializes instead
        assert_eq!(
            gate(&cfg(SysexPolicy::Background), big, false),
            Gate::Inline
        );
        // over max_bytes: dropped under every policy
        for policy in [
            SysexPolicy::Serialize,
            SysexPolicy::Background,
            SysexPolicy::Skip,
        ] {
            assert_eq!(gate(&cfg(policy), (1 << 20) + 1, true), Gate::Drop);
        }
    }

    /// The background lane enforces its byte bound: once `max_queue_bytes`
    /// is parked, further messages are refused (the caller drops + warns)
    /// instead of growing memory unboundedly.
    #[test]
    fn background_lane_bounds_queue_memory() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sent2 = sent.clone();
        let stats = Arc::new(SysexStats::default());
        // a "port" that blocks long enough to prove the bound: the worker
        // holds each message until the test releases it
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let lane = SysexLane::spawn(
            move |b: &[u8]| {
                sent2.lock().unwrap().push(b.to_vec());
                let _ = release_rx.recv();
            },
            stats.clone(),
        );
        let msg = vec![0xF0u8; 1024]; // 1 KiB each, bound is 4 KiB
        for _ in 0..4 {
            assert!(lane.try_enqueue(msg.clone(), 4096));
        }
        // ~4 KiB parked (worker may already be holding one) — next is refused
        let mut refused = 0;
        for _ in 0..4 {
            if !lane.try_enqueue(msg.clone(), 4096) {
                refused += 1;
            }
        }
        assert!(refused > 0, "byte bound never refused an enqueue");
        // let the worker drain everything queued
        for _ in 0..8 {
            let _ = release_tx.send(());
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while lane.queued.load(std::sync::atomic::Ordering::Relaxed) > 0 {
            assert!(std::time::Instant::now() < deadline, "lane did not drain");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(!sent.lock().unwrap().is_empty());
    }

    /// Deferred sends report their duration into the shared stats — the
    /// "measure long-message send time" diagnostic.
    #[test]
    fn lane_send_duration_is_measured() {
        let stats = Arc::new(SysexStats::default());
        let lane = SysexLane::spawn(
            |_: &[u8]| {
                std::thread::sleep(std::time::Duration::from_millis(10));
            },
            stats.clone(),
        );
        assert!(lane.try_enqueue(vec![0xF0u8; 512], 4096));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while stats.last_send_us.load(std::sync::atomic::Ordering::Relaxed) == 0 {
            assert!(std::time::Instant::now() < deadline, "no send recorded");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(stats.max_send_us.load(std::sync::atomic::Ordering::Relaxed) > 0);
    }

    /// End-to-end through `Playback`: under Skip, a huge SysEx must not
    /// shift the note scheduled right after it. Uses a synthetic sink that
    /// routes big messages through the same gate + stats as `PortSink`.
    #[test]
    fn skip_policy_keeps_note_timing_after_huge_sysex() {
        use std::sync::atomic::Ordering::Relaxed;
        struct GatedSink {
            cfg: SysexConfig,
            stats: Arc<SysexStats>,
            log: Arc<Mutex<Vec<(u64, Vec<u8>)>>>,
        }
        impl EventSink for GatedSink {
            fn send_at(&mut self, bytes: &[u8], _rem_us: u64) {
                if bytes.first() == Some(&0xF0) {
                    match gate(&self.cfg, bytes.len(), false) {
                        Gate::Drop => {
                            self.stats.dropped.fetch_add(1, Relaxed);
                            return;
                        }
                        Gate::Defer => unreachable!("no lane"),
                        Gate::Inline => {
                            // simulate WinMM serializing a big dump —
                            // under Skip this branch is never reached
                            std::thread::sleep(std::time::Duration::from_millis(300));
                        }
                    }
                }
                let us = std::time::Instant::now().elapsed().as_micros() as u64;
                self.log.lock().unwrap().push((us, bytes.to_vec()));
            }
            fn panic(&mut self) {}
        }
        let stats = Arc::new(SysexStats::default());
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = GatedSink {
            cfg: cfg(SysexPolicy::Skip),
            stats: stats.clone(),
            log: log.clone(),
        };
        let dump = vec![0xF0u8; 8 * 1024];
        let events = vec![
            (0u64, 0usize, dump),
            (50_000u64, 0usize, vec![0x90, 60, 100]),
        ];
        let mut pb = Playback::start(vec![Box::new(sink)], events, 0, None);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while log.lock().unwrap().is_empty() {
            assert!(std::time::Instant::now() < deadline, "note never arrived");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        pb.stop();
        assert_eq!(stats.dropped.load(Relaxed), 1, "dump was not dropped");
        // the note landed — the dump never blocked the thread for it
        assert_eq!(log.lock().unwrap()[0].1, vec![0x90, 60, 100]);
    }

    /// Policy label round-trip for sidecar persistence.
    #[test]
    fn sysex_policy_labels_round_trip() {
        for p in [
            SysexPolicy::Serialize,
            SysexPolicy::Background,
            SysexPolicy::Skip,
        ] {
            assert_eq!(SysexPolicy::from_label(p.label()), Some(p));
            assert_ne!(p.cycle(), p);
        }
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
