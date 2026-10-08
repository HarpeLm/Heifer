//! End-to-end decoding of real HEIC files. Run `scripts/fetch-fixtures.sh` first; missing
//! files are skipped.

use heifer_isobmff::HeifFile;

fn load(name: &str) -> Option<Vec<u8>> {
    let path = format!("{}/../../tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let data = std::fs::read(&path).ok();
    if data.is_none() {
        eprintln!("skipping: {path} not found (run scripts/fetch-fixtures.sh)");
    }
    data
}

#[test]
fn decodes_all_fixtures() {
    for (name, w, h, alpha) in [
        ("single_image.heic", 1440, 960, false),
        ("grid.heic", 960, 640, false),
        ("alpha.heic", 1440, 960, false),
        ("collection.heic", 1440, 960, false),
        ("burst.heic", 1280, 720, false),
        ("libheif_example.heic", 1280, 854, false),
    ] {
        let Some(bytes) = load(name) else { continue };
        let img = heifer::decode(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!((img.width, img.height), (w, h), "{name}");
        assert_eq!(img.has_alpha, alpha, "{name}");
        assert_eq!(img.data.len(), (w * h * 4) as usize);
        assert_eq!(img.to_rgb8().len(), (w * h * 3) as usize);
    }
}

#[test]
fn grid_tiles_are_placed_in_order() {
    let Some(bytes) = load("grid.heic") else {
        return;
    };
    let file = HeifFile::parse(&bytes).unwrap();
    let canvas = heifer::decode(&bytes).unwrap();
    let tiles = file.referenced_items(file.primary_id, b"dimg");
    for (i, &tile_id) in tiles.iter().enumerate() {
        let tile = heifer::decode_item(&file, tile_id, 0).unwrap();
        let (x0, y0) = ((i as u32 % 2) * tile.width, (i as u32 / 2) * tile.height);
        assert_eq!(
            canvas.crop(x0, y0, tile.width, tile.height),
            tile,
            "tile {i}"
        );
    }
}

#[test]
fn alpha_auxiliary_image_is_attached() {
    let Some(bytes) = load("alpha.heic") else {
        return;
    };
    let file = HeifFile::parse(&bytes).unwrap();
    // Item 1005 has an alpha plane (item 1008); the primary overlay composites it.
    let with_alpha = heifer::decode_item(&file, 1005, 0).unwrap();
    assert!(with_alpha.has_alpha);
    let alphas: Vec<u16> = with_alpha.data.chunks(4).map(|p| p[3]).collect();
    assert!(alphas.contains(&0), "fully transparent pixels expected");
    // The coded alpha reaches 234 (limited range 16..=235), i.e. 254 once expanded.
    assert!(
        alphas.iter().any(|&a| a >= 254),
        "nearly opaque pixels expected"
    );
}

#[test]
fn truncated_files_return_errors() {
    let Some(bytes) = load("single_image.heic") else {
        return;
    };
    for len in [0, 10, 100, 600, bytes.len() / 2] {
        assert!(heifer::decode(&bytes[..len]).is_err(), "length {len}");
    }
}

#[test]
fn parallel_and_sequential_decoding_match() {
    let Some(bytes) = load("grid.heic") else {
        return;
    };
    let sequential = heifer::decode_with_options(
        &bytes,
        &heifer::Options {
            max_threads: 1,
            ..Default::default()
        },
    )
    .unwrap();
    for threads in [0, 2, 3, 16] {
        let parallel = heifer::decode_with_options(
            &bytes,
            &heifer::Options {
                max_threads: threads,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(parallel, sequential, "max_threads = {threads}");
    }
}

/// Real-world files from pillow-heif's test suite (`scripts/fetch-real.sh`).
fn load_real(name: &str) -> Option<Vec<u8>> {
    load(&format!("real/{name}"))
}

#[test]
fn real_world_photos() {
    // (file, width, height, alpha): sizes checked against the files' ispe/clap/irot and
    // compared with ffmpeg (≈ 60 dB PSNR).
    for (name, w, h, alpha) in [
        ("heif_other__pug.heic", 4032, 3024, false), // iPhone, 48 tiles, HDR gain map
        ("heif_other__arrow.heic", 3024, 4032, false), // rotated 270°
        ("heif_other__spatial_photo.heic", 2560, 2560, false), // iPhone spatial (stereo) photo
        ("heif_special__xiaomi.heic", 2592, 1944, false), // Android
        ("heif_special__aux_YCbCr.heic", 4000, 1848, false), // Samsung, gain map
        ("benchmarks__image_large.heic", 8000, 6000, false),
        ("heif_other__invalid_id.heic", 1197, 1227, false), // grid output crop
        ("heif_other__empty_icc.heic", 1025, 900, false),   // primary item, not the largest
        ("heif__RGBA_10__128x128.heif", 128, 128, true),
        ("heif__RGB_12__29x100.heif", 29, 100, false), // odd size: ispe crop
        ("heif__L_10__128x128.heif", 128, 128, false), // monochrome 10-bit
    ] {
        let Some(bytes) = load_real(name) else {
            continue;
        };
        let img = heifer::decode(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            (img.width, img.height, img.has_alpha),
            (w, h, alpha),
            "{name}"
        );
    }
}

#[test]
fn high_bit_depth_is_preserved() {
    for (name, depth) in [
        ("heif__RGB_10__128x128.heif", 10),
        ("heif__RGB_12__128x128.heif", 12),
    ] {
        let Some(bytes) = load_real(name) else {
            continue;
        };
        assert_eq!(heifer::decode(&bytes).unwrap().bit_depth, depth, "{name}");
    }
}

#[test]
fn broken_real_files_return_errors() {
    for name in [
        "heif_corrupted__corrupted.heic",
        "heif_corrupted__empty.heic",
        "heif_truncated__truncated.heic",
    ] {
        let Some(bytes) = load_real(name) else {
            continue;
        };
        assert!(heifer::decode(&bytes).is_err(), "{name}");
    }
}

#[test]
fn metadata_of_real_photos() {
    // (file, make, model, date, orientation, gps, has xmp, has icc)
    for (name, make, model, date, orientation, gps, xmp, icc) in [
        (
            "heif_other__pug.heic",
            "Apple",
            "iPhone 13 Pro",
            "2023:02:21 14:33:08",
            Some(1),
            true,
            true,
            true,
        ),
        (
            "heif_other__arrow.heic",
            "Apple",
            "iPhone 8 Plus",
            "2018:07:31 14:52:07",
            Some(6),
            false,
            false,
            true,
        ),
        (
            "heif_special__200MP.heic",
            "samsung",
            "Galaxy S24 Ultra",
            "2025:01:19 14:53:27",
            Some(6),
            true,
            true,
            true,
        ),
        (
            "heif_other__L_exif_xmp_iptc.heic",
            "SONY",
            "ILCE-7SM3",
            "2020:09:14 11:09:34",
            None,
            false,
            true,
            false,
        ),
    ] {
        let Some(bytes) = load_real(name) else {
            continue;
        };
        let meta = heifer::read_metadata(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        let exif = meta
            .exif_fields()
            .unwrap_or_else(|| panic!("{name}: no EXIF"));
        assert_eq!(exif.make(), Some(make), "{name}");
        assert_eq!(exif.model(), Some(model), "{name}");
        assert_eq!(exif.date_time(), Some(date), "{name}");
        assert_eq!(exif.orientation(), orientation, "{name}");
        assert_eq!(exif.has_gps(), gps, "{name}");
        assert_eq!(meta.xmp.is_some(), xmp, "{name}");
        assert_eq!(meta.icc_profile.is_some(), icc, "{name}");
        if let Some(x) = &meta.xmp {
            assert!(
                x.windows(9).any(|w| w == b"x:xmpmeta"),
                "{name}: XMP is not an XMP packet"
            );
        }
        if let Some(icc) = &meta.icc_profile {
            assert_eq!(&icc[36..40], b"acsp", "{name}: invalid ICC profile");
        }
    }
}

#[test]
fn files_without_metadata() {
    let Some(bytes) = load("grid.heic") else {
        return;
    };
    assert_eq!(
        heifer::read_metadata(&bytes).unwrap(),
        heifer::Metadata::default()
    );
}
