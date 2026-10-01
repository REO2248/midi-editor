//! UI regression tests: deterministic element-tree goldens and scripted
//! interactions against the real `EditorView` in a headless GPUI window.
//!
//! Rasterized screenshots are impossible on Windows — gpui's headless
//! renderer only exists on macOS — so "goldens" here capture resolved
//! geometry, visibility and labels of every `.test_support()`-observed
//! element plus the view's scroll/zoom/selection state: the places a
//! layout regression actually lands. Updating a golden is an explicit,
//! reviewable step:
//!
//!   set MIDI_EDITOR_UPDATE_GOLDENS=1
//!   cargo test -p midi-editor ui_tests
//!
//! Determinism: fixed 1440x900 window, gpui's test text system has fixed
//! metrics (no real font rasterization), the theme is pinned dark like
//! `main()`, and every test pins `i18n::set_test_lang` on its own thread
//! so locales can't bleed between parallel tests.

use crate::{empty_doc, EditorView, LaneMode, PluginState, Sub, Tool, TopMenu, NOTE_H};
use document::Document;
use gpui_kit::component::input::InputState;
use gpui_kit::component::Root;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    point, px, size, AnyWindowHandle, AppContext as _, Bounds, Entity, Pixels, ScrollDelta,
    TestAppContext, Window,
};
use smf_core::{Division, Event as SmfEvent, EventKind};
use std::fmt::Write as _;

// --- fixtures ----------------------------------------------------------------

fn ev(tick: u64, seq: u32, kind: EventKind) -> SmfEvent {
    SmfEvent {
        tick,
        seq,
        raw_body: None,
        kind,
    }
}
fn chan(tick: u64, seq: u32, status: u8, d1: u8, d2: u8) -> SmfEvent {
    ev(
        tick,
        seq,
        EventKind::Channel {
            status,
            data: [d1, d2],
            len: 2,
        },
    )
}
fn meta(tick: u64, seq: u32, mt: u8, data: &[u8]) -> SmfEvent {
    ev(
        tick,
        seq,
        EventKind::Meta {
            meta_type: mt,
            data: bytes::Bytes::copy_from_slice(data),
        },
    )
}

/// Two content tracks over a conductor track: dense 4-note chords across
/// two bars on ch 0, bass notes + CC64 pedal on ch 1, and one dangling
/// note-on (never paired) so diagnostics have something to report.
fn fixture_doc() -> Document {
    let t0 = smf_core::Track {
        events: vec![
            meta(0, 0, 0x03, b"Conductor"),
            meta(0, 1, 0x51, &[0x07, 0xA1, 0x20]), // 500000 us/qn
            meta(0, 2, 0x58, &[0x04, 0x02, 0x18, 0x08]), // 4/4
        ],
    };
    let mut events = vec![meta(0, 0, 0x03, b"Piano")];
    let mut seq = 1u32;
    for q in 0..8u64 {
        for &k in [60u8, 64, 67, 72].iter() {
            let key = k + (q % 3) as u8;
            events.push(chan(q * 480, seq, 0x90, key, 90));
            events.push(chan(q * 480 + 420, seq + 1, 0x80, key, 0));
            seq += 2;
        }
    }
    // unpaired note-on → dangling diagnostic
    events.push(chan(8 * 480, seq, 0x90, 76, 100));
    let t1 = smf_core::Track { events };
    let t2 = smf_core::Track {
        events: vec![
            meta(0, 0, 0x03, b"Bass"),
            chan(0, 1, 0xB1, 64, 127), // sustain pedal down
            chan(0, 2, 0x91, 36, 96),
            chan(960, 3, 0x81, 36, 0),
            chan(960, 4, 0x91, 43, 96),
            chan(1920, 5, 0x81, 43, 0),
            chan(1920, 6, 0xB1, 64, 0), // pedal up
        ],
    };
    Document::from_file(smf_core::File {
        format: 1,
        division: Division::Metrical(480),
        tracks: vec![t0, t1, t2],
        warnings: vec![],
    })
}

// --- test window -------------------------------------------------------------

fn open_editor(cx: &mut TestAppContext, doc: Document) -> (Entity<EditorView>, AnyWindowHandle) {
    let mut slot = None;
    let handle = cx.open_window(size(px(1440.), px(900.)), |window, cx| {
        let input = cx
            .new(|cx| InputState::new(window, cx).placeholder(crate::i18n::t("field.track_name")));
        let prop_input = cx.new(|cx| InputState::new(window, cx));
        let meta_input = cx.new(|cx| InputState::new(window, cx));
        let view = cx.new(|cx| {
            let v = EditorView::new_for_test(doc, input, prop_input, meta_input, window, cx);
            window.focus(&v.focus.clone(), cx);
            v
        });
        slot = Some(view.clone());
        Root::new(view, window, cx)
    });
    (slot.unwrap(), handle.into())
}

fn init(cx: &mut TestAppContext, lang: &'static str) {
    crate::i18n::set_test_lang(lang);
    cx.update(gpui_kit::init);
    cx.update(|cx| {
        gpui_kit::component::theme::Theme::change(
            gpui_kit::component::theme::ThemeMode::Dark,
            None,
            cx,
        );
    });
}

// --- golden capture ----------------------------------------------------------

fn r(v: Pixels) -> f32 {
    (f32::from(v) * 2.0).round() / 2.0
}
fn fmt_bounds(b: Bounds<Pixels>) -> String {
    format!(
        "[{},{},{}x{}]",
        r(b.origin.x),
        r(b.origin.y),
        r(b.size.width),
        r(b.size.height)
    )
}

fn menu_name(m: TopMenu) -> &'static str {
    match m {
        TopMenu::File => "File",
        TopMenu::Edit => "Edit",
        TopMenu::View => "View",
        TopMenu::Track => "Track",
        TopMenu::Output => "Output",
        TopMenu::Transport => "Transport",
        TopMenu::Help => "Help",
    }
}
fn lane_name(m: LaneMode) -> String {
    match m {
        LaneMode::Velocity => "Vel".into(),
        LaneMode::CC(n) => format!("CC{n}"),
        LaneMode::PitchBend => "PB".into(),
        LaneMode::ChanAT => "CAT".into(),
        LaneMode::PolyAT => "PAT".into(),
    }
}
fn tool_name(t: Tool) -> &'static str {
    match t {
        Tool::Select => "Select",
        Tool::Draw => "Draw",
        Tool::Erase => "Erase",
    }
}

/// View state + every observed element, sorted by element path so the
/// golden is stable across hash-map iteration order. Bounds are rounded
/// to half-pixels — layout regressions move things by whole pixels, so
/// this stays an exact compare, not a fuzzy one.
fn dump(window: &Window, v: &EditorView) -> String {
    let mut out = String::new();
    let menu = v.open_menu.map(|(m, _)| menu_name(m)).unwrap_or("-");
    let sub = v
        .open_sub
        .map(|(s, _)| match s {
            Sub::Tool => "Tool",
            Sub::Snap => "Snap",
            Sub::Quant => "Quant",
            Sub::Oct => "Oct",
            Sub::LenSet => "LenSet",
            Sub::VelSet => "VelSet",
            Sub::Chan => "Chan",
            Sub::Dest => "Dest",
            Sub::DefDest => "DefDest",
            Sub::InPort => "InPort",
            Sub::Lane => "Lane",
            Sub::Enc => "Enc",
            Sub::Recent => "Recent",
            Sub::RelSet => "RelSet",
            Sub::AudVel => "AudVel",
            Sub::AudDur => "AudDur",
            Sub::Theme => "Theme",
            Sub::RowH => "RowH",
            Sub::Scale => "Scale",
            Sub::LegatoGap => "LegatoGap",
            Sub::Swing => "Swing",
            Sub::AllTrack => "AllTrack",
            Sub::Meta => "Meta",
            Sub::MetDest => "MetDest",
            Sub::CountIn => "CountIn",
            Sub::Monitor => "Monitor",
            Sub::NoteLen => "NoteLen",
            Sub::InsVel => "InsVel",
        })
        .unwrap_or("-");
    let _ = writeln!(
        out,
        "view: tool={} sel_track={} snap_idx={} lane={} events_panel={} help={} out_status={} menu={} sub={}",
        tool_name(v.tool),
        v.sel_track,
        v.snap_idx,
        lane_name(v.lane_mode()),
        v.show_events,
        v.help_open,
        v.show_output_status,
        menu,
        sub,
    );
    let _ = writeln!(
        out,
        "view: scroll=({:.1},{:.1}) zoom={:.4} selection={} play_us={} notes={} events={} status={:?}",
        v.scroll_x,
        v.scroll_y,
        v.zoom,
        v.selection.len(),
        v.play_us,
        v.notes.len(),
        v.events.len(),
        v.status,
    );
    let _ = writeln!(
        out,
        "canvas: roll={} ruler={} lane={} mini={}",
        fmt_bounds(v.roll_bounds.get()),
        fmt_bounds(v.ruler_bounds.get()),
        fmt_bounds(
            v.lane_bounds
                .get(v.lane_focus)
                .map(|c| c.get())
                .unwrap_or_default(),
        ),
        fmt_bounds(v.mini_bounds.get()),
    );
    let mut snaps = gpui_kit::base::test_support::snapshots(window);
    snaps.sort_by_key(|s| format!("{:?}", s.path()));
    for s in snaps {
        let _ = writeln!(
            out,
            "elem {:?} {} vis={} role={:?} label={:?} value={:?} checked={:?} sel={:?} dis={:?}",
            s.path(),
            fmt_bounds(s.bounds()),
            s.visible(),
            s.role(),
            s.label(),
            s.value(),
            s.checked(),
            s.selected(),
            s.disabled(),
        );
    }
    out
}

fn golden(name: &str, actual: String) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("goldens");
    let path = dir.join(format!("{name}.golden.txt"));
    if std::env::var_os("MIDI_EDITOR_UPDATE_GOLDENS").is_some() {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, &actual).unwrap();
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!("missing golden {path:?}; regenerate with MIDI_EDITOR_UPDATE_GOLDENS=1")
    });
    assert_eq!(
        expected, actual,
        "golden {name} drifted — review the diff, then refresh deliberately with MIDI_EDITOR_UPDATE_GOLDENS=1"
    );
}

fn render_and_golden(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    view: &Entity<EditorView>,
    name: &str,
) {
    cx.update_window(window, |_, w, cx| {
        w.render_frame(cx);
        golden(name, dump(w, view.read(cx)));
    })
    .unwrap();
}

// --- golden scenes -----------------------------------------------------------

#[gpui_kit::test]
fn golden_empty_file(cx: &mut TestAppContext) {
    init(cx, "en");
    let (view, window) = open_editor(cx, empty_doc());
    render_and_golden(cx, window, &view, "empty_file");
}

#[gpui_kit::test]
fn golden_dense_editor(cx: &mut TestAppContext) {
    init(cx, "en");
    let (view, window) = open_editor(cx, fixture_doc());
    render_and_golden(cx, window, &view, "dense_editor");
}

/// Event list + diagnostics: the fixture's dangling note-on puts the
/// "[fix]" row into the panel header.
#[gpui_kit::test]
fn golden_event_list_diagnostics(cx: &mut TestAppContext) {
    init(cx, "en");
    let (view, window) = open_editor(cx, fixture_doc());
    cx.update_window(window, |_, w, cx| {
        w.render_frame(cx);
        assert!(
            w.try_find("fix-diags").is_some(),
            "fixture has a dangling note-on — the diagnostics fix row must render"
        );
        golden("event_list_diagnostics", dump(w, view.read(cx)));
    })
    .unwrap();
}

/// File menu open: the dropdown's row layout is exactly the kind of
/// popup-sizing regression these tests exist to catch.
#[gpui_kit::test]
fn golden_file_menu(cx: &mut TestAppContext) {
    init(cx, "en");
    let (view, window) = open_editor(cx, fixture_doc());
    cx.update_window(window, |_, w, cx| {
        w.render_frame(cx);
        w.click("menu.file", cx);
        w.render_frame(cx);
        assert!(
            w.try_find("file.new").is_some(),
            "File dropdown did not open"
        );
        golden("file_menu", dump(w, view.read(cx)));
    })
    .unwrap();
}

/// Cascading submenu (Edit > Snap grid): geometry that has regressed
/// before — popup position must land beside its parent row.
#[gpui_kit::test]
fn golden_edit_submenu(cx: &mut TestAppContext) {
    init(cx, "en");
    let (view, window) = open_editor(cx, fixture_doc());
    cx.update_window(window, |_, w, cx| {
        w.render_frame(cx);
        w.click("menu.edit", cx);
        w.render_frame(cx);
        w.hover("e.snap", cx);
        w.render_frame(cx);
        assert!(
            w.try_find("sub-popup").is_some(),
            "submenu did not open on hover"
        );
        golden("edit_submenu", dump(w, view.read(cx)));
    })
    .unwrap();
}

/// Controller lane: CC lane on the Bass track's pedal data.
#[gpui_kit::test]
fn golden_controller_lane(cx: &mut TestAppContext) {
    init(cx, "en");
    let (view, window) = open_editor(cx, fixture_doc());
    cx.update_window(window, |_, w, cx| {
        view.update(cx, |v, cx| {
            v.sel_track = 2;
            v.set_lane(LaneMode::CC(64), cx);
            cx.notify();
        });
        w.render_frame(cx);
        golden("controller_lane", dump(w, view.read(cx)));
    })
    .unwrap();
}

/// Plugin-unavailable state: failed load in the Output status panel.
#[gpui_kit::test]
fn golden_plugin_unavailable(cx: &mut TestAppContext) {
    init(cx, "en");
    let (view, window) = open_editor(cx, fixture_doc());
    cx.update_window(window, |_, w, cx| {
        view.update(cx, |v, cx| {
            v.plugin_state.insert(
                1,
                PluginState::Failed {
                    path: "C:/Fixtures/TestSynth.vst3".into(),
                    phase: "load",
                    msg: "the VST3 bundle was removed".into(),
                },
            );
            v.show_output_status = true;
            cx.notify();
        });
        w.render_frame(cx);
        assert!(w.try_find("output-status-panel").is_some());
        golden("plugin_unavailable", dump(w, view.read(cx)));
    })
    .unwrap();
}

/// Japanese locale: menu bar, panels and status render from the JA table.
#[gpui_kit::test]
fn golden_japanese(cx: &mut TestAppContext) {
    init(cx, "ja");
    let (view, window) = open_editor(cx, fixture_doc());
    render_and_golden(cx, window, &view, "japanese");
}

/// High DPI: 2x scale factor must not change logical layout — every
/// element's bounds stay identical, so this asserts equality rather than
/// storing a second copy of the same geometry.
#[gpui_kit::test]
fn high_dpi_layout_is_unchanged(cx: &mut TestAppContext) {
    init(cx, "en");
    let (view, window) = open_editor(cx, fixture_doc());
    let before = cx
        .update_window(window, |_, w, cx| {
            w.render_frame(cx);
            dump(w, view.read(cx))
        })
        .unwrap();
    cx.simulate_window_scale_factor_change(window, 2.0);
    let after = cx
        .update_window(window, |_, w, cx| {
            w.render_frame(cx);
            dump(w, view.read(cx))
        })
        .unwrap();
    assert_eq!(
        before, after,
        "scale factor change must not perturb logical layout"
    );
}

// --- scripted interactions ---------------------------------------------------

/// Menu navigation: open File, hover-switch to Edit, then Escape closes.
#[gpui_kit::test]
fn menu_opens_switches_and_closes(cx: &mut TestAppContext) {
    init(cx, "en");
    let (_view, window) = open_editor(cx, fixture_doc());
    cx.update_window(window, |_, w, cx| {
        w.render_frame(cx);
        w.click("menu.file", cx);
        assert!(w.try_find("file.new").is_some(), "File menu did not open");
        // while a menu is open, hovering a sibling head switches to it
        w.hover("menu.edit", cx);
        assert!(
            w.try_find("edit.undo").is_some(),
            "Edit menu did not take over"
        );
        w.press("escape", cx);
        assert!(
            w.try_find("menu-popup").is_none(),
            "Escape did not close the menu"
        );
    })
    .unwrap();
}

/// Submenu cascade opens on hover over a ▸ row.
#[gpui_kit::test]
fn submenu_cascades_on_hover(cx: &mut TestAppContext) {
    init(cx, "en");
    let (_view, window) = open_editor(cx, fixture_doc());
    cx.update_window(window, |_, w, cx| {
        w.render_frame(cx);
        w.click("menu.edit", cx);
        w.hover("e.tool", cx);
        w.render_frame(cx);
        assert!(w.try_find("sub-popup").is_some(), "tool submenu missing");
    })
    .unwrap();
}

/// Track column: click selects; M mutes; S solos.
#[gpui_kit::test]
fn track_select_mute_solo(cx: &mut TestAppContext) {
    init(cx, "en");
    let (view, window) = open_editor(cx, fixture_doc());
    cx.update_window(window, |_, w, cx| {
        w.render_frame(cx);
        w.click(("track", 1usize), cx);
        assert_eq!(view.read(cx).sel_track, 1);
        w.click(("mute", 1usize), cx);
        w.click(("solo", 2usize), cx);
        let (muted, soloed) = view.update(cx, |v, _| {
            let sh = crate::lock_shared(&v.shared);
            (sh.muted.clone(), sh.soloed.clone())
        });
        assert!(muted.contains(&1), "mute click did not mark track 1");
        assert!(soloed.contains(&2), "solo click did not mark track 2");
    })
    .unwrap();
}

/// Ctrl+A selects every note in the selected track.
#[gpui_kit::test]
fn ctrl_a_selects_track_notes(cx: &mut TestAppContext) {
    init(cx, "en");
    let (view, window) = open_editor(cx, fixture_doc());
    cx.update_window(window, |_, w, cx| {
        w.render_frame(cx);
        let expected = view.update(cx, |v, _| {
            v.sel_track = 1;
            v.notes.iter().filter(|n| n.track == 1).count()
        });
        w.press("ctrl-a", cx);
        assert_eq!(view.read(cx).selection.len(), expected);
        assert!(expected > 0);
    })
    .unwrap();
}

/// Rubber-band drag on empty canvas around the chord block marquees
/// the notes inside it.
#[gpui_kit::test]
fn marquee_drag_selects_notes(cx: &mut TestAppContext) {
    init(cx, "en");
    let (view, window) = open_editor(cx, fixture_doc());
    cx.update_window(window, |_, w, cx| {
        w.render_frame(cx);
        let b = view.read(cx).roll_bounds.get();
        // notes sit at keys 60..75 → rows (127-60)..(127-75); the default
        // scroll_y puts them roughly 150..330 px into the roll viewport
        let from = point(b.origin.x + px(5.), b.origin.y + px(120.));
        let to = point(b.origin.x + px(340.), b.origin.y + px(380.));
        w.drag(from, to, cx);
        assert!(
            !view.read(cx).selection.is_empty(),
            "marquee over the chord block selected nothing"
        );
    })
    .unwrap();
}

/// Wheel on the timeline scrolls vertically without panning or zooming.
/// (Ctrl+wheel zoom can't be scripted — the test scroll helper can't
/// attach modifiers; zoom itself is covered by the toolbar/key test.)
#[gpui_kit::test]
fn wheel_scrolls_vertically(cx: &mut TestAppContext) {
    init(cx, "en");
    let (view, window) = open_editor(cx, fixture_doc());
    cx.update_window(window, |_, w, cx| {
        w.render_frame(cx);
        let (sx0, sy0, z0) = {
            let v = view.read(cx);
            (v.scroll_x, v.scroll_y, v.zoom)
        };
        w.scroll("timeline", ScrollDelta::Pixels(point(px(0.), px(40.))), cx);
        let (sx1, sy1, z1) = {
            let v = view.read(cx);
            (v.scroll_x, v.scroll_y, v.zoom)
        };
        assert_eq!(sx0, sx1, "plain wheel must not pan the timeline");
        assert!(sy1 > sy0, "wheel did not scroll the roll");
        assert_eq!(z0, z1, "plain wheel must not zoom");
    })
    .unwrap();
}

/// Zoom toolbar button + Ctrl+0 reset.
#[gpui_kit::test]
fn zoom_buttons_and_reset(cx: &mut TestAppContext) {
    init(cx, "en");
    let (view, window) = open_editor(cx, fixture_doc());
    cx.update_window(window, |_, w, cx| {
        w.render_frame(cx);
        let z0 = view.read(cx).zoom;
        w.click("i.zin", cx);
        let z1 = view.read(cx).zoom;
        assert!(z1 > z0, "zoom-in button did not increase zoom");
        w.press("ctrl-0", cx);
        assert_eq!(view.read(cx).zoom, 0.08, "Ctrl+0 did not reset zoom");
    })
    .unwrap();
}

/// Lane-mode chip cycles Vel -> CC1 -> ... -> PB -> Vel.
#[gpui_kit::test]
fn lane_chip_cycles_modes(cx: &mut TestAppContext) {
    init(cx, "en");
    let (view, window) = open_editor(cx, fixture_doc());
    cx.update_window(window, |_, w, cx| {
        w.render_frame(cx);
        assert_eq!(view.read(cx).lane_mode(), LaneMode::Velocity);
        w.click("st-lane", cx);
        assert_eq!(view.read(cx).lane_mode(), LaneMode::CC(1));
    })
    .unwrap();
}

/// #131: a selection-scoped transform with an empty selection must not
/// touch the document; the explicit whole-track op still works.
#[gpui_kit::test]
fn region_op_requires_selection(cx: &mut TestAppContext) {
    init(cx, "en");
    let (view, window) = open_editor(cx, fixture_doc());
    cx.update_window(window, |_, w, cx| {
        w.render_frame(cx);
        view.update(cx, |v, _| {
            let rev0 = crate::lock_shared(&v.shared).doc.revision();
            v.selection.clear();
            v.apply_region_op("transpose", |d, t, f, to| d.transpose_ops(t, f, to, 1));
            assert_eq!(
                crate::lock_shared(&v.shared).doc.revision(),
                rev0,
                "empty-selection transpose mutated the document"
            );
            assert!(!v.status.is_empty(), "no-selection op gave no status");
            // single-note selection → applies to its track/range only
            let on_id = v
                .notes
                .iter()
                .find(|n| n.track == 1)
                .expect("fixture track-1 note")
                .on_id;
            v.selection.insert(on_id);
            v.apply_region_op("transpose", |d, t, f, to| d.transpose_ops(t, f, to, 1));
            let rev1 = crate::lock_shared(&v.shared).doc.revision();
            assert!(rev1 > rev0, "selected-note transpose did nothing");
            // explicit whole-track path works regardless of selection
            v.selection.clear();
            v.apply_track_op("transpose", |d, t, f, to| d.transpose_ops(t, f, to, -1));
            assert!(
                crate::lock_shared(&v.shared).doc.revision() > rev1,
                "whole-track op did nothing"
            );
        });
    })
    .unwrap();
}

/// Drag a chord note right: the note's start moves to the snapped tick.
#[gpui_kit::test]
fn note_drag_moves_note(cx: &mut TestAppContext) {
    init(cx, "en");
    let (view, window) = open_editor(cx, fixture_doc());
    cx.update_window(window, |_, w, cx| {
        w.render_frame(cx);
        // first note of track 1: tick 0, key 60 — its on-screen center
        let (from, before) = {
            let v = view.read(cx);
            let b = v.roll_bounds.get();
            let y = b.origin.y + px((127. - 60.) * NOTE_H - v.scroll_y + 6.5);
            let x = b.origin.x + px(0. - v.scroll_x + 8.);
            let n = v
                .notes
                .iter()
                .find(|n| n.track == 1 && n.key == 60 && n.start_tick == 0)
                .expect("fixture note 60@0 on track 1");
            (point(x, y), n.start_tick)
        };
        w.drag(from, point(from.x + px(48.), from.y), cx);
        let after = view.update(cx, |v, _| {
            let sh = crate::lock_shared(&v.shared);
            sh.doc
                .notes()
                .iter()
                .find(|n| n.track == 1 && n.key == 60)
                .map(|n| n.start_tick)
        });
        let after = after.expect("moved note missing from document");
        assert!(after > before, "drag did not move the note's start tick");
    })
    .unwrap();
}

// --- #133: playhead-aware tempo / signature controls ------------------------

/// Tempo bump at the playhead writes a tempo event at the playhead tick and
/// leaves the tick-0 tempo untouched.
#[gpui_kit::test]
fn tempo_bump_writes_at_playhead(cx: &mut TestAppContext) {
    init(cx, "en");
    let (view, window) = open_editor(cx, fixture_doc());
    cx.update_window(window, |_, w, cx| {
        w.render_frame(cx);
        view.update(cx, |v, _cx| {
            v.play_us = v.doc(|d| d.tempo_map.tick_to_us(480));
            v.bump_tempo(1.0);
            let sh = crate::lock_shared(&v.shared);
            assert!(
                crate::edit_ops::tempo_event_at(&sh.doc, 0, 480).is_some(),
                "no tempo event written at the playhead tick"
            );
            assert!(
                (crate::edit_ops::tempo_bpm_at(&sh.doc, 0, 0) - 120.0).abs() < 0.01,
                "tick-0 tempo was rewritten"
            );
            let (_, bpm) = crate::edit_ops::tempo_event_at(&sh.doc, 0, 480).unwrap();
            assert!((bpm - 121.0).abs() < 0.5);
        });
    })
    .unwrap();
}

/// Signature cycle at the playhead inserts a signature event there without
/// touching the file's tick-0 signature.
#[gpui_kit::test]
fn sig_cycle_writes_at_playhead(cx: &mut TestAppContext) {
    init(cx, "en");
    let (view, window) = open_editor(cx, fixture_doc());
    cx.update_window(window, |_, w, cx| {
        w.render_frame(cx);
        view.update(cx, |v, _cx| {
            v.play_us = v.doc(|d| d.tempo_map.tick_to_us(960));
            v.cycle_time_sig();
            let sh = crate::lock_shared(&v.shared);
            let (_, num, den) = crate::edit_ops::sig_event_at(&sh.doc, 0, 960)
                .expect("no signature event at the playhead tick");
            assert_eq!((num, den), (3, 4));
            // tick-0 4/4 is still in force before the playhead
            let m = sh.doc.meter_map_for(0).meter_at(0);
            assert_eq!((m.num, 1u32 << m.den_pow), (4, 4));
        });
    })
    .unwrap();
}

/// `delete_tempo_sig` removes the event at the playhead tick; earlier events
/// stay in force so the map falls back correctly.
#[gpui_kit::test]
fn tempo_delete_at_playhead(cx: &mut TestAppContext) {
    init(cx, "en");
    let (view, window) = open_editor(cx, fixture_doc());
    cx.update_window(window, |_, w, cx| {
        w.render_frame(cx);
        view.update(cx, |v, cx| {
            v.play_us = v.doc(|d| d.tempo_map.tick_to_us(480));
            v.bump_tempo(1.0);
            v.delete_tempo_sig(0x51, cx);
            let sh = crate::lock_shared(&v.shared);
            assert!(crate::edit_ops::tempo_event_at(&sh.doc, 0, 480).is_none());
            assert!((crate::edit_ops::tempo_bpm_at(&sh.doc, 0, 480) - 120.0).abs() < 0.01);
        });
    })
    .unwrap();
}
