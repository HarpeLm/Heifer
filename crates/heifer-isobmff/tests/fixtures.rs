//! Tests on real files. Run `scripts/fetch-fixtures.sh` first; missing files are skipped.

use heifer_isobmff::{FourCC, HeifFile};

fn load(name: &str) -> Option<Vec<u8>> {
    let path = format!("{}/../../tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let data = std::fs::read(&path).ok();
    if data.is_none() {
        eprintln!("skipping: {path} not found (run scripts/fetch-fixtures.sh)");
    }
    data
}

#[test]
fn single_image_with_thumbnail() {
    let Some(data) = load("single_image.heic") else {
        return;
    };
    let file = HeifFile::parse(&data).unwrap();
    assert!(file.file_type.has_brand(b"heic"));
    assert_eq!(file.primary_id, 1002);
    assert_eq!(file.image_size(1002).unwrap(), Some((1440, 960)));
    assert_eq!(file.referencing_items(1002, b"thmb"), vec![1005]);

    let config = file.hevc_config(1002).unwrap().unwrap();
    assert_eq!((config.chroma_format, config.bit_depth_luma), (1, 8));
    assert_eq!(config.nal_units.len(), 3, "VPS + SPS + PPS");

    let bitstream = file.hevc_bitstream(1002).unwrap();
    assert!(bitstream.starts_with(&[0, 0, 0, 1]));
    // First NAL unit is a VPS (type 32).
    assert_eq!(bitstream[4] >> 1 & 0x3F, 32);
}

#[test]
fn grid_of_four_tiles() {
    let Some(data) = load("grid.heic") else {
        return;
    };
    let file = HeifFile::parse(&data).unwrap();
    let primary = file.primary_item().unwrap();
    assert_eq!(primary.item_type, FourCC(*b"grid"));

    let grid = file.grid(primary.id).unwrap();
    assert_eq!((grid.rows, grid.columns), (2, 2));
    assert_eq!((grid.output_width, grid.output_height), (960, 640));

    let tiles = file.referenced_items(primary.id, b"dimg");
    assert_eq!(tiles, &[1002, 1005, 1008, 1011]);
    for &tile in tiles {
        assert_eq!(file.image_size(tile).unwrap(), Some((480, 320)));
        assert!(!file.hevc_bitstream(tile).unwrap().is_empty());
    }
}

#[test]
fn alpha_auxiliary_image() {
    let Some(data) = load("alpha.heic") else {
        return;
    };
    let file = HeifFile::parse(&data).unwrap();
    let alpha = file.referencing_items(1005, b"auxl");
    assert_eq!(alpha, vec![1008]);
    assert!(file.item(1008).unwrap().hidden);
}

#[test]
fn all_fixtures_parse() {
    for name in [
        "single_image.heic",
        "grid.heic",
        "alpha.heic",
        "collection.heic",
        "burst.heic",
        "libheif_example.heic",
    ] {
        let Some(data) = load(name) else { continue };
        let file = HeifFile::parse(&data).unwrap_or_else(|e| panic!("{name}: {e}"));
        for item in file.items.iter().filter(|i| i.item_type.0 == *b"hvc1") {
            file.hevc_bitstream(item.id)
                .unwrap_or_else(|e| panic!("{name} item {}: {e}", item.id));
        }
    }
}

#[test]
fn truncated_files_never_panic() {
    let Some(data) = load("grid.heic") else {
        return;
    };
    for len in (0..1200).chain([data.len() / 2, data.len() - 1]) {
        let _ = HeifFile::parse(&data[..len.min(data.len())]).map(|f| {
            for item in &f.items {
                let _ = f.item_data(item.id);
                let _ = f.hevc_bitstream(item.id);
            }
        });
    }
}

fn bx(kind: &[u8; 4], content: &[u8]) -> Vec<u8> {
    let mut b = ((content.len() + 8) as u32).to_be_bytes().to_vec();
    b.extend_from_slice(kind);
    b.extend_from_slice(content);
    b
}

/// A minimal HEIF file with one `hvc1` item whose `iloc` entry has `extents` extents of
/// length 0 (meaning "until the end of the file").
fn file_with_repeated_extents(extents: u16) -> Vec<u8> {
    let mut iloc = vec![0, 0, 0, 0, 0x44, 0x00, 0, 1, 0, 1, 0, 0];
    iloc.extend_from_slice(&extents.to_be_bytes());
    for _ in 0..extents {
        iloc.extend_from_slice(&[0; 8]); // offset 0, length 0
    }
    let mut infe = vec![2, 0, 0, 0, 0, 1, 0, 0];
    infe.extend_from_slice(b"hvc1\0");
    let mut iinf = vec![0, 0, 0, 0, 0, 1];
    iinf.extend(bx(b"infe", &infe));
    let mut hdlr = vec![0; 8];
    hdlr.extend_from_slice(b"pict");
    hdlr.extend_from_slice(&[0; 13]);
    let mut meta = vec![0, 0, 0, 0];
    meta.extend(bx(b"hdlr", &hdlr));
    meta.extend(bx(b"pitm", &[0, 0, 0, 0, 0, 1]));
    meta.extend(bx(b"iinf", &iinf));
    meta.extend(bx(b"iloc", &iloc));
    let mut file = bx(b"ftyp", b"heic\0\0\0\0mif1heic");
    file.extend(bx(b"meta", &meta));
    file
}

#[test]
fn overlapping_extents_cannot_expand_data() {
    // Found by fuzzing: thousands of `iloc` extents of length 0 used to copy the whole file
    // thousands of times (a 5 MB file with 65535 extents would need over 300 GB).
    let one = file_with_repeated_extents(1);
    let file = HeifFile::parse(&one).unwrap();
    assert_eq!(file.item_data(1).unwrap().len(), one.len());

    let many = file_with_repeated_extents(u16::MAX);
    let file = HeifFile::parse(&many).unwrap();
    assert!(file.item_data(1).is_err());
    assert!(file.hevc_bitstream(1).is_err());
}
