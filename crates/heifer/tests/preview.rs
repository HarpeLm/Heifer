//! Integration tests for reader sRGB decoding, colour fallback, and bounded-memory preview
//! decoding.

use std::io::Cursor;
use std::path::Path;

use heifer::{
    Error, Image, Options, PreviewOptions, ReadError, decode_preview_rgba8,
    decode_reader_preview_rgba8, decode_reader_srgb_rgba8, decode_rgba8, decode_srgb_rgba8,
};

fn fixture(name: &str) -> Option<Vec<u8>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name);
    std::fs::read(path).ok()
}

fn reference_downscale(src: &Image<u8>, dst_w: u32, dst_h: u32) -> Image<u8> {
    let mut accum = vec![[0u64; 5]; dst_w as usize * dst_h as usize];
    for y in 0..src.height {
        let oy = (u64::from(y) * u64::from(dst_h) / u64::from(src.height)) as usize;
        for x in 0..src.width {
            let ox = (u64::from(x) * u64::from(dst_w) / u64::from(src.width)) as usize;
            let s = (y as usize * src.width as usize + x as usize) * 4;
            let a = &mut accum[oy * dst_w as usize + ox];
            a[0] += u64::from(src.data[s]);
            a[1] += u64::from(src.data[s + 1]);
            a[2] += u64::from(src.data[s + 2]);
            a[3] += u64::from(src.data[s + 3]);
            a[4] += 1;
        }
    }
    let mut data = Vec::with_capacity(dst_w as usize * dst_h as usize * 4);
    for a in accum {
        let half = a[4] / 2;
        data.push(((a[0] + half) / a[4]) as u8);
        data.push(((a[1] + half) / a[4]) as u8);
        data.push(((a[2] + half) / a[4]) as u8);
        data.push(((a[3] + half) / a[4]) as u8);
    }
    Image {
        width: dst_w,
        height: dst_h,
        bit_depth: 8,
        has_alpha: src.has_alpha,
        data,
    }
}

#[test]
fn decode_reader_srgb_rgba8_matches_slice() {
    for name in [
        "single_image.heic",
        "grid.heic",
        "real/heif_other__pug.heic",
        "real/heif_other__arrow.heic",
    ] {
        let Some(bytes) = fixture(name) else { continue };
        let opts = Options::new();
        let from_slice = decode_srgb_rgba8(&bytes, &opts).unwrap();
        let from_reader = decode_reader_srgb_rgba8(Cursor::new(&bytes), &opts).unwrap();
        assert_eq!(from_reader.width, from_slice.width, "{name}");
        assert_eq!(from_reader.height, from_slice.height, "{name}");
        assert_eq!(from_reader.has_alpha, from_slice.has_alpha, "{name}");
        assert!(from_reader.data == from_slice.data, "{name} pixels differ");
    }
}

#[cfg(feature = "encoder")]
#[test]
fn fallback_to_coded_color_returns_coded_pixels_for_unsupported_icc() {
    let mut pixels = Vec::with_capacity(16 * 16 * 4);
    for y in 0..16u16 {
        for x in 0..16u16 {
            pixels.extend_from_slice(&[x * 16, y * 16, 128, 255]);
        }
    }
    let src = Image {
        width: 16,
        height: 16,
        bit_depth: 8,
        has_alpha: false,
        data: pixels,
    };
    // Minimal invalid/non-RGB ICC profile bytes that `icc::Profile::parse` rejects as Unsupported.
    let mut meta = heifer::Metadata::new();
    meta.icc_profile = Some(b"not-a-valid-rgb-icc-profile".to_vec());
    let heic =
        heifer::encode_with_metadata(&src, &meta, &heifer::EncodeOptions::new().with_quality(90))
            .unwrap();

    // Strict mode (default) rejects the unsupported ICC profile.
    assert!(matches!(
        decode_srgb_rgba8(&heic, &Options::new()),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        decode_reader_srgb_rgba8(Cursor::new(&heic), &Options::new()),
        Err(ReadError::Heif(Error::Unsupported(_)))
    ));

    // Fallback mode returns the exact coded pixels without re-decoding.
    let fallback_opts = Options::new().with_fallback_to_coded_color(true);
    let coded = decode_rgba8(&heic, &Options::new()).unwrap();
    let srgb_slice = decode_srgb_rgba8(&heic, &fallback_opts).unwrap();
    let srgb_reader = decode_reader_srgb_rgba8(Cursor::new(&heic), &fallback_opts).unwrap();
    assert!(srgb_slice.data == coded.data);
    assert!(srgb_reader.data == coded.data);

    // Preview with fallback also succeeds.
    let preview = decode_preview_rgba8(
        &heic,
        &PreviewOptions::new(8, 8).with_decode_options(fallback_opts),
    )
    .unwrap();
    assert_eq!((preview.width, preview.height), (8, 8));
}

#[test]
fn grid_preview_matches_full_decode_downscale_and_reader() {
    for name in [
        "grid.heic",
        "real/heif_other__pug.heic",
        "real/heif_other__arrow.heic",
    ] {
        let Some(bytes) = fixture(name) else { continue };
        let full = decode_rgba8(&bytes, &Options::new()).unwrap();

        let opts_par = PreviewOptions::new(320, 320).with_to_srgb(false);
        let opts_seq = opts_par.with_decode_options(Options::new().with_max_threads(1));

        let preview_par = decode_preview_rgba8(&bytes, &opts_par).unwrap();
        let preview_seq = decode_preview_rgba8(&bytes, &opts_seq).unwrap();
        let preview_reader = decode_reader_preview_rgba8(Cursor::new(&bytes), &opts_par).unwrap();

        assert!(preview_par.width <= 320 && preview_par.height <= 320);
        assert_eq!(preview_par.width, preview_seq.width, "{name}");
        assert_eq!(preview_par.height, preview_seq.height, "{name}");
        assert!(preview_par.data == preview_seq.data, "{name} seq vs par");
        assert!(
            preview_par.data == preview_reader.data,
            "{name} reader vs slice"
        );

        let expected = reference_downscale(&full, preview_par.width, preview_par.height);
        if preview_par.data != expected.data {
            let (i, (&a, &b)) = preview_par
                .data
                .iter()
                .zip(&expected.data)
                .enumerate()
                .find(|(_, (a, b))| a != b)
                .unwrap();
            panic!(
                "{name} tile-by-tile preview ({}x{}) differs from full decode ({}x{}) + downscale at byte {i} (px {},{} ch {}): got {a}, expected {b}",
                preview_par.width,
                preview_par.height,
                full.width,
                full.height,
                (i / 4) % preview_par.width as usize,
                (i / 4) / preview_par.width as usize,
                i % 4,
            );
        }
    }
}

#[test]
fn preview_decodes_200mp_under_tight_per_tile_pixel_limit() {
    let Some(bytes) = fixture("real/heif_special__200MP.heic") else {
        return;
    };
    // 1 MP limit rejects the 200 MP full canvas decode...
    let tight = Options::new().with_max_pixels(1_000_000);
    assert!(decode_rgba8(&bytes, &tight).is_err());

    // ...but tile-by-tile preview decodes 512x512 tiles directly into a 512px-bounded preview.
    let preview_opts =
        PreviewOptions::new(512, 512).with_decode_options(tight.with_fallback_to_coded_color(true));
    let preview = decode_preview_rgba8(&bytes, &preview_opts).unwrap();
    assert_eq!((preview.width, preview.height), (384, 512));
    assert_eq!(preview.data.len(), 384 * 512 * 4);
}

#[test]
fn preview_preserves_alpha_and_never_upscales() {
    let Some(bytes) = fixture("real/heif__RGBA_8__128x128.heif") else {
        return;
    };
    let info = heifer::read_info(&bytes).unwrap();
    let no_upscale = decode_preview_rgba8(&bytes, &PreviewOptions::new(10_000, 10_000)).unwrap();
    assert_eq!(
        (no_upscale.width, no_upscale.height),
        (info.width, info.height)
    );
    assert!(no_upscale.has_alpha);

    let small = decode_preview_rgba8(&bytes, &PreviewOptions::new(64, 64)).unwrap();
    assert!(small.width <= 64 && small.height <= 64);
    assert!(small.has_alpha);
}
