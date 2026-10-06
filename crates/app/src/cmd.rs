//! Command registry — the one canonical definition of every command:
//! stable id, i18n label key, default shortcut(s), enabled predicate,
//! and action. The menubar, command palette, shortcuts overlay, and the
//! key dispatcher all read this table, so a command's binding and label
//! can never disagree between surfaces.

use crate::{i18n::t, EditorView, Follow, PaletteMode, PendingAction, ScanMode, Tool};
use gpui_kit::{Context, Keystroke, SharedString, Window};
use std::collections::HashMap;

/// Command action — shared by menu clicks, palette runs, and keystrokes.
pub type Act = fn(&mut EditorView, &mut Window, &mut Context<EditorView>);

#[derive(Debug)]
pub struct Command {
    /// stable id — persisted in prefs.json key overrides
    pub id: &'static str,
    /// i18n key — resolved per render so labels follow the UI language
    pub label_key: &'static str,
    /// default bindings, "ctrl+shift+z" style; keys\[0\] shows in menus
    pub keys: &'static [&'static str],
    /// palette/menu enable predicate; None = always enabled
    pub enabled: Option<fn(&EditorView, &mut Context<EditorView>) -> bool>,
    pub act: Act,
}

pub fn find(id: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|c| c.id == id)
}

pub fn label(c: &Command) -> SharedString {
    t(c.label_key).into()
}

/// Canonical keystroke descriptor from a key event — "ctrl+shift+z",
/// "space", "f1". Modifiers serialize in ctrl, alt, shift order; the
/// literal space key becomes "space" so descriptors stay readable text.
pub fn describe(k: &Keystroke) -> String {
    let mut s = String::new();
    if k.modifiers.control {
        s.push_str("ctrl+");
    }
    if k.modifiers.alt {
        s.push_str("alt+");
    }
    if k.modifiers.shift {
        s.push_str("shift+");
    }
    s.push_str(normalize_key_name(k.key.as_str()));
    s
}

/// Normalize a key name for comparison/storage: trims, lowercases, and
/// maps the literal " " onto the word "space".
fn normalize_key_name(k: &str) -> &str {
    match k.trim() {
        "" | " " | "space" | "spacebar" => "space",
        "+" => "plus",
        other => other,
    }
}

/// Parse a descriptor like " Ctrl+Shift+Z " into canonical form;
/// `None` when the descriptor has no non-modifier key.
pub fn parse(desc: &str) -> Option<String> {
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut key = None;
    for part in desc.split('+') {
        match part.trim().to_lowercase().as_str() {
            "ctrl" | "control" | "cmd" | "command" | "meta" => ctrl = true,
            "alt" | "option" => alt = true,
            "shift" => shift = true,
            "" => {}
            k => key = Some(normalize_key_name(k).to_string()),
        }
    }
    let key = key?;
    let mut s = String::new();
    if ctrl {
        s.push_str("ctrl+");
    }
    if alt {
        s.push_str("alt+");
    }
    if shift {
        s.push_str("shift+");
    }
    s.push_str(&key);
    Some(s)
}

/// Display form of a canonical descriptor: "ctrl+shift+z" → "Ctrl+Shift+Z".
pub fn format_key(desc: &str) -> String {
    desc.split('+')
        .map(|p| match p {
            "ctrl" => "Ctrl".to_string(),
            "alt" => "Alt".to_string(),
            "shift" => "Shift".to_string(),
            "space" => "Space".to_string(),
            "escape" => "Esc".to_string(),
            "delete" => "Del".to_string(),
            "backspace" => "Backspace".to_string(),
            "left" => "←".to_string(),
            "right" => "→".to_string(),
            "up" => "↑".to_string(),
            "down" => "↓".to_string(),
            p if p.len() == 1 => p.to_uppercase(),
            p if p.starts_with('f') && p[1..].chars().all(|c| c.is_ascii_digit()) => {
                p.to_uppercase()
            }
            p => {
                let mut c = p.chars();
                match c.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// User keybinding overrides on top of [`COMMANDS`] defaults.
/// `overrides`: command id -> canonical keystroke descriptor (replaces ALL
/// of the command's default bindings when present).
#[derive(Default, Clone)]
pub struct KeyMap {
    pub overrides: HashMap<String, String>,
}

impl KeyMap {
    /// Effective bindings for a command (override or defaults). Overrides
    /// are re-canonicalized on read: a hand-edited prefs.json with
    /// "Ctrl+Z" still resolves to "ctrl+z", and garbage falls back to
    /// defaults rather than silently unbinding the command.
    pub fn bindings(&self, id: &str) -> Vec<String> {
        match self.overrides.get(id).and_then(|d| parse(d)) {
            Some(d) => vec![d],
            None => find(id)
                .map(|c| c.keys.iter().map(|s| s.to_string()).collect())
                .unwrap_or_default(),
        }
    }

    /// Menu/help display string for a command's first effective binding.
    pub fn shortcut_label(&self, id: &str) -> String {
        self.bindings(id)
            .first()
            .map(|d| format_key(d))
            .unwrap_or_default()
    }

    /// The command bound to a canonical descriptor.
    pub fn command_at(&self, desc: &str) -> Option<&'static Command> {
        COMMANDS
            .iter()
            .find(|c| self.bindings(c.id).iter().any(|b| b == desc))
    }

    /// Another command already bound to `desc` (excluding `except_id`).
    pub fn conflicts(&self, desc: &str, except_id: &str) -> Option<&'static Command> {
        COMMANDS
            .iter()
            .find(|c| c.id != except_id && self.bindings(c.id).iter().any(|b| b == desc))
    }

    /// Rebind `id` to `desc` (canonical). Err(conflicting command) leaves
    /// the map untouched — an occupied key is never silently stolen.
    pub fn assign(&mut self, id: &str, desc: &str) -> Result<(), &'static Command> {
        if let Some(other) = self.conflicts(desc, id) {
            return Err(other);
        }
        // assigning the default back clears the override
        let is_default = find(id)
            .map(|c| c.keys.len() == 1 && c.keys[0] == desc)
            .unwrap_or(false);
        if is_default {
            self.overrides.remove(id);
        } else {
            self.overrides.insert(id.to_string(), desc.to_string());
        }
        Ok(())
    }

    /// Remove a command's override (restore its defaults).
    pub fn reset(&mut self, id: &str) {
        self.overrides.remove(id);
    }

    pub fn is_default(&self, id: &str) -> bool {
        !self.overrides.contains_key(id)
    }
}

fn has_ev_sel(v: &EditorView, _cx: &mut Context<EditorView>) -> bool {
    !v.sel_events.is_empty()
}

fn has_sel(v: &EditorView, _cx: &mut Context<EditorView>) -> bool {
    // any focused-context selection (#152): notes, lane marquee, event rows
    !v.selection.is_empty() || !v.lane_sel.is_empty() || !v.sel_events.is_empty()
}

fn has_clip(v: &EditorView, cx: &mut Context<EditorView>) -> bool {
    // in-process copy, or a `midi-editor/smf-clip` payload left on the OS
    // clipboard by ANOTHER midi-editor process (#142) — Paste is enabled
    // whenever `paste()` could actually find a clip
    v.clipboard.is_some()
        || cx
            .read_from_clipboard()
            .is_some_and(|item| crate::clip_from_item(&item).is_some())
}

fn can_undo(v: &EditorView, _cx: &mut Context<EditorView>) -> bool {
    !crate::lock_shared(&v.shared).undo.is_empty()
}

fn nudge_st(v: &mut EditorView, dt: i64, dk: i32, cx: &mut Context<EditorView>) {
    let s = v.snap_ticks();
    let st = if s > 0 { s } else { v.ppq() as i64 / 8 };
    v.nudge(dt * st, dk, cx);
}

macro_rules! cmd {
    ($id:literal, $key:literal, $keys:expr, $en:expr, $act:expr) => {
        Command {
            id: $id,
            label_key: $key,
            keys: $keys,
            enabled: $en,
            act: $act,
        }
    };
}

/// Every fixed command in the app. Parameterized menu rows (destinations,
/// recent files, channel/snap/quantize pickers) are not commands — they
/// carry data, not ids.
pub static COMMANDS: &[Command] = &[
    // file
    cmd!("file.new", "menu.new", &["ctrl+n"], None, |v, w, cx| {
        v.confirm_discard_or_save(PendingAction::NewFile, w, cx)
    }),
    cmd!("file.open", "menu.open", &["ctrl+o"], None, |v, w, cx| {
        v.confirm_discard_or_save(PendingAction::OpenDialog, w, cx)
    }),
    cmd!("file.save", "menu.save", &["ctrl+s"], None, |v, _w, cx| v
        .save(cx)),
    cmd!(
        "file.save_as",
        "menu.save_as",
        &["ctrl+shift+s"],
        None,
        |v, _w, cx| { v.save_as(cx) }
    ),
    // edit
    cmd!(
        "edit.undo",
        "menu.undo",
        &["ctrl+z"],
        Some(can_undo),
        |v, _w, cx| v.undo(cx)
    ),
    cmd!(
        "edit.redo",
        "menu.redo",
        &["ctrl+y", "ctrl+shift+z"],
        None,
        |v, _w, cx| v.redo(cx)
    ),
    cmd!(
        "edit.select_all",
        "menu.select_all",
        &["ctrl+a"],
        None,
        |v, w, cx| v.select_all(w, cx)
    ),
    cmd!(
        "edit.cut",
        "edit.cut",
        &["ctrl+x"],
        Some(has_sel),
        |v, w, cx| v.copy_selected(true, w, cx)
    ),
    cmd!(
        "edit.copy",
        "edit.copy",
        &["ctrl+c"],
        Some(has_sel),
        |v, w, cx| v.copy_selected(false, w, cx)
    ),
    cmd!(
        "edit.paste",
        "edit.paste",
        &["ctrl+v"],
        Some(has_clip),
        |v, _w, cx| v.paste(cx)
    ),
    cmd!(
        "edit.duplicate",
        "edit.duplicate",
        &["ctrl+d"],
        Some(has_sel),
        |v, _w, cx| v.duplicate_selected(cx)
    ),
    cmd!(
        "edit.delete",
        "menu.delete",
        &["delete", "backspace"],
        Some(has_sel),
        |v, w, cx| v.delete_selected(w, cx)
    ),
    cmd!(
        "edit.marker_ins",
        "edit.marker_ins",
        &["m"],
        None,
        |v, w, cx| {
            let tick = v.doc(|d| d.tempo_map.us_to_tick(v.play_us));
            v.open_meta_edit(0, tick, 0x06, 0, w, cx);
        }
    ),
    cmd!(
        "edit.meta_edit",
        "edit.meta_edit",
        &["e"],
        Some(|v: &EditorView, _cx: &mut Context<EditorView>| v.meta_sel.is_some()),
        |v, w, cx| {
            if let Some((tr, id)) = v.meta_sel {
                let m = v.doc(|d| {
                    d.tracks.get(tr).and_then(|t| {
                        t.events
                            .iter()
                            .find(|e| e.id == id)
                            .and_then(|e| match &e.kind {
                                smf_core::EventKind::Meta { meta_type, .. } => {
                                    Some((e.tick, *meta_type))
                                }
                                _ => None,
                            })
                    })
                });
                if let Some((tick, mt)) = m {
                    v.open_meta_edit(tr, tick, mt, id, w, cx);
                }
            }
        }
    ),
    cmd!(
        "edit.transpose_up",
        "edit.transpose_up",
        &[],
        None,
        |v, _w, _cx| {
            v.apply_region_op("transpose +1", |d, t, f, to, ch| {
                d.transpose_ops(t, f, to, 1, ch)
            });
        }
    ),
    cmd!(
        "edit.transpose_dn",
        "edit.transpose_dn",
        &[],
        None,
        |v, _w, _cx| {
            v.apply_region_op("transpose -1", |d, t, f, to, ch| {
                d.transpose_ops(t, f, to, -1, ch)
            });
        }
    ),
    cmd!("edit.humanize", "edit.humanize", &[], None, |v, _w, _cx| {
        v.apply_region_op("humanize", |d, t, f, to, _ch| {
            d.humanize_ops(t, f, to, 12, 8, d.revision())
        });
    }),
    cmd!("edit.legato", "edit.legato", &[], None, |v, _w, _cx| {
        v.apply_region_op("legato", |d, t, f, to, _ch| d.legato_ops(t, f, to, 0));
    }),
    cmd!("edit.split", "edit.split", &[], None, |v, _w, cx| {
        v.split_at_playhead(cx);
    }),
    cmd!("edit.join", "edit.join", &[], None, |v, _w, _cx| {
        v.apply_region_op("join", |d, t, f, to, _ch| d.join_ops(t, f, to));
    }),
    cmd!(
        "edit.fix_overlaps",
        "edit.fix_overlaps",
        &[],
        None,
        |v, _w, _cx| {
            v.apply_region_op("fix overlaps", |d, t, f, to, _ch| {
                d.fix_overlaps_ops(t, f, to)
            });
        }
    ),
    cmd!("edit.vel_up", "edit.vel_up", &[], None, |v, _w, _cx| {
        v.apply_region_op("vel ×1.25", |d, t, f, to, ch| {
            d.scale_velocity_ops(t, f, to, 1.25, ch)
        });
    }),
    cmd!("edit.vel_dn", "edit.vel_dn", &[], None, |v, _w, _cx| {
        v.apply_region_op("vel ×0.8", |d, t, f, to, ch| {
            d.scale_velocity_ops(t, f, to, 0.8, ch)
        });
    }),
    // nudge (grid step / 1 tick / semitone / octave)
    cmd!("nav.left", "nav.left", &["left"], None, |v, _w, cx| {
        nudge_st(v, -1, 0, cx)
    }),
    cmd!("nav.right", "nav.right", &["right"], None, |v, _w, cx| {
        nudge_st(v, 1, 0, cx)
    }),
    cmd!("nav.up", "nav.up", &["up"], None, |v, _w, cx| nudge_st(
        v, 0, 1, cx
    )),
    cmd!("nav.down", "nav.down", &["down"], None, |v, _w, cx| {
        nudge_st(v, 0, -1, cx)
    }),
    cmd!(
        "nav.left_tick",
        "nav.left_tick",
        &["shift+left"],
        None,
        |v, _w, cx| v.nudge(-1, 0, cx)
    ),
    cmd!(
        "nav.right_tick",
        "nav.right_tick",
        &["shift+right"],
        None,
        |v, _w, cx| v.nudge(1, 0, cx)
    ),
    cmd!(
        "nav.up_oct",
        "nav.up_oct",
        &["shift+up"],
        None,
        |v, _w, cx| v.nudge(0, 12, cx)
    ),
    cmd!(
        "nav.down_oct",
        "nav.down_oct",
        &["shift+down"],
        None,
        |v, _w, cx| v.nudge(0, -12, cx)
    ),
    // tools
    cmd!("tool.select", "tool.select", &["1"], None, |v, _w, cx| v
        .set_tool(Tool::Select, cx)),
    cmd!("tool.draw", "tool.draw", &["2"], None, |v, _w, cx| v
        .set_tool(Tool::Draw, cx)),
    cmd!("tool.erase", "tool.erase", &["3"], None, |v, _w, cx| v
        .set_tool(Tool::Erase, cx)),
    // view
    cmd!("view.events", "view.events", &[], None, |v, _w, _cx| {
        v.show_events = !v.show_events;
        v.persist();
    }),
    cmd!("view.hc", "view.hc", &[], None, |v, _w, cx| v.toggle_hc(cx)),
    // panel-switch shortcuts (#183): keyboard-only users can reach every
    // major pane without the mouse
    cmd!("nav.focus_tracks", "nav.focus_tracks", &["ctrl+1"], None, |v, w, cx| {
        w.focus(&v.tracks_fh, cx);
    }),
    cmd!("nav.focus_roll", "nav.focus_roll", &["ctrl+2"], None, |v, w, cx| {
        w.focus(&v.roll_fh, cx);
    }),
    cmd!("nav.focus_lane", "nav.focus_lane", &["ctrl+3"], None, |v, w, cx| {
        w.focus(&v.lane_fh, cx);
    }),
    cmd!("nav.focus_events", "nav.focus_events", &["ctrl+4"], None, |v, w, cx| {
        w.focus(&v.events_fh, cx);
    }),
    cmd!("view.fold", "view.fold", &[], None, |v, _w, cx| {
        let on = !v.fold;
        v.set_fold(on, cx);
    }),
    cmd!(
        "view.scale_fold",
        "view.scale_fold",
        &[],
        None,
        |v, _w, cx| {
            let on = !v.scale_fold;
            v.set_scale_fold(on, cx);
        }
    ),
    cmd!("view.drum", "view.drum", &[], None, |v, _w, cx| {
        let on = !v.drum;
        v.set_drum(on, cx);
    }),
    cmd!(
        "view.zoom_in",
        "view.zoom_in",
        &["ctrl+=", "ctrl+plus"],
        None,
        |v, _w, cx| v.zoom_by(1.3, cx)
    ),
    cmd!(
        "view.zoom_out",
        "view.zoom_out",
        &["ctrl+-"],
        None,
        |v, _w, cx| v.zoom_by(1.0 / 1.3, cx)
    ),
    cmd!(
        "view.follow_off",
        "view.follow_off",
        &[],
        None,
        |v, _w, _cx| {
            v.follow = Follow::Off;
            v.follow_hold = None;
            v.persist();
        }
    ),
    cmd!(
        "view.follow_page",
        "view.follow_page",
        &[],
        None,
        |v, _w, _cx| {
            v.follow = Follow::Page;
            v.follow_hold = None;
            v.persist();
        }
    ),
    cmd!(
        "view.follow_smooth",
        "view.follow_smooth",
        &[],
        None,
        |v, _w, _cx| {
            v.follow = Follow::Smooth;
            v.follow_hold = None;
            v.persist();
        }
    ),
    cmd!(
        "view.zoom_sel",
        "view.zoom_sel",
        &["z"],
        Some(has_sel),
        |v, _w, cx| v.zoom_to_selection(cx)
    ),
    cmd!(
        "view.zoom_song",
        "view.zoom_song",
        &["shift+z"],
        None,
        |v, _w, cx| v.zoom_to_song(cx)
    ),
    cmd!(
        "view.go_playhead",
        "view.go_playhead",
        &["g"],
        None,
        |v, _w, cx| v.go_playhead(cx)
    ),
    cmd!(
        "view.marker_prev",
        "view.marker_prev",
        &[",", "["],
        None,
        |v, _w, cx| v.marker_step(-1, cx)
    ),
    cmd!(
        "view.marker_next",
        "view.marker_next",
        &[".", "]"],
        None,
        |v, _w, cx| v.marker_step(1, cx)
    ),
    cmd!(
        "view.event_prev",
        "view.event_prev",
        &["shift+,"],
        None,
        |v, _w, cx| v.event_step(-1, cx)
    ),
    cmd!(
        "view.event_next",
        "view.event_next",
        &["shift+."],
        None,
        |v, _w, cx| v.event_step(1, cx)
    ),
    cmd!(
        "events.nudge_dn",
        "events.nudge_dn",
        &["-", "_"],
        Some(has_ev_sel),
        |v, _w, cx| v.nudge_sel_events(-1, cx)
    ),
    cmd!(
        "events.nudge_up",
        "events.nudge_up",
        &["=", "plus"],
        Some(has_ev_sel),
        |v, _w, cx| v.nudge_sel_events(1, cx)
    ),
    cmd!(
        "view.zoom_reset",
        "view.zoom_reset",
        &["ctrl+0"],
        None,
        |v, _w, cx| v.zoom_set(0.08, cx)
    ),
    // track
    cmd!("track.rename", "track.rename", &[], None, |v, w, cx| v
        .focus_rename(w, cx)),
    cmd!("track.add", "track.add", &["ctrl+t"], None, |v, _w, cx| {
        // append at the end — no sidecar track maps need shifting
        let (ops, new_index) = {
            let mut sh = crate::lock_shared(&v.shared);
            let index = sh.doc.tracks.len();
            (sh.doc.add_track_ops(None, None), index)
        };
        if !ops.is_empty() {
            v.apply_tx("add track", ops);
            v.sel_track = new_index;
        }
        cx.notify();
    }),
    cmd!(
        "track.duplicate",
        "track.duplicate",
        &["ctrl+shift+d"],
        None,
        |v, _w, cx| {
            // clone events + channel prefix + routing right after the source
            let ops = {
                let mut sh = crate::lock_shared(&v.shared);
                sh.doc.duplicate_track_ops(v.sel_track)
            };
            if !ops.is_empty() {
                let (src, dst) = (v.sel_track, v.sel_track + 1);
                v.shift_track_maps_for_insert(src);
                v.apply_tx("duplicate track", ops);
                // the copy inherits the source's routing and mix state
                {
                    let mut sh = crate::lock_shared(&v.shared);
                    if let Some(d) = sh.track_dest.get(&src).copied() {
                        sh.track_dest.insert(dst, d);
                    }
                    if sh.muted.contains(&src) {
                        sh.muted.insert(dst);
                    }
                    if sh.soloed.contains(&src) {
                        sh.soloed.insert(dst);
                    }
                }
                v.sel_track = dst;
            }
            cx.notify();
        }
    ),
    cmd!(
        "track.delete",
        "track.delete",
        &["ctrl+backspace"],
        None,
        |v, w, cx| { v.prompt_delete_track(w, cx) }
    ),
    cmd!("track.mute", "track.mute", &[], None, |v, _w, _cx| {
        let t = v.sel_track;
        {
            let mut sh = crate::lock_shared(&v.shared);
            let before = sh.muted.contains(&t);
            if !sh.muted.remove(&t) {
                sh.muted.insert(t);
            }
            // mute/solo are undoable session edits (#204)
            sh.apply_session(mcp_server::SessionOp::SetMute {
                track: t,
                before,
                after: !before,
            });
        }
        v.persist();
        // live mix control: the running pass updates in place (#140)
        v.refresh_live_schedule();
    }),
    cmd!("track.solo", "track.solo", &[], None, |v, _w, _cx| {
        let t = v.sel_track;
        {
            let mut sh = crate::lock_shared(&v.shared);
            let before = sh.soloed.contains(&t);
            if !sh.soloed.remove(&t) {
                sh.soloed.insert(t);
            }
            sh.apply_session(mcp_server::SessionOp::SetSolo {
                track: t,
                before,
                after: !before,
            });
        }
        v.persist();
        v.refresh_live_schedule();
    }),
    cmd!(
        "track.plugin_gui",
        "track.plugin_gui",
        &[],
        None,
        |v, _w, _cx| v.open_plugin_gui()
    ),
    // output
    cmd!("output.rescan", "output.rescan", &[], None, |v, _w, cx| {
        v.rescan_plugins(ScanMode::Changed);
        cx.notify();
    }),
    cmd!(
        "output.host_status",
        "output.host_status",
        &[],
        None,
        |v, _w, cx| {
            v.show_output_status = true;
            cx.notify();
        }
    ),
    cmd!("output.retry", "output.retry", &[], None, |v, _w, _cx| {
        let d = crate::lock_shared(&v.shared).dest_of(v.sel_track);
        v.ensure_plugin(d, true);
    }),
    // transport
    cmd!(
        "transport.play_stop",
        "transport.play_stop",
        &["space"],
        None,
        |v, _w, cx| v.toggle_play(cx)
    ),
    // pause/continue: stop in place (play point kept) / resume — the
    // in-place counterpart of Stop (#156)
    cmd!(
        "transport.pause",
        "transport.pause",
        &[],
        None,
        |v, _w, cx| v.toggle_pause(cx)
    ),
    // return to where the current transport pass began
    cmd!(
        "transport.return_start",
        "transport.return_start",
        &[],
        None,
        |v, _w, cx| v.return_to_start(cx)
    ),
    // go to song start (tick 0)
    cmd!(
        "transport.go_start",
        "transport.go_start",
        &["home"],
        None,
        |v, _w, cx| v.go_to_start(cx)
    ),
    // return-on-stop preference (#156)
    cmd!(
        "transport.return_on_stop",
        "transport.return_on_stop",
        &[],
        None,
        |v, _w, _cx| {
            v.return_to_start_on_stop = !v.return_to_start_on_stop;
            v.save_global();
        }
    ),
    cmd!(
        "transport.record",
        "transport.record",
        &[],
        None,
        |v, _w, cx| v.transport_record(cx)
    ),
    cmd!("rec.arm", "rec.arm", &[], None, |v, _w, _cx| {
        v.toggle_arm()
    }),
    cmd!(
        "transport.loop",
        "transport.loop",
        &[],
        None,
        |v, _w, _cx| {
            {
                let mut sh = crate::lock_shared(&v.shared);
                sh.loop_enabled = !sh.loop_enabled;
            }
            v.persist();
            v.refresh_live_schedule();
        }
    ),
    // explicit loop locators (#130) — Set Start/End to Playhead, Set to
    // Selection, Clear. A bound that collides with the other clears it
    // rather than silently swapping or producing an inverted range.
    cmd!(
        "loop.set_start",
        "loop.set_start",
        &[],
        None,
        |v, _w, _cx| {
            let t = v.playhead_tick();
            {
                let mut sh = crate::lock_shared(&v.shared);
                sh.loop_start = Some(t);
                if sh.loop_end.is_some_and(|e| e <= t) {
                    sh.loop_end = None;
                }
            }
            v.persist();
            v.refresh_live_schedule();
        }
    ),
    cmd!("loop.set_end", "loop.set_end", &[], None, |v, _w, _cx| {
        let t = v.playhead_tick();
        {
            let mut sh = crate::lock_shared(&v.shared);
            sh.loop_end = Some(t);
            if sh.loop_start.is_some_and(|s| s >= t) {
                sh.loop_start = None;
            }
        }
        v.persist();
        v.refresh_live_schedule();
    }),
    cmd!(
        "loop.set_selection",
        "loop.set_selection",
        &[],
        None,
        |v, _w, _cx| {
            let mut lo = u64::MAX;
            let mut hi = 0u64;
            for n in v.notes.iter().filter(|n| v.selection.contains(&n.on_id)) {
                lo = lo.min(n.start_tick);
                hi = hi.max(n.end_tick.unwrap_or(n.start_tick + 1));
            }
            if lo > hi {
                v.status = t("status.nosel").into();
                return;
            }
            {
                let mut sh = crate::lock_shared(&v.shared);
                sh.loop_start = Some(lo);
                sh.loop_end = Some(hi);
            }
            v.persist();
            v.refresh_live_schedule();
        }
    ),
    cmd!("loop.clear", "loop.clear", &[], None, |v, _w, _cx| {
        {
            let mut sh = crate::lock_shared(&v.shared);
            sh.loop_start = None;
            sh.loop_end = None;
        }
        v.persist();
        v.refresh_live_schedule();
    }),
    // tempo / signature edits at the playhead (#133) — the dialog inserts a
    // new event prefilled with the value in force, or edits the existing
    // event at that tick; delete removes it
    cmd!("tempo.edit", "tempo.edit", &[], None, |v, w, cx| v
        .open_tempo_sig_edit(0x51, w, cx)),
    cmd!("tempo.delete", "tempo.delete", &[], None, |v, _w, cx| {
        v.delete_tempo_sig(0x51, cx)
    }),
    cmd!("sig.edit", "sig.edit", &[], None, |v, w, cx| v
        .open_tempo_sig_edit(0x58, w, cx)),
    cmd!("sig.delete", "sig.delete", &[], None, |v, _w, cx| {
        v.delete_tempo_sig(0x58, cx)
    }),
    cmd!("transport.met", "transport.met", &[], None, |v, _w, _cx| {
        {
            let mut sh = crate::lock_shared(&v.shared);
            sh.metronome = !sh.metronome;
        }
        v.persist();
        v.refresh_live_schedule();
    }),
    cmd!(
        "transport.chase_sysex",
        "transport.chase_sysex",
        &[],
        None,
        |v, _w, _cx| {
            {
                let mut sh = crate::lock_shared(&v.shared);
                sh.chase_sysex = !sh.chase_sysex;
            }
            v.persist();
            v.refresh_live_schedule();
        }
    ),
    cmd!(
        "transport.count_in",
        "transport.count_in",
        &[],
        None,
        |v, _w, _cx| {
            const BARS: [u8; 4] = [0, 1, 2, 4];
            let i = BARS.iter().position(|&b| b == v.count_in_bars).unwrap_or(0);
            v.count_in_bars = BARS[(i + 1) % BARS.len()];
            v.save_global();
        }
    ),
    // explicit emergency silence — full CC123/121/120 sweep (#161)
    cmd!(
        "transport.panic",
        "transport.panic",
        &[],
        None,
        |v, _w, cx| {
            v.midi_panic();
            cx.notify();
        }
    ),
    // reset-on-stop preference: off = stop releases notes only
    cmd!(
        "transport.reset_on_stop",
        "transport.reset_on_stop",
        &[],
        None,
        |v, _w, _cx| {
            v.reset_on_stop = !v.reset_on_stop;
            v.save_global();
        }
    ),
    cmd!(
        "transport.audition",
        "transport.audition",
        &[],
        None,
        |v, _w, _cx| {
            v.aud_enabled = !v.aud_enabled;
            // disabling mid-ring must silence immediately
            if !v.aud_enabled {
                v.audition_off();
            }
            v.save_global();
        }
    ),
    // app / help
    cmd!(
        "help.shortcuts",
        "help.shortcuts",
        &["f1"],
        None,
        |v, _w, cx| {
            v.help_open = !v.help_open;
            cx.notify();
        }
    ),
    cmd!("help.about", "help.about", &[], None, |v, _w, _cx| {
        v.status = concat!("midi-editor ", env!("BUILD_IDENTITY"), " — pure-SMF editor").into();
    }),
    cmd!("help.mcp", "help.mcp", &[], None, |v, _w, _cx| {
        v.status = "MCP: http://127.0.0.1:7878/mcp (mcp-bridge for stdio clients)".into();
    }),
    cmd!("help.logs", "help.open_logs", &[], None, |v, _w, cx| v
        .open_logs(cx)),
    cmd!("help.diag", "help.export_diag", &[], None, |v, _w, cx| {
        v.export_diagnostics(cx)
    }),
    cmd!(
        "app.palette",
        "ui.palette",
        &["ctrl+shift+p"],
        None,
        |v, w, cx| { v.open_palette(PaletteMode::Commands, w, cx) }
    ),
    cmd!("app.keys", "ui.keys", &[], None, |v, w, cx| {
        v.open_palette(PaletteMode::Keys, w, cx)
    }),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_canonicalizes_modifiers_and_key() {
        let mut k = Keystroke::parse("ctrl-shift-Z").unwrap();
        assert_eq!(describe(&k), "ctrl+shift+z");
        k = Keystroke::parse("space").unwrap();
        assert_eq!(describe(&k), "space");
    }

    #[test]
    fn parse_accepts_messy_input() {
        assert_eq!(parse(" Ctrl + Shift + Z ").as_deref(), Some("ctrl+shift+z"));
        assert_eq!(parse("Space").as_deref(), Some("space"));
        assert_eq!(parse("ctrl+"), None);
    }

    #[test]
    fn format_key_renders_display_strings() {
        assert_eq!(format_key("ctrl+shift+z"), "Ctrl+Shift+Z");
        assert_eq!(format_key("space"), "Space");
        assert_eq!(format_key("shift+left"), "Shift+←");
        assert_eq!(format_key("f1"), "F1");
    }

    #[test]
    fn effective_bindings_override_defaults() {
        let mut km = KeyMap::default();
        assert_eq!(km.bindings("file.save"), vec!["ctrl+s"]);
        km.assign("file.save", "ctrl+alt+s").unwrap();
        assert_eq!(km.bindings("file.save"), vec!["ctrl+alt+s"]);
        assert_eq!(km.shortcut_label("file.save"), "Ctrl+Alt+S");
        assert!(!km.is_default("file.save"));
        km.reset("file.save");
        assert_eq!(km.bindings("file.save"), vec!["ctrl+s"]);
        assert!(km.is_default("file.save"));
    }

    #[test]
    fn assign_refuses_conflicts_and_keeps_state() {
        let mut km = KeyMap::default();
        let err = km.assign("file.new", "ctrl+o").unwrap_err();
        assert_eq!(err.id, "file.open");
        assert!(km.is_default("file.new"));
        // same command re-binding to its own key is not a conflict
        km.assign("file.open", "ctrl+o").unwrap();
        assert!(km.is_default("file.open"));
    }

    #[test]
    fn command_at_uses_effective_bindings() {
        let mut km = KeyMap::default();
        assert_eq!(km.command_at("ctrl+z").unwrap().id, "edit.undo");
        km.assign("edit.undo", "ctrl+u").unwrap();
        assert_eq!(km.command_at("ctrl+u").unwrap().id, "edit.undo");
        // overridden default is gone
        assert!(km.command_at("ctrl+z").is_none());
    }

    #[test]
    fn command_ids_and_defaults_are_unique() {
        let mut ids = std::collections::HashSet::new();
        let mut defaults = std::collections::HashSet::new();
        for c in COMMANDS {
            assert!(ids.insert(c.id), "duplicate id {}", c.id);
            for k in c.keys {
                assert_eq!(parse(k).as_deref(), Some(*k), "non-canonical {k}");
                assert!(defaults.insert(*k), "{k} bound twice in defaults");
            }
        }
    }
}
