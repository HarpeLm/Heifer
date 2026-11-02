//! Measures heifer and other HEIC decoders and encoders; driven by `run.py`.
//!
//! heifer-benchmarks decode <file.heic> <threads> <out_dir>   (with RAYON_NUM_THREADS=<threads>)
//! heifer-benchmarks encode <image.png> <out_dir>
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Instant;

type Decoder = Box<dyn Fn(&[u8]) -> Result<(u32, u32, Vec<u8>), String>>;

fn rgb_to_rgba(rgb: &[u8]) -> Vec<u8> {
    rgb.chunks_exact(3)
        .flat_map(|p| [p[0], p[1], p[2], 255])
        .collect()
}

/// Median of a slice of times (empty slice gives 0).
fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    s[s.len() / 2]
}

/// Smallest of a slice of times.
fn best(v: &[f64]) -> f64 {
    v.iter().fold(f64::MAX, |m, &x| m.min(x))
}

fn decoders(threads: usize) -> Vec<(&'static str, Decoder)> {
    let zune = |hw: bool| -> Decoder {
        Box::new(move |data: &[u8]| {
            use zune_heic::zune_core::bytestream::ZCursor;
            use zune_heic::zune_core::options::DecoderOptions;
            let o = DecoderOptions::default()
                .set_max_width(1 << 16)
                .set_max_height(1 << 16)
                .set_num_threads(threads as u8)
                .hvec_set_use_videotoolbox(hw);
            let mut d = zune_heic::HeifDecoder::new_with_options(ZCursor::new(data), o);
            let px = d.decode().map_err(|e| format!("{e:?}"))?;
            let (w, h) = (d.width().ok_or("no width")?, d.height().ok_or("no height")?);
            let px = if px.len() == w * h * 3 {
                rgb_to_rgba(&px)
            } else {
                px
            };
            Ok((w as u32, h as u32, px))
        })
    };
    vec![
        (
            "heifer",
            Box::new(move |data: &[u8]| {
                let o = heifer::Options {
                    max_threads: threads,
                    max_pixels: 1 << 30,
                };
                let img = heifer::decode_with_options(data, &o).map_err(|e| e.to_string())?;
                Ok((img.width, img.height, img.to_rgba8()))
            }),
        ),
        (
            "heifer8",
            Box::new(move |data: &[u8]| {
                let o = heifer::Options {
                    max_threads: threads,
                    max_pixels: 1 << 30,
                };
                let img = heifer::decode_rgba8(data, &o).map_err(|e| e.to_string())?;
                Ok((img.width, img.height, img.data))
            }),
        ),
        (
            "heic-rs",
            Box::new(|data: &[u8]| {
                let o = heic_rs::DecodeOptions {
                    layout: heic_rs::PixelLayout::Rgba8,
                    ..Default::default()
                };
                let img = heic_rs::decode(data, &o).map_err(|e| e.to_string())?;
                Ok((img.width, img.height, img.data))
            }),
        ),
        (
            "heic",
            Box::new(|data: &[u8]| {
                let out = heic::DecoderConfig::new()
                    .decode(data, heic::PixelLayout::Rgba8)
                    .map_err(|e| format!("{e:?}"))?;
                Ok((out.width, out.height, out.data))
            }),
        ),
        (
            "heic-decoder",
            Box::new(|data: &[u8]| {
                let img = heic_decoder::decode(data).map_err(|e| e.to_string())?;
                let rgb = img.to_rgb8().map_err(|e| e.to_string())?;
                Ok((img.width as u32, img.height as u32, rgb_to_rgba(&rgb.data)))
            }),
        ),
        ("zune-heic", zune(false)),
        ("zune-heic+hw", zune(true)),
    ]
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a[1].as_str() {
        "decode" => {
            let data = std::fs::read(&a[2]).unwrap();
            let threads: usize = a[3].parse().unwrap();
            let out = std::path::Path::new(&a[4]);
            std::fs::create_dir_all(out).unwrap();
            for (name, dec) in decoders(threads) {
                let mut times = Vec::new();
                let mut result = None;
                for _ in 0..5 {
                    let t = Instant::now();
                    let r = catch_unwind(AssertUnwindSafe(|| dec(&data)));
                    times.push(t.elapsed().as_secs_f64() * 1000.0);
                    match r {
                        Ok(Ok(img)) => result = Some(img),
                        Ok(Err(e)) => {
                            println!("{name}\tERR\t{e}");
                            break;
                        }
                        Err(_) => {
                            println!("{name}\tPANIC");
                            break;
                        }
                    }
                }
                if let Some((w, h, px)) = result {
                    times.sort_by(f64::total_cmp);
                    std::fs::write(out.join(format!("{name}.rgba")), &px).unwrap();
                    println!("{name}\t{:.1}\t{w}x{h}", times[times.len() / 2]);
                }
            }
        }
        "encode" => {
            let img = image::open(&a[2]).unwrap().to_rgb8();
            let (w, h) = img.dimensions();
            let out = std::path::Path::new(&a[3]);
            std::fs::create_dir_all(out).unwrap();
            for q in [40u8, 55, 70, 82, 90] {
                let t = Instant::now();
                let o = heifer::EncodeOptions {
                    quality: q,
                    max_threads: 0,
                };
                let heic = heifer::encode_rgb8(w, h, img.as_raw(), &o).unwrap();
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                let t1 = Instant::now();
                let o1 = heifer::EncodeOptions {
                    quality: q,
                    max_threads: 1,
                };
                heifer::encode_rgb8(w, h, img.as_raw(), &o1).unwrap();
                let ms1 = t1.elapsed().as_secs_f64() * 1000.0;
                let p = out.join(format!("heifer_q{q}.heic"));
                std::fs::write(&p, &heic).unwrap();
                println!(
                    "heifer\tq{q}\t{}\t{ms:.0}\t{ms1:.0}\t{}",
                    heic.len(),
                    p.display()
                );
            }
            for (ename, effort, subsample) in [
                ("still265-fast", still265::Effort::Fast, true),
                ("still265-slow", still265::Effort::Slow, true),
                ("still265-slow-444", still265::Effort::Slow, false),
            ] {
                for qp in [21u8, 25, 29, 33, 38] {
                    let t = Instant::now();
                    let mut image = still265::Image::from_rgb8(
                        img.as_raw(),
                        w,
                        h,
                        still265::ColorSpace::YCbCr,
                        false,
                        8,
                    );
                    if subsample {
                        image.subsample_to_420(1); // 4:2:0, like heifer, Apple and iPhone photos
                    }
                    let enc = still265::RustStillHevcEncoder::new(effort);
                    let r = catch_unwind(AssertUnwindSafe(|| {
                        still265::encode_heic(
                            image,
                            &enc,
                            qp,
                            still265::HeicEncodeOptions::default(),
                        )
                    }));
                    let ms = t.elapsed().as_secs_f64() * 1000.0;
                    match r {
                        Ok(Ok(heic)) => {
                            let p = out.join(format!("{ename}_qp{qp}.heic"));
                            std::fs::write(&p, &heic).unwrap();
                            println!(
                                "{ename}\tqp{qp}\t{}\t{ms:.0}\t-\t{}",
                                heic.len(),
                                p.display()
                            );
                        }
                        Ok(Err(e)) => println!("{ename}\tqp{qp}\tERR {e:?}"),
                        Err(_) => println!("{ename}\tqp{qp}\tPANIC"),
                    }
                }
            }
        }
        // Profiles one decoder in a loop (best-of-N), for sampling profilers and stable
        // timings. `heifer-benchmarks profile <decoder> <file> <iters>`. For a single-threaded
        // competitor set RAYON_NUM_THREADS=1 in the environment. Not used by run.py.
        "profile" => {
            let data = std::fs::read(&a[3]).unwrap();
            let iters: usize = a[4].parse().unwrap();
            let dec = decoders(1)
                .into_iter()
                .find(|(name, _)| *name == a[2])
                .unwrap_or_else(|| panic!("unknown decoder {}", a[2]))
                .1;
            let mut best = f64::MAX;
            let mut total = 0.0;
            for _ in 0..iters {
                let t = Instant::now();
                let img = dec(&data).unwrap();
                std::hint::black_box(img);
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                best = best.min(ms);
                total += ms;
            }
            println!(
                "{}\t{}\tbest {best:.1} ms\tmean {:.1} ms over {iters} iters",
                a[2],
                a[3],
                total / iters as f64
            );
        }
        "ab" => {
            // Intra-process interleaved A/B: alternates the two decoders every iteration and
            // flips their order every other pair, so drift and contention hit both equally.
            let data = std::fs::read(&a[4]).unwrap();
            let iters: usize = a[5].parse().unwrap();
            let all = decoders(1);
            let find = |name: &str| -> &Decoder {
                &all
                    .iter()
                    .find(|(n, _)| *n == name)
                    .unwrap_or_else(|| panic!("unknown decoder {name}"))
                    .1
            };
            let (da, db) = (find(&a[2]), find(&a[3]));
            let mut ta = Vec::with_capacity(iters);
            let mut tb = Vec::with_capacity(iters);
            for i in 0..iters {
                let run = |d: &Decoder, t: &mut Vec<f64>| {
                    let start = Instant::now();
                    let img = d(&data).unwrap();
                    std::hint::black_box(img);
                    t.push(start.elapsed().as_secs_f64() * 1000.0);
                };
                if i % 2 == 1 {
                    run(db, &mut tb);
                    run(da, &mut ta);
                } else {
                    run(da, &mut ta);
                    run(db, &mut tb);
                }
            }
            let ratio: Vec<f64> = ta.iter().zip(&tb).map(|(a, b)| a / b).collect();
            let mut rs = ratio.clone();
            rs.sort_by(f64::total_cmp);
            println!(
                "{}\t{}\t{}\tbest {:.1}\t{:.1}\tmedian {:.1}\t{:.1}\tratio(min) {:.3}\tratio(med) {:.3}",
                a[2],
                a[3],
                a[4],
                best(&ta),
                best(&tb),
                median(&ta),
                median(&tb),
                best(&ta) / best(&tb),
                rs[rs.len() / 2],
            );
        }
        _ => panic!("decode|encode|profile|ab"),
    }
}