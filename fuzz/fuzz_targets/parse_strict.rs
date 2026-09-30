#![no_main]
use libfuzzer_sys::fuzz_target;

// The strict midly-backed path with no lenient fallback and no catch_unwind:
// any panic or unbounded allocation here is a real finding. Errors are fine.
// Inputs tripping the known midly fps=-128 panic are skipped (see lib.rs).
fuzz_target!(|data: &[u8]| {
    if midi_editor_fuzz::hits_known_midly_panic(data) {
        return;
    }
    let _ = smf_core::parse_strict(data);
});
