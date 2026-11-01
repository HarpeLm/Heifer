//! Colour management: `decode_srgb`, and the ICC profile the encoder writes.

use heifer::heifer_isobmff::HeifFile;
use heifer::heifer_isobmff::boxes::{ColorInfo, Property};
use heifer::{EncodeOptions, Image, Metadata};

fn load(name: &str) -> Option<Vec<u8>> {
    let path = format!("{}/../../tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let data = std::fs::read(&path).ok();
    if data.is_none() {
        eprintln!("skipping: {path} not found (run scripts/fetch-fixtures.sh)");
    }
    data
}

/// The sRGB profile of the system, or `None` where there is none (CI runners other than macOS).
fn srgb_profile() -> Option<Vec<u8>> {
    std::fs::read("/System/Library/ColorSync/Profiles/sRGB Profile.icc").ok()
}

/// The `colr` properties of an item, descriptor before transformative ones.
fn colors(file: &HeifFile<'_>, id: heifer::heifer_isobmff::ItemId) -> Vec<ColorInfo> {
    file.item_properties(id)
        .unwrap()
        .filter_map(|p| match p {
            Property::Color(c) => Some(c.clone()),
            _ => None,
        })
        .collect()
}

/// A small RGB gradient, opaque.
fn gradient(width: u32, height: u32) -> Image {
    let mut image = Image::filled(width, height, 8, [0, 0, 0, 255]);
    for y in 0..height {
        for x in 0..width {
            let i = (y * width + x) as usize;
            image.data[i * 4] = u16::from((x * 255 / width.max(1)) as u8);
            image.data[i * 4 + 1] = u16::from((y * 255 / height.max(1)) as u8);
            image.data[i * 4 + 2] = u16::from(((x + y) * 255 / (width + height).max(1)) as u8);
        }
    }
    image
}

#[test]
fn files_without_colour_information_are_left_as_coded() {
    // These fixtures carry neither an ICC profile nor `nclx`: `decode_srgb` cannot know the
    // colour space, so it returns the samples untouched.
    for name in ["alpha.heic", "grid.heic", "single_image.heic"] {
        let Some(bytes) = load(name) else { continue };
        let raw = heifer::decode(&bytes).unwrap();
        let srgb = heifer::decode_srgb(&bytes).unwrap();
        assert_eq!((srgb.width, srgb.height), (raw.width, raw.height), "{name}");
        assert!(srgb.data == raw.data, "{name}: samples changed");
    }
}

#[test]
fn nclx_srgb_files_are_left_as_coded() {
    // Our own encoder tags sRGB files with `nclx` (BT.709 primaries, sRGB transfer): converting
    // them to sRGB must be a no-op.
    let rgba: Vec<u8> = (0..64 * 48)
        .flat_map(|i| {
            let v = (i % 256) as u8;
            [v, v.wrapping_mul(3), v.wrapping_mul(7), 255]
        })
        .collect();
    let bytes = heifer::encode_rgba8(64, 48, &rgba, &EncodeOptions::default()).unwrap();
    let raw = heifer::decode(&bytes).unwrap();
    let srgb = heifer::decode_srgb(&bytes).unwrap();
    assert!(srgb.data == raw.data);
    assert!(
        heifer::decode_srgb_rgba8(&bytes, &heifer::Options::default())
            .unwrap()
            .data
            == heifer::decode_rgba8(&bytes, &heifer::Options::default())
                .unwrap()
                .data
    );
}

#[test]
fn icc_profile_is_applied_to_the_decoded_image() {
    // `heif_other__pug.heic` carries a Display P3 ICC profile: converting to sRGB must move the
    // samples (a large gamut maps outside sRGB) without changing the image's shape.
    let Some(bytes) = load("real/heif_other__pug.heic") else {
        return;
    };
    let raw = heifer::decode(&bytes).unwrap();
    let srgb = heifer::decode_srgb(&bytes).unwrap();
    assert_eq!((srgb.width, srgb.height), (raw.width, raw.height));
    assert_eq!(srgb.has_alpha, raw.has_alpha);
    let changed = raw
        .data
        .iter()
        .zip(&srgb.data)
        .filter(|(a, b)| a != b)
        .count();
    assert!(
        changed > raw.data.len() / 4,
        "the ICC profile was ignored ({changed} samples changed)"
    );
    // The 8-bit variant agrees with the wide one on 8-bit files.
    if raw.bit_depth <= 8 {
        let narrow = heifer::decode_srgb_rgba8(&bytes, &heifer::Options::default()).unwrap();
        assert!(narrow.data == srgb.to_rgba8());
    }
}

#[test]
fn icc_profile_replaces_nclx_on_every_item() {
    let Some(icc) = srgb_profile() else {
        eprintln!("skipping: no system sRGB profile");
        return;
    };
    let image = gradient(600, 600); // a grid of four tiles, plus a thumbnail
    let metadata = Metadata {
        icc_profile: Some(icc.clone()),
        ..Default::default()
    };
    let bytes = heifer::encode_with_metadata(&image, &metadata, &EncodeOptions::default()).unwrap();

    let file = HeifFile::parse(&bytes).unwrap();
    let grid = file.primary_id;
    assert!(file.item(grid).unwrap().item_type.0 == *b"grid");
    let mut items = vec![grid];
    items.extend_from_slice(file.referenced_items(grid, b"dimg")); // tiles
    items.extend(file.referencing_items(grid, b"thmb")); // thumbnail
    for id in items {
        let colors = colors(&file, id);
        assert_eq!(colors.len(), 1, "item {id}: {colors:?}");
        match &colors[0] {
            ColorInfo::Icc(embedded) => assert_eq!(embedded, &icc),
            other => panic!("item {id}: expected the ICC profile, got {other:?}"),
        }
    }

    // The profile is read back, and decoding to sRGB succeeds.
    let read = heifer::read_metadata(&bytes).unwrap();
    assert_eq!(read.icc_profile.as_deref(), Some(icc.as_slice()));
    heifer::decode_srgb(&bytes).unwrap();
}

#[test]
fn srgb_icc_conversion_is_nearly_the_identity() {
    let Some(icc) = srgb_profile() else {
        eprintln!("skipping: no system sRGB profile");
        return;
    };
    let image = gradient(64, 48);
    let metadata = Metadata {
        icc_profile: Some(icc),
        ..Default::default()
    };
    let options = EncodeOptions {
        quality: 100,
        ..Default::default()
    };
    let bytes = heifer::encode_with_metadata(&image, &metadata, &options).unwrap();

    // The file is tagged sRGB (through its profile): converting to sRGB must not move the
    // samples. Comparing `decode_srgb` to `decode` isolates the profile conversion from the
    // (lossy) codec round trip.
    let raw = heifer::decode(&bytes).unwrap();
    let decoded = heifer::decode_srgb(&bytes).unwrap();
    let max = raw
        .data
        .iter()
        .zip(&decoded.data)
        .map(|(a, b)| i32::from(*a).abs_diff(i32::from(*b)))
        .max()
        .unwrap();
    assert!(max <= 1, "the sRGB profile changed the samples by {max}");
}
