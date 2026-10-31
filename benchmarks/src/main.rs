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
        _ => panic!("decode|encode"),
    }
}
