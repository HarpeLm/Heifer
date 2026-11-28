//! AV1 OBUs, headers, tiles and frame decoding on arbitrary bytes, raw and inside AVIF items:
//! must never panic.
#![no_main]
use heifer_av1_dec::{Decoder, ObuType, SequenceHeader, SymbolDecoder, frames, obus};
use libfuzzer_sys::fuzz_target;

fn read(data: &[u8], config_obus: &[u8]) {
    for obu in obus(data).flatten() {
        if obu.obu_type == ObuType::SequenceHeader {
            let _ = SequenceHeader::parse(obu.payload);
        }
    }
    let mut decoder = Decoder::new();
    for frame in frames(data, config_obus).into_iter().flatten() {
        for tile in &frame.tiles {
            if let Ok(mut symbols) = SymbolDecoder::new(tile.data, frame.header.disable_cdf_update)
            {
                let mut cdf = [4096, 12288, 32768, 0];
                for _ in 0..64 {
                    let _ = symbols.read_symbol(&mut cdf);
                    let _ = symbols.read_literal(3);
                }
                let _ = symbols.finish();
            }
        }
        // Keeps the planes small: the size limit is the caller's job.
        if frame.header.upscaled_width * frame.header.frame_height <= 1 << 20 {
            let _ = decoder.decode(&frame);
        }
    }
}

fuzz_target!(|data: &[u8]| {
    read(data, &[]);
    if let Ok(file) = heifer_isobmff::HeifFile::parse(data) {
        for item in &file.items {
            let config = file.av1_config(item.id).ok().flatten();
            let config_obus = config.as_ref().map_or(&[][..], |c| &c.config_obus);
            read(config_obus, &[]);
            if item.item_type.0 == *b"av01"
                && let Ok(bytes) = file.item_data(item.id)
            {
                read(&bytes, config_obus);
            }
        }
    }
});
