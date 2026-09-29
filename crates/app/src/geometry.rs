//! Pure piano-roll view math shared by hit-testing, painting, and scrolling.
//! GPUI types stay out so the coordinate mapping has one source of truth and
//! is unit-testable: render, `EditorView::hit`, and the scroll/zoom paths all
//! call these functions instead of re-deriving the arithmetic inline.

use crate::NOTE_H;

pub(crate) const ZOOM_MIN: f32 = 0.005;
pub(crate) const ZOOM_MAX: f32 = 1.0;

/// (tick, key) under a canvas-local (x, y). Tick truncates (a pixel inside
/// tick n is tick n) and floors at 0; the key is the painted row band — every
/// pixel of a row resolves to that key, never the one below it. Key may fall
/// outside 0..=127 (callers decide: marquee anchors reject it, drags clamp
/// via [`clamp_move_delta`]).
pub(crate) fn roll_hit(x: f32, y: f32, scroll_x: f32, scroll_y: f32, zoom: f32) -> (i64, i32) {
    let tick = ((x + scroll_x) / zoom) as i64;
    let row = ((y + scroll_y) / NOTE_H).floor() as i32;
    (tick.max(0), 127 - row)
}

/// Clamp one scroll axis into `[0, content - view]`. Content shorter than the
/// view pins to 0 — no scrolling into empty space past the last note.
pub(crate) fn clamp_span(scroll: f32, content: f32, view: f32) -> f32 {
    if !scroll.is_finite() {
        return 0.0;
    }
    scroll.clamp(0.0, (content - view).max(0.0))
}

/// Scroll offset after a zoom change that keeps the tick at `offset_x` pixels
/// from the viewport's left edge fixed under the cursor.
pub(crate) fn reanchor(scroll_x: f32, old_zoom: f32, new_zoom: f32, offset_x: f32) -> f32 {
    let anchor_tick = (offset_x + scroll_x) / old_zoom;
    (anchor_tick * new_zoom - offset_x).max(0.0)
}

/// First/last tick that can be visible in the viewport (exit bound inclusive,
/// one tick of slack for 1px note bodies).
pub(crate) fn tick_window(scroll_x: f32, zoom: f32, view_w: f32) -> (i64, i64) {
    let t0 = ((scroll_x.max(0.0)) / zoom) as i64;
    let t1 = t0 + (view_w.max(0.0) / zoom) as i64 + 1;
    (t0, t1)
}

/// Iteration bounds over the start-tick-sorted note list while a Move drag
/// shifts some notes by `dtick`: entry must back off by the rightward shift
/// (notes dragged into view from the left), exit must extend by the leftward
/// one (notes dragged into view from the right).
pub(crate) fn drag_window(t0: i64, t1: i64, dtick: i64) -> (i64, i64) {
    (t0 - dtick.max(0), t1 + (-dtick).max(0))
}

/// Clamp a Move drag's deltas so the dragged note stays on the keyboard and
/// before tick 0 while the drag is in progress (commit applies the same
/// clamps, but the preview must not show impossible positions either).
pub(crate) fn clamp_move_delta(dtick: i64, orig_start: u64, dkey: i32, orig_key: u8) -> (i64, i32) {
    let dtick = dtick.max(-(orig_start as i64));
    let lo = -(orig_key as i32);
    let hi = 127 - orig_key as i32;
    (dtick, dkey.clamp(lo, hi))
}

/// Scroll offsets that land freshly opened content in frame: the first note
/// gets a small left margin, the median pitch sits 16 rows below the top
/// edge (centered in a typical roll). `mid_key: None` = no notes — keep the
/// familiar C3-ish default view.
pub(crate) fn content_view(first_tick: u64, mid_key: Option<i32>, zoom: f32) -> (f32, f32) {
    let scroll_x = (first_tick as f32 * zoom - 200.0).max(0.0);
    let scroll_y = match mid_key {
        Some(k) => (127 - k - 16).max(0) as f32 * NOTE_H,
        None => (127.0 - 84.0) * NOTE_H,
    };
    (scroll_x, scroll_y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_maps_rows_by_band_not_rounding() {
        // row k spans [(127-k)*H, (127-k+1)*H) in scrolled space; every
        // pixel of the band is key k — the old .round() made the bottom half
        // of each row select the key below it
        let y = |key: i32, off: f32| (127 - key) as f32 * NOTE_H + off - 500.0;
        assert_eq!(roll_hit(0.0, y(60, 0.0), 0.0, 500.0, 0.1).1, 60);
        assert_eq!(roll_hit(0.0, y(60, NOTE_H - 0.01), 0.0, 500.0, 0.1).1, 60);
        assert_eq!(roll_hit(0.0, y(60, -0.01), 0.0, 500.0, 0.1).1, 61); // row above
        assert_eq!(roll_hit(0.0, y(60, NOTE_H), 0.0, 500.0, 0.1).1, 59); // row below
    }

    #[test]
    fn hit_tick_floors_and_clamps_to_zero() {
        assert_eq!(roll_hit(0.0, 0.0, 0.0, 0.0, 0.5).0, 0);
        assert_eq!(roll_hit(9.9, 0.0, 0.0, 0.0, 0.5).0, 19); // 19.8 ticks
        assert_eq!(roll_hit(-50.0, 0.0, 0.0, 0.0, 0.5).0, 0);
    }

    #[test]
    fn clamp_span_pins_when_content_fits() {
        assert_eq!(clamp_span(-10.0, 500.0, 300.0), 0.0);
        assert_eq!(clamp_span(400.0, 500.0, 300.0), 200.0);
        assert_eq!(clamp_span(100.0, 100.0, 300.0), 0.0);
        assert_eq!(clamp_span(f32::NAN, 100.0, 300.0), 0.0);
    }

    #[test]
    fn reanchor_keeps_cursor_tick_fixed() {
        // tick under a cursor 100px in, zoom doubling: the same tick must
        // still sit 100px from the left edge
        let s = reanchor(400.0, 0.1, 0.2, 100.0);
        let tick_before = (100.0 + 400.0) / 0.1;
        let tick_after = (100.0 + s) / 0.2;
        assert!((tick_before - tick_after).abs() < 1e-3);
        // identity zoom is a no-op
        assert_eq!(reanchor(123.0, 0.1, 0.1, 55.0), 123.0);
        // anchoring near the left edge at high zoom-out pins at 0, never < 0
        assert_eq!(reanchor(0.0, 0.02, 0.01, 10.0), 0.0);
    }

    #[test]
    fn drag_window_widens_toward_the_shift_direction() {
        // dragging right: entry backs off so notes dragged into view from
        // the left are still visited
        assert_eq!(drag_window(100, 200, 50), (50, 200));
        // dragging left: exit extends so right-shifted-into-view notes are hit
        assert_eq!(drag_window(100, 200, -50), (100, 250));
        assert_eq!(drag_window(100, 200, 0), (100, 200));
    }

    #[test]
    fn clamp_move_delta_keeps_note_on_the_keyboard() {
        // key 3 dragged down 10 semitones: clamps to -3 (key 0)
        assert_eq!(clamp_move_delta(0, 100, -10, 3), (0, -3));
        // key 125 dragged up: clamps to +2 (key 127)
        assert_eq!(clamp_move_delta(0, 100, 5, 125), (0, 2));
        // start tick 10 dragged 100 ticks left: clamps to -10 (tick 0)
        assert_eq!(clamp_move_delta(-100, 10, 0, 60), (-10, 0));
        assert_eq!(clamp_move_delta(30, 10, 7, 60), (30, 7));
    }

    #[test]
    fn tick_window_covers_viewport_with_slack() {
        assert_eq!(tick_window(0.0, 0.1, 100.0), (0, 1001));
        assert_eq!(tick_window(100.0, 0.1, 100.0), (1000, 2001));
    }

    #[test]
    fn content_view_lands_on_the_music() {
        // notes from tick 10000, median key 60: first note near the left
        // edge with margin, key 60 sixteen rows from the top
        let (x, y) = content_view(10000, Some(60), 0.08);
        assert_eq!(x, 10000.0 * 0.08 - 200.0);
        assert_eq!(y, (127 - 60 - 16) as f32 * NOTE_H);
        // high content never yields a negative offset
        let (x, _) = content_view(480, Some(60), 0.08);
        assert_eq!(x, 0.0);
        // content above the centering row pins at the top
        let (_, y) = content_view(0, Some(120), 0.08);
        assert_eq!(y, 0.0);
        // no notes: the default C3-ish view
        let (x, y) = content_view(0, None, 0.08);
        assert_eq!((x, y), (0.0, (127.0 - 84.0) * NOTE_H));
    }
}
