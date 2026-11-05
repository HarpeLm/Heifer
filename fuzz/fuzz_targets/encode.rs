//! Encoder round trip: any small image, at any quality and in 8 or 10 bits, must encode to a
//! file that heifer
//! decodes back to an image of the same size, and to an HEVC stream that decodes.
#![no_main]
use heifer_hevc_dec::decoder::{DecodeOptions, decode_picture};
use heifer_hevc_enc::encoder::{ChromaFormat, CodingOptions, EncoderConfig, Planes16, encode_picture16_with};
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
    // 8 or 10 bits per sample.
    let (bit_depth, shift) = if flags & 4 == 0 { (8, 0) } else { (10, 2) };
    let wide = |i: usize| (u16::from(sample(i)) << shift) | u16::from(sample(i + 3) & 3) * u16::from(shift > 0);
    let y: Vec<u16> = (0..n).map(wide).collect();
    // Chroma format from flags bits 3-4: 4:2:0, 4:2:2 or 4:4:4.
    let format = match (flags >> 3) % 3 {
        0 => ChromaFormat::Chroma420,
        1 => ChromaFormat::Chroma422,
        _ => ChromaFormat::Chroma444,
    };
    let cn = match format {
        ChromaFormat::Chroma420 => w.div_ceil(2) * h.div_ceil(2),
        ChromaFormat::Chroma422 => w.div_ceil(2) * h,
        _ => w * h,
    } as usize;
    let cb: Vec<u16> = (0..cn).map(|i| wide(i * 7 + 1)).collect();
    let cr: Vec<u16> = (0..cn).map(|i| wide(i * 13 + 2)).collect();
    let colour = flags & 1 == 0;
    let planes = Planes16 { y: &y, chroma: colour.then_some((&cb[..], &cr[..])), bit_depth };
    let config = EncoderConfig { qp: qp % 52, ..EncoderConfig::default() };
    let options = CodingOptions {
        chroma_format: if colour { format } else { ChromaFormat::Monochrome },
        lossless: false,
    };
    let picture = encode_picture16_with(w, h, planes, &options, &config).expect("valid input must encode");
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
