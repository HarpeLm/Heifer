//! Image sequences: track structure and keyframe decoding on the Nokia samples.

fn fixture(name: &str) -> Option<Vec<u8>> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name);
    std::fs::read(path).ok()
}

#[test]
fn sequences_describe_and_decode_keyframes() {
    // (file, frames, keyframes, width, height, seconds)
    let cases = [
        ("candle_animation.heic", 120, vec![0, 60], 256, 144, 4.8),
        (
            "sea1_animation.heic",
            120,
            (0..8).map(|i| i * 16).collect(),
            256,
            144,
            4.8,
        ),
        ("rally_burst.heic", 60, vec![0], 640, 360, 2.4),
    ];
    for (name, frames, keyframes, width, height, seconds) in cases {
        let Some(bytes) = fixture(name) else {
            eprintln!("skipping {name}: run scripts/fetch-fixtures.sh");
            continue;
        };
        let seq = heifer::read_sequence(&bytes).unwrap().expect(name);
        assert_eq!(seq.frames.len(), frames, "{name}");
        assert_eq!((seq.width, seq.height), (width, height), "{name}");
        assert!((seq.duration() - seconds).abs() < 1e-9, "{name}");
        let keys: Vec<usize> = (0..frames).filter(|&i| seq.frames[i].keyframe).collect();
        assert_eq!(keys, keyframes, "{name}");
        for &k in &keys {
            let image = heifer::decode_sequence_frame(&bytes, k).unwrap();
            assert_eq!(
                (image.width, image.height),
                (width, height),
                "{name} frame {k}"
            );
            // Real pictures, not a flat fill.
            let first = &image.data[..4];
            assert!(
                image.data.chunks_exact(4).any(|p| p != first),
                "{name} frame {k}"
            );
        }
        // Every frame decodes in order, and decoding one frame alone (from its keyframe)
        // gives the same image.
        let all: Vec<heifer::Image> = heifer::decode_sequence(&bytes)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(all.len(), frames, "{name}");
        for (i, image) in all.iter().enumerate() {
            assert_eq!(
                (image.width, image.height),
                (width, height),
                "{name} frame {i}"
            );
        }
        for i in [1, frames / 2 + 1, frames - 1] {
            let single = heifer::decode_sequence_frame(&bytes, i).unwrap();
            assert_eq!(single, all[i], "{name} frame {i}");
        }
        // The sequence moves: some predicted frame differs from the first one.
        assert!(all[1..].iter().any(|f| f != &all[0]), "{name}");
        assert!(heifer::decode_sequence_frame(&bytes, frames).is_err());
    }
}

#[test]
fn still_images_have_no_sequence() {
    let rgba = vec![90u8; 32 * 32 * 4];
    let bytes = heifer::encode_rgba8(32, 32, &rgba, &Default::default()).unwrap();
    assert_eq!(heifer::read_sequence(&bytes).unwrap(), None);
    assert!(heifer::decode_sequence_frame(&bytes, 0).is_err());
}

#[test]
fn damaged_sequences_return_errors() {
    let Some(bytes) = fixture("candle_animation.heic") else {
        return;
    };
    // Truncated files and flipped bytes in the track boxes: errors, never panics.
    for end in (0..4000.min(bytes.len())).step_by(3) {
        let _ = heifer::read_sequence(&bytes[..end]);
        let _ = heifer::decode_sequence_frame(&bytes[..end], 0);
    }
    for at in 0..1900.min(bytes.len()) {
        let mut damaged = bytes.clone();
        damaged[at] ^= 0xA5;
        let _ = heifer::read_sequence(&damaged);
    }
}

#[test]
fn b_frames_come_out_in_display_order() {
    // 24 frames by x265 with B frames (pyramid, weighted bi-prediction), keyframes at 0 and
    // 12, stored out of display order. Frame N has luma `(X + 2Y + 3N) mod 64 + 6N`, so
    // the mean brightness grows with the display index. The frames were checked identical
    // to ffmpeg's.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/bframes.mp4");
    let bytes = std::fs::read(path).unwrap();
    let seq = heifer::read_sequence(&bytes).unwrap().unwrap();
    assert_eq!(seq.frames.len(), 24);
    assert!(seq.frames.iter().all(|f| (f.duration - 0.04).abs() < 1e-9));
    let keys: Vec<usize> = (0..24).filter(|&i| seq.frames[i].keyframe).collect();
    assert_eq!(keys, [0, 12]);

    let mean = |image: &heifer::Image| {
        let sum: u64 = image.data.chunks_exact(4).map(|p| u64::from(p[1])).sum();
        sum as f64 / (image.data.len() / 4) as f64
    };
    let all: Vec<heifer::Image> = heifer::decode_sequence(&bytes)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(all.len(), 24);
    for pair in all.windows(2) {
        assert!(mean(&pair[1]) > mean(&pair[0]) + 3.0, "frames out of order");
    }
    // Decoding one frame alone gives the same image as decoding them all.
    for k in [0, 3, 7, 12, 15, 23] {
        let one = heifer::decode_sequence_frame(&bytes, k).unwrap();
        assert!(one.data == all[k].data, "frame {k}");
    }
    assert!(heifer::decode_sequence_frame(&bytes, 24).is_err());
}

#[test]
fn parallel_decoding_gives_the_sequential_frames() {
    // Groups of pictures decoded on several threads must give exactly the frames of one
    // thread, in the same (display) order.
    let mut files = vec![(
        "bframes.mp4",
        std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/bframes.mp4"),
        )
        .unwrap(),
    )];
    for name in [
        "sea1_animation.heic",
        "candle_animation.heic",
        "rally_burst.heic",
    ] {
        if let Some(bytes) = fixture(name) {
            files.push((name, bytes));
        }
    }
    for (name, bytes) in &files {
        let decode = |threads| -> Vec<heifer::Image> {
            let options = heifer::Options::new().with_max_threads(threads);
            heifer::decode_sequence_with_options(bytes, &options)
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        let one = decode(1);
        for threads in [2, 3, 8] {
            let many = decode(threads);
            assert_eq!(many.len(), one.len(), "{name}, {threads} threads");
            assert!(many == one, "{name}: frames differ with {threads} threads");
        }
    }
}

#[test]
fn parallel_decoding_stops_at_the_same_error() {
    // Garbage in a frame of the second group of pictures: both ways stop with an error after
    // the same frames, and parallel decoding neither hangs nor panics.
    let mut bytes = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/bframes.mp4"),
    )
    .unwrap();
    let track = heifer::heifer_isobmff::track::parse_tracks(&bytes)
        .unwrap()
        .remove(0);
    let s = track.samples[14];
    let at = s.offset as usize + 8;
    for b in &mut bytes[at..at + 40] {
        *b ^= 0x5A;
    }
    let decode = |threads| -> (Vec<heifer::Image>, bool) {
        let options = heifer::Options::new().with_max_threads(threads);
        let mut ok = Vec::new();
        for frame in heifer::decode_sequence_with_options(&bytes, &options).unwrap() {
            match frame {
                Ok(image) => ok.push(image),
                Err(_) => return (ok, true),
            }
        }
        (ok, false)
    };
    let (one, one_failed) = decode(1);
    let (many, many_failed) = decode(4);
    assert_eq!(one_failed, many_failed);
    assert_eq!(one.len(), many.len());
    assert!(one == many);
    eprintln!("{} frames before the error, error: {one_failed}", one.len());
}

#[test]
fn sync_samples_that_are_not_random_access_pictures_decode_the_same_in_parallel() {
    // Fuzzing inputs whose container flags as sync samples pictures that are not random access
    // points: a fresh decoder, as a group thread uses, refuses to start on them while the
    // sequential decoder carries on. Such sequences must be decoded on one thread, the same
    // way whatever the thread count.
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/regressions/parallel");
    for name in [
        "22cbc4aa0f870c2bcfabeb6d0b5b4ef74756fdc7",
        "e7181f7d8fcd9f753cf5f2e62ed013d6432e4a5b",
        "38d51f649602c8461109314e6fa58e0eba4aca19",
    ] {
        let bytes = std::fs::read(dir.join(name)).unwrap();
        let decode = |threads| -> Vec<Result<heifer::Image, String>> {
            let options = heifer::Options::new()
                .with_max_threads(threads)
                .with_max_pixels(1 << 17);
            heifer::decode_sequence_with_options(&bytes, &options)
                .unwrap()
                .take(8)
                .map(|frame| frame.map_err(|e| e.to_string()))
                .collect()
        };
        let (one, many) = (decode(1), decode(4));
        assert_eq!(one.len(), many.len(), "{name}: frame count");
        for (k, (a, b)) in one.iter().zip(&many).enumerate() {
            match (a, b) {
                (Ok(a), Ok(b)) => assert!(a == b, "{name}: frame {k} differs"),
                (Err(a), Err(b)) => assert_eq!(a, b, "{name}: frame {k} error"),
                _ => panic!("{name}: frame {k} succeeds in only one mode"),
            }
        }
    }
}

#[cfg(feature = "encoder")]
#[test]
fn encoded_sequences_read_back() {
    // 12 frames whose brightness grows, of an odd size (rounded up in 4:2:0, then cropped).
    let (w, h) = (97u32, 61u32);
    let frames: Vec<heifer::Image> = (0..12u16)
        .map(|n| {
            let mut image = heifer::Image::filled(w, h, 8, [0, 0, 0, 255]);
            for (i, p) in image.data.chunks_exact_mut(4).enumerate() {
                let (x, y) = (i as u32 % w, i as u32 / w);
                let v = ((x + 2 * y) % 64) as u16 + 15 * n;
                p[..3].copy_from_slice(&[v, v / 2 + 40, 200 - v / 2]);
            }
            image
        })
        .collect();
    let durations: Vec<f64> = (0..12).map(|n| if n == 11 { 0.5 } else { 0.04 }).collect();
    let options = heifer::EncodeOptions::new().with_quality(90);
    let bytes = heifer::encode_sequence(&frames, &durations, &options).unwrap();

    // Brands, primary image and track.
    assert_eq!(
        heifer::check_file_type(&bytes),
        heifer::FileTypeResult::Supported
    );
    let still = heifer::decode(&bytes).unwrap();
    assert_eq!((still.width, still.height), (w, h));
    let seq = heifer::read_sequence(&bytes).unwrap().unwrap();
    assert_eq!((seq.width, seq.height, seq.frames.len()), (w, h, 12));
    // One keyframe every 30 frames: only the first of 12; the others are P pictures.
    let keys: Vec<usize> = (0..12).filter(|&i| seq.frames[i].keyframe).collect();
    assert_eq!(keys, [0]);
    assert!((seq.frames[0].duration - 0.04).abs() < 1e-9);
    assert!((seq.frames[11].duration - 0.5).abs() < 1e-9);

    // Every frame decodes close to its original, on one thread or several.
    for threads in [1, 4] {
        let decoded: Vec<heifer::Image> = heifer::decode_sequence_with_options(
            &bytes,
            &heifer::Options::new().with_max_threads(threads),
        )
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
        assert_eq!(decoded.len(), 12);
        for (n, (a, b)) in frames.iter().zip(&decoded).enumerate() {
            assert_eq!((b.width, b.height), (w, h));
            let mse: f64 = a
                .data
                .iter()
                .zip(&b.data)
                .map(|(&x, &y)| (f64::from(x) - f64::from(y)).powi(2))
                .sum::<f64>()
                / a.data.len() as f64;
            let psnr = 10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10();
            assert!(psnr > 35.0, "frame {n}: {psnr:.1} dB");
        }
    }
    // A frame decoded alone (every frame is a keyframe).
    assert!(heifer::decode_sequence_frame(&bytes, 7).is_ok());
}

#[cfg(feature = "encoder")]
#[test]
fn encode_sequence_refuses_bad_input() {
    let a = heifer::Image::filled(32, 32, 8, [10, 20, 30, 255]);
    let b = heifer::Image::filled(16, 32, 8, [10, 20, 30, 255]);
    let o = heifer::EncodeOptions::new();
    assert!(heifer::encode_sequence(&[], &[], &o).is_err());
    assert!(heifer::encode_sequence(&[a.clone(), b], &[0.1, 0.1], &o).is_err());
    assert!(heifer::encode_sequence(std::slice::from_ref(&a), &[0.1, 0.1], &o).is_err());
    assert!(heifer::encode_sequence(std::slice::from_ref(&a), &[f64::NAN], &o).is_err());
}

#[cfg(feature = "encoder")]
#[test]
fn transparent_sequences_keep_alpha_and_keyframe_interval() {
    // 10 frames of 65×49 with moving alpha and custom keyframe interval 4 (keyframes at 0, 4, 8).
    let (w, h) = (65u32, 49u32);
    let frames: Vec<heifer::Image> = (0..10u16)
        .map(|n| {
            let mut image = heifer::Image::filled(w, h, 8, [0, 0, 0, 255]);
            image.has_alpha = true;
            for (i, p) in image.data.chunks_exact_mut(4).enumerate() {
                let (x, y) = (i as u16 % w as u16, i as u16 / w as u16);
                let r = (80.0 + 70.0 * (f64::from(x + n * 3) / 11.0).sin()) as u16;
                let g = (140.0 + 60.0 * (f64::from(y + n * 2) / 9.0).cos()) as u16;
                let b = (130.0 + 50.0 * (f64::from(x + y + n * 4) / 13.0).sin()) as u16;
                let a = if x + n * 3 < w as u16 / 2 {
                    ((y * 255) / (h as u16 - 1)).min(255)
                } else {
                    255
                };
                p.copy_from_slice(&[r, g, b, a]);
            }
            image
        })
        .collect();
    let durations = vec![0.05; 10];
    let options = heifer::EncodeOptions::new()
        .with_quality(90)
        .with_keyframe_interval(4);
    let bytes = heifer::encode_sequence(&frames, &durations, &options).unwrap();

    let seq = heifer::read_sequence(&bytes).unwrap().unwrap();
    let keys: Vec<usize> = (0..10).filter(|&i| seq.frames[i].keyframe).collect();
    assert_eq!(keys, [0, 4, 8]);

    let still = heifer::decode(&bytes).unwrap();
    assert!(still.has_alpha);

    let one: Vec<heifer::Image> =
        heifer::decode_sequence_with_options(&bytes, &heifer::Options::new().with_max_threads(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
    let many: Vec<heifer::Image> =
        heifer::decode_sequence_with_options(&bytes, &heifer::Options::new().with_max_threads(4))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
    assert_eq!(one.len(), 10);
    assert!(one == many);
    assert_eq!(heifer::decode_sequence_frame(&bytes, 6).unwrap(), one[6]);

    for (n, (orig, dec)) in frames.iter().zip(&one).enumerate() {
        assert!(dec.has_alpha, "frame {n} lost alpha");
        let psnr = |channel: usize| -> f64 {
            let mse: f64 = orig
                .data
                .iter()
                .skip(channel)
                .step_by(4)
                .zip(dec.data.iter().skip(channel).step_by(4))
                .map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2))
                .sum::<f64>()
                / (w as f64 * h as f64);
            10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10()
        };
        for c in 0..4 {
            assert!(psnr(c) > 33.0, "frame {n} ch {c}: {:.1} dB", psnr(c));
        }
    }

    // `encode_sequence_to_size` fits a smaller budget than quality 90, and rejects tiny ones.
    let budget = (bytes.len() * 3 / 4).max(1500);
    let (fitted, q) =
        heifer::encode_sequence_to_size(&frames, &durations, budget, &options).unwrap();
    assert!(
        fitted.len() <= budget,
        "{} > {budget} (q={q})",
        fitted.len()
    );
    assert!(heifer::encode_sequence_to_size(&frames, &durations, 100, &options).is_err());
}

#[cfg(feature = "encoder")]
#[test]
fn long_encoded_sequences_have_a_keyframe_every_30_frames() {
    // 64 frames of a scene moving 2 samples right per frame.
    let (sw, w, h) = (320u32, 96u32, 64u32);
    let scene: Vec<u16> = (0..sw * h)
        .map(|i| {
            let (x, y) = (i % sw, i / sw);
            (((x * 7 + y * 3) % 97) + ((x / 5 + y / 7) % 2) * 80) as u16
        })
        .collect();
    let frames: Vec<heifer::Image> = (0..64u32)
        .map(|n| {
            let mut image = heifer::Image::filled(w, h, 8, [0, 0, 0, 255]);
            for (i, p) in image.data.chunks_exact_mut(4).enumerate() {
                let (x, y) = (i as u32 % w, i as u32 / w);
                let v = scene[(y * sw + x + 2 * n) as usize];
                p[..3].copy_from_slice(&[v, v / 2 + 60, 200 - v / 2]);
            }
            image
        })
        .collect();
    let durations = vec![1.0 / 30.0; 64];
    let bytes =
        heifer::encode_sequence(&frames, &durations, &heifer::EncodeOptions::new()).unwrap();
    let seq = heifer::read_sequence(&bytes).unwrap().unwrap();
    let keys: Vec<usize> = (0..64).filter(|&i| seq.frames[i].keyframe).collect();
    assert_eq!(keys, [0, 30, 60]);
    // Decodes on one thread and several (three closed groups), and seeks from a keyframe.
    let one: Vec<heifer::Image> =
        heifer::decode_sequence_with_options(&bytes, &heifer::Options::new().with_max_threads(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
    let many: Vec<heifer::Image> =
        heifer::decode_sequence_with_options(&bytes, &heifer::Options::new().with_max_threads(4))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
    assert_eq!(one.len(), 64);
    assert!(one == many);
    assert!(heifer::decode_sequence_frame(&bytes, 45).unwrap() == one[45]);
    eprintln!("64 frames {w}x{h}: {} bytes", bytes.len());
}

#[cfg(feature = "encoder")]
#[test]
fn b_frames_preserve_variable_durations_and_reorder() {
    // 8 frames with smooth linear motion and variable frame durations so `ctts` offsets
    // vary per frame across two B-frame mini-GOPs (`I0, P3, B1, B2, P6, B4, B5, P7`).
    let (w, h) = (64u32, 48u32);
    let frames: Vec<heifer::Image> = (0..8u32)
        .map(|n| {
            let mut image = heifer::Image::filled(w, h, 8, [0, 0, 0, 255]);
            for (i, p) in image.data.chunks_exact_mut(4).enumerate() {
                let (x, y) = (i as u32 % w, i as u32 / w);
                let dx = f64::from(x) - 18.0 - 2.0 * f64::from(n);
                let dy = f64::from(y) - 14.0 - 1.0 * f64::from(n);
                let v = (128.0 + 95.0 * (-(dx * dx + dy * dy) / 55.0).exp())
                    .round()
                    .clamp(0.0, 255.0) as u16;
                p[..3].copy_from_slice(&[v, 255 - v / 2, (v / 2 + 64).min(255)]);
            }
            image
        })
        .collect();
    let durations = [0.04, 0.08, 0.02, 0.12, 0.06, 0.10, 0.05, 0.15];
    let opts_b = heifer::EncodeOptions::new()
        .with_quality(82)
        .with_b_frames(2);
    let bytes = heifer::encode_sequence(&frames, &durations, &opts_b).unwrap();

    let seq = heifer::read_sequence(&bytes).unwrap().unwrap();
    assert_eq!(seq.frames.len(), 8);
    assert!(bytes.windows(4).any(|w| w == b"edts"));
    assert!(bytes.windows(4).any(|w| w == b"elst"));
    let tracks = heifer::heifer_isobmff::track::parse_tracks(&bytes).unwrap();
    assert!(tracks[0].samples.iter().all(|s| s.composition_offset >= 0));
    assert!(seq.frames[0].keyframe);
    assert!(seq.frames[1..].iter().all(|f| !f.keyframe));
    for (i, (&got, &expected)) in seq
        .frames
        .iter()
        .map(|f| &f.duration)
        .zip(&durations)
        .enumerate()
    {
        assert!(
            (got - expected).abs() < 0.002,
            "frame {i} duration mismatch: got {got}, expected {expected}"
        );
    }

    let one: Vec<heifer::Image> =
        heifer::decode_sequence_with_options(&bytes, &heifer::Options::new().with_max_threads(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
    let many: Vec<heifer::Image> =
        heifer::decode_sequence_with_options(&bytes, &heifer::Options::new().with_max_threads(4))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
    assert_eq!(one.len(), 8);
    assert!(one == many);
    for (idx, dec) in one.iter().enumerate() {
        assert_eq!(heifer::decode_sequence_frame(&bytes, idx).unwrap(), *dec);
        let mse: f64 = frames[idx]
            .data
            .iter()
            .zip(&dec.data)
            .map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2))
            .sum::<f64>()
            / frames[idx].data.len() as f64;
        let psnr = 10.0 * (255.0_f64.powi(2) / mse.max(1e-9)).log10();
        assert!(psnr > 34.0, "frame {idx} PSNR too low: {psnr:.1} dB");
    }
}

#[cfg(feature = "encoder")]
#[test]
fn ten_bit_b_frame_alpha_sequences_preserve_precision() {
    // 5 frames of 10-bit RGBA with moving smooth gradient + transparency, using hierarchical
    // B-frames (`b_frames = 3`) and an auxiliary alpha track.
    let (w, h) = (64u32, 48u32);
    let frames: Vec<heifer::Image> = (0..5u16)
        .map(|n| {
            let mut image = heifer::Image::filled(w, h, 10, [0, 0, 0, 1023]);
            image.has_alpha = true;
            for (i, p) in image.data.chunks_exact_mut(4).enumerate() {
                let (x, y) = (
                    f64::from(i as u16 % w as u16),
                    f64::from(i as u16 / w as u16),
                );
                let t = f64::from(n);
                let r = (512.0 + 420.0 * ((x + t * 3.0) / 11.0).sin()).round() as u16;
                let g = (512.0 + 420.0 * ((y + t * 2.0) / 9.0).cos()).round() as u16;
                let b = (512.0 + 380.0 * ((x + y + t * 4.0) / 13.0).sin()).round() as u16;
                let a = if (x as u16) < w as u16 / 2 {
                    (((y as u16) * 1023) / (h as u16 - 1)).min(1023)
                } else {
                    1023
                };
                p.copy_from_slice(&[r, g, b, a]);
            }
            image
        })
        .collect();
    let durations = vec![0.04; 5];
    let options = heifer::EncodeOptions::new()
        .with_quality(90)
        .with_b_frames(3);
    let bytes = heifer::encode_sequence(&frames, &durations, &options).unwrap();

    let one: Vec<heifer::Image> =
        heifer::decode_sequence_with_options(&bytes, &heifer::Options::new().with_max_threads(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
    let many: Vec<heifer::Image> =
        heifer::decode_sequence_with_options(&bytes, &heifer::Options::new().with_max_threads(4))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
    assert_eq!(one.len(), 5);
    assert!(one == many);
    for (idx, (orig, dec)) in frames.iter().zip(&one).enumerate() {
        assert_eq!(dec.bit_depth, 10);
        assert!(dec.has_alpha);
        assert_eq!(heifer::decode_sequence_frame(&bytes, idx).unwrap(), *dec);
        let mse: f64 = orig
            .data
            .iter()
            .zip(&dec.data)
            .map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2))
            .sum::<f64>()
            / orig.data.len() as f64;
        let psnr = 10.0 * (1023.0_f64.powi(2) / mse.max(1e-9)).log10();
        assert!(psnr > 38.0, "10-bit frame {idx} PSNR too low: {psnr:.1} dB");
    }
}

#[cfg(feature = "encoder")]
#[test]
fn sequence_formats_and_speed_presets_round_trip() {
    let (w, h) = (48u32, 32u32);
    let frames: Vec<heifer::Image> = (0..4u16)
        .map(|n| {
            let mut image = heifer::Image::filled(w, h, 8, [0, 0, 0, 255]);
            image.has_alpha = true;
            for (i, p) in image.data.chunks_exact_mut(4).enumerate() {
                let (x, y) = (i as u16 % w as u16, i as u16 / w as u16);
                p[0] = (x * 5 + n * 7) % 256;
                p[1] = (y * 7 + n * 5) % 256;
                p[2] = ((x ^ y) * 9 + n * 11) % 256;
                p[3] = if x < 16 { 180 } else { 255 };
            }
            image
        })
        .collect();
    let durations = [0.05; 4];
    for (format, speed) in [
        (heifer::EncodeFormat::Ycbcr422, heifer::EncodeSpeed::Fast),
        (heifer::EncodeFormat::Ycbcr444, heifer::EncodeSpeed::Medium),
        (heifer::EncodeFormat::Lossless, heifer::EncodeSpeed::Fast),
    ] {
        let opts = heifer::EncodeOptions::new()
            .with_quality(88)
            .with_speed(speed);
        let bytes =
            heifer::encode_sequence_with_format(&frames, &durations, format, &opts).unwrap();
        let decoded: Vec<heifer::Image> = heifer::decode_sequence(&bytes)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(decoded.len(), 4);
        if format == heifer::EncodeFormat::Lossless {
            for (idx, (orig, dec)) in frames.iter().zip(&decoded).enumerate() {
                assert_eq!(dec.data, orig.data, "lossless frame {idx} differed");
            }
        }
    }
}
