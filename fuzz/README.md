# Fuzzing the SMF parser

`cargo-fuzz` targets for the hostile-input boundary: SMF parsing,
serialization, and the parse→write normalization pipeline.

## Layout

- `fuzz_targets/` — libFuzzer entry points:
  - `parse_strict` — midly-backed strict parse, no guard (panics = findings)
  - `parse_lenient` — the tolerant recovery walker alone
  - `parse_write` — guarded `parse` → `write` → re-parse → document layer
  - `write_parse` — arbitrary tracks → `write` → strict-parse + fixpoint
  - `normalize_fixpoint` — `write(parse(x))` must be a fixpoint
- `corpus/shared/` — committed seed corpus (edge-case fixtures, truncations,
  a synthesized multi-track song, plus redistributable real-world files —
  see "Corpus" below)
- `corpus/<target>/` — per-target corpora that grow as libFuzzer finds
  interesting inputs (gitignored — they're regenerable; only crashers are
  kept, under `regressions/`)
- `corpus/not_strict/` — seeds that crash the strict path only (known
  upstream bug, below); give these only to `parse_lenient`, which never
  calls midly
- `regressions/<target>/` — minimized crashers, kept forever
- `src/bin/gen_corpus.rs` — regenerates the synthetic part of `corpus/shared`
  and `corpus/not_strict`
- `src/lib.rs` — helpers shared by the targets, incl. the known-bug filter

## Known upstream panic (filtered)

midly 0.5.3 panics on an SMF division high byte of `0x80`: it negates the
SMPTE fps byte as `i8`, and `-128` overflows
(`midly/src/primitive.rs` `Timing::read`). `smf_core::parse` recovers via
`catch_unwind` + lenient in normal builds (see the
`smpte_degenerate_fps_roundtrips` test), but libFuzzer builds abort on panic,
so every target that reaches midly skips inputs hitting only this bug via
`hits_known_midly_panic()` — otherwise the fuzzer rediscovers it within
seconds and no other panic can ever be found. `parse_lenient` needs no
filter since it never reaches midly.

## Findings so far (fixed and pinned by regressions/unit tests)

- `document`: out-of-bounds index into the 128-entry note/CC tables when a
  corrupt channel data byte carried the top bit (`notes`, `chase_events`)
- `document`: `TempoMap` multiply/add overflow on u64-scale delta ticks
- `smf-core::write`: Metrical division ≥ `0x8000` emitted an SMPTE header
  (midly panics); format 0 with != 1 track emitted an unparseable file;
  channel `len` disagreeing with the status nibble desynchronized the
  stream — data-byte count is now derived from the status
- `smf-core::parse_lenient`: system bytes `> 0xF0` were treated as channel
  statuses (oscillating running-status rewrites, no fixpoint); truncated
  meta/SysEx events missing the length VLQ were re-emitted verbatim so the
  next reader consumed the following event's delta as the length

## Running

Windows/MSVC: the fuzzer binary needs the ASan runtime DLL that ships with
MSVC, so run inside a `vcvars64` environment (the VS BuildTools
`Hostx64/x64` bin dir must be on PATH — `clang_rt.asan_dynamic-x86_64.dll`
lives there):

```bat
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
cargo +nightly fuzz run parse_strict fuzz\corpus\parse_strict fuzz\corpus\shared
cargo +nightly fuzz run parse_lenient fuzz\corpus\parse_lenient fuzz\corpus\shared fuzz\corpus\not_strict
```

The first corpus dir listed is where newly discovered inputs are written, so
keep `corpus/shared` listed last to leave it pristine. Useful bounds for a
smoke run: `-- -max_total_time=60 -rss_limit_mb=2048`.

CI runs a ~45s bounded pass per target (`fuzz-smoke` job in `ci.yml`); the
`fuzz.yml` workflow runs longer sweeps weekly and on demand and uploads any
crash artifacts.

## Handling a crash

```bat
:: shrink the crasher, then keep it as a permanent regression fixture
cargo +nightly fuzz tmin <target> fuzz\artifacts\<target>\<crashfile>
copy fuzz\artifacts\<target>\<minimized> fuzz\regressions\<target>\
```

`crates/smf-core/tests/fuzz_corpus.rs` replays every file under `corpus/` and
`regressions/` in normal `cargo test`, so a copied artifact becomes an
automatic CI regression check. Fix the bug first, then commit the artifact.

`cargo +nightly fuzz cmin <target>` minimizes the coverage corpus itself.

## Corpus

Synthetic seeds are produced by `cargo run --manifest-path fuzz/Cargo.toml
--bin gen_corpus` (deterministic — do not hand-edit them). Two files come
from real-world sources that permit redistribution:

- `magenta_example.mid`, `magenta_primer.mid` — test assets from
  [magenta](https://github.com/magenta/magenta) (Apache-2.0)
