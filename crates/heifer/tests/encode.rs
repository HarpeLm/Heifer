//! Round trips through the encoder and the decoder.

use heifer::{EncodeOptions, Image, decode, encode, encode_rgb8, encode_rgba8};

/// A synthetic photo-like RGBA image: smooth gradients, edges and some texture, with an alpha
/// gradient in the bottom half when `alpha` is set.
fn test_image(w: u32, h: u32, alpha: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            // Features a few pixels wide or more, as in photos.
            let (fx, fy) = (f64::from(x) / f64::from(w), f64::from(y) / f64::from(h));
            let (px, py) = (f64::from(x), f64::from(y));
            let ring =
                ((px - f64::from(w) / 2.0).hypot(py - f64::from(h) / 2.0) / 9.0).sin() * 30.0;
            let r = 60.0 + 150.0 * fx + ring;
            let g = 200.0 - 120.0 * fy
                + if (x / 37 + y / 23) % 2 == 0 {
                    25.0
                } else {
                    0.0
                };
            let b = 128.0 + 100.0 * (px / 31.0).sin() * (py / 23.0).cos();
            let a = if alpha && y >= h / 2 {
                255.0 * f64::from(x) / f64::from(w)
            } else {
                255.0
            };
            out.extend([r, g, b, a].map(|v| v.round().clamp(0.0, 255.0) as u8));
        }
    }
    out
}

fn psnr(a: &[u8], b: &[u8], channels: impl Fn(usize) -> bool) -> f64 {
    let (mut sum, mut n) = (0.0, 0usize);
    for (i, (&a, &b)) in a.iter().zip(b).enumerate() {
        if channels(i % 4) {
            sum += (f64::from(a) - f64::from(b)).powi(2);
            n += 1;
        }
    }
    10.0 * (255.0f64.powi(2) / (sum / n as f64).max(1e-9)).log10()
}

fn round_trip(w: u32, h: u32, alpha: bool, quality: u8) -> (Vec<u8>, Image, f64) {
    let rgba = test_image(w, h, alpha);
    let options = EncodeOptions {
        quality,
        ..EncodeOptions::default()
    };
    let heic = encode_rgba8(w, h, &rgba, &options).unwrap();
    let image = decode(&heic).unwrap();
    assert_eq!((image.width, image.height, image.bit_depth), (w, h, 8));
    assert_eq!(image.has_alpha, alpha);
    let decoded = image.to_rgba8();
    let p = psnr(&rgba, &decoded, |c| c < 3);
    if alpha {
        let pa = psnr(&rgba, &decoded, |c| c == 3);
        assert!(pa > 38.0, "{w}×{h} alpha: {pa:.1} dB");
    }
    (heic, image, p)
}

#[test]
fn small_images() {
    for (w, h) in [(16, 9), (64, 48), (77, 51), (512, 512)] {
        let (_, _, p) = round_trip(w, h, false, 80);
        assert!(p > 35.0, "{w}×{h}: {p:.1} dB");
    }
}

#[test]
fn tiny_images() {
    // A few pixels cannot keep their colours through 4:2:0 subsampling, but grey levels stay.
    for (w, h) in [(1, 1), (2, 3), (3, 2), (5, 7)] {
        let rgba: Vec<u8> = (0..w * h)
            .flat_map(|i| {
                let v = (i * 37 % 200) as u8;
                [v, v, v, 255]
            })
            .collect();
        let options = EncodeOptions {
            quality: 100,
            ..EncodeOptions::default()
        };
        let image = decode(&encode_rgba8(w, h, &rgba, &options).unwrap()).unwrap();
        assert_eq!((image.width, image.height), (w, h));
        let error = rgba
            .iter()
            .zip(image.to_rgba8())
            .map(|(&a, b)| a.abs_diff(b))
            .max();
        assert!(error <= Some(2), "{w}×{h}: {error:?}");
    }
}

#[test]
fn grid_images() {
    // 3×2 tiles, the last column and row partly outside the image.
    let (heic, _, p) = round_trip(1100, 700, false, 80);
    assert!(p > 35.0, "{p:.1} dB");
    let file = heifer::heifer_isobmff::HeifFile::parse(&heic).unwrap();
    let grid = file.grid(file.primary_id).unwrap();
    assert_eq!((grid.rows, grid.columns), (2, 3));
}

#[test]
fn alpha() {
    for (w, h) in [(33, 17), (600, 300)] {
        let (_, image, p) = round_trip(w, h, true, 80);
        assert!(p > 35.0, "{w}×{h}: {p:.1} dB");
        // Opaque top half, transparent left of the bottom half.
        assert_eq!(image.data[3], 255);
        let bottom_left = ((h - 1) * w) as usize * 4 + 3;
        assert!(image.data[bottom_left] < 8);
    }
}

#[test]
fn quality_controls_size_and_fidelity() {
    let results: Vec<(usize, f64)> = [20, 50, 80, 95]
        .iter()
        .map(|&q| {
            let (heic, _, p) = round_trip(256, 192, false, q);
            (heic.len(), p)
        })
        .collect();
    for pair in results.windows(2) {
        assert!(
            pair[0].0 < pair[1].0 && pair[0].1 < pair[1].1,
            "{results:?}"
        );
    }
}

#[test]
fn encode_decoded_image_and_rgb() {
    let rgba = test_image(40, 30, false);
    let rgb: Vec<u8> = rgba
        .chunks_exact(4)
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect();
    let options = EncodeOptions::default();
    let from_rgb = encode_rgb8(40, 30, &rgb, &options).unwrap();
    let from_rgba = encode_rgba8(40, 30, &rgba, &options).unwrap();
    assert_eq!(from_rgb, from_rgba);
    let image = decode(&from_rgb).unwrap();
    let again = encode(&image, &options).unwrap();
    assert!(!decode(&again).unwrap().has_alpha);
}

#[test]
fn rejects_bad_input() {
    let options = EncodeOptions::default();
    assert!(encode_rgba8(0, 1, &[], &options).is_err());
    assert!(encode_rgba8(2, 2, &[0; 15], &options).is_err());
    assert!(encode_rgb8(2, 2, &[0; 16], &options).is_err());
}

#[test]
fn metadata_is_kept() {
    let rgba = test_image(600, 300, false); // a grid: metadata goes on the grid item
    let image = decode(&encode_rgba8(600, 300, &rgba, &EncodeOptions::default()).unwrap()).unwrap();
    let metadata = heifer::Metadata {
        exif: Some(b"II*\0\x08\0\0\0\0\0".to_vec()),
        xmp: Some(b"<x:xmpmeta/>".to_vec()),
        icc_profile: Some(b"icc".to_vec()),
    };
    let heic = heifer::encode_with_metadata(&image, &metadata, &EncodeOptions::default()).unwrap();
    assert_eq!(heifer::read_metadata(&heic).unwrap(), metadata);
    assert_eq!(decode(&heic).unwrap().width, 600);
}

#[test]
fn deep_images_are_coded_in_10_bits() {
    // A smooth 16-bit gradient: 8 bits would leave at most 256 grey levels (visible steps).
    let (w, h) = (1024u32, 64u32);
    let mut data = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let v = (x * 64) as u16;
            let a = if y < 32 { 65535 } else { (x * 64) as u16 };
            data.extend([v, v, v, a]);
        }
    }
    let image = Image {
        width: w,
        height: h,
        bit_depth: 16,
        has_alpha: true,
        data,
    };
    let options = EncodeOptions {
        quality: 90,
        ..EncodeOptions::default()
    };
    let heic = encode(&image, &options).unwrap();
    let file = heifer::heifer_isobmff::HeifFile::parse(&heic).unwrap();
    let tiles = file.referenced_items(file.primary_id, b"dimg");
    let config = file.hevc_config(tiles[0]).unwrap().unwrap();
    assert_eq!(
        (config.profile_idc, config.bit_depth_luma),
        (2, 10),
        "Main 10"
    );

    let decoded = decode(&heic).unwrap();
    assert_eq!(
        (decoded.width, decoded.height, decoded.bit_depth),
        (w, h, 10)
    );
    assert!(decoded.has_alpha);
    let row: Vec<u16> = (0..w as usize)
        .map(|x| decoded.data[(10 * w as usize + x) * 4])
        .collect();
    let mut levels = row.clone();
    levels.sort_unstable();
    levels.dedup();
    assert!(levels.len() > 600, "{} grey levels", levels.len());
    // Close to the original (scaled to 10 bits).
    let max_error = row
        .iter()
        .enumerate()
        .map(|(x, &v)| (i32::from(v) - (x as i32 * 64 * 1023 / 65535)).abs())
        .max();
    assert!(max_error <= Some(6), "{max_error:?}");
}
