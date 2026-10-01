//! Phase 1: real SMF document in a modern-editor shell.
//! Open/save .mid, piano roll + event list views, playback to a MIDI port,
//! basic editing (draw / drag / delete) all going through
//! `Document::apply(Transaction)` so undo is shared with MCP edits.

mod a11y;
mod audition;
mod chrome;
mod cmd;
mod diagnostics;
mod docevents;
mod edit_ops;
mod geometry;
mod guard;
mod i18n;
mod icons;
mod menu;
mod nav;
mod playback;
mod plugin_state;
mod plugins;
mod prefs;
mod recording;
mod recovery;
mod render;
mod shutdown;
mod theme;
#[cfg(test)]
mod ui_tests;
mod watch;

// re-export moved items so `use super::*` inside ui_tests/tests keeps working
#[allow(unused_imports)]
pub(crate) use chrome::*;
#[allow(unused_imports)]
pub(crate) use docevents::*;
#[allow(unused_imports)]
pub(crate) use edit_ops::*;
#[allow(unused_imports)]
pub(crate) use nav::*;
#[allow(unused_imports)]
pub(crate) use playback::*;
#[allow(unused_imports)]
pub(crate) use plugins::*;
#[allow(unused_imports)]
pub(crate) use prefs::*;
#[allow(unused_imports)]
pub(crate) use recording::*;

use audition::Audition;
use geometry::{
    clamp_move_delta, clamp_span, content_view, reanchor, roll_hit, ZOOM_MAX, ZOOM_MIN,
};
use i18n::{t, tf};
use menu::{menu_x, next_selectable, row_y, MenuRow, MENUS};

use commands::UndoStack;
use document::{Document, Event as DocEvent, EventId, Note, Op, PositionFormat, TimeDisplay};
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

/// Which loop locator a ruler drag is moving (#130).
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum LoopDrag {
    Start,
    End,
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
    pub(crate) fn key(self) -> &'static str {
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
    pub(crate) fn key(self) -> &'static str {
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
    /// Edit → apply a transform to the entire selected track (#131)
    AllTrack,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LaneMode {
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
    pub(crate) fn cycle(self) -> Self {
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
    pub(crate) fn label(self) -> String {
        match self {
            LaneMode::Velocity => "Vel".to_string(),
            LaneMode::CC(n) => format!("CC{n}"),
            LaneMode::PitchBend => "PB".to_string(),
            LaneMode::ChanAT => "CAT".to_string(),
            LaneMode::PolyAT => "PAT".to_string(),
        }
    }
    /// full-scale value for the lane's y axis
    pub(crate) fn vrange(self) -> f32 {
        match self {
            LaneMode::PitchBend => 16383.0,
            _ => 127.0,
        }
    }
    /// does this lane edit events (vs. velocity bars on the note view)
    pub(crate) fn is_event_lane(self) -> bool {
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
    tempo0: f64,
    /// detected GM/GS/XG reset SysEx — a display hint for patch naming
    mode_hint: Option<smf_core::ModeHint>,
    /// last tick with a note — the scrollable extent of the timeline
    song_end: u64,
}

impl DocUi {
    /// `notes` is the already-derived note view for this revision (passed in
    /// so the pairing pass runs once per revision, not once per consumer).
    /// `seq_sel` scopes markers/tempo/song-end to one sequence for
    /// format-2 documents (None = whole document, formats 0/1).
    pub(crate) fn build(
        doc: &Document,
        notes: &[Note],
        enc_override: Option<smf_core::TextEncoding>,
        seq_sel: Option<usize>,
    ) -> Self {
        let hint = enc_override.or_else(|| doc.text_encoding_hint());
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

type LaneCache =
    HashMap<(LaneMode, Option<u8>), ((u64, u64), usize, Arc<Vec<(EventId, u64, i32, i32)>>)>;

type EvRowOut = (Vec<EvRow>, Vec<Option<(usize, usize, EventId)>>);

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
    lane_caches: LaneCache,
    /// lane marquee selection — event ids of non-note lane points
    lane_sel: BTreeSet<EventId>,
    /// last window-space cursor position, kept while a roll/lane drag is
    /// active so edge auto-scroll can keep the drag deltas current
    mouse_pos: Option<Point<Pixels>>,
    sel_track: usize,
    /// insert/edit channel per track — pure editor state (sidecar), never
    /// an SMF event. Absent entries fall back to the track's `FF 20`
    /// channel prefix. Per-event channels rule playback as always.
    edit_ch: HashMap<usize, u8>,
    /// selected note `on_id`s (marquee multi-select)
    selection: BTreeSet<EventId>,
    drag: Option<Drag>,
    /// active loop-locator drag on the ruler (#130)
    loop_drag: Option<LoopDrag>,
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
    /// µs position where the current transport pass began — the anchor
    /// Return-to-Start and return-on-stop use (#156)
    play_start_us: u64,
    /// return-to-start-on-stop preference (Cubase-style): a transport stop
    /// moves the play point back to where the pass began (#156)
    return_to_start_on_stop: bool,
    /// restart at `loop_start_us` when playback reaches the end
    /// (`loop_enabled` itself lives in `shared` so MCP can toggle it)
    loop_start_us: u64,
    /// live-schedule routing snapshot (#140/#141): dest → sink index, the
    /// transport-lane sink indices, and track → dest as of the last schedule
    /// build. A committed edit with an unchanged dest map needs only an
    /// events patch; a routing change rebuilds sinks and patches both.
    live_sink_of: HashMap<usize, usize>,
    live_transport: Vec<usize>,
    live_dest_of: HashMap<usize, usize>,
    /// a routing refresh was deferred because a newly-assigned plugin is
    /// still loading — retried when its slot reports Ready
    live_route_dirty: bool,
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
    /// reset-on-stop preference: full CC121/120 controller reset on
    /// transport stop — off means a normal stop only releases notes (#161)
    reset_on_stop: bool,
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
    /// overdub vs replace-in-range recording write mode (sidecar pref)
    rec_mode: RecMode,
    /// punch range in ticks — only events inside are committed;
    /// replace mode erases exactly this window
    punch_in: Option<u64>,
    punch_out: Option<u64>,
    /// (track, from_tick, to_tick) of the last committed take — target of
    /// the separate, reversible "quantize take" transaction
    last_take: Option<(usize, u64, u64)>,
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

pub(crate) fn build_dest_catalog(
    plugins: &[output::PluginInfo],
) -> Vec<(String, midi_io::Destination)> {
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

/// Audible-track filter shared by every timeline the schedule consumes:
/// any solo wins over all mutes; with no solos, mutes are the filter.
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
        let (plugin_req, plugin_evt, host_thread) = output::spawn_plugin_host();
        tracing::info!("plugin host worker spawned");
        let hd = output::host_diag();
        tracing::info!(
            helper = ?hd.helper,
            probe = ?hd.probe,
            audio_device = ?hd.audio_device,
            "host diagnostics"
        );
        let mut v = Self::build(
            sh,
            status,
            &path,
            initial_plugins,
            plugin_req,
            plugin_evt,
            hd,
            host_thread,
            &g,
            input,
            prop_input,
            meta_input,
            window,
            cx,
        );
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

    /// Shared initializer: assembles the view around an already-loaded
    /// document + plugin-host channel pair, then derives the view caches.
    /// Side effects (plugin host thread spawn, prefs I/O, scans) stay in `new`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn build(
        sh: Shared,
        status: SharedString,
        path: &Option<PathBuf>,
        initial_plugins: Vec<output::PluginInfo>,
        plugin_req: std::sync::mpsc::Sender<output::PluginReq>,
        plugin_evt: std::sync::mpsc::Receiver<output::PluginEvent>,
        host_diag: output::HostDiag,
        host_thread: std::thread::JoinHandle<()>,
        g: &GlobalPrefs,
        input: Entity<InputState>,
        prop_input: Entity<InputState>,
        meta_input: Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let shared = Arc::new(Mutex::new(sh));
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
            loop_drag: None,
            tool: Tool::Select,
            snap_idx: 7, // 1/16
            erase_ids: BTreeSet::new(),
            edit_ch: HashMap::new(),
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
            host_diag,
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
            play_start_us: 0,
            return_to_start_on_stop: g.return_to_start_on_stop.unwrap_or(true),
            loop_start_us: 0,
            live_sink_of: HashMap::new(),
            live_transport: Vec::new(),
            live_dest_of: HashMap::new(),
            live_route_dirty: false,
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
            reset_on_stop: g.reset_on_stop,
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
            rec_mode: RecMode::Overdub,
            punch_in: None,
            punch_out: None,
            last_take: None,
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
        v
    }

    /// Recompute the active palette from prefs + OS flags and push the
    /// matching mode onto gpui-component so text inputs stay legible.
    pub(crate) fn apply_theme(&mut self, cx: &mut Context<Self>) {
        self.theme = theme::Theme::resolve(self.hc_pref, self.theme_mode, self.sys_dark);
        let mode = if self.theme == theme::Theme::light() {
            gpui_kit::component::theme::ThemeMode::Light
        } else {
            gpui_kit::component::theme::ThemeMode::Dark
        };
        gpui_kit::component::theme::Theme::change(mode, None, cx);
        cx.notify();
    }

    pub(crate) fn set_theme_mode(&mut self, mode: theme::ThemeMode, cx: &mut Context<Self>) {
        self.theme_mode = mode;
        self.apply_theme(cx);
        self.save_global();
    }

    pub(crate) fn on_sys_appearance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    /// Test-only constructor: skips the plugin host thread, plugin scan,
    /// audio probe, and user prefs so tests stay inert and deterministic.
    /// The shared doc has no path, so `persist()` is a no-op.
    #[cfg(test)]
    pub(crate) fn new_for_test(
        doc: Document,
        input: Entity<InputState>,
        prop_input: Entity<InputState>,
        meta_input: Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut sh = Shared::new(doc);
        sh.path = None;
        sh.saved_revision = sh.doc.revision();
        sh.dests = vec![
            (
                "Test MIDI Out".into(),
                midi_io::Destination::MidiPort {
                    port_name: "Test MIDI Out".into(),
                    ord: 0,
                },
            ),
            (
                "Test Synth".into(),
                midi_io::Destination::Plugin {
                    plugin_path: "C:/Fixtures/TestSynth.vst3".into(),
                    component_id: None,
                    vendor: None,
                    plugin_name: None,
                },
            ),
        ];
        // no host worker: requests sent to plugin_req go nowhere and no
        // plugin events ever arrive, which is exactly the inert state
        // the plugin-unavailable golden wants
        let (plugin_req, _req_rx) = std::sync::mpsc::channel::<output::PluginReq>();
        let (_evt_tx, plugin_evt) = std::sync::mpsc::channel::<output::PluginEvent>();
        Self::build(
            sh,
            "test document".into(),
            &None,
            Vec::new(),
            plugin_req,
            plugin_evt,
            output::HostDiag {
                helper: None,
                probe: None,
                audio_device: Err("test audio".into()),
            },
            std::thread::spawn(|| {}),
            &GlobalPrefs::default(),
            input,
            prop_input,
            meta_input,
            window,
            cx,
        )
    }

    pub(crate) fn doc<R>(&self, f: impl FnOnce(&Document) -> R) -> R {
        let sh = lock_shared(&self.shared);
        f(&sh.doc)
    }

    pub(crate) fn refresh_derived(&mut self) {
        let arc = self.shared.clone();
        let mut sh = lock_shared(&arc);
        self.refresh_derived_sh(&mut sh);
    }

    pub(crate) fn refresh_derived_sh(&mut self, sh: &mut Shared) {
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
    pub(crate) fn lane_events_cached(
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
    pub(crate) fn poly_keys_present(&self) -> Vec<u8> {
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
    pub(crate) fn cycle_poly_key(&mut self, back: bool, cx: &mut Context<Self>) {
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
    pub(crate) fn doc_end_ticks(&self) -> u64 {
        // at least four coarse cells: real bars under the viewed track's
        // meter map for metrical, displayed seconds for SMPTE
        let min_extent = self.doc(|d| match d.time_display() {
            TimeDisplay::Metrical { .. } => d.meter_map_for(self.sel_track).bar_ticks_at(0) * 4,
            TimeDisplay::Smpte { .. } => d.time_display().bar_ticks() * 4,
        });
        if self.is_seq() {
            self.doc(|d| d.track_end_tick(self.sel_track))
                .max(min_extent)
        } else {
            self.doc_ui.song_end.max(min_extent)
        }
    }

    /// Whether the loaded file is SMF format 2 — tracks are independent
    /// sequences, never a single shared song timeline.
    pub(crate) fn is_seq(&self) -> bool {
        self.doc(|d| d.is_sequential())
    }

    /// Switch the viewed track/sequence. Format 2 clears the note
    /// selection: notes of another sequence are neither visible nor
    /// editable while this one is being shown and played.
    pub(crate) fn select_track(&mut self, i: usize, cx: &mut Context<Self>) {
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
    pub(crate) fn td(&self) -> TimeDisplay {
        self.doc(|d| d.time_display())
    }

    /// Event id → semantic tag for events that participate in an RPN/NRPN
    /// write (selector or data entry) or look like stray data entry CCs.
    pub(crate) fn rpn_row_tags(doc: &Document) -> HashMap<EventId, String> {
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

    pub(crate) fn build_event_rows(&self, doc: &Document) -> EvRowOut {
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
                // bar.beat.tick under the real FF58 map for metrical,
                // hh:mm:ss.ff timecode for SMPTE — position labels
                // always match the file's timing
                let pos = doc.format_position_for(ti, e.tick);
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

    pub(crate) fn apply_tx(&mut self, label: &str, ops: Vec<Op>) {
        let arc = self.shared.clone();
        {
            let mut sh = lock_shared(&arc);
            match sh.apply(label, ops) {
                Ok(_) => self.refresh_derived_sh(&mut sh),
                Err(e) => self.status = tf("status.apply_failed", &[("e", &e.to_string())]).into(),
            }
        }
        // committed transactions reach the running pass (#141)
        self.refresh_live_schedule();
    }

    // --- event-properties inspector -------------------------------------

    // --- keyboard focus & navigation ----------------------------------------

    /// Ordered, bounded teardown of everything the app owns. Runs from
    /// `on_window_should_close` and `on_app_quit`; the Shutdown latch
    /// makes the second call a no-op.
    pub(crate) fn perform_shutdown(&mut self) {
        let mut sd = std::mem::take(&mut self.shutdown);
        sd.run(self);
        self.shutdown = sd;
    }

    // --- note audition (issue #39) -------------------------------------------
}

impl Drop for EditorView {
    fn drop(&mut self) {
        // last line of defense: no preview note outlives the view
        self.audition.all_off();
    }
}

/// First non-flag argument = the file to open. `args_os` (not `args`) because
/// a shell-open verb can deliver non-UTF-8 paths on Windows and `args()`
/// panics on them; Explorer always quotes "%1", but flags like `--foo` must
/// never be mistaken for a filename.
fn file_arg() -> Option<PathBuf> {
    file_arg_from(std::env::args_os().skip(1))
}

fn file_arg_from(mut args: impl Iterator<Item = std::ffi::OsString>) -> Option<PathBuf> {
    args.find(|a| !a.to_string_lossy().starts_with('-'))
        .map(PathBuf::from)
}

fn main() {
    // exact build identity for bug reports — same string as Help>About and
    // MCP serverInfo: "<semver>+<commit>[.dirty]"
    if std::env::args_os()
        .skip(1)
        .any(|a| a == "--version" || a == "-V")
    {
        println!(
            "midi-editor {} ({} {})",
            env!("BUILD_IDENTITY"),
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        return;
    }
    // --smoke <in.mid> [copy.mid]: load via the normal document path, print a
    // document summary, optionally save a copy, exit. Package validation runs
    // this on machines with no audio hardware or GPU session, so it must run
    // before any audio/VST environment setup or gpui init.
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("--smoke") {
        std::process::exit(smoke(&argv));
    }
    output::init_env();
    let log_dir = diagnostics::init_logging();
    diagnostics::install_panic_hook();
    diagnostics::log_boot();
    tracing::info!(log_dir = %log_dir.display(), "logging initialized");
    let path = file_arg();
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

/// Headless package-validation path used by release CI: parse the input,
/// build the document (by_id index + tempo map), report a summary in the
/// same shape as the MCP `document_summary` tool, and when an output path
/// is given serialize a copy through the normal save path.
fn smoke(argv: &[String]) -> i32 {
    let Some(input) = argv.get(2).map(PathBuf::from) else {
        eprintln!("usage: midi-editor --smoke <in.mid> [copy.mid]");
        return 2;
    };
    let bytes = match std::fs::read(&input) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("smoke: read {}: {e}", input.display());
            return 1;
        }
    };
    let file = match smf_core::parse(&bytes) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("smoke: parse {}: {e}", input.display());
            return 1;
        }
    };
    let doc = document::Document::from_file(file);
    let last_tick = doc
        .tracks
        .iter()
        .flat_map(|t| t.events.iter().map(|e| e.tick))
        .max()
        .unwrap_or(0);
    let summary = serde_json::json!({
        "format": doc.format,
        "division": format!("{:?}", doc.division),
        "tracks": doc.tracks.len(),
        "events": doc.tracks.iter().map(|t| t.events.len()).sum::<usize>(),
        "notes": doc.notes().len(),
        "last_tick": last_tick,
        "duration_us": doc.tempo_map.tick_to_us(last_tick),
        "revision": doc.revision(),
    });
    println!("smoke summary: {summary}");
    if let Some(out) = argv.get(3).map(PathBuf::from) {
        let bytes = doc.serialize(smf_core::WriteOptions {
            running_status: true,
        });
        if let Err(e) = std::fs::write(&out, &bytes) {
            eprintln!("smoke: write {}: {e}", out.display());
            return 1;
        }
        println!("smoke wrote {} bytes to {}", bytes.len(), out.display());
    }
    0
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
#[cfg(test)]
mod tests {
    use crate::{
        assemble_events, empty_doc, file_arg_from, plugin_plan, route_events, track_audible,
        GlobalPrefs, PluginPlan, PluginState, Prefs,
    };
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    /// Every freshly parsed document reports revision 0, so the derived-view
    /// caches must not key on the revision alone: before `doc_epoch` existed,
    /// opening a file right after an untouched document (also revision 0)
    /// skipped the cache rebuild and the roll stayed empty until the next
    /// edit happened to bump the revision.
    #[test]
    pub(crate) fn fresh_documents_share_revision_zero() {
        assert_eq!(empty_doc().revision(), empty_doc().revision());
    }

    pub(crate) fn testdir(name: &str) -> std::path::PathBuf {
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
    pub(crate) fn sidecar_v0_migrates_to_v1() {
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
    pub(crate) fn sidecar_corruption_recovers_backup() {
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
    pub(crate) fn sidecar_newer_version_keeps_known_fields() {
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
    pub(crate) fn sidecar_bad_numerics_are_clamped() {
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
    pub(crate) fn sidecar_missing_destinations_still_parse() {
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
    pub(crate) fn global_prefs_v0_loads_and_newer_survives() {
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
    pub(crate) fn legacy_prefs_json_without_audio_fields_parses() {
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
    pub(crate) fn audio_selection_missing_fields_are_system_default() {
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
    pub(crate) fn transport_points_cover_tempo_and_meter_at_document_positions() {
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

    pub(crate) fn loading(path: &Path) -> PluginState {
        PluginState::Loading {
            path: path.to_path_buf(),
            since: Instant::now(),
        }
    }

    pub(crate) fn failed(path: &Path) -> PluginState {
        PluginState::Failed {
            path: path.to_path_buf(),
            phase: "load",
            msg: String::new(),
        }
    }

    pub(crate) fn ready(path: &Path) -> PluginState {
        PluginState::Ready {
            path: path.to_path_buf(),
        }
    }

    /// Host lifecycle ordering for one destination index: the wanted bundle
    /// stays resident, duplicate loads are suppressed while one is in flight,
    /// and a re-pointed index retires its stale instance before opening the
    /// next — the slot never holds two plugins at once.
    #[test]
    pub(crate) fn plugin_plan_lifecycle_ordering() {
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
    pub(crate) fn a11y_tree_exposes_primary_controls(cx: &mut TestAppContext) {
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

    pub(crate) fn chan(tick: u64, seq: u32, status: u8, d0: u8, d1: u8) -> smf_core::Event {
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

    pub(crate) fn doc(tracks: Vec<Vec<smf_core::Event>>) -> Document {
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

    pub(crate) fn note_doc() -> Document {
        doc(vec![vec![
            chan(0, 0, 0x90, 60, 100),
            chan(480, 1, 0x80, 60, 40),
        ]])
    }

    pub(crate) fn apply(d: &mut Document, ops: Vec<Op>) {
        d.apply(document::Transaction {
            label: "t".into(),
            base: d.revision(),
            ops,
        })
        .unwrap();
    }

    #[test]
    pub(crate) fn prop_note_fields_edit_on_and_off_events() {
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
    pub(crate) fn prop_note_rejects_bad_numbers_before_ops() {
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
    pub(crate) fn prop_dangling_note_rejects_end_edits() {
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
    pub(crate) fn prop_event_fields_edit_channel_meta_and_bytes() {
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
        // meta type accepts 0x hex (sequencer-specific — structural
        // types like 0x2F End-of-Track are covered below)
        let (_, ei_m) = super::find_event(&d, id_meta).unwrap();
        let ops = edit_event_field(&mut d, 0, ei_m, PropField::MetaType, "0x7F").unwrap();
        apply(&mut d, ops);
        let (_, ei_m) = super::find_event(&d, id_meta).unwrap();
        match &d.tracks[0].events[ei_m].kind {
            EventKind::Meta { meta_type, .. } => assert_eq!(*meta_type, 0x7F),
            _ => panic!(),
        }
        // hex payload replaces the data bytes
        let ops = edit_event_field(&mut d, 0, ei_m, PropField::HexData, "03 12 ff").unwrap();
        apply(&mut d, ops);
        let (_, ei_m) = super::find_event(&d, id_meta).unwrap();
        match &d.tracks[0].events[ei_m].kind {
            EventKind::Meta { data, .. } => assert_eq!(&data[..], &[0x03, 0x12, 0xff]),
            _ => panic!(),
        }
        // converting an event to End-of-Track collapses to a single
        // terminator at the track end — exactly one EOT survives, its
        // payload is no longer field-editable
        let ops = edit_event_field(&mut d, 0, ei_m, PropField::MetaType, "0x2F").unwrap();
        apply(&mut d, ops);
        let n_eot = d.tracks[0]
            .events
            .iter()
            .filter(|e| {
                matches!(
                    e.kind,
                    EventKind::Meta {
                        meta_type: 0x2F,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(n_eot, 1, "exactly one End-of-Track survives");
        let ei_m = d.tracks[0]
            .events
            .iter()
            .position(|e| {
                matches!(
                    e.kind,
                    EventKind::Meta {
                        meta_type: 0x2F,
                        ..
                    }
                )
            })
            .unwrap();
        assert_eq!(ei_m, d.tracks[0].events.len() - 1, "EOT not at track end");
        assert!(edit_event_field(&mut d, 0, ei_m, PropField::HexData, "00").is_err());
        // its tick stays editable (intentional silent tail)
        assert!(!edit_event_field(&mut d, 0, ei_m, PropField::Tick, "960")
            .unwrap()
            .is_empty());
    }

    #[test]
    pub(crate) fn prop_event_rejects_bad_input_and_unsupported_fields() {
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
    pub(crate) fn prop_batch_edits_every_supported_target() {
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
    pub(crate) fn lane_layout_prefs_round_trip() {
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

    pub(crate) fn note(key: u8) -> Vec<u8> {
        vec![0x90, key, 100]
    }

    pub(crate) fn set(xs: &[usize]) -> HashSet<usize> {
        xs.iter().copied().collect()
    }

    #[test]
    pub(crate) fn track_audible_solo_wins_over_mute() {
        let muted = set(&[1, 2]);
        let soloed = set(&[2]);
        assert!(!track_audible(0, &muted, &soloed));
        assert!(!track_audible(1, &muted, &soloed));
        assert!(track_audible(2, &muted, &soloed));
    }

    #[test]
    pub(crate) fn track_audible_mute_filters_without_solo() {
        let muted = set(&[1]);
        let soloed = set(&[]);
        assert!(track_audible(0, &muted, &soloed));
        assert!(!track_audible(1, &muted, &soloed));
        assert!(track_audible(2, &muted, &soloed));
    }

    #[test]
    pub(crate) fn route_events_filters_and_remaps_to_sinks() {
        // tracks → dests: t0→d0, t1→d1, t2→d0; sinks opened for d0,d2 only
        let timeline = vec![
            (10, 0, note(60)),
            (20, 1, note(62)), // muted out
            (30, 2, note(64)),
            (40, 1, note(65)), // muted out
        ];
        let sink_of: HashMap<usize, usize> = HashMap::from([(0, 0), (2, 1)]);
        let got = route_events(
            timeline,
            |tr| tr != 1,       // audible = "not track 1"
            |tr| [0, 1, 0][tr], // dest_of
            &sink_of,
        );
        // t0→d0→sink0, t2→d0→sink0; a d2 event would map to sink1
        assert_eq!(got, vec![(10, 0, note(60)), (30, 0, note(64))]);
        // destination with no open sink → event dropped
        let sink_of2: HashMap<usize, usize> = HashMap::from([(2, 0)]);
        let got2 = route_events(
            vec![(10, 0, note(60)), (20, 3, note(62))],
            |_| true,
            |_| 0, // every track targets dest 0
            &sink_of2,
        );
        assert!(got2.is_empty());
    }

    #[test]
    pub(crate) fn assemble_keeps_sysex_before_channel_before_click_at_equal_us() {
        // concatenated in sysex, channel, click order — stable sort must keep
        // that order when µs tie
        let events = vec![
            (100, 0, vec![0xF0, 0x7E, 0xF7]), // sysex
            (100, 0, note(60)),               // channel
            (100, 0, vec![0x99, 76, 110]),    // click
        ];
        let got = assemble_events(events, vec![], vec![], 0);
        assert_eq!(got[0].2, vec![0xF0, 0x7E, 0xF7]);
        assert_eq!(got[1].2, note(60));
        assert_eq!(got[2].2, vec![0x99, 76, 110]);
    }

    #[test]
    pub(crate) fn assemble_sorts_across_lists_even_without_metronome() {
        // regression: the merged list used to sort only under `if metronome`,
        // so a later SysEx sent before an earlier note and the note fired
        // late every pass
        let events = vec![
            (300, 0, vec![0xF0, 0x7E, 0xF7]), // sysex at 300µs
            (100, 0, note(60)),               // channel event at 100µs
        ];
        let got = assemble_events(events, vec![], vec![], 0);
        assert_eq!(got[0].2, note(60));
        assert_eq!(got[1].2, vec![0xF0, 0x7E, 0xF7]);
    }

    #[test]
    pub(crate) fn assemble_splices_chase_before_first_event_at_start() {
        let events = vec![(50, 0, note(60)), (150, 0, note(64)), (250, 0, note(65))];
        let chase = vec![(100, 0, vec![0xB0, 7, 90])];
        let got = assemble_events(events, chase, vec![], 100);
        // chase lands after the past event (50), before the first at/after (150)
        assert_eq!(got[1].2, vec![0xB0, 7, 90]);
        assert_eq!(got[2].2, note(64));
    }

    #[test]
    pub(crate) fn assemble_splices_chase_sysex_before_channel_chase() {
        let events = vec![(100, 0, note(60))];
        let chase = vec![(100, 0, vec![0xB0, 7, 90])];
        let chase_sx = vec![(100, 0, vec![0xF0, 0x7E, 0xF7])];
        let got = assemble_events(events, chase, chase_sx, 100);
        // chased reset first, then chased channel state, then the real event
        assert_eq!(got[0].2, vec![0xF0, 0x7E, 0xF7]);
        assert_eq!(got[1].2, vec![0xB0, 7, 90]);
        assert_eq!(got[2].2, note(60));
    }

    #[test]
    pub(crate) fn file_arg_skips_flags_and_picks_first_path() {
        use std::ffi::OsString;
        let args = vec![
            OsString::from("--fullscreen"),
            OsString::from(r"C:\Music\my song.mid"),
            OsString::from("extra.mid"),
        ];
        assert_eq!(
            file_arg_from(args.into_iter()),
            Some(PathBuf::from(r"C:\Music\my song.mid"))
        );
        assert_eq!(file_arg_from(Vec::new().into_iter()), None);
        assert_eq!(
            file_arg_from(vec![OsString::from("--only-flags")].into_iter()),
            None
        );
    }
}
