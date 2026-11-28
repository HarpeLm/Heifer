//! Reads the OBUs and sequence headers of real AVIF files and checks them against the container.
//!
//! Uses the AVIF fixtures of `scripts/fetch-fixtures.sh`. Set `HEIFER_AVIF_DIR` to also check
//! every `.avif` file of another directory (for example the whole AOMediaCodec/av1-avif set).

use std::path::{Path, PathBuf};

use heifer_av1_dec::{ObuType, SequenceHeader, obus};
use heifer_isobmff::HeifFile;

fn avif_files() -> Vec<PathBuf> {
    let mut dirs = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/avif")];
    dirs.extend(std::env::var_os("HEIFER_AVIF_DIR").map(PathBuf::from));
    let mut files: Vec<PathBuf> = dirs
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flatten()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "avif"))
        .collect();
    files.sort();
    files
}

/// The single sequence header among `data`'s OBUs, if any; every OBU must parse.
fn sequence_header(data: &[u8], what: &str) -> Option<SequenceHeader> {
    let mut header = None;
    for obu in obus(data) {
        let obu = obu.unwrap_or_else(|e| panic!("{what}: {e}"));
        if obu.obu_type == ObuType::SequenceHeader {
            let h = SequenceHeader::parse(obu.payload).unwrap_or_else(|e| panic!("{what}: {e}"));
            if let Some(previous) = &header {
                assert_eq!(previous, &h, "{what}: two different sequence headers");
            }
            header = Some(h);
        }
    }
    header
}

#[test]
fn sequence_headers_match_the_container() {
    let files = avif_files();
    if files.is_empty() {
        eprintln!("skipping: run scripts/fetch-fixtures.sh");
        return;
    }
    let mut items = 0;
    for path in &files {
        let name = path.file_name().unwrap().to_string_lossy();
        let bytes = std::fs::read(path).unwrap();
        let file = HeifFile::parse(&bytes).unwrap();
        for item in file.items.iter().filter(|i| i.item_type.0 == *b"av01") {
            let what = format!("{name} item {}", item.id);
            let config = file.av1_config(item.id).unwrap().expect("av1C");
            let data = file.item_data(item.id).unwrap();
            // The sequence header is in the item data, in `av1C`, or in both. The one in the data
            // wins, as in dav1d: Microsoft/Tomsk_with_thumbnails.avif gives its 320×180 thumbnail
            // the main image's 1280×720 header in `av1C`.
            let from_config = sequence_header(&config.config_obus, &what);
            let h = sequence_header(&data, &what)
                .or(from_config)
                .unwrap_or_else(|| panic!("{what}: no sequence header"));
            assert_eq!(h.profile, config.profile, "{what}");
            assert_eq!(h.color.bit_depth, config.bit_depth, "{what}");
            assert_eq!(h.color.monochrome, config.monochrome, "{what}");
            assert_eq!(h.color.subsampling_x, config.subsampling_x, "{what}");
            assert_eq!(h.color.subsampling_y, config.subsampling_y, "{what}");
            if let Some((w, h_px)) = file.image_size(item.id).unwrap() {
                assert!(
                    w <= h.max_frame_width && h_px <= h.max_frame_height,
                    "{what}: ispe {w}×{h_px} larger than {}×{}",
                    h.max_frame_width,
                    h.max_frame_height
                );
            }
            items += 1;
        }
    }
    eprintln!("{} files, {items} AV1 items", files.len());
    assert!(items > 0);
}
