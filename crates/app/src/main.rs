//! Phase 1: real SMF document in a modern-editor shell.
//! Open/save .mid, piano roll + event list views, playback to a MIDI port,
//! basic editing (draw / drag / delete) all going through
//! `Document::apply(Transaction)` so undo is shared with MCP edits.

mod i18n;
mod render;
use i18n::t;

use commands::UndoStack;
use document::{Document, Event as DocEvent, EventId, Note, Op};
use mcp_server::{Shared, SharedDoc};
use std::sync::Mutex;
use smf_core::EventKind;
use gpui_kit::*;
use gpui_kit::component::input::InputState;
use gpui_kit::component::Root;
use midi_io::{EventSink, Playback, PortSink};
use std::collections::{BTreeSet, HashMap};
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
    /// canvas bounds as painted last frame â€” for hit-testing
    roll_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// seek-ruler strip bounds
    ruler_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// velocity lane bounds â€” same trick for the lane's hit-testing
    lane_bounds: Rc<Cell<Bounds<Pixels>>>,
    scroll_x: f32,
    scroll_y: f32,
    zoom: f32,
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
    /// (`loop_enabled` itself lives in `shared` so MCP can toggle it)
    loop_start_us: u64,
    /// what the bottom lane edits
    lane_mode: LaneMode,
    /// live MIDI input capture while `rec` is armed
    rec: Option<Rec>,
    focus: FocusHandle,
    input: Entity<InputState>,
    status: SharedString,
}

/// Armed recording: timestamps channel messages against the playhead's Âµs base.
struct Rec {
    _input: midi_io::Input,
    buf: std::sync::Arc<Mutex<Vec<(u64, Vec<u8>)>>>,
    /// document time (Âµs) corresponding to Input's t=0
    base_us: u64,
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
        let loaded = path.as_ref().map(load_document);
        // the warning(s) belong in the status line, not swallowed
        let (doc, status): (Document, SharedString) = match loaded {
            Some(Ok((d, w))) if !w.is_empty() => (
                d,
                format!("loaded — {} warning(s): {}", w.len(), w.join("; ")).into(),
            ),
            Some(Ok((d, _))) => (d, "loaded".into()),
            Some(Err(e)) => (empty_doc(), format!("load failed: {e}").into()),
            None => (empty_doc(), "new document".into()),
        };
        let mut sh = Shared::new(doc);
        sh.path = path.clone();
        sh.saved_revision = sh.doc.revision();
        // destination catalog: real MIDI ports by name, then discovered VST3s
        sh.dests = midi_io::list_outputs()
            .unwrap_or_default()
            .into_iter()
            .map(|p| {
                (
                    p.name.clone(),
                    midi_io::Destination::MidiPort { port_name: p.name },
                )
            })
            .collect();
        for p in output::discover_plugins() {
            sh.dests.push((
                format!("{} [VST3]", p.name),
                midi_io::Destination::Plugin {
                    plugin_path: p.path.to_string_lossy().into_owned(),
                },
            ));
        }
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
            active_plugins: Vec::new(),
            plugin_window: None,
            gui_plugin: None,
            enc_override: None,
            playback: None,
            play_us: 0,
            loop_start_us: 0,
            lane_mode: LaneMode::Velocity,
            rec: None,
            focus: cx.focus_handle(),
            input,
            status,
        };
        v.sel_track = v.pick_default_track();
        v.refresh_derived();
        if let Some(p) = &path {
            v.apply_prefs(p);
        }
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
                        0x58 => format!("TimeSig {}/{}", data.first().copied().unwrap_or(4), data.get(1).copied().unwrap_or(4)),
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

    /// Run a semantic region transform (`Document` *_ops generator) on the
    /// selection's range â€” or the whole selected track when nothing is
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
            let mut sh = self.shared.lock().unwrap();
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
            let mut sh = self.shared.lock().unwrap();
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
            let mut sh = self.shared.lock().unwrap();
            sh.doc.set_time_sig_ops(0, next.0, next.1)
        };
        self.apply_tx("set time signature", ops);
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
                drop(sh);
                self.persist();
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
            Ok((d, load_warnings)) => {
                self.stop_playback();
                // swap the document in place â€” the MCP server holds this same Arc
                {
                    let mut sh = self.shared.lock().unwrap();
                    sh.doc = d;
                    sh.undo = UndoStack::new(512);
                    sh.path = Some(path.clone());
                    sh.saved_revision = sh.doc.revision();
                }
                self.sel_track = self.pick_default_track();
                self.selection.clear();
                {
                    let mut sh = self.shared.lock().unwrap();
                    sh.muted.clear();
                    sh.soloed.clear();
                    sh.track_dest.clear();
                }
                self.apply_prefs(&path);
                self.refresh_derived();
                self.status = if load_warnings.is_empty() {
                    "loaded".into()
                } else {
                    format!("loaded — {} warning(s): {}",
                        load_warnings.len(),
                        load_warnings.join("; ")).into()
                };
            }
            Err(e) => self.status = e.to_string().into(),
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

    fn start_playback(&mut self) {
        // snapshot routing state so no lock is held while opening sinks
        let (dests, dest_of_track, muted, soloed, metronome, loop_enabled) = {
            let sh = self.shared.lock().unwrap();
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
        self.active_plugins.clear();
        for d in needed {
            let Some((_, dest)) = dests.get(d) else { continue };
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
                output::Destination::Plugin { plugin_path } => {
                    match output::PluginOutput::open(PathBuf::from(plugin_path).as_path()) {
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
        let mut events: Vec<(u64, usize, Vec<u8>)> = tagged
            .into_iter()
            .filter_map(|(us, tr, b)| sink_of.get(&dest_of(tr)).map(|&s| (us, s, b)))
            .collect();
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
        self.loop_start_us = self.play_us;
        self.playback = Some(Playback::start(
            sinks,
            events,
            self.play_us,
            loop_enabled.then_some(self.loop_start_us),
        ));
    }

    fn stop_playback(&mut self) {
        if let Some(mut p) = self.playback.take() {
            self.play_us = p.position_us();
            p.stop();
        }
        self.active_plugins.clear();
        self.finish_record();
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
        match midi_io::Input::open(0, move |us, b| {
            buf2.lock().unwrap().push((us, b.to_vec()));
        }) {
            Ok(input) => {
                self.rec = Some(Rec {
                    _input: input,
                    buf,
                    base_us: self.play_us,
                });
                if self.playback.is_none() {
                    self.start_playback();
                }
                self.status = format!("rec â†’ T{}", self.sel_track + 1).into();
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
        let msgs = std::mem::take(&mut *rec.buf.lock().unwrap());
        let mut sh = self.shared.lock().unwrap();
        if sh.doc.tracks.is_empty() {
            let ops = sh.doc.add_track_ops(None);
            let _ = sh.apply("add track", ops);
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
            let tick = sh.doc.tempo_map.us_to_tick(rec.base_us + us);
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
            self.status = "rec: no events".into();
            return;
        }
        self.apply_tx("record", vec![Op::InsertEvents { track, events }]);
        self.status = format!("rec: {n} events").into();
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

    /// Note whose right edge is within ~6px of `pos` â€” a resize target.
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

    /// Small chip with a literal label (symbols/numbers need no i18n key).
    /// `on` receives the ClickEvent so chips can honour Shift=Ã—10 etc.
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



fn load_document(path: &PathBuf) -> Result<(Document, Vec<String>), String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let file = smf_core::parse(&bytes).map_err(|e| e.to_string())?;
    let warnings = file.warnings.clone();
    Ok((Document::from_file(file), warnings))
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
    zoom: Option<f32>,
    scroll_x: Option<f32>,
    scroll_y: Option<f32>,
    sel_track: Option<usize>,
    enc: Option<String>,
    lane: Option<String>,
}

fn prefs_path(doc_path: &PathBuf) -> PathBuf {
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
    /// into `shared.dests`. Unavailable ports/plugins keep their identity â€”
    /// the assignment stays visible and plays again once the device is back.
    fn resolve_dest(&mut self, d: &output::Destination) -> usize {
        self.shared
            .lock()
            .unwrap()
            .ensure_dest(&dest_label(d), d.clone())
    }

    fn apply_prefs(&mut self, doc_path: &PathBuf) {
        let Ok(text) = std::fs::read_to_string(prefs_path(doc_path)) else {
            return;
        };
        let Ok(p) = serde_json::from_str::<Prefs>(&text) else {
            return;
        };
        if let Some(d) = &p.default_dest {
            let i = self.resolve_dest(d);
            self.shared.lock().unwrap().default_dest = i;
        }
        let overrides: Vec<(usize, usize)> = p
            .track_dest
            .iter()
            .map(|(t, d)| (*t, self.resolve_dest(d)))
            .collect();
        {
            let mut sh = self.shared.lock().unwrap();
            for (t, d) in overrides {
                sh.track_dest.insert(t, d);
            }
            sh.muted = p.muted.into_iter().collect();
            sh.soloed = p.soloed.into_iter().collect();
            sh.metronome = p.metronome;
            sh.loop_enabled = p.loop_enabled;
        }
        if let Some(z) = p.zoom {
            self.zoom = z;
        }
        if let Some(x) = p.scroll_x {
            self.scroll_x = x;
        }
        if let Some(y) = p.scroll_y {
            self.scroll_y = y;
        }
        if let Some(t) = p.sel_track {
            self.sel_track = t;
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
    }

    fn persist(&self) {
        let sh = self.shared.lock().unwrap();
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
        };
        if let Ok(text) = serde_json::to_string_pretty(&prefs) {
            let _ = std::fs::write(prefs_path(&path), text);
        }
    }
}




fn main() {
    std::panic::set_hook(Box::new(|i| eprintln!("panic: {i}")));
    let path = std::env::args().nth(1).map(PathBuf::from);
    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        // dark UI â€” gpui-component's default theme follows the OS and renders
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
            let (cur, reqs) = {
                let mut sh = shared.lock().unwrap();
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
                    if dirty {
                        v.refresh_derived();
                        // cover routing changes that came from MCP tools
                        v.persist();
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
