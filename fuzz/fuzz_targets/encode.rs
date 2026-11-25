//! Encoder round trip: any small image or sequence, at any quality and in 8 or 10 bits, must
//! encode to a file that heifer decodes back to matching frames, and to an HEVC stream that
//! decodes.
#![no_main]
use heifer::{EncodeFormat, EncodeOptions, EncodeSpeed, Image, Options};
use heifer_hevc_dec::decoder::{DecodeOptions, decode_picture};
use heifer_hevc_enc::encoder::{
    ChromaFormat, CodingOptions, EncoderConfig, Planes16, SpeedPreset, encode_picture16_with,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let [w, h, qp, flags, pixels @ ..] = data else {
        return;
    };
    if pixels.is_empty() {
        return;
    }
    let (w, h) = (u32::from(w % 80) + 1, u32::from(h % 80) + 1);
    let n = (w * h) as usize;
    let sample = |i: usize| pixels[i % pixels.len()];

    // The HEVC encoder alone, with every QP, preset, lossy or lossless, in 8 or 10 bits.
    let (bit_depth, shift) = if flags & 4 == 0 { (8, 0) } else { (10, 2) };
    let wide = |i: usize| {
        (u16::from(sample(i)) << shift) | u16::from(sample(i + 3) & 3) * u16::from(shift > 0)
    };
    let y: Vec<u16> = (0..n).map(wide).collect();
    let lossless = flags & 32 != 0;
    let format = if lossless {
        ChromaFormat::Chroma444
    } else {
        match (flags >> 3) % 3 {
            0 => ChromaFormat::Chroma420,
            1 => ChromaFormat::Chroma422,
            _ => ChromaFormat::Chroma444,
        }
    };
    let cn = match format {
        ChromaFormat::Chroma420 => w.div_ceil(2) * h.div_ceil(2),
        ChromaFormat::Chroma422 => w.div_ceil(2) * h,
        _ => w * h,
    } as usize;
    let cb: Vec<u16> = (0..cn).map(|i| wide(i * 7 + 1)).collect();
    let cr: Vec<u16> = (0..cn).map(|i| wide(i * 13 + 2)).collect();
    let colour = flags & 1 == 0;
    let planes = Planes16 {
        y: &y,
        chroma: colour.then_some((&cb[..], &cr[..])),
        bit_depth,
    };
    let speed = match (flags >> 6) % 3 {
        0 => SpeedPreset::Fast,
        1 => SpeedPreset::Medium,
        _ => SpeedPreset::Slow,
    };
    let config = EncoderConfig {
        qp: qp % 52,
        speed,
        ..EncoderConfig::default()
    };
    let options = CodingOptions {
        chroma_format: if colour {
            format
        } else {
            ChromaFormat::Monochrome
        },
        lossless,
    };
    let picture =
        encode_picture16_with(w, h, planes, &options, &config).expect("valid input must encode");
    let frame = decode_picture(&picture.annex_b(), DecodeOptions::default())
        .expect("the encoder's stream must decode");
    assert_eq!(
        (frame.widths[0], frame.heights[0]),
        (picture.width, picture.height)
    );

    // The whole file, with alpha when some pixel is transparent.
    let rgba: Vec<u8> = (0..n * 4)
        .map(|i| {
            if i % 4 == 3 && flags & 2 == 0 {
                255
            } else {
                sample(i)
            }
        })
        .collect();
    let encode_opts = EncodeOptions::new()
        .with_quality(qp % 101)
        .with_max_threads(1);
    let heic = heifer::encode_rgba8(w, h, &rgba, &encode_opts).expect("valid input must encode");
    let image = heifer::decode(&heic).expect("the encoder's file must decode");
    assert_eq!((image.width, image.height), (w, h));
    assert_eq!(image.has_alpha, rgba.chunks_exact(4).any(|p| p[3] != 255));

    // Image sequence round trip: B-frames (including hierarchical), TMVP, `ctts`/`elst`, formats,
    // 8/10 bits, optional alpha track, and lossless inter.
    let (sw, sh) = (u32::from(w % 24) + 8, u32::from(h % 24) + 8);
    let frame_count = 2 + usize::from(flags & 3);
    let seq_max = (1u16 << bit_depth) - 1;
    let seq_fmt = match (flags >> 2) & 3 {
        0 => EncodeFormat::Ycbcr420,
        1 => EncodeFormat::Ycbcr422,
        2 => EncodeFormat::Ycbcr444,
        _ => EncodeFormat::Lossless,
    };
    let seq_alpha = flags & 2 != 0;
    let frames: Vec<Image> = (0..frame_count)
        .map(|f| {
            let mut img = Image::filled(sw, sh, bit_depth, [0, 0, 0, seq_max]);
            img.has_alpha = seq_alpha;
            for (p_idx, px) in img.data.chunks_exact_mut(4).enumerate() {
                let s_idx = p_idx + f * 3;
                let ch = |off: usize| {
                    let v = u16::from(sample(s_idx + off));
                    if bit_depth == 10 {
                        (v << 2) | (v >> 6)
                    } else {
                        v
                    }
                };
                px[0] = ch(0);
                px[1] = ch(1);
                px[2] = ch(2);
                px[3] = if seq_alpha { ch(3) } else { seq_max };
            }
            img
        })
        .collect();
    let durations: Vec<f64> = (0..frame_count)
        .map(|i| 0.02 + f64::from(sample(i) % 5) * 0.01)
        .collect();
    let seq_opts = EncodeOptions::new()
        .with_quality(qp % 101)
        .with_max_threads(1)
        .with_keyframe_interval(usize::from(qp % 4) + 1)
        .with_b_frames(usize::from(flags & 3))
        .with_speed(EncodeSpeed::Fast);
    let heics = heifer::encode_sequence_with_format(&frames, &durations, seq_fmt, &seq_opts)
        .expect("valid sequence must encode");
    let info = heifer::read_sequence(&heics)
        .expect("sequence header must parse")
        .expect("sequence must exist");
    assert_eq!(info.frames.len(), frame_count);

    let dec1: Vec<Image> =
        heifer::decode_sequence_with_options(&heics, &Options::new().with_max_threads(1))
            .expect("sequential decode must start")
            .collect::<Result<_, _>>()
            .expect("sequential frames must decode");
    let dec2: Vec<Image> =
        heifer::decode_sequence_with_options(&heics, &Options::new().with_max_threads(2))
            .expect("parallel decode must start")
            .collect::<Result<_, _>>()
            .expect("parallel frames must decode");
    assert_eq!(dec1.len(), frame_count);
    assert_eq!(dec1, dec2);
    let last = heifer::decode_sequence_frame_with_options(
        &heics,
        frame_count - 1,
        &Options::new().with_max_threads(1),
    )
    .expect("direct frame seek must decode");
    assert_eq!(last, dec1[frame_count - 1]);
    if seq_fmt == EncodeFormat::Lossless {
        for (orig, dec) in frames.iter().zip(&dec1) {
            assert_eq!(dec.data, orig.data, "lossless sequence frame differed");
        }
    }
});
