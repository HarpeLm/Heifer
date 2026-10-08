//! HEVC decoding of an arbitrary Annex B bitstream: must never panic.
#![no_main]
use heifer_hevc_dec::decoder::{DecodeOptions, decode_picture};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Small size limit so that each run is fast; larger pictures are rejected early.
    let options = DecodeOptions { verify_hash: true, max_pixels: 1 << 17, ..Default::default() };
    let _ = decode_picture(data, options);
});
