#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|document: &[u8]| {
    let _ = reporch_format::parse_authoring_spec(document);
});
