//! Differential fuzz target for HDR gain map extraction (`read_gain_map_with_options`,
//! `read_gain_map_reader`) and HDR decoding (`decode_hdr_with_options`).
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let options = heifer::Options::new()
        .with_max_threads(1)
        .with_max_pixels(1 << 17);

    let from_slice = heifer::read_gain_map_with_options(data, &options);
    let from_reader = heifer::read_gain_map_reader(std::io::Cursor::new(data), &options);
    if let (Ok(a), Ok(b)) = (&from_slice, &from_reader) {
        assert_eq!(a, b, "read_gain_map and read_gain_map_reader differ");
    }
    if let Ok(Some(gm)) = &from_slice {
        assert!(gm.width > 0 && gm.height > 0);
        assert!(gm.headroom.is_finite() && gm.headroom >= 1.0);
        assert_eq!(gm.data.len(), gm.width as usize * gm.height as usize);
        let s = gm.sample(0, 0, gm.width, gm.height);
        assert!(s.is_finite());
    }

    if let Ok(hdr) = heifer::decode_hdr_with_options(data, &options, 4.0) {
        assert!(hdr.width > 0 && hdr.height > 0);
        assert!(hdr.headroom.is_finite() && hdr.headroom >= 1.0);
        assert_eq!(hdr.data.len(), hdr.width as usize * hdr.height as usize * 3);
    }
});
