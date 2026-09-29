//! Minimal i18n layer. Every user-visible string goes through `t(key)`.
//!
//! - English ("en") is the default and the fallback for missing keys.
//! - Japanese ("ja") ships alongside; more locales = another table entry.
//! - Language is picked once at startup: `MIDI_EDITOR_LANG` env var,
//!   else the OS UI language, else "en".
//!
//! Keep keys hierarchical (`menu.open`, `status.saved`) and English-first:
//! a missing translation falls back to the `en` table, then to the key
//! itself so gaps are visible during development instead of crashing.

use std::collections::HashMap;
use std::sync::OnceLock;

static TABLE: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();

static EN: &[(&str, &str)] = &[
    ("app.title", "midi-editor"),
    ("menu.open", "Open…"),
    ("menu.save", "Save"),
    ("menu.save_as", "Save As…"),
    ("menu.play", "Play"),
    ("menu.stop", "Stop"),
    ("menu.undo", "Undo"),
    ("menu.redo", "Redo"),
    ("menu.add_track", "Add Track"),
    ("transport.met", "met"),
    ("status.no_port", "no MIDI out"),
    ("status.no_file", "untitled"),
    ("status.saved", "saved"),
    ("status.failed", "error"),
    ("events.header", "Events"),
    ("events.issues", "issues"),
    ("status.fixed", "diagnostics fixed"),
    ("tracks.header", "Tracks"),
    ("field.track_name", "Track name"),
    // menubar
    ("menu.file", "File"),
    ("menu.edit", "Edit"),
    ("menu.view", "View"),
    ("menu.track", "Track"),
    ("menu.transport", "Transport"),
    ("menu.help", "Help"),
    // File
    ("menu.new", "New"),
    // Edit
    ("menu.select_all", "Select All"),
    ("menu.delete", "Delete"),
    ("edit.quantize", "Quantize"),
    ("edit.transpose_up", "Transpose +1"),
    ("edit.transpose_dn", "Transpose -1"),
    ("edit.vel_up", "Velocity +25%"),
    ("edit.vel_dn", "Velocity -20%"),
    // View
    ("view.events", "Event List"),
    ("view.zoom_in", "Zoom In"),
    ("view.zoom_out", "Zoom Out"),
    ("view.zoom_reset", "Zoom Reset"),
    ("view.lane", "Lane"),
    ("view.encoding", "Text Encoding"),
    ("enc.auto", "Auto"),
    // Track
    ("track.rename", "Rename…"),
    ("track.mute", "Mute"),
    ("track.solo", "Solo"),
    ("track.channel", "Output Channel"),
    ("track.dest", "Output Destination"),
    ("track.default_dest", "Inherit default"),
    ("track.plugin_gui", "Plugin GUI…"),
    // Transport
    ("transport.record", "Record"),
    ("transport.loop", "Loop"),
    ("transport.play_stop", "Play / Stop"),
    // Help
    ("help.about", "About midi-editor"),
    ("help.mcp", "MCP: http://127.0.0.1:7878/mcp"),
];

static JA: &[(&str, &str)] = &[
    ("menu.open", "開く…"),
    ("menu.save", "保存"),
    ("menu.save_as", "名前を付けて保存…"),
    ("menu.play", "再生"),
    ("menu.stop", "停止"),
    ("menu.undo", "元に戻す"),
    ("menu.redo", "やり直し"),
    ("menu.add_track", "トラック追加"),
    ("transport.met", "メトロノーム"),
    ("status.no_port", "MIDI出力なし"),
    ("status.no_file", "無題"),
    ("status.saved", "保存しました"),
    ("status.failed", "エラー"),
    ("events.header", "イベント"),
    ("events.issues", "件の問題"),
    ("status.fixed", "診断を修正しました"),
    ("tracks.header", "トラック"),
    ("field.track_name", "トラック名"),
    ("menu.file", "ファイル"),
    ("menu.edit", "編集"),
    ("menu.view", "表示"),
    ("menu.track", "トラック"),
    ("menu.transport", "トランスポート"),
    ("menu.help", "ヘルプ"),
    ("menu.new", "新規"),
    ("menu.select_all", "すべて選択"),
    ("menu.delete", "削除"),
    ("edit.quantize", "クオンタイズ"),
    ("edit.transpose_up", "半音上げ"),
    ("edit.transpose_dn", "半音下げ"),
    ("edit.vel_up", "ベロシティ +25%"),
    ("edit.vel_dn", "ベロシティ -20%"),
    ("view.events", "イベントリスト"),
    ("view.zoom_in", "ズームイン"),
    ("view.zoom_out", "ズームアウト"),
    ("view.zoom_reset", "ズームリセット"),
    ("view.lane", "レーン"),
    ("view.encoding", "文字エンコーディング"),
    ("enc.auto", "自動"),
    ("track.rename", "名前を変更…"),
    ("track.mute", "ミュート"),
    ("track.solo", "ソロ"),
    ("track.channel", "出力チャンネル"),
    ("track.dest", "出力先"),
    ("track.default_dest", "デフォルトに従う"),
    ("track.plugin_gui", "プラグインGUI…"),
    ("transport.record", "録音"),
    ("transport.loop", "ループ"),
    ("transport.play_stop", "再生 / 停止"),
    ("help.about", "midi-editor について"),
    ("help.mcp", "MCP: http://127.0.0.1:7878/mcp"),
];

fn detect_lang() -> &'static str {
    if let Ok(l) = std::env::var("MIDI_EDITOR_LANG") {
        if l.starts_with("ja") {
            return "ja";
        }
        return "en";
    }
    // OS UI language — coarse prefix match is enough for shipped locales
    let lang = std::env::var("LANG").unwrap_or_default();
    if lang.starts_with("ja") { "ja" } else { "en" }
}

fn table() -> &'static HashMap<&'static str, &'static str> {
    TABLE.get_or_init(|| {
        let src: &[(&str, &str)] = match detect_lang() {
            "ja" => JA,
            _ => EN,
        };
        src.iter().copied().collect()
    })
}

/// Look up `key` in the active locale, falling back to English then the key.
pub fn t(key: &'static str) -> &'static str {
    if let Some(v) = table().get(key) {
        return v;
    }
    EN.iter().find(|(k, _)| *k == key).map(|(_, v)| *v).unwrap_or(key)
}

#[allow(dead_code)]
/// `t` with `{name}` substitution: `tf("status.saved", &[("file", name)])`.
pub fn tf(key: &'static str, args: &[(&str, &str)]) -> String {
    let mut s = t(key).to_string();
    for (k, v) in args {
        s = s.replace(&format!("{{{k}}}"), v);
    }
    s
}

#[allow(dead_code)]
/// Current locale id ("en", "ja").
pub fn lang() -> &'static str {
    detect_lang()
}
