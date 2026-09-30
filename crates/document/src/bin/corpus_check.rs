// Batch-verify the fidelity contract over .mid files.
//
// Modes:
//   corpus_check <dir> [dir...]          — legacy scan: byte-exact /
//                                          fixpoint / diagnose must not panic
//   corpus_check --verify <dir>          — validate each fixture in the dir
//                                          against its manifest.json entry
//                                          (exit 1 on any mismatch)
//   corpus_check --matrix <dir> [out.md] — same run plus an interoperability
//                                          matrix in markdown on stdout or
//                                          written to out.md
//
// The manifest is produced by `corpus_gen`; each entry pins the expected
// parse result, warnings, byte-exactness, fixpoint, track/note counts,
// diagnose() codes, chase sanity, and serialize/reopen result.

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
struct Manifest {
    fixtures: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    file: String,
    class: String,
    mirrors: String,
    expect: Expect,
}

#[derive(Deserialize)]
struct Expect {
    parse: String,
    byte_exact: bool,
    fixpoint: bool,
    warnings: Vec<String>,
    tracks: u16,
    notes: usize,
    diagnose: Vec<String>,
    chase_ok: bool,
    reopen: String,
}

/// observed behavior of one fixture — also the row emitted to the matrix
struct Obs {
    entry_file: String,
    class: String,
    mirrors: String,
    present: bool,
    parse: String, // "ok" | "error" | "missing"
    warnings: Vec<String>,
    byte_exact: Option<bool>,
    fixpoint: Option<bool>,
    tracks: Option<usize>,
    notes: Option<usize>,
    diagnose: Vec<String>,
    chase_ok: Option<bool>,
    reopen: Option<String>,
    fails: Vec<String>,
}

fn observe(path: &Path, e: &Entry) -> Obs {
    let mut o = Obs {
        entry_file: e.file.clone(),
        class: e.class.clone(),
        mirrors: e.mirrors.clone(),
        present: true,
        parse: "missing".into(),
        warnings: Vec::new(),
        byte_exact: None,
        fixpoint: None,
        tracks: None,
        notes: None,
        diagnose: Vec::new(),
        chase_ok: None,
        reopen: None,
        fails: Vec::new(),
    };
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(err) => {
            o.present = false;
            o.fails.push(format!("read: {err}"));
            return o;
        }
    };
    let file = match smf_core::parse(&bytes) {
        Ok(f) => f,
        Err(err) => {
            o.parse = "error".into();
            if e.expect.parse != "error" {
                o.fails.push(format!("parse: {err}"));
            }
            return o;
        }
    };
    o.parse = "ok".into();
    if e.expect.parse != "ok" {
        o.fails
            .push("parse succeeded but manifest expects error".into());
    }

    o.warnings = file.warnings.clone();
    if o.warnings.len() != e.expect.warnings.len()
        || !e
            .expect
            .warnings
            .iter()
            .all(|w| o.warnings.iter().any(|a| a.contains(w.as_str())))
    {
        o.fails.push(format!(
            "warnings: {:?} != expected {:?}",
            o.warnings, e.expect.warnings
        ));
    }

    let opts = smf_core::WriteOptions::default();
    let out = smf_core::write(file.format, file.division, &file.tracks, opts);
    let exact = out == bytes;
    o.byte_exact = Some(exact);
    if exact != e.expect.byte_exact {
        o.fails.push(format!("byte_exact: {exact}"));
    }
    let fixpoint = match smf_core::parse(&out) {
        Ok(f2) => smf_core::write(f2.format, f2.division, &f2.tracks, opts) == out,
        Err(err) => {
            o.fails
                .push(format!("re-parse of written bytes failed: {err}"));
            false
        }
    };
    o.fixpoint = Some(fixpoint);
    if fixpoint != e.expect.fixpoint {
        o.fails.push(format!("fixpoint: {fixpoint}"));
    }

    let doc = document::Document::from_file(file);
    let mut diags: Vec<String> = doc.diagnose().iter().map(|d| d.code.to_string()).collect();
    diags.sort();
    o.diagnose = diags.clone();
    let mut want_diags = e.expect.diagnose.clone();
    want_diags.sort();
    if diags != want_diags {
        o.fails
            .push(format!("diagnose: {diags:?} != {want_diags:?}"));
    }
    o.tracks = Some(doc.tracks.len());
    if Some(e.expect.tracks as usize) != o.tracks {
        o.fails.push(format!("tracks: {:?}", o.tracks));
    }
    let notes = doc.notes().len();
    o.notes = Some(notes);
    if notes != e.expect.notes {
        o.fails.push(format!("notes: {notes}"));
    }
    // playback sanity: chase at song start and mid-file must not panic
    let last = doc
        .tracks
        .iter()
        .flat_map(|t| t.events.iter().map(|ev| ev.tick))
        .max()
        .unwrap_or(0);
    let mid_us = doc.tempo_map.tick_to_us(last) / 2;
    let chased = !std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = doc.chase_events(0);
        let _ = doc.chase_events(mid_us);
        let _ = doc.chase_sysex(0);
    }))
    .is_err();
    o.chase_ok = Some(chased);
    if e.expect.chase_ok && !chased {
        o.fails.push("chase panicked".into());
    }
    let reopen = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        smf_core::parse(&doc.serialize(opts))
    })) {
        Ok(Ok(_)) => "ok",
        Ok(Err(err)) => {
            let _ = err;
            "error"
        }
        Err(_) => "panic",
    };
    o.reopen = Some(reopen.to_string());
    if reopen != e.expect.reopen {
        o.fails.push(format!("reopen: {reopen}"));
    }
    o
}

fn verify(dir: &Path, matrix_out: Option<&Path>) -> i32 {
    let manifest_path = dir.join("manifest.json");
    let manifest: Manifest = match std::fs::read_to_string(&manifest_path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            eprintln!("{manifest_path:?}: {e}");
            std::process::exit(2)
        }),
        Err(e) => {
            eprintln!("{manifest_path:?}: {e}");
            std::process::exit(2);
        }
    };
    let mut obs = Vec::new();
    let mut fails = 0usize;
    for e in &manifest.fixtures {
        let o = observe(&dir.join(&e.file), e);
        fails += o.fails.len();
        let status = if o.fails.is_empty() { "PASS" } else { "FAIL" };
        println!("{status} {} ({}): {}", o.entry_file, o.class, o.mirrors);
        for f in &o.fails {
            println!("      - {f}");
        }
        obs.push(o);
    }
    if let Some(out_path) = matrix_out {
        let md = render_matrix(&obs);
        std::fs::write(out_path, md).unwrap_or_else(|e| {
            eprintln!("write {}: {e}", out_path.display());
            std::process::exit(2);
        });
        println!("wrote {}", out_path.display());
    }
    println!(
        "== corpus verify: {} fixtures, {} failures",
        obs.len(),
        fails
    );
    (fails > 0) as i32
}

fn render_matrix(obs: &[Obs]) -> String {
    let mut md = String::from(
        "# SMF interoperability matrix\n\n\
         Generated by `corpus_check --matrix fixtures/corpus` from\n\
         `fixtures/corpus/manifest.json` (regenerate: `cargo run -p document --bin corpus_gen`).\n\
         Every fixture is generated in-repo and license-clean; `mirrors` names\n\
         the real-world source class each file emulates.\n\n\
         | Fixture | Class | Parse | Warnings | Byte-exact | Fixpoint | Tracks | Notes | Diagnostics | Chase | Reopen |\n\
         |---|---|---|---|---|---|---|---|---|---|---|\n",
    );
    for o in obs {
        let warns = if o.warnings.is_empty() {
            "—".into()
        } else {
            o.warnings.len().to_string()
        };
        let diags = if o.diagnose.is_empty() {
            "—".into()
        } else {
            o.diagnose.join(", ")
        };
        let cell = |v: Option<bool>| match v {
            Some(true) => "yes",
            Some(false) => "no",
            None => "—",
        };
        md.push_str(&format!(
            "| `{}` | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            o.entry_file,
            o.class,
            if o.present {
                o.parse.as_str()
            } else {
                "missing"
            },
            warns,
            cell(o.byte_exact),
            cell(o.fixpoint),
            o.tracks
                .map(|t| t.to_string())
                .unwrap_or_else(|| "—".into()),
            o.notes.map(|n| n.to_string()).unwrap_or_else(|| "—".into()),
            diags,
            cell(o.chase_ok),
            o.reopen.as_deref().unwrap_or("—"),
        ));
    }
    md.push_str(
        "\n**Warnings** counts are the parse warnings emitted for that file\n\
         (the manifest records the expected warning text). **Byte-exact** =\n\
         write(parse(file)) reproduces the original bytes; \"no\" files must still\n\
         be a **Fixpoint** (re-saving a saved file is stable). **Reopen** =\n\
         parse(serialize(Document)) through the document path.\n",
    );
    md
}

fn scan(dirs: &[String]) -> i32 {
    let (mut exact, mut normalized, mut failed) = (0, 0, 0);
    let mut entries: Vec<_> = dirs
        .iter()
        .flat_map(|dir| {
            std::fs::read_dir(dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .map(|e| e.path())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|e| {
                    eprintln!("{dir}: read_dir failed: {e}");
                    Vec::new()
                })
        })
        .filter(|p| {
            p.extension()
                .map(|e| e == "mid" || e == "smf")
                .unwrap_or(false)
        })
        .filter(|p| p.file_name().map(|n| n != "manifest.json").unwrap_or(true))
        .collect();
    entries.sort();
    for path in entries {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                println!("{name}: READ ERR {e}");
                failed += 1;
                continue;
            }
        };
        // trusted corpus input: bypass the hostile-input safety limits
        match smf_core::parse_with_limits(&bytes, &smf_core::Limits::unlimited()) {
            Ok(file) => {
                let opts = smf_core::WriteOptions::default();
                let out = smf_core::write(file.format, file.division, &file.tracks, opts);
                let warnings = file.warnings.clone();
                // document-level path must also survive
                let doc = document::Document::from_file(file);
                let diags = doc.diagnose();
                let _ = doc.serialize(smf_core::WriteOptions::default());
                if !diags.is_empty() {
                    let mut counts = std::collections::BTreeMap::new();
                    for d in &diags {
                        *counts.entry(d.code).or_insert(0u32) += 1;
                    }
                    println!("{name}: diagnostics {counts:?}");
                }
                if out == bytes {
                    exact += 1;
                } else {
                    // non-clean input is allowed to normalize — but it must be
                    // a fixpoint: writing the written file must be stable.
                    let out2 = match smf_core::parse_with_limits(&out, &smf_core::Limits::unlimited()) {
                        Ok(f2) => smf_core::write(f2.format, f2.division, &f2.tracks, opts),
                        Err(e) => {
                            failed += 1;
                            println!("{name}: re-parse failed: {e}");
                            continue;
                        }
                    };
                    if out2 == out {
                        normalized += 1;
                        println!(
                            "{name}: normalized {} -> {} bytes, warnings: {:?}",
                            bytes.len(),
                            out.len(),
                            warnings
                        );
                        continue;
                    }
                    failed += 1;
                    println!(
                        "{name}: NOT A FIXPOINT {} -> {} -> {} bytes",
                        bytes.len(),
                        out.len(),
                        out2.len()
                    );
                    continue;
                }
            }
            Err(e) => {
                failed += 1;
                println!("{name}: PARSE ERR {e}");
            }
        }
    }
    println!("== exact:{exact} normalized:{normalized} fail:{failed}");
    (failed > 0) as i32
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("--verify") => {
            let dir = args
                .get(1)
                .map(PathBuf::from)
                .unwrap_or_else(|| "fixtures/corpus".into());
            verify(&dir, None)
        }
        Some("--matrix") => {
            let dir = args
                .get(1)
                .map(PathBuf::from)
                .unwrap_or_else(|| "fixtures/corpus".into());
            if let Some(out) = args.get(2) {
                verify(&dir, Some(&PathBuf::from(out)))
            } else {
                // print matrix to stdout instead of writing a file
                let dir2 = dir.clone();
                let manifest: Manifest = serde_json::from_str(
                    &std::fs::read_to_string(dir2.join("manifest.json")).unwrap(),
                )
                .unwrap();
                let obs: Vec<Obs> = manifest
                    .fixtures
                    .iter()
                    .map(|e| observe(&dir2.join(&e.file), e))
                    .collect();
                print!("{}", render_matrix(&obs));
                obs.iter().map(|o| o.fails.len()).sum::<usize>().min(1) as i32
            }
        }
        _ => scan(&args),
    };
    std::process::exit(code);
}
