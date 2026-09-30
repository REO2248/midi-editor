# Mutation testing

This crate set uses [cargo-mutants](https://mutants.rs/) to measure whether the
test suite actually detects wrong behavior in the core edit logic — the code
paths shared by the GUI and the embedded MCP server (transactions, undo stack,
SMF write path).

## Scope

`.cargo/mutants.toml` restricts mutation to the core crates:

- `crates/document` — transaction apply/revert, undo invariants, derived views
- `crates/commands` — UndoStack, edit commands built on top of `document`
- `crates/smf-core` — parse/write/length invariants

Helper binaries (`src/bin/**`) and examples are excluded: they are developer
tools whose mutants the test suite can't observe. The app and MCP-server crates
are out of scope — mutants there are dominated by UI glue.

## Running

```sh
# whole scope, local machine (long — ~500 mutants per crate at ~15s each)
cargo mutants --output mutants.out

# one crate
cargo mutants -p document --output mutants-document
cargo mutants -p commands --output mutants-commands
cargo mutants -p smf-core --output mutants-smf-core

# only mutants touching changed code (fast PR check)
cargo mutants --in-diff origin/main...HEAD
```

Results land in `<output-dir>/mutants.out/` (`caught.txt`, `missed.txt`,
`timeout.txt`, `unviable.txt`). Per-mutant timeout defaults to 5× the baseline
test time (min 20 s); pass `--timeout <secs>` to pin it.

A weekly workflow (`.github/workflows/mutants.yml`, Wednesdays 05:00 UTC) and a
`workflow_dispatch` manual trigger run the full scope and upload `mutants.out`
as an artifact. Manual runs can be narrowed with the `packages` and `in_diff`
inputs.

## Current score

Baseline taken on the issue-52 branch (2026-09-30, cargo-mutants 27.1.0):

| crate | mutants | caught | missed | timeout | unviable | score |
|-------|--------:|-------:|-------:|--------:|---------:|------:|
| commands | 14 | 14 | 0 | 0 | 0 | **100%** |
| document | 505 | 368 | 110 | 0 | 27 | **77%** |
| smf-core | ~218 | 44 / 62 evaluated | 13 | 1 | 4 | partial |

- `commands` was a complete run.
- `document` was a complete run *before* the test strengthening described
  below (368 of 478 scored mutants killed = 77%). A follow-up run with the
  strengthened suite was stopped after 47 mutants (39 caught / 4 missed); the
  4 survivors there are now covered by `conventional_metas_are_cached`.
- `smf-core` was stopped after 62 mutants (44 caught / 13 missed / 1
  timeout); its survivors were `detect_extra_chunks` and `parse_lenient`
  header guards, now covered by `tests/extra_chunks.rs`.

(Update these rows after each run; the weekly workflow's `mutants.out`
artifact is the source of truth for full campaigns.)

## Allowlist policy

No mutants are allowlisted today. A mutant may be skipped (via `exclude_globs`
or `#[mutants::skip]`) only when it is genuinely equivalent or unreachable —
for example a `<`/`<=` distinction that no reachable input can observe. Record
the reason next to the skip.

## Known weak areas (from missed mutants)

- `Document::chase_events` / `chase_sysex` — seek-time channel-state
  reconstruction (controller chase, pedal semantics, RPN/NRPN tracking) has
  thin coverage; several comparison/guard mutants survive.
- `Document::humanize_ops` — the seeded-RNG mixing arithmetic (`^=`, `>>`,
  `%`, `+`, `*`) produces a different-but-equivalent jitter stream under
  mutation; survivors here are largely equivalent mutants, but a test that
  pins the exact deterministic sequence would tighten this.
- `TempoMap` internals — `points()`/`ppq()` accessor mutants survive; the map
  is only exercised indirectly via `tick_to_us`.
- `Document::from_file` meta guards (`name.is_none()`, `!data.is_empty()`,
  `text_encoding_hint`) — were missed before
  `conventional_metas_are_cached` was added.

## Mutants that found real bugs

Mutation-driven review plus the strengthened property tests caught two
transaction-layer defects, now fixed with regression cases:

- `revert` of an out-of-range `Op::InsertTrack` skipped the removal entirely
  (index checked against the *post-apply* length), duplicating a track on
  undo. `revert` now mirrors apply's `min(len)` clamp.
- `Op::RemoveTrack` past the end of the track list was silently ignored on
  apply but unconditionally re-inserted on revert — also duplicating a track.
  It is now rejected as `ApplyError::UnknownTrack`, matching event-op
  behavior. (`crates/document/tests/props.rs`:
  `out_of_range_track_ops_do_not_panic`)
