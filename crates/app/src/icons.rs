//! Material Symbols (outlined, 24px) embedded as build-time assets and
//! tinted through the element's text color at paint time.

use gpui_kit::*;

fn bytes(name: &str) -> &'static [u8] {
    match name {
        "arrow_downward" => include_bytes!("../assets/icons/arrow_downward.svg"),
        "arrow_selector_tool" => include_bytes!("../assets/icons/arrow_selector_tool.svg"),
        "arrow_upward" => include_bytes!("../assets/icons/arrow_upward.svg"),
        "compress" => include_bytes!("../assets/icons/compress.svg"),
        "edit" => include_bytes!("../assets/icons/edit.svg"),
        "fiber_manual_record" => include_bytes!("../assets/icons/fiber_manual_record.svg"),
        "folder_open" => include_bytes!("../assets/icons/folder_open.svg"),
        "grid_on" => include_bytes!("../assets/icons/grid_on.svg"),
        "help" => include_bytes!("../assets/icons/help.svg"),
        "ink_eraser" => include_bytes!("../assets/icons/ink_eraser.svg"),
        "keyboard_arrow_down" => include_bytes!("../assets/icons/keyboard_arrow_down.svg"),
        "loop" => include_bytes!("../assets/icons/loop.svg"),
        "metronome" => include_bytes!("../assets/icons/metronome.svg"),
        "note_add" => include_bytes!("../assets/icons/note_add.svg"),
        "piano" => include_bytes!("../assets/icons/piano.svg"),
        "skip_previous" => include_bytes!("../assets/icons/skip_previous.svg"),
        "play_arrow" => include_bytes!("../assets/icons/play_arrow.svg"),
        "redo" => include_bytes!("../assets/icons/redo.svg"),
        "repeat" => include_bytes!("../assets/icons/repeat.svg"),
        "save" => include_bytes!("../assets/icons/save.svg"),
        "stop" => include_bytes!("../assets/icons/stop.svg"),
        "timer" => include_bytes!("../assets/icons/timer.svg"),
        "tune" => include_bytes!("../assets/icons/tune.svg"),
        "undo" => include_bytes!("../assets/icons/undo.svg"),
        "view_list" => include_bytes!("../assets/icons/view_list.svg"),
        "zoom_in" => include_bytes!("../assets/icons/zoom_in.svg"),
        "zoom_out" => include_bytes!("../assets/icons/zoom_out.svg"),
        _ => &[],
    }
}

pub fn icon(name: &'static str, size: f32, color: u32) -> Svg {
    svg()
        .data(bytes(name))
        .w(px(size))
        .h(px(size))
        .text_color(rgb(color))
}
