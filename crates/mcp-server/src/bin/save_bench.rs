//! Save-path benchmark for issue #8: synthetic 100k- and 1M-event SMFs,
//! reporting the editor-lock hold time of `begin_save` (clone) against
//! serialize + durable-write time — and against the old path, which held
//! the lock for the whole serialize.
//!
//! Run: `cargo run -p mcp-server --bin save_bench --release`

use std::sync::{Arc, Mutex};
use std::time::Instant;

use document::Document;
use mcp_server::service;
use mcp_server::Shared;

fn synth(events: usize) -> Document {
    // two tracks so format-1 output stays representative
    let tracks: Vec<smf_core::Track> = (0..2)
        .map(|t| smf_core::Track {
            events: (0..events / 2)
                .map(|i| smf_core::Event {
                    tick: (i as u64) * 120,
                    seq: 0,
                    raw_body: None,
                    kind: smf_core::EventKind::Channel {
                        status: 0x90 | (t as u8),
                        data: [40 + (i % 48) as u8, 96],
                        len: 2,
                    },
                })
                .collect(),
        })
        .collect();
    Document::from_file(smf_core::File {
        format: 1,
        division: smf_core::Division::Metrical(480),
        tracks,
        warnings: vec![],
    })
}

fn bench(events: usize) {
    let dir = std::env::temp_dir().join("midi-editor-save-bench");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("bench-{events}.mid"));
    let shared = Arc::new(Mutex::new(Shared::new(synth(events))));
    shared.lock().unwrap().path = Some(path.clone());

    // what the pre-split path cost the UI: serialize under the live lock
    let old_lock = {
        let t0 = Instant::now();
        {
            let sh = shared.lock().unwrap();
            let _ = sh.doc.serialize(smf_core::WriteOptions {
                running_status: false,
            });
        }
        t0.elapsed()
    };

    // begin: one short lock — path resolution + snapshot clone
    let t0 = Instant::now();
    let ticket = service::begin_save(&shared, Default::default()).unwrap();
    let begin_wall = t0.elapsed();

    // serialize the snapshot off-lock (timed separately from the write)
    let t0 = Instant::now();
    let bytes = ticket.snapshot().serialize(smf_core::WriteOptions {
        running_status: false,
    });
    let serialize = t0.elapsed();

    // finish: serialize again internally + durable write + commit
    let t0 = Instant::now();
    let out = service::finish_save(ticket).unwrap();
    let finish = t0.elapsed();

    let mib = bytes.len() as f64 / (1024.0 * 1024.0);
    println!(
        "events={events:>8}  bytes={mib:>6.1}MiB  \
         lock_held={:?}  begin_wall={:?}  serialize={:?}  finish(ser+write+commit)={:?}  \
         old_lock_held={:?}",
        out.lock_held, begin_wall, serialize, finish, old_lock
    );
}

fn main() {
    println!("midi-editor save benchmark (debug build — use --release for real numbers)");
    bench(100_000);
    bench(1_000_000);
}
