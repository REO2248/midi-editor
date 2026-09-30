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
                Destination::MidiPort { port_name: a },
                Destination::MidiPort { port_name: b },
            ) => a == b,
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
    if let Some(exact) = catalog.iter().find(|d| {
        matches!(d, Destination::Plugin { plugin_path: p, .. } if p == plugin_path)
    }) {
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
        assert!(matches!(got, Destination::Plugin { component_id: Some(c), .. } if c == "UID_SURGE"));

        // moved bundle → resolved by component ID
        let stored = plugin(r"C:\VST3\Dexed.vst3", Some("UID_DEX"));
        let (got, outcome) = resolve_plugin_dest(&stored, &catalog);
        assert!(matches!(outcome, Resolved::Moved(_)));
        assert!(matches!(&got, Destination::Plugin { plugin_path, .. } if plugin_path == r"D:\Instruments\Dexed.vst3"));

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
            assert!(matches!(&got, Destination::Plugin { plugin_path, .. } if plugin_path == r"C:\VST3\Vendor\Dup.vst3"));
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
}
