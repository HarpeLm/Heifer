//! Fuzz target for `HeifDecoder` (lazy headers, pre-decode `Limits`, 8/16-bit read) and `HeifSequenceDecoder`.
#![no_main]
use image::ImageDecoder;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 12 {
        return;
    }
    // Derive limit and metadata toggles from trailing bytes so `ftyp` and box headers stay intact.
    let tail = &data[data.len().saturating_sub(8)..];
    let mix = tail
        .iter()
        .fold(0x9e37_79b9u32, |acc, &b| acc.rotate_left(7) ^ u32::from(b));
    let options = heifer::Options::new()
        .with_max_threads(1)
        .with_max_pixels(1 << 17);

    if let Ok(mut dec) = heifer::image_crate::HeifDecoder::from_bytes(data, &options) {
        let (w, h) = dec.dimensions();
        let _ = dec.color_type();
        let _ = dec.orientation();
        let total = dec.total_bytes();
        if mix & 1 != 0 {
            let _ = dec.icc_profile();
        }
        if mix & 2 != 0 {
            let _ = dec.exif_metadata();
        }
        if mix & 4 != 0 {
            let _ = dec.xmp_metadata();
        }
        let mut limits = image::Limits::no_limits();
        let max_w = (mix & 8 != 0).then(|| ((mix >> 4) & 0xff).saturating_mul(16).max(1));
        let max_h = (mix & 16 != 0).then(|| ((mix >> 12) & 0xff).saturating_mul(16).max(1));
        let max_alloc = (mix & 32 != 0).then(|| u64::from((mix >> 20) & 0xff) << 12);
        limits.max_image_width = max_w;
        limits.max_image_height = max_h;
        limits.max_alloc = max_alloc;
        if dec.set_limits(limits).is_ok() {
            if let Some(mw) = max_w {
                assert!(w <= mw);
            }
            if let Some(mh) = max_h {
                assert!(h <= mh);
            }
            if total <= (1 << 20) {
                let mut buf = vec![0u8; total as usize];
                if dec.read_image(&mut buf).is_ok() {
                    if let Ok(img) = heifer::decode_with_options(data, &options) {
                        assert_eq!((img.width, img.height), (w, h));
                    }
                }
            }
        }
    }

    if let Ok(seq) = heifer::image_crate::HeifSequenceDecoder::from_bytes(data.to_vec(), &options) {
        use image::AnimationDecoder;
        for frame in seq.into_frames().take(4) {
            let _ = frame;
        }
    }
});
