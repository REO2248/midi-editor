//! View state + navigation: zoom/scroll/fold/lanes/snap/tool, palette
//! (command palette) key handling, keyboard focus areas and menu arrow-key
//! navigation, cursor/track stepping, pointer hit-tests and drag tracking.
//! Everything here mutates only EditorView state — document truth changes
//! still go through `apply_tx` in the parent module.

use super::*;

pub(crate) const GM_DRUMS: [&str; 47] = [
    "Acoustic Bass Drum",
    "Bass Drum 1",
    "Side Stick",
    "Acoustic Snare",
    "Hand Clap",
    "Electric Snare",
    "Low Floor Tom",
    "Closed Hi-Hat",
    "High Floor Tom",
    "Pedal Hi-Hat",
    "Low Tom",
    "Open Hi-Hat",
    "Low-Mid Tom",
    "Hi-Mid Tom",
    "Crash Cymbal 1",
    "High Tom",
    "Ride Cymbal 1",
    "Chinese Cymbal",
    "Ride Bell",
    "Tambourine",
    "Splash Cymbal",
    "Cowbell",
    "Crash Cymbal 2",
    "Vibraslap",
    "Ride Cymbal 2",
    "Hi Bongo",
    "Low Bongo",
    "Mute Hi Conga",
    "Open Hi Conga",
    "Low Conga",
    "High Timbale",
    "Low Timbale",
    "High Agogo",
    "Low Agogo",
    "Cabasa",
    "Maracas",
    "Short Whistle",
    "Long Whistle",
    "Short Guiro",
    "Long Guiro",
    "Claves",
    "Hi Wood Block",
    "Low Wood Block",
    "Mute Cuica",
    "Open Cuica",
    "Mute Triangle",
    "Open Triangle",
];

/// GM drum name for a key, if it has one (35..=81).
pub(crate) fn drum_name(key: u8) -> Option<&'static str> {
    (35..=81)
        .contains(&key)
        .then(|| GM_DRUMS[key as usize - 35])
}

/// Identity row map: all 128 keys, highest first (row 0 = key 127).
pub(crate) fn all_keys() -> Vec<u8> {
    (0u8..128).rev().collect()
}

/// Row map for a folded view: only the pitches `notes` actually uses,
/// highest first. `drum` restricts to channel 9 (0-indexed) on the selected
/// track — the percussion view. Empty input falls back to the identity map
/// so the roll is never blank.
pub(crate) fn used_keys(notes: &[Note], drum: bool, sel_track: usize) -> Vec<u8> {
    let mut mask = [false; 128];
    for n in notes {
        if drum && !(n.track == sel_track && n.channel == 9) {
            continue;
        }
        mask[n.key as usize] = true;
    }
    let v: Vec<u8> = (0..128u8).rev().filter(|k| mask[*k as usize]).collect();
    if v.is_empty() {
        all_keys()
    } else {
        v
    }
}

/// Pitch-class membership of a diatonic scale (major or natural minor).
pub(crate) fn scale_pcs_of(root: u8, minor: bool) -> [bool; 12] {
    const MAJ: [u8; 7] = [0, 2, 4, 5, 7, 9, 11];
    const MIN: [u8; 7] = [0, 2, 3, 5, 7, 8, 10];
    let mut out = [false; 12];
    for &iv in if minor { &MIN } else { &MAJ } {
        out[((root + iv) % 12) as usize] = true;
    }
    out
}

/// Tonic pitch class of a 0x59 key signature: sf counts fifths from C
/// (negative = flats), mi selects the relative minor a minor third up.
pub(crate) fn keysig_root(sf: i8, minor: bool) -> u8 {
    let maj = (7 * sf as i32).rem_euclid(12);
    (if minor { maj + 9 } else { maj } % 12) as u8
}

/// Snap grid divisors of a whole note; 0 = snap off.
pub(crate) const SNAPS: [(u32, bool, &str); 10] = [
    (0, false, "off"),
    (1, false, "1"),
    (2, false, "1/2"),
    (4, false, "1/4"),
    (4, true, "1/4T"),
    (8, false, "1/8"),
    (8, true, "1/8T"),
    (16, false, "1/16"),
    (16, true, "1/16T"),
    (32, false, "1/32"),
];

impl EditorView {
    /// Select every note in the selected track.
    pub(crate) fn select_all(&mut self, cx: &mut Context<Self>) {
        self.selection = self
            .notes
            .iter()
            .filter(|n| n.track == self.sel_track)
            .map(|n| n.on_id)
            .collect();
        cx.notify();
    }

    /// Zoom by a factor around the viewport center (toolbar buttons/keys).
    pub(crate) fn zoom_by(&mut self, f: f32, cx: &mut Context<Self>) {
        self.zoom_set((self.zoom * f).clamp(ZOOM_MIN, ZOOM_MAX), cx);
    }

    /// Zoom to an absolute factor, keeping the tick at the viewport center
    /// fixed so the view doesn't lurch toward tick 0 on every change.
    pub(crate) fn zoom_set(&mut self, z: f32, cx: &mut Context<Self>) {
        let half = f32::from(self.roll_bounds.get().size.width) / 2.0;
        self.scroll_x = reanchor(self.scroll_x, self.zoom, z, half);
        self.zoom = z;
        self.clamp_scroll();
        self.persist();
        cx.notify();
    }

    /// Keep both scroll axes inside the content: 128 key rows vertically, the
    /// song end horizontally. Called from render so window resizes, zooms,
    /// and edits self-heal without every call site remembering to.
    pub(crate) fn clamp_scroll(&mut self) {
        let b = self.roll_bounds.get();
        let w = f32::from(b.size.width);
        let h = f32::from(b.size.height);
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        self.scroll_y = clamp_span(self.scroll_y, self.vis_keys.len() as f32 * self.note_h, h);
        // the edit cursor is live content too: a doc narrower than the
        // viewport would pin scroll_x=0 and let the cursor walk off-screen
        let end = self
            .doc_end_ticks()
            .max(self.cursor_tick + self.cursor_insert_len());
        self.scroll_x = clamp_span(self.scroll_x, end as f32 * self.zoom + 32.0, w);
    }

    /// Point the view at the current document's content (first note with a
    /// left margin, median pitch centered). Runs on open/new-file before the
    /// per-file sidecar is applied, so a saved position still wins — but a
    /// first-time open never inherits the previous file's scroll offsets,
    /// which pointed at wherever the old file happened to be looking.
    pub(crate) fn reset_view_to_content(&mut self) {
        let first = self.notes.first().map(|n| n.start_tick).unwrap_or(0);
        let mid = if self.notes.is_empty() {
            None
        } else {
            let mut keys: Vec<u8> = self.notes.iter().map(|n| n.key).collect();
            keys.sort_unstable();
            Some(keys[keys.len() / 2] as i32)
        };
        // center on the median pitch's *row* (the row map may be folded)
        let mid_row = mid.and_then(|k| {
            let r = self.row_of[k as usize];
            (r >= 0).then_some(r)
        });
        let (x, y) = content_view(first, mid_row, self.zoom, self.note_h);
        self.scroll_x = x;
        self.scroll_y = y;
        self.cursor_tick = first;
        self.cursor_key = mid.unwrap_or(60);
        self.ev_sel = 0;
    }

    /// Frame the selected notes in the viewport (10% breathing room).
    pub(crate) fn zoom_to_selection(&mut self, cx: &mut Context<Self>) {
        let mut lo = u64::MAX;
        let mut hi = 0u64;
        for n in self
            .notes
            .iter()
            .filter(|n| self.selection.contains(&n.on_id))
        {
            lo = lo.min(n.start_tick);
            hi = hi.max(n.end_tick.unwrap_or(n.start_tick + 1));
        }
        if lo > hi {
            self.status = t("status.nosel").into();
            cx.notify();
            return;
        }
        self.zoom_to_span(lo, hi, cx);
    }

    /// Frame the whole song.
    pub(crate) fn zoom_to_song(&mut self, cx: &mut Context<Self>) {
        self.zoom_to_span(0, self.doc_end_ticks(), cx);
    }

    pub(crate) fn zoom_to_span(&mut self, lo: u64, hi: u64, cx: &mut Context<Self>) {
        let w = f32::from(self.roll_bounds.get().size.width);
        if w <= 0.0 {
            return;
        }
        let span = (hi - lo).max(1) as f32;
        self.zoom = (w * 0.9 / span).clamp(ZOOM_MIN, ZOOM_MAX);
        self.scroll_x = (lo as f32 * self.zoom - w * 0.05).max(0.0);
        self.clamp_scroll();
        self.persist();
        cx.notify();
    }

    /// Seek the playhead to the previous/next marker in the file.
    pub(crate) fn marker_step(&mut self, dir: i64, cx: &mut Context<Self>) {
        let tick = self.doc(|d| d.tempo_map.us_to_tick(self.play_us));
        let dst = if dir > 0 {
            self.doc_ui.markers.iter().find(|m| m.0 > tick)
        } else {
            self.doc_ui.markers.iter().rev().find(|m| m.0 < tick)
        };
        match dst {
            Some(&(t, id, tr, ref _n)) => {
                self.meta_sel = Some((tr, id));
                self.seek_to_tick(t, false, cx);
                self.center_playhead();
            }
            None => {
                self.status = t("status.no_marker").into();
                cx.notify();
            }
        }
    }

    /// Seek the playhead to the previous/next event of the selected track —
    /// the same jump whether the roll or the event list drove the command.
    pub(crate) fn event_step(&mut self, dir: i64, cx: &mut Context<Self>) {
        let tick = self.doc(|d| d.tempo_map.us_to_tick(self.play_us));
        let tr = self.sel_track;
        let dst = self.doc(|d| {
            let it = d
                .tracks
                .get(tr)
                .into_iter()
                .flat_map(|t| t.events.iter().map(|e| e.tick));
            if dir > 0 {
                it.filter(|&x| x > tick).min()
            } else {
                it.filter(|&x| x < tick).max()
            }
        });
        match dst {
            Some(t) => {
                self.seek_to_tick(t, false, cx);
                self.center_playhead();
            }
            None => {
                self.status = t("status.no_event").into();
                cx.notify();
            }
        }
    }

    /// Mode of the focused lane — drives the View > Lane checkmarks and
    /// the status-bar chip.
    pub(crate) fn lane_mode(&self) -> LaneMode {
        self.lanes
            .get(self.lane_focus)
            .map(|c| c.mode)
            .unwrap_or(LaneMode::Velocity)
    }

    pub(crate) fn set_lane(&mut self, m: LaneMode, cx: &mut Context<Self>) {
        if let Some(c) = self.lanes.get_mut(self.lane_focus) {
            c.mode = m;
        }
        self.lane_sel.clear();
        self.persist();
        cx.notify();
    }

    /// Stack a new lane below the existing ones, preferring a mode not
    /// already shown.
    pub(crate) fn add_lane(&mut self, cx: &mut Context<Self>) {
        if self.lanes.len() >= LANES_MAX {
            return;
        }
        let mode = LANE_MODES
            .iter()
            .find(|m| !self.lanes.iter().any(|c| c.mode == **m))
            .copied()
            .unwrap_or(LaneMode::Velocity);
        self.lanes.push(LaneCfg {
            mode,
            ..LaneCfg::default()
        });
        self.lane_focus = self.lanes.len() - 1;
        self.persist();
        cx.notify();
    }

    pub(crate) fn remove_lane(&mut self, cx: &mut Context<Self>) {
        if self.lanes.len() <= 1 {
            return;
        }
        self.lanes.remove(self.lane_focus.min(self.lanes.len() - 1));
        self.lane_focus = self.lane_focus.min(self.lanes.len() - 1);
        self.persist();
        cx.notify();
    }

    /// snap interval in ticks (0 = off)
    /// Current snap step in ticks (0 = off). Triplet entries are 2/3 of the
    /// duple cell — 1/8T = a third of a quarter note.
    /// Ticks per quarter note for display grids; SMPTE docs use the same
    /// 480 fallback the pre-format-2 code did (position labels only).
    pub(crate) fn ppq(&self) -> u64 {
        self.doc(|d| d.tempo_map.ppq().unwrap_or(480))
    }

    pub(crate) fn snap_ticks(&self) -> i64 {
        let (div, trip, _) = SNAPS[self.snap_idx];
        if div == 0 {
            return 0;
        }
        let base = self.td().snap_base_ticks() as i64 / div as i64;
        if trip {
            base * 2 / 3
        } else {
            base
        }
    }

    /// floor `t` onto the snap grid (used for note starts)
    pub(crate) fn snap_down(&self, t: i64) -> i64 {
        let s = self.snap_ticks();
        if s <= 0 {
            t
        } else {
            t - t.rem_euclid(s)
        }
    }

    /// nearest grid point (used for note ends / dragged positions)
    pub(crate) fn snap_round(&self, t: i64) -> i64 {
        let s = self.snap_ticks();
        if s <= 0 {
            t
        } else {
            ((t.max(0) + s / 2) / s) * s
        }
    }

    pub(crate) fn set_tool(&mut self, tool: Tool, cx: &mut Context<Self>) {
        self.tool = tool;
        self.persist();
        cx.notify();
    }

    pub(crate) fn set_snap(&mut self, idx: usize, cx: &mut Context<Self>) {
        self.snap_idx = idx;
        self.persist();
        cx.notify();
    }

    /// Vertical zoom to an absolute row height, keeping the row at
    /// `anchor_off` pixels from the viewport's top edge fixed (the cursor's
    /// pitch stays under the cursor).
    pub(crate) fn vzoom_set(&mut self, h: f32, anchor_off: f32, cx: &mut Context<Self>) {
        let anchor_row = (anchor_off.max(0.0) + self.scroll_y) / self.note_h;
        self.note_h = h.clamp(NOTE_H_MIN, NOTE_H_MAX);
        self.scroll_y = (anchor_row * self.note_h - anchor_off.max(0.0)).max(0.0);
        self.clamp_scroll();
        self.persist();
        cx.notify();
    }

    /// Menu-driven vertical zoom — anchors on the selected note's pitch when
    /// there is one, else the viewport center.
    pub(crate) fn vzoom_by(&mut self, f: f32, cx: &mut Context<Self>) {
        let anchor = self
            .selection
            .iter()
            .next()
            .and_then(|id| self.notes.iter().find(|n| n.on_id == *id))
            .and_then(|n| {
                let r = self.row_of[n.key as usize];
                (r >= 0).then_some(r as f32 * self.note_h - self.scroll_y)
            })
            .unwrap_or_else(|| f32::from(self.roll_bounds.get().size.height) / 2.0);
        self.vzoom_set(self.note_h * f, anchor, cx);
    }

    /// Fold/drum/scale toggles rebuild the row map on the next refresh; the
    /// selection may hold keys that fold out, which paint/hit-test already
    /// skip via `row_of`.
    pub(crate) fn set_fold(&mut self, on: bool, cx: &mut Context<Self>) {
        self.fold = on;
        self.refresh_derived();
        self.clamp_scroll();
        self.persist();
        cx.notify();
    }

    pub(crate) fn set_drum(&mut self, on: bool, cx: &mut Context<Self>) {
        self.drum = on;
        self.refresh_derived();
        self.clamp_scroll();
        self.persist();
        cx.notify();
    }

    pub(crate) fn set_scale(&mut self, sel: i8, minor: bool, cx: &mut Context<Self>) {
        self.scale_sel = sel;
        self.scale_minor = minor;
        self.refresh_derived();
        self.persist();
        cx.notify();
    }

    pub(crate) fn cycle_snap(&mut self, cx: &mut Context<Self>) {
        self.snap_idx = (self.snap_idx + 1) % SNAPS.len();
        self.persist();
        cx.notify();
    }

    /// Focus the track-name input with the current name selected (Track >
    /// Rename, or Enter/F2 while the track list is focused).
    pub(crate) fn focus_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.doc(|d| {
            d.tracks
                .get(self.sel_track)
                .and_then(|t| t.name.as_ref())
                .map(|b| smf_core::decode_text(b, self.enc_override.or(d.text_encoding_hint())))
                .unwrap_or_default()
        });
        self.input.update(cx, |i, cx| {
            i.set_value(name, window, cx);
            i.select_all(window, cx);
        });
        let fh = self.input.read(cx).focus_handle(cx);
        window.focus(&fh, cx);
        cx.notify();
    }

    /// Open the palette overlay (Commands or Keys mode), closing menus.
    pub(crate) fn open_palette(
        &mut self,
        mode: PaletteMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_menu = None;
        self.open_sub = None;
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(t("ui.palette_hint")));
        let sub = cx.subscribe(
            &input,
            |v, _e, ev: &gpui_kit::component::input::InputEvent, cx| {
                if matches!(ev, gpui_kit::component::input::InputEvent::Change) {
                    // filter text changed → reset selection + repaint
                    if let Some(p) = v.palette.as_mut() {
                        p.sel = 0;
                    }
                    cx.notify();
                }
            },
        );
        self.palette = Some(Palette {
            mode,
            input: input.clone(),
            sel: 0,
            capture: None,
            _sub: sub,
        });
        let fh = input.read(cx).focus_handle(cx);
        window.focus(&fh, cx);
        cx.notify();
    }

    pub(crate) fn close_palette(&mut self, cx: &mut Context<Self>) {
        if self.palette.take().is_some() {
            cx.notify();
        }
    }

    /// Commands matching the palette's filter text (label or id).
    pub(crate) fn palette_rows(&self, cx: &App) -> Vec<&'static cmd::Command> {
        let Some(p) = &self.palette else {
            return Vec::new();
        };
        let q = p.input.read(cx).value().trim().to_lowercase();
        cmd::COMMANDS
            .iter()
            .filter(|c| {
                q.is_empty() || t(c.label_key).to_lowercase().contains(&q) || c.id.contains(&q)
            })
            .collect()
    }

    /// Every key pressed while the palette is open flows here (the root
    /// handler routes it); nav/select keys are consumed, the rest reach
    /// the filter input's own handler first and are ignored here.
    pub(crate) fn palette_key(
        &mut self,
        ev: &KeyDownEvent,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(p) = &self.palette else { return };
        let mode = p.mode;
        let capturing = p.capture;
        let k = ev.keystroke.key.as_str();

        // Keys-mode capture eats the next real keystroke as the new binding.
        if let Some(id) = capturing {
            match k {
                "escape" => {
                    if let Some(p) = self.palette.as_mut() {
                        p.capture = None;
                    }
                }
                // ignore modifiers on their own — wait for a real key
                "control" | "shift" | "alt" | "capslock" | "function" | "platform" => {}
                _ => {
                    if let Some(p) = self.palette.as_mut() {
                        p.capture = None;
                    }
                    let desc = cmd::describe(&ev.keystroke);
                    match self.keys.assign(id, &desc) {
                        Ok(()) => {
                            self.save_global();
                            let c = cmd::find(id).unwrap();
                            self.status = tf(
                                "ui.keys_bound",
                                &[("label", &cmd::label(c)), ("key", &cmd::format_key(&desc))],
                            )
                            .into();
                        }
                        Err(other) => {
                            self.status =
                                tf("ui.keys_conflict", &[("label", &cmd::label(other))]).into();
                        }
                    }
                }
            }
            // return focus to the filter after capture
            if let Some(p) = &self.palette {
                let fh = p.input.read(cx).focus_handle(cx);
                w.focus(&fh, cx);
            }
            cx.notify();
            cx.stop_propagation();
            return;
        }

        let rows_len = self.palette_rows(cx).len();
        let mut step = |d: isize| {
            if let Some(p) = self.palette.as_mut() {
                let n = rows_len.max(1) as isize;
                p.sel = ((p.sel as isize + d) % n + n) as usize % n as usize;
            }
        };
        match (k, ev.keystroke.modifiers.control) {
            ("escape", _) => self.close_palette(cx),
            ("tab", _) => {
                if let Some(p) = self.palette.as_mut() {
                    p.mode = match p.mode {
                        PaletteMode::Commands => PaletteMode::Keys,
                        PaletteMode::Keys => PaletteMode::Commands,
                    };
                    p.sel = 0;
                }
                cx.notify();
            }
            ("up", _) | ("down", _) => {
                step(if k == "up" { -1 } else { 1 });
                cx.notify();
            }
            ("pageup" | "page_up", _) | ("pagedown" | "page_down", _) => {
                step(if k.starts_with("pageup") || k == "page_up" {
                    -10
                } else {
                    10
                });
                cx.notify();
            }
            ("home", _) => {
                if let Some(p) = self.palette.as_mut() {
                    p.sel = 0;
                }
                cx.notify();
            }
            ("end", _) => {
                if let Some(p) = self.palette.as_mut() {
                    p.sel = rows_len.saturating_sub(1);
                }
                cx.notify();
            }
            ("enter", _) => self.palette_activate(w, cx),
            ("delete", false) | ("backspace", false) | ("r", true)
                // Keys mode: reset the selected command to its defaults.
                // (Del is also consumed by the filter input when focused, so
                // Ctrl+R is the reliable path — both are offered.)
                if mode == PaletteMode::Keys => {
                    let rows = self.palette_rows(cx);
                    if let Some(c) = rows.get(self.palette.as_ref().unwrap().sel).copied() {
                        self.keys.reset(c.id);
                        self.save_global();
                        self.status = tf("ui.keys_reset", &[("label", &cmd::label(c))]).into();
                        cx.notify();
                    }
                }
            _ => {}
        }
        cx.stop_propagation();
    }

    /// Enter or click on the selected palette row: run it (Commands) or
    /// begin keystroke capture (Keys).
    pub(crate) fn palette_activate(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let rows = self.palette_rows(cx);
        let Some(p) = &self.palette else { return };
        let mode = p.mode;
        let Some(&c) = rows.get(p.sel) else { return };
        match mode {
            PaletteMode::Commands => {
                self.close_palette(cx);
                (c.act)(self, w, cx);
            }
            PaletteMode::Keys => {
                // capture next keystroke: move focus off the filter so the
                // key can't be typed as text
                self.palette.as_mut().unwrap().capture = Some(c.id);
                w.focus(&self.focus.clone(), cx);
            }
        }
    }

    /// The focus area currently holding keyboard focus. The root handle and
    /// the roll handle both count as `Roll` — the canvas is the default
    /// editing context.
    pub(crate) fn area(&self, window: &Window, cx: &App) -> FocusArea {
        if self.menu_fh.contains_focused(window, cx) {
            FocusArea::MenuBar
        } else if self.tracks_fh.contains_focused(window, cx) {
            FocusArea::Tracks
        } else if self.lane_fh.contains_focused(window, cx) {
            FocusArea::Lane
        } else if self.events_fh.contains_focused(window, cx) {
            FocusArea::Events
        } else {
            FocusArea::Roll
        }
    }

    /// Focus handle for a region (with visibility fallbacks).
    pub(crate) fn fh_for(&self, area: FocusArea) -> FocusHandle {
        match area {
            FocusArea::MenuBar => self.menu_fh.clone(),
            FocusArea::Tracks => self.tracks_fh.clone(),
            FocusArea::Roll => self.roll_fh.clone(),
            FocusArea::Lane => self.lane_fh.clone(),
            FocusArea::Events if self.show_events => self.events_fh.clone(),
            FocusArea::Events => self.roll_fh.clone(),
        }
    }

    /// Focus can never be absent or land on a hidden region — reroute it.
    /// Called from render each frame so nothing can leave focus invisible.
    pub(crate) fn repair_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.focused(cx).is_none() {
            window.focus(&self.roll_fh, cx);
        }
        if !self.show_events && self.events_fh.contains_focused(window, cx) {
            window.focus(&self.roll_fh, cx);
        }
    }

    /// Open dropdown `m` under its menubar label (used by mouse and keys).
    pub(crate) fn open_menu_at(&mut self, m: TopMenu, cx: &mut Context<Self>) {
        self.open_menu = Some((m, menu_x(m)));
        self.open_sub = None;
        self.menu_sel = None;
        self.sub_sel = None;
        self.menu_bar_sel = MENUS.iter().position(|(mm, _, _)| *mm == m).unwrap_or(0);
        cx.notify();
    }

    /// Switch the open dropdown to a neighbouring menubar entry.
    pub(crate) fn menu_sibling(&mut self, dir: i64, cx: &mut Context<Self>) {
        let Some((m, _)) = self.open_menu else {
            return;
        };
        let i = MENUS.iter().position(|(mm, _, _)| *mm == m).unwrap_or(0) as i64;
        let ni = (i + dir).rem_euclid(MENUS.len() as i64) as usize;
        self.open_menu_at(MENUS[ni].0, cx);
    }

    /// Arrow-key movement inside the open dropdown/cascade.
    pub(crate) fn menu_step(&mut self, dir: i32, cx: &mut Context<Self>) {
        if self.open_sub.is_some() {
            self.sub_sel = next_selectable(&self.sub_rows, self.sub_sel, dir);
        } else {
            self.menu_sel = next_selectable(&self.menu_rows, self.menu_sel, dir);
        }
        cx.notify();
    }

    /// Open the cascade for the selected submenu row.
    pub(crate) fn open_selected_sub(&mut self, cx: &mut Context<Self>) {
        if let Some(i) = self.menu_sel {
            if let Some(MenuRow::Sub { sub, .. }) = self.menu_rows.get(i) {
                let y = row_y(&self.menu_rows, i);
                self.open_sub = Some((*sub, y));
                self.sub_sel = None;
            }
        }
        cx.notify();
    }

    /// Enter/Space on the highlighted row: run a leaf, descend into a ▸ row.
    pub(crate) fn menu_activate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_sub.is_none() {
            if let Some(i) = self.menu_sel {
                if matches!(self.menu_rows.get(i), Some(MenuRow::Sub { .. })) {
                    self.open_selected_sub(cx);
                    return;
                }
            }
        }
        let (rows, sel) = if self.open_sub.is_some() {
            (&self.sub_rows, self.sub_sel)
        } else {
            (&self.menu_rows, self.menu_sel)
        };
        let act = match sel.and_then(|i| rows.get(i)) {
            Some(MenuRow::Leaf(l)) => Some(l.act.clone()),
            _ => None,
        };
        if let Some(act) = act {
            self.open_menu = None;
            self.open_sub = None;
            self.menu_sel = None;
            self.sub_sel = None;
            act(self, window, cx);
            // restore focus to the region the command ran against
            let fh = self.fh_for(self.last_area);
            window.focus(&fh, cx);
        }
    }

    /// Keyboard control for the open menu. Returns true when the key was
    /// consumed; unhandled keys keep bubbling so chords still work.
    pub(crate) fn menu_key(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.open_menu.is_none() {
            return false;
        }
        match ev.keystroke.key.as_str() {
            "left" => {
                if self.open_sub.is_some() {
                    self.open_sub = None;
                    self.sub_sel = None;
                    cx.notify();
                } else {
                    self.menu_sibling(-1, cx);
                }
            }
            "right" => {
                if self.open_sub.is_none() {
                    let is_sub = self
                        .menu_sel
                        .and_then(|i| self.menu_rows.get(i))
                        .is_some_and(|r| matches!(r, MenuRow::Sub { .. }));
                    if is_sub {
                        self.open_selected_sub(cx);
                    } else {
                        self.menu_sibling(1, cx);
                    }
                }
            }
            "up" => self.menu_step(-1, cx),
            "down" => self.menu_step(1, cx),
            "enter" | " " | "space" => {
                // nothing highlighted yet: select the first row rather than
                // firing it — Enter activates on the next press
                if self.menu_sel.is_none() && self.sub_sel.is_none() {
                    self.menu_step(1, cx);
                } else {
                    self.menu_activate(window, cx);
                }
            }
            "escape" => {
                self.open_menu = None;
                self.open_sub = None;
                self.menu_sel = None;
                self.sub_sel = None;
                cx.notify();
            }
            _ => return false,
        }
        true
    }

    /// Arrows on the roll: nudge the selection, or move the edit cursor when
    /// nothing is selected (Enter inserts at the cursor).
    pub(crate) fn roll_arrow(&mut self, dtick: i64, dkey: i32, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            self.cursor_move(dtick, dkey, cx);
        } else {
            self.nudge(dtick, dkey, cx);
        }
    }

    /// Note length used by cursor inserts — same rule as `insert_note`.
    pub(crate) fn cursor_insert_len(&self) -> u64 {
        self.snap_ticks().max(self.ppq() as i64 / 4) as u64
    }

    /// Move the roll edit cursor and scroll it into view.
    pub(crate) fn cursor_move(&mut self, dtick: i64, dkey: i32, cx: &mut Context<Self>) {
        self.cursor_tick = (self.cursor_tick as i64 + dtick).max(0) as u64;
        self.cursor_key = (self.cursor_key + dkey).clamp(0, 127);
        self.ensure_cursor_visible();
        cx.notify();
    }

    /// Scroll the roll so the edit cursor is on screen with a small margin.
    pub(crate) fn ensure_cursor_visible(&mut self) {
        let b = self.roll_bounds.get();
        let w = f32::from(b.size.width);
        let h = f32::from(b.size.height);
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let x0 = self.cursor_tick as f32 * self.zoom;
        let x1 = x0 + self.cursor_insert_len() as f32 * self.zoom;
        let margin = 32.0;
        if x0 < self.scroll_x + margin {
            self.scroll_x = (x0 - margin).max(0.0);
        } else if x1 > self.scroll_x + w - margin {
            self.scroll_x = (x1 - w + margin).max(0.0);
        }
        let row_top = (127 - self.cursor_key) as f32 * NOTE_H;
        let row_bot = row_top + NOTE_H;
        if row_top < self.scroll_y + margin {
            self.scroll_y = (row_top - margin).max(0.0);
        } else if row_bot > self.scroll_y + h - margin {
            self.scroll_y = row_bot - h + margin;
        }
        self.clamp_scroll();
    }

    /// Enter on the roll: select the note under the cursor, or insert a new
    /// note at it when the cell is empty.
    pub(crate) fn cursor_activate(&mut self, cx: &mut Context<Self>) {
        let end = self.cursor_tick + self.cursor_insert_len();
        let hit = self.notes.iter().find(|n| {
            n.track == self.sel_track
                && n.key == self.cursor_key as u8
                && n.start_tick < end
                && n.end_tick.unwrap_or(n.start_tick + 1) > self.cursor_tick
        });
        if let Some(n) = hit {
            self.selection = BTreeSet::from([n.on_id]);
            cx.notify();
        } else {
            let len = self.cursor_insert_len();
            self.insert_note_len(self.cursor_tick, self.cursor_key as u8, len, cx);
        }
    }

    /// ±track selection from the keyboard.
    pub(crate) fn track_step(&mut self, dir: i64, cx: &mut Context<Self>) {
        let n = self.doc(|d| d.tracks.len());
        if n == 0 {
            return;
        }
        let i = (self.sel_track as i64 + dir).clamp(0, n as i64 - 1) as usize;
        if i != self.sel_track {
            self.sel_track = i;
            cx.notify();
        }
    }

    pub(crate) fn toggle_mute(&mut self, i: usize) {
        {
            let mut sh = lock_shared(&self.shared);
            if !sh.muted.remove(&i) {
                sh.muted.insert(i);
            }
        }
        self.persist();
    }

    pub(crate) fn toggle_solo(&mut self, i: usize) {
        {
            let mut sh = lock_shared(&self.shared);
            if !sh.soloed.remove(&i) {
                sh.soloed.insert(i);
            }
        }
        self.persist();
    }

    /// Cycle the track's insert/edit channel — editor state, not a file
    /// write. The `FF 20` channel-prefix meta is only touched through the
    /// explicit Track ▸ Channel Prefix command (or MCP); playback always
    /// follows each event's own channel.
    pub(crate) fn cycle_chan(&mut self, i: usize) {
        let cur = self.edit_channel_of(
            i,
            self.doc(|d| d.tracks.get(i).map(|t| t.out_channel).unwrap_or(0)),
        );
        self.edit_ch.insert(i, (cur + 1) % 16);
        self.persist();
    }

    /// Apply the rename field to the selected track.
    pub(crate) fn apply_rename(&mut self, cx: &mut Context<Self>) {
        let name = self.input.read(cx).value().to_string();
        if name.is_empty() {
            return;
        }
        let ops = {
            let mut sh = lock_shared(&self.shared);
            sh.doc.set_track_name_ops(self.sel_track, &name)
        };
        self.apply_tx("set track name", ops);
    }

    /// Enter in the rename field: commit the name and hand focus back to
    /// the track list so focus never stays trapped in the input.
    pub(crate) fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_rename(cx);
        window.focus(&self.tracks_fh, cx);
        cx.notify();
    }

    /// Move the event-list selection by `dir` rows, scrolling it into view.
    pub(crate) fn ev_step(&mut self, dir: i64, cx: &mut Context<Self>) {
        let n = self.events.len();
        if n == 0 {
            return;
        }
        let i = (self.ev_sel as i64 + dir).clamp(0, n as i64 - 1) as usize;
        if i != self.ev_sel {
            self.ev_sel = i;
            // keyboard navigation selects like a plain click — the
            // inspector follows the cursor row
            if let Some(&Some((_, _, id))) = self.event_refs.get(i) {
                self.sel_events = BTreeSet::from([id]);
                self.prop_field = None;
            }
            self.events_scroll
                .scroll_to_item(i, ScrollStrategy::Nearest);
            cx.notify();
        }
    }

    /// Enter on the event list: move the playhead to the row's tick.
    pub(crate) fn ev_activate(&mut self, cx: &mut Context<Self>) {
        if let Some(row) = self.events.get(self.ev_sel) {
            let tick = row.tick;
            self.seek_to_tick(tick, false, cx);
        }
    }

    /// tick,key under a window-space mouse position
    pub(crate) fn hit(&self, pos: Point<Pixels>) -> (i64, i32) {
        let b = self.roll_bounds.get();
        let x = f32::from(pos.x) - f32::from(b.origin.x);
        let y = f32::from(pos.y) - f32::from(b.origin.y);
        roll_hit(
            x,
            y,
            self.scroll_x,
            self.scroll_y,
            self.zoom,
            self.note_h,
            &self.vis_keys,
        )
    }

    /// Recompute the active drag's deltas from the last cursor position.
    /// Called from mouse-move, and again after edge auto-scroll shifts the
    /// view under a stationary cursor — deltas are cursor-relative, so they
    /// change as the scroll offset does.
    pub(crate) fn update_drag(&mut self) {
        let Some(pos) = self.mouse_pos else { return };
        let Some(mode) = self.drag.as_ref().map(|d| d.mode) else {
            return;
        };
        match mode {
            DragMode::Velocity | DragMode::LaneEvent | DragMode::LaneMarquee => {
                let li = self.drag.as_ref().map(|d| d.lane).unwrap_or(0);
                let Some(cell) = self.lane_bounds.get(li) else {
                    return;
                };
                let b = cell.get();
                let x = f32::from(pos.x) - f32::from(b.origin.x);
                let y = f32::from(pos.y) - f32::from(b.origin.y);
                let h = f32::from(b.size.height).max(1.0);
                let vrange = self.lanes.get(li).map(|c| c.mode.vrange()).unwrap_or(127.0);
                let val = ((1.0 - y / h) * vrange) as i32;
                if let Some(d) = self.drag.as_mut() {
                    d.dkey = val;
                    if d.mode == DragMode::LaneMarquee {
                        d.b_tick = ((x + self.scroll_x) / self.zoom) as i64;
                        d.b_key = val;
                    }
                }
            }
            DragMode::LaneResize => {
                // `orig_start` carries the grab-time height in centipx;
                // `a_tick` the pointer y it was grabbed at
                let grabbed = self.drag.as_ref().map(|d| (d.lane, d.orig_start, d.a_tick));
                if let Some((li, h0, y0)) = grabbed {
                    let dy = f32::from(pos.y) - y0 as f32;
                    if let Some(c) = self.lanes.get_mut(li) {
                        c.h = (h0 as f32 / 100.0 + dy).clamp(LANE_H_MIN, LANE_H_MAX);
                    }
                }
            }
            _ => {
                let (tick, key) = self.hit(pos);
                // erase stroke: every note swept joins the pending delete set
                let erase_id = (mode == DragMode::Erase)
                    .then(|| self.note_at(pos).or_else(|| self.edge_at(pos)))
                    .flatten()
                    .map(|n| n.on_id);
                // preview strike to send after the drag borrow is released —
                // (track, ch, key, vel, at_tick) when a pitch drag moves
                let mut strike = None;
                if let Some(d) = self.drag.as_mut() {
                    match d.mode {
                        DragMode::Move | DragMode::Duplicate => {
                            let (dt, dk) = clamp_move_delta(
                                tick - d.orig_start as i64,
                                d.orig_start,
                                key - d.orig_key as i32,
                                d.orig_key,
                            );
                            if dk != d.dkey {
                                strike = Some((
                                    d.track,
                                    d.aud_ch,
                                    (d.orig_key as i32 + dk).clamp(0, 127) as u8,
                                    d.aud_vel,
                                    (d.orig_start as i64 + dt).max(0) as u64,
                                ));
                            }
                            d.dtick = dt;
                            d.dkey = dk;
                        }
                        DragMode::Resize => {
                            d.dtick = tick - d.orig_end.unwrap_or(d.orig_start) as i64;
                        }
                        DragMode::Marquee => {
                            if self.tool == Tool::Draw && key != d.b_key {
                                strike = Some((
                                    d.track,
                                    d.aud_ch,
                                    key.clamp(0, 127) as u8,
                                    d.aud_vel,
                                    tick.max(0) as u64,
                                ));
                            }
                            d.b_tick = tick;
                            d.b_key = key;
                        }
                        _ => {}
                    }
                }
                if let Some((tr, ch, k, v, at)) = strike {
                    self.audition_strike(tr, ch, k, v, at);
                }
                if let Some(id) = erase_id {
                    self.erase_ids.insert(id);
                }
            }
        }
    }

    /// While a drag is active and the cursor rests near a canvas edge, pan
    /// the view a step per animation frame (classic DAW edge-scroll). Returns
    /// whether the view moved, so the caller knows to keep animating.
    pub(crate) fn drag_auto_pan(&mut self) -> bool {
        const EDGE: f32 = 28.0;
        const SPEED: f32 = 14.0;
        const SLOP: f32 = 96.0;
        let Some(mode) = self.drag.as_ref().map(|d| d.mode) else {
            return false;
        };
        let Some(pos) = self.mouse_pos else {
            return false;
        };
        let b = self.roll_bounds.get();
        let (x, y) = (f32::from(pos.x), f32::from(pos.y));
        let (bx, by) = (f32::from(b.origin.x), f32::from(b.origin.y));
        let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
        if w <= 0.0 || h <= 0.0 {
            return false;
        }
        let mut dx = 0.0f32;
        let mut dy = 0.0f32;
        if x < bx + EDGE && x > bx - SLOP {
            dx = -SPEED;
        }
        if x > bx + w - EDGE && x < bx + w + SLOP {
            dx = SPEED;
        }
        // vertical panning only makes sense for roll-space drags — lane
        // drags map the y axis to a value, not to pitch
        if !matches!(
            mode,
            DragMode::Velocity | DragMode::LaneEvent | DragMode::LaneMarquee | DragMode::LaneResize
        ) {
            if y < by + EDGE && y > by - SLOP {
                dy = -SPEED;
            }
            if y > by + h - EDGE && y < by + h + SLOP {
                dy = SPEED;
            }
        }
        if dx == 0.0 && dy == 0.0 {
            return false;
        }
        let before = (self.scroll_x, self.scroll_y);
        self.scroll_x = (self.scroll_x + dx).max(0.0);
        self.scroll_y = (self.scroll_y + dy).max(0.0);
        self.clamp_scroll();
        let moved = (self.scroll_x, self.scroll_y) != before;
        if moved {
            self.update_drag();
        }
        moved
    }

    pub(crate) fn note_at(&self, pos: Point<Pixels>) -> Option<Note> {
        let (tick, key) = self.hit(pos);
        let seq = self.is_seq();
        self.notes
            .iter()
            .rev()
            // ghosts of other sequences are display-only — clicks can't
            // select or drag them (a ghost's lane is ambiguous anyway)
            .filter(|n| !seq || n.track == self.sel_track)
            .find(|n| {
                n.key as i32 == key
                    && tick >= n.start_tick as i64
                    && tick
                        <= n.end_tick
                            .unwrap_or(n.start_tick + self.td().min_grid_ticks())
                            as i64
            })
            .cloned()
    }

    /// Note whose right edge is within ~6px of `pos` — a resize target.
    pub(crate) fn edge_at(&self, pos: Point<Pixels>) -> Option<Note> {
        let (tick, key) = self.hit(pos);
        let seq = self.is_seq();
        self.notes
            .iter()
            .rev()
            .filter(|n| !seq || n.track == self.sel_track)
            .find(|n| {
                n.key as i32 == key
                    && n.end_tick.is_some()
                    && tick >= n.start_tick as i64
                    && ((n.end_tick.unwrap() as i64) - tick) as f32 * self.zoom <= 6.0
                    && ((n.end_tick.unwrap() as i64) - tick) as f32 * self.zoom >= -2.0
            })
            .cloned()
    }

    /// Channel the selected track's previews route through — the track's
    /// insert/edit channel (explicit editor state), defaulting to its
    /// `FF 20` channel prefix. Per-event channels always rule on file
    /// playback; this only decides what NEW/preview events sound on.
    pub(crate) fn sel_track_ch(&self) -> u8 {
        self.doc(|d| {
            let prefix = d
                .tracks
                .get(self.sel_track)
                .map(|t| t.out_channel)
                .unwrap_or(0);
            self.edit_channel_of(self.sel_track, prefix)
        })
    }

    /// The effective insert/edit channel for `track`: the user's explicit
    /// per-track choice when set, else the `FF 20` channel prefix, else 0.
    pub(crate) fn edit_channel_of(&self, track: usize, prefix: u8) -> u8 {
        self.edit_ch.get(&track).copied().unwrap_or(prefix & 0x0F)
    }

    /// Piano-key under a window-space position on the key strip — row
    /// position maps through `vis_keys` so folded/drum views audition the
    /// visible row under the cursor.
    pub(crate) fn kbd_key(&self, pos: Point<Pixels>) -> Option<u8> {
        let b = self.kbd_bounds.get();
        let y = f32::from(pos.y) - f32::from(b.origin.y);
        let row = ((y + self.scroll_y) / self.note_h) as i32;
        (row >= 0)
            .then(|| self.vis_keys.get(row as usize).copied())
            .flatten()
    }

    #[allow(dead_code)]
    pub(crate) fn button(
        label: &'static str,
        cx: &Context<Self>,
        on: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        div()
            .id(label)
            .px_2()
            .py_1()
            .rounded_sm()
            .bg(rgb(theme::current().bg_chip))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(theme::current().bg_chip_hover)))
            .child(t(label))
            .on_click(cx.listener(move |this, _ev, _w, cx| on(this, cx)))
    }

    /// Toggle the high-contrast palette; the override persists in the
    /// app-wide prefs. When the OS flag was driving the theme, the first
    /// toggle just flips the effective state.
    pub(crate) fn toggle_hc(&mut self, cx: &mut Context<Self>) {
        let on = self.theme == theme::Theme::high_contrast();
        self.hc_pref = Some(!on);
        self.apply_theme(cx);
        self.save_global();
    }

    /// Small chip with a literal label (symbols/numbers need no i18n key).
    /// `on` receives the ClickEvent so chips can honour Shift=×10 etc.
    /// `a11y_name` is the screen-reader name (visible labels are often terse).
    pub(crate) fn chip(
        id: &'static str,
        label: impl Into<SharedString>,
        a11y_name: impl Into<SharedString>,
        cx: &mut Context<Self>,
        on: impl Fn(&mut Self, &ClickEvent, &mut Context<Self>) + 'static,
    ) -> ObservedElement<Stateful<Div>> {
        div()
            .id(id)
            .test_support()
            .role(Role::Button)
            .aria_label(a11y_name)
            .px_2()
            .py_1()
            .rounded_sm()
            .bg(rgb(theme::current().bg_chip))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(theme::current().bg_chip_hover)))
            .text_color(rgb(theme::current().accent))
            .text_size(px(11.0))
            .child(label.into())
            .on_click(cx.listener(move |this, ev, _w, cx| on(this, ev, cx)))
    }
}
