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
    ("status.no_port", "no MIDI out"),
    ("status.no_file", "untitled"),
    ("status.saved", "saved"),
    ("status.failed", "error"),
    ("events.header", "Events"),
    ("events.issues", "issues"),
    ("status.fixed", "diagnostics fixed"),
    ("tracks.header", "Tracks"),
    ("field.track_name", "Track name"),
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
    ("status.no_port", "MIDI出力なし"),
    ("status.no_file", "無題"),
    ("status.saved", "保存しました"),
    ("status.failed", "エラー"),
    ("events.header", "イベント"),
    ("events.issues", "件の問題"),
    ("status.fixed", "診断を修正しました"),
    ("tracks.header", "トラック"),
    ("field.track_name", "トラック名"),
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
