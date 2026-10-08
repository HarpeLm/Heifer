//! Measures decoding time (container + HEVC + RGB conversion, no file output).
//!
//! Usage: cargo run --release -p heifer --example bench -- <file.heic> [runs] [threads]
//! Prints the median time in milliseconds.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args.first().ok_or("usage: bench <file.heic> [runs] [threads]")?;
    let runs: usize = args.get(1).map_or(Ok(5), |s| s.parse())?;
    let threads: usize = args.get(2).map_or(Ok(0), |s| s.parse())?;
    let bytes = std::fs::read(path)?;
    let options = heifer::Options { max_threads: threads, ..Default::default() };
    let mut times = Vec::with_capacity(runs);
    for _ in 0..runs {
        let start = std::time::Instant::now();
        std::hint::black_box(heifer::decode_with_options(&bytes, &options)?);
        times.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(f64::total_cmp);
    println!("{:.1}", times[times.len() / 2]);
    Ok(())
}
