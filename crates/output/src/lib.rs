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
    /// Bundles that failed the probe during this scan, with the reason.
    pub skipped: Vec<(std::path::PathBuf, String)>,
    /// Bundles quarantined by the cache (previous crash/timeout, unchanged
    /// files) — not probed this scan, with the stored reason.
    pub quarantined: Vec<(std::path::PathBuf, String)>,
    /// Plugins served from the cache without re-probing.
    pub cached_ok: usize,
}

/// Default per-plugin probe bound — re-exported so callers don't depend on
/// vst3-host's internals.
pub const DEFAULT_SCAN_TIMEOUT: std::time::Duration = vst3_host::DEFAULT_PROBE_TIMEOUT;

/// File-system identity of a bundle: cheap proxy for "contents changed".
/// `.vst3` on Windows is usually a directory bundle, so the stamp walks the
/// tree (bounded) rather than statting the top-level entry only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BundleStamp {
    pub files: u64,
    pub size: u64,
    pub mtime_ms: u64,
}

fn mtime_ms(md: &std::fs::Metadata) -> u64 {
    md.modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Walk a bundle (dir or file) and fold its contents into one stamp. Bounded:
/// gives up past 4096 entries — a pathological plugin tree just re-probes.
pub fn bundle_stamp(path: &std::path::Path) -> BundleStamp {
    let mut s = BundleStamp::default();
    let mut visited = std::collections::HashSet::new();
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        if s.files >= 4096 {
            break;
        }
        let Ok(md) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        if md.is_dir() {
            // canonicalize to break symlink cycles within the tree
            let canon = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
            if !visited.insert(canon) {
                continue;
            }
            s.mtime_ms = s.mtime_ms.max(mtime_ms(&md));
            if let Ok(rd) = std::fs::read_dir(&p) {
                for ent in rd.flatten() {
                    stack.push(ent.path());
                }
            }
        } else if md.is_file() {
            s.files += 1;
            s.size += md.len();
            s.mtime_ms = s.mtime_ms.max(mtime_ms(&md));
        }
    }
    s
}

/// A remembered scan outcome for one canonical bundle path.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ScanCacheEntry {
    pub stamp: BundleStamp,
    pub ok: bool,
    pub name: String,
    pub vendor: String,
    /// Probe failure reason (`ok == false`); shown verbatim in the UI.
    #[serde(default)]
    pub reason: String,
    /// Unix ms of the last actual probe attempt.
    #[serde(default)]
    pub last_attempt_ms: u64,
    #[serde(default)]
    pub attempts: u32,
}

/// Persistent scan cache + quarantine list, stored app-wide (not per song).
/// Keyed by canonical bundle path; a matching `BundleStamp` means "unchanged
/// since the recorded probe" — an updated plugin can never collide into a
/// stale result.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ScanCache {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub entries: std::collections::BTreeMap<String, ScanCacheEntry>,
}

impl ScanCache {
    const VERSION: u32 = 1;

    /// Load the cache file; a missing or corrupt file yields an empty cache —
    /// never fatal, a bad cache just costs one extra scan.
    pub fn load(path: &std::path::Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str::<Self>(&s).ok())
            .filter(|c| c.version == Self::VERSION)
            .unwrap_or_default()
    }

    pub fn save(&self, path: &std::path::Path) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let tmp = path.with_extension("tmp");
            if std::fs::write(&tmp, s).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }
}

/// Partition the discovered candidates into "answer from cache" vs "must
/// probe". Pure (no process spawning) so the decision logic is unit-testable.
fn plan_scan(
    cache: &ScanCache,
    candidates: &[std::path::PathBuf],
    force: &std::collections::HashSet<String>,
) -> (
    Vec<PluginInfo>,
    Vec<(std::path::PathBuf, String)>,
    Vec<(std::path::PathBuf, BundleStamp)>,
) {
    let mut cached = Vec::new();
    let mut quarantined = Vec::new();
    let mut probe = Vec::new();
    for p in candidates {
        let canon = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
        let key = canon.to_string_lossy().into_owned();
        let stamp = bundle_stamp(p);
        if !force.contains(&key) {
            if let Some(e) = cache.entries.get(&key) {
                if e.stamp == stamp {
                    if e.ok {
                        cached.push(PluginInfo {
                            name: e.name.clone(),
                            path: p.clone(),
                            vendor: e.vendor.clone(),
                        });
                    } else {
                        quarantined.push((p.clone(), e.reason.clone()));
                    }
                    continue;
                }
            }
        }
        probe.push((p.clone(), stamp));
    }
    (cached, quarantined, probe)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Whether the probe binary resolves — mirrors what `discover_plugins_safe`
/// checks, without spawning anything.
fn probe_available() -> bool {
    sidecar_binary("vst3-host-probe").is_some()
        || std::env::var_os("VST3_HOST_PROBE_PATH")
            .map(|p| std::path::Path::new(&p).exists())
            .unwrap_or(false)
}

/// Cache-aware scan. `cache_file == None` behaves like the old full scan
/// (still writes nothing). `rescan_all` ignores every cached entry;
/// `retry` force-re-probes one path (a quarantined bundle the user asked for).
pub fn discover_plugins_cached(
    cache_file: Option<&std::path::Path>,
    probe_timeout: std::time::Duration,
    rescan_all: bool,
    retry: Option<&std::path::Path>,
) -> ScanReport {
    let candidates: Vec<std::path::PathBuf> = discover_plugin_paths()
        .into_iter()
        .map(|p| p.path)
        .collect();
    let mut cache = cache_file.map(ScanCache::load).unwrap_or_default();
    let force: std::collections::HashSet<String> = if rescan_all {
        candidates
            .iter()
            .map(|p| {
                std::fs::canonicalize(p)
                    .unwrap_or_else(|_| p.clone())
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    } else if let Some(p) = retry {
        std::iter::once(
            std::fs::canonicalize(p)
                .unwrap_or_else(|_| p.to_path_buf())
                .to_string_lossy()
                .into_owned(),
        )
        .collect()
    } else {
        std::collections::HashSet::new()
    };
    let (cached, quarantined, probe_list) = plan_scan(&cache, &candidates, &force);

    let mut found = cached;
    let cached_ok = found.len();
    let mut skipped = Vec::new();
    if probe_available() {
        for (path, stamp) in &probe_list {
            let key = std::fs::canonicalize(path)
                .unwrap_or_else(|_| path.clone())
                .to_string_lossy()
                .into_owned();
            let attempts = cache.entries.get(&key).map(|e| e.attempts + 1).unwrap_or(1);
            match vst3_host::probe_plugin_info_isolated(path, probe_timeout) {
                Ok(info) => {
                    found.push(PluginInfo {
                        name: info.info.name.clone(),
                        path: path.clone(),
                        vendor: info.info.vendor.clone(),
                    });
                    cache.entries.insert(
                        key,
                        ScanCacheEntry {
                            stamp: *stamp,
                            ok: true,
                            name: info.info.name.clone(),
                            vendor: info.info.vendor.clone(),
                            reason: String::new(),
                            last_attempt_ms: now_ms(),
                            attempts,
                        },
                    );
                }
                Err(e) => {
                    let reason = match &e {
                        vst3_host::Error::PluginTimeout => "timed out".to_string(),
                        other => format!("failed: {other}"),
                    };
                    skipped.push((path.clone(), reason.clone()));
                    cache.entries.insert(
                        key,
                        ScanCacheEntry {
                            stamp: *stamp,
                            ok: false,
                            name: String::new(),
                            vendor: String::new(),
                            reason,
                            last_attempt_ms: now_ms(),
                            attempts,
                        },
                    );
                }
            }
        }
    } else {
        // no probe binary — can't distinguish real plugins from broken ones;
        // list every uncached path by filename, same as the old fallback
        for (path, _) in &probe_list {
            found.push(PluginInfo {
                name: path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "unknown".into()),
                path: path.clone(),
                vendor: String::new(),
            });
        }
    }

    // drop entries for bundles that are gone entirely
    let live: std::collections::HashSet<String> = candidates
        .iter()
        .map(|p| {
            std::fs::canonicalize(p)
                .unwrap_or_else(|_| p.clone())
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    cache.entries.retain(|k, _| live.contains(k));
    if let Some(f) = cache_file {
        cache.version = ScanCache::VERSION;
        cache.save(f);
    }

    found.sort_by(|a, b| a.name.cmp(&b.name));
    found.dedup_by(|a, b| a.path == b.path);
    ScanReport {
        plugins: found,
        probe_used: probe_available(),
        skipped,
        quarantined,
        cached_ok,
    }
}

pub fn discover_plugins() -> ScanReport {
    discover_plugins_cached(None, DEFAULT_SCAN_TIMEOUT, false, None)
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

/// A loaded, playing VST3 instrument instance. Dropping it stops the audio
/// stream and unloads the plugin.
pub struct PluginOutput {
    // keep the stream alive; MIDI goes through `sink`
    _handle: vst3_host::AudioHandle,
    sink: vst3_host::MidiSink,
    /// samples per µs — for translating the playback thread's remaining-time
    /// hint into `send_midi_at` offsets
    us_to_samples: f64,
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
        Ok(Self {
            _handle: handle,
            sink,
            us_to_samples: config.sample_rate / 1_000_000.0,
        })
    }

    /// `EventSink` impl — wakes one audio block early and schedules the event
    /// at the sample offset matching its deadline.
    pub fn event_sink(&self) -> PluginSink {
        PluginSink {
            sink: self.sink.clone(),
            plugin: Some(self._handle.plugin()),
            lead_us: (512.0 / self.us_to_samples) as u64,
            us_to_samples: self.us_to_samples,
        }
    }

    pub fn midi_panic(&self) {
        self._handle.midi_panic();
    }

    /// samples per µs (playback engine translates `rem_us` to sample offsets)
    pub fn us_to_samples(&self) -> f64 {
        self.us_to_samples
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
pub fn spawn_plugin_host() -> (
    std::sync::mpsc::Sender<PluginReq>,
    std::sync::mpsc::Receiver<PluginEvent>,
) {
    let (req_tx, req_rx) = std::sync::mpsc::channel::<PluginReq>();
    let (evt_tx, evt_rx) = std::sync::mpsc::channel::<PluginEvent>();
    std::thread::spawn(move || {
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
    });
    (req_tx, evt_rx)
}

#[derive(Clone)]
pub struct PluginSink {
    sink: vst3_host::MidiSink,
    /// control-plane handle for SysEx, which the lock-free `MidiEvent` queue
    /// cannot carry. `None` = SysEx is dropped (legacy construction).
    plugin: Option<std::sync::Arc<std::sync::Mutex<vst3_host::Plugin>>>,
    lead_us: u64,
    us_to_samples: f64,
}

impl midi_io::EventSink for PluginSink {
    fn lead_us(&self) -> u64 {
        self.lead_us
    }
    fn send_at(&mut self, bytes: &[u8], rem_us: u64) {
        let offset = (rem_us as f64 * self.us_to_samples) as i32;
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
    use std::path::{Path, PathBuf};

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("med-scan-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Make a directory-bundle `.vst3` with one payload file.
    fn fake_bundle(dir: &Path, name: &str, payload: &[u8]) -> PathBuf {
        let b = dir.join(format!("{name}.vst3"));
        let inner = b.join("Contents").join("x86_64-win");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join(format!("{name}.vst3")), payload).unwrap();
        b
    }

    fn canon(p: &Path) -> String {
        std::fs::canonicalize(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .to_string_lossy()
            .into_owned()
    }

    fn entry(stamp: BundleStamp, ok: bool, reason: &str) -> ScanCacheEntry {
        ScanCacheEntry {
            stamp,
            ok,
            name: "cached-name".into(),
            vendor: "cached-vendor".into(),
            reason: reason.into(),
            last_attempt_ms: 7,
            attempts: 1,
        }
    }

    #[test]
    fn stamp_detects_content_changes() {
        let dir = tmpdir("stamp");
        let b = fake_bundle(&dir, "A", &[1, 2, 3]);
        let s1 = bundle_stamp(&b);
        assert_eq!(s1.files, 1);
        assert_eq!(s1.size, 3);
        // a payload change bumps size; a new file bumps the file count
        std::fs::write(b.join("Contents/x86_64-win/A.vst3"), &[1, 2, 3, 4]).unwrap();
        let s2 = bundle_stamp(&b);
        assert_ne!(s1, s2, "size change must invalidate");
        std::fs::write(b.join("Contents/x86_64-win/extra.bin"), &[9]).unwrap();
        let s3 = bundle_stamp(&b);
        assert_eq!(s3.files, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stamp_handles_file_bundles_and_missing_paths() {
        let dir = tmpdir("stampf");
        let f = dir.join("Solo.vst3");
        std::fs::write(&f, &[1, 2, 3, 4, 5]).unwrap();
        let s = bundle_stamp(&f);
        assert_eq!((s.files, s.size), (1, 5));
        assert_eq!(bundle_stamp(&dir.join("gone.vst3")), BundleStamp::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_roundtrip_and_corrupt_tolerance() {
        let dir = tmpdir("cache");
        let file = dir.join("plugin_scan_cache.json");
        let mut c = ScanCache {
            version: ScanCache::VERSION,
            ..Default::default()
        };
        c.entries.insert(
            "k".into(),
            entry(
                BundleStamp {
                    files: 1,
                    size: 2,
                    mtime_ms: 3,
                },
                false,
                "crashed: x",
            ),
        );
        c.save(&file);
        let loaded = ScanCache::load(&file);
        let e = loaded.entries.get("k").unwrap();
        assert!(!e.ok && e.reason == "crashed: x" && e.last_attempt_ms == 7);
        std::fs::write(&file, b"not json").unwrap();
        assert!(ScanCache::load(&file).entries.is_empty());
        assert!(ScanCache::load(&dir.join("missing.json")).entries.is_empty());
        // wrong version is discarded
        std::fs::write(&file, r#"{"version":99,"entries":{}}"#).unwrap();
        assert!(ScanCache::load(&file).entries.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_scan_uses_cache_skips_quarantine_and_refreshes_changed() {
        let dir = tmpdir("plan");
        let ok_b = fake_bundle(&dir, "Ok", &[1]);
        let bad_b = fake_bundle(&dir, "Bad", &[2]);
        let new_b = fake_bundle(&dir, "New", &[3]);

        let mut cache = ScanCache::default();
        cache.entries.insert(
            canon(&ok_b),
            entry(bundle_stamp(&ok_b), true, ""),
        );
        cache.entries.insert(
            canon(&bad_b),
            entry(bundle_stamp(&bad_b), false, "crashed: abort()"),
        );

        let candidates = vec![ok_b.clone(), bad_b.clone(), new_b.clone()];
        let (cached, quarantined, probe) =
            plan_scan(&cache, &candidates, &std::collections::HashSet::new());
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].name, "cached-name", "cache supplies metadata");
        assert_eq!(quarantined.len(), 1);
        assert_eq!(quarantined[0].0, bad_b);
        assert!(
            quarantined[0].1.contains("crashed"),
            "stored reason surfaces"
        );
        assert_eq!(probe.len(), 1);
        assert_eq!(probe[0].0, new_b, "only uncached paths are probed");

        // same quarantined bundle, changed on disk -> re-probed
        std::fs::write(bad_b.join("Contents/x86_64-win/Bad.vst3"), &[2, 2, 2, 2]).unwrap();
        let (_, q2, p2) =
            plan_scan(&cache, &candidates, &std::collections::HashSet::new());
        assert!(q2.is_empty());
        assert_eq!(p2.len(), 2, "updated quarantined bundle re-probes");

        // force set bypasses the cache even for a matching stamp
        let force: std::collections::HashSet<String> =
            [canon(&ok_b)].into_iter().collect();
        let (c3, _, p3) = plan_scan(&cache, &candidates, &force);
        assert!(c3.is_empty());
        assert_eq!(p3.len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
