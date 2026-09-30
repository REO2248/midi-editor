//! Phase 1: real SMF document in a modern-editor shell.
//! Open/save .mid, piano roll + event list views, playback to a MIDI port,
//! basic editing (draw / drag / delete) all going through
//! `Document::apply(Transaction)` so undo is shared with MCP edits.

mod geometry;
mod i18n;
mod icons;
mod render;
use geometry::{
    clamp_move_delta, clamp_span, content_view, reanchor, roll_hit, ZOOM_MAX, ZOOM_MIN,
};
use i18n::{t, tf};

use commands::UndoStack;
use document::{Document, Event as DocEvent, EventId, Note, Op};
use gpui_kit::component::input::InputState;
use gpui_kit::component::Root;
use gpui_kit::*;
use mcp_server::{Shared, SharedDoc};
use midi_io::{EventSink, Playback, PortSink};
use smf_core::Division;
use smf_core::EventKind;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;

const NOTE_H: f32 = 13.0;
const TRACK_COLORS: [u32; 8] = [
    0x4f8cff, 0xff8c4f, 0x4fd08c, 0xd04fff, 0xffd24f, 0x4fd0ff, 0xff4f7a, 0x9dff4f,
];
const SEL_COLOR: u32 = 0xffffff;
const DANGLING_COLOR: u32 = 0xff4f4f;

/// What a left-drag on the piano roll is doing.
#[derive(Clone, Copy, PartialEq)]
enum DragMode {
    /// move note(s) in pitch+time
    Move,
    /// stretch the note's right edge (duration)
    Resize,
    /// rubber-band select on empty canvas
    Marquee,
    /// vertical drag in the velocity lane; `dkey` carries the new velocity
    Velocity,
    /// lane drag editing a CC/PB event (`on_id` = event id; `dkey` = value)
    LaneEvent,
    /// alt-drag: copy the selection instead of moving it
    Duplicate,
    /// erase tool: every note touched joins `erase_ids`, deleted on commit
    Erase,
}

/// Menubar dropdown that is currently open.
#[derive(Clone, Copy, PartialEq)]
enum TopMenu {
    File,
    Edit,
    View,
    Track,
    Output,
    Transport,
    Help,
}

/// Second-level (cascading) menu that is open inside a dropdown.
#[derive(Clone, Copy, PartialEq)]
enum Sub {
    Chan,
    Dest,
    DefDest,
    InPort,
    Lane,
    Enc,
    Tool,
    Snap,
    Recent,
    Quant,
    LenSet,
    VelSet,
    Oct,
}

#[derive(Clone, Copy, PartialEq)]
enum DestPick {
    Track,
    Default,
}

/// What the bottom lane edits for the selected track.
#[derive(Clone, Copy, PartialEq)]
enum LaneMode {
    Velocity,
    /// Control Change lane, controller number in the field
    CC(u8),
    PitchBend,
}

impl LaneMode {
    fn cycle(self) -> Self {
        match self {
            LaneMode::Velocity => LaneMode::CC(1),
            LaneMode::CC(1) => LaneMode::CC(7),
            LaneMode::CC(7) => LaneMode::CC(10),
            LaneMode::CC(10) => LaneMode::CC(11),
            LaneMode::CC(11) => LaneMode::CC(64),
            LaneMode::CC(_) => LaneMode::PitchBend,
            LaneMode::PitchBend => LaneMode::Velocity,
        }
    }
    fn label(self) -> String {
        match self {
            LaneMode::Velocity => "Vel".to_string(),
            LaneMode::CC(n) => format!("CC{n}"),
            LaneMode::PitchBend => "PB".to_string(),
        }
    }
}

/// One clipboard note — tick offset from the copy anchor.
#[derive(Clone)]
struct ClipNote {
    dtick: i64,
    key: u8,
    len: u64,
    vel: u8,
    ch: u8,
    track: usize,
}

/// Piano-roll edit tool — the toolbar's radio group.
#[derive(Clone, Copy, PartialEq)]
enum Tool {
    /// click/drag selects notes; note-edge drags resize
    Select,
    /// click or drag on empty canvas draws a note
    Draw,
    /// click or sweep over notes deletes them (one undo step per stroke)
    Erase,
}

/// Snap grid divisors of a whole note; 0 = snap off.
const SNAPS: [(u32, bool, &str); 10] = [
    (0, false, "off"),
    (1, false, "1"),
    (2, false, "1/2"),
    (4, false, "1/4"),
    (4, true, "1/4T"),
    (8, false, "1/8"),
    (8, true, "1/8T"),
    (16, false, "1/16"),
    (16, true, "1/16T"),
    (32, false, "1/32"),
];

struct Drag {
    mode: DragMode,
    on_id: EventId,
    off_id: Option<EventId>,
    track: usize,
    orig_start: u64,
    orig_end: Option<u64>,
    orig_key: u8,
    /// Move: delta applied to start+key (and to every selected note).
    /// Resize: delta applied to the end tick.
    dtick: i64,
    dkey: i32,
    /// Marquee corners in (tick, key) space
    a_tick: i64,
    a_key: i32,
    b_tick: i64,
    b_key: i32,
}

/// Document-derived data the chrome (menu bar, marker strip, minimap,
/// transport readouts) shows. Everything here is a pure function of the
/// document revision (+ the text-encoding hint), so it is computed once per
/// edit instead of once per frame.
#[derive(Default)]
struct DocUi {
    markers: Vec<(u64, String)>,
    n_diags: usize,
    track_names: Vec<String>,
    track_chs: Vec<u8>,
    sig: String,
    tempo0: f64,
    /// last tick with a note — the scrollable extent of the timeline
    song_end: u64,
}

impl DocUi {
    /// `notes` is the already-derived note view for this revision (passed in
    /// so the pairing pass runs once per revision, not once per consumer).
    fn build(doc: &Document, notes: &[Note], enc_override: Option<smf_core::TextEncoding>) -> Self {
        let hint = enc_override.or_else(|| doc.text_encoding_hint());
        // meta 0x06/0x05 markers, from any track, at their tick
        let mut markers = Vec::new();
        for t in &doc.tracks {
            for e in &t.events {
                if let EventKind::Meta {
                    meta_type: 0x05 | 0x06,
                    data,
                } = &e.kind
                {
                    markers.push((e.tick, smf_core::decode_text(data, hint)));
                }
            }
        }
        markers.sort_unstable();
        let tempo0 = doc
            .tempo_map
            .points()
            .first()
            .map(|(_, mpq, _)| 60_000_000.0 / *mpq as f64)
            .unwrap_or(120.0);
        let sig = doc
            .tracks
            .first()
            .and_then(|t| {
                t.events.iter().find_map(|e| match &e.kind {
                    EventKind::Meta {
                        meta_type: 0x58,
                        data,
                    } if data.len() >= 2 => Some(format!("{}/{}", data[0], 1u8 << data[1])),
                    _ => None,
                })
            })
            .unwrap_or_else(|| "4/4".into());
        let track_names = doc
            .tracks
            .iter()
            .enumerate()
            .map(|(i, tr)| {
                tr.name
                    .as_ref()
                    .map(|b| smf_core::decode_text(b, hint))
                    .unwrap_or_else(|| format!("Track {}", i + 1))
            })
            .collect();
        let track_chs = doc.tracks.iter().map(|t| t.out_channel).collect();
        Self {
            n_diags: doc.diagnose().len(),
            markers,
            track_names,
            track_chs,
            sig,
            tempo0,
            song_end: notes
                .iter()
                .map(|n| n.end_tick.unwrap_or(n.start_tick))
                .max()
                .unwrap_or(0),
        }
    }
}

struct EditorView {
    shared: SharedDoc,
    /// Bumped every time the whole document is swapped in (open / new file).
    /// Every freshly parsed file reports revision 0, so caches keyed on the
    /// revision alone cannot tell two documents apart — opening a file after
    /// an untouched one left the roll showing the previous (often empty)
    /// note view. Cache keys are `(doc_epoch, revision)`.
    doc_epoch: u64,
    notes_key: (u64, u64),
    notes: Arc<Vec<Note>>,
    ev_key: (u64, u64),
    events: Arc<Vec<SharedString>>,
    /// Document-derived UI data (markers, track names, diagnostics count…).
    /// Rebuilt only when the document key or encoding hint changes — render
    /// runs at animation-frame rate during playback and must not rescan
    /// every event each frame.
    doc_ui: Arc<DocUi>,
    doc_ui_key: (u64, u64),
    doc_ui_enc: Option<smf_core::TextEncoding>,
    /// lane (velocity/CC/PB) points cache — keys on epoch + revision +
    /// track + mode
    lane_cache: Arc<Vec<(EventId, u64, i32)>>,
    lane_key: (u64, u64),
    lane_track: usize,
    lane_mode_cached: LaneMode,
    /// last window-space cursor position, kept while a roll/lane drag is
    /// active so edge auto-scroll can keep the drag deltas current
    mouse_pos: Option<Point<Pixels>>,
    sel_track: usize,
    /// selected note `on_id`s (marquee multi-select)
    selection: BTreeSet<EventId>,
    drag: Option<Drag>,
    /// active piano-roll tool
    tool: Tool,
    /// index into SNAPS — grid snap divisor of a whole note
    snap_idx: usize,
    /// on_ids swept by the erase tool during a drag; deleted as one tx
    erase_ids: BTreeSet<EventId>,
    /// note clipboard (cut/copy/paste)
    clipboard: Vec<ClipNote>,
    /// canvas bounds as painted last frame — for hit-testing
    roll_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// seek-ruler strip bounds
    ruler_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// velocity lane bounds — same trick for the lane's hit-testing
    lane_bounds: Rc<Cell<Bounds<Pixels>>>,
    mini_bounds: Rc<Cell<Bounds<Pixels>>>,
    scroll_x: f32,
    scroll_y: f32,
    zoom: f32,
    /// Plugin instances kept warm on the host worker thread, keyed by dest
    /// index. They persist across play/stop/loop so parameter state survives
    /// and Play doesn't pay a load stall — only an explicit destination
    /// change, rescan, or app exit unloads them.
    plugin_slots: HashMap<usize, output::PluginSlot>,
    /// dest index → bundle path currently being loaded by the host thread
    plugin_state: HashMap<usize, PluginState>,
    plugin_req: std::sync::mpsc::Sender<output::PluginReq>,
    plugin_evt: std::sync::mpsc::Receiver<output::PluginEvent>,
    play_pending: bool,
    plugin_meta: HashMap<String, output::PluginInfo>,
    scan_rx: Option<std::sync::mpsc::Receiver<output::ScanReport>>,
    scan_note: Option<String>,
    scan_probe_used: Option<bool>,
    host_diag: output::HostDiag,
    show_output_status: bool,
    /// Standalone window hosting the open plugin editor (in-process
    /// instance — isolated plugins cannot host a GUI on Windows).
    plugin_window: Option<vst3_host::PluginWindow>,
    /// (dest index, in-process editor instance) for editor↔playback sync:
    /// param edits drain into the playing slot live, and full state is
    /// transferred on open/close via save_state/load_state.
    editor_plugin: Option<(usize, std::sync::Arc<std::sync::Mutex<vst3_host::Plugin>>)>,
    /// Manual text-encoding override for display decoding (None = auto/XF hint)
    enc_override: Option<smf_core::TextEncoding>,
    playback: Option<Playback>,
    play_us: u64,
    /// restart at `loop_start_us` when playback reaches the end
    /// (`loop_enabled` itself lives in `shared` so MCP can toggle it)
    loop_start_us: u64,
    /// what the bottom lane edits
    lane_mode: LaneMode,
    /// live MIDI input capture while `rec` is armed
    rec: Option<Rec>,
    /// open menubar dropdown + the x-coordinate it was opened at
    open_menu: Option<(TopMenu, f32)>,
    /// open cascading submenu + the y of its parent item
    open_sub: Option<(Sub, f32)>,
    /// right-docked event list panel visibility
    show_events: bool,
    /// F1 keyboard-shortcuts overlay
    help_open: bool,
    /// one-bar count-in before MIDI recording starts (global pref)
    count_in: bool,
    /// recently opened files (global pref, newest first)
    recent: Vec<SharedString>,
    /// recording source — MIDI input port name; empty = first available
    midi_in: SharedString,
    focus: FocusHandle,
    input: Entity<InputState>,
    status: SharedString,
}

enum PluginState {
    Loading {
        path: PathBuf,
        since: std::time::Instant,
    },
    Ready {
        path: PathBuf,
    },
    Failed {
        path: PathBuf,
        phase: &'static str,
        msg: String,
    },
}

/// What `ensure_plugin` should do for a destination — the load/unload
/// decision lifted out of the UI so the ordering rules are unit-testable
/// without a plugin (or a window).
#[derive(Debug, PartialEq, Eq)]
enum PluginPlan {
    /// the wanted bundle is already resident in the slot — nothing to do
    Satisfied,
    /// same bundle still loading, or a failed load we may not retry yet
    Wait,
    /// a load must be issued; `retire` when a warm slot is evicted first
    Open { retire: bool },
}

/// `state` is the tracked lifecycle state for the index, `warm` the bundle
/// currently resident in its slot (if any), `target` the bundle the
/// destination now points at. Order of checks matters: a Ready+resident
/// match short-circuits before the wait guards, and any other mismatch
/// reloads — retiring the stale slot first so the host never holds two
/// instances for one index.
fn plugin_plan(
    state: Option<&PluginState>,
    warm: Option<&Path>,
    target: &Path,
    force: bool,
) -> PluginPlan {
    if let Some(PluginState::Ready { path }) = state {
        if path == target && warm == Some(target) {
            return PluginPlan::Satisfied;
        }
    } else if warm == Some(target) {
        return PluginPlan::Satisfied;
    }
    match state {
        Some(PluginState::Loading { path, .. }) if path == target => PluginPlan::Wait,
        Some(PluginState::Failed { path, .. }) if path == target && !force => PluginPlan::Wait,
        _ => PluginPlan::Open {
            retire: warm.is_some(),
        },
    }
}

/// Captured (µs, raw channel bytes) pairs from the input callback.
type RecBuf = std::sync::Arc<Mutex<Vec<(u64, Vec<u8>)>>>;

/// Armed recording: timestamps channel messages against the playhead's µs base.
struct Rec {
    _input: midi_io::Input,
    buf: RecBuf,
    /// document time (µs) corresponding to Input's t=0
    base_us: u64,
    /// count-in duration — input before this is discarded
    cin_us: u64,
}

/// Output destination catalog: real MIDI ports by name, then discovered
/// VST3s. Rebuilt on `Output ▸ Rescan Plugins`.
fn build_dest_catalog(plugins: &[output::PluginInfo]) -> Vec<(String, midi_io::Destination)> {
    let mut dests: Vec<(String, midi_io::Destination)> = midi_io::list_outputs()
        .unwrap_or_default()
        .into_iter()
        .map(|p| {
            (
                p.name.clone(),
                midi_io::Destination::MidiPort { port_name: p.name },
            )
        })
        .collect();
    for p in plugins {
        dests.push((
            p.name.clone(),
            midi_io::Destination::Plugin {
                plugin_path: p.path.to_string_lossy().into_owned(),
            },
        ));
    }
    dests
}

/// Lock the shared editor state, surviving a poisoned mutex — one panic
/// inside a critical section must not brick every later lock on the UI and
/// MCP threads.
pub(crate) fn lock_shared(m: &Mutex<Shared>) -> std::sync::MutexGuard<'_, Shared> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn empty_doc() -> Document {
    let f = smf_core::File {
        format: 1,
        division: Division::Metrical(480),
        tracks: vec![smf_core::Track { events: vec![] }],
        warnings: vec![],
    };
    Document::from_file(f)
}

impl EditorView {
    fn new(path: Option<PathBuf>, input: Entity<InputState>, cx: &mut Context<Self>) -> Self {
        let loaded = path.as_deref().map(load_document);
        // the warning(s) belong in the status line, not swallowed
        let (doc, status): (Document, SharedString) = match loaded {
            Some(Ok((d, w))) if !w.is_empty() => (
                d,
                tf(
                    "status.loaded_warn",
                    &[("n", &w.len().to_string()), ("w", &w.join("; "))],
                )
                .into(),
            ),
            Some(Ok((d, _))) => (d, t("status.loaded").into()),
            Some(Err(e)) => (empty_doc(), tf("status.load_failed", &[("e", &e)]).into()),
            None => (empty_doc(), t("status.new_doc").into()),
        };
        let mut sh = Shared::new(doc);
        sh.path = path.clone();
        sh.saved_revision = sh.doc.revision();
        let initial_plugins = output::discover_plugin_paths();
        sh.dests = build_dest_catalog(&initial_plugins);
        let g = GlobalPrefs::load();
        let shared = Arc::new(Mutex::new(sh));
        let (plugin_req, plugin_evt) = output::spawn_plugin_host();
        let mut v = Self {
            shared,
            doc_epoch: 0,
            notes_key: (u64::MAX, u64::MAX),
            notes: Arc::new(vec![]),
            ev_key: (u64::MAX, u64::MAX),
            events: Arc::new(vec![]),
            doc_ui: Arc::new(DocUi::default()),
            doc_ui_key: (u64::MAX, u64::MAX),
            doc_ui_enc: None,
            lane_cache: Arc::new(vec![]),
            lane_key: (u64::MAX, u64::MAX),
            lane_track: 0,
            lane_mode_cached: LaneMode::Velocity,
            mouse_pos: None,
            sel_track: 0,
            selection: BTreeSet::new(),
            drag: None,
            tool: Tool::Select,
            snap_idx: 7, // 1/16
            erase_ids: BTreeSet::new(),
            clipboard: Vec::new(),
            roll_bounds: Rc::new(Cell::new(Bounds::new(
                point(px(0.0), px(0.0)),
                size(px(0.0), px(0.0)),
            ))),
            ruler_bounds: Rc::new(Cell::new(Bounds::new(
                point(px(0.0), px(0.0)),
                size(px(0.0), px(0.0)),
            ))),
            lane_bounds: Rc::new(Cell::new(Bounds::new(
                point(px(0.0), px(0.0)),
                size(px(0.0), px(0.0)),
            ))),
            mini_bounds: Rc::new(Cell::new(Bounds::new(
                point(px(0.0), px(0.0)),
                size(px(0.0), px(0.0)),
            ))),
            scroll_x: 0.0,
            scroll_y: (127.0 - 84.0) * NOTE_H, // show ~C3..C7
            zoom: 0.08,
            plugin_slots: HashMap::new(),
            plugin_state: HashMap::new(),
            plugin_req,
            plugin_evt,
            play_pending: false,
            plugin_meta: initial_plugins
                .into_iter()
                .map(|p| (p.path.to_string_lossy().into_owned(), p))
                .collect(),
            scan_rx: None,
            scan_note: None,
            scan_probe_used: None,
            host_diag: output::host_diag(),
            show_output_status: false,
            plugin_window: None,
            editor_plugin: None,
            enc_override: None,
            playback: None,
            play_us: 0,
            loop_start_us: 0,
            lane_mode: LaneMode::Velocity,
            rec: None,
            open_menu: None,
            help_open: false,
            count_in: g.count_in,
            recent: g.recent.iter().map(|p| p.as_str().into()).collect(),
            midi_in: g.midi_in.clone().into(),
            open_sub: None,
            show_events: true,
            focus: cx.focus_handle(),
            input,
            status,
        };
        v.sel_track = v.pick_default_track();
        v.refresh_derived();
        if let Some(p) = &path {
            v.apply_prefs(p);
            v.push_recent(p);
        }
        v.rescan_plugins();
        v
    }

    fn doc<R>(&self, f: impl FnOnce(&Document) -> R) -> R {
        let sh = lock_shared(&self.shared);
        f(&sh.doc)
    }

    /// New untitled document in place.
    fn new_file(&mut self, cx: &mut Context<Self>) {
        // an armed recording belongs to the document being replaced — drop
        // it with a warning instead of silently losing the take
        let rec_discarded = self.rec.take().is_some();
        self.stop_playback();
        {
            let mut sh = lock_shared(&self.shared);
            sh.doc = empty_doc();
            sh.undo = UndoStack::new(512);
            sh.path = None;
            sh.saved_revision = sh.doc.revision();
            sh.muted.clear();
            sh.soloed.clear();
            sh.track_dest.clear();
        }
        self.doc_epoch += 1; // the fresh document reports revision 0 again
        self.selection.clear();
        self.drag = None;
        self.erase_ids.clear();
        self.mouse_pos = None;
        self.sel_track = 0;
        self.enc_override = None;
        self.play_us = 0;
        self.refresh_derived();
        self.reset_view_to_content();
        self.status = if rec_discarded {
            format!("{} — {}", t("status.new_doc"), t("status.rec_discarded")).into()
        } else {
            t("status.new_doc").into()
        };
        cx.notify();
    }

    /// Select every note in the selected track.
    fn select_all(&mut self, cx: &mut Context<Self>) {
        self.selection = self
            .notes
            .iter()
            .filter(|n| n.track == self.sel_track)
            .map(|n| n.on_id)
            .collect();
        cx.notify();
    }

    /// Zoom by a factor around the viewport center (toolbar buttons/keys).
    fn zoom_by(&mut self, f: f32, cx: &mut Context<Self>) {
        self.zoom_set((self.zoom * f).clamp(ZOOM_MIN, ZOOM_MAX), cx);
    }

    /// Zoom to an absolute factor, keeping the tick at the viewport center
    /// fixed so the view doesn't lurch toward tick 0 on every change.
    fn zoom_set(&mut self, z: f32, cx: &mut Context<Self>) {
        let half = f32::from(self.roll_bounds.get().size.width) / 2.0;
        self.scroll_x = reanchor(self.scroll_x, self.zoom, z, half);
        self.zoom = z;
        self.clamp_scroll();
        self.persist();
        cx.notify();
    }

    /// Keep both scroll axes inside the content: 128 key rows vertically, the
    /// song end horizontally. Called from render so window resizes, zooms,
    /// and edits self-heal without every call site remembering to.
    fn clamp_scroll(&mut self) {
        let b = self.roll_bounds.get();
        let w = f32::from(b.size.width);
        let h = f32::from(b.size.height);
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        self.scroll_y = clamp_span(self.scroll_y, 128.0 * NOTE_H, h);
        self.scroll_x = clamp_span(self.scroll_x, self.doc_end_ticks() as f32 * self.zoom, w);
    }

    /// Point the view at the current document's content (first note with a
    /// left margin, median pitch centered). Runs on open/new-file before the
    /// per-file sidecar is applied, so a saved position still wins — but a
    /// first-time open never inherits the previous file's scroll offsets,
    /// which pointed at wherever the old file happened to be looking.
    fn reset_view_to_content(&mut self) {
        let first = self.notes.first().map(|n| n.start_tick).unwrap_or(0);
        let mid = if self.notes.is_empty() {
            None
        } else {
            let mut keys: Vec<u8> = self.notes.iter().map(|n| n.key).collect();
            keys.sort_unstable();
            Some(keys[keys.len() / 2] as i32)
        };
        let (x, y) = content_view(first, mid, self.zoom);
        self.scroll_x = x;
        self.scroll_y = y;
    }

    fn set_enc(&mut self, enc: Option<smf_core::TextEncoding>, cx: &mut Context<Self>) {
        self.enc_override = enc;
        self.ev_key = (u64::MAX, u64::MAX); // force event-row rebuild
        self.refresh_derived();
        self.persist();
        cx.notify();
    }

    fn set_lane(&mut self, m: LaneMode, cx: &mut Context<Self>) {
        self.lane_mode = m;
        self.persist();
        cx.notify();
    }

    /// snap interval in ticks (0 = off)
    /// Current snap step in ticks (0 = off). Triplet entries are 2/3 of the
    /// duple cell — 1/8T = a third of a quarter note.
    fn snap_ticks(&self) -> i64 {
        let (div, trip, _) = SNAPS[self.snap_idx];
        if div == 0 {
            return 0;
        }
        let base = (self.ppq() as i64 * 4) / div as i64;
        if trip {
            base * 2 / 3
        } else {
            base
        }
    }

    /// floor `t` onto the snap grid (used for note starts)
    fn snap_down(&self, t: i64) -> i64 {
        let s = self.snap_ticks();
        if s <= 0 {
            t
        } else {
            t - t.rem_euclid(s)
        }
    }

    /// nearest grid point (used for note ends / dragged positions)
    fn snap_round(&self, t: i64) -> i64 {
        let s = self.snap_ticks();
        if s <= 0 {
            t
        } else {
            ((t.max(0) + s / 2) / s) * s
        }
    }

    fn set_tool(&mut self, tool: Tool, cx: &mut Context<Self>) {
        self.tool = tool;
        self.persist();
        cx.notify();
    }

    fn set_snap(&mut self, idx: usize, cx: &mut Context<Self>) {
        self.snap_idx = idx;
        self.persist();
        cx.notify();
    }

    fn cycle_snap(&mut self, cx: &mut Context<Self>) {
        self.snap_idx = (self.snap_idx + 1) % SNAPS.len();
        self.persist();
        cx.notify();
    }

    /// delete the notes whose on_ids are in `erase_ids` as one undo step
    fn commit_erase(&mut self, cx: &mut Context<Self>) {
        let ids = std::mem::take(&mut self.erase_ids);
        if ids.is_empty() {
            return;
        }
        let sh = lock_shared(&self.shared);
        let mut ops = Vec::new();
        for &on_id in &ids {
            let off_id = self
                .notes
                .iter()
                .find(|n| n.on_id == on_id)
                .and_then(|n| n.off_id);
            if let Some(op) = Self::remove_note_op(&sh, on_id, off_id) {
                ops.push(op);
            }
        }
        drop(sh);
        if !ops.is_empty() {
            self.apply_tx("erase notes", ops);
        }
        self.selection.clear();
        cx.notify();
    }

    /// Focus the track-name input (Track > Rename).
    fn focus_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fh = self.input.read(cx).focus_handle(cx);
        window.focus(&fh, cx);
        cx.notify();
    }

    fn pick_default_track(&self) -> usize {
        self.doc(|d| {
            d.tracks
                .iter()
                .position(|t| {
                    t.events
                        .iter()
                        .any(|e| matches!(e.kind, EventKind::Channel { .. }))
                })
                .unwrap_or(0)
        })
    }

    fn refresh_derived(&mut self) {
        let arc = self.shared.clone();
        let mut sh = lock_shared(&arc);
        self.refresh_derived_sh(&mut sh);
    }

    fn refresh_derived_sh(&mut self, sh: &mut Shared) {
        let key = (self.doc_epoch, sh.doc.revision());
        if self.notes_key != key {
            self.notes = Arc::new(sh.doc.notes());
            self.notes_key = key;
        }
        if self.ev_key != key {
            self.events = Arc::new(self.build_event_rows(&sh.doc));
            self.ev_key = key;
        }
        if self.doc_ui_key != key || self.doc_ui_enc != self.enc_override {
            self.doc_ui = Arc::new(DocUi::build(&sh.doc, &self.notes, self.enc_override));
            self.doc_ui_key = key;
            self.doc_ui_enc = self.enc_override;
        }
    }

    /// Control events of the selected track for the bottom lane, cached on
    /// (epoch, revision, track, lane mode) — render must not rescan the
    /// track while animating the playhead.
    fn lane_events_cached(&mut self) -> Arc<Vec<(EventId, u64, i32)>> {
        let key = (self.doc_epoch, self.doc(|d| d.revision()));
        if self.lane_key != key
            || self.lane_track != self.sel_track
            || self.lane_mode_cached != self.lane_mode
        {
            let tr = self.sel_track;
            let mode = self.lane_mode;
            let mut v = Vec::new();
            self.doc(|d| {
                if let Some(t) = d.tracks.get(tr) {
                    for e in &t.events {
                        if let EventKind::Channel { status, data, .. } = &e.kind {
                            match (mode, status & 0xF0) {
                                (LaneMode::CC(cc), 0xB0) if data[0] == cc => {
                                    v.push((e.id, e.tick, data[1] as i32))
                                }
                                (LaneMode::PitchBend, 0xE0) => {
                                    v.push((e.id, e.tick, ((data[1] as i32) << 7) | data[0] as i32))
                                }
                                _ => {}
                            }
                        }
                    }
                }
            });
            v.sort_by_key(|e| e.1);
            self.lane_cache = Arc::new(v);
            self.lane_key = key;
            self.lane_track = tr;
            self.lane_mode_cached = mode;
        }
        self.lane_cache.clone()
    }

    /// Tick position of the song end (scroll extent, minimap scale).
    fn doc_end_ticks(&self) -> u64 {
        self.doc_ui.song_end.max(self.ppq() * 16)
    }

    fn ppq(&self) -> u64 {
        match self.doc(|d| d.division) {
            Division::Metrical(p) => (p as u64).max(1),
            Division::Smpte { .. } => 480,
        }
    }

    fn build_event_rows(&self, doc: &Document) -> Vec<SharedString> {
        let ppq = match doc.division {
            Division::Metrical(p) => (p as u64).max(1),
            Division::Smpte { .. } => 480,
        };
        let hint = self.enc_override.or(doc.text_encoding_hint());
        let mut rows = Vec::new();
        for d in doc.diagnose() {
            rows.push(format!("[{}] tk{} @{}", d.code, d.track + 1, d.tick).into());
        }
        for (ti, tr) in doc.tracks.iter().enumerate() {
            for e in &tr.events {
                let bar = e.tick / (ppq * 4) + 1;
                let beat = (e.tick % (ppq * 4)) / ppq + 1;
                let tk = e.tick % ppq;
                let body = match &e.kind {
                    EventKind::Channel { status, data, .. } => {
                        let ch = (status & 0x0F) + 1;
                        let name = match status & 0xF0 {
                            0x80 => "NoteOff",
                            0x90 => "NoteOn ",
                            0xA0 => "PolyAT ",
                            0xB0 => "CC     ",
                            0xC0 => "PC     ",
                            0xD0 => "ChanAT ",
                            0xE0 => "PB     ",
                            _ => "Ch?    ",
                        };
                        format!("{name} ch{ch:<2} {:>3} {:>3}", data[0], data[1])
                    }
                    EventKind::Meta { meta_type, data } => match *meta_type {
                        0x03 => format!("TrkName {}", smf_core::decode_text(data, hint)),
                        0x51 if data.len() == 3 => {
                            let mpq = u32::from_be_bytes([0, data[0], data[1], data[2]]);
                            format!("Tempo   {:.2} bpm", 60_000_000.0 / mpq as f64)
                        }
                        0x2F => "EndOfTrack".to_string(),
                        0x58 => format!(
                            "TimeSig {}/{}",
                            data.first().copied().unwrap_or(4),
                            data.get(1).copied().unwrap_or(4)
                        ),
                        0x59 => "KeySig".to_string(),
                        other @ 0x01..=0x09 => {
                            format!("Meta 0x{other:02X} {}", smf_core::decode_text(data, hint))
                        }
                        other => format!("Meta 0x{other:02X} {}B", data.len()),
                    },
                    EventKind::SysEx(d) => format!("SysEx   {}B", d.len()),
                    EventKind::Escape(d) => format!("Escape  {}B", d.len()),
                };
                rows.push(SharedString::from(format!(
                    "{bar:>4}.{beat}.{tk:>3}  T{ti}  {body}"
                )));
            }
        }
        rows
    }

    fn apply_tx(&mut self, label: &str, ops: Vec<Op>) {
        let arc = self.shared.clone();
        let mut sh = lock_shared(&arc);
        match sh.apply(label, ops) {
            Ok(_) => self.refresh_derived_sh(&mut sh),
            Err(e) => self.status = tf("status.apply_failed", &[("e", &e.to_string())]).into(),
        }
    }

    /// Run a semantic region transform (`Document` *_ops generator) on the
    /// selection's range — or the whole selected track when nothing is
    /// selected. The same generators power the MCP tools, so GUI and AI edits
    /// share semantics and undo.
    fn apply_region_op(
        &mut self,
        label: &str,
        f: impl Fn(&mut Document, usize, u64, u64) -> Vec<Op>,
    ) {
        let (tracks, from, to) = if self.selection.is_empty() {
            (vec![self.sel_track], 0, u64::MAX)
        } else {
            let mut tracks = BTreeSet::new();
            let (mut lo, mut hi) = (u64::MAX, 0u64);
            for n in self.notes.iter() {
                if self.selection.contains(&n.on_id) {
                    tracks.insert(n.track);
                    lo = lo.min(n.start_tick);
                    hi = hi.max(n.end_tick.unwrap_or(n.start_tick));
                }
            }
            (tracks.into_iter().collect::<Vec<_>>(), lo, hi + 1)
        };
        let ops = {
            let mut sh = lock_shared(&self.shared);
            tracks
                .into_iter()
                .flat_map(|t| f(&mut sh.doc, t, from, to))
                .collect::<Vec<_>>()
        };
        if ops.is_empty() {
            self.status = format!("{label}: nothing to change").into();
        } else {
            self.apply_tx(label, ops);
            self.status = label.to_string().into();
        }
    }

    /// Set the tick-0 tempo to current bpm + delta (via the shared op layer).
    fn bump_tempo(&mut self, delta: f64) {
        let cur = self.doc(|d| {
            d.tempo_map
                .points()
                .first()
                .map(|(_, mpq, _)| 60_000_000.0 / *mpq as f64)
                .unwrap_or(120.0)
        });
        let ops = {
            let mut sh = lock_shared(&self.shared);
            sh.doc.set_tempo_ops(0, (cur + delta).clamp(10.0, 400.0))
        };
        self.apply_tx("set tempo", ops);
    }

    /// Cycle the tick-0 time signature through common meters.
    fn cycle_time_sig(&mut self) {
        const SIGS: [(u8, u8); 6] = [(4, 4), (3, 4), (2, 4), (5, 4), (6, 8), (7, 8)];
        let cur = self.doc(|d| {
            d.tracks.first().and_then(|t| {
                t.events.iter().find_map(|e| match &e.kind {
                    EventKind::Meta {
                        meta_type: 0x58,
                        data,
                    } if data.len() >= 2 => Some((data[0], 1u8 << data[1])),
                    _ => None,
                })
            })
        });
        let next = match cur.and_then(|c| SIGS.iter().position(|s| *s == c)) {
            Some(i) => SIGS[(i + 1) % SIGS.len()],
            None => SIGS[1],
        };
        let ops = {
            let mut sh = lock_shared(&self.shared);
            sh.doc.set_time_sig_ops(0, next.0, next.1)
        };
        self.apply_tx("set time signature", ops);
    }

    #[allow(dead_code)]
    fn insert_note(&mut self, tick: u64, key: u8, cx: &mut Context<Self>) {
        let len = self.snap_ticks().max(self.ppq() as i64 / 4) as u64;
        self.insert_note_len(tick, key, len, cx);
    }

    fn insert_note_len(&mut self, tick: u64, key: u8, len: u64, cx: &mut Context<Self>) {
        let (on_id, off_id, track) = {
            let mut sh = lock_shared(&self.shared);
            let track = self.sel_track.min(sh.doc.tracks.len().saturating_sub(1));
            (sh.doc.alloc_event_id(), sh.doc.alloc_event_id(), track)
        };
        let tick = self.snap_down(tick as i64).max(0) as u64;
        let on = DocEvent {
            id: on_id,
            tick,
            seq: u32::MAX / 2,
            raw_body: None,
            kind: EventKind::Channel {
                status: 0x90,
                data: [key, 100],
                len: 2,
            },
        };
        let off = DocEvent {
            id: off_id,
            tick: tick + len,
            seq: u32::MAX / 2,
            raw_body: None,
            kind: EventKind::Channel {
                status: 0x80,
                data: [key, 0],
                len: 2,
            },
        };
        self.apply_tx(
            "insert note",
            vec![Op::InsertEvents {
                track,
                events: vec![on, off],
            }],
        );
        self.selection = BTreeSet::from([on_id]);
        cx.notify();
    }

    /// Remove a note's on+off events; returns the op (or None if id unknown).
    fn remove_note_op(sh: &Shared, on_id: EventId, off_id: Option<EventId>) -> Option<Op> {
        let mut track = 0usize;
        let mut removed: Vec<(usize, document::Event)> = Vec::new();
        'outer: for (ti, t) in sh.doc.tracks.iter().enumerate() {
            for (ei, e) in t.events.iter().enumerate() {
                if e.id == on_id {
                    track = ti;
                    removed.push((ei, e.clone()));
                    break 'outer;
                }
            }
        }
        if removed.is_empty() {
            return None;
        }
        if let Some(off_id) = off_id {
            if let Some(pos) = sh.doc.tracks[track]
                .events
                .iter()
                .position(|e| e.id == off_id)
            {
                removed.push((pos, sh.doc.tracks[track].events[pos].clone()));
            }
        }
        Some(Op::RemoveEvents { track, removed })
    }

    fn delete_selected(&mut self, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            return;
        }
        let sh = lock_shared(&self.shared);
        let mut ops = Vec::new();
        for &on_id in &self.selection {
            let off_id = self
                .notes
                .iter()
                .find(|n| n.on_id == on_id)
                .and_then(|n| n.off_id);
            if let Some(op) = Self::remove_note_op(&sh, on_id, off_id) {
                ops.push(op);
            }
        }
        drop(sh);
        if !ops.is_empty() {
            self.apply_tx("delete notes", ops);
        }
        self.selection.clear();
        cx.notify();
    }

    /// Copy the selection into the note clipboard (`cut` also deletes it).
    fn copy_selected(&mut self, cut: bool, cx: &mut Context<Self>) {
        let ppq = self.ppq();
        let sel: Vec<Note> = self
            .notes
            .iter()
            .filter(|n| self.selection.contains(&n.on_id))
            .cloned()
            .collect();
        if sel.is_empty() {
            self.status = t("status.nosel").into();
            cx.notify();
            return;
        }
        let lo = sel.iter().map(|n| n.start_tick).min().unwrap();
        self.clipboard = sel
            .iter()
            .map(|n| ClipNote {
                dtick: (n.start_tick - lo) as i64,
                key: n.key,
                len: n
                    .end_tick
                    .unwrap_or(n.start_tick + ppq / 4)
                    .saturating_sub(n.start_tick)
                    .max(1),
                vel: n.vel,
                ch: n.channel,
                track: n.track,
            })
            .collect();
        let n = self.clipboard.len();
        if cut {
            self.delete_selected(cx);
        }
        self.status = tf("status.copied", &[("n", &n.to_string())]).into();
        cx.notify();
    }

    /// Insert `items` as fresh notes at `anchor` — shared by paste/duplicate.
    fn insert_clip(
        &mut self,
        items: &[ClipNote],
        anchor: u64,
        label: &str,
        cx: &mut Context<Self>,
    ) {
        if items.is_empty() {
            return;
        }
        let mut ops = Vec::new();
        let mut sel_ids = Vec::new();
        {
            let mut sh = lock_shared(&self.shared);
            let ntr = sh.doc.tracks.len();
            let mut per_track: BTreeMap<usize, Vec<DocEvent>> = BTreeMap::new();
            for c in items {
                let track = c.track.min(ntr.saturating_sub(1));
                let tick = (anchor as i64 + c.dtick).max(0) as u64;
                let ch = c.ch & 0x0F;
                let on_id = sh.doc.alloc_event_id();
                let off_id = sh.doc.alloc_event_id();
                sel_ids.push(on_id);
                per_track.entry(track).or_default().extend([
                    DocEvent {
                        id: on_id,
                        tick,
                        seq: u32::MAX / 2,
                        raw_body: None,
                        kind: EventKind::Channel {
                            status: 0x90 | ch,
                            data: [c.key, c.vel],
                            len: 2,
                        },
                    },
                    DocEvent {
                        id: off_id,
                        tick: tick + c.len,
                        seq: u32::MAX / 2,
                        raw_body: None,
                        kind: EventKind::Channel {
                            status: 0x80 | ch,
                            data: [c.key, 0],
                            len: 2,
                        },
                    },
                ]);
            }
            for (track, events) in per_track {
                ops.push(Op::InsertEvents { track, events });
            }
        }
        self.apply_tx(label, ops);
        self.selection = sel_ids.into_iter().collect();
        cx.notify();
    }

    /// Paste the clipboard at the edit cursor (playhead), snapped to the grid.
    fn paste(&mut self, cx: &mut Context<Self>) {
        if self.clipboard.is_empty() {
            self.status = t("status.noclip").into();
            cx.notify();
            return;
        }
        let anchor = self
            .snap_down(self.doc(|d| d.tempo_map.us_to_tick(self.play_us)) as i64)
            .max(0) as u64;
        let src = self.clipboard.clone();
        self.insert_clip(&src, anchor, "paste notes", cx);
    }

    /// Duplicate the selection, tiled immediately after it (Ctrl+D).
    fn duplicate_selected(&mut self, cx: &mut Context<Self>) {
        let ppq = self.ppq();
        let sel: Vec<Note> = self
            .notes
            .iter()
            .filter(|n| self.selection.contains(&n.on_id))
            .cloned()
            .collect();
        if sel.is_empty() {
            self.status = t("status.nosel").into();
            cx.notify();
            return;
        }
        let lo = sel.iter().map(|n| n.start_tick).min().unwrap();
        let hi = sel
            .iter()
            .map(|n| n.end_tick.unwrap_or(n.start_tick))
            .max()
            .unwrap();
        let items: Vec<ClipNote> = sel
            .iter()
            .map(|n| ClipNote {
                dtick: (n.start_tick - lo) as i64,
                key: n.key,
                len: n
                    .end_tick
                    .unwrap_or(n.start_tick + ppq / 4)
                    .saturating_sub(n.start_tick)
                    .max(1),
                vel: n.vel,
                ch: n.channel,
                track: n.track,
            })
            .collect();
        self.insert_clip(&items, hi, "duplicate notes", cx);
    }

    /// Move every selected note by (dtick, dkey) — arrow-key nudge.
    fn nudge(&mut self, dtick: i64, dkey: i32, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            return;
        }
        let mut ops = Vec::new();
        {
            let sh = lock_shared(&self.shared);
            for n in self
                .notes
                .iter()
                .filter(|n| self.selection.contains(&n.on_id))
            {
                let nk = (n.key as i32 + dkey).clamp(0, 127) as u8;
                if dtick == 0 && nk == n.key {
                    continue;
                }
                // the track may have been removed by an MCP edit/undo since
                // the note view was built — skip instead of indexing into it
                let Some(track) = sh.doc.tracks.get(n.track) else {
                    continue;
                };
                for e in track.events.iter() {
                    if e.id != n.on_id && n.off_id != Some(e.id) {
                        continue;
                    }
                    let mut after = e.clone();
                    after.tick = after.tick.saturating_add_signed(dtick);
                    if e.id == n.on_id {
                        if let EventKind::Channel { data, .. } = &mut after.kind {
                            data[0] = nk;
                        }
                    }
                    ops.push(Op::UpdateEvent {
                        track: n.track,
                        before: e.clone(),
                        after,
                    });
                }
            }
        }
        if !ops.is_empty() {
            self.apply_tx("nudge", ops);
        }
        cx.notify();
    }

    /// Move the playhead to `tick`; `play` (or an already-playing transport)
    /// restarts the engine from there.
    fn seek_to_tick(&mut self, tick: u64, play: bool, cx: &mut Context<Self>) {
        self.play_us = self.doc(|d| d.tempo_map.tick_to_us(tick));
        if play || self.playback.is_some() {
            self.stop_playback();
            self.start_playback();
        }
        cx.notify();
    }

    fn commit_drag(&mut self, cx: &mut Context<Self>) {
        let Some(d) = self.drag.take() else { return };
        match d.mode {
            DragMode::Erase => {
                self.commit_erase(cx);
                return;
            }
            DragMode::Marquee => {
                // draw tool: the drag box (or a click's point) becomes a note
                if self.tool == Tool::Draw {
                    if (0..=127).contains(&d.a_key) {
                        let (a, b) = (d.a_tick.min(d.b_tick), d.a_tick.max(d.b_tick));
                        let len = (b - a).max(self.snap_ticks().max(1));
                        self.insert_note_len(a.max(0) as u64, d.a_key as u8, len as u64, cx);
                    }
                    return;
                }
                // rect select: notes intersecting the rubber-band box
                let (t0, t1) = (d.a_tick.min(d.b_tick), d.a_tick.max(d.b_tick));
                let (k0, k1) = (d.a_key.min(d.b_key), d.a_key.max(d.b_key));
                self.selection = self
                    .notes
                    .iter()
                    .filter(|n| {
                        let st = n.start_tick as i64;
                        let en = n.end_tick.unwrap_or(n.start_tick) as i64;
                        let key = n.key as i32;
                        st <= t1 && en >= t0 && key >= k0 && key <= k1
                    })
                    .map(|n| n.on_id)
                    .collect();
                cx.notify();
                return;
            }
            DragMode::Resize => {
                if d.dtick == 0 {
                    return;
                }
                let Some(orig_end) = d.orig_end else { return };
                let new_end = (self
                    .snap_round(orig_end as i64 + d.dtick)
                    .max(d.orig_start as i64 + 1)) as u64;
                let sh = lock_shared(&self.shared);
                let mut ops = Vec::new();
                if let Some(off_id) = d.off_id {
                    // the track may be gone (MCP remove/undo during the drag)
                    if let Some(track) = sh.doc.tracks.get(d.track) {
                        for e in &track.events {
                            if e.id == off_id {
                                let mut after = e.clone();
                                after.tick = new_end;
                                ops.push(Op::UpdateEvent {
                                    track: d.track,
                                    before: e.clone(),
                                    after,
                                });
                            }
                        }
                    }
                }
                drop(sh);
                if !ops.is_empty() {
                    self.apply_tx("resize note", ops);
                }
                cx.notify();
                return;
            }
            DragMode::Velocity => {
                let vel = d.dkey.clamp(1, 127) as u8;
                let sh = lock_shared(&self.shared);
                let mut ops = Vec::new();
                if let Some(track) = sh.doc.tracks.get(d.track) {
                    for e in &track.events {
                        if e.id == d.on_id {
                            let mut after = e.clone();
                            if let EventKind::Channel { data, .. } = &mut after.kind {
                                data[1] = vel;
                            }
                            ops.push(Op::UpdateEvent {
                                track: d.track,
                                before: e.clone(),
                                after,
                            });
                        }
                    }
                }
                drop(sh);
                if !ops.is_empty() {
                    self.apply_tx("set velocity", ops);
                }
                cx.notify();
                return;
            }
            DragMode::Move | DragMode::Duplicate => {}
            DragMode::LaneEvent => {
                // CC/PB lane: update an existing event's value, or insert a
                // new one when the drag started on empty lane space
                let mut sh = lock_shared(&self.shared);
                let mut ops = Vec::new();
                // the track may be gone (MCP remove/undo during the drag)
                let Some(track_events) = sh.doc.tracks.get(d.track) else {
                    drop(sh);
                    cx.notify();
                    return;
                };
                if d.on_id == 0 {
                    let ch = track_events.out_channel & 0x0F;
                    let (status, data) = match self.lane_mode {
                        LaneMode::CC(cc) => (0xB0 | ch, [cc, d.dkey.clamp(0, 127) as u8]),
                        LaneMode::PitchBend => {
                            let v = d.dkey.clamp(0, 16383) as u16;
                            (0xE0 | ch, [(v & 0x7F) as u8, (v >> 7) as u8])
                        }
                        LaneMode::Velocity => unreachable!(),
                    };
                    let id = sh.doc.alloc_event_id();
                    ops.push(Op::InsertEvents {
                        track: d.track,
                        events: vec![DocEvent {
                            id,
                            tick: self.snap_down(d.a_tick).max(0) as u64,
                            seq: 0,
                            raw_body: None,
                            kind: EventKind::Channel {
                                status,
                                data,
                                len: 2,
                            },
                        }],
                    });
                } else {
                    for e in &track_events.events {
                        if e.id == d.on_id {
                            let mut after = e.clone();
                            if let EventKind::Channel { data, .. } = &mut after.kind {
                                match self.lane_mode {
                                    LaneMode::CC(_) => data[1] = d.dkey.clamp(0, 127) as u8,
                                    LaneMode::PitchBend => {
                                        let v = d.dkey.clamp(0, 16383) as u16;
                                        data[0] = (v & 0x7F) as u8;
                                        data[1] = (v >> 7) as u8;
                                    }
                                    LaneMode::Velocity => unreachable!(),
                                }
                            }
                            ops.push(Op::UpdateEvent {
                                track: d.track,
                                before: e.clone(),
                                after,
                            });
                        }
                    }
                }
                drop(sh);
                if !ops.is_empty() {
                    self.apply_tx("edit lane", ops);
                }
                cx.notify();
                return;
            }
        }
        if d.dtick == 0 && d.dkey == 0 {
            return;
        }
        // magnet: the dragged note's resulting start snaps to the grid
        let d = Drag {
            dtick: self.snap_round(d.orig_start as i64 + d.dtick) - d.orig_start as i64,
            ..d
        };
        if d.dtick == 0 && d.dkey == 0 {
            return;
        }
        // move every selected note by the same delta; fall back to the
        // dragged note if the selection never took (e.g. programmatic)
        let ids: BTreeSet<EventId> = if self.selection.contains(&d.on_id) {
            self.selection.clone()
        } else {
            BTreeSet::from([d.on_id])
        };
        let notes = self.notes.clone();
        let duplicate = d.mode == DragMode::Duplicate;
        let mut sh = lock_shared(&self.shared);
        // two passes: shift-and-clone while iterating immutably, mint ids after
        let mut staged: Vec<(usize, document::Event, bool)> = Vec::new();
        for (ti, t) in sh.doc.tracks.iter().enumerate() {
            for e in &t.events {
                let Some(n) = notes.iter().find(|n| {
                    ids.contains(&n.on_id) && (e.id == n.on_id || n.off_id == Some(e.id))
                }) else {
                    continue;
                };
                let is_on = e.id == n.on_id;
                let orig = if is_on {
                    n.start_tick
                } else {
                    n.end_tick.unwrap_or(n.start_tick)
                };
                let mut after = e.clone();
                after.tick = ((orig as i64 + d.dtick).max(0)) as u64;
                if let EventKind::Channel { data, .. } = &mut after.kind {
                    data[0] = (n.key as i32 + d.dkey).clamp(0, 127) as u8;
                }
                // raw_body is dropped centrally when the kind changed; a
                // pure time move keeps the verbatim body bytes
                staged.push((ti, after, is_on));
            }
        }
        let mut ops = Vec::new();
        for (ti, mut after, _is_on) in staged {
            if duplicate {
                after.id = sh.doc.alloc_event_id();
                ops.push(Op::InsertEvents {
                    track: ti,
                    events: vec![after],
                });
            } else {
                let before = sh.doc.tracks[ti]
                    .events
                    .iter()
                    .find(|e| {
                        e.id == {
                            // original id is preserved on `after` for Move
                            after.id
                        }
                    })
                    .cloned();
                if let Some(before) = before {
                    ops.push(Op::UpdateEvent {
                        track: ti,
                        before,
                        after,
                    });
                }
            }
        }
        drop(sh);
        if !ops.is_empty() {
            self.apply_tx(
                if duplicate {
                    "duplicate notes"
                } else {
                    "move notes"
                },
                ops,
            );
        }
        cx.notify();
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        let arc = self.shared.clone();
        let mut sh = lock_shared(&arc);
        if let Some(l) = {
            let Shared { doc, undo, .. } = &mut *sh;
            undo.undo(doc)
        } {
            self.status = tf("status.undo", &[("label", &l)]).into();
            self.selection.clear();
            self.refresh_derived_sh(&mut sh);
            drop(sh);
            cx.notify();
        }
    }

    fn redo(&mut self, cx: &mut Context<Self>) {
        let arc = self.shared.clone();
        let mut sh = lock_shared(&arc);
        if let Some(l) = {
            let Shared { doc, undo, .. } = &mut *sh;
            undo.redo(doc)
        } {
            self.status = tf("status.redo", &[("label", &l)]).into();
            self.selection.clear();
            self.refresh_derived_sh(&mut sh);
            drop(sh);
            cx.notify();
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let path = {
            let sh = lock_shared(&self.shared);
            sh.path.clone()
        };
        let Some(p) = path else {
            self.save_as(cx);
            return;
        };
        // serialize under the lock, write outside it — and remember the
        // revision the bytes were taken at so concurrent edits stay dirty
        let (bytes, rev) = {
            let sh = lock_shared(&self.shared);
            (
                sh.doc.serialize(smf_core::WriteOptions {
                    running_status: false,
                }),
                sh.doc.revision(),
            )
        };
        match mcp_server::write_atomic(&p, &bytes) {
            Ok(()) => {
                let mut sh = lock_shared(&self.shared);
                sh.saved_revision = rev;
                drop(sh);
                self.status = t("status.saved").into();
                self.persist();
            }
            Err(e) => self.status = format!("{e}").into(),
        }
        cx.notify();
    }

    fn save_as(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_new_path(
            &std::env::current_dir().unwrap_or_default(),
            Some("untitled.mid"),
        );
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                if let Some(this) = this.upgrade() {
                    this.update(cx, |v, cx| {
                        crate::lock_shared(&v.shared).path = Some(path);
                        v.save(cx);
                    });
                }
            }
        })
        .detach();
    }

    fn open_dialog(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(p) = paths.into_iter().next() {
                    if let Some(this) = this.upgrade() {
                        this.update(cx, |v, cx| v.open(p, cx));
                    }
                }
            }
        })
        .detach();
    }

    fn open(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        match load_document(&path) {
            Ok((d, load_warnings)) => {
                // an armed recording belongs to the previous document —
                // drop it with a warning instead of silently losing the take
                let rec_discarded = self.rec.take().is_some();
                self.stop_playback();
                // swap the document in place — the MCP server holds this same Arc
                {
                    let mut sh = lock_shared(&self.shared);
                    sh.doc = d;
                    sh.undo = UndoStack::new(512);
                    sh.path = Some(path.clone());
                    sh.saved_revision = sh.doc.revision();
                }
                // the new document also reports revision 0 — bump the epoch
                // so revision-keyed derived views cannot stay stale
                self.doc_epoch += 1;
                self.sel_track = self.pick_default_track();
                self.selection.clear();
                self.drag = None;
                self.erase_ids.clear();
                self.mouse_pos = None;
                self.enc_override = None;
                self.play_us = 0;
                {
                    let mut sh = lock_shared(&self.shared);
                    sh.muted.clear();
                    sh.soloed.clear();
                    sh.track_dest.clear();
                }
                // rebuild the derived views, then land the view on the new
                // content — a saved per-file sidecar (applied next) overrides
                self.refresh_derived();
                self.reset_view_to_content();
                self.apply_prefs(&path);
                self.push_recent(&path);
                let mut status = if load_warnings.is_empty() {
                    t("status.loaded").to_string()
                } else {
                    tf(
                        "status.loaded_warn",
                        &[
                            ("n", &load_warnings.len().to_string()),
                            ("w", &load_warnings.join("; ")),
                        ],
                    )
                };
                if rec_discarded {
                    status = format!("{status} — {}", t("status.rec_discarded"));
                }
                self.status = status.into();
            }
            Err(e) => self.status = tf("status.load_failed", &[("e", &e.to_string())]).into(),
        }
        cx.notify();
    }

    fn toggle_play(&mut self, cx: &mut Context<Self>) {
        if self.playback.is_some() {
            self.stop_playback();
        } else {
            self.start_playback();
        }
        cx.notify();
    }

    /// If dest `d` is a VST3 bundle not already loaded/loading, ask the host
    /// thread to warm it. Called when a destination is assigned and from
    /// `refresh_plugins` — Play then never pays the load stall.
    fn ensure_plugin(&mut self, d: usize, force: bool) {
        let path = {
            let sh = lock_shared(&self.shared);
            match sh.dests.get(d).map(|(_, dest)| dest) {
                Some(output::Destination::Plugin { plugin_path }) => PathBuf::from(plugin_path),
                _ => return,
            }
        };
        let PluginPlan::Open { retire } = plugin_plan(
            self.plugin_state.get(&d),
            self.plugin_slots.get(&d).map(|s| s.path.as_path()),
            &path,
            force,
        ) else {
            return;
        };
        // index now points at a different bundle — retire the old instance
        if retire && self.plugin_slots.remove(&d).is_some() {
            let _ = self.plugin_req.send(output::PluginReq::Drop(d));
        }
        self.plugin_state.insert(
            d,
            PluginState::Loading {
                path: path.clone(),
                since: std::time::Instant::now(),
            },
        );
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.status = tf("plugin.loading", &[("name", name.as_str())]).into();
        let _ = self.plugin_req.send(output::PluginReq::Open(d, path));
    }

    /// Warm instances for every VST3 destination a track or the default
    /// currently resolves to. Cheap to call often — no-ops once satisfied.
    fn refresh_plugins(&mut self) {
        let idxs: Vec<usize> = {
            let sh = lock_shared(&self.shared);
            let mut v: Vec<usize> = sh.track_dest.values().copied().collect();
            v.push(sh.default_dest);
            v
        };
        for d in idxs {
            self.ensure_plugin(d, false);
        }
    }

    fn poll_plugin_events(&mut self) -> bool {
        let mut changed = false;
        let now = std::time::Instant::now();
        let timed_out: Vec<usize> = self.plugin_state.iter().filter_map(|(&d, s)| {
            matches!(s, PluginState::Loading { since, .. } if now.duration_since(*since) >= std::time::Duration::from_secs(20)).then_some(d)
        }).collect();
        for d in timed_out {
            if let Some(PluginState::Loading { path, .. }) = self.plugin_state.remove(&d) {
                self.plugin_state.insert(
                    d,
                    PluginState::Failed {
                        path,
                        phase: "load",
                        msg: t("plugin.timeout").to_string(),
                    },
                );
                let _ = self.plugin_req.send(output::PluginReq::Drop(d));
                changed = true;
            }
        }
        while let Ok(event) = self.plugin_evt.try_recv() {
            let Some(PluginState::Loading { path, .. }) = self.plugin_state.get(&event.dest) else {
                continue;
            };
            if path != &event.path {
                continue;
            }
            match event.result {
                Ok(slot) => {
                    let name = slot
                        .path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    self.plugin_slots.insert(event.dest, slot);
                    self.plugin_state
                        .insert(event.dest, PluginState::Ready { path: event.path });
                    self.status = tf("plugin.ready", &[("name", name.as_str())]).into();
                }
                Err(e) => {
                    let phase = match e {
                        output::PluginError::Host(_) => "host",
                        output::PluginError::Load(_) => "load",
                        output::PluginError::Audio(_) => "audio",
                    };
                    let name = event
                        .path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    self.plugin_state.insert(
                        event.dest,
                        PluginState::Failed {
                            path: event.path,
                            phase,
                            msg: e.to_string(),
                        },
                    );
                    self.status = tf("plugin.failed", &[("name", name.as_str())]).into();
                }
            }
            changed = true;
        }
        if self.play_pending {
            let needed_loading = {
                let sh = lock_shared(&self.shared);
                sh.doc.tracks.iter().enumerate().any(|(t, _)| {
                    let d = sh.dest_of(t);
                    matches!(self.plugin_state.get(&d), Some(PluginState::Loading { .. }))
                })
            };
            if !needed_loading {
                self.play_pending = false;
                self.start_playback();
                changed = true;
            }
        }
        changed
    }

    fn start_playback(&mut self) {
        // snapshot routing state so no lock is held while opening sinks
        let (dests, dest_of_track, muted, soloed, metronome, loop_enabled, chase_sysex) = {
            let sh = lock_shared(&self.shared);
            let map: HashMap<usize, usize> = (0..sh.doc.tracks.len())
                .map(|t| (t, sh.dest_of(t)))
                .collect();
            (
                sh.dests.clone(),
                map,
                sh.muted.clone(),
                sh.soloed.clone(),
                sh.metronome,
                sh.loop_enabled,
                sh.chase_sysex,
            )
        };
        let dest_of = |t: usize| dest_of_track.get(&t).copied().unwrap_or(0);
        if dests.is_empty() {
            self.status = t("status.no_port").into();
            return;
        }
        let audible = |tr: usize| {
            if !soloed.is_empty() {
                soloed.contains(&tr)
            } else {
                !muted.contains(&tr)
            }
        };
        let tagged: Vec<(u64, usize, Vec<u8>)> = self
            .doc(|d| d.timeline_tagged())
            .into_iter()
            .filter(|(_, tr, _)| audible(*tr))
            .collect();
        // open each destination that at least one event needs
        let needed: BTreeSet<usize> = tagged.iter().map(|(_, tr, _)| dest_of(*tr)).collect();
        let mut sinks: Vec<Box<dyn EventSink>> = Vec::new();
        let mut sink_of: HashMap<usize, usize> = HashMap::new();
        // host transport hints for plugin instances (arps/LFO sync): current
        // bpm + time signature, then `playing` flipped on per used instance
        let (bpm, sig_num, sig_den) = self.transport_hints();
        self.poll_plugin_events();
        for d in needed {
            let Some((_, dest)) = dests.get(d) else {
                continue;
            };
            match dest {
                output::Destination::MidiPort { port_name } => {
                    match midi_io::Output::open_named(port_name) {
                        Ok(out) => {
                            sink_of.insert(d, sinks.len());
                            sinks.push(Box::new(PortSink::new(out)));
                        }
                        Err(e) => self.status = format!("{e}").into(),
                    }
                }
                output::Destination::Plugin { .. } => {
                    self.ensure_plugin(d, false);
                    if matches!(self.plugin_state.get(&d), Some(PluginState::Loading { .. })) {
                        let name = dests[d].0.clone();
                        self.play_pending = true;
                        self.status = tf("plugin.waiting", &[("name", name.as_str())]).into();
                        return;
                    }
                    if self.plugin_slots.contains_key(&d)
                        && matches!(self.plugin_state.get(&d), Some(PluginState::Ready { .. }))
                    {
                        let slot = self.plugin_slots.get(&d).expect("slot just loaded");
                        sink_of.insert(d, sinks.len());
                        sinks.push(Box::new(slot.sink.clone()));
                        if let Ok(mut p) = slot.plugin.lock() {
                            let _ = p.set_tempo(bpm);
                            let _ = p.set_time_signature(sig_num, sig_den);
                            let _ = p.set_playing(true);
                        }
                    } else if let Some(PluginState::Failed { .. }) = self.plugin_state.get(&d) {
                        self.status = tf("plugin.failed", &[("name", dests[d].0.as_str())]).into();
                    }
                }
            }
        }
        if sinks.is_empty() {
            self.status = t("status.no_port").into();
            return;
        }
        // SysEx first among same-time events: setup traffic (GM/XG resets,
        // patch dumps) must land before notes struck at the same instant.
        // The stable sort below keeps sysex < channel < click at equal µs.
        let mut events: Vec<(u64, usize, Vec<u8>)> = self
            .doc(|d| d.timeline_sysex())
            .into_iter()
            .filter(|(_, tr, _)| audible(*tr))
            .filter_map(|(us, tr, b)| sink_of.get(&dest_of(tr)).map(|&s| (us, s, b)))
            .collect();
        events.extend(tagged.into_iter().filter_map(|(us, tr, b)| {
            sink_of.get(&dest_of(tr)).map(|&s| (us, s, b))
        }));
        if metronome {
            // prefer a plain MIDI port for clicks; fall back to any sink
            let click_sink = dests
                .iter()
                .enumerate()
                .find(|(_, (_, d))| matches!(d, output::Destination::MidiPort { .. }))
                .and_then(|(d, _)| sink_of.get(&d).copied())
                .or_else(|| sink_of.values().next().copied());
            if let Some(s) = click_sink {
                let ppq = self.ppq();
                let end_us = events.iter().map(|e| e.0).max().unwrap_or(0);
                let mut beat = 0u64;
                loop {
                    let us = self.doc(|d| d.tempo_map.tick_to_us(beat * ppq));
                    if us > end_us {
                        break;
                    }
                    let note = if beat.is_multiple_of(4) { 76 } else { 77 };
                    events.push((us, s, vec![0x99, note, 110]));
                    events.push((us + 20_000, s, vec![0x99, note, 0]));
                    beat += 1;
                }
                events.sort_by_key(|e| e.0);
            }
        }
        // chase: re-establish the state the timeline had built up before the
        // play position (CC/program/bend/at, plus notes already sounding) so
        // mid-song starts and loop wraps sound like a continuous pass. Insert
        // at the same partition the playback thread seeks with: after every
        // past event, before the first event at/after the position — so real
        // events at the exact play time land after (and override) the chase.
        let start_us = self.play_us;
        let chase: Vec<(u64, usize, Vec<u8>)> = self
            .doc(|d| d.chase_events(start_us))
            .into_iter()
            .filter(|(_, tr, _)| audible(*tr))
            .filter_map(|(us, tr, b)| sink_of.get(&dest_of(tr)).map(|&s| (us, s, b)))
            .collect();
        let at = events.partition_point(|e| e.0 < start_us);
        events.splice(at..at, chase);
        // opt-in SysEx chase, spliced BEFORE the channel chase so a chased
        // reset cannot wipe the program/CC state the channel chase restores
        if chase_sysex {
            let sx: Vec<(u64, usize, Vec<u8>)> = self
                .doc(|d| d.chase_sysex(start_us))
                .into_iter()
                .filter(|(_, tr, _)| audible(*tr))
                .filter_map(|(us, tr, b)| sink_of.get(&dest_of(tr)).map(|&s| (us, s, b)))
                .collect();
            events.splice(at..at, sx);
        }
        self.loop_start_us = self.play_us;
        self.playback = Some(Playback::start(
            sinks,
            events,
            self.play_us,
            loop_enabled.then_some(self.loop_start_us),
        ));
    }

    fn stop_playback(&mut self) {
        self.play_pending = false;
        if let Some(mut p) = self.playback.take() {
            self.play_us = p.position_us();
            p.stop();
        }
        // silence every warm instance but keep it loaded — the next Play
        // (and parameter edits made meanwhile) start instantly
        for slot in self.plugin_slots.values() {
            let mut s = slot.sink.clone();
            s.panic();
            if let Ok(mut pl) = slot.plugin.try_lock() {
                let _ = pl.set_playing(false);
            }
        }
        self.finish_record();
    }

    /// (bpm, sig_num, sig_den) of the document's head — the transport state
    /// a hosted plugin should see.
    fn transport_hints(&self) -> (f64, i32, i32) {
        self.doc(|d| {
            let bpm = d
                .tempo_map
                .points()
                .first()
                .map(|(_, mpq, _)| 60_000_000.0 / (*mpq).max(1) as f64)
                .unwrap_or(120.0);
            let mut sig = (4i32, 4i32);
            'find: for tr in &d.tracks {
                for e in &tr.events {
                    if let EventKind::Meta {
                        meta_type: 0x58,
                        data,
                    } = &e.kind
                    {
                        if data.len() >= 2 {
                            sig = (data[0] as i32, 1i32 << (data[1] & 0x1f));
                            break 'find;
                        }
                    }
                }
            }
            (bpm, sig.0, sig.1)
        })
    }

    /// Arm/disarm live capture from the first MIDI input port onto the
    /// selected track. Arm also starts playback so timing is audible; a
    /// second press commits the take as one undoable transaction.
    fn toggle_record(&mut self) {
        if self.rec.is_some() {
            self.finish_record();
            return;
        }
        let buf = std::sync::Arc::new(Mutex::new(Vec::new()));
        let buf2 = buf.clone();
        // optional one-bar count-in: capture starts after it elapses
        let cin_us = if self.count_in {
            self.doc(|d| d.tempo_map.tick_to_us(self.ppq() * 4))
        } else {
            0
        };
        let cb = move |us, b: &[u8]| {
            buf2.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((us, b.to_vec()));
        };
        let opened = if self.midi_in.is_empty() {
            midi_io::Input::open(0, cb)
        } else {
            midi_io::Input::open_named(&self.midi_in, cb)
        };
        match opened {
            Ok(input) => {
                self.rec = Some(Rec {
                    _input: input,
                    buf,
                    base_us: self.play_us,
                    cin_us,
                });
                if self.playback.is_none() {
                    self.start_playback();
                }
                self.status = tf(
                    "status.rec_armed",
                    &[("n", &(self.sel_track + 1).to_string())],
                )
                .into();
            }
            Err(e) => self.status = format!("rec: {e}").into(),
        }
    }

    /// Commit the captured take into the selected track (raw channel events;
    /// the notes() view pairs on/off for display).
    fn finish_record(&mut self) {
        let Some(rec) = self.rec.take() else {
            return;
        };
        let msgs = std::mem::take(&mut *rec.buf.lock().unwrap_or_else(|e| e.into_inner()));
        let mut sh = lock_shared(&self.shared);
        if sh.doc.tracks.is_empty() {
            let ops = sh.doc.add_track_ops(None);
            if let Err(e) = sh.apply("add track", ops) {
                drop(sh);
                self.status = tf("status.apply_failed", &[("e", &e.to_string())]).into();
                return;
            }
        }
        let track = self.sel_track.min(sh.doc.tracks.len().saturating_sub(1));
        let mut events = Vec::new();
        for (us, b) in msgs {
            // channel voice messages only; realtime/sysex are not captured
            if b.is_empty() || b[0] < 0x80 || b[0] >= 0xF0 {
                continue;
            }
            let len = match b[0] & 0xF0 {
                0xC0 | 0xD0 => 1,
                _ => 2,
            };
            if b.len() < 1 + len as usize {
                continue;
            }
            if us < rec.cin_us {
                continue;
            }
            let tick = sh.doc.tempo_map.us_to_tick(rec.base_us + (us - rec.cin_us));
            events.push(document::Event {
                id: sh.doc.alloc_event_id(),
                tick,
                seq: u32::MAX / 2,
                raw_body: None,
                kind: EventKind::Channel {
                    status: b[0],
                    data: [b[1], b.get(2).copied().unwrap_or(0)],
                    len,
                },
            });
        }
        let n = events.len();
        drop(sh);
        if n == 0 {
            self.status = t("status.rec_no_events").into();
            return;
        }
        self.apply_tx("record", vec![Op::InsertEvents { track, events }]);
        self.status = tf("status.rec_done", &[("n", &n.to_string())]).into();
    }

    fn rescan_plugins(&mut self) {
        self.status = t("status.scanning").into();
        let (tx, rx) = std::sync::mpsc::channel();
        self.scan_rx = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(output::discover_plugins());
        });
    }

    fn apply_catalog(&mut self, report: output::ScanReport) {
        let skipped = report.skipped.len();
        self.scan_probe_used = Some(report.probe_used);
        self.plugin_meta = report
            .plugins
            .iter()
            .cloned()
            .map(|p| (p.path.to_string_lossy().into_owned(), p))
            .collect();
        self.scan_note = if skipped == 0 {
            None
        } else {
            Some(
                report
                    .skipped
                    .iter()
                    .map(|(p, r)| {
                        format!(
                            "{} — {}",
                            p.file_stem()
                                .map(|s| s.to_string_lossy())
                                .unwrap_or_default(),
                            r
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; "),
            )
        };
        let fresh = build_dest_catalog(&report.plugins);
        let mut sh = lock_shared(&self.shared);
        if fresh == sh.dests {
            let ns = sh.dests.len().to_string();
            drop(sh);
            self.status = tf("status.rescan", &[("n", ns.as_str())]).into();
            return;
        }
        let old_default = sh.dests.get(sh.default_dest).map(|(_, d)| d.clone());
        let old_tracks: Vec<(usize, midi_io::Destination)> = sh
            .track_dest
            .iter()
            .filter_map(|(t, i)| sh.dests.get(*i).map(|(_, d)| (*t, d.clone())))
            .collect();
        sh.dests = fresh;
        sh.default_dest = old_default
            .and_then(|d| sh.dests.iter().position(|(_, dd)| *dd == d))
            .unwrap_or(0);
        sh.track_dest = old_tracks
            .into_iter()
            .filter_map(|(t, d)| sh.dests.iter().position(|(_, dd)| *dd == d).map(|i| (t, i)))
            .collect();
        let n = sh.dests.len();
        drop(sh);
        if let Some(mut pw) = self.plugin_window.take() {
            pw.close();
        }
        self.editor_plugin = None;
        // dest indices were just remapped — every slot is stale
        self.plugin_slots.clear();
        self.plugin_state.clear();
        let _ = self.plugin_req.send(output::PluginReq::Clear);
        self.refresh_plugins();
        let ns = n.to_string();
        self.status = tf("status.rescan", &[("n", ns.as_str())]).into();
    }

    /// Toggle the selected destination's plugin editor. Process-isolated
    /// plugins cannot host a GUI on Windows (the helper's GUI loop is
    /// macOS-only), so the editor always loads a separate in-process
    /// instance inside a `PluginWindow` — a standalone Win32 window that
    /// hosts the plugin's editor view.
    fn open_plugin_gui(&mut self) {
        if let Some(mut pw) = self.plugin_window.take() {
            pw.close();
            self.sync_editor_state_into_slot();
            self.editor_plugin = None;
            return;
        }
        let (d, path) = {
            let sh = lock_shared(&self.shared);
            let d = sh.dest_of(self.sel_track);
            let p = sh.dests.get(d).and_then(|(_, dd)| match dd {
                output::Destination::Plugin { plugin_path } => Some(PathBuf::from(plugin_path)),
                _ => None,
            });
            (d, p)
        };
        let Some(path) = path else { return };
        match output::load_for_gui(&path) {
            Ok(a) => {
                // adopt the live state of the playing instance so the editor
                // shows what's actually being heard
                if let Some(slot) = self.plugin_slots.get(&d).filter(|s| s.path == path) {
                    let state = slot.plugin.lock().ok().and_then(|p| p.save_state().ok());
                    if let Some(data) = state {
                        if let Ok(mut e) = a.lock() {
                            let _ = e.load_state(&data);
                        }
                    }
                }
                let mut pw = vst3_host::PluginWindow::new(a.clone());
                match pw.open() {
                    Ok(()) => {
                        self.plugin_window = Some(pw);
                        self.editor_plugin = Some((d, a));
                    }
                    Err(e) => {
                        self.status = tf("plugin.gui_open_failed", &[("e", &e.to_string())]).into()
                    }
                }
            }
            Err(e) => self.status = format!("{}: {e}", t("plugin.gui_failed")).into(),
        }
    }

    /// Push the in-process editor's full state into the playing instance —
    /// covers program/bank changes parameter-edit draining can't see.
    fn sync_editor_state_into_slot(&mut self) {
        let Some((d, editor)) = self.editor_plugin.take() else {
            return;
        };
        let Some(slot) = self.plugin_slots.get(&d) else {
            return;
        };
        let data = editor.lock().ok().and_then(|e| e.save_state().ok());
        if let Some(data) = data {
            if let Ok(mut p) = slot.plugin.lock() {
                let _ = p.load_state(&data);
            }
        }
    }

    /// tick,key under a window-space mouse position
    fn hit(&self, pos: Point<Pixels>) -> (i64, i32) {
        let b = self.roll_bounds.get();
        let x = f32::from(pos.x) - f32::from(b.origin.x);
        let y = f32::from(pos.y) - f32::from(b.origin.y);
        roll_hit(x, y, self.scroll_x, self.scroll_y, self.zoom)
    }

    /// Center the timeline view on the minimap position under window-x
    /// (click or drag on the overview strip).
    fn seek_minimap(&mut self, window_x: f32) {
        let b = self.mini_bounds.get();
        let w = f32::from(b.size.width);
        if w <= 0.0 {
            return;
        }
        let frac = ((window_x - f32::from(b.origin.x)) / w).clamp(0.0, 1.0);
        let t = frac * self.doc_end_ticks() as f32;
        let vw = f32::from(self.roll_bounds.get().size.width) / self.zoom;
        self.scroll_x = ((t - vw / 2.0) * self.zoom).max(0.0);
        self.clamp_scroll();
    }

    /// Recompute the active drag's deltas from the last cursor position.
    /// Called from mouse-move, and again after edge auto-scroll shifts the
    /// view under a stationary cursor — deltas are cursor-relative, so they
    /// change as the scroll offset does.
    fn update_drag(&mut self) {
        let Some(pos) = self.mouse_pos else { return };
        let Some(mode) = self.drag.as_ref().map(|d| d.mode) else {
            return;
        };
        match mode {
            DragMode::Velocity | DragMode::LaneEvent => {
                let b = self.lane_bounds.get();
                let y = f32::from(pos.y) - f32::from(b.origin.y);
                let h = f32::from(b.size.height).max(1.0);
                let vrange = match self.lane_mode {
                    LaneMode::PitchBend => 16383.0,
                    _ => 127.0,
                };
                let val = ((1.0 - y / h) * vrange) as i32;
                if let Some(d) = self.drag.as_mut() {
                    d.dkey = val;
                }
            }
            _ => {
                let (tick, key) = self.hit(pos);
                // erase stroke: every note swept joins the pending delete set
                let erase_id = (mode == DragMode::Erase)
                    .then(|| self.note_at(pos).or_else(|| self.edge_at(pos)))
                    .flatten()
                    .map(|n| n.on_id);
                if let Some(d) = self.drag.as_mut() {
                    match d.mode {
                        DragMode::Move | DragMode::Duplicate => {
                            let (dt, dk) = clamp_move_delta(
                                tick - d.orig_start as i64,
                                d.orig_start,
                                key - d.orig_key as i32,
                                d.orig_key,
                            );
                            d.dtick = dt;
                            d.dkey = dk;
                        }
                        DragMode::Resize => {
                            d.dtick = tick - d.orig_end.unwrap_or(d.orig_start) as i64;
                        }
                        DragMode::Marquee => {
                            d.b_tick = tick;
                            d.b_key = key;
                        }
                        _ => {}
                    }
                }
                if let Some(id) = erase_id {
                    self.erase_ids.insert(id);
                }
            }
        }
    }

    /// While a drag is active and the cursor rests near a canvas edge, pan
    /// the view a step per animation frame (classic DAW edge-scroll). Returns
    /// whether the view moved, so the caller knows to keep animating.
    fn drag_auto_pan(&mut self) -> bool {
        const EDGE: f32 = 28.0;
        const SPEED: f32 = 14.0;
        const SLOP: f32 = 96.0;
        let Some(mode) = self.drag.as_ref().map(|d| d.mode) else {
            return false;
        };
        let Some(pos) = self.mouse_pos else {
            return false;
        };
        let b = self.roll_bounds.get();
        let (x, y) = (f32::from(pos.x), f32::from(pos.y));
        let (bx, by) = (f32::from(b.origin.x), f32::from(b.origin.y));
        let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
        if w <= 0.0 || h <= 0.0 {
            return false;
        }
        let mut dx = 0.0f32;
        let mut dy = 0.0f32;
        if x < bx + EDGE && x > bx - SLOP {
            dx = -SPEED;
        }
        if x > bx + w - EDGE && x < bx + w + SLOP {
            dx = SPEED;
        }
        // vertical panning only makes sense for roll-space drags — lane
        // drags map the y axis to a value, not to pitch
        if !matches!(mode, DragMode::Velocity | DragMode::LaneEvent) {
            if y < by + EDGE && y > by - SLOP {
                dy = -SPEED;
            }
            if y > by + h - EDGE && y < by + h + SLOP {
                dy = SPEED;
            }
        }
        if dx == 0.0 && dy == 0.0 {
            return false;
        }
        let before = (self.scroll_x, self.scroll_y);
        self.scroll_x = (self.scroll_x + dx).max(0.0);
        self.scroll_y = (self.scroll_y + dy).max(0.0);
        self.clamp_scroll();
        let moved = (self.scroll_x, self.scroll_y) != before;
        if moved {
            self.update_drag();
        }
        moved
    }

    fn note_at(&self, pos: Point<Pixels>) -> Option<Note> {
        let (tick, key) = self.hit(pos);
        self.notes
            .iter()
            .rev()
            .find(|n| {
                n.key as i32 == key
                    && tick >= n.start_tick as i64
                    && tick <= n.end_tick.unwrap_or(n.start_tick + self.ppq() / 4) as i64
            })
            .cloned()
    }

    /// Note whose right edge is within ~6px of `pos` — a resize target.
    fn edge_at(&self, pos: Point<Pixels>) -> Option<Note> {
        let (tick, key) = self.hit(pos);
        self.notes
            .iter()
            .rev()
            .find(|n| {
                n.key as i32 == key
                    && n.end_tick.is_some()
                    && tick >= n.start_tick as i64
                    && ((n.end_tick.unwrap() as i64) - tick) as f32 * self.zoom <= 6.0
                    && ((n.end_tick.unwrap() as i64) - tick) as f32 * self.zoom >= -2.0
            })
            .cloned()
    }

    #[allow(dead_code)]
    fn button(
        label: &'static str,
        cx: &Context<Self>,
        on: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        div()
            .id(label)
            .px_2()
            .py_1()
            .rounded_sm()
            .bg(rgb(0x2a2a35))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(0x3a3a48)))
            .child(t(label))
            .on_click(cx.listener(move |this, _ev, _w, cx| on(this, cx)))
    }

    /// Small chip with a literal label (symbols/numbers need no i18n key).
    /// `on` receives the ClickEvent so chips can honour Shift=×10 etc.
    fn chip(
        id: &'static str,
        label: impl Into<SharedString>,
        cx: &mut Context<Self>,
        on: impl Fn(&mut Self, &ClickEvent, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .px_2()
            .py_1()
            .rounded_sm()
            .bg(rgb(0x2a2a35))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(0x3a3a48)))
            .text_color(rgb(0x9fd0ff))
            .text_size(px(11.0))
            .child(label.into())
            .on_click(cx.listener(move |this, ev, _w, cx| on(this, ev, cx)))
    }
}

fn load_document(path: &std::path::Path) -> Result<(Document, Vec<String>), String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let file = smf_core::parse(&bytes).map_err(|e| e.to_string())?;
    let warnings = file.warnings.clone();
    Ok((Document::from_file(file), warnings))
}

/// App-wide preferences: recent files + record count-in + recording source.
/// Stored at %APPDATA%/midi-editor/prefs.json (unlike the per-song sidecar).
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct GlobalPrefs {
    recent: Vec<String>,
    count_in: bool,
    /// MIDI input port name to record from; empty = first available port
    midi_in: String,
}

impl GlobalPrefs {
    fn path() -> PathBuf {
        let base = std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir());
        base.join("midi-editor")
    }

    fn load() -> Self {
        std::fs::read_to_string(Self::path().join("prefs.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        let dir = Self::path();
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(dir.join("prefs.json"), s);
        }
    }
}

/// Session state that cannot live inside the SMF: per-track output
/// assignments (by stable destination identity, not runtime index), mute/solo,
/// metronome/loop, view transform. Written next to the document as
/// `song.mid.editor.json`.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Prefs {
    default_dest: Option<output::Destination>,
    track_dest: HashMap<usize, output::Destination>,
    muted: Vec<usize>,
    soloed: Vec<usize>,
    metronome: bool,
    loop_enabled: bool,
    /// None in old sidecars = keep the default (off)
    chase_sysex: Option<bool>,
    zoom: Option<f32>,
    scroll_x: Option<f32>,
    scroll_y: Option<f32>,
    sel_track: Option<usize>,
    enc: Option<String>,
    lane: Option<String>,
    show_events: Option<bool>,
    tool: Option<String>,
    snap: Option<usize>,
}

fn prefs_path(doc_path: &std::path::Path) -> PathBuf {
    PathBuf::from(format!("{}.editor.json", doc_path.display()))
}

/// Display label for a destination identity (sidecar paths -> stem).
fn dest_label(d: &output::Destination) -> String {
    match d {
        output::Destination::MidiPort { port_name } => port_name.clone(),
        output::Destination::Plugin { plugin_path } => {
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
    fn resolve_dest(&mut self, d: &output::Destination) -> usize {
        lock_shared(&self.shared).ensure_dest(&dest_label(d), d.clone())
    }

    fn apply_prefs(&mut self, doc_path: &std::path::Path) {
        let Ok(text) = std::fs::read_to_string(prefs_path(doc_path)) else {
            return;
        };
        let Ok(p) = serde_json::from_str::<Prefs>(&text) else {
            return;
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
        {
            let mut sh = lock_shared(&self.shared);
            for (t, d) in overrides {
                sh.track_dest.insert(t, d);
            }
            sh.muted = p.muted.into_iter().collect();
            sh.soloed = p.soloed.into_iter().collect();
            sh.metronome = p.metronome;
            sh.loop_enabled = p.loop_enabled;
            if let Some(c) = p.chase_sysex {
                sh.chase_sysex = c;
            }
        }
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
        self.enc_override = p.enc.as_deref().map(|e| match e {
            "utf8" => smf_core::TextEncoding::Utf8,
            "sjis" => smf_core::TextEncoding::ShiftJis,
            _ => smf_core::TextEncoding::Latin1,
        });
        self.lane_mode = match p.lane.as_deref() {
            Some("pb") => LaneMode::PitchBend,
            Some(s) if s.starts_with("cc") => s[2..]
                .parse()
                .map(LaneMode::CC)
                .unwrap_or(LaneMode::Velocity),
            _ => LaneMode::Velocity,
        };
        if let Some(v) = p.show_events {
            self.show_events = v;
        }
        self.tool = match p.tool.as_deref() {
            Some("draw") => Tool::Draw,
            Some("erase") => Tool::Erase,
            _ => Tool::Select,
        };
        if let Some(i) = p.snap {
            self.snap_idx = i.min(SNAPS.len() - 1);
        }
        // start warming any VST3 destinations the prefs just restored
        self.refresh_plugins();
    }

    /// MRU update + persist to the app-wide prefs file.
    fn push_recent(&mut self, path: &std::path::Path) {
        let s = path.to_string_lossy().into_owned();
        self.recent.retain(|r| r.as_str() != s);
        self.recent.insert(0, s.as_str().into());
        self.recent.truncate(10);
        self.save_global();
    }

    fn save_global(&self) {
        GlobalPrefs {
            recent: self.recent.iter().map(|r| r.to_string()).collect(),
            count_in: self.count_in,
            midi_in: self.midi_in.to_string(),
        }
        .save();
    }

    fn persist(&self) {
        let sh = lock_shared(&self.shared);
        let Some(path) = sh.path.clone() else {
            return;
        };
        let prefs = Prefs {
            default_dest: sh.dests.get(sh.default_dest).map(|(_, d)| d.clone()),
            track_dest: sh
                .track_dest
                .iter()
                .filter_map(|(t, d)| sh.dests.get(*d).map(|(_, dest)| (*t, dest.clone())))
                .collect(),
            muted: sh.muted.iter().copied().collect(),
            soloed: sh.soloed.iter().copied().collect(),
            metronome: sh.metronome,
            loop_enabled: sh.loop_enabled,
            chase_sysex: Some(sh.chase_sysex),
            zoom: Some(self.zoom),
            scroll_x: Some(self.scroll_x),
            scroll_y: Some(self.scroll_y),
            sel_track: Some(self.sel_track),
            enc: self.enc_override.map(|e| {
                match e {
                    smf_core::TextEncoding::Utf8 => "utf8",
                    smf_core::TextEncoding::ShiftJis => "sjis",
                    smf_core::TextEncoding::Latin1 => "latin1",
                }
                .to_string()
            }),
            lane: Some(match self.lane_mode {
                LaneMode::Velocity => "vel".into(),
                LaneMode::CC(c) => format!("cc{c}"),
                LaneMode::PitchBend => "pb".into(),
            }),
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
        };
        if let Ok(text) = serde_json::to_string_pretty(&prefs) {
            let _ = std::fs::write(prefs_path(&path), text);
        }
    }
}

fn main() {
    output::init_env();
    std::panic::set_hook(Box::new(|i| eprintln!("panic: {i}")));
    let path = std::env::args().nth(1).map(PathBuf::from);
    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        // dark UI — gpui-component's default theme follows the OS and renders
        // the text input's selection overlay white; pin dark explicitly
        gpui_kit::component::theme::Theme::change(
            gpui_kit::component::theme::ThemeMode::Dark,
            None,
            cx,
        );
        let path = path.clone();
        cx.spawn(async move |cx| {
            cx.open_window(WindowOptions::default(), move |window, cx| {
                let input =
                    cx.new(|cx| InputState::new(window, cx).placeholder(t("field.track_name")));
                let view = cx.new(|cx| {
                    let v = EditorView::new(path.clone(), input, cx);
                    spawn_mcp(v.shared.clone());
                    spawn_doc_watch(cx, v.shared.clone());
                    window.focus(&v.focus.clone(), cx);
                    v
                });
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("failed to open window");
        })
        .detach();
    });
}

/// In-app MCP server: Streamable-HTTP on 127.0.0.1:7878/mcp on its own
/// tokio runtime thread. Bearer token = MIDI_MCP_TOKEN env (unset = open on
/// loopback only). `mcp-bridge` is the stdio frontend for stdio-only clients.
fn spawn_mcp(shared: SharedDoc) {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("mcp http: failed to build tokio runtime: {e}");
                return;
            }
        };
        rt.block_on(async move {
            let token = std::env::var("MIDI_MCP_TOKEN").ok();
            if let Err(e) = mcp_server::serve_http(shared, "127.0.0.1:7878", token).await {
                eprintln!("mcp http: {e}");
            }
        });
    });
}

/// Poll the shared doc's notify counter so MCP-driven edits repaint the UI
/// even while the user is idle.
fn spawn_doc_watch(cx: &mut Context<EditorView>, shared: SharedDoc) {
    cx.spawn(async move |this, cx| {
        let mut last = 0u64;
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(150))
                .await;
            let (cur, reqs) = {
                let mut sh = lock_shared(&shared);
                (
                    sh.gui_notify.load(std::sync::atomic::Ordering::Relaxed),
                    std::mem::take(&mut sh.transport_req),
                )
            };
            let dirty = cur != last || !reqs.is_empty();
            if cur != last {
                last = cur;
            }
            if let Some(this) = this.upgrade() {
                this.update(cx, |v, cx| {
                    // MCP transport requests -> real playback actions
                    for r in reqs {
                        match r {
                            mcp_server::TransportReq::Play if v.playback.is_none() => {
                                v.start_playback();
                            }
                            mcp_server::TransportReq::Stop => v.stop_playback(),
                            mcp_server::TransportReq::Seek { tick } => {
                                v.play_us = v.doc(|d| d.tempo_map.tick_to_us(tick));
                                if v.playback.is_some() {
                                    v.stop_playback();
                                    v.start_playback();
                                }
                            }
                            _ => {}
                        }
                    }
                    let plugin_changed = v.poll_plugin_events();
                    if let Some(rx) = &v.scan_rx {
                        if let Ok(report) = rx.try_recv() {
                            v.scan_rx = None;
                            v.apply_catalog(report);
                            cx.notify();
                        }
                    }
                    if dirty {
                        v.refresh_derived();
                        // cover routing changes that came from MCP tools —
                        // also warms any newly-assigned VST3 destination
                        v.persist();
                        v.refresh_plugins();
                    }
                    // repaint while playing so the playhead/counter advance;
                    // also while a plugin editor is open so its native event
                    // queue gets serviced even when the app is idle
                    // keep repainting while a plugin editor is open so its
                    // platform events get pumped and user-close is noticed
                    if let Some(pw) = &v.plugin_window {
                        let _ = pw.service_platform_events();
                        // live param sync: forward the editor's edits into
                        // the playing instance (best-effort each tick)
                        if let Some((d, editor)) = &v.editor_plugin {
                            let edits = editor
                                .lock()
                                .map(|mut e| e.take_parameter_edits())
                                .unwrap_or_default();
                            if !edits.is_empty() {
                                if let Some(slot) = v.plugin_slots.get(d) {
                                    if let Ok(mut p) = slot.plugin.try_lock() {
                                        for ed in edits {
                                            if let Some(val) = ed.value {
                                                let _ = p.set_parameter(ed.id, val);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if pw.closed_by_user() {
                            v.plugin_window = None;
                            v.sync_editor_state_into_slot();
                        }
                    }
                    if dirty || plugin_changed || v.playback.is_some() || v.plugin_window.is_some()
                    {
                        cx.notify();
                    }
                });
            } else {
                break;
            }
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use crate::{empty_doc, plugin_plan, PluginPlan, PluginState};
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    /// Every freshly parsed document reports revision 0, so the derived-view
    /// caches must not key on the revision alone: before `doc_epoch` existed,
    /// opening a file right after an untouched document (also revision 0)
    /// skipped the cache rebuild and the roll stayed empty until the next
    /// edit happened to bump the revision.
    #[test]
    fn fresh_documents_share_revision_zero() {
        assert_eq!(empty_doc().revision(), empty_doc().revision());
    }

    fn loading(path: &Path) -> PluginState {
        PluginState::Loading {
            path: path.to_path_buf(),
            since: Instant::now(),
        }
    }

    fn failed(path: &Path) -> PluginState {
        PluginState::Failed {
            path: path.to_path_buf(),
            phase: "load",
            msg: String::new(),
        }
    }

    fn ready(path: &Path) -> PluginState {
        PluginState::Ready {
            path: path.to_path_buf(),
        }
    }

    /// Host lifecycle ordering for one destination index: the wanted bundle
    /// stays resident, duplicate loads are suppressed while one is in flight,
    /// and a re-pointed index retires its stale instance before opening the
    /// next — the slot never holds two plugins at once.
    #[test]
    fn plugin_plan_lifecycle_ordering() {
        let a = PathBuf::from(r"C:\VST3\A.vst3");
        let b = PathBuf::from(r"C:\VST3\B.vst3");

        // fresh index, nothing resident → load
        assert_eq!(
            plugin_plan(None, None, &a, false),
            PluginPlan::Open { retire: false }
        );

        // already loaded + resident → untouched
        assert_eq!(
            plugin_plan(Some(&ready(&a)), Some(&a), &a, false),
            PluginPlan::Satisfied
        );

        // state says Ready but the slot is empty (lost instance) → reload
        assert_eq!(
            plugin_plan(Some(&ready(&a)), None, &a, false),
            PluginPlan::Open { retire: false }
        );

        // resident but state lost its Ready marker → leave alone
        assert_eq!(
            plugin_plan(Some(&failed(&a)), Some(&a), &a, false),
            PluginPlan::Satisfied
        );

        // same bundle already loading → don't double-load
        assert_eq!(
            plugin_plan(Some(&loading(&a)), None, &a, false),
            PluginPlan::Wait
        );

        // failed load of the same bundle is sticky until a forced retry
        assert_eq!(
            plugin_plan(Some(&failed(&a)), None, &a, false),
            PluginPlan::Wait
        );
        assert_eq!(
            plugin_plan(Some(&failed(&a)), None, &a, true),
            PluginPlan::Open { retire: false }
        );

        // destination re-pointed while something else is resident → the old
        // instance is retired before the new one opens
        assert_eq!(
            plugin_plan(Some(&ready(&b)), Some(&b), &a, false),
            PluginPlan::Open { retire: true }
        );
        assert_eq!(
            plugin_plan(Some(&loading(&b)), Some(&b), &a, false),
            PluginPlan::Open { retire: true }
        );
    }
}
