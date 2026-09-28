//! Rendering: impl Render for EditorView (toolbar, track column, ruler,
//! piano roll canvas, lane, event list) plus the chip/button helpers.
//! Private items are visible here because this is a child module of the
//! crate root where EditorView is defined.
#![allow(unused_imports)]

use crate::*;
use crate::i18n::t;
use commands::UndoStack;
use document::{Document, Event as DocEvent, EventId, Note, Op};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::*;
use mcp_server::{Shared, SharedDoc};
use smf_core::EventKind;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex, Weak};
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

        // advance playhead / auto-stop (looping happens inside the
        // playback thread; reaching this branch means playback ended)
        if let Some(p) = &self.playback {
            self.play_us = p.position_us();
            if !p.is_running() {
                self.playback = None;
                self.play_us = 0;
            }
        }
        let (playhead_tick, title, dirty, n_diags, track_names, dests, eff_dest, loop_en, met_en, muted_set, soloed_set, has_track_dest, markers, tempo0, sig, track_chs) = {
            let sh = self.shared.lock().unwrap();
            let hint = self.enc_override.or(sh.doc.text_encoding_hint());
            let markers: Vec<(u64, String)> = sh
                .doc
                .tracks
                .iter()
                .flat_map(|t| &t.events)
                .filter_map(|e| match &e.kind {
                    EventKind::Meta { meta_type, data }
                        if matches!(*meta_type, 0x05 | 0x06) =>
                    {
                        Some((e.tick, smf_core::decode_text(data, hint)))
                    }
                    _ => None,
                })
                .collect();
            let tempo0 = sh
                .doc
                .tempo_map
                .points()
                .first()
                .map(|(_, mpq, _)| 60_000_000.0 / *mpq as f64)
                .unwrap_or(120.0);
            let sig = sh
                .doc
                .tracks
                .first()
                .and_then(|t| {
                    t.events.iter().find_map(|e| match &e.kind {
                        EventKind::Meta {
                            meta_type: 0x58,
                            data,
                        } if data.len() >= 2 => {
                            Some(format!("{}/{}", data[0], 1u8 << data[1]))
                        }
                        _ => None,
                    })
                })
                .unwrap_or_else(|| "4/4".into());
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
                sh.dests.clone(),
                sh.dest_of(self.sel_track),
                sh.loop_enabled,
                sh.metronome,
                sh.muted.clone(),
                sh.soloed.clone(),
                sh.track_dest.contains_key(&self.sel_track),
                markers,
                tempo0,
                sig,
                sh.doc.tracks.iter().map(|t| t.out_channel).collect::<Vec<u8>>(),
            )
        };
        let ppq = self.ppq();
        let pos = {
            let bar = playhead_tick / (ppq * 4) + 1;
            let beat = (playhead_tick % (ppq * 4)) / ppq + 1;
            format!("{bar}.{beat}.{:>3}", playhead_tick % ppq)
        };
        let port_label = {
            let name = dests
                .get(eff_dest)
                .map(|(l, _)| l.clone())
                .unwrap_or_else(|| t("status.no_port").to_string());
            let mark = if has_track_dest { "" } else { "*" };
            format!("T{}{} â–¸ {}", self.sel_track + 1, mark, name)
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
            dests.get(eff_dest).map(|(_, d)| d),
            Some(output::Destination::Plugin { .. })
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
                    .id("rec")
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .bg(if self.rec.is_some() {
                        rgb(0x7a2a2a)
                    } else {
                        rgb(0x2a2a35)
                    })
                    .hover(|s| s.bg(rgb(0x6a3a3a)))
                    .text_color(rgb(if self.rec.is_some() {
                        0xff8c8c
                    } else {
                        0x8f8fb0
                    }))
                    .child("â—")
                    .on_click(cx.listener(|v, _e, _w, cx| {
                        v.toggle_record();
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id("loop")
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .bg(if loop_en {
                        rgb(0x3a5c2a)
                    } else {
                        rgb(0x2a2a35)
                    })
                    .hover(|s| s.bg(rgb(0x3a3a48)))
                    .text_color(rgb(if loop_en {
                        0xb4ff8c
                    } else {
                        0x8f8fb0
                    }))
                    .child("loop")
                    .on_click(cx.listener(|v, _e, _w, cx| {
                        let mut sh = v.shared.lock().unwrap();
                        sh.loop_enabled = !sh.loop_enabled;
                        drop(sh);
                        v.persist();
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id("met")
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .bg(if met_en {
                        rgb(0x3a5c2a)
                    } else {
                        rgb(0x2a2a35)
                    })
                    .hover(|s| s.bg(rgb(0x3a3a48)))
                    .text_color(rgb(if met_en {
                        0xb4ff8c
                    } else {
                        0x8f8fb0
                    }))
                    .child(t("transport.met"))
                    .on_click(cx.listener(|v, _e, _w, cx| {
                        let mut sh = v.shared.lock().unwrap();
                        sh.metronome = !sh.metronome;
                        drop(sh);
                        v.persist();
                        cx.notify();
                    })),
            )
            .child(div().text_color(rgb(0x8f8fb0)).font_family("Cascadia Mono").child(pos))
            .child(div().w(px(8.0)))
            // tempo + meter editing
            .child(
                div()
                    .text_color(rgb(0x8f8fb0))
                    .text_size(px(11.0))
                    .child(format!("{tempo0:.0}â™©")),
            )
            .child(Self::chip("bpm-dn", "-", cx, |v, e: &ClickEvent, cx| {
                v.bump_tempo(if e.modifiers().shift { -10.0 } else { -1.0 });
                cx.notify();
            }))
            .child(Self::chip("bpm-up", "+", cx, |v, e: &ClickEvent, cx| {
                v.bump_tempo(if e.modifiers().shift { 10.0 } else { 1.0 });
                cx.notify();
            }))
            .child(Self::chip("sig", sig.clone(), cx, |v, _e, cx| {
                v.cycle_time_sig();
                cx.notify();
            }))
            // selection ops (whole selected track when nothing is selected)
            .child(Self::chip("quant", "quant", cx, |v, _e, cx| {
                let g = v.ppq() / 4;
                v.apply_region_op("quantize", move |d, tr, f, to| {
                    d.quantize_ops(tr, f, to, g, 100)
                });
                cx.notify();
            }))
            .child(Self::chip("tr-up", "tr+", cx, |v, _e, cx| {
                v.apply_region_op("transpose +1", |d, t, f, to| d.transpose_ops(t, f, to, 1));
                cx.notify();
            }))
            .child(Self::chip("tr-dn", "tr-", cx, |v, _e, cx| {
                v.apply_region_op("transpose -1", |d, t, f, to| d.transpose_ops(t, f, to, -1));
                cx.notify();
            }))
            .child(Self::chip("vel-up", "vel+", cx, |v, _e, cx| {
                v.apply_region_op("vel Ã—1.25", |d, t, f, to| d.scale_velocity_ops(t, f, to, 1.25));
                cx.notify();
            }))
            .child(Self::chip("vel-dn", "vel-", cx, |v, _e, cx| {
                v.apply_region_op("vel Ã—0.8", |d, t, f, to| d.scale_velocity_ops(t, f, to, 0.8));
                cx.notify();
            }))
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
                        {
                            let mut sh = v.shared.lock().unwrap();
                            if !sh.dests.is_empty() {
                                // cycles the SELECTED track's assignment through
                                // [inherit default] -> dest 0..n -> inherit
                                let n = sh.dests.len();
                                let tr = v.sel_track;
                                match sh.track_dest.get(&tr).copied() {
                                    None => {
                                        sh.track_dest.insert(tr, 0);
                                    }
                                    Some(d) if d + 1 < n => {
                                        sh.track_dest.insert(tr, d + 1);
                                    }
                                    Some(_) => {
                                        sh.track_dest.remove(&tr);
                                    }
                                }
                                if let Some(&d) = sh.track_dest.get(&tr) {
                                    sh.default_dest = d;
                                }
                            }
                        }
                        v.persist();
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
                        v.persist();
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
                        let (d, path) = {
                            let sh = v.shared.lock().unwrap();
                            let d = sh.dest_of(v.sel_track);
                            let path = sh.dests.get(d).and_then(|(_, dd)| match dd {
                                output::Destination::Plugin { plugin_path } => {
                                    Some(PathBuf::from(plugin_path))
                                }
                                _ => None,
                            });
                            (d, path)
                        };
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
            .child(Self::chip("rename", "rename", cx, |v, _e, cx| {
                let name = v.input.read(cx).value().to_string();
                let ops = {
                    let mut sh = v.shared.lock().unwrap();
                    sh.doc.set_track_name_ops(v.sel_track, &name)
                };
                v.apply_tx("set track name", ops);
                cx.notify();
            }))
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
                                {
                                    let mut sh = v.shared.lock().unwrap();
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
                                    let mut sh = v.shared.lock().unwrap();
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
                                    let mut sh = v.shared.lock().unwrap();
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
                            .child(format!("c{}", track_chs.get(i).copied().unwrap_or(0) + 1)),
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
                                cx.notify();
                            }),
                        ),
                )
                // marker/lyric strip â€” meta 0x06/0x05 shown at their tick
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
                        // a marquee that never left its anchor = click â†’ insert
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
                                    v.persist();
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
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, w, cx| {
                // typing in the track-name field must not trigger editor keys
                if this.input.read(cx).focus_handle(cx).is_focused(w) {
                    return;
                }
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
