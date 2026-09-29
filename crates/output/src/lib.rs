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
}

/// VST3 host factory: plugins load into `vst3-host-helper` subprocesses so a
/// crashing or hanging plugin cannot take the app down. `auto_recover`
/// respawns the helper and retries control-plane commands transparently.
fn new_host() -> Result<vst3_host::Vst3Host, PluginError> {
    vst3_host::Vst3Host::builder()
        .scan_default_paths()
        .with_process_isolation(true)
        .auto_recover_plugins(true)
        .auto_recover_max_retries(1)
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
pub fn discover_plugins() -> Vec<PluginInfo> {
    if let Ok(host) = new_host() {
        let report = host.discover_plugins_safe();
        if report.scan_ran() && !report.plugins.is_empty() {
            let mut found: Vec<PluginInfo> = report
                .plugins
                .iter()
                .map(|p| PluginInfo {
                    name: p.info.name.clone(),
                    path: p.info.path.clone(),
                })
                .collect();
            found.sort_by(|a, b| a.name.cmp(&b.name));
            found.dedup_by(|a, b| a.path == b.path);
            return found;
        }
    }
    discover_plugin_paths()
}

/// Fallback scan: enumerate `.vst3` bundles without loading them.
fn discover_plugin_paths() -> Vec<PluginInfo> {
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

pub struct PluginSink {
    sink: vst3_host::MidiSink,
    lead_us: u64,
    us_to_samples: f64,
}

impl midi_io::EventSink for PluginSink {
    fn lead_us(&self) -> u64 {
        self.lead_us
    }
    fn send_at(&mut self, bytes: &[u8], rem_us: u64) {
        if let Some(ev) = channel_event(bytes) {
            let offset = (rem_us as f64 * self.us_to_samples) as i32;
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
