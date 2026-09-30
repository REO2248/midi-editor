# midi-editor (working title)

A modern, pure-SMF MIDI file editor with an embedded MCP server — a "VSCode for MIDI".

- Modern DAW-class UX (gpui), faithful Standard MIDI File round-trip (SysEx, per-event
  channel, same-tick ordering, Shift-JIS text metas preserved byte-exact)
- Track destinations: built-in GM synth / MIDI out ports (loopMIDI, physical) / hosted VST3
- MCP-native: humans and LLM agents edit the same document through one transaction path

Status: functional. File open/save (byte-exact SMF round-trip), piano roll +
event list, note editing (draw/drag/edge-resize/marquee multi-select/Alt-drag
duplicate), undo/redo, multi-destination playback (MIDI port or hosted VST3
instrument via vst3-host + cpal, per-track routing, mute/solo), track
headers + channel, tempo/time-signature editing, quantize/transpose/velocity
ops, marker strip, MIDI-input recording, seek ruler + loop, lane editor for
velocity/CC/pitch bend, import diagnostics with one-click normalize,
text-encoding override, plugin GUI windows, per-file sidecar persistence, and
a 38-tool MCP surface — all against a real .mid document.

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
together), Alt-drag duplicates, drag a note's right edge to resize, click
selects (shift toggles), Delete removes the selection, Ctrl+Z / Ctrl+Y (or
Ctrl+Shift+Z) undo/redo, Ctrl+S saves, Ctrl+O opens, Space or the Play button
toggles playback, `loop` restarts from the position playback began. Click the
ruler above the roll to seek; the green marker tracks the playhead; the strip
under it shows markers/lyrics. The track column selects tracks and toggles
Mute/Solo, `c{n}` on a row cycles the track's output channel (writes the
FF 20 channel-prefix meta), and `rename` commits the name field. Header
chips: `bpm` tempo ±1 (Shift ±10) at the playhead, the time-signature chip
cycles 4/4→3/4→6/8→…, `quant`/`tr−`/`tr+`/`vel−`/`vel+` apply to the
selection (or whole selected track when nothing is selected) — all single
undoable transactions. `●` arms record: playback starts and MIDI input
(detected input ports) captures onto the selected track as one undoable
transaction; click again to commit. The bottom lane edits velocity — or
click its corner chip to switch to CC 1/7/10/11/64 or pitch bend
(click-drag empty space inserts a point, dragging a point edits it). The
destination button (`T<n> ▸ name`) cycles the *selected track's* output: MIDI
ports (GM synth, loopMIDI, physical interfaces) and discovered VST3 plugins;
`*` means the track inherits the default destination. When a plugin is
selected, a `GUI` button opens the plugin's own editor window. `ENC` cycles
text decoding (auto/UTF-8/SJIS/Latin-1) for track names and meta text —
`auto` uses UTF-8 → Shift-JIS → Latin-1 with the XF `FF 09` "JP" marker as a
hint. When the event list header shows `[fix]`, clicking it normalizes
imported-file issues (dangling note-ons, missing EOT, tempo outside the
conductor track) as a single undoable transaction. `met` adds a GM woodblock
click on every beat (accented downbeats) routed to a MIDI destination. Mouse
wheel scrolls, Ctrl+wheel zooms.

Destinations, mute/solo, metronome/loop state, zoom/scroll, selected track and
encoding persist per-file in `<song>.mid.editor.json` next to the .mid — the
SMF itself is never touched by editor state. VST3 patch/parameter state lives
in a companion binary file `<song>.mid.editor.state`: records are keyed by the
plugin's class uid (bundle path as a fallback), restored into each plugin
after load and before playback, and kept even while a plugin is missing —
incompatible or corrupt state files are skipped with a status note, never
fatal.

Plugin scanning is cached app-wide in
`%APPDATA%/midi-editor/plugin_scan_cache.json`: each bundle is fingerprinted
(file count + size + newest mtime inside the `.vst3`), and unchanged bundles
reuse their last probe result, so a plugin that crashed or timed out is
quarantined with its reason instead of slowing every startup. Updating the
bundle on disk invalidates the entry and triggers a fresh probe
automatically. The Output menu offers **Rescan changed plugins** (cached,
default) and **Rescan all plugins** (ignores the cache); quarantined entries
appear in the Plugin Host Status panel where a click re-probes just that
bundle. The per-plugin probe bound defaults to 10 s and can be overridden via
`probe_timeout_secs` in `prefs.json`.

VST3 destinations persist by *component identity*, not just path: alongside
the bundle path the sidecar records the plugin's class/component ID (plus
vendor/name), so a bundle that moved or was reinstalled resolves onto the
same plugin instead of losing its routing. The recorded path stays the
preferred hint; when several installs expose the same component ID the
closest one is picked deterministically and a note appears in the status
line. Sidecars written by older versions (path only) migrate automatically
on first resolve.

## MCP server

The app embeds a Streamable-HTTP MCP server at `http://127.0.0.1:7878/mcp`
while it is running. **Authentication is on by default** — every request
needs `Authorization: Bearer <token>`:

- `MIDI_MCP_TOKEN` set → that token is required.
- unset → on first launch the app generates a random 256-bit token into
  `%LOCALAPPDATA%\midi-editor\mcp-token` (a per-user private dir) and
  re-reads it per request — editing or deleting the file rotates/revokes
  the credential without restarting the app.
- `MIDI_MCP_ALLOW_INSECURE=1` → explicit opt-out: open loopback, exactly
  like the old default. The status bar shows a red `mcp: OPEN` chip.
- Auth failures are logged by client IP (never the credential) and
  throttled after 10 failures per minute.
- If the token file can't be provisioned, the MCP endpoint stays off
  rather than serving unauthenticated.

**Loopback security model.** The endpoint binds to loopback only, so remote
machines cannot reach it; the residual risks are local. A hostile web page in
the user's browser could otherwise issue cross-origin requests to the endpoint
(CSRF / DNS rebinding), so requests are rejected before tool dispatch when a
present `Host` header is not `localhost`/`127.0.0.1`/`[::1]` or a present
`Origin` header is not `http(s)://<loopback>:<any port>` — enforced both by an
explicit guard middleware and rmcp's own allowlists. Non-browser MCP clients
send no `Origin` and are unaffected; browser tooling served from localhost
(e.g. MCP Inspector) still works. The effective mode is printed on stderr at
startup and reported by the `diagnostics` tool under `security`.

Transport and tool limits keep one request from exhausting the process:
request bodies are capped at 4 MiB, at most 16 requests may be in flight
(excess → `429`), and each request has 60 s to produce a response
(exceeded → `504`) — rejections carry a JSON-RPC-shaped error body. Per
call, `apply_patch` accepts ≤ 1000 ops (each ≤ 10 000 inserted events),
read tools paginate via `limit`/`offset` (≤ 10 000 rows), `diagnostics`
caps at 500 rows with a `total` field, and hex payloads decode to at most
1 MiB.

For stdio-only clients (Claude Desktop etc.) use the bridge, which
discovers the provisioned token file automatically:

```
# proxy to the running app (default URL above; --url/--token to override)
mcp-bridge

# or standalone: edit a file headlessly over stdio
mcp-bridge --file song.mid
```

Claude Desktop `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "midi-editor": {
      "command": "C:\\path\\to\\mcp-bridge.exe"
    }
  }
}
```

**Read tools**: `editor_info` (semver, commit, MCP surface version, SMF
features, destination kinds, live feature flags, per-tool version/deprecation
table — call first for feature detection), `document_summary` (format,
tracks, notes, duration, revision, dirty flag), `list_notes`, `query_events`
(raw events incl. raw_hex), `get_tempo_map`, `get_meta` (names/markers/lyrics
decoded), `get_cc` (latest CC value per track/channel/cc), `diagnostics`
(import-quality findings), `list_midi_ports`, `list_destinations`,
`transaction_status`, `transaction_history` (bounded log of commits/undos/redos
with per-transaction change summaries), `changes_since_revision` (aggregate
diff since a revision, `truncated` when the capped history can't reach it).
`list_notes` / `query_events` / `get_meta` / `get_cc` paginate: pass `limit`
(≤10000) and feed each response's `next_cursor` back as `cursor`; cursors are
keyed to the document revision, so an edit mid-walk returns a `stale_cursor`
error with a restart hint instead of a silently wrong page. `fields` selects
row keys to keep (e.g. omit `raw_hex`/`data_hex` on bulk scans). Every
mutation reply carries a `summary` of what changed (counts by event kind,
tracks touched, tick range).

**File-write scope**: `save` with no `path` always writes the open
document. An explicit `path` is scoped — it is canonicalized (following
symlinks/junctions and `..`) and must land under the document's
directory. Extra roots are opt-in via `MIDI_MCP_ALLOWED_ROOTS`
(`;`-separated dirs) for the HTTP server; standalone stdio mode
(`mcp-bridge --file`) also allows its working directory and uses
`MIDI_MCP_STDIO_ALLOWED_ROOTS` instead.

**Edit tools** — one call = one undoable transaction, or wrap many calls in a
named checkpoint (`begin_transaction` / `commit_transaction` /
`rollback_transaction`): staged edits land on a private copy, commit folds
them into a single undo step, rollback leaves the document byte-for-byte
unchanged, and a concurrent GUI edit turns the commit into a stale-revision
conflict. `commit_transaction {dry_run:true}` validates the whole batch
without applying. `apply_patch`
(low-level ops: insert_note / insert_events / remove_events / move_note /
set_tempo; `base_revision` optimistic concurrency, `dry_run` previews without
applying), `quantize`, `transpose`, `scale_velocity`, `set_channel`,
`set_program` (bank MSB+LSB+PC), `set_cc`, `set_pitch_bend`, `set_tempo`,
`set_time_signature`, `set_track_channel`, `set_track_name`, `add_track`,
`remove_track`, `delete_range`, `duplicate_range`, `normalize` (apply
import fixes by diagnostic code), `undo`, `redo`, `save`.

**Transport**: `transport` (play/stop/seek), `set_track_destination` (route a
track to a MIDI port or hosted VST3). MCP edits repaint the GUI live.

Text metas decode by heuristic: UTF-8 → Shift-JIS → Latin-1, with the XF
`FF 09` "JP" charset marker acting as a file-wide hint. Raw bytes are never
rewritten — round-trip stays byte-exact.

## Diagnostics

Structured logs write to `%APPDATA%\midi-editor\logs\midi-editor.log.<date>`
(daily rotation, last 7 files kept, non-blocking writes so audio/playback never
stalls). Panic location + backtrace land there too. Help → **Open Logs Folder**
reveals them; Help → **Export Diagnostics Bundle** writes a single
`midi-editor-diagnostics-<ts>.txt` under `%APPDATA%\midi-editor\diagnostics\`
containing environment facts and redacted log tails — token/secret-shaped
values are masked, and no MIDI file content is ever included.

## i18n

UI strings live in `crates/app/src/i18n.rs` (English default, Japanese bundled).
`MIDI_EDITOR_LANG=ja` or a `ja*` `LANG` selects Japanese; adding a locale means
adding a table there — no string literals in UI code.
