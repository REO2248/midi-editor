//! Accessibility support: labels and synthetic nodes for the AccessKit tree
//! (UI Automation on Windows).
//!
//! Interactive elements get roles/names/states from `.role()`/`.aria_*()` in
//! `render.rs`. Painted canvas content — piano-roll notes, the playhead, the
//! selection summary, controller-lane points — has no element tree of its
//! own, so it is described here as synthetic children of the canvas's
//! wrapper element. All strings go through i18n so announcements follow the
//! app language.

use crate::geometry::{drag_window, tick_window};
use crate::i18n::{t, tf};
use crate::{DragMode, LaneMode, NOTE_H};
use document::{EventId, Note};
use gpui_kit::accesskit::{Node, Orientation, Rect, Role};
use gpui_kit::{px, A11ySubtreeBuilder, Bounds, Pixels};
use std::collections::BTreeSet;
use std::sync::Arc;

/// Cap on synthetic nodes per subtree: the UIA tree update is sent every
/// frame, so unbounded children would hurt the very users it helps.
const MAX_NODES: usize = 600;

/// Pitch name with octave: 60 -> "C4", 69 -> "A4".
pub(crate) fn note_name(key: i64) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    let k = key.clamp(0, 127);
    format!("{}{}", NAMES[(k % 12) as usize], k / 12 - 1)
}

/// `bar.beat.tick` position text, matching the transport LCD.
pub(crate) fn pos_label(tick: u64, ppq: u64) -> String {
    let ppq = ppq.max(1);
    let bar = tick / (ppq * 4) + 1;
    let beat = (tick % (ppq * 4)) / ppq + 1;
    format!("{bar}.{beat}.{:>3}", tick % ppq)
}

/// Accessible name for a piano-roll note, e.g.
/// "C4 at 3.1.000, 2.0 beats, velocity 96, Piano".
pub(crate) fn note_label(n: &Note, ppq: u64, track_name: &str) -> String {
    let key = note_name(n.key as i64);
    let pos = pos_label(n.start_tick, ppq);
    let dur_ticks = (n.end_tick.unwrap_or(n.start_tick) as i64 - n.start_tick as i64).max(0);
    let dur = format!("{:.1}", dur_ticks as f64 / ppq.max(1) as f64);
    let vel = n.vel.to_string();
    let args = [
        ("key", key.as_str()),
        ("pos", pos.as_str()),
        ("dur", dur.as_str()),
        ("vel", vel.as_str()),
        ("track", track_name),
    ];
    if n.end_tick.is_some() {
        tf("a11y.note", &args)
    } else {
        tf("a11y.note_open", &args)
    }
}

/// Accessible name for a controller-lane point (CC / pitch bend).
pub(crate) fn lane_point_label(mode: LaneMode, tick: u64, val: i32, ppq: u64) -> String {
    let m = mode.label();
    let v = val.to_string();
    let pos = pos_label(tick, ppq);
    tf(
        "a11y.lane_pt",
        &[
            ("mode", m.as_str()),
            ("val", v.as_str()),
            ("pos", pos.as_str()),
        ],
    )
}

/// Window-space rect -> physical-pixel AccessKit rect (same conversion
/// element nodes get: logical px * scale factor).
fn rect(x: f32, y: f32, w: f32, h: f32, scale: f32) -> Rect {
    let s = scale.max(0.0) as f64;
    Rect {
        x0: x as f64 * s,
        y0: y as f64 * s,
        x1: (x as f64 + w as f64) * s,
        y1: (y as f64 + h as f64) * s,
    }
}

/// Snapshot of everything the roll's synthetic subtree needs; captured in
/// `render` because the builder callback cannot reach the view.
pub(crate) struct RollA11y {
    /// bounds of the roll canvas (from its layout callback)
    pub bounds: Bounds<Pixels>,
    pub scale: f32,
    pub scroll_x: f32,
    pub scroll_y: f32,
    pub zoom: f32,
    pub ppq: u64,
    pub notes: Arc<Vec<Note>>,
    pub selection: BTreeSet<EventId>,
    pub track_names: Vec<String>,
    /// (mode, on_id, dtick, dkey) of an in-flight drag — positions move live
    pub drag: Option<(DragMode, EventId, i64, i32)>,
    /// marquee corners (tick, key) while rubber-banding
    pub marquee: Option<(i64, i32, i64, i32)>,
    pub playhead_tick: u64,
}

impl RollA11y {
    /// Playhead, selection summary, marquee range, then one
    /// `ListBoxOption` per visible note so a screen reader can enumerate
    /// notes with pitch/position/duration/velocity/track.
    pub(crate) fn build(self, b: &mut A11ySubtreeBuilder) {
        let bounds = self.bounds;
        if bounds.size.width <= px(0.0) || bounds.size.height <= px(0.0) {
            return;
        }
        let (ox, oy) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
        let (vw, vh) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let (scroll_x, scroll_y, zoom) = (self.scroll_x, self.scroll_y, self.zoom);
        let scale = self.scale;
        let track_name =
            |i: usize| -> &str { self.track_names.get(i).map(|s| s.as_str()).unwrap_or("") };

        // playhead — a thin labelled marker so its position is discoverable
        {
            let x = ox + self.playhead_tick as f32 * zoom - scroll_x;
            let mut node = Node::new(Role::Label);
            node.set_label(tf(
                "a11y.playhead",
                &[("pos", pos_label(self.playhead_tick, self.ppq).as_str())],
            ));
            node.set_bounds(rect(x, oy, 1.5, vh, scale));
            b.push_child(b.synthetic_node_id("playhead"), node);
        }

        // selection summary — present whenever the selection is non-empty
        if !self.selection.is_empty() {
            let n = self.selection.len().to_string();
            let mut node = Node::new(Role::Label);
            node.set_label(tf("a11y.sel_count", &[("n", n.as_str())]));
            node.set_bounds(rect(ox, oy, vw, vh, scale));
            b.push_child(b.synthetic_node_id("sel_count"), node);
        }

        // marquee rubber band — the provisional selected range
        if let Some((a_t, a_k, b_t, b_k)) = self.marquee {
            let (t0, t1) = (a_t.min(b_t).max(0) as u64, a_t.max(b_t).max(0) as u64);
            let (k0, k1) = (a_k.min(b_k), a_k.max(b_k));
            let x0 = ox + t0 as f32 * zoom - scroll_x;
            let x1 = ox + t1 as f32 * zoom - scroll_x;
            let y0 = oy + (127.0 - k1 as f32) * NOTE_H - scroll_y;
            let y1 = oy + (127.0 - k0 as f32) * NOTE_H - scroll_y;
            let mut node = Node::new(Role::Region);
            node.set_label(t("a11y.marquee"));
            node.set_bounds(rect(x0, y0, (x1 - x0).max(1.0), (y1 - y0).max(1.0), scale));
            b.push_child(b.synthetic_node_id("marquee"), node);
        }

        // notes — same visibility window the paint pass uses
        let w = vw;
        let (vt0, vt1) = tick_window(scroll_x, zoom, w);
        let move_dtick = match self.drag {
            Some((DragMode::Move | DragMode::Duplicate, _, dtick, _)) => dtick,
            _ => 0,
        };
        let (entry, exit) = drag_window(vt0, vt1, move_dtick);
        let first = self
            .notes
            .partition_point(|n| (n.start_tick as i64) < entry);
        let mut pushed = 0usize;
        for (i, n) in self.notes.iter().enumerate().skip(first) {
            if pushed >= MAX_NODES {
                break;
            }
            if (n.start_tick as i64) > exit {
                break;
            }
            let mut st = n.start_tick as i64;
            let mut en = n.end_tick.unwrap_or(n.start_tick + self.ppq / 4) as i64;
            let mut key = n.key as i32;
            let mut ghost_orig = false;
            if let Some((mode, d_on, dtick, dkey)) = self.drag {
                match mode {
                    DragMode::Move | DragMode::Duplicate
                        if d_on == n.on_id || self.selection.contains(&n.on_id) =>
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
            let x = ox + st as f32 * zoom - scroll_x;
            let wpx = ((en - st).max(1) as f32 * zoom).max(3.0);
            if x + wpx < ox {
                continue;
            }
            let y = oy + (127.0 - key as f32) * NOTE_H - scroll_y;
            if y < oy - NOTE_H || y > oy + vh {
                continue;
            }
            let mut node = Node::new(Role::ListBoxOption);
            node.set_label(note_label(n, self.ppq, track_name(n.track)));
            node.set_selected(self.selection.contains(&n.on_id));
            node.set_bounds(rect(x, y + 1.0, wpx, NOTE_H - 2.0, scale));
            node.set_position_in_set(i + 1);
            node.set_size_of_set(self.notes.len());
            b.push_child(b.synthetic_node_id(n.on_id), node);
            pushed += 1;
            if ghost_orig {
                // the duplicate's still-visible original also gets a node
                let grect = rect(
                    ox + n.start_tick as f32 * zoom - scroll_x,
                    oy + (127.0 - n.key as f32) * NOTE_H - scroll_y + 1.0,
                    ((n.end_tick.unwrap_or(n.start_tick) - n.start_tick).max(1) as f32 * zoom)
                        .max(3.0),
                    NOTE_H - 2.0,
                    scale,
                );
                let mut gnode = Node::new(Role::ListBoxOption);
                gnode.set_label(note_label(n, self.ppq, track_name(n.track)));
                gnode.set_selected(true);
                gnode.set_bounds(grect);
                b.push_child(b.synthetic_node_id(("ghost", n.on_id)), gnode);
            }
        }
    }
}

/// Controller-lane subtree: one `Slider` per automation point (or velocity
/// bar), so values are exposed numerically, not just as pixels.
pub(crate) struct LaneA11y {
    pub bounds: Bounds<Pixels>,
    pub scale: f32,
    pub scroll_x: f32,
    pub zoom: f32,
    pub ppq: u64,
    pub mode: LaneMode,
    /// (event id, tick, value 0..127 or 0..16383 for PB)
    pub events: Arc<Vec<(EventId, u64, i32, i32)>>,
    /// velocity mode: notes of the selected track supply the bars
    pub notes: Arc<Vec<Note>>,
    pub sel_track: usize,
    pub track_names: Vec<String>,
    pub drag: Option<(DragMode, EventId, i64, i32)>,
}

impl LaneA11y {
    pub(crate) fn build(self, b: &mut A11ySubtreeBuilder) {
        let bounds = self.bounds;
        if bounds.size.width <= px(0.0) || bounds.size.height <= px(0.0) {
            return;
        }
        let (ox, oy) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
        let (vw, vh) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let (scroll_x, zoom) = (self.scroll_x, self.zoom);
        let scale = self.scale;
        let vrange = if self.mode == LaneMode::PitchBend {
            16383.0
        } else {
            127.0
        };
        let mut pushed = 0usize;
        let point_node =
            |b: &mut A11ySubtreeBuilder, id: u64, label: String, val: f64, x: f32, y: f32| {
                let mut node = Node::new(Role::Slider);
                node.set_label(label);
                node.set_numeric_value(val);
                node.set_min_numeric_value(0.0);
                node.set_max_numeric_value(vrange as f64);
                node.set_orientation(Orientation::Vertical);
                node.set_bounds(rect(x - 2.0, y - 2.0, 4.0, 4.0, scale));
                b.push_child(b.synthetic_node_id(("pt", id)), node);
            };
        match self.mode {
            LaneMode::Velocity => {
                for n in self.notes.iter().filter(|n| n.track == self.sel_track) {
                    if pushed >= MAX_NODES {
                        break;
                    }
                    let x = ox + n.start_tick as f32 * zoom - scroll_x;
                    if x < ox || x > ox + vw {
                        continue;
                    }
                    let mut vel = n.vel as f32 / 127.0;
                    if let Some((DragMode::Velocity, d_on, _, dkey)) = self.drag {
                        if d_on == n.on_id {
                            vel = (dkey as f32 / 127.0).clamp(0.0, 1.0);
                        }
                    }
                    let y = oy + (vh - 4.0) * (1.0 - vel) + 2.0;
                    let label = tf(
                        "a11y.vel_pt",
                        &[
                            ("key", note_name(n.key as i64).as_str()),
                            ("vel", (vel * 127.0).round().to_string().as_str()),
                            ("pos", pos_label(n.start_tick, self.ppq).as_str()),
                            (
                                "track",
                                self.track_names
                                    .get(n.track)
                                    .map(|s| s.as_str())
                                    .unwrap_or(""),
                            ),
                        ],
                    );
                    point_node(b, n.on_id, label, vel as f64 * 127.0, x, y);
                    pushed += 1;
                }
            }
            _ => {
                for (id, tick, val, _key) in self.events.iter() {
                    if pushed >= MAX_NODES {
                        break;
                    }
                    let mut v = *val;
                    if let Some((DragMode::LaneEvent, d_on, _, dkey)) = self.drag {
                        if d_on == *id {
                            v = dkey.clamp(0, vrange as i32);
                        }
                    }
                    let x = ox + *tick as f32 * zoom - scroll_x;
                    if x < ox || x > ox + vw {
                        continue;
                    }
                    let y = oy + (vh - 4.0) * (1.0 - v as f32 / vrange) + 2.0;
                    point_node(
                        b,
                        *id,
                        lane_point_label(self.mode, *tick, v, self.ppq),
                        v as f64,
                        x,
                        y,
                    );
                    pushed += 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(key: u8, start: u64, end: Option<u64>, vel: u8, track: usize) -> Note {
        Note {
            on_id: start * 16 + key as u64,
            off_id: end.map(|_| start * 16 + 8),
            start_tick: start,
            end_tick: end,
            key,
            vel,
            channel: 0,
            track,
            off_vel: 0,
            off_via_on: false,
        }
    }

    #[test]
    fn note_names_match_midi_octave_convention() {
        assert_eq!(note_name(60), "C4");
        assert_eq!(note_name(69), "A4");
        assert_eq!(note_name(21), "A0");
        assert_eq!(note_name(127), "G9");
        assert_eq!(note_name(61), "C#4");
    }

    #[test]
    fn pos_label_matches_transport_format() {
        assert_eq!(pos_label(0, 480), "1.1.  0");
        assert_eq!(pos_label(480, 480), "1.2.  0");
        assert_eq!(pos_label(1920, 480), "2.1.  0");
        assert_eq!(pos_label(2050, 480), "2.1.130");
    }

    #[test]
    fn note_label_describes_pitch_dur_vel_track() {
        let s = note_label(&n(60, 480, Some(960), 96, 0), 480, "Piano");
        assert!(s.contains("C4"), "{s}");
        assert!(s.contains("1.2.  0"), "{s}");
        assert!(s.contains("1.0"), "{s}");
        assert!(s.contains("96"), "{s}");
        assert!(s.contains("Piano"), "{s}");
    }

    #[test]
    fn dangling_note_says_so() {
        let s = note_label(&n(60, 0, None, 80, 1), 480, "Drums");
        assert_ne!(s, note_label(&n(60, 0, Some(480), 80, 1), 480, "Drums"));
    }

    #[test]
    fn lane_point_label_has_mode_value_pos() {
        let s = lane_point_label(LaneMode::CC(7), 960, 100, 480);
        assert!(s.contains("CC7"), "{s}");
        assert!(s.contains("100"), "{s}");
        assert!(s.contains("1.3.  0"), "{s}");
    }
}
