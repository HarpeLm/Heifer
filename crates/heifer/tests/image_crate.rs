//! Integration with the `image` crate. Run with `--features image`.
#![cfg(feature = "image")]

use image::{ColorType, DynamicImage, GenericImageView, ImageDecoder};

fn path(name: &str) -> Option<std::path::PathBuf> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name);
    p.exists().then_some(p)
}

#[test]
fn image_open_decodes_heic_after_registration() {
    assert!(
        heifer::image_crate::register_image_decoder_hooks()
            || image::hooks::decoding_hook_registered("heic".as_ref())
    );
    let Some(p) = path("real/heif_other__pug.heic") else {
        return;
    };
    let img = image::open(&p).unwrap();
    assert_eq!(img.dimensions(), (4032, 3024));
    assert_eq!(img.color(), ColorType::Rgb8);
    // Same pixels as heifer's own API.
    let direct = heifer::decode(&std::fs::read(&p).unwrap()).unwrap();
    assert_eq!(img.to_rgb8().into_raw(), direct.to_rgb8());
}

#[test]
fn content_detection_from_memory() {
    heifer::image_crate::register_image_decoder_hooks();
    // iPhone files use the `heic` major brand, detected from the content.
    let Some(p) = path("real/heif_other__arrow.heic") else {
        return;
    };
    let img = image::load_from_memory(&std::fs::read(&p).unwrap()).unwrap();
    assert_eq!(img.dimensions(), (3024, 4032));
}

#[test]
fn decoder_metadata_and_orientation() {
    let Some(p) = path("real/heif_other__arrow.heic") else {
        return;
    };
    let mut dec = heifer::image_crate::HeifDecoder::new(std::fs::File::open(&p).unwrap()).unwrap();
    assert_eq!(dec.dimensions(), (3024, 4032), "rotation already applied");
    // EXIF says orientation 6, but irot is already applied: no further transform.
    assert_eq!(
        dec.orientation().unwrap(),
        image::metadata::Orientation::NoTransforms
    );
    assert!(dec.exif_metadata().unwrap().is_some());
    assert!(dec.icc_profile().unwrap().is_some());
}

#[test]
fn high_bit_depth_and_alpha() {
    let Some(p) = path("real/heif__RGBA_10__128x128.heif") else {
        return;
    };
    let dec = heifer::image_crate::HeifDecoder::new(std::fs::File::open(&p).unwrap()).unwrap();
    assert_eq!(dec.color_type(), ColorType::Rgba16);
    let img = DynamicImage::from_decoder(dec).unwrap();
    let px = img.to_rgba16();
    assert!(px.pixels().any(|p| p[3] < 65535), "alpha expected");
    assert!(px.pixels().any(|p| p[0] > 255), "16-bit range expected");
}

fn gradient(w: u32, h: u32) -> image::RgbaImage {
    image::RgbaImage::from_fn(w, h, |x, y| {
        let a = if y < h / 2 { 255 } else { (x * 255 / w) as u8 };
        image::Rgba([(x * 255 / w) as u8, (y * 255 / h) as u8, 128, a])
    })
}

fn encode_with_image(img: &DynamicImage) -> Vec<u8> {
    let mut heic = Vec::new();
    img.write_with_encoder(heifer::image_crate::HeifEncoder::new(&mut heic))
        .unwrap();
    heic
}

#[test]
fn encoder_writes_every_colour_type() {
    let rgba = DynamicImage::ImageRgba8(gradient(70, 40));
    for (img, alpha) in [
        (DynamicImage::ImageRgb8(rgba.to_rgb8()), false),
        (rgba.clone(), true),
        (DynamicImage::ImageLuma8(rgba.to_luma8()), false),
        (DynamicImage::ImageLumaA8(rgba.to_luma_alpha8()), true),
        (DynamicImage::ImageRgb16(rgba.to_rgb16()), false),
        (DynamicImage::ImageRgba16(rgba.to_rgba16()), true),
    ] {
        let decoded = heifer::decode(&encode_with_image(&img)).unwrap();
        assert_eq!(
            (decoded.width, decoded.height),
            (70, 40),
            "{:?}",
            img.color()
        );
        assert_eq!(decoded.has_alpha, alpha, "{:?}", img.color());
        // Same pixels, within compression error.
        let expected = img.to_rgba8();
        let max_error = expected
            .as_raw()
            .iter()
            .zip(decoded.to_rgba8())
            .map(|(&a, b)| a.abs_diff(b))
            .max()
            .unwrap();
        assert!(max_error < 40, "{:?}: {max_error}", img.color());
    }
}

#[test]
fn encoder_with_format_codes_444_and_lossless() {
    use heifer::EncodeFormat;
    use heifer::image_crate::HeifEncoder;

    let rgb = DynamicImage::ImageRgba8(gradient(64, 48)).to_rgb8();
    let img = DynamicImage::ImageRgb8(rgb.clone());
    let encode = |format| {
        let mut heic = Vec::new();
        img.write_with_encoder(HeifEncoder::with_format(
            &mut heic,
            heifer::EncodeOptions::default(),
            format,
        ))
        .unwrap();
        heic
    };

    let lossless = heifer::decode(&encode(EncodeFormat::Lossless)).unwrap();
    assert_eq!(
        lossless.to_rgb8(),
        rgb.as_raw().clone(),
        "lossless is exact"
    );

    let full = heifer::decode(&encode(EncodeFormat::Ycbcr444)).unwrap();
    assert_eq!((full.width, full.height), (64, 48));
}

#[test]
fn encoder_rejects_float_images() {
    let img = DynamicImage::ImageRgb32F(image::Rgb32FImage::new(4, 4));
    let mut out = Vec::new();
    assert!(
        img.write_with_encoder(heifer::image_crate::HeifEncoder::new(&mut out))
            .is_err()
    );
}

#[test]
fn encoder_stores_icc_and_exif() {
    use image::ImageEncoder;
    // Big-endian TIFF with one IFD0 entry: orientation (SHORT) = 6.
    let exif = b"MM\0*\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0\x06\0\0\0\0\0\0".to_vec();
    let mut prefixed = b"Exif\0\0".to_vec();
    prefixed.extend_from_slice(&exif);
    let img = gradient(16, 16);
    let mut heic = Vec::new();
    let mut encoder = heifer::image_crate::HeifEncoder::new(&mut heic);
    encoder.set_icc_profile(b"fake icc".to_vec()).unwrap();
    encoder.set_exif_metadata(prefixed).unwrap();
    encoder
        .write_image(img.as_raw(), 16, 16, image::ExtendedColorType::Rgba8)
        .unwrap();

    let meta = heifer::read_metadata(&heic).unwrap();
    assert_eq!(meta.icc_profile.as_deref(), Some(&b"fake icc"[..]));
    // Stored without the prefix, with the orientation reset to upright.
    assert_eq!(meta.exif_fields().unwrap().orientation(), Some(1));
    assert_eq!(meta.exif.unwrap().len(), exif.len());

    // And read back through the decoder.
    let mut decoder = heifer::image_crate::HeifDecoder::new(&heic[..]).unwrap();
    assert_eq!(
        decoder.icc_profile().unwrap().as_deref(),
        Some(&b"fake icc"[..])
    );
}

#[test]
fn decoder_with_options() {
    // A 1100×700 image: several grid tiles, decoded on one thread here.
    let (w, h) = (1100u32, 700u32);
    let rgba: Vec<u8> = (0..w * h * 4)
        .map(|i| (i % 4 * 60 + i / 4 % w / 5) as u8)
        .collect();
    let heic = heifer::encode_rgba8(w, h, &rgba, &heifer::EncodeOptions::default()).unwrap();
    let options = heifer::Options {
        max_threads: 1,
        ..Default::default()
    };
    let dec = heifer::image_crate::HeifDecoder::with_options(&heic[..], &options).unwrap();
    assert_eq!(dec.dimensions(), (w, h));
    let img = DynamicImage::from_decoder(dec).unwrap();
    assert_eq!(
        img.to_rgb8().into_raw(),
        heifer::decode(&heic).unwrap().to_rgb8()
    );

    // Too many pixels for the limit: an error, not a panic.
    let small = heifer::Options {
        max_pixels: 1000,
        ..Default::default()
    };
    assert!(heifer::image_crate::HeifDecoder::with_options(&heic[..], &small).is_err());
}

#[test]
fn sequences_decode_as_animations() {
    use heifer::image_crate::HeifSequenceDecoder;
    use image::AnimationDecoder;

    // 24 frames of 40 ms whose brightness grows with the display index (see sequence.rs).
    let bytes = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/bframes.mp4"),
    )
    .unwrap();
    let decoder = HeifSequenceDecoder::new(std::io::Cursor::new(&bytes)).unwrap();
    assert_eq!(decoder.len(), 24);
    let frames = decoder.into_frames().collect_frames().unwrap();
    assert_eq!(frames.len(), 24);
    let mean = |f: &image::Frame| {
        let b = f.buffer();
        b.pixels().map(|p| f64::from(p.0[1])).sum::<f64>() / f64::from(b.width() * b.height())
    };
    for pair in frames.windows(2) {
        assert!(mean(&pair[1]) > mean(&pair[0]) + 3.0, "frames out of order");
    }
    for f in &frames {
        assert_eq!(f.delay().numer_denom_ms(), (40, 1));
        assert_eq!(f.buffer().dimensions(), (128, 96));
        assert_eq!((f.left(), f.top()), (0, 0));
    }
    // The same pixels as heifer's own iterator.
    let own: Vec<heifer::Image> = heifer::decode_sequence(&bytes)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(frames[5].buffer().as_raw(), &own[5].to_rgba8());

    // A still image without a sequence is refused.
    if let Some(p) = path("single_image.heic") {
        let still = std::fs::read(p).unwrap();
        assert!(heifer::decode(&still).is_ok());
        assert!(HeifSequenceDecoder::from_bytes(still, &heifer::Options::default()).is_err());
    }
}
