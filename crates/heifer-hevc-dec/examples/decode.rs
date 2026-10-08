//! Decodes an HEVC Annex B file (`.h265`) to raw planar YUV (8-bit, or 16-bit little-endian
//! for higher bit depths), for comparison with `ffmpeg -f rawvideo`.
//!
//! Usage: cargo run --release -p heifer-hevc-dec --example decode -- <in.h265> <out.yuv> [--no-filters]

use heifer_hevc_dec::decoder::{DecodeOptions, HashCheck, decode_picture_checked};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [input, output, rest @ ..] = args.as_slice() else {
        return Err("usage: decode <in.h265> <out.yuv> [--no-filters]".into());
    };
    let options = DecodeOptions {
        skip_loop_filters: rest.iter().any(|a| a == "--no-filters"),
        verify_hash: true,
    };
    let (frame, check) = decode_picture_checked(&std::fs::read(input)?, options)?;
    let mut out = Vec::new();
    for (c, plane) in frame.planes.iter().enumerate() {
        let wide = frame.bit_depth[usize::from(c > 0)] > 8;
        for &s in plane {
            if wide {
                out.extend_from_slice(&s.to_le_bytes());
            } else {
                out.push(s as u8);
            }
        }
    }
    std::fs::write(output, out)?;
    if let HashCheck::Verified(kind) = check {
        println!("{kind} hash verified");
    }
    println!(
        "{}x{} chroma_format={} bit_depth={:?}",
        frame.widths[0], frame.heights[0], frame.chroma_format, frame.bit_depth
    );
    Ok(())
}
