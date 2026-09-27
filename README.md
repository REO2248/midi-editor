# midi-editor (working title)

A modern, pure-SMF MIDI file editor with an embedded MCP server — a "VSCode for MIDI".

- Modern DAW-class UX (gpui), faithful Standard MIDI File round-trip (SysEx, per-event
  channel, same-tick ordering, Shift-JIS text metas preserved byte-exact)
- Track destinations: built-in GM synth / MIDI out ports (loopMIDI, physical) / hosted VST3
- MCP-native: humans and LLM agents edit the same document through one transaction path

Design docs live in `docs/research/` — start with `00-synthesis.md`.

Status: Phase 0 spike (toolchain + architecture verification).

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
