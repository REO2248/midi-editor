# Upgrade / downgrade notes

How midi-editor versions its persisted state and what moving between builds
does to it. Build identity (`<semver>+<commit>[.dirty]`) comes from
`midi-editor --version`, Help > About, or MCP `serverInfo.version` — quote it
in bug reports.

## State files and their schema epochs

| File | Owner | Current `schema_version` |
| --- | --- | --- |
| `<song>.mid.editor.json` (per-song sidecar, next to the `.mid`) | `Prefs` in `crates/app/src/main.rs` | `1` |
| `%APPDATA%\midi-editor\prefs.json` (recent files, count-in, MIDI input) | `GlobalPrefs` in `crates/app/src/main.rs` | `1` |
| VST3 plugin state | the plugin itself, inside its own `.vst3` bundle | n/a — the app persists only the plugin *path* + routing |

Files written before `schema_version` existed deserialize as `0`. Both
structs load with `#[serde(default)]`, so a missing field falls back to its
default instead of failing the whole file; unknown fields are ignored on
read **but dropped on the next save** (serde reserializes only known
fields — a downgrade can silently strip fields a newer version wrote).

## Reading older state (forward direction)

- Sidecar `0` → `1`: fully transparent; `schema_version` is the only
  structural addition, all pre-existing fields keep their meaning.
- Missing `Option<>` fields (`zoom`, `sel_track`, `enc`, …) and missing
  non-`Option` collections (`track_dest`, `muted`, `soloed`) all load via
  defaults — a partial or hand-trimmed sidecar is still usable.

## Writing newer state, opening it in an older build (downgrade)

Safe in practice, with one hard edge:

- Unknown fields are ignored by the older build, then **dropped when it
  next saves the sidecar** — e.g. a `schema_version`-aware build's fields
  don't survive a round-trip through a build that predates them.
- The hard edge: if a future version adds a `Destination` variant (the
  `default_dest`/`track_dest` enum), an older build fails to deserialize
  that variant, which fails the **entire** sidecar — the song still opens,
  but view state / routing / mute-solo reset to defaults. Enum-shape changes
  are therefore a *breaking* sidecar change and must ship under a
  `schema_version` bump + a changelog `Breaking` entry.
- Downgrading past a breaking `schema_version` bump means deleting the
  sidecar to be deterministic — it lives next to the song and is safe to
  delete (only session state, never musical data; the `.mid` itself is
  always byte-exact SMF and version-agnostic).

## MCP schema

The MCP surface is the tool list + each tool's input/output shape served by
`127.0.0.1:7878/mcp` (and `mcp-bridge` for stdio clients):

- `serverInfo.version` = the same build identity string — pin automation to
  it when reproducibility matters.
- Adding tools or optional input fields is a *minor* change; removing a
  tool, renaming it, or tightening required inputs is *breaking* and goes in
  the changelog `Breaking` section.
- Removed/unknown tools fail loudly (`CallToolResult::error`), never
  silently no-op — a client built against a newer schema won't corrupt a
  document on an older server.
- One tool call = one `Document::apply` transaction = one undo step; that
  contract is stable across versions.

## Global prefs

`prefs.json` is small user data (recent list, count-in, recording input).
Same rules as sidecars: missing → defaults, unknown → dropped on save. Worst
case a downgrade forgets your recent-files list — delete the file to fully
reset.

## Release tags and artifacts

Releases are annotated git tags `v<semver>` (`git tag -a v0.1.0`). Each
release ships a versioned zip + `SHA256SUMS.txt` + `manifest.json`
(`scripts/release.ps1`); the manifest records `version`, `git_sha`,
`git_dirty`, UTC build time, and per-artifact SHA-256 + size, so a bug
report can be matched to exact bytes without guessing from commit history.
