//! midi-bench — deterministic, GUI-free benchmark harness for large and
//! pathological SMF files.
//!
//! Fixtures are generated in memory (no checked-in blobs, no licensing
//! questions). Each case reports wall time (median/min over iterations) and
//! incremental peak heap (tracked global allocator). `--check` compares
//! medians against `baseline.json`, scaled by a calibration case so only
//! large regressions fail — not ordinary machine variance.
//!
//!   midi-bench [--case NAME] [--check] [--write-baseline]
//!              [--baseline PATH] [--out PATH]

use document::{Document, TempoMap, Transaction};
use mcp_server::{dispatch, Shared, SharedDoc};
use serde_json::{json, Value};
use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::hint::black_box;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

// ---------------- peak-heap accounting ----------------

static CUR_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);

struct TrackingAlloc;

unsafe impl GlobalAlloc for TrackingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let cur = CUR_BYTES.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK_BYTES.fetch_max(cur, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        unsafe { System.dealloc(p, layout) };
        CUR_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOC: TrackingAlloc = TrackingAlloc;

fn reset_peak() {
    PEAK_BYTES.store(CUR_BYTES.load(Ordering::Relaxed), Ordering::Relaxed);
}

fn peak_delta() -> u64 {
    PEAK_BYTES
        .load(Ordering::Relaxed)
        .saturating_sub(CUR_BYTES.load(Ordering::Relaxed)) as u64
}

// ---------------- fixtures (hand-built bytes, no licensing surface) ----------------

fn vlq(mut n: u64, out: &mut Vec<u8>) {
    let mut buf = [0u8; 8];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            break;
        }
    }
    let last = buf.len() - 1;
    for b in &mut buf[i..last] {
        *b |= 0x80;
    }
    out.extend_from_slice(&buf[i..]);
}

fn header(format: u16, ntrks: u16, ppq: u16) -> Vec<u8> {
    let mut v = b"MThd".to_vec();
    v.extend_from_slice(&6u32.to_be_bytes());
    v.extend_from_slice(&format.to_be_bytes());
    v.extend_from_slice(&ntrks.to_be_bytes());
    v.extend_from_slice(&ppq.to_be_bytes());
    v
}

fn mtrk(body: &[u8]) -> Vec<u8> {
    let mut v = b"MTrk".to_vec();
    v.extend_from_slice(&(body.len() as u32).to_be_bytes());
    v.extend_from_slice(body);
    v
}

fn conductor_track() -> Vec<u8> {
    let mut t = Vec::new();
    t.extend_from_slice(&[0, 0xFF, 0x51, 0x03]);
    t.extend_from_slice(&500_000u32.to_be_bytes()[1..]); // 120 bpm
    t.extend_from_slice(&[0, 0xFF, 0x58, 0x04, 4, 2, 24, 8]); // 4/4
    t.extend_from_slice(&[0, 0xFF, 0x2F, 0x00]);
    t
}

fn assemble(tracks: Vec<Vec<u8>>) -> Vec<u8> {
    let mut out = header(1, tracks.len() as u16, 480);
    for t in &tracks {
        out.extend_from_slice(&mtrk(t));
    }
    out
}

/// Format 1, `events` channel events as back-to-back 16th-note on/off pairs.
fn notes_fixture(events: usize) -> Vec<u8> {
    let mut t1 = Vec::with_capacity(events * 6);
    for i in 0..events / 2 {
        let key = 36 + (i % 52) as u8;
        vlq(120, &mut t1);
        t1.extend_from_slice(&[0x90, key, 96]);
        vlq(120, &mut t1);
        t1.extend_from_slice(&[0x80, key, 0]);
    }
    t1.extend_from_slice(&[0, 0xFF, 0x2F, 0x00]);
    assemble(vec![conductor_track(), t1])
}

/// Dense controller traffic: one CC event every 30 ticks.
fn dense_cc_fixture(events: usize) -> Vec<u8> {
    let mut t1 = Vec::with_capacity(events * 5);
    for i in 0..events {
        vlq(30, &mut t1);
        t1.extend_from_slice(&[0xB0, (i % 120) as u8, (i % 128) as u8]);
    }
    t1.extend_from_slice(&[0, 0xFF, 0x2F, 0x00]);
    assemble(vec![conductor_track(), t1])
}

/// Dense GS-style SysEx traffic: 48-byte payloads every 240 ticks.
fn dense_sysex_fixture(msgs: usize) -> Vec<u8> {
    let mut t1 = Vec::with_capacity(msgs * 56);
    for i in 0..msgs {
        vlq(240, &mut t1);
        t1.push(0xF0);
        vlq(48, &mut t1);
        for j in 0..47 {
            t1.push((0x20 + (i + j) % 0x60) as u8);
        }
        t1.push(0xF7);
    }
    t1.extend_from_slice(&[0, 0xFF, 0x2F, 0x00]);
    assemble(vec![conductor_track(), t1])
}

/// PATHOLOGICAL but valid: every event at delta 0 on tick 0 — a single-tick
/// cluster of zero-length notes. Stresses same-tick ordering, the notes()
/// LIFO pairing, and diagnose's zero-length-note emission.
fn same_tick_fixture(events: usize) -> Vec<u8> {
    let mut t1 = Vec::with_capacity(events * 5);
    for i in 0..events / 2 {
        let key = 40 + (i % 40) as u8;
        t1.extend_from_slice(&[0x00, 0x90, key, 100]);
        t1.extend_from_slice(&[0x00, 0x80, key, 0]);
    }
    t1.extend_from_slice(&[0, 0xFF, 0x2F, 0x00]);
    assemble(vec![conductor_track(), t1])
}

/// PATHOLOGICAL but valid: `events` note-ons on one key with no note-offs.
/// Worst case for chase (every pending note is re-struck) and for
/// diagnose's dangling-noteon scan.
fn dangling_fixture(events: usize) -> Vec<u8> {
    let mut t1 = Vec::with_capacity(events * 5);
    for _ in 0..events {
        t1.extend_from_slice(&[0x0A, 0x90, 60, 64]); // delta 10 ticks
    }
    t1.extend_from_slice(&[0, 0xFF, 0x2F, 0x00]);
    assemble(vec![conductor_track(), t1])
}

// ---------------- runner ----------------

struct CaseResult {
    name: &'static str,
    iters: usize,
    median_us: u64,
    min_us: u64,
    peak_bytes: u64,
    /// units produced on the last run — guards against optimized-away work
    units: u64,
}

fn timed<F: FnMut() -> u64>(name: &'static str, iters: usize, mut f: F) -> CaseResult {
    black_box(f()); // warmup / validity
    reset_peak();
    let mut times = Vec::with_capacity(iters);
    let mut units = 0;
    for _ in 0..iters {
        let t = Instant::now();
        units = f();
        black_box(units);
        times.push(t.elapsed().as_micros() as u64);
    }
    times.sort_unstable();
    CaseResult {
        name,
        iters,
        median_us: times[times.len() / 2],
        min_us: times[0],
        peak_bytes: peak_delta(),
        units,
    }
}

fn parse_doc(bytes: &[u8]) -> Document {
    Document::from_file(smf_core::parse(bytes).expect("generated fixture must parse"))
}

fn query_args(limit: usize, offset: usize) -> Value {
    json!({"from_tick": 0, "to_tick": u64::MAX, "limit": limit, "offset": offset})
}

fn shared_with(doc: Document) -> SharedDoc {
    Arc::new(Mutex::new(Shared::new(doc)))
}

/// Solo `query_events` vs the same call while 3 threads hammer the shared
/// doc — the delta is the cost of `Shared`'s lock serialization.
fn lock_contention_case() -> CaseResult {
    let bytes = notes_fixture(100_000);
    let shared = shared_with(parse_doc(&bytes));
    let args = query_args(200, 0);

    let solo = timed("query_events_contended", 7, || {
        dispatch("query_events", &args, shared.clone());
        1
    });

    let stop = Arc::new(AtomicBool::new(false));
    let workers: Vec<_> = (0..3)
        .map(|_| {
            let sh = shared.clone();
            let stop = stop.clone();
            let args = args.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    dispatch("query_events", &args, sh.clone());
                }
            })
        })
        .collect();
    let contended = timed("query_events_contended", 7, || {
        dispatch("query_events", &args, shared.clone());
        1
    });
    stop.store(true, Ordering::Relaxed);
    for w in workers {
        w.join().unwrap();
    }
    CaseResult {
        median_us: contended.median_us,
        min_us: contended.min_us,
        peak_bytes: contended.peak_bytes,
        // carry the solo/contended ratio so reports show lock impact
        units: contended.median_us.max(1) / solo.median_us.max(1),
        ..contended
    }
}

fn all_cases() -> Vec<CaseResult> {
    let mut out = Vec::new();

    // machine-speed calibration: fixed parse+serialize of a small file
    {
        let bytes = notes_fixture(10_000);
        out.push(timed("calibrate", 15, || {
            let doc = parse_doc(&bytes);
            doc.serialize(smf_core::WriteOptions {
                running_status: true,
            })
            .len() as u64
        }));
    }

    for (label, bytes) in [
        ("10k", notes_fixture(10_000)),
        ("100k", notes_fixture(100_000)),
        ("1m", notes_fixture(1_000_000)),
    ] {
        let (p_iters, l_iters) = match label {
            "1m" => (3, 2),
            "100k" => (7, 5),
            _ => (10, 7),
        };
        let name: &'static str = match label {
            "10k" => "parse_notes_10k",
            "100k" => "parse_notes_100k",
            _ => "parse_notes_1m",
        };
        out.push(timed(name, p_iters, || {
            smf_core::parse(&bytes).unwrap().tracks.len() as u64
        }));
        let name: &'static str = match label {
            "100k" => "load_notes_100k",
            "1m" => "load_notes_1m",
            _ => "load_notes_10k",
        };
        out.push(timed(name, l_iters, || {
            parse_doc(&bytes)
                .tracks
                .iter()
                .map(|t| t.events.len())
                .sum::<usize>() as u64
        }));
    }

    // per-op cases on the 100k / 1m notes fixtures
    let doc_100k = parse_doc(&notes_fixture(100_000));
    let doc_1m = parse_doc(&notes_fixture(1_000_000));

    out.push(timed("tempo_map_1m", 3, || {
        TempoMap::build(&doc_1m.tracks, doc_1m.division)
            .points()
            .len() as u64
    }));
    out.push(timed("notes_derived_100k", 5, || {
        doc_100k.notes().len() as u64
    }));
    out.push(timed("notes_derived_1m", 2, || doc_1m.notes().len() as u64));
    out.push(timed("diagnose_100k", 3, || {
        doc_100k.diagnose().len() as u64
    }));
    out.push(timed("diagnose_1m", 2, || doc_1m.diagnose().len() as u64));

    let mid_us_100k = {
        let last = doc_100k
            .tracks
            .iter()
            .flat_map(|t| t.events.iter().map(|e| e.tick))
            .max()
            .unwrap_or(0);
        doc_100k.tempo_map.tick_to_us(last / 2)
    };
    out.push(timed("chase_100k_mid", 3, || {
        doc_100k.chase_events(mid_us_100k).len() as u64
    }));

    out.push(timed("serialize_100k", 5, || {
        doc_100k
            .serialize(smf_core::WriteOptions {
                running_status: true,
            })
            .len() as u64
    }));
    out.push(timed("serialize_1m", 2, || {
        doc_1m
            .serialize(smf_core::WriteOptions {
                running_status: true,
            })
            .len() as u64
    }));
    // the UI's render-data prep path
    out.push(timed("timeline_tagged_1m", 2, || {
        doc_1m.timeline_tagged().len() as u64
    }));

    // dense fixtures
    let cc_bytes = dense_cc_fixture(100_000);
    out.push(timed("parse_dense_cc_100k", 5, || {
        smf_core::parse(&cc_bytes).unwrap().tracks.len() as u64
    }));
    let sx_bytes = dense_sysex_fixture(50_000);
    out.push(timed("parse_dense_sysex_50k", 5, || {
        smf_core::parse(&sx_bytes).unwrap().tracks.len() as u64
    }));
    let doc_sx = parse_doc(&sx_bytes);
    out.push(timed("timeline_sysex_50k", 3, || {
        doc_sx.timeline_sysex().len() as u64
    }));

    // pathological fixtures
    let tick_bytes = same_tick_fixture(100_000);
    out.push(timed("parse_same_tick_100k", 3, || {
        smf_core::parse(&tick_bytes).unwrap().tracks.len() as u64
    }));
    let doc_tick = parse_doc(&tick_bytes);
    out.push(timed("notes_same_tick_100k", 3, || {
        doc_tick.notes().len() as u64
    }));
    out.push(timed("diagnose_same_tick_100k", 3, || {
        doc_tick.diagnose().len() as u64
    }));
    out.push(timed("chase_same_tick_100k", 3, || {
        doc_tick.chase_events(1).len() as u64
    }));

    let dang_bytes = dangling_fixture(100_000);
    out.push(timed("parse_dangling_100k", 3, || {
        smf_core::parse(&dang_bytes).unwrap().tracks.len() as u64
    }));
    let doc_dang = parse_doc(&dang_bytes);
    let last_tick = doc_dang
        .tracks
        .iter()
        .flat_map(|t| t.events.iter().map(|e| e.tick))
        .max()
        .unwrap_or(0);
    let end_us = doc_dang.tempo_map.tick_to_us(last_tick) + 1_000;
    out.push(timed("chase_dangling_100k", 2, || {
        doc_dang.chase_events(end_us).len() as u64
    }));
    out.push(timed("diagnose_dangling_100k", 3, || {
        doc_dang.diagnose().len() as u64
    }));

    // region ops + transaction apply on the 100k doc
    {
        let mut d = parse_doc(&notes_fixture(100_000));
        out.push(timed("quantize_ops_100k", 3, || {
            d.quantize_ops(1, 0, u64::MAX, 120, 50, None).len() as u64
        }));
        let ops = d.delete_range_ops(1, 0, 480 * 4);
        let mut tx = Transaction {
            label: "bench delete".into(),
            base: d.revision(),
            ops,
        };
        out.push(timed("apply_delete_4bars_100k", 7, || {
            tx.base = d.revision();
            d.apply(tx.clone()).unwrap();
            let rev = d.revision();
            d.revert(&tx); // untimed — restores state for the next iter
            rev
        }));
    }

    // MCP tool path: dispatch holds Shared's lock for the whole call, so
    // these medians are also the practical lock-hold times.
    let shared_1m = shared_with(doc_1m);
    let resp_len = |r| match r {
        rmcp::model::CallToolResponse::Complete(r) => r.content.len() as u64,
        _ => u64::MAX,
    };
    out.push(timed("query_events_1m_window", 3, || {
        resp_len(dispatch(
            "query_events",
            &query_args(500, 0),
            shared_1m.clone(),
        ))
    }));
    out.push(timed("query_events_1m_deep_page", 3, || {
        resp_len(dispatch(
            "query_events",
            &query_args(100, 990_000),
            shared_1m.clone(),
        ))
    }));
    {
        let shared_100k = shared_with(doc_100k);
        // track 0 holds 3 conductor metas (ids 1-3), so content-track
        // note-on ids start at 4 and step by 2 (on, off, on, off, ...)
        let ops_json: Vec<Value> = (0..50)
            .map(|i| json!({"op": "move_note", "on_id": 4 + i * 2, "dtick": 240, "dkey": 0}))
            .collect();
        let apply_args = json!({"label": "bench move", "ops": ops_json});
        out.push(timed("apply_patch_50moves_100k", 5, || {
            match dispatch("apply_patch", &apply_args, shared_100k.clone()) {
                rmcp::model::CallToolResponse::Complete(r) => r.is_error.unwrap_or(false) as u64,
                _ => u64::MAX,
            }
        }));
    }

    out.push(lock_contention_case());
    out
}

// ---------------- baseline / budget check ----------------

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Baseline {
    /// per-case baseline median wall time, microseconds
    cases: HashMap<String, u64>,
    /// calibration-case median: machine-speed normalizer
    calibrate_us: u64,
}

fn default_baseline_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("baseline.json")
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut check = false;
    let mut write_base = false;
    let mut baseline_path = default_baseline_path();
    let mut out_path = None;
    let mut only: Option<String> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--check" => check = true,
            "--write-baseline" => write_base = true,
            "--baseline" => baseline_path = args.next().unwrap().into(),
            "--out" => out_path = Some(args.next().unwrap()),
            "--case" => only = Some(args.next().unwrap()),
            _ => panic!("unknown flag {a}"),
        }
    }

    let results: Vec<CaseResult> = all_cases()
        .into_iter()
        .filter(|c| only.as_deref().is_none_or(|o| c.name == o))
        .collect();

    // ---- report ----
    println!(
        "{:<28} {:>10} {:>10} {:>6} {:>12} units",
        "case", "median", "min", "iters", "peak+delta"
    );
    for r in &results {
        println!(
            "{:<28} {:>8.1}ms {:>8.1}ms {:>6} {:>10.1}MB {}",
            r.name,
            r.median_us as f64 / 1000.0,
            r.min_us as f64 / 1000.0,
            r.iters,
            r.peak_bytes as f64 / 1_048_576.0,
            r.units
        );
    }

    // ---- machine-speed normalization ----
    let calib = results
        .iter()
        .find(|r| r.name == "calibrate")
        .map(|r| r.median_us)
        .unwrap_or(0);

    if write_base {
        let base = Baseline {
            cases: results
                .iter()
                .map(|r| (r.name.into(), r.median_us))
                .collect(),
            calibrate_us: calib,
        };
        std::fs::write(&baseline_path, serde_json::to_string_pretty(&base).unwrap()).unwrap();
        println!("wrote baseline to {}", baseline_path.display());
    }

    let mut failed = false;
    let mut check_rows: Vec<Value> = Vec::new();
    if check {
        let raw = std::fs::read_to_string(&baseline_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", baseline_path.display()));
        let base: Baseline = serde_json::from_str(&raw).unwrap();
        // slower machine -> ratio > 1 -> looser budgets; faster machine
        // clamps at 0.5 so budgets never tighten below half the baseline
        let ratio = (calib as f64 / base.calibrate_us.max(1) as f64).clamp(0.5, 4.0);
        println!("calibration ratio: {ratio:.2}x baseline machine");
        for r in &results {
            if r.name == "calibrate" {
                continue;
            }
            let Some(b) = base.cases.get(r.name) else {
                println!("{:>28}  NO BASELINE", r.name);
                continue;
            };
            let budget = (*b as f64) * 3.0 * ratio;
            let ok = (r.median_us as f64) <= budget;
            if !ok {
                failed = true;
            }
            println!(
                "{:>28}  {:>8.1}ms vs budget {:>8.1}ms  {}",
                r.name,
                r.median_us as f64 / 1000.0,
                budget / 1000.0,
                if ok { "ok" } else { "REGRESSION" }
            );
            check_rows.push(json!({
                "case": r.name, "median_us": r.median_us,
                "budget_us": budget as u64, "ok": ok,
            }));
        }
        println!("== bench check: {}", if failed { "FAIL" } else { "PASS" });
    }

    if let Some(p) = out_path {
        let report = json!({
            "calibrate_us": calib,
            "cases": results.iter().map(|r| json!({
                "name": r.name, "median_us": r.median_us, "min_us": r.min_us,
                "iters": r.iters, "peak_delta_bytes": r.peak_bytes, "units": r.units,
            })).collect::<Vec<_>>(),
            "check": check_rows,
        });
        if let Some(dir) = std::path::Path::new(&p).parent() {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(&p, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    }
    if failed {
        std::process::exit(1);
    }
}
