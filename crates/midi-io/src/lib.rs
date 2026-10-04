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
    /// hosted VST3 plugin instance. `plugin_path` is where the bundle lives
    /// NOW; the durable identity is `component_id` (the VST3 class UID), so
    /// routing survives the bundle moving or being reinstalled elsewhere.
    /// The extra fields are absent in sidecars written before identity
    /// persistence — serde defaults migrate them on read.
    Plugin {
        plugin_path: String,
        /// VST3 class/component ID (TUID hex string) when a probe saw it
        #[serde(default)]
        component_id: Option<String>,
        /// plugin vendor, for display + disambiguation
        #[serde(default)]
        vendor: Option<String>,
        /// human name, for display + disambiguation
        #[serde(default)]
        plugin_name: Option<String>,
    },
}

impl Destination {
    /// Same endpoint for routing purposes — not byte equality. Ports match
    /// by name. Plugins match when the path agrees (covers in-place bundle
    /// upgrades that may change the reported class id) OR when both sides
    /// carry a component ID and those agree (covers the bundle moving).
    pub fn same_identity(&self, other: &Destination) -> bool {
        match (self, other) {
            (
                Destination::MidiPort {
                    port_name: a,
                    ord: oa,
                },
                Destination::MidiPort {
                    port_name: b,
                    ord: ob,
                },
            ) => a == b && oa == ob,
            (
                Destination::Plugin {
                    plugin_path: pa,
                    component_id: ca,
                    ..
                },
                Destination::Plugin {
                    plugin_path: pb,
                    component_id: cb,
                    ..
                },
            ) => pa == pb || matches!((ca, cb), (Some(a), Some(b)) if a == b),
            _ => false,
        }
    }
}

/// Where `stored` ended up after `resolve_plugin_dest` matched it against
/// the live plugin catalog.
pub enum Resolved {
    /// exact path still in the catalog (the preferred hint won)
    SamePath,
    /// path gone; remapped onto the same component ID found elsewhere —
    /// carries the catalog path it resolved to
    Moved(std::path::PathBuf),
    /// several installs expose the component ID; one was picked
    /// deterministically (longest common path prefix, then lowest path)
    Ambiguous(std::path::PathBuf),
    /// nothing matched — the stored identity is kept as-is so the
    /// destination stays named and revives when the plugin returns
    Missing,
}

/// Match a stored plugin destination against the scanned catalog, preferring
/// the recorded path and falling back to the class/component ID when the
/// bundle moved. Non-plugin destinations pass through unchanged.
pub fn resolve_plugin_dest(
    stored: &Destination,
    catalog: &[Destination],
) -> (Destination, Resolved) {
    let Destination::Plugin {
        plugin_path,
        component_id,
        ..
    } = stored
    else {
        return (stored.clone(), Resolved::SamePath);
    };
    fn path_of(d: &Destination) -> &str {
        match d {
            Destination::Plugin { plugin_path, .. } => plugin_path,
            _ => "",
        }
    }
    // 1. preferred hint: exact path match — adopt the catalog's fresh metadata
    if let Some(exact) = catalog
        .iter()
        .find(|d| matches!(d, Destination::Plugin { plugin_path: p, .. } if p == plugin_path))
    {
        return (exact.clone(), Resolved::SamePath);
    }
    // 2. component-ID match: the bundle moved or was reinstalled
    if let Some(cid) = component_id {
        let mut matches: Vec<&Destination> = catalog
            .iter()
            .filter(|d| {
                matches!(d, Destination::Plugin { component_id: c, .. } if c.as_deref() == Some(cid.as_str()))
            })
            .collect();
        match matches.len() {
            0 => {}
            1 => {
                let found = matches.pop().expect("one match");
                return (
                    found.clone(),
                    Resolved::Moved(std::path::PathBuf::from(path_of(found))),
                );
            }
            _ => {
                matches.sort_by(|a, b| {
                    // deterministic pick: longest shared path prefix with the
                    // stored location wins, then the lowest path for stability
                    let common = |p: &str| {
                        std::path::Path::new(p)
                            .components()
                            .zip(std::path::Path::new(plugin_path).components())
                            .take_while(|(x, y)| x == y)
                            .count()
                    };
                    let ra = (common(path_of(a)), path_of(a));
                    let rb = (common(path_of(b)), path_of(b));
                    rb.0.cmp(&ra.0).then(ra.1.cmp(rb.1))
                });
                let found = matches[0];
                return (
                    found.clone(),
                    Resolved::Ambiguous(std::path::PathBuf::from(path_of(found))),
                );
            }
        }
    }
    (stored.clone(), Resolved::Missing)
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
    /// Damper pedal off goes first — a held pedal catches the notes-off
    /// and keeps ringing — and the wheel is re-centered after (#215).
    pub fn panic(&mut self) {
        for ch in 0u8..16 {
            let _ = self.send(&[0xB0 | ch, 64, 0]); // Damper off
            let _ = self.send(&[0xB0 | ch, 123, 0]); // All Notes Off
            let _ = self.send(&[0xB0 | ch, 121, 0]); // Reset All Controllers
            let _ = self.send(&[0xE0 | ch, 0, 64]); // bend center
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
        let arrival = now.saturating_duration_since(self.t0).as_micros() as u64;
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

    /// Stream clock reading at call time, in the same latency-compensated
    /// domain `stamp()` reports — µs since `new()` under arrival stamping;
    /// once a device timestamp anchors the map, advances by host time from
    /// the anchor (the backend clock can't be queried outside the callback).
    /// Used to rebase a take's zero when recording starts long after the
    /// input opened (#159 arm-vs-record split). Sharing `stamp()`'s
    /// compensated timebase keeps the take anchor comparable to stamped
    /// events: an uncompensated `now_us` made the first `latency_us` of a
    /// take clamp to zero and drop count-in strikes (#220).
    pub fn now_us(&self) -> u64 {
        let raw = match self.anchor {
            Some((_, i0)) => {
                let base = i0.saturating_duration_since(self.t0).as_micros() as u64;
                base.saturating_add(i0.elapsed().as_micros() as u64)
            }
            None => self.t0.elapsed().as_micros() as u64,
        };
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
    /// stream clock — `now_us()` reads the timestamp domain `cb` sees
    tb: std::sync::Arc<std::sync::Mutex<Timebase>>,
}

impl Input {
    /// Current reading of the timestamp domain the callback reports (#159):
    /// lets a take rebase its zero when recording engages after monitoring.
    pub fn now_us(&self) -> u64 {
        self.tb.lock().unwrap_or_else(|e| e.into_inner()).now_us()
    }
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
        let tb = std::sync::Arc::new(std::sync::Mutex::new(Timebase::new(
            opts.latency_us,
            opts.diag,
        )));
        let tb2 = tb.clone();
        let conn = inp
            .connect(
                &port,
                "midi-editor-in",
                move |ts, bytes, _| {
                    let us = tb2
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .stamp(ts, std::time::Instant::now());
                    cb(us, bytes)
                },
                (),
            )
            .map_err(|e| Error::Connect(e.to_string()))?;
        Ok(Self {
            _conn: conn,
            name,
            ord,
            tb,
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
            // damper off first — a held pedal catches the notes-off and
            // keeps the notes ringing through the pause (#215); the wheel
            // re-centers so a bend doesn't hold its detune into the stop
            self.send_at(&[0xB0 | ch, 64, 0], 0);
            self.send_at(&[0xB0 | ch, 123, 0], 0);
            self.send_at(&[0xE0 | ch, 0, 64], 0);
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

/// Time source the playback schedule runs against. `SystemClock` drives real
/// playback; tests substitute a manual clock so sequencing is exercised
/// deterministically — no wall-clock sleeps, no timing tolerances.
pub trait Clock {
    /// Current time in µs. The epoch is arbitrary — only differences matter.
    fn now_us(&self) -> u64;
    /// Block until `target_us` (same epoch as `now_us`) or until `stop` or
    /// `watch` flips. Returns true if the target was reached; false on abort.
    /// `watch` interrupts the wait so a queued `SchedulePatch` applies at the
    /// next event boundary instead of the next deadline.
    fn wait_until_us(
        &mut self,
        target_us: u64,
        stop: &std::sync::atomic::AtomicBool,
        watch: &std::sync::atomic::AtomicBool,
    ) -> bool;
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
    fn wait_until_us(
        &mut self,
        target_us: u64,
        stop: &std::sync::atomic::AtomicBool,
        watch: &std::sync::atomic::AtomicBool,
    ) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        let target = self.t0 + std::time::Duration::from_micros(target_us);
        loop {
            let now = std::time::Instant::now();
            if now >= target {
                return true;
            }
            if stop.load(Relaxed) || watch.load(Relaxed) {
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

/// A live update to a running schedule, queued via `Playback::update`.
/// `events` replaces the whole schedule in the same absolute-µs domain;
/// `loop_from_us` replaces the wrap point. When `sinks` is `Some` the
/// destination set changed (a routing edit): every old sink is panicked and
/// the new array takes over — event sink indices then refer to the NEW
/// array. With `sinks: None` (content, mute/solo, or loop edit) sinks stay
/// and only notes that lost their scheduled note-off are released — a
/// channel-scoped All Notes Off, so tracks sharing one destination keep
/// their own sounding notes.
pub struct SchedulePatch {
    pub events: Vec<(u64, usize, Vec<u8>)>,
    pub loop_from_us: Option<u64>,
    /// loop's right locator in the same us domain — `None` wraps at the
    /// schedule's last event (the old unbounded behavior)
    pub loop_end_us: Option<u64>,
    pub sinks: Option<Vec<Box<dyn EventSink>>>,
}

/// Control-plane message for a running schedule.
pub enum SchedMsg {
    /// Swap the remaining schedule (and optionally the routing).
    Patch(SchedulePatch),
    /// Full panic on every open sink — the transport keeps running. This is
    /// the "emergency silence" path, distinct from normal-stop cleanup.
    Panic,
}

/// Track one channel message delivery against the sounding-note table.
fn note_sent(
    sounding: &mut std::collections::BTreeMap<(usize, u8), u32>,
    idx: usize,
    bytes: &[u8],
) {
    if bytes.len() < 3 {
        return;
    }
    let ch = bytes[0] & 0x0F;
    match bytes[0] & 0xF0 {
        0x90 if bytes[2] != 0 => *sounding.entry((idx, ch)).or_insert(0) += 1,
        0x80 | 0x90 => {
            let key = (idx, ch);
            if let Some(c) = sounding.get_mut(&key) {
                *c = c.saturating_sub(1);
                if *c == 0 {
                    sounding.remove(&key);
                }
            }
        }
        _ => {}
    }
}

/// Drain every queued patch. Returns true when at least one applied.
/// `from_us` is the timeline point playback resumes from — a sounding note
/// survives only if the new schedule still delivers its note-off at or after
/// that point; otherwise the channel gets CC123 and the entry drops.
fn drain_updates(
    updates: &std::sync::mpsc::Receiver<SchedMsg>,
    events: &mut Vec<(u64, usize, Vec<u8>)>,
    loop_from_us: &mut Option<u64>,
    loop_end_us: &mut Option<u64>,
    sinks: &mut Vec<Box<dyn EventSink>>,
    sounding: &mut std::collections::BTreeMap<(usize, u8), u32>,
    from_us: u64,
) -> bool {
    let mut applied = false;
    while let Ok(msg) = updates.try_recv() {
        let SchedMsg::Patch(p) = msg else {
            // explicit Panic: silence everything now, transport continues —
            // it does not reschedule, so `applied` stays untouched and the
            // caller doesn't rewind `i` into already-sent events
            for s in sinks.iter_mut() {
                s.panic();
            }
            sounding.clear();
            continue;
        };
        applied = true;
        *events = p.events;
        *loop_from_us = p.loop_from_us;
        *loop_end_us = p.loop_end_us;
        match p.sinks {
            Some(new) => {
                for s in sinks.iter_mut() {
                    s.panic();
                }
                *sinks = new;
                sounding.clear();
            }
            None => {
                let keys: Vec<(usize, u8)> = sounding.keys().copied().collect();
                for (s, ch) in keys {
                    let survives = events.iter().any(|(us, si, b)| {
                        *us >= from_us
                            && *si == s
                            && b.len() >= 3
                            && (b[0] & 0x0F) == ch
                            && ((b[0] & 0xF0) == 0x80 || ((b[0] & 0xF0) == 0x90 && b[2] == 0))
                    });
                    if survives {
                        continue;
                    }
                    if let Some(sink) = sinks.get_mut(s) {
                        sink.send_at(&[0xB0 | ch, 123, 0], 0);
                    }
                    sounding.remove(&(s, ch));
                }
            }
        }
    }
    applied
}

/// The scheduling core of `Playback`, generic over `Clock` so tests can run
/// it synchronously on a fake clock. See `Playback::start` for the contract.
///
/// `events` must be sorted by absolute µs; `start_us` seeks (earlier events
/// skipped, clock base = `start_us`); `loop_from_us` notes-offs every sink at
/// the end of each pass and restarts the schedule at that point. `watch`
/// (set by `Playback::update`) interrupts the current wait: queued
/// `SchedulePatch`es are drained, sounding notes that lost their note-off are
/// released, and the remaining schedule resumes from the reached position.
/// Every exit path — stop, end of timeline, or a loop with nothing left to
/// replay — ends with `notes_off()` on every sink, or `panic()` when
/// `panic_on_stop` (reset-on-stop) is set. `pos` is updated as the
/// schedule advances so the UI can draw a playhead.
#[allow(clippy::too_many_arguments)]
pub fn run_schedule(
    clock: &mut impl Clock,
    sinks: &mut Vec<Box<dyn EventSink>>,
    mut events: Vec<(u64, usize, Vec<u8>)>,
    start_us: u64,
    mut loop_from_us: Option<u64>,
    mut loop_end_us: Option<u64>,
    stop: &std::sync::atomic::AtomicBool,
    pos: &std::sync::atomic::AtomicU64,
    watch: &std::sync::atomic::AtomicBool,
    updates: &std::sync::mpsc::Receiver<SchedMsg>,
    panic_on_stop: bool,
) {
    use std::sync::atomic::Ordering::{Acquire, Relaxed};
    let mut base_us = start_us;
    let mut epoch0_us = clock.now_us();
    let mut i = events.partition_point(|(us, _, _)| *us < base_us);
    // (sink, channel) -> note-ons delivered without their note-off yet.
    // Schedule patches consult it so orphaned notes get a channel-scoped
    // All Notes Off instead of hanging.
    let mut sounding: std::collections::BTreeMap<(usize, u8), u32> =
        std::collections::BTreeMap::new();
    'outer: loop {
        // the pass ends at the right locator when one is set — recomputed
        // per event so a patch moving the bound applies mid-pass; an empty
        // or inverted range is ignored so a degenerate loop can't starve
        while i < events.len()
            && events[i].0
                < loop_end_us
                    .filter(|le| loop_from_us.is_none_or(|ls| *le > ls))
                    .unwrap_or(u64::MAX)
        {
            if stop.load(Relaxed) {
                break 'outer;
            }
            if watch.load(Acquire) {
                watch.store(false, Relaxed);
                // rebase at the timeline point actually reached — events
                // due during the drain stay skipped rather than bursting
                let now = clock.now_us();
                base_us += now.saturating_sub(epoch0_us);
                epoch0_us = now;
                if drain_updates(
                    updates,
                    &mut events,
                    &mut loop_from_us,
                    &mut loop_end_us,
                    sinks,
                    &mut sounding,
                    base_us,
                ) {
                    i = events.partition_point(|(us, _, _)| *us < base_us);
                    pos.store(base_us, Relaxed);
                }
                continue;
            }
            let (us, sink_idx, bytes) = &events[i];
            i += 1;
            let us = *us;
            let Some(sink) = sinks.get_mut(*sink_idx) else {
                continue;
            };
            let wake_us = (epoch0_us + (us - base_us)).saturating_sub(sink.lead_us());
            if !clock.wait_until_us(wake_us, stop, watch) {
                if stop.load(Relaxed) {
                    break 'outer;
                }
                continue;
            }
            pos.store(us, Relaxed);
            let deadline_us = epoch0_us + (us - base_us);
            let rem = deadline_us.saturating_sub(clock.now_us());
            sink.send_at(bytes, rem);
            note_sent(&mut sounding, *sink_idx, bytes);
        }
        // a patch may have queued (or emptied) the timeline while the pass
        // ran out — apply it before the wrap decision
        if watch.load(Acquire) {
            watch.store(false, Relaxed);
            let now = clock.now_us();
            base_us += now.saturating_sub(epoch0_us);
            epoch0_us = now;
            if drain_updates(
                updates,
                &mut events,
                &mut loop_from_us,
                &mut loop_end_us,
                sinks,
                &mut sounding,
                base_us,
            ) {
                i = events.partition_point(|(us, _, _)| *us < base_us);
                pos.store(base_us, Relaxed);
                continue 'outer;
            }
        }
        // bounded loop: hold until the right locator before wrapping —
        // the cycle is temporal, so content shorter than the range leaves
        // silence instead of wrapping early (#130)
        if let (Some(ls), Some(le)) = (loop_from_us, loop_end_us) {
            if le > ls && le > base_us {
                let deadline = epoch0_us + (le - base_us);
                if !clock.wait_until_us(deadline, stop, watch) {
                    if stop.load(Relaxed) {
                        break;
                    }
                    continue 'outer; // watch fired — the drain runs next pass
                }
            }
        }
        // loop wrap: release notes but keep tails and controller
        // state — the schedule restarts with chase events at the
        // loop point, which re-establish whatever should sound
        for s in sinks.iter_mut() {
            s.notes_off();
        }
        sounding.clear();
        match loop_from_us {
            Some(ls) => {
                let ni = events.partition_point(|(us, _, _)| *us < ls);
                // with a right locator the range loops even while empty —
                // the temporal cycle itself is the point; unbounded keeps
                // the old "nothing left to replay → stop" guard
                if loop_end_us.is_none() && ni >= events.len() {
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
    // exit cleanup policy (#161): a normal stop releases sounding notes
    // (CC123) and preserves controller/modulation state; `panic_on_stop`
    // (the reset-on-stop preference) adds the full CC123/121/120 sweep.
    if panic_on_stop {
        for s in sinks.iter_mut() {
            s.panic();
        }
    } else {
        for s in sinks.iter_mut() {
            s.notes_off();
        }
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
    updated: std::sync::Arc<std::sync::atomic::AtomicBool>,
    tx: std::sync::mpsc::Sender<SchedMsg>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Playback {
    /// `events` must be sorted by absolute µs. `start_us` seeks: events before
    /// it are skipped and the clock starts at `start_us`. Each event carries
    /// the index of the sink to deliver it to. With `loop_from_us`, reaching
    /// the end all-notes-offs every sink and restarts the schedule at that
    /// point — sinks (and VST3 audio streams) stay alive across the boundary.
    /// Queue later edits through `update` instead of restarting.
    pub fn start(
        mut sinks: Vec<Box<dyn EventSink>>,
        events: Vec<(u64, usize, Vec<u8>)>,
        start_us: u64,
        loop_from_us: Option<u64>,
        loop_end_us: Option<u64>,
        panic_on_stop: bool,
    ) -> Self {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let position_us = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let updated = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel::<SchedMsg>();
        let (stop2, pos2, watch2) = (stop.clone(), position_us.clone(), updated.clone());
        let thread = std::thread::spawn(move || {
            set_timer_resolution(1);
            let mut clock = SystemClock::default();
            run_schedule(
                &mut clock,
                &mut sinks,
                events,
                start_us,
                loop_from_us,
                loop_end_us,
                &stop2,
                &pos2,
                &watch2,
                &rx,
                panic_on_stop,
            );
            set_timer_resolution(0);
        });
        Self {
            stop,
            position_us,
            updated,
            tx,
            thread: Some(thread),
        }
    }

    /// Queue a schedule patch for the running thread; it applies at the next
    /// event boundary (bounded by the 2 ms wait granularity). Events use the
    /// same absolute-µs domain the schedule was built with. No-op once the
    /// schedule thread has exited.
    pub fn update(&self, patch: SchedulePatch) {
        // send first, then flag: an Acquire load of `updated` on the worker
        // guarantees the queued patch is visible to it
        let _ = self.tx.send(SchedMsg::Patch(patch));
        self.updated
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Emergency silence on the running pass: every open sink gets the full
    /// panic burst while the transport keeps going — the user-facing Panic
    /// command, distinct from stop cleanup (which follows `panic_on_stop`).
    pub fn panic_now(&self) {
        let _ = self.tx.send(SchedMsg::Panic);
        self.updated
            .store(true, std::sync::atomic::Ordering::Release);
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
    type TimingLog = Arc<Mutex<Vec<(Vec<u8>, u64)>>>;
    type SendLog = Arc<Mutex<Vec<(u64, Vec<u8>)>>>;

    use super::*;
    use std::sync::atomic::Ordering::Relaxed;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::sync::{Arc, Mutex};

    fn plugin(path: &str, cid: Option<&str>) -> Destination {
        Destination::Plugin {
            plugin_path: path.into(),
            component_id: cid.map(str::to_string),
            vendor: None,
            plugin_name: None,
        }
    }

    /// Routing identity must survive metadata churn: same path always means
    /// same destination (in-place upgrade), and equal component IDs mean the
    /// same plugin even when the bundle moved.
    #[test]
    fn destination_identity_is_path_or_component_id() {
        let a_old = plugin(r"C:\VST3\A.vst3", Some("UID_A"));
        let a_moved = plugin(r"D:\Moved\A.vst3", Some("UID_A"));
        let a_upgraded = plugin(r"C:\VST3\A.vst3", Some("UID_A2"));
        let b = plugin(r"C:\VST3\B.vst3", Some("UID_B"));
        let a_path_only = plugin(r"C:\VST3\A.vst3", None);

        assert!(a_old.same_identity(&a_moved));
        assert!(a_old.same_identity(&a_upgraded)); // same install dir
        assert!(a_path_only.same_identity(&a_old)); // legacy sidecar
        assert!(!a_old.same_identity(&b));
        assert!(!a_moved.same_identity(&b));
        assert!(!a_moved.same_identity(&a_path_only)); // no shared key
    }

    /// Resolution order: exact path first (preferred hint), then a single
    /// component-ID match (moved), a deterministic pick among several, and
    /// the stored identity untouched when nothing matches.
    #[test]
    fn resolve_prefers_path_then_component_id() {
        let catalog = vec![
            plugin(r"C:\VST3\Surge.vst3", Some("UID_SURGE")),
            plugin(r"D:\Instruments\Dexed.vst3", Some("UID_DEX")),
        ];

        // exact path → catalog entry adopted, path hint honored
        let stored = plugin(r"C:\VST3\Surge.vst3", None);
        let (got, outcome) = resolve_plugin_dest(&stored, &catalog);
        assert!(matches!(outcome, Resolved::SamePath));
        assert!(
            matches!(got, Destination::Plugin { component_id: Some(c), .. } if c == "UID_SURGE")
        );

        // moved bundle → resolved by component ID
        let stored = plugin(r"C:\VST3\Dexed.vst3", Some("UID_DEX"));
        let (got, outcome) = resolve_plugin_dest(&stored, &catalog);
        assert!(matches!(outcome, Resolved::Moved(_)));
        assert!(
            matches!(&got, Destination::Plugin { plugin_path, .. } if plugin_path == r"D:\Instruments\Dexed.vst3")
        );

        // nothing matches → identity preserved, flagged missing
        let stored = plugin(r"C:\VST3\Gone.vst3", Some("UID_GONE"));
        let (got, outcome) = resolve_plugin_dest(&stored, &catalog);
        assert!(matches!(outcome, Resolved::Missing));
        assert_eq!(got, stored);
    }

    /// Two installs exposing the same component ID pick the one closest to
    /// the recorded path — deterministically, regardless of catalog order.
    #[test]
    fn resolve_multiple_matches_is_deterministic() {
        let near = plugin(r"C:\VST3\Vendor\Dup.vst3", Some("UID_DUP"));
        let far = plugin(r"E:\Other\Dup.vst3", Some("UID_DUP"));
        let stored = plugin(r"C:\VST3\Dup.vst3", Some("UID_DUP"));
        for catalog in [
            vec![near.clone(), far.clone()],
            vec![far.clone(), near.clone()],
        ] {
            let (got, outcome) = resolve_plugin_dest(&stored, &catalog);
            assert!(matches!(outcome, Resolved::Ambiguous(_)));
            assert!(
                matches!(&got, Destination::Plugin { plugin_path, .. } if plugin_path == r"C:\VST3\Vendor\Dup.vst3")
            );
        }
    }

    /// Sidecars written before identity persistence (path-only Plugin
    /// destination) still deserialize.
    #[test]
    fn legacy_path_only_destination_deserializes() {
        let json = r#"{"Plugin":{"plugin_path":"C:\\VST3\\Old.vst3"}}"#;
        let d: Destination = serde_json::from_str(json).unwrap();
        assert!(matches!(
            d,
            Destination::Plugin {
                plugin_path: _,
                component_id: None,
                vendor: None,
                plugin_name: None
            }
        ));
    }

    /// Manual clock: `wait_until_us` jumps straight to the target — the
    /// schedule runs synchronously, so assertions see exact µs values.
    struct FakeClock {
        now: u64,
    }

    impl Clock for FakeClock {
        fn now_us(&self) -> u64 {
            self.now
        }
        fn wait_until_us(&mut self, target_us: u64, stop: &AtomicBool, watch: &AtomicBool) -> bool {
            self.now = target_us;
            !stop.load(Relaxed) && !watch.load(Relaxed)
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

    impl RecordingSink {
        /// Wall-clock `Playback::start` tests only record traffic — no
        /// external stop is ever raised.
        fn recording(log: Arc<Mutex<Vec<Vec<u8>>>>) -> Self {
            Self {
                log,
                stop: Arc::new(AtomicBool::new(false)),
                stop_after: usize::MAX,
                sends: 0,
            }
        }
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

    /// Sink that also records `rem_us` — the value a VST3 sink converts to a
    /// sample offset, so its schedule-vs-deadline relationship is testable.
    struct TimingSink(TimingLog, u64);

    impl EventSink for TimingSink {
        fn lead_us(&self) -> u64 {
            self.1
        }
        fn send_at(&mut self, bytes: &[u8], rem_us: u64) {
            self.0.lock().unwrap().push((bytes.to_vec(), rem_us));
        }
        fn panic(&mut self) {}
        fn notes_off(&mut self) {} // stop() cleanup isn't scheduled traffic
    }

    fn wait_for(log: &TimingLog, n: usize) -> Vec<(Vec<u8>, u64)> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let snap = log.lock().unwrap().clone();
            if snap.len() >= n {
                return snap;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "schedule delivered {} events, wanted {n}",
                snap.len()
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// A sink waking `lead_us` early sees `rem ≈ lead` at delivery — the
    /// offset a VST3 sink schedules on the same clock the plugin's
    /// ProcessContext advances on. Slack is generous for wall-clock jitter;
    /// the invariant is rem lands just under/at the lead, never past it.
    #[test]
    fn send_at_rem_tracks_lead_within_jitter() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let lead = 20_000u64;
        let events = vec![
            (200_000u64, 0usize, vec![0x90, 60, 100]),
            (300_000u64, 0usize, vec![0x80, 60, 0]),
        ];
        let mut pb = Playback::start(
            vec![Box::new(TimingSink(log.clone(), lead))],
            events,
            0,
            None,
            None,
            false,
        );
        let snap = wait_for(&log, 2);
        pb.stop();
        for (b, rem) in &snap {
            assert!(
                *rem <= lead && *rem + 10_000 >= lead,
                "rem={rem} for {b:x?} drifted too far from lead={lead}"
            );
        }
    }

    /// Seeking past an event skips it and keeps the survivor's rem on the
    /// same deadline clock — the sample position it produces is the same as
    /// if playback had run through from zero.
    #[test]
    fn seek_skips_past_events_and_keeps_deadline_rem() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let lead = 10_000u64;
        let events = vec![
            (100_000u64, 0usize, vec![0x90, 60, 100]), // before the seek point
            (1_000_000u64, 0usize, vec![0x90, 64, 100]),
        ];
        let mut pb = Playback::start(
            vec![Box::new(TimingSink(log.clone(), lead))],
            events,
            900_000, // 100 ms before the second event
            None,
            None,
            false,
        );
        let snap = wait_for(&log, 1);
        pb.stop();
        assert_eq!(snap.len(), 1, "the pre-seek event leaked: {snap:x?}");
        assert_eq!(snap[0].0, vec![0x90, 64, 100]);
        // same lead discipline as an unseeked event: rem ≤ lead, close to it
        assert!(
            snap[0].1 <= lead && snap[0].1 + 10_000 >= lead,
            "rem={}",
            snap[0].1
        );
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
        let watch = AtomicBool::new(false);
        let (_tx, rx) = std::sync::mpsc::channel();
        run_schedule(
            &mut clock,
            &mut sinks,
            events,
            0,
            Some(0),
            None,
            &stop,
            &pos,
            &watch,
            &rx,
            true,
        );
        let snapshot = log.lock().unwrap().clone();
        // note-on played twice → the schedule wrapped and replayed
        assert_eq!(
            snapshot
                .iter()
                .filter(|b| *b == &vec![0x90, 60, 100])
                .count(),
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

    /// #137 — a parked count-in pass: the hold emits only clicks, the
    /// song's first event lands exactly on the deferred boundary, and
    /// `pos` reports parked-domain µs (the view subtracts the hold to
    /// recover document position). Sink also captures `pos` per send so
    /// the wall-time ↔ playhead ↔ recorded-tick contract is checkable:
    /// input heard ON a boundary event maps to `base + rel − cin` — the
    /// record start — so click and capture share the same boundary.
    #[test]
    fn parked_countin_aligns_wall_playhead_and_capture() {
        /// `(position µs at send, bytes)` — the value the UI playhead
        /// mirrors, paired with what was emitted.
        type PosLog = Arc<Mutex<Vec<(u64, Vec<u8>)>>>;

        /// `pos`-observing sink: records the schedule position reported
        /// for every send.
        struct PosSink {
            log: PosLog,
            pos: Arc<AtomicU64>,
        }

        impl EventSink for PosSink {
            fn send_at(&mut self, bytes: &[u8], _rem_us: u64) {
                let p = self.pos.load(Relaxed);
                self.log.lock().unwrap().push((p, bytes.to_vec()));
            }
            fn notes_off(&mut self) {}
            fn panic(&mut self) {}
        }

        let start = 1_000_000u64;
        let cin = 500_000u64;
        let boundary = start + cin;
        // parked schedule as the view builds it: clicks fill the hold,
        // an accented click + the first note sit on the boundary
        let events = vec![
            (start, 0usize, vec![0x99, 77, 110]),
            (start + 250_000, 0usize, vec![0x99, 77, 110]),
            (boundary, 0usize, vec![0x99, 76, 110]),
            (boundary, 0usize, vec![0x90, 60, 100]),
            (boundary + 100_000, 0usize, vec![0x80, 60, 0]),
        ];
        let log: PosLog = Arc::new(Mutex::new(Vec::new()));
        let pos = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let watch = AtomicBool::new(false);
        let (_tx, rx) = std::sync::mpsc::channel();
        let mut sinks: Vec<Box<dyn EventSink>> = vec![Box::new(PosSink {
            log: log.clone(),
            pos: pos.clone(),
        })];
        let mut clock = FakeClock { now: 0 };
        run_schedule(
            &mut clock,
            &mut sinks,
            events,
            start,
            None,
            None,
            &stop,
            pos.as_ref(),
            &watch,
            &rx,
            false,
        );
        let got = log.lock().unwrap().clone();
        // `pos` reports each event's parked µs at send time
        assert_eq!(
            got.iter().map(|(p, _)| *p).collect::<Vec<_>>(),
            vec![
                start,
                start + 250_000,
                boundary,
                boundary,
                boundary + 100_000
            ]
        );
        // inside the hold only clicks (0x99) emitted; the first channel
        // event landed exactly at the boundary — never a moment early
        let first_note = got.iter().position(|(_, b)| b[0] == 0x90).unwrap();
        assert_eq!(got[first_note].0, boundary);
        assert!(got[..first_note].iter().all(|(_, b)| b[0] == 0x99));
        // the capture contract: the take's `rel` is the input clock since
        // engage (epoch ≈ ref), so an event heard at parked time `p` has
        // rel = p − start and maps to doc_us = base + rel − cin = p − cin
        // — a note struck when the boundary event sounds lands exactly on
        // the record start (`base`)
        let base = start;
        let struck_on_boundary = base + (boundary - start) - cin;
        assert_eq!(struck_on_boundary, base);
        // and one struck mid-song lands on the heard event's doc position
        let heard = boundary + 100_000;
        assert_eq!(base + (heard - start) - cin, heard - cin);
    }

    // --- live schedule updates (#140/#141) ----------------------------------

    /// Sink that queues a `SchedulePatch` after its `fire_after`-th send —
    /// the update lands mid-run while later events are still pending. This
    /// is what `Playback::update` does (send, then flag), driven from inside
    /// the synchronous fake-clock run.
    struct PatchSink {
        log: Arc<Mutex<Vec<Vec<u8>>>>,
        tx: std::sync::mpsc::Sender<SchedMsg>,
        watch: Arc<AtomicBool>,
        patch: Mutex<Option<SchedulePatch>>,
        /// like `patch` but queues `SchedMsg::Panic` — the mid-run
        /// equivalent of `Playback::panic_now`
        panic: bool,
        fire_after: usize,
        sends: usize,
        stop_after: Option<(usize, Arc<AtomicBool>)>,
    }

    impl EventSink for PatchSink {
        fn send_at(&mut self, bytes: &[u8], _rem_us: u64) {
            self.log.lock().unwrap().push(bytes.to_vec());
            self.sends += 1;
            if self.sends == self.fire_after {
                let msg = if self.panic {
                    Some(SchedMsg::Panic)
                } else {
                    self.patch.lock().unwrap().take().map(SchedMsg::Patch)
                };
                if let Some(m) = msg {
                    let _ = self.tx.send(m);
                    self.watch.store(true, std::sync::atomic::Ordering::Release);
                }
            }
            if let Some((n, stop)) = &self.stop_after {
                if self.sends == *n {
                    stop.store(true, Relaxed);
                }
            }
        }
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

    fn is_cc123_on(b: &[u8], ch: u8) -> bool {
        b.len() == 3 && b[0] == 0xB0 | ch && b[1] == 123
    }

    /// A committed edit swaps the future schedule mid-pass: the deleted
    /// note's dangling on gets a channel-scoped All Notes Off, while the
    /// untouched channel on the same sink keeps its scheduled note-off —
    /// mute-style silence without disturbing sibling tracks.
    #[test]
    fn events_patch_releases_only_orphaned_channels() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let pos = AtomicU64::new(0);
        let watch = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        // patched timeline: ch0's note-off is deleted; ch1's stays
        let patch = SchedulePatch {
            events: vec![(5_000u64, 0usize, vec![0x81, 64, 0])],
            loop_from_us: None,
            loop_end_us: None,
            sinks: None,
        };
        let mut sinks: Vec<Box<dyn EventSink>> = vec![Box::new(PatchSink {
            log: log.clone(),
            tx,
            watch: watch.clone(),
            patch: Mutex::new(Some(patch)),
            panic: false,
            fire_after: 2,
            sends: 0,
            stop_after: None,
        })];
        let events = vec![
            (0u64, 0usize, vec![0x90, 60, 100]),
            (1_000u64, 0usize, vec![0x91, 64, 100]),
            (5_000u64, 0usize, vec![0x81, 64, 0]),
            (6_000u64, 0usize, vec![0x80, 60, 0]),
        ];
        let mut clock = FakeClock { now: 0 };
        run_schedule(
            &mut clock, &mut sinks, events, 0, None, None, &stop, &pos, &watch, &rx, false,
        );
        let snap = log.lock().unwrap().clone();
        // orphaned ch0: CC123 lands before the surviving ch1 note-off
        let orphan = snap
            .iter()
            .position(|b| is_cc123_on(b, 0))
            .expect("ch0 CC123");
        let ch1_off = snap
            .iter()
            .position(|b| *b == vec![0x81, 64, 0])
            .expect("ch1 note-off played");
        assert!(orphan < ch1_off, "orphan release must precede ch1 off");
        // the surviving channel sees no early CC123 — only the final panic's
        let ch1_cc123 = snap
            .iter()
            .position(|b| is_cc123_on(b, 1))
            .expect("panic releases ch1");
        assert!(ch1_cc123 > ch1_off, "ch1 must not be cut early");
    }

    /// A routing change panics the old sink and continues the pass on the
    /// replacement — future events arrive at the new destination.
    #[test]
    fn routing_patch_panics_old_sink_and_moves_to_new() {
        let log_a = Arc::new(Mutex::new(Vec::new()));
        let log_b = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let pos = AtomicU64::new(0);
        let watch = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let patch = SchedulePatch {
            events: vec![(5_000u64, 0usize, vec![0x80, 60, 0])],
            loop_from_us: None,
            loop_end_us: None,
            sinks: Some(vec![Box::new(RecordingSink::recording(log_b.clone()))]),
        };
        let mut sinks: Vec<Box<dyn EventSink>> = vec![Box::new(PatchSink {
            log: log_a.clone(),
            tx,
            watch: watch.clone(),
            patch: Mutex::new(Some(patch)),
            panic: false,
            fire_after: 1,
            sends: 0,
            stop_after: None,
        })];
        let events = vec![
            (0u64, 0usize, vec![0x90, 60, 100]),
            (5_000u64, 0usize, vec![0x80, 60, 0]),
        ];
        let mut clock = FakeClock { now: 0 };
        run_schedule(
            &mut clock, &mut sinks, events, 0, None, None, &stop, &pos, &watch, &rx, false,
        );
        let a = log_a.lock().unwrap().clone();
        let b = log_b.lock().unwrap().clone();
        // old sink: note-on, then panic cleanup, nothing else
        assert_eq!(a[0], vec![0x90, 60, 100]);
        assert!(a[1..].iter().all(|b| b[0] & 0xF0 == 0xB0));
        assert!(a[1..].iter().any(|b| b[1] == 120));
        // new sink: receives the remaining schedule verbatim, then the
        // end-of-run panic
        assert_eq!(b[0], vec![0x80, 60, 0]);
        assert!(b[1..].iter().all(|m| m[0] & 0xF0 == 0xB0));
    }

    /// A patch can install a loop the running schedule didn't have: the new
    /// wrap point takes effect at the end of the current pass.
    #[test]
    fn patch_can_install_loop_point() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let pos = AtomicU64::new(0);
        let watch = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let patch = SchedulePatch {
            events: vec![
                (0u64, 0usize, vec![0x90, 60, 100]),
                (5_000u64, 0usize, vec![0x80, 60, 0]),
            ],
            loop_from_us: Some(0),
            loop_end_us: None,
            sinks: None,
        };
        let mut sinks: Vec<Box<dyn EventSink>> = vec![Box::new(PatchSink {
            log: log.clone(),
            tx,
            watch: watch.clone(),
            patch: Mutex::new(Some(patch)),
            panic: false,
            fire_after: 1,
            sends: 0,
            // sends counted: on(1), patch → rebase replays on(2), off(3),
            // wrap notes-off bypasses send_at, pass2 on(4) off(5) → stop
            stop_after: Some((5, stop.clone())),
        })];
        let events = vec![
            (0u64, 0usize, vec![0x90, 60, 100]),
            (5_000u64, 0usize, vec![0x80, 60, 0]),
        ];
        let mut clock = FakeClock { now: 0 };
        run_schedule(
            &mut clock, &mut sinks, events, 0, None, None, &stop, &pos, &watch, &rx, false,
        );
        let snap = log.lock().unwrap().clone();
        // rebase replay + the installed loop's wrap replay = 3 note-ons
        assert_eq!(
            snap.iter().filter(|b| *b == &vec![0x90, 60, 100]).count(),
            3
        );
        // wrap cleanup happened between passes
        assert!(snap.iter().any(|b| is_cc123_on(b, 0)));
    }

    /// Clock that flips `stop` after `max` bounded-loop waits — drives an
    /// empty or all-silent range for a fixed number of cycles, where a
    /// send-counting sink could never trigger (no events are delivered).
    struct WaitStopClock {
        now: u64,
        waits: u64,
        max: u64,
        stop: Arc<AtomicBool>,
    }

    impl Clock for WaitStopClock {
        fn now_us(&self) -> u64 {
            self.now
        }
        fn wait_until_us(&mut self, target_us: u64, stop: &AtomicBool, watch: &AtomicBool) -> bool {
            self.now = target_us;
            self.waits += 1;
            if self.waits >= self.max {
                self.stop.store(true, Relaxed);
            }
            !stop.load(Relaxed) && !watch.load(Relaxed)
        }
    }

    /// #130: an explicit right locator turns the wrap into a temporal cycle —
    /// content that ends early waits out the rest of the range instead of
    /// wrapping ahead of time.
    #[test]
    fn bounded_loop_waits_for_right_locator() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let pos = AtomicU64::new(0);
        let watch = Arc::new(AtomicBool::new(false));
        let (_tx, rx) = std::sync::mpsc::channel();
        let mut sinks: Vec<Box<dyn EventSink>> = vec![Box::new(PatchSink {
            log: log.clone(),
            tx: _tx.clone(),
            watch: watch.clone(),
            patch: Mutex::new(None),
            panic: false,
            fire_after: usize::MAX,
            sends: 0,
            // on, off, wrap CC123s bypass send_at, on, off → stop
            stop_after: Some((4, stop.clone())),
        })];
        let events = vec![
            (0u64, 0usize, vec![0x90, 60, 100]),
            (5_000u64, 0usize, vec![0x80, 60, 0]),
        ];
        let mut clock = WaitStopClock {
            now: 0,
            waits: 0,
            max: u64::MAX,
            stop: stop.clone(),
        };
        // loop 0..20_000 over a 5_000-long phrase → two passes per log
        run_schedule(
            &mut clock,
            &mut sinks,
            events,
            0,
            Some(0),
            Some(20_000),
            &stop,
            &pos,
            &watch,
            &rx,
            false,
        );
        let snap = log.lock().unwrap().clone();
        // two passes → two note-ons; CC123 cleanup between them
        assert_eq!(
            snap.iter().filter(|b| *b == &vec![0x90, 60, 100]).count(),
            2
        );
        assert!(snap.iter().any(|b| is_cc123_on(b, 0)));
        // the clock actually held until the right locator before wrapping —
        // the wrap wait drove `now` to 20_000 (and beyond on the 2nd pass)
        assert!(clock.now >= 20_000);
    }

    /// #130: a bounded loop over a range with no scheduled events keeps
    /// cycling on the temporal bound — it neither exits early nor spins a
    /// single pass.
    #[test]
    fn empty_bounded_range_cycles_until_stop() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let pos = AtomicU64::new(0);
        let watch = Arc::new(AtomicBool::new(false));
        let (_tx, rx) = std::sync::mpsc::channel();
        let mut sinks: Vec<Box<dyn EventSink>> = vec![Box::new(PatchSink {
            log: log.clone(),
            tx: _tx,
            watch: watch.clone(),
            patch: Mutex::new(None),
            panic: false,
            fire_after: usize::MAX,
            sends: 0,
            stop_after: None,
        })];
        // only event sits past the right locator — never delivered
        let events = vec![(50_000u64, 0usize, vec![0x90, 60, 100])];
        let mut clock = WaitStopClock {
            now: 0,
            waits: 0,
            max: 4, // four wrap waits, then stop
            stop: stop.clone(),
        };
        run_schedule(
            &mut clock,
            &mut sinks,
            events,
            0,
            Some(0),
            Some(10_000),
            &stop,
            &pos,
            &watch,
            &rx,
            false,
        );
        // it cycled on the bound repeatedly — each wait hit the locator
        assert!(clock.waits >= 4);
        // and never delivered the out-of-range note
        assert!(log.lock().unwrap().iter().all(|b| b[0] & 0xF0 == 0xB0));
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
        while stats
            .last_send_us
            .load(std::sync::atomic::Ordering::Relaxed)
            == 0
        {
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
            log: SendLog,
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
        let mut pb = Playback::start(vec![Box::new(sink)], events, 0, None, None, false);
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

    /// Seek: events scheduled before `start_us` are skipped at the same
    /// partition boundary the caller inserts chase events into.
    #[test]
    fn seek_skips_events_before_start() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let events = vec![
            (0u64, 0usize, vec![0x90, 60, 100]),
            (4_000u64, 0usize, vec![0x90, 62, 100]),
            (8_000u64, 0usize, vec![0x90, 64, 100]),
        ];
        let mut pb = Playback::start(
            vec![Box::new(RecordingSink::recording(log.clone()))],
            events,
            5_000,
            None,
            None,
            false,
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while log.lock().unwrap().is_empty() {
            assert!(std::time::Instant::now() < deadline, "no event arrived");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        pb.stop();
        let sent = log.lock().unwrap().clone();
        // only the 8ms event plays; the two earlier ones are behind the seek
        assert!(sent.contains(&vec![0x90, 64, 100]));
        assert!(!sent.contains(&vec![0x90, 60, 100]));
        assert!(!sent.contains(&vec![0x90, 62, 100]));
    }

    /// Events sharing a timestamp are delivered in schedule order — the
    /// ordering rule that lets SysEx/setup traffic precede notes struck at
    /// the same instant.
    #[test]
    fn equal_timestamps_keep_schedule_order() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let events = vec![
            (0u64, 0usize, vec![0xF0, 0x7E, 0xF7]), // sysex first
            (0u64, 0usize, vec![0xB0, 121, 0]),
            (0u64, 0usize, vec![0x90, 60, 100]),
        ];
        let mut pb = Playback::start(
            vec![Box::new(RecordingSink::recording(log.clone()))],
            events,
            0,
            None,
            None,
            false,
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let have = log.lock().unwrap().len();
            if have >= 3 {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "events not delivered");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let sent = log.lock().unwrap().clone();
        pb.stop();
        // trailing notes_off/panic resets may follow; only the schedule's
        // own order matters here
        assert_eq!(
            &sent[..3],
            [
                vec![0xF0, 0x7E, 0xF7],
                vec![0xB0, 121, 0],
                vec![0x90, 60, 100]
            ]
            .as_slice()
        );
    }

    /// Stopping mid-play always ends with the full panic reset (121/120),
    /// not just the loop-boundary notes-off.
    #[test]
    fn stop_sends_full_panic_reset() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let events = vec![
            (0u64, 0usize, vec![0x90, 60, 100]),
            (60_000_000u64, 0usize, vec![0x80, 60, 0]), // far out: still running when stopped
        ];
        let mut pb = Playback::start(
            vec![Box::new(RecordingSink::recording(log.clone()))],
            events,
            0,
            None,
            None,
            true, // reset-on-stop preference enabled
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
        pb.stop();
        let sent = log.lock().unwrap().clone();
        for ctl in [123u8, 121, 120] {
            assert!(
                sent.iter().any(|b| b == &vec![0xB0, ctl, 0]),
                "missing panic controller {ctl}"
            );
        }
    }

    /// Default stop cleanup is notes-off only: CC123 silences sounding
    /// notes, but CC121/120 (controller/sound reset) are NOT sent — a
    /// normal stop must not zero modulation/expression on external gear.
    #[test]
    fn normal_stop_sends_notes_off_only() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let events = vec![
            (0u64, 0usize, vec![0x90, 60, 100]),
            (60_000_000u64, 0usize, vec![0x80, 60, 0]),
        ];
        let mut pb = Playback::start(
            vec![Box::new(RecordingSink::recording(log.clone()))],
            events,
            0,
            None,
            None,
            false,
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
        pb.stop();
        let sent = log.lock().unwrap().clone();
        assert!(
            sent.iter().any(|b| b == &vec![0xB0, 123, 0]),
            "missing All Notes Off"
        );
        for ctl in [121u8, 120] {
            assert!(
                !sent.iter().any(|b| b == &vec![0xB0, ctl, 0]),
                "normal stop sent reset controller {ctl}"
            );
        }
    }

    /// Explicit Panic on the running pass delivers the full reset burst
    /// while the transport keeps scheduling — driven as `SchedMsg::Panic`
    /// (what `Playback::panic_now` sends) on the fake clock (#161).
    #[test]
    fn panic_now_sends_full_reset_while_running() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let pos = AtomicU64::new(0);
        let watch = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let mut sinks: Vec<Box<dyn EventSink>> = vec![Box::new(PatchSink {
            log: log.clone(),
            tx,
            watch: watch.clone(),
            patch: Mutex::new(None),
            panic: true,
            // panic queues after the first note-on; stop after the last
            fire_after: 1,
            sends: 0,
            stop_after: Some((3, stop.clone())),
        })];
        let events = vec![
            (0u64, 0usize, vec![0x90, 60, 100]),
            (5_000u64, 0usize, vec![0x80, 60, 0]),
            (8_000u64, 0usize, vec![0x90, 62, 100]),
        ];
        let mut clock = FakeClock { now: 0 };
        run_schedule(
            &mut clock, &mut sinks, events, 0, None, None, &stop, &pos, &watch, &rx, false,
        );
        let sent = log.lock().unwrap().clone();
        for ctl in [123u8, 121, 120] {
            assert!(
                sent.iter().any(|b| b == &vec![0xB0, ctl, 0]),
                "missing panic controller {ctl}"
            );
        }
        // the burst landed mid-run: panic bytes sit between the two note-ons
        let on60 = sent.iter().position(|b| *b == vec![0x90, 60, 100]).unwrap();
        let on62 = sent.iter().position(|b| *b == vec![0x90, 62, 100]).unwrap();
        let panic_at = sent.iter().position(|b| *b == vec![0xB0, 121, 0]).unwrap();
        assert!(on60 < panic_at && panic_at < on62);
    }
}

#[cfg(test)]
mod now_us_tests {
    use super::Timebase;
    use std::time::Duration;

    /// #220 — `now_us` shares `stamp()`'s latency-compensated timebase, so
    /// the take anchor and stamped events are comparable and a strike in
    /// the first latency window is not clamped away.
    #[test]
    fn now_us_subtracts_input_latency() {
        let t0 = std::time::Instant::now();
        let tb = Timebase::new_at(1_000, None, t0);
        std::thread::sleep(Duration::from_millis(20));
        let raw = t0.elapsed().as_micros() as u64;
        let n = tb.now_us();
        let d = raw.saturating_sub(n);
        assert!(
            (500..=1_500).contains(&d),
            "now_us must sit ~1000µs (the latency) below the raw clock, got {d}"
        );
    }

    #[test]
    fn now_us_saturates_at_zero() {
        let t0 = std::time::Instant::now();
        let tb = Timebase::new_at(1_000_000, None, t0);
        assert_eq!(tb.now_us(), 0, "before the latency window: clamps at zero");
    }
}
