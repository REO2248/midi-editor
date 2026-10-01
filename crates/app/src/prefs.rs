//! Preferences: per-song sidecar (`Prefs`) and app-wide `GlobalPrefs`,
//! both versioned + atomically written via `persist::json`. Loading,
//! sanitizing and applying them into a live `EditorView` happens here.

use super::*;

/// App-wide preferences: recent files + record count-in + recording source.
/// Stored at %APPDATA%/midi-editor/prefs.json (unlike the per-song sidecar).
/// Versioned + atomically persisted via `persist::json` — a torn write can
/// no longer silently reset recent files.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct GlobalPrefs {
    /// schema version — absent in v0 files
    #[serde(default)]
    pub(crate) version: u32,
    #[serde(default)]
    pub(crate) recent: Vec<String>,
    #[serde(default)]
    pub(crate) count_in: bool,
    /// MIDI input port name to record from; empty = first available port
    #[serde(default)]
    pub(crate) midi_in: String,
    /// manual input-latency compensation subtracted from every recorded
    /// timestamp, in ms — for keyboards/interfaces with a known pipeline
    /// delay. Missing in older prefs files.
    #[serde(default)]
    pub(crate) in_latency_ms: u64,
    /// VST3 audio output device display name; None = system default
    #[serde(default)]
    pub(crate) audio_device: Option<String>,
    /// preferred sample rate for hosted-plugin streams; None = 44100
    #[serde(default)]
    pub(crate) sample_rate: Option<f64>,
    /// preferred buffer size in samples; None = 512
    #[serde(default)]
    pub(crate) buffer_size: Option<u32>,
    /// Per-plugin probe bound in seconds (default
    /// `output::DEFAULT_SCAN_TIMEOUT`); quarantined entries respect it too.
    pub(crate) probe_timeout_secs: Option<u64>,
    /// note audition preview on/off (None in older files = on)
    #[serde(default)]
    pub(crate) audition: Option<bool>,
    /// preview velocity for piano-key/draw strikes (None = 100)
    #[serde(default)]
    pub(crate) aud_vel: Option<u8>,
    /// max preview sustain in ms (None = 400)
    #[serde(default)]
    pub(crate) aud_ms: Option<u64>,
    /// high-contrast override: Some(force on/off), None = follow the OS flag
    pub(crate) hc: Option<bool>,
    /// appearance mode: "system" | "dark" | "light" (None = system)
    pub(crate) theme: Option<String>,
    /// keybinding overrides: command id -> "ctrl+shift+z" descriptor
    #[serde(default)]
    pub(crate) keymap: HashMap<String, String>,
    /// reset-on-stop: normal transport stop sends the full CC123/121/120
    /// sweep instead of notes-off only (#161)
    #[serde(default)]
    pub(crate) reset_on_stop: bool,
    /// count-in length in bars: 0 = off, 1/2/4 = bars (#137). When absent,
    /// the legacy `count_in` bool maps to 1/0 bars.
    #[serde(default)]
    pub(crate) count_in_bars: Option<u8>,
    /// return-to-start-on-stop (Cubase preference): a transport stop moves
    /// the play point back to where the pass began. None (older files) =
    /// on, the DAW-conventional default (#156).
    pub(crate) return_to_start_on_stop: Option<bool>,
}

impl Default for GlobalPrefs {
    fn default() -> Self {
        Self {
            version: <Self as persist::json::Versioned>::VERSION,
            recent: Vec::new(),
            count_in: false,
            midi_in: String::new(),
            in_latency_ms: 0,
            audio_device: None,
            sample_rate: None,
            buffer_size: None,
            probe_timeout_secs: None,
            audition: None,
            aud_vel: None,
            aud_ms: None,
            hc: None,
            theme: None,
            keymap: HashMap::new(),
            reset_on_stop: false,
            return_to_start_on_stop: None,
            count_in_bars: None,
        }
    }
}

impl persist::json::Versioned for GlobalPrefs {
    const VERSION: u32 = 1;

    fn migrate(doc: &mut serde_json::Value) {
        // v0 → v1: identical shape, the version stamp is the only change
        doc["version"] = 1.into();
    }

    fn sanitize(&mut self) {
        self.recent.retain(|p| !p.is_empty());
        self.recent.dedup();
        self.recent.truncate(10); // same cap as push_recent
    }
}

impl GlobalPrefs {
    pub(crate) fn path() -> PathBuf {
        let base = std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir());
        base.join("midi-editor")
    }

    pub(crate) fn load() -> Self {
        let l = persist::json::load_json::<GlobalPrefs>(&Self::path().join("prefs.json"));
        for d in &l.diagnostics {
            tracing::warn!("prefs: {d}");
        }
        l.value.unwrap_or_default()
    }

    pub(crate) fn save(&self) {
        let dir = Self::path();
        let _ = std::fs::create_dir_all(&dir);
        let _ = persist::json::save_json(&dir.join("prefs.json"), self);
    }
}

/// App-wide plugin scan cache + quarantine list — shared across songs, so it
/// lives next to prefs.json rather than in a per-file sidecar.
pub(crate) fn scan_cache_path() -> PathBuf {
    GlobalPrefs::path().join("plugin_scan_cache.json")
}

/// Session state that cannot live inside the SMF: per-track output
/// assignments (by stable destination identity, not runtime index), mute/solo,
/// metronome/loop, view transform. Written next to the document as
/// `song.mid.editor.json`.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Prefs {
    /// schema version — absent in v0 files
    #[serde(default)]
    pub(crate) version: u32,
    pub(crate) default_dest: Option<output::Destination>,
    #[serde(default)]
    pub(crate) track_dest: HashMap<usize, output::Destination>,
    #[serde(default)]
    pub(crate) muted: Vec<usize>,
    #[serde(default)]
    pub(crate) soloed: Vec<usize>,
    #[serde(default)]
    pub(crate) metronome: bool,
    /// explicit metronome click destination identity — None = follow the
    /// document default destination (#137)
    pub(crate) met_dest: Option<output::Destination>,
    #[serde(default)]
    pub(crate) loop_enabled: bool,
    /// explicit loop locators in ticks — None = unset (#130); absent in
    /// old sidecars
    #[serde(default)]
    pub(crate) loop_start: Option<u64>,
    #[serde(default)]
    pub(crate) loop_end: Option<u64>,
    /// None in old sidecars = keep the default (overdub)
    pub(crate) rec_mode: Option<String>,
    /// punch bounds in ticks — None = not set
    pub(crate) punch_in: Option<u64>,
    pub(crate) punch_out: Option<u64>,
    /// None in old sidecars = keep the default (off)
    pub(crate) chase_sysex: Option<bool>,
    /// None in old sidecars = keep the default (`SysexPolicy::Serialize`)
    pub(crate) sysex_policy: Option<String>,
    pub(crate) zoom: Option<f32>,
    pub(crate) scroll_x: Option<f32>,
    pub(crate) scroll_y: Option<f32>,
    pub(crate) sel_track: Option<usize>,
    pub(crate) enc: Option<String>,
    /// legacy single-lane sidecar key — read as a fallback when `lanes`
    /// is absent; new saves always write `lanes`
    pub(crate) lane: Option<String>,
    /// legacy poly-aftertouch lane key filter — read as a fallback for
    /// sidecars written before per-lane keys existed; new saves write
    /// the key inside each lane of `lanes`
    pub(crate) poly_key: Option<u8>,
    /// stacked bottom lanes, top to bottom. Absent in old sidecars =
    /// the default single velocity lane
    pub(crate) lanes: Option<Vec<LanePref>>,
    pub(crate) show_events: Option<bool>,
    pub(crate) tool: Option<String>,
    pub(crate) snap: Option<usize>,
    /// vertical zoom (row height px) and fold/drum/scale view toggles
    pub(crate) note_h: Option<f32>,
    pub(crate) fold: Option<bool>,
    pub(crate) drum: Option<bool>,
    pub(crate) scale: Option<i8>,
    pub(crate) scale_minor: Option<bool>,
    /// None in old sidecars = keep the default (page)
    pub(crate) follow: Option<String>,
    /// per-track insert/edit channel (editor state; `FF 20` prefix is the
    /// fallback when a track has no entry). New in v2 sidecars.
    #[serde(default)]
    pub(crate) edit_ch: HashMap<usize, u8>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            version: <Self as persist::json::Versioned>::VERSION,
            default_dest: None,
            track_dest: HashMap::new(),
            muted: Vec::new(),
            soloed: Vec::new(),
            metronome: false,
            met_dest: None,
            loop_enabled: false,
            loop_start: None,
            loop_end: None,
            chase_sysex: None,
            sysex_policy: None,
            rec_mode: None,
            punch_in: None,
            punch_out: None,
            zoom: None,
            scroll_x: None,
            scroll_y: None,
            sel_track: None,
            enc: None,
            lane: None,
            show_events: None,
            tool: None,
            snap: None,
            note_h: None,
            fold: None,
            drum: None,
            scale: None,
            scale_minor: None,
            follow: None,
            poly_key: None,
            lanes: None,
            edit_ch: HashMap::new(),
        }
    }
}

impl persist::json::Versioned for Prefs {
    const VERSION: u32 = 1;

    fn migrate(doc: &mut serde_json::Value) {
        // v0 → v1: identical shape, the version stamp is the only change
        doc["version"] = 1.into();
    }

    /// clamp doc-independent fields at load time; track-index bounds that
    /// depend on the document are still checked where they're applied
    fn sanitize(&mut self) {
        // a NaN/non-positive zoom makes every roll coordinate NaN —
        // nothing paints; drop it to the default instead
        self.zoom = self
            .zoom
            .and_then(|z| (z.is_finite() && z > 0.0).then(|| z.clamp(ZOOM_MIN, ZOOM_MAX)));
        self.scroll_x = self
            .scroll_x
            .and_then(|x| x.is_finite().then(|| x.max(0.0)));
        self.scroll_y = self
            .scroll_y
            .and_then(|y| y.is_finite().then(|| y.max(0.0)));
        self.snap = self.snap.map(|i| i.min(SNAPS.len() - 1));
        self.enc = self
            .enc
            .take()
            .filter(|e| matches!(e.as_str(), "utf8" | "sjis" | "latin1"));
        self.tool = self
            .tool
            .take()
            .filter(|t| matches!(t.as_str(), "select" | "draw" | "erase"));
        self.lane = self.lane.take().filter(|l| {
            l == "vel"
                || l == "pb"
                || l.strip_prefix("cc")
                    .and_then(|n| n.parse::<u8>().ok())
                    .is_some_and(|c| c <= 127)
        });
        // unbounded track indexes are noise, not data — keep only plausible
        // indices (document bounds are still enforced at apply time)
        self.follow = self
            .follow
            .take()
            .filter(|f| matches!(f.as_str(), "off" | "page" | "smooth"));
        self.muted.retain(|t| *t < 1024);
        self.soloed.retain(|t| *t < 1024);
        self.track_dest.retain(|t, _| *t < 1024);
        self.edit_ch.retain(|t, c| *t < 1024 && *c < 16);
    }
}

/// Per-lane layout as stored in the sidecar (`mode` uses the same
/// "vel"/"`cc<n>`"/"pb" codec as the legacy `lane` key).
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub(crate) struct LanePref {
    pub(crate) mode: String,
    pub(crate) h: f32,
    #[serde(default)]
    pub(crate) collapsed: bool,
    /// poly-AT key filter for `pat` lanes (None = all keys)
    pub(crate) poly_key: Option<u8>,
}

pub(crate) fn prefs_path(doc_path: &std::path::Path) -> PathBuf {
    PathBuf::from(format!("{}.editor.json", doc_path.display()))
}

/// Display label for a destination identity (sidecar paths -> stem).
pub(crate) fn dest_label(d: &output::Destination) -> String {
    match d {
        output::Destination::MidiPort { port_name, ord } => {
            if *ord == 0 {
                port_name.clone()
            } else {
                format!("{port_name} #{}", ord + 1)
            }
        }
        output::Destination::Plugin { plugin_path, .. } => {
            let stem = PathBuf::from(plugin_path)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "plugin".into());
            format!("{stem} [VST3]")
        }
    }
}

impl EditorView {
    /// Find or re-create the dest matching a stored identity; returns its index
    /// into `shared.dests`. Unavailable ports/plugins keep their identity —
    /// the assignment stays visible and plays again once the device is back.
    /// Plugin destinations resolve through the class/component ID first: a
    /// bundle that moved keeps its routing (and its recorded path is updated
    /// on the next save).
    pub(crate) fn resolve_dest(&mut self, d: &output::Destination) -> usize {
        let mut sh = lock_shared(&self.shared);
        let catalog: Vec<output::Destination> = sh.dests.iter().map(|(_, d)| d.clone()).collect();
        let (resolved, outcome) = midi_io::resolve_plugin_dest(d, &catalog);
        let stem = |p: &std::path::PathBuf| {
            p.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        match outcome {
            midi_io::Resolved::Moved(p) => {
                self.status = tf("output.plugin_moved", &[("name", stem(&p).as_str())]).into();
            }
            midi_io::Resolved::Ambiguous(p) => {
                self.status = tf("output.plugin_ambiguous", &[("name", stem(&p).as_str())]).into();
            }
            _ => {}
        }
        sh.ensure_dest(&dest_label(&resolved), resolved)
    }

    /// Apply the per-song sidecar. A corrupt or quarantined sidecar can
    /// never break the open — the returned diagnostics are surfaced on the
    /// status line instead of being silently discarded.
    pub(crate) fn apply_prefs(&mut self, doc_path: &std::path::Path) -> Vec<String> {
        let l = persist::json::load_json::<Prefs>(&prefs_path(doc_path));
        let Some(p) = l.value else {
            return l.diagnostics;
        };
        if let Some(d) = &p.default_dest {
            let i = self.resolve_dest(d);
            lock_shared(&self.shared).default_dest = i;
        }
        let overrides: Vec<(usize, usize)> = p
            .track_dest
            .iter()
            .map(|(t, d)| (*t, self.resolve_dest(d)))
            .collect();
        let met_dest = p.met_dest.as_ref().map(|d| self.resolve_dest(d));
        {
            let mut sh = lock_shared(&self.shared);
            for (t, d) in overrides {
                sh.track_dest.insert(t, d);
            }
            sh.muted = p.muted.into_iter().collect();
            sh.soloed = p.soloed.into_iter().collect();
            sh.metronome = p.metronome;
            sh.met_dest = met_dest;
            sh.loop_enabled = p.loop_enabled;
            sh.loop_start = p.loop_start;
            sh.loop_end = p.loop_end;
            if let Some(c) = p.chase_sysex {
                sh.chase_sysex = c;
            }
            if let Some(sp) = p
                .sysex_policy
                .as_deref()
                .and_then(midi_io::SysexPolicy::from_label)
            {
                sh.sysex_policy = sp;
            }
        }
        if let Some(m) = p.rec_mode.as_deref().and_then(RecMode::from_label) {
            self.rec_mode = m;
        }
        self.punch_in = p.punch_in;
        self.punch_out = p.punch_out;
        self.last_take = None;
        // a hand-edited or corrupted sidecar must not blank the roll: a NaN
        // or non-positive zoom makes every coordinate NaN (nothing paints)
        if let Some(z) = p.zoom {
            if z.is_finite() && z > 0.0 {
                self.zoom = z.clamp(ZOOM_MIN, ZOOM_MAX);
            }
        }
        if let Some(x) = p.scroll_x {
            if x.is_finite() {
                self.scroll_x = x.max(0.0);
            }
        }
        if let Some(y) = p.scroll_y {
            if y.is_finite() {
                self.scroll_y = y.max(0.0);
            }
        }
        if let Some(t) = p.sel_track {
            let n = self.doc(|d| d.tracks.len());
            self.sel_track = t.min(n.saturating_sub(1));
        }
        self.edit_ch = p.edit_ch;
        self.enc_override = p.enc.as_deref().map(|e| match e {
            "utf8" => smf_core::TextEncoding::Utf8,
            "sjis" => smf_core::TextEncoding::ShiftJis,
            _ => smf_core::TextEncoding::Latin1,
        });
        // stacked lanes restore verbatim; a hand-edited sidecar drops
        // non-finite heights instead of producing NaN-sized panels
        self.lanes = match p.lanes {
            Some(ls) => {
                let v: Vec<LaneCfg> = ls
                    .iter()
                    .filter(|lp| lp.h.is_finite())
                    .take(LANES_MAX)
                    .map(|lp| LaneCfg {
                        mode: lane_mode_parse(&lp.mode),
                        h: lp.h.clamp(LANE_H_MIN, LANE_H_MAX),
                        collapsed: lp.collapsed,
                        poly_key: lp.poly_key.filter(|k| *k < 128),
                    })
                    .collect();
                if v.is_empty() {
                    vec![LaneCfg::default()]
                } else {
                    v
                }
            }
            None => vec![LaneCfg {
                mode: p
                    .lane
                    .as_deref()
                    .map(lane_mode_parse)
                    .unwrap_or(LaneMode::Velocity),
                poly_key: p.poly_key.filter(|k| *k < 128),
                ..LaneCfg::default()
            }],
        };
        self.lane_focus = self.lanes.len() - 1;
        if let Some(v) = p.show_events {
            self.show_events = v;
        }
        self.tool = match p.tool.as_deref() {
            Some("draw") => Tool::Draw,
            Some("erase") => Tool::Erase,
            _ => Tool::Select,
        };
        if let Some(i) = p.snap {
            self.snap_idx = i;
        }
        // load this song's plugin state table and push saved state into any
        // destinations still warm from the previous document; instances that
        // load after this point restore when their PluginEvent arrives, ahead
        // of the Ready flag playback waits on
        self.plugin_states =
            plugin_state::PluginStateStore::load(&plugin_state::state_path(doc_path));
        self.state_file_dirty = false;
        self.state_restored.clear();
        if let Some(h) = p.note_h {
            if h.is_finite() && h > 0.0 {
                self.note_h = h.clamp(NOTE_H_MIN, NOTE_H_MAX);
            }
        }
        if let Some(f) = p.fold {
            self.fold = f;
        }
        if let Some(d) = p.drum {
            self.drum = d;
        }
        if let Some(s) = p.scale {
            self.scale_sel = s.clamp(-2, 11);
        }
        if let Some(m) = p.scale_minor {
            self.scale_minor = m;
        }
        self.follow = match p.follow.as_deref() {
            Some("off") => Follow::Off,
            Some("smooth") => Follow::Smooth,
            _ => Follow::Page,
        };
        // start warming any VST3 destinations the prefs just restored
        self.refresh_plugins();
        let plugin_dests: Vec<usize> = {
            let sh = lock_shared(&self.shared);
            (0..sh.dests.len())
                .filter(|i| matches!(sh.dests[*i].1, output::Destination::Plugin { .. }))
                .collect()
        };
        for d in plugin_dests {
            self.restore_plugin_state(d);
        }
        l.diagnostics
    }

    /// MRU update + persist to the app-wide prefs file.
    pub(crate) fn push_recent(&mut self, path: &std::path::Path) {
        let s = path.to_string_lossy().into_owned();
        self.recent.retain(|r| r.as_str() != s);
        self.recent.insert(0, s.as_str().into());
        self.recent.truncate(10);
        self.save_global();
    }

    pub(crate) fn save_global(&self) {
        GlobalPrefs {
            version: <GlobalPrefs as persist::json::Versioned>::VERSION,
            recent: self.recent.iter().map(|r| r.to_string()).collect(),
            // legacy bool kept for older builds reading the same file —
            // `count_in_bars` is authoritative (#137)
            count_in: self.count_in_bars > 0,
            midi_in: self.midi_in.to_string(),
            in_latency_ms: self.in_latency_ms,
            count_in_bars: Some(self.count_in_bars),
            audio_device: self.audio_sel.device.clone(),
            sample_rate: self.audio_sel.sample_rate,
            buffer_size: self.audio_sel.buffer_size,
            probe_timeout_secs: Some(self.probe_timeout_secs),
            audition: Some(self.aud_enabled),
            aud_vel: Some(self.aud_vel),
            aud_ms: Some(self.aud_ms),
            hc: self.hc_pref,
            theme: Some(self.theme_mode.name().to_string()),
            keymap: self.keys.overrides.clone(),
            reset_on_stop: self.reset_on_stop,
            return_to_start_on_stop: Some(self.return_to_start_on_stop),
        }
        .save();
    }

    pub(crate) fn persist(&mut self) {
        let sh = lock_shared(&self.shared);
        let Some(path) = sh.path.clone() else {
            return;
        };
        let prefs = Prefs {
            version: <Prefs as persist::json::Versioned>::VERSION,
            default_dest: sh.dests.get(sh.default_dest).map(|(_, d)| d.clone()),
            track_dest: sh
                .track_dest
                .iter()
                .filter_map(|(t, d)| sh.dests.get(*d).map(|(_, dest)| (*t, dest.clone())))
                .collect(),
            muted: sh.muted.iter().copied().collect(),
            soloed: sh.soloed.iter().copied().collect(),
            metronome: sh.metronome,
            met_dest: sh
                .met_dest
                .and_then(|d| sh.dests.get(d).map(|(_, dest)| dest.clone())),
            loop_enabled: sh.loop_enabled,
            loop_start: sh.loop_start,
            loop_end: sh.loop_end,
            chase_sysex: Some(sh.chase_sysex),
            sysex_policy: Some(sh.sysex_policy.label().to_string()),
            rec_mode: Some(self.rec_mode.label().to_string()),
            punch_in: self.punch_in,
            punch_out: self.punch_out,
            zoom: Some(self.zoom),
            scroll_x: Some(self.scroll_x),
            scroll_y: Some(self.scroll_y),
            sel_track: Some(self.sel_track),
            edit_ch: self.edit_ch.clone(),
            note_h: Some(self.note_h),
            fold: Some(self.fold),
            drum: Some(self.drum),
            scale: Some(self.scale_sel),
            scale_minor: Some(self.scale_minor),
            enc: self.enc_override.map(|e| {
                match e {
                    smf_core::TextEncoding::Utf8 => "utf8",
                    smf_core::TextEncoding::ShiftJis => "sjis",
                    smf_core::TextEncoding::Latin1 => "latin1",
                }
                .to_string()
            }),
            lane: self.lanes.first().map(|c| lane_mode_str(c.mode)),
            // legacy mirror of the first poly-AT lane's key so readers
            // that predate `lanes` still see a filter
            poly_key: self
                .lanes
                .iter()
                .find(|c| c.mode == LaneMode::PolyAT)
                .and_then(|c| c.poly_key),
            lanes: Some(
                self.lanes
                    .iter()
                    .map(|c| LanePref {
                        mode: lane_mode_str(c.mode),
                        h: c.h,
                        collapsed: c.collapsed,
                        poly_key: c.poly_key,
                    })
                    .collect(),
            ),
            show_events: Some(self.show_events),
            tool: Some(
                match self.tool {
                    Tool::Select => "select",
                    Tool::Draw => "draw",
                    Tool::Erase => "erase",
                }
                .into(),
            ),
            snap: Some(self.snap_idx),
            follow: Some(
                match self.follow {
                    Follow::Off => "off",
                    Follow::Page => "page",
                    Follow::Smooth => "smooth",
                }
                .into(),
            ),
        };
        // atomic temp+replace with a bounded .bak of the previous valid
        // version — a crash mid-write can no longer reset the sidecar
        let _ = persist::json::save_json(&prefs_path(&path), &prefs);
        drop(sh);
        // persist is the routine durability point: capture any slot whose
        // state was marked dirty and write the companion file now
        self.flush_plugin_states(true);
    }
}
