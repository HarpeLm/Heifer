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
