//! Differential fuzz target for `read_info`, `read_info_reader`, `decode_reader_rgba8`, and `decode_with_options`.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let options = heifer::Options::new()
        .with_max_threads(1)
        .with_max_pixels(1 << 17);
    let info_slice = heifer::read_info(data);
    let info_reader = heifer::read_info_reader(std::io::Cursor::new(data));
    if let (Ok(a), Ok(b)) = (&info_slice, &info_reader) {
        assert_eq!(a, b, "read_info and read_info_reader differ");
    }

    let decoded = heifer::decode_with_options(data, &options);
    let decoded_rgba8 = heifer::decode_reader_rgba8(std::io::Cursor::new(data), &options);

    if let Ok(img) = &decoded {
        let info = info_slice
            .as_ref()
            .expect("read_info must succeed when decode succeeds");
        assert_eq!(
            (info.width, info.height, info.bit_depth),
            (img.width, img.height, img.bit_depth),
            "read_info and decode dimensions or bit depth differ"
        );
        assert!(info.has_primary_image);
        if info.item_type.0 != *b"iovl" {
            assert_eq!(info.has_alpha, img.has_alpha);
        } else {
            assert!(!img.has_alpha || info.has_alpha);
        }
    }

    if let (Ok(wide), Ok(narrow)) = (&decoded, &decoded_rgba8)
        && wide.bit_depth == 8
    {
        assert_eq!(wide.to_rgba8(), narrow.data, "8-bit reader decode differs");
    }
});
