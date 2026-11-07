//! HEVC decoding of an arbitrary Annex B bitstream: must never panic.
#![no_main]
use heifer_hevc_dec::decoder::{DecodeOptions, decode_picture};
use heifer_hevc_dec::nal::access_units;
use heifer_hevc_dec::sequence::SequenceDecoder;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Small size limit so that each run is fast; larger pictures are rejected early.
    let options = DecodeOptions { verify_hash: true, max_pixels: 1 << 17, ..Default::default() };
    let _ = decode_picture(data, options);
    // The same bytes as a sequence: inter prediction, reference pictures, POC.
    let mut decoder = SequenceDecoder::new(options);
    for unit in access_units(data).into_iter().take(16) {
        let _ = decoder.decode(unit);
    }
});
