# Working on midi-editor

This is a Rust workspace for a pure Standard MIDI File (SMF) editor with a
GPUI desktop app and an MCP server. English is the default UI language;
Japanese is supported through the same i18n table.

## Where to work

- `crates/smf-core`: parse and serialize SMF events. Preserve raw bytes,
  SysEx, event ordering, channels, and text metadata on an unedited round trip.
- `crates/document`: canonical event model, derived note/tempo views, import
  diagnostics, and transactions.
- `crates/commands`: semantic editing operations shared by the app and MCP.
- `crates/midi-io`: MIDI ports, playback scheduling, and recording.
- `crates/output`: VST3 discovery, isolated playback host, and audio output.
- `crates/app`: GPUI editor; rendering is in `render.rs`, strings in `i18n.rs`.
- `crates/mcp-server`: MCP reads, edits, transport, and stdio bridge. The
  tool surface is contract-versioned: `editor_info` reports
  `MCP_SURFACE_VERSION` plus per-tool `version`/`deprecated` metadata, and
  `tests/schema_snapshot.json` fails CI on any surface diff. On an
  intentional schema change bump the tool's `version` (and
  `MCP_SURFACE_VERSION` when breaking), then regenerate the snapshot with
  `MCP_UPDATE_SCHEMA_SNAPSHOT=1 cargo test -p mcp-server --test schema_snapshot`.

## Invariants

- Treat SMF events as the source of truth. Do not rebuild or normalize an
  imported file merely by opening and saving it.
- Route GUI and MCP edits through the document transaction path so they share
  revision checks and undo/redo. Keep editor preferences and output assignments
  in the per-file sidecar rather than writing them into the MIDI file.
- Keep VST3 playback instances on the dedicated host worker; do not load them
  synchronously on every Play or move a non-`Send` audio handle to UI state.
  A separate in-process instance may be used for the plugin editor on Windows.
- Add user-facing strings to both English and Japanese tables in `i18n.rs`.
  Keep English as the default.

## Verification

- On Windows with the MSVC toolchain, run `cargo test --workspace --locked`
  and `cargo build --workspace --locked` for code changes. The CI workflow
  runs the non-UI tests and builds the workspace on Windows.
- For SMF changes, check byte-exact unedited round trips and malformed-file
  recovery. For editing changes, check transaction and undo/redo behavior.
- For VST3 changes, check discovery, helper startup, load/ready/error states,
  and the missing-helper path. A release package needs
  `vst3-host-helper.exe` and `vst3-host-probe.exe` beside `midi-editor.exe`.
