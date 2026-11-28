//! AV1 OBUs and sequence headers on arbitrary bytes, raw and inside AVIF items: must never panic.
#![no_main]
use heifer_av1_dec::{ObuType, SequenceHeader, obus};
use libfuzzer_sys::fuzz_target;

fn read(data: &[u8]) {
    for obu in obus(data).flatten() {
        if obu.obu_type == ObuType::SequenceHeader {
            let _ = SequenceHeader::parse(obu.payload);
        }
    }
}

fuzz_target!(|data: &[u8]| {
    read(data);
    if let Ok(file) = heifer_isobmff::HeifFile::parse(data) {
        for item in &file.items {
            if let Ok(Some(config)) = file.av1_config(item.id) {
                read(&config.config_obus);
            }
            if item.item_type.0 == *b"av01"
                && let Ok(bytes) = file.item_data(item.id)
            {
                read(&bytes);
            }
        }
    }
});
