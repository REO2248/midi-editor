//! Phase 1: real SMF document in a modern-editor shell.
//! Open/save .mid, piano roll + event list views, playback to a MIDI port,
//! basic editing (draw / drag / delete) all going through
//! `Document::apply(Transaction)` so undo is shared with MCP edits.

mod a11y;
mod audition;
mod cmd;
mod diagnostics;
mod geometry;
mod guard;
mod i18n;
mod icons;
mod menu;
mod plugin_state;
mod recovery;
mod render;
mod shutdown;
mod theme;
mod watch;

use audition::Audition;
use geometry::{
    clamp_move_delta, clamp_span, content_view, reanchor, roll_hit, ZOOM_MAX, ZOOM_MIN,
};
use i18n::{t, tf};
use menu::{menu_x, next_selectable, row_y, MenuRow, MENUS};

use commands::UndoStack;
use document::{Document, Event as DocEvent, EventId, Note, Op, TimeDisplay};
use gpui_kit::base::{ObservedElement, TestSupportExt};
use gpui_kit::component::input::InputState;
use gpui_kit::component::Root;
use gpui_kit::*;
use mcp_server::{Shared, SharedDoc};
use midi_io::{EventSink, Playback, PortSink};
use smf_core::Division;
use smf_core::EventKind;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;

const NOTE_H: f32 = 13.0;
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
    /// lane drag editing a CC/PB/AT event (`on_id` = event id; `dkey` = value)
    LaneEvent,
    /// rubber-band select inside a lane (tick × value box → `lane_sel`)
    LaneMarquee,
    /// drag on a lane's header strip resizes that lane's body height
    LaneResize,
    /// alt-drag: copy the selection instead of moving it
    Duplicate,
    /// erase tool: every note touched joins `erase_ids`, deleted on commit
    Erase,
}

/// A document-replacing action parked behind the discard guard
/// (`guard.rs`). `CloseWindow` covers window close and, transitively, app
/// quit — quitting always goes through closing the last window.
#[derive(Clone)]
pub(crate) enum PendingAction {
    NewFile,
    /// show the open-file dialog once the guard passes
    OpenDialog,
    /// open this path (recent menu, drag/drop, a future CLI hand-off)
    OpenPath(PathBuf),
    /// remove the editor window
    CloseWindow,
}

/// Keyboard-focusable regions. Each maps to a `FocusHandle` tracked by its
/// container div, so GPUI dispatch, Tab traversal and focus rings are native.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FocusArea {
    MenuBar,
    Tracks,
    Roll,
    Lane,
    Events,
}

impl FocusArea {
    fn key(self) -> &'static str {
        match self {
            FocusArea::MenuBar => "focus.menubar",
            FocusArea::Tracks => "focus.tracks",
            FocusArea::Roll => "focus.roll",
            FocusArea::Lane => "focus.lane",
            FocusArea::Events => "focus.events",
        }
    }
}

/// One event-list row: display text plus the event's tick for seek-on-Enter.
#[derive(Clone)]
struct EvRow {
    tick: u64,
    text: SharedString,
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

impl TopMenu {
    fn key(self) -> &'static str {
        match self {
            TopMenu::File => "menu.file",
            TopMenu::Edit => "menu.edit",
            TopMenu::View => "menu.view",
            TopMenu::Track => "menu.track",
            TopMenu::Output => "menu.output",
            TopMenu::Transport => "menu.transport",
            TopMenu::Help => "menu.help",
        }
    }
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
    RelSet,
    Oct,
    AudVel,
    AudDur,
    Theme,
    /// View → row height (vertical zoom preset)
    RowH,
    /// View → scale highlight root
    Scale,
    /// Edit → legato gap/overlap presets
    LegatoGap,
    /// Edit → swing amount presets
    Swing,
    Meta,
}

#[derive(Clone, Copy, PartialEq)]
enum DestPick {
    Track,
    Default,
}

/// How the timeline tracks the playhead while the transport runs.
#[derive(Clone, Copy, PartialEq)]
enum Follow {
    /// viewport stays put
    Off,
    /// jump a viewport ahead when the playhead walks off the right edge
    Page,
    /// keep the playhead pinned ~1/3 from the left edge
    Smooth,
}

/// Follow is suspended this long after a manual scroll so it never fights
/// the user's own pan (any follow/nav command resumes it immediately).
const FOLLOW_HOLD: std::time::Duration = std::time::Duration::from_secs(4);

/// What a bottom lane edits for the selected track.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum LaneMode {
    Velocity,
    /// Control Change lane, controller number in the field
    CC(u8),
    PitchBend,
    /// channel pressure (aftertouch, 0xD0)
    ChanAT,
    /// polyphonic key pressure (0xA0); the key filter lives in `poly_key`
    PolyAT,
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
            LaneMode::PitchBend => LaneMode::ChanAT,
            LaneMode::ChanAT => LaneMode::PolyAT,
            LaneMode::PolyAT => LaneMode::Velocity,
        }
    }
    fn label(self) -> String {
        match self {
            LaneMode::Velocity => "Vel".to_string(),
            LaneMode::CC(n) => format!("CC{n}"),
            LaneMode::PitchBend => "PB".to_string(),
            LaneMode::ChanAT => "CAT".to_string(),
            LaneMode::PolyAT => "PAT".to_string(),
        }
    }
    /// full-scale value for the lane's y axis
    fn vrange(self) -> f32 {
        match self {
            LaneMode::PitchBend => 16383.0,
            _ => 127.0,
        }
    }
    /// does this lane edit events (vs. velocity bars on the note view)
    fn is_event_lane(self) -> bool {
        !matches!(self, LaneMode::Velocity)
    }
}

/// Modes offered by the View > Lane menu and the add-lane picker.
const LANE_MODES: [LaneMode; 7] = [
    LaneMode::Velocity,
    LaneMode::CC(1),
    LaneMode::CC(7),
    LaneMode::CC(10),
    LaneMode::CC(11),
    LaneMode::CC(64),
    LaneMode::PitchBend,
];

/// One stacked lane in the bottom strip: the stream it edits, its body
/// height in px, and whether it is collapsed to just its header.
#[derive(Clone, Copy, PartialEq)]
struct LaneCfg {
    mode: LaneMode,
    h: f32,
    collapsed: bool,
    poly_key: Option<u8>,
}

impl Default for LaneCfg {
    fn default() -> Self {
        LaneCfg {
            mode: LaneMode::Velocity,
            h: LANE_H,
            collapsed: false,
            poly_key: None,
        }
    }
}

const LANE_H: f32 = 56.0;
/// header strip shown above every lane (and the only thing shown when
/// the lane is collapsed); it doubles as the drag-to-resize handle
const LANE_HDR: f32 = 16.0;
const LANE_H_MIN: f32 = 24.0;
const LANE_H_MAX: f32 = 220.0;
const LANES_MAX: usize = 8;

fn lane_mode_str(m: LaneMode) -> String {
    match m {
        LaneMode::Velocity => "vel".into(),
        LaneMode::CC(c) => format!("cc{c}"),
        LaneMode::PitchBend => "pb".into(),
        LaneMode::ChanAT => "cat".into(),
        LaneMode::PolyAT => "pat".into(),
    }
}

fn lane_mode_parse(s: &str) -> LaneMode {
    match s {
        "pb" => LaneMode::PitchBend,
        "cat" => LaneMode::ChanAT,
        "pat" => LaneMode::PolyAT,
        s if s.starts_with("cc") => s[2..]
            .parse()
            .map(LaneMode::CC)
            .unwrap_or(LaneMode::Velocity),
        _ => LaneMode::Velocity,
    }
}

/// One clipboard note — tick offset from the copy anchor.
#[derive(Clone)]
struct ClipNote {
    dtick: i64,
    key: u8,
    len: u64,
    vel: u8,
    /// release velocity + wire form carried through copy/paste/duplicate
    /// so a copied note doesn't flatten its release to 0x80 vel-0
    off_vel: u8,
    off_via_on: bool,
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

/// Vertical zoom bounds for the piano-key row height (px).
const NOTE_H_MIN: f32 = 4.0;
const NOTE_H_MAX: f32 = 40.0;

/// GM drum-kit names for keys 35..=81 — the drum view labels folded rows
/// with these instead of pitch names.
const GM_DRUMS: [&str; 47] = [
    "Acoustic Bass Drum",
    "Bass Drum 1",
    "Side Stick",
    "Acoustic Snare",
    "Hand Clap",
    "Electric Snare",
    "Low Floor Tom",
    "Closed Hi-Hat",
    "High Floor Tom",
    "Pedal Hi-Hat",
    "Low Tom",
    "Open Hi-Hat",
    "Low-Mid Tom",
    "Hi-Mid Tom",
    "Crash Cymbal 1",
    "High Tom",
    "Ride Cymbal 1",
    "Chinese Cymbal",
    "Ride Bell",
    "Tambourine",
    "Splash Cymbal",
    "Cowbell",
    "Crash Cymbal 2",
    "Vibraslap",
    "Ride Cymbal 2",
    "Hi Bongo",
    "Low Bongo",
    "Mute Hi Conga",
    "Open Hi Conga",
    "Low Conga",
    "High Timbale",
    "Low Timbale",
    "High Agogo",
    "Low Agogo",
    "Cabasa",
    "Maracas",
    "Short Whistle",
    "Long Whistle",
    "Short Guiro",
    "Long Guiro",
    "Claves",
    "Hi Wood Block",
    "Low Wood Block",
    "Mute Cuica",
    "Open Cuica",
    "Mute Triangle",
    "Open Triangle",
];

/// GM drum name for a key, if it has one (35..=81).
fn drum_name(key: u8) -> Option<&'static str> {
    (35..=81)
        .contains(&key)
        .then(|| GM_DRUMS[key as usize - 35])
}

/// Identity row map: all 128 keys, highest first (row 0 = key 127).
fn all_keys() -> Vec<u8> {
    (0u8..128).rev().collect()
}

/// Row map for a folded view: only the pitches `notes` actually uses,
/// highest first. `drum` restricts to channel 9 (0-indexed) on the selected
/// track — the percussion view. Empty input falls back to the identity map
/// so the roll is never blank.
fn used_keys(notes: &[Note], drum: bool, sel_track: usize) -> Vec<u8> {
    let mut mask = [false; 128];
    for n in notes {
        if drum && !(n.track == sel_track && n.channel == 9) {
            continue;
        }
        mask[n.key as usize] = true;
    }
    let v: Vec<u8> = (0..128u8).rev().filter(|k| mask[*k as usize]).collect();
    if v.is_empty() {
        all_keys()
    } else {
        v
    }
}

/// Pitch-class membership of a diatonic scale (major or natural minor).
fn scale_pcs_of(root: u8, minor: bool) -> [bool; 12] {
    const MAJ: [u8; 7] = [0, 2, 4, 5, 7, 9, 11];
    const MIN: [u8; 7] = [0, 2, 3, 5, 7, 8, 10];
    let mut out = [false; 12];
    for &iv in if minor { &MIN } else { &MAJ } {
        out[((root + iv) % 12) as usize] = true;
    }
    out
}

/// Tonic pitch class of a 0x59 key signature: sf counts fifths from C
/// (negative = flats), mi selects the relative minor a minor third up.
fn keysig_root(sf: i8, minor: bool) -> u8 {
    let maj = (7 * sf as i32).rem_euclid(12);
    (if minor { maj + 9 } else { maj } % 12) as u8
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
    /// Audition routing captured at drag start (0 where unused): the
    /// velocity and channel preview strikes for this drag use.
    aud_vel: u8,
    aud_ch: u8,
    /// index into `lanes` — only meaningful for lane drags
    lane: usize,
}

/// Document-derived data the chrome (menu bar, marker strip, minimap,
/// transport readouts) shows. Everything here is a pure function of the
/// document revision (+ the text-encoding hint), so it is computed once per
/// edit instead of once per frame.
#[derive(Default)]
struct DocUi {
    /// (tick, event id, track, text) — meta 0x05/0x06 for the timeline strip
    markers: Vec<(u64, EventId, usize, SharedString)>,
    n_diags: usize,
    track_names: Vec<String>,
    track_chs: Vec<u8>,
    sig: String,
    tempo0: f64,
    /// detected GM/GS/XG reset SysEx — a display hint for patch naming
    mode_hint: Option<smf_core::ModeHint>,
    /// last tick with a note — the scrollable extent of the timeline
    song_end: u64,
}

impl DocUi {
    /// `notes` is the already-derived note view for this revision (passed in
    /// so the pairing pass runs once per revision, not once per consumer).
    /// `seq_sel` scopes markers/tempo/sig/song-end to one sequence for
    /// format-2 documents (None = whole document, formats 0/1).
    fn build(
        doc: &Document,
        notes: &[Note],
        enc_override: Option<smf_core::TextEncoding>,
        seq_sel: Option<usize>,
    ) -> Self {
        let hint = enc_override.or_else(|| doc.text_encoding_hint());
        let scan: &[document::Track] = match seq_sel.and_then(|i| doc.tracks.get(i)) {
            Some(t) => std::slice::from_ref(t),
            None => &doc.tracks,
        };
        // meta 0x06/0x05 markers, from any track, at their tick
        let mut markers = Vec::new();
        for (ti, t) in doc.tracks.iter().enumerate() {
            if seq_sel.is_some_and(|si| si != ti) {
                continue;
            }
            for e in &t.events {
                if let EventKind::Meta {
                    meta_type: 0x05 | 0x06,
                    data,
                } = &e.kind
                {
                    markers.push((e.tick, e.id, ti, smf_core::decode_text(data, hint).into()));
                }
            }
        }
        markers.sort_unstable();
        let tempo0 = doc
            .tempo_map_for(seq_sel.unwrap_or(0))
            .points()
            .first()
            .map(|(_, mpq, _)| 60_000_000.0 / *mpq as f64)
            .unwrap_or(120.0);
        let sig = scan
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
            mode_hint: doc.synth_mode(),
            song_end: notes
                .iter()
                .filter(|n| seq_sel.is_none_or(|i| n.track == i))
                .map(|n| n.end_tick.unwrap_or(n.start_tick))
                .max()
                .unwrap_or(0),
        }
    }
}

/// Meta edit dialog target: `id > 0` rewrites that event's bytes (same
/// meta_type), `id == 0` inserts a new meta at (track, tick).
#[derive(Clone, Copy)]
struct MetaEdit {
    track: usize,
    tick: u64,
    meta_type: u8,
    id: EventId,
}

struct EditorView {
    shared: SharedDoc,
    /// Bumped every time the whole document is swapped in (open / new file).
    /// Every freshly parsed file reports revision 0, so caches keyed on the
    /// revision alone cannot tell two documents apart — opening a file after
    /// an untouched one left the roll showing the previous (often empty)
    /// note view. Cache keys are `(doc_epoch, revision)`; event rows and
    /// DocUi add the viewed sequence (format 2 scopes them per sequence).
    doc_epoch: u64,
    notes_key: (u64, u64),
    notes: Arc<Vec<Note>>,
    ev_key: (u64, u64, usize),
    events: Arc<Vec<EvRow>>,
    /// Document-derived UI data (markers, track names, diagnostics count…).
    /// Rebuilt only when the document key or encoding hint changes — render
    /// runs at animation-frame rate during playback and must not rescan
    /// every event each frame.
    doc_ui: Arc<DocUi>,
    doc_ui_key: (u64, u64, usize),
    doc_ui_enc: Option<smf_core::TextEncoding>,
    /// region focus handles — Tab traversal follows DOM order
    menu_fh: FocusHandle,
    tracks_fh: FocusHandle,
    roll_fh: FocusHandle,
    lane_fh: FocusHandle,
    events_fh: FocusHandle,
    /// highlighted menubar label while the bar holds keyboard focus
    menu_bar_sel: usize,
    /// last non-menubar focus region — commands restore focus to it so a
    /// menu round-trip returns the user where they were
    last_area: FocusArea,
    /// keyboard selection inside the open dropdown / cascade (indexes into
    /// `menu_rows` / `sub_rows`)
    menu_sel: Option<usize>,
    sub_sel: Option<usize>,
    /// row model of the open menu / submenu, rebuilt each frame while open
    /// so labels, checks and enabled flags are always live
    menu_rows: Vec<MenuRow>,
    sub_rows: Vec<MenuRow>,
    /// piano-roll edit cursor — used to move/insert when nothing is selected
    cursor_tick: u64,
    cursor_key: i32,
    /// selected event-list row and its scroll position handle
    ev_sel: usize,
    events_scroll: UniformListScrollHandle,
    /// per-(mode, poly key) lane point caches — each entry keys on
    /// epoch + revision + track, so stacked lanes sharing a stream
    /// still reuse one scan. Tuple inside: (id, tick, value, key|-1)
    lane_caches:
        HashMap<(LaneMode, Option<u8>), ((u64, u64), usize, Arc<Vec<(EventId, u64, i32, i32)>>)>,
    /// lane marquee selection — event ids of non-note lane points
    lane_sel: BTreeSet<EventId>,
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
    /// piano-key strip bounds (the clickable keyboard left of the roll)
    kbd_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// seek-ruler strip bounds
    ruler_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// per-lane canvas bounds — same trick for each lane's hit-testing
    lane_bounds: Vec<Rc<Cell<Bounds<Pixels>>>>,
    mini_bounds: Rc<Cell<Bounds<Pixels>>>,
    scroll_x: f32,
    scroll_y: f32,
    zoom: f32,
    /// piano-key row height — the vertical zoom factor (sidecar pref)
    note_h: f32,
    /// fold view: only pitches used by the document get a row
    fold: bool,
    /// drum view: fold to channel-9 pitches of the selected track with GM
    /// drum names (implies fold semantics for percussion editing)
    drum: bool,
    /// scale highlight: -2 = follow the key signature, -1 = off,
    /// 0..=11 = manual root (with `scale_minor` picking the mode)
    scale_sel: i8,
    scale_minor: bool,
    /// visible row→key map (identity = all 128); rebuilt by refresh_derived
    vis_keys: Vec<u8>,
    /// reverse of `vis_keys`: key→row, -1 when folded out
    row_of: [i32; 128],
    /// pitch classes painted as scale-member rows (None = highlight off)
    scale_pcs: Option<[bool; 12]>,
    /// cache key for the view-derived fields above
    view_key: (u64, u64, bool, bool, usize, i8, bool),
    /// key signature hint parsed from meta 0x59 at the playhead
    keysig: Option<(i8, bool)>,
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
    /// in-flight save worker — at most one: a second Ctrl+S while a slow
    /// save runs is ignored rather than queued
    save_rx: Option<std::sync::mpsc::Receiver<Result<mcp_server::service::SaveOutcome, String>>>,
    scan_note: Option<String>,
    scan_probe_used: Option<bool>,
    /// Plugins served from the scan cache in the last scan (status display).
    scan_cached: usize,
    /// Bundles the cache holds as failed (crash/timeout) — shown in the
    /// Output status panel where each can be force-retried.
    quarantined: Vec<(PathBuf, String)>,
    /// Per-plugin probe bound (global pref, seconds).
    probe_timeout_secs: u64,
    host_diag: output::HostDiag,
    show_output_status: bool,
    /// explicit audio configuration for hosted plugins (device/rate/buffer);
    /// persisted in GlobalPrefs — applied to every `PluginReq::Open`
    audio_sel: output::AudioSelection,
    /// dest -> last stream-loss auto-reopen, throttled so a dead device
    /// doesn't hot-loop reopens
    audio_retry: HashMap<usize, std::time::Instant>,
    /// cached output-device names for the audio settings panel (refreshed
    /// every few seconds so hot-plug shows up)
    audio_devices: Vec<String>,
    audio_devices_at: std::time::Instant,
    /// Per-song VST3 state records (the `<song>.mid.editor.state` companion
    /// file): keyed by class uid, or bundle path while a uid is unknown.
    plugin_states: plugin_state::PluginStateStore,
    /// dest indexes whose live slot needs a state (re)capture — set wherever
    /// slot parameters actually change (editor drains, editor sync, MIDI
    /// playback), drained by `flush_plugin_states`.
    pending_state_capture: BTreeSet<usize>,
    /// in-memory records changed since the companion file was last written
    state_file_dirty: bool,
    /// last companion-file write — tick-driven flushes throttle to ~1/s so a
    /// knob drag can't turn into a disk-write loop
    last_state_write: std::time::Instant,
    /// dest indexes whose slot already received its saved state — prevents a
    /// double `load_state` on doc swaps where the instance stayed warm
    state_restored: std::collections::HashSet<usize>,
    /// Standalone window hosting the open plugin editor (in-process
    /// instance — isolated plugins cannot host a GUI on Windows).
    plugin_window: Option<vst3_host::PluginWindow>,
    /// (dest index, in-process editor instance) for editor↔playback sync:
    /// param edits drain into the playing slot live, and full state is
    /// transferred on open/close via save_state/load_state.
    editor_plugin: Option<(usize, std::sync::Arc<std::sync::Mutex<vst3_host::Plugin>>)>,
    /// dest index → restart notifications already logged for the instance
    /// there — a notification the host doesn't act on is recorded once, not
    /// every frame (`apply_restart_flags`). Cleared with the slot.
    restart_logged: HashMap<usize, output::RestartLog>,
    /// Manual text-encoding override for display decoding (None = auto/XF hint)
    enc_override: Option<smf_core::TextEncoding>,
    playback: Option<Playback>,
    play_us: u64,
    /// restart at `loop_start_us` when playback reaches the end
    /// (`loop_enabled` itself lives in `shared` so MCP can toggle it)
    loop_start_us: u64,
    /// playhead follow mode during playback (per-song pref)
    follow: Follow,
    /// manual scroll pauses follow until this instant
    follow_hold: Option<std::time::Instant>,
    /// stacked bottom lanes (never empty)
    lanes: Vec<LaneCfg>,
    /// lane that lane gestures and the View > Lane menu act on
    lane_focus: usize,
    /// live MIDI input capture while `rec` is armed
    rec: Option<Rec>,
    /// per-sink SysEx counters for the current/last playback pass — read at
    /// stop to surface a deferred/dropped/worst-send diagnostic
    sysex_stats: Vec<std::sync::Arc<midi_io::SysexStats>>,
    /// open menubar dropdown + the x-coordinate it was opened at
    open_menu: Option<(TopMenu, f32)>,
    /// open cascading submenu + the y of its parent item
    open_sub: Option<(Sub, f32)>,
    /// note audition/scrub preview worker (issue #39) — owns preview sinks
    /// on its own thread and enforces every strike's note-off itself, so a
    /// UI stall can never strand a sounding note
    audition: Audition,
    /// Transport ▸ Audition Notes (global prefs)
    aud_enabled: bool,
    /// preview velocity for piano-key/draw strikes (note clicks use the
    /// note's own velocity)
    aud_vel: u8,
    /// max preview sustain in ms — release ends the note earlier
    aud_ms: u64,
    /// destination indexes whose sink the worker already holds
    aud_ships: HashSet<usize>,
    /// ports that failed to open — one status line, not a retry per click
    aud_failed: HashSet<usize>,
    /// piano-key strip scrub: the row currently held (also marks drag)
    scrub_key: Option<u8>,
    /// last-seen window activation — a transition to inactive releases any
    /// preview note (focus-loss guarantee)
    win_active: bool,
    /// right-docked event list panel visibility
    show_events: bool,
    /// F1 keyboard-shortcuts overlay
    help_open: bool,
    /// command palette / keybindings overlay (Ctrl+Shift+P)
    palette: Option<Palette>,
    /// effective keybindings: registry defaults + user overrides
    keys: cmd::KeyMap,
    /// separate input for the meta editor (track-name `input` stays exclusive)
    meta_input: Entity<InputState>,
    /// open meta edit dialog: which event (id>0) or insert point (id=0)
    meta_edit: Option<MetaEdit>,
    /// (track, event id) of a clicked marker / meta event — for Del / `e` edit
    meta_sel: Option<(usize, EventId)>,
    /// set by menu clicks (which lack a Window) — render picks it up, opens
    /// the dialog, and focuses the input
    meta_pending: Option<(usize, u64, u8, EventId)>,
    /// set when the meta dialog closes from a path that lacks a Window
    /// (Enter in the input subscription) — render refocuses the editor so
    /// editor keys (Del, arrows) keep working after commit
    meta_refocus: bool,
    /// one-bar count-in before MIDI recording starts (global pref)
    count_in: bool,
    /// recently opened files (global pref, newest first)
    recent: Vec<SharedString>,
    /// recording source — MIDI input port name; empty = first available
    midi_in: SharedString,
    /// manual input-latency compensation in ms (global pref)
    in_latency_ms: u64,
    /// active color palette — dark, light, or high-contrast accessible
    theme: theme::Theme,
    /// user's stored HC override (None = follow the OS high-contrast flag)
    hc_pref: Option<bool>,
    /// appearance preference (System follows the OS light/dark flag)
    theme_mode: theme::ThemeMode,
    /// last OS appearance seen — from `window.appearance()` updates
    sys_dark: bool,
    /// keeps the OS appearance-change observer alive
    _appearance: Option<Subscription>,
    focus: FocusHandle,
    input: Entity<InputState>,
    status: SharedString,
    /// identity of the backing .mid at open/last save — the basis for
    /// external-change detection (see `watch.rs`)
    file_stamp: Option<watch::FileStamp>,
    /// a "file changed/deleted on disk" prompt is already up — only one
    /// per external change event until the stamp is re-baselined
    ext_prompted: bool,
    /// some window.prompt is awaiting an answer — prompt() panics on
    /// re-entrant use, so a second one must not be opened
    prompt_active: bool,
    /// last time we stat'd the backing file (2s cadence)
    last_ext_check: std::time::Instant,
    /// a Save/Don't-Save/Cancel prompt is awaiting an answer — repeat
    /// triggers must not stack another one (`window.prompt` is not re-entrant)
    guard_active: bool,
    /// the discard guard approved closing: the re-entrant
    /// `on_window_should_close` that `remove_window` fires must pass
    close_confirmed: bool,
    /// our window's handle — needed to open prompts from listeners and
    /// the doc-watch loop where no &mut Window is passed
    window_handle: Option<AnyWindowHandle>,
    /// teardown coordinator — owns every worker handle (see `shutdown.rs`)
    shutdown: shutdown::Shutdown,
    /// keeps the on_app_quit subscription registered for the view's life
    _quit_sub: Option<Subscription>,
    /// (track, event index, id) for each real event-list row — diagnostic
    /// rows carry None. Parallel to `events`.
    event_refs: Arc<Vec<Option<(usize, usize, EventId)>>>,
    /// event-list selection (independent from the roll note `selection`)
    sel_events: BTreeSet<EventId>,
    /// anchor row index for shift-range selection in the event list
    ev_anchor: Option<usize>,
    /// inspector field currently being edited, if any
    prop_field: Option<PropField>,
    /// numeric/hex editor for the inspector's active field
    prop_input: Entity<InputState>,
}

/// An editable/read-only property shown in the event inspector. The same
/// key is reused across event kinds — the row label says what it means.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PropField {
    Tick,
    Channel,
    D0,
    D1,
    /// 14-bit pitch-bend value, displayed as -8192..8191
    PbValue,
    /// meta event type byte — byte-level, warns
    MetaType,
    /// raw payload bytes as hex — byte-level, warns
    HexData,
    NoteStart,
    NoteEnd,
    NoteDur,
    NoteVel,
    NoteRelVel,
    NoteChannel,
    TrackChannel,
}

/// One row of the inspector: label + current value (+ edit/warn flags).
struct PropRow {
    field: Option<PropField>,
    label: SharedString,
    value: String,
    warn: bool,
}

/// Which list the palette overlay shows.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PaletteMode {
    /// search + run commands
    Commands,
    /// view + rebind keyboard shortcuts
    Keys,
}

/// Command palette state (also drives the keybindings overlay).
pub struct Palette {
    pub mode: PaletteMode,
    pub input: Entity<InputState>,
    pub sel: usize,
    /// command id awaiting a new shortcut (Keys mode capture)
    pub capture: Option<&'static str>,
    /// keeps the input->repaint observation alive
    _sub: Subscription,
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

/// How a plugin (re)scan treats the persistent scan cache.
enum ScanMode {
    /// Serve unchanged bundles from the cache; probe only new/changed paths.
    Changed,
    /// Ignore the cache entirely and re-probe every bundle found.
    All,
    /// Force-re-probe one bundle (a quarantined plugin the user retried).
    Retry(PathBuf),
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
    input: midi_io::Input,
    /// the input port vanished mid-take — the watcher reconnects the exact
    /// (name, ord) endpoint when it returns
    input_lost: bool,
    buf: RecBuf,
    /// document time (µs) corresponding to Input's t=0
    base_us: u64,
    /// count-in duration — input before this is discarded
    cin_us: u64,
    /// jitter counters — how much callback-delivery delay the backend
    /// timestamps absorbed this take (surfaced as a debug diagnostic)
    diag: std::sync::Arc<midi_io::InputDiag>,
}

/// Output destination catalog: real MIDI ports by name, then discovered
/// VST3s. Rebuilt on `Output ▸ Rescan Plugins`.
fn build_dest_catalog(plugins: &[output::PluginInfo]) -> Vec<(String, midi_io::Destination)> {
    let ports = midi_io::list_outputs().unwrap_or_default();
    // same-name devices need a visible discriminator in the menu
    let mut name_counts: HashMap<String, usize> = HashMap::new();
    for p in &ports {
        *name_counts.entry(p.name.clone()).or_default() += 1;
    }
    let mut dests: Vec<(String, midi_io::Destination)> = ports
        .into_iter()
        .map(|p| {
            let label = if name_counts.get(p.name.as_str()).copied().unwrap_or(0) > 1 {
                format!("{} #{}", p.name, p.ord + 1)
            } else {
                p.name.clone()
            };
            (
                label,
                midi_io::Destination::MidiPort {
                    port_name: p.name,
                    ord: p.ord,
                },
            )
        })
        .collect();
    for p in plugins {
        dests.push((
            p.name.clone(),
            midi_io::Destination::Plugin {
                plugin_path: p.path.to_string_lossy().into_owned(),
                component_id: p.uid.clone(),
                vendor: (!p.vendor.is_empty()).then(|| p.vendor.clone()),
                plugin_name: Some(p.name.clone()),
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

/// Parse a key-signature spec: "-3 minor" / "2 major" / "-3" use literal
/// sf bytes; names ("eb", "f#m", "a minor") resolve through the circle of
/// fifths — minor shifts sf by -3 relative to the same-named major key.
fn parse_key_sig(s: &str) -> Option<(i8, u8)> {
    let mut rest = s.trim().to_lowercase();
    if rest.is_empty() {
        return None;
    }
    let mut mi = 0u8;
    for suf in ["minor", "min", "m"] {
        if let Some(r) = rest.strip_suffix(suf) {
            rest = r.trim().to_string();
            mi = 1;
            break;
        }
    }
    if mi == 0 {
        for suf in ["major", "maj"] {
            if let Some(r) = rest.strip_suffix(suf) {
                rest = r.trim().to_string();
                break;
            }
        }
    }
    let sf: i8 = match rest.parse() {
        Ok(v) => v,
        Err(_) => {
            const NAMES: [(&str, i8); 15] = [
                ("cb", -7),
                ("gb", -6),
                ("db", -5),
                ("ab", -4),
                ("eb", -3),
                ("bb", -2),
                ("f", -1),
                ("c", 0),
                ("g", 1),
                ("d", 2),
                ("a", 3),
                ("e", 4),
                ("b", 5),
                ("f#", 6),
                ("c#", 7),
            ];
            NAMES
                .iter()
                .find(|(n, _)| *n == rest)
                .map(|(_, v)| v - 3 * mi as i8)?
        }
    };
    (-7..=7).contains(&sf).then_some((sf, mi))
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

/// `EditorView::transport_points` on a bare `Document` — free so tests can
/// drive it without a view. Tempo map + every `0x58` meter meta as `(µs,
/// TransportCmd)`, sorted by µs.
#[cfg(test)]
fn transport_points_of(d: &Document) -> Vec<(u64, output::TransportCmd)> {
    transport_points_for(d, None)
}

/// `tr` selects the sequence in format-2 documents (each sequence has its
/// own tempo map and meta events); `None` uses the document-level maps.
fn transport_points_for(d: &Document, tr: Option<usize>) -> Vec<(u64, output::TransportCmd)> {
    let owned;
    let (tm, tracks): (&document::TempoMap, &[document::Track]) = match tr {
        Some(t) => {
            owned = d.tempo_map_for(t);
            (&owned, std::slice::from_ref(&d.tracks[t]))
        }
        None => (&d.tempo_map, &d.tracks),
    };
    let mut pts: Vec<(u64, output::TransportCmd)> = tm
        .points()
        .iter()
        .map(|(_, mpq, us)| {
            (
                *us,
                output::TransportCmd::Tempo(60_000_000.0 / (*mpq).max(1) as f64),
            )
        })
        .collect();
    let mut sig_pts = Vec::new();
    for tr in tracks {
        for e in &tr.events {
            if let EventKind::Meta {
                meta_type: 0x58,
                data,
            } = &e.kind
            {
                if data.len() >= 2 {
                    sig_pts.push((
                        tm.tick_to_us(e.tick),
                        output::TransportCmd::TimeSig(i32::from(data[0]), 1i32 << (data[1] & 0x1f)),
                    ));
                }
            }
        }
    }
    // SMF defaults live outside the event stream: a map with no tempo/meta
    // at tick 0 still plays 120bpm in 4/4 — state the plugin must be told
    // explicitly since a chase before the first point yields nothing.
    if !matches!(pts.first(), Some((us, _)) if *us == 0) {
        pts.push((0, output::TransportCmd::Tempo(120.0)));
    }
    if !sig_pts.iter().any(|(us, _)| *us == 0) {
        sig_pts.push((0, output::TransportCmd::TimeSig(4, 4)));
    }
    pts.extend(sig_pts);
    pts.sort_by_key(|(us, _)| *us);
    pts
}

impl EditorView {
    pub fn new(
        path: Option<PathBuf>,
        input: Entity<InputState>,
        prop_input: Entity<InputState>,
        meta_input: Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let loaded = path.as_deref().map(mcp_server::service::load_document);
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
            Some(Err(e)) => (
                empty_doc(),
                tf("status.load_failed", &[("e", &e.to_string())]).into(),
            ),
            None => (empty_doc(), t("status.new_doc").into()),
        };
        let mut sh = Shared::new(doc);
        sh.path = path.clone();
        sh.gui_attached = true;
        let initial_plugins = output::discover_plugin_paths();
        sh.dests = build_dest_catalog(&initial_plugins);
        sh.port_present = midi_io::list_outputs()
            .unwrap_or_default()
            .into_iter()
            .map(|p| (p.name, p.ord))
            .collect();
        let g = GlobalPrefs::load();
        let shared = Arc::new(Mutex::new(sh));
        let (plugin_req, plugin_evt, host_thread) = output::spawn_plugin_host();
        tracing::info!("plugin host worker spawned");
        let hd = output::host_diag();
        tracing::info!(
            helper = ?hd.helper,
            probe = ?hd.probe,
            audio_device = ?hd.audio_device,
            "host diagnostics"
        );
        let mut v = Self {
            shared,
            doc_epoch: 0,
            notes_key: (u64::MAX, u64::MAX),
            notes: Arc::new(vec![]),
            ev_key: (u64::MAX, u64::MAX, usize::MAX),
            events: Arc::new(vec![]),
            doc_ui: Arc::new(DocUi::default()),
            doc_ui_key: (u64::MAX, u64::MAX, usize::MAX),
            doc_ui_enc: None,
            menu_fh: cx.focus_handle().tab_stop(true),
            tracks_fh: cx.focus_handle().tab_stop(true),
            roll_fh: cx.focus_handle().tab_stop(true),
            lane_fh: cx.focus_handle().tab_stop(true),
            events_fh: cx.focus_handle().tab_stop(true),
            menu_bar_sel: 0,
            last_area: FocusArea::Roll,
            menu_sel: None,
            sub_sel: None,
            menu_rows: Vec::new(),
            sub_rows: Vec::new(),
            cursor_tick: 0,
            cursor_key: 60,
            ev_sel: 0,
            events_scroll: UniformListScrollHandle::new(),
            lane_caches: HashMap::new(),
            lane_sel: BTreeSet::new(),
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
            kbd_bounds: Rc::new(Cell::new(Bounds::new(
                point(px(0.0), px(0.0)),
                size(px(0.0), px(0.0)),
            ))),
            ruler_bounds: Rc::new(Cell::new(Bounds::new(
                point(px(0.0), px(0.0)),
                size(px(0.0), px(0.0)),
            ))),
            lane_bounds: Vec::new(),
            mini_bounds: Rc::new(Cell::new(Bounds::new(
                point(px(0.0), px(0.0)),
                size(px(0.0), px(0.0)),
            ))),
            scroll_x: 0.0,
            scroll_y: (127.0 - 84.0) * NOTE_H, // show ~C3..C7
            zoom: 0.08,
            note_h: NOTE_H,
            fold: false,
            drum: false,
            scale_sel: -2,
            scale_minor: false,
            vis_keys: all_keys(),
            row_of: {
                let mut r = [-1i32; 128];
                for (i, k) in all_keys().iter().enumerate() {
                    r[*k as usize] = i as i32;
                }
                r
            },
            scale_pcs: None,
            view_key: (u64::MAX, u64::MAX, false, false, 0, i8::MAX, false),
            keysig: None,
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
            save_rx: None,
            scan_note: None,
            scan_probe_used: None,
            scan_cached: 0,
            quarantined: Vec::new(),
            probe_timeout_secs: g
                .probe_timeout_secs
                .unwrap_or(output::DEFAULT_SCAN_TIMEOUT.as_secs()),
            host_diag: hd,
            show_output_status: false,
            audio_sel: output::AudioSelection {
                device: g.audio_device.clone(),
                sample_rate: g.sample_rate,
                buffer_size: g.buffer_size,
            },
            audio_retry: HashMap::new(),
            audio_devices: output::output_devices(),
            audio_devices_at: std::time::Instant::now(),
            plugin_states: plugin_state::PluginStateStore::default(),
            pending_state_capture: BTreeSet::new(),
            state_file_dirty: false,
            last_state_write: std::time::Instant::now(),
            state_restored: std::collections::HashSet::new(),
            plugin_window: None,
            editor_plugin: None,
            restart_logged: HashMap::new(),
            enc_override: None,
            playback: None,
            play_us: 0,
            loop_start_us: 0,
            follow: Follow::Page,
            follow_hold: None,
            lanes: vec![LaneCfg::default()],
            lane_focus: 0,
            rec: None,
            sysex_stats: Vec::new(),
            open_menu: None,
            help_open: false,
            palette: None,
            keys: cmd::KeyMap {
                overrides: g.keymap.clone(),
            },
            meta_input,
            meta_edit: None,
            meta_sel: None,
            meta_pending: None,
            meta_refocus: false,
            count_in: g.count_in,
            recent: g.recent.iter().map(|p| p.as_str().into()).collect(),
            midi_in: g.midi_in.clone().into(),
            in_latency_ms: g.in_latency_ms,
            theme_mode: theme::ThemeMode::from_pref(g.theme.as_deref()),
            sys_dark: matches!(
                window.appearance(),
                WindowAppearance::Dark | WindowAppearance::VibrantDark
            ),
            theme: theme::Theme::dark(), // replaced by apply_theme below
            hc_pref: g.hc,
            _appearance: None,
            open_sub: None,
            show_events: true,
            audition: Audition::spawn(),
            aud_enabled: g.audition.unwrap_or(true),
            aud_vel: g.aud_vel.unwrap_or(100).clamp(1, 127),
            aud_ms: g.aud_ms.unwrap_or(500),
            aud_ships: HashSet::new(),
            aud_failed: HashSet::new(),
            scrub_key: None,
            win_active: true,
            focus: cx.focus_handle(),
            input,
            event_refs: Arc::new(vec![]),
            sel_events: BTreeSet::new(),
            ev_anchor: None,
            prop_field: None,
            prop_input,
            status,
            file_stamp: path.as_deref().and_then(watch::stat_file),
            ext_prompted: false,
            prompt_active: false,
            last_ext_check: std::time::Instant::now(),
            guard_active: false,
            close_confirmed: false,
            window_handle: None,
            shutdown: shutdown::Shutdown::default(),
            _quit_sub: None,
        };
        v.shutdown.track_host(host_thread);
        // app-quit path (menu Quit / task kill minus window close) —
        // on_window_should_close alone doesn't cover it
        v._quit_sub = Some(cx.on_app_quit(|v, _| {
            v.perform_shutdown();
            async {}
        }));
        // the OS flips light/dark under a running app — follow it while
        // the preference is System
        v._appearance =
            Some(cx.observe_window_appearance(window, |v, w, cx| v.on_sys_appearance(w, cx)));
        v.apply_theme(cx);
        v.sel_track = v.pick_default_track();
        v.refresh_derived();
        if let Some(p) = &path {
            let diags = v.apply_prefs(p);
            if !diags.is_empty() {
                v.status = tf("status.prefs_warn", &[("e", &diags.join("; "))]).into();
            }
            v.push_recent(p);
        }
        v.rescan_plugins(ScanMode::Changed);
        v
    }

    /// Recompute the active palette from prefs + OS flags and push the
    /// matching mode onto gpui-component so text inputs stay legible.
    fn apply_theme(&mut self, cx: &mut Context<Self>) {
        self.theme = theme::Theme::resolve(self.hc_pref, self.theme_mode, self.sys_dark);
        let mode = if self.theme == theme::Theme::light() {
            gpui_kit::component::theme::ThemeMode::Light
        } else {
            gpui_kit::component::theme::ThemeMode::Dark
        };
        gpui_kit::component::theme::Theme::change(mode, None, cx);
        cx.notify();
    }

    fn set_theme_mode(&mut self, mode: theme::ThemeMode, cx: &mut Context<Self>) {
        self.theme_mode = mode;
        self.apply_theme(cx);
        self.save_global();
    }

    fn on_sys_appearance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dark = matches!(
            window.appearance(),
            WindowAppearance::Dark | WindowAppearance::VibrantDark
        );
        if dark != self.sys_dark {
            self.sys_dark = dark;
            if self.theme_mode == theme::ThemeMode::System {
                self.apply_theme(cx);
            }
        }
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
        // the outgoing song keeps its plugin state — flush before the path
        // and the state table are dropped with the document
        self.flush_plugin_states(true);
        self.plugin_states = plugin_state::PluginStateStore::default();
        self.state_file_dirty = false;
        self.pending_state_capture.clear();
        mcp_server::service::swap_document(&self.shared, empty_doc(), None);
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
        // the replaced document's snapshots no longer apply
        recovery::clear_recovery();
        // untitled has no backing file to watch
        self.file_stamp = None;
        self.ext_prompted = false;
        self.status = if rec_discarded {
            format!("{} — {}", t("status.new_doc"), t("status.rec_discarded")).into()
        } else {
            t("status.new_doc").into()
        };
        cx.notify();
    }

    /// Adopt a recovery snapshot in place: swaps in its document, keeps the
    /// source path so an explicit Save writes back to it — but marks the
    /// doc dirty via an unreachable saved_revision so nothing is written
    /// until the user says so.
    fn restore_snapshot(
        &mut self,
        meta: &recovery::SnapshotMeta,
        payload: &[u8],
        cx: &mut Context<Self>,
    ) {
        match smf_core::parse(payload) {
            Ok(file) => {
                let rec_discarded = self.rec.take().is_some();
                self.stop_playback();
                {
                    let mut sh = lock_shared(&self.shared);
                    sh.doc = Document::from_file(file);
                    sh.undo = UndoStack::new(512);
                    if sh.path.is_none() {
                        sh.path = meta.source_path.clone();
                    }
                    // an unreachable marker: dirty until a verified save,
                    // matching "recovery never overwrites without Save"
                    sh.saved_revision = u64::MAX;
                }
                self.reset_view_for_new_doc();
                if let Some(src) = &meta.source_path {
                    self.apply_prefs(src);
                }
                let mut status = t("recovery.restored").to_string();
                if rec_discarded {
                    status = format!("{status} — {}", t("status.rec_discarded"));
                }
                self.status = status.into();
            }
            // corrupt payload — fail safely, keep the file for diagnosis
            Err(e) => self.status = tf("recovery.failed", &[("e", &e.to_string())]).into(),
        }
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
        self.scroll_y = clamp_span(self.scroll_y, self.vis_keys.len() as f32 * self.note_h, h);
        // the edit cursor is live content too: a doc narrower than the
        // viewport would pin scroll_x=0 and let the cursor walk off-screen
        let end = self
            .doc_end_ticks()
            .max(self.cursor_tick + self.cursor_insert_len());
        self.scroll_x = clamp_span(self.scroll_x, end as f32 * self.zoom + 32.0, w);
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
        // center on the median pitch's *row* (the row map may be folded)
        let mid_row = mid.and_then(|k| {
            let r = self.row_of[k as usize];
            (r >= 0).then_some(r)
        });
        let (x, y) = content_view(first, mid_row, self.zoom, self.note_h);
        self.scroll_x = x;
        self.scroll_y = y;
        self.cursor_tick = first;
        self.cursor_key = mid.unwrap_or(60);
        self.ev_sel = 0;
    }

    /// Keep the playhead on screen while playing, per `follow` mode.
    /// Never fires while a drag is live or `follow_hold` is active.
    fn follow_playhead(&mut self, tick: u64) {
        let w = f32::from(self.roll_bounds.get().size.width);
        if w <= 0.0 {
            return;
        }
        let x = tick as f32 * self.zoom;
        match self.follow {
            Follow::Off => {}
            Follow::Page => {
                let m = 48.0;
                if x < self.scroll_x + m || x > self.scroll_x + w - m {
                    self.scroll_x = (x - w * 0.15).max(0.0);
                }
            }
            Follow::Smooth => {
                self.scroll_x = (x - w / 3.0).max(0.0);
            }
        }
        self.clamp_scroll();
    }

    /// Scroll so the playhead sits at viewport center.
    fn center_playhead(&mut self) {
        let w = f32::from(self.roll_bounds.get().size.width);
        if w <= 0.0 {
            return;
        }
        let tick = self.doc(|d| d.tempo_map.us_to_tick(self.play_us));
        self.scroll_x = (tick as f32 * self.zoom - w / 2.0).max(0.0);
        self.clamp_scroll();
    }

    /// Go to playhead — also the explicit "resume follow" gesture.
    fn go_playhead(&mut self, cx: &mut Context<Self>) {
        self.follow_hold = None;
        self.center_playhead();
        cx.notify();
    }

    /// Frame the selected notes in the viewport (10% breathing room).
    fn zoom_to_selection(&mut self, cx: &mut Context<Self>) {
        let mut lo = u64::MAX;
        let mut hi = 0u64;
        for n in self
            .notes
            .iter()
            .filter(|n| self.selection.contains(&n.on_id))
        {
            lo = lo.min(n.start_tick);
            hi = hi.max(n.end_tick.unwrap_or(n.start_tick + 1));
        }
        if lo > hi {
            self.status = t("status.nosel").into();
            cx.notify();
            return;
        }
        self.zoom_to_span(lo, hi, cx);
    }

    /// Frame the whole song.
    fn zoom_to_song(&mut self, cx: &mut Context<Self>) {
        self.zoom_to_span(0, self.doc_end_ticks(), cx);
    }

    fn zoom_to_span(&mut self, lo: u64, hi: u64, cx: &mut Context<Self>) {
        let w = f32::from(self.roll_bounds.get().size.width);
        if w <= 0.0 {
            return;
        }
        let span = (hi - lo).max(1) as f32;
        self.zoom = (w * 0.9 / span).clamp(ZOOM_MIN, ZOOM_MAX);
        self.scroll_x = (lo as f32 * self.zoom - w * 0.05).max(0.0);
        self.clamp_scroll();
        self.persist();
        cx.notify();
    }

    /// Seek the playhead to the previous/next marker in the file.
    fn marker_step(&mut self, dir: i64, cx: &mut Context<Self>) {
        let tick = self.doc(|d| d.tempo_map.us_to_tick(self.play_us));
        let dst = if dir > 0 {
            self.doc_ui.markers.iter().find(|m| m.0 > tick)
        } else {
            self.doc_ui.markers.iter().rev().find(|m| m.0 < tick)
        };
        match dst {
            Some(&(t, id, tr, ref _n)) => {
                self.meta_sel = Some((tr, id));
                self.seek_to_tick(t, false, cx);
                self.center_playhead();
            }
            None => {
                self.status = t("status.no_marker").into();
                cx.notify();
            }
        }
    }

    /// Seek the playhead to the previous/next event of the selected track —
    /// the same jump whether the roll or the event list drove the command.
    fn event_step(&mut self, dir: i64, cx: &mut Context<Self>) {
        let tick = self.doc(|d| d.tempo_map.us_to_tick(self.play_us));
        let tr = self.sel_track;
        let dst = self.doc(|d| {
            let it = d
                .tracks
                .get(tr)
                .into_iter()
                .flat_map(|t| t.events.iter().map(|e| e.tick));
            if dir > 0 {
                it.filter(|&x| x > tick).min()
            } else {
                it.filter(|&x| x < tick).max()
            }
        });
        match dst {
            Some(t) => {
                self.seek_to_tick(t, false, cx);
                self.center_playhead();
            }
            None => {
                self.status = t("status.no_event").into();
                cx.notify();
            }
        }
    }

    fn set_enc(&mut self, enc: Option<smf_core::TextEncoding>, cx: &mut Context<Self>) {
        self.enc_override = enc;
        self.ev_key = (u64::MAX, u64::MAX, usize::MAX); // force event-row rebuild
        self.refresh_derived();
        self.persist();
        cx.notify();
    }

    /// Mode of the focused lane — drives the View > Lane checkmarks and
    /// the status-bar chip.
    fn lane_mode(&self) -> LaneMode {
        self.lanes
            .get(self.lane_focus)
            .map(|c| c.mode)
            .unwrap_or(LaneMode::Velocity)
    }

    fn set_lane(&mut self, m: LaneMode, cx: &mut Context<Self>) {
        if let Some(c) = self.lanes.get_mut(self.lane_focus) {
            c.mode = m;
        }
        self.lane_sel.clear();
        self.persist();
        cx.notify();
    }

    /// Stack a new lane below the existing ones, preferring a mode not
    /// already shown.
    fn add_lane(&mut self, cx: &mut Context<Self>) {
        if self.lanes.len() >= LANES_MAX {
            return;
        }
        let mode = LANE_MODES
            .iter()
            .find(|m| !self.lanes.iter().any(|c| c.mode == **m))
            .copied()
            .unwrap_or(LaneMode::Velocity);
        self.lanes.push(LaneCfg {
            mode,
            ..LaneCfg::default()
        });
        self.lane_focus = self.lanes.len() - 1;
        self.persist();
        cx.notify();
    }

    fn remove_lane(&mut self, cx: &mut Context<Self>) {
        if self.lanes.len() <= 1 {
            return;
        }
        self.lanes.remove(self.lane_focus.min(self.lanes.len() - 1));
        self.lane_focus = self.lane_focus.min(self.lanes.len() - 1);
        self.persist();
        cx.notify();
    }

    /// snap interval in ticks (0 = off)
    /// Current snap step in ticks (0 = off). Triplet entries are 2/3 of the
    /// duple cell — 1/8T = a third of a quarter note.
    /// Ticks per quarter note for display grids; SMPTE docs use the same
    /// 480 fallback the pre-format-2 code did (position labels only).
    fn ppq(&self) -> u64 {
        self.doc(|d| d.tempo_map.ppq().unwrap_or(480))
    }

    /// Move the playhead `bars` measures (used by the ruler/minimap
    /// accessibility Increment/Decrement actions).
    fn seek_bars(&mut self, bars: i64, cx: &mut Context<Self>) {
        let step = self.ppq() as i64 * 4;
        let cur = self.doc(|d| d.tempo_map.us_to_tick(self.play_us)) as i64;
        let tick = (cur + bars * step).max(0).min(self.doc_end_ticks() as i64);
        self.play_us = self.doc(|d| d.tempo_map.tick_to_us(tick as u64));
        cx.notify();
    }

    fn snap_ticks(&self) -> i64 {
        let (div, trip, _) = SNAPS[self.snap_idx];
        if div == 0 {
            return 0;
        }
        let base = self.td().snap_base_ticks() as i64 / div as i64;
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

    /// Vertical zoom to an absolute row height, keeping the row at
    /// `anchor_off` pixels from the viewport's top edge fixed (the cursor's
    /// pitch stays under the cursor).
    fn vzoom_set(&mut self, h: f32, anchor_off: f32, cx: &mut Context<Self>) {
        let anchor_row = (anchor_off.max(0.0) + self.scroll_y) / self.note_h;
        self.note_h = h.clamp(NOTE_H_MIN, NOTE_H_MAX);
        self.scroll_y = (anchor_row * self.note_h - anchor_off.max(0.0)).max(0.0);
        self.clamp_scroll();
        self.persist();
        cx.notify();
    }

    /// Menu-driven vertical zoom — anchors on the selected note's pitch when
    /// there is one, else the viewport center.
    fn vzoom_by(&mut self, f: f32, cx: &mut Context<Self>) {
        let anchor = self
            .selection
            .iter()
            .next()
            .and_then(|id| self.notes.iter().find(|n| n.on_id == *id))
            .and_then(|n| {
                let r = self.row_of[n.key as usize];
                (r >= 0).then_some(r as f32 * self.note_h - self.scroll_y)
            })
            .unwrap_or_else(|| f32::from(self.roll_bounds.get().size.height) / 2.0);
        self.vzoom_set(self.note_h * f, anchor, cx);
    }

    /// Fold/drum/scale toggles rebuild the row map on the next refresh; the
    /// selection may hold keys that fold out, which paint/hit-test already
    /// skip via `row_of`.
    fn set_fold(&mut self, on: bool, cx: &mut Context<Self>) {
        self.fold = on;
        self.refresh_derived();
        self.clamp_scroll();
        self.persist();
        cx.notify();
    }

    fn set_drum(&mut self, on: bool, cx: &mut Context<Self>) {
        self.drum = on;
        self.refresh_derived();
        self.clamp_scroll();
        self.persist();
        cx.notify();
    }

    fn set_scale(&mut self, sel: i8, minor: bool, cx: &mut Context<Self>) {
        self.scale_sel = sel;
        self.scale_minor = minor;
        self.refresh_derived();
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

    /// Focus the track-name input with the current name selected (Track >
    /// Rename, or Enter/F2 while the track list is focused).
    fn focus_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.doc(|d| {
            d.tracks
                .get(self.sel_track)
                .and_then(|t| t.name.as_ref())
                .map(|b| smf_core::decode_text(b, self.enc_override.or(d.text_encoding_hint())))
                .unwrap_or_default()
        });
        self.input.update(cx, |i, cx| {
            i.set_value(name, window, cx);
            i.select_all(window, cx);
        });
        let fh = self.input.read(cx).focus_handle(cx);
        window.focus(&fh, cx);
        cx.notify();
    }

    /// Open the palette overlay (Commands or Keys mode), closing menus.
    fn open_palette(&mut self, mode: PaletteMode, window: &mut Window, cx: &mut Context<Self>) {
        self.open_menu = None;
        self.open_sub = None;
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(t("ui.palette_hint")));
        let sub = cx.subscribe(
            &input,
            |v, _e, ev: &gpui_kit::component::input::InputEvent, cx| {
                if matches!(ev, gpui_kit::component::input::InputEvent::Change) {
                    // filter text changed → reset selection + repaint
                    if let Some(p) = v.palette.as_mut() {
                        p.sel = 0;
                    }
                    cx.notify();
                }
            },
        );
        self.palette = Some(Palette {
            mode,
            input: input.clone(),
            sel: 0,
            capture: None,
            _sub: sub,
        });
        let fh = input.read(cx).focus_handle(cx);
        window.focus(&fh, cx);
        cx.notify();
    }

    fn close_palette(&mut self, cx: &mut Context<Self>) {
        if self.palette.take().is_some() {
            cx.notify();
        }
    }

    /// Commands matching the palette's filter text (label or id).
    fn palette_rows(&self, cx: &App) -> Vec<&'static cmd::Command> {
        let Some(p) = &self.palette else {
            return Vec::new();
        };
        let q = p.input.read(cx).value().trim().to_lowercase();
        cmd::COMMANDS
            .iter()
            .filter(|c| {
                q.is_empty() || t(c.label_key).to_lowercase().contains(&q) || c.id.contains(&q)
            })
            .collect()
    }

    /// Every key pressed while the palette is open flows here (the root
    /// handler routes it); nav/select keys are consumed, the rest reach
    /// the filter input's own handler first and are ignored here.
    fn palette_key(&mut self, ev: &KeyDownEvent, w: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = &self.palette else { return };
        let mode = p.mode;
        let capturing = p.capture;
        let k = ev.keystroke.key.as_str();

        // Keys-mode capture eats the next real keystroke as the new binding.
        if let Some(id) = capturing {
            match k {
                "escape" => {
                    if let Some(p) = self.palette.as_mut() {
                        p.capture = None;
                    }
                }
                // ignore modifiers on their own — wait for a real key
                "control" | "shift" | "alt" | "capslock" | "function" | "platform" => {}
                _ => {
                    if let Some(p) = self.palette.as_mut() {
                        p.capture = None;
                    }
                    let desc = cmd::describe(&ev.keystroke);
                    match self.keys.assign(id, &desc) {
                        Ok(()) => {
                            self.save_global();
                            let c = cmd::find(id).unwrap();
                            self.status = tf(
                                "ui.keys_bound",
                                &[("label", &cmd::label(c)), ("key", &cmd::format_key(&desc))],
                            )
                            .into();
                        }
                        Err(other) => {
                            self.status =
                                tf("ui.keys_conflict", &[("label", &cmd::label(other))]).into();
                        }
                    }
                }
            }
            // return focus to the filter after capture
            if let Some(p) = &self.palette {
                let fh = p.input.read(cx).focus_handle(cx);
                w.focus(&fh, cx);
            }
            cx.notify();
            cx.stop_propagation();
            return;
        }

        let rows_len = self.palette_rows(cx).len();
        let mut step = |d: isize| {
            if let Some(p) = self.palette.as_mut() {
                let n = rows_len.max(1) as isize;
                p.sel = ((p.sel as isize + d) % n + n) as usize % n as usize;
            }
        };
        match (k, ev.keystroke.modifiers.control) {
            ("escape", _) => self.close_palette(cx),
            ("tab", _) => {
                if let Some(p) = self.palette.as_mut() {
                    p.mode = match p.mode {
                        PaletteMode::Commands => PaletteMode::Keys,
                        PaletteMode::Keys => PaletteMode::Commands,
                    };
                    p.sel = 0;
                }
                cx.notify();
            }
            ("up", _) | ("down", _) => {
                step(if k == "up" { -1 } else { 1 });
                cx.notify();
            }
            ("pageup" | "page_up", _) | ("pagedown" | "page_down", _) => {
                step(if k.starts_with("pageup") || k == "page_up" {
                    -10
                } else {
                    10
                });
                cx.notify();
            }
            ("home", _) => {
                if let Some(p) = self.palette.as_mut() {
                    p.sel = 0;
                }
                cx.notify();
            }
            ("end", _) => {
                if let Some(p) = self.palette.as_mut() {
                    p.sel = rows_len.saturating_sub(1);
                }
                cx.notify();
            }
            ("enter", _) => self.palette_activate(w, cx),
            ("delete", false) | ("backspace", false) | ("r", true) => {
                // Keys mode: reset the selected command to its defaults.
                // (Del is also consumed by the filter input when focused, so
                // Ctrl+R is the reliable path — both are offered.)
                if mode == PaletteMode::Keys {
                    let rows = self.palette_rows(cx);
                    if let Some(c) = rows.get(self.palette.as_ref().unwrap().sel).copied() {
                        self.keys.reset(c.id);
                        self.save_global();
                        self.status = tf("ui.keys_reset", &[("label", &cmd::label(c))]).into();
                        cx.notify();
                    }
                }
            }
            _ => {}
        }
        cx.stop_propagation();
    }

    /// Enter or click on the selected palette row: run it (Commands) or
    /// begin keystroke capture (Keys).
    fn palette_activate(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let rows = self.palette_rows(cx);
        let Some(p) = &self.palette else { return };
        let mode = p.mode;
        let Some(&c) = rows.get(p.sel) else { return };
        match mode {
            PaletteMode::Commands => {
                self.close_palette(cx);
                (c.act)(self, w, cx);
            }
            PaletteMode::Keys => {
                // capture next keystroke: move focus off the filter so the
                // key can't be typed as text
                self.palette.as_mut().unwrap().capture = Some(c.id);
                w.focus(&self.focus.clone(), cx);
            }
        }
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
        // format 2: event rows and document chrome are scoped to the
        // viewed sequence — its index joins the cache key
        let seq_sel = if sh.doc.is_sequential() {
            self.sel_track
        } else {
            usize::MAX
        };
        let vkey = (key.0, key.1, seq_sel);
        if self.ev_key != vkey {
            let (rows, refs) = self.build_event_rows(&sh.doc);
            self.events = Arc::new(rows);
            self.event_refs = Arc::new(refs);
            // drop event-list selections that no longer exist
            let ids: BTreeSet<EventId> = self
                .event_refs
                .iter()
                .flatten()
                .map(|&(_, _, id)| id)
                .collect();
            self.sel_events.retain(|id| ids.contains(id));
            // the row count changes with edits — keep the selection valid
            self.ev_sel = self.ev_sel.min(self.events.len().saturating_sub(1));
            self.ev_key = vkey;
        }
        if self.doc_ui_key != vkey || self.doc_ui_enc != self.enc_override {
            self.doc_ui = Arc::new(DocUi::build(
                &sh.doc,
                &self.notes,
                self.enc_override,
                (seq_sel != usize::MAX).then_some(seq_sel),
            ));
            self.doc_ui_key = vkey;
            self.doc_ui_enc = self.enc_override;
        }
        // view-derived state: the visible row map (fold/drum), the key
        // signature at the playhead, and the scale-highlight pitch classes.
        // Keyed on the doc revision + the view toggles — all view-only, never
        // written back into the document.
        let vkey = (
            self.doc_epoch,
            sh.doc.revision(),
            self.fold,
            self.drum,
            self.sel_track,
            self.scale_sel,
            self.scale_minor,
        );
        if self.view_key != vkey {
            self.vis_keys = if self.drum {
                used_keys(&self.notes, true, self.sel_track)
            } else if self.fold {
                used_keys(&self.notes, false, self.sel_track)
            } else {
                all_keys()
            };
            self.row_of = [-1; 128];
            for (r, &k) in self.vis_keys.iter().enumerate() {
                self.row_of[k as usize] = r as i32;
            }
            let at = sh.doc.tempo_map.us_to_tick(self.play_us);
            self.keysig = sh.doc.key_signature(at);
            self.scale_pcs = match self.scale_sel {
                -1 => None,
                -2 => self
                    .keysig
                    .map(|(sf, m)| scale_pcs_of(keysig_root(sf, m), m)),
                r => Some(scale_pcs_of(r as u8, self.scale_minor)),
            };
            self.view_key = vkey;
        }
    }

    /// Control events of the selected track for a bottom lane, cached per
    /// (mode, poly key) on (epoch, revision, track) — render must not
    /// rescan the track while animating the playhead, and stacked lanes
    /// sharing a stream reuse one scan.
    /// Tuple: (event id, tick, value, poly key or -1)
    fn lane_events_cached(
        &mut self,
        mode: LaneMode,
        pkey: Option<u8>,
    ) -> Arc<Vec<(EventId, u64, i32, i32)>> {
        let key = (self.doc_epoch, self.doc(|d| d.revision()));
        let stale = self
            .lane_caches
            .get(&(mode, pkey))
            .map(|(k, tr, _)| *k != key || *tr != self.sel_track)
            .unwrap_or(true);
        if stale {
            let tr = self.sel_track;
            let mut v = Vec::new();
            self.doc(|d| {
                if let Some(t) = d.tracks.get(tr) {
                    for e in &t.events {
                        if let EventKind::Channel { status, data, .. } = &e.kind {
                            match (mode, status & 0xF0) {
                                (LaneMode::CC(cc), 0xB0) if data[0] == cc => {
                                    v.push((e.id, e.tick, data[1] as i32, -1))
                                }
                                (LaneMode::PitchBend, 0xE0) => v.push((
                                    e.id,
                                    e.tick,
                                    ((data[1] as i32) << 7) | data[0] as i32,
                                    -1,
                                )),
                                (LaneMode::ChanAT, 0xD0) => {
                                    v.push((e.id, e.tick, data[0] as i32, -1))
                                }
                                (LaneMode::PolyAT, 0xA0)
                                    if pkey.is_none() || pkey == Some(data[0]) =>
                                {
                                    v.push((e.id, e.tick, data[1] as i32, data[0] as i32))
                                }
                                _ => {}
                            }
                        }
                    }
                }
            });
            v.sort_by_key(|e| e.1);
            self.lane_caches
                .insert((mode, pkey), (key, tr, Arc::new(v)));
        }
        self.lane_caches[&(mode, pkey)].2.clone()
    }

    /// Keys that have poly-AT events in the selected track (for the key chip).
    fn poly_keys_present(&self) -> Vec<u8> {
        self.doc(|d| {
            let mut keys: Vec<u8> = d
                .tracks
                .get(self.sel_track)
                .map(|t| {
                    t.events
                        .iter()
                        .filter_map(|e| match &e.kind {
                            EventKind::Channel { status, data, .. } if status & 0xF0 == 0xA0 => {
                                Some(data[0])
                            }
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            keys.sort_unstable();
            keys.dedup();
            keys
        })
    }

    /// Cycle the poly-AT key filter through the keys present in the track
    /// (all → first key → … → last key → all). Shift steps a key at a time.
    fn cycle_poly_key(&mut self, back: bool, cx: &mut Context<Self>) {
        let keys = self.poly_keys_present();
        let Some(cfg) = self.lanes.get_mut(self.lane_focus) else {
            return;
        };
        if keys.is_empty() {
            cfg.poly_key = if back {
                cfg.poly_key.map(|k| k.wrapping_sub(1))
            } else {
                cfg.poly_key.map(|k| k.wrapping_add(1))
            };
        } else {
            let i = cfg.poly_key.and_then(|k| keys.iter().position(|&p| p == k));
            cfg.poly_key = match (i, back) {
                (None, false) => Some(keys[0]),
                (None, true) => Some(*keys.last().unwrap()),
                (Some(i), false) if i + 1 < keys.len() => Some(keys[i + 1]),
                (Some(_), false) => None,
                (Some(0), true) => None,
                (Some(i), true) => Some(keys[i - 1]),
            };
        }
        self.persist();
        cx.notify();
    }

    /// Tick position of the song end (scroll extent, minimap scale).
    /// Format 2: the viewed sequence's own span — other sequences may be
    /// longer, and sizing the view by them would lie about this one's end.
    /// At least four coarse cells (bars / seconds) past the content.
    fn doc_end_ticks(&self) -> u64 {
        if self.is_seq() {
            self.doc(|d| d.track_end_tick(self.sel_track))
                .max(self.td().bar_ticks() * 4)
        } else {
            self.doc_ui.song_end.max(self.td().bar_ticks() * 4)
        }
    }

    /// Whether the loaded file is SMF format 2 — tracks are independent
    /// sequences, never a single shared song timeline.
    fn is_seq(&self) -> bool {
        self.doc(|d| d.is_sequential())
    }

    /// Switch the viewed track/sequence. Format 2 clears the note
    /// selection: notes of another sequence are neither visible nor
    /// editable while this one is being shown and played.
    fn select_track(&mut self, i: usize, cx: &mut Context<Self>) {
        if i != self.sel_track && self.is_seq() {
            // the sequence join in ev_key/doc_ui_key rebuilds rows/chrome
            self.selection.clear();
        }
        self.sel_track = i;
        cx.notify();
    }

    /// UI timing mode for the loaded document — the explicit answer to
    /// "what does a tick mean here": metrical bar/beat or SMPTE timecode,
    /// never a synthesized 480 PPQ.
    fn td(&self) -> TimeDisplay {
        self.doc(|d| d.time_display())
    }

    /// Event id → semantic tag for events that participate in an RPN/NRPN
    /// write (selector or data entry) or look like stray data entry CCs.
    fn rpn_row_tags(doc: &Document) -> HashMap<EventId, String> {
        let mut tags = HashMap::new();
        for e in doc.rpn_entries() {
            let label = if e.is_null() {
                format!("{} null", if e.nrpn { "NRPN" } else { "RPN" })
            } else if let Some(name) = e.param_name() {
                format!(
                    "{} {}.{} {}",
                    if e.nrpn { "NRPN" } else { "RPN" },
                    e.param_msb,
                    e.param_lsb,
                    name
                )
            } else {
                format!(
                    "{} {}.{}",
                    if e.nrpn { "NRPN" } else { "RPN" },
                    e.param_msb,
                    e.param_lsb
                )
            };
            for id in e.ids() {
                tags.insert(id, label.clone());
            }
        }
        tags
    }

    fn build_event_rows(
        &self,
        doc: &Document,
    ) -> (Vec<EvRow>, Vec<Option<(usize, usize, EventId)>>) {
        let td = doc.time_display();
        let seq = doc.is_sequential();
        let hint = self.enc_override.or(doc.text_encoding_hint());
        let rpn_tags = Self::rpn_row_tags(doc);
        let pc_names: HashMap<EventId, (u8, u8, Option<String>)> = doc
            .program_changes()
            .iter()
            .map(|p| (p.id, (p.bank_msb, p.bank_lsb, doc.program_name(p))))
            .collect();
        let mut rows = Vec::new();
        let mut refs = Vec::new();
        for d in doc.diagnose() {
            rows.push(EvRow {
                tick: d.tick,
                text: format!("[{}] tk{} @{}", d.code, d.track + 1, d.tick).into(),
            });
            refs.push(None);
        }
        if seq {
            // explicit mode marker: these are sequences, not one timeline
            rows.push(EvRow {
                tick: 0,
                text: format!(
                    "[fmt2] sequence {}/{} — independent timelines",
                    self.sel_track + 1,
                    doc.tracks.len()
                )
                .into(),
            });
            refs.push(None);
        }
        for (ti, tr) in doc.tracks.iter().enumerate() {
            if seq && ti != self.sel_track {
                continue; // a format-2 event list shows one sequence
            }
            for (ei, e) in tr.events.iter().enumerate() {
                // bar.beat.tick for metrical, hh:mm:ss.ff timecode for
                // SMPTE — position labels always match the file's timing
                let pos = td.format_tick(e.tick);
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
                        // RPN/NRPN member CCs get their entry label; ch10
                        // notes get GM percussion names; program changes get
                        // bank + friendly patch name (unknown banks numeric)
                        let mut tag = String::new();
                        if status & 0xF0 == 0xB0 {
                            if matches!(data[0], 6 | 38 | 98..=101) {
                                let part = match data[0] {
                                    6 => "data-msb",
                                    38 => "data-lsb",
                                    99 | 101 => "sel-msb",
                                    _ => "sel-lsb",
                                };
                                tag = match rpn_tags.get(&e.id) {
                                    Some(label) => format!("  [{label} {part}]"),
                                    None if data[0] == 6 || data[0] == 38 => {
                                        "  [unbound data-entry]".into()
                                    }
                                    None => "  [unbound selector]".into(),
                                };
                            }
                        } else if status & 0xF0 == 0x90 || status & 0xF0 == 0x80 {
                            if (status & 0x0F) == 9 {
                                if let Some(d) = smf_core::gm_drum_name(data[0]) {
                                    tag = format!("  [{d}]");
                                }
                            }
                        } else if status & 0xF0 == 0xC0 {
                            if let Some((msb, lsb, name)) = pc_names.get(&e.id) {
                                tag = match name {
                                    Some(n) => format!("  bank {msb}.{lsb}  [{n}]"),
                                    None => format!("  bank {msb}.{lsb}"),
                                };
                            }
                        }
                        format!("{name} ch{ch:<2} {:>3} {:>3}{tag}", data[0], data[1])
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
                rows.push(EvRow {
                    tick: e.tick,
                    text: SharedString::from(format!("{pos:>11}  T{ti}  {body}")),
                });
                refs.push(Some((ti, ei, e.id)));
            }
        }
        (rows, refs)
    }

    fn apply_tx(&mut self, label: &str, ops: Vec<Op>) {
        let arc = self.shared.clone();
        let mut sh = lock_shared(&arc);
        match sh.apply(label, ops) {
            Ok(_) => self.refresh_derived_sh(&mut sh),
            Err(e) => self.status = tf("status.apply_failed", &[("e", &e.to_string())]).into(),
        }
    }

    // --- event-properties inspector -------------------------------------

    /// Click on an event-list row: plain = single-select + inspect, ctrl =
    /// toggle, shift = range from the last anchor. Diagnostic rows (no ref)
    /// just clear the inspector selection.
    fn ev_row_click(
        &mut self,
        row: usize,
        ctrl: bool,
        shift: bool,
        double: bool,
        cx: &mut Context<Self>,
    ) {
        self.ev_sel = row;
        let Some(&Some((_, _, id))) = self.event_refs.get(row) else {
            self.sel_events.clear();
            self.prop_field = None;
            self.meta_sel = None;
            cx.notify();
            return;
        };
        if shift {
            let anchor = self.ev_anchor.unwrap_or(row).min(self.event_refs.len() - 1);
            let (lo, hi) = (anchor.min(row), anchor.max(row));
            for r in &self.event_refs[lo..=hi] {
                if let Some((_, _, id)) = r {
                    self.sel_events.insert(*id);
                }
            }
        } else if ctrl {
            if !self.sel_events.remove(&id) {
                self.sel_events.insert(id);
            }
            self.ev_anchor = Some(row);
        } else {
            self.sel_events = BTreeSet::from([id]);
            self.ev_anchor = Some(row);
        }
        self.prop_field = None;
        // clicking a meta row arms the `e`/Del meta-edit shortcut path
        self.meta_sel = self
            .event_refs
            .get(row)
            .copied()
            .flatten()
            .filter(|&(tr, ei, _)| {
                self.doc(|d| {
                    matches!(
                        d.tracks.get(tr).and_then(|t| t.events.get(ei)),
                        Some(e) if matches!(e.kind, EventKind::Meta { .. })
                    )
                })
            })
            .map(|(tr, _, id)| (tr, id));
        // double-clicking a meta row opens the edit dialog (deferred to
        // render — this path has no Window)
        if double {
            if let Some((tr, id)) = self.meta_sel {
                let m = self.doc(|d| {
                    d.tracks.get(tr).and_then(|t| {
                        t.events
                            .iter()
                            .find(|e| e.id == id)
                            .and_then(|e| match &e.kind {
                                EventKind::Meta { meta_type, .. } => Some((e.tick, *meta_type)),
                                _ => None,
                            })
                    })
                });
                if let Some((tick, mt)) = m {
                    self.meta_pending = Some((tr, tick, mt, id));
                }
            }
        }
        cx.notify();
    }

    /// Header + rows for the inspector panel, in its current mode
    /// (event / note / track depending on what is selected).
    fn prop_rows(&self, d: &Document) -> (SharedString, Vec<PropRow>) {
        let mut rows = Vec::new();
        if !self.sel_events.is_empty() {
            let sel: Vec<(usize, usize, EventId)> = self
                .event_refs
                .iter()
                .flatten()
                .copied()
                .filter(|(_, _, id)| self.sel_events.contains(id))
                .collect();
            if sel.len() == 1 {
                let (ti, ei, _) = sel[0];
                let e = &d.tracks[ti].events[ei];
                return (
                    tf("prop.event_title", &[("t", &(ti + 1).to_string())]).into(),
                    event_prop_rows(e),
                );
            }
            for f in [
                PropField::Tick,
                PropField::Channel,
                PropField::D0,
                PropField::D1,
            ] {
                rows.push(PropRow {
                    field: Some(f),
                    label: prop_field_label(f, None),
                    value: String::new(),
                    warn: false,
                });
            }
            return (
                tf("prop.multi_title", &[("n", &sel.len().to_string())]).into(),
                rows,
            );
        }
        if !self.selection.is_empty() {
            if self.selection.len() == 1 {
                let on_id = *self.selection.iter().next().unwrap();
                if let Some(n) = d.notes().into_iter().find(|n| n.on_id == on_id) {
                    return (
                        tf("prop.note_title", &[("k", &n.key.to_string())]).into(),
                        note_prop_rows(&n, d),
                    );
                }
            }
            rows.push(PropRow {
                field: Some(PropField::NoteChannel),
                label: prop_field_label(PropField::NoteChannel, None),
                value: String::new(),
                warn: false,
            });
            rows.push(PropRow {
                field: Some(PropField::NoteVel),
                label: prop_field_label(PropField::NoteVel, None),
                value: String::new(),
                warn: false,
            });
            return (
                tf(
                    "prop.multi_title",
                    &[("n", &self.selection.len().to_string())],
                )
                .into(),
                rows,
            );
        }
        // track mode
        let ti = self.sel_track;
        if let Some(tr) = d.tracks.get(ti) {
            let name = tr
                .name
                .as_deref()
                .map(|b| smf_core::decode_text(b, self.enc_override.or(d.text_encoding_hint())))
                .unwrap_or_default();
            rows.push(PropRow {
                field: None,
                label: t("prop.name").into(),
                value: name,
                warn: false,
            });
            rows.push(PropRow {
                field: Some(PropField::TrackChannel),
                label: t("prop.channel").into(),
                value: (tr.out_channel + 1).to_string(),
                warn: false,
            });
            rows.push(PropRow {
                field: None,
                label: t("prop.port").into(),
                value: tr.out_port.to_string(),
                warn: false,
            });
            rows.push(PropRow {
                field: None,
                label: t("prop.count").into(),
                value: tr.events.len().to_string(),
                warn: false,
            });
        }
        (
            tf("prop.track_title", &[("t", &(ti + 1).to_string())]).into(),
            rows,
        )
    }

    /// What the inspector's active edit applies to.
    fn prop_target(&self) -> PropTarget {
        if !self.sel_events.is_empty() {
            return PropTarget::Events(
                self.event_refs
                    .iter()
                    .flatten()
                    .copied()
                    .filter(|(_, _, id)| self.sel_events.contains(id))
                    .collect(),
            );
        }
        if !self.selection.is_empty() {
            return PropTarget::Notes(self.selection.iter().copied().collect());
        }
        PropTarget::Track(self.sel_track)
    }

    /// Commit the inspector's active field: parse + validate first, then a
    /// single transaction covering every selected target. Invalid input
    /// lands in the status line and no transaction is created.
    fn prop_apply(&mut self, cx: &mut Context<Self>) {
        let Some(field) = self.prop_field else { return };
        let text = self.prop_input.read(cx).value().to_string();
        let target = self.prop_target();
        let result = {
            let mut sh = lock_shared(&self.shared);
            prop_edit_ops(&mut sh.doc, &target, field, &text)
        };
        match result {
            Err(e) => {
                self.status = e.into();
            }
            Ok(ops) if ops.is_empty() => {
                self.status = t("prop.unsupported").into();
            }
            Ok(ops) => {
                self.apply_tx("edit property", ops);
                self.status = t("prop.applied").into();
            }
        }
        cx.notify();
    }

    /// Load the inspector field's current value into the input and focus it.
    fn prop_edit(
        &mut self,
        field: PropField,
        value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.prop_field = Some(field);
        self.prop_input.update(cx, |i, cx| {
            i.set_value(value, window, cx);
        });
        let fh = self.prop_input.read(cx).focus_handle(cx);
        window.focus(&fh, cx);
        cx.notify();
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

    /// Track the tempo chip edits: the conductor for format 0/1, the
    /// viewed sequence for format 2.
    fn tempo_track(&self) -> usize {
        self.doc(|d| {
            if d.is_sequential() {
                self.sel_track.min(d.tracks.len().saturating_sub(1))
            } else {
                0
            }
        })
    }

    /// Split notes spanning the playhead: the selection when there is one,
    /// else every note on the selected track that straddles the line.
    fn split_at_playhead(&mut self, cx: &mut Context<Self>) {
        let at = self.doc(|d| d.tempo_map.us_to_tick(self.play_us));
        let ops = {
            let mut sh = lock_shared(&self.shared);
            if self.selection.is_empty() {
                sh.doc.split_ops(self.sel_track, 0, u64::MAX, at)
            } else {
                sh.doc.split_ids_ops(&self.selection, at)
            }
        };
        if ops.is_empty() {
            self.status = t("status.split_none").into();
        } else {
            self.apply_tx("split", ops);
            self.status = "split".into();
        }
        cx.notify();
    }

    /// Set the tick-0 tempo to current bpm + delta (via the shared op layer).
    fn bump_tempo(&mut self, delta: f64) {
        let tr = self.tempo_track();
        let cur = self.doc(|d| {
            d.tempo_map_for(tr)
                .points()
                .first()
                .map(|(_, mpq, _)| 60_000_000.0 / *mpq as f64)
                .unwrap_or(120.0)
        });
        let ops = {
            let mut sh = lock_shared(&self.shared);
            sh.doc
                .set_tempo_ops(tr, 0, (cur + delta).clamp(10.0, 400.0))
        };
        self.apply_tx("set tempo", ops);
    }

    /// Cycle the tick-0 time signature through common meters.
    fn cycle_time_sig(&mut self) {
        const SIGS: [(u8, u8); 6] = [(4, 4), (3, 4), (2, 4), (5, 4), (6, 8), (7, 8)];
        let tr = self.tempo_track();
        let cur = self.doc(|d| {
            d.tracks.get(tr).and_then(|t| {
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
            sh.doc.set_time_sig_ops(tr, 0, next.0, next.1)
        };
        self.apply_tx("set time signature", ops);
    }

    #[allow(dead_code)]
    fn insert_note(&mut self, tick: u64, key: u8, cx: &mut Context<Self>) {
        let len = self.snap_ticks().max(self.td().min_grid_ticks() as i64) as u64;
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
        self.sel_events.clear();
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

    /// Exact edit on the selected event-list row when it belongs to an
    /// RPN/NRPN entry: data events nudge the entered value (7-bit MSB step,
    /// 14-bit LSB step); selector events nudge the parameter number. The
    /// whole write stays in valid selector→data order because only the
    /// targeted bytes are rewritten.
    /// Semantic nudge for one event that belongs to an RPN/NRPN write:
    /// on a selector CC it moves the parameter number; on a data-entry CC
    /// it moves the written value. `None` when the event isn't in an entry.
    fn nudge_rpn_ops(sh: &mut Shared, id: EventId, delta: i32) -> Option<(Vec<Op>, String)> {
        let e = sh.doc.rpn_entry_containing(id)?;
        if e.sel_ids.contains(&id) {
            let param = (e.param14() as i32 + delta).clamp(0, 16383) as u16;
            Some((
                sh.doc
                    .update_rpn_param_ops(&e, (param >> 7) as u8, (param & 0x7F) as u8),
                format!("RPN {}.{}", param >> 7, param & 0x7F),
            ))
        } else if e.data_msb_id.is_some() || e.data_lsb_id.is_some() {
            let v = e.value()?;
            // 7-bit entries carry the value in data[1] itself; 14-bit
            // entries split it across CC6 (msb) + CC38 (lsb)
            let cap = if e.is_14bit() { 16383 } else { 127 };
            let nv = (v as i32 + delta).clamp(0, cap) as u16;
            let (msb, lsb) = if e.is_14bit() {
                ((nv >> 7) as u8, Some((nv & 0x7F) as u8))
            } else {
                (nv as u8, None)
            };
            Some((
                sh.doc.update_rpn_value_ops(&e, msb, lsb),
                format!("RPN = {nv}"),
            ))
        } else {
            Some((Vec::new(), String::new()))
        }
    }

    fn delete_selected(&mut self, cx: &mut Context<Self>) {
        if self.meta_sel.is_some() {
            self.delete_meta(cx);
            return;
        }
        let mut sh = lock_shared(&self.shared);
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
        // lane marquee selection + event-list row selection delete whole
        // events through the same transaction (skip ids a note op already
        // queues — a doubly-removed id would abort the whole transaction)
        let mut queued: BTreeSet<EventId> = BTreeSet::new();
        for op in &ops {
            if let Op::RemoveEvents { removed, .. } = op {
                queued.extend(removed.iter().map(|(_, e)| e.id));
            }
        }
        let extra: Vec<EventId> = self
            .lane_sel
            .iter()
            .copied()
            .chain(self.sel_events.iter().copied())
            .filter(|id| !queued.contains(id))
            .collect();
        // an event that is part of an RPN/NRPN write deletes the whole
        // parameter entry — removing a lone selector/data CC would leave
        // a corrupt half-write in the file
        let mut entries: Vec<document::RpnEntry> = Vec::new();
        let mut entry_ids: BTreeSet<EventId> = BTreeSet::new();
        for id in &extra {
            if entry_ids.contains(id) {
                continue;
            }
            if let Some(e) = sh.doc.rpn_entry_containing(*id) {
                entry_ids.extend(e.ids());
                entries.push(e);
            }
        }
        for e in &entries {
            ops.extend(sh.doc.remove_rpn_entry_ops(e));
        }
        queued.extend(entry_ids.iter().copied());
        let extra: Vec<EventId> = extra
            .into_iter()
            .filter(|id| !queued.contains(id))
            .collect();
        ops.extend(sh.doc.remove_events_ops(&extra));
        drop(sh);
        if !ops.is_empty() {
            self.apply_tx("delete", ops);
        }
        self.selection.clear();
        self.lane_sel.clear();
        self.sel_events.clear();
        cx.notify();
    }

    /// Adjust the event-list selection's primary value by `delta`
    /// (velocity/CC/pressure → data[1]; program/channel pressure → data[0];
    /// pitch bend → 14-bit). Exact per-event editing from the keyboard.
    fn nudge_sel_events(&mut self, delta: i32, cx: &mut Context<Self>) {
        if self.sel_events.is_empty() {
            return;
        }
        let wanted: BTreeSet<EventId> = self.sel_events.clone();
        let mut sh = lock_shared(&self.shared);
        let mut ops = Vec::new();
        // events inside an RPN/NRPN write take the semantic path: nudging
        // a selector moves the parameter number, nudging a data-entry CC
        // rewrites the whole entry coherently. Everything else gets the
        // plain data-byte nudge.
        let mut labels = Vec::new();
        let mut raw: BTreeSet<EventId> = wanted.clone();
        for id in &wanted {
            if let Some((eops, lbl)) = Self::nudge_rpn_ops(&mut sh, *id, delta) {
                ops.extend(eops);
                if !lbl.is_empty() {
                    labels.push(lbl);
                }
                raw.remove(id);
            }
        }
        for (ti, t) in sh.doc.tracks.iter().enumerate() {
            for e in &t.events {
                if !raw.contains(&e.id) {
                    continue;
                }
                let EventKind::Channel { status, .. } = &e.kind else {
                    continue;
                };
                let status = *status;
                let mut after = e.clone();
                let EventKind::Channel { data: d, .. } = &mut after.kind else {
                    unreachable!();
                };
                match status & 0xF0 {
                    0x90 | 0x80 | 0xA0 | 0xB0 => {
                        let lo = if status & 0xF0 == 0x90 { 1 } else { 0 };
                        d[1] = (d[1] as i32 + delta).clamp(lo, 127) as u8;
                    }
                    0xC0 | 0xD0 => d[0] = (d[0] as i32 + delta).clamp(0, 127) as u8,
                    0xE0 => {
                        let v = ((d[1] as i32) << 7 | d[0] as i32) + delta;
                        let v = v.clamp(0, 16383);
                        d[0] = (v & 0x7F) as u8;
                        d[1] = (v >> 7) as u8;
                    }
                    _ => continue,
                }
                ops.push(Op::UpdateEvent {
                    track: ti,
                    before: e.clone(),
                    after,
                });
            }
        }
        drop(sh);
        if !ops.is_empty() {
            self.apply_tx("edit event", ops);
            if let Some(l) = labels.first() {
                self.status = l.clone().into();
            }
        }
        cx.notify();
    }

    /// Open the meta edit dialog. `id > 0` edits that event (prefilled);
    /// `id == 0` creates a new meta at (track, tick).
    fn open_meta_edit(
        &mut self,
        track: usize,
        tick: u64,
        meta_type: u8,
        id: EventId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sh = lock_shared(&self.shared);
        let cur = if id != 0 {
            sh.doc
                .tracks
                .get(track)
                .and_then(|t| t.events.iter().find(|e| e.id == id))
                .and_then(|e| match &e.kind {
                    EventKind::Meta {
                        meta_type: mt,
                        data,
                    } if *mt == meta_type => {
                        if meta_type == 0x59 && data.len() >= 2 {
                            Some(format!(
                                "{} {}",
                                data[0] as i8,
                                if data[1] == 1 { "minor" } else { "major" }
                            ))
                        } else {
                            Some(smf_core::decode_text(data, None))
                        }
                    }
                    _ => None,
                })
                .unwrap_or_default()
        } else {
            String::new()
        };
        drop(sh);
        self.meta_input.update(cx, |i, cx| {
            i.set_value(cur, window, cx);
        });
        self.meta_edit = Some(MetaEdit {
            track,
            tick,
            meta_type,
            id,
        });
        window.focus(&self.meta_input.read(cx).focus_handle(cx), cx);
        cx.notify();
    }

    /// Apply the dialog: encode via `enc_override` (UTF-8 default) for text
    /// metas, parse "sf minor" / key names for key signature.
    fn commit_meta_edit(&mut self, cx: &mut Context<Self>) {
        let Some(me) = self.meta_edit.take() else {
            return;
        };
        let text = self.meta_input.read(cx).value().to_string();
        let ops = {
            let mut sh = lock_shared(&self.shared);
            if me.meta_type == 0x59 {
                match parse_key_sig(&text) {
                    Some((sf, mi)) => sh.doc.set_key_sig_ops(me.tick, sf, mi),
                    None => {
                        self.meta_edit = Some(me);
                        self.status = t("status.keysig_parse").into();
                        cx.notify();
                        return;
                    }
                }
            } else {
                sh.doc.set_meta_text_ops(
                    me.track,
                    me.tick,
                    me.meta_type,
                    me.id,
                    &text,
                    self.enc_override,
                )
            }
        };
        if ops.is_empty() {
            self.status = t("status.meta_none").into();
        } else {
            self.apply_tx("meta", ops);
        }
        self.meta_refocus = true;
        cx.notify();
    }

    /// Delete the marker/meta selected via strip click or `[`/`]` nav.
    fn delete_meta(&mut self, cx: &mut Context<Self>) {
        let Some((track, id)) = self.meta_sel else {
            return;
        };
        let ops = {
            let mut sh = lock_shared(&self.shared);
            sh.doc.remove_meta_ops(track, id)
        };
        if ops.is_empty() {
            self.status = t("status.meta_none").into();
        } else {
            self.apply_tx("delete meta", ops);
            self.meta_sel = None;
        }
        cx.notify();
    }

    /// Meta type label for the dialog title + menu.
    fn meta_type_label(meta_type: u8) -> &'static str {
        match meta_type {
            0x01 => "meta.text",
            0x02 => "meta.copyright",
            0x04 => "meta.instrument",
            0x05 => "meta.lyric",
            0x06 => "meta.marker",
            0x07 => "meta.cue",
            0x59 => "meta.keysig",
            _ => "meta.text",
        }
    }

    /// Copy the selection into the note clipboard (`cut` also deletes it).
    fn copy_selected(&mut self, cut: bool, cx: &mut Context<Self>) {
        let min_len = self.td().min_grid_ticks();
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
                    .unwrap_or(n.start_tick + min_len)
                    .saturating_sub(n.start_tick)
                    .max(1),
                vel: n.vel,
                off_vel: n.off_vel,
                off_via_on: n.off_via_on,
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
            // format 2: paste lands in the viewed sequence regardless of
            // which sequence the clipboard notes were copied from
            let seq_target = sh
                .doc
                .is_sequential()
                .then(|| self.sel_track.min(ntr.saturating_sub(1)));
            let mut per_track: BTreeMap<usize, Vec<DocEvent>> = BTreeMap::new();
            for c in items {
                let track = seq_target.unwrap_or_else(|| c.track.min(ntr.saturating_sub(1)));
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
                            // keep the copied note's off form: a 0x90v0
                            // can't carry release velocity; 0x80 can
                            status: (if c.off_via_on { 0x90 } else { 0x80 }) | ch,
                            data: [c.key, if c.off_via_on { 0 } else { c.off_vel }],
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
        // the anchor lives in the viewed sequence's timeline (per-seq map)
        let anchor = self
            .snap_down(
                self.doc(|d| d.tempo_map_for(self.sel_track).us_to_tick(self.play_us)) as i64,
            )
            .max(0) as u64;
        let src = self.clipboard.clone();
        self.insert_clip(&src, anchor, "paste notes", cx);
    }

    /// Duplicate the selection, tiled immediately after it (Ctrl+D).
    fn duplicate_selected(&mut self, cx: &mut Context<Self>) {
        let min_len = self.td().min_grid_ticks();
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
                    .unwrap_or(n.start_tick + min_len)
                    .saturating_sub(n.start_tick)
                    .max(1),
                vel: n.vel,
                off_vel: n.off_vel,
                off_via_on: n.off_via_on,
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
                    // the pitch moves on BOTH ends — an off left at the old
                    // key leaves the on dangling and re-pairs wrong
                    if let EventKind::Channel { data, .. } = &mut after.kind {
                        data[0] = nk;
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

    // --- keyboard focus & navigation ----------------------------------------

    /// The focus area currently holding keyboard focus. The root handle and
    /// the roll handle both count as `Roll` — the canvas is the default
    /// editing context.
    fn area(&self, window: &Window, cx: &App) -> FocusArea {
        if self.menu_fh.contains_focused(window, cx) {
            FocusArea::MenuBar
        } else if self.tracks_fh.contains_focused(window, cx) {
            FocusArea::Tracks
        } else if self.lane_fh.contains_focused(window, cx) {
            FocusArea::Lane
        } else if self.events_fh.contains_focused(window, cx) {
            FocusArea::Events
        } else {
            FocusArea::Roll
        }
    }

    /// Focus handle for a region (with visibility fallbacks).
    fn fh_for(&self, area: FocusArea) -> FocusHandle {
        match area {
            FocusArea::MenuBar => self.menu_fh.clone(),
            FocusArea::Tracks => self.tracks_fh.clone(),
            FocusArea::Roll => self.roll_fh.clone(),
            FocusArea::Lane => self.lane_fh.clone(),
            FocusArea::Events if self.show_events => self.events_fh.clone(),
            FocusArea::Events => self.roll_fh.clone(),
        }
    }

    /// Focus can never be absent or land on a hidden region — reroute it.
    /// Called from render each frame so nothing can leave focus invisible.
    fn repair_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.focused(cx).is_none() {
            window.focus(&self.roll_fh, cx);
        }
        if !self.show_events && self.events_fh.contains_focused(window, cx) {
            window.focus(&self.roll_fh, cx);
        }
    }

    /// Open dropdown `m` under its menubar label (used by mouse and keys).
    fn open_menu_at(&mut self, m: TopMenu, cx: &mut Context<Self>) {
        self.open_menu = Some((m, menu_x(m)));
        self.open_sub = None;
        self.menu_sel = None;
        self.sub_sel = None;
        self.menu_bar_sel = MENUS.iter().position(|(mm, _, _)| *mm == m).unwrap_or(0);
        cx.notify();
    }

    /// Switch the open dropdown to a neighbouring menubar entry.
    fn menu_sibling(&mut self, dir: i64, cx: &mut Context<Self>) {
        let Some((m, _)) = self.open_menu else {
            return;
        };
        let i = MENUS.iter().position(|(mm, _, _)| *mm == m).unwrap_or(0) as i64;
        let ni = (i + dir).rem_euclid(MENUS.len() as i64) as usize;
        self.open_menu_at(MENUS[ni].0, cx);
    }

    /// Arrow-key movement inside the open dropdown/cascade.
    fn menu_step(&mut self, dir: i32, cx: &mut Context<Self>) {
        if self.open_sub.is_some() {
            self.sub_sel = next_selectable(&self.sub_rows, self.sub_sel, dir);
        } else {
            self.menu_sel = next_selectable(&self.menu_rows, self.menu_sel, dir);
        }
        cx.notify();
    }

    /// Open the cascade for the selected submenu row.
    fn open_selected_sub(&mut self, cx: &mut Context<Self>) {
        if let Some(i) = self.menu_sel {
            if let Some(MenuRow::Sub { sub, .. }) = self.menu_rows.get(i) {
                let y = row_y(&self.menu_rows, i);
                self.open_sub = Some((*sub, y));
                self.sub_sel = None;
            }
        }
        cx.notify();
    }

    /// Enter/Space on the highlighted row: run a leaf, descend into a ▸ row.
    fn menu_activate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_sub.is_none() {
            if let Some(i) = self.menu_sel {
                if matches!(self.menu_rows.get(i), Some(MenuRow::Sub { .. })) {
                    self.open_selected_sub(cx);
                    return;
                }
            }
        }
        let (rows, sel) = if self.open_sub.is_some() {
            (&self.sub_rows, self.sub_sel)
        } else {
            (&self.menu_rows, self.menu_sel)
        };
        let act = match sel.and_then(|i| rows.get(i)) {
            Some(MenuRow::Leaf(l)) => Some(l.act.clone()),
            _ => None,
        };
        if let Some(act) = act {
            self.open_menu = None;
            self.open_sub = None;
            self.menu_sel = None;
            self.sub_sel = None;
            act(self, window, cx);
            // restore focus to the region the command ran against
            let fh = self.fh_for(self.last_area);
            window.focus(&fh, cx);
        }
    }

    /// Keyboard control for the open menu. Returns true when the key was
    /// consumed; unhandled keys keep bubbling so chords still work.
    fn menu_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.open_menu.is_none() {
            return false;
        }
        match ev.keystroke.key.as_str() {
            "left" => {
                if self.open_sub.is_some() {
                    self.open_sub = None;
                    self.sub_sel = None;
                    cx.notify();
                } else {
                    self.menu_sibling(-1, cx);
                }
            }
            "right" => {
                if self.open_sub.is_none() {
                    let is_sub = self
                        .menu_sel
                        .and_then(|i| self.menu_rows.get(i))
                        .is_some_and(|r| matches!(r, MenuRow::Sub { .. }));
                    if is_sub {
                        self.open_selected_sub(cx);
                    } else {
                        self.menu_sibling(1, cx);
                    }
                }
            }
            "up" => self.menu_step(-1, cx),
            "down" => self.menu_step(1, cx),
            "enter" | " " | "space" => {
                // nothing highlighted yet: select the first row rather than
                // firing it — Enter activates on the next press
                if self.menu_sel.is_none() && self.sub_sel.is_none() {
                    self.menu_step(1, cx);
                } else {
                    self.menu_activate(window, cx);
                }
            }
            "escape" => {
                self.open_menu = None;
                self.open_sub = None;
                self.menu_sel = None;
                self.sub_sel = None;
                cx.notify();
            }
            _ => return false,
        }
        true
    }

    /// Arrows on the roll: nudge the selection, or move the edit cursor when
    /// nothing is selected (Enter inserts at the cursor).
    fn roll_arrow(&mut self, dtick: i64, dkey: i32, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            self.cursor_move(dtick, dkey, cx);
        } else {
            self.nudge(dtick, dkey, cx);
        }
    }

    /// Note length used by cursor inserts — same rule as `insert_note`.
    fn cursor_insert_len(&self) -> u64 {
        self.snap_ticks().max(self.ppq() as i64 / 4) as u64
    }

    /// Move the roll edit cursor and scroll it into view.
    fn cursor_move(&mut self, dtick: i64, dkey: i32, cx: &mut Context<Self>) {
        self.cursor_tick = (self.cursor_tick as i64 + dtick).max(0) as u64;
        self.cursor_key = (self.cursor_key + dkey).clamp(0, 127);
        self.ensure_cursor_visible();
        cx.notify();
    }

    /// Scroll the roll so the edit cursor is on screen with a small margin.
    fn ensure_cursor_visible(&mut self) {
        let b = self.roll_bounds.get();
        let w = f32::from(b.size.width);
        let h = f32::from(b.size.height);
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let x0 = self.cursor_tick as f32 * self.zoom;
        let x1 = x0 + self.cursor_insert_len() as f32 * self.zoom;
        let margin = 32.0;
        if x0 < self.scroll_x + margin {
            self.scroll_x = (x0 - margin).max(0.0);
        } else if x1 > self.scroll_x + w - margin {
            self.scroll_x = (x1 - w + margin).max(0.0);
        }
        let row_top = (127 - self.cursor_key) as f32 * NOTE_H;
        let row_bot = row_top + NOTE_H;
        if row_top < self.scroll_y + margin {
            self.scroll_y = (row_top - margin).max(0.0);
        } else if row_bot > self.scroll_y + h - margin {
            self.scroll_y = row_bot - h + margin;
        }
        self.clamp_scroll();
    }

    /// Enter on the roll: select the note under the cursor, or insert a new
    /// note at it when the cell is empty.
    fn cursor_activate(&mut self, cx: &mut Context<Self>) {
        let end = self.cursor_tick + self.cursor_insert_len();
        let hit = self.notes.iter().find(|n| {
            n.track == self.sel_track
                && n.key == self.cursor_key as u8
                && n.start_tick < end
                && n.end_tick.unwrap_or(n.start_tick + 1) > self.cursor_tick
        });
        if let Some(n) = hit {
            self.selection = BTreeSet::from([n.on_id]);
            cx.notify();
        } else {
            let len = self.cursor_insert_len();
            self.insert_note_len(self.cursor_tick, self.cursor_key as u8, len, cx);
        }
    }

    /// ±track selection from the keyboard.
    fn track_step(&mut self, dir: i64, cx: &mut Context<Self>) {
        let n = self.doc(|d| d.tracks.len());
        if n == 0 {
            return;
        }
        let i = (self.sel_track as i64 + dir).clamp(0, n as i64 - 1) as usize;
        if i != self.sel_track {
            self.sel_track = i;
            cx.notify();
        }
    }

    fn toggle_mute(&mut self, i: usize) {
        {
            let mut sh = lock_shared(&self.shared);
            if !sh.muted.remove(&i) {
                sh.muted.insert(i);
            }
        }
        self.persist();
    }

    fn toggle_solo(&mut self, i: usize) {
        {
            let mut sh = lock_shared(&self.shared);
            if !sh.soloed.remove(&i) {
                sh.soloed.insert(i);
            }
        }
        self.persist();
    }

    fn cycle_chan(&mut self, i: usize) {
        let ops = {
            let mut sh = lock_shared(&self.shared);
            let cur = sh.doc.tracks.get(i).map(|t| t.out_channel).unwrap_or(0);
            sh.doc.set_track_channel_ops(i, (cur + 1) % 16)
        };
        self.apply_tx("set track channel", ops);
    }

    /// Apply the rename field to the selected track.
    fn apply_rename(&mut self, cx: &mut Context<Self>) {
        let name = self.input.read(cx).value().to_string();
        if name.is_empty() {
            return;
        }
        let ops = {
            let mut sh = lock_shared(&self.shared);
            sh.doc.set_track_name_ops(self.sel_track, &name)
        };
        self.apply_tx("set track name", ops);
    }

    /// Enter in the rename field: commit the name and hand focus back to
    /// the track list so focus never stays trapped in the input.
    fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_rename(cx);
        window.focus(&self.tracks_fh, cx);
        cx.notify();
    }

    /// Up/down in the lane: ±velocity on the selected notes.
    fn nudge_vel(&mut self, dv: i32, cx: &mut Context<Self>) {
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
                let nv = (n.vel as i32 + dv).clamp(1, 127) as u8;
                if nv == n.vel {
                    continue;
                }
                let Some(track) = sh.doc.tracks.get(n.track) else {
                    continue;
                };
                for e in track.events.iter() {
                    if e.id != n.on_id {
                        continue;
                    }
                    let mut after = e.clone();
                    if let EventKind::Channel { data, .. } = &mut after.kind {
                        data[1] = nv;
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
            self.apply_tx("set velocity", ops);
        }
        cx.notify();
    }

    /// Move the event-list selection by `dir` rows, scrolling it into view.
    fn ev_step(&mut self, dir: i64, cx: &mut Context<Self>) {
        let n = self.events.len();
        if n == 0 {
            return;
        }
        let i = (self.ev_sel as i64 + dir).clamp(0, n as i64 - 1) as usize;
        if i != self.ev_sel {
            self.ev_sel = i;
            // keyboard navigation selects like a plain click — the
            // inspector follows the cursor row
            if let Some(&Some((_, _, id))) = self.event_refs.get(i) {
                self.sel_events = BTreeSet::from([id]);
                self.prop_field = None;
            }
            self.events_scroll
                .scroll_to_item(i, ScrollStrategy::Nearest);
            cx.notify();
        }
    }

    /// Enter on the event list: move the playhead to the row's tick.
    fn ev_activate(&mut self, cx: &mut Context<Self>) {
        if let Some(row) = self.events.get(self.ev_sel) {
            let tick = row.tick;
            self.seek_to_tick(tick, false, cx);
        }
    }

    /// Move the playhead to `tick`; `play` (or an already-playing transport)
    /// restarts the engine from there.
    fn seek_to_tick(&mut self, tick: u64, play: bool, cx: &mut Context<Self>) {
        self.play_us = self.doc(|d| d.tempo_map_for(self.sel_track).tick_to_us(tick));
        if play || self.playback.is_some() {
            self.stop_playback();
            self.start_playback();
        }
        cx.notify();
    }

    fn commit_drag(&mut self, cx: &mut Context<Self>) {
        // every release ends any sounding preview (draw scrub, pitch drag,
        // key strip) — the worker's own deadline is the backstop
        let was_scrub = self.scrub_key.is_some();
        self.audition_off();
        if was_scrub {
            cx.notify();
        }
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
                // rect select: notes intersecting the rubber-band box — the
                // box corners are keys from hit(); compare in row space so a
                // folded view selects exactly the visible rows it covers.
                // Same sequence gate as note_at/edge_at: a marquee must
                // never select another sequence's ghosts for deletion.
                let (t0, t1) = (d.a_tick.min(d.b_tick), d.a_tick.max(d.b_tick));
                let row_sel = |key: i32| -> i32 {
                    if (0..=127).contains(&key) {
                        self.row_of[key as usize]
                    } else {
                        -1
                    }
                };
                let (r0, r1) = (
                    row_sel(d.a_key).min(row_sel(d.b_key)),
                    row_sel(d.a_key).max(row_sel(d.b_key)),
                );
                let seq = self.is_seq();
                self.selection = self
                    .notes
                    .iter()
                    .filter(|n| {
                        (!seq || n.track == self.sel_track) && {
                            let st = n.start_tick as i64;
                            let en = n.end_tick.unwrap_or(n.start_tick) as i64;
                            let row = self.row_of[n.key as usize];
                            st <= t1 && en >= t0 && row >= r0 && row <= r1
                        }
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
            DragMode::LaneMarquee => {
                // rubber-band inside a lane: select every lane event in the
                // (tick, value) box; Velocity mode selects notes instead
                let (t0, t1) = (d.a_tick.min(d.b_tick).max(0), d.a_tick.max(d.b_tick).max(0));
                let (v0, v1) = (d.a_key.min(d.b_key), d.a_key.max(d.b_key));
                let cfg = self.lanes.get(d.lane).copied().unwrap_or_default();
                if cfg.mode == LaneMode::Velocity {
                    self.selection = self
                        .notes
                        .iter()
                        .filter(|n| {
                            n.track == self.sel_track
                                && n.start_tick as i64 >= t0
                                && n.start_tick as i64 <= t1
                                && n.vel as i32 >= v0
                                && n.vel as i32 <= v1
                        })
                        .map(|n| n.on_id)
                        .collect();
                } else {
                    self.lane_sel = self
                        .lane_events_cached(cfg.mode, cfg.poly_key)
                        .iter()
                        .filter(|(_, tick, val, _key)| {
                            *tick as i64 >= t0 && *tick as i64 <= t1 && *val >= v0 && *val <= v1
                        })
                        .map(|(id, _, _, _)| *id)
                        .collect();
                }
                cx.notify();
                return;
            }
            DragMode::LaneResize => {
                // the height already tracked the cursor in update_drag —
                // committing only persists the new layout
                self.persist();
                cx.notify();
                return;
            }
            DragMode::LaneEvent => {
                // CC/PB/AT lane: update an existing event's value, or insert a
                // new one when the drag started on empty lane space
                // snap before locking: snap_down -> doc() re-acquires `sh`
                let ins_tick = self.snap_down(d.a_tick).max(0) as u64;
                let mut sh = lock_shared(&self.shared);
                let mut ops = Vec::new();
                // the track may be gone (MCP remove/undo during the drag)
                let Some(track_events) = sh.doc.tracks.get(d.track) else {
                    drop(sh);
                    cx.notify();
                    return;
                };
                let cfg = self.lanes.get(d.lane).copied().unwrap_or_default();
                let lane_mode = cfg.mode;
                if d.on_id == 0 {
                    let ch = track_events.out_channel & 0x0F;
                    let (status, data, len) = match lane_mode {
                        LaneMode::CC(cc) => (0xB0 | ch, [cc, d.dkey.clamp(0, 127) as u8], 2u8),
                        LaneMode::PitchBend => {
                            let v = d.dkey.clamp(0, 16383) as u16;
                            (0xE0 | ch, [(v & 0x7F) as u8, (v >> 7) as u8], 2)
                        }
                        LaneMode::ChanAT => (0xD0 | ch, [d.dkey.clamp(0, 127) as u8, 0], 1),
                        LaneMode::PolyAT => {
                            let key = cfg.poly_key.unwrap_or_else(|| {
                                // no filter: use the key of a note at that tick,
                                // else middle C
                                self.notes
                                    .iter()
                                    .filter(|n| {
                                        n.track == d.track
                                            && n.start_tick <= d.a_tick.max(0) as u64
                                            && n.end_tick.unwrap_or(u64::MAX)
                                                > d.a_tick.max(0) as u64
                                    })
                                    .min_by_key(|n| n.start_tick)
                                    .map(|n| n.key)
                                    .unwrap_or(60)
                            });
                            (0xA0 | ch, [key, d.dkey.clamp(0, 127) as u8], 2)
                        }
                        LaneMode::Velocity => unreachable!(),
                    };
                    let id = sh.doc.alloc_event_id();
                    ops.push(Op::InsertEvents {
                        track: d.track,
                        events: vec![DocEvent {
                            id,
                            tick: ins_tick,
                            seq: 0,
                            raw_body: None,
                            kind: EventKind::Channel { status, data, len },
                        }],
                    });
                } else {
                    for e in &track_events.events {
                        if e.id == d.on_id {
                            let mut after = e.clone();
                            if let EventKind::Channel { data, .. } = &mut after.kind {
                                match lane_mode {
                                    LaneMode::CC(_) => data[1] = d.dkey.clamp(0, 127) as u8,
                                    LaneMode::PitchBend => {
                                        let v = d.dkey.clamp(0, 16383) as u16;
                                        data[0] = (v & 0x7F) as u8;
                                        data[1] = (v >> 7) as u8;
                                    }
                                    LaneMode::ChanAT => data[0] = d.dkey.clamp(0, 127) as u8,
                                    LaneMode::PolyAT => data[1] = d.dkey.clamp(0, 127) as u8,
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
        if self.save_rx.is_some() {
            return; // one save in flight — a second Ctrl+S isn't queued
        }
        // check the backing file's identity before touching it — a save
        // must never silently overwrite somebody else's changes
        let path = lock_shared(&self.shared).path.clone();
        if let Some(p) = &path {
            match watch::check_file(p, self.file_stamp) {
                watch::FileEvent::Unchanged => {}
                // timestamp/size drifted but content is identical — safe
                // to write, just adopt the fresh stamp
                watch::FileEvent::Touched(s) => self.file_stamp = Some(s),
                ev => {
                    self.prompt_save_conflict(p.clone(), ev, cx);
                    return;
                }
            }
        }
        self.write_to(path.as_deref(), cx);
    }

    /// The actual write shared by save() and the conflict resolutions —
    /// the stamp re-check is skipped because the caller already decided
    /// to write (Overwrite/Recreate resolutions).
    fn write_file(&mut self, p: &std::path::Path, cx: &mut Context<Self>) {
        self.write_to(Some(p), cx)
    }

    /// The same persistence core MCP save uses: snapshot under a short
    /// lock, then serialize+write on a worker so a slow save never
    /// freezes the UI. `file_stamp` re-baselines only after the durable
    /// replace lands, so a failed write can't make the next conflict
    /// check blind. `p == None` falls back to the document's own path.
    fn write_to(&mut self, p: Option<&std::path::Path>, cx: &mut Context<Self>) {
        let req = mcp_server::service::SaveRequest {
            path: p,
            ..Default::default()
        };
        match mcp_server::service::begin_save(&self.shared, req) {
            Ok(ticket) => {
                // non-modal progress only when the save can be perceptible
                if ticket.event_count() > 100_000 {
                    self.status = t("status.saving").into();
                }
                let shared = self.shared.clone();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let _ = tx
                        .send(mcp_server::service::finish_save(ticket).map_err(|e| e.to_string()));
                    drop(shared);
                });
                self.save_rx = Some(rx);
            }
            Err(mcp_server::service::SaveError::NoPath) => {
                self.save_as(cx);
                return;
            }
            Err(e) => {
                tracing::error!(path = ?p, error = %e, "save failed");
                self.status = format!("{e}").into();
            }
        }
        cx.notify();
    }

    /// Synchronous save to the current backing path through the same shared
    /// save core — used by the discard guard, which must know the write
    /// finished before it proceeds. Returns false — leaving the document
    /// dirty — when there is no path (callers route through a Save-As
    /// prompt) or the write fails; the caller still sees the error in the
    /// status line.
    fn try_save(&mut self, cx: &mut Context<Self>) -> bool {
        let path = lock_shared(&self.shared).path.clone();
        let ok = match mcp_server::service::save_document(&self.shared, Default::default()) {
            Ok(_) => {
                self.status = t("status.saved").into();
                self.persist();
                recovery::clear_recovery();
                if let Some(p) = &path {
                    self.file_stamp = watch::stat_file(p);
                }
                self.ext_prompted = false;
                true
            }
            Err(mcp_server::service::SaveError::NoPath) => false,
            Err(e) => {
                self.status = format!("{e}").into();
                false
            }
        };
        cx.notify();
        ok
    }

    /// The guard's save step — `NeedsPath` sends the flow through a
    /// Save-As prompt instead of writing silently.
    fn save_for_guard(&mut self, cx: &mut Context<Self>) -> guard::SaveOutcome {
        if lock_shared(&self.shared).path.is_none() {
            return guard::SaveOutcome::NeedsPath;
        }
        if self.try_save(cx) {
            guard::SaveOutcome::Saved
        } else {
            guard::SaveOutcome::Failed
        }
    }

    /// Run an action the discard guard cleared (or that never needed it).
    fn perform_pending(&mut self, action: PendingAction, cx: &mut Context<Self>) {
        match action {
            PendingAction::NewFile => self.new_file(cx),
            PendingAction::OpenDialog => self.open_dialog(cx),
            PendingAction::OpenPath(p) => self.open(p, cx),
            PendingAction::CloseWindow => {
                self.close_confirmed = true;
                if let Some(wh) = self.window_handle {
                    wh.update(cx, |_, w, _app| w.remove_window()).ok();
                }
            }
        }
    }

    /// The save-time conflict prompt: Reload (take the disk version,
    /// discarding local changes), Save As (keep local under a new path),
    /// Overwrite (explicit — destroy the external change), or Cancel.
    fn prompt_save_conflict(&mut self, p: PathBuf, ev: watch::FileEvent, cx: &mut Context<Self>) {
        if self.prompt_active {
            self.status = t("watch.save_blocked").into();
            cx.notify();
            return;
        }
        let Some(wh) = self.window_handle else {
            self.status = t("watch.save_blocked").into();
            cx.notify();
            return;
        };
        let name = p.display().to_string();
        let (title, detail, answers) = match ev {
            watch::FileEvent::Missing => (
                t("watch.missing_title").to_string(),
                tf("watch.missing_detail", &[("p", &name)]),
                vec![
                    PromptButton::Ok(t("watch.save_as").into()),
                    PromptButton::Other(t("watch.recreate").into()),
                    PromptButton::Cancel(t("watch.cancel").into()),
                ],
            ),
            _ => (
                t("watch.changed_title").to_string(),
                tf("watch.changed_detail", &[("p", &name)]),
                vec![
                    PromptButton::Other(t("watch.reload").into()),
                    PromptButton::Ok(t("watch.save_as").into()),
                    PromptButton::Other(t("watch.overwrite").into()),
                    PromptButton::Cancel(t("watch.cancel").into()),
                ],
            ),
        };
        self.prompt_active = true;
        // open the prompt from a deferred task: save() is invoked inside
        // the window's own event-handler update, and nesting a window
        // update there is rejected — spawning moves it outside the handler
        cx.spawn(async move |this, cx| {
            let rx = wh
                .update(cx, |_, w, app| {
                    w.prompt(PromptLevel::Warning, &title, Some(&detail), &answers, app)
                })
                .ok();
            let Some(rx) = rx else {
                this.update(cx, |v, cx| {
                    v.prompt_active = false;
                    v.status = t("watch.save_blocked").into();
                    cx.notify();
                })
                .ok();
                return;
            };
            let idx = rx.await.unwrap_or(usize::MAX);
            this.update(cx, |v, cx| {
                v.prompt_active = false;
                v.resolve_save_conflict(&p, ev, idx, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Apply the save-conflict answer. Index follows the button order built
    /// in `prompt_save_conflict`; any unexpected index is Cancel.
    fn resolve_save_conflict(
        &mut self,
        p: &PathBuf,
        ev: watch::FileEvent,
        idx: usize,
        cx: &mut Context<Self>,
    ) {
        match ev {
            // [Reload, Save As, Overwrite, Cancel]
            watch::FileEvent::Modified => match idx {
                // Reload = the only path that resets undo/history — and it
                // only happens through this explicit choice
                0 => self.open(p.clone(), cx),
                1 => self.save_as(cx),
                2 => self.write_file(p, cx),
                _ => {
                    self.status = t("watch.save_cancelled").into();
                    cx.notify();
                }
            },
            // [Save As, Recreate, Cancel]
            watch::FileEvent::Missing => match idx {
                0 => self.save_as(cx),
                1 => self.write_file(p, cx),
                _ => {
                    self.status = t("watch.save_cancelled").into();
                    cx.notify();
                }
            },
            _ => {}
        }
    }

    /// While-open poll, called from the doc-watch loop (~2s cadence).
    /// Surfaces external modification or deletion with a prompt; `Touched`
    /// just re-baselines silently. One prompt per episode — `ext_prompted`
    /// releases only when the stamp is re-baselined by open/save.
    fn check_external_change(&mut self, cx: &mut Context<Self>) {
        if self.last_ext_check.elapsed() < std::time::Duration::from_secs(2) {
            return;
        }
        self.last_ext_check = std::time::Instant::now();
        if self.ext_prompted || self.prompt_active {
            return;
        }
        let Some(p) = lock_shared(&self.shared).path.clone() else {
            return;
        };
        match watch::check_file(&p, self.file_stamp) {
            watch::FileEvent::Unchanged => {}
            watch::FileEvent::Touched(s) => self.file_stamp = Some(s),
            ev => {
                self.ext_prompted = true;
                self.prompt_ext_change(p, ev, cx);
            }
        }
    }

    /// The while-open notice. Clean doc: Enter=Reload is safe (nothing is
    /// lost). Dirty doc: Enter=Keep Editing — Reload stays available but
    /// can't be triggered by a reflex Enter.
    fn prompt_ext_change(&mut self, p: PathBuf, ev: watch::FileEvent, cx: &mut Context<Self>) {
        let Some(wh) = self.window_handle else {
            self.ext_prompted = false;
            return;
        };
        let dirty = {
            let sh = lock_shared(&self.shared);
            sh.doc.revision() != sh.saved_revision
        };
        let name = p.display().to_string();
        let (title, detail, answers) = match (ev, dirty) {
            (watch::FileEvent::Missing, _) => (
                t("watch.missing_title").to_string(),
                tf("watch.missing_open_detail", &[("p", &name)]),
                vec![
                    PromptButton::Ok(t("watch.keep").into()),
                    PromptButton::Other(t("watch.save_as").into()),
                ],
            ),
            (watch::FileEvent::Modified, false) => (
                t("watch.changed_title").to_string(),
                tf("watch.changed_open_detail", &[("p", &name)]),
                vec![
                    PromptButton::Ok(t("watch.reload").into()),
                    PromptButton::Other(t("watch.keep").into()),
                ],
            ),
            _ => (
                t("watch.changed_title").to_string(),
                tf("watch.changed_dirty_detail", &[("p", &name)]),
                vec![
                    PromptButton::Ok(t("watch.keep").into()),
                    PromptButton::Other(t("watch.reload").into()),
                ],
            ),
        };
        let Ok(rx) = wh.update(cx, |_, w, app| {
            w.prompt(PromptLevel::Warning, &title, Some(&detail), &answers, app)
        }) else {
            self.ext_prompted = false;
            return;
        };
        self.prompt_active = true;
        cx.spawn(async move |this, cx| {
            let idx = rx.await.unwrap_or(usize::MAX);
            this.update(cx, |v, cx| {
                v.prompt_active = false;
                v.resolve_ext_change(&p, ev, dirty, idx, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Index meaning depends on the button order chosen by
    /// `prompt_ext_change`: Missing = [Keep, Save As]; clean-modified =
    /// [Reload, Keep]; dirty-modified = [Keep, Reload].
    fn resolve_ext_change(
        &mut self,
        p: &PathBuf,
        ev: watch::FileEvent,
        dirty: bool,
        idx: usize,
        cx: &mut Context<Self>,
    ) {
        match ev {
            watch::FileEvent::Missing => {
                if idx == 1 {
                    self.save_as(cx);
                }
            }
            watch::FileEvent::Modified => {
                let reload = if dirty { idx == 1 } else { idx == 0 };
                if reload {
                    self.open(p.clone(), cx);
                }
            }
            _ => {}
        }
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

    /// Help → Open Logs: reveal the rolling log directory in Explorer.
    fn open_logs(&mut self, cx: &mut Context<Self>) {
        let dir = diagnostics::log_dir();
        if std::fs::create_dir_all(&dir).is_ok() {
            diagnostics::open_in_explorer(&dir);
            self.status = tf("status.logs_dir", &[("p", &dir.display().to_string())]).into();
        } else {
            self.status = t("status.logs_open_failed").into();
        }
        cx.notify();
    }

    /// Help → Export Diagnostics Bundle: sanitized env facts + redacted
    /// tails of the retained logs, one attachable text file.
    fn export_diagnostics(&mut self, cx: &mut Context<Self>) {
        let (dests, mcp_auth) = {
            let sh = lock_shared(&self.shared);
            (
                sh.dests
                    .iter()
                    .map(|(_, d)| format!("{d:?}"))
                    .collect::<Vec<_>>()
                    .join(", "),
                std::env::var("MIDI_MCP_TOKEN").is_ok(),
            )
        };
        let hd = output::host_diag();
        let host_lines = format!(
            "app_version={}\naudio_device={:?}\nhelper={:?}\nprobe={:?}\ndests={}\nmcp_auth={}\ncount_in={} midi_in={}",
            env!("CARGO_PKG_VERSION"),
            hd.audio_device,
            hd.helper,
            hd.probe,
            dests,
            mcp_auth,
            self.count_in,
            self.midi_in,
        );
        let dir = diagnostics::app_data_dir().join("diagnostics");
        match diagnostics::export_bundle(&dir, &host_lines) {
            Ok(p) => {
                self.status =
                    tf("status.bundle_written", &[("p", &p.display().to_string())]).into();
                diagnostics::reveal_file(&p);
            }
            Err(e) => {
                tracing::error!(error = %e, "diagnostics bundle export failed");
                self.status = tf("status.bundle_failed", &[("e", &e.to_string())]).into();
            }
        }
        cx.notify();
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

    /// Post-swap view reset shared by open() and snapshot restore:
    /// everything that referenced the old document is cleared or rebuilt.
    fn reset_view_for_new_doc(&mut self) {
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
        // rebuild the derived views, then land the view on the new content
        self.refresh_derived();
        self.reset_view_to_content();
    }

    fn open(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        match mcp_server::service::load_document(&path) {
            Ok((d, load_warnings)) => {
                tracing::info!(
                    path = %path.display(),
                    warnings = load_warnings.len(),
                    "document opened"
                );
                // an armed recording belongs to the previous document —
                // drop it with a warning instead of silently losing the take
                let rec_discarded = self.rec.take().is_some();
                self.stop_playback();
                // flush plugin state while the outgoing song's path (and its
                // state file) is still the active one — `apply_prefs` loads
                // the incoming song's table after the swap
                self.flush_plugin_states(true);
                // swap the document in place — the MCP server holds this same Arc
                mcp_server::service::swap_document(&self.shared, d, Some(path.clone()));
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
                // rebuild the derived views, then land the view on the new
                // content — a saved per-file sidecar (applied next) overrides
                self.refresh_derived();
                self.reset_view_to_content();
                // the freshly-opened file is the new identity baseline
                self.file_stamp = watch::stat_file(&path);
                self.ext_prompted = false;
                let pref_diags = self.apply_prefs(&path);
                self.push_recent(&path);
                // the previous document's snapshots no longer apply
                recovery::clear_recovery();
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
                if !pref_diags.is_empty() {
                    status = format!(
                        "{status} — {}",
                        tf("status.prefs_warn", &[("e", &pref_diags.join("; "))])
                    );
                }
                if rec_discarded {
                    status = format!("{status} — {}", t("status.rec_discarded"));
                }
                self.status = status.into();
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "open failed");
                self.status = tf("status.load_failed", &[("e", &e.to_string())]).into();
            }
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
                Some(output::Destination::Plugin { plugin_path, .. }) => PathBuf::from(plugin_path),
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
        // index now points at a different bundle — retire the old instance,
        // capturing its state first so a re-point doesn't lose the patch
        if retire && self.plugin_slots.contains_key(&d) {
            self.capture_plugin_state(d);
            self.state_restored.remove(&d);
            self.restart_logged.remove(&d);
            self.plugin_slots.remove(&d);
            let _ = self.plugin_req.send(output::PluginReq::Drop(d));
            // an audition sink bound to that slot is stale too
            if self.aud_ships.remove(&d) {
                self.audition.drop_sink(d);
            }
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
        let _ = self
            .plugin_req
            .send(output::PluginReq::Open(d, path, self.audio_sel.clone()));
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

    /// Apply + persist a new audio configuration for hosted plugins. Every
    /// live instance is reopened onto it (state is preserved across the
    /// reopen, so a rate/buffer/device change doesn't lose the program);
    /// loading/failed slots pick it up on their next open attempt.
    fn apply_audio_selection(&mut self, sel: output::AudioSelection) {
        self.audio_sel = sel;
        self.save_global();
        for d in self.plugin_slots.keys().copied().collect::<Vec<_>>() {
            self.ensure_plugin(d, true);
        }
    }

    fn poll_plugin_events(&mut self) -> bool {
        let mut changed = false;
        let now = std::time::Instant::now();
        // hot-plug: re-enumerate output devices every few seconds so the
        // audio settings panel tracks additions/removals while it's open
        if now.duration_since(self.audio_devices_at) >= std::time::Duration::from_secs(3) {
            self.audio_devices = output::output_devices();
            self.audio_devices_at = now;
        }
        let timed_out: Vec<usize> = self.plugin_state.iter().filter_map(|(&d, s)| {
            matches!(s, PluginState::Loading { since, .. } if now.duration_since(*since) >= std::time::Duration::from_secs(20)).then_some(d)
        }).collect();
        for d in timed_out {
            if let Some(PluginState::Loading { path, .. }) = self.plugin_state.remove(&d) {
                tracing::warn!(dest = d, path = %path.display(), "plugin load timed out");
                self.plugin_state.insert(
                    d,
                    PluginState::Failed {
                        path,
                        phase: "load",
                        msg: t("plugin.timeout").to_string(),
                    },
                );
                let _ = self.plugin_req.send(output::PluginReq::Drop(d));
                if self.aud_ships.remove(&d) {
                    self.audition.drop_sink(d);
                }
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
                    tracing::info!(dest = event.dest, plugin = %name, "plugin ready");
                    // fresh instance → fresh notification log
                    self.restart_logged.remove(&event.dest);
                    self.state_restored.remove(&event.dest);
                    self.plugin_slots.insert(event.dest, slot);
                    // saved sidecar state goes in before Ready — playback may
                    // start as soon as this slot reports ready
                    self.restore_plugin_state(event.dest);
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
                    tracing::warn!(
                        dest = event.dest,
                        plugin = %name,
                        phase,
                        error = %e,
                        "plugin load failed"
                    );
                    self.state_restored.remove(&event.dest);
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
        // VST3 restart notifications (IComponentHandler::restartComponent):
        // serviced per ready slot each frame. service_host_requests runs the
        // VST3-required stop/deactivate/reactivate lifecycle on the isolated
        // helper's control thread and returns every flag raised; the audit
        // then reacts where the host tracks state (latency re-read, bus
        // re-query, component reload) and logs the rest once per instance.
        // try_lock keeps a busy audio block from stalling a UI frame — the
        // next frame picks the flags up.
        let mut drained: Vec<(usize, vst3_host::RestartFlags)> = Vec::new();
        // dest indexes whose stream errored mid-flight (device unplugged) —
        // reopened below, throttled so a device that never comes up doesn't
        // hot-loop stream builds
        let mut lost: Vec<(usize, String)> = Vec::new();
        for (d, slot) in &self.plugin_slots {
            if !matches!(self.plugin_state.get(d), Some(PluginState::Ready { .. })) {
                continue;
            }
            if let Some(err) = slot.take_stream_error() {
                lost.push((*d, err));
            }
            if let Some(flags) = slot
                .plugin
                .try_lock()
                .ok()
                .and_then(|mut p| p.service_host_requests().ok())
            {
                if !flags.is_empty() {
                    drained.push((*d, flags));
                }
            }
        }
        let mut reloads = Vec::new();
        for (d, flags) in drained {
            changed |= self.apply_restart_flags(d, flags, &mut reloads);
        }
        for d in reloads {
            self.ensure_plugin(d, true);
            changed = true;
        }
        // device loss: reopen — the backend falls back to the current
        // default when the selected device is gone, so this recovers onto
        // whatever output still exists instead of staying dead
        for (d, err) in lost {
            let cooled = self
                .audio_retry
                .get(&d)
                .map(|t| t.elapsed() >= std::time::Duration::from_secs(5))
                .unwrap_or(true);
            if !cooled {
                continue;
            }
            self.audio_retry.insert(d, std::time::Instant::now());
            self.status = tf("audio.device_lost", &[("e", err.as_str())]).into();
            self.ensure_plugin(d, true);
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

    /// Route one plugin's drained `restartComponent` flags through the
    /// notification audit (`output::restart_notes` → `restart_action`).
    /// Reactions needing a `&mut self` follow-up after the drain loop
    /// (instance reloads) are queued on `reloads` for the caller.
    fn apply_restart_flags(
        &mut self,
        d: usize,
        flags: vst3_host::RestartFlags,
        reloads: &mut Vec<usize>,
    ) -> bool {
        let mut changed = false;
        let name = self
            .plugin_slots
            .get(&d)
            .and_then(|s| s.path.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_default();
        for note in output::restart_notes(flags) {
            match output::restart_action(note) {
                output::RestartAction::RefreshLatency => {
                    if let Some(slot) = self.plugin_slots.get(&d) {
                        let n = slot.refresh_latency();
                        tracing::info!("{name} (dest {d}): kLatencyChanged -> {n} samples");
                    }
                    changed = true;
                }
                output::RestartAction::RequeryIo => {
                    // the lifecycle already ran inside service_host_requests;
                    // nothing caches the layout, so re-query it for the log
                    let ch = self
                        .plugin_slots
                        .get(&d)
                        .and_then(|s| s.plugin.try_lock().ok().map(|p| p.output_channel_count()));
                    tracing::info!("{name} (dest {d}): kIoChanged -> {ch:?} output channel(s)");
                }
                output::RestartAction::Reload => {
                    tracing::info!("{name} (dest {d}): kReloadComponent -> reopening");
                    reloads.push(d);
                    changed = true;
                }
                output::RestartAction::LogOnly => {
                    if self.restart_logged.entry(d).or_default().first_seen(note) {
                        tracing::info!(
                            "{name} (dest {d}): {} noted — no host state to rebuild",
                            note.name()
                        );
                    }
                }
            }
        }
        changed
    }

    fn start_playback(&mut self) {
        self.audition_off();
        // snapshot routing state so no lock is held while opening sinks
        let (
            dests,
            dest_of_track,
            muted,
            soloed,
            metronome,
            loop_enabled,
            chase_sysex,
            sequential,
            sxp,
        ) = {
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
                sh.doc.is_sequential(),
                sh.sysex_policy,
            )
        };
        let sxp_cfg = midi_io::SysexConfig {
            policy: sxp,
            ..Default::default()
        };
        self.sysex_stats.clear();
        let dest_of = |t: usize| dest_of_track.get(&t).copied().unwrap_or(0);
        if dests.is_empty() {
            self.status = t("status.no_port").into();
            return;
        }
        let sel_track = self.sel_track;
        let audible = |tr: usize| {
            if !soloed.is_empty() {
                // explicit solo is the opt-in way to hear sequences together
                soloed.contains(&tr)
            } else if sequential {
                // format 2: only the viewed sequence plays — sequences are
                // independent patterns, not lanes of one song
                tr == sel_track && !muted.contains(&tr)
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
        // dest index -> the plugin's transport lane: tempo/meter map events
        // ride the same schedule as notes and apply at block boundaries
        let mut transport_of: HashMap<usize, usize> = HashMap::new();
        self.poll_plugin_events();
        for d in needed {
            let Some((_, dest)) = dests.get(d) else {
                continue;
            };
            match dest {
                output::Destination::MidiPort { port_name, ord } => {
                    // bind by (name, ord) — a same-name sibling must never
                    // silently take over this destination
                    match midi_io::Output::open_ord(port_name, *ord) {
                        Ok(out) => {
                            let sink = PortSink::with_config(out, sxp_cfg);
                            self.sysex_stats.push(sink.stats());
                            sink_of.insert(d, sinks.len());
                            sinks.push(Box::new(sink));
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
                        transport_of.insert(d, sinks.len());
                        sinks.push(Box::new(output::TransportSink::new(slot.plugin.clone())));
                        if let Ok(mut p) = slot.plugin.lock() {
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
        events.extend(
            tagged
                .into_iter()
                .filter_map(|(us, tr, b)| sink_of.get(&dest_of(tr)).map(|&s| (us, s, b))),
        );
        if metronome {
            // prefer a plain MIDI port for clicks; fall back to any sink
            let click_sink = dests
                .iter()
                .enumerate()
                .find(|(_, (_, d))| matches!(d, output::Destination::MidiPort { .. }))
                .and_then(|(d, _)| sink_of.get(&d).copied())
                .or_else(|| sink_of.values().next().copied());
            if let Some(s) = click_sink {
                // one click per beat / per second — never a fake-PPQ beat
                let click = self.td().click_ticks();
                let end_us = events.iter().map(|e| e.0).max().unwrap_or(0);
                let mut beat = 0u64;
                loop {
                    let us = self.doc(|d| d.tempo_map_for(self.sel_track).tick_to_us(beat * click));
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
        // transport map -> scheduled updates on each plugin's transport
        // lane, plus the state in effect at the start position (the chase —
        // on loop wrap it replays from the loop point's partition, so the
        // wrap restores loop-start tempo/meter before the next boundary)
        let transport_pts = self.transport_points();
        for &s in transport_of.values() {
            for (us, cmd) in &transport_pts {
                events.push((*us, s, output::encode_transport(cmd)));
            }
            for (us, cmd) in output::chase_transport(&transport_pts, start_us) {
                events.push((us, s, output::encode_transport(&cmd)));
            }
        }
        // globally sort before seek partitioning and the chase splice —
        // transport payloads join the same ordered stream
        events.sort_by_key(|e| e.0);
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
        self.audition_off();
        self.play_pending = false;
        if let Some(mut p) = self.playback.take() {
            self.play_us = p.position_us();
            p.stop();
        }
        // surface the long-message diagnostic for the pass that just ended:
        // a dump that was deferred or dropped is silent unless reported
        let (mut inl, mut def, mut drop_n, mut worst) = (0u64, 0u64, 0u64, 0u64);
        for s in self.sysex_stats.drain(..) {
            let (i, d, x, _l, m) = s.snapshot();
            inl += i;
            def += d;
            drop_n += x;
            worst = worst.max(m);
        }
        if drop_n > 0 || def > 0 {
            let (i, d, x, ms) = (
                inl.to_string(),
                def.to_string(),
                drop_n.to_string(),
                (worst / 1000).to_string(),
            );
            self.status = tf(
                "status.sysex_diag",
                &[("i", &i), ("d", &d), ("x", &x), ("ms", &ms)],
            )
            .into();
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
        // playback can move plugin state (CC-mapped params); queue a capture
        for d in self.plugin_slots.keys() {
            self.pending_state_capture.insert(*d);
        }
        self.finish_record();
    }

    /// The document's tempo map + meter map as scheduled transport updates
    /// `(µs, TransportCmd)`, sorted by µs. `TransportSink`s drive these into
    /// hosted plugins so their `ProcessContext` follows mid-song changes at
    /// audio block boundaries instead of staying at the head values.
    /// Format-2 documents schedule from the viewed sequence's own maps.
    fn transport_points(&self) -> Vec<(u64, output::TransportCmd)> {
        self.doc(|d| {
            let tr = if d.is_sequential() {
                Some(self.sel_track.min(d.tracks.len().saturating_sub(1)))
            } else {
                None
            };
            transport_points_for(d, tr)
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
        // optional count-in: one bar for metrical, one second for SMPTE
        let cin_us = if self.count_in {
            self.doc(|d| {
                d.tempo_map_for(self.sel_track)
                    .tick_to_us(self.td().bar_ticks())
            })
        } else {
            0
        };
        let cb = move |us, b: &[u8]| {
            buf2.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((us, b.to_vec()));
        };
        let diag = midi_io::InputDiag::new();
        let opts = midi_io::InputOpts {
            latency_us: self.in_latency_ms * 1000,
            diag: Some(diag.clone()),
        };
        let opened = if self.midi_in.is_empty() {
            midi_io::Input::open_opts(0, opts, cb)
        } else {
            midi_io::Input::open_named_opts(&self.midi_in, opts, cb)
        };
        match opened {
            Ok(input) => {
                self.rec = Some(Rec {
                    input,
                    input_lost: false,
                    buf,
                    base_us: self.play_us,
                    cin_us,
                    diag,
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
            // the take lands in the selected track — for format 2 that
            // sequence's own tempo map converts live-µs back to ticks
            let tick = sh
                .doc
                .tempo_map_for(track)
                .us_to_tick(rec.base_us + (us - rec.cin_us));
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
        // timing diagnostic: how much callback delivery delay the backend
        // timestamps absorbed — would have been recorded as timing error
        {
            use std::sync::atomic::Ordering::Relaxed;
            let (st, un, gap) = (
                rec.diag.stamped.load(Relaxed),
                rec.diag.unstamped.load(Relaxed),
                rec.diag.gap_max_us.load(Relaxed),
            );
            tracing::debug!(
                "rec input timing: {st} device-stamped, {un} arrival-fallback, worst callback delay {}ms",
                gap / 1000
            );
        }
        drop(sh);
        if n == 0 {
            self.status = t("status.rec_no_events").into();
            return;
        }
        self.apply_tx("record", vec![Op::InsertEvents { track, events }]);
        self.status = tf("status.rec_done", &[("n", &n.to_string())]).into();
    }

    fn rescan_plugins(&mut self, mode: ScanMode) {
        self.status = t("status.scanning").into();
        let (tx, rx) = std::sync::mpsc::channel();
        self.scan_rx = Some(rx);
        let cache_file = scan_cache_path();
        let timeout = std::time::Duration::from_secs(self.probe_timeout_secs);
        let handle = std::thread::spawn(move || {
            let (all, retry) = match &mode {
                ScanMode::All => (true, None),
                ScanMode::Retry(p) => (false, Some(p.as_path())),
                ScanMode::Changed => (false, None),
            };
            let _ = tx.send(output::discover_plugins_cached(
                Some(&cache_file),
                timeout,
                all,
                retry,
            ));
        });
        self.shutdown.track_scan(handle);
    }

    /// Ordered, bounded teardown of everything the app owns. Runs from
    /// `on_window_should_close` and `on_app_quit`; the Shutdown latch
    /// makes the second call a no-op.
    fn perform_shutdown(&mut self) {
        let mut sd = std::mem::take(&mut self.shutdown);
        sd.run(self);
        self.shutdown = sd;
    }

    fn apply_catalog(&mut self, report: output::ScanReport) {
        self.scan_probe_used = Some(report.probe_used);
        self.scan_cached = report.cached_ok;
        self.quarantined = report.quarantined.clone();
        self.plugin_meta = report
            .plugins
            .iter()
            .cloned()
            .map(|p| (p.path.to_string_lossy().into_owned(), p))
            .collect();
        let fmt_skip = |(p, r): &(PathBuf, String)| {
            format!(
                "{} — {}",
                p.file_stem()
                    .map(|s| s.to_string_lossy())
                    .unwrap_or_default(),
                r
            )
        };
        let mut note: Vec<String> = report.skipped.iter().map(fmt_skip).collect();
        note.extend(
            report
                .quarantined
                .iter()
                .map(|s| format!("{}: {}", t("output.quarantined_short"), fmt_skip(s))),
        );
        self.scan_note = if note.is_empty() {
            None
        } else {
            Some(note.join("; "))
        };
        let fresh = build_dest_catalog(&report.plugins);
        tracing::info!(
            dests = fresh.len(),
            skipped = report.skipped.len(),
            probe_used = report.probe_used,
            "destination catalog applied"
        );
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
            .map(|d| {
                // identity-aware lookup (path or component id), then re-add
                // offline entries rather than dropping the assignment —
                // they play again once replugged
                match sh.dests.iter().position(|(_, dd)| dd.same_identity(&d)) {
                    Some(i) => i,
                    None => sh.ensure_dest(&dest_label(&d), d),
                }
            })
            .unwrap_or(0);
        sh.track_dest = old_tracks
            .into_iter()
            .map(|(t, d)| {
                let i = match sh.dests.iter().position(|(_, dd)| dd.same_identity(&d)) {
                    Some(i) => i,
                    None => sh.ensure_dest(&dest_label(&d), d),
                };
                (t, i)
            })
            .collect();
        let n = sh.dests.len();
        drop(sh);
        if let Some(mut pw) = self.plugin_window.take() {
            pw.close();
        }
        self.editor_plugin = None;
        // dest indices were just remapped — every slot is stale; capture
        // their state first so a rescan doesn't lose dialed-in patches
        self.capture_all_plugin_states();
        self.state_restored.clear();
        self.plugin_slots.clear();
        self.restart_logged.clear();
        self.plugin_state.clear();
        // audition sinks key on the same indexes — rebuild on next strike
        self.aud_ships.clear();
        self.aud_failed.clear();
        self.audition.clear_sinks();
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
                output::Destination::Plugin { plugin_path, .. } => Some(PathBuf::from(plugin_path)),
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

    /// Copy the in-process editor's full state into the playing instance —
    /// covers program/bank changes parameter-edit draining can't see.
    fn push_editor_state(
        &mut self,
        d: usize,
        editor: &std::sync::Arc<std::sync::Mutex<vst3_host::Plugin>>,
    ) {
        let Some(slot) = self.plugin_slots.get(&d) else {
            return;
        };
        let data = editor.lock().ok().and_then(|e| e.save_state().ok());
        if let Some(data) = data {
            if let Ok(mut p) = slot.plugin.lock() {
                let _ = p.load_state(&data);
            }
            // the playing instance's state just changed wholesale — persist it
            self.pending_state_capture.insert(d);
            self.flush_plugin_states(true);
        }
    }

    /// Capture one warm slot's component+controller state into the per-song
    /// store. The blob is keyed by the loaded plugin's own class uid so the
    /// record follows the component when the bundle path moves; while a uid
    /// is unknown the bundle path is the key. No-op without a readable slot.
    fn capture_plugin_state(&mut self, d: usize) {
        let Some(slot) = self.plugin_slots.get(&d) else {
            return;
        };
        let (uid, blob) = match slot.plugin.lock() {
            Ok(p) => match p.save_state() {
                Ok(b) => (p.info().uid.clone(), b),
                Err(_) => return,
            },
            Err(_) => return,
        };
        let key = slot.path.to_string_lossy().into_owned();
        let meta = self.plugin_meta.get(&key);
        let changed = self.plugin_states.insert(plugin_state::PluginStateRecord {
            uid,
            path: key.clone(),
            vendor: meta.map(|m| m.vendor.clone()).unwrap_or_default(),
            name: meta.map(|m| m.name.clone()).unwrap_or_else(|| {
                slot.path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            }),
            version: meta.map(|m| m.version.clone()).unwrap_or_default(),
            saved_unix_ms: plugin_state::now_unix_ms(),
            state: blob,
        });
        self.state_file_dirty |= changed;
    }

    /// Snapshot every warm slot — used at doc swaps and catalog rebuilds,
    /// where the instances the state belongs to are about to be retired.
    fn capture_all_plugin_states(&mut self) {
        let ds: Vec<usize> = self.plugin_slots.keys().copied().collect();
        for d in ds {
            self.capture_plugin_state(d);
        }
    }

    /// Drain pending captures into the store and write the companion file
    /// when records changed. `force` bypasses the ~1 s write throttle used
    /// by the periodic tick — teardown points (persist, doc swap, rescan,
    /// editor close) always force so a quick exit can't strand state.
    fn flush_plugin_states(&mut self, force: bool) {
        let pending = std::mem::take(&mut self.pending_state_capture);
        for d in pending {
            self.capture_plugin_state(d);
        }
        if !self.state_file_dirty {
            return;
        }
        let doc_path = lock_shared(&self.shared).path.clone();
        let Some(doc_path) = doc_path else {
            // untitled document: keep records in memory until Save As gives
            // the song (and its sidecars) a home
            return;
        };
        if !force && self.last_state_write.elapsed() < std::time::Duration::from_secs(1) {
            return;
        }
        match self
            .plugin_states
            .save(&plugin_state::state_path(&doc_path))
        {
            Ok(_) => {
                self.state_file_dirty = false;
                self.last_state_write = std::time::Instant::now();
            }
            Err(e) => tracing::warn!("plugin state write failed: {e}"),
        }
    }

    /// Push the song's saved state into a warm or just-loaded slot. Runs once
    /// per (re)load — `state_restored` prevents a double `load_state` when a
    /// doc is reopened over still-warm instances. A non-empty record uid that
    /// disagrees with the loaded plugin's real uid is rejected as
    /// incompatible; any failure is a status line note, never fatal — the
    /// plugin still loads and plays with its defaults.
    fn restore_plugin_state(&mut self, d: usize) {
        if self.state_restored.contains(&d) {
            return;
        }
        let Some(slot) = self.plugin_slots.get(&d) else {
            return;
        };
        let plugin = slot.plugin.clone();
        let path = slot.path.clone();
        let uid = plugin
            .lock()
            .map(|p| p.info().uid.clone())
            .unwrap_or_default();
        let err = {
            let Some(rec) = self.plugin_states.lookup(&uid, &path) else {
                return;
            };
            if !rec.uid.is_empty() && !uid.is_empty() && rec.uid != uid {
                Some("class id mismatch".to_string())
            } else {
                match plugin.lock() {
                    Ok(mut p) => p.load_state(&rec.state).err().map(|e| e.to_string()),
                    Err(_) => None,
                }
            }
        };
        // mark even on failure — a rejected blob should not retry every tick
        self.state_restored.insert(d);
        if let Some(e) = err {
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.status = tf(
                "plugin.state_restore_failed",
                &[("name", name.as_str()), ("e", e.as_str())],
            )
            .into();
        }
    }

    /// `push_editor_state` on editor close, then the handle is released.
    fn sync_editor_state_into_slot(&mut self) {
        let Some((d, editor)) = self.editor_plugin.take() else {
            return;
        };
        self.push_editor_state(d, &editor);
    }

    /// tick,key under a window-space mouse position
    fn hit(&self, pos: Point<Pixels>) -> (i64, i32) {
        let b = self.roll_bounds.get();
        let x = f32::from(pos.x) - f32::from(b.origin.x);
        let y = f32::from(pos.y) - f32::from(b.origin.y);
        roll_hit(
            x,
            y,
            self.scroll_x,
            self.scroll_y,
            self.zoom,
            self.note_h,
            &self.vis_keys,
        )
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
        // minimap pan counts as a manual scroll — pause follow briefly
        self.follow_hold = Some(std::time::Instant::now() + FOLLOW_HOLD);
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
            DragMode::Velocity | DragMode::LaneEvent | DragMode::LaneMarquee => {
                let li = self.drag.as_ref().map(|d| d.lane).unwrap_or(0);
                let Some(cell) = self.lane_bounds.get(li) else {
                    return;
                };
                let b = cell.get();
                let x = f32::from(pos.x) - f32::from(b.origin.x);
                let y = f32::from(pos.y) - f32::from(b.origin.y);
                let h = f32::from(b.size.height).max(1.0);
                let vrange = self.lanes.get(li).map(|c| c.mode.vrange()).unwrap_or(127.0);
                let val = ((1.0 - y / h) * vrange) as i32;
                if let Some(d) = self.drag.as_mut() {
                    d.dkey = val;
                    if d.mode == DragMode::LaneMarquee {
                        d.b_tick = ((x + self.scroll_x) / self.zoom) as i64;
                        d.b_key = val;
                    }
                }
            }
            DragMode::LaneResize => {
                // `orig_start` carries the grab-time height in centipx;
                // `a_tick` the pointer y it was grabbed at
                let grabbed = self.drag.as_ref().map(|d| (d.lane, d.orig_start, d.a_tick));
                if let Some((li, h0, y0)) = grabbed {
                    let dy = f32::from(pos.y) - y0 as f32;
                    if let Some(c) = self.lanes.get_mut(li) {
                        c.h = (h0 as f32 / 100.0 + dy).clamp(LANE_H_MIN, LANE_H_MAX);
                    }
                }
            }
            _ => {
                let (tick, key) = self.hit(pos);
                // erase stroke: every note swept joins the pending delete set
                let erase_id = (mode == DragMode::Erase)
                    .then(|| self.note_at(pos).or_else(|| self.edge_at(pos)))
                    .flatten()
                    .map(|n| n.on_id);
                // preview strike to send after the drag borrow is released —
                // (track, ch, key, vel, at_tick) when a pitch drag moves
                let mut strike = None;
                if let Some(d) = self.drag.as_mut() {
                    match d.mode {
                        DragMode::Move | DragMode::Duplicate => {
                            let (dt, dk) = clamp_move_delta(
                                tick - d.orig_start as i64,
                                d.orig_start,
                                key - d.orig_key as i32,
                                d.orig_key,
                            );
                            if dk != d.dkey {
                                strike = Some((
                                    d.track,
                                    d.aud_ch,
                                    (d.orig_key as i32 + dk).clamp(0, 127) as u8,
                                    d.aud_vel,
                                    (d.orig_start as i64 + dt).max(0) as u64,
                                ));
                            }
                            d.dtick = dt;
                            d.dkey = dk;
                        }
                        DragMode::Resize => {
                            d.dtick = tick - d.orig_end.unwrap_or(d.orig_start) as i64;
                        }
                        DragMode::Marquee => {
                            if self.tool == Tool::Draw && key != d.b_key {
                                strike = Some((
                                    d.track,
                                    d.aud_ch,
                                    key.clamp(0, 127) as u8,
                                    d.aud_vel,
                                    tick.max(0) as u64,
                                ));
                            }
                            d.b_tick = tick;
                            d.b_key = key;
                        }
                        _ => {}
                    }
                }
                if let Some((tr, ch, k, v, at)) = strike {
                    self.audition_strike(tr, ch, k, v, at);
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
        if !matches!(
            mode,
            DragMode::Velocity | DragMode::LaneEvent | DragMode::LaneMarquee | DragMode::LaneResize
        ) {
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
        let seq = self.is_seq();
        self.notes
            .iter()
            .rev()
            // ghosts of other sequences are display-only — clicks can't
            // select or drag them (a ghost's lane is ambiguous anyway)
            .filter(|n| !seq || n.track == self.sel_track)
            .find(|n| {
                n.key as i32 == key
                    && tick >= n.start_tick as i64
                    && tick
                        <= n.end_tick
                            .unwrap_or(n.start_tick + self.td().min_grid_ticks())
                            as i64
            })
            .cloned()
    }

    /// Note whose right edge is within ~6px of `pos` — a resize target.
    fn edge_at(&self, pos: Point<Pixels>) -> Option<Note> {
        let (tick, key) = self.hit(pos);
        let seq = self.is_seq();
        self.notes
            .iter()
            .rev()
            .filter(|n| !seq || n.track == self.sel_track)
            .find(|n| {
                n.key as i32 == key
                    && n.end_tick.is_some()
                    && tick >= n.start_tick as i64
                    && ((n.end_tick.unwrap() as i64) - tick) as f32 * self.zoom <= 6.0
                    && ((n.end_tick.unwrap() as i64) - tick) as f32 * self.zoom >= -2.0
            })
            .cloned()
    }

    // --- note audition (issue #39) -------------------------------------------

    /// Channel the selected track's previews route through.
    fn sel_track_ch(&self) -> u8 {
        self.doc(|d| {
            d.tracks
                .get(self.sel_track)
                .map(|t| t.out_channel & 0x0F)
                .unwrap_or(0)
        })
    }

    /// Piano-key under a window-space position on the key strip — row
    /// position maps through `vis_keys` so folded/drum views audition the
    /// visible row under the cursor.
    fn kbd_key(&self, pos: Point<Pixels>) -> Option<u8> {
        let b = self.kbd_bounds.get();
        let y = f32::from(pos.y) - f32::from(b.origin.y);
        let row = ((y + self.scroll_y) / self.note_h) as i32;
        (row >= 0)
            .then(|| self.vis_keys.get(row as usize).copied())
            .flatten()
    }

    /// Make sure the audition worker holds a sink for destination `d`.
    /// Ports are opened once then owned by the worker; plugins reuse the
    /// already-warm slot (a still-loading plugin simply skips this strike —
    /// the next click works once the slot is ready).
    fn audition_sink(&mut self, d: usize) -> bool {
        if self.aud_ships.contains(&d) {
            return true;
        }
        let dest = lock_shared(&self.shared)
            .dests
            .get(d)
            .map(|(_, dd)| dd.clone());
        match dest {
            Some(output::Destination::MidiPort { port_name, ord }) => {
                match midi_io::Output::open_ord(&port_name, ord) {
                    Ok(out) => {
                        self.audition.set_sink(d, Box::new(PortSink::new(out)));
                        self.aud_ships.insert(d);
                        true
                    }
                    Err(e) => {
                        if self.aud_failed.insert(d) {
                            self.status = tf("status.aud_failed", &[("e", &e.to_string())]).into();
                        }
                        false
                    }
                }
            }
            Some(output::Destination::Plugin { .. }) => {
                self.ensure_plugin(d, false);
                if self.plugin_slots.contains_key(&d)
                    && matches!(self.plugin_state.get(&d), Some(PluginState::Ready { .. }))
                {
                    let sink = self
                        .plugin_slots
                        .get(&d)
                        .expect("slot just loaded")
                        .sink
                        .clone();
                    self.audition.set_sink(d, Box::new(sink));
                    self.aud_ships.insert(d);
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    /// Preview one pitch through `track`'s destination + `ch`'s bank/program
    /// state at `at_tick`. The worker schedules the note-off itself.
    fn audition_strike(&mut self, track: usize, ch: u8, key: u8, vel: u8, at_tick: u64) {
        if !self.aud_enabled {
            return;
        }
        let (d, setup) = {
            let sh = lock_shared(&self.shared);
            (sh.dest_of(track), sh.doc.channel_setup(track, ch, at_tick))
        };
        if !self.audition_sink(d) {
            return;
        }
        self.audition.setup(d, ch, setup);
        self.audition.strike(d, ch, key, vel, self.aud_ms);
    }

    /// Release every preview note — mouse-up, focus loss, doc swap,
    /// destination change, quit.
    fn audition_off(&mut self) {
        self.scrub_key = None;
        self.audition.all_off();
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

    /// Toggle the high-contrast palette; the override persists in the
    /// app-wide prefs. When the OS flag was driving the theme, the first
    /// toggle just flips the effective state.
    fn toggle_hc(&mut self, cx: &mut Context<Self>) {
        let on = self.theme == theme::Theme::high_contrast();
        self.hc_pref = Some(!on);
        self.apply_theme(cx);
        self.save_global();
    }

    /// Small chip with a literal label (symbols/numbers need no i18n key).
    /// `on` receives the ClickEvent so chips can honour Shift=×10 etc.
    /// `a11y_name` is the screen-reader name (visible labels are often terse).
    fn chip(
        id: &'static str,
        label: impl Into<SharedString>,
        a11y_name: impl Into<SharedString>,
        cx: &mut Context<Self>,
        on: impl Fn(&mut Self, &ClickEvent, &mut Context<Self>) + 'static,
    ) -> ObservedElement<Stateful<Div>> {
        div()
            .id(id)
            .test_support()
            .role(Role::Button)
            .aria_label(a11y_name)
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

/// What the inspector edits: event-list rows, roll-selected notes, or the
/// selected track itself.
enum PropTarget {
    Events(Vec<(usize, usize, EventId)>),
    Notes(Vec<EventId>),
    Track(usize),
}

/// i18n label for a field, specialized per event kind where useful.
fn prop_field_label(f: PropField, ev: Option<&DocEvent>) -> SharedString {
    let key = match f {
        PropField::Tick => "prop.tick",
        PropField::Channel | PropField::NoteChannel | PropField::TrackChannel => "prop.channel",
        PropField::MetaType => "prop.meta_type",
        PropField::HexData => "prop.hex_data",
        PropField::NoteStart => "prop.start",
        PropField::NoteEnd => "prop.end",
        PropField::NoteDur => "prop.duration",
        PropField::NoteVel => "prop.velocity",
        PropField::NoteRelVel => "prop.rel_velocity",
        PropField::PbValue => "prop.pb_value",
        PropField::D0 | PropField::D1 => {
            let hi = match ev.map(|e| &e.kind) {
                Some(EventKind::Channel { status, .. }) => status & 0xF0,
                _ => 0,
            };
            match (hi, f) {
                (0x80, PropField::D0) => "prop.key",
                (0x80, PropField::D1) => "prop.rel_velocity",
                (0x90, PropField::D0) => "prop.key",
                (0x90, PropField::D1) => "prop.velocity",
                (0xA0, PropField::D0) => "prop.key",
                (0xA0, PropField::D1) => "prop.pressure",
                (0xB0, PropField::D0) => "prop.controller",
                (0xB0, PropField::D1) => "prop.value",
                (0xC0, PropField::D0) => "prop.program",
                (0xD0, PropField::D0) => "prop.pressure",
                _ => {
                    if f == PropField::D0 {
                        "prop.d0"
                    } else {
                        "prop.d1"
                    }
                }
            }
        }
    };
    t(key).into()
}

fn hex_of(data: &[u8]) -> String {
    data.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Per-kind inspector rows for one event. Editable rows carry `Some(field)`;
/// `warn` flags byte-level edits (malformed bytes can corrupt the event).
fn event_prop_rows(e: &DocEvent) -> Vec<PropRow> {
    let mut rows = vec![PropRow {
        field: Some(PropField::Tick),
        label: prop_field_label(PropField::Tick, Some(e)),
        value: e.tick.to_string(),
        warn: false,
    }];
    match &e.kind {
        EventKind::Channel { status, data, len } => {
            let hi = status & 0xF0;
            rows.push(PropRow {
                field: Some(PropField::Channel),
                label: prop_field_label(PropField::Channel, Some(e)),
                value: ((status & 0x0F) + 1).to_string(),
                warn: false,
            });
            if hi == 0xE0 {
                let v = (((data[1] as u16) << 7) | data[0] as u16) as i32 - 8192;
                rows.push(PropRow {
                    field: Some(PropField::PbValue),
                    label: prop_field_label(PropField::PbValue, Some(e)),
                    value: v.to_string(),
                    warn: false,
                });
            } else {
                rows.push(PropRow {
                    field: Some(PropField::D0),
                    label: prop_field_label(PropField::D0, Some(e)),
                    value: data[0].to_string(),
                    warn: false,
                });
                if *len >= 2 {
                    rows.push(PropRow {
                        field: Some(PropField::D1),
                        label: prop_field_label(PropField::D1, Some(e)),
                        value: data[1].to_string(),
                        warn: false,
                    });
                }
            }
        }
        EventKind::Meta { meta_type, data } => {
            rows.push(PropRow {
                field: Some(PropField::MetaType),
                label: prop_field_label(PropField::MetaType, Some(e)),
                value: format!("0x{meta_type:02X}"),
                warn: true,
            });
            rows.push(PropRow {
                field: Some(PropField::HexData),
                label: prop_field_label(PropField::HexData, Some(e)),
                value: hex_of(data),
                warn: true,
            });
        }
        EventKind::SysEx(data) | EventKind::Escape(data) => {
            rows.push(PropRow {
                field: Some(PropField::HexData),
                label: prop_field_label(PropField::HexData, Some(e)),
                value: hex_of(data),
                warn: true,
            });
        }
    }
    rows
}

fn note_prop_rows(n: &Note, d: &Document) -> Vec<PropRow> {
    let end = n.end_tick.map(|e| e.to_string()).unwrap_or_default();
    let dur = n
        .end_tick
        .map(|e| e.saturating_sub(n.start_tick).to_string())
        .unwrap_or_default();
    let rel = n
        .off_id
        .and_then(|id| find_event(d, id))
        .and_then(|(ti, ei)| match &d.tracks[ti].events[ei].kind {
            EventKind::Channel { data, .. } => Some(data[1].to_string()),
            _ => None,
        })
        .unwrap_or_default();
    let mk = |field: PropField, label: SharedString, value: String| PropRow {
        field: Some(field),
        label,
        value,
        warn: false,
    };
    vec![
        mk(
            PropField::NoteStart,
            t("prop.start").into(),
            n.start_tick.to_string(),
        ),
        mk(PropField::NoteEnd, t("prop.end").into(), end),
        mk(PropField::NoteDur, t("prop.duration").into(), dur),
        mk(
            PropField::NoteChannel,
            t("prop.channel").into(),
            (n.channel + 1).to_string(),
        ),
        mk(
            PropField::NoteVel,
            t("prop.velocity").into(),
            n.vel.to_string(),
        ),
        mk(PropField::NoteRelVel, t("prop.rel_velocity").into(), rel),
    ]
}

fn parse_num(text: &str, lo: i64, hi: i64, name: &str) -> Result<i64, String> {
    let v: i64 = text
        .trim()
        .parse()
        .map_err(|_| format!("invalid {name}: {text}"))?;
    if v < lo || v > hi {
        return Err(format!("{name} out of range {lo}..={hi}: {v}"));
    }
    Ok(v)
}

/// "80 3c 40" / "803c40" / "0x80,0x3c,0x40" / "" all parse; anything else
/// is rejected before a transaction exists.
fn parse_hex(text: &str) -> Result<Vec<u8>, String> {
    let cleaned: String = text.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if cleaned.len() % 2 != 0 {
        return Err(format!("hex data needs whole bytes: {text}"));
    }
    (0..cleaned.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&cleaned[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

fn find_event(d: &Document, id: EventId) -> Option<(usize, usize)> {
    for (ti, tr) in d.tracks.iter().enumerate() {
        for (ei, e) in tr.events.iter().enumerate() {
            if e.id == id {
                return Some((ti, ei));
            }
        }
    }
    None
}

/// Validate `text` for `field` against every target, producing the ops for
/// one transaction. Err = rejected before any transaction; Ok(vec![]) =
/// field unsupported by all targets (e.g. velocity on a meta event).
fn prop_edit_ops(
    d: &mut Document,
    target: &PropTarget,
    field: PropField,
    text: &str,
) -> Result<Vec<Op>, String> {
    match target {
        PropTarget::Track(ti) => match field {
            PropField::TrackChannel => {
                let ch = parse_num(text, 1, 16, "channel")? as u8 - 1;
                Ok(d.set_track_channel_ops(*ti, ch))
            }
            _ => Ok(vec![]),
        },
        PropTarget::Events(ids) => {
            let mut ops = Vec::new();
            for &(ti, ei, _) in ids {
                ops.extend(edit_event_field(d, ti, ei, field, text)?);
            }
            Ok(ops)
        }
        PropTarget::Notes(ids) => {
            let mut ops = Vec::new();
            for &on_id in ids {
                ops.extend(edit_note_field(d, on_id, field, text)?);
            }
            Ok(ops)
        }
    }
}

/// Edit one field of one event. Unsupported field-for-kind returns empty
/// ops (multi-select batch applies only where meaningful); invalid input
/// is an Err before any transaction exists.
fn edit_event_field(
    d: &mut Document,
    ti: usize,
    ei: usize,
    field: PropField,
    text: &str,
) -> Result<Vec<Op>, String> {
    let ev = match d.tracks.get(ti).and_then(|t| t.events.get(ei)) {
        Some(e) => e.clone(),
        None => return Err(format!("event not found: track {ti} index {ei}")),
    };
    let mk = |after: DocEvent| {
        vec![Op::UpdateEvent {
            track: ti,
            before: ev.clone(),
            after,
        }]
    };
    match field {
        PropField::Tick => {
            let mut a = ev.clone();
            a.tick = parse_num(text, 0, i64::MAX, "tick")? as u64;
            Ok(mk(a))
        }
        PropField::Channel => {
            let ch = parse_num(text, 1, 16, "channel")? as u8 - 1;
            if let EventKind::Channel { .. } = ev.kind {
                let mut a = ev.clone();
                if let EventKind::Channel { status, .. } = &mut a.kind {
                    *status = (*status & 0xF0) | ch;
                }
                Ok(mk(a))
            } else {
                Ok(vec![])
            }
        }
        PropField::D0 => {
            let v = parse_num(text, 0, 127, "data0")? as u8;
            if let EventKind::Channel { .. } = ev.kind {
                let mut a = ev.clone();
                if let EventKind::Channel { data, .. } = &mut a.kind {
                    data[0] = v;
                }
                Ok(mk(a))
            } else {
                Ok(vec![])
            }
        }
        PropField::D1 => {
            let v = parse_num(text, 0, 127, "data1")? as u8;
            match ev.kind {
                EventKind::Channel { len, .. } if len >= 2 => {
                    let mut a = ev.clone();
                    if let EventKind::Channel { data, .. } = &mut a.kind {
                        data[1] = v;
                    }
                    Ok(mk(a))
                }
                _ => Ok(vec![]),
            }
        }
        PropField::PbValue => {
            let v = parse_num(text, -8192, 8191, "pitch bend")?;
            if let EventKind::Channel { status, .. } = ev.kind {
                if status & 0xF0 != 0xE0 {
                    return Ok(vec![]);
                }
                let u = (v + 8192) as u16;
                let mut a = ev.clone();
                if let EventKind::Channel { data, .. } = &mut a.kind {
                    data[0] = (u & 0x7F) as u8;
                    data[1] = ((u >> 7) & 0x7F) as u8;
                }
                Ok(mk(a))
            } else {
                Ok(vec![])
            }
        }
        PropField::MetaType => {
            let s = text.trim();
            let mt = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                Some(h) => {
                    u8::from_str_radix(h, 16).map_err(|_| format!("invalid meta type: {text}"))?
                }
                None => parse_num(s, 0, 255, "meta type")? as u8,
            };
            if let EventKind::Meta { .. } = ev.kind {
                let mut a = ev.clone();
                if let EventKind::Meta { meta_type, .. } = &mut a.kind {
                    *meta_type = mt;
                }
                Ok(mk(a))
            } else {
                Ok(vec![])
            }
        }
        PropField::HexData => {
            let bytes = parse_hex(text)?;
            let mut a = ev.clone();
            match &mut a.kind {
                EventKind::Meta { data, .. } | EventKind::SysEx(data) | EventKind::Escape(data) => {
                    *data = bytes.into();
                    Ok(mk(a))
                }
                _ => Ok(vec![]),
            }
        }
        // note-shaped fields apply through the note path, not single events
        _ => Ok(vec![]),
    }
}

/// Edit one field of one paired note (its NoteOn / NoteOff events).
/// Unsupported fields (e.g. duration on a dangling on) return empty ops.
fn edit_note_field(
    d: &mut Document,
    on_id: EventId,
    field: PropField,
    text: &str,
) -> Result<Vec<Op>, String> {
    let Some((oti, oei)) = find_event(d, on_id) else {
        return Ok(vec![]);
    };
    let note = d.notes().into_iter().find(|n| n.on_id == on_id);
    let Some(n) = note else { return Ok(vec![]) };
    let on = d.tracks[oti].events[oei].clone();
    let off = n
        .off_id
        .and_then(|id| find_event(d, id))
        .map(|(ti, ei)| d.tracks[ti].events[ei].clone());
    let mut ops = Vec::new();
    let mut upd = |pos: (usize, usize), after: DocEvent| {
        ops.push(Op::UpdateEvent {
            track: pos.0,
            before: d.tracks[pos.0].events[pos.1].clone(),
            after,
        });
    };
    match field {
        PropField::NoteStart => {
            let v = parse_num(text, 0, i64::MAX, "start")? as u64;
            if let Some(end) = n.end_tick {
                if v >= end {
                    return Err(format!("start must be before end ({end}): {v}"));
                }
            }
            let mut a = on.clone();
            a.tick = v;
            upd((oti, oei), a);
        }
        PropField::NoteEnd => {
            let v = parse_num(text, 0, i64::MAX, "end")? as u64;
            let Some(off) = off else {
                return Err("dangling note has no note-off to edit".into());
            };
            if v <= n.start_tick {
                return Err(format!("end must be after start ({}): {v}", n.start_tick));
            }
            let fti = find_event(d, n.off_id.unwrap()).unwrap();
            let mut a = off;
            a.tick = v;
            upd(fti, a);
        }
        PropField::NoteDur => {
            let v = parse_num(text, 1, i64::MAX, "duration")? as u64;
            let Some(off) = off else {
                return Err("dangling note has no note-off to edit".into());
            };
            let fti = find_event(d, n.off_id.unwrap()).unwrap();
            let mut a = off;
            a.tick = n.start_tick + v;
            upd(fti, a);
        }
        PropField::NoteVel => {
            let v = parse_num(text, 1, 127, "velocity")? as u8;
            let mut a = on.clone();
            if let EventKind::Channel { data, .. } = &mut a.kind {
                data[1] = v;
            }
            upd((oti, oei), a);
        }
        PropField::NoteRelVel => {
            let v = parse_num(text, 0, 127, "release velocity")? as u8;
            let Some(off) = off else {
                return Err("dangling note has no note-off to edit".into());
            };
            let fti = find_event(d, n.off_id.unwrap()).unwrap();
            let mut a = off;
            if let EventKind::Channel { data, .. } = &mut a.kind {
                data[1] = v;
            }
            upd(fti, a);
        }
        PropField::NoteChannel => {
            let ch = parse_num(text, 1, 16, "channel")? as u8 - 1;
            let mut a = on.clone();
            if let EventKind::Channel { status, .. } = &mut a.kind {
                *status = (*status & 0xF0) | ch;
            }
            upd((oti, oei), a);
            if let Some(off) = off {
                let fti = find_event(d, n.off_id.unwrap()).unwrap();
                let mut a = off;
                if let EventKind::Channel { status, .. } = &mut a.kind {
                    *status = (*status & 0xF0) | ch;
                }
                upd(fti, a);
            }
        }
        _ => return Ok(vec![]),
    }
    Ok(ops)
}

/// App-wide preferences: recent files + record count-in + recording source.
/// Stored at %APPDATA%/midi-editor/prefs.json (unlike the per-song sidecar).
/// Versioned + atomically persisted via `persist::json` — a torn write can
/// no longer silently reset recent files.
#[derive(serde::Serialize, serde::Deserialize)]
struct GlobalPrefs {
    /// schema version — absent in v0 files
    #[serde(default)]
    version: u32,
    #[serde(default)]
    recent: Vec<String>,
    #[serde(default)]
    count_in: bool,
    /// MIDI input port name to record from; empty = first available port
    #[serde(default)]
    midi_in: String,
    /// manual input-latency compensation subtracted from every recorded
    /// timestamp, in ms — for keyboards/interfaces with a known pipeline
    /// delay. Missing in older prefs files.
    #[serde(default)]
    in_latency_ms: u64,
    /// VST3 audio output device display name; None = system default
    #[serde(default)]
    audio_device: Option<String>,
    /// preferred sample rate for hosted-plugin streams; None = 44100
    #[serde(default)]
    sample_rate: Option<f64>,
    /// preferred buffer size in samples; None = 512
    #[serde(default)]
    buffer_size: Option<u32>,
    /// Per-plugin probe bound in seconds (default
    /// `output::DEFAULT_SCAN_TIMEOUT`); quarantined entries respect it too.
    probe_timeout_secs: Option<u64>,
    /// note audition preview on/off (None in older files = on)
    #[serde(default)]
    audition: Option<bool>,
    /// preview velocity for piano-key/draw strikes (None = 100)
    #[serde(default)]
    aud_vel: Option<u8>,
    /// max preview sustain in ms (None = 400)
    #[serde(default)]
    aud_ms: Option<u64>,
    /// high-contrast override: Some(force on/off), None = follow the OS flag
    hc: Option<bool>,
    /// appearance mode: "system" | "dark" | "light" (None = system)
    theme: Option<String>,
    /// keybinding overrides: command id -> "ctrl+shift+z" descriptor
    #[serde(default)]
    keymap: HashMap<String, String>,
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
    fn path() -> PathBuf {
        let base = std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir());
        base.join("midi-editor")
    }

    fn load() -> Self {
        let l = persist::json::load_json::<GlobalPrefs>(&Self::path().join("prefs.json"));
        for d in &l.diagnostics {
            tracing::warn!("prefs: {d}");
        }
        l.value.unwrap_or_default()
    }

    fn save(&self) {
        let dir = Self::path();
        let _ = std::fs::create_dir_all(&dir);
        let _ = persist::json::save_json(&dir.join("prefs.json"), self);
    }
}

/// App-wide plugin scan cache + quarantine list — shared across songs, so it
/// lives next to prefs.json rather than in a per-file sidecar.
fn scan_cache_path() -> PathBuf {
    GlobalPrefs::path().join("plugin_scan_cache.json")
}

/// Session state that cannot live inside the SMF: per-track output
/// assignments (by stable destination identity, not runtime index), mute/solo,
/// metronome/loop, view transform. Written next to the document as
/// `song.mid.editor.json`.
#[derive(serde::Serialize, serde::Deserialize)]
struct Prefs {
    /// schema version — absent in v0 files
    #[serde(default)]
    version: u32,
    default_dest: Option<output::Destination>,
    #[serde(default)]
    track_dest: HashMap<usize, output::Destination>,
    #[serde(default)]
    muted: Vec<usize>,
    #[serde(default)]
    soloed: Vec<usize>,
    #[serde(default)]
    metronome: bool,
    #[serde(default)]
    loop_enabled: bool,
    /// None in old sidecars = keep the default (off)
    chase_sysex: Option<bool>,
    /// None in old sidecars = keep the default (`SysexPolicy::Serialize`)
    sysex_policy: Option<String>,
    zoom: Option<f32>,
    scroll_x: Option<f32>,
    scroll_y: Option<f32>,
    sel_track: Option<usize>,
    enc: Option<String>,
    /// legacy single-lane sidecar key — read as a fallback when `lanes`
    /// is absent; new saves always write `lanes`
    lane: Option<String>,
    /// legacy poly-aftertouch lane key filter — read as a fallback for
    /// sidecars written before per-lane keys existed; new saves write
    /// the key inside each lane of `lanes`
    poly_key: Option<u8>,
    /// stacked bottom lanes, top to bottom. Absent in old sidecars =
    /// the default single velocity lane
    lanes: Option<Vec<LanePref>>,
    show_events: Option<bool>,
    tool: Option<String>,
    snap: Option<usize>,
    /// vertical zoom (row height px) and fold/drum/scale view toggles
    note_h: Option<f32>,
    fold: Option<bool>,
    drum: Option<bool>,
    scale: Option<i8>,
    scale_minor: Option<bool>,
    /// None in old sidecars = keep the default (page)
    follow: Option<String>,
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
            loop_enabled: false,
            chase_sysex: None,
            sysex_policy: None,
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
    }
}

/// Per-lane layout as stored in the sidecar (`mode` uses the same
/// "vel"/"cc<n>"/"pb" codec as the legacy `lane` key).
#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct LanePref {
    mode: String,
    h: f32,
    #[serde(default)]
    collapsed: bool,
    /// poly-AT key filter for `pat` lanes (None = all keys)
    poly_key: Option<u8>,
}

fn prefs_path(doc_path: &std::path::Path) -> PathBuf {
    PathBuf::from(format!("{}.editor.json", doc_path.display()))
}

/// Display label for a destination identity (sidecar paths -> stem).
fn dest_label(d: &output::Destination) -> String {
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
    fn resolve_dest(&mut self, d: &output::Destination) -> usize {
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
    fn apply_prefs(&mut self, doc_path: &std::path::Path) -> Vec<String> {
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
            if let Some(sp) = p
                .sysex_policy
                .as_deref()
                .and_then(midi_io::SysexPolicy::from_label)
            {
                sh.sysex_policy = sp;
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
            in_latency_ms: self.in_latency_ms,
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
            ..Default::default()
        }
        .save();
    }

    fn persist(&mut self) {
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
            loop_enabled: sh.loop_enabled,
            chase_sysex: Some(sh.chase_sysex),
            sysex_policy: Some(sh.sysex_policy.label().to_string()),
            zoom: Some(self.zoom),
            scroll_x: Some(self.scroll_x),
            scroll_y: Some(self.scroll_y),
            sel_track: Some(self.sel_track),
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

impl Drop for EditorView {
    fn drop(&mut self) {
        // last line of defense: no preview note outlives the view
        self.audition.all_off();
    }
}

fn main() {
    output::init_env();
    let log_dir = diagnostics::init_logging();
    diagnostics::install_panic_hook();
    diagnostics::log_boot();
    tracing::info!(log_dir = %log_dir.display(), "logging initialized");
    let path = std::env::args().nth(1).map(PathBuf::from);
    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        // the component theme (text inputs etc.) is applied in
        // EditorView::apply_theme once prefs resolve the effective palette
        let path = path.clone();
        cx.spawn(async move |cx| {
            cx.open_window(WindowOptions::default(), move |window, cx| {
                let input =
                    cx.new(|cx| InputState::new(window, cx).placeholder(t("field.track_name")));
                let prop_input =
                    cx.new(|cx| InputState::new(window, cx).placeholder(t("prop.value")));
                let meta_input =
                    cx.new(|cx| InputState::new(window, cx).placeholder(t("field.meta_value")));
                let view = cx.new(|cx| {
                    let mut v = EditorView::new(
                        path.clone(),
                        input,
                        prop_input,
                        meta_input.clone(),
                        window,
                        cx,
                    );
                    // Enter inside the meta dialog applies it.
                    cx.subscribe(
                        &meta_input,
                        |this, _e, ev: &gpui_kit::component::input::InputEvent, cx| {
                            if matches!(
                                ev,
                                gpui_kit::component::input::InputEvent::PressEnter { .. }
                            ) {
                                this.commit_meta_edit(cx);
                            }
                        },
                    )
                    .detach();
                    v.window_handle = Some(window.window_handle());
                    let (mcp_stop, mcp_thread) = spawn_mcp(v.shared.clone());
                    v.shutdown.track_mcp(mcp_stop, mcp_thread);
                    spawn_doc_watch(cx, v.shared.clone());
                    window.focus(&v.focus.clone(), cx);
                    v
                });
                // the close button (and any quit path going through window
                // close) runs the same discard guard as New/Open; once the
                // guard passes, the teardown coordinator runs once
                // (idempotent with the on_app_quit hook)
                let weak = view.downgrade();
                window.on_window_should_close(cx, move |window, cx| {
                    let Some(view) = weak.upgrade() else {
                        return true;
                    };
                    view.update(cx, |v, cx| {
                        if v.close_confirmed || !v.needs_discard_guard() {
                            v.perform_shutdown();
                            return true;
                        }
                        v.confirm_discard_or_save(PendingAction::CloseWindow, window, cx);
                        false
                    })
                });
                maybe_prompt_restore(view.clone(), path.clone(), window, cx);
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("failed to open window");
        })
        .detach();
    });
}

/// In-app MCP server: Streamable-HTTP on 127.0.0.1:7878/mcp on its own
/// tokio runtime thread. Auth: MIDI_MCP_TOKEN, else an auto-provisioned
/// per-user token file; unauthenticated only via MIDI_MCP_ALLOW_INSECURE.
/// `mcp-bridge` is the stdio frontend for stdio-only clients.
fn spawn_mcp(
    shared: SharedDoc,
) -> (
    tokio::sync::oneshot::Sender<()>,
    std::thread::JoinHandle<()>,
) {
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let handle = std::thread::spawn(move || {
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
            let auth = match mcp_server::resolve_http_auth() {
                Ok(a) => a,
                // never silently serve unauthenticated: no token source,
                // no MCP endpoint
                Err(e) => {
                    tracing::error!(error = %e, "mcp http disabled");
                    return;
                }
            };
            if let Err(e) = mcp_server::serve_http(shared, "127.0.0.1:7878", auth, stop_rx).await {
                tracing::error!(error = %e, "mcp http stopped");
            }
        });
    });
    (stop_tx, handle)
}

/// Startup recovery. After a crash the newest snapshot that is newer than
/// its source prompts Restore / Discard / Inspect — Inspect re-prompts
/// with the snapshot's provenance so the decision is informed. Restore
/// swaps the recovered document in and marks it dirty; the original file
/// is only ever written by an explicit Save. Runs as a window task so it
/// doesn't block app startup.
fn maybe_prompt_restore(
    view: Entity<EditorView>,
    argv_path: Option<PathBuf>,
    window: &mut Window,
    cx: &mut App,
) {
    // bounded retention — stale/overflow snapshots die on startup
    recovery::cleanup_stale(
        &recovery::recovery_dir(),
        recovery::KEEP_MAX,
        recovery::MAX_AGE,
        std::time::SystemTime::now(),
    );
    let Some((snap_path, meta, payload)) =
        recovery::find_candidate(&recovery::recovery_dir(), argv_path.as_deref())
    else {
        return;
    };
    window
        .spawn(cx, async move |wcx| {
            let src = meta
                .source_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| t("recovery.untitled").to_string());
            let mut shown_details = false;
            loop {
                let detail = if shown_details {
                    tf(
                        "recovery.inspect_detail",
                        &[
                            ("src", &src),
                            ("saved", &meta.saved_revision.to_string()),
                            ("rev", &meta.current_revision.to_string()),
                            ("size", &meta.payload_len.to_string()),
                            ("ver", &meta.app_version),
                            ("ago", &fmt_rel_time(meta.timestamp)),
                        ],
                    )
                } else {
                    tf(
                        "recovery.found",
                        &[("src", &src), ("ago", &fmt_rel_time(meta.timestamp))],
                    )
                };
                let idx = wcx
                    .prompt(
                        PromptLevel::Warning,
                        t("recovery.title"),
                        Some(&detail),
                        &[
                            PromptButton::Ok(t("recovery.restore").into()),
                            PromptButton::Other(t("recovery.discard").into()),
                            PromptButton::Cancel(
                                if shown_details {
                                    t("recovery.later")
                                } else {
                                    t("recovery.inspect")
                                }
                                .into(),
                            ),
                        ],
                    )
                    .await
                    .unwrap_or(usize::MAX);
                match idx {
                    // Restore — swap the snapshot's document into the view
                    0 => {
                        wcx.update(|_w, app| {
                            view.update(app, |v, cx| {
                                v.restore_snapshot(&meta, &payload, cx);
                            })
                        })
                        .ok();
                        break;
                    }
                    // Discard — explicit discard clears the snapshot
                    1 => {
                        let _ = std::fs::remove_file(&snap_path);
                        break;
                    }
                    // Inspect — one expansion, then the same three fates
                    _ if !shown_details => shown_details = true,
                    // Later/dismissed — keep the snapshot, nothing happens;
                    // the next verified save or discard clears it
                    _ => break,
                }
            }
        })
        .detach();
}

/// "3 minutes ago" style label for snapshot timestamps.
fn fmt_rel_time(unix_ts: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let ago = now.saturating_sub(unix_ts);
    if ago < 60 {
        tf("time.sec_ago", &[("n", &ago.to_string())])
    } else if ago < 3600 {
        tf("time.min_ago", &[("n", &(ago / 60).to_string())])
    } else if ago < 86400 {
        tf("time.hr_ago", &[("n", &(ago / 3600).to_string())])
    } else {
        tf("time.day_ago", &[("n", &(ago / 86400).to_string())])
    }
}

impl EditorView {
    /// Periodic MIDI endpoint reconcile (called ~every 2 s by the doc
    /// watcher): refresh the present-port set, append newly discovered
    /// outputs to the catalog — an assignment targeting a vanished port
    /// keeps its identity and plays again on replug — and reopen an armed
    /// recording's input connection when its endpoint comes back.
    /// Returns true when the catalog or availability changed.
    fn reconcile_ports(&mut self) -> bool {
        let outs = midi_io::list_outputs().unwrap_or_default();
        let present: std::collections::HashSet<(String, usize)> =
            outs.iter().map(|p| (p.name.clone(), p.ord)).collect();
        let mut changed = false;
        {
            let mut sh = lock_shared(&self.shared);
            if sh.port_present != present {
                // brand-new outputs become assignable immediately
                let mut name_counts: HashMap<String, usize> = HashMap::new();
                for p in &outs {
                    *name_counts.entry(p.name.clone()).or_default() += 1;
                }
                for p in &outs {
                    let d = midi_io::Destination::MidiPort {
                        port_name: p.name.clone(),
                        ord: p.ord,
                    };
                    if !sh.dests.iter().any(|(_, dd)| *dd == d) {
                        let label = if name_counts.get(p.name.as_str()).copied().unwrap_or(0) > 1 {
                            format!("{} #{}", p.name, p.ord + 1)
                        } else {
                            p.name.clone()
                        };
                        sh.dests.push((label, d));
                    }
                }
                sh.port_present = present;
                changed = true;
            }
        }
        // armed input whose endpoint returned: swap in a fresh connection —
        // its t=0 restarts, so re-anchor the take's doc-time base at now
        if let Some(rec) = self.rec.as_mut() {
            let (name, ord) = (rec.input.name.clone(), rec.input.ord);
            let found = midi_io::list_inputs()
                .unwrap_or_default()
                .iter()
                .any(|p| p.name == name && p.ord == ord);
            if found && rec.input_lost {
                let buf2 = rec.buf.clone();
                let cb = move |us, b: &[u8]| {
                    buf2.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push((us, b.to_vec()));
                };
                if let Ok(inp) = midi_io::Input::open_ord(&name, ord, cb) {
                    rec.input = inp;
                    rec.input_lost = false;
                    // the new connection's t=0 restarts — re-anchor the
                    // doc-time base and clear the already-consumed count-in
                    rec.base_us = self
                        .playback
                        .as_ref()
                        .map(|p| p.position_us())
                        .unwrap_or(self.play_us);
                    rec.cin_us = 0;
                    tracing::info!("recording input '{name}' reconnected");
                    changed = true;
                }
            } else if !found && !rec.input_lost {
                rec.input_lost = true;
                tracing::warn!("recording input '{name}' disappeared; reconnect on return");
                changed = true;
            }
        }
        changed
    }
}

/// Poll the shared doc's notify counter so MCP-driven edits repaint the UI
/// even while the user is idle.
fn spawn_doc_watch(cx: &mut Context<EditorView>, shared: SharedDoc) {
    cx.spawn(async move |this, cx| {
        let mut last = 0u64;
        let mut last_tx = 0u64;
        let mut tick = 0u32;
        // autosave: the last revision a snapshot captured and the last
        // write attempt (started one debounce early so the first dirty
        // revision snapshots without an artificial delay)
        let mut last_snap_rev: Option<u64> = None;
        let mut last_snap_write =
            std::time::Instant::now() - recovery::DEBOUNCE;
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(150))
                .await;
            {
                let sh = lock_shared(&shared);
                let rev = sh.doc.revision();
                let doc_dirty = rev != sh.saved_revision;
                drop(sh);
                if doc_dirty
                    && last_snap_rev != Some(rev)
                    && last_snap_write.elapsed() >= recovery::DEBOUNCE
                {
                    last_snap_write = std::time::Instant::now();
                    let dir = recovery::recovery_dir();
                    if recovery::write_snapshot(&shared, &dir, std::time::SystemTime::now()).is_ok() {
                        last_snap_rev = Some(rev);
                    }
                }
            }
            let (cur, reqs, mcp_tx) = {
                let mut sh = lock_shared(&shared);
                (
                    sh.gui_notify.load(std::sync::atomic::Ordering::Relaxed),
                    std::mem::take(&mut sh.transport_req),
                    sh.last_mcp_tx.clone(),
                )
            };
            let dirty = cur != last || !reqs.is_empty();
            if cur != last {
                last = cur;
            }
            // surface the newest agent-originated transaction in the status bar
            let mcp_label = mcp_tx
                .as_ref()
                .filter(|r| r.revision > last_tx)
                .map(|r| {
                    last_tx = r.revision;
                    r.label.clone()
                });
            if let Some(this) = this.upgrade() {
                this.update(cx, |v, cx| {
                    // hotplug reconcile ~every 2 s: fresh endpoints join the
                    // catalog, vanished ones stay visible but marked offline,
                    // and an armed recording's input reconnects on return
                    tick += 1;
                    let ports_changed = tick % 13 == 0 && v.reconcile_ports();
                    // MCP transport requests -> real playback actions
                    for r in reqs {
                        match r {
                            mcp_server::TransportReq::Play if v.playback.is_none() => {
                                v.start_playback();
                            }
                            mcp_server::TransportReq::Stop => v.stop_playback(),
                            mcp_server::TransportReq::Seek { tick } => {
                                v.play_us = v.doc(|d| {
                                    d.tempo_map_for(v.sel_track).tick_to_us(tick)
                                });
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
                    if let Some(rx) = &v.save_rx {
                        if let Ok(done) = rx.try_recv() {
                            v.save_rx = None;
                            match done {
                                // committed=false means the document was
                                // swapped mid-save — its state governs
                                Ok(out) if out.committed => {
                                    tracing::info!(path = %out.path.display(), rev = out.revision, "document saved");
                                    v.status = t("status.saved").into();
                                    v.persist();
                                    // a verified normal save clears recovery
                                    recovery::clear_recovery();
                                    // the just-written file is the new
                                    // identity baseline for external-watch
                                    v.file_stamp = watch::stat_file(&out.path);
                                    v.ext_prompted = false;
                                }
                                Ok(_) => {}
                                Err(e) => v.status = e.into(),
                            }
                            cx.notify();
                        }
                    }
                    if dirty {
                        v.refresh_derived();
                        // cover routing changes that came from MCP tools —
                        // also warms any newly-assigned VST3 destination
                        v.persist();
                        v.refresh_plugins();
                        // a preview ringing on a route MCP just changed
                        // must not keep sounding into the wrong place
                        v.audition_off();
                    }
                    // watch the backing .mid for external modification /
                    // deletion (only the MIDI file — the sidecar doesn't count)
                    v.check_external_change(cx);
                    if let Some(l) = mcp_label {
                        v.status = tf("status.mcp_edit", &[("label", &l)]).into();
                    }
                    // repaint while playing so the playhead/counter advance;
                    // also while a plugin editor is open so its native event
                    // queue gets serviced even when the app is idle
                    // keep repainting while a plugin editor is open so its
                    // platform events get pumped and user-close is noticed
                    let mut editor_resync = None;
                    if let Some(pw) = &v.plugin_window {
                        let _ = pw.service_platform_events();
                        // live param sync: forward the editor's edits into
                        // the playing instance (best-effort each tick)
                        if let Some((d, editor)) = v.editor_plugin.clone() {
                            let edits = editor
                                .lock()
                                .map(|mut e| e.take_parameter_edits())
                                .unwrap_or_default();
                            if !edits.is_empty() {
                                if let Some(slot) = v.plugin_slots.get(&d) {
                                    if let Ok(mut p) = slot.plugin.try_lock() {
                                        for ed in edits {
                                            if let Some(val) = ed.value {
                                                let _ = p.set_parameter(ed.id, val);
                                            }
                                        }
                                    }
                                }
                                // the playing slot's state changed — queue a
                                // capture; the throttled flush below bounds
                                // the write rate while a knob is dragged
                                v.pending_state_capture.insert(d);
                            }
                            // the editor instance's own restartComponent drain:
                            // a preset/state change made inside the plugin's
                            // GUI arrives as kParamValuesChanged, not as
                            // parameter edits — push the editor's state into
                            // the playing instance so they agree. (Drained
                            // with take_restart_flags, not serviced: a
                            // GUI-only instance has no processing lifecycle.
                            // This is its control thread — the UI thread that
                            // loaded it.)
                            let flags = editor
                                .lock()
                                .map(|mut e| e.take_restart_flags())
                                .unwrap_or_default();
                            let mut resync = false;
                            for note in output::restart_notes(flags) {
                                match note {
                                    output::RestartNote::ParamValues
                                    | output::RestartNote::ParamTitles => resync = true,
                                    _ => {
                                        if v
                                            .restart_logged
                                            .entry(d)
                                            .or_default()
                                            .first_seen(note)
                                        {
                                            tracing::info!(
                                                "editor instance (dest {d}): {} noted",
                                                note.name()
                                            );
                                        }
                                    }
                                }
                            }
                            if resync {
                                editor_resync = Some((d, editor));
                            }
                        }
                        if pw.closed_by_user() {
                            v.plugin_window = None;
                            v.sync_editor_state_into_slot();
                        }
                    }
                    if let Some((d, editor)) = editor_resync {
                        v.push_editor_state(d, &editor);
                    }
                    if !v.pending_state_capture.is_empty() {
                        v.flush_plugin_states(false);
                    }
                    if dirty
                        || plugin_changed
                        || ports_changed
                        || v.playback.is_some()
                        || v.plugin_window.is_some()
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
    use crate::{empty_doc, plugin_plan, GlobalPrefs, PluginPlan, PluginState, Prefs};
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

    fn testdir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join("midi-editor-prefs-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// v0 sidecars (every existing `.editor.json`) carry no version field —
    /// migration stamps v1 and the same fields load.
    #[test]
    fn sidecar_v0_migrates_to_v1() {
        let dir = testdir("sidecar_v0_migrates_to_v1");
        let p = dir.join("song.mid.editor.json");
        std::fs::write(
            &p,
            br#"{"metronome":true,"loop_enabled":true,"zoom":0.05,"snap":3,"muted":[2]}"#,
        )
        .unwrap();
        let l = persist::json::load_json::<Prefs>(&p);
        let prefs = l.value.expect("v0 sidecar loads");
        assert_eq!(prefs.version, 1);
        assert!(prefs.metronome && prefs.loop_enabled);
        assert_eq!(prefs.snap, Some(3));
        assert_eq!(prefs.muted, vec![2]);
    }

    /// corrupt sidecar → quarantined, open proceeds, .bak recovers prior state
    #[test]
    fn sidecar_corruption_recovers_backup() {
        let dir = testdir("sidecar_corruption_recovers_backup");
        let p = dir.join("song.mid.editor.json");
        persist::json::save_json(
            &p,
            &Prefs {
                metronome: true,
                zoom: Some(0.1),
                ..Default::default()
            },
        )
        .unwrap();
        persist::json::save_json(
            &p,
            &Prefs {
                loop_enabled: true,
                ..Default::default()
            },
        )
        .unwrap();
        std::fs::write(&p, b"{\"version\":1,\"zoom\":").unwrap();
        let l = persist::json::load_json::<Prefs>(&p);
        assert!(l.recovered_from_backup);
        let prefs = l.value.expect("backup recovers last valid");
        assert!(prefs.metronome, "previous valid version, not the torn one");
        assert!(!p.exists(), "corrupt primary quarantined");
    }

    /// unknown future fields and a newer version still load known data —
    /// the file is left alone for the newer build that wrote it
    #[test]
    fn sidecar_newer_version_keeps_known_fields() {
        let dir = testdir("sidecar_newer_version_keeps_known_fields");
        let p = dir.join("song.mid.editor.json");
        std::fs::write(
            &p,
            br#"{"version":7,"metronome":true,"future_thing":[1,2,3],"zoom":0.2}"#,
        )
        .unwrap();
        let l = persist::json::load_json::<Prefs>(&p);
        let prefs = l.value.expect("newer sidecar loads");
        assert!(prefs.metronome);
        assert!(p.exists());
        assert!(!l.diagnostics.is_empty());
    }

    /// out-of-range numerics are clamped at load, before apply_prefs
    #[test]
    fn sidecar_bad_numerics_are_clamped() {
        let dir = testdir("sidecar_bad_numerics_are_clamped");
        let p = dir.join("song.mid.editor.json");
        std::fs::write(
            &p,
            br#"{"version":1,"zoom":-4.0,"scroll_x":-9.5,"snap":999,"enc":"koi8","tool":"laser","lane":"cc999"}"#,
        )
        .unwrap();
        let l = persist::json::load_json::<Prefs>(&p);
        let prefs = l.value.unwrap();
        assert_eq!(prefs.zoom, None, "non-positive zoom dropped, not clamped");
        assert_eq!(prefs.scroll_x, Some(0.0));
        assert_eq!(prefs.snap, Some(crate::SNAPS.len() - 1));
        assert_eq!(prefs.enc, None);
        assert_eq!(prefs.tool, None);
        assert_eq!(prefs.lane, None);
    }

    /// destinations that no longer exist still deserialize — they keep
    /// their identity and resolve again when the device comes back
    #[test]
    fn sidecar_missing_destinations_still_parse() {
        let dir = testdir("sidecar_missing_destinations_still_parse");
        let p = dir.join("song.mid.editor.json");
        std::fs::write(
            &p,
            br#"{"version":1,"default_dest":{"MidiPort":{"port_name":"gone-port"}},"track_dest":{"3":{"Plugin":{"plugin_path":"C:\\VST3\\absent.vst3"}}}}"#,
        )
        .unwrap();
        let l = persist::json::load_json::<Prefs>(&p);
        let prefs = l.value.expect("missing destinations parse fine");
        assert!(prefs.default_dest.is_some());
        assert!(prefs.track_dest.contains_key(&3));
    }

    #[test]
    fn global_prefs_v0_loads_and_newer_survives() {
        let dir = testdir("global_prefs_v0_loads_and_newer_survives");
        let p = dir.join("prefs.json");
        std::fs::write(
            &p,
            br#"{"recent":["a.mid"],"count_in":true,"midi_in":"p1"}"#,
        )
        .unwrap();
        let l = persist::json::load_json::<GlobalPrefs>(&p);
        let g = l.value.expect("v0 globals load");
        assert_eq!(g.version, 1);
        assert_eq!(g.recent, vec!["a.mid"]);
        assert!(g.count_in);
        assert_eq!(g.midi_in, "p1");
    }

    /// GlobalPrefs written before audio settings existed has no audio keys;
    /// it must still parse (serde defaults) instead of resetting recents.
    #[test]
    fn legacy_prefs_json_without_audio_fields_parses() {
        let g: super::GlobalPrefs =
            serde_json::from_str(r#"{"recent":["a.mid"],"count_in":true,"midi_in":"port"}"#)
                .unwrap();
        assert_eq!(g.recent, ["a.mid"]);
        assert!(g.audio_device.is_none() && g.sample_rate.is_none() && g.buffer_size.is_none());
    }

    /// An `AudioSelection` deserializes from a minimal/legacy prefs JSON —
    /// missing fields must come out as `None` (system defaults) rather than
    /// a parse error, so old prefs files keep loading.
    #[test]
    fn audio_selection_missing_fields_are_system_default() {
        let sel: output::AudioSelection = serde_json::from_str("{}").unwrap();
        assert!(sel.device.is_none() && sel.sample_rate.is_none() && sel.buffer_size.is_none());
        let rt: output::AudioSelection = serde_json::from_str(
            &serde_json::to_string(&output::AudioSelection {
                device: Some("Speakers".into()),
                sample_rate: Some(48000.0),
                buffer_size: Some(1024),
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(rt.device.as_deref(), Some("Speakers"));
        assert_eq!(rt.sample_rate, Some(48000.0));
        assert_eq!(rt.buffer_size, Some(1024));
    }

    /// A document with mid-song tempo + meter changes produces transport
    /// points at deterministic µs positions (the tempo map's own µs
    /// accounting), sorted — the schedule the transport sink applies.
    #[test]
    fn transport_points_cover_tempo_and_meter_at_document_positions() {
        use document::{Document, Op, Transaction};
        let mut d = empty_doc();
        fn apply(d: &mut Document, ops: Vec<Op>) {
            d.apply(Transaction {
                label: "t".into(),
                base: d.revision(),
                ops,
            })
            .unwrap();
        }
        let ops = d.set_time_sig_ops(0, 0, 3, 4);
        apply(&mut d, ops);
        let ops = d.set_tempo_ops(0, 960, 60.0); // 960 ticks @480ppq = 2 quarters in
        apply(&mut d, ops);
        let ops = d.set_time_sig_ops(0, 1920, 6, 8);
        apply(&mut d, ops);
        let pts = super::transport_points_of(&d);
        assert!(pts.windows(2).all(|w| w[0].0 <= w[1].0), "{pts:?}");
        // head tempo 120 @0; tempo change to 60bpm lands at 1_000_000µs
        // (two 500 ms quarters); the second sig lands one 60bpm minute later:
        // 1_000_000 + 2 quarters * 1_000_000 = 3_000_000
        for want in [
            (0u64, output::TransportCmd::Tempo(120.0)),
            (1_000_000, output::TransportCmd::Tempo(60.0)),
            (0, output::TransportCmd::TimeSig(3, 4)),
            (3_000_000, output::TransportCmd::TimeSig(6, 8)),
        ] {
            assert!(pts.contains(&want), "missing {want:?} in {pts:?}");
        }
        // the seek chase at 2_500_000 re-asserts 60bpm + 3/4 — the state the
        // map had built up to that position
        assert_eq!(
            output::chase_transport(&pts, 2_500_000),
            vec![
                (2_500_000, output::TransportCmd::Tempo(60.0)),
                (2_500_000, output::TransportCmd::TimeSig(3, 4)),
            ]
        );
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

    use crate::{i18n::t, EditorView};
    use gpui_kit::component::input::InputState;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{px, size, AppContext, Role, TestAppContext};

    /// Accessibility smoke test: the a11y facts registered via
    /// `.test_support()` mirror exactly what the real UIA tree would carry
    /// (roles, names, selected/checked/expanded state) for every primary
    /// control, and the interactions a screen reader drives still work.
    #[gpui_kit::test]
    fn a11y_tree_exposes_primary_controls(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
            EditorView::new(
                None,
                cx.new(|cx| InputState::new(window, cx).placeholder(t("field.track_name"))),
                cx.new(|cx| InputState::new(window, cx).placeholder(t("prop.value"))),
                cx.new(|cx| InputState::new(window, cx).placeholder(t("field.meta_value"))),
                window,
                cx,
            )
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);

            // shell landmarks
            assert_eq!(window.find("editor").role(), Some(Role::Application));
            assert_eq!(window.find("menu-bar").role(), Some(Role::MenuBar));
            assert_eq!(window.find("transport-bar").role(), Some(Role::Toolbar));
            assert_eq!(window.find("status-bar").role(), Some(Role::ContentInfo));
            assert_eq!(window.find("status").role(), Some(Role::Status));
            assert!(window.find("status").label().is_some());
            assert_eq!(window.find("piano-roll").role(), Some(Role::Group));
            assert_eq!(window.find("lanes").role(), Some(Role::Group));
            assert_eq!(window.find(("lane", 0usize)).role(), Some(Role::Group));
            assert_eq!(window.find("ruler").role(), Some(Role::Slider));
            assert_eq!(window.find("minimap").role(), Some(Role::Slider));
            assert_eq!(window.find("events-list").role(), Some(Role::List));
            assert_eq!(window.find("track-list").role(), Some(Role::List));

            // transport buttons are named toggles
            assert_eq!(window.find("i.play").role(), Some(Role::Button));
            assert_eq!(window.find("i.play").label(), Some(t("tip.play")));
            assert_eq!(window.find("i.loop").checked(), Some(false));

            // track rows expose name + channel + mute/solo state
            assert_eq!(
                window.find(("track", 0usize)).role(),
                Some(Role::ListBoxOption)
            );
            assert_eq!(window.find(("track", 0usize)).label(), Some("Track 1"));
            assert_eq!(window.find(("mute", 0usize)).checked(), Some(false));
            window.click(("mute", 0usize), cx);
            window.render_frame(cx);
            assert_eq!(window.find(("mute", 0usize)).checked(), Some(true));
            assert_eq!(window.find(("solo", 0usize)).checked(), Some(false));

            // opening a menu announces it expanded and exposes MenuItem rows
            assert_eq!(window.find("menu.file").expanded(), Some(false));
            window.click("menu.file", cx);
            window.render_frame(cx);
            assert_eq!(window.find("menu.file").expanded(), Some(true));
            assert_eq!(window.find("menu-popup").role(), Some(Role::Menu));
            assert_eq!(window.find("file.save").role(), Some(Role::MenuItem));
            assert!(window.find("file.save").label().is_some());

            // spinner / spinbutton values are machine-readable
            assert_eq!(window.find("bpm").role(), Some(Role::SpinButton));
            assert_eq!(window.find("snap").role(), Some(Role::SpinButton));
        })
        .unwrap();
    }

    // --- event-properties inspector --------------------------------------

    use super::{edit_event_field, edit_note_field, prop_edit_ops, PropField, PropTarget};
    use document::{Document, Op};
    use smf_core::{Division, EventKind};

    fn chan(tick: u64, seq: u32, status: u8, d0: u8, d1: u8) -> smf_core::Event {
        smf_core::Event {
            tick,
            seq,
            raw_body: None,
            kind: EventKind::Channel {
                status,
                data: [d0, d1],
                len: match status & 0xF0 {
                    0xC0 | 0xD0 => 1,
                    _ => 2,
                },
            },
        }
    }

    fn doc(tracks: Vec<Vec<smf_core::Event>>) -> Document {
        Document::from_file(smf_core::File {
            format: 1,
            division: Division::Metrical(480),
            tracks: tracks
                .into_iter()
                .map(|events| smf_core::Track { events })
                .collect(),
            warnings: vec![],
        })
    }

    fn note_doc() -> Document {
        doc(vec![vec![
            chan(0, 0, 0x90, 60, 100),
            chan(480, 1, 0x80, 60, 40),
        ]])
    }

    fn apply(d: &mut Document, ops: Vec<Op>) {
        d.apply(document::Transaction {
            label: "t".into(),
            base: d.revision(),
            ops,
        })
        .unwrap();
    }

    #[test]
    fn prop_note_fields_edit_on_and_off_events() {
        let mut d = note_doc();
        let n = d.notes().remove(0);
        let ops = edit_note_field(&mut d, n.on_id, PropField::NoteVel, "90").unwrap();
        apply(&mut d, ops);
        let ops = edit_note_field(&mut d, n.on_id, PropField::NoteRelVel, "7").unwrap();
        apply(&mut d, ops);
        let ops = edit_note_field(&mut d, n.on_id, PropField::NoteChannel, "3").unwrap();
        apply(&mut d, ops);
        let ops = edit_note_field(&mut d, n.on_id, PropField::NoteEnd, "960").unwrap();
        apply(&mut d, ops);
        let n = d.notes().remove(0);
        assert_eq!(n.vel, 90);
        assert_eq!(n.channel, 2);
        assert_eq!(n.end_tick, Some(960));
        let (_, ei) = super::find_event(&d, n.off_id.unwrap()).unwrap();
        match &d.tracks[0].events[ei].kind {
            EventKind::Channel { status, data, .. } => {
                assert_eq!(*status, 0x82); // status high nibble kept, ch = 3-1
                assert_eq!(data[1], 7); // release velocity
            }
            _ => panic!("expected channel event"),
        }
    }

    #[test]
    fn prop_note_rejects_bad_numbers_before_ops() {
        let mut d = note_doc();
        let n = d.notes().remove(0);
        assert!(edit_note_field(&mut d, n.on_id, PropField::NoteVel, "0").is_err());
        assert!(edit_note_field(&mut d, n.on_id, PropField::NoteVel, "128").is_err());
        assert!(edit_note_field(&mut d, n.on_id, PropField::NoteVel, "x").is_err());
        assert!(edit_note_field(&mut d, n.on_id, PropField::NoteChannel, "17").is_err());
        assert!(edit_note_field(&mut d, n.on_id, PropField::NoteEnd, "0").is_err());
        assert!(edit_note_field(&mut d, n.on_id, PropField::NoteStart, "480").is_err());
        assert!(edit_note_field(&mut d, n.on_id, PropField::NoteDur, "0").is_err());
    }

    #[test]
    fn prop_dangling_note_rejects_end_edits() {
        let mut d = doc(vec![vec![chan(0, 0, 0x90, 60, 100)]]);
        let n = d.notes().remove(0);
        assert!(n.off_id.is_none());
        assert!(edit_note_field(&mut d, n.on_id, PropField::NoteEnd, "960").is_err());
        assert!(edit_note_field(&mut d, n.on_id, PropField::NoteRelVel, "7").is_err());
        // start/velocity/channel still fine on the on event
        assert!(!edit_note_field(&mut d, n.on_id, PropField::NoteVel, "80")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn prop_event_fields_edit_channel_meta_and_bytes() {
        let mut d = doc(vec![vec![
            chan(0, 0, 0x90, 60, 100),
            chan(0, 1, 0xE0, 0x00, 0x40), // pb center
            smf_core::Event {
                tick: 0,
                seq: 2,
                raw_body: None,
                kind: EventKind::Meta {
                    meta_type: 0x51,
                    data: vec![0x07, 0xA1, 0x20].into(),
                },
            },
        ]]);
        let id_note = d.tracks[0].events[0].id;
        let id_pb = d.tracks[0].events[1].id;
        let id_meta = d.tracks[0].events[2].id;
        // channel: keeps the status high nibble
        let ops = edit_event_field(&mut d, 0, 0, PropField::Channel, "5").unwrap();
        apply(&mut d, ops);
        match &d.tracks[0].events[0].kind {
            EventKind::Channel { status, .. } => assert_eq!(*status, 0x94),
            _ => panic!(),
        }
        // d0/d1
        let ops = edit_event_field(&mut d, 0, 0, PropField::D0, "64").unwrap();
        apply(&mut d, ops);
        let ops = edit_event_field(&mut d, 0, 0, PropField::D1, "30").unwrap();
        apply(&mut d, ops);
        match &d.tracks[0].events[0].kind {
            EventKind::Channel { data, .. } => assert_eq!(*data, [64, 30]),
            _ => panic!(),
        }
        // tick move re-sorts the track — locate by id afterwards
        let ops = edit_event_field(&mut d, 0, 0, PropField::Tick, "240").unwrap();
        apply(&mut d, ops);
        let (ti, ei) = super::find_event(&d, id_note).unwrap();
        assert_eq!(d.tracks[ti].events[ei].tick, 240);
        // pitch bend: -8192..8191 → 14-bit encoding
        let (_, ei_pb) = super::find_event(&d, id_pb).unwrap();
        let ops = edit_event_field(&mut d, 0, ei_pb, PropField::PbValue, "8191").unwrap();
        apply(&mut d, ops);
        match &d.tracks[0].events[ei_pb].kind {
            EventKind::Channel { data, .. } => assert_eq!(*data, [0x7F, 0x7F]),
            _ => panic!(),
        }
        // meta type accepts 0x hex
        let (_, ei_m) = super::find_event(&d, id_meta).unwrap();
        let ops = edit_event_field(&mut d, 0, ei_m, PropField::MetaType, "0x2F").unwrap();
        apply(&mut d, ops);
        match &d.tracks[0].events[ei_m].kind {
            EventKind::Meta { meta_type, .. } => assert_eq!(*meta_type, 0x2F),
            _ => panic!(),
        }
        // hex payload replaces the data bytes
        let ops = edit_event_field(&mut d, 0, ei_m, PropField::HexData, "03 12 ff").unwrap();
        apply(&mut d, ops);
        match &d.tracks[0].events[ei_m].kind {
            EventKind::Meta { data, .. } => assert_eq!(&data[..], &[0x03, 0x12, 0xff]),
            _ => panic!(),
        }
    }

    #[test]
    fn prop_event_rejects_bad_input_and_unsupported_fields() {
        let mut d = doc(vec![vec![
            chan(0, 0, 0x90, 60, 100),
            chan(0, 1, 0xC0, 12, 0), // PC has no d1
        ]]);
        assert!(edit_event_field(&mut d, 0, 0, PropField::Tick, "-1").is_err());
        assert!(edit_event_field(&mut d, 0, 0, PropField::Channel, "0").is_err());
        assert!(edit_event_field(&mut d, 0, 0, PropField::D0, "128").is_err());
        assert!(edit_event_field(&mut d, 0, 0, PropField::PbValue, "9000").is_err());
        assert!(edit_event_field(&mut d, 0, 0, PropField::HexData, "abc").is_err());
        // field unsupported by the kind → empty ops, not an error
        assert!(edit_event_field(&mut d, 0, 0, PropField::MetaType, "1")
            .unwrap()
            .is_empty());
        assert!(edit_event_field(&mut d, 0, 1, PropField::D1, "5")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn prop_batch_edits_every_supported_target() {
        let mut d = doc(vec![vec![
            chan(0, 0, 0x90, 60, 100),
            chan(240, 1, 0xB0, 7, 100),
            smf_core::Event {
                tick: 0,
                seq: 2,
                raw_body: None,
                kind: EventKind::Meta {
                    meta_type: 0x06,
                    data: b"text".to_vec().into(),
                },
            },
        ]]);
        // channel=4 applies to the two channel events, skips the meta one
        let target = PropTarget::Events(vec![
            (0, 0, d.tracks[0].events[0].id),
            (0, 1, d.tracks[0].events[1].id),
            (0, 2, d.tracks[0].events[2].id),
        ]);
        let ops = prop_edit_ops(&mut d, &target, PropField::Channel, "4").unwrap();
        assert_eq!(ops.len(), 2);
        apply(&mut d, ops);
        // tick applies to all three
        let ops = prop_edit_ops(&mut d, &target, PropField::Tick, "100").unwrap();
        assert_eq!(ops.len(), 3);
    }

    /// The stacked-lane sidecar round-trips, and a legacy single-lane
    /// `lane` key still parses on old files.
    #[test]
    fn lane_layout_prefs_round_trip() {
        let p: crate::Prefs = serde_json::from_str(
            r#"{"track_dest":{},"muted":[],"soloed":[],"metronome":false,"loop_enabled":false,"lanes":[{"mode":"cc11","h":80.0,"collapsed":true},{"mode":"pb","h":40.0,"collapsed":false}]}"#,
        )
        .unwrap();
        let ls = p.lanes.unwrap();
        assert_eq!(ls.len(), 2);
        assert_eq!(ls[0].mode, "cc11");
        assert!(ls[0].collapsed);
        assert!(!ls[1].collapsed);
        let back: crate::Prefs = serde_json::from_str(
            r#"{"track_dest":{},"muted":[],"soloed":[],"metronome":false,"loop_enabled":false,"lane":"pb"}"#,
        )
        .unwrap();
        assert!(back.lanes.is_none());
        assert!(matches!(
            crate::lane_mode_parse(back.lane.as_deref().unwrap()),
            crate::LaneMode::PitchBend
        ));
        assert_eq!(crate::lane_mode_str(crate::LaneMode::CC(11)), "cc11");
    }
}
