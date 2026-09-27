//! Phase 1: real SMF document in a modern-editor shell.
//! Open/save .mid, piano roll + event list views, playback to a MIDI port,
//! basic editing (draw / drag / delete) all going through
//! `Document::apply(Transaction)` so undo is shared with MCP edits.

mod i18n;
use i18n::t;

use commands::UndoStack;
use document::{Document, Event as DocEvent, EventId, Note, Op, Transaction};
use smf_core::EventKind;
use gpui_kit::*;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::Root;
use midi_io::{Playback, PortInfo};
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

struct Drag {
    on_id: EventId,
    off_id: Option<EventId>,
    track: usize,
    orig_start: u64,
    orig_end: Option<u64>,
    orig_key: u8,
    /// current preview delta (ticks, semitones)
    dtick: i64,
    dkey: i32,
}

struct EditorView {
    doc: Document,
    undo: UndoStack,
    path: Option<PathBuf>,
    clean_rev: u64,
    notes_rev: u64,
    notes: Arc<Vec<Note>>,
    ev_rev: u64,
    events: Arc<Vec<SharedString>>,
    sel_track: usize,
    selected: Option<EventId>,
    drag: Option<Drag>,
    /// canvas bounds as painted last frame — for hit-testing
    roll_bounds: Rc<Cell<Bounds<Pixels>>>,
    scroll_x: f32,
    scroll_y: f32,
    zoom: f32,
    ports: Vec<PortInfo>,
    port_idx: usize,
    playback: Option<Playback>,
    play_us: u64,
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
    fn new(path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (doc, status, open_path) = match &path {
            Some(p) => match load_document(p) {
                Ok(d) => (d, "loaded".into(), Some(p.clone())),
                Err(e) => (empty_doc(), format!("load failed: {e}").into(), Some(p.clone())),
            },
            None => (empty_doc(), "new document".into(), None),
        };
        let mut v = Self {
            doc,
            undo: UndoStack::new(512),
            path: open_path,
            clean_rev: 0,
            notes_rev: u64::MAX,
            notes: Arc::new(vec![]),
            ev_rev: u64::MAX,
            events: Arc::new(vec![]),
            sel_track: 0,
            selected: None,
            drag: None,
            roll_bounds: Rc::new(Cell::new(Bounds::new(point(px(0.0), px(0.0)), size(px(0.0), px(0.0))))),
            scroll_x: 0.0,
            scroll_y: (127.0 - 84.0) * NOTE_H, // show ~C3..C7
            zoom: 0.08,
            ports: midi_io::list_outputs().unwrap_or_default(),
            port_idx: 0,
            playback: None,
            play_us: 0,
            focus: cx.focus_handle(),
            input: cx.new(|cx| InputState::new(window, cx).placeholder(t("field.track_name"))),
            status,
        };
        v.sel_track = v.pick_default_track();
        v.refresh_derived();
        v
    }

    fn pick_default_track(&self) -> usize {
        self.doc
            .tracks
            .iter()
            .position(|t| {
                t.events
                    .iter()
                    .any(|e| matches!(e.kind, EventKind::Channel { .. }))
            })
            .unwrap_or(0)
    }

    fn refresh_derived(&mut self) {
        let rev = self.doc.revision();
        if self.notes_rev != rev {
            self.notes = Arc::new(self.doc.notes());
            self.notes_rev = rev;
        }
        if self.ev_rev != rev {
            self.events = Arc::new(self.build_event_rows());
            self.ev_rev = rev;
        }
    }

    fn ppq(&self) -> u64 {
        match self.doc.division {
            Division::Metrical(p) => (p as u64).max(1),
            Division::Smpte { .. } => 480,
        }
    }

    fn build_event_rows(&self) -> Vec<SharedString> {
        let ppq = self.ppq();
        let mut rows = Vec::new();
        for (ti, tr) in self.doc.tracks.iter().enumerate() {
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
                        0x03 => format!("TrkName {}", lossy(data)),
                        0x51 if data.len() == 3 => {
                            let mpq = u32::from_be_bytes([0, data[0], data[1], data[2]]);
                            format!("Tempo   {:.2} bpm", 60_000_000.0 / mpq as f64)
                        }
                        0x2F => "EndOfTrack".to_string(),
                        0x58 => format!("TimeSig {}/{}", data.get(0).copied().unwrap_or(4), data.get(1).copied().unwrap_or(4)),
                        0x59 => "KeySig".to_string(),
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
        let tx = Transaction {
            label: label.into(),
            base: self.doc.revision(),
            ops,
        };
        match self.doc.apply(tx.clone()) {
            Ok(_) => {
                self.undo.push(tx);
                self.refresh_derived();
            }
            Err(e) => self.status = format!("apply: {e}").into(),
        }
    }

    fn insert_note(&mut self, tick: u64, key: u8, cx: &mut Context<Self>) {
        let ppq = self.ppq();
        let track = self.sel_track.min(self.doc.tracks.len().saturating_sub(1));
        let snap = ppq / 4;
        let tick = (tick / snap) * snap;
        let on_id = self.doc.alloc_event_id();
        let off_id = self.doc.alloc_event_id();
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
        self.selected = Some(on_id);
        cx.notify();
    }

    fn delete_selected(&mut self, cx: &mut Context<Self>) {
        let Some(on_id) = self.selected else { return };
        let mut removed: Vec<(usize, document::Event)> = Vec::new();
        let mut track = 0usize;
        'outer: for (ti, t) in self.doc.tracks.iter().enumerate() {
            for (ei, e) in t.events.iter().enumerate() {
                if e.id == on_id {
                    track = ti;
                    removed.push((ei, e.clone()));
                    break 'outer;
                }
            }
        }
        if removed.is_empty() {
            return;
        }
        // also remove its paired noteOff if any
        if let Some(note) = self.notes.iter().find(|n| n.on_id == on_id) {
            if let Some(off_id) = note.off_id {
                if let Some(pos) = self.doc.tracks[track]
                    .events
                    .iter()
                    .position(|e| e.id == off_id)
                {
                    removed.push((pos, self.doc.tracks[track].events[pos].clone()));
                }
            }
        }
        self.apply_tx("delete note", vec![Op::RemoveEvents { track, removed }]);
        self.selected = None;
        cx.notify();
    }

    fn commit_drag(&mut self, cx: &mut Context<Self>) {
        let Some(d) = self.drag.take() else { return };
        if d.dtick == 0 && d.dkey == 0 {
            return;
        }
        let mut ops = Vec::new();
        for (ti, t) in self.doc.tracks.iter().enumerate() {
            if ti != d.track {
                continue;
            }
            for e in &t.events {
                let is_on = e.id == d.on_id;
                let is_off = d.off_id == Some(e.id);
                if !is_on && !is_off {
                    continue;
                }
                let mut after = e.clone();
                after.tick = ((if is_on { d.orig_start } else { d.orig_end.unwrap_or(d.orig_start) }) as i64
                    + d.dtick)
                    .max(0) as u64;
                if let EventKind::Channel { data, .. } = &mut after.kind {
                    data[0] = (d.orig_key as i32 + d.dkey).clamp(0, 127) as u8;
                }
                after.raw_body = None; // re-encode from kind
                ops.push(Op::UpdateEvent {
                    track: ti,
                    before: e.clone(),
                    after,
                });
            }
        }
        if !ops.is_empty() {
            self.apply_tx("move note", ops);
        }
        cx.notify();
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        if let Some(l) = self.undo.undo(&mut self.doc) {
            self.status = format!("undo {l}").into();
            self.selected = None;
            self.refresh_derived();
            cx.notify();
        }
    }

    fn redo(&mut self, cx: &mut Context<Self>) {
        if let Some(l) = self.undo.redo(&mut self.doc) {
            self.status = format!("redo {l}").into();
            self.selected = None;
            self.refresh_derived();
            cx.notify();
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        match &self.path {
            Some(p) => {
                let bytes = self.doc.serialize(smf_core::WriteOptions {
                    running_status: false,
                });
                match std::fs::write(p, bytes) {
                    Ok(_) => {
                        self.clean_rev = self.doc.revision();
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
                        v.path = Some(path);
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
                self.doc = d;
                self.undo = UndoStack::new(512);
                self.path = Some(path);
                self.clean_rev = self.doc.revision();
                self.sel_track = self.pick_default_track();
                self.selected = None;
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

    fn start_playback(&mut self) {
        let Some(port) = self.ports.get(self.port_idx) else {
            self.status = t("status.no_port").into();
            return;
        };
        match midi_io::Output::open(port.index) {
            Ok(out) => {
                let events = self.doc.timeline();
                self.playback = Some(Playback::start(out, events, self.play_us));
            }
            Err(e) => self.status = format!("{e}").into(),
        }
    }

    fn stop_playback(&mut self) {
        if let Some(mut p) = self.playback.take() {
            self.play_us = p.position_us();
            p.stop();
        }
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

fn lossy(b: &bytes::Bytes) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn load_document(path: &PathBuf) -> Result<Document, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let file = smf_core::parse(&bytes).map_err(|e| e.to_string())?;
    Ok(Document::from_file(file))
}

impl Render for EditorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh_derived();

        // advance playhead / auto-stop
        if let Some(p) = &self.playback {
            self.play_us = p.position_us();
            if !p.is_running() {
                self.playback = None;
                self.play_us = 0;
            }
        }
        let playhead_tick = self.doc.tempo_map.us_to_tick(self.play_us);

        let title = self
            .path
            .as_ref()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| t("status.no_file").to_string());
        let dirty = self.doc.revision() != self.clean_rev;
        let ppq = self.ppq();
        let pos = {
            let bar = playhead_tick / (ppq * 4) + 1;
            let beat = (playhead_tick % (ppq * 4)) / ppq + 1;
            format!("{bar}.{beat}.{:>3}", playhead_tick % ppq)
        };
        let port_label = self
            .ports
            .get(self.port_idx)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| t("status.no_port").to_string());

        // --- piano roll canvas -------------------------------------------------
        let notes = self.notes.clone();
        let (scroll_x, scroll_y, zoom) = (self.scroll_x, self.scroll_y, self.zoom);
        let selected = self.selected;
        let drag = self.drag.as_ref().map(|d| (d.on_id, d.dtick, d.dkey));
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
                    if let Some((d_on, dtick, dkey)) = drag {
                        if d_on == n.on_id {
                            st += dtick;
                            en += dtick;
                            key += dkey;
                        }
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
                    let c = if Some(n.on_id) == selected {
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
            },
        );

        // --- header -------------------------------------------------------------
        let play_label: &'static str = if self.playback.is_some() {
            t("menu.stop")
        } else {
            t("menu.play")
        };
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
                        if !v.ports.is_empty() {
                            v.port_idx = (v.port_idx + 1) % v.ports.len();
                        }
                        cx.notify();
                    })),
            )
            .child(div().flex_1())
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
                        .child(format!("{} ({})", t("events.header"), self.events.len())),
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

        let body = body.child(
            div()
                .flex_1()
                .h_full()
                .relative()
                .overflow_hidden()
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
                    cx.listener(|this, ev: &MouseDownEvent, _w, cx| {
                        if let Some(n) = this.note_at(ev.position) {
                            this.selected = Some(n.on_id);
                            this.drag = Some(Drag {
                                on_id: n.on_id,
                                off_id: n.off_id,
                                track: n.track,
                                orig_start: n.start_tick,
                                orig_end: n.end_tick,
                                orig_key: n.key,
                                dtick: 0,
                                dkey: 0,
                            });
                        } else {
                            let (tick, key) = this.hit(ev.position);
                            if (0..=127).contains(&key) {
                                this.selected = None;
                                this.insert_note(tick as u64, key as u8, cx);
                            }
                        }
                        cx.notify();
                    }),
                )
                .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, cx| {
                    if this.drag.is_some() && ev.pressed_button == Some(MouseButton::Left) {
                        let (tick, key) = this.hit(ev.position);
                        if let Some(d) = &mut this.drag {
                            d.dtick = tick - d.orig_start as i64;
                            d.dkey = key - d.orig_key as i32;
                        }
                        cx.notify();
                    }
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _ev: &MouseUpEvent, _w, cx| this.commit_drag(cx)),
                )
                .child(roll.size_full()),
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
    let path = std::env::args().nth(1).map(PathBuf::from);
    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        let path = path.clone();
        cx.spawn(async move |cx| {
            cx.open_window(WindowOptions::default(), move |window, cx| {
                let view = cx.new(|cx| {
                    let v = EditorView::new(path.clone(), window, cx);
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
