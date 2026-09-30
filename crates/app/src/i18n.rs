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
    ("status.new_doc", "new document"),
    ("status.loaded", "loaded"),
    ("status.loaded_warn", "loaded — {n} warning(s): {w}"),
    ("status.load_failed", "load failed: {e}"),
    ("status.rec_discarded", "recording discarded"),
    ("status.apply_failed", "apply: {e}"),
    ("status.undo", "undo: {label}"),
    ("status.redo", "redo: {label}"),
    ("status.rec_armed", "rec → T{n}"),
    ("status.rec_no_events", "rec: no events"),
    ("status.rec_done", "rec: {n} events"),
    ("plugin.gui_open_failed", "plugin GUI: {e}"),
    ("events.header", "Events"),
    ("events.issues", "issues"),
    ("status.fixed", "diagnostics fixed"),
    ("tracks.header", "Tracks"),
    ("field.track_name", "Track name"),
    // event-properties inspector
    ("prop.event_title", "Event (T{t})"),
    ("prop.note_title", "Note {k}"),
    ("prop.track_title", "Track {t}"),
    ("prop.multi_title", "{n} selected"),
    ("prop.tick", "tick"),
    ("prop.channel", "channel"),
    ("prop.meta_type", "meta type"),
    ("prop.hex_data", "data (hex)"),
    ("prop.start", "start"),
    ("prop.end", "end"),
    ("prop.duration", "duration"),
    ("prop.velocity", "velocity"),
    ("prop.rel_velocity", "release vel"),
    ("prop.pb_value", "bend value"),
    ("prop.key", "key"),
    ("prop.pressure", "pressure"),
    ("prop.controller", "controller"),
    ("prop.value", "value"),
    ("prop.program", "program"),
    ("prop.d0", "data0"),
    ("prop.d1", "data1"),
    ("prop.name", "name"),
    ("prop.port", "port"),
    ("prop.count", "events"),
    ("prop.apply", "Apply"),
    ("prop.applied", "property updated"),
    ("prop.unsupported", "field not editable for this selection"),
    ("prop.raw_warn", "raw bytes: malformed data can corrupt the event"),
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
    ("transport.chase_sysex", "Chase SysEx on Play"),
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
    ("output.cat_midi", "MIDI Ports"),
    ("output.cat_vst3", "VST3 Instruments"),
    ("output.no_ports", "No MIDI output ports"),
    ("output.no_plugins", "No VST3 plugins found"),
    ("output.retry", "Retry Plugin Load"),
    ("output.host_status", "Plugin Host Status…"),
    ("output.status_title", "Plugin Host Status"),
    ("output.helper", "Isolation helper"),
    ("output.probe", "Scan probe"),
    ("output.audio", "Audio device"),
    ("output.scan", "Plugin scan"),
    ("output.missing", "missing"),
    ("output.helper_hint", "Place vst3-host-helper.exe and vst3-host-probe.exe next to midi-editor.exe"),
    ("output.probe_used", "probe: isolated"),
    ("output.probe_unused", "probe: unavailable (filename scan)"),
    ("output.close", "Close"),
    ("status.scanning", "scanning plugins…"),
    ("plugin.loading", "loading {name}…"),
    ("plugin.ready", "{name} ready"),
    ("plugin.failed", "{name} failed to load"),
    ("plugin.waiting", "waiting for {name}… playback starts when ready"),
    ("plugin.timeout", "timed out after 20 s"),
    ("plugin.gui_failed", "plugin editor failed"),
    ("plugin.state_ready", "ready"),
    ("plugin.state_loading", "loading"),
    ("plugin.state_failed", "failed"),
    ("plugin.state_idle", "not loaded"),
    ("plugin.phase_host", "host init"),
    ("plugin.phase_load", "plugin load"),
    ("plugin.phase_audio", "audio start"),
    ("status.rescan", "rescanned: {n} destination(s)"),
];

static JA: &[(&str, &str)] = &[
    ("app.title", "midi-editor"),
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
    ("status.new_doc", "新規ドキュメント"),
    ("status.loaded", "読み込みました"),
    ("status.loaded_warn", "読み込み — 警告 {n} 件: {w}"),
    ("status.load_failed", "読み込みに失敗: {e}"),
    ("status.rec_discarded", "録音を破棄しました"),
    ("status.apply_failed", "適用エラー: {e}"),
    ("status.undo", "元に戻す: {label}"),
    ("status.redo", "やり直し: {label}"),
    ("status.rec_armed", "録音 → T{n}"),
    ("status.rec_no_events", "録音: イベントなし"),
    ("status.rec_done", "録音: {n} イベント"),
    ("plugin.gui_open_failed", "プラグインGUI: {e}"),
    ("events.header", "イベント"),
    ("events.issues", "件の問題"),
    ("status.fixed", "診断を修正しました"),
    ("tracks.header", "トラック"),
    ("field.track_name", "トラック名"),
    // event-properties inspector
    ("prop.event_title", "イベント (T{t})"),
    ("prop.note_title", "ノート {k}"),
    ("prop.track_title", "トラック {t}"),
    ("prop.multi_title", "{n} 件選択中"),
    ("prop.tick", "ティック"),
    ("prop.channel", "チャンネル"),
    ("prop.meta_type", "メタイベント種別"),
    ("prop.hex_data", "データ (hex)"),
    ("prop.start", "開始"),
    ("prop.end", "終了"),
    ("prop.duration", "長さ"),
    ("prop.velocity", "ベロシティ"),
    ("prop.rel_velocity", "リリースベロシティ"),
    ("prop.pb_value", "ベンド値"),
    ("prop.key", "キー"),
    ("prop.pressure", "プレッシャー"),
    ("prop.controller", "コントローラー"),
    ("prop.value", "値"),
    ("prop.program", "プログラム"),
    ("prop.d0", "データ0"),
    ("prop.d1", "データ1"),
    ("prop.name", "名前"),
    ("prop.port", "ポート"),
    ("prop.count", "イベント数"),
    ("prop.apply", "適用"),
    ("prop.applied", "プロパティを更新しました"),
    ("prop.unsupported", "この選択では編集できないフィールドです"),
    ("prop.raw_warn", "生バイト: 不正なデータはイベントを壊す可能性があります"),
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
    ("output.cat_midi", "MIDIポート"),
    ("output.cat_vst3", "VST3インストゥルメント"),
    ("output.no_ports", "MIDI出力ポートがありません"),
    ("output.no_plugins", "VST3プラグインが見つかりません"),
    ("output.retry", "プラグインを再読み込み"),
    ("output.host_status", "プラグインホストの状態…"),
    ("output.status_title", "プラグインホストの状態"),
    ("output.helper", "分離ヘルパー"),
    ("output.probe", "スキャンプローブ"),
    ("output.audio", "オーディオデバイス"),
    ("output.scan", "プラグインスキャン"),
    ("output.missing", "見つかりません"),
    ("output.helper_hint", "vst3-host-helper.exe と vst3-host-probe.exe を midi-editor.exe と同じフォルダに置いてください"),
    ("output.probe_used", "プローブ: 分離実行"),
    ("output.probe_unused", "プローブ: 利用不可（ファイル名スキャン）"),
    ("output.close", "閉じる"),
    ("status.scanning", "プラグインをスキャン中…"),
    ("plugin.loading", "{name} を読み込み中…"),
    ("plugin.ready", "{name} 準備完了"),
    ("plugin.failed", "{name} の読み込みに失敗"),
    ("plugin.waiting", "{name} を待機中… 準備完了後に再生します"),
    ("plugin.timeout", "20秒でタイムアウト"),
    ("plugin.gui_failed", "プラグインエディタを開けません"),
    ("plugin.state_ready", "準備完了"),
    ("plugin.state_loading", "読み込み中"),
    ("plugin.state_failed", "失敗"),
    ("plugin.state_idle", "未読み込み"),
    ("plugin.phase_host", "ホスト初期化"),
    ("plugin.phase_load", "プラグイン読み込み"),
    ("plugin.phase_audio", "オーディオ開始"),
    ("status.rescan", "再スキャン: {n} 件の出力先"),
    ("edit.legato", "レガート"),
    ("edit.set_length", "長さを統一"),
    ("edit.set_velocity", "ベロシティを統一"),
    ("transport.count_in", "カウントイン録音"),
    ("transport.chase_sysex", "再生時にSysExをチェイス"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// The invariant from AGENTS.md: every key exists in BOTH tables. A key
    /// that only exists in EN silently ships English to JA users; a key only
    /// in JA is unreachable in EN and dead.
    #[test]
    fn en_ja_tables_have_identical_keys() {
        let en: HashSet<_> = EN.iter().map(|(k, _)| *k).collect();
        let ja: HashSet<_> = JA.iter().map(|(k, _)| *k).collect();
        let mut en_only: Vec<_> = en.difference(&ja).collect();
        let mut ja_only: Vec<_> = ja.difference(&en).collect();
        en_only.sort();
        ja_only.sort();
        assert!(en_only.is_empty(), "missing from JA table: {en_only:?}");
        assert!(ja_only.is_empty(), "missing from EN table: {ja_only:?}");
    }

    /// Duplicate keys silently shadow earlier entries — catch them here.
    #[test]
    fn tables_have_no_duplicate_keys() {
        for (name, table) in [("EN", EN), ("JA", JA)] {
            let set: HashSet<_> = table.iter().map(|(k, _)| *k).collect();
            assert_eq!(set.len(), table.len(), "{name} has duplicate keys");
        }
    }

    #[test]
    fn tf_substitutes_placeholders() {
        // exercise the EN path explicitly (t() depends on the test env lang)
        let s = tf("status.rec_done", &[("n", "42")]);
        assert!(s.contains("42"), "placeholder substituted: {s}");
    }
}

