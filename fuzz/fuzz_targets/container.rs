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
            let _ = file.region_item(item.id);
            let _ = file.user_descriptions(item.id);
        }
    }
    if let Ok(tracks) = heifer_isobmff::track::parse_tracks(data) {
        for track in &tracks {
            for i in 0..track.samples.len().min(64) {
                if let (Some(config), Ok(sample)) = (&track.hevc_config, track.sample_data(data, i))
                {
                    let _ = heifer_isobmff::hevc_annex_b(config, sample);
                }
            }
        }
    }
});
