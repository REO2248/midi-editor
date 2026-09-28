//! Phase 1: real SMF document in a modern-editor shell.
//! Open/save .mid, piano roll + event list views, playback to a MIDI port,
//! basic editing (draw / drag / delete) all going through
//! `Document::apply(Transaction)` so undo is shared with MCP edits.

mod i18n;
use i18n::t;

use commands::UndoStack;
use document::{Document, Event as DocEvent, EventId, Note, Op};
use mcp_server::{Shared, SharedDoc};
use std::sync::Mutex;
use smf_core::EventKind;
use gpui_kit::*;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::Root;
use midi_io::{EventSink, Playback, PortSink};
use std::collections::{BTreeSet, HashMap, HashSet};
use smf_core::Division;
use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

const NOTE_H: f32 = 13.0;
const TRACK_COLORS: [u32; 8] = [
    0x4f8cff, 0xff8c4f, 0x4fd08c, 0xd04fff, 0xffd24f, 0x4fd0ff, 0xff4f7a, 0x9dff4f,
];
const SEL_COLOR: u32 = 0xffffff;
const DANGLING_COLOR: u32 = 0xff4f4f;

/// One selectable output destination: a hardware/software MIDI port or a
/// hosted VST3 instrument.
enum DestKind {
    /// index into midir's port list
    Midi(usize),
    /// path to the .vst3 bundle
    Plugin(PathBuf),
}

struct DestSpec {
    label: String,
    kind: DestKind,
}

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

struct EditorView {
    shared: SharedDoc,
    notes_rev: u64,
    notes: Arc<Vec<Note>>,
    ev_rev: u64,
    events: Arc<Vec<SharedString>>,
    sel_track: usize,
    /// selected note `on_id`s (marquee multi-select)
    selection: BTreeSet<EventId>,
    drag: Option<Drag>,
    /// canvas bounds as painted last frame — for hit-testing
    roll_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// seek-ruler strip bounds
    ruler_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// velocity lane bounds — same trick for the lane's hit-testing
    lane_bounds: Rc<Cell<Bounds<Pixels>>>,
    scroll_x: f32,
    scroll_y: f32,
    zoom: f32,
    /// All playback destinations: MIDI ports (GS Wavetable, loopMIDI,
    /// physical IFs) followed by discovered VST3 plugins.
    dests: Vec<DestSpec>,
    /// Dest used by tracks with no explicit assignment.
    default_dest: usize,
    /// track index -> index into `dests`
    track_dest: HashMap<usize, usize>,
    muted: HashSet<usize>,
    soloed: HashSet<usize>,
    /// Plugin instances opened for the current playback, keyed by dest index;
    /// dropping them stops their audio streams.
    active_plugins: Vec<(usize, output::PluginOutput)>,
    /// Standalone native window showing a plugin's GUI editor.
    plugin_window: Option<vst3_host::PluginWindow>,
    /// GUI-only plugin instance (loaded without audio when not playing);
    /// kept alive so the editor window stays valid.
    gui_plugin: Option<std::sync::Arc<std::sync::Mutex<vst3_host::Plugin>>>,
    /// Manual text-encoding override for display decoding (None = auto/XF hint)
    enc_override: Option<smf_core::TextEncoding>,
    playback: Option<Playback>,
    play_us: u64,
    /// restart at `loop_start_us` when playback reaches the end
    loop_enabled: bool,
    loop_start_us: u64,
    /// what the bottom lane edits
    lane_mode: LaneMode,
    focus: FocusHandle,
    input: Entity<InputState>,
    status: SharedString,
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
        let mut sh = Shared::new(match &path {
            Some(p) => match load_document(p) {
                Ok(d) => d,
                Err(_) => empty_doc(),
            },
            None => empty_doc(),
        });
        let status: SharedString = match &path {
            Some(p) => match load_document(p) {
                Ok(_) => "loaded".into(),
                Err(e) => format!("load failed: {e}").into(),
            },
            None => "new document".into(),
        };
        sh.path = path.clone();
        sh.saved_revision = sh.doc.revision();
        let shared = Arc::new(Mutex::new(sh));
        let mut v = Self {
            shared,
            notes_rev: u64::MAX,
            notes: Arc::new(vec![]),
            ev_rev: u64::MAX,
            events: Arc::new(vec![]),
            sel_track: 0,
            selection: BTreeSet::new(),
            drag: None,
            roll_bounds: Rc::new(Cell::new(Bounds::new(point(px(0.0), px(0.0)), size(px(0.0), px(0.0))))),
            ruler_bounds: Rc::new(Cell::new(Bounds::new(point(px(0.0), px(0.0)), size(px(0.0), px(0.0))))),
            lane_bounds: Rc::new(Cell::new(Bounds::new(point(px(0.0), px(0.0)), size(px(0.0), px(0.0))))),
            scroll_x: 0.0,
            scroll_y: (127.0 - 84.0) * NOTE_H, // show ~C3..C7
            zoom: 0.08,
            dests: {
                let mut d: Vec<DestSpec> = midi_io::list_outputs()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|p| DestSpec {
                        label: p.name,
                        kind: DestKind::Midi(p.index),
                    })
                    .collect();
                for p in output::discover_plugins() {
                    d.push(DestSpec {
                        label: format!("{} [VST3]", p.name),
                        kind: DestKind::Plugin(p.path),
                    });
                }
                d
            },
            default_dest: 0,
            track_dest: HashMap::new(),
            muted: HashSet::new(),
            soloed: HashSet::new(),
            active_plugins: Vec::new(),
            plugin_window: None,
            gui_plugin: None,
            enc_override: None,
            playback: None,
            play_us: 0,
            loop_enabled: false,
            loop_start_us: 0,
            lane_mode: LaneMode::Velocity,
            focus: cx.focus_handle(),
            input,
            status,
        };
        v.sel_track = v.pick_default_track();
        v.refresh_derived();
        v
    }

    fn doc<R>(&self, f: impl FnOnce(&Document) -> R) -> R {
        let sh = self.shared.lock().unwrap();
        f(&sh.doc)
    }

    fn pick_default_track(&self) -> usize {
        self.doc(|d| d
            .tracks
            .iter()
            .position(|t| {
                t.events
                    .iter()
                    .any(|e| matches!(e.kind, EventKind::Channel { .. }))
            })
            .unwrap_or(0))
    }

    fn refresh_derived(&mut self) {
        let arc = self.shared.clone();
        let mut sh = arc.lock().unwrap();
        self.refresh_derived_sh(&mut sh);
    }

    fn refresh_derived_sh(&mut self, sh: &mut Shared) {
        let rev = sh.doc.revision();
        if self.notes_rev != rev {
            self.notes = Arc::new(sh.doc.notes());
            self.notes_rev = rev;
        }
        if self.ev_rev != rev {
            self.events = Arc::new(self.build_event_rows(&sh.doc));
            self.ev_rev = rev;
        }
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
                        0x58 => format!("TimeSig {}/{}", data.get(0).copied().unwrap_or(4), data.get(1).copied().unwrap_or(4)),
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
        let mut sh = arc.lock().unwrap();
        match sh.apply(label, ops) {
            Ok(_) => self.refresh_derived_sh(&mut sh),
            Err(e) => self.status = format!("apply: {e}").into(),
        }
    }

    fn insert_note(&mut self, tick: u64, key: u8, cx: &mut Context<Self>) {
        let ppq = self.ppq();
        let (on_id, off_id, track) = {
            let mut sh = self.shared.lock().unwrap();
            let track = self.sel_track.min(sh.doc.tracks.len().saturating_sub(1));
            (sh.doc.alloc_event_id(), sh.doc.alloc_event_id(), track)
        };
        let snap = ppq / 4;
        let tick = (tick / snap) * snap;
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
            tick: tick + ppq,
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
        let sh = self.shared.lock().unwrap();
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

    fn commit_drag(&mut self, cx: &mut Context<Self>) {
        let Some(d) = self.drag.take() else { return };
        match d.mode {
            DragMode::Marquee => {
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
                let new_end = ((orig_end as i64 + d.dtick).max(d.orig_start as i64 + 1)) as u64;
                let sh = self.shared.lock().unwrap();
                let mut ops = Vec::new();
                if let Some(off_id) = d.off_id {
                    for e in &sh.doc.tracks[d.track].events {
                        if e.id == off_id {
                            let mut after = e.clone();
                            after.tick = new_end;
                            after.raw_body = None;
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
                    self.apply_tx("resize note", ops);
                }
                cx.notify();
                return;
            }
            DragMode::Velocity => {
                let vel = d.dkey.clamp(1, 127) as u8;
                let sh = self.shared.lock().unwrap();
                let mut ops = Vec::new();
                for e in &sh.doc.tracks[d.track].events {
                    if e.id == d.on_id {
                        let mut after = e.clone();
                        if let EventKind::Channel { data, .. } = &mut after.kind {
                            data[1] = vel;
                        }
                        after.raw_body = None;
                        ops.push(Op::UpdateEvent {
                            track: d.track,
                            before: e.clone(),
                            after,
                        });
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
                let mut sh = self.shared.lock().unwrap();
                let mut ops = Vec::new();
                if d.on_id == 0 {
                    let ch = sh.doc.tracks[d.track].out_channel & 0x0F;
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
                            tick: d.a_tick.max(0) as u64,
                            seq: 0,
                            raw_body: None,
                            kind: EventKind::Channel { status, data, len: 2 },
                        }],
                    });
                } else {
                    for e in &sh.doc.tracks[d.track].events {
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
                            after.raw_body = None;
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
        // move every selected note by the same delta; fall back to the
        // dragged note if the selection never took (e.g. programmatic)
        let ids: BTreeSet<EventId> = if self.selection.contains(&d.on_id) {
            self.selection.clone()
        } else {
            BTreeSet::from([d.on_id])
        };
        let notes = self.notes.clone();
        let duplicate = d.mode == DragMode::Duplicate;
        let mut sh = self.shared.lock().unwrap();
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
                after.raw_body = None; // re-encode from kind
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
                let before = sh
                    .doc
                    .tracks[ti]
                    .events
                    .iter()
                    .find(|e| e.id == {
                        // original id is preserved on `after` for Move
                        after.id
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
            self.apply_tx(if duplicate { "duplicate notes" } else { "move notes" }, ops);
        }
        cx.notify();
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        let arc = self.shared.clone();
        let mut sh = arc.lock().unwrap();
        if let Some(l) = { let Shared { doc, undo, .. } = &mut *sh; undo.undo(doc) } {
            self.status = format!("undo {l}").into();
            self.selection.clear();
            self.refresh_derived_sh(&mut sh);
            drop(sh);
            cx.notify();
        }
    }

    fn redo(&mut self, cx: &mut Context<Self>) {
        let arc = self.shared.clone();
        let mut sh = arc.lock().unwrap();
        if let Some(l) = { let Shared { doc, undo, .. } = &mut *sh; undo.redo(doc) } {
            self.status = format!("redo {l}").into();
            self.selection.clear();
            self.refresh_derived_sh(&mut sh);
            drop(sh);
            cx.notify();
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let path = {
            let sh = self.shared.lock().unwrap();
            sh.path.clone()
        };
        match path {
            Some(p) => {
                let mut sh = self.shared.lock().unwrap();
                let bytes = sh.doc.serialize(smf_core::WriteOptions {
                    running_status: false,
                });
                match std::fs::write(&p, bytes) {
                    Ok(_) => {
                        sh.saved_revision = sh.doc.revision();
                        self.status = t("status.saved").into();
                    }
                    Err(e) => self.status = format!("{e}").into(),
                }
            }
            None => self.save_as(cx),
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
                        v.shared.lock().unwrap().path = Some(path);
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
            Ok(d) => {
                self.stop_playback();
                // swap the document in place — the MCP server holds this same Arc
                {
                    let mut sh = self.shared.lock().unwrap();
                    sh.doc = d;
                    sh.undo = UndoStack::new(512);
                    sh.path = Some(path);
                    sh.saved_revision = sh.doc.revision();
                }
                self.sel_track = self.pick_default_track();
                self.selection.clear();
                self.refresh_derived();
                self.status = "loaded".into();
            }
            Err(e) => self.status = format!("{e}").into(),
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

    /// Effective destination index for a track: its explicit assignment or
    /// the global default.
    fn dest_of(&self, track: usize) -> usize {
        self.track_dest
            .get(&track)
            .copied()
            .unwrap_or(self.default_dest)
            .min(self.dests.len().saturating_sub(1))
    }

    /// Whether a track's events reach an output: muted tracks never play;
    /// when anything is soloed, only soloed tracks play.
    fn audible(&self, track: usize) -> bool {
        if !self.soloed.is_empty() {
            self.soloed.contains(&track)
        } else {
            !self.muted.contains(&track)
        }
    }

    fn start_playback(&mut self) {
        if self.dests.is_empty() {
            self.status = t("status.no_port").into();
            return;
        }
        let tagged: Vec<(u64, usize, Vec<u8>)> = self
            .doc(|d| d.timeline_tagged())
            .into_iter()
            .filter(|(_, tr, _)| self.audible(*tr))
            .collect();
        // open each destination that at least one event needs
        let needed: BTreeSet<usize> =
            tagged.iter().map(|(_, tr, _)| self.dest_of(*tr)).collect();
        let mut sinks: Vec<Box<dyn EventSink>> = Vec::new();
        let mut sink_of: HashMap<usize, usize> = HashMap::new();
        self.active_plugins.clear();
        for d in needed {
            let Some(spec) = self.dests.get(d) else { continue };
            match &spec.kind {
                DestKind::Midi(idx) => match midi_io::Output::open(*idx) {
                    Ok(out) => {
                        sink_of.insert(d, sinks.len());
                        sinks.push(Box::new(PortSink::new(out)));
                    }
                    Err(e) => self.status = format!("{e}").into(),
                },
                DestKind::Plugin(path) => {
                    match output::PluginOutput::open(path) {
                        Ok(plugin) => {
                            sink_of.insert(d, sinks.len());
                            sinks.push(Box::new(plugin.event_sink()));
                            self.active_plugins.push((d, plugin));
                        }
                        Err(e) => self.status = format!("{e}").into(),
                    }
                }
            }
        }
        if sinks.is_empty() {
            self.status = t("status.no_port").into();
            return;
        }
        let events: Vec<(u64, usize, Vec<u8>)> = tagged
            .into_iter()
            .filter_map(|(us, tr, b)| {
                sink_of.get(&self.dest_of(tr)).map(|&s| (us, s, b))
            })
            .collect();
        self.loop_start_us = self.play_us;
        self.playback = Some(Playback::start(sinks, events, self.play_us));
    }

    fn stop_playback(&mut self) {
        if let Some(mut p) = self.playback.take() {
            self.play_us = p.position_us();
            p.stop();
        }
        self.active_plugins.clear();
    }

    /// tick,key under a window-space mouse position
    fn hit(&self, pos: Point<Pixels>) -> (i64, i32) {
        let b = self.roll_bounds.get();
        let x = f32::from(pos.x) - f32::from(b.origin.x);
        let y = f32::from(pos.y) - f32::from(b.origin.y);
        let tick = ((x + self.scroll_x) / self.zoom) as i64;
        let key = 127.0 - (y + self.scroll_y) / NOTE_H;
        (tick.max(0), key.round() as i32)
    }

    fn note_at(&self, pos: Point<Pixels>) -> Option<Note> {
        let (tick, key) = self.hit(pos);
        self.notes.iter().rev().find(|n| {
            n.key as i32 == key
                && tick >= n.start_tick as i64
                && tick <= n.end_tick.unwrap_or(n.start_tick + self.ppq() / 4) as i64
        }).cloned()
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

    fn button(label: &'static str, cx: &Context<Self>, on: impl Fn(&mut Self, &mut Context<Self>) + 'static) -> Stateful<Div> {
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
}



fn load_document(path: &PathBuf) -> Result<Document, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let file = smf_core::parse(&bytes).map_err(|e| e.to_string())?;
    Ok(Document::from_file(file))
}

impl Render for EditorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // pump the plugin editor window's native event queue while it's open
        if let Some(w) = &mut self.plugin_window {
            let _ = w.service_platform_events();
            if w.closed_by_user() {
                self.plugin_window = None;
                self.gui_plugin = None;
            }
        }
        self.refresh_derived();

        // advance playhead / auto-stop
        if let Some(p) = &self.playback {
            self.play_us = p.position_us();
            if !p.is_running() {
                self.playback = None;
                if self.loop_enabled {
                    self.play_us = self.loop_start_us;
                    self.start_playback();
                } else {
                    self.play_us = 0;
                }
            }
        }
        let (playhead_tick, title, dirty, n_diags, track_names) = {
            let sh = self.shared.lock().unwrap();
            let hint = self.enc_override.or(sh.doc.text_encoding_hint());
            (
                sh.doc.tempo_map.us_to_tick(self.play_us),
                sh.path
                    .as_ref()
                    .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .unwrap_or_else(|| t("status.no_file").to_string()),
                sh.doc.revision() != sh.saved_revision,
                sh.doc.diagnose().len(),
                sh.doc
                    .tracks
                    .iter()
                    .enumerate()
                    .map(|(i, tr)| {
                        tr.name
                            .as_ref()
                            .map(|b| smf_core::decode_text(b, hint))
                            .unwrap_or_else(|| format!("Track {}", i + 1))
                    })
                    .collect::<Vec<String>>(),
            )
        };
        let ppq = self.ppq();
        let pos = {
            let bar = playhead_tick / (ppq * 4) + 1;
            let beat = (playhead_tick % (ppq * 4)) / ppq + 1;
            format!("{bar}.{beat}.{:>3}", playhead_tick % ppq)
        };
        let port_label = {
            let eff = self.dest_of(self.sel_track);
            let name = self
                .dests
                .get(eff)
                .map(|d| d.label.clone())
                .unwrap_or_else(|| t("status.no_port").to_string());
            let mark = if self.track_dest.contains_key(&self.sel_track) {
                ""
            } else {
                "*"
            };
            format!("T{}{} ▸ {}", self.sel_track + 1, mark, name)
        };

        // --- piano roll canvas -------------------------------------------------
        let notes = self.notes.clone();
        let (scroll_x, scroll_y, zoom) = (self.scroll_x, self.scroll_y, self.zoom);
        let selection = self.selection.clone();
        let drag = self
            .drag
            .as_ref()
            .map(|d| (d.mode, d.on_id, d.dtick, d.dkey));
        let marquee = self.drag.as_ref().and_then(|d| {
            (d.mode == DragMode::Marquee).then_some((d.a_tick, d.a_key, d.b_tick, d.b_key))
        });
        let bounds_cell = self.roll_bounds.clone();
        let playing = self.playback.is_some();
        let play_x_tick = playhead_tick;

        let roll = canvas(
            move |bounds, _window, _cx| {
                bounds_cell.set(bounds);
            },
            move |bounds, _state, window, _cx| {
                if playing {
                    window.request_animation_frame();
                }
                let w = bounds.size.width;
                let h = bounds.size.height;
                // key rows
                let k0 = (scroll_y / NOTE_H).max(0.0) as i32;
                let k1 = ((scroll_y + f32::from(h)) / NOTE_H + 1.0).min(128.0) as i32;
                for k in k0..k1 {
                    let black = matches!(k % 12, 1 | 3 | 6 | 8 | 10);
                    let y = bounds.origin.y + px(k as f32 * NOTE_H - scroll_y);
                    if black {
                        window.paint_quad(fill(
                            Bounds::new(point(bounds.origin.x, y), size(w, px(NOTE_H))),
                            rgb(0x1a1a21),
                        ));
                    }
                    window.paint_quad(fill(
                        Bounds::new(point(bounds.origin.x, y), size(w, px(1.0))),
                        rgb(if k % 12 == 0 { 0x2e2e3a } else { 0x232329 }),
                    ));
                }
                // beat/bar lines
                let tick0 = (scroll_x / zoom).max(0.0) as u64;
                let tick1 = tick0 + (f32::from(w) / zoom) as u64 + ppq;
                let mut t = tick0 / ppq * ppq;
                while t <= tick1 {
                    let x = bounds.origin.x + px(t as f32 * zoom - scroll_x);
                    let bar = t % (ppq * 4) == 0;
                    window.paint_quad(fill(
                        Bounds::new(point(x, bounds.origin.y), size(px(1.0), h)),
                        rgb(if bar { 0x3d3d52 } else { 0x2a2a35 }),
                    ));
                    t += ppq;
                }
                // notes
                for n in notes.iter() {
                    let mut st = n.start_tick as i64;
                    let mut en = n.end_tick.unwrap_or(n.start_tick + ppq / 4) as i64;
                    let mut key = n.key as i32;
                    let mut ghost_orig = false;
                    if let Some((mode, d_on, dtick, dkey)) = drag {
                        match mode {
                            DragMode::Move | DragMode::Duplicate
                                if d_on == n.on_id || selection.contains(&n.on_id) =>
                            {
                                if mode == DragMode::Duplicate {
                                    ghost_orig = true;
                                }
                                st += dtick;
                                en += dtick;
                                key += dkey;
                            }
                            DragMode::Resize if d_on == n.on_id => {
                                en = (en + dtick).max(st + 1);
                            }
                            _ => {}
                        }
                    }
                    if ghost_orig {
                        // alt-drag copy: keep the source visible underneath
                        let ox = bounds.origin.x + px(n.start_tick as f32 * zoom - scroll_x);
                        let ow = ((n.end_tick.unwrap_or(n.start_tick) - n.start_tick).max(1) as f32
                            * zoom)
                            .max(3.0);
                        let oy = bounds.origin.y
                            + px((127.0 - n.key as f32) * NOTE_H - scroll_y);
                        window.paint_quad(fill(
                            Bounds::new(point(ox, oy + px(1.0)), size(px(ow), px(NOTE_H - 2.0))),
                            rgba(0x80808044),
                        ));
                    }
                    let x = bounds.origin.x + px(st as f32 * zoom - scroll_x);
                    let wpx = ((en - st).max(1) as f32 * zoom).max(3.0);
                    if x + px(wpx) < bounds.origin.x {
                        continue;
                    }
                    if x > bounds.origin.x + w {
                        break;
                    }
                    let y = bounds.origin.y + px((127.0 - key as f32) * NOTE_H - scroll_y);
                    if y < bounds.origin.y - px(NOTE_H) || y > bounds.origin.y + h {
                        continue;
                    }
                    let c = if selection.contains(&n.on_id) {
                        SEL_COLOR
                    } else if n.end_tick.is_none() {
                        DANGLING_COLOR
                    } else {
                        TRACK_COLORS[n.track % TRACK_COLORS.len()]
                    };
                    window.paint_quad(fill(
                        Bounds::new(point(x, y + px(1.0)), size(px(wpx), px(NOTE_H - 2.0))),
                        rgb(c),
                    ));
                }
                // playhead
                let px_x = bounds.origin.x + px(play_x_tick as f32 * zoom - scroll_x);
                if px_x >= bounds.origin.x && px_x <= bounds.origin.x + w {
                    window.paint_quad(fill(
                        Bounds::new(point(px_x, bounds.origin.y), size(px(1.5), h)),
                        rgb(0x50ff9f),
                    ));
                }
                // marquee rubber band
                if let Some((a_t, a_k, b_t, b_k)) = marquee {
                    let (t0, t1) = (a_t.min(b_t), a_t.max(b_t));
                    let (k0, k1) = (a_k.min(b_k), a_k.max(b_k));
                    let x0 = bounds.origin.x + px(t0 as f32 * zoom - scroll_x);
                    let x1 = bounds.origin.x + px(t1 as f32 * zoom - scroll_x);
                    let y0 = bounds.origin.y + px((127.0 - k1 as f32) * NOTE_H - scroll_y);
                    let y1 = bounds.origin.y + px((127.0 - k0 as f32) * NOTE_H - scroll_y);
                    window.paint_quad(fill(
                        Bounds::new(point(x0, y0), size(x1 - x0, y1 - y0)),
                        rgba(0x4f8cff33),
                    ));
                }
            },
        );

        // --- header -------------------------------------------------------------
        let play_label: &'static str = if self.playback.is_some() {
            t("menu.stop")
        } else {
            t("menu.play")
        };
        let sel_is_plugin = matches!(
            self.dests
                .get(self.dest_of(self.sel_track))
                .map(|s| &s.kind),
            Some(DestKind::Plugin(_))
        );
        let header = div()
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .h(px(42.0))
            .bg(rgb(0x14141a))
            .child(div().text_color(rgb(0x8f8fb0)).child(t("app.title")))
            .child(
                div()
                    .text_color(if dirty { rgb(0xffd24f) } else { rgb(0xd8d8e0) })
                    .child(format!("{title}{}", if dirty { " *" } else { "" })),
            )
            .child(Self::button("menu.open", cx, |v, cx| v.open_dialog(cx)))
            .child(Self::button("menu.save", cx, |v, cx| v.save(cx)))
            .child(div().w(px(8.0)))
            .child(
                div()
                    .id("play")
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(rgb(0x245c3a))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(0x2f7a4d)))
                    .child(play_label)
                    .on_click(cx.listener(|v, _e, _w, cx| v.toggle_play(cx))),
            )
            .child(
                div()
                    .id("loop")
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .bg(if self.loop_enabled {
                        rgb(0x3a5c2a)
                    } else {
                        rgb(0x2a2a35)
                    })
                    .hover(|s| s.bg(rgb(0x3a3a48)))
                    .text_color(rgb(if self.loop_enabled {
                        0xb4ff8c
                    } else {
                        0x8f8fb0
                    }))
                    .child("loop")
                    .on_click(cx.listener(|v, _e, _w, cx| {
                        v.loop_enabled = !v.loop_enabled;
                        cx.notify();
                    })),
            )
            .child(div().text_color(rgb(0x8f8fb0)).font_family("Cascadia Mono").child(pos))
            .child(div().w(px(8.0)))
            .child(
                div()
                    .id("port")
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(rgb(0x2a2a35))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(0x3a3a48)))
                    .text_color(rgb(0x9fd0ff))
                    .child(port_label)
                    .on_click(cx.listener(|v, _e, _w, cx| {
                        if !v.dests.is_empty() {
                            // cycles the SELECTED track's assignment through
                            // [inherit default] -> dest 0..n -> inherit
                            let n = v.dests.len();
                            let tr = v.sel_track;
                            match v.track_dest.get(&tr).copied() {
                                None => v.track_dest.insert(tr, 0),
                                Some(d) if d + 1 < n => {
                                    v.track_dest.insert(tr, d + 1)
                                }
                                Some(_) => v.track_dest.remove(&tr),
                            };
                            if let Some(&d) = v.track_dest.get(&tr) {
                                v.default_dest = d;
                            }
                        }
                        cx.notify();
                    })),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("enc")
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(rgb(0x2a2a35))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(0x3a3a48)))
                    .text_color(rgb(0xc8c8a8))
                    .text_size(px(11.0))
                    .child(format!(
                        "ENC {}",
                        match self.enc_override {
                            None => "auto",
                            Some(smf_core::TextEncoding::Utf8) => "UTF-8",
                            Some(smf_core::TextEncoding::ShiftJis) => "SJIS",
                            Some(smf_core::TextEncoding::Latin1) => "Latin-1",
                        }
                    ))
                    .on_click(cx.listener(|v, _e, _w, cx| {
                        v.enc_override = match v.enc_override {
                            None => Some(smf_core::TextEncoding::Utf8),
                            Some(smf_core::TextEncoding::Utf8) => {
                                Some(smf_core::TextEncoding::ShiftJis)
                            }
                            Some(smf_core::TextEncoding::ShiftJis) => {
                                Some(smf_core::TextEncoding::Latin1)
                            }
                            Some(smf_core::TextEncoding::Latin1) => None,
                        };
                        v.ev_rev = u64::MAX; // force event-row rebuild
                        v.refresh_derived();
                        cx.notify();
                    })),
            )
            .children(sel_is_plugin.then(|| {
                div()
                    .id("plug-gui")
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(rgb(0x2a2a35))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(0x3a3a48)))
                    .text_color(rgb(0xd0a8ff))
                    .text_size(px(11.0))
                    .child("GUI")
                    .on_click(cx.listener(|v, _e, _w, cx| {
                        let d = v.dest_of(v.sel_track);
                        let path = v.dests.get(d).and_then(|s| match &s.kind {
                            DestKind::Plugin(p) => Some(p.clone()),
                            _ => None,
                        });
                        if let Some(path) = path {
                            // prefer the instance already driving playback
                            let live = v
                                .active_plugins
                                .iter()
                                .find(|(i, _)| *i == d)
                                .map(|(_, p)| p.plugin_handle());
                            let (arc, is_live) = match live {
                                Some(a) => (Some(a), true),
                                None => (output::load_for_gui(&path).ok(), false),
                            };
                            match arc {
                                Some(a) => {
                                    let mut w = vst3_host::PluginWindow::new(a.clone());
                                    match w.open() {
                                        Ok(()) => {
                                            if !is_live {
                                                v.gui_plugin = Some(a);
                                            }
                                            v.plugin_window = Some(w);
                                        }
                                        Err(e) => {
                                            v.status = format!("plugin GUI: {e}").into()
                                        }
                                    }
                                }
                                None => {
                                    v.status = "plugin load failed".into();
                                }
                            }
                        }
                        cx.notify();
                    }))
            }))
            .child(div().w(px(8.0)))
            .child(div().w(px(240.0)).child(Input::new(&self.input)))
            .child(
                div()
                    .text_color(rgb(0x77778a))
                    .text_size(px(11.0))
                    .child(format!("{}", self.status)),
            );

        // --- body: event list + roll -------------------------------------------
        let body = div().flex().flex_1().min_h(px(0.0)).child(
            div()
                .w(px(380.0))
                .h_full()
                .flex()
                .flex_col()
                .bg(rgb(0x17171d))
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .text_size(px(11.0))
                        .text_color(rgb(0x77778a))
                        .child(format!("{} ({})", t("events.header"), self.events.len()))
                        .children((n_diags > 0).then(|| {
                            div()
                                .id("fix-diags")
                                .ml_2()
                                .px_1()
                                .text_size(px(10.0))
                                .text_color(rgb(0xffb454))
                                .cursor_pointer()
                                .child(format!("{} {} [fix]", n_diags, t("events.issues")))
                                .on_click(cx.listener(|v, _e, _w, cx| {
                                    let ops = {
                                        let mut sh = v.shared.lock().unwrap();
                                        sh.doc.fix_ops(&[])
                                    };
                                    if !ops.is_empty() {
                                        v.apply_tx("fix diagnostics", ops);
                                        v.status = t("status.fixed").into();
                                    }
                                    cx.notify();
                                }))
                        })),
                )
                .child({
                    let events = self.events.clone();
                    uniform_list("events", events.len(), move |range, _w, _cx| {
                        range
                            .map(|i| {
                                div()
                                    .h(px(18.0))
                                    .px_2()
                                    .text_size(px(11.0))
                                    .font_family("Cascadia Mono")
                                    .text_color(rgb(0xb8b8c8))
                                    .child(events[i].clone())
                            })
                            .collect()
                    })
                    .h_full()
                    .flex_1()
                }),
        );

        // --- track column: select / mute / solo -------------------------------
        let track_col = div()
            .w(px(140.0))
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(0x1b1b24))
            .child(
                div()
                    .px_2()
                    .py_1()
                    .text_size(px(11.0))
                    .text_color(rgb(0x77778a))
                    .child(t("tracks.header")),
            )
            .children(track_names.iter().enumerate().map(|(i, name)| {
                let sel = self.sel_track == i;
                let muted = self.muted.contains(&i);
                let soloed = self.soloed.contains(&i);
                let color = TRACK_COLORS[i % TRACK_COLORS.len()];
                div()
                    .id(("track", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .h(px(22.0))
                    .px_1()
                    .cursor_pointer()
                    .bg(if sel { rgb(0x2a2a3a) } else { rgb(0x1b1b24) })
                    .hover(|s| s.bg(rgb(0x252532)))
                    .on_click(cx.listener(move |v, _e, _w, cx| {
                        v.sel_track = i;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .w(px(10.0))
                            .h(px(10.0))
                            .rounded_sm()
                            .bg(rgb(if muted { 0x555560 } else { color })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .px_1()
                            .text_size(px(11.0))
                            .text_color(rgb(if muted { 0x707080 } else { 0xd8d8e0 }))
                            .overflow_hidden()
                            .child(format!("{name}")),
                    )
                    .child(
                        div()
                            .id(("mute", i))
                            .px_1()
                            .text_size(px(9.0))
                            .text_color(rgb(if muted { 0xffb454 } else { 0x707080 }))
                            .on_click(cx.listener(move |v, _e: &ClickEvent, _w, cx| {
                                cx.stop_propagation();
                                if !v.muted.remove(&i) {
                                    v.muted.insert(i);
                                }
                                cx.notify();
                            }))
                            .child("M"),
                    )
                    .child(
                        div()
                            .id(("solo", i))
                            .px_1()
                            .text_size(px(9.0))
                            .text_color(rgb(if soloed { 0xffd24f } else { 0x707080 }))
                            .on_click(cx.listener(move |v, _e: &ClickEvent, _w, cx| {
                                cx.stop_propagation();
                                if !v.soloed.remove(&i) {
                                    v.soloed.insert(i);
                                }
                                cx.notify();
                            }))
                            .child("S"),
                    )
            }));

        // velocity / CC / pitch-bend lane (selected track only)
        let lane_sel_track = self.sel_track;
        let lane_mode = self.lane_mode;
        // control events of the selected track matching the lane mode:
        // (event id, tick, value 0..127 or 0..16383 for PB)
        let lane_events: Vec<(EventId, u64, i32)> = {
            let sh = self.shared.lock().unwrap();
            let tr = lane_sel_track.min(sh.doc.tracks.len().saturating_sub(1));
            let mut v = Vec::new();
            if let Some(t) = sh.doc.tracks.get(tr) {
                for e in &t.events {
                    if let EventKind::Channel { status, data, .. } = &e.kind {
                        match (lane_mode, status & 0xF0) {
                            (LaneMode::CC(cc), 0xB0) if data[0] == cc => {
                                v.push((e.id, e.tick, data[1] as i32))
                            }
                            (LaneMode::PitchBend, 0xE0) => v.push((
                                e.id,
                                e.tick,
                                ((data[1] as i32) << 7) | data[0] as i32,
                            )),
                            _ => {}
                        }
                    }
                }
            }
            v.sort_by_key(|e| e.1);
            v
        };
        let lane_bounds_cell = self.lane_bounds.clone();
        let lane = canvas(
            move |bounds, _window, _cx| {
                lane_bounds_cell.set(bounds);
            },
            {
                let lane_notes = self.notes.clone();
                let lane_selection = self.selection.clone();
                let lane_events = lane_events.clone();
                let drag_v = drag;
                move |bounds, _state, window, _cx| {
                    let h: f32 = bounds.size.height.into();
                    let vrange = if lane_mode == LaneMode::PitchBend { 16383.0 } else { 127.0 };
                    match lane_mode {
                        LaneMode::Velocity => {
                            for n in lane_notes.iter().filter(|n| n.track == lane_sel_track) {
                                let x = bounds.origin.x + px(n.start_tick as f32 * zoom - scroll_x);
                                if x < bounds.origin.x || x > bounds.origin.x + bounds.size.width {
                                    continue;
                                }
                                let mut vel = n.vel as f32 / 127.0;
                                if let Some((DragMode::Velocity, d_on, _, dkey)) = drag_v {
                                    if d_on == n.on_id {
                                        vel = (dkey as f32 / 127.0).clamp(0.0, 1.0);
                                    }
                                }
                                let bh = px((h - 6.0) * vel);
                                let y = bounds.origin.y + px(h) - bh - px(3.0);
                                let c = if lane_selection.contains(&n.on_id) {
                                    SEL_COLOR
                                } else {
                                    TRACK_COLORS[n.track % TRACK_COLORS.len()]
                                };
                                window.paint_quad(fill(
                                    Bounds::new(point(x, y), size(px(2.0), bh)),
                                    rgb(c),
                                ));
                            }
                        }
                        _ => {
                            // stepped automation line: dot + run to next point
                            let mut prev: Option<(Pixels, Pixels)> = None;
                            for (id, tick, val) in &lane_events {
                                let mut v = *val;
                                if let Some((DragMode::LaneEvent, d_on, _, dkey)) = drag_v {
                                    if d_on == *id {
                                        v = dkey.clamp(0, vrange as i32);
                                    }
                                }
                                let x = bounds.origin.x + px(*tick as f32 * zoom - scroll_x);
                                let y = bounds.origin.y
                                    + px((h - 4.0) * (1.0 - v as f32 / vrange) + 2.0);
                                if let Some((px_, py_)) = prev {
                                    // horizontal run at previous level, then
                                    // a vertical connector at this event's x
                                    window.paint_quad(fill(
                                        Bounds::new(
                                            point(px_, py_),
                                            size(x - px_, px(1.0)),
                                        ),
                                        rgba(0x4fd0ff88),
                                    ));
                                    window.paint_quad(fill(
                                        Bounds::new(
                                            point(x, y.min(py_)),
                                            size(px(1.0), (y - py_).abs().max(px(1.0))),
                                        ),
                                        rgba(0x4fd0ff88),
                                    ));
                                }
                                window.paint_quad(fill(
                                    Bounds::new(point(x - px(2.0), y - px(2.0)), size(px(4.0), px(4.0))),
                                    rgb(0x4fd0ff),
                                ));
                                prev = Some((x, y));
                            }
                            // drag insert ghost
                            if let Some((DragMode::LaneEvent, 0, a_tick, dkey)) = drag_v {
                                let x = bounds.origin.x + px(a_tick as f32 * zoom - scroll_x);
                                let y = bounds.origin.y
                                    + px((h - 4.0) * (1.0 - dkey.clamp(0, vrange as i32) as f32 / vrange) + 2.0);
                                window.paint_quad(fill(
                                    Bounds::new(point(x - px(2.0), y - px(2.0)), size(px(4.0), px(4.0))),
                                    rgb(SEL_COLOR),
                                ));
                            }
                        }
                    }
                }
            },
        );

        // seek ruler: bar ticks/numbers, click positions the playhead
        let ruler_bounds_cell = self.ruler_bounds.clone();
        let ruler_play_tick = playhead_tick;
        let ruler = canvas(
            move |bounds, _window, _cx| {
                ruler_bounds_cell.set(bounds);
            },
            move |bounds, _state, window, _cx| {
                let w = bounds.size.width;
                let tick0 = (scroll_x / zoom).max(0.0) as u64;
                let tick1 = tick0 + (f32::from(w) / zoom) as u64 + ppq * 4;
                let mut t = tick0 / (ppq * 4) * (ppq * 4);
                while t <= tick1 {
                    let x = bounds.origin.x + px(t as f32 * zoom - scroll_x);
                    window.paint_quad(fill(
                        Bounds::new(point(x, bounds.origin.y + px(12.0)), size(px(1.0), px(8.0))),
                        rgb(0x55556a),
                    ));
                    t += ppq * 4;
                }
                // playhead marker
                let hx = bounds.origin.x + px(ruler_play_tick as f32 * zoom - scroll_x);
                if hx >= bounds.origin.x && hx <= bounds.origin.x + w {
                    window.paint_quad(fill(
                        Bounds::new(point(hx - px(2.0), bounds.origin.y), size(px(4.0), px(12.0))),
                        rgb(0x50ff9f),
                    ));
                }
            },
        );

        let body = body.child(track_col).child(
            div()
                .flex_1()
                .h_full()
                .flex()
                .flex_col()
                .child(
                    div()
                        .h(px(26.0))
                        .w_full()
                        .bg(rgb(0x17171d))
                        .border_b_1()
                        .border_color(rgb(0x2a2a35))
                        .cursor_pointer()
                        .child(ruler.size_full())
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, ev: &MouseDownEvent, _w, cx| {
                                let b = this.ruler_bounds.get();
                                let x = f32::from(ev.position.x) - f32::from(b.origin.x);
                                let tick = ((x + this.scroll_x) / this.zoom).max(0.0) as u64;
                                let us = this.doc(|d| d.tempo_map.tick_to_us(tick));
                                let was_playing = this.playback.is_some();
                                if was_playing {
                                    this.stop_playback();
                                }
                                this.play_us = us;
                                if was_playing {
                                    this.start_playback();
                                }
                                this.status = format!("seek {tick}").into();
                                cx.notify();
                            }),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .relative()
                        .overflow_hidden()
                        .child(roll.size_full())
                        .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _w, cx| {
                    let d = ev.delta.pixel_delta(px(20.0));
                    if ev.modifiers.control {
                        this.zoom = (this.zoom * (1.0 - d.y.to_f64() as f32 * 0.002)).clamp(0.005, 0.8);
                    } else {
                        this.scroll_x = (this.scroll_x + d.x.to_f64() as f32).max(0.0);
                        this.scroll_y = (this.scroll_y + d.y.to_f64() as f32).max(0.0);
                    }
                    cx.notify();
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, ev: &MouseDownEvent, w, cx| {
                        w.focus(&this.focus, cx);
                        let shift = ev.modifiers.shift;
                        if let Some(n) = this.edge_at(ev.position) {
                            this.sel_track = n.track;
                            this.drag = Some(Drag {
                                mode: DragMode::Resize,
                                on_id: n.on_id,
                                off_id: n.off_id,
                                track: n.track,
                                orig_start: n.start_tick,
                                orig_end: n.end_tick,
                                orig_key: n.key,
                                dtick: 0,
                                dkey: 0,
                                a_tick: 0,
                                a_key: 0,
                                b_tick: 0,
                                b_key: 0,
                            });
                        } else if let Some(n) = this.note_at(ev.position) {
                            if shift {
                                if !this.selection.remove(&n.on_id) {
                                    this.selection.insert(n.on_id);
                                }
                            } else if !this.selection.contains(&n.on_id) {
                                this.selection = BTreeSet::from([n.on_id]);
                            }
                            this.sel_track = n.track;
                            this.drag = Some(Drag {
                                mode: if ev.modifiers.alt {
                                    DragMode::Duplicate
                                } else {
                                    DragMode::Move
                                },
                                on_id: n.on_id,
                                off_id: n.off_id,
                                track: n.track,
                                orig_start: n.start_tick,
                                orig_end: n.end_tick,
                                orig_key: n.key,
                                dtick: 0,
                                dkey: 0,
                                a_tick: 0,
                                a_key: 0,
                                b_tick: 0,
                                b_key: 0,
                            });
                        } else {
                            let (tick, key) = this.hit(ev.position);
                            if (0..=127).contains(&key) {
                                if !shift {
                                    this.selection.clear();
                                }
                                // becomes a marquee on drag; a click without
                                // drag inserts a note at the anchor
                                this.drag = Some(Drag {
                                    mode: DragMode::Marquee,
                                    on_id: 0,
                                    off_id: None,
                                    track: 0,
                                    orig_start: 0,
                                    orig_end: None,
                                    orig_key: 0,
                                    dtick: 0,
                                    dkey: 0,
                                    a_tick: tick,
                                    a_key: key,
                                    b_tick: tick,
                                    b_key: key,
                                });
                            }
                        }
                        cx.notify();
                    }),
                )
                .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, cx| {
                    if ev.pressed_button != Some(MouseButton::Left) {
                        return;
                    }
                    let (tick, key) = this.hit(ev.position);
                    let Some(d) = &mut this.drag else { return };
                    match d.mode {
                        DragMode::Move => {
                            d.dtick = tick - d.orig_start as i64;
                            d.dkey = key - d.orig_key as i32;
                        }
                        DragMode::Resize => {
                            d.dtick = tick - d.orig_end.unwrap_or(d.orig_start) as i64;
                        }
                        DragMode::Marquee => {
                            d.b_tick = tick;
                            d.b_key = key;
                        }
                        DragMode::Velocity | DragMode::LaneEvent => {}
                        DragMode::Duplicate => {
                            d.dtick = tick - d.orig_start as i64;
                            d.dkey = key - d.orig_key as i32;
                        }
                    }
                    cx.notify();
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, ev: &MouseUpEvent, _w, cx| {
                        // a marquee that never left its anchor = click → insert
                        let click_insert = this.drag.as_ref().is_some_and(|d| {
                            d.mode == DragMode::Marquee
                                && (d.b_tick - d.a_tick).abs() < 2
                                && (d.b_key - d.a_key).abs() == 0
                        });
                        if click_insert {
                            let d = this.drag.take().unwrap();
                            if (0..=127).contains(&d.a_key) {
                                this.insert_note(d.a_tick.max(0) as u64, d.a_key as u8, cx);
                            }
                        } else {
                            this.commit_drag(cx);
                        }
                        let _ = ev;
                    }),
                )
                )
                .child(
                    div()
                        .h(px(56.0))
                        .w_full()
                        .bg(rgb(0x14141a))
                        .border_t_1()
                        .border_color(rgb(0x2a2a35))
                        .relative()
                        .child(lane.size_full())
                        .child(
                            // lane-mode chip: Vel -> CC1 -> CC7 -> CC10 ->
                            // CC11 -> CC64 -> PB -> Vel
                            div()
                                .id("lane-mode")
                                .absolute()
                                .top(px(2.0))
                                .right(px(4.0))
                                .px_1()
                                .rounded_sm()
                                .bg(rgb(0x2a2a35))
                                .cursor_pointer()
                                .hover(|s| s.bg(rgb(0x3a3a48)))
                                .text_size(px(9.0))
                                .text_color(rgb(0x9fd0ff))
                                .child(lane_mode.label())
                                .on_click(cx.listener(|v, _e: &ClickEvent, _w, cx| {
                                    cx.stop_propagation();
                                    v.lane_mode = v.lane_mode.cycle();
                                    cx.notify();
                                })),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, ev: &MouseDownEvent, _w, cx| {
                                let b = this.lane_bounds.get();
                                let x = f32::from(ev.position.x) - f32::from(b.origin.x);
                                let y = f32::from(ev.position.y) - f32::from(b.origin.y);
                                let tick = ((x + this.scroll_x) / this.zoom).max(0.0) as u64;
                                let h = f32::from(b.size.height);
                                match this.lane_mode {
                                    LaneMode::Velocity => {
                                        let vel =
                                            ((1.0 - y / h) * 127.0) as i32;
                                        // nearest note in the selected track
                                        if let Some(n) = this
                                            .notes
                                            .iter()
                                            .filter(|n| n.track == this.sel_track)
                                            .min_by_key(|n| {
                                                let st = n.start_tick as i64;
                                                (tick as i64 - st).abs()
                                            })
                                        {
                                            this.selection = BTreeSet::from([n.on_id]);
                                            this.drag = Some(Drag {
                                                mode: DragMode::Velocity,
                                                on_id: n.on_id,
                                                off_id: n.off_id,
                                                track: n.track,
                                                orig_start: n.start_tick,
                                                orig_end: n.end_tick,
                                                orig_key: n.key,
                                                dtick: 0,
                                                dkey: vel.clamp(1, 127),
                                                a_tick: 0,
                                                a_key: 0,
                                                b_tick: 0,
                                                b_key: 0,
                                            });
                                        }
                                    }
                                    mode => {
                                        // CC/PB: grab the nearest lane event
                                        // within ~10px, else insert a new one
                                        // at the click and drag it
                                        let vrange = if mode == LaneMode::PitchBend {
                                            16383.0
                                        } else {
                                            127.0
                                        };
                                        let val =
                                            ((1.0 - y / h) * vrange) as i32;
                                        let tr = this.sel_track;
                                        let found = {
                                            let sh = this.shared.lock().unwrap();
                                            sh.doc.tracks.get(tr).and_then(|t| {
                                                t.events
                                                    .iter()
                                                    .filter(|e| {
                                                        matches!(e.kind,
                                                            EventKind::Channel { status, data, .. }
                                                            if match mode {
                                                                LaneMode::CC(cc) => status & 0xF0 == 0xB0 && data[0] == cc,
                                                                LaneMode::PitchBend => status & 0xF0 == 0xE0,
                                                                LaneMode::Velocity => false,
                                                            })
                                                    })
                                                    .min_by_key(|e| {
                                                        (e.tick as i64 - tick as i64).abs()
                                                    })
                                                    .filter(|e| {
                                                        ((e.tick as f32 - tick as f32) * this.zoom)
                                                            .abs()
                                                            <= 10.0
                                                    })
                                                    .map(|e| e.id)
                                            })
                                        };
                                        this.drag = Some(Drag {
                                            mode: DragMode::LaneEvent,
                                            on_id: found.unwrap_or(0),
                                            off_id: None,
                                            track: tr,
                                            orig_start: 0,
                                            orig_end: None,
                                            orig_key: 0,
                                            dtick: 0,
                                            dkey: val.clamp(0, vrange as i32),
                                            a_tick: tick as i64,
                                            a_key: 0,
                                            b_tick: 0,
                                            b_key: 0,
                                        });
                                    }
                                }
                                cx.notify();
                            }),
                        )
                        .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, cx| {
                            let Some(d) = &mut this.drag else { return };
                            if !matches!(d.mode, DragMode::Velocity | DragMode::LaneEvent)
                                || ev.pressed_button != Some(MouseButton::Left)
                            {
                                return;
                            }
                            let b = this.lane_bounds.get();
                            let y = f32::from(ev.position.y) - f32::from(b.origin.y);
                            let h = f32::from(b.size.height);
                            d.dkey = if d.mode == DragMode::Velocity {
                                ((1.0 - y / h) * 127.0) as i32
                            } else {
                                let vrange = match this.lane_mode {
                                    LaneMode::PitchBend => 16383.0,
                                    _ => 127.0,
                                };
                                ((1.0 - y / h) * vrange) as i32
                            };
                            cx.notify();
                        }))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _ev: &MouseUpEvent, _w, cx| {
                                this.commit_drag(cx)
                            }),
                        ),
                ),
        );

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x1b1b22))
            .text_color(rgb(0xd8d8e0))
            .key_context("editor")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _w, cx| {
                let k = ev.keystroke.key.as_str();
                let ctrl = ev.keystroke.modifiers.control;
                let shift = ev.keystroke.modifiers.shift;
                match (ctrl, shift, k) {
                    (true, false, "z") => this.undo(cx),
                    (true, false, "y") | (true, true, "z") => this.redo(cx),
                    (true, false, "s") => this.save(cx),
                    (true, false, "o") => this.open_dialog(cx),
                    (false, false, "delete") | (false, false, "backspace") => {
                        this.delete_selected(cx)
                    }
                    (false, false, " ") => this.toggle_play(cx),
                    _ => {}
                }
            }))
            .child(header)
            .child(body)
    }
}

fn main() {
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
                let input = cx.new(|cx| {
                    InputState::new(window, cx).placeholder(t("field.track_name"))
                });
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
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
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
            let cur = shared.lock().unwrap().gui_notify.load(std::sync::atomic::Ordering::Relaxed);
            let dirty = cur != last;
            if dirty {
                last = cur;
            }
            if let Some(this) = this.upgrade() {
                this.update(cx, |v, cx| {
                    if dirty {
                        v.refresh_derived();
                    }
                    // repaint while playing so the playhead/counter advance;
                    // also while a plugin editor is open so its native event
                    // queue gets serviced even when the app is idle
                    if dirty || v.playback.is_some() || v.plugin_window.is_some() {
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
