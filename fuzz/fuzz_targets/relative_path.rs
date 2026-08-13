#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|path: &str| {
    let _ = studio_core::normalize_relative_path(path);
});
