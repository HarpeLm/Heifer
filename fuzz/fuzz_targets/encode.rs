//! Encoder round trip: any small image, at any quality, must encode to a file that heifer
//! decodes back to an image of the same size, and to an HEVC stream that decodes.
#![no_main]
use heifer_hevc_dec::decoder::{DecodeOptions, decode_picture};
use heifer_hevc_enc::encoder::{EncoderConfig, Planes, encode_picture};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let [w, h, qp, flags, pixels @ ..] = data else { return };
    if pixels.is_empty() {
        return;
    }
    let (w, h) = (u32::from(w % 80) + 1, u32::from(h % 80) + 1);
    let n = (w * h) as usize;
    let sample = |i: usize| pixels[i % pixels.len()];

    // The HEVC encoder alone, with every QP, in colour or monochrome.
    let y: Vec<u8> = (0..n).map(sample).collect();
    let cn = (w.div_ceil(2) * h.div_ceil(2)) as usize;
    let cb: Vec<u8> = (0..cn).map(|i| sample(i * 7 + 1)).collect();
    let cr: Vec<u8> = (0..cn).map(|i| sample(i * 13 + 2)).collect();
    let planes = Planes { y: &y, chroma: (flags & 1 == 0).then_some((&cb[..], &cr[..])) };
    let config = EncoderConfig { qp: qp % 52, ..EncoderConfig::default() };
    let picture = encode_picture(w, h, planes, &config).expect("valid input must encode");
    let frame = decode_picture(&picture.annex_b(), DecodeOptions::default())
        .expect("the encoder's stream must decode");
    assert_eq!((frame.widths[0], frame.heights[0]), (picture.width, picture.height));

    // The whole file, with alpha when some pixel is transparent.
    let rgba: Vec<u8> = (0..n * 4).map(|i| if i % 4 == 3 && flags & 2 == 0 { 255 } else { sample(i) }).collect();
    let options = heifer::EncodeOptions { quality: qp % 101, max_threads: 1 };
    let heic = heifer::encode_rgba8(w, h, &rgba, &options).expect("valid input must encode");
    let image = heifer::decode(&heic).expect("the encoder's file must decode");
    assert_eq!((image.width, image.height), (w, h));
    assert_eq!(image.has_alpha, rgba.chunks_exact(4).any(|p| p[3] != 255));
});
