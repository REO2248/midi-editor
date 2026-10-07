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
    /// VST3 class/component ID (TUID hex) — the durable plugin identity
    /// across bundle moves. `None` when only the filename fallback ran
    /// (no probe) since the path alone is all that scan knows.
    pub uid: Option<String>,
    /// Plugin version string, when the probe reported one.
    pub version: String,
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
    /// Probed class/component uid — the durable identity cached entries
    /// restore (`empty` for pre-uid caches, which simply re-probe).
    #[serde(default)]
    pub uid: String,
    #[serde(default)]
    pub version: String,
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
type ScanBuckets = (
    Vec<PluginInfo>,
    Vec<(std::path::PathBuf, String)>,
    Vec<(std::path::PathBuf, BundleStamp)>,
);

fn plan_scan(
    cache: &ScanCache,
    candidates: &[std::path::PathBuf],
    force: &std::collections::HashSet<String>,
) -> ScanBuckets {
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
                            uid: (!e.uid.is_empty()).then(|| e.uid.clone()),
                            version: e.version.clone(),
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
                        uid: (!info.info.uid.is_empty()).then(|| info.info.uid.clone()),
                        version: info.info.version.clone(),
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
                            uid: info.info.uid.clone(),
                            version: info.info.version.clone(),
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
                            uid: String::new(),
                            version: String::new(),
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
                uid: None,
                version: String::new(),
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
                        uid: None,
                        version: String::new(),
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

/// User-chosen audio configuration for hosted-plugin streams: output device,
/// sample rate, buffer size. `None` fields mean "system default". Persisted
/// globally; a device that has disappeared falls back to the default.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AudioSelection {
    /// output device display name (cpal `DeviceDescription::name`)
    pub device: Option<String>,
    pub sample_rate: Option<f64>,
    pub buffer_size: Option<u32>,
}

/// Output device display names, for the settings panel.
pub fn output_devices() -> Vec<String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    cpal::default_host()
        .output_devices()
        .map(|ds| {
            ds.filter_map(|d| d.description().ok().map(|x| x.name().to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Live per-instance stream state, written by `RoutedBackend` and read by
/// the app. `device` is what the stream actually opened on (after any
/// fallback); `error` records the last stream error (e.g. device unplugged).
#[derive(Debug, Default)]
pub struct StreamState {
    pub device: Option<String>,
    pub error: Option<String>,
}

/// `Arc<Mutex<StreamState>>` shared between one backend and its `PluginSlot`.
pub type SharedStreamState = std::sync::Arc<std::sync::Mutex<StreamState>>;

/// Snapshot of one stream's active configuration, for diagnostics.
#[derive(Debug, Clone)]
pub struct AudioDiag {
    /// device the stream actually opened on (post-fallback resolution)
    pub device: Option<String>,
    pub sample_rate: f64,
    pub block_size: u32,
    /// last stream error, if any
    pub stream_error: Option<String>,
}

/// `AudioBackend` wrapper applying an `AudioSelection`: resolves the chosen
/// output device by name — falling back to the host default when it has
/// disappeared so a reopened stream still comes up — passes stream creation
/// through to cpal, and publishes the resolved device plus stream errors
/// into `SharedStreamState` so device loss is visible and recoverable.
pub struct RoutedBackend {
    inner: vst3_host::backends::cpal_backend::CpalBackend,
    selected: Option<String>,
    state: SharedStreamState,
}

impl RoutedBackend {
    pub fn new(sel: &AudioSelection, state: SharedStreamState) -> Result<Self, PluginError> {
        Ok(Self {
            inner: vst3_host::backends::cpal_backend::CpalBackend::new()
                .map_err(|e| PluginError::Audio(e.to_string()))?,
            selected: sel.device.clone(),
            state,
        })
    }
}

impl vst3_host::AudioBackend for RoutedBackend {
    type Stream = vst3_host::backends::cpal_backend::CpalStream;
    type Device = cpal::Device;
    type Error = vst3_host::Error;

    fn enumerate_output_devices(&self) -> Result<Vec<Self::Device>, Self::Error> {
        self.inner.enumerate_output_devices()
    }

    fn enumerate_input_devices(&self) -> Result<Vec<Self::Device>, Self::Error> {
        self.inner.enumerate_input_devices()
    }

    /// The selected device while it's present, else the host default; the
    /// resolved name is published either way.
    fn default_output_device(&self) -> Option<Self::Device> {
        use cpal::traits::DeviceTrait;
        fn name_of(d: &cpal::Device) -> Option<String> {
            d.description().ok().map(|x| x.name().to_string())
        }
        let chosen = self.selected.as_deref().and_then(|want| {
            self.inner
                .enumerate_output_devices()
                .ok()?
                .into_iter()
                .find(|d| name_of(d).as_deref() == Some(want))
        });
        let device = chosen.or_else(|| self.inner.default_output_device());
        if let Ok(mut st) = self.state.lock() {
            st.device = device.as_ref().and_then(name_of);
        }
        device
    }

    fn default_input_device(&self) -> Option<Self::Device> {
        self.inner.default_input_device()
    }

    fn create_output_stream(
        &self,
        device: &Self::Device,
        config: vst3_host::AudioConfig,
        data_callback: Box<dyn FnMut(&mut [f32]) + Send>,
        mut error_callback: Box<dyn FnMut(Self::Error) + Send>,
    ) -> Result<Self::Stream, Self::Error> {
        let state = self.state.clone();
        self.inner.create_output_stream(
            device,
            config,
            data_callback,
            Box::new(move |e| {
                if let Ok(mut st) = state.lock() {
                    st.error = Some(e.to_string());
                }
                error_callback(e);
            }),
        )
    }

    fn create_input_stream(
        &self,
        device: &Self::Device,
        config: vst3_host::AudioConfig,
        data_callback: Box<dyn FnMut(&[f32]) + Send>,
        error_callback: Box<dyn FnMut(Self::Error) + Send>,
    ) -> Result<Self::Stream, Self::Error> {
        self.inner
            .create_input_stream(device, config, data_callback, error_callback)
    }
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
    /// resolved device + stream errors, shared with the backend
    stream_state: SharedStreamState,
    /// (sample rate, block size) the stream was opened with
    audio_config: (f64, u32),
}

impl PluginOutput {
    /// Load `path` (a .vst3 bundle), start its audio stream on the default
    /// output device via cpal, and return a playable destination.
    pub fn open(path: &std::path::Path) -> Result<Self, PluginError> {
        Self::open_with(path, &AudioSelection::default())
    }

    /// `open` with an explicit audio configuration: `sel.device` picks the
    /// output device by name (the host default when it's missing or
    /// unplugged), `sel.sample_rate`/`sel.buffer_size` become the stream and
    /// plugin processing setup. Block size is clamped to what the device
    /// advertises by the cpal backend.
    pub fn open_with(path: &std::path::Path, sel: &AudioSelection) -> Result<Self, PluginError> {
        let mut host = new_host()?;
        let plugin = host
            .load_plugin(path)
            .map_err(|e| PluginError::Load(e.to_string()))?;
        let mut config = vst3_host::AudioConfig::default();
        if let Some(sr) = sel.sample_rate {
            config.sample_rate = sr;
        }
        if let Some(bs) = sel.buffer_size {
            config.block_size = bs as usize;
        }
        let stream_state = SharedStreamState::default();
        let backend = RoutedBackend::new(sel, stream_state.clone())?;
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
            stream_state,
            audio_config: (config.sample_rate, config.block_size as u32),
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

    /// Live stream state shared with the backend (resolved device, last
    /// stream error).
    pub fn stream_state(&self) -> SharedStreamState {
        self.stream_state.clone()
    }

    /// (sample rate, block size) the stream was opened with.
    pub fn audio_config(&self) -> (f64, u32) {
        self.audio_config
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
    /// which instance of the bundle this slot hosts — 1 for the base
    /// instance; lets a warm slot satisfy only its own destination (#222)
    pub instance: u64,
    /// live latency tracker shared with `sink` — the compensation math reads
    /// it per event, so `refresh_latency` retimes playback in place
    pub latency: LatencyComp,
    /// (sample rate, block size) the stream was opened with
    pub audio: (f64, u32),
    /// live stream state shared with the backend — resolved device name and
    /// last stream error, for diagnostics and device-loss recovery
    pub stream: SharedStreamState,
}

impl PluginSlot {
    /// Snapshot of the stream's active configuration for diagnostics.
    pub fn audio_diag(&self) -> AudioDiag {
        let st = self.stream.lock().ok();
        AudioDiag {
            device: st.as_ref().and_then(|s| s.device.clone()),
            sample_rate: self.audio.0,
            block_size: self.audio.1,
            stream_error: st.and_then(|s| s.error.clone()),
        }
    }

    /// The last stream error (e.g. device unplugged), cleared on read.
    pub fn take_stream_error(&self) -> Option<String> {
        self.stream.lock().ok().and_then(|mut s| s.error.take())
    }
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
    /// load+start audio for dest index `usize` with the given audio
    /// configuration; result arrives on the event channel. A same-dest
    /// reopen preserves plugin state — it is a reconfigure (device/rate/
    /// buffer change) or a device-loss recovery, not a plugin swap.
    /// The trailing `u64` is the 1-based instance number (#222) — a
    /// dest index hosts at most one instance, so this mainly decorates
    /// the slot/event for warm-slot matching on the app side.
    Open(usize, std::path::PathBuf, AudioSelection, u64),
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
    /// 1-based instance number of the opened plugin (#222)
    pub instance: u64,
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
                PluginReq::Open(d, path, sel, inst) => {
                    // reopening a live destination keeps the plugin's state
                    // (program, params) — the new instance continues where
                    // the old stream left off
                    let prior_state = owned.get(&d).and_then(|p| {
                        p.plugin_handle()
                            .lock()
                            .map(|g| g.save_state().ok())
                            .unwrap_or_else(|poisoned| poisoned.into_inner().save_state().ok())
                    });
                    match PluginOutput::open_with(&path, &sel) {
                        Ok(p) => {
                            if let Some(state) = prior_state {
                                let _ = p.plugin_handle().lock().map(|mut g| g.load_state(&state));
                            }
                            let slot = PluginSlot {
                                sink: p.event_sink(),
                                plugin: p.plugin_handle(),
                                path,
                                instance: inst,
                                latency: p.latency(),
                                audio: p.audio_config(),
                                stream: p.stream_state(),
                            };
                            owned.insert(d, p);
                            let _ = evt_tx.send(PluginEvent {
                                dest: d,
                                path: slot.path.clone(),
                                instance: inst,
                                result: Ok(slot),
                            });
                        }
                        Err(e) => {
                            let _ = evt_tx.send(PluginEvent {
                                dest: d,
                                path,
                                instance: inst,
                                result: Err(e),
                            });
                        }
                    }
                }
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
    fn level(&self) -> f32 {
        // #203 metering: the plugin's per-block output peak (linear, 1.0 =
        // 0 dBFS). try_lock guest (#190): the audio callback owns this mutex
        // for the duration of each block, so a contended poll reports 0.0
        // ("no reading this frame") instead of stalling the UI thread — the
        // poller's decay covers the gap. `get_output_levels` itself only
        // takes the small level mutex inside `Plugin`.
        let Some(plugin) = self.plugin.as_ref() else {
            return 0.0;
        };
        let p = match plugin.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return 0.0,
        };
        p.get_output_levels()
            .channels
            .iter()
            .map(|c| c.peak)
            .fold(0.0, f32::max)
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

/// One scheduled transport update for a hosted plugin — encoded into a
/// playback event payload by `encode_transport` and applied by
/// `TransportSink`.
#[derive(Debug, Clone, PartialEq)]
pub enum TransportCmd {
    /// beats per minute → `Plugin::set_tempo`
    Tempo(f64),
    /// numerator, denominator → `Plugin::set_time_signature`
    TimeSig(i32, i32),
}

/// Wire tag for transport payloads in an event list. `0xF7` (EOX) can never
/// appear as a channel-message status byte and SysEx chases always start
/// `0xF0`, so transport payloads share the schedule with note/SysEx events
/// without collisions.
pub const TRANSPORT_TAG: u8 = 0xF7;

/// How far ahead of its deadline a transport update is handed to the
/// plugin's control queue. The host applies queued transport commands at
/// the start of the next audio block, so the lead must comfortably cover
/// one block — 25 ms ≈ 2 blocks at 44.1 kHz / 512 — for a change to land at
/// a block boundary within ~one block of its document-time deadline.
const TRANSPORT_LEAD_US: u64 = 25_000;

/// Encode a transport update into an event payload.
pub fn encode_transport(cmd: &TransportCmd) -> Vec<u8> {
    match *cmd {
        TransportCmd::Tempo(bpm) => {
            let mut v = Vec::with_capacity(10);
            v.extend_from_slice(&[TRANSPORT_TAG, 1]);
            v.extend_from_slice(&bpm.to_be_bytes());
            v
        }
        TransportCmd::TimeSig(n, d) => {
            let mut v = Vec::with_capacity(10);
            v.extend_from_slice(&[TRANSPORT_TAG, 2]);
            v.extend_from_slice(&n.to_be_bytes());
            v.extend_from_slice(&d.to_be_bytes());
            v
        }
    }
}

/// Decode a transport payload. Untagged or malformed bytes return `None` —
/// a `TransportSink` sharing the schedule must never mistake channel or
/// SysEx traffic for a transport update.
pub fn decode_transport(bytes: &[u8]) -> Option<TransportCmd> {
    match bytes {
        [TRANSPORT_TAG, 1, rest @ ..] if rest.len() == 8 => Some(TransportCmd::Tempo(
            f64::from_be_bytes(rest.try_into().ok()?),
        )),
        [TRANSPORT_TAG, 2, rest @ ..] if rest.len() == 8 => {
            let (n, d) = rest.split_at(4);
            Some(TransportCmd::TimeSig(
                i32::from_be_bytes(n.try_into().ok()?),
                i32::from_be_bytes(d.try_into().ok()?),
            ))
        }
        _ => None,
    }
}

/// The transport state in effect at `pos_us`, retimed to `pos_us`. A seek
/// or loop wrap skips past boundary points — this is the chase that
/// re-asserts the position's tempo and meter so the plugin hears the state
/// it would have reached playing through, not whatever was last scheduled.
/// `points` must be sorted by µs (the playback schedule already is).
pub fn chase_transport(points: &[(u64, TransportCmd)], pos_us: u64) -> Vec<(u64, TransportCmd)> {
    let mut tempo = None;
    let mut sig = None;
    for (us, c) in points {
        if *us > pos_us {
            break;
        }
        match c {
            TransportCmd::Tempo(_) => tempo = Some(c.clone()),
            TransportCmd::TimeSig(..) => sig = Some(c.clone()),
        }
    }
    [tempo, sig]
        .into_iter()
        .flatten()
        .map(|c| (pos_us, c))
        .collect()
}

/// What a decoded transport update is applied to — the plugin's control
/// queue in production, a recorder in tests (the synthetic processor that
/// captures the context it would receive).
pub trait TransportTarget: Send {
    fn apply_transport(&mut self, cmd: &TransportCmd);
}

impl TransportTarget for std::sync::Arc<std::sync::Mutex<vst3_host::Plugin>> {
    fn apply_transport(&mut self, cmd: &TransportCmd) {
        let mut p = self.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        match *cmd {
            TransportCmd::Tempo(bpm) => {
                let _ = p.set_tempo(bpm);
            }
            TransportCmd::TimeSig(n, d) => {
                let _ = p.set_time_signature(n, d);
            }
        }
    }
}

/// `EventSink` that turns scheduled transport payloads into plugin
/// `ProcessContext` updates (`set_tempo`/`set_time_signature`) through the
/// lock-free control queue — the host applies them at the start of the next
/// audio block, on the same deadline clock as the note stream, so a
/// tempo/meter change lands at a block boundary rather than at a ~16 ms
/// UI-frame poll.
///
/// It is not a MIDI destination: `panic`/`notes_off` are no-ops, and
/// untagged payloads are ignored by `decode_transport`.
#[derive(Clone)]
pub struct TransportSink<T: TransportTarget> {
    target: T,
}

impl<T: TransportTarget> TransportSink<T> {
    pub fn new(target: T) -> Self {
        Self { target }
    }
}

impl<T: TransportTarget> midi_io::EventSink for TransportSink<T> {
    fn lead_us(&self) -> u64 {
        TRANSPORT_LEAD_US
    }
    fn send_at(&mut self, bytes: &[u8], _rem_us: u64) {
        if let Some(cmd) = decode_transport(bytes) {
            self.target.apply_transport(&cmd);
        }
    }
    fn panic(&mut self) {}
    fn notes_off(&mut self) {}
}

/// One `IComponentHandler::restartComponent` notification, named — the
/// audit view of `vst3_host::RestartFlags`' predicate set. Ordering is the
/// VST3 bit order so a drain walks them deterministically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RestartNote {
    ParamValues,
    ReloadComponent,
    ParamTitles,
    Latency,
    Io,
    MidiCcAssignment,
    NoteExpression,
    IoTitles,
    Prefetchable,
    Routing,
    Keyswitch,
    ParamIdMapping,
}

impl RestartNote {
    /// Every notification the audit knows, in VST3 bit order.
    pub const ALL: [RestartNote; 12] = [
        RestartNote::ReloadComponent,
        RestartNote::Io,
        RestartNote::ParamValues,
        RestartNote::ParamTitles,
        RestartNote::Latency,
        RestartNote::MidiCcAssignment,
        RestartNote::NoteExpression,
        RestartNote::IoTitles,
        RestartNote::Prefetchable,
        RestartNote::Routing,
        RestartNote::Keyswitch,
        RestartNote::ParamIdMapping,
    ];

    /// The `k…Changed`/`kReloadComponent` name, for diagnostics.
    pub fn name(self) -> &'static str {
        match self {
            RestartNote::ParamValues => "kParamValuesChanged",
            RestartNote::ReloadComponent => "kReloadComponent",
            RestartNote::ParamTitles => "kParamTitlesChanged",
            RestartNote::Latency => "kLatencyChanged",
            RestartNote::Io => "kIoChanged",
            RestartNote::MidiCcAssignment => "kMidiCCAssignmentChanged",
            RestartNote::NoteExpression => "kNoteExpressionChanged",
            RestartNote::IoTitles => "kIoTitlesChanged",
            RestartNote::Prefetchable => "kPrefetchableSupportChanged",
            RestartNote::Routing => "kRoutingInfoChanged",
            RestartNote::Keyswitch => "kKeyswitchChanged",
            RestartNote::ParamIdMapping => "kParamIDMappingChanged",
        }
    }
}

/// The notifications raised since the last drain, in VST3 bit order —
/// the only `RestartFlags`-aware shim; the policy below is pure.
pub fn restart_notes(f: vst3_host::RestartFlags) -> Vec<RestartNote> {
    let mut out = Vec::with_capacity(4);
    if f.reload_component() {
        out.push(RestartNote::ReloadComponent);
    }
    if f.io_changed() {
        out.push(RestartNote::Io);
    }
    if f.param_values_changed() {
        out.push(RestartNote::ParamValues);
    }
    if f.param_titles_changed() {
        out.push(RestartNote::ParamTitles);
    }
    if f.latency_changed() {
        out.push(RestartNote::Latency);
    }
    if f.midi_cc_assignment_changed() {
        out.push(RestartNote::MidiCcAssignment);
    }
    if f.note_expression_changed() {
        out.push(RestartNote::NoteExpression);
    }
    if f.io_titles_changed() {
        out.push(RestartNote::IoTitles);
    }
    if f.prefetchable_support_changed() {
        out.push(RestartNote::Prefetchable);
    }
    if f.routing_info_changed() {
        out.push(RestartNote::Routing);
    }
    if f.keyswitch_changed() {
        out.push(RestartNote::Keyswitch);
    }
    if f.param_id_mapping_changed() {
        out.push(RestartNote::ParamIdMapping);
    }
    out
}

/// What one notification asks of this host. The audit is deliberately
/// small: we keep no parameter/bus/title caches of our own, so most flags
/// need only a record; the few that touch playback get a real reaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartAction {
    /// re-read `getLatencySamples` into the shared tracker
    RefreshLatency,
    /// bus layout changed — `service_host_requests` already ran the
    /// deactivate/reactivate lifecycle; re-query the arrangement for
    /// diagnostics (nothing else in the app caches it)
    RequeryIo,
    /// the plugin demands a full reload — destroy and reopen the instance
    Reload,
    /// nothing this host caches needs rebuilding; record it once
    LogOnly,
}

/// Notification → reaction. Pure, so the policy is fully unit-testable.
pub fn restart_action(n: RestartNote) -> RestartAction {
    match n {
        RestartNote::Latency => RestartAction::RefreshLatency,
        RestartNote::Io => RestartAction::RequeryIo,
        RestartNote::ReloadComponent => RestartAction::Reload,
        // parameter values/titles, note-expression, routing, keyswitch and
        // id-mapping metadata: the app keeps no caches of these (the GUI
        // editor reads them live), so they only need a log record
        _ => RestartAction::LogOnly,
    }
}

/// Per-plugin dedupe for restart notes — a notification the host doesn't
/// act on is logged the first time it's seen per instance, not per frame.
#[derive(Debug, Default)]
pub struct RestartLog(std::collections::HashSet<RestartNote>);

impl RestartLog {
    /// True the first time `n` is reported for this instance.
    pub fn first_seen(&mut self, n: RestartNote) -> bool {
        self.0.insert(n)
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
    fn every_restart_note_maps_to_an_action() {
        use RestartAction::*;
        for n in RestartNote::ALL {
            let a = restart_action(n);
            assert!(matches!(a, RefreshLatency | RequeryIo | Reload | LogOnly));
        }
        assert_eq!(restart_action(RestartNote::Latency), RefreshLatency);
        assert_eq!(restart_action(RestartNote::Io), RequeryIo);
        assert_eq!(restart_action(RestartNote::ReloadComponent), Reload);
        assert_eq!(restart_action(RestartNote::ParamTitles), LogOnly);
        assert_eq!(restart_action(RestartNote::Routing), LogOnly);
        assert_eq!(restart_action(RestartNote::NoteExpression), LogOnly);
    }

    #[test]
    fn restart_note_names_cover_vst3_flags() {
        // names are the VST3 flag spellings so logs can be grepped against
        // the Steinberg restartComponent docs
        for n in RestartNote::ALL {
            assert!(n.name().starts_with('k'), "{}", n.name());
        }
        assert_eq!(RestartNote::ALL.len(), 12);
    }

    #[test]
    fn restart_log_logs_once_per_note_per_instance() {
        let mut log = RestartLog::default();
        assert!(log.first_seen(RestartNote::NoteExpression));
        assert!(!log.first_seen(RestartNote::NoteExpression));
        assert!(log.first_seen(RestartNote::Keyswitch));
        // a fresh instance (e.g. reopened plugin) logs fresh
        let mut next = RestartLog::default();
        assert!(next.first_seen(RestartNote::NoteExpression));
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

    /// `StreamState::error` is the device-loss signal the app polls for; the
    /// read clears it so one stream error reopens the slot exactly once.
    #[test]
    fn stream_state_error_reads_once() {
        let shared = SharedStreamState::default();
        shared.lock().unwrap().error = Some("device lost".into());
        // mirror PluginSlot::take_stream_error on a bare state — the slot
        // itself needs a live plugin, so the drain logic is tested here
        let take = |s: &SharedStreamState| s.lock().ok().and_then(|mut g| g.error.take());
        assert_eq!(take(&shared).as_deref(), Some("device lost"));
        assert!(take(&shared).is_none());
    }

    #[test]
    fn transport_codec_roundtrips_and_rejects_garbage() {
        for cmd in [
            TransportCmd::Tempo(121.5),
            TransportCmd::Tempo(60.0),
            TransportCmd::TimeSig(7, 8),
        ] {
            assert_eq!(decode_transport(&encode_transport(&cmd)), Some(cmd));
        }
        // channel traffic, a stray F7 payload, short and unknown kinds
        assert_eq!(decode_transport(&[0x90, 60, 100]), None);
        assert_eq!(decode_transport(&[0xF7, 1, 0, 0]), None);
        assert_eq!(
            decode_transport(&[TRANSPORT_TAG, 9, 0, 0, 0, 0, 0, 0, 0, 0]),
            None
        );
    }

    /// The recording "processor" behind `TransportSink`: every update the
    /// playback schedule delivers is decoded and pushed to the target — the
    /// context a real plugin would see applied at the next block.
    struct RecTarget(std::sync::Arc<std::sync::Mutex<Vec<TransportCmd>>>);

    impl TransportTarget for RecTarget {
        fn apply_transport(&mut self, cmd: &TransportCmd) {
            self.0.lock().unwrap().push(cmd.clone());
        }
    }

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
            uid: "cached-uid".into(),
            version: "1.0".into(),
        }
    }

    #[test]
    fn transport_sink_forwards_only_decoded_updates() {
        use midi_io::EventSink;
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut sink = TransportSink::new(RecTarget(log.clone()));
        sink.send_at(&encode_transport(&TransportCmd::Tempo(140.0)), 5_000);
        sink.send_at(&[0x90, 60, 100], 0); // a note: not transport, ignored
        sink.send_at(&encode_transport(&TransportCmd::TimeSig(3, 4)), 0);
        sink.notes_off(); // loop-wrap cleanup must not reach the plugin
        assert_eq!(
            *log.lock().unwrap(),
            vec![TransportCmd::Tempo(140.0), TransportCmd::TimeSig(3, 4)]
        );
    }

    #[test]
    fn transport_lead_covers_two_blocks_at_44k() {
        // the host applies queued transport commands at the next block
        // start; arriving ~2 blocks early keeps the effective change within
        // ~one block of its document-time deadline
        let block_us = (512.0 / 44_100.0 * 1e6) as u64;
        assert!(TRANSPORT_LEAD_US >= block_us * 2);
    }

    #[test]
    fn chase_transport_restores_state_at_position() {
        let pts = vec![
            (0u64, TransportCmd::Tempo(120.0)),
            (1_000, TransportCmd::TimeSig(3, 4)),
            (2_000, TransportCmd::Tempo(90.0)),
            (3_000, TransportCmd::TimeSig(6, 8)),
        ];
        // seek mid-map: last tempo AND last sig in effect at the position
        assert_eq!(
            chase_transport(&pts, 2_500),
            vec![
                (2_500, TransportCmd::Tempo(90.0)),
                (2_500, TransportCmd::TimeSig(3, 4)),
            ]
        );
        // nothing before the position: no updates to chase
        assert!(chase_transport(&pts[..0], 0).is_empty());
        // landing exactly on a boundary re-asserts the boundary's own state
        assert_eq!(
            chase_transport(&pts, 2_000),
            vec![
                (2_000, TransportCmd::Tempo(90.0)),
                (2_000, TransportCmd::TimeSig(3, 4)),
            ]
        );
    }

    #[test]
    fn offset_truncation_error_stays_below_one_sample() {
        // offset() truncates rem*s toward zero — the documented rounding
        // error of sample scheduling is strictly under one sample
        let c = LatencyComp::new(0.0441); // 44.1 kHz
        for rem in (0..100_000).step_by(997) {
            let ideal = rem as f64 * 0.0441;
            let got = c.offset(rem) as f64;
            assert!(
                (0.0..1.0).contains(&(ideal - got)),
                "rem={rem} ideal={ideal} got={got}"
            );
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
        std::fs::write(b.join("Contents/x86_64-win/A.vst3"), [1, 2, 3, 4]).unwrap();
        let s2 = bundle_stamp(&b);
        assert_ne!(s1, s2, "size change must invalidate");
        std::fs::write(b.join("Contents/x86_64-win/extra.bin"), [9]).unwrap();
        let s3 = bundle_stamp(&b);
        assert_eq!(s3.files, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stamp_handles_file_bundles_and_missing_paths() {
        let dir = tmpdir("stampf");
        let f = dir.join("Solo.vst3");
        std::fs::write(&f, [1, 2, 3, 4, 5]).unwrap();
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
        assert!(ScanCache::load(&dir.join("missing.json"))
            .entries
            .is_empty());
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
        cache
            .entries
            .insert(canon(&ok_b), entry(bundle_stamp(&ok_b), true, ""));
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
        std::fs::write(bad_b.join("Contents/x86_64-win/Bad.vst3"), [2, 2, 2, 2]).unwrap();
        let (_, q2, p2) = plan_scan(&cache, &candidates, &std::collections::HashSet::new());
        assert!(q2.is_empty());
        assert_eq!(p2.len(), 2, "updated quarantined bundle re-probes");

        // force set bypasses the cache even for a matching stamp
        let force: std::collections::HashSet<String> = [canon(&ok_b)].into_iter().collect();
        let (c3, _, p3) = plan_scan(&cache, &candidates, &force);
        assert!(c3.is_empty());
        assert_eq!(p3.len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    use vst3_host::MidiEvent;

    /// SMF channel bytes → host events, verbatim: a zero-velocity Note On is
    /// a Note Off (running-status convention), pitch bend packs 14 bits, and
    /// anything the plugin queue can't carry (SysEx, realtime, truncated)
    /// maps to None instead of a garbled event.
    #[test]
    fn channel_event_maps_every_message_kind() {
        match channel_event(&[0x90, 60, 100]) {
            Some(MidiEvent::NoteOn {
                channel,
                note,
                velocity,
            }) => {
                assert_eq!(channel.as_index(), 0);
                assert_eq!((note, velocity), (60, 100));
            }
            other => panic!("note on: {other:?}"),
        }
        // vel-0 note on must arrive as note off
        assert!(matches!(
            channel_event(&[0x9F, 60, 0]),
            Some(MidiEvent::NoteOff { note: 60, .. })
        ));
        assert!(matches!(
            channel_event(&[0x80, 64, 40]),
            Some(MidiEvent::NoteOff {
                note: 64,
                velocity: 40,
                ..
            })
        ));
        assert!(matches!(
            channel_event(&[0xB1, 7, 100]),
            Some(MidiEvent::ControlChange {
                controller: 7,
                value: 100,
                ..
            })
        ));
        assert!(matches!(
            channel_event(&[0xC2, 12]),
            Some(MidiEvent::ProgramChange { program: 12, .. })
        ));
        assert!(matches!(
            channel_event(&[0xA3, 60, 90]),
            Some(MidiEvent::PolyAftertouch {
                note: 60,
                pressure: 90,
                ..
            })
        ));
        assert!(matches!(
            channel_event(&[0xD4, 55]),
            Some(MidiEvent::ChannelAftertouch { pressure: 55, .. })
        ));
        match channel_event(&[0xE5, 0x00, 0x40]) {
            Some(MidiEvent::PitchBend { channel, value }) => {
                assert_eq!(channel.as_index(), 5);
                assert_eq!(value, 0x2000); // center
            }
            other => panic!("pitch bend: {other:?}"),
        }
        // non-channel messages never enter the plugin event queue
        assert!(channel_event(&[0xF0, 0x7E]).is_none());
        assert!(channel_event(&[0xF8]).is_none());
        assert!(channel_event(&[0x90]).is_none());
        assert!(channel_event(&[]).is_none());
    }

    /// The host worker drains requests in order and unloads with it: Open
    /// replies arrive FIFO (errors included), Drop/Clear are absorbed, and
    /// Shutdown closes the event channel — all without needing a real plugin.
    #[test]
    fn host_worker_request_ordering_and_exit() {
        let (tx, rx, _worker) = spawn_plugin_host();
        let bogus = std::path::PathBuf::from(r"C:\no\such\bundle.vst3");
        tx.send(PluginReq::Open(
            7,
            bogus.clone(),
            AudioSelection::default(),
            1,
        ))
        .unwrap();
        tx.send(PluginReq::Drop(3)).unwrap();
        tx.send(PluginReq::Open(
            2,
            bogus.clone(),
            AudioSelection::default(),
            2,
        ))
        .unwrap();
        tx.send(PluginReq::Clear).unwrap();
        tx.send(PluginReq::Shutdown).unwrap();

        let first = rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("first reply");
        assert_eq!(first.dest, 7);
        assert_eq!(first.path, bogus);
        assert_eq!(first.instance, 1);
        assert!(first.result.is_err(), "bogus bundle must fail to open");
        let second = rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("second reply");
        assert_eq!(second.dest, 2);
        assert_eq!(second.instance, 2, "events carry the request's instance");
        assert!(second.result.is_err());
        // after Shutdown the worker exits and the event channel closes
        assert!(rx.recv_timeout(std::time::Duration::from_secs(30)).is_err());
    }
}
