#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| teamspace::fuzz::stored_snapshot(data));
