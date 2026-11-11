#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| teamspace::fuzz::loopback_callback(data));
