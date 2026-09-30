#![no_main]
use libfuzzer_sys::fuzz_target;

// The tolerant recovery walker alone: corrupt lengths, hostile VLQs, and
// truncated events must never panic or allocate past the input size.
fuzz_target!(|data: &[u8]| {
    let _ = smf_core::parse_lenient(data);
});
