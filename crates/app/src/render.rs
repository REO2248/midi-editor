//! Rendering: impl Render for EditorView (toolbar, track column, ruler,
//! piano roll canvas, lane, event list) plus the chip/button helpers.
//! Private items are visible here because this is a child module of the
//! crate root where EditorView is defined.

use crate::a11y;
use crate::geometry::{drag_window, tick_window, ZOOM_MAX, ZOOM_MIN};
use crate::i18n::{t, tf};
use crate::icons::icon;
use crate::menu::{LeafRow, MenuRow, MENUS};
use crate::theme::metrics;
use crate::*;
use gpui_kit::base::{ObservedElement, TestSupportExt};
use gpui_kit::component::input::Input;
use gpui_kit::prelude::FluentBuilder;
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

/// Snap-menu label: metrical files subdivide a whole note, SMPTE files a
/// second — "1/16" vs "1/16s" makes the redefined grid explicit instead
/// of silently suggesting beats that don't exist. "off" stays bare.
fn snap_label(label: &'static str, td: TimeDisplay) -> String {
    if td.is_smpte() && label != "off" {
        format!("{label}s")
    } else {
        label.to_string()
    }
}

impl Render for EditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
        let tempo0 = doc_ui.tempo0;
        let sig = doc_ui.sig.clone();
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
        let (grid_minor, grid_major) = td.grid_ticks(self.zoom);
        let note_min = td.min_grid_ticks();
        let badge = td.badge();
        let pos = td.format_tick(playhead_tick);

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
                    let cur = scrub_key == Some(key as u8);
                    let y = bounds.origin.y + px(r as f32 * note_h - scroll_y);
                    window.paint_quad(fill(
                        Bounds::new(
                            point(bounds.origin.x, y),
                            size(w, px((note_h - 1.0).max(1.0))),
                        ),
                        rgb(if cur {
                            ACCENT
                        } else if black {
                            0x101016
                        } else {
                            0x2a2a34
                        }),
                    ));
                    // C guide line across the strip, like the roll's rows
                    if key % 12 == 0 {
                        window.paint_quad(fill(
                            Bounds::new(
                                point(bounds.origin.x, y + px(note_h - 1.0)),
                                size(w, px(1.0)),
                            ),
                            rgb(0x3a3a48),
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
                            rgb(0x1e2436),
                        ));
                    } else if black {
                        window.paint_quad(fill(
                            Bounds::new(point(bounds.origin.x, y), size(w, px(note_h))),
                            rgb(0x1a1a21),
                        ));
                    }
                    window.paint_quad(fill(
                        Bounds::new(point(bounds.origin.x, y), size(w, px(1.0))),
                        rgb(if key % 12 == 0 { 0x2e2e3a } else { 0x232329 }),
                    ));
                }
                // beat/bar lines — quarter/bar for metrical, frame/second
                // for SMPTE (minor lines collapse when < ~4px apart)
                let tick0 = (scroll_x / zoom).max(0.0) as u64;
                let tick1 = tick0 + (f32::from(w) / zoom) as u64 + grid_minor;
                let mut t = tick0 / grid_minor * grid_minor;
                while t <= tick1 {
                    let x = bounds.origin.x + px(t as f32 * zoom - scroll_x);
                    let strong = t.is_multiple_of(grid_major);
                    window.paint_quad(fill(
                        Bounds::new(point(x, bounds.origin.y), size(px(1.0), h)),
                        rgb(if strong { 0x3d3d52 } else { 0x2a2a35 }),
                    ));
                    t += grid_minor;
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
                                rgba(0x80808044),
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
                        SEL_COLOR
                    } else if n.end_tick.is_none() {
                        DANGLING_COLOR
                    } else {
                        let c = th.track_colors[n.track % th.track_colors.len()];
                        if n.track == active_track {
                            c
                        } else {
                            blend(c, 0x12121a, 0.62)
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
                        rgb(0x50ff9f),
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
                            rgb(ACCENT),
                            BorderStyle::Solid,
                        ));
                        window.paint_quad(fill(
                            Bounds::new(point(cx0 + px(cw) + px(1.0), cy0), size(px(1.5), px(ch))),
                            rgb(ACCENT),
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
                            rgba(0x4f8cff33),
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
            .bg(rgb(BG_BAR))
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
                    .text_color(rgb(0x7a86a8))
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
                        rgb(BG_RAISED)
                    } else {
                        rgb(BG_BAR)
                    })
                    .text_color(rgb(if is_open || bar_sel {
                        0xffffff
                    } else {
                        0x9a9ab0
                    }))
                    .hover(|s| s.bg(rgb(0x1d1d28)))
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
            .bg(rgb(BG_PANEL))
            .border_b_1()
            .border_color(rgb(BORDER_C))
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
                    .id("pos")
                    .test_support()
                    .role(Role::Label)
                    .aria_label(tf("a11y.pos", &[("pos", pos.as_str())]))
                    .px_2()
                    .h(px(24.0))
                    .flex()
                    .items_center()
                    .bg(rgb(0x0b0b11))
                    .border_1()
                    .border_color(rgb(BORDER_C))
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
                    .bg(rgb(0x0b0b11))
                    .border_1()
                    .border_color(rgb(BORDER_C))
                    .rounded_sm()
                    .text_color(rgb(if td.is_smpte() { 0xffb46a } else { 0x9fd0ff }))
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
                    .bg(rgb(0x0b0b11))
                    .border_1()
                    .border_color(rgb(BORDER_C))
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
                            .text_size(px(metrics::TEXT_MD))
                            .font_family("Cascadia Mono")
                            .text_color(rgb(if SNAPS[self.snap_idx].0 > 0 {
                                0xd8d8e0
                            } else {
                                0x55556a
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
                    let g = v.snap_ticks().max(v.td().min_grid_ticks() as i64) as u64;
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
            .bg(rgb(BG_PANEL))
            .border_l_1()
            .border_color(rgb(if events_focused { ACCENT } else { BORDER_C }))
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
                    .child(format!("{} ({})", t("events.header"), self.events.len()))
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
                                            rgb(0xffffff)
                                        } else {
                                            rgb(th.events_text)
                                        })
                                        .bg(if selected {
                                            rgba(0x4f8cff44)
                                        } else if cur && ev_focused {
                                            rgba(0xffffff14)
                                        } else {
                                            rgba(0x00000000)
                                        })
                                        .cursor_pointer()
                                        .hover(|s| s.bg(rgba(0xffffff12)))
                                        .child(events[i].text.clone())
                                        .on_mouse_down(MouseButton::Left, move |ev, w, cx| {
                                            view.update(cx, |this, cx| {
                                                this.ev_row_click(
                                                    i,
                                                    ev.modifiers.control,
                                                    ev.modifiers.shift,
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
            .w(px(150.0))
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(0x1b1b24))
            .border_r_1()
            .border_color(rgb(if tracks_focused { ACCENT } else { BORDER_C }))
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
                            .bg(if sel { rgb(0x2a2a3a) } else { rgb(0x1b1b24) })
                            .hover(|s| s.bg(rgb(0x252532)))
                            .on_click(cx.listener(move |v, _e, w, cx| {
                                // format 2: this click also picks the
                                // sequence being viewed/played
                                v.select_track(i, cx);
                                w.focus(&v.tracks_fh, cx);
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
                                    .text_color(rgb(if muted { 0xffb454 } else { 0x707080 }))
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
                                    .text_color(rgb(if soloed { 0xffd24f } else { 0x707080 }))
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
                                            (track_chs.get(i).copied().unwrap_or(0) + 1)
                                                .to_string()
                                                .as_str(),
                                        )],
                                    ))
                                    .px_1()
                                    .text_size(px(9.0))
                                    .text_color(rgb(0x7070a0))
                                    .on_click(cx.listener(move |v, _e: &ClickEvent, w, cx| {
                                        cx.stop_propagation();
                                        v.cycle_chan(i);
                                        w.focus(&v.tracks_fh, cx);
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
                    .child(Self::chip(
                        "rename",
                        "rename",
                        t("a11y.rename"),
                        cx,
                        |v, _e, cx| {
                            v.apply_rename(cx);
                            cx.notify();
                        },
                    )),
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
                // coarse ticks: one bar for metrical, one second for SMPTE
                let tick0 = (scroll_x / zoom).max(0.0) as u64;
                let tick1 = tick0 + (f32::from(w) / zoom) as u64 + grid_major;
                let mut t = tick0 / grid_major * grid_major;
                while t <= tick1 {
                    let x = bounds.origin.x + px(t as f32 * zoom - scroll_x);
                    window.paint_quad(fill(
                        Bounds::new(point(x, bounds.origin.y + px(12.0)), size(px(1.0), px(8.0))),
                        rgb(0x55556a),
                    ));
                    t += grid_major;
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
                        .bg(rgb(0x111118))
                        .border_b_1()
                        .border_color(rgb(BORDER_C))
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
                        .bg(rgb(0x17171d))
                        .border_b_1()
                        .border_color(rgb(0x2a2a35))
                        .cursor_pointer()
                        .child(ruler.size_full())
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, ev: &MouseDownEvent, w, cx| {
                                w.focus(&this.roll_fh, cx);
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
                .child({
                    // a11y snapshot inputs for the piano-roll synthetic
                    // subtree (note/selection/playhead/marquee nodes)
                    let roll_bounds_a11y = self.roll_bounds.clone();
                    let notes_a11y = self.notes.clone();
                    let selection_a11y = self.selection.clone();
                    let track_names_a11y = track_names.clone();
                    let scale_a11y = window.scale_factor();
                    let ppq = self.ppq();
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
                                scale: scale_a11y,
                                scroll_x,
                                scroll_y,
                                zoom,
                                ppq,
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
                                .w(px(48.0))
                                .h_full()
                                .relative()
                                .overflow_hidden()
                                .bg(rgb(0x17171d))
                                .border_r_1()
                                .border_color(rgb(BORDER_C))
                                .cursor_pointer()
                                .child(kbd.size_full())
                                .children((0..strip_keys.len() as i32).filter_map(|r| {
                                    let k = strip_keys[r as usize];
                                    if k % 12 != 0 {
                                        return None;
                                    }
                                    let y = r as f32 * note_h - scroll_y
                                        + (note_h - 8.0) / 2.0;
                                    (y > -12.0).then(|| {
                                        div()
                                            .absolute()
                                            .right(px(2.0))
                                            .top(px(y))
                                            .text_size(px(7.0))
                                            .text_color(rgb(0x8080a0))
                                            .child(format!("C{}", k as i32 / 12 - 1))
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
                                            let at = this
                                                .doc(|d| d.tempo_map.us_to_tick(this.play_us));
                                            this.scrub_key = Some(k);
                                            this.audition_strike(tr, ch, k, vel, at);
                                            cx.notify();
                                        }
                                    }),
                                )
                                .on_mouse_move(cx.listener(
                                    |this, ev: &MouseMoveEvent, _w, cx| {
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
                                                let at = this.doc(|d| {
                                                    d.tempo_map.us_to_tick(this.play_us)
                                                });
                                                this.audition_strike(tr, ch, k, vel, at);
                                            }
                                            cx.notify();
                                        }
                                    },
                                ))
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
                                    rgb(ACCENT)
                                } else {
                                    rgba(0x00000000)
                                })
                                .track_focus(&self.roll_fh)
                                .on_key_down(cx.listener(
                                    |this, ev: &KeyDownEvent, _w, cx| {
                                        if this.open_menu.is_some() {
                                            return;
                                        }
                                        if ev.keystroke.key == "enter" {
                                            this.cursor_activate(cx);
                                            cx.stop_propagation();
                                        }
                                    },
                                ))
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
                                        .text_color(rgb(0x9aa0c0))
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
                            });
                            cx.notify();
                            return;
                        }
                        if let Some(n) = this.edge_at(ev.position) {
                            this.sel_track = n.track;
                            this.audition_strike(n.track, n.channel, n.key, n.vel, n.start_tick);
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
                            this.audition_strike(n.track, n.channel, n.key, n.vel, n.start_tick);
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
                .child({
                    // a11y snapshot inputs for the controller-lane synthetic
                    // subtree (one slider node per point)
                    let lane_bounds_a11y = self.lane_bounds.clone();
                    let lane_events_a11y = lane_events.clone();
                    let notes_a11y = self.notes.clone();
                    let track_names_a11y = track_names.clone();
                    let sel_track_a11y = self.sel_track;
                    let scale_a11y = window.scale_factor();
                    let ppq = self.ppq();
                    div()
                        .id("lane")
                        .test_support()
                        .role(Role::Group)
                        .aria_label(tf(
                            "a11y.lane",
                            &[("mode", lane_mode.label().as_str())],
                        ))
                        .a11y_synthetic_children(move |b| {
                            a11y::LaneA11y {
                                bounds: lane_bounds_a11y.get(),
                                scale: scale_a11y,
                                scroll_x,
                                zoom,
                                ppq,
                                mode: lane_mode,
                                events: lane_events_a11y,
                                notes: notes_a11y,
                                sel_track: sel_track_a11y,
                                track_names: track_names_a11y,
                                drag,
                            }
                            .build(b);
                        })
                        .h(px(56.0))
                        .w_full()
                        .bg(rgb(0x14141a))
                        .border_t_1()
                        .border_color(rgb(if area == FocusArea::Lane {
                            ACCENT
                        } else {
                            0x2a2a35
                        }))
                        .relative()
                        .track_focus(&self.lane_fh)
                        .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _w, cx| {
                            if this.open_menu.is_some() {
                                return;
                            }
                            // bare keys only — Ctrl/Alt chords belong to the
                            // global handler (Ctrl+V must paste, not cycle)
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
                                    this.lane_mode = this.lane_mode.cycle();
                                    this.persist();
                                    cx.notify();
                                }
                                _ => return,
                            }
                            cx.stop_propagation();
                        }))
                        .child(lane.size_full())
                        .child(
                            // lane-mode chip: Vel -> CC1 -> CC7 -> CC10 ->
                            // CC11 -> CC64 -> PB -> Vel
                            div()
                                .id("lane-mode")
                                .test_support()
                                .role(Role::Button)
                                .aria_label(tf(
                                    "a11y.lane_mode",
                                    &[("mode", lane_mode.label().as_str())],
                                ))
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
                            cx.listener(|this, ev: &MouseDownEvent, w, cx| {
                                w.focus(&this.lane_fh, cx);
                                this.mouse_pos = Some(ev.position);
                                let b = this.lane_bounds.get();
                                let x = f32::from(ev.position.x) - f32::from(b.origin.x);
                                let y = f32::from(ev.position.y) - f32::from(b.origin.y);
                                let tick = ((x + this.scroll_x) / this.zoom).max(0.0) as u64;
                                this.cursor_tick = tick; // share the roll's edit cursor
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
                                                dkey: vel.clamp(1, 127),
                                                a_tick: 0,
                                                a_key: 0,
                                                b_tick: 0,
                                                b_key: 0,
                                                aud_vel: 0,
                                                aud_ch: 0,
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
                                            aud_vel: 0,
                                            aud_ch: 0,
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
                        )
                }),
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
                    .text_color(rgb(0x77778a))
                    .child(format!("{}", self.status)),
            )
            .children(plugin_chip)
            // MCP auth posture — an unauthenticated endpoint must be visible
            .child({
                let (label, color) = match mcp_auth_mode {
                    mcp_server::McpAuthMode::Bearer => (t("status.mcp_auth"), 0x8fd0a0),
                    mcp_server::McpAuthMode::Open => (t("status.mcp_open"), 0xe06060),
                    mcp_server::McpAuthMode::Stdio => (t("status.mcp_off"), 0x77778a),
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
                    .text_color(rgb(0x77778a))
                    .whitespace_nowrap()
                    .child(format!("{}: {}", t("focus.label"), t(area.key()))),
            )
            .child(
                // live monitor scale — proves PerMonitorV2 at runtime (a
                // bitmap-stretched app would always report 100%)
                div()
                    .text_color(rgb(0x77778a))
                    .whitespace_nowrap()
                    .child(format!(
                        "{}: {}%",
                        t("ui.scale"),
                        (window.scale_factor() * 100.0).round() as i32
                    )),
            )
            .child(Self::chip(
                "st-lane",
                lane_mode.label(),
                tf("a11y.lane_mode", &[("mode", lane_mode.label().as_str())]),
                cx,
                |v, _e, cx| {
                    v.set_lane(v.lane_mode.cycle(), cx);
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
            let items: Vec<MenuRow> = match m {
                TopMenu::File => vec![
                    Self::mi("f.new", t("menu.new"), "", None, cx, |v, w, cx| {
                        v.confirm_discard_or_save(PendingAction::NewFile, w, cx);
                    }),
                    Self::mi("f.open", t("menu.open"), "Ctrl+O", None, cx, |v, w, cx| {
                        v.confirm_discard_or_save(PendingAction::OpenDialog, w, cx);
                    }),
                    Self::mi_sub("f.recent", t("menu.recent"), Sub::Recent, cx),
                    Self::msep(),
                    Self::mi("f.save", t("menu.save"), "Ctrl+S", None, cx, |v, _e, cx| {
                        v.save(cx);
                    }),
                    Self::mi("f.savas", t("menu.save_as"), "", None, cx, |v, _e, cx| {
                        v.save_as(cx);
                    }),
                ],
                TopMenu::Edit => vec![
                    Self::mi("e.undo", t("menu.undo"), "Ctrl+Z", None, cx, |v, _e, cx| {
                        v.undo(cx);
                    }),
                    Self::mi("e.redo", t("menu.redo"), "Ctrl+Y", None, cx, |v, _e, cx| {
                        v.redo(cx);
                    }),
                    Self::msep(),
                    Self::mi(
                        "e.selall",
                        t("menu.select_all"),
                        "Ctrl+A",
                        None,
                        cx,
                        |v, _e, cx| {
                            v.select_all(cx);
                        },
                    ),
                    Self::msep(),
                    Self::mi("e.cut", t("edit.cut"), "Ctrl+X", None, cx, |v, _e, cx| {
                        v.copy_selected(true, cx);
                    }),
                    Self::mi("e.copy", t("edit.copy"), "Ctrl+C", None, cx, |v, _e, cx| {
                        v.copy_selected(false, cx);
                    }),
                    Self::mi(
                        "e.paste",
                        t("edit.paste"),
                        "Ctrl+V",
                        None,
                        cx,
                        |v, _e, cx| {
                            v.paste(cx);
                        },
                    ),
                    Self::mi(
                        "e.dup",
                        t("edit.duplicate"),
                        "Ctrl+D",
                        None,
                        cx,
                        |v, _e, cx| {
                            v.duplicate_selected(cx);
                        },
                    ),
                    Self::mi("e.del", t("menu.delete"), "Del", None, cx, |v, _e, cx| {
                        v.delete_selected(cx);
                    }),
                    Self::msep(),
                    Self::mi_sub("e.tool", t("edit.tool"), Sub::Tool, cx),
                    Self::mi_sub("e.snap", t("edit.snap"), Sub::Snap, cx),
                    Self::msep(),
                    Self::mi_sub("e.quant", t("edit.quantize"), Sub::Quant, cx),
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
                    ),
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
                    ),
                    Self::mi_sub("e.oct", t("edit.octave"), Sub::Oct, cx),
                    Self::msep(),
                    Self::mi("e.human", t("edit.humanize"), "", None, cx, |v, _e, _cx| {
                        v.apply_region_op("humanize", |d, t, f, to| {
                            // revision as seed: identical ops replay the same
                            // take; a different doc state reseeds the jitter
                            d.humanize_ops(t, f, to, 12, 8, d.revision())
                        });
                    }),
                    Self::mi("e.split", t("edit.split"), "", None, cx, |v, _e, cx| {
                        v.split_at_playhead(cx);
                    }),
                    Self::mi_sub("e.swing", t("edit.swing"), Sub::Swing, cx),
                    Self::mi("e.join", t("edit.join"), "", None, cx, |v, _e, _cx| {
                        v.apply_region_op("join", |d, t, f, to| d.join_ops(t, f, to));
                    }),
                    Self::mi(
                        "e.fixov",
                        t("edit.fix_overlaps"),
                        "",
                        None,
                        cx,
                        |v, _e, _cx| {
                            v.apply_region_op("fix overlaps", |d, t, f, to| {
                                d.fix_overlaps_ops(t, f, to)
                            });
                        },
                    ),
                    Self::mi_sub("e.legato", t("edit.legato"), Sub::LegatoGap, cx),
                    Self::mi_sub("e.len", t("edit.set_length"), Sub::LenSet, cx),
                    Self::mi_sub("e.velset", t("edit.set_velocity"), Sub::VelSet, cx),
                    Self::mi_sub("e.relset", t("edit.set_release"), Sub::RelSet, cx),
                    Self::msep(),
                    Self::mi("e.velup", t("edit.vel_up"), "", None, cx, |v, _e, _cx| {
                        v.apply_region_op("vel ×1.25", |d, t, f, to| {
                            d.scale_velocity_ops(t, f, to, 1.25)
                        });
                    }),
                    Self::mi("e.veldn", t("edit.vel_dn"), "", None, cx, |v, _e, _cx| {
                        v.apply_region_op("vel ×0.8", |d, t, f, to| {
                            d.scale_velocity_ops(t, f, to, 0.8)
                        });
                    }),
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
                    ),
                    Self::mi_sub("v.theme", t("view.theme"), Sub::Theme, cx),
                    Self::mi(
                        "v.hc",
                        t("view.hc"),
                        "",
                        Some(self.theme == theme::Theme::high_contrast()),
                        cx,
                        |v, _e, cx| {
                            v.toggle_hc(cx);
                        },
                    ),
                    Self::msep(),
                    Self::mi(
                        "v.zin",
                        t("view.zoom_in"),
                        "Ctrl+=",
                        None,
                        cx,
                        |v, _e, cx| {
                            v.zoom_by(1.3, cx);
                        },
                    ),
                    Self::mi(
                        "v.zout",
                        t("view.zoom_out"),
                        "Ctrl+-",
                        None,
                        cx,
                        |v, _e, cx| {
                            v.zoom_by(1.0 / 1.3, cx);
                        },
                    ),
                    Self::mi(
                        "v.z0",
                        t("view.zoom_reset"),
                        "Ctrl+0",
                        None,
                        cx,
                        |v, _e, cx| {
                            v.zoom_set(0.08, cx);
                        },
                    ),
                    Self::msep(),
                    Self::mi_sub("v.rowh", t("view.row_height"), Sub::RowH, cx),
                    Self::mi(
                        "v.fold",
                        t("view.fold"),
                        "",
                        Some(self.fold),
                        cx,
                        |v, _e, cx| {
                            v.set_fold(!v.fold, cx);
                        },
                    ),
                    Self::mi(
                        "v.drum",
                        t("view.drum"),
                        "",
                        Some(self.drum),
                        cx,
                        |v, _e, cx| {
                            v.set_drum(!v.drum, cx);
                        },
                    ),
                    Self::mi_sub("v.scale", t("view.scale"), Sub::Scale, cx),
                    Self::msep(),
                    Self::mi_sub("v.lane", t("view.lane"), Sub::Lane, cx),
                    Self::mi_sub("v.enc", t("view.encoding"), Sub::Enc, cx),
                ],
                TopMenu::Track => {
                    let mut items = vec![
                        Self::mi("t.rename", t("track.rename"), "", None, cx, |v, w, cx| {
                            v.focus_rename(w, cx);
                        }),
                        Self::msep(),
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
                        ),
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
                        ),
                        Self::msep(),
                        Self::mi_sub("t.chan", t("track.channel"), Sub::Chan, cx),
                        Self::mi_sub("t.dest", t("track.dest"), Sub::Dest, cx),
                    ];
                    if sel_is_plugin {
                        items.push(Self::msep());
                        items.push(Self::mi(
                            "t.gui",
                            t("track.plugin_gui"),
                            "",
                            None,
                            cx,
                            |v, _e, _cx| {
                                v.open_plugin_gui();
                            },
                        ));
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
                            "",
                            Some(self.plugin_window.is_some()),
                            cx,
                            |v, _e, _cx| v.open_plugin_gui(),
                        ));
                    }
                    if sel_plugin_failed {
                        items.push(Self::mi(
                            "o.retry",
                            t("output.retry"),
                            "",
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
                        Self::mi("o.rescan", t("output.rescan"), "", None, cx, |v, _e, cx| {
                            v.rescan_plugins(crate::ScanMode::Changed);
                            cx.notify();
                        }),
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
                        ),
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
                    ),
                    Self::mi(
                        "tr.rec",
                        t("transport.record"),
                        "",
                        Some(self.rec.is_some()),
                        cx,
                        |v, _e, _cx| {
                            v.toggle_record();
                        },
                    ),
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
                    ),
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
                    ),
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
                    ),
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
                        "tr.cin",
                        t("transport.count_in"),
                        "",
                        Some(self.count_in),
                        cx,
                        |v, _e, _cx| {
                            v.count_in = !v.count_in;
                            v.save_global();
                        },
                    ),
                    Self::msep(),
                    Self::mi(
                        "tr.aud",
                        t("transport.audition"),
                        "",
                        Some(self.aud_enabled),
                        cx,
                        |v, _e, _cx| {
                            v.aud_enabled = !v.aud_enabled;
                            // disabling mid-ring must silence immediately
                            if !v.aud_enabled {
                                v.audition_off();
                            }
                            v.save_global();
                        },
                    ),
                    Self::mi_sub("tr.audv", t("transport.aud_vel"), Sub::AudVel, cx),
                    Self::mi_sub("tr.audd", t("transport.aud_dur"), Sub::AudDur, cx),
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
                    ),
                    Self::mi("h.about", t("help.about"), "", None, cx, |v, _e, _cx| {
                        v.status = concat!(
                            "midi-editor ",
                            env!("CARGO_PKG_VERSION"),
                            " — pure-SMF editor"
                        )
                        .into();
                    }),
                    Self::mi("h.mcp", t("help.mcp"), "", None, cx, |v, _e, _cx| {
                        v.status =
                            "MCP: http://127.0.0.1:7878/mcp (mcp-bridge for stdio clients)".into();
                    }),
                    Self::mi("h.logs", t("help.open_logs"), "", None, cx, |v, _e, cx| {
                        v.open_logs(cx);
                    }),
                    Self::mi(
                        "h.diag",
                        t("help.export_diag"),
                        "",
                        None,
                        cx,
                        |v, _e, cx| {
                            v.export_diagnostics(cx);
                        },
                    ),
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
                .bg(rgb(BG_RAISED))
                .border_1()
                .border_color(rgb(BORDER_C))
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
                    Sub::Quant => {
                        let g = self.snap_ticks().max(self.td().min_grid_ticks() as i64) as u64;
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
                            })
                            .collect()
                    }
                    Sub::LenSet => {
                        // metrical: note fractions; SMPTE: frame/second
                        // spans — never a fake-PPQ musical grid
                        let opts: Vec<(String, u64)> = match td {
                            TimeDisplay::Metrical { ppq } => vec![
                                ("1/32".into(), ppq / 8),
                                ("1/16".into(), ppq / 4),
                                ("1/8".into(), ppq / 2),
                                ("1/4".into(), ppq),
                                ("1 bar".into(), ppq * 4),
                            ],
                            TimeDisplay::Smpte { .. } => {
                                let f = td.cell_ticks();
                                let s = td.bar_ticks();
                                vec![
                                    ("1 frame".into(), f),
                                    ("5 frames".into(), f * 5),
                                    ("10 frames".into(), f * 10),
                                    ("1 s".into(), s),
                                    ("5 s".into(), s * 5),
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
                            })
                            .collect()
                    }
                    // release velocity lives on the note-OFF — a nonzero
                    // value upgrades a 0x90-vel0 off to a real 0x80
                    Sub::RelSet => {
                        let opts = [
                            ("zero (0)", 0u8),
                            ("soft (32)", 32),
                            ("medium (64)", 64),
                            ("hard (100)", 100),
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
                    .bg(rgb(BG_RAISED))
                    .border_1()
                    .border_color(rgb(BORDER_C))
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
            const ROWS: [(&str, &str); 29] = [
                ("Space", "Play / stop"),
                ("F1", "This panel"),
                ("F10", "Focus the menu bar"),
                ("Tab / Shift+Tab", "Move focus between regions"),
                ("Esc", "Close menus / clear selection"),
                ("Ctrl+N / O / S", "New / Open / Save"),
                ("Ctrl+Z / Y", "Undo / redo"),
                ("Ctrl+A", "Select all notes"),
                ("Ctrl+X / C / V", "Cut / copy / paste"),
                ("Ctrl+D", "Duplicate selection"),
                ("Del", "Delete selection"),
                ("1 / 2 / 3", "Select / draw / erase tool"),
                ("← →", "Nudge by grid step / move cursor"),
                ("Shift+← →", "Nudge by 1 tick"),
                ("↑ ↓", "Transpose by semitone / move cursor"),
                ("Shift+↑ ↓", "Transpose by octave"),
                ("Enter (roll)", "Select note / insert note at cursor"),
                ("↑ ↓ (tracks)", "Select track"),
                ("M / S / C (tracks)", "Mute / solo / channel"),
                ("Enter / F2 (tracks)", "Rename track"),
                ("← → ↑ ↓ (menus)", "Navigate menus"),
                ("↑ ↓ + Enter (event list)", "Select event / seek to it"),
                ("↑ ↓ (lane)", "Velocity of selected notes"),
                ("Alt+drag note", "Duplicate note(s)"),
                ("Right-edge drag", "Resize note"),
                ("Click ruler", "Seek playhead"),
                ("Double-click ruler", "Play from here"),
                ("Click minimap", "Jump to position"),
                ("Ctrl+wheel", "Zoom timeline"),
            ];
            let panel = div()
                .id("help-panel")
                .test_support()
                .role(Role::Dialog)
                .aria_label(t("help.shortcuts"))
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
                .text_size(px(metrics::TEXT_LG))
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
                        .hover(|s| s.bg(rgb(0x2a2a35)))
                        .text_color(rgb(if current { 0x9fd0ff } else { 0xd8d8e0 }))
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
                        .bg(rgb(if o == cur_sr { 0x2f4f6f } else { 0x2a2a35 }))
                        .hover(|s| s.bg(rgb(0x3a3a48)))
                        .text_color(rgb(if o == cur_sr { 0xd8f0ff } else { 0x9fd0ff }))
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
                        .bg(rgb(if o == cur_bs { 0x2f4f6f } else { 0x2a2a35 }))
                        .hover(|s| s.bg(rgb(0x3a3a48)))
                        .text_color(rgb(if o == cur_bs { 0xd8f0ff } else { 0x9fd0ff }))
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
                        .text_color(rgb(0x9999aa))
                        .child(note.clone())
                        .into_any_element(),
                );
            }
            rows.push(
                div()
                    .h(px(1.0))
                    .mx_2()
                    .my_1()
                    .bg(rgb(0x2a2a35))
                    .into_any_element(),
            );
            rows.push(
                div()
                    .h(px(18.0))
                    .px_2()
                    .mx_1()
                    .text_size(px(9.5))
                    .text_color(rgb(0x7a7a90))
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
                        (t("plugin.state_ready"), 0x8fd0a0, false, detail, audio)
                    }
                    Some(PluginState::Loading { .. }) => {
                        (t("plugin.state_loading"), 0xe0b050, false, None, None)
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
                            None,
                        )
                    }
                    _ => (t("plugin.state_idle"), 0x77778a, false, None, None),
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
                            .child(div().text_color(rgb(0x888899)).child(vendor)),
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
                            .text_color(rgb(if ok { 0x9999aa } else { 0xe06060 }))
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
                        .text_color(rgb(0xe0b050))
                        .child(
                            div()
                                .flex()
                                .gap_1()
                                .items_center()
                                .child(format!("{}  {}", name, t("plugin.state_failed")))
                                .child(
                                    div()
                                        .text_color(rgb(0x888899))
                                        .child(t("output.quarantined_short").to_string()),
                                ),
                        )
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(rgb(0x9999aa))
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
                .bg(rgb(0x20202c))
                .border_1()
                .border_color(rgb(0x3c3c4a))
                .rounded_lg()
                .shadow_lg()
                .text_size(px(metrics::TEXT_LG))
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
            .id("editor")
            .test_support()
            .role(Role::Application)
            .aria_label(t("a11y.editor"))
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
                let k = ev.keystroke.key.as_str();
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
                match (ctrl, shift, k) {
                    (true, false, "z") => this.undo(cx),
                    (true, false, "y") | (true, true, "z") => this.redo(cx),
                    (true, false, "s") => this.save(cx),
                    (true, false, "o") => {
                        this.confirm_discard_or_save(PendingAction::OpenDialog, w, cx)
                    }
                    (true, false, "n") => {
                        this.confirm_discard_or_save(PendingAction::NewFile, w, cx)
                    }
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
                        this.sel_events.clear();
                        cx.notify();
                    }
                    (true, false, "x") => this.copy_selected(true, cx),
                    (true, false, "c") => this.copy_selected(false, cx),
                    (true, false, "v") => this.paste(cx),
                    (true, false, "d") => this.duplicate_selected(cx),
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
                    (false, false, " ") | (false, false, "space") => this.toggle_play(cx),
                    // arrows/edit keys act on the roll only while the roll
                    // context (its handle or the root fallback) owns focus —
                    // tracks/lane/events have their own bindings
                    (false, s, "left" | "right" | "up" | "down" | "enter")
                        if this.roll_fh.contains_focused(w, cx) || this.focus.is_focused(w) =>
                    {
                        match (s, k) {
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
                    }
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
            .on_drop(cx.listener(|v, paths: &ExternalPaths, w, cx| {
                if let Some(p) = paths.paths().iter().find(|p| {
                    matches!(
                        p.extension().and_then(|e| e.to_str()),
                        Some("mid") | Some("smf") | Some("midi")
                    )
                }) {
                    v.confirm_discard_or_save(PendingAction::OpenPath(p.clone()), w, cx);
                }
            }))
    }
}

// --- menubar helpers -----------------------------------------------------------

impl EditorView {
    /// Section header row inside a dropdown.
    fn mhead(label: impl Into<SharedString>) -> MenuRow {
        MenuRow::Head(label.into())
    }

    fn dest_rows(
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
                let selected = if kind == DestPick::Track {
                    has_track_dest && eff_dest == i
                } else {
                    def_dest == i
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
                        if kind == DestPick::Track {
                            sh.track_dest.insert(v.sel_track, i);
                        } else {
                            sh.default_dest = i;
                        }
                        drop(sh);
                        v.audition_off();
                        v.persist();
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
                        if kind == DestPick::Track {
                            sh.track_dest.insert(v.sel_track, i);
                        } else {
                            sh.default_dest = i;
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

    /// Dropdown leaf row; hovering clears the open cascade.
    fn mi(
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
            enabled: true,
            act: std::rc::Rc::new(f),
        })
    }

    /// Submenu leaf row — must not clear the cascade it lives in.
    fn mi_leaf(
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        shortcut: &'static str,
        check: Option<bool>,
        cx: &mut Context<Self>,
        f: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> MenuRow {
        Self::mi(id, label, shortcut, check, cx, f)
    }

    /// Informational row ("no MIDI ports", "scanning…") — dimmed, not
    /// keyboard-selectable and not activatable.
    fn mi_dis(
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
    fn mi_plugin(
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
    fn mi_sub(
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
    fn msep() -> MenuRow {
        MenuRow::Sep
    }

    /// Render one menu model row. `i` is its index in `menu_rows` (dropdown)
    /// or `sub_rows` (cascade); `selected` marks the keyboard selection which
    /// mouse hover also drives, so both inputs highlight the same row.
    fn row_el(
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
                        .on_mouse_move(cx.listener(move |v, _e: &MouseMoveEvent, _w, cx| {
                            if in_sub {
                                v.sub_sel = Some(i);
                            } else {
                                v.menu_sel = Some(i);
                                // leaving a submenu parent closes the cascade
                                v.open_sub = None;
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
    fn prop_panel(&self, cx: &mut Context<Self>) -> Div {
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
                    .hover(|s| s.bg(rgba(0xffffff10)))
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
struct Tip(SharedString);

impl Render for Tip {
    fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let th = theme::current();
        div()
            .px_2()
            .py_1()
            .bg(rgb(0x26262e))
            .border_1()
            .border_color(rgb(0x3c3c4a))
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
    fn vsep() -> Div {
        div()
            .w(px(1.0))
            .h(px(metrics::VSEP_H))
            .mx_1()
            .bg(rgb(theme::current().border))
    }

    /// Icon button: 26px square, tooltip, neutral gray icon.
    /// Screen readers get `tip` as the name and the on/off state as a toggle.
    fn ibtn(
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
    fn ibtn_w(
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
    fn ibtn_c(
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
