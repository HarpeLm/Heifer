//! Converts a PNG or HEIC image to HEIC.
//!
//! ```text
//! cargo run --release --example to_heic -- input.png output.heic [quality]
//! ```

use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let [_, input, output, rest @ ..] = args.as_slice() else {
        eprintln!("usage: to_heic <input.png|input.heic> <output.heic> [quality 0-100]");
        std::process::exit(2);
    };
    let quality = rest.first().map_or(Ok(80), |q| q.parse())?;
    let bytes = std::fs::read(input)?;
    let image = if input.ends_with(".heic") || input.ends_with(".heif") {
        heifer::decode(&bytes)?
    } else {
        let rgba = image::load_from_memory(&bytes)?.to_rgba8();
        let (width, height) = rgba.dimensions();
        heifer::Image {
            width,
            height,
            bit_depth: 8,
            has_alpha: rgba.pixels().any(|p| p[3] != 255),
            data: rgba.into_raw().into_iter().map(u16::from).collect(),
        }
    };
    let start = Instant::now();
    let options = heifer::EncodeOptions {
        quality,
        ..Default::default()
    };
    let heic = heifer::encode(&image, &options)?;
    let elapsed = start.elapsed();
    std::fs::write(output, &heic)?;
    println!(
        "{}×{}: {} bytes ({:.2} bits/pixel) in {:.2?}",
        image.width,
        image.height,
        heic.len(),
        heic.len() as f64 * 8.0 / (f64::from(image.width) * f64::from(image.height)),
        elapsed
    );
    Ok(())
}
