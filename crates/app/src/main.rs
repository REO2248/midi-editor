//! Phase 0 spike: gpui-kit window + canvas piano roll (12k notes) +
//! uniform_list event list + Japanese-capable input.

use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::Root;
use gpui_kit::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

const NOTE_COUNT: usize = 12_000;
const EVENT_COUNT: usize = 8_000;
const PPQ: u32 = 480;

struct Note {
    tick: u64,
    key: u8,
    dur: u64,
    track: u8,
}

fn demo_notes() -> Arc<Vec<Note>> {
    let mut v = Vec::with_capacity(NOTE_COUNT);
    let mut tick = 0u64;
    let mut key = 24u8;
    for i in 0..NOTE_COUNT {
        tick += 30 + (i as u64 * 37) % 360;
        key = 21 + ((key - 21 + 5) % 88) as u8; // pentatonic-ish walk
        v.push(Note {
            tick,
            key,
            dur: 120 + (i as u64 % 7) * 60,
            track: (i % 4) as u8,
        });
    }
    Arc::new(v)
}

fn demo_events() -> Arc<Vec<SharedString>> {
    let kinds = ["NoteOn", "NoteOff", "CC", "PC", "PB", "Meta", "SysEx"];
    Arc::new(
        (0..EVENT_COUNT)
            .map(|i| {
                let bar = i as u32 / (PPQ * 4) + 1;
                let beat = (i as u32 % (PPQ * 4)) / PPQ + 1;
                SharedString::from(format!(
                    "{:>4}:{:>2}:{:>3}  {:<7} ch{:<2} {:>3} {:>3}",
                    bar,
                    beat,
                    i % 480,
                    kinds[i % kinds.len()],
                    i % 16 + 1,
                    i % 128,
                    (i * 7) % 128
                ))
            })
            .collect(),
    )
}

struct SpikeView {
    notes: Arc<Vec<Note>>,
    events: Arc<Vec<SharedString>>,
    input: Entity<InputState>,
    /// piano-roll scroll in pixels-of-tick-space
    scroll_x: f32,
    scroll_y: f32,
    zoom: f32, // px per tick
    frames: Arc<AtomicU64>,
    start: Instant,
}

impl SpikeView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            notes: demo_notes(),
            events: demo_events(),
            input: cx.new(|cx| InputState::new(window, cx).placeholder("トラック名 / 日本語入力テスト")),
            scroll_x: 0.0,
            scroll_y: 420.0, // center the visible range on the demo notes' keys
            zoom: 0.05,
            frames: Arc::new(AtomicU64::new(0)),
            start: Instant::now(),
        }
    }
}

const NOTE_H: f32 = 12.0;
const TRACK_COLORS: [u32; 4] = [0x4f8cff, 0xff8c4f, 0x4fd08c, 0xd04fff];

impl Render for SpikeView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let notes = self.notes.clone();
        let scroll_x = self.scroll_x;
        let scroll_y = self.scroll_y;
        let zoom = self.zoom;
        let frames = self.frames.clone();
        let fps = {
            let f = self.frames.load(Ordering::Relaxed);
            let secs = self.start.elapsed().as_secs_f32().max(0.001);
            (f as f32 / secs) as u32
        };

        let roll = canvas(
            move |_bounds, _window, _cx| (),  // canvas defaults to 0x0 — size_full below
            move |bounds, _state, window, _cx| {
                frames.fetch_add(1, Ordering::Relaxed);
                window.request_animation_frame();

                let w = bounds.size.width;
                let h = bounds.size.height;

                // key row separators
                let key0 = (scroll_y / NOTE_H).max(0.0) as i32;
                let key1 = ((scroll_y + h.to_f64() as f32) / NOTE_H + 1.0).min(128.0) as i32;
                for k in key0..key1 {
                    let y = bounds.origin.y + px(k as f32 * NOTE_H - scroll_y);
                    let c = if k % 12 == 0 { 0x2a2a33 } else { 0x232329 };
                    window.paint_quad(fill(
                        Bounds::new(point(bounds.origin.x, y), size(w, px(1.0))),
                        rgb(c),
                    ));
                }
                // beat lines
                let tick0 = (scroll_x / zoom).max(0.0) as u64;
                let tick1 = tick0 + (w.to_f64() as f32 / zoom) as u64 + PPQ as u64;
                let mut t = tick0 / PPQ as u64 * PPQ as u64;
                while t <= tick1 {
                    let x = bounds.origin.x + px(t as f32 * zoom - scroll_x);
                    let strong = t % (PPQ as u64 * 4) == 0;
                    window.paint_quad(fill(
                        Bounds::new(point(x, bounds.origin.y), size(px(1.0), h)),
                        rgb(if strong { 0x3a3a4a } else { 0x2a2a33 }),
                    ));
                    t += PPQ as u64;
                }
                // notes (visible range cull by linear scan — spike)
                for n in notes.iter() {
                    let x = bounds.origin.x + px(n.tick as f32 * zoom - scroll_x);
                    if x + px(n.dur as f32 * zoom) < bounds.origin.x {
                        continue;
                    }
                    if x > bounds.origin.x + w {
                        break;
                    }
                    let y = bounds.origin.y + px((127 - n.key) as f32 * NOTE_H - scroll_y);
                    if y < bounds.origin.y - px(NOTE_H) || y > bounds.origin.y + h {
                        continue;
                    }
                    window.paint_quad(fill(
                        Bounds::new(
                            point(x, y),
                            size(px((n.dur as f32 * zoom).max(2.0)), px(NOTE_H - 1.0)),
                        ),
                        rgb(TRACK_COLORS[n.track as usize]),
                    ));
                }
            },
        );

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x1b1b22))
            .text_color(rgb(0xd8d8e0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .px_4()
                    .h(px(44.0))
                    .bg(rgb(0x14141a))
                    .child(div().child("midi-editor spike"))
                    .child(div().child(format!("{fps} fps")).text_color(rgb(0x888899)))
                    .child(div().w(px(320.0)).child(Input::new(&self.input))),
            )
            .child(
                div().flex().flex_1().min_h(px(0.0)).child(
                    // left: event list
                    div()
                        .w(px(360.0))
                        .h_full()
                        .bg(rgb(0x17171d))
                        .child({
                            let events = self.events.clone();
                            uniform_list("events", events.len(), move |range, _w, _cx| {
                                range
                                    .map(|i| {
                                        div()
                                            .h(px(20.0))
                                            .px_2()
                                            .text_size(px(12.0))
                                            .font_family("Cascadia Mono")
                                            .child(events[i].clone())
                                    })
                                    .collect()
                            })
                            .h_full()
                        }),
                )
                .child(
                    // center: piano roll
                    div()
                        .flex_1()
                        .h_full()
                        .relative()
                        .overflow_hidden()
                        .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _w, cx| {
                            let d = ev.delta.pixel_delta(px(20.0));
                            if ev.modifiers.control {
                                this.zoom = (this.zoom * (1.0 - d.y.to_f64() as f32 * 0.002))
                                    .clamp(0.005, 0.5);
                            } else {
                                this.scroll_x =
                                    (this.scroll_x + d.x.to_f64() as f32).max(0.0);
                                this.scroll_y =
                                    (this.scroll_y + d.y.to_f64() as f32).max(0.0);
                            }
                            cx.notify();
                        }))
                        .child(roll.size_full()),
                ),
            )
    }
}

fn main() {
    gpui_kit::application().run(|cx| {
        gpui_kit::init(cx);
        cx.spawn(async move |cx| {
            cx.open_window(WindowOptions::default(), |window, cx| {
                let view = cx.new(|cx| SpikeView::new(window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("failed to open window");
        })
        .detach();
    });
}
