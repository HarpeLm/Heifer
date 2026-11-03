//! Auxiliary images: listing and decoding alpha planes and depth maps.

use heifer::heifer_hevc_enc::encoder::{
    ChromaFormat, CodingOptions, EncodedPicture, EncoderConfig, Planes, Planes16, encode_picture,
    encode_picture_with, encode_picture16_with,
};
use heifer::heifer_isobmff::boxes::HevcConfig;
use heifer::heifer_isobmff::writer::{ALPHA_URN, HeifWriter, ImageSpec};
use heifer::{
    DEPTH_URN, EncodeFormat, EncodeOptions, Error, decode_auxiliary, decode_auxiliary_urn,
};

fn load_real(name: &str) -> Option<Vec<u8>> {
    let path = format!(
        "{}/../../tests/fixtures/real/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let data = std::fs::read(&path).ok();
    if data.is_none() {
        eprintln!("skipping: {path} not found (run scripts/fetch-fixtures.sh)");
    }
    data
}

/// An `ImageSpec` describing `picture`, with its own size.
fn spec<'a>(picture: &'a EncodedPicture, auxiliary_type: Option<&'a str>) -> ImageSpec<'a> {
    ImageSpec {
        width: picture.width,
        height: picture.height,
        config: HevcConfig {
            profile_idc: picture.profile_idc,
            level_idc: picture.level_idc,
            chroma_format: picture.chroma_format,
            bit_depth_luma: 8,
            bit_depth_chroma: 8,
            nal_length_size: 4,
            nal_units: picture.parameter_sets.clone(),
        },
        slices: &picture.slices,
        color: None,
        auxiliary_type,
        hidden: auxiliary_type.is_some(),
        crop: None,
    }
}

/// A lossless monochrome depth picture, `side × side`, whose samples encode the position.
fn depth_picture(side: u32) -> (Vec<u8>, EncodedPicture) {
    let samples: Vec<u8> = (0..side * side)
        .map(|i| ((i % side) * 4 + (i / side) * 7) as u8)
        .collect();
    let options = CodingOptions {
        chroma_format: ChromaFormat::Monochrome,
        lossless: true,
    };
    let picture = encode_picture_with(
        side,
        side,
        Planes {
            y: &samples,
            chroma: None,
        },
        &options,
        &EncoderConfig::default(),
    )
    .unwrap();
    (samples, picture)
}

/// A 16×16 4:2:0 grey primary picture.
fn primary_picture() -> EncodedPicture {
    let y = [96u8; 16 * 16];
    let c = [128u8; 8 * 8];
    encode_picture(
        16,
        16,
        Planes {
            y: &y,
            chroma: Some((&c, &c)),
        },
        &EncoderConfig::default(),
    )
    .unwrap()
}

#[test]
fn alpha_plane_is_listed_and_decoded() {
    let (width, height) = (32u32, 24u32);
    let rgba: Vec<u8> = (0..width * height)
        .flat_map(|i| {
            let alpha = ((i % width) * 8) as u8;
            [40, 80, 120, alpha]
        })
        .collect();
    let bytes = heifer::encode_rgba8_with_format(
        width,
        height,
        &rgba,
        EncodeFormat::Lossless,
        &EncodeOptions::default(),
    )
    .unwrap();

    let aux = heifer::auxiliary_images(&bytes).unwrap();
    assert_eq!(aux.len(), 1, "{aux:?}");
    assert!(aux[0].is_alpha());
    assert!(!aux[0].is_depth());
    assert_eq!((aux[0].width, aux[0].height), (width, height));
    assert_eq!(aux[0].bit_depth, 8);
    assert_eq!(aux[0].item_type.to_string(), "hvc1");

    // The alpha plane is monochrome: its sample is replicated in the RGB channels.
    let plane = decode_auxiliary(&bytes, aux[0].id).unwrap();
    assert_eq!((plane.width, plane.height), (width, height));
    assert!(!plane.has_alpha);
    for y in 0..height as usize {
        for x in 0..width as usize {
            let expected = (x * 8) as u16;
            let i = (y * width as usize + x) * 4;
            assert_eq!(plane.data[i], expected, "({x}, {y})");
            assert_eq!(plane.data[i + 1], expected, "({x}, {y})");
            assert_eq!(plane.data[i + 2], expected, "({x}, {y})");
        }
    }

    // The same plane by URN, as a caller looking for alpha would do.
    let by_urn = decode_auxiliary_urn(&bytes, "urn:mpeg:hevc:2015:auxid:1")
        .unwrap()
        .expect("alpha auxiliary image");
    assert_eq!(by_urn, plane);
}

#[test]
fn depth_map_is_listed_and_decoded_losslessly() {
    let (samples, depth) = depth_picture(16);
    let primary = primary_picture();

    let mut writer = HeifWriter::new();
    let primary_id = writer.add_image(&spec(&primary, None));
    let depth_id = writer.add_image(&spec(&depth, Some(DEPTH_URN)));
    writer.add_reference(*b"auxl", depth_id, &[primary_id]);
    writer.set_primary(primary_id);
    let bytes = writer.finish();

    let aux = heifer::auxiliary_images(&bytes).unwrap();
    assert_eq!(aux.len(), 1, "{aux:?}");
    assert_eq!(aux[0].id, depth_id);
    assert!(aux[0].is_depth());
    assert!(!aux[0].is_alpha());
    assert_eq!((aux[0].width, aux[0].height), (16, 16));
    assert_eq!(aux[0].bit_depth, 8);

    let decoded = decode_auxiliary_urn(&bytes, DEPTH_URN)
        .unwrap()
        .expect("depth auxiliary image");
    assert_eq!((decoded.width, decoded.height), (16, 16));
    for (i, &sample) in samples.iter().enumerate() {
        assert_eq!(decoded.data[i * 4], u16::from(sample), "sample {i}");
    }

    // No auxiliary image of that type in an ordinary file.
    let plain =
        heifer::encode_rgb8(16, 16, &[0u8; 16 * 16 * 3], &EncodeOptions::default()).unwrap();
    assert!(heifer::auxiliary_images(&plain).unwrap().is_empty());
    assert!(decode_auxiliary_urn(&plain, DEPTH_URN).unwrap().is_none());
}

#[test]
fn real_depth_item_is_listed_but_unsupported() {
    let Some(bytes) = load_real("heif_special__unsupported_depth_image.heic") else {
        return;
    };
    let aux = heifer::auxiliary_images(&bytes).unwrap();
    assert_eq!(aux.len(), 1, "{aux:?}");
    assert!(aux[0].is_depth());
    assert_eq!(aux[0].urn, "urn:mpeg:hevc:2015:auxid:2");
    assert_eq!((aux[0].width, aux[0].height), (64, 32));
    assert_eq!(aux[0].bit_depth, 8);
    assert_eq!(aux[0].item_type.to_string(), "unci");

    let error = decode_auxiliary(&bytes, aux[0].id).expect_err("unci is not decoded");
    assert!(
        matches!(&error, Error::Unsupported(message) if message.contains("unci")),
        "unexpected error: {error}"
    );
}

#[test]
fn grid_auxiliary_image_reports_its_tiles_bit_depth() {
    // A 10-bit monochrome tile: the grid item itself has no `pixi` or `hvcC`, so the depth
    // must come from its first input.
    let samples = [512u16; 8 * 8];
    let picture = encode_picture16_with(
        8,
        8,
        Planes16 {
            y: &samples,
            chroma: None,
            bit_depth: 10,
        },
        &CodingOptions {
            chroma_format: ChromaFormat::Monochrome,
            lossless: false,
        },
        &EncoderConfig::default(),
    )
    .unwrap();
    let config = HevcConfig {
        profile_idc: picture.profile_idc,
        level_idc: picture.level_idc,
        chroma_format: picture.chroma_format,
        bit_depth_luma: 10,
        bit_depth_chroma: 10,
        nal_length_size: 4,
        nal_units: picture.parameter_sets.clone(),
    };
    let tile_spec = ImageSpec {
        hidden: true,
        ..ImageSpec::new(8, 8, config, &picture.slices)
    };

    let mut writer = HeifWriter::new();
    let primary = primary_picture();
    let primary_id = writer.add_image(&spec(&primary, None));
    let tiles = [writer.add_image(&tile_spec), writer.add_image(&tile_spec)];
    let alpha = writer.add_grid(1, 2, 16, 8, &tiles, None, Some(ALPHA_URN));
    writer.add_reference(*b"auxl", alpha, &[primary_id]);
    writer.set_primary(primary_id);
    let bytes = writer.finish();

    let aux = heifer::auxiliary_images(&bytes).unwrap();
    assert_eq!(aux.len(), 1, "{aux:?}");
    assert!(aux[0].is_alpha());
    assert_eq!((aux[0].width, aux[0].height), (16, 8));
    assert_eq!(aux[0].bit_depth, 10);
    assert_eq!(aux[0].item_type.to_string(), "grid");
}

#[test]
fn decode_all_still_skips_auxiliary_images() {
    let (width, height) = (32u32, 24u32);
    let rgba: Vec<u8> = (0..width * height)
        .flat_map(|i| [200, 10, 60, (i % 200) as u8])
        .collect();
    let with_alpha = heifer::encode_rgba8(width, height, &rgba, &EncodeOptions::default()).unwrap();
    assert_eq!(heifer::auxiliary_images(&with_alpha).unwrap().len(), 1);
    assert_eq!(heifer::decode_all(&with_alpha).unwrap().len(), 1);

    let (_, depth) = depth_picture(8);
    let primary = primary_picture();
    let mut writer = HeifWriter::new();
    let primary_id = writer.add_image(&spec(&primary, None));
    let depth_id = writer.add_image(&spec(&depth, Some(DEPTH_URN)));
    writer.add_reference(*b"auxl", depth_id, &[primary_id]);
    writer.set_primary(primary_id);
    let with_depth = writer.finish();
    assert_eq!(heifer::auxiliary_images(&with_depth).unwrap().len(), 1);
    assert_eq!(heifer::decode_all(&with_depth).unwrap().len(), 1);
}
