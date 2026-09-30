#![no_main]
use libfuzzer_sys::fuzz_target;
use smf_core::WriteOptions;

// Normalization fixpoint: parse -> write -> parse -> write must be stable.
// Byte-exact on clean input; on dirty input the first write normalizes and
// the second must produce identical bytes (both at the smf layer and the
// document layer).
fuzz_target!(|data: &[u8]| {
    if midi_editor_fuzz::hits_known_midly_panic(data) {
        return;
    }
    let Ok(f) = smf_core::parse(data) else { return };
    let opts = WriteOptions::default();
    let out1 = smf_core::write(f.format, f.division, &f.tracks, opts);
    let f2 = smf_core::parse(&out1).expect("written output must re-parse");
    let out2 = smf_core::write(f2.format, f2.division, &f2.tracks, opts);
    assert_eq!(out1, out2, "parse/write pipeline must reach a fixpoint");

    let doc = document::Document::from_file(f2);
    let d1 = doc.serialize(opts);
    let d2 = document::Document::from_file(
        smf_core::parse(&d1).expect("document serialization must re-parse"),
    )
    .serialize(opts);
    assert_eq!(d1, d2, "document serialize must reach a fixpoint");
});
