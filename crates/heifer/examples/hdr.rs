//! Prints the HDR headroom of a photo and statistics of its HDR rendition.
//!
//!   cargo run --release -p heifer --example hdr -- photo.heic [x,y ...]

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args.first().ok_or("usage: hdr <file.heic> [x,y ...]")?;
    let bytes = std::fs::read(path)?;
    let start = std::time::Instant::now();
    let hdr = heifer::decode_hdr(&bytes)?;
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    let peak = hdr.data.iter().copied().fold(0.0f32, f32::max);
    let above = hdr
        .data
        .chunks_exact(3)
        .filter(|p| p.iter().any(|&v| v > 1.0))
        .count();
    println!(
        "{}x{}, headroom {:.3}, peak {:.3}, {:.1}% of pixels brighter than SDR white, {ms:.0} ms",
        hdr.width,
        hdr.height,
        hdr.headroom,
        peak,
        100.0 * above as f64 / (hdr.width as f64 * hdr.height as f64)
    );
    let gain = heifer::read_gain_map(&bytes)?;
    for p in &args[1..] {
        let (x, y) = p.split_once(',').ok_or("points are x,y")?;
        let (x, y): (u32, u32) = (x.parse()?, y.parse()?);
        let i = (y * hdr.width + x) as usize * 3;
        let g = gain
            .as_ref()
            .map_or(0.0, |g| g.sample(x, y, hdr.width, hdr.height));
        println!(
            "{x},{y}: {:.4} {:.4} {:.4}  (gain {g:.3})",
            hdr.data[i],
            hdr.data[i + 1],
            hdr.data[i + 2]
        );
    }
    Ok(())
}
