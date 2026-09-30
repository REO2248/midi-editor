//! Per-track output destination. The playback engine fans events out to the
//! destination of each track: a MIDI port (GS Wavetable, loopMIDI cable,
//! physical interface — all the same WinMM path) or a hosted VST3 plugin
//! instance driven by vst3-host + cpal.

/// Destination identity lives in midi-io (name-addressed ports); re-exported
/// here so `output::Destination` keeps working.
pub use midi_io::Destination;

/// A discovered VST3 bundle.
#[derive(Debug, Clone)]
pub struct PluginInfo {
    pub name: String,
    pub path: std::path::PathBuf,
    pub vendor: String,
}

pub fn sidecar_binary(stem: &str) -> Option<std::path::PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let exe = dir.join(format!("{stem}.exe"));
    if exe.exists() {
        return Some(exe);
    }
    let plain = dir.join(stem);
    plain.exists().then_some(plain)
}

pub fn init_env() {
    for (env, stem) in [
        ("VST3_HOST_PROBE_PATH", "vst3-host-probe"),
        ("VST3_HOST_HELPER_PATH", "vst3-host-helper"),
    ] {
        if std::env::var_os(env).is_none() {
            if let Some(path) = sidecar_binary(stem) {
                #[allow(unused_unsafe)]
                unsafe {
                    std::env::set_var(env, path);
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct HostDiag {
    pub helper: Option<std::path::PathBuf>,
    pub probe: Option<std::path::PathBuf>,
    pub audio_device: Result<String, String>,
}

pub fn host_diag() -> HostDiag {
    use cpal::traits::{DeviceTrait, HostTrait};
    let audio_device = cpal::default_host()
        .default_output_device()
        .ok_or_else(|| "no default output device".to_string())
        .and_then(|device| {
            device
                .description()
                .map(|d| d.name().to_string())
                .map_err(|e| e.to_string())
        });
    HostDiag {
        helper: sidecar_binary("vst3-host-helper"),
        probe: sidecar_binary("vst3-host-probe"),
        audio_device,
    }
}

/// VST3 host factory: plugins load into `vst3-host-helper` subprocesses so a
/// crashing or hanging plugin cannot take the app down. `auto_recover`
/// respawns the helper and retries control-plane commands transparently.
fn new_host() -> Result<vst3_host::Vst3Host, PluginError> {
    let mut builder = vst3_host::Vst3Host::builder()
        .scan_default_paths()
        .with_process_isolation(true)
        .auto_recover_plugins(true)
        .auto_recover_max_retries(1);
    if let Some(path) = sidecar_binary("vst3-host-helper") {
        builder = builder.helper_path(path);
    }
    builder
        .build()
        .map_err(|e| PluginError::Host(e.to_string()))
}

/// In-process host for editor windows only. vst3-host's isolated GUI loop is
/// macOS-only — on Windows an isolated plugin cannot open its editor, so the
/// editor instance loads in-process instead. Used solely for `load_for_gui`;
/// playback always goes through `new_host()`.
fn new_host_in_process() -> Result<vst3_host::Vst3Host, PluginError> {
    vst3_host::Vst3Host::builder()
        .scan_default_paths()
        .build()
        .map_err(|e| PluginError::Host(e.to_string()))
}

/// Scan the standard VST3 install locations. Prefers the crash-resistant probe
/// (each bundle is introspected in a subprocess); falls back to listing
/// filenames when the probe binary isn't shipped alongside the app.
#[derive(Debug)]
pub struct ScanReport {
    pub plugins: Vec<PluginInfo>,
    pub probe_used: bool,
    pub skipped: Vec<(std::path::PathBuf, String)>,
}

pub fn discover_plugins() -> ScanReport {
    if let Ok(host) = new_host() {
        let report = host.discover_plugins_safe();
        if report.scan_ran() {
            let mut found: Vec<PluginInfo> = report
                .plugins
                .iter()
                .map(|p| PluginInfo {
                    name: p.info.name.clone(),
                    path: p.info.path.clone(),
                    vendor: p.info.vendor.clone(),
                })
                .collect();
            found.sort_by(|a, b| a.name.cmp(&b.name));
            found.dedup_by(|a, b| a.path == b.path);
            let skipped = report
                .skipped
                .iter()
                .map(|s| {
                    let reason = match s {
                        vst3_host::SafeDiscoverySkip::Crashed { detail, .. } => {
                            format!("crashed: {detail}")
                        }
                        vst3_host::SafeDiscoverySkip::TimedOut { .. } => "timed out".into(),
                        vst3_host::SafeDiscoverySkip::Failed { detail, .. } => {
                            format!("failed: {detail}")
                        }
                    };
                    (s.path().to_path_buf(), reason)
                })
                .collect();
            return ScanReport {
                plugins: found,
                probe_used: true,
                skipped,
            };
        }
    }
    ScanReport {
        plugins: discover_plugin_paths(),
        probe_used: false,
        skipped: Vec::new(),
    }
}

/// Fallback scan: enumerate `.vst3` bundles without loading them.
pub fn discover_plugin_paths() -> Vec<PluginInfo> {
    let mut found = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in vst3_scan_dirs() {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for ent in rd.flatten() {
                let p = ent.path();
                if p.extension().map(|e| e == "vst3").unwrap_or(false) {
                    // several scan roots can resolve to the same bundle
                    // (32/64-bit Common Files junctions, copies in both)
                    let key = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
                    if !seen.insert(key) {
                        continue;
                    }
                    found.push(PluginInfo {
                        name: p
                            .file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "unknown".into()),
                        path: p,
                        vendor: String::new(),
                    });
                }
            }
        }
    }
    found
}

fn vst3_scan_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    for key in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Ok(v) = std::env::var(key) {
            dirs.push(std::path::PathBuf::from(&v).join("Common Files\\VST3"));
            dirs.push(std::path::PathBuf::from(&v).join("VST3"));
        }
    }
    // CommonProgramFiles already ends in "Common Files"
    if let Ok(v) = std::env::var("CommonProgramFiles") {
        dirs.push(std::path::PathBuf::from(&v).join("VST3"));
    }
    if let Ok(v) = std::env::var("CommonProgramFiles(x86)") {
        dirs.push(std::path::PathBuf::from(&v).join("VST3"));
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        dirs.push(std::path::PathBuf::from(local).join("Programs\\Common\\VST3"));
    }
    dirs
}

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("host init failed: {0}")]
    Host(String),
    #[error("plugin load failed: {0}")]
    Load(String),
    #[error("audio start failed: {0}")]
    Audio(String),
}

/// Live view of one hosted plugin's reported processing latency
/// (`IAudioProcessor::getLatencySamples`), in samples.
///
/// Compensation policy: the playback engine wakes each destination
/// `lead_us` early and schedules the event at a sample offset matching its
/// deadline, so the DSP block that renders the event finishes when the
/// deadline arrives. A plugin whose audio emerges `latency` samples late
/// must receive its MIDI `latency` samples sooner — the sink adds the
/// reported latency to its wake-up lead and subtracts it from the scheduled
/// offset (`send_midi_at` clamps at 0 if the deadline already passed). Two
/// plugins reporting different latencies then land their audible output on
/// the same musical instant, aligned with latency-free MIDI ports.
///
/// The count sits in a shared atomic so a `kLatencyChanged` restart can be
/// reflected into live scheduling (`PluginSlot::refresh_latency`) without
/// rebuilding the stream or the sink.
#[derive(Debug, Clone)]
pub struct LatencyComp {
    samples: std::sync::Arc<std::sync::atomic::AtomicU32>,
    /// samples per µs at this stream's rate
    us_to_samples: f64,
}

impl LatencyComp {
    pub fn new(us_to_samples: f64) -> Self {
        Self {
            samples: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
            us_to_samples,
        }
    }

    /// Store a freshly-read `getLatencySamples` value. Takes effect on the
    /// next scheduled event.
    pub fn set_samples(&self, samples: u32) {
        self.samples
            .store(samples, std::sync::atomic::Ordering::Relaxed);
    }

    /// Most recently reported latency, in samples.
    pub fn samples(&self) -> u32 {
        self.samples.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Reported latency in µs (rounded up — waking late is never safe).
    pub fn as_us(&self) -> u64 {
        (self.samples() as f64 / self.us_to_samples).ceil() as u64
    }

    /// Wake-up lead: the sink's base block lead plus the reported latency.
    pub fn lead_us(&self, base_us: u64) -> u64 {
        base_us.saturating_add(self.as_us())
    }

    /// Sample offset for a deadline `rem_us` away, pulled earlier by the
    /// reported latency. Floors at 0 — an event whose compensated time is
    /// already past fires in the current block rather than being dropped.
    pub fn offset(&self, rem_us: u64) -> i32 {
        ((rem_us as f64 * self.us_to_samples) as i64 - i64::from(self.samples())).max(0) as i32
    }
}

/// A loaded, playing VST3 instrument instance. Dropping it stops the audio
/// stream and unloads the plugin.
pub struct PluginOutput {
    // keep the stream alive; MIDI goes through `sink`
    _handle: vst3_host::AudioHandle,
    sink: vst3_host::MidiSink,
    /// samples per µs — for translating the playback thread's remaining-time
    /// hint into `send_midi_at` offsets
    us_to_samples: f64,
    /// reported processing latency; shared with every cloned sink
    latency: LatencyComp,
}

impl PluginOutput {
    /// Load `path` (a .vst3 bundle), start its audio stream on the default
    /// output device via cpal, and return a playable destination.
    pub fn open(path: &std::path::Path) -> Result<Self, PluginError> {
        let mut host = new_host()?;
        let plugin = host
            .load_plugin(path)
            .map_err(|e| PluginError::Load(e.to_string()))?;
        let config = vst3_host::AudioConfig::default();
        let backend = vst3_host::backends::CpalBackend::new()
            .map_err(|e| PluginError::Audio(e.to_string()))?;
        let handle = vst3_host::play_with_backend(&backend, plugin, config)
            .map_err(|e| PluginError::Audio(e.to_string()))?;
        let sink = handle.midi_sink();
        let latency = LatencyComp::new(config.sample_rate / 1_000_000.0);
        // read after the stream is up: latency is only committed once the
        // plugin saw its processing setup
        latency.set_samples(handle.lock().latency_samples());
        Ok(Self {
            _handle: handle,
            sink,
            us_to_samples: config.sample_rate / 1_000_000.0,
            latency,
        })
    }

    /// `EventSink` impl — wakes one audio block plus the reported plugin
    /// latency early and schedules the event at the compensated sample
    /// offset matching its deadline.
    pub fn event_sink(&self) -> PluginSink {
        PluginSink {
            sink: self.sink.clone(),
            plugin: Some(self._handle.plugin()),
            latency: self.latency.clone(),
            base_lead_us: (512.0 / self.us_to_samples) as u64,
        }
    }

    pub fn midi_panic(&self) {
        self._handle.midi_panic();
    }

    /// samples per µs (playback engine translates `rem_us` to sample offsets)
    pub fn us_to_samples(&self) -> f64 {
        self.us_to_samples
    }

    /// Shared latency tracker — clone into slots/sinks; `set_samples` on any
    /// clone is seen by all of them on their next scheduled event.
    pub fn latency(&self) -> LatencyComp {
        self.latency.clone()
    }

    /// Handle to the live plugin instance (e.g. to open its GUI editor).
    pub fn plugin_handle(&self) -> std::sync::Arc<std::sync::Mutex<vst3_host::Plugin>> {
        self._handle.plugin()
    }

    /// Peak output across all channels since the last read (for metering/tests).
    pub fn level(&self) -> f32 {
        self._handle
            .output_levels()
            .channels
            .iter()
            .map(|c| c.peak)
            .fold(0.0, f32::max)
    }
}

/// Load a plugin just for its GUI — no audio stream. The returned instance
/// is not wired to any output; use it to inspect/edit the editor, or hand it
/// to `vst3_host::PluginWindow`. Runs in-process because process-isolated
/// plugins cannot host editors on Windows.
pub fn load_for_gui(
    path: &std::path::Path,
) -> Result<std::sync::Arc<std::sync::Mutex<vst3_host::Plugin>>, PluginError> {
    let mut host = new_host_in_process()?;
    let plugin = host
        .load_plugin(path)
        .map_err(|e| PluginError::Load(e.to_string()))?;
    Ok(std::sync::Arc::new(std::sync::Mutex::new(plugin)))
}

/// Send-safe handle to a plugin instance living on the host worker thread.
/// `PluginOutput` owns the audio stream and is deliberately not `Send`, so it
/// stays on the worker; the app keeps this handle — the sink (lock-free MIDI
/// queue), the shared plugin proxy (parameter/state control plane), and the
/// bundle path — all `Send` — and the plugin keeps running between plays.
pub struct PluginSlot {
    /// lock-free event sink; cheap to clone into each playback run
    pub sink: PluginSink,
    /// shared plugin instance — `set_parameter`/`save_state`/`load_state`/
    /// `set_playing`/`set_tempo` all route into the isolated helper
    pub plugin: std::sync::Arc<std::sync::Mutex<vst3_host::Plugin>>,
    /// the .vst3 bundle this instance was loaded from (guards against stale
    /// slots after a destination re-point or rescan)
    pub path: std::path::PathBuf,
    /// live latency tracker shared with `sink` — the compensation math reads
    /// it per event, so `refresh_latency` retimes playback in place
    pub latency: LatencyComp,
}

impl PluginSlot {
    /// Re-read `getLatencySamples` into the shared tracker. Call it when the
    /// plugin reports a latency change (`kLatencyChanged`) — the sink picks
    /// the new value up on its next event, no stream or instance rebuild.
    /// Returns the value now in effect.
    pub fn refresh_latency(&self) -> u32 {
        let samples = self
            .plugin
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .latency_samples();
        self.latency.set_samples(samples);
        samples
    }
}

/// Work requests for the plugin host worker thread.
pub enum PluginReq {
    /// load+start audio for dest index `usize`; result arrives on the event channel
    Open(usize, std::path::PathBuf),
    /// unload the instance for a dest index (dest re-pointed/rescan)
    Drop(usize),
    /// drop every instance (rescan rebuilt the catalog)
    Clear,
    /// worker exits; instances unload with it
    Shutdown,
}

pub struct PluginEvent {
    pub dest: usize,
    pub path: std::path::PathBuf,
    pub result: Result<PluginSlot, PluginError>,
}

/// Spawn the plugin host thread. It owns every `PluginOutput` (their
/// `AudioHandle`s are not `Send`); the app talks to it through the request
/// channel and receives `PluginSlot` handles on the returned receiver.
/// Requests are processed in order; `Open` replies carry `(dest, result)`.
/// Returns `(requests, events, worker handle)` — the handle joins after
/// `PluginReq::Shutdown` once owned plugin instances have unloaded.
pub fn spawn_plugin_host() -> (
    std::sync::mpsc::Sender<PluginReq>,
    std::sync::mpsc::Receiver<PluginEvent>,
    std::thread::JoinHandle<()>,
) {
    let (req_tx, req_rx) = std::sync::mpsc::channel::<PluginReq>();
    let (evt_tx, evt_rx) = std::sync::mpsc::channel::<PluginEvent>();
    let handle = std::thread::spawn(move || {
        let mut owned: std::collections::HashMap<usize, PluginOutput> =
            std::collections::HashMap::new();
        while let Ok(req) = req_rx.recv() {
            match req {
                PluginReq::Open(d, path) => match PluginOutput::open(&path) {
                    Ok(p) => {
                        let slot = PluginSlot {
                            sink: p.event_sink(),
                            plugin: p.plugin_handle(),
                            path,
                            latency: p.latency(),
                        };
                        owned.insert(d, p);
                        let _ = evt_tx.send(PluginEvent {
                            dest: d,
                            path: slot.path.clone(),
                            result: Ok(slot),
                        });
                    }
                    Err(e) => {
                        let _ = evt_tx.send(PluginEvent {
                            dest: d,
                            path,
                            result: Err(e),
                        });
                    }
                },
                PluginReq::Drop(d) => {
                    owned.remove(&d);
                }
                PluginReq::Clear => owned.clear(),
                PluginReq::Shutdown => break,
            }
        }
        // dropping `owned` unloads every instance; helper subprocesses die
        // with their PluginOutput drops
    });
    (req_tx, evt_rx, handle)
}

#[derive(Clone)]
pub struct PluginSink {
    sink: vst3_host::MidiSink,
    /// control-plane handle for SysEx, which the lock-free `MidiEvent` queue
    /// cannot carry. `None` = SysEx is dropped (legacy construction).
    plugin: Option<std::sync::Arc<std::sync::Mutex<vst3_host::Plugin>>>,
    /// reported plugin latency → wake-up lead / scheduled-offset compensation
    latency: LatencyComp,
    /// wake-up lead before latency compensation (one audio block)
    base_lead_us: u64,
}

impl midi_io::EventSink for PluginSink {
    fn lead_us(&self) -> u64 {
        self.latency.lead_us(self.base_lead_us)
    }
    fn send_at(&mut self, bytes: &[u8], rem_us: u64) {
        let offset = self.latency.offset(rem_us);
        if bytes.first() == Some(&0xF0) {
            // SysEx rides the owned-event path; the lock serializes against
            // audio blocks (bounded by one block) instead of the event ring
            let Some(plugin) = self.plugin.as_ref() else {
                return;
            };
            let mut p = plugin
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Err(e) = p.send_sysex_at(bytes.to_vec(), offset) {
                tracing::warn!("plugin sysex rejected: {e}");
            }
            return;
        }
        if let Some(ev) = channel_event(bytes) {
            self.sink.send_midi_at(ev, offset);
        }
    }
    fn panic(&mut self) {
        // queue an all-notes-off on every channel through the same lock-free path
        for ch in 0..16 {
            let Some(channel) = vst3_host::MidiChannel::from_index(ch) else {
                continue;
            };
            for ctl in [123u8, 121, 120] {
                self.sink.send_midi(vst3_host::MidiEvent::ControlChange {
                    channel,
                    controller: ctl,
                    value: 0,
                });
            }
        }
    }
}

/// Decode raw SMF channel-message bytes into a `vst3_host::MidiEvent`.
/// Returns None for anything we can't map (SysEx, realtime, malformed).
fn channel_event(b: &[u8]) -> Option<vst3_host::MidiEvent> {
    use vst3_host::MidiEvent::*;
    let &status = b.first()?;
    let ch = vst3_host::MidiChannel::from_index(status & 0x0F)?;
    let d1 = *b.get(1)?;
    match status & 0xF0 {
        0x80 => Some(NoteOff {
            channel: ch,
            note: d1,
            velocity: *b.get(2)?,
        }),
        0x90 => {
            let velocity = *b.get(2)?;
            if velocity == 0 {
                Some(NoteOff {
                    channel: ch,
                    note: d1,
                    velocity: 64,
                })
            } else {
                Some(NoteOn {
                    channel: ch,
                    note: d1,
                    velocity,
                })
            }
        }
        0xA0 => Some(PolyAftertouch {
            channel: ch,
            note: d1,
            pressure: *b.get(2)?,
        }),
        0xB0 => Some(ControlChange {
            channel: ch,
            controller: d1,
            value: *b.get(2)?,
        }),
        0xC0 => Some(ProgramChange {
            channel: ch,
            program: d1,
        }),
        0xD0 => Some(ChannelAftertouch {
            channel: ch,
            pressure: d1,
        }),
        0xE0 => Some(PitchBend {
            channel: ch,
            value: (d1 as u16) | ((*b.get(2)? as u16) << 7),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S_PER_US: f64 = 44_100.0 / 1_000_000.0; // 44.1 kHz

    #[test]
    fn offset_without_latency_is_rem_to_samples() {
        let c = LatencyComp::new(S_PER_US);
        // 10 ms → 441 samples
        assert_eq!(c.offset(10_000), 441);
        assert_eq!(c.lead_us(1_000), 1_000);
    }

    #[test]
    fn offset_pulls_earlier_by_latency() {
        let c = LatencyComp::new(S_PER_US);
        c.set_samples(441); // 10 ms at 44.1 kHz
        assert_eq!(c.offset(20_000), 882 - 441);
        // a deadline closer than the latency still fires, at offset 0
        assert_eq!(c.offset(5_000), 0);
    }

    #[test]
    fn lead_includes_latency_rounded_up() {
        let c = LatencyComp::new(S_PER_US);
        c.set_samples(100); // 100/44.1 ≈ 2267.57 µs → 2268
        assert_eq!(c.lead_us(11_000), 11_000 + 2_268);
    }

    /// Heard-time relative to the event deadline, in µs: the sink wakes at
    /// `deadline − rem`, renders the event `offset` samples later, and the
    /// audio then travels `latency` through the plugin.
    fn heard_delta_us(c: &LatencyComp, rem_us: u64) -> i64 {
        ((c.offset(rem_us) as i64 + i64::from(c.samples())) as f64 / S_PER_US) as i64
            - rem_us as i64
    }

    /// The compensation contract: destinations reporting different latencies
    /// deliver audible output at the same instant for one deadline — within
    /// one sample of rounding error.
    #[test]
    fn differing_latencies_align_to_one_timeline() {
        let base_lead = 11_610u64; // one 512-sample block at 44.1 kHz
        for l in [0u32, 441, 2048, 8192] {
            let c = LatencyComp::new(S_PER_US);
            c.set_samples(l);
            // nominal wake: the engine slept lead_us, so rem == lead
            let d = heard_delta_us(&c, c.lead_us(base_lead));
            assert!(d.abs() <= 1, "latency {l}: heard {d}µs off deadline");
        }
        // a late wake that dips below the latency floor lands late by at most
        // `latency − rem`, never dropped
        let c = LatencyComp::new(S_PER_US);
        c.set_samples(4096);
        let rem = 10_000u64; // « 4096/44.1 ≈ 92_880 µs
        assert_eq!(c.offset(rem), 0);
        let d = heard_delta_us(&c, rem);
        assert!(d > 0 && d <= (4096.0 / S_PER_US).ceil() as i64);
    }

    #[test]
    fn shared_atomic_propagates_to_cloned_sinks() {
        let c = LatencyComp::new(S_PER_US);
        let cloned = c.clone(); // what `event_sink()` hands to the app
        cloned.set_samples(2000);
        assert_eq!(c.samples(), 2000);
        assert_eq!(c.offset(100_000), 4410 - 2000);
        // and a later re-read updates scheduling again without a rebuild
        c.set_samples(0);
        assert_eq!(cloned.offset(100_000), 4410);
    }
}
