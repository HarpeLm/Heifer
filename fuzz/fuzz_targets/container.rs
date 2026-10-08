//! Container parsing and item data access on arbitrary bytes: must never panic.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(file) = heifer_isobmff::HeifFile::parse(data) {
        for item in &file.items {
            let _ = file.item_data(item.id);
            let _ = file.hevc_bitstream(item.id);
            let _ = file.grid(item.id);
            let _ = file.image_size(item.id);
        }
    }
});
