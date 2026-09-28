# midi-editor (working title)

A modern, pure-SMF MIDI file editor with an embedded MCP server — a "VSCode for MIDI".

- Modern DAW-class UX (gpui), faithful Standard MIDI File round-trip (SysEx, per-event
  channel, same-tick ordering, Shift-JIS text metas preserved byte-exact)
- Track destinations: built-in GM synth / MIDI out ports (loopMIDI, physical) / hosted VST3
- MCP-native: humans and LLM agents edit the same document through one transaction path

Design docs live in `docs/research/` — start with `00-synthesis.md`.

Status: Phase 1 + early Phase 2 — file open/save, piano roll + event list,
note editing (draw/drag/edge-resize/marquee multi-select/velocity lane),
undo/redo, multi-destination playback (MIDI port or hosted VST3 instrument via
vst3-host + cpal, per-track routing), import diagnostics, and a live MCP tool
surface all work against a real .mid document.

## Build (Windows)

Requires MSVC build tools (Rust `stable-msvc`). In a shell:

```bat
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
cargo build --bin midi-editor
```

From Git Bash / PowerShell use the helper: `cmd /c C:\Users\Administrator\vcargo.cmd <cargo args>`.

Spike binaries: `cargo run --bin midi_ports` (midir port enumeration),
`cargo run --bin vst3_scan` (VST3 discovery + load attempt),
`cargo run --bin mcp_bridge` (MCP stdio server).

## Running

```
cargo run --bin midi-editor [file.mid]
```

Controls: left-click empty space draws a note (16th-note snap), empty-drag is
a marquee multi-select, drag a note to move it (all selected notes move
together), drag a note's right edge to resize, click selects (shift toggles),
Delete removes the selection, the bottom strip edits note velocity for the
current track, Ctrl+Z / Ctrl+Y (or Ctrl+Shift+Z) undo/redo, Ctrl+S saves,
Ctrl+O opens, Space or the Play button toggles playback. The destination
button (`T<n> ▸ name`) cycles the *selected track's* output: MIDI ports (GM
synth, loopMIDI, physical interfaces) and discovered VST3 plugins; `*`
means the track inherits the default destination. Mouse wheel scrolls,
Ctrl+wheel zooms.

## MCP server

The app embeds a Streamable-HTTP MCP server at `http://127.0.0.1:7878/mcp`
while it is running. Set `MIDI_MCP_TOKEN` to require `Authorization: Bearer`.

For stdio-only clients (Claude Desktop etc.) use the bridge:

```
# proxy to the running app (default URL above; --url/--token to override)
mcp-bridge

# or standalone: edit a file headlessly over stdio
mcp-bridge --file song.mid
```

Tools: `document_summary`, `list_notes`, `query_events`, `diagnostics`
(import-quality findings: dangling noteOn, zero-length notes, missing EOT,
tempo outside the conductor track), `apply_patch`
(insert_note / insert_events / remove_events / move_note / set_tempo; atomic,
one undo step, optional `base_revision` optimistic check), `undo`, `redo`,
`save`. MCP edits repaint the GUI live.

Text metas decode by heuristic: UTF-8 → Shift-JIS → Latin-1, with the XF
`FF 09` "JP" charset marker acting as a file-wide hint. Raw bytes are never
rewritten — round-trip stays byte-exact.

## i18n

UI strings live in `crates/app/src/i18n.rs` (English default, Japanese bundled).
`MIDI_EDITOR_LANG=ja` or a `ja*` `LANG` selects Japanese; adding a locale means
adding a table there — no string literals in UI code.
