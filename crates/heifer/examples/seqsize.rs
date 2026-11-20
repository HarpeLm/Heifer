//! Measures sequence encoding: `seqsize <file> [quality]`, prints the original size, the size
//! heifer re-encodes it to, and the PSNR of each frame.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let original = std::fs::read(&args[0])?;
    let quality: u8 = args.get(1).map_or(Ok(80), |s| s.parse())?;
    let frames: Vec<heifer::Image> =
        heifer::decode_sequence(&original)?.collect::<Result<_, _>>()?;
    let durations: Vec<f64> = heifer::read_sequence(&original)?
        .unwrap()
        .frames
        .iter()
        .map(|f| f.duration)
        .collect();

    let options = heifer::EncodeOptions::new().with_quality(quality);
    let start = std::time::Instant::now();
    let encoded = heifer::encode_sequence(&frames, &durations, &options)?;
    let seconds = start.elapsed().as_secs_f64();

    let decoded: Vec<heifer::Image> =
        heifer::decode_sequence(&encoded)?.collect::<Result<_, _>>()?;
    let mut worst = f64::MAX;
    let mut total = 0.0;
    for (a, b) in frames.iter().zip(&decoded) {
        let mse: f64 = a
            .data
            .iter()
            .zip(&b.data)
            .map(|(&x, &y)| (f64::from(x) - f64::from(y)).powi(2))
            .sum::<f64>()
            / a.data.len() as f64;
        let psnr = 10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10();
        worst = worst.min(psnr);
        total += psnr;
    }
    let mean = total / frames.len() as f64;
    let (w, h) = (frames[0].width, frames[0].height);
    println!(
        "{w}x{h}, {} frames, quality {quality}: {} -> {} bytes ({:.2}x), PSNR mean {mean:.1} dB, worst {worst:.1} dB, {seconds:.2}s",
        frames.len(),
        original.len(),
        encoded.len(),
        encoded.len() as f64 / original.len() as f64,
    );
    Ok(())
}
