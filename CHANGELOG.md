# Changelog

All notable changes to midi-editor are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[semantic versioning](https://semver.org/): `MAJOR.MINOR.PATCH`, where `0.x`
means anything may change between minor releases — read
[docs/UPGRADING.md](docs/UPGRADING.md) before jumping versions.

Draft the next `Unreleased` section with `scripts/changelog.ps1` (groups
commit subjects by conventional prefix: `feat` → Added, `fix` → Fixed,
`breaking`/`!` → Breaking, everything else → Changed). At release time the
`Unreleased` heading is renamed to the new version, tagged `v<semver>`, and a
fresh empty `Unreleased` starts on top.

## [Unreleased]

## [0.1.0] - 2026-09-30

First versioned baseline. Pre-versioning history below is summarized so
upgrade notes have an anchor; `0.1.0` is the first tag in the new scheme.

### Added

- Workspace semantic version `0.1.0` (one `[workspace.package] version`
  shared by all crates) and an embedded build identity
  `<semver>+<commit>[.dirty]` produced by each reporting crate's `build.rs`
  from `git rev-parse` — no timestamps, so the string is reproducible.
- `midi-editor --version` / `mcp-bridge --version` print the identity;
  Help > About and MCP `serverInfo.version` report the same string.
- `schema_version` in the per-song `<name>.mid.editor.json` sidecar and in
  `%APPDATA%\midi-editor\prefs.json`; missing fields now deserialize via
  defaults instead of failing the whole file (`#[serde(default)]`).
- `docs/UPGRADING.md`: sidecar / global-prefs / MCP schema compatibility
  rules and downgrade risks.
- `scripts/release.ps1`: builds the release workspace and produces a
  versioned zip, `SHA256SUMS.txt`, and `manifest.json` carrying version +
  commit + dirty provenance. `scripts/changelog.ps1`: drafts changelog
  sections from `git log`.
- Baseline feature set (pre-versioning): byte-exact pure-SMF round-trip
  (SysEx, per-event channel, same-tick ordering, Shift-JIS metas); piano
  roll + event list editing (draw/drag/resize/marquee/Alt-duplicate);
  undo/redo transactions; GM synth / MIDI ports / hosted VST3 playback
  with per-track routing, mute/solo and chase incl. SysEx; tempo/time-sig,
  quantize/transpose/velocity ops; MIDI-in recording; marker strip and
  velocity/CC/pitch-bend lanes; import diagnostics + normalize; text-
  encoding override; plugin GUI windows; `.editor.json` sidecar
  persistence; embedded MCP server (`127.0.0.1:7878/mcp`) + `mcp-bridge`
  stdio frontend — one tool call = one undo step.
