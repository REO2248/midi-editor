//! UI chrome: menu/dropdown builders (`mi*`, `dest_rows`, `mhead`,
//! `row_el`), the lane/controller and event-properties panels, toolbar
//! icon buttons (`ibtn*`, `vsep`) and the tooltip view. Render-time only —
//! these helpers build elements; all state lives on `EditorView` in the
//! crate root and document edits still go through `apply_tx`.

use super::*;
use crate::icons::icon;
use crate::menu::LeafRow;
use crate::render::LANE_KEY_COLORS;
use crate::theme::metrics;
use gpui_kit::component::input::Input;
use gpui_kit::prelude::FluentBuilder;

impl EditorView {
    /// Section header row inside a dropdown.
    pub(crate) fn mhead(label: impl Into<SharedString>) -> MenuRow {
        MenuRow::Head(label.into())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn dest_rows(
        &self,
        kind: DestPick,
        dests: &[(String, midi_io::Destination)],
        port_present: &std::collections::HashSet<(String, usize)>,
        eff_dest: usize,
        def_dest: usize,
        has_track_dest: bool,
        cx: &mut Context<Self>,
    ) -> Vec<MenuRow> {
        let mut rows = Vec::new();
        // current explicit metronome routing (None = follows the document
        // default destination) — #137
        let met_sel = crate::lock_shared(&self.shared).met_dest;
        if kind == DestPick::Metronome {
            rows.push(Self::mi_leaf(
                "metdest.default",
                t("metdest.default"),
                "",
                Some(met_sel.is_none()),
                cx,
                |v, _e, _cx| {
                    crate::lock_shared(&v.shared).met_dest = None;
                    v.persist();
                    v.refresh_live_schedule();
                },
            ));
            rows.push(Self::msep());
        }
        if kind == DestPick::Track {
            rows.push(Self::mi_leaf(
                "dest.default",
                t("track.default_dest"),
                "",
                Some(!has_track_dest),
                cx,
                |v, _e, _cx| {
                    crate::lock_shared(&v.shared)
                        .track_dest
                        .remove(&v.sel_track);
                    // a preview routed to the old destination must stop
                    v.audition_off();
                    v.persist();
                },
            ));
            rows.push(Self::msep());
        }
        rows.push(Self::mhead(t("output.cat_midi")));
        let midi: Vec<(usize, String)> = dests
            .iter()
            .enumerate()
            .filter_map(|(i, (name, d))| {
                matches!(d, output::Destination::MidiPort { .. }).then_some((i, name.clone()))
            })
            .collect();
        if midi.is_empty() {
            rows.push(Self::mi_dis(
                "dest.noports",
                t("output.no_ports"),
                "",
                None,
                cx,
                |_v, _e, _cx| {},
            ));
        } else {
            for (i, label) in midi {
                let selected = match kind {
                    DestPick::Track => has_track_dest && eff_dest == i,
                    DestPick::Default => def_dest == i,
                    DestPick::Metronome => met_sel == Some(i),
                };
                // an offline port keeps its row + assignment — marked so it
                // is not confused with a live endpoint
                let offline = matches!(
                    &dests[i].1,
                    output::Destination::MidiPort { port_name, ord }
                        if !port_present.contains(&(port_name.clone(), *ord))
                );
                let label: SharedString = if offline {
                    format!("{label} {}", t("output.offline")).into()
                } else {
                    label.into()
                };
                rows.push(Self::mi_leaf(
                    ("dest", i),
                    label,
                    "",
                    Some(selected),
                    cx,
                    move |v, _e, _cx| {
                        let mut sh = crate::lock_shared(&v.shared);
                        match kind {
                            DestPick::Track => {
                                sh.track_dest.insert(v.sel_track, i);
                            }
                            DestPick::Default => sh.default_dest = i,
                            DestPick::Metronome => sh.met_dest = Some(i),
                        }
                        drop(sh);
                        v.audition_off();
                        v.persist();
                        // destination change reaches the running pass
                        // (events + sinks) — #140
                        v.refresh_live_schedule();
                    },
                ));
            }
        }
        rows.push(Self::msep());
        let plugins: Vec<(usize, String, String, String)> = dests
            .iter()
            .enumerate()
            .filter_map(|(i, (name, d))| {
                let output::Destination::Plugin { plugin_path, .. } = d else {
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
        rows.push(Self::mhead(format!(
            "{} ({})",
            t("output.cat_vst3"),
            plugins.len()
        )));
        if plugins.is_empty() {
            if self.scan_rx.is_some() {
                rows.push(Self::mi_dis(
                    "dest.scanning",
                    t("status.scanning"),
                    "",
                    None,
                    cx,
                    |_v, _e, _cx| {},
                ));
            } else {
                rows.push(Self::mi_dis(
                    "dest.noplugins",
                    t("output.no_plugins"),
                    "",
                    None,
                    cx,
                    |_v, _e, _cx| {},
                ));
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
                    rows.push(Self::mhead(format!("  {vendor}")));
                }
                let (badge, color) = match self.plugin_state.get(&i) {
                    Some(PluginState::Ready { .. }) => ("●", Some(0x8fd0a0)),
                    Some(PluginState::Loading { .. }) => ("◌ …", Some(0xe0b050)),
                    Some(PluginState::Failed { .. }) => ("✕", Some(theme::current().danger)),
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
                let selected = match kind {
                    DestPick::Track => has_track_dest && eff_dest == i,
                    DestPick::Default => def_dest == i,
                    DestPick::Metronome => met_sel == Some(i),
                };
                let path2 = path.clone();
                rows.push(Self::mi_plugin(
                    ("plugin", i),
                    label,
                    badge,
                    color,
                    Some(selected),
                    detail,
                    cx,
                    move |v, _e, _cx| {
                        let mut sh = crate::lock_shared(&v.shared);
                        match kind {
                            DestPick::Track => {
                                sh.track_dest.insert(v.sel_track, i);
                            }
                            DestPick::Default => sh.default_dest = i,
                            DestPick::Metronome => sh.met_dest = Some(i),
                        }
                        drop(sh);
                        v.audition_off();
                        v.persist();
                        v.ensure_plugin(i, true);
                        let _ = path2;
                    },
                ));
            }
        }
        rows
    }

    /// One bottom lane: a header strip (mode chip, key filter, add/remove,
    /// collapse — and the resize handle) plus the body canvas when expanded.
    #[allow(clippy::too_many_arguments)] // render plumbing, all cheap copies
    pub(crate) fn lane_panel(
        &mut self,
        li: usize,
        cfg: LaneCfg,
        area: FocusArea,
        play_tick: u64,
        playing: bool,
        scroll_x: f32,
        zoom: f32,
        track_names: Vec<String>,
        scale_factor: f32,
        cx: &mut Context<Self>,
    ) -> Div {
        // one bounds cell per lane canvas, for hit-testing
        while self.lane_bounds.len() <= li {
            self.lane_bounds.push(Rc::new(Cell::new(Bounds::new(
                point(px(0.0), px(0.0)),
                size(px(0.0), px(0.0)),
            ))));
        }
        let cell = self.lane_bounds[li].clone();
        let mode = cfg.mode;
        let pkey = cfg.poly_key;
        let lane_events = self.lane_events_cached(mode, pkey);
        let lane_sel_track = self.sel_track;
        let th = self.theme;
        // this lane's drag preview — (mode, on_id, dkey); the full drag
        // tuple (adds a/b anchors) drives the marquee band + insert ghost
        let drag_v = self
            .drag
            .as_ref()
            .filter(|d| d.lane == li)
            .map(|d| (d.mode, d.on_id, d.dkey));
        let lane_drag = self.drag.as_ref().filter(|d| d.lane == li).and_then(|d| {
            matches!(d.mode, DragMode::LaneEvent | DragMode::LaneMarquee).then_some((
                d.mode, d.on_id, d.a_tick, d.a_key, d.b_tick, d.b_key, d.dkey,
            ))
        });
        let hit_cell = cell.clone();
        let lane = canvas(
            move |bounds, _window, _cx| {
                hit_cell.set(bounds);
            },
            {
                let lane_notes = self.notes.clone();
                let lane_selection = self.selection.clone();
                let lane_sel = self.lane_sel.clone();
                let lane_events = lane_events.clone();
                move |bounds, _state, window, _cx| {
                    if playing {
                        window.request_animation_frame();
                    }
                    let h: f32 = bounds.size.height.into();
                    let w: f32 = bounds.size.width.into();
                    let vrange = mode.vrange();
                    // only the visible tick window is walked per frame —
                    // dense controller data stays interactive with several
                    // lanes stacked (lane_events is sorted by tick)
                    let t0 = (scroll_x / zoom).max(0.0) as i64;
                    let t1 = t0 + (w / zoom).max(0.0) as i64 + 2;
                    match mode {
                        LaneMode::Velocity => {
                            for n in lane_notes.iter().filter(|n| n.track == lane_sel_track) {
                                let x = bounds.origin.x + px(n.start_tick as f32 * zoom - scroll_x);
                                if x < bounds.origin.x || x > bounds.origin.x + px(w) {
                                    continue;
                                }
                                let mut vel = n.vel as f32 / 127.0;
                                if let Some((DragMode::Velocity, d_on, dkey)) = drag_v {
                                    if d_on == n.on_id {
                                        vel = (dkey as f32 / 127.0).clamp(0.0, 1.0);
                                    }
                                }
                                let bh = px((h - 6.0) * vel);
                                let y = bounds.origin.y + px(h) - bh - px(3.0);
                                let c = if lane_selection.contains(&n.on_id) {
                                    theme::current().sel
                                } else {
                                    th.track_colors[n.track % th.track_colors.len()]
                                };
                                window.paint_quad(fill(
                                    Bounds::new(point(x, y), size(px(2.0), bh)),
                                    rgb(c),
                                ));
                            }
                        }
                        _ => {
                            // stepped automation line: dot + run to next point
                            // (all-keys poly-AT draws dots only — a stepped line
                            // across keys would be meaningless)
                            let stepped = !(mode == LaneMode::PolyAT && pkey.is_none());
                            let lo = lane_events
                                .partition_point(|e| (e.1 as i64) < t0)
                                .saturating_sub(1);
                            let hi = lane_events.partition_point(|e| (e.1 as i64) <= t1).max(lo);
                            let mut prev: Option<(Pixels, Pixels)> = None;
                            for (id, tick, val, key) in lane_events[lo..hi].iter() {
                                let mut v = *val;
                                if let Some((DragMode::LaneEvent, d_on, dkey)) = drag_v {
                                    if d_on == *id {
                                        v = dkey.clamp(0, vrange as i32);
                                    }
                                }
                                let x = bounds.origin.x + px(*tick as f32 * zoom - scroll_x);
                                let y = bounds.origin.y
                                    + px((h - 4.0) * (1.0 - v as f32 / vrange) + 2.0);
                                if stepped {
                                    if let Some((px_, py_)) = prev {
                                        // horizontal run at previous level,
                                        // then a vertical connector at this
                                        // event's x
                                        window.paint_quad(fill(
                                            Bounds::new(point(px_, py_), size(x - px_, px(1.0))),
                                            rgba(theme::current().lane_fill),
                                        ));
                                        window.paint_quad(fill(
                                            Bounds::new(
                                                point(x, y.min(py_)),
                                                size(px(1.0), (y - py_).abs().max(px(1.0))),
                                            ),
                                            rgba(theme::current().lane_fill),
                                        ));
                                    }
                                    prev = Some((x, y));
                                }
                                let c = if lane_sel.contains(id) {
                                    theme::current().sel
                                } else if mode == LaneMode::PolyAT && pkey.is_none() && *key >= 0 {
                                    // key-aware tint: hue family per key group
                                    LANE_KEY_COLORS[(*key as usize / 16) % LANE_KEY_COLORS.len()]
                                } else {
                                    theme::current().lane
                                };
                                window.paint_quad(fill(
                                    Bounds::new(
                                        point(x - px(2.0), y - px(2.0)),
                                        size(px(4.0), px(4.0)),
                                    ),
                                    rgb(c),
                                ));
                            }
                            // drag insert ghost
                            if let Some((DragMode::LaneEvent, 0, a_tick, _, _, _, dkey)) = lane_drag
                            {
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
                                    rgb(theme::current().sel),
                                ));
                            }
                        }
                    }
                    // lane rubber band (tick × value box)
                    if let Some((DragMode::LaneMarquee, _, a_t, a_v, b_t, b_v, _)) = lane_drag {
                        let (t0, t1) = (a_t.min(b_t), a_t.max(b_t));
                        let (v0, v1) = (a_v.min(b_v), a_v.max(b_v));
                        let x0 = bounds.origin.x + px(t0 as f32 * zoom - scroll_x);
                        let x1 = bounds.origin.x + px(t1 as f32 * zoom - scroll_x);
                        let y0 = bounds.origin.y + px((h - 4.0) * (1.0 - v1 as f32 / vrange) + 2.0);
                        let y1 = bounds.origin.y + px((h - 4.0) * (1.0 - v0 as f32 / vrange) + 2.0);
                        window.paint_quad(fill(
                            Bounds::new(point(x0, y0), size(x1 - x0, (y1 - y0).max(px(1.0)))),
                            rgba(theme::current().sel_fill),
                        ));
                    }
                    // shared time cursor — the same playhead x in every lane
                    let hx = bounds.origin.x + px(play_tick as f32 * zoom - scroll_x);
                    if hx >= bounds.origin.x && hx <= bounds.origin.x + px(w) {
                        window.paint_quad(fill(
                            Bounds::new(point(hx, bounds.origin.y), size(px(1.0), px(h))),
                            rgba(theme::current().ok_fill),
                        ));
                    }
                }
            },
        );

        let header = div()
            .id(("lane-hdr", li))
            .h(px(LANE_HDR))
            .w_full()
            .flex()
            .items_center()
            .px_1()
            .gap_1()
            .bg(rgb(th.bg_bar))
            .border_t_1()
            .border_color(rgb(if li == 0 && area == FocusArea::Lane {
                th.accent
            } else {
                th.border
            }))
            .cursor_pointer()
            // the header is the resize handle — drag it vertically; the
            // chips stop propagation so this only fires on the strip itself
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, _w, cx| {
                    this.mouse_pos = Some(ev.position);
                    this.lane_focus = li;
                    let h0 = this.lanes.get(li).map(|c| c.h).unwrap_or(LANE_H);
                    this.drag = Some(Drag {
                        mode: DragMode::LaneResize,
                        on_id: 0,
                        off_id: None,
                        track: 0,
                        orig_start: (h0 * 100.0) as u64,
                        orig_end: None,
                        orig_key: 0,
                        dtick: 0,
                        dkey: 0,
                        a_tick: f32::from(ev.position.y) as i64,
                        a_key: 0,
                        b_tick: 0,
                        b_key: 0,
                        aud_vel: 0,
                        aud_ch: 0,
                        lane: li,
                    });
                    cx.notify();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _ev: &MouseUpEvent, _w, cx| this.commit_drag(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _ev: &MouseUpEvent, _w, cx| this.commit_drag(cx)),
            )
            // lane-mode chip: cycles Vel -> CC1 -> CC7 -> CC10 -> CC11 ->
            // CC64 -> PB -> CAT -> PAT -> Vel on the focused lane
            .child(
                div()
                    .id(("lane-mode", li))
                    .test_support()
                    .role(Role::Button)
                    .aria_label(tf("a11y.lane_mode", &[("mode", mode.label().as_str())]))
                    .px_1()
                    .rounded_sm()
                    .bg(rgb(th.bg_chip))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(th.bg_chip_hover)))
                    .text_size(px(9.0))
                    .text_color(rgb(theme::current().accent))
                    .child(mode.label())
                    // swallow the mouse_down so a chip press can't start a
                    // lane insert-drag or a header resize underneath it
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_v, _e, _w, cx| cx.stop_propagation()),
                    )
                    .on_click(cx.listener(move |v, _e: &ClickEvent, _w, cx| {
                        cx.stop_propagation();
                        v.lane_focus = li;
                        let m = v.lane_mode().cycle();
                        v.set_lane(m, cx);
                    })),
            )
            .children(
                // poly-AT key filter chip — only on PAT lanes; cycles
                // all -> keys present -> all (shift steps backwards)
                (mode == LaneMode::PolyAT).then(|| {
                    div()
                        .id(("lane-key", li))
                        .px_1()
                        .rounded_sm()
                        .bg(rgb(th.bg_chip))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(th.bg_chip_hover)))
                        .text_size(px(9.0))
                        .text_color(rgb(theme::current().accent))
                        .child(match pkey {
                            Some(k) => format!("k{k}"),
                            None => "k*".to_string(),
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_v, _e, _w, cx| cx.stop_propagation()),
                        )
                        .on_click(cx.listener(move |v, e: &ClickEvent, _w, cx| {
                            cx.stop_propagation();
                            v.lane_focus = li;
                            v.cycle_poly_key(e.modifiers().shift, cx);
                        }))
                        .into_any_element()
                }),
            )
            .child(div().flex_1())
            .children(
                (li + 1 == self.lanes.len() && self.lanes.len() < LANES_MAX).then(|| {
                    div()
                        .id("lane-add")
                        .px_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(th.bg_hover)))
                        .text_size(px(9.0))
                        .text_color(rgb(theme::current().accent))
                        .child("+")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_v, _e, _w, cx| cx.stop_propagation()),
                        )
                        .on_click(cx.listener(|v, _e: &ClickEvent, _w, cx| {
                            cx.stop_propagation();
                            v.add_lane(cx);
                        }))
                }),
            )
            .child(
                div()
                    .id(("lane-collapse", li))
                    .px_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(th.bg_hover)))
                    .text_size(px(9.0))
                    .text_color(rgb(theme::current().accent))
                    .child(if cfg.collapsed { "▸" } else { "▾" })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_v, _e, _w, cx| cx.stop_propagation()),
                    )
                    .on_click(cx.listener(move |v, _e: &ClickEvent, _w, cx| {
                        cx.stop_propagation();
                        if let Some(c) = v.lanes.get_mut(li) {
                            c.collapsed = !c.collapsed;
                        }
                        v.persist();
                        cx.notify();
                    })),
            )
            .children((self.lanes.len() > 1).then(|| {
                div()
                    .id(("lane-del", li))
                    .px_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(th.bg_hover)))
                    .text_size(px(9.0))
                    .text_color(rgb(theme::current().accent))
                    .child("×")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_v, _e, _w, cx| cx.stop_propagation()),
                    )
                    .on_click(cx.listener(move |v, _e: &ClickEvent, _w, cx| {
                        cx.stop_propagation();
                        v.lane_focus = li;
                        v.remove_lane(cx);
                    }))
            }));

        let body = div()
            .id(("lane", li))
            .test_support()
            .role(Role::Group)
            .aria_label(tf("a11y.lane", &[("mode", mode.label().as_str())]))
            .a11y_synthetic_children({
                let cell = cell.clone();
                let lane_events = lane_events.clone();
                let notes = self.notes.clone();
                let sel_track = lane_sel_track;
                let pos = self.doc(|d| d.position_format_for(self.sel_track));
                move |b| {
                    a11y::LaneA11y {
                        bounds: cell.get(),
                        scale: scale_factor,
                        scroll_x,
                        zoom,
                        mode,
                        pos,
                        events: lane_events,
                        notes,
                        sel_track,
                        track_names,
                        drag: drag_v.map(|(m, on, dkey)| (m, on, 0, dkey)),
                    }
                    .build(b);
                }
            })
            .h(px(cfg.h))
            .w_full()
            .bg(rgb(th.bg_lane))
            .relative()
            .child(lane.size_full())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener({
                    let cell = cell.clone();
                    move |this: &mut Self, ev: &MouseDownEvent, w, cx| {
                    w.focus(&this.lane_fh, cx);
                    this.mouse_pos = Some(ev.position);
                    this.lane_focus = li;
                    let b = cell.get();
                    let x = f32::from(ev.position.x) - f32::from(b.origin.x);
                    let y = f32::from(ev.position.y) - f32::from(b.origin.y);
                    let tick = ((x + this.scroll_x) / this.zoom).max(0.0) as u64;
                    this.cursor_tick = tick; // share the roll's edit cursor
                    let h = f32::from(b.size.height);
                    let vrange = mode.vrange();
                    let val = ((1.0 - y / h) * vrange) as i32;
                    // shift+drag = rubber-band select inside the lane
                    if ev.modifiers.shift {
                        this.drag = Some(Drag {
                            mode: DragMode::LaneMarquee,
                            on_id: 0,
                            off_id: None,
                            track: this.sel_track,
                            orig_start: 0,
                            orig_end: None,
                            orig_key: 0,
                            dtick: 0,
                            dkey: 0,
                            a_tick: tick as i64,
                            a_key: val,
                            b_tick: tick as i64,
                            b_key: val,
                            aud_vel: 0,
                            aud_ch: 0,
                            lane: li,
                        });
                        cx.notify();
                        return;
                    }
                    match mode {
                        LaneMode::Velocity => {
                            // the note bar under the cursor (within ~6px) —
                            // a click on empty lane space must not edit some
                            // distant note
                            let bar_dx =
                                |n: &document::Note| n.start_tick as f32 * this.zoom - x;
                            if let Some(n) = this
                                .notes
                                .iter()
                                .filter(|n| n.track == this.sel_track)
                                .min_by(|a, b| bar_dx(a).abs().total_cmp(&bar_dx(b).abs()))
                                .filter(|n| bar_dx(n).abs() <= 6.0)
                            {
                                this.selection = BTreeSet::from([n.on_id]);
                                this.sel_events.clear();
                                this.drag = Some(Drag {
                                    mode: DragMode::Velocity,
                                    on_id: n.on_id,
                                    off_id: n.off_id,
                                    track: n.track,
                                    orig_start: n.start_tick,
                                    orig_end: n.end_tick,
                                    orig_key: n.key,
                                    dtick: 0,
                                    dkey: val.clamp(1, 127),
                                    a_tick: 0,
                                    a_key: 0,
                                    b_tick: 0,
                                    b_key: 0,
                                    aud_vel: 0,
                                    aud_ch: 0,
                                    lane: li,
                                });
                            }
                        }
                        _ => {
                            // CC/PB/AT: grab the nearest lane event within
                            // ~10px, else insert a new one at the click and
                            // drag it
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
                                                    LaneMode::ChanAT => status & 0xF0 == 0xD0,
                                                    LaneMode::PolyAT => status & 0xF0 == 0xA0 && (pkey.is_none() || pkey == Some(data[0])),
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
                                aud_vel: 0,
                                aud_ch: 0,
                                lane: li,
                            });
                        }
                    }
                    cx.notify();
                    }
                }),
            )
            // right-click a lane point deletes it
            .on_mouse_down(
                MouseButton::Right,
                cx.listener({
                    let cell = cell.clone();
                    move |this: &mut Self, ev: &MouseDownEvent, _w, cx| {
                    if !mode.is_event_lane() {
                        return;
                    }
                    let b = cell.get();
                    let x = f32::from(ev.position.x) - f32::from(b.origin.x);
                    let tick = ((x + this.scroll_x) / this.zoom).max(0.0) as u64;
                    let found = this
                        .lane_events_cached(mode, pkey)
                        .iter()
                        .min_by_key(|e| (e.1 as i64 - tick as i64).abs())
                        .filter(|e| ((e.1 as f32 - tick as f32) * this.zoom).abs() <= 10.0)
                        .map(|e| e.0);
                    if let Some(id) = found {
                        let ops = {
                            let mut sh = crate::lock_shared(&this.shared);
                            sh.doc.remove_events_ops(&[id])
                        };
                        this.lane_sel.remove(&id);
                        this.apply_tx("delete lane event", ops);
                    }
                    cx.notify();
                    }
                }),
            )
            // drag deltas are forwarded by the root mouse-move listener, so
            // a lane drag keeps tracking even when the cursor crosses into
            // the ruler or roll
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _ev: &MouseUpEvent, _w, cx| this.commit_drag(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _ev: &MouseUpEvent, _w, cx| this.commit_drag(cx)),
            );

        let mut panel = div().w_full().flex_col().child(header);
        if !cfg.collapsed {
            panel = panel.child(body);
        }
        panel
    }

    /// Dropdown row — clears the open cascade when hovered.
    pub(crate) fn mi(
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        shortcut: impl Into<SharedString>,
        check: Option<bool>,
        _cx: &mut Context<Self>,
        f: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> MenuRow {
        MenuRow::Leaf(LeafRow {
            id: id.into(),
            label: label.into(),
            shortcut: shortcut.into(),
            check,
            badge_color: None,
            detail: None,
            enabled: true,
            act: std::rc::Rc::new(f),
        })
    }

    /// Menu row for a registry command — label, effective shortcut, enabled
    /// state, and action all come from the command table (one definition).
    pub(crate) fn mi_cmd(
        &self,
        id: &'static str,
        check: Option<bool>,
        _cx: &mut Context<Self>,
    ) -> MenuRow {
        let c = cmd::find(id).expect("unknown command id");
        let enabled = c.enabled.map(|f| f(self)).unwrap_or(true);
        MenuRow::Leaf(LeafRow {
            id: c.id.into(),
            label: cmd::label(c),
            shortcut: self.keys.shortcut_label(c.id).into(),
            check,
            badge_color: None,
            detail: None,
            enabled,
            act: std::rc::Rc::new(move |v, w, cx| (c.act)(v, w, cx)),
        })
    }

    /// Submenu leaf row — must not clear the cascade it lives in.
    pub(crate) fn mi_leaf(
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        shortcut: impl Into<SharedString>,
        check: Option<bool>,
        cx: &mut Context<Self>,
        f: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> MenuRow {
        Self::mi(id, label, shortcut, check, cx, f)
    }

    /// Informational row ("no MIDI ports", "scanning…") — dimmed, not
    /// keyboard-selectable and not activatable.
    pub(crate) fn mi_dis(
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        shortcut: &'static str,
        check: Option<bool>,
        _cx: &mut Context<Self>,
        f: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> MenuRow {
        MenuRow::Leaf(LeafRow {
            id: id.into(),
            label: label.into(),
            shortcut: shortcut.into(),
            check,
            badge_color: None,
            detail: None,
            enabled: false,
            act: std::rc::Rc::new(f),
        })
    }

    /// Plugin destination row — status badge + failure-detail tooltip.
    #[allow(clippy::too_many_arguments)] // GPUI builder plumbing, not logic
    pub(crate) fn mi_plugin(
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        badge: &'static str,
        badge_color: Option<u32>,
        check: Option<bool>,
        detail: Option<String>,
        _cx: &mut Context<Self>,
        f: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> MenuRow {
        MenuRow::Leaf(LeafRow {
            id: id.into(),
            label: label.into(),
            shortcut: badge.into(),
            check,
            badge_color,
            detail: detail.map(Into::into),
            enabled: true,
            act: std::rc::Rc::new(f),
        })
    }

    /// Dropdown row that cascades into `sub`.
    pub(crate) fn mi_sub(
        id: &'static str,
        label: impl Into<SharedString>,
        sub: Sub,
        _cx: &mut Context<Self>,
    ) -> MenuRow {
        MenuRow::Sub {
            id: id.into(),
            label: label.into(),
            sub,
        }
    }

    /// Dropdown separator line.
    pub(crate) fn msep() -> MenuRow {
        MenuRow::Sep
    }

    /// Render one menu model row. `i` is its index in `menu_rows` (dropdown)
    /// or `sub_rows` (cascade); `selected` marks the keyboard selection which
    /// mouse hover also drives, so both inputs highlight the same row.
    pub(crate) fn row_el(
        row: &MenuRow,
        i: usize,
        selected: bool,
        in_sub: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let th = theme::current();
        match row {
            MenuRow::Sep => div()
                .h(px(1.0))
                .mx_2()
                .my_1()
                .bg(rgb(th.border))
                .into_any_element(),
            MenuRow::Head(l) => div()
                .id(("mhead", i))
                .role(Role::Label)
                .h(px(metrics::MENU_HEAD))
                .px_2()
                .mx_1()
                .text_size(px(metrics::TEXT_XS))
                .text_color(rgb(th.text_head))
                .child(l.clone())
                .into_any_element(),
            MenuRow::Sub { id, label, sub } => {
                let sub = *sub;
                div()
                    .id(id.clone())
                    .test_support()
                    .role(Role::MenuItem)
                    .aria_label(label.clone())
                    .flex()
                    .items_center()
                    .h(px(metrics::MENU_ROW))
                    .px_2()
                    .mx_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .bg(if selected {
                        rgb(th.bg_hover)
                    } else {
                        rgba(0x00000000)
                    })
                    .hover(|s| s.bg(rgb(th.bg_hover)))
                    .text_size(px(metrics::TEXT_LG))
                    .text_color(rgb(th.text))
                    .whitespace_nowrap()
                    .child(div().w(px(metrics::CHECK_W)))
                    .child(div().flex_1().child(label.clone()))
                    .child(
                        div()
                            .pl_2()
                            .text_color(rgb(th.text_faint))
                            .text_size(px(metrics::TEXT_SM))
                            .child("▸"),
                    )
                    .on_mouse_move(cx.listener(move |v, e: &MouseMoveEvent, _w, cx| {
                        v.menu_sel = Some(i);
                        // Only hover-open when no cascade is up: once one is
                        // open, a diagonal cursor path toward a submenu item
                        // would cross the sibling rows and replace the submenu
                        // mid-flight (classic "safe triangle" problem).
                        if v.open_sub.is_none() {
                            v.open_sub = Some((sub, f32::from(e.position.y)));
                        }
                        cx.notify();
                    }))
                    .on_click(cx.listener(move |v, e: &ClickEvent, _w, cx| {
                        cx.stop_propagation();
                        v.menu_sel = Some(i);
                        v.open_sub = Some((sub, f32::from(e.position().y)));
                        cx.notify();
                    }))
                    .into_any_element()
            }
            MenuRow::Leaf(l) => {
                let act = l.act.clone();
                let enabled = l.enabled;
                let mut el = div()
                    .id(l.id.clone())
                    .test_support()
                    .role(if l.check.is_some() {
                        Role::MenuItemCheckBox
                    } else {
                        Role::MenuItem
                    })
                    .aria_label(l.label.clone())
                    .when(l.check.is_some(), |this| {
                        this.aria_toggled(if l.check == Some(true) {
                            Toggled::True
                        } else {
                            Toggled::False
                        })
                    })
                    .when(!l.shortcut.is_empty(), |this| {
                        this.aria_keyshortcuts(l.shortcut.clone())
                    })
                    .flex()
                    .items_center()
                    .h(px(metrics::MENU_ROW))
                    .px_2()
                    .mx_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .bg(if selected {
                        rgb(th.bg_hover)
                    } else {
                        rgba(0x00000000)
                    })
                    .text_size(px(metrics::TEXT_LG))
                    .text_color(rgb(if enabled { th.text } else { th.text_faint }))
                    .whitespace_nowrap()
                    .child(
                        div()
                            .w(px(metrics::CHECK_W))
                            .text_size(px(metrics::TEXT_SM))
                            .text_color(rgb(th.lcd))
                            .child(if l.check == Some(true) { "✓" } else { "" }),
                    )
                    .child(div().flex_1().child(l.label.clone()))
                    .child(
                        div()
                            .pl_2()
                            .text_color(rgb(l.badge_color.unwrap_or(th.text_faint)))
                            .text_size(px(metrics::TEXT_SM))
                            .child(l.shortcut.clone()),
                    );
                if enabled {
                    el = el
                        .hover(|s| s.bg(rgb(th.bg_hover)))
                        .on_mouse_move(cx.listener(move |v, e: &MouseMoveEvent, _w, cx| {
                            if in_sub {
                                v.sub_sel = Some(i);
                            } else {
                                v.menu_sel = Some(i);
                                // leaving a submenu's parent row closes the
                                // cascade — only for rows ABOVE its anchor: a
                                // diagonal sweep into the submenu's lower items
                                // crosses the below-anchor rows inside the
                                // parent and must not unmount it mid-flight
                                if v.open_sub.is_some_and(|(_, y)| f32::from(e.position.y) < y) {
                                    v.open_sub = None;
                                }
                            }
                            cx.notify();
                        }))
                        .on_click(cx.listener(move |v, _e, w, cx| {
                            cx.stop_propagation();
                            v.open_menu = None;
                            v.open_sub = None;
                            act(v, w, cx);
                            cx.notify();
                        }));
                }
                if let Some(detail) = l.detail.clone() {
                    el = el.tooltip(move |_w, cx| {
                        let detail = detail.clone();
                        cx.new(|_| Tip(detail)).into()
                    });
                }
                el.into_any_element()
            }
        }
    }

    /// Inspector panel under the event list: shows the selected events' or
    /// notes' fields; clicking a field row starts an edit applied to every
    /// selected row.
    pub(crate) fn prop_panel(&self, cx: &mut Context<Self>) -> Div {
        let th = theme::current();
        let (title, rows) = self.doc(|d| self.prop_rows(d));
        let any_warn = rows.iter().any(|r| r.warn);
        let mut panel = div()
            .flex()
            .flex_col()
            .gap_y(px(2.0))
            .border_t_1()
            .border_color(rgb(th.border))
            .px_2()
            .py_1()
            .child(
                div()
                    .text_size(px(metrics::TEXT_SM))
                    .text_color(rgb(th.text_dim))
                    .child(title),
            );
        for (ix, r) in rows.into_iter().enumerate() {
            let mut row = div()
                .id(("prop-row", ix))
                .flex()
                .gap_2()
                .text_size(px(metrics::TEXT_MD))
                .child(
                    div()
                        .w(px(86.0))
                        .text_color(if r.warn {
                            rgb(th.warn)
                        } else {
                            rgb(th.text_dim)
                        })
                        .child(if r.warn {
                            SharedString::from(format!("{}!", r.label))
                        } else {
                            r.label.clone()
                        }),
                )
                .child(
                    div()
                        .font_family("Cascadia Mono")
                        .text_color(rgb(th.text))
                        .overflow_hidden()
                        .child(r.value.clone()),
                );
            if let Some(field) = r.field {
                let value = r.value.clone();
                row = row
                    .cursor_pointer()
                    .hover(|s| s.bg(rgba(theme::current().hover_wash)))
                    .on_click(
                        cx.listener(move |v, _e, w, cx| v.prop_edit(field, value.clone(), w, cx)),
                    );
            }
            panel = panel.child(row);
        }
        let mut foot = div().flex().gap_1().items_center().child(
            div()
                .w(px(150.0))
                .h(px(22.0))
                .child(Input::new(&self.prop_input)),
        );
        if self.prop_field.is_some() {
            foot = foot.child(Self::chip(
                "prop.apply",
                t("prop.apply"),
                t("prop.apply"),
                cx,
                |v, _e, cx| v.prop_apply(cx),
            ));
        }
        if any_warn {
            foot = foot.child(
                div()
                    .text_size(px(metrics::TEXT_SM))
                    .text_color(rgb(th.warn))
                    .child(t("prop.raw_warn")),
            );
        }
        panel.child(foot)
    }
}

// --- toolbar helpers -------------------------------------------------------------

/// Tooltip bubble view.
pub(crate) struct Tip(pub(crate) SharedString);

impl Render for Tip {
    fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let th = theme::current();
        div()
            .px_2()
            .py_1()
            .bg(rgb(theme::current().bg_tooltip))
            .border_1()
            .border_color(rgb(theme::current().border_strong))
            .rounded_md()
            .shadow_lg()
            .text_size(px(metrics::TEXT_MD))
            .text_color(rgb(th.text))
            .whitespace_nowrap()
            .child(self.0.clone())
    }
}

impl EditorView {
    /// 1px vertical separator between toolbar icon groups.
    pub(crate) fn vsep() -> Div {
        div()
            .w(px(1.0))
            .h(px(metrics::VSEP_H))
            .mx_1()
            .bg(rgb(theme::current().border))
    }

    /// Icon button: 26px square, tooltip, neutral gray icon.
    /// Screen readers get `tip` as the name and the on/off state as a toggle.
    pub(crate) fn ibtn(
        id: &'static str,
        ic: &'static str,
        tip: &'static str,
        on: bool,
        cx: &mut Context<Self>,
        f: impl Fn(&mut Self, &ClickEvent, &mut Context<Self>) + 'static,
    ) -> ObservedElement<Stateful<Div>> {
        let th = theme::current();
        div()
            .id(id)
            .test_support()
            .role(Role::Button)
            .aria_label(tip)
            .aria_toggled(if on { Toggled::True } else { Toggled::False })
            .w(px(metrics::ICON_BTN))
            .h(px(metrics::ICON_BTN))
            .flex()
            .items_center()
            .justify_center()
            .rounded_sm()
            .cursor_pointer()
            .bg(if on {
                rgb(th.accent_bg)
            } else {
                rgb(th.bg_panel)
            })
            .border_1()
            .border_color(if on {
                rgb(th.accent_edge)
            } else {
                rgb(th.bg_panel)
            })
            .hover(move |s| s.bg(rgb(th.bg_hover)))
            .tooltip(move |_w, cx| cx.new(|_| Tip(tip.into())).into())
            .child(icon(ic, 16.0, if on { th.accent } else { th.icon_off }))
            .on_click(cx.listener(move |v, e, _w, cx| {
                f(v, e, cx);
                cx.notify();
            }))
    }

    /// `ibtn` whose handler also receives the window — needed by actions
    /// that open a window-level prompt such as the discard guard.
    pub(crate) fn ibtn_w(
        id: &'static str,
        ic: &'static str,
        tip: &'static str,
        on: bool,
        cx: &mut Context<Self>,
        f: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        let th = theme::current();
        div()
            .id(id)
            .w(px(metrics::ICON_BTN))
            .h(px(metrics::ICON_BTN))
            .flex()
            .items_center()
            .justify_center()
            .rounded_sm()
            .cursor_pointer()
            .bg(if on {
                rgb(th.accent_bg)
            } else {
                rgb(th.bg_panel)
            })
            .border_1()
            .border_color(if on {
                rgb(th.accent_edge)
            } else {
                rgb(th.bg_panel)
            })
            .hover(move |s| s.bg(rgb(th.bg_hover)))
            .tooltip(move |_w, cx| cx.new(|_| Tip(tip.into())).into())
            .child(icon(ic, 16.0, if on { th.accent } else { th.icon_off }))
            .on_click(cx.listener(move |v, _e, w, cx| {
                f(v, w, cx);
                cx.notify();
            }))
    }

    /// Icon button whose active state gets a custom accent color.
    pub(crate) fn ibtn_c(
        id: &'static str,
        ic: &'static str,
        tip: &'static str,
        on: bool,
        accent: u32,
        cx: &mut Context<Self>,
        f: impl Fn(&mut Self, &ClickEvent, &mut Context<Self>) + 'static,
    ) -> ObservedElement<Stateful<Div>> {
        let th = theme::current();
        div()
            .id(id)
            .test_support()
            .role(Role::Button)
            .aria_label(tip)
            .aria_toggled(if on { Toggled::True } else { Toggled::False })
            .w(px(metrics::ICON_BTN))
            .h(px(metrics::ICON_BTN))
            .flex()
            .items_center()
            .justify_center()
            .rounded_sm()
            .cursor_pointer()
            .bg(if on {
                rgb(th.accent_bg)
            } else {
                rgb(th.bg_panel)
            })
            .border_1()
            .border_color(if on {
                rgb(th.accent_edge)
            } else {
                rgb(th.bg_panel)
            })
            .hover(move |s| s.bg(rgb(th.bg_hover)))
            .tooltip(move |_w, cx| cx.new(|_| Tip(tip.into())).into())
            .child(icon(ic, 16.0, if on { accent } else { th.icon_off }))
            .on_click(cx.listener(move |v, e, _w, cx| {
                f(v, e, cx);
                cx.notify();
            }))
    }
}
