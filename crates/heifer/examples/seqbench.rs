//! Measures sequence decoding: `seqbench <file> [threads] [runs]`, prints frames per second.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let bytes = std::fs::read(&args[0])?;
    let threads: usize = args.get(1).map_or(Ok(1), |s| s.parse())?;
    let runs: usize = args.get(2).map_or(Ok(3), |s| s.parse())?;
    let options = heifer::Options::new().with_max_threads(threads);
    let mut best = f64::MAX;
    let mut n = 0;
    for _ in 0..runs {
        let start = std::time::Instant::now();
        n = 0;
        for frame in heifer::decode_sequence_with_options(&bytes, &options)? {
            std::hint::black_box(frame?);
            n += 1;
        }
        best = best.min(start.elapsed().as_secs_f64());
    }
    println!("{n} frames, {:.1} fps ({threads} threads)", n as f64 / best);
    Ok(())
}
