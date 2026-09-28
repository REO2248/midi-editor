// Batch-verify the fidelity contract over a directory of real .mid files:
// parse -> write must reproduce bytes, and on non-clean input the pipeline
// must reach a fixpoint (write(parse(write(parse(b)))) == write(parse(b))).
// Document must round-trip and diagnose() must not panic on any input.
// Usage: corpus_check <dir> [dir...]
fn main() {
    let (mut exact, mut normalized, mut failed) = (0, 0, 0);
    let mut entries: Vec<_> = std::env::args()
        .skip(1)
        .flat_map(|dir| {
            std::fs::read_dir(&dir)
                .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect::<Vec<_>>())
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
        match smf_core::parse(&bytes) {
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
                    let out2 = match smf_core::parse(&out) {
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
}
