//! Inputs that once crashed the `hevc` fuzz target are decoded the same way, without a panic.

use heifer_hevc_dec::decoder::{DecodeOptions, decode_picture};
use heifer_hevc_dec::nal::access_units;
use heifer_hevc_dec::sequence::SequenceDecoder;

#[test]
fn hevc_fuzz_regressions_do_not_panic() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/regressions/hevc");
    let mut count = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let data = std::fs::read(entry.unwrap().path()).unwrap();
        let options = DecodeOptions::new()
            .with_verify_hash(true)
            .with_max_pixels(1 << 17);
        let _ = decode_picture(&data, options);
        let mut decoder = SequenceDecoder::new(options);
        for unit in access_units(&data).into_iter().take(16) {
            let _ = decoder.decode(unit);
        }
        count += 1;
    }
    assert!(count >= 2, "{count} inputs");
}
