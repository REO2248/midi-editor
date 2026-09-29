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
    ("status.plugin_fail", "plugin unavailable"),
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
    ("menu.output", "Output"),
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
    // tools + snap
    ("edit.tool", "Tool"),
    ("edit.snap", "Snap"),
    ("menu.recent", "Open Recent"),
    ("menu.recent_empty", "(empty)"),
    ("edit.cut", "Cut"),
    ("edit.copy", "Copy"),
    ("edit.paste", "Paste"),
    ("edit.duplicate", "Duplicate"),
    ("edit.octave", "Transpose Octave"),
    ("edit.humanize", "Humanize"),
    ("edit.legato", "Legato"),
    ("edit.set_length", "Set Length"),
    ("edit.set_velocity", "Set Velocity"),
    ("transport.count_in", "Record Count-In"),
    ("help.shortcuts", "Keyboard Shortcuts"),
    ("status.copied", "copied {n} note(s)"),
    ("status.nosel", "no selection"),
    ("status.noclip", "clipboard is empty"),
    ("tool.select", "Select"),
    ("tool.draw", "Draw"),
    ("tool.erase", "Erase"),
    // toolbar tooltips
    ("tip.new", "New file (Ctrl+N)"),
    ("tip.open", "Open… (Ctrl+O)"),
    ("tip.save", "Save (Ctrl+S)"),
    ("tip.undo", "Undo (Ctrl+Z)"),
    ("tip.redo", "Redo (Ctrl+Y)"),
    ("tip.play", "Play / stop (Space)"),
    ("tip.stop", "Stop & return to start"),
    ("tip.rec", "Record MIDI input"),
    ("tip.loop", "Loop playback"),
    ("tip.met", "Metronome"),
    ("tip.sel", "Select tool (1)"),
    ("tip.draw", "Draw tool (2)"),
    ("tip.erase", "Erase tool (3)"),
    ("tip.snap", "Snap to grid (click to cycle)"),
    ("tip.tempo", "Tempo — click +1, right-click −1, Shift −10"),
    ("tip.quantize", "Quantize to 16th notes"),
    ("tip.trup", "Transpose +1 semitone"),
    ("tip.trdn", "Transpose -1 semitone"),
    ("tip.vel", "Velocity ×1.25  (Shift: ×0.8)"),
    ("tip.zin", "Zoom in (Ctrl+=)"),
    ("tip.zout", "Zoom out (Ctrl+-)"),
    ("tip.dest", "Output destination — opens the Output menu"),
    ("tip.gui", "Plugin GUI"),
    // Output menu
    ("output.default_dest", "Default Destination"),
    ("output.midi_in", "MIDI Input"),
    ("output.rescan", "Rescan Plugins"),
    ("output.editor_open", "Open Plugin Editor"),
    ("output.editor_close", "Close Plugin Editor"),
    ("output.first_input", "(first available)"),
    ("output.no_inputs", "(no MIDI inputs)"),
    ("status.rescan", "rescanned: {n} destination(s)"),
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
    ("status.plugin_fail", "プラグインを利用できません"),
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
    ("menu.output", "出力"),
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
    ("edit.tool", "ツール"),
    ("edit.snap", "スナップ"),
    ("menu.recent", "最近使ったファイル"),
    ("menu.recent_empty", "(なし)"),
    ("edit.cut", "切り取り"),
    ("edit.copy", "コピー"),
    ("edit.paste", "貼り付け"),
    ("edit.duplicate", "複製"),
    ("edit.octave", "オクターブ移調"),
    ("edit.humanize", "ヒューマナイズ"),
    ("output.default_dest", "デフォルト出力先"),
    ("output.midi_in", "MIDI入力"),
    ("output.rescan", "プラグインを再スキャン"),
    ("output.editor_open", "プラグインエディタを開く"),
    ("output.editor_close", "プラグインエディタを閉じる"),
    ("output.first_input", "(先頭のポート)"),
    ("output.no_inputs", "(MIDI入力なし)"),
    ("status.rescan", "再スキャン: {n} 件の出力先"),
    ("edit.legato", "レガート"),
    ("edit.set_length", "長さを統一"),
    ("edit.set_velocity", "ベロシティを統一"),
    ("transport.count_in", "カウントイン録音"),
    ("help.shortcuts", "キーボードショートカット"),
    ("status.copied", "{n}個のノートをコピー"),
    ("status.nosel", "選択がありません"),
    ("status.noclip", "クリップボードは空です"),
    ("tool.select", "選択"),
    ("tool.draw", "描画"),
    ("tool.erase", "消去"),
    ("tip.new", "新規 (Ctrl+N)"),
    ("tip.open", "開く… (Ctrl+O)"),
    ("tip.save", "保存 (Ctrl+S)"),
    ("tip.undo", "元に戻す (Ctrl+Z)"),
    ("tip.redo", "やり直し (Ctrl+Y)"),
    ("tip.play", "再生 / 停止 (Space)"),
    ("tip.stop", "停止して先頭へ"),
    ("tip.rec", "MIDI入力を録音"),
    ("tip.loop", "ループ再生"),
    ("tip.met", "メトロノーム"),
    ("tip.sel", "選択ツール (1)"),
    ("tip.draw", "描画ツール (2)"),
    ("tip.erase", "消去ツール (3)"),
    ("tip.snap", "グリッドにスナップ(クリックで切替)"),
    ("tip.tempo", "テンポ — クリック +1, 右クリック −1, Shift −10"),
    ("tip.quantize", "16分音符グリッドにクオンタイズ"),
    ("tip.trup", "半音上げ"),
    ("tip.trdn", "半音下げ"),
    ("tip.vel", "ベロシティ ×1.25 (Shift: ×0.8)"),
    ("tip.zin", "ズームイン (Ctrl+=)"),
    ("tip.zout", "ズームアウト (Ctrl+-)"),
    ("tip.dest", "選択トラックの出力先"),
    ("tip.gui", "プラグインGUI"),
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
