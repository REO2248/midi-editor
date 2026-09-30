# VST3 host conformance QA

How we validate midi-editor as a VST3 *host* before releases. CI covers host-side
lifecycle/ordering rules without any third-party plugin; the manual matrix below
is what a QA run exercises against real plugins on a Windows machine with an
audio device.

## Fixtures

No plugin binaries are vendored in this repo (license + size). Install fixtures
at QA time into `%ProgramFiles%\Common Files\VST3` or `%LOCALAPPDATA%\Programs\Common\VST3`:

| Plugin | License | Role in matrix |
| --- | --- | --- |
| Surge XT | GPL-3.0 (redistributable) | primary instrument: state, GUI, arpeggiator/tempo sync |
| Dexed | GPL-3.0 (redistributable) | secondary instrument + SysEx (supports Dexed dumps) |
| Steinberg SDK samples (AGain, NoteExpressionSynth) | GPL-3.0 / proprietary SDK | spec-reference plugins: validator + editorhost conformance |
| Vital / other free binaries | free, not redistributable | broad-compat spot checks only |

Steinberg's VST3 SDK ships `validator` (validates a plugin's interface
conformance) and `editorhost`/`testhost` (reference host). Build them locally
from the SDK — the SDK license does not allow us to redistribute the binaries,
but running them in QA is fine:

```
# plugin-side sanity: fixture must pass Steinberg's own validator first,
# otherwise a failure is the plugin's, not ours
validator.exe "C:\Program Files\Common Files\VST3\AGain.vst3"
# reference-host comparison: if a fixture misbehaves only under midi-editor,
# repeat the scenario in editorhost to confirm it's our bug
```

## Smoke harnesses already in the workspace

- `cargo run -p output --bin vst3_scan` — discovery + isolated probe + offline
  `render_to_wav` (peak-checks rendered audio; needs no audio device)
- `cargo run -p output --bin vst3_live` — live audio open, note on/off,
  output-level check (needs an audio device)
- Plugin Host Status panel in the app — probe/helper/audio-device diagnostics,
  per-plugin load phase, quarantined bundles

## QA matrix

Run with both GUI (`midi-editor.exe`) and headless where noted. Record results
in the release checklist below.

| # | Area | Procedure | Expected |
| --- | --- | --- | --- |
| 1 | Scan / discovery | Fresh install → launch app | plugin appears in Output ▸ VST3 list, probe: isolated |
| 2 | Load/unload | Assign plugin to a track, play, repoint to another plugin mid-load | no crash; old instance retired (no doubled instances), new one loads |
| 3 | State capture/restore | Change preset/params → save .mid → close → reopen | same sound; `editor.state` companion written next to the sidecar |
| 4 | GUI open/close | Open plugin editor, close it, reopen, then quit app with editor open | window opens each time; app exits cleanly |
| 5 | Transport changes | Play → pause mid-note → seek → loop | notes silence on stop; no stuck/hanging notes at seek/loop boundary |
| 6 | Tempo / meter | Change BPM chip and time signature while playing | plugin tempo-synced content (arps, LFOs) follows |
| 7 | MIDI events | Play a track with notes, CC, pitch bend, program change | plugin responds to all message kinds |
| 8 | SysEx / data events | SMF with F0 events (e.g. GM reset, Dexed dump) + chase_sysex on | dump lands before same-tick notes; plugin state updated |
| 9 | Sample-rate changes | Switch audio device / device sample rate, replay | no distortion or pitch shift; stream rebuilds cleanly |
| 10 | Helper crash recovery | `taskkill /f /im vst3-host-helper.exe` while loaded | app stays up; failure surfaces in Output status; retry reloads |
| 11 | Broken plugin quarantine | Install fixture, rename its inner .vst3 binary so it crashes → rescan | quarantined with reason; later rescans skip it without delay |
| 12 | Missing plugin | Open a .mid routed to an uninstalled plugin | routing/state preserved; file/sidecar not corrupted |

## CI regression coverage (no third-party plugins required)

`cargo test --workspace --locked` runs all of these on every PR:

- `output::host_worker_request_ordering_and_exit` — host worker drains
  Open/Drop/Clear/Shutdown in order and exits cleanly
- `output::channel_event_maps_every_message_kind` — SMF→host event decode
  (notes, CC, PC, aftertouch, 14-bit pitch bend; SysEx/realtime excluded)
- `midi_io::loop_wrap_releases_notes_without_full_reset`,
  `seek_skips_events_before_start`,
  `equal_timestamps_keep_schedule_order`,
  `stop_sends_full_panic_reset` — schedule ordering and reset semantics
- `app::plugin_plan_lifecycle_ordering` — load/unload decision matrix:
  resident-plugin keepalive, in-flight dedup, failed-load stickiness,
  stale-slot retirement before re-open
- `app::plugin_state` tests — sidecar state persistence round-trip

## Release checklist

Copy per release; fill the Result column.

| Row | Scenario | Result (pass/fail + notes) | Tester + date |
| --- | --- | --- | --- |
| 1-12 | matrix above | | |
| A | `validator` clean on every fixture used | | |
| B | `vst3_scan` peak-check renders audio for Surge XT | | |
| C | `vst3_live` reports `AUDIO OK` on a real device | | |
| D | `cargo test --workspace --locked` green on the release commit | | |
