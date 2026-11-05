//! Full decoding of arbitrary bytes as a HEIF file, and IPTC parsing: must never panic.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let options = heifer::Options {
        max_threads: 1,
        max_pixels: 1 << 17,
    };
    let from_slice = heifer::decode_with_options(data, &options);
    // The reader path must agree with the slice path.
    let from_reader = heifer::decode_reader_with_options(std::io::Cursor::new(data), &options);
    if let (Ok(a), Ok(b)) = (&from_slice, &from_reader) {
        assert_eq!(a, b, "reader and slice decodes differ");
    }
    // IPTC: from the file, and the bytes themselves as an IIM block.
    if let Ok(Some(seq)) = heifer::read_sequence(data) {
        if let Some(k) = seq.frames.iter().position(|f| f.keyframe) {
            let _ = heifer::decode_sequence_frame(data, k);
        }
    }
    if let Ok(Some(block)) = heifer::read_iptc(data) {
        let _ = heifer::Iptc::parse(&block).map(|i| (i.caption(), i.keywords()));
    }
    let _ = heifer::Iptc::parse(data).map(|i| (i.caption(), i.keywords(), i.date_created()));
});
