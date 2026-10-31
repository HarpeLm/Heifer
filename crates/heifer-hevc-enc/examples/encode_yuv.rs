//! Encodes a raw YUV 4:2:0 picture (as written by `ffmpeg -pix_fmt yuv420p -f rawvideo`, or
//! `yuv420p10le` for 10 bits) to an HEVC Annex B stream, for rate-distortion measurements.
//!
//! ```text
//! cargo run --release -p heifer-hevc-enc --example encode_yuv -- in.yuv 1920 1080 27 out.h265 [10]
//! ```

use heifer_hevc_enc::encoder::{EncoderConfig, Planes16, encode_picture16};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let (input, width, height, qp, output, bit_depth) = match args.as_slice() {
        [_, i, w, h, q, o] => (i, w, h, q, o, 8u8),
        [_, i, w, h, q, o, b] => (i, w, h, q, o, b.parse()?),
        _ => {
            eprintln!("usage: encode_yuv <in.yuv> <width> <height> <qp> <out.h265> [bit depth]");
            std::process::exit(2);
        }
    };
    let (w, h): (usize, usize) = (width.parse()?, height.parse()?);
    let data = std::fs::read(input)?;
    // 8-bit files have one byte per sample, deeper ones two (little-endian).
    let samples: Vec<u16> = if bit_depth > 8 {
        data.chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect()
    } else {
        data.iter().map(|&v| u16::from(v)).collect()
    };
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let (y, rest) = samples.split_at(w * h);
    let (cb, rest) = rest.split_at(cw * ch);
    let config = EncoderConfig {
        qp: qp.parse()?,
        full_range: false,
        ..EncoderConfig::default()
    };
    let start = std::time::Instant::now();
    let planes = Planes16 {
        y,
        chroma: Some((cb, &rest[..cw * ch])),
        bit_depth,
    };
    let picture = encode_picture16(w as u32, h as u32, planes, &config)?;
    let elapsed = start.elapsed();
    let stream = picture.annex_b();
    std::fs::write(output, &stream)?;
    println!(
        "{} bytes in {:.0} ms",
        stream.len(),
        elapsed.as_secs_f64() * 1000.0
    );
    Ok(())
}
