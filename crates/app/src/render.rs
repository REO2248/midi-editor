//! Rendering: impl Render for EditorView (toolbar, track column, ruler,
//! piano roll canvas, lane, event list) plus the chip/button helpers.
//! Private items are visible here because this is a child module of the
//! crate root where EditorView is defined.

use crate::a11y;
use crate::cmd;
use crate::geometry::{drag_window, tick_window, ZOOM_MAX, ZOOM_MIN};
use crate::i18n::{t, tf};
use crate::icons::icon;
use crate::menu::{MenuRow, MENUS};
use crate::theme::metrics;
use crate::*;
use gpui_kit::base::TestSupportExt;
use gpui_kit::component::input::Input;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::any::Any;
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Linear blend of two 0xRRGGBB colors — ghost-track dimming.
fn blend(c: u32, to: u32, f: f32) -> u32 {
    let r = (((c >> 16) & 0xFF) as f32 * (1.0 - f) + ((to >> 16) & 0xFF) as f32 * f) as u32;
    let g = (((c >> 8) & 0xFF) as f32 * (1.0 - f) + ((to >> 8) & 0xFF) as f32 * f) as u32;
    let b = ((c & 0xFF) as f32 * (1.0 - f) + (to & 0xFF) as f32 * f) as u32;
    (r << 16) | (g << 8) | b
}

/// Per-key-group tint for the all-keys poly-AT lane (key/16 → index).
pub(crate) const LANE_KEY_COLORS: [u32; 8] = [
    0x4fd0ff, 0x8fd0a0, 0xe0b050, 0xd070e0, 0x70d0d0, 0xe0e070, 0xa090ff, 0xff9090,
];

/// Snap-menu label: metrical files subdivide a whole note, SMPTE files a
/// second — "1/16" vs "1/16s" makes the redefined grid explicit instead
/// of silently suggesting beats that don't exist. "off" stays bare.
impl EditorView {
    /// Quantize grid rows (checkable) — labeled with the SNAPS table.
    pub(crate) fn quant_grid_rows(&self, cx: &mut Context<Self>) -> Vec<MenuRow> {
        let td = self.td();
        SNAPS
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let label = snap_label(s.2, td);
                Self::mi_leaf(
                    ("qgrid", i),
                    label,
                    "",
                    Some(self.q_snap == i),
                    cx,
                    move |v, _e, _cx| {
                        v.q_snap = i;
                    },
                )
            })
            .collect()
    }

    /// Quantize strength rows — 100/75/50 with the current pick checked.
    pub(crate) fn quant_str_rows(&self, cx: &mut Context<Self>) -> Vec<MenuRow> {
        [100u32, 75, 50]
            .iter()
            .enumerate()
            .map(|(i, &st)| {
                Self::mi_leaf(
                    ("qstr", i),
                    tf("quant.strength", &[("p", st.to_string().as_str())]),
                    "",
                    Some(self.q_str == st),
                    cx,
                    move |v, _e, _cx| v.q_str = st,
                )
            })
            .collect()
    }
}

fn snap_label(label: &'static str, td: TimeDisplay) -> String {
    if td.is_smpte() && label != "off" {
        format!("{label}s")
    } else {
        label.to_string()
    }
}

impl Render for EditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // native title bar follows the document (#163) — writes only on
        // change so frames don't syscall
        let title = self.window_title();
        if title != self.last_title {
            window.set_window_title(&title);
            self.last_title = title;
        }
        // the plugin editor lives in the helper subprocess's own window —
        // no native event queue to pump here
        // focus-loss guarantee: deactivate = release preview notes (the
        // worker's deadline cap is the backstop for a kill without repaint)
        let active = window.is_window_active();
        if self.win_active && !active {
            self.audition_off();
        }
        self.win_active = active;
        self.refresh_derived();
        let th = self.theme;
        // chrome helpers (menus, chips, icon buttons) read the same palette
        theme::set_current(th);
        // a hidden region must never hold keyboard focus
        self.repair_focus(window, cx);
        // `sel_track` can outlive its track if an MCP edit removed it
        let n_tracks = self.doc(|d| d.tracks.len());
        if n_tracks > 0 {
            self.sel_track = self.sel_track.min(n_tracks - 1);
        }
        let area = self.area(window, cx);
        // remember the last working region — menu commands restore focus to it
        if area != FocusArea::MenuBar {
            self.last_area = area;
        }
        // menu models rebuild every frame while open; drop them when closed
        if self.open_menu.is_none() {
            self.menu_rows.clear();
            self.sub_rows.clear();
            self.menu_sel = None;
            self.sub_sel = None;
        }
        // menu actions arrive without a Window — the dialog opens here
        if let Some((tr, tick, mt, id)) = self.meta_pending.take() {
            self.open_meta_edit(tr, tick, mt, id, window, cx);
        }
        // a closed meta dialog must hand keyboard focus back to the editor
        // (Enter has no Window in the input subscription, so it's deferred)
        if self.meta_refocus {
            self.meta_refocus = false;
            window.focus(&self.focus.clone(), cx);
        }
        // keep both scroll axes inside the content (resizes, zooms, edits all
        // self-heal here) and edge-scroll while a drag is parked at a border;
        // `panning` keeps animation frames flowing only while it actually moves
        self.clamp_scroll();
        let panning = self.drag_auto_pan();

        // advance playhead / auto-stop (looping happens inside the
        // playback thread; reaching this branch means playback ended)
        if let Some(p) = &self.playback {
            self.play_us = self.live_pos_us(p);
            if !p.is_running() {
                self.playback = None;
                // natural end follows the same stop policy as a manual
                // stop — return to the pass start when enabled (#156)
                if self.return_to_start_on_stop {
                    self.play_us = self.play_start_us;
                }
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
            mcp_auth_mode,
            mcp_auth_detail,
            port_present,
        ) = {
            let sh = crate::lock_shared(&self.shared);
            (
                // playhead ruler units come from the viewed sequence's map
                sh.doc
                    .tempo_map_for(self.sel_track)
                    .us_to_tick(self.play_us),
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
                sh.mcp_security.auth_mode,
                sh.mcp_security.auth_detail.clone(),
                sh.port_present.clone(),
            )
        };
        // revision-cached document chrome (markers, names, diagnostics, …) —
        // rebuilt on edits only, never per animation frame
        let doc_ui = self.doc_ui.clone();
        let n_diags = doc_ui.n_diags;
        // tempo in force at the playhead on the conductor track (#133) —
        // not the tick-0 tempo
        let tempo0 = {
            let tr = self.tempo_track();
            self.doc(|d| crate::edit_ops::tempo_bpm_at(d, tr, d.tempo_map.us_to_tick(self.play_us)))
        };
        // signature in force at the playhead on the viewed track — the
        // file's real FF58 map, not the first signature in the file
        let sig = self.doc(|d| {
            let m = d.meter_map_for(self.sel_track).meter_at(playhead_tick);
            format!("{}/{}", m.num, 1u32 << m.den_pow.min(15))
        });
        let track_names = doc_ui.track_names.clone();
        let track_chs = doc_ui.track_chs.clone();
        let markers = &doc_ui.markers;
        // SMF format 2: tracks are independent sequences — the app shows
        // one at a time (the selected "track" IS the viewed sequence) and
        // plays only it unless tracks are explicitly soloed
        let (is_seq, n_tracks) = self.doc(|d| (d.is_sequential(), d.tracks.len()));
        // explicit UI timing mode — metrical bar/beat or SMPTE timecode,
        // drawn straight from the SMF division (never a pretend PPQ)
        let td = self.td();
        // position labels + bar/beat grid follow the viewed track's real
        // FF58 meter map — format 2: that sequence's own signatures
        let pos_fmt = self.doc(|d| d.position_format_for(self.sel_track));
        let (grid_minor, grid_major) = td.grid_ticks(self.zoom);
        let note_min = td.min_grid_ticks();
        let badge = td.badge();
        let pos = pos_fmt.fmt(playhead_tick);

        // playhead follow — suspended while a drag is live or the user just
        // scrolled manually (`follow_hold`)
        if self.playback.is_some()
            && self.follow != Follow::Off
            && self.drag.is_none()
            && !self
                .follow_hold
                .is_some_and(|t| t > std::time::Instant::now())
        {
            self.follow_playhead(playhead_tick);
        }

        // --- piano roll canvas -------------------------------------------------
        let notes = self.notes.clone();
        let notes_span = self.notes_span;
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
        // visible row→key map + row height (vertical zoom / fold / drum)
        let vis_keys = self.vis_keys.clone();
        let row_of = self.row_of;
        let note_h = self.note_h;
        let scale_pcs = self.scale_pcs;

        // piano-key strip: clickable/scrubbable keyboard that auditions the
        // pitch through the selected track's routing (issue #39)
        let kbd_bounds_cell = self.kbd_bounds.clone();
        let scrub_key = self.scrub_key;
        let kbd_keys = vis_keys.clone();
        let strip_keys = vis_keys.clone();
        let kbd = canvas(
            move |bounds, _window, _cx| {
                kbd_bounds_cell.set(bounds);
            },
            move |bounds, _state, window, _cx| {
                let w = bounds.size.width;
                let nrows = kbd_keys.len() as f32;
                let r0 = (scroll_y / note_h).max(0.0) as i32;
                let r1 =
                    ((scroll_y + f32::from(bounds.size.height)) / note_h + 1.0).min(nrows) as i32;
                for r in r0..r1 {
                    let key = kbd_keys[r as usize];
                    let black = matches!(key % 12, 1 | 3 | 6 | 8 | 10);
                    let cur = scrub_key == Some(key);
                    let y = bounds.origin.y + px(r as f32 * note_h - scroll_y);
                    window.paint_quad(fill(
                        Bounds::new(
                            point(bounds.origin.x, y),
                            size(w, px((note_h - 1.0).max(1.0))),
                        ),
                        rgb(if cur {
                            theme::current().accent
                        } else if black {
                            0x101016
                        } else {
                            0x2a2a34
                        }),
                    ));
                    // C guide line across the strip, like the roll's rows
                    if key.is_multiple_of(12) {
                        window.paint_quad(fill(
                            Bounds::new(
                                point(bounds.origin.x, y + px(note_h - 1.0)),
                                size(w, px(1.0)),
                            ),
                            rgb(theme::current().bg_chip_hover),
                        ));
                    }
                }
            },
        );
        // keyboard edit cursor — only when the roll context owns focus and no
        // note selection exists (a selection is itself the edit target)
        let show_cursor = area == FocusArea::Roll && self.selection.is_empty();
        let cursor_t = self.cursor_tick;
        let cursor_k = self.cursor_key;
        let cursor_len = self.cursor_insert_len();
        let pos_fmt_roll = pos_fmt.clone();

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
                // key rows — `vis_keys` maps painted row → piano key (the
                // identity map is all 128; fold/drum views shrink it)
                let nrows = vis_keys.len() as f32;
                let r0 = (scroll_y / note_h).max(0.0) as i32;
                let r1 = ((scroll_y + f32::from(h)) / note_h + 1.0).min(nrows) as i32;
                for r in r0..r1 {
                    let key = vis_keys[r as usize];
                    let black = matches!(key % 12, 1 | 3 | 6 | 8 | 10);
                    let in_scale = scale_pcs.map(|p| p[(key % 12) as usize]).unwrap_or(false);
                    let y = bounds.origin.y + px(r as f32 * note_h - scroll_y);
                    if in_scale {
                        window.paint_quad(fill(
                            Bounds::new(point(bounds.origin.x, y), size(w, px(note_h))),
                            rgb(theme::current().scale_row),
                        ));
                    } else if black {
                        window.paint_quad(fill(
                            Bounds::new(point(bounds.origin.x, y), size(w, px(note_h))),
                            rgb(theme::current().bg_key),
                        ));
                    }
                    window.paint_quad(fill(
                        Bounds::new(point(bounds.origin.x, y), size(w, px(1.0))),
                        rgb(if key.is_multiple_of(12) {
                            theme::current().grid_oct
                        } else {
                            theme::current().grid_row
                        }),
                    ));
                }
                // beat/bar lines — real FF58 bar boundaries for metrical
                // (bars are not a fixed stride once the meter changes),
                // frame/second for SMPTE (minor lines collapse when
                // < ~4px apart)
                let tick0 = (scroll_x / zoom).max(0.0) as u64;
                match &pos_fmt_roll {
                    PositionFormat::Bbt(mm) => {
                        let tick1 = tick0 + (f32::from(w) / zoom) as u64 + mm.bar_ticks_at(tick0);
                        // minor beat lines collapse into bar-only when
                        // they'd draw too close — same rule as SMPTE;
                        // spacing is the denominator unit (one line per
                        // subdivision, accented on compound beats)
                        let min_beat_px = mm.unit_ticks_of(mm.meter_at(tick0)) as f32 * zoom;
                        for (t, down) in mm.beat_lines_between(tick0, tick1) {
                            if !down && min_beat_px < 4.0 {
                                continue;
                            }
                            let x = bounds.origin.x + px(t as f32 * zoom - scroll_x);
                            window.paint_quad(fill(
                                Bounds::new(point(x, bounds.origin.y), size(px(1.0), h)),
                                rgb(if down {
                                    theme::current().grid_bar
                                } else {
                                    theme::current().border
                                }),
                            ));
                        }
                    }
                    PositionFormat::Smpte(_) => {
                        let tick1 = tick0 + (f32::from(w) / zoom) as u64 + grid_minor;
                        let mut t = tick0 / grid_minor * grid_minor;
                        while t <= tick1 {
                            let x = bounds.origin.x + px(t as f32 * zoom - scroll_x);
                            let strong = t.is_multiple_of(grid_major);
                            window.paint_quad(fill(
                                Bounds::new(point(x, bounds.origin.y), size(px(1.0), h)),
                                rgb(if strong {
                                    theme::current().grid_bar
                                } else {
                                    theme::current().border
                                }),
                            ));
                            t += grid_minor;
                        }
                    }
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
                // widen the entry bound by the widest note: a sustained
                // note whose start scrolled past the left edge must stay
                // visible until its release follows (#188)
                let entry = crate::geometry::cull_entry(entry, notes_span);
                let first = notes.partition_point(|n| (n.start_tick as i64) < entry);
                for n in &notes[first..] {
                    if (n.start_tick as i64) > exit {
                        break;
                    }
                    let mut st = n.start_tick as i64;
                    let mut en = n.end_tick.unwrap_or(n.start_tick + note_min) as i64;
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
                        let orow = row_of[n.key as usize];
                        if orow >= 0 {
                            let oy = bounds.origin.y + px(orow as f32 * note_h - scroll_y);
                            window.paint_quad(fill(
                                Bounds::new(
                                    point(ox, oy + px(1.0)),
                                    size(px(ow), px(note_h - 2.0)),
                                ),
                                rgba(theme::current().ghost_fill),
                            ));
                        }
                    }
                    let x = bounds.origin.x + px(st as f32 * zoom - scroll_x);
                    let wpx = ((en - st).max(1) as f32 * zoom).max(3.0);
                    if x + px(wpx) < bounds.origin.x {
                        continue;
                    }
                    // folded-out keys (or a drag delta past the map) get no row
                    let row = row_of[key.clamp(0, 127) as usize];
                    if row < 0 {
                        continue;
                    }
                    let y = bounds.origin.y + px(row as f32 * note_h - scroll_y);
                    if y < bounds.origin.y - px(note_h) || y > bounds.origin.y + h {
                        continue;
                    }
                    let c = if selection.contains(&n.on_id) {
                        theme::current().sel
                    } else if n.end_tick.is_none() {
                        DANGLING_COLOR
                    } else {
                        let c = th.track_colors[n.track % th.track_colors.len()];
                        if n.track == active_track {
                            c
                        } else {
                            blend(c, theme::current().dim_target, 0.62)
                        }
                    };
                    window.paint_quad(fill(
                        Bounds::new(point(x, y + px(1.0)), size(px(wpx), px(note_h - 2.0))),
                        rgb(c),
                    ));
                }
                // playhead
                let px_x = bounds.origin.x + px(play_x_tick as f32 * zoom - scroll_x);
                if px_x >= bounds.origin.x && px_x <= bounds.origin.x + w {
                    window.paint_quad(fill(
                        Bounds::new(point(px_x, bounds.origin.y), size(px(1.5), h)),
                        rgb(theme::current().ok),
                    ));
                }
                // edit cursor — marks where Enter inserts / where keys edit;
                // folded out keys (row -1) paint nothing
                if show_cursor {
                    let cur_row = if (0..=127).contains(&cursor_k) {
                        row_of[cursor_k as usize]
                    } else {
                        -1
                    };
                    if cur_row >= 0 {
                        let cx0 = bounds.origin.x + px(cursor_t as f32 * zoom - scroll_x);
                        let cy0 =
                            bounds.origin.y + px(cur_row as f32 * note_h - scroll_y) + px(1.0);
                        let cw = (cursor_len as f32 * zoom).max(3.0);
                        let ch = (note_h - 2.0).max(1.0);
                        window.paint_quad(outline(
                            Bounds::new(point(cx0, cy0), size(px(cw), px(ch))),
                            rgb(theme::current().accent),
                            BorderStyle::Solid,
                        ));
                        window.paint_quad(fill(
                            Bounds::new(point(cx0 + px(cw) + px(1.0), cy0), size(px(1.5), px(ch))),
                            rgb(theme::current().accent),
                        ));
                    }
                }
                // marquee rubber band — corners are keys from hit(); paint
                // in row space so the band matches what commit_drag selects
                if let Some((a_t, a_k, b_t, b_k)) = marquee {
                    let row_at = |k: i32| -> i32 {
                        if (0..=127).contains(&k) {
                            row_of[k as usize]
                        } else {
                            -1
                        }
                    };
                    let ra = row_at(a_k);
                    let rb = row_at(b_k);
                    if ra.max(rb) >= 0 {
                        let (t0, t1) = (a_t.min(b_t), a_t.max(b_t));
                        let (rt, rbm) = (ra.min(rb).max(0), ra.max(rb));
                        let x0 = bounds.origin.x + px(t0 as f32 * zoom - scroll_x);
                        let x1 = bounds.origin.x + px(t1 as f32 * zoom - scroll_x);
                        let y0 = bounds.origin.y + px(rt as f32 * note_h - scroll_y);
                        let y1 = bounds.origin.y + px((rbm + 1) as f32 * note_h - scroll_y);
                        window.paint_quad(fill(
                            Bounds::new(point(x0, y0), size(x1 - x0, y1 - y0)),
                            rgba(theme::current().sel_fill),
                        ));
                    }
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
        let menu_focused = area == FocusArea::MenuBar;
        let mut menu_bar = div()
            .id("menu-bar")
            .test_support()
            .role(Role::MenuBar)
            .aria_label(t("a11y.menubar"))
            .flex()
            .items_center()
            .h(px(28.0))
            .pl_1()
            .pr_3()
            .bg(rgb(theme::current().bg_bar))
            .border_b_1()
            .border_color(rgb(if menu_focused { th.accent } else { th.border }))
            .text_size(px(metrics::TEXT_LG))
            .track_focus(&self.menu_fh)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, w, cx| {
                // open menus are driven by the root handler
                if this.open_menu.is_some() {
                    return;
                }
                // region keys are bare keys only — chords bubble to root
                if ev.keystroke.modifiers.control || ev.keystroke.modifiers.alt {
                    return;
                }
                match ev.keystroke.key.as_str() {
                    "left" | "right" => {
                        let d: usize = if ev.keystroke.key == "left" {
                            MENUS.len() - 1
                        } else {
                            1
                        };
                        this.menu_bar_sel = (this.menu_bar_sel + d) % MENUS.len();
                        cx.stop_propagation();
                        cx.notify();
                    }
                    "down" | "enter" | " " | "space" => {
                        let m = MENUS[this.menu_bar_sel.min(MENUS.len() - 1)].0;
                        this.open_menu_at(m, cx);
                        cx.stop_propagation();
                    }
                    "escape" => {
                        w.focus(&this.roll_fh, cx);
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            }))
            .child(
                div()
                    .w(px(96.0))
                    .px_2()
                    .text_color(rgb(theme::current().text_link))
                    .whitespace_nowrap()
                    .child(t("app.title")),
            );
        let mut mx = 96.0f32;
        for (i, (m, key, w)) in MENUS.iter().enumerate() {
            let m = *m;
            let key = *key;
            let w = *w;
            let is_open = open_menu.map(|(mm, _)| mm) == Some(m);
            let bar_sel = menu_focused && i == self.menu_bar_sel;
            menu_bar = menu_bar.child(
                div()
                    .id(key)
                    .test_support()
                    .role(Role::MenuItem)
                    .aria_label(t(key))
                    .aria_expanded(is_open)
                    .w(px(w))
                    .h(px(22.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .rounded_sm()
                    .bg(if is_open || bar_sel {
                        rgb(theme::current().bg_raised)
                    } else {
                        rgb(theme::current().bg_bar)
                    })
                    .text_color(rgb(if is_open || bar_sel {
                        theme::current().text_bright
                    } else {
                        theme::current().icon_off
                    }))
                    .hover(|s| s.bg(rgb(theme::current().bg_menu_hover)))
                    .child(t(key))
                    .on_click(cx.listener(move |v, _e, w, cx| {
                        w.focus(&v.menu_fh, cx);
                        v.menu_bar_sel = i;
                        if is_open {
                            v.open_menu = None;
                            v.open_sub = None;
                        } else {
                            v.open_menu_at(m, cx);
                        }
                        cx.notify();
                    }))
                    .on_mouse_move(cx.listener(move |v, _e, _w, cx| {
                        // while a menu is open, hovering a sibling label switches
                        if v.open_menu.is_some() && v.open_menu.map(|(mm, _)| mm) != Some(m) {
                            v.open_menu = Some((m, mx));
                            v.open_sub = None;
                            v.menu_sel = None;
                            v.sub_sel = None;
                            v.menu_bar_sel = i;
                            cx.notify();
                        }
                    })),
            );
            mx += w;
        }
        menu_bar = menu_bar.child(div().flex_1()).child(
            div()
                .text_size(px(metrics::TEXT_LG))
                .text_color(rgb(if dirty { th.warn } else { th.icon_off }))
                .whitespace_nowrap()
                .child(format!("{title}{}", if dirty { " •" } else { "" })),
        );

        // --- transport / tool bar: icon groups, DAW style -------------------------
        let transport_bar = div()
            .id("transport-bar")
            .test_support()
            .role(Role::Toolbar)
            .aria_label(t("a11y.toolbar"))
            .flex()
            .items_center()
            .gap(px(2.0))
            .px_2()
            .h(px(40.0))
            .bg(rgb(theme::current().bg_panel))
            .border_b_1()
            .border_color(rgb(theme::current().border))
            // file ops
            .child(Self::ibtn_w(
                "i.new",
                "note_add",
                t("tip.new"),
                false,
                cx,
                |v, w, cx| {
                    v.confirm_discard_or_save(PendingAction::NewFile, w, cx);
                },
            ))
            .child(Self::ibtn_w(
                "i.open",
                "folder_open",
                t("tip.open"),
                false,
                cx,
                |v, w, cx| {
                    v.confirm_discard_or_save(PendingAction::OpenDialog, w, cx);
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
                theme::current().ok,
                cx,
                |v, _e, cx| v.toggle_play(cx),
            ))
            .child(Self::ibtn(
                "i.stop",
                "stop",
                t("tip.stop"),
                false,
                cx,
                |v, _e, cx| v.transport_stop(cx),
            ))
            .child(Self::ibtn(
                "i.start",
                "skip_previous",
                t("tip.go_start"),
                false,
                cx,
                |v, _e, cx| v.go_to_start(cx),
            ))
            .child(Self::ibtn_c(
                "i.rec",
                "fiber_manual_record",
                t("tip.rec"),
                self.is_recording(),
                theme::current().danger,
                cx,
                |v, _e, cx| v.transport_record(cx),
            ))
            .child(Self::ibtn_c(
                "i.loop",
                "loop",
                t("tip.loop"),
                loop_en,
                theme::current().accent,
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
                theme::current().accent,
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
                    .id("pos")
                    .test_support()
                    .role(Role::Label)
                    .aria_label(tf("a11y.pos", &[("pos", pos.as_str())]))
                    .px_2()
                    .h(px(24.0))
                    .flex()
                    .items_center()
                    .bg(rgb(theme::current().bg_input))
                    .border_1()
                    .border_color(rgb(theme::current().border))
                    .rounded_sm()
                    .text_color(rgb(th.lcd))
                    .text_size(px(metrics::TEXT_LG))
                    .font_family("Cascadia Mono")
                    .whitespace_nowrap()
                    .child(pos.clone()),
            )
            // explicit timing-mode badge: ppq for metrical, fps for SMPTE
            // — amber for SMPTE so a timecode file never masquerades as a
            // musical grid
            .child(
                div()
                    .px_2()
                    .h(px(24.0))
                    .flex()
                    .items_center()
                    .bg(rgb(theme::current().bg_input))
                    .border_1()
                    .border_color(rgb(theme::current().border))
                    .rounded_sm()
                    .text_color(rgb(if td.is_smpte() {
                        theme::current().warn_alt
                    } else {
                        theme::current().accent
                    }))
                    .text_size(px(11.0))
                    .font_family("Cascadia Mono")
                    .whitespace_nowrap()
                    .child(badge.clone()),
            )
            .child(
                div()
                    .id("bpm")
                    .test_support()
                    .role(Role::SpinButton)
                    .aria_label(tf(
                        "a11y.tempo",
                        &[("bpm", format!("{tempo0:.0}").as_str())],
                    ))
                    .aria_numeric_value(tempo0)
                    .aria_min_numeric_value(10.0)
                    .aria_max_numeric_value(400.0)
                    .on_a11y_action(AccessibleAction::Increment, {
                        let this = cx.entity().downgrade();
                        move |_, _, cx| {
                            this.update(cx, |this, cx| {
                                this.bump_tempo(1.0);
                                cx.notify();
                            })
                            .ok();
                        }
                    })
                    .on_a11y_action(AccessibleAction::Decrement, {
                        let this = cx.entity().downgrade();
                        move |_, _, cx| {
                            this.update(cx, |this, cx| {
                                this.bump_tempo(-1.0);
                                cx.notify();
                            })
                            .ok();
                        }
                    })
                    .px_2()
                    .h(px(24.0))
                    .flex()
                    .items_center()
                    .bg(rgb(theme::current().bg_input))
                    .border_1()
                    .border_color(rgb(theme::current().border))
                    .rounded_sm()
                    .cursor_pointer()
                    .text_color(rgb(th.lcd))
                    .text_size(px(metrics::TEXT_LG))
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
            .child(Self::chip(
                "sig",
                sig.clone(),
                tf("a11y.sig", &[("sig", sig.as_str())]),
                cx,
                |v, _e, cx| {
                    v.cycle_time_sig();
                    cx.notify();
                },
            ))
            // format-2 marker: the viewed sequence and the total — the
            // mode is explicit in chrome, never implicit in the file
            .children(is_seq.then(|| {
                let i = (self.sel_track + 1).to_string();
                let n = n_tracks.to_string();
                let label = tf("chip.seq", &[("i", i.as_str()), ("n", n.as_str())]);
                Self::chip("seq", label.clone(), label, cx, move |v, _e, cx| {
                    v.select_track((v.sel_track + 1) % n_tracks.max(1), cx);
                    cx.notify();
                })
            }))
            .child(Self::vsep())
            // edit tools
            .child(Self::ibtn_c(
                "i.sel",
                "arrow_selector_tool",
                t("tip.sel"),
                self.tool == Tool::Select,
                theme::current().accent,
                cx,
                |v, _e, cx| v.set_tool(Tool::Select, cx),
            ))
            .child(Self::ibtn_c(
                "i.draw",
                "edit",
                t("tip.draw"),
                self.tool == Tool::Draw,
                theme::current().accent,
                cx,
                |v, _e, cx| v.set_tool(Tool::Draw, cx),
            ))
            .child(Self::ibtn_c(
                "i.erase",
                "ink_eraser",
                t("tip.erase"),
                self.tool == Tool::Erase,
                theme::current().accent,
                cx,
                |v, _e, cx| v.set_tool(Tool::Erase, cx),
            ))
            .child(Self::vsep())
            // snap grid cycle: off / 1 / 1/2 / 1/4 / 1/8 / 1/16 / 1/32 —
            // for SMPTE files the fractions are seconds ("1/16s" ≈ 62ms)
            .child(
                div()
                    .id("snap")
                    .test_support()
                    .role(Role::SpinButton)
                    .aria_label(tf("a11y.snap", &[("grid", SNAPS[self.snap_idx].2)]))
                    .aria_numeric_value(self.snap_idx as f64)
                    .aria_min_numeric_value(0.0)
                    .aria_max_numeric_value((SNAPS.len() - 1) as f64)
                    .on_a11y_action(AccessibleAction::Increment, {
                        let this = cx.entity().downgrade();
                        move |_, _, cx| {
                            this.update(cx, |this, cx| this.cycle_snap(cx)).ok();
                        }
                    })
                    .on_a11y_action(AccessibleAction::Decrement, {
                        let this = cx.entity().downgrade();
                        move |_, _, cx| {
                            this.update(cx, |this, cx| {
                                this.snap_idx = (this.snap_idx + SNAPS.len() - 1) % SNAPS.len();
                                this.persist();
                                cx.notify();
                            })
                            .ok();
                        }
                    })
                    .h(px(26.0))
                    .pl_1()
                    .pr_2()
                    .flex()
                    .items_center()
                    .rounded_sm()
                    .bg(rgb(if SNAPS[self.snap_idx].0 > 0 {
                        theme::current().bg_raised
                    } else {
                        theme::current().bg_off
                    }))
                    .border_1()
                    .border_color(rgb(if SNAPS[self.snap_idx].0 > 0 {
                        theme::current().accent_edge
                    } else {
                        theme::current().border
                    }))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::current().bg_hover)))
                    .tooltip(move |_w, cx| cx.new(|_| Tip(t("tip.snap").into())).into())
                    .child(icon(
                        "grid_on",
                        15.0,
                        if SNAPS[self.snap_idx].0 > 0 {
                            theme::current().accent
                        } else {
                            theme::current().state_off
                        },
                    ))
                    .child(
                        div()
                            .pl_1()
                            .text_size(px(metrics::TEXT_MD))
                            .font_family("Cascadia Mono")
                            .text_color(rgb(if SNAPS[self.snap_idx].0 > 0 {
                                theme::current().text
                            } else {
                                theme::current().state_off
                            }))
                            .whitespace_nowrap()
                            .child(snap_label(SNAPS[self.snap_idx].2, td)),
                    )
                    .on_click(cx.listener(|v, _e, _w, cx| v.cycle_snap(cx))),
            )
            .child(Self::vsep())
            // selection ops (selection range, else whole track)
            .child(Self::ibtn(
                "i.quant",
                "compress",
                t(if td.is_smpte() {
                    "tip.quantize_smpte"
                } else {
                    "tip.quantize"
                }),
                false,
                cx,
                |v, _e, cx| {
                    let g = v.quantize_grid();
                    let st = v.q_str;
                    v.apply_region_op("quantize", move |d, tr, f, to| {
                        d.quantize_ops(tr, f, to, g, st)
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
        let events_focused = area == FocusArea::Events;
        let events_panel = div()
            .id("events-panel")
            .test_support()
            .role(Role::Region)
            .aria_label(t("a11y.events_list"))
            .w(px(340.0))
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::current().bg_panel))
            .border_l_1()
            .border_color(rgb(if events_focused {
                theme::current().accent
            } else {
                theme::current().border
            }))
            .track_focus(&self.events_fh)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _w, cx| {
                if this.open_menu.is_some() {
                    return;
                }
                match ev.keystroke.key.as_str() {
                    "up" => this.ev_step(-1, cx),
                    "down" => this.ev_step(1, cx),
                    "pageup" => this.ev_step(-20, cx),
                    "pagedown" => this.ev_step(20, cx),
                    "home" => this.ev_step(i64::MIN, cx),
                    "end" => this.ev_step(i64::MAX, cx),
                    "enter" => this.ev_activate(cx),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .px_2()
                    .h(px(26.0))
                    .border_b_1()
                    .border_color(rgb(th.border))
                    .text_size(px(metrics::TEXT_MD))
                    .text_color(rgb(th.text_muted))
                    .child(format!(
                        "{} ({}){}{}",
                        t("events.header"),
                        self.events.len(),
                        if self.ev_filter.is_active() {
                            format!(" [{}]", t("events.filtered"))
                        } else {
                            String::new()
                        },
                        self.doc_ui
                            .mode_hint
                            .map(|m| format!(" [{}]", m.label()))
                            .unwrap_or_default(),
                    ))
                    .child(div().flex_1())
                    .children((n_diags > 0).then(|| {
                        div()
                            .id("fix-diags")
                            .test_support()
                            .role(Role::Button)
                            .aria_label(tf(
                                "a11y.fix_diags",
                                &[("n", n_diags.to_string().as_str())],
                            ))
                            .ml_2()
                            .px_1()
                            .text_size(px(metrics::TEXT_SM))
                            .text_color(rgb(th.warn_alt))
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
                let refs = self.event_refs.clone();
                let sel = self.sel_events.clone();
                let ev_sel = self.ev_sel;
                let ev_focused = area == FocusArea::Events;
                let view = cx.entity();
                div()
                    .id("events-list")
                    .test_support()
                    .role(Role::List)
                    .aria_label(t("a11y.events_list"))
                    .flex_1()
                    .min_h(px(0.0))
                    .child(
                        uniform_list("events", events.len(), move |range, _w, _cx| {
                            range
                                .map(|i| {
                                    let selected = refs[i]
                                        .map(|(_, _, id)| sel.contains(&id))
                                        .unwrap_or(false);
                                    let cur = i == ev_sel;
                                    let view = view.clone();
                                    div()
                                        .id(("ev", i))
                                        .test_support()
                                        .role(Role::ListItem)
                                        .aria_label(events[i].text.clone())
                                        .h(px(18.0))
                                        .px_2()
                                        .text_size(px(metrics::TEXT_MD))
                                        .font_family("Cascadia Mono")
                                        .text_color(if selected || cur {
                                            rgb(theme::current().text_bright)
                                        } else {
                                            rgb(th.events_text)
                                        })
                                        .bg(if selected {
                                            rgba(theme::current().sel_fill)
                                        } else if cur && ev_focused {
                                            rgba(theme::current().hover_wash)
                                        } else {
                                            rgba(0x00000000)
                                        })
                                        .cursor_pointer()
                                        .hover(|s| s.bg(rgba(theme::current().hover_wash)))
                                        // clip at the panel edge — a long row
                                        // (RPN annotations, dense CC text)
                                        // used to paint over its neighbors
                                        // instead of truncating (#170); the
                                        // fixed row height keeps one line
                                        .overflow_hidden()
                                        .child(
                                            div()
                                                .w_full()
                                                .overflow_hidden()
                                                .child(events[i].text.clone()),
                                        )
                                        .on_mouse_down(MouseButton::Left, move |ev, w, cx| {
                                            view.update(cx, |this, cx| {
                                                this.ev_row_click(
                                                    i,
                                                    ev.modifiers.control,
                                                    ev.modifiers.shift,
                                                    ev.click_count == 2,
                                                    cx,
                                                );
                                                w.focus(&this.events_fh, cx);
                                            });
                                        })
                                })
                                .collect()
                        })
                        .h_full()
                        .track_scroll(&self.events_scroll),
                    )
            })
            .child(self.prop_panel(cx));

        let body = div().flex().flex_1().min_h(px(0.0));

        // --- track column: select / mute / solo -------------------------------
        let tracks_focused = area == FocusArea::Tracks;
        let track_col = div()
            .id("track-col")
            .test_support()
            .w(px(150.0))
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::current().bg_row))
            .border_r_1()
            .border_color(rgb(if tracks_focused {
                theme::current().accent
            } else {
                theme::current().border
            }))
            .track_focus(&self.tracks_fh)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, w, cx| {
                if this.open_menu.is_some() {
                    return;
                }
                // the rename input inside this column owns its keys; Enter /
                // Escape are handled at the root
                if this.input.read(cx).focus_handle(cx).is_focused(w) {
                    return;
                }
                let i = this.sel_track;
                // bare keys only — Ctrl/Alt chords (Ctrl+S/C/V…) must reach
                // the global handler, not toggle solo/channel/etc.
                if ev.keystroke.modifiers.control || ev.keystroke.modifiers.alt {
                    return;
                }
                match ev.keystroke.key.as_str() {
                    "up" => this.track_step(-1, cx),
                    "down" => this.track_step(1, cx),
                    "home" => {
                        this.sel_track = 0;
                        cx.notify();
                    }
                    "end" => {
                        let n = this.doc(|d| d.tracks.len());
                        if n > 0 {
                            this.sel_track = n - 1;
                        }
                        cx.notify();
                    }
                    "m" => this.toggle_mute(i),
                    "s" => this.toggle_solo(i),
                    "c" => this.cycle_chan(i),
                    "enter" | "f2" => this.focus_rename(w, cx),
                    _ => return,
                }
                cx.notify();
                cx.stop_propagation();
            }))
            .child(
                div()
                    .px_2()
                    .h(px(26.0))
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(rgb(th.border))
                    .text_size(px(metrics::TEXT_MD))
                    .text_color(rgb(th.text_muted))
                    .child(t(if is_seq {
                        "tracks.header_seq"
                    } else {
                        "tracks.header"
                    })),
            )
            .child(
                // scrollable when a file has more tracks than fit the panel
                div()
                    .id("track-list")
                    .test_support()
                    .role(Role::List)
                    .aria_label(t("tracks.header"))
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .children(track_names.iter().enumerate().map(|(i, name)| {
                        let sel = self.sel_track == i;
                        let muted = muted_set.contains(&i);
                        let soloed = soloed_set.contains(&i);
                        let color = th.track_colors[i % th.track_colors.len()];
                        // screen-reader description: channel + mute/solo state
                        let mut desc = tf(
                            "a11y.ch",
                            &[(
                                "ch",
                                (track_chs.get(i).copied().unwrap_or(0) + 1)
                                    .to_string()
                                    .as_str(),
                            )],
                        );
                        for (on, key) in [(muted, "a11y.muted"), (soloed, "a11y.soloed")] {
                            if on {
                                desc.push_str(", ");
                                desc.push_str(t(key));
                            }
                        }
                        div()
                            .id(("track", i))
                            .test_support()
                            .role(Role::ListBoxOption)
                            .aria_label(name.to_string())
                            .aria_selected(sel)
                            .aria_description(desc)
                            .aria_position_in_set(i + 1)
                            .aria_size_of_set(track_names.len())
                            .flex()
                            .flex_row()
                            .items_center()
                            .h(px(22.0))
                            .px_1()
                            .cursor_pointer()
                            .bg(if sel {
                                rgb(theme::current().bg_row_sel)
                            } else {
                                rgb(theme::current().bg_row)
                            })
                            .hover(|s| s.bg(rgb(theme::current().bg_row_hover)))
                            .on_click(cx.listener(move |v, _e, w, cx| {
                                // format 2: this click also picks the
                                // sequence being viewed/played
                                v.select_track(i, cx);
                                w.focus(&v.tracks_fh, cx);
                            }))
                            .child(div().w(px(10.0)).h(px(10.0)).rounded_sm().bg(rgb(if muted {
                                theme::current().swatch_off
                            } else {
                                color
                            })))
                            .child(
                                div()
                                    .flex_1()
                                    .px_1()
                                    .text_size(px(metrics::TEXT_MD))
                                    .text_color(rgb(if muted {
                                        th.text_muted_name
                                    } else {
                                        th.text
                                    }))
                                    .when(muted, |s| s.italic())
                                    .overflow_hidden()
                                    .child(name.to_string()),
                            )
                            .child(
                                // record arm — DAW per-track arm state,
                                // separate from transport Record (#159)
                                div()
                                    .id(("arm", i))
                                    .test_support()
                                    .role(Role::CheckBox)
                                    .aria_label(tf("a11y.arm", &[("track", name.as_str())]))
                                    .aria_toggled(if self.armed_track == Some(i) {
                                        Toggled::True
                                    } else {
                                        Toggled::False
                                    })
                                    .px_1()
                                    .text_size(px(9.0))
                                    .text_color(rgb(if self.armed_track == Some(i) {
                                        theme::current().danger
                                    } else {
                                        theme::current().text_muted_name
                                    }))
                                    .on_click(cx.listener(move |v, _e: &ClickEvent, w, cx| {
                                        cx.stop_propagation();
                                        v.sel_track = i;
                                        v.toggle_arm();
                                        w.focus(&v.tracks_fh, cx);
                                        cx.notify();
                                    }))
                                    .child("R"),
                            )
                            .child(
                                div()
                                    .id(("mute", i))
                                    .test_support()
                                    .role(Role::CheckBox)
                                    .aria_label(tf("a11y.mute", &[("track", name.as_str())]))
                                    .aria_toggled(if muted {
                                        Toggled::True
                                    } else {
                                        Toggled::False
                                    })
                                    .px_1()
                                    .text_size(px(9.0))
                                    .text_color(rgb(if muted {
                                        theme::current().warn_alt
                                    } else {
                                        theme::current().text_muted_name
                                    }))
                                    .on_click(cx.listener(move |v, _e: &ClickEvent, w, cx| {
                                        cx.stop_propagation();
                                        v.toggle_mute(i);
                                        w.focus(&v.tracks_fh, cx);
                                        cx.notify();
                                    }))
                                    .child("M"),
                            )
                            .child(
                                div()
                                    .id(("solo", i))
                                    .test_support()
                                    .role(Role::CheckBox)
                                    .aria_label(tf("a11y.solo", &[("track", name.as_str())]))
                                    .aria_toggled(if soloed {
                                        Toggled::True
                                    } else {
                                        Toggled::False
                                    })
                                    .px_1()
                                    .text_size(px(9.0))
                                    .text_color(rgb(if soloed {
                                        theme::current().warn
                                    } else {
                                        theme::current().text_muted_name
                                    }))
                                    .on_click(cx.listener(move |v, _e: &ClickEvent, w, cx| {
                                        cx.stop_propagation();
                                        v.toggle_solo(i);
                                        w.focus(&v.tracks_fh, cx);
                                        cx.notify();
                                    }))
                                    .child("S"),
                            )
                            .child(
                                div()
                                    .id(("ch", i))
                                    .test_support()
                                    .role(Role::Button)
                                    .aria_label(tf(
                                        "a11y.track_ch",
                                        &[(
                                            "ch",
                                            (self.edit_channel_of(
                                                i,
                                                track_chs.get(i).copied().unwrap_or(0),
                                            ) + 1)
                                                .to_string()
                                                .as_str(),
                                        )],
                                    ))
                                    .px_1()
                                    .text_size(px(9.0))
                                    .text_color(rgb(theme::current().ch_text))
                                    .on_click(cx.listener(move |v, _e: &ClickEvent, w, cx| {
                                        cx.stop_propagation();
                                        v.cycle_chan(i);
                                        w.focus(&v.tracks_fh, cx);
                                        cx.notify();
                                    }))
                                    .child(format!(
                                        "c{}",
                                        self.edit_channel_of(
                                            i,
                                            track_chs.get(i).copied().unwrap_or(0),
                                        ) + 1
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
                    .border_color(rgb(theme::current().border))
                    .child(div().flex_1().min_w(px(0.0)).child(Input::new(&self.input)))
                    .child(Self::chip(
                        "rename",
                        t("ui.rename_chip"),
                        t("a11y.rename"),
                        cx,
                        |v, _e, cx| {
                            v.apply_rename(cx);
                            cx.notify();
                        },
                    )),
            );

        // stacked controller lanes: each panel has a header strip (mode
        // chip, key filter, add/remove, collapse — and the resize handle)
        // plus a body canvas; lanes share the selection, playhead and
        // scroll so a drag or the cursor lines up across them
        let lane_panels: Vec<AnyElement> = {
            let lanes = self.lanes.clone();
            let scale_a11y = window.scale_factor();
            lanes
                .iter()
                .enumerate()
                .map(|(li, cfg)| {
                    self.lane_panel(
                        li,
                        *cfg,
                        area,
                        play_x_tick,
                        playing,
                        scroll_x,
                        zoom,
                        track_names.clone(),
                        scale_a11y,
                        &mut *cx,
                    )
                    .into_any_element()
                })
                .collect()
        };
        // the stack is one focus region — clicks pick `lane_focus`, the
        // shared lane_fh keeps Tab traversal and these keys stable
        let lanes_stack = div()
            .id("lanes")
            .test_support()
            .role(Role::Group)
            .aria_label(tf(
                "a11y.lane",
                &[("mode", self.lane_mode().label().as_str())],
            ))
            .w_full()
            .flex_col()
            .bg(rgb(theme::current().bg_lane))
            .border_t_1()
            .border_color(rgb(if area == FocusArea::Lane {
                theme::current().accent
            } else {
                theme::current().border
            }))
            .track_focus(&self.lane_fh)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _w, cx| {
                if this.open_menu.is_some() {
                    return;
                }
                // bare keys only — Ctrl/Alt chords belong to the global
                // handler (Ctrl+V must paste, not cycle)
                if ev.keystroke.modifiers.control || ev.keystroke.modifiers.alt {
                    return;
                }
                let shift = ev.keystroke.modifiers.shift;
                match (shift, ev.keystroke.key.as_str()) {
                    // left/right share the roll's edit cursor
                    (false, "left") => this.cursor_move(-this.snap_ticks(), 0, cx),
                    (false, "right") => this.cursor_move(this.snap_ticks(), 0, cx),
                    (true, "left") => this.cursor_move(-1, 0, cx),
                    (true, "right") => this.cursor_move(1, 0, cx),
                    // up/down edit velocities of selected notes
                    (false, "up") => this.nudge_vel(8, cx),
                    (false, "down") => this.nudge_vel(-8, cx),
                    (true, "up") => this.nudge_vel(1, cx),
                    (true, "down") => this.nudge_vel(-1, cx),
                    (false, "v") => {
                        let m = this.lane_mode().cycle();
                        this.set_lane(m, cx);
                    }
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .children(lane_panels);

        // seek ruler: bar ticks/numbers, click positions the playhead
        // minimap: whole-song overview with viewport rectangle
        let mini_bounds_cell = self.mini_bounds.clone();
        let mini_notes = self.notes.clone();
        let mini_active = self.sel_track;
        let roll_bounds_cell = self.roll_bounds.clone();
        let song_end = self.doc_end_ticks();
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
                    let c = th.track_colors[n.track % th.track_colors.len()];
                    let c = if n.track == mini_active {
                        c
                    } else {
                        blend(c, theme::current().mini_dim, 0.55)
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
                    rgba(theme::current().viewport_fill),
                ));
                window.paint_quad(fill(
                    Bounds::new(point(vx, bounds.origin.y), size(vw, px(1.0))),
                    rgb(theme::current().accent_dim),
                ));
                window.paint_quad(fill(
                    Bounds::new(point(vx, bounds.origin.y + px(h - 1.0)), size(vw, px(1.0))),
                    rgb(theme::current().accent_dim),
                ));
                // playhead
                let pxx = bounds.origin.x + px(mini_play as f32 * sx);
                window.paint_quad(fill(
                    Bounds::new(point(pxx, bounds.origin.y), size(px(1.0), px(h))),
                    rgb(theme::current().ok),
                ));
            },
        );

        let ruler_bounds_cell = self.ruler_bounds.clone();
        let ruler_play_tick = playhead_tick;
        let pos_fmt_ruler = pos_fmt.clone();
        let (loop_s, loop_e) = {
            let sh = crate::lock_shared(&self.shared);
            (sh.loop_start, sh.loop_end)
        };
        let ruler = canvas(
            move |bounds, _window, _cx| {
                ruler_bounds_cell.set(bounds);
            },
            move |bounds, _state, window, _cx| {
                let w = bounds.size.width;
                // coarse ticks: real FF58 bar lines for metrical (meter
                // changes make bars variable), one second for SMPTE
                let tick0 = (scroll_x / zoom).max(0.0) as u64;
                match &pos_fmt_ruler {
                    PositionFormat::Bbt(mm) => {
                        let tick1 = tick0 + (f32::from(w) / zoom) as u64 + mm.bar_ticks_at(tick0);
                        for t in mm.bar_starts_between(tick0, tick1) {
                            let x = bounds.origin.x + px(t as f32 * zoom - scroll_x);
                            window.paint_quad(fill(
                                Bounds::new(
                                    point(x, bounds.origin.y + px(12.0)),
                                    size(px(1.0), px(8.0)),
                                ),
                                rgb(theme::current().state_off),
                            ));
                        }
                    }
                    PositionFormat::Smpte(_) => {
                        let tick1 = tick0 + (f32::from(w) / zoom) as u64 + grid_major;
                        let mut t = tick0 / grid_major * grid_major;
                        while t <= tick1 {
                            let x = bounds.origin.x + px(t as f32 * zoom - scroll_x);
                            window.paint_quad(fill(
                                Bounds::new(
                                    point(x, bounds.origin.y + px(12.0)),
                                    size(px(1.0), px(8.0)),
                                ),
                                rgb(theme::current().state_off),
                            ));
                            t += grid_major;
                        }
                    }
                }
                // explicit loop locators (#130): range band + edge handles
                if let (Some(ls), Some(le)) = (loop_s, loop_e) {
                    if le > ls {
                        let x0 = bounds.origin.x + px(ls as f32 * zoom - scroll_x);
                        let x1 = bounds.origin.x + px(le as f32 * zoom - scroll_x);
                        window.paint_quad(fill(
                            Bounds::new(
                                point(x0, bounds.origin.y),
                                size(px(f32::from(x1 - x0).max(0.0)), px(4.0)),
                            ),
                            rgb(theme::current().accent),
                        ));
                    }
                }
                for lt in [loop_s, loop_e].into_iter().flatten() {
                    let x = bounds.origin.x + px(lt as f32 * zoom - scroll_x);
                    if x >= bounds.origin.x - px(4.0) && x <= bounds.origin.x + w + px(4.0) {
                        window.paint_quad(fill(
                            Bounds::new(
                                point(x - px(3.0), bounds.origin.y),
                                size(px(6.0), px(8.0)),
                            ),
                            rgb(theme::current().accent),
                        ));
                    }
                }
                // playhead marker
                let hx = bounds.origin.x + px(ruler_play_tick as f32 * zoom - scroll_x);
                if hx >= bounds.origin.x && hx <= bounds.origin.x + w {
                    window.paint_quad(fill(
                        Bounds::new(
                            point(hx - px(2.0), bounds.origin.y),
                            size(px(4.0), px(12.0)),
                        ),
                        rgb(theme::current().ok),
                    ));
                }
            },
        );

        let body = body.child(track_col).child(
            div()
                .id("timeline")
                .test_support()
                .flex_1()
                .h_full()
                .flex()
                .flex_col()
                // one wheel handler for the whole timeline (roll, ruler,
                // minimap, lane) — the strips share the same scroll offset
                .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _w, cx| {
                    let d = ev.delta.pixel_delta(px(20.0));
                    if ev.modifiers.control && ev.modifiers.shift {
                        // vertical zoom around the cursor: the row under it
                        // stays put (same anchor math as the horizontal zoom)
                        let b = this.roll_bounds.get();
                        let off = (f32::from(ev.position.y) - f32::from(b.origin.y))
                            .clamp(0.0, f32::from(b.size.height));
                        let anchor_row = (off + this.scroll_y) / this.note_h;
                        this.note_h = (this.note_h * (1.0 - d.y.to_f64() as f32 * 0.002))
                            .clamp(NOTE_H_MIN, NOTE_H_MAX);
                        this.scroll_y = (anchor_row * this.note_h - off).max(0.0);
                    } else if ev.modifiers.control {
                        // zoom around the cursor: the tick under it stays put
                        let b = this.roll_bounds.get();
                        let off = (f32::from(ev.position.x) - f32::from(b.origin.x))
                            .clamp(0.0, f32::from(b.size.width));
                        let anchor_tick = (off + this.scroll_x) / this.zoom;
                        this.zoom = (this.zoom * (1.0 - d.y.to_f64() as f32 * 0.002))
                            .clamp(ZOOM_MIN, ZOOM_MAX);
                        this.scroll_x = (anchor_tick * this.zoom - off).max(0.0);
                    } else {
                        this.scroll_x = (this.scroll_x + d.x.to_f64() as f32).max(0.0);
                        this.scroll_y = (this.scroll_y + d.y.to_f64() as f32).max(0.0);
                        // manual pan pauses playhead-follow briefly
                        this.follow_hold = Some(std::time::Instant::now() + FOLLOW_HOLD);
                    }
                    this.clamp_scroll();
                    cx.notify();
                }))
                .child(
                    div()
                        .id("minimap")
                        .test_support()
                        .role(Role::Slider)
                        .aria_label(t("a11y.minimap"))
                        .aria_numeric_value(playhead_tick as f64)
                        .aria_min_numeric_value(0.0)
                        .aria_max_numeric_value(song_end as f64)
                        .aria_orientation(Orientation::Horizontal)
                        .on_a11y_action(AccessibleAction::Increment, {
                            let this = cx.entity().downgrade();
                            move |_, _, cx| {
                                this.update(cx, |this, cx| this.seek_bars(1, cx)).ok();
                            }
                        })
                        .on_a11y_action(AccessibleAction::Decrement, {
                            let this = cx.entity().downgrade();
                            move |_, _, cx| {
                                this.update(cx, |this, cx| this.seek_bars(-1, cx)).ok();
                            }
                        })
                        .h(px(20.0))
                        .w_full()
                        .pl(px(ROLL_GUTTER))
                        .pr(px(1.0))
                        .bg(rgb(theme::current().bg_canvas))
                        .border_b_1()
                        .border_color(rgb(theme::current().border))
                        .cursor_pointer()
                        .child(minimap.size_full())
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, ev: &MouseDownEvent, w, cx| {
                                w.focus(&this.roll_fh, cx);
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
                        .id("ruler")
                        .test_support()
                        .role(Role::Slider)
                        .aria_label(t("a11y.ruler"))
                        .aria_numeric_value(playhead_tick as f64)
                        .aria_min_numeric_value(0.0)
                        .aria_max_numeric_value(song_end as f64)
                        .aria_orientation(Orientation::Horizontal)
                        .on_a11y_action(AccessibleAction::Increment, {
                            let this = cx.entity().downgrade();
                            move |_, _, cx| {
                                this.update(cx, |this, cx| this.seek_bars(1, cx)).ok();
                            }
                        })
                        .on_a11y_action(AccessibleAction::Decrement, {
                            let this = cx.entity().downgrade();
                            move |_, _, cx| {
                                this.update(cx, |this, cx| this.seek_bars(-1, cx)).ok();
                            }
                        })
                        .h(px(26.0))
                        .w_full()
                        .pl(px(ROLL_GUTTER))
                        .pr(px(1.0))
                        .bg(rgb(theme::current().bg_panel))
                        .border_b_1()
                        .border_color(rgb(theme::current().border))
                        .cursor_pointer()
                        .child(ruler.size_full())
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, ev: &MouseDownEvent, w, cx| {
                                w.focus(&this.roll_fh, cx);
                                let b = this.ruler_bounds.get();
                                let x = f32::from(ev.position.x) - f32::from(b.origin.x);
                                let tick = ((x + this.scroll_x) / this.zoom).max(0.0) as u64;
                                // grabbing a loop locator edge moves the
                                // bound instead of seeking (#130)
                                let near = |lt: Option<u64>| {
                                    lt.is_some_and(|t| {
                                        (t as f32 * this.zoom - this.scroll_x - x).abs() <= 6.0
                                    })
                                };
                                let (ls, le) = {
                                    let sh = crate::lock_shared(&this.shared);
                                    (sh.loop_start, sh.loop_end)
                                };
                                if near(le) {
                                    this.loop_drag = Some(crate::LoopDrag::End);
                                    cx.notify();
                                    return;
                                }
                                if near(ls) {
                                    this.loop_drag = Some(crate::LoopDrag::Start);
                                    cx.notify();
                                    return;
                                }
                                // double-click on the ruler plays from that bar position
                                this.seek_to_tick(tick, ev.click_count == 2, cx);
                            }),
                        )
                        .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, cx| {
                            // locator drag wins over scrub and works while
                            // transport runs — the patch lands on mouse-up
                            if let Some(which) = this.loop_drag {
                                if ev.pressed_button != Some(MouseButton::Left) {
                                    this.loop_drag = None;
                                    return;
                                }
                                let b = this.ruler_bounds.get();
                                let x = f32::from(ev.position.x) - f32::from(b.origin.x);
                                let tick = ((x + this.scroll_x) / this.zoom).max(0.0) as u64;
                                {
                                    let mut sh = crate::lock_shared(&this.shared);
                                    match which {
                                        crate::LoopDrag::Start => {
                                            sh.loop_start = Some(crate::nav::clamp_loop_locator(
                                                tick,
                                                sh.loop_end,
                                                true,
                                            ));
                                        }
                                        crate::LoopDrag::End => {
                                            sh.loop_end = Some(crate::nav::clamp_loop_locator(
                                                tick,
                                                sh.loop_start,
                                                false,
                                            ));
                                        }
                                    }
                                }
                                cx.notify();
                                return;
                            }
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
                        }))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _ev: &MouseUpEvent, _w, cx| {
                                if this.loop_drag.take().is_some() {
                                    this.persist();
                                    this.refresh_live_schedule();
                                    cx.notify();
                                }
                            }),
                        )
                        .on_mouse_up_out(
                            MouseButton::Left,
                            cx.listener(|this, _ev: &MouseUpEvent, _w, cx| {
                                if this.loop_drag.take().is_some() {
                                    this.persist();
                                    this.refresh_live_schedule();
                                    cx.notify();
                                }
                            }),
                        ),
                )
                // marker/lyric strip — meta 0x06/0x05 shown at their tick;
                // click selects + seeks (then `e` edits, `Del` removes)
                .child(
                    div()
                        .h(px(14.0))
                        .w_full()
                        .relative()
                        .overflow_hidden()
                        .bg(rgb(theme::current().bg_panel))
                        .children(markers.iter().enumerate().filter_map(
                            |(i, (tk, id, ti, txt))| {
                                let (tk, id, ti) = (*tk, *id, *ti);
                                // markers are absolute inside a full-width
                                // strip — `pl` doesn't shift them, so the
                                // roll's left offset is added explicitly
                                let x = tk as f32 * zoom - scroll_x + ROLL_GUTTER;
                                let sel = self.meta_sel == Some((ti, id));
                                (x > -80.0).then(|| {
                                    div()
                                        .id(("mark", i))
                                        .absolute()
                                        .left(px(x))
                                        .top(px(0.0))
                                        .text_size(px(9.0))
                                        .text_color(if sel {
                                            rgb(theme::current().text_bright)
                                        } else {
                                            rgb(theme::current().accent)
                                        })
                                        .whitespace_nowrap()
                                        .cursor_pointer()
                                        .child(txt.clone())
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(
                                                move |this, ev: &MouseDownEvent, _w, cx| {
                                                    cx.stop_propagation();
                                                    this.meta_sel = Some((ti, id));
                                                    this.seek_to_tick(tk, false, cx);
                                                    this.mouse_pos = Some(ev.position);
                                                },
                                            ),
                                        )
                                })
                            },
                        )),
                )
                .child({
                    // a11y snapshot inputs for the piano-roll synthetic
                    // subtree (note/selection/playhead/marquee nodes)
                    let roll_bounds_a11y = self.roll_bounds.clone();
                    let notes_a11y = self.notes.clone();
                    let selection_a11y = self.selection.clone();
                    let track_names_a11y = track_names.clone();
                    let scale_a11y = window.scale_factor();
                    let ppq = self.ppq();
                    let pos_a11y = self.doc(|d| d.position_format_for(self.sel_track));
                    let mc_off = self.mc_off();
                    div()
                        .id("piano-roll")
                        .test_support()
                        .role(Role::Group)
                        .aria_label(t("a11y.piano_roll"))
                        .aria_description(tf(
                            "a11y.roll_desc",
                            &[
                                ("n", self.notes.len().to_string().as_str()),
                                ("sel", self.selection.len().to_string().as_str()),
                            ],
                        ))
                        .a11y_synthetic_children(move |b| {
                            a11y::RollA11y {
                                bounds: roll_bounds_a11y.get(),
                                mc_off,
                                scale: scale_a11y,
                                scroll_x,
                                scroll_y,
                                zoom,
                                ppq,
                                pos: pos_a11y,
                                notes: notes_a11y,
                                selection: selection_a11y,
                                track_names: track_names_a11y,
                                drag,
                                marquee,
                                playhead_tick,
                            }
                            .build(b);
                        })
                        .flex_1()
                        // a bare div is display:block — flex_row alone
                        // leaves children stacked vertically (rolled the
                        // canvas below the fold); display:flex is required
                        .flex()
                        .flex_row()
                        .relative()
                        .overflow_hidden()
                        // piano-key strip — click or scrub to audition the
                        // pitch through the selected track's routing
                        .child(
                            div()
                                .w(px(KBD_W))
                                .h_full()
                                .relative()
                                .overflow_hidden()
                                .bg(rgb(theme::current().bg_panel))
                                .border_r_1()
                                .border_color(rgb(theme::current().border))
                                .cursor_pointer()
                                .child(kbd.size_full())
                                .children((0..strip_keys.len() as i32).filter_map(|r| {
                                    let k = strip_keys[r as usize];
                                    if !k.is_multiple_of(12) {
                                        return None;
                                    }
                                    let y = r as f32 * note_h - scroll_y + (note_h - 8.0) / 2.0;
                                    (y > -12.0).then(|| {
                                        div()
                                            .absolute()
                                            .right(px(2.0))
                                            .top(px(y))
                                            .text_size(px(7.0))
                                            .text_color(rgb(theme::current().text_dim))
                                            .child(format!(
                                                "C{}",
                                                k as i32 / 12 - 1 + self.mc_off() as i32
                                            ))
                                    })
                                }))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, ev: &MouseDownEvent, w, cx| {
                                        cx.stop_propagation();
                                        w.focus(&this.roll_fh, cx);
                                        this.mouse_pos = Some(ev.position);
                                        if let Some(k) = this.kbd_key(ev.position) {
                                            let ch = this.sel_track_ch();
                                            let tr = this.sel_track;
                                            let vel = this.aud_vel;
                                            let at =
                                                this.doc(|d| d.tempo_map.us_to_tick(this.play_us));
                                            this.scrub_key = Some(k);
                                            this.audition_strike(tr, ch, k, vel, at);
                                            cx.notify();
                                        }
                                    }),
                                )
                                .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, cx| {
                                    if this.scrub_key.is_none() {
                                        return;
                                    }
                                    cx.stop_propagation();
                                    if ev.pressed_button != Some(MouseButton::Left) {
                                        this.audition_off();
                                        cx.notify();
                                        return;
                                    }
                                    if let Some(k) = this.kbd_key(ev.position) {
                                        if this.scrub_key != Some(k) {
                                            this.scrub_key = Some(k);
                                            let ch = this.sel_track_ch();
                                            let tr = this.sel_track;
                                            let vel = this.aud_vel;
                                            let at =
                                                this.doc(|d| d.tempo_map.us_to_tick(this.play_us));
                                            this.audition_strike(tr, ch, k, vel, at);
                                        }
                                        cx.notify();
                                    }
                                }))
                                .on_mouse_up(
                                    MouseButton::Left,
                                    cx.listener(|this, _ev: &MouseUpEvent, _w, cx| {
                                        if this.scrub_key.is_some() {
                                            this.audition_off();
                                            cx.notify();
                                        }
                                    }),
                                )
                                .on_mouse_up_out(
                                    MouseButton::Left,
                                    cx.listener(|this, _ev: &MouseUpEvent, _w, cx| {
                                        if this.scrub_key.is_some() {
                                            this.audition_off();
                                            cx.notify();
                                        }
                                    }),
                                ),
                        )
                        .child(
                            div()
                                .flex_1()
                                .h_full()
                                .relative()
                                .overflow_hidden()
                                .border_1()
                                .border_color(if area == FocusArea::Roll {
                                    rgb(theme::current().accent)
                                } else {
                                    rgba(0x00000000)
                                })
                                .track_focus(&self.roll_fh)
                                .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _w, cx| {
                                    if this.open_menu.is_some() {
                                        return;
                                    }
                                    if ev.keystroke.key == "enter" {
                                        this.cursor_activate(cx);
                                        cx.stop_propagation();
                                    }
                                }))
                                .child(roll.size_full())
                                // drum view: GM names on the folded rows'
                                // left edge
                                .children(if self.drum {
                                    let h = f32::from(self.roll_bounds.get().size.height);
                                    let r0 = (self.scroll_y / self.note_h).max(0.0) as i32;
                                    let r1 = ((self.scroll_y + h) / self.note_h + 1.0)
                                        .min(self.vis_keys.len() as f32)
                                        as i32;
                                    (r0..r1)
                                        .map(|r| {
                                            let key = self.vis_keys[r as usize];
                                            let y = r as f32 * self.note_h - self.scroll_y
                                                + (self.note_h - 8.0) / 2.0;
                                            div()
                                                .absolute()
                                                .left(px(2.0))
                                                .top(px(y))
                                                .text_size(px(8.0))
                                                .text_color(rgb(theme::current().text))
                                                .whitespace_nowrap()
                                                .child(
                                                    drum_name(key)
                                                        .map(|s| s.to_string())
                                                        .unwrap_or_else(|| key.to_string()),
                                                )
                                        })
                                        .collect::<Vec<_>>()
                                } else {
                                    Vec::new()
                                }),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, ev: &MouseDownEvent, w, cx| {
                                w.focus(&this.roll_fh, cx);
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
                                        aud_vel: 0,
                                        aud_ch: 0,
                                        lane: 0,
                                    });
                                    cx.notify();
                                    return;
                                }
                                if let Some(n) = this.edge_at(ev.position) {
                                    this.sel_track = n.track;
                                    this.audition_strike(
                                        n.track,
                                        n.channel,
                                        n.key,
                                        n.vel,
                                        n.start_tick,
                                    );
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
                                        aud_vel: n.vel,
                                        aud_ch: n.channel,
                                        lane: 0,
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
                                    this.audition_strike(
                                        n.track,
                                        n.channel,
                                        n.key,
                                        n.vel,
                                        n.start_tick,
                                    );
                                    this.sel_events.clear();
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
                                        lane: 0,
                                        orig_end: n.end_tick,
                                        orig_key: n.key,
                                        dtick: 0,
                                        dkey: 0,
                                        a_tick: 0,
                                        a_key: 0,
                                        b_tick: 0,
                                        b_key: 0,
                                        aud_vel: n.vel,
                                        aud_ch: n.channel,
                                    });
                                } else {
                                    let (tick, key) = this.hit(ev.position);
                                    if (0..=127).contains(&key) {
                                        if !shift {
                                            this.selection.clear();
                                        }
                                        // draw tool auditions the pitch under the
                                        // anchor; scrub changes preview on update_drag
                                        let aud_ch = this.sel_track_ch();
                                        let aud_vel = this.aud_vel;
                                        if this.tool == Tool::Draw {
                                            this.audition_strike(
                                                this.sel_track,
                                                aud_ch,
                                                key as u8,
                                                aud_vel,
                                                tick.max(0) as u64,
                                            );
                                        }
                                        // becomes a marquee on drag; a click without
                                        // drag inserts a note at the anchor
                                        this.drag = Some(Drag {
                                            mode: DragMode::Marquee,
                                            on_id: 0,
                                            off_id: None,
                                            track: this.sel_track,
                                            orig_start: 0,
                                            orig_end: None,
                                            orig_key: 0,
                                            dtick: 0,
                                            dkey: 0,
                                            a_tick: tick,
                                            a_key: key,
                                            b_tick: tick,
                                            b_key: key,
                                            aud_vel,
                                            aud_ch,
                                            lane: 0,
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
                })
                .child(lanes_stack),
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
                Some(PluginState::Ready { .. }) => (
                    "●",
                    theme::current().lcd,
                    t("plugin.state_ready").to_string(),
                ),
                Some(PluginState::Loading { .. }) => (
                    "◌ …",
                    theme::current().warn,
                    t("plugin.state_loading").to_string(),
                ),
                Some(PluginState::Failed { phase, msg, .. }) => {
                    ("✕", theme::current().danger, format!("{phase}: {msg}"))
                }
                _ => (
                    "",
                    theme::current().text_muted,
                    t("plugin.state_idle").to_string(),
                ),
            };
            Some(
                div()
                    .id("plugin-chip")
                    .test_support()
                    .role(Role::Button)
                    .aria_label(format!("{name}, {tip}"))
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
            .id("status-bar")
            .test_support()
            .role(Role::ContentInfo)
            .aria_label(t("a11y.status"))
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .h(px(metrics::MENU_ROW))
            .bg(rgb(th.bg_bar))
            .border_t_1()
            .border_color(rgb(th.border))
            .text_size(px(metrics::TEXT_MD))
            .child(
                // live region: UIA announces aria_label changes politely —
                // `status` only changes on real events, so no flooding
                div()
                    .id("status")
                    .test_support()
                    .role(Role::Status)
                    .aria_label(self.status.to_string())
                    .a11y_synthetic_children(|b| {
                        b.parent_node().set_live(accesskit::Live::Polite);
                    })
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_color(rgb(theme::current().text_muted))
                    .child(format!("{}", self.status)),
            )
            .children(plugin_chip)
            // MCP auth posture — an unauthenticated endpoint must be visible
            .child({
                let (label, color) = match mcp_auth_mode {
                    mcp_server::McpAuthMode::Bearer => (t("status.mcp_auth"), theme::current().lcd),
                    mcp_server::McpAuthMode::Open => {
                        (t("status.mcp_open"), theme::current().danger)
                    }
                    mcp_server::McpAuthMode::Stdio => {
                        (t("status.mcp_off"), theme::current().text_muted)
                    }
                };
                div()
                    .id("mcp-auth-chip")
                    .px_1()
                    .text_color(rgb(color))
                    .child(label)
                    .tooltip(move |_w, cx| {
                        let tip = mcp_auth_detail.clone();
                        cx.new(|_| Tip(tip.into())).into()
                    })
            })
            .child(
                div()
                    .text_color(rgb(theme::current().text_muted))
                    .whitespace_nowrap()
                    .child(format!("{}: {}", t("focus.label"), t(area.key()))),
            )
            .child(
                // live monitor scale — proves PerMonitorV2 at runtime (a
                // bitmap-stretched app would always report 100%)
                div()
                    .text_color(rgb(theme::current().text_muted))
                    .whitespace_nowrap()
                    .child(format!(
                        "{}: {}%",
                        t("ui.scale"),
                        (window.scale_factor() * 100.0).round() as i32
                    )),
            )
            .child(Self::chip(
                "st-lane",
                self.lane_mode().label(),
                tf(
                    "a11y.lane_mode",
                    &[("mode", self.lane_mode().label().as_str())],
                ),
                cx,
                |v, _e, cx| {
                    let m = v.lane_mode().cycle();
                    v.set_lane(m, cx);
                },
            ))
            .child(Self::chip(
                "st-enc",
                format!("enc {enc_label}"),
                tf("a11y.enc", &[("enc", enc_label.as_str())]),
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
                    .id("status-pos")
                    .test_support()
                    .role(Role::Label)
                    .aria_label(tf("a11y.pos", &[("pos", pos.as_str())]))
                    .text_color(rgb(theme::current().text_muted))
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
            let items: Vec<MenuRow> = match m {
                TopMenu::File => vec![
                    self.mi_cmd("file.new", None, cx),
                    self.mi_cmd("file.open", None, cx),
                    Self::mi_sub("f.recent", t("menu.recent"), Sub::Recent, cx),
                    Self::msep(),
                    self.mi_cmd("file.save", None, cx),
                    self.mi_cmd("file.save_as", None, cx),
                ],
                TopMenu::Edit => vec![
                    self.mi_cmd("edit.undo", None, cx),
                    self.mi_cmd("edit.redo", None, cx),
                    Self::msep(),
                    self.mi_cmd("edit.select_all", None, cx),
                    Self::msep(),
                    self.mi_cmd("edit.cut", None, cx),
                    self.mi_cmd("edit.copy", None, cx),
                    self.mi_cmd("edit.paste", None, cx),
                    self.mi_cmd("edit.duplicate", None, cx),
                    self.mi_cmd("edit.delete", None, cx),
                    Self::mi_sub("e.meta", t("edit.meta"), Sub::Meta, cx),
                    Self::msep(),
                    Self::mi_sub("e.tool", t("edit.tool"), Sub::Tool, cx),
                    Self::mi_sub("e.snap", t("edit.snap"), Sub::Snap, cx),
                    Self::mi_sub("e.notelen", t("edit.note_len"), Sub::NoteLen, cx),
                    Self::mi_sub("e.insvel", t("edit.ins_vel"), Sub::InsVel, cx),
                    Self::msep(),
                    Self::mi_sub("e.quant", t("edit.quantize"), Sub::Quant, cx),
                    self.mi_cmd("edit.transpose_up", None, cx),
                    self.mi_cmd("edit.transpose_dn", None, cx),
                    Self::mi_sub("e.oct", t("edit.octave"), Sub::Oct, cx),
                    Self::msep(),
                    self.mi_cmd("edit.humanize", None, cx),
                    self.mi_cmd("edit.split", None, cx),
                    Self::mi_sub("e.swing", t("edit.swing"), Sub::Swing, cx),
                    self.mi_cmd("edit.join", None, cx),
                    self.mi_cmd("edit.fix_overlaps", None, cx),
                    Self::mi_sub("e.legato", t("edit.legato"), Sub::LegatoGap, cx),
                    Self::mi_sub("e.len", t("edit.set_length"), Sub::LenSet, cx),
                    Self::mi_sub("e.velset", t("edit.set_velocity"), Sub::VelSet, cx),
                    Self::mi_sub("e.relset", t("edit.set_release"), Sub::RelSet, cx),
                    Self::msep(),
                    Self::mi_sub("e.alltrack", t("edit.apply_track"), Sub::AllTrack, cx),
                    Self::msep(),
                    self.mi_cmd("edit.vel_up", None, cx),
                    self.mi_cmd("edit.vel_dn", None, cx),
                ],
                TopMenu::View => vec![
                    self.mi_cmd("view.events", Some(self.show_events), cx),
                    Self::mi_sub("v.evftype", t("view.ev_ftype"), Sub::EvFType, cx),
                    Self::mi_sub("v.evfchan", t("view.ev_fchan"), Sub::EvFChan, cx),
                    Self::mi_sub("v.midc", t("view.middle_c"), Sub::MidC, cx),
                    Self::mi_sub("v.theme", t("view.theme"), Sub::Theme, cx),
                    self.mi_cmd(
                        "view.hc",
                        Some(self.theme == theme::Theme::high_contrast()),
                        cx,
                    ),
                    Self::msep(),
                    self.mi_cmd("view.zoom_in", None, cx),
                    self.mi_cmd("view.zoom_out", None, cx),
                    self.mi_cmd("view.zoom_reset", None, cx),
                    Self::msep(),
                    self.mi_cmd("view.follow_off", Some(self.follow == Follow::Off), cx),
                    self.mi_cmd("view.follow_page", Some(self.follow == Follow::Page), cx),
                    self.mi_cmd(
                        "view.follow_smooth",
                        Some(self.follow == Follow::Smooth),
                        cx,
                    ),
                    Self::msep(),
                    self.mi_cmd("view.zoom_sel", None, cx),
                    self.mi_cmd("view.zoom_song", None, cx),
                    Self::msep(),
                    self.mi_cmd("view.go_playhead", None, cx),
                    self.mi_cmd("view.marker_prev", None, cx),
                    self.mi_cmd("view.marker_next", None, cx),
                    self.mi_cmd("view.event_prev", None, cx),
                    self.mi_cmd("view.event_next", None, cx),
                    Self::msep(),
                    Self::mi_sub("v.rowh", t("view.row_height"), Sub::RowH, cx),
                    self.mi_cmd("view.fold", Some(self.fold), cx),
                    self.mi_cmd("view.drum", Some(self.drum), cx),
                    Self::mi_sub("v.scale", t("view.scale"), Sub::Scale, cx),
                    Self::msep(),
                    Self::mi_sub("v.lane", t("view.lane"), Sub::Lane, cx),
                    Self::mi_sub("v.enc", t("view.encoding"), Sub::Enc, cx),
                ],
                TopMenu::Track => {
                    let mut items = vec![
                        self.mi_cmd("track.rename", None, cx),
                        Self::msep(),
                        self.mi_cmd("track.mute", Some(muted_set.contains(&self.sel_track)), cx),
                        self.mi_cmd("track.solo", Some(soloed_set.contains(&self.sel_track)), cx),
                        Self::msep(),
                        Self::mi_sub("t.chan", t("track.channel"), Sub::Chan, cx),
                        Self::mi_sub("t.dest", t("track.dest"), Sub::Dest, cx),
                    ];
                    if sel_is_plugin {
                        items.push(Self::msep());
                        items.push(self.mi_cmd("track.plugin_gui", None, cx));
                    }
                    items
                }
                TopMenu::Output => {
                    let mut items = vec![
                        Self::mi_sub("o.def", t("output.default_dest"), Sub::DefDest, cx),
                        Self::mi_sub("o.in", t("output.midi_in"), Sub::InPort, cx),
                        Self::msep(),
                    ];
                    if sel_is_plugin {
                        items.push(Self::mi(
                            "o.gui",
                            if self.plugin_window.is_some() {
                                t("output.editor_close")
                            } else {
                                t("output.editor_open")
                            },
                            self.keys.shortcut_label("track.plugin_gui"),
                            Some(self.plugin_window.is_some()),
                            cx,
                            |v, _e, _cx| v.open_plugin_gui(),
                        ));
                    }
                    if sel_plugin_failed {
                        items.push(Self::mi(
                            "o.retry",
                            t("output.retry"),
                            self.keys.shortcut_label("output.retry"),
                            None,
                            cx,
                            |v, _e, _cx| {
                                let d = crate::lock_shared(&v.shared).dest_of(v.sel_track);
                                v.ensure_plugin(d, true);
                            },
                        ));
                    }
                    items.extend([
                        Self::msep(),
                        self.mi_cmd("output.rescan", None, cx),
                        Self::mi(
                            "o.rescan_all",
                            t("output.rescan_all"),
                            "",
                            None,
                            cx,
                            |v, _e, cx| {
                                v.rescan_plugins(crate::ScanMode::All);
                                cx.notify();
                            },
                        ),
                        Self::mi(
                            "o.audio",
                            t("output.audio_settings"),
                            "",
                            None,
                            cx,
                            |v, _e, cx| {
                                v.show_output_status = true;
                                cx.notify();
                            },
                        ),
                        self.mi_cmd("output.host_status", None, cx),
                    ]);
                    items
                }
                TopMenu::Transport => vec![
                    self.mi_cmd("transport.play_stop", Some(self.playback.is_some()), cx),
                    self.mi_cmd("transport.pause", None, cx),
                    self.mi_cmd("transport.return_start", None, cx),
                    self.mi_cmd("transport.go_start", None, cx),
                    self.mi_cmd(
                        "transport.return_on_stop",
                        Some(self.return_to_start_on_stop),
                        cx,
                    ),
                    self.mi_cmd("transport.record", Some(self.is_recording()), cx),
                    self.mi_cmd(
                        "rec.arm",
                        Some(self.armed_track == Some(self.sel_track)),
                        cx,
                    ),
                    Self::mi_sub("tr.mon", t("transport.monitor"), Sub::Monitor, cx),
                    self.mi_cmd("transport.loop", Some(loop_en), cx),
                    self.mi_cmd("loop.set_start", None, cx),
                    self.mi_cmd("loop.set_end", None, cx),
                    self.mi_cmd("loop.set_selection", None, cx),
                    self.mi_cmd("loop.clear", None, cx),
                    self.mi_cmd("tempo.edit", None, cx),
                    self.mi_cmd("tempo.delete", None, cx),
                    self.mi_cmd("sig.edit", None, cx),
                    self.mi_cmd("sig.delete", None, cx),
                    self.mi_cmd("transport.met", Some(met_en), cx),
                    Self::mi_sub("tr.metdest", t("transport.met_dest"), Sub::MetDest, cx),
                    self.mi_cmd("transport.chase_sysex", Some(chsy_en), cx),
                    Self::mi(
                        "tr.sxp",
                        tf(
                            "transport.sysex_pol",
                            &[(
                                "mode",
                                t(match sxp {
                                    midi_io::SysexPolicy::Serialize => "transport.sysex_ser",
                                    midi_io::SysexPolicy::Background => "transport.sysex_bg",
                                    midi_io::SysexPolicy::Skip => "transport.sysex_skip",
                                }),
                            )],
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
                    ),
                    Self::mi(
                        "tr.mode",
                        tf(
                            "transport.rec_mode",
                            &[(
                                "mode",
                                t(match self.rec_mode {
                                    RecMode::Overdub => "transport.rec_overdub",
                                    RecMode::Replace => "transport.rec_replace",
                                }),
                            )],
                        ),
                        "",
                        None,
                        cx,
                        |v, _e, _cx| {
                            v.rec_mode = match v.rec_mode {
                                RecMode::Overdub => RecMode::Replace,
                                RecMode::Replace => RecMode::Overdub,
                            };
                            v.persist();
                        },
                    ),
                    Self::mi(
                        "tr.pin",
                        tf(
                            "transport.punch_in",
                            &[("t", playhead_tick.to_string().as_str())],
                        ),
                        "",
                        Some(self.punch_in.is_some()),
                        cx,
                        |v, _e, _cx| {
                            v.punch_in = Some(v.playhead_tick());
                            v.persist();
                        },
                    ),
                    Self::mi(
                        "tr.pout",
                        tf(
                            "transport.punch_out",
                            &[("t", playhead_tick.to_string().as_str())],
                        ),
                        "",
                        Some(self.punch_out.is_some()),
                        cx,
                        |v, _e, _cx| {
                            v.punch_out = Some(v.playhead_tick());
                            v.persist();
                        },
                    ),
                    Self::mi(
                        "tr.pclr",
                        match self.punch_range() {
                            Some((a, b)) => tf(
                                "transport.punch_clear",
                                &[("a", a.to_string().as_str()), ("b", b.to_string().as_str())],
                            ),
                            None => tf("transport.punch_clear", &[("a", "-"), ("b", "-")]),
                        },
                        "",
                        None,
                        cx,
                        |v, _e, _cx| {
                            v.punch_in = None;
                            v.punch_out = None;
                            v.persist();
                        },
                    ),
                    Self::mi(
                        "tr.rtake",
                        t("transport.discard_take"),
                        "",
                        Some(self.rec.is_some()),
                        cx,
                        |v, _e, _cx| {
                            v.discard_record();
                        },
                    ),
                    Self::mi(
                        "tr.qtake",
                        t("transport.quant_take"),
                        "",
                        Some(self.last_take.is_some()),
                        cx,
                        |v, _e, _cx| {
                            v.quantize_last_take();
                        },
                    ),
                    Self::mi_sub("tr.countin", t("transport.count_in"), Sub::CountIn, cx),
                    self.mi_cmd("transport.panic", None, cx),
                    self.mi_cmd("transport.reset_on_stop", Some(self.reset_on_stop), cx),
                    Self::msep(),
                    self.mi_cmd("transport.audition", Some(self.aud_enabled), cx),
                    Self::mi_sub("tr.audv", t("transport.aud_vel"), Sub::AudVel, cx),
                    Self::mi_sub("tr.audd", t("transport.aud_dur"), Sub::AudDur, cx),
                ],
                TopMenu::Help => vec![
                    self.mi_cmd("app.palette", None, cx),
                    self.mi_cmd("app.keys", None, cx),
                    Self::msep(),
                    self.mi_cmd("help.shortcuts", None, cx),
                    self.mi_cmd("help.about", None, cx),
                    self.mi_cmd("help.mcp", None, cx),
                    self.mi_cmd("help.logs", None, cx),
                    self.mi_cmd("help.diag", None, cx),
                ],
            };
            // keep the last-rendered model for keyboard navigation
            self.menu_rows = items.clone();
            // dropdown panel under the clicked label
            let popup_max_h = (f32::from(window.viewport_size().height) - 40.0).max(120.0);
            let popup_h = (items.len() as f32 * metrics::MENU_ROW + 16.0).min(popup_max_h);
            let popup = div()
                .id("menu-popup")
                .test_support()
                .role(Role::Menu)
                .aria_label(t(m.key()))
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
                .bg(rgb(theme::current().bg_raised))
                .border_1()
                .border_color(rgb(theme::current().border))
                .rounded_md()
                .shadow_lg()
                .children(
                    items
                        .iter()
                        .enumerate()
                        .map(|(i, r)| Self::row_el(r, i, self.menu_sel == Some(i), false, cx)),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|_v, _e, _w, cx| cx.stop_propagation()),
                );
            // cascading submenu (also inside the overlay so clicks elsewhere close all)
            let sub_popup = self.open_sub.map(|(s, y)| {
                let x2 = mx + 208.0;
                let rows: Vec<MenuRow> = match s {
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
                                    v.audition_off();
                                    v.apply_tx("set track channel", ops);
                                },
                            )
                        })
                        .collect(),
                    Sub::Dest => self.dest_rows(
                        DestPick::Track,
                        &dests,
                        &port_present,
                        eff_dest,
                        def_dest,
                        has_track_dest,
                        cx,
                    ),
                    Sub::DefDest => self.dest_rows(
                        DestPick::Default,
                        &dests,
                        &port_present,
                        eff_dest,
                        def_dest,
                        has_track_dest,
                        cx,
                    ),
                    Sub::MetDest => self.dest_rows(
                        DestPick::Metronome,
                        &dests,
                        &port_present,
                        eff_dest,
                        def_dest,
                        has_track_dest,
                        cx,
                    ),
                    Sub::Monitor => {
                        let mut rows: Vec<MenuRow> = [
                            crate::recording::MonMode::Off,
                            crate::recording::MonMode::Auto,
                            crate::recording::MonMode::In,
                        ]
                        .iter()
                        .map(|&m| {
                            Self::mi_leaf(
                                ("mon", m.label().len()),
                                t(match m {
                                    crate::recording::MonMode::Off => "mon.off",
                                    crate::recording::MonMode::Auto => "mon.auto",
                                    crate::recording::MonMode::In => "mon.in",
                                }),
                                "",
                                Some(self.monitor == m),
                                cx,
                                move |v, _e, _cx| {
                                    v.monitor = m;
                                    v.update_monitor();
                                    v.save_global();
                                },
                            )
                        })
                        .collect();
                        // SysEx policies (#160): capture into takes and
                        // the thru echo — independent, both visible
                        rows.push(Self::msep());
                        rows.push(Self::mi_leaf(
                            "mon.rec_sx",
                            t("mon.rec_sx"),
                            "",
                            Some(self.rec_sysex),
                            cx,
                            |v, _e, _cx| {
                                v.rec_sysex = !v.rec_sysex;
                                if let Some(r) = v.rec.as_ref() {
                                    r.sx_gate
                                        .store(v.rec_sysex, std::sync::atomic::Ordering::Relaxed);
                                }
                                v.save_global();
                            },
                        ));
                        rows.push(Self::mi_leaf(
                            "mon.echo_sx",
                            t("mon.echo_sx"),
                            "",
                            Some(self.rec_mon_sysex),
                            cx,
                            |v, _e, _cx| {
                                v.rec_mon_sysex = !v.rec_mon_sysex;
                                if let Some(r) = v.rec.as_ref() {
                                    r.mon_sx_gate.store(
                                        v.rec_mon_sysex,
                                        std::sync::atomic::Ordering::Relaxed,
                                    );
                                }
                                v.save_global();
                            },
                        ));
                        rows
                    }
                    Sub::CountIn => [0u8, 1, 2, 4]
                        .iter()
                        .map(|&b| {
                            let label: SharedString = match b {
                                0 => t("countin.off").into(),
                                _ => tf("countin.bars", &[("n", b.to_string().as_str())]).into(),
                            };
                            Self::mi_leaf(
                                ("countin", b as usize),
                                label,
                                "",
                                Some(self.count_in_bars == b),
                                cx,
                                move |v, _e, _cx| {
                                    v.count_in_bars = b;
                                    v.save_global();
                                },
                            )
                        })
                        .collect(),
                    Sub::InPort => {
                        let ports = midi_io::list_inputs().unwrap_or_default();
                        let mut rows: Vec<MenuRow> = vec![Self::mi_leaf(
                            "in.default",
                            t("output.first_input"),
                            "",
                            Some(self.midi_in.is_empty()),
                            cx,
                            |v, _e, _cx| {
                                v.midi_in = "".into();
                                v.save_global();
                            },
                        )];
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
                        }));
                        if ports.is_empty() {
                            rows.push(Self::mi_dis(
                                "in.none",
                                t("output.no_inputs"),
                                "",
                                None,
                                cx,
                                |_, _, _| {},
                            ));
                        }
                        // manual input-latency compensation, cycles presets
                        rows.push(Self::mi_leaf(
                            "in.lat",
                            tf(
                                "output.in_latency",
                                &[("ms", self.in_latency_ms.to_string().as_str())],
                            ),
                            "",
                            None,
                            cx,
                            |v, _e, _cx| {
                                const STEPS: [u64; 7] = [0, 1, 2, 5, 10, 20, 50];
                                let i = STEPS
                                    .iter()
                                    .position(|&s| s == v.in_latency_ms)
                                    .unwrap_or(0);
                                v.in_latency_ms = STEPS[(i + 1) % STEPS.len()];
                                v.save_global();
                            },
                        ));
                        // armed track's input channel filter (#159):
                        // All / 1..16 — SysEx passes unfiltered by design
                        rows.push(Self::msep());
                        rows.push(Self::mhead(t("input.chan")));
                        rows.push(Self::mi_leaf(
                            "inchan.all",
                            t("inchan.all"),
                            "",
                            Some(self.rec_in_ch.is_none()),
                            cx,
                            |v, _e, _cx| {
                                v.rec_in_ch = None;
                                v.save_global();
                            },
                        ));
                        rows.extend((0u8..16).map(|ch| {
                            Self::mi_leaf(
                                ("inchan", ch as usize),
                                format!("{}", ch + 1),
                                "",
                                Some(self.rec_in_ch == Some(ch)),
                                cx,
                                move |v, _e, _cx| {
                                    v.rec_in_ch = Some(ch);
                                    v.save_global();
                                },
                            )
                        }));
                        rows
                    }
                    Sub::Lane => {
                        // mode picks apply to the focused lane; the
                        // add/remove items manage the stack itself
                        let mut rows: Vec<MenuRow> = LANE_MODES
                            .iter()
                            .enumerate()
                            .map(|(i, lm)| {
                                Self::mi_leaf(
                                    ("lane", i),
                                    lm.label(),
                                    "",
                                    Some(self.lane_mode() == *lm),
                                    cx,
                                    move |v, _e, cx| v.set_lane(*lm, cx),
                                )
                            })
                            .collect();
                        rows.push(Self::msep());
                        rows.push(Self::mi_leaf(
                            "lane-add",
                            t("view.lane.add"),
                            "",
                            None,
                            cx,
                            |v, _e, cx| v.add_lane(cx),
                        ));
                        rows.push(Self::mi_leaf(
                            "lane-del",
                            t("view.lane.remove"),
                            "",
                            None,
                            cx,
                            |v, _e, cx| v.remove_lane(cx),
                        ));
                        rows
                    }
                    Sub::Meta => {
                        // insert at the playhead, conductor track — editing
                        // existing metas happens via strip/event-list clicks
                        const METAS: [u8; 7] = [0x06, 0x05, 0x07, 0x01, 0x02, 0x04, 0x59];
                        METAS
                            .iter()
                            .enumerate()
                            .map(|(i, mt)| {
                                let mt = *mt;
                                Self::mi_leaf(
                                    ("meta", i),
                                    t(EditorView::meta_type_label(mt)),
                                    "",
                                    None,
                                    cx,
                                    move |v, _e, cx| {
                                        let tick = v.doc(|d| d.tempo_map.us_to_tick(v.play_us));
                                        // no Window in menu callbacks —
                                        // render opens + focuses next frame
                                        v.meta_pending = Some((0, tick, mt, 0));
                                        cx.notify();
                                    },
                                )
                            })
                            .collect()
                    }
                    Sub::Tool => {
                        let opts = [
                            (Tool::Select, "tool.select"),
                            (Tool::Draw, "tool.draw"),
                            (Tool::Erase, "tool.erase"),
                        ];
                        opts.into_iter()
                            .enumerate()
                            .map(|(i, (tool, label))| {
                                Self::mi_leaf(
                                    ("tool", i),
                                    t(label),
                                    self.keys.shortcut_label(label),
                                    Some(self.tool == tool),
                                    cx,
                                    move |v, _e, cx| v.set_tool(tool, cx),
                                )
                            })
                            .collect()
                    }
                    Sub::RowH => {
                        // taller / shorter / reset + absolute presets — the
                        // wheel does Ctrl+Shift vertical zoom too
                        let mut rows = vec![
                            Self::mi_leaf(
                                "rowh.up",
                                t("view.row_taller"),
                                "Ctrl+Shift+Wheel",
                                None,
                                cx,
                                |v, _e, cx| v.vzoom_by(1.25, cx),
                            ),
                            Self::mi_leaf(
                                "rowh.dn",
                                t("view.row_shorter"),
                                "",
                                None,
                                cx,
                                |v, _e, cx| v.vzoom_by(1.0 / 1.25, cx),
                            ),
                            Self::mi_leaf(
                                "rowh.reset",
                                t("view.row_reset"),
                                "",
                                Some((self.note_h - NOTE_H).abs() < 0.5),
                                cx,
                                |v, _e, cx| {
                                    v.vzoom_set(
                                        NOTE_H,
                                        f32::from(v.roll_bounds.get().size.height) / 2.0,
                                        cx,
                                    );
                                },
                            ),
                            Self::msep(),
                        ];
                        for (i, h) in [8.0f32, 13.0, 18.0, 26.0, 34.0].iter().enumerate() {
                            let h = *h;
                            rows.push(Self::mi_leaf(
                                ("rowh.preset", i),
                                format!("{h}px"),
                                "",
                                Some((self.note_h - h).abs() < 0.5),
                                cx,
                                move |v, _e, cx| {
                                    v.vzoom_set(
                                        h,
                                        f32::from(v.roll_bounds.get().size.height) / 2.0,
                                        cx,
                                    );
                                },
                            ));
                        }
                        rows
                    }
                    Sub::Scale => {
                        const PC_NAMES: [&str; 12] = [
                            "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
                        ];
                        let mut rows = vec![
                            Self::mi_leaf(
                                "scale.off",
                                t("view.scale_off"),
                                "",
                                Some(self.scale_sel == -1),
                                cx,
                                |v, _e, cx| v.set_scale(-1, false, cx),
                            ),
                            Self::mi_leaf(
                                "scale.auto",
                                t("view.scale_auto"),
                                "",
                                Some(self.scale_sel == -2),
                                cx,
                                |v, _e, cx| v.set_scale(-2, false, cx),
                            ),
                            Self::msep(),
                            Self::mi_leaf(
                                "scale.minor",
                                t("view.scale_minor"),
                                "",
                                Some(self.scale_minor),
                                cx,
                                |v, _e, cx| {
                                    let sel = if v.scale_sel >= 0 { v.scale_sel } else { 0 };
                                    v.set_scale(sel, !v.scale_minor, cx);
                                },
                            ),
                            Self::msep(),
                        ];
                        for (i, name) in PC_NAMES.iter().enumerate() {
                            rows.push(Self::mi_leaf(
                                ("scale.root", i),
                                *name,
                                "",
                                Some(self.scale_sel == i as i8),
                                cx,
                                move |v, _e, cx| v.set_scale(i as i8, v.scale_minor, cx),
                            ));
                        }
                        rows
                    }
                    Sub::Snap => SNAPS
                        .iter()
                        .enumerate()
                        .map(|(i, (_div, _trip, label))| {
                            Self::mi_leaf(
                                ("snap", i),
                                snap_label(label, td),
                                "",
                                Some(self.snap_idx == i),
                                cx,
                                move |v, _e, cx| v.set_snap(i, cx),
                            )
                        })
                        .collect(),
                    Sub::Recent => {
                        if self.recent.is_empty() {
                            vec![Self::mi_dis(
                                "recent.empty",
                                t("menu.recent_empty"),
                                "",
                                None,
                                cx,
                                |_v, _e, _cx| {},
                            )]
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
                                        move |v, w, cx| {
                                            v.confirm_discard_or_save(
                                                PendingAction::OpenPath(path.clone()),
                                                w,
                                                cx,
                                            );
                                        },
                                    )
                                })
                                .collect()
                        }
                    }
                    // Quantize submenu (#139): the grid + strength are
                    // visible settings picked before Apply — the toolbar
                    // button and this menu resolve the same state.
                    Sub::Quant => {
                        let mut rows = vec![
                            Self::mi_leaf(
                                "quant.apply",
                                t("quant.apply"),
                                "",
                                None,
                                cx,
                                |v, _e, cx| {
                                    let g = v.quantize_grid();
                                    let st = v.q_str;
                                    v.apply_region_op("quantize", move |d, t, f, to| {
                                        d.quantize_ops(t, f, to, g, st)
                                    });
                                    cx.notify();
                                },
                            ),
                            Self::msep(),
                        ];
                        rows.extend(self.quant_grid_rows(cx));
                        rows.push(Self::msep());
                        rows.extend(self.quant_str_rows(cx));
                        rows
                    }
                    Sub::Oct => {
                        let opts = [(t("edit.oct_up"), 12i32), (t("edit.oct_dn"), -12)];
                        opts.into_iter()
                            .enumerate()
                            .map(|(i, (label, st))| {
                                Self::mi_leaf(("oct", i), label, "", None, cx, move |v, _e, _cx| {
                                    v.apply_region_op("octave", move |d, t, f, to| {
                                        d.transpose_ops(t, f, to, st)
                                    });
                                })
                            })
                            .collect()
                    }
                    Sub::NoteLen => {
                        let opts: Vec<NoteLen> = vec![
                            NoteLen::Grid,
                            NoteLen::LastUsed,
                            NoteLen::Fixed {
                                den: 4,
                                trip: false,
                                dot: false,
                            },
                            NoteLen::Fixed {
                                den: 8,
                                trip: false,
                                dot: false,
                            },
                            NoteLen::Fixed {
                                den: 16,
                                trip: false,
                                dot: false,
                            },
                            NoteLen::Fixed {
                                den: 32,
                                trip: false,
                                dot: false,
                            },
                            NoteLen::Fixed {
                                den: 8,
                                trip: true,
                                dot: false,
                            },
                            NoteLen::Fixed {
                                den: 16,
                                trip: true,
                                dot: false,
                            },
                            NoteLen::Fixed {
                                den: 8,
                                trip: false,
                                dot: true,
                            },
                            NoteLen::Fixed {
                                den: 4,
                                trip: false,
                                dot: true,
                            },
                        ];
                        opts.into_iter()
                            .enumerate()
                            .map(|(i, nl)| {
                                Self::mi_leaf(
                                    ("nlen", i),
                                    nl.label(),
                                    "",
                                    Some(self.note_len == nl),
                                    cx,
                                    move |v, _e, _cx| {
                                        v.note_len = nl;
                                    },
                                )
                            })
                            .collect()
                    }
                    Sub::InsVel => {
                        let mut rows = Vec::new();
                        rows.push(Self::mi_leaf(
                            "ivel.last",
                            tf("ins_vel.last", &[("v", self.last_vel.to_string().as_str())]),
                            "",
                            Some(self.vel_src == VelSrc::LastUsed),
                            cx,
                            |v, _e, _cx| v.vel_src = VelSrc::LastUsed,
                        ));
                        rows.extend([127u8, 112, 96, 80, 64, 48, 32].iter().enumerate().map(
                            |(i, &v_)| {
                                Self::mi_leaf(
                                    ("ivel", i),
                                    format!("{v_}"),
                                    "",
                                    Some(self.vel_src == VelSrc::Fixed(v_)),
                                    cx,
                                    move |v, _e, _cx| v.vel_src = VelSrc::Fixed(v_),
                                )
                            },
                        ));
                        rows
                    }
                    Sub::EvFType => {
                        let mut rows = vec![Self::mi_leaf(
                            "evf.all",
                            t("evf.all"),
                            "",
                            Some(self.ev_filter.kinds.is_empty()),
                            cx,
                            |v, _e, cx| {
                                v.ev_filter.kinds.clear();
                                cx.notify();
                            },
                        )];
                        rows.extend(EvKind::ALL.iter().enumerate().map(|(i, &k)| {
                            let on = self.ev_filter.kinds.is_empty()
                                || self.ev_filter.kinds.contains(&k);
                            Self::mi_leaf(
                                ("evf", i),
                                t(k.i18n()),
                                "",
                                Some(on),
                                cx,
                                move |v, _e, cx| v.toggle_ev_kind(k, cx),
                            )
                        }));
                        rows
                    }
                    Sub::EvFChan => {
                        let mut rows = vec![Self::mi_leaf(
                            "evc.all",
                            t("evf.all"),
                            "",
                            Some(self.ev_filter.chan.is_none()),
                            cx,
                            |v, _e, cx| v.set_ev_chan(None, cx),
                        )];
                        rows.extend((0u8..16).enumerate().map(|(i, c)| {
                            Self::mi_leaf(
                                ("evc", i),
                                format!("ch {}", c + 1),
                                "",
                                Some(self.ev_filter.chan == Some(c)),
                                cx,
                                move |v, _e, cx| v.set_ev_chan(Some(c), cx),
                            )
                        }));
                        rows
                    }
                    Sub::MidC => [3u8, 4, 5]
                        .into_iter()
                        .enumerate()
                        .map(|(i, mc)| {
                            Self::mi_leaf(
                                ("midc", i),
                                format!("C{mc}"),
                                "",
                                Some(self.middle_c == mc),
                                cx,
                                move |v, _e, cx| v.set_middle_c(mc, cx),
                            )
                        })
                        .collect(),
                    Sub::LenSet => {
                        // metrical: note fractions + a real bar at the
                        // edit cursor's position in the meter map; SMPTE:
                        // frame/second spans — never a fake-PPQ grid
                        let opts: Vec<(String, u64)> = match td {
                            TimeDisplay::Metrical { ppq } => vec![
                                ("1/32".into(), ppq / 8),
                                ("1/16".into(), ppq / 4),
                                ("1/8".into(), ppq / 2),
                                ("1/4".into(), ppq),
                                (
                                    t("edit.len_1bar").into(),
                                    self.doc(|d| {
                                        d.meter_map_for(self.sel_track)
                                            .bar_ticks_at(self.cursor_tick)
                                    }),
                                ),
                            ],
                            TimeDisplay::Smpte { .. } => {
                                let f = td.cell_ticks();
                                let s = td.bar_ticks();
                                vec![
                                    (t("edit.len_1frame").into(), f),
                                    (t("edit.len_5frames").into(), f * 5),
                                    (t("edit.len_10frames").into(), f * 10),
                                    (t("edit.len_1s").into(), s),
                                    (t("edit.len_5s").into(), s * 5),
                                ]
                            }
                        };
                        opts.into_iter()
                            .enumerate()
                            .map(|(i, (label, ticks))| {
                                Self::mi_leaf(("len", i), label, "", None, cx, move |v, _e, _cx| {
                                    v.apply_region_op("set length", move |d, t, f, to| {
                                        d.set_length_ops(t, f, to, ticks)
                                    });
                                })
                            })
                            .collect()
                    }
                    Sub::LegatoGap => {
                        // gap in ticks relative to ppq: 0 touches the next
                        // note, >0 leaves space, <0 overlaps into it
                        let ppq = self.ppq() as i64;
                        let opts: [(&str, i64); 4] = [
                            ("touch (0 gap)", 0),
                            ("gap 1/32", ppq / 8),
                            ("gap 1/16", ppq / 4),
                            ("overlap 1/32", -(ppq / 8)),
                        ];
                        opts.into_iter()
                            .enumerate()
                            .map(|(i, (label, gap))| {
                                Self::mi_leaf(("leg", i), label, "", None, cx, move |v, _e, _cx| {
                                    v.apply_region_op("legato", move |d, t, f, to| {
                                        d.legato_ops(t, f, to, gap)
                                    });
                                })
                            })
                            .collect()
                    }
                    Sub::Swing => {
                        // swing grid = 16th note; amount = % of one cell the
                        // off-beat shifts later
                        let grid = self.ppq() / 4;
                        let opts: [(&str, u32); 4] = [
                            ("swing 50%", 50),
                            ("swing 66%", 66),
                            ("swing 75%", 75),
                            ("swing 100%", 100),
                        ];
                        opts.into_iter()
                            .enumerate()
                            .map(|(i, (label, amount))| {
                                Self::mi_leaf(("swg", i), label, "", None, cx, move |v, _e, _cx| {
                                    v.apply_region_op("swing", move |d, t, f, to| {
                                        d.swing_ops(t, f, to, grid, amount)
                                    });
                                })
                            })
                            .collect()
                    }
                    Sub::VelSet => {
                        let opts = [
                            (t("edit.vel_pp"), 32u8),
                            (t("edit.vel_mp"), 72),
                            (t("edit.vel_f"), 100),
                            (t("edit.vel_max"), 127),
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
                            })
                            .collect()
                    }
                    // release velocity lives on the note-OFF — a nonzero
                    // value upgrades a 0x90-vel0 off to a real 0x80
                    Sub::RelSet => {
                        let opts = [
                            (t("edit.rel_zero"), 0u8),
                            (t("edit.rel_soft"), 32),
                            (t("edit.rel_med"), 64),
                            (t("edit.rel_hard"), 100),
                        ];
                        opts.into_iter()
                            .enumerate()
                            .map(|(i, (label, vel))| {
                                Self::mi_leaf(
                                    ("rset", i),
                                    label,
                                    "",
                                    None,
                                    cx,
                                    move |v, _e, _cx| {
                                        v.apply_region_op(
                                            "set release velocity",
                                            move |d, t, f, to| {
                                                d.set_release_velocity_ops(t, f, to, vel)
                                            },
                                        );
                                    },
                                )
                            })
                            .collect()
                    }
                    // Edit ▸ Apply to Entire Track — the explicit
                    // whole-track scope (#131). Selection-scoped commands
                    // no-op without a selection; these never need one.
                    Sub::AllTrack => {
                        type TrackOp =
                            Box<dyn Fn(&mut Document, usize, u64, u64) -> Vec<Op> + Send>;
                        let g = self.snap_ticks().max(self.td().min_grid_ticks() as i64) as u64;
                        let ppq = self.ppq() as i64;
                        let items: Vec<(String, TrackOp)> = vec![
                            (
                                format!("Quantize 100% ({})", t("edit.snap")),
                                Box::new(move |d, tr, f, to| d.quantize_ops(tr, f, to, g, 100)),
                            ),
                            (
                                t("edit.transpose_up").to_string(),
                                Box::new(|d, tr, f, to| d.transpose_ops(tr, f, to, 1)),
                            ),
                            (
                                t("edit.transpose_dn").to_string(),
                                Box::new(|d, tr, f, to| d.transpose_ops(tr, f, to, -1)),
                            ),
                            (
                                t("edit.oct_up").to_string(),
                                Box::new(|d, tr, f, to| d.transpose_ops(tr, f, to, 12)),
                            ),
                            (
                                t("edit.oct_dn").to_string(),
                                Box::new(|d, tr, f, to| d.transpose_ops(tr, f, to, -12)),
                            ),
                            (
                                t("edit.vel_up").to_string(),
                                Box::new(|d, tr, f, to| d.scale_velocity_ops(tr, f, to, 1.25)),
                            ),
                            (
                                t("edit.vel_dn").to_string(),
                                Box::new(|d, tr, f, to| d.scale_velocity_ops(tr, f, to, 0.8)),
                            ),
                            (
                                t("edit.humanize").to_string(),
                                Box::new(move |d, tr, f, to| {
                                    d.humanize_ops(tr, f, to, ppq / 32, 5, d.revision())
                                }),
                            ),
                            (
                                format!("{}: touch (0 gap)", t("edit.legato")),
                                Box::new(|d, tr, f, to| d.legato_ops(tr, f, to, 0)),
                            ),
                            (
                                t("edit.fix_overlaps").to_string(),
                                Box::new(|d, tr, f, to| d.fix_overlaps_ops(tr, f, to)),
                            ),
                            (
                                t("edit.join").to_string(),
                                Box::new(|d, tr, f, to| d.join_ops(tr, f, to)),
                            ),
                        ];
                        items
                            .into_iter()
                            .enumerate()
                            .map(|(i, (label, op))| {
                                Self::mi_leaf(
                                    ("allt", i),
                                    label.clone(),
                                    "",
                                    None,
                                    cx,
                                    move |v, _e, _cx| {
                                        v.apply_track_op(&label, |d, tr, f, to| op(d, tr, f, to));
                                    },
                                )
                            })
                            .collect()
                    }
                    Sub::Theme => {
                        let opts = [
                            ("theme.system", theme::ThemeMode::System),
                            ("theme.dark", theme::ThemeMode::Dark),
                            ("theme.light", theme::ThemeMode::Light),
                        ];
                        opts.into_iter()
                            .enumerate()
                            .map(|(i, (key, mode))| {
                                Self::mi_leaf(
                                    ("theme", i),
                                    t(key),
                                    "",
                                    Some(self.theme_mode == mode),
                                    cx,
                                    move |v, _e, cx| v.set_theme_mode(mode, cx),
                                )
                            })
                            .collect()
                    }
                    Sub::AudVel => [64u8, 80, 100, 112, 127]
                        .into_iter()
                        .enumerate()
                        .map(|(i, vel)| {
                            Self::mi_leaf(
                                ("audv", i),
                                format!("{vel}"),
                                "",
                                Some(self.aud_vel == vel),
                                cx,
                                move |v, _e, _cx| {
                                    v.aud_vel = vel;
                                    v.save_global();
                                },
                            )
                        })
                        .collect(),
                    Sub::AudDur => [150u64, 300, 500, 1000]
                        .into_iter()
                        .enumerate()
                        .map(|(i, ms)| {
                            Self::mi_leaf(
                                ("audd", i),
                                format!("{ms} ms"),
                                "",
                                Some(self.aud_ms == ms),
                                cx,
                                move |v, _e, _cx| {
                                    v.aud_ms = ms;
                                    v.save_global();
                                },
                            )
                        })
                        .collect(),
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
                            })
                            .collect()
                    }
                };
                self.sub_rows = rows.clone();
                let vh = f32::from(window.viewport_size().height);
                let desired = rows.len() as f32 * metrics::MENU_ROW + 16.0;
                let max_h = (vh - 40.0).max(120.0);
                let h = desired.min(max_h);
                let top = (y - 30.0).clamp(0.0, (vh - h - 8.0).max(0.0));
                div()
                    .id("sub-popup")
                    .test_support()
                    .role(Role::Menu)
                    .aria_label(t("a11y.submenu"))
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
                    .bg(rgb(theme::current().bg_raised))
                    .border_1()
                    .border_color(rgb(theme::current().border))
                    .rounded_md()
                    .shadow_lg()
                    .children(
                        rows.iter()
                            .enumerate()
                            .map(|(i, r)| Self::row_el(r, i, self.sub_sel == Some(i), true, cx)),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_v, _e, _w, cx| cx.stop_propagation()),
                    )
            });
            div()
                .id("menu-overlay")
                .test_support()
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

        // shortcuts overlay (F1 / Help > Keyboard Shortcuts) — command rows are
        // derived from the registry + keymap so the panel always shows the
        // actual active bindings, including user overrides
        let help_layer = self.help_open.then(|| {
            let row = |k: String, v: SharedString| {
                div()
                    .flex()
                    .h(px(20.0))
                    .items_center()
                    .child(
                        div()
                            .w(px(150.0))
                            .font_family("Cascadia Mono")
                            .text_color(rgb(theme::current().lcd))
                            .child(k),
                    )
                    .child(div().text_color(rgb(theme::current().text)).child(v))
                    .into_any_element()
            };
            let cmd_rows: Vec<AnyElement> = cmd::COMMANDS
                .iter()
                .filter_map(|c| {
                    let s = self.keys.shortcut_label(c.id);
                    if s.is_empty() {
                        None
                    } else {
                        Some(row(s.to_string(), cmd::label(c)))
                    }
                })
                .collect();
            // mouse-only gestures — not keyboard bindings
            let gesture_rows: Vec<AnyElement> = [
                (t("help.g_dup"), "Alt+drag note"),
                (t("help.g_resize"), "Right-edge drag"),
                (t("help.g_seek"), "Click ruler"),
                (t("help.g_playfrom"), "Double-click ruler"),
                (t("help.g_minimap"), "Click minimap"),
                (t("help.g_zoom"), "Ctrl+wheel"),
                (t("help.g_drop"), "Drag .mid file"),
                (t("help.g_marker_click"), "Click marker"),
                (t("help.g_marker_nav"), "[ / ]"),
                (t("help.g_meta_edit"), "M / E"),
            ]
            .into_iter()
            .map(|(desc, gesture)| row(gesture.to_string(), desc.into()))
            .collect();
            // region-scoped navigation — handled by focused regions, not the
            // global keymap, so they stay an explicit list
            let region_rows: Vec<AnyElement> = [
                (t("help.n_regions"), "Tab / Shift+Tab"),
                (t("help.n_menubar"), "F10"),
                (t("help.n_menus"), "← → ↑ ↓ (menus)"),
                (t("help.n_tracks"), "↑ ↓ · M / S / C · Enter / F2 (tracks)"),
                (t("help.n_events"), "↑ ↓ + Enter (event list)"),
                (t("help.n_lane"), "↑ ↓ (lane)"),
            ]
            .into_iter()
            .map(|(desc, keys)| row(keys.to_string(), desc.into()))
            .collect();
            let panel = div()
                .id("help-panel")
                .test_support()
                .role(Role::Dialog)
                .aria_label(t("help.shortcuts"))
                .flex()
                .flex_col()
                .w(px(440.0))
                .max_h(px(560.0))
                .overflow_y_scroll()
                .py_2()
                .px_3()
                .bg(rgb(theme::current().bg_raised))
                .border_1()
                .border_color(rgb(theme::current().border_strong))
                .rounded_lg()
                .shadow_lg()
                .text_size(px(metrics::TEXT_LG))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|_v, _e, _w, cx| cx.stop_propagation()),
                )
                .child(
                    div()
                        .text_size(px(14.0))
                        .text_color(rgb(theme::current().accent))
                        .pb_2()
                        .child(t("help.shortcuts")),
                )
                .children(cmd_rows)
                .child(
                    div()
                        .pt_2()
                        .pb_1()
                        .text_color(rgb(theme::current().accent))
                        .child(t("help.regions")),
                )
                .children(region_rows)
                .child(
                    div()
                        .pt_2()
                        .pb_1()
                        .text_color(rgb(theme::current().accent))
                        .child(t("help.gestures")),
                )
                .children(gesture_rows);
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgba(theme::current().scrim))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|v, _e, _w, cx| {
                        v.help_open = false;
                        cx.notify();
                    }),
                )
                .child(panel)
        });

        // meta edit dialog — title = type label, hint shows write encoding
        let meta_layer = self.meta_edit.map(|me| {
            let enc = self
                .enc_override
                .map(|e| e.label())
                .unwrap_or("utf8")
                .to_string();
            let hint = if me.meta_type == 0x59 {
                "e.g. -3 minor · eb major · f#m".to_string()
            } else {
                tf("meta.hint", &[("enc", &enc)])
            };
            let panel = div()
                .id("meta-panel")
                .flex()
                .flex_col()
                .w(px(420.0))
                .py_2()
                .px_3()
                .gap_1()
                .bg(rgb(theme::current().bg_raised))
                .border_1()
                .border_color(rgb(theme::current().border_strong))
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
                        .text_color(rgb(theme::current().accent))
                        .pb_2()
                        .child(t(EditorView::meta_type_label(me.meta_type))),
                )
                .child(Input::new(&self.meta_input).w_full())
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(rgb(theme::current().text_muted))
                        .child(hint),
                );
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgba(theme::current().scrim))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|v, _e, w, cx| {
                        v.meta_edit = None;
                        w.focus(&v.focus.clone(), cx);
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
                            .text_color(rgb(if path.is_some() {
                                theme::current().text
                            } else {
                                theme::current().danger
                            }))
                            .child(value),
                    ),
                );
                if path.is_none() {
                    row = row.child(
                        div()
                            .text_size(px(metrics::TEXT_MD))
                            .text_color(rgb(th.text_dim))
                            .child(hint),
                    );
                }
                row.into_any_element()
            };
            let scan = if self.scan_rx.is_some() {
                t("status.scanning").to_string()
            } else {
                let n = self.plugin_meta.len().to_string();
                let c = self.scan_cached.to_string();
                let to = self.probe_timeout_secs.to_string();
                let mode = match self.scan_probe_used {
                    Some(true) => t("output.probe_used"),
                    _ => t("output.probe_unused"),
                };
                tf(
                    "output.scan_summary",
                    &[
                        ("n", n.as_str()),
                        ("c", c.as_str()),
                        ("mode", mode),
                        ("timeout", to.as_str()),
                    ],
                )
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
                    .child(format!("{}: {}", t("output.scan"), scan))
                    .into_any_element(),
            ];
            // Audio settings: the persisted selection (device/rate/buffer)
            // every plugin stream is opened with; picking a value reopens
            // live instances onto it. The resolved per-stream device shows
            // on each plugin row below — a selected device that disappears
            // falls back to the system default there.
            rows.push(
                div()
                    .h(px(1.0))
                    .mx_2()
                    .my_1()
                    .bg(rgb(th.border))
                    .into_any_element(),
            );
            rows.push(
                div()
                    .h(px(metrics::MENU_HEAD))
                    .px_2()
                    .mx_1()
                    .text_size(px(metrics::TEXT_XS))
                    .text_color(rgb(th.text_head))
                    .child(t("output.cat_audio"))
                    .into_any_element(),
            );
            let mut dev_picks: Vec<(usize, String, Option<String>)> =
                vec![(0, t("audio.sys_default").to_string(), None)];
            dev_picks.extend(
                self.audio_devices
                    .iter()
                    .enumerate()
                    .map(|(i, n)| (i + 1, n.clone(), Some(n.clone()))),
            );
            for (i, label, dev) in dev_picks {
                let current = self.audio_sel.device == dev;
                rows.push(
                    div()
                        .id(("audio-dev", i))
                        .px_1()
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(theme::current().border)))
                        .text_color(rgb(if current {
                            theme::current().accent
                        } else {
                            theme::current().text
                        }))
                        .child(format!(
                            "{}: {}{}",
                            t("audio.device"),
                            label,
                            if current { "  ✓" } else { "" }
                        ))
                        .on_click(cx.listener(move |v, _e, _w, cx| {
                            let mut sel = v.audio_sel.clone();
                            sel.device = dev.clone();
                            v.apply_audio_selection(sel);
                            cx.notify();
                        }))
                        .into_any_element(),
                );
            }
            // sample-rate / buffer-size option chips
            let cur_sr = self.audio_sel.sample_rate.unwrap_or(44100.0) as u32;
            let cur_bs = self.audio_sel.buffer_size.unwrap_or(512);
            let mut sr_row = div()
                .flex()
                .gap_1()
                .items_center()
                .child(format!("{}:", t("audio.rate")));
            for (i, o) in [44100u32, 48000, 96000].iter().enumerate() {
                let o = *o;
                sr_row = sr_row.child(
                    div()
                        .id(("audio-sr", i))
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .bg(rgb(if o == cur_sr {
                            theme::current().accent_bg
                        } else {
                            theme::current().bg_chip
                        }))
                        .hover(|s| s.bg(rgb(theme::current().bg_chip_hover)))
                        .text_color(rgb(if o == cur_sr {
                            theme::current().text_bright
                        } else {
                            theme::current().accent
                        }))
                        .text_size(px(11.0))
                        .child(o.to_string())
                        .on_click(cx.listener(move |v, _e, _w, cx| {
                            let mut sel = v.audio_sel.clone();
                            sel.sample_rate = Some(o as f64);
                            v.apply_audio_selection(sel);
                            cx.notify();
                        })),
                );
            }
            rows.push(sr_row.into_any_element());
            let mut bs_row = div()
                .flex()
                .gap_1()
                .items_center()
                .child(format!("{}:", t("audio.buffer")));
            for (i, o) in [256u32, 512, 1024, 2048].iter().enumerate() {
                let o = *o;
                bs_row = bs_row.child(
                    div()
                        .id(("audio-bs", i))
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .bg(rgb(if o == cur_bs {
                            theme::current().accent_bg
                        } else {
                            theme::current().bg_chip
                        }))
                        .hover(|s| s.bg(rgb(theme::current().bg_chip_hover)))
                        .text_color(rgb(if o == cur_bs {
                            theme::current().text_bright
                        } else {
                            theme::current().accent
                        }))
                        .text_size(px(11.0))
                        .child(format!("{} smp", o))
                        .on_click(cx.listener(move |v, _e, _w, cx| {
                            let mut sel = v.audio_sel.clone();
                            sel.buffer_size = Some(o);
                            v.apply_audio_selection(sel);
                            cx.notify();
                        })),
                );
            }
            rows.push(bs_row.into_any_element());
            if let Some(note) = &self.scan_note {
                rows.push(
                    div()
                        .text_color(rgb(theme::current().text_dim))
                        .child(note.clone())
                        .into_any_element(),
                );
            }
            rows.push(
                div()
                    .h(px(1.0))
                    .mx_2()
                    .my_1()
                    .bg(rgb(theme::current().border))
                    .into_any_element(),
            );
            rows.push(
                div()
                    .h(px(18.0))
                    .px_2()
                    .mx_1()
                    .text_size(px(9.5))
                    .text_color(rgb(theme::current().text_head))
                    .child(t("output.cat_vst3"))
                    .into_any_element(),
            );
            for (i, (name, dest)) in dests.iter().enumerate() {
                let output::Destination::Plugin { plugin_path, .. } = dest else {
                    continue;
                };
                let vendor = self
                    .plugin_meta
                    .get(plugin_path)
                    .map(|p| p.vendor.clone())
                    .unwrap_or_default();
                let (state, color, retry, detail, audio) = match self.plugin_state.get(&i) {
                    Some(PluginState::Ready { .. }) => {
                        let slot = self.plugin_slots.get(&i);
                        let detail = slot.map(|slot| {
                            let n = slot.latency.samples().to_string();
                            let ms = format!("{:.1}", slot.latency.as_us() as f64 / 1000.0);
                            crate::i18n::tf(
                                "plugin.latency",
                                &[("n", n.as_str()), ("ms", ms.as_str())],
                            )
                        });
                        let audio = slot.map(|s| {
                            let a = s.audio_diag();
                            let dev = a
                                .device
                                .unwrap_or_else(|| t("audio.sys_default").to_string());
                            let ok = a.stream_error.is_none();
                            let state = a
                                .stream_error
                                .unwrap_or_else(|| t("audio.stream_ok").to_string());
                            (
                                format!(
                                    "{} Hz · {} smp · {} · {}",
                                    a.sample_rate as u32, a.block_size, dev, state
                                ),
                                ok,
                            )
                        });
                        (
                            t("plugin.state_ready"),
                            theme::current().lcd,
                            false,
                            detail,
                            audio,
                        )
                    }
                    Some(PluginState::Loading { .. }) => (
                        t("plugin.state_loading"),
                        theme::current().warn,
                        false,
                        None,
                        None,
                    ),
                    Some(PluginState::Failed { phase, msg, .. }) => {
                        let phase = match *phase {
                            "host" => t("plugin.phase_host"),
                            "audio" => t("plugin.phase_audio"),
                            _ => t("plugin.phase_load"),
                        };
                        (
                            t("plugin.state_failed"),
                            theme::current().danger,
                            true,
                            Some(format!("{phase}: {msg}")),
                            None,
                        )
                    }
                    _ => (
                        t("plugin.state_idle"),
                        theme::current().text_muted,
                        false,
                        None,
                        None,
                    ),
                };
                let mut row = div()
                    .id(("output-status", i))
                    .test_support()
                    .role(Role::ListItem)
                    .aria_label(format!("{name} {state}"))
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
                            .child(
                                div()
                                    .text_color(rgb(theme::current().text_muted))
                                    .child(vendor),
                            ),
                    );
                if let Some(detail) = detail {
                    row = row.child(
                        div()
                            .text_size(px(metrics::TEXT_MD))
                            .text_color(rgb(th.text_dim))
                            .child(detail),
                    );
                }
                // active stream configuration (resolved device, rate, block
                // size, stream state) — red when the stream last errored
                if let Some((line, ok)) = audio {
                    row = row.child(
                        div()
                            .text_size(px(11.0))
                            .text_color(rgb(if ok {
                                theme::current().text_dim
                            } else {
                                theme::current().danger
                            }))
                            .child(line),
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
            // quarantine: bundles the cache recorded as crash/timeout and
            // skipped this scan — each row force-retries that one plugin
            if !self.quarantined.is_empty() {
                rows.push(
                    div()
                        .h(px(metrics::MENU_HEAD))
                        .px_2()
                        .mx_1()
                        .text_size(px(metrics::TEXT_XS))
                        .text_color(rgb(th.text_head))
                        .child(t("output.quarantined_short"))
                        .into_any_element(),
                );
            }
            for (i, (path, reason)) in self.quarantined.iter().enumerate() {
                let name = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let path = path.clone();
                rows.push(
                    div()
                        .id(("output-quarantine", i))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .text_color(rgb(theme::current().warn))
                        .child(
                            div()
                                .flex()
                                .gap_1()
                                .items_center()
                                .child(format!("{}  {}", name, t("plugin.state_failed")))
                                .child(
                                    div()
                                        .text_color(rgb(theme::current().text_muted))
                                        .child(t("output.quarantined_short").to_string()),
                                ),
                        )
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(rgb(theme::current().text_dim))
                                .child(format!("{} — {}", reason, t("output.click_rescan"))),
                        )
                        .on_click(cx.listener(move |v, _e, _w, cx| {
                            v.rescan_plugins(crate::ScanMode::Retry(path.clone()));
                            cx.notify();
                        }))
                        .into_any_element(),
                );
            }
            let panel_content_h = 24.0 + rows.len() as f32 * 26.0 + 32.0 + 32.0;
            let panel = div()
                .id("output-status-panel")
                .test_support()
                .role(Role::Dialog)
                .aria_label(t("output.host_status"))
                .w(px(520.0))
                .flex()
                .flex_col()
                .gap_1()
                .p_3()
                .bg(rgb(theme::current().bg_raised))
                .border_1()
                .border_color(rgb(theme::current().border_strong))
                .rounded_lg()
                .shadow_lg()
                .text_size(px(metrics::TEXT_LG))
                .child(
                    div()
                        .text_size(px(14.0))
                        .text_color(rgb(theme::current().accent))
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
                            t("output.rescan"),
                            cx,
                            |v, _e, cx| {
                                v.rescan_plugins(crate::ScanMode::Changed);
                                cx.notify();
                            },
                        ))
                        .child(Self::chip(
                            "status.close",
                            t("output.close"),
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
                .bg(rgba(theme::current().scrim))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|v, _e, _w, cx| {
                        v.show_output_status = false;
                        cx.notify();
                    }),
                )
                .child(panel)
        });

        // command palette / keybindings overlay (Ctrl+Shift+P, Help menu)
        let palette_layer = self.palette.as_ref().map(|p| {
            let keys_mode = p.mode == PaletteMode::Keys;
            let capturing = p.capture;
            let rows = self.palette_rows(cx);
            let list_rows: Vec<AnyElement> = if rows.is_empty() {
                vec![div()
                    .px_2()
                    .py_1()
                    .text_color(rgb(theme::current().text_dim))
                    .child(t("ui.no_matches"))
                    .into_any_element()]
            } else {
                rows.iter()
                    .enumerate()
                    .map(|(i, c)| {
                        let sel = i == p.sel;
                        let right: SharedString = if keys_mode && capturing == Some(c.id) {
                            t("ui.keys_capture").into()
                        } else {
                            self.keys.shortcut_label(c.id).into()
                        };
                        let overridden = keys_mode && !self.keys.is_default(c.id);
                        div()
                            .id(("pal", i))
                            .flex()
                            .items_center()
                            .h(px(26.0))
                            .px_2()
                            .mx_1()
                            .rounded_sm()
                            .when(sel, |d| d.bg(rgb(theme::current().accent_bg)))
                            .text_size(px(12.0))
                            .text_color(rgb(if sel {
                                theme::current().text_bright
                            } else {
                                theme::current().text
                            }))
                            .child(div().flex_1().whitespace_nowrap().child(cmd::label(c)))
                            .when(overridden, |d| {
                                d.child(div().text_color(rgb(theme::current().warn)).child("●"))
                            })
                            .child(
                                div()
                                    .pl_2()
                                    .font_family("Cascadia Mono")
                                    .text_size(px(10.0))
                                    .text_color(rgb(if sel {
                                        theme::current().text_bright
                                    } else {
                                        theme::current().text_faint
                                    }))
                                    .child(right),
                            )
                            .on_mouse_move(cx.listener(move |v, _e: &MouseMoveEvent, _w, cx| {
                                if let Some(p) = v.palette.as_mut() {
                                    if p.sel != i {
                                        p.sel = i;
                                        cx.notify();
                                    }
                                }
                            }))
                            .on_click(cx.listener(move |v, _e, w, cx| {
                                cx.stop_propagation();
                                if let Some(p) = v.palette.as_mut() {
                                    p.sel = i;
                                }
                                v.palette_activate(w, cx);
                                cx.notify();
                            }))
                            .into_any_element()
                    })
                    .collect()
            };
            let title = if keys_mode {
                t("ui.keys")
            } else {
                t("ui.palette")
            };
            let hint = if keys_mode {
                t("ui.keys_hint")
            } else {
                t("ui.palette_hint")
            };
            div()
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .bg(rgba(theme::current().scrim))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|v, _e, _w, cx| {
                        v.close_palette(cx);
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .id("palette-panel")
                        .mt(px(96.0))
                        .flex()
                        .flex_col()
                        .w(px(520.0))
                        .h(px(420.0))
                        .bg(rgb(theme::current().bg_raised))
                        .border_1()
                        .border_color(rgb(theme::current().border_strong))
                        .rounded_lg()
                        .shadow_lg()
                        .text_size(px(12.0))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_v, _e, _w, cx| cx.stop_propagation()),
                        )
                        .child(
                            div().flex().items_center().px_3().pt_2().pb_1().child(
                                div()
                                    .text_size(px(13.0))
                                    .text_color(rgb(theme::current().accent))
                                    .child(title),
                            ),
                        )
                        .child(div().px_2().pb_1().child(Input::new(&p.input)))
                        .child(
                            div()
                                .id("pal-scroll")
                                .flex_1()
                                .overflow_y_scroll()
                                .py_1()
                                .children(list_rows),
                        )
                        .child(
                            div()
                                .px_3()
                                .py_1()
                                .border_t_1()
                                .border_color(rgb(theme::current().border_strong))
                                .text_size(px(10.0))
                                .text_color(rgb(theme::current().text_faint))
                                .child(hint),
                        ),
                )
        });

        div()
            .id("editor")
            .test_support()
            .role(Role::Application)
            .aria_label(t("a11y.editor"))
            .flex()
            .flex_col()
            .relative()
            .size_full()
            .bg(rgb(theme::current().bg_root))
            .text_color(rgb(theme::current().text))
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
                let k = ev.keystroke.key.as_str();
                // meta dialog swallows keys (typing must not trigger editor
                // keys); Enter applies, Esc cancels
                if this.meta_edit.is_some() {
                    if let (false, "escape") = (ev.keystroke.modifiers.control, k) {
                        this.meta_edit = None;
                        let f = this.focus.clone();
                        w.focus(&f, cx);
                        cx.notify();
                    }
                    return;
                }
                let ctrl = ev.keystroke.modifiers.control;
                let shift = ev.keystroke.modifiers.shift;
                // the rename input owns its keys: Enter commits, Escape and
                // Tab move focus back out
                if this.input.read(cx).focus_handle(cx).is_focused(w) {
                    match (ctrl, k) {
                        (false, "enter") => this.commit_rename(w, cx),
                        (false, "escape") => w.focus(&this.tracks_fh, cx),
                        (false, "tab") => {
                            if shift {
                                w.focus_prev(cx);
                            } else {
                                w.focus_next(cx);
                            }
                        }
                        _ => {}
                    }
                    return;
                }
                // the inspector's value field owns its keys too — typing a
                // number must not hit tool/clipboard commands
                if this.prop_input.read(cx).focus_handle(cx).is_focused(w) {
                    return;
                }
                // palette/keys overlay owns every key while open
                if this.palette.is_some() {
                    this.palette_key(ev, w, cx);
                    return;
                }
                // while a menu is open, menu navigation owns arrows/enter/
                // escape; other chords (Ctrl+S …) still work
                if this.open_menu.is_some() && this.menu_key(ev, w, cx) {
                    return;
                }
                // modal overlays swallow Tab — focus must never move behind
                // them where the ring can't be seen (Esc closes them)
                if !ctrl && k == "tab" {
                    if this.help_open || this.show_output_status {
                        return;
                    }
                    if shift {
                        w.focus_prev(cx);
                    } else {
                        w.focus_next(cx);
                    }
                    return;
                }
                if !ctrl && k == "f10" {
                    this.menu_bar_sel = 0;
                    w.focus(&this.menu_fh, cx);
                    return;
                }
                let st = {
                    let s = this.snap_ticks();
                    if s > 0 {
                        s
                    } else {
                        this.td().nudge_ticks() as i64
                    }
                };
                if k == "escape" && !ctrl {
                    // layered dismissal (#178): close the topmost overlay
                    // first and keep the selection — Esc-ing a menu or the
                    // F1 overlay used to wipe a chord selection too. Only a
                    // bare Esc (nothing open) clears the selection.
                    let had_overlay = this.open_menu.is_some()
                        || this.open_sub.is_some()
                        || this.help_open
                        || this.show_output_status
                        || this.meta_edit.is_some();
                    this.open_menu = None;
                    this.open_sub = None;
                    this.help_open = false;
                    this.show_output_status = false;
                    if !crate::nav::escape_clears_selection(had_overlay) {
                        cx.notify();
                        return;
                    }
                    this.selection.clear();
                    this.sel_events.clear();
                    this.meta_sel = None;
                    cx.notify();
                    return;
                }
                // arrows/edit keys act on the roll only while the roll
                // context (its handle or the root fallback) owns focus —
                // tracks/lane/events have their own bindings
                if !ctrl
                    && matches!(k, "left" | "right" | "up" | "down" | "enter")
                    && (this.roll_fh.contains_focused(w, cx) || this.focus.is_focused(w))
                {
                    match (shift, k) {
                        (false, "left") => this.roll_arrow(-st, 0, cx),
                        (false, "right") => this.roll_arrow(st, 0, cx),
                        (true, "left") => this.roll_arrow(-1, 0, cx),
                        (true, "right") => this.roll_arrow(1, 0, cx),
                        (false, "up") => this.roll_arrow(0, 1, cx),
                        (false, "down") => this.roll_arrow(0, -1, cx),
                        (true, "up") => this.roll_arrow(0, 12, cx),
                        (true, "down") => this.roll_arrow(0, -12, cx),
                        (false, "enter") => this.cursor_activate(cx),
                        _ => {}
                    }
                    return;
                }
                // everything else is a registry command: the keymap (defaults
                // + user overrides) maps the keystroke to a canonical action
                if let Some(c) = this.keys.command_at(&cmd::describe(&ev.keystroke)) {
                    if c.enabled.map(|f| f(this, cx)).unwrap_or(true) {
                        (c.act)(this, w, cx);
                    }
                }
            }))
            .child(menu_bar)
            .child(transport_bar)
            .child(body)
            .child(status_bar)
            .children(menu_layer)
            .children(help_layer)
            .children(meta_layer)
            .children(output_status)
            .children(palette_layer)
            // drag a .mid file anywhere to open it
            .can_drop(|drag: &dyn Any, _w, _cx| drag.is::<ExternalPaths>())
            .drag_over::<ExternalPaths>(|s, _p, _w, _cx| s.bg(rgb(theme::current().accent_drop)))
            .on_drop(cx.listener(|v, paths: &ExternalPaths, w, cx| {
                if let Some(p) = paths.paths().iter().find(|p| crate::is_midi_path(p)) {
                    v.confirm_discard_or_save(PendingAction::OpenPath(p.clone()), w, cx);
                }
            }))
    }
}

// --- menubar helpers -----------------------------------------------------------
