//! Full decoding of arbitrary bytes as a HEIF file: must never panic.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = heifer::decode_with_options(data, &heifer::Options { max_threads: 1, max_pixels: 1 << 17 });
});
