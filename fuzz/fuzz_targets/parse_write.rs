#![no_main]
use libfuzzer_sys::fuzz_target;
use smf_core::WriteOptions;

// Full guarded pipeline: parse -> write -> re-parse must always succeed, and
// the document layer (derived views, diagnostics, serialization) must survive
// anything the parser accepts.
fuzz_target!(|data: &[u8]| {
    if midi_editor_fuzz::hits_known_midly_panic(data) {
        return;
    }
    let Ok(f) = smf_core::parse(data) else { return };
    let opts = WriteOptions::default();
    let out = smf_core::write(f.format, f.division, &f.tracks, opts);
    let f2 = smf_core::parse(&out).expect("writer output must re-parse");
    let doc = document::Document::from_file(f2);
    let _ = doc.diagnose();
    let ser = doc.serialize(opts);
    smf_core::parse(&ser).expect("document serialization must re-parse");
});
