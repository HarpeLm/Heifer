//! Output in display order of a raw HEVC stream with B frames.

use heifer_hevc_dec::decoder::DecodeOptions;
use heifer_hevc_dec::nal::access_units;
use heifer_hevc_dec::recon::Frame;
use heifer_hevc_dec::sequence::SequenceDecoder;

/// 24 frames by x265 (B frames, pyramid, weighted bi-prediction, keyframes at 0 and 12).
/// Frame N has luma `(X + 2Y + 3N) mod 64 + 6N`, so the mean grows with the display index.
/// The frames in display order were checked identical to ffmpeg's.
fn stream() -> Vec<u8> {
    std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/bframes.hevc"))
        .unwrap()
}

fn mean(f: &Frame) -> f64 {
    f.planes[0].iter().map(|&v| f64::from(v)).sum::<f64>() / f.planes[0].len() as f64
}

#[test]
fn raw_stream_comes_out_in_display_order() {
    let data = stream();
    let options = DecodeOptions::default();
    let mut decoder = SequenceDecoder::new(options);
    let mut shown = Vec::new();
    for unit in access_units(&data) {
        shown.extend(decoder.decode_in_display_order(unit).unwrap());
    }
    shown.extend(decoder.flush());
    assert_eq!(shown.len(), 24);
    for pair in shown.windows(2) {
        assert!(mean(&pair[1]) > mean(&pair[0]) + 3.0, "frames out of order");
    }

    // Same pictures as in decoding order, which is not the display order.
    let mut decoder = SequenceDecoder::new(options);
    let decoded: Vec<Frame> = access_units(&data)
        .into_iter()
        .map(|u| decoder.decode(u).unwrap())
        .collect();
    assert!(decoded.windows(2).any(|p| mean(&p[1]) < mean(&p[0])));
    let mut a: Vec<u64> = shown.iter().map(|f| mean(f).to_bits()).collect();
    let mut b: Vec<u64> = decoded.iter().map(|f| mean(f).to_bits()).collect();
    a.sort_unstable();
    b.sort_unstable();
    assert_eq!(a, b);
    assert!(
        decoder.flush().is_empty(),
        "decode does not keep pictures for output"
    );
}
