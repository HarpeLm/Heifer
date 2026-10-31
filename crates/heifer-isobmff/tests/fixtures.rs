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

/// `(id, construction_method, extents)`, with extents as `(extent_index, offset, length)`.
type TestItem<'a> = (u16, u16, &'a [(u32, u32, u32)]);

/// A minimal HEIF file whose items are `(id, construction_method, extents)`, with extents as
/// `(extent_index, offset, length)`, `iloc` references as `(from, to)` and an `idat` box.
fn file_with_constructed_items(
    items: &[TestItem<'_>],
    iloc_refs: &[(u16, &[u16])],
    idat: &[u8],
) -> Vec<u8> {
    // Version 1; offset, length and index sizes 4, no base offset.
    let mut iloc = vec![1, 0, 0, 0, 0x44, 0x04];
    iloc.extend_from_slice(&(items.len() as u16).to_be_bytes());
    let mut iinf = vec![0, 0, 0, 0];
    iinf.extend_from_slice(&(items.len() as u16).to_be_bytes());
    for &(id, method, extents) in items {
        iloc.extend_from_slice(&id.to_be_bytes());
        iloc.extend_from_slice(&method.to_be_bytes());
        iloc.extend_from_slice(&[0, 0]); // data_reference_index
        iloc.extend_from_slice(&(extents.len() as u16).to_be_bytes());
        for &(index, offset, length) in extents {
            for v in [index, offset, length] {
                iloc.extend_from_slice(&v.to_be_bytes());
            }
        }
        let mut infe = vec![2, 0, 0, 0];
        infe.extend_from_slice(&id.to_be_bytes());
        infe.extend_from_slice(&[0, 0]);
        infe.extend_from_slice(b"raw \0");
        iinf.extend(bx(b"infe", &infe));
    }
    let mut iref = vec![0, 0, 0, 0];
    for &(from, to) in iloc_refs {
        let mut r = from.to_be_bytes().to_vec();
        r.extend_from_slice(&(to.len() as u16).to_be_bytes());
        to.iter()
            .for_each(|t| r.extend_from_slice(&t.to_be_bytes()));
        iref.extend(bx(b"iloc", &r));
    }
    let mut hdlr = vec![0; 8];
    hdlr.extend_from_slice(b"pict");
    hdlr.extend_from_slice(&[0; 13]);
    let mut meta = vec![0, 0, 0, 0];
    meta.extend(bx(b"hdlr", &hdlr));
    meta.extend(bx(b"pitm", &[0, 0, 0, 0, 0, 1]));
    meta.extend(bx(b"iinf", &iinf));
    meta.extend(bx(b"iloc", &iloc));
    meta.extend(bx(b"iref", &iref));
    meta.extend(bx(b"idat", idat));
    let mut file = bx(b"ftyp", b"heic\0\0\0\0mif1heic");
    file.extend(bx(b"meta", &meta));
    file
}

#[test]
fn construction_method_item() {
    let data = file_with_constructed_items(
        &[
            (1, 1, &[(0, 0, 8)]),            // "ABCDEFGH" from idat
            (2, 2, &[(1, 2, 3), (1, 0, 2)]), // "CDE" + "AB" from item 1
            (3, 2, &[(0, 1, 0)]),            // index 0 means 1: item 2 from byte 1
            (4, 2, &[(1, 6, 2)]),            // a single range of item 1: borrowed
            (5, 2, &[(2, 0, 1), (1, 0, 1)]), // two sources: item 4 then item 1
        ],
        &[(2, &[1]), (3, &[2]), (4, &[1]), (5, &[1, 4])],
        b"ABCDEFGH",
    );
    let file = HeifFile::parse(&data).unwrap();
    assert_eq!(*file.item_data(2).unwrap(), *b"CDEAB");
    assert_eq!(*file.item_data(3).unwrap(), *b"DEAB");
    let borrowed = file.item_data(4).unwrap();
    assert!(matches!(borrowed, std::borrow::Cow::Borrowed(_)));
    assert_eq!(*borrowed, *b"GH");
    assert_eq!(*file.item_data(5).unwrap(), *b"GA");
}

#[test]
fn construction_method_item_errors() {
    let data = file_with_constructed_items(
        &[
            (1, 1, &[(0, 0, 4)]),
            (2, 2, &[(2, 0, 1)]), // only one `iloc` reference
            (3, 2, &[(1, 0, 1)]), // no `iloc` reference at all
            (4, 2, &[(1, 0, 0)]), // references itself
            (5, 2, &[(1, 2, 4)]), // past the end of item 1
        ],
        &[(2, &[1]), (4, &[4]), (5, &[1])],
        b"ABCD",
    );
    let file = HeifFile::parse(&data).unwrap();
    assert!(file.item_data(1).is_ok());
    for id in 2..=5 {
        assert!(file.item_data(id).is_err(), "item {id}");
    }
}

#[test]
fn construction_method_item_cannot_blow_up() {
    // Each item uses the previous one twice, through two `iloc` references to it: without
    // limits, item 40 would need 2^40 constructions of item 1.
    let extents: &[(u32, u32, u32)] = &[(1, 0, 0), (2, 0, 0)];
    let mut items: Vec<TestItem<'_>> = vec![(1, 1, &[(0, 0, 1)])];
    let mut targets = Vec::new();
    for id in 2..=40u16 {
        items.push((id, 2, extents));
        targets.push([id - 1, id - 1]);
    }
    let refs: Vec<(u16, &[u16])> = (2..=40u16)
        .zip(&targets)
        .map(|(id, t)| (id, t.as_slice()))
        .collect();
    let data = file_with_constructed_items(&items, &refs, b"A");
    let file = HeifFile::parse(&data).unwrap();
    assert_eq!(*file.item_data(3).unwrap(), *b"AAAA");
    assert!(file.item_data(40).is_err());
}
