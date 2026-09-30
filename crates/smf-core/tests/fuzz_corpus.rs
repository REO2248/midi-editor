//! Replay the persistent fuzz corpus + any minimized crash regressions
//! through the parser invariants inside normal `cargo test` runs — CI covers
//! every committed fixture without needing cargo-fuzz installed.
//!
//! Sources walked recursively:
//!   fuzz/corpus/**      — seeds (edge fixtures + real-world files)
//!   fuzz/regressions/** — minimized crashers kept as permanent fixtures
//!
//! Invariants per file: parse never panics, writer output re-parses, and
//! write∘parse reaches a fixpoint.
use smf_core::*;

fn corpus_files() -> Vec<std::path::PathBuf> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fuzz");
    let mut files = Vec::new();
    let mut stack: Vec<std::path::PathBuf> = vec![root.join("corpus"), root.join("regressions")];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                files.push(p);
            }
        }
    }
    files.sort();
    files
}

fn check_invariants(name: &str, bytes: &[u8]) {
    // may Err, must never panic
    let Ok(f) = parse(bytes) else { return };
    let opts = WriteOptions::default();
    let out1 = write(f.format, f.division, &f.tracks, opts);
    let f2 = parse(&out1).unwrap_or_else(|e| panic!("{name}: written output must re-parse: {e}"));
    let out2 = write(f2.format, f2.division, &f2.tracks, opts);
    assert_eq!(out1, out2, "{name}: parse/write must reach a fixpoint");
}

#[test]
fn corpus_and_regressions_hold_invariants() {
    let files = corpus_files();
    assert!(
        !files.is_empty(),
        "fuzz corpus directory is missing — run `cargo run --manifest-path fuzz/Cargo.toml --bin gen_corpus`"
    );
    for path in files {
        let bytes = std::fs::read(&path).unwrap();
        check_invariants(&path.display().to_string(), &bytes);
    }
}
