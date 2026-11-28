//! Differential fuzz target for bounded-memory preview decoding (`decode_preview_rgba8`,
//! `decode_reader_preview_rgba8`) and reader sRGB decoding (`decode_reader_srgb_rgba8`,
//! `Options::fallback_to_coded_color`).
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let decode = heifer::Options::new()
        .with_max_threads(1)
        .with_max_pixels(1 << 17)
        .with_fallback_to_coded_color(true);

    // Bounded-memory preview: slice vs reader parity, with sRGB conversion + fallback.
    let preview_opts = heifer::PreviewOptions::new(128, 128)
        .with_decode_options(decode)
        .with_max_canvas_pixels(1 << 19)
        .with_to_srgb(true);
    let from_slice = heifer::decode_preview_rgba8(data, &preview_opts);
    let from_reader =
        heifer::decode_reader_preview_rgba8(std::io::Cursor::new(data), &preview_opts);
    if let (Ok(a), Ok(b)) = (&from_slice, &from_reader) {
        assert_eq!(a, b, "decode_preview_rgba8 and reader differ");
    }
    if let Ok(img) = &from_slice {
        assert!(img.width > 0 && img.width <= 128);
        assert!(img.height > 0 && img.height <= 128);
        assert_eq!(img.data.len(), img.width as usize * img.height as usize * 4);
    }

    // Raw coded-color preview path (`to_srgb = false`).
    let raw_preview = preview_opts.with_to_srgb(false);
    if let Ok(img) = heifer::decode_preview_rgba8(data, &raw_preview) {
        assert!(img.width > 0 && img.width <= 128);
        assert!(img.height > 0 && img.height <= 128);
        assert_eq!(img.data.len(), img.width as usize * img.height as usize * 4);
    }

    // Full sRGB RGBA8 slice vs reader parity with ICC fallback enabled.
    let srgb_slice = heifer::decode_srgb_rgba8(data, &decode);
    let srgb_reader = heifer::decode_reader_srgb_rgba8(std::io::Cursor::new(data), &decode);
    if let (Ok(a), Ok(b)) = (&srgb_slice, &srgb_reader) {
        assert_eq!(
            a, b,
            "decode_srgb_rgba8 and decode_reader_srgb_rgba8 differ"
        );
    }
});
