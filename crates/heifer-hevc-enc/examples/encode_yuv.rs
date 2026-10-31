//! Encodes a raw 8-bit YUV 4:2:0 picture (as written by `ffmpeg -pix_fmt yuv420p -f rawvideo`)
//! to an HEVC Annex B stream, for rate-distortion measurements.
//!
//! ```text
//! cargo run --release -p heifer-hevc-enc --example encode_yuv -- in.yuv 1920 1080 27 out.h265
//! ```

use heifer_hevc_enc::encoder::{EncoderConfig, Planes, encode_picture};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let [_, input, width, height, qp, output] = args.as_slice() else {
        eprintln!("usage: encode_yuv <in.yuv> <width> <height> <qp> <out.h265>");
        std::process::exit(2);
    };
    let (w, h): (usize, usize) = (width.parse()?, height.parse()?);
    let data = std::fs::read(input)?;
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let (y, rest) = data.split_at(w * h);
    let (cb, rest) = rest.split_at(cw * ch);
    let config = EncoderConfig {
        qp: qp.parse()?,
        full_range: false,
        ..EncoderConfig::default()
    };
    let start = std::time::Instant::now();
    let planes = Planes {
        y,
        chroma: Some((cb, &rest[..cw * ch])),
    };
    let picture = encode_picture(w as u32, h as u32, planes, &config)?;
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
