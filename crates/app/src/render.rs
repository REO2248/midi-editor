//! Rendering: impl Render for EditorView (toolbar, track column, ruler,
//! piano roll canvas, lane, event list) plus the chip/button helpers.
//! Private items are visible here because this is a child module of the
//! crate root where EditorView is defined.

use crate::geometry::{drag_window, tick_window, ZOOM_MAX, ZOOM_MIN};
use crate::i18n::{t, tf};
use crate::icons::icon;
use crate::*;
use gpui_kit::component::input::Input;
use gpui_kit::*;
use smf_core::EventKind;
use std::any::Any;
use std::collections::BTreeSet;
use std::path::PathBuf;

const BG_BAR: u32 = 0x0f0f15;
const BG_PANEL: u32 = 0x17171d;
const BG_RAISED: u32 = 0x20202c;
const BORDER_C: u32 = 0x2a2a35;
/// Linear blend of two 0xRRGGBB colors — ghost-track dimming.
fn blend(c: u32, to: u32, f: f32) -> u32 {
    let r = (((c >> 16) & 0xFF) as f32 * (1.0 - f) + ((to >> 16) & 0xFF) as f32 * f) as u32;
    let g = (((c >> 8) & 0xFF) as f32 * (1.0 - f) + ((to >> 8) & 0xFF) as f32 * f) as u32;
    let b = ((c & 0xFF) as f32 * (1.0 - f) + (to & 0xFF) as f32 * f) as u32;
    (r << 16) | (g << 8) | b
}

const ACCENT: u32 = 0x9fd0ff;

impl Render for EditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // the plugin editor lives in the helper subprocess's own window —
        // no native event queue to pump here
        self.refresh_derived();
        // keep both scroll axes inside the content (resizes, zooms, edits all
        // self-heal here) and edge-scroll while a drag is parked at a border;
        // `panning` keeps animation frames flowing only while it actually moves
        self.clamp_scroll();
        let panning = self.drag_auto_pan();

        // advance playhead / auto-stop (looping happens inside the
        // playback thread; reaching this branch means playback ended)
        if let Some(p) = &self.playback {
            self.play_us = p.position_us();
            if !p.is_running() {
                self.playback = None;
                self.play_us = 0;
            }
        }
        let (
            playhead_tick,
            title,
            dirty,
            dests,
            eff_dest,
            def_dest,
            loop_en,
            met_en,
            chsy_en,
            sxp,
            muted_set,
            soloed_set,
            has_track_dest,
        ) = {
            let sh = crate::lock_shared(&self.shared);
            (
                sh.doc.tempo_map.us_to_tick(self.play_us),
                sh.path
                    .as_ref()
                    .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .unwrap_or_else(|| t("status.no_file").to_string()),
                sh.doc.revision() != sh.saved_revision,
                sh.dests.clone(),
                sh.dest_of(self.sel_track),
                sh.default_dest,
                sh.loop_enabled,
                sh.metronome,
                sh.chase_sysex,
                sh.sysex_policy,
                sh.muted.clone(),
                sh.soloed.clone(),
                sh.track_dest.contains_key(&self.sel_track),
            )
        };
        // revision-cached document chrome (markers, names, diagnostics, …) —
        // rebuilt on edits only, never per animation frame
        let doc_ui = self.doc_ui.clone();
        let n_diags = doc_ui.n_diags;
        let tempo0 = doc_ui.tempo0;
        let sig = doc_ui.sig.clone();
        let track_names = doc_ui.track_names.clone();
        let track_chs = doc_ui.track_chs.clone();
        let markers = &doc_ui.markers;
        let ppq = self.ppq();
        let pos = {
            let bar = playhead_tick / (ppq * 4) + 1;
            let beat = (playhead_tick % (ppq * 4)) / ppq + 1;
            format!("{bar}.{beat}.{:>3}", playhead_tick % ppq)
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
        let active_track = self.sel_track;

        let roll = canvas(
            move |bounds, _window, _cx| {
                bounds_cell.set(bounds);
            },
            move |bounds, _state, window, _cx| {
                if playing || panning {
                    // panning: edge auto-pan frames; playing: playhead
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
                    let bar = t.is_multiple_of(ppq * 4);
                    window.paint_quad(fill(
                        Bounds::new(point(x, bounds.origin.y), size(px(1.0), h)),
                        rgb(if bar { 0x3d3d52 } else { 0x2a2a35 }),
                    ));
                    t += ppq;
                }
                // notes — enter the sorted list by binary search. While a
                // Move/Duplicate drag shifts notes, widen the window toward
                // the shift direction: a plain right-edge `break` on the
                // dragged x would drop still-visible unselected notes.
                let (vt0, vt1) = tick_window(scroll_x, zoom, f32::from(w));
                let move_dtick = match drag {
                    Some((DragMode::Move | DragMode::Duplicate, _, dtick, _)) => dtick,
                    _ => 0,
                };
                let (entry, exit) = drag_window(vt0, vt1, move_dtick);
                let first = notes.partition_point(|n| (n.start_tick as i64) < entry);
                for n in &notes[first..] {
                    if (n.start_tick as i64) > exit {
                        break;
                    }
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
                        let ow = ((n.end_tick.unwrap_or(n.start_tick) - n.start_tick).max(1)
                            as f32
                            * zoom)
                            .max(3.0);
                        let oy = bounds.origin.y + px((127.0 - n.key as f32) * NOTE_H - scroll_y);
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
                    let y = bounds.origin.y + px((127.0 - key as f32) * NOTE_H - scroll_y);
                    if y < bounds.origin.y - px(NOTE_H) || y > bounds.origin.y + h {
                        continue;
                    }
                    let c = if selection.contains(&n.on_id) {
                        SEL_COLOR
                    } else if n.end_tick.is_none() {
                        DANGLING_COLOR
                    } else {
                        let c = TRACK_COLORS[n.track % TRACK_COLORS.len()];
                        if n.track == active_track {
                            c
                        } else {
                            blend(c, 0x12121a, 0.62)
                        }
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

        // --- palette -------------------------------------------------------------
        let sel_is_plugin = matches!(
            dests.get(eff_dest).map(|(_, d)| d),
            Some(output::Destination::Plugin { .. })
        );
        let sel_plugin_failed = matches!(
            self.plugin_state.get(&eff_dest),
            Some(PluginState::Failed { .. })
        );

        // --- menu bar -----------------------------------------------------------
        let open_menu = self.open_menu;
        let menus: [(TopMenu, &str, f32); 7] = [
            (TopMenu::File, "menu.file", 46.0),
            (TopMenu::Edit, "menu.edit", 46.0),
            (TopMenu::View, "menu.view", 52.0),
            (TopMenu::Track, "menu.track", 58.0),
            (TopMenu::Output, "menu.output", 62.0),
            (TopMenu::Transport, "menu.transport", 90.0),
            (TopMenu::Help, "menu.help", 50.0),
        ];
        let mut menu_bar = div()
            .flex()
            .items_center()
            .h(px(28.0))
            .pl_1()
            .pr_3()
            .bg(rgb(BG_BAR))
            .border_b_1()
            .border_color(rgb(BORDER_C))
            .text_size(px(12.0))
            .child(
                div()
                    .w(px(96.0))
                    .px_2()
                    .text_color(rgb(0x7a86a8))
                    .whitespace_nowrap()
                    .child(t("app.title")),
            );
        let mut mx = 96.0f32;
        for (m, key, w) in menus {
            let is_open = open_menu.map(|(mm, _)| mm) == Some(m);
            menu_bar = menu_bar.child(
                div()
                    .id(key)
                    .w(px(w))
                    .h(px(22.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .rounded_sm()
                    .bg(if is_open { rgb(BG_RAISED) } else { rgb(BG_BAR) })
                    .text_color(rgb(if is_open { 0xffffff } else { 0x9a9ab0 }))
                    .hover(|s| s.bg(rgb(0x1d1d28)))
                    .child(t(key))
                    .on_click(cx.listener(move |v, _e, _w, cx| {
                        v.open_menu = if is_open { None } else { Some((m, mx)) };
                        v.open_sub = None;
                        cx.notify();
                    }))
                    .on_mouse_move(cx.listener(move |v, _e, _w, cx| {
                        // while a menu is open, hovering a sibling label switches
                        if v.open_menu.is_some() && v.open_menu.map(|(mm, _)| mm) != Some(m) {
                            v.open_menu = Some((m, mx));
                            v.open_sub = None;
                            cx.notify();
                        }
                    })),
            );
            mx += w;
        }
        menu_bar = menu_bar.child(div().flex_1()).child(
            div()
                .text_size(px(12.0))
                .text_color(rgb(if dirty { 0xffd24f } else { 0x9a9ab0 }))
                .whitespace_nowrap()
                .child(format!("{title}{}", if dirty { " •" } else { "" })),
        );

        // --- transport / tool bar: icon groups, DAW style -------------------------
        let transport_bar = div()
            .flex()
            .items_center()
            .gap(px(2.0))
            .px_2()
            .h(px(40.0))
            .bg(rgb(BG_PANEL))
            .border_b_1()
            .border_color(rgb(BORDER_C))
            // file ops
            .child(Self::ibtn(
                "i.new",
                "note_add",
                t("tip.new"),
                false,
                cx,
                |v, _e, cx| {
                    v.new_file(cx);
                },
            ))
            .child(Self::ibtn(
                "i.open",
                "folder_open",
                t("tip.open"),
                false,
                cx,
                |v, _e, cx| {
                    v.open_dialog(cx);
                },
            ))
            .child(Self::ibtn(
                "i.save",
                "save",
                t("tip.save"),
                false,
                cx,
                |v, _e, cx| {
                    v.save(cx);
                },
            ))
            .child(Self::vsep())
            // history
            .child(Self::ibtn(
                "i.undo",
                "undo",
                t("tip.undo"),
                false,
                cx,
                |v, _e, cx| {
                    v.undo(cx);
                },
            ))
            .child(Self::ibtn(
                "i.redo",
                "redo",
                t("tip.redo"),
                false,
                cx,
                |v, _e, cx| {
                    v.redo(cx);
                },
            ))
            .child(Self::vsep())
            // transport
            .child(Self::ibtn_c(
                "i.play",
                if self.playback.is_some() {
                    "stop"
                } else {
                    "play_arrow"
                },
                t("tip.play"),
                self.playback.is_some(),
                0x4fd08c,
                cx,
                |v, _e, cx| v.toggle_play(cx),
            ))
            .child(Self::ibtn(
                "i.stop",
                "stop",
                t("tip.stop"),
                false,
                cx,
                |v, _e, cx| {
                    v.stop_playback();
                    v.play_us = 0;
                    cx.notify();
                },
            ))
            .child(Self::ibtn_c(
                "i.rec",
                "fiber_manual_record",
                t("tip.rec"),
                self.rec.is_some(),
                0xff6a5a,
                cx,
                |v, _e, _cx| v.toggle_record(),
            ))
            .child(Self::ibtn_c(
                "i.loop",
                "loop",
                t("tip.loop"),
                loop_en,
                0x9fd0ff,
                cx,
                |v, _e, _cx| {
                    {
                        let mut sh = crate::lock_shared(&v.shared);
                        sh.loop_enabled = !sh.loop_enabled;
                    }
                    v.persist();
                },
            ))
            .child(Self::ibtn_c(
                "i.met",
                "timer",
                t("tip.met"),
                met_en,
                0x9fd0ff,
                cx,
                |v, _e, _cx| {
                    {
                        let mut sh = crate::lock_shared(&v.shared);
                        sh.metronome = !sh.metronome;
                    }
                    v.persist();
                },
            ))
            .child(Self::vsep())
            // position + tempo + meter readouts (LCD style)
            .child(
                div()
                    .px_2()
                    .h(px(24.0))
                    .flex()
                    .items_center()
                    .bg(rgb(0x0b0b11))
                    .border_1()
                    .border_color(rgb(BORDER_C))
                    .rounded_sm()
                    .text_color(rgb(0x8fd0a0))
                    .text_size(px(12.0))
                    .font_family("Cascadia Mono")
                    .whitespace_nowrap()
                    .child(pos.clone()),
            )
            .child(
                div()
                    .id("bpm")
                    .px_2()
                    .h(px(24.0))
                    .flex()
                    .items_center()
                    .bg(rgb(0x0b0b11))
                    .border_1()
                    .border_color(rgb(BORDER_C))
                    .rounded_sm()
                    .cursor_pointer()
                    .text_color(rgb(0x8fd0a0))
                    .text_size(px(12.0))
                    .font_family("Cascadia Mono")
                    .whitespace_nowrap()
                    .tooltip(move |_w, cx| cx.new(|_| Tip(t("tip.tempo").into())).into())
                    .child(format!("{tempo0:.0}♩"))
                    .on_click(cx.listener(|v, e: &ClickEvent, _w, cx| {
                        v.bump_tempo(if e.modifiers().shift { -10.0 } else { 1.0 });
                        cx.notify();
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|v, _e, _w, cx| {
                            v.bump_tempo(-1.0);
                            cx.notify();
                        }),
                    ),
            )
            .child(Self::chip("sig", sig.clone(), cx, |v, _e, cx| {
                v.cycle_time_sig();
                cx.notify();
            }))
            .child(Self::vsep())
            // edit tools
            .child(Self::ibtn_c(
                "i.sel",
                "arrow_selector_tool",
                t("tip.sel"),
                self.tool == Tool::Select,
                ACCENT,
                cx,
                |v, _e, cx| v.set_tool(Tool::Select, cx),
            ))
            .child(Self::ibtn_c(
                "i.draw",
                "edit",
                t("tip.draw"),
                self.tool == Tool::Draw,
                ACCENT,
                cx,
                |v, _e, cx| v.set_tool(Tool::Draw, cx),
            ))
            .child(Self::ibtn_c(
                "i.erase",
                "ink_eraser",
                t("tip.erase"),
                self.tool == Tool::Erase,
                ACCENT,
                cx,
                |v, _e, cx| v.set_tool(Tool::Erase, cx),
            ))
            .child(Self::vsep())
            // snap grid cycle: off / 1 / 1/2 / 1/4 / 1/8 / 1/16 / 1/32
            .child(
                div()
                    .id("snap")
                    .h(px(26.0))
                    .pl_1()
                    .pr_2()
                    .flex()
                    .items_center()
                    .rounded_sm()
                    .bg(rgb(if SNAPS[self.snap_idx].0 > 0 {
                        BG_RAISED
                    } else {
                        0x141419
                    }))
                    .border_1()
                    .border_color(rgb(if SNAPS[self.snap_idx].0 > 0 {
                        0x3d5a75
                    } else {
                        BORDER_C
                    }))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(0x2f2f42)))
                    .tooltip(move |_w, cx| cx.new(|_| Tip(t("tip.snap").into())).into())
                    .child(icon(
                        "grid_on",
                        15.0,
                        if SNAPS[self.snap_idx].0 > 0 {
                            ACCENT
                        } else {
                            0x55556a
                        },
                    ))
                    .child(
                        div()
                            .pl_1()
                            .text_size(px(11.0))
                            .font_family("Cascadia Mono")
                            .text_color(rgb(if SNAPS[self.snap_idx].0 > 0 {
                                0xd8d8e0
                            } else {
                                0x55556a
                            }))
                            .whitespace_nowrap()
                            .child(SNAPS[self.snap_idx].2),
                    )
                    .on_click(cx.listener(|v, _e, _w, cx| v.cycle_snap(cx))),
            )
            .child(Self::vsep())
            // selection ops (selection range, else whole track)
            .child(Self::ibtn(
                "i.quant",
                "compress",
                t("tip.quantize"),
                false,
                cx,
                |v, _e, cx| {
                    let g = v.snap_ticks().max(v.ppq() as i64 / 4) as u64;
                    v.apply_region_op("quantize", move |d, tr, f, to| {
                        d.quantize_ops(tr, f, to, g, 100)
                    });
                    cx.notify();
                },
            ))
            .child(Self::ibtn(
                "i.trdn",
                "arrow_downward",
                t("tip.trdn"),
                false,
                cx,
                |v, _e, cx| {
                    v.apply_region_op("transpose -1", |d, t, f, to| d.transpose_ops(t, f, to, -1));
                    cx.notify();
                },
            ))
            .child(Self::ibtn(
                "i.trup",
                "arrow_upward",
                t("tip.trup"),
                false,
                cx,
                |v, _e, cx| {
                    v.apply_region_op("transpose +1", |d, t, f, to| d.transpose_ops(t, f, to, 1));
                    cx.notify();
                },
            ))
            .child(Self::ibtn(
                "i.vel",
                "tune",
                t("tip.vel"),
                false,
                cx,
                |v, e: &ClickEvent, cx| {
                    if e.modifiers().shift {
                        v.apply_region_op("vel ×0.8", |d, t, f, to| {
                            d.scale_velocity_ops(t, f, to, 0.8)
                        });
                    } else {
                        v.apply_region_op("vel ×1.25", |d, t, f, to| {
                            d.scale_velocity_ops(t, f, to, 1.25)
                        });
                    }
                    cx.notify();
                },
            ))
            .child(div().flex_1())
            // zoom
            .child(Self::ibtn(
                "i.zout",
                "zoom_out",
                t("tip.zout"),
                false,
                cx,
                |v, _e, cx| {
                    v.zoom_by(1.0 / 1.3, cx);
                },
            ))
            .child(Self::ibtn(
                "i.zin",
                "zoom_in",
                t("tip.zin"),
                false,
                cx,
                |v, _e, cx| {
                    v.zoom_by(1.3, cx);
                },
            ))
            .children(sel_is_plugin.then(|| {
                Self::ibtn("i.gui", "piano", t("tip.gui"), false, cx, |v, _e, cx| {
                    v.open_plugin_gui();
                    cx.notify();
                })
            }));
        // --- event list: right-docked panel -------------------------------------
        let events_panel = div()
            .w(px(340.0))
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(BG_PANEL))
            .border_l_1()
            .border_color(rgb(BORDER_C))
            .child(
                div()
                    .flex()
                    .items_center()
                    .px_2()
                    .h(px(26.0))
                    .border_b_1()
                    .border_color(rgb(BORDER_C))
                    .text_size(px(11.0))
                    .text_color(rgb(0x77778a))
                    .child(format!("{} ({})", t("events.header"), self.events.len()))
                    .child(div().flex_1())
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
                                    let mut sh = crate::lock_shared(&v.shared);
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
            });

        let body = div().flex().flex_1().min_h(px(0.0));

        // --- track column: select / mute / solo -------------------------------
        let track_col = div()
            .w(px(150.0))
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(0x1b1b24))
            .border_r_1()
            .border_color(rgb(BORDER_C))
            .child(
                div()
                    .px_2()
                    .h(px(26.0))
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(rgb(BORDER_C))
                    .text_size(px(11.0))
                    .text_color(rgb(0x77778a))
                    .child(t("tracks.header")),
            )
            .child(
                // scrollable when a file has more tracks than fit the panel
                div()
                    .id("track-list")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .children(track_names.iter().enumerate().map(|(i, name)| {
                        let sel = self.sel_track == i;
                        let muted = muted_set.contains(&i);
                        let soloed = soloed_set.contains(&i);
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
                            .child(div().w(px(10.0)).h(px(10.0)).rounded_sm().bg(rgb(if muted {
                                0x555560
                            } else {
                                color
                            })))
                            .child(
                                div()
                                    .flex_1()
                                    .px_1()
                                    .text_size(px(11.0))
                                    .text_color(rgb(if muted { 0x707080 } else { 0xd8d8e0 }))
                                    .overflow_hidden()
                                    .child(name.to_string()),
                            )
                            .child(
                                div()
                                    .id(("mute", i))
                                    .px_1()
                                    .text_size(px(9.0))
                                    .text_color(rgb(if muted { 0xffb454 } else { 0x707080 }))
                                    .on_click(cx.listener(move |v, _e: &ClickEvent, _w, cx| {
                                        cx.stop_propagation();
                                        {
                                            let mut sh = crate::lock_shared(&v.shared);
                                            if !sh.muted.remove(&i) {
                                                sh.muted.insert(i);
                                            }
                                        }
                                        v.persist();
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
                                        {
                                            let mut sh = crate::lock_shared(&v.shared);
                                            if !sh.soloed.remove(&i) {
                                                sh.soloed.insert(i);
                                            }
                                        }
                                        v.persist();
                                        cx.notify();
                                    }))
                                    .child("S"),
                            )
                            .child(
                                div()
                                    .id(("ch", i))
                                    .px_1()
                                    .text_size(px(9.0))
                                    .text_color(rgb(0x7070a0))
                                    .on_click(cx.listener(move |v, _e: &ClickEvent, _w, cx| {
                                        cx.stop_propagation();
                                        let ops = {
                                            let mut sh = crate::lock_shared(&v.shared);
                                            let cur = sh
                                                .doc
                                                .tracks
                                                .get(i)
                                                .map(|t| t.out_channel)
                                                .unwrap_or(0);
                                            sh.doc.set_track_channel_ops(i, (cur + 1) % 16)
                                        };
                                        v.apply_tx("set track channel", ops);
                                        cx.notify();
                                    }))
                                    .child(format!(
                                        "c{}",
                                        track_chs.get(i).copied().unwrap_or(0) + 1
                                    )),
                            )
                    })),
            )
            // rename field for the selected track, anchored at the panel bottom
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_1()
                    .h(px(30.0))
                    .border_t_1()
                    .border_color(rgb(BORDER_C))
                    .child(div().flex_1().min_w(px(0.0)).child(Input::new(&self.input)))
                    .child(Self::chip("rename", "rename", cx, |v, _e, cx| {
                        let name = v.input.read(cx).value().to_string();
                        let ops = {
                            let mut sh = crate::lock_shared(&v.shared);
                            sh.doc.set_track_name_ops(v.sel_track, &name)
                        };
                        v.apply_tx("set track name", ops);
                        cx.notify();
                    })),
            );

        // velocity / CC / pitch-bend lane (selected track only)
        let lane_sel_track = self.sel_track;
        let lane_mode = self.lane_mode;
        // control events of the selected track matching the lane mode:
        // (event id, tick, value 0..127 or 0..16383 for PB) — cached on
        // (revision, track, mode) so playhead animation is allocation-free
        let lane_events = self.lane_events_cached();
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
                    let vrange = if lane_mode == LaneMode::PitchBend {
                        16383.0
                    } else {
                        127.0
                    };
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
                            for (id, tick, val) in lane_events.iter() {
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
                                        Bounds::new(point(px_, py_), size(x - px_, px(1.0))),
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
                                    Bounds::new(
                                        point(x - px(2.0), y - px(2.0)),
                                        size(px(4.0), px(4.0)),
                                    ),
                                    rgb(0x4fd0ff),
                                ));
                                prev = Some((x, y));
                            }
                            // drag insert ghost
                            if let Some((DragMode::LaneEvent, 0, a_tick, dkey)) = drag_v {
                                let x = bounds.origin.x + px(a_tick as f32 * zoom - scroll_x);
                                let y = bounds.origin.y
                                    + px((h - 4.0)
                                        * (1.0 - dkey.clamp(0, vrange as i32) as f32 / vrange)
                                        + 2.0);
                                window.paint_quad(fill(
                                    Bounds::new(
                                        point(x - px(2.0), y - px(2.0)),
                                        size(px(4.0), px(4.0)),
                                    ),
                                    rgb(SEL_COLOR),
                                ));
                            }
                        }
                    }
                }
            },
        );

        // seek ruler: bar ticks/numbers, click positions the playhead
        // minimap: whole-song overview with viewport rectangle
        let mini_bounds_cell = self.mini_bounds.clone();
        let mini_notes = self.notes.clone();
        let mini_active = self.sel_track;
        let roll_bounds_cell = self.roll_bounds.clone();
        let song_end = doc_ui.song_end.max(ppq * 16);
        let mini_play = play_x_tick;
        let minimap = canvas(
            move |b, _w, _cx| mini_bounds_cell.set(b),
            move |bounds, _state, window, _cx| {
                if playing || panning {
                    // panning: viewport rect follows; playing: playhead
                    window.request_animation_frame();
                }
                let w: f32 = bounds.size.width.into();
                let h: f32 = bounds.size.height.into();
                let sx = w / song_end as f32;
                // overview only — decimate dense songs so this paint stays
                // cheap at animation-frame rate
                let stride = mini_notes.len() / 1500 + 1;
                for n in mini_notes.iter().step_by(stride) {
                    let x = bounds.origin.x + px(n.start_tick as f32 * sx);
                    let y = bounds.origin.y + px((127.0 - n.key as f32) / 128.0 * (h - 2.0) + 1.0);
                    let nw = ((n.end_tick.unwrap_or(n.start_tick) - n.start_tick).max(1) as f32
                        * sx)
                        .max(1.5);
                    let c = TRACK_COLORS[n.track % TRACK_COLORS.len()];
                    let c = if n.track == mini_active {
                        c
                    } else {
                        blend(c, 0x111118, 0.55)
                    };
                    window.paint_quad(fill(
                        Bounds::new(point(x, y), size(px(nw), px((h / 64.0).max(1.2)))),
                        rgb(c),
                    ));
                }
                // viewport rectangle
                let vt0 = scroll_x / zoom;
                let vw_ticks = f32::from(roll_bounds_cell.get().size.width) / zoom;
                let vx = bounds.origin.x + px(vt0 * sx);
                let vw = px((vw_ticks * sx).max(6.0));
                window.paint_quad(fill(
                    Bounds::new(point(vx, bounds.origin.y), size(vw, px(h))),
                    rgba(0x9fd0ff1c),
                ));
                window.paint_quad(fill(
                    Bounds::new(point(vx, bounds.origin.y), size(vw, px(1.0))),
                    rgb(0x4f7fb0),
                ));
                window.paint_quad(fill(
                    Bounds::new(point(vx, bounds.origin.y + px(h - 1.0)), size(vw, px(1.0))),
                    rgb(0x4f7fb0),
                ));
                // playhead
                let pxx = bounds.origin.x + px(mini_play as f32 * sx);
                window.paint_quad(fill(
                    Bounds::new(point(pxx, bounds.origin.y), size(px(1.0), px(h))),
                    rgb(0x50ff9f),
                ));
            },
        );

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
                        Bounds::new(
                            point(hx - px(2.0), bounds.origin.y),
                            size(px(4.0), px(12.0)),
                        ),
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
                // one wheel handler for the whole timeline (roll, ruler,
                // minimap, lane) — the strips share the same scroll offset
                .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _w, cx| {
                    let d = ev.delta.pixel_delta(px(20.0));
                    if ev.modifiers.control {
                        // zoom around the cursor: the tick under it stays put
                        let b = this.roll_bounds.get();
                        let off = (f32::from(ev.position.x) - f32::from(b.origin.x))
                            .clamp(0.0, f32::from(b.size.width));
                        let anchor_tick = (off + this.scroll_x) / this.zoom;
                        this.zoom =
                            (this.zoom * (1.0 - d.y.to_f64() as f32 * 0.002)).clamp(ZOOM_MIN, ZOOM_MAX);
                        this.scroll_x = (anchor_tick * this.zoom - off).max(0.0);
                    } else {
                        this.scroll_x = (this.scroll_x + d.x.to_f64() as f32).max(0.0);
                        this.scroll_y = (this.scroll_y + d.y.to_f64() as f32).max(0.0);
                    }
                    this.clamp_scroll();
                    cx.notify();
                }))
                .child(
                    div()
                        .h(px(20.0))
                        .w_full()
                        .bg(rgb(0x111118))
                        .border_b_1()
                        .border_color(rgb(BORDER_C))
                        .cursor_pointer()
                        .child(minimap.size_full())
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, ev: &MouseDownEvent, _w, cx| {
                                this.seek_minimap(f32::from(ev.position.x));
                                cx.notify();
                            }),
                        )
                        .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, cx| {
                            // drag on the minimap pans the view; ignore when a
                            // roll drag is mid-flight (cursor crossed strips)
                            if ev.pressed_button != Some(MouseButton::Left) || this.drag.is_some() {
                                return;
                            }
                            this.seek_minimap(f32::from(ev.position.x));
                            cx.notify();
                        })),
                )
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
                                // double-click on the ruler plays from that bar position
                                this.seek_to_tick(tick, ev.click_count == 2, cx);
                            }),
                        )
                        .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, cx| {
                            // scrub: drag on the ruler moves the playhead.
                            // While playing, keep the engine running — a
                            // restart per move event would stutter audio.
                            if ev.pressed_button != Some(MouseButton::Left)
                                || this.drag.is_some()
                                || this.playback.is_some()
                            {
                                return;
                            }
                            let b = this.ruler_bounds.get();
                            let x = f32::from(ev.position.x) - f32::from(b.origin.x);
                            let tick = ((x + this.scroll_x) / this.zoom).max(0.0) as u64;
                            this.play_us = this.doc(|d| d.tempo_map.tick_to_us(tick));
                            cx.notify();
                        })),
                )
                // marker/lyric strip — meta 0x06/0x05 shown at their tick
                .child(
                    div()
                        .h(px(14.0))
                        .w_full()
                        .relative()
                        .overflow_hidden()
                        .bg(rgb(0x17171d))
                        .children(markers.iter().filter_map(|(tk, txt)| {
                            let x = *tk as f32 * zoom - scroll_x;
                            (x > -80.0).then(|| {
                                div()
                                    .absolute()
                                    .left(px(x))
                                    .top(px(0.0))
                                    .text_size(px(9.0))
                                    .text_color(rgb(0x9fd0ff))
                                    .whitespace_nowrap()
                                    .child(txt.clone())
                            })
                        })),
                )
                .child(
                    div()
                        .flex_1()
                        .relative()
                        .overflow_hidden()
                        .child(roll.size_full())
                        .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, ev: &MouseDownEvent, w, cx| {
                        w.focus(&this.focus, cx);
                        this.mouse_pos = Some(ev.position);
                        let shift = ev.modifiers.shift;
                        // erase tool: any note under the cursor joins the stroke
                        if this.tool == Tool::Erase {
                            if let Some(n) = this
                                .note_at(ev.position)
                                .or_else(|| this.edge_at(ev.position))
                            {
                                this.erase_ids.insert(n.on_id);
                                this.sel_track = n.track;
                            }
                            this.drag = Some(Drag {
                                mode: DragMode::Erase,
                                on_id: 0,
                                off_id: None,
                                track: 0,
                                orig_start: 0,
                                orig_end: None,
                                orig_key: 0,
                                dtick: 0,
                                dkey: 0,
                                a_tick: 0,
                                a_key: 0,
                                b_tick: 0,
                                b_key: 0,
                            });
                            cx.notify();
                            return;
                        }
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
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _ev: &MouseUpEvent, _w, cx| {
                        // draw-tool marquee → insert lives in commit_drag
                        this.commit_drag(cx);
                    }),
                )
                // a release over the ruler/lane/track column must still commit
                // — GPUI only fires on_mouse_up for the hovered element
                .on_mouse_up_out(
                    MouseButton::Left,
                    cx.listener(|this, _ev: &MouseUpEvent, _w, cx| {
                        this.commit_drag(cx);
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
                                    v.persist();
                                    cx.notify();
                                })),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, ev: &MouseDownEvent, _w, cx| {
                                this.mouse_pos = Some(ev.position);
                                let b = this.lane_bounds.get();
                                let x = f32::from(ev.position.x) - f32::from(b.origin.x);
                                let y = f32::from(ev.position.y) - f32::from(b.origin.y);
                                let tick = ((x + this.scroll_x) / this.zoom).max(0.0) as u64;
                                let h = f32::from(b.size.height);
                                match this.lane_mode {
                                    LaneMode::Velocity => {
                                        let vel =
                                            ((1.0 - y / h) * 127.0) as i32;
                                        // the note bar under the cursor (within
                                        // ~6px) — a click on empty lane space
                                        // must not edit some distant note
                                        let bar_dx = |n: &document::Note| {
                                            n.start_tick as f32 * this.zoom - x
                                        };
                                        if let Some(n) = this
                                            .notes
                                            .iter()
                                            .filter(|n| n.track == this.sel_track)
                                            .min_by(|a, b| {
                                                bar_dx(a).abs().total_cmp(&bar_dx(b).abs())
                                            })
                                            .filter(|n| bar_dx(n).abs() <= 6.0)
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
                                            let sh = crate::lock_shared(&this.shared);
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
                        // drag deltas are forwarded by the root mouse-move
                        // listener, so a lane drag keeps tracking even when
                        // the cursor crosses into the ruler or roll
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _ev: &MouseUpEvent, _w, cx| {
                                this.commit_drag(cx)
                            }),
                        )
                        .on_mouse_up_out(
                            MouseButton::Left,
                            cx.listener(|this, _ev: &MouseUpEvent, _w, cx| {
                                this.commit_drag(cx)
                            }),
                        ),
                ),
        );
        let body = body.children(self.show_events.then_some(events_panel));

        // --- status bar ---------------------------------------------------------
        let enc_label = match self.enc_override {
            None => "auto".to_string(),
            Some(e) => e.label().to_string(),
        };
        let plugin_chip = if sel_is_plugin {
            let name = dests
                .get(eff_dest)
                .map(|(n, _)| n.clone())
                .unwrap_or_default();
            let (badge, color, tip) = match self.plugin_state.get(&eff_dest) {
                Some(PluginState::Ready { .. }) => {
                    ("●", 0x8fd0a0, t("plugin.state_ready").to_string())
                }
                Some(PluginState::Loading { .. }) => {
                    ("◌ …", 0xe0b050, t("plugin.state_loading").to_string())
                }
                Some(PluginState::Failed { phase, msg, .. }) => {
                    ("✕", 0xe06060, format!("{phase}: {msg}"))
                }
                _ => ("", 0x77778a, t("plugin.state_idle").to_string()),
            };
            Some(
                div()
                    .id("plugin-chip")
                    .px_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .text_color(rgb(color))
                    .child(format!("{badge} {name}"))
                    .tooltip(move |_w, cx| {
                        let tip = tip.clone();
                        cx.new(|_| Tip(tip.into())).into()
                    })
                    .on_click(cx.listener(|v, _e, _w, cx| {
                        v.show_output_status = true;
                        cx.notify();
                    })),
            )
        } else {
            None
        };
        let status_bar = div()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .h(px(24.0))
            .bg(rgb(BG_BAR))
            .border_t_1()
            .border_color(rgb(BORDER_C))
            .text_size(px(11.0))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_color(rgb(0x77778a))
                    .child(format!("{}", self.status)),
            )
            .children(plugin_chip)
            .child(Self::chip("st-lane", lane_mode.label(), cx, |v, _e, cx| {
                v.set_lane(v.lane_mode.cycle(), cx);
            }))
            .child(Self::chip(
                "st-enc",
                format!("enc {enc_label}"),
                cx,
                |v, _e, cx| {
                    let next = match v.enc_override {
                        None => Some(smf_core::TextEncoding::Utf8),
                        Some(smf_core::TextEncoding::Utf8) => {
                            Some(smf_core::TextEncoding::ShiftJis)
                        }
                        Some(smf_core::TextEncoding::ShiftJis) => {
                            Some(smf_core::TextEncoding::Latin1)
                        }
                        Some(smf_core::TextEncoding::Latin1) => None,
                    };
                    v.set_enc(next, cx);
                },
            ))
            .child(
                div()
                    .text_color(rgb(0x77778a))
                    .font_family("Cascadia Mono")
                    .whitespace_nowrap()
                    .child(format!("{pos}  {}", {
                        let sel = self.selection.len();
                        if sel > 0 {
                            format!("sel:{sel}")
                        } else {
                            String::new()
                        }
                    })),
            );

        // --- open menu dropdown --------------------------------------------------
        let menu_layer = self.open_menu.map(|(m, mx)| {
            let items: Vec<AnyElement> = match m {
                TopMenu::File => vec![
                    Self::mi("f.new", t("menu.new"), "", None, cx, |v, _e, cx| {
                        v.new_file(cx);
                    })
                    .into_any_element(),
                    Self::mi("f.open", t("menu.open"), "Ctrl+O", None, cx, |v, _e, cx| {
                        v.open_dialog(cx);
                    })
                    .into_any_element(),
                    Self::mi_sub("f.recent", t("menu.recent"), Sub::Recent, cx).into_any_element(),
                    Self::msep().into_any_element(),
                    Self::mi("f.save", t("menu.save"), "Ctrl+S", None, cx, |v, _e, cx| {
                        v.save(cx);
                    })
                    .into_any_element(),
                    Self::mi("f.savas", t("menu.save_as"), "", None, cx, |v, _e, cx| {
                        v.save_as(cx);
                    })
                    .into_any_element(),
                ],
                TopMenu::Edit => vec![
                    Self::mi("e.undo", t("menu.undo"), "Ctrl+Z", None, cx, |v, _e, cx| {
                        v.undo(cx);
                    })
                    .into_any_element(),
                    Self::mi("e.redo", t("menu.redo"), "Ctrl+Y", None, cx, |v, _e, cx| {
                        v.redo(cx);
                    })
                    .into_any_element(),
                    Self::msep().into_any_element(),
                    Self::mi(
                        "e.selall",
                        t("menu.select_all"),
                        "Ctrl+A",
                        None,
                        cx,
                        |v, _e, cx| {
                            v.select_all(cx);
                        },
                    )
                    .into_any_element(),
                    Self::msep().into_any_element(),
                    Self::mi("e.cut", t("edit.cut"), "Ctrl+X", None, cx, |v, _e, cx| {
                        v.copy_selected(true, cx);
                    })
                    .into_any_element(),
                    Self::mi("e.copy", t("edit.copy"), "Ctrl+C", None, cx, |v, _e, cx| {
                        v.copy_selected(false, cx);
                    })
                    .into_any_element(),
                    Self::mi(
                        "e.paste",
                        t("edit.paste"),
                        "Ctrl+V",
                        None,
                        cx,
                        |v, _e, cx| {
                            v.paste(cx);
                        },
                    )
                    .into_any_element(),
                    Self::mi(
                        "e.dup",
                        t("edit.duplicate"),
                        "Ctrl+D",
                        None,
                        cx,
                        |v, _e, cx| {
                            v.duplicate_selected(cx);
                        },
                    )
                    .into_any_element(),
                    Self::mi("e.del", t("menu.delete"), "Del", None, cx, |v, _e, cx| {
                        v.delete_selected(cx);
                    })
                    .into_any_element(),
                    Self::msep().into_any_element(),
                    Self::mi_sub("e.tool", t("edit.tool"), Sub::Tool, cx).into_any_element(),
                    Self::mi_sub("e.snap", t("edit.snap"), Sub::Snap, cx).into_any_element(),
                    Self::msep().into_any_element(),
                    Self::mi_sub("e.quant", t("edit.quantize"), Sub::Quant, cx).into_any_element(),
                    Self::mi(
                        "e.trup",
                        t("edit.transpose_up"),
                        "",
                        None,
                        cx,
                        |v, _e, _cx| {
                            v.apply_region_op("transpose +1", |d, t, f, to| {
                                d.transpose_ops(t, f, to, 1)
                            });
                        },
                    )
                    .into_any_element(),
                    Self::mi(
                        "e.trdn",
                        t("edit.transpose_dn"),
                        "",
                        None,
                        cx,
                        |v, _e, _cx| {
                            v.apply_region_op("transpose -1", |d, t, f, to| {
                                d.transpose_ops(t, f, to, -1)
                            });
                        },
                    )
                    .into_any_element(),
                    Self::mi_sub("e.oct", t("edit.octave"), Sub::Oct, cx).into_any_element(),
                    Self::msep().into_any_element(),
                    Self::mi("e.human", t("edit.humanize"), "", None, cx, |v, _e, _cx| {
                        v.apply_region_op("humanize", |d, t, f, to| {
                            d.humanize_ops(t, f, to, 12, 8)
                        });
                    })
                    .into_any_element(),
                    Self::mi("e.legato", t("edit.legato"), "", None, cx, |v, _e, _cx| {
                        v.apply_region_op("legato", |d, t, f, to| d.legato_ops(t, f, to));
                    })
                    .into_any_element(),
                    Self::mi_sub("e.len", t("edit.set_length"), Sub::LenSet, cx).into_any_element(),
                    Self::mi_sub("e.velset", t("edit.set_velocity"), Sub::VelSet, cx)
                        .into_any_element(),
                    Self::msep().into_any_element(),
                    Self::mi("e.velup", t("edit.vel_up"), "", None, cx, |v, _e, _cx| {
                        v.apply_region_op("vel ×1.25", |d, t, f, to| {
                            d.scale_velocity_ops(t, f, to, 1.25)
                        });
                    })
                    .into_any_element(),
                    Self::mi("e.veldn", t("edit.vel_dn"), "", None, cx, |v, _e, _cx| {
                        v.apply_region_op("vel ×0.8", |d, t, f, to| {
                            d.scale_velocity_ops(t, f, to, 0.8)
                        });
                    })
                    .into_any_element(),
                ],
                TopMenu::View => vec![
                    Self::mi(
                        "v.events",
                        t("view.events"),
                        "",
                        Some(self.show_events),
                        cx,
                        |v, _e, _cx| {
                            v.show_events = !v.show_events;
                            v.persist();
                        },
                    )
                    .into_any_element(),
                    Self::msep().into_any_element(),
                    Self::mi(
                        "v.zin",
                        t("view.zoom_in"),
                        "Ctrl+=",
                        None,
                        cx,
                        |v, _e, cx| {
                            v.zoom_by(1.3, cx);
                        },
                    )
                    .into_any_element(),
                    Self::mi(
                        "v.zout",
                        t("view.zoom_out"),
                        "Ctrl+-",
                        None,
                        cx,
                        |v, _e, cx| {
                            v.zoom_by(1.0 / 1.3, cx);
                        },
                    )
                    .into_any_element(),
                    Self::mi(
                        "v.z0",
                        t("view.zoom_reset"),
                        "Ctrl+0",
                        None,
                        cx,
                        |v, _e, cx| {
                            v.zoom_set(0.08, cx);
                        },
                    )
                    .into_any_element(),
                    Self::msep().into_any_element(),
                    Self::mi_sub("v.lane", t("view.lane"), Sub::Lane, cx).into_any_element(),
                    Self::mi_sub("v.enc", t("view.encoding"), Sub::Enc, cx).into_any_element(),
                ],
                TopMenu::Track => {
                    let mut items = vec![
                        Self::mi("t.rename", t("track.rename"), "", None, cx, |v, w, cx| {
                            v.focus_rename(w, cx);
                        })
                        .into_any_element(),
                        Self::msep().into_any_element(),
                        Self::mi(
                            "t.mute",
                            t("track.mute"),
                            "",
                            Some(muted_set.contains(&self.sel_track)),
                            cx,
                            |v, _e, _cx| {
                                let t = v.sel_track;
                                {
                                    let mut sh = crate::lock_shared(&v.shared);
                                    if !sh.muted.remove(&t) {
                                        sh.muted.insert(t);
                                    }
                                }
                                v.persist();
                            },
                        )
                        .into_any_element(),
                        Self::mi(
                            "t.solo",
                            t("track.solo"),
                            "",
                            Some(soloed_set.contains(&self.sel_track)),
                            cx,
                            |v, _e, _cx| {
                                let t = v.sel_track;
                                {
                                    let mut sh = crate::lock_shared(&v.shared);
                                    if !sh.soloed.remove(&t) {
                                        sh.soloed.insert(t);
                                    }
                                }
                                v.persist();
                            },
                        )
                        .into_any_element(),
                        Self::msep().into_any_element(),
                        Self::mi_sub("t.chan", t("track.channel"), Sub::Chan, cx)
                            .into_any_element(),
                        Self::mi_sub("t.dest", t("track.dest"), Sub::Dest, cx).into_any_element(),
                    ];
                    if sel_is_plugin {
                        items.push(Self::msep().into_any_element());
                        items.push(
                            Self::mi(
                                "t.gui",
                                t("track.plugin_gui"),
                                "",
                                None,
                                cx,
                                |v, _e, _cx| {
                                    v.open_plugin_gui();
                                },
                            )
                            .into_any_element(),
                        );
                    }
                    items
                }
                TopMenu::Output => {
                    let mut items = vec![
                        Self::mi_sub("o.def", t("output.default_dest"), Sub::DefDest, cx)
                            .into_any_element(),
                        Self::mi_sub("o.in", t("output.midi_in"), Sub::InPort, cx)
                            .into_any_element(),
                        Self::msep().into_any_element(),
                    ];
                    if sel_is_plugin {
                        items.push(
                            Self::mi(
                                "o.gui",
                                if self.plugin_window.is_some() {
                                    t("output.editor_close")
                                } else {
                                    t("output.editor_open")
                                },
                                "",
                                Some(self.plugin_window.is_some()),
                                cx,
                                |v, _e, _cx| v.open_plugin_gui(),
                            )
                            .into_any_element(),
                        );
                    }
                    if sel_plugin_failed {
                        items.push(
                            Self::mi("o.retry", t("output.retry"), "", None, cx, |v, _e, _cx| {
                                let d = crate::lock_shared(&v.shared).dest_of(v.sel_track);
                                v.ensure_plugin(d, true);
                            })
                            .into_any_element(),
                        );
                    }
                    items.extend([
                        Self::msep().into_any_element(),
                        Self::mi("o.rescan", t("output.rescan"), "", None, cx, |v, _e, cx| {
                            v.rescan_plugins();
                            cx.notify();
                        })
                        .into_any_element(),
                        Self::mi(
                            "o.status",
                            t("output.host_status"),
                            "",
                            None,
                            cx,
                            |v, _e, cx| {
                                v.show_output_status = true;
                                cx.notify();
                            },
                        )
                        .into_any_element(),
                    ]);
                    items
                }
                TopMenu::Transport => vec![
                    Self::mi(
                        "tr.play",
                        t("transport.play_stop"),
                        "Space",
                        Some(self.playback.is_some()),
                        cx,
                        |v, _e, cx| {
                            v.toggle_play(cx);
                        },
                    )
                    .into_any_element(),
                    Self::mi(
                        "tr.rec",
                        t("transport.record"),
                        "",
                        Some(self.rec.is_some()),
                        cx,
                        |v, _e, _cx| {
                            v.toggle_record();
                        },
                    )
                    .into_any_element(),
                    Self::mi(
                        "tr.loop",
                        t("transport.loop"),
                        "",
                        Some(loop_en),
                        cx,
                        |v, _e, _cx| {
                            {
                                let mut sh = crate::lock_shared(&v.shared);
                                sh.loop_enabled = !sh.loop_enabled;
                            }
                            v.persist();
                        },
                    )
                    .into_any_element(),
                    Self::mi(
                        "tr.met",
                        t("transport.met"),
                        "",
                        Some(met_en),
                        cx,
                        |v, _e, _cx| {
                            {
                                let mut sh = crate::lock_shared(&v.shared);
                                sh.metronome = !sh.metronome;
                            }
                            v.persist();
                        },
                    )
                    .into_any_element(),
                    Self::mi(
                        "tr.chsy",
                        t("transport.chase_sysex"),
                        "",
                        Some(chsy_en),
                        cx,
                        |v, _e, _cx| {
                            {
                                let mut sh = crate::lock_shared(&v.shared);
                                sh.chase_sysex = !sh.chase_sysex;
                            }
                            v.persist();
                        },
                    )
                    .into_any_element(),
                    Self::mi(
                        "tr.sxp",
                        tf(
                            "transport.sysex_pol",
                            &[("mode", t(match sxp {
                                midi_io::SysexPolicy::Serialize => "transport.sysex_ser",
                                midi_io::SysexPolicy::Background => "transport.sysex_bg",
                                midi_io::SysexPolicy::Skip => "transport.sysex_skip",
                            }))],
                        ),
                        "",
                        None,
                        cx,
                        |v, _e, _cx| {
                            {
                                let mut sh = crate::lock_shared(&v.shared);
                                sh.sysex_policy = sh.sysex_policy.cycle();
                            }
                            v.persist();
                        },
                    )
                    .into_any_element(),
                    Self::mi(
                        "tr.cin",
                        t("transport.count_in"),
                        "",
                        Some(self.count_in),
                        cx,
                        |v, _e, _cx| {
                            v.count_in = !v.count_in;
                            v.save_global();
                        },
                    )
                    .into_any_element(),
                ],
                TopMenu::Help => vec![
                    Self::mi(
                        "h.keys",
                        t("help.shortcuts"),
                        "F1",
                        None,
                        cx,
                        |v, _e, cx| {
                            v.help_open = !v.help_open;
                            cx.notify();
                        },
                    )
                    .into_any_element(),
                    Self::mi("h.about", t("help.about"), "", None, cx, |v, _e, _cx| {
                        v.status = concat!(
                            "midi-editor ",
                            env!("CARGO_PKG_VERSION"),
                            " — pure-SMF editor"
                        )
                        .into();
                    })
                    .into_any_element(),
                    Self::mi("h.mcp", t("help.mcp"), "", None, cx, |v, _e, _cx| {
                        v.status =
                            "MCP: http://127.0.0.1:7878/mcp (mcp-bridge for stdio clients)".into();
                    })
                    .into_any_element(),
                ],
            };
            // dropdown panel under the clicked label
            let popup_max_h = (f32::from(window.viewport_size().height) - 40.0).max(120.0);
            let popup_h = (items.len() as f32 * 24.0 + 16.0).min(popup_max_h);
            let popup = div()
                .id("menu-popup")
                .absolute()
                .top(px(0.0))
                .left(px(mx))
                .w(px(210.0))
                .h(px(popup_h))
                .max_h(px(popup_max_h))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .py_1()
                .bg(rgb(BG_RAISED))
                .border_1()
                .border_color(rgb(BORDER_C))
                .rounded_md()
                .shadow_lg()
                .children(items)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|_v, _e, _w, cx| cx.stop_propagation()),
                );
            // cascading submenu (also inside the overlay so clicks elsewhere close all)
            let sub_popup = self.open_sub.map(|(s, y)| {
                let x2 = mx + 208.0;
                let rows: Vec<AnyElement> = match s {
                    Sub::Chan => (0u8..16)
                        .map(|ch| {
                            let cur = track_chs.get(self.sel_track).copied().unwrap_or(0);
                            Self::mi_leaf(
                                ("chan", ch as usize),
                                format!("Channel {}", ch + 1),
                                "",
                                Some(cur == ch),
                                cx,
                                move |v, _e, _cx| {
                                    let tr = v.sel_track;
                                    let ops = {
                                        let mut sh = crate::lock_shared(&v.shared);
                                        sh.doc.set_track_channel_ops(tr, ch)
                                    };
                                    v.apply_tx("set track channel", ops);
                                },
                            )
                            .into_any_element()
                        })
                        .collect(),
                    Sub::Dest => self.dest_rows(
                        DestPick::Track,
                        &dests,
                        eff_dest,
                        def_dest,
                        has_track_dest,
                        cx,
                    ),
                    Sub::DefDest => self.dest_rows(
                        DestPick::Default,
                        &dests,
                        eff_dest,
                        def_dest,
                        has_track_dest,
                        cx,
                    ),
                    Sub::InPort => {
                        let ports = midi_io::list_inputs().unwrap_or_default();
                        let mut rows: Vec<AnyElement> = vec![Self::mi_leaf(
                            "in.default",
                            t("output.first_input"),
                            "",
                            Some(self.midi_in.is_empty()),
                            cx,
                            |v, _e, _cx| {
                                v.midi_in = "".into();
                                v.save_global();
                            },
                        )
                        .into_any_element()];
                        rows.extend(ports.iter().enumerate().map(|(i, p)| {
                            let name = p.name.clone();
                            Self::mi_leaf(
                                ("inport", i),
                                name.clone(),
                                "",
                                Some(self.midi_in.as_str() == name),
                                cx,
                                move |v, _e, _cx| {
                                    v.midi_in = name.clone().into();
                                    v.save_global();
                                },
                            )
                            .into_any_element()
                        }));
                        if ports.is_empty() {
                            rows.push(
                                Self::mi_leaf(
                                    "in.none",
                                    t("output.no_inputs"),
                                    "",
                                    None,
                                    cx,
                                    |_, _, _| {},
                                )
                                .into_any_element(),
                            );
                        }
                        rows
                    }
                    Sub::Lane => {
                        const LANES: [LaneMode; 7] = [
                            LaneMode::Velocity,
                            LaneMode::CC(1),
                            LaneMode::CC(7),
                            LaneMode::CC(10),
                            LaneMode::CC(11),
                            LaneMode::CC(64),
                            LaneMode::PitchBend,
                        ];
                        LANES
                            .iter()
                            .enumerate()
                            .map(|(i, lm)| {
                                Self::mi_leaf(
                                    ("lane", i),
                                    lm.label(),
                                    "",
                                    Some(self.lane_mode == *lm),
                                    cx,
                                    move |v, _e, cx| v.set_lane(*lm, cx),
                                )
                                .into_any_element()
                            })
                            .collect()
                    }
                    Sub::Tool => {
                        let opts = [
                            ("1", Tool::Select, "tool.select"),
                            ("2", Tool::Draw, "tool.draw"),
                            ("3", Tool::Erase, "tool.erase"),
                        ];
                        opts.into_iter()
                            .enumerate()
                            .map(|(i, (key, tool, label))| {
                                Self::mi_leaf(
                                    ("tool", i),
                                    t(label),
                                    key,
                                    Some(self.tool == tool),
                                    cx,
                                    move |v, _e, cx| v.set_tool(tool, cx),
                                )
                                .into_any_element()
                            })
                            .collect()
                    }
                    Sub::Snap => SNAPS
                        .iter()
                        .enumerate()
                        .map(|(i, (_div, _trip, label))| {
                            Self::mi_leaf(
                                ("snap", i),
                                *label,
                                "",
                                Some(self.snap_idx == i),
                                cx,
                                move |v, _e, cx| v.set_snap(i, cx),
                            )
                            .into_any_element()
                        })
                        .collect(),
                    Sub::Recent => {
                        if self.recent.is_empty() {
                            vec![Self::mi_leaf(
                                "recent.empty",
                                t("menu.recent_empty"),
                                "",
                                None,
                                cx,
                                |_v, _e, _cx| {},
                            )
                            .into_any_element()]
                        } else {
                            self.recent
                                .iter()
                                .enumerate()
                                .map(|(i, p)| {
                                    let name = p
                                        .rsplit(['\\', '/'])
                                        .next()
                                        .unwrap_or(p.as_str())
                                        .to_string();
                                    let path = std::path::PathBuf::from(p.as_str());
                                    Self::mi_leaf(
                                        ("recent", i),
                                        name,
                                        "",
                                        None,
                                        cx,
                                        move |v, _e, cx| {
                                            v.open(path.clone(), cx);
                                        },
                                    )
                                    .into_any_element()
                                })
                                .collect()
                        }
                    }
                    Sub::Quant => {
                        let g = self.snap_ticks().max(self.ppq() as i64 / 4) as u64;
                        [("100%", 100u32), ("75%", 75), ("50%", 50)]
                            .into_iter()
                            .enumerate()
                            .map(|(i, (label, str_))| {
                                Self::mi_leaf(
                                    ("quant", i),
                                    format!("Quantize {label}"),
                                    "",
                                    None,
                                    cx,
                                    move |v, _e, _cx| {
                                        v.apply_region_op("quantize", move |d, t, f, to| {
                                            d.quantize_ops(t, f, to, g, str_)
                                        });
                                    },
                                )
                                .into_any_element()
                            })
                            .collect()
                    }
                    Sub::Oct => {
                        let opts = [("+1 octave", 12i32), ("-1 octave", -12)];
                        opts.into_iter()
                            .enumerate()
                            .map(|(i, (label, st))| {
                                Self::mi_leaf(("oct", i), label, "", None, cx, move |v, _e, _cx| {
                                    v.apply_region_op("octave", move |d, t, f, to| {
                                        d.transpose_ops(t, f, to, st)
                                    });
                                })
                                .into_any_element()
                            })
                            .collect()
                    }
                    Sub::LenSet => {
                        let ppq = self.ppq();
                        let opts: [(&str, u64); 5] = [
                            ("1/32", ppq / 8),
                            ("1/16", ppq / 4),
                            ("1/8", ppq / 2),
                            ("1/4", ppq),
                            ("1 bar", ppq * 4),
                        ];
                        opts.into_iter()
                            .enumerate()
                            .map(|(i, (label, ticks))| {
                                Self::mi_leaf(("len", i), label, "", None, cx, move |v, _e, _cx| {
                                    v.apply_region_op("set length", move |d, t, f, to| {
                                        d.set_length_ops(t, f, to, ticks)
                                    });
                                })
                                .into_any_element()
                            })
                            .collect()
                    }
                    Sub::VelSet => {
                        let opts = [
                            ("pianissimo (32)", 32u8),
                            ("mezzo (72)", 72),
                            ("forte (100)", 100),
                            ("max (127)", 127),
                        ];
                        opts.into_iter()
                            .enumerate()
                            .map(|(i, (label, vel))| {
                                Self::mi_leaf(
                                    ("vset", i),
                                    label,
                                    "",
                                    None,
                                    cx,
                                    move |v, _e, _cx| {
                                        v.apply_region_op("set velocity", move |d, t, f, to| {
                                            d.set_velocity_ops(t, f, to, vel)
                                        });
                                    },
                                )
                                .into_any_element()
                            })
                            .collect()
                    }
                    Sub::Enc => {
                        let opts: [(Option<smf_core::TextEncoding>, &str); 4] = [
                            (None, "auto"),
                            (Some(smf_core::TextEncoding::Utf8), "UTF-8"),
                            (Some(smf_core::TextEncoding::ShiftJis), "Shift-JIS"),
                            (Some(smf_core::TextEncoding::Latin1), "Latin-1"),
                        ];
                        opts.into_iter()
                            .enumerate()
                            .map(|(i, (e, label))| {
                                Self::mi_leaf(
                                    ("enc", i),
                                    label,
                                    "",
                                    Some(self.enc_override == e),
                                    cx,
                                    move |v, _e, cx| v.set_enc(e, cx),
                                )
                                .into_any_element()
                            })
                            .collect()
                    }
                };
                let vh = f32::from(window.viewport_size().height);
                let desired = rows.len() as f32 * 24.0 + 16.0;
                let max_h = (vh - 40.0).max(120.0);
                let h = desired.min(max_h);
                let top = (y - 30.0).clamp(0.0, (vh - h - 8.0).max(0.0));
                div()
                    .id("sub-popup")
                    .absolute()
                    .top(px(top))
                    .left(px(x2))
                    .w(px(190.0))
                    .h(px(h))
                    .max_h(px(h))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .py_1()
                    .bg(rgb(BG_RAISED))
                    .border_1()
                    .border_color(rgb(BORDER_C))
                    .rounded_md()
                    .shadow_lg()
                    .children(rows)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_v, _e, _w, cx| cx.stop_propagation()),
                    )
            });
            div()
                .id("menu-overlay")
                .absolute()
                .top(px(28.0))
                .left(px(0.0))
                .right(px(0.0))
                .bottom(px(0.0))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|v, _e, _w, cx| {
                        // click-outside only dismisses the menu — the click
                        // must not fall through to the canvas underneath
                        cx.stop_propagation();
                        v.open_menu = None;
                        v.open_sub = None;
                        cx.notify();
                    }),
                )
                .child(popup)
                .children(sub_popup)
        });

        // shortcuts overlay (F1 / Help > Keyboard Shortcuts)
        let help_layer = self.help_open.then(|| {
            const ROWS: [(&str, &str); 21] = [
                ("Space", "Play / stop"),
                ("F1", "This panel"),
                ("Esc", "Close menus / clear selection"),
                ("Ctrl+N / O / S", "New / Open / Save"),
                ("Ctrl+Z / Y", "Undo / redo"),
                ("Ctrl+A", "Select all notes"),
                ("Ctrl+X / C / V", "Cut / copy / paste"),
                ("Ctrl+D", "Duplicate selection"),
                ("Del", "Delete selection"),
                ("1 / 2 / 3", "Select / draw / erase tool"),
                ("← →", "Nudge by grid step"),
                ("Shift+← →", "Nudge by 1 tick"),
                ("↑ ↓", "Transpose by semitone"),
                ("Shift+↑ ↓", "Transpose by octave"),
                ("Alt+drag note", "Duplicate note(s)"),
                ("Right-edge drag", "Resize note"),
                ("Click ruler", "Seek playhead"),
                ("Double-click ruler", "Play from here"),
                ("Click minimap", "Jump to position"),
                ("Ctrl+wheel", "Zoom timeline"),
                ("Drag .mid file", "Drop to open"),
            ];
            let panel = div()
                .id("help-panel")
                .flex()
                .flex_col()
                .w(px(420.0))
                .py_2()
                .px_3()
                .bg(rgb(0x20202c))
                .border_1()
                .border_color(rgb(0x3c3c4a))
                .rounded_lg()
                .shadow_lg()
                .text_size(px(12.0))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|_v, _e, _w, cx| cx.stop_propagation()),
                )
                .child(
                    div()
                        .text_size(px(14.0))
                        .text_color(rgb(0x9fd0ff))
                        .pb_2()
                        .child(t("help.shortcuts")),
                )
                .children(ROWS.iter().map(|(k, v)| {
                    div()
                        .flex()
                        .h(px(20.0))
                        .items_center()
                        .child(
                            div()
                                .w(px(150.0))
                                .font_family("Cascadia Mono")
                                .text_color(rgb(0x8fd0a0))
                                .child(*k),
                        )
                        .child(div().text_color(rgb(0xd8d8e0)).child(*v))
                }));
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgba(0x00000066))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|v, _e, _w, cx| {
                        v.help_open = false;
                        cx.notify();
                    }),
                )
                .child(panel)
        });

        let output_status = self.show_output_status.then(|| {
            let diag = &self.host_diag;
            let diag_row = |label: SharedString, path: &Option<PathBuf>, hint: SharedString| {
                let value = path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| t("output.missing").to_string());
                let mut row = div().flex().flex_col().gap_1().child(
                    div().flex().gap_1().child(format!("{label}:")).child(
                        div()
                            .text_color(rgb(if path.is_some() { 0xd8d8e0 } else { 0xe06060 }))
                            .child(value),
                    ),
                );
                if path.is_none() {
                    row = row.child(
                        div()
                            .text_size(px(11.0))
                            .text_color(rgb(0x9999aa))
                            .child(hint),
                    );
                }
                row.into_any_element()
            };
            let scan = if self.scan_rx.is_some() {
                t("status.scanning").to_string()
            } else {
                let n = self.plugin_meta.len();
                let mode = match self.scan_probe_used {
                    Some(true) => t("output.probe_used"),
                    _ => t("output.probe_unused"),
                };
                format!("{n} plugins, {mode}")
            };
            let mut rows: Vec<AnyElement> = vec![
                diag_row(
                    t("output.helper").into(),
                    &diag.helper,
                    t("output.helper_hint").into(),
                ),
                diag_row(
                    t("output.probe").into(),
                    &diag.probe,
                    t("output.helper_hint").into(),
                ),
                div()
                    .child(format!(
                        "{}: {}",
                        t("output.audio"),
                        diag.audio_device.clone().unwrap_or_else(|e| e)
                    ))
                    .into_any_element(),
                div()
                    .child(format!("{}: {}", t("output.scan"), scan))
                    .into_any_element(),
            ];
            if let Some(note) = &self.scan_note {
                rows.push(
                    div()
                        .text_color(rgb(0x9999aa))
                        .child(note.clone())
                        .into_any_element(),
                );
            }
            rows.push(Self::msep().into_any_element());
            rows.push(Self::mhead(t("output.cat_vst3")).into_any_element());
            for (i, (name, dest)) in dests.iter().enumerate() {
                let output::Destination::Plugin { plugin_path } = dest else {
                    continue;
                };
                let vendor = self
                    .plugin_meta
                    .get(plugin_path)
                    .map(|p| p.vendor.clone())
                    .unwrap_or_default();
                let (state, color, retry, detail) = match self.plugin_state.get(&i) {
                    Some(PluginState::Ready { .. }) => {
                        (t("plugin.state_ready"), 0x8fd0a0, false, None)
                    }
                    Some(PluginState::Loading { .. }) => {
                        (t("plugin.state_loading"), 0xe0b050, false, None)
                    }
                    Some(PluginState::Failed { phase, msg, .. }) => {
                        let phase = match *phase {
                            "host" => t("plugin.phase_host"),
                            "audio" => t("plugin.phase_audio"),
                            _ => t("plugin.phase_load"),
                        };
                        (
                            t("plugin.state_failed"),
                            0xe06060,
                            true,
                            Some(format!("{phase}: {msg}")),
                        )
                    }
                    _ => (t("plugin.state_idle"), 0x77778a, false, None),
                };
                let mut row = div()
                    .id(("output-status", i))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .text_color(rgb(color))
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .items_center()
                            .child(format!("{}  {}", name, state))
                            .child(div().text_color(rgb(0x888899)).child(vendor)),
                    );
                if let Some(detail) = detail {
                    row = row.child(
                        div()
                            .text_size(px(11.0))
                            .text_color(rgb(0x9999aa))
                            .child(detail),
                    );
                }
                rows.push(
                    row.on_click(cx.listener(move |v, _e, _w, cx| {
                        if retry {
                            v.ensure_plugin(i, true);
                            cx.notify();
                        }
                    }))
                    .into_any_element(),
                );
            }
            let panel_content_h = 24.0 + rows.len() as f32 * 26.0 + 32.0 + 32.0;
            let panel = div()
                .id("output-status-panel")
                .w(px(520.0))
                .flex()
                .flex_col()
                .gap_1()
                .p_3()
                .bg(rgb(0x20202c))
                .border_1()
                .border_color(rgb(0x3c3c4a))
                .rounded_lg()
                .shadow_lg()
                .text_size(px(12.0))
                .child(
                    div()
                        .text_size(px(14.0))
                        .text_color(rgb(0x9fd0ff))
                        .child(t("output.status_title")),
                )
                .children(rows)
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .pt_2()
                        .child(Self::chip(
                            "status.rescan",
                            t("output.rescan"),
                            cx,
                            |v, _e, cx| {
                                v.rescan_plugins();
                                cx.notify();
                            },
                        ))
                        .child(Self::chip(
                            "status.close",
                            t("output.close"),
                            cx,
                            |v, _e, cx| {
                                v.show_output_status = false;
                                cx.notify();
                            },
                        )),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|_v, _e, _w, cx| cx.stop_propagation()),
                );
            let panel = if panel_content_h > 520.0 {
                panel.h(px(520.0)).max_h(px(520.0)).overflow_y_scroll()
            } else {
                panel
            };
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgba(0x00000066))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|v, _e, _w, cx| {
                        v.show_output_status = false;
                        cx.notify();
                    }),
                )
                .child(panel)
        });

        div()
            .flex()
            .flex_col()
            .relative()
            .size_full()
            .bg(rgb(0x1b1b22))
            .text_color(rgb(0xd8d8e0))
            .key_context("editor")
            .track_focus(&self.focus)
            // Drag follow-through at the window level: GPUI only delivers
            // mouse-move to the hovered element, so a roll drag must keep
            // updating while the cursor crosses the ruler, lane, or toolbar.
            // Also recovers a drag whose button went up outside the window.
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, cx| {
                this.mouse_pos = Some(ev.position);
                if this.drag.is_none() {
                    return;
                }
                if ev.pressed_button != Some(MouseButton::Left) {
                    this.commit_drag(cx);
                    return;
                }
                this.update_drag();
                cx.notify();
            }))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, w, cx| {
                // typing in the track-name field must not trigger editor keys
                if this.input.read(cx).focus_handle(cx).is_focused(w) {
                    return;
                }
                let k = ev.keystroke.key.as_str();
                let ctrl = ev.keystroke.modifiers.control;
                let shift = ev.keystroke.modifiers.shift;
                let st = {
                    let s = this.snap_ticks();
                    if s > 0 {
                        s
                    } else {
                        this.ppq() as i64 / 8
                    }
                };
                match (ctrl, shift, k) {
                    (true, false, "z") => this.undo(cx),
                    (true, false, "y") | (true, true, "z") => this.redo(cx),
                    (true, false, "s") => this.save(cx),
                    (true, false, "o") => this.open_dialog(cx),
                    (true, false, "n") => this.new_file(cx),
                    (true, false, "a") => this.select_all(cx),
                    (true, false, "=") | (true, false, "+") => this.zoom_by(1.3, cx),
                    (true, false, "-") => this.zoom_by(1.0 / 1.3, cx),
                    (true, false, "0") => this.zoom_set(0.08, cx),
                    (false, false, "escape") => {
                        this.open_menu = None;
                        this.open_sub = None;
                        this.help_open = false;
                        this.show_output_status = false;
                        this.selection.clear();
                        cx.notify();
                    }
                    (true, false, "x") => this.copy_selected(true, cx),
                    (true, false, "c") => this.copy_selected(false, cx),
                    (true, false, "v") => this.paste(cx),
                    (true, false, "d") => this.duplicate_selected(cx),
                    (false, false, "left") => this.nudge(-st, 0, cx),
                    (false, false, "right") => this.nudge(st, 0, cx),
                    (false, true, "left") => this.nudge(-1, 0, cx),
                    (false, true, "right") => this.nudge(1, 0, cx),
                    (false, false, "up") => this.nudge(0, 1, cx),
                    (false, false, "down") => this.nudge(0, -1, cx),
                    (false, true, "up") => this.nudge(0, 12, cx),
                    (false, true, "down") => this.nudge(0, -12, cx),
                    (false, false, "f1") => {
                        this.help_open = !this.help_open;
                        cx.notify();
                    }
                    (false, false, "delete") | (false, false, "backspace") => {
                        this.delete_selected(cx)
                    }
                    (false, false, "1") => this.set_tool(Tool::Select, cx),
                    (false, false, "2") => this.set_tool(Tool::Draw, cx),
                    (false, false, "3") => this.set_tool(Tool::Erase, cx),
                    (false, false, " ") => this.toggle_play(cx),
                    _ => {}
                }
            }))
            .child(menu_bar)
            .child(transport_bar)
            .child(body)
            .child(status_bar)
            .children(menu_layer)
            .children(help_layer)
            .children(output_status)
            // drag a .mid file anywhere to open it
            .can_drop(|drag: &dyn Any, _w, _cx| drag.is::<ExternalPaths>())
            .drag_over::<ExternalPaths>(|s, _p, _w, _cx| s.bg(rgb(0x16202e)))
            .on_drop(cx.listener(|v, paths: &ExternalPaths, _w, cx| {
                if let Some(p) = paths.paths().iter().find(|p| {
                    matches!(
                        p.extension().and_then(|e| e.to_str()),
                        Some("mid") | Some("smf") | Some("midi")
                    )
                }) {
                    v.open(p.clone(), cx);
                }
            }))
    }
}

// --- menubar helpers -----------------------------------------------------------

impl EditorView {
    fn mhead(label: impl Into<SharedString>) -> Div {
        div()
            .h(px(18.0))
            .px_2()
            .mx_1()
            .text_size(px(9.5))
            .text_color(rgb(0x7a7a90))
            .child(label.into())
    }

    fn dest_rows(
        &self,
        kind: DestPick,
        dests: &[(String, midi_io::Destination)],
        eff_dest: usize,
        def_dest: usize,
        has_track_dest: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows = Vec::new();
        if kind == DestPick::Track {
            rows.push(
                Self::mi_leaf(
                    "dest.default",
                    t("track.default_dest"),
                    "",
                    Some(!has_track_dest),
                    cx,
                    |v, _e, _cx| {
                        crate::lock_shared(&v.shared)
                            .track_dest
                            .remove(&v.sel_track);
                        v.persist();
                    },
                )
                .into_any_element(),
            );
            rows.push(Self::msep().into_any_element());
        }
        rows.push(Self::mhead(t("output.cat_midi")).into_any_element());
        let midi: Vec<(usize, String)> = dests
            .iter()
            .enumerate()
            .filter_map(|(i, (name, d))| {
                matches!(d, output::Destination::MidiPort { .. }).then_some((i, name.clone()))
            })
            .collect();
        if midi.is_empty() {
            rows.push(
                Self::mi_leaf(
                    "dest.noports",
                    t("output.no_ports"),
                    "",
                    None,
                    cx,
                    |_v, _e, _cx| {},
                )
                .into_any_element(),
            );
        } else {
            for (i, label) in midi {
                let selected = if kind == DestPick::Track {
                    has_track_dest && eff_dest == i
                } else {
                    def_dest == i
                };
                rows.push(
                    Self::mi_leaf(
                        ("dest", i),
                        label,
                        "",
                        Some(selected),
                        cx,
                        move |v, _e, _cx| {
                            let mut sh = crate::lock_shared(&v.shared);
                            if kind == DestPick::Track {
                                sh.track_dest.insert(v.sel_track, i);
                            } else {
                                sh.default_dest = i;
                            }
                            drop(sh);
                            v.persist();
                        },
                    )
                    .into_any_element(),
                );
            }
        }
        rows.push(Self::msep().into_any_element());
        let plugins: Vec<(usize, String, String, String)> = dests
            .iter()
            .enumerate()
            .filter_map(|(i, (name, d))| {
                let output::Destination::Plugin { plugin_path } = d else {
                    return None;
                };
                let vendor = self
                    .plugin_meta
                    .get(plugin_path)
                    .map(|p| p.vendor.clone())
                    .unwrap_or_default();
                Some((i, name.clone(), plugin_path.clone(), vendor))
            })
            .collect();
        let mut vendors = std::collections::BTreeSet::new();
        for (_, _, _, vendor) in &plugins {
            if !vendor.is_empty() {
                vendors.insert(vendor.clone());
            }
        }
        rows.push(
            Self::mhead(format!("{} ({})", t("output.cat_vst3"), plugins.len())).into_any_element(),
        );
        if plugins.is_empty() {
            if self.scan_rx.is_some() {
                rows.push(
                    Self::mi_leaf(
                        "dest.scanning",
                        t("status.scanning"),
                        "",
                        None,
                        cx,
                        |_v, _e, _cx| {},
                    )
                    .into_any_element(),
                );
            } else {
                rows.push(
                    Self::mi_leaf(
                        "dest.noplugins",
                        t("output.no_plugins"),
                        "",
                        None,
                        cx,
                        |_v, _e, _cx| {},
                    )
                    .into_any_element(),
                );
            }
        } else {
            let mut plugins = plugins;
            plugins.sort_by(|a, b| {
                if vendors.len() >= 2 {
                    a.3.cmp(&b.3).then(a.1.cmp(&b.1))
                } else {
                    a.1.cmp(&b.1)
                }
            });
            let mut last_vendor = String::new();
            for (i, label, path, vendor) in plugins {
                if vendors.len() >= 2 && vendor != last_vendor {
                    last_vendor = vendor.clone();
                    rows.push(Self::mhead(format!("  {vendor}")).into_any_element());
                }
                let (badge, color) = match self.plugin_state.get(&i) {
                    Some(PluginState::Ready { .. }) => ("●", Some(0x8fd0a0)),
                    Some(PluginState::Loading { .. }) => ("◌ …", Some(0xe0b050)),
                    Some(PluginState::Failed { .. }) => ("✕", Some(0xe06060)),
                    _ => ("", None),
                };
                let detail = match self.plugin_state.get(&i) {
                    Some(PluginState::Failed { phase, msg, .. }) => {
                        let phase = match *phase {
                            "host" => t("plugin.phase_host"),
                            "audio" => t("plugin.phase_audio"),
                            _ => t("plugin.phase_load"),
                        };
                        Some(format!("{phase}: {msg}"))
                    }
                    _ => None,
                };
                let selected = if kind == DestPick::Track {
                    has_track_dest && eff_dest == i
                } else {
                    def_dest == i
                };
                let path2 = path.clone();
                let row = Self::mi_inner(
                    ("plugin", i),
                    label,
                    badge,
                    color,
                    Some(selected),
                    false,
                    cx,
                    move |v, _e, _cx| {
                        let mut sh = crate::lock_shared(&v.shared);
                        if kind == DestPick::Track {
                            sh.track_dest.insert(v.sel_track, i);
                        } else {
                            sh.default_dest = i;
                        }
                        drop(sh);
                        v.persist();
                        v.ensure_plugin(i, true);
                        let _ = path2;
                    },
                );
                let row = if let Some(detail) = detail {
                    row.tooltip(move |_w, cx| {
                        let detail = detail.clone();
                        cx.new(|_| Tip(detail.into())).into()
                    })
                } else {
                    row
                };
                rows.push(row.into_any_element());
            }
        }
        rows
    }

    /// One dropdown row: optional check glyph, label, right-aligned shortcut.
    /// Clicking closes the whole menu and runs `f`. Dropdown rows clear the
    /// open cascade on hover; submenu leaf rows (`mi_leaf`) must NOT clear it,
    /// or hovering a submenu item unmounts its own submenu before the click.
    #[allow(clippy::too_many_arguments)] // GPUI builder plumbing, not logic
    fn mi_inner(
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        shortcut: impl Into<SharedString>,
        badge_color: Option<u32>,
        check: Option<bool>,
        clears_sub: bool,
        cx: &mut Context<Self>,
        f: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .flex()
            .items_center()
            .h(px(24.0))
            .px_2()
            .mx_1()
            .rounded_sm()
            .cursor_pointer()
            .hover(|s| s.bg(rgb(0x2f2f42)))
            .text_size(px(12.0))
            .text_color(rgb(0xd8d8e0))
            .whitespace_nowrap()
            .child(
                div()
                    .w(px(14.0))
                    .text_size(px(10.0))
                    .text_color(rgb(0x8fd0a0))
                    .child(if check == Some(true) { "✓" } else { "" }),
            )
            .child(div().flex_1().child(label.into()))
            .child(
                div()
                    .pl_2()
                    .text_color(rgb(badge_color.unwrap_or(0x666677)))
                    .text_size(px(10.0))
                    .child(shortcut.into()),
            )
            .on_mouse_move(cx.listener(move |v, _e: &MouseMoveEvent, _w, cx| {
                // leaving a submenu parent closes the cascade
                if clears_sub && v.open_sub.is_some() {
                    v.open_sub = None;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |v, e, w, cx| {
                cx.stop_propagation();
                v.open_menu = None;
                v.open_sub = None;
                let _ = e;
                f(v, w, cx);
                cx.notify();
            }))
    }

    /// Dropdown row — clears the open cascade when hovered.
    fn mi(
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        shortcut: &'static str,
        check: Option<bool>,
        cx: &mut Context<Self>,
        f: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        Self::mi_inner(id, label, shortcut, None, check, true, cx, f)
    }

    /// Submenu leaf row — must not clear the cascade it lives in.
    fn mi_leaf(
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        shortcut: &'static str,
        check: Option<bool>,
        cx: &mut Context<Self>,
        f: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        Self::mi_inner(id, label, shortcut, None, check, false, cx, f)
    }

    /// Dropdown row that cascades: hovering opens its submenu at the row's y.
    fn mi_sub(
        id: &'static str,
        label: &'static str,
        sub: Sub,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .flex()
            .items_center()
            .h(px(24.0))
            .px_2()
            .mx_1()
            .rounded_sm()
            .cursor_pointer()
            .hover(|s| s.bg(rgb(0x2f2f42)))
            .text_size(px(12.0))
            .text_color(rgb(0xd8d8e0))
            .whitespace_nowrap()
            .child(div().w(px(14.0)))
            .child(div().flex_1().child(label))
            .child(
                div()
                    .pl_2()
                    .text_color(rgb(0x666677))
                    .text_size(px(10.0))
                    .child("▸"),
            )
            .on_mouse_move(cx.listener(move |v, e: &MouseMoveEvent, _w, cx| {
                // Only hover-open when no cascade is up: once one is open, a
                // diagonal cursor path toward a submenu item would cross the
                // sibling rows and replace the submenu mid-flight (classic
                // "safe triangle" problem). Siblings still switch via click.
                let y = f32::from(e.position.y);
                if v.open_sub.is_none() {
                    v.open_sub = Some((sub, y));
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |v, e: &ClickEvent, _w, cx| {
                cx.stop_propagation();
                v.open_sub = Some((sub, f32::from(e.position().y)));
                cx.notify();
            }))
    }

    /// Dropdown separator line.
    fn msep() -> Div {
        div().h(px(1.0)).mx_2().my_1().bg(rgb(0x2a2a35))
    }
}

// --- toolbar helpers -------------------------------------------------------------

/// Tooltip bubble view.
struct Tip(SharedString);

impl Render for Tip {
    fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .bg(rgb(0x26262e))
            .border_1()
            .border_color(rgb(0x3c3c4a))
            .rounded_md()
            .shadow_lg()
            .text_size(px(11.0))
            .text_color(rgb(0xd8d8e0))
            .whitespace_nowrap()
            .child(self.0.clone())
    }
}

impl EditorView {
    /// 1px vertical separator between toolbar icon groups.
    fn vsep() -> Div {
        div().w(px(1.0)).h(px(20.0)).mx_1().bg(rgb(0x2a2a35))
    }

    /// Icon button: 26px square, tooltip, neutral gray icon.
    fn ibtn(
        id: &'static str,
        ic: &'static str,
        tip: &'static str,
        on: bool,
        cx: &mut Context<Self>,
        f: impl Fn(&mut Self, &ClickEvent, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .w(px(26.0))
            .h(px(26.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded_sm()
            .cursor_pointer()
            .bg(if on { rgb(0x2b3d4f) } else { rgb(BG_PANEL) })
            .border_1()
            .border_color(if on { rgb(0x3d5a75) } else { rgb(BG_PANEL) })
            .hover(|s| s.bg(rgb(0x2f2f42)))
            .tooltip(move |_w, cx| cx.new(|_| Tip(tip.into())).into())
            .child(icon(ic, 16.0, if on { ACCENT } else { 0x9a9ab0 }))
            .on_click(cx.listener(move |v, e, _w, cx| {
                f(v, e, cx);
                cx.notify();
            }))
    }

    /// Icon button whose active state gets a custom accent color.
    fn ibtn_c(
        id: &'static str,
        ic: &'static str,
        tip: &'static str,
        on: bool,
        accent: u32,
        cx: &mut Context<Self>,
        f: impl Fn(&mut Self, &ClickEvent, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .w(px(26.0))
            .h(px(26.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded_sm()
            .cursor_pointer()
            .bg(if on { rgb(0x2b3d4f) } else { rgb(BG_PANEL) })
            .border_1()
            .border_color(if on { rgb(0x3d5a75) } else { rgb(BG_PANEL) })
            .hover(|s| s.bg(rgb(0x2f2f42)))
            .tooltip(move |_w, cx| cx.new(|_| Tip(tip.into())).into())
            .child(icon(ic, 16.0, if on { accent } else { 0x9a9ab0 }))
            .on_click(cx.listener(move |v, e, _w, cx| {
                f(v, e, cx);
                cx.notify();
            }))
    }
}
