//! Decoding of uncompressed images (`unci`, ISO/IEC 23001-17) as primary images.
//!
//! The synthetic files exercise every layout the decoder supports. The libheif fixtures at the
//! end are optional: run `scripts/fetch-fixtures.sh` to download them.

use heifer::Error;
use heifer::image::Image;

/// One component of a synthetic uncompressed frame.
#[derive(Clone, Copy)]
struct Component {
    depth: u8,
    align: u8,
}

const fn c8() -> Component {
    Component { depth: 8, align: 0 }
}

/// Layout of a synthetic uncompressed frame, mirroring the fields of `uncC` and `cmpd`.
struct Layout {
    width: u32,
    height: u32,
    components: Vec<Component>,
    /// `cmpd` component types; empty to omit the `cmpd` box.
    types: Vec<u16>,
    sampling: u8,
    interleave: u8,
    pixel_size: u32,
    row_align_size: u32,
    tile_align_size: u32,
    columns: u32,
    rows: u32,
    block_size: u8,
    little_endian: bool,
}

impl Layout {
    fn new(width: u32, height: u32, components: &[Component], types: &[u16]) -> Self {
        Self {
            width,
            height,
            components: components.to_vec(),
            types: types.to_vec(),
            sampling: 0,
            interleave: 0,
            pixel_size: 0,
            row_align_size: 0,
            tile_align_size: 0,
            columns: 1,
            rows: 1,
            block_size: 0,
            little_endian: false,
        }
    }
}

fn bx(kind: &[u8; 4], content: &[u8]) -> Vec<u8> {
    let mut b = ((content.len() + 8) as u32).to_be_bytes().to_vec();
    b.extend_from_slice(kind);
    b.extend_from_slice(content);
    b
}

fn full_box(kind: &[u8; 4], version: u8, flags: u32, content: &[u8]) -> Vec<u8> {
    let mut body = ((u32::from(version) << 24) | (flags & 0xFF_FFFF))
        .to_be_bytes()
        .to_vec();
    body.extend_from_slice(content);
    bx(kind, &body)
}

fn ispe(width: u32, height: u32) -> Vec<u8> {
    let mut content = width.to_be_bytes().to_vec();
    content.extend_from_slice(&height.to_be_bytes());
    full_box(b"ispe", 0, 0, &content)
}

fn cmpd(types: &[u16]) -> Vec<u8> {
    let mut content = (types.len() as u32).to_be_bytes().to_vec();
    for &component_type in types {
        content.extend_from_slice(&component_type.to_be_bytes());
    }
    bx(b"cmpd", &content)
}

fn uncc(layout: &Layout) -> Vec<u8> {
    let mut content = Vec::new();
    content.extend_from_slice(&0u32.to_be_bytes()); // profile
    content.extend_from_slice(&(layout.components.len() as u32).to_be_bytes());
    for (index, component) in layout.components.iter().enumerate() {
        content.extend_from_slice(&(index as u16).to_be_bytes());
        content.push(component.depth - 1);
        content.push(0); // unsigned
        content.push(component.align);
    }
    content.push(layout.sampling);
    content.push(layout.interleave);
    content.push(layout.block_size);
    content.push(if layout.little_endian { 0x80 } else { 0 });
    content.extend_from_slice(&layout.pixel_size.to_be_bytes());
    content.extend_from_slice(&layout.row_align_size.to_be_bytes());
    content.extend_from_slice(&layout.tile_align_size.to_be_bytes());
    content.extend_from_slice(&(layout.columns - 1).to_be_bytes());
    content.extend_from_slice(&(layout.rows - 1).to_be_bytes());
    full_box(b"uncC", 0, 0, &content)
}

fn infe(id: u16, item_type: &[u8; 4]) -> Vec<u8> {
    let mut content = id.to_be_bytes().to_vec();
    content.extend_from_slice(&0u16.to_be_bytes()); // item_protection_index
    content.extend_from_slice(item_type);
    content.push(0); // name
    full_box(b"infe", 2, 0, &content)
}

/// A minimal HEIF file whose primary image is one uncompressed item stored in `idat`.
fn uncompressed_file(layout: &Layout, data: &[u8]) -> Vec<u8> {
    let mut iloc = vec![0x44, 0x00];
    iloc.extend_from_slice(&1u16.to_be_bytes());
    iloc.extend_from_slice(&1u16.to_be_bytes());
    iloc.extend_from_slice(&1u16.to_be_bytes()); // construction method 1 (idat)
    iloc.extend_from_slice(&0u16.to_be_bytes()); // data_reference_index
    iloc.extend_from_slice(&1u16.to_be_bytes()); // extent_count
    iloc.extend_from_slice(&0u32.to_be_bytes()); // extent_offset
    iloc.extend_from_slice(&(data.len() as u32).to_be_bytes());

    let mut ipco = ispe(layout.width, layout.height);
    if !layout.types.is_empty() {
        ipco.extend(cmpd(&layout.types));
    }
    ipco.extend(uncc(layout));
    let property_count = if layout.types.is_empty() { 2 } else { 3 };
    let mut ipma = 1u32.to_be_bytes().to_vec();
    ipma.extend_from_slice(&1u16.to_be_bytes());
    ipma.push(property_count);
    for index in 1..=property_count {
        ipma.push(index);
    }

    let mut hdlr = vec![0; 4];
    hdlr.extend_from_slice(b"pict");
    hdlr.extend_from_slice(&[0; 13]);
    let mut iinf = 1u16.to_be_bytes().to_vec();
    iinf.extend(infe(1, b"unci"));
    let mut meta = full_box(b"hdlr", 0, 0, &hdlr);
    meta.extend(full_box(b"pitm", 0, 0, &1u16.to_be_bytes()));
    meta.extend(full_box(b"iloc", 1, 0, &iloc));
    meta.extend(full_box(b"iinf", 0, 0, &iinf));
    meta.extend(bx(
        b"iprp",
        &[bx(b"ipco", &ipco), full_box(b"ipma", 0, 0, &ipma)].concat(),
    ));
    meta.extend(bx(b"idat", data));

    let mut bytes = bx(b"ftyp", b"mif1\0\0\0\0mif1heic");
    bytes.extend(full_box(b"meta", 0, 0, &meta));
    bytes
}

fn rgba(image: &Image, x: u32, y: u32) -> [u16; 4] {
    let i = (y as usize * image.width as usize + x as usize) * 4;
    [
        image.data[i],
        image.data[i + 1],
        image.data[i + 2],
        image.data[i + 3],
    ]
}

#[test]
fn component_interleave_rgb_is_decoded() {
    let layout = Layout::new(2, 2, &[c8(), c8(), c8()], &[4, 5, 6]);
    let data = [
        255, 0, 0, 255, // red
        0, 255, 255, 0, // green
        0, 0, 128, 255, // blue
    ];
    let bytes = uncompressed_file(&layout, &data);
    let image = heifer::decode(&bytes).unwrap();
    assert_eq!((image.width, image.height, image.bit_depth), (2, 2, 8));
    assert!(!image.has_alpha);
    assert_eq!(rgba(&image, 0, 0), [255, 0, 0, 255]);
    assert_eq!(rgba(&image, 1, 0), [0, 255, 0, 255]);
    assert_eq!(rgba(&image, 0, 1), [0, 255, 128, 255]);
    assert_eq!(rgba(&image, 1, 1), [255, 0, 255, 255]);
}

#[test]
fn pixel_interleave_rgba_is_decoded() {
    let mut layout = Layout::new(2, 2, &[c8(), c8(), c8(), c8()], &[4, 5, 6, 7]);
    layout.interleave = 1;
    let data = [
        255, 0, 0, 255, 0, 255, 0, 128, // row 0
        0, 0, 255, 64, 10, 20, 30, 40, // row 1
    ];
    let bytes = uncompressed_file(&layout, &data);
    let image = heifer::decode(&bytes).unwrap();
    assert!(image.has_alpha);
    assert_eq!(rgba(&image, 0, 0), [255, 0, 0, 255]);
    assert_eq!(rgba(&image, 1, 0), [0, 255, 0, 128]);
    assert_eq!(rgba(&image, 0, 1), [0, 0, 255, 64]);
    assert_eq!(rgba(&image, 1, 1), [10, 20, 30, 40]);
}

#[test]
fn row_interleave_skips_alignment_padding() {
    let mut layout = Layout::new(2, 2, &[c8(), c8(), c8()], &[4, 5, 6]);
    layout.interleave = 3;
    layout.row_align_size = 4;
    let data = [
        255, 0, 9, 9, 0, 255, 9, 9, 0, 0, 9, 9, // row 0, R, G, B
        0, 255, 7, 7, 255, 0, 7, 7, 128, 255, 7, 7, // row 1
    ];
    let bytes = uncompressed_file(&layout, &data);
    let image = heifer::decode(&bytes).unwrap();
    assert_eq!(rgba(&image, 0, 0), [255, 0, 0, 255]);
    assert_eq!(rgba(&image, 1, 0), [0, 255, 0, 255]);
    assert_eq!(rgba(&image, 0, 1), [0, 255, 128, 255]);
    assert_eq!(rgba(&image, 1, 1), [255, 0, 255, 255]);
}

#[test]
fn component_interleave_tiles_are_row_major() {
    let mut layout = Layout::new(4, 2, &[c8(), c8(), c8()], &[4, 5, 6]);
    layout.columns = 2;
    let data = [
        1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, // tile 0: R, G, B
        13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, // tile 1
    ];
    let bytes = uncompressed_file(&layout, &data);
    let image = heifer::decode(&bytes).unwrap();
    assert_eq!(rgba(&image, 0, 0), [1, 5, 9, 255]);
    assert_eq!(rgba(&image, 1, 0), [2, 6, 10, 255]);
    assert_eq!(rgba(&image, 2, 0), [13, 17, 21, 255]);
    assert_eq!(rgba(&image, 3, 1), [16, 20, 24, 255]);
}

#[test]
fn tile_component_interleave_stores_components_first() {
    let mut layout = Layout::new(4, 2, &[c8(), c8(), c8()], &[4, 5, 6]);
    layout.columns = 2;
    layout.interleave = 4;
    let data = [
        1, 2, 3, 4, 13, 14, 15, 16, // R: tile 0, tile 1
        5, 6, 7, 8, 17, 18, 19, 20, // G
        9, 10, 11, 12, 21, 22, 23, 24, // B
    ];
    let bytes = uncompressed_file(&layout, &data);
    let image = heifer::decode(&bytes).unwrap();
    assert_eq!(rgba(&image, 0, 0), [1, 5, 9, 255]);
    assert_eq!(rgba(&image, 2, 0), [13, 17, 21, 255]);
    assert_eq!(rgba(&image, 0, 1), [3, 7, 11, 255]);
    assert_eq!(rgba(&image, 3, 1), [16, 20, 24, 255]);
}

#[test]
fn packed_seven_bit_components_are_decoded() {
    let mut layout = Layout::new(
        1,
        1,
        &[
            Component { depth: 7, align: 0 },
            Component { depth: 7, align: 0 },
            Component { depth: 7, align: 0 },
        ],
        &[4, 5, 6],
    );
    layout.interleave = 1;
    // 1111111 0000001 1000000, padded to three bytes.
    let data = [0xFE, 0x06, 0x00];
    let bytes = uncompressed_file(&layout, &data);
    let image = heifer::decode(&bytes).unwrap();
    assert_eq!(image.bit_depth, 7);
    assert_eq!(rgba(&image, 0, 0), [127, 1, 64, 127]);
}

#[test]
fn byte_aligned_components_are_decoded() {
    let components = [
        Component {
            depth: 12,
            align: 2,
        },
        Component {
            depth: 12,
            align: 2,
        },
        Component {
            depth: 12,
            align: 2,
        },
    ];
    let mut layout = Layout::new(1, 1, &components, &[4, 5, 6]);
    layout.interleave = 1;
    let data = [0x01, 0x23, 0x04, 0x56, 0x07, 0x89];
    let bytes = uncompressed_file(&layout, &data);
    let image = heifer::decode(&bytes).unwrap();
    assert_eq!(image.bit_depth, 12);
    assert_eq!(rgba(&image, 0, 0), [0x123, 0x456, 0x789, 0xFFF]);
}

#[test]
fn sixteen_bit_components_are_decoded() {
    let components = [
        Component {
            depth: 16,
            align: 0,
        },
        Component {
            depth: 16,
            align: 0,
        },
        Component {
            depth: 16,
            align: 0,
        },
    ];
    let layout = Layout::new(1, 1, &components, &[4, 5, 6]);
    let data = [0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC];
    let bytes = uncompressed_file(&layout, &data);
    let image = heifer::decode(&bytes).unwrap();
    assert_eq!(image.bit_depth, 16);
    assert_eq!(rgba(&image, 0, 0), [0x1234, 0x5678, 0x9ABC, 0xFFFF]);
}

#[test]
fn ycbcr_420_component_interleave_is_decoded() {
    let mut layout = Layout::new(4, 2, &[c8(), c8(), c8()], &[1, 2, 3]);
    layout.sampling = 2;
    let mut data = vec![128u8; 4 * 2]; // Y
    data.extend_from_slice(&[128, 128]); // Cb
    data.extend_from_slice(&[128, 128]); // Cr
    let bytes = uncompressed_file(&layout, &data);
    let image = heifer::decode(&bytes).unwrap();
    assert_eq!((image.width, image.height, image.bit_depth), (4, 2, 8));
    for y in 0..2 {
        for x in 0..4 {
            assert_eq!(rgba(&image, x, y), [128, 128, 128, 255], "pixel {x},{y}");
        }
    }
}

#[test]
fn ycbcr_420_mixed_interleave_is_decoded() {
    let mut layout = Layout::new(4, 2, &[c8(), c8(), c8()], &[1, 2, 3]);
    layout.sampling = 2;
    layout.interleave = 2;
    let mut data = vec![128u8; 4 * 2]; // Y, two rows
    data.extend_from_slice(&[128, 128, 128, 128]); // Cb, Cr for row 0
    data.extend_from_slice(&[128, 128, 128, 128]); // Cb, Cr for row 1
    let bytes = uncompressed_file(&layout, &data);
    let image = heifer::decode(&bytes).unwrap();
    for y in 0..2 {
        for x in 0..4 {
            assert_eq!(rgba(&image, x, y), [128, 128, 128, 255], "pixel {x},{y}");
        }
    }
}

#[test]
fn eight_bit_wide_decoding_matches() {
    let layout = Layout::new(2, 2, &[c8(), c8(), c8()], &[4, 5, 6]);
    let data = [255, 0, 0, 255, 0, 255, 255, 0, 0, 0, 128, 255];
    let bytes = uncompressed_file(&layout, &data);
    let wide = heifer::decode(&bytes).unwrap();
    let narrow = heifer::decode_rgba8(&bytes, &heifer::Options::default()).unwrap();
    assert_eq!(narrow.bit_depth, 8);
    assert_eq!(wide.width, narrow.width);
    for (a, b) in wide.data.iter().zip(&narrow.data) {
        assert_eq!(*a as u8, *b);
    }
}

#[test]
fn unsupported_uncompressed_layouts_are_refused() {
    let mut layout = Layout::new(2, 2, &[c8(), c8(), c8()], &[4, 5, 6]);
    layout.block_size = 1;
    let error = heifer::decode(&uncompressed_file(&layout, &[0; 12])).unwrap_err();
    assert!(
        matches!(&error, Error::Unsupported(m) if m.contains("blocks")),
        "{error}"
    );

    let mut layout = Layout::new(2, 2, &[c8(), c8(), c8()], &[4, 5, 6]);
    layout.little_endian = true;
    let error = heifer::decode(&uncompressed_file(&layout, &[0; 12])).unwrap_err();
    assert!(
        matches!(&error, Error::Unsupported(m) if m.contains("little-endian")),
        "{error}"
    );

    let mut layout = Layout::new(2, 2, &[c8()], &[0]);
    layout.interleave = 5;
    let error = heifer::decode(&uncompressed_file(&layout, &[0; 4])).unwrap_err();
    assert!(
        matches!(&error, Error::Unsupported(m) if m.contains("multi-Y")),
        "{error}"
    );
}

fn fixture(name: &str) -> Option<Vec<u8>> {
    let path = format!(
        "{}/../../tests/fixtures/real/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(_) => {
            eprintln!("skipping: {path} not found (run scripts/fetch-fixtures.sh)");
            None
        }
    }
}

#[test]
fn libheif_rgb_fixtures_match_across_interleave_modes() {
    const FILES: [&str; 5] = [
        "libheif__uncompressed_comp_RGB.heif",
        "libheif__uncompressed_comp_RGB_tiled.heif",
        "libheif__uncompressed_pix_RGB_tiled.heif",
        "libheif__uncompressed_row_RGB_tiled.heif",
        "libheif__uncompressed_tile_RGB_tiled.heif",
    ];
    for name in FILES {
        let Some(bytes) = fixture(name) else {
            return;
        };
        let image = heifer::decode(&bytes).unwrap();
        assert_eq!(
            (image.width, image.height, image.bit_depth),
            (30, 20, 8),
            "{name}"
        );
        assert!(!image.has_alpha, "{name}");
        assert_eq!(rgba(&image, 0, 0), [255, 0, 0, 255], "{name}");
        assert_eq!(rgba(&image, 4, 0), [0, 128, 0, 255], "{name}");
        assert_eq!(rgba(&image, 8, 0), [0, 0, 255, 255], "{name}");
        assert_eq!(rgba(&image, 28, 0), [128, 128, 128, 255], "{name}");
        assert_eq!(rgba(&image, 8, 4), [255, 255, 255, 255], "{name}");
        assert_eq!(rgba(&image, 8, 8), [0, 0, 0, 255], "{name}");
    }
}

#[test]
fn libheif_alpha_fixture_is_decoded() {
    let Some(bytes) = fixture("libheif__uncompressed_comp_ABGR.heif") else {
        return;
    };
    let image = heifer::decode(&bytes).unwrap();
    assert!(image.has_alpha);
    assert_eq!(rgba(&image, 0, 0), [255, 0, 0, 255]);
    assert_eq!(rgba(&image, 4, 4), [0, 0, 255, 204]);
}

#[test]
fn libheif_mono_fixture_is_decoded() {
    let Some(bytes) = fixture("libheif__uncompressed_comp_M.heif") else {
        return;
    };
    let image = heifer::decode(&bytes).unwrap();
    assert!(!image.has_alpha);
    assert_eq!(rgba(&image, 0, 0), [255, 255, 255, 255]);
    assert_eq!(rgba(&image, 4, 0), [0, 0, 0, 255]);
    assert_eq!(rgba(&image, 8, 4), [255, 255, 255, 255]);
}

#[test]
fn libheif_sixteen_bit_fixture_is_decoded() {
    let Some(bytes) = fixture("libheif__uncompressed_comp_B16R16G16.heif") else {
        return;
    };
    let image = heifer::decode(&bytes).unwrap();
    assert_eq!(image.bit_depth, 16);
    assert_eq!(rgba(&image, 0, 0), [65535, 0, 0, 65535]);
    assert_eq!(rgba(&image, 4, 0), [0, 32896, 0, 65535]);
}

#[test]
fn libheif_ycbcr_fixture_is_decoded() {
    let Some(bytes) = fixture("libheif__uncompressed_comp_YUV_420.heif") else {
        return;
    };
    let image = heifer::decode(&bytes).unwrap();
    assert_eq!((image.width, image.height), (32, 20));
    let red = rgba(&image, 0, 0);
    assert!(red[0] > 240 && red[1] < 10 && red[2] < 10, "{red:?}");
    let blue = rgba(&image, 8, 0);
    assert!(blue[0] < 10 && blue[1] < 10 && blue[2] > 240, "{blue:?}");
}
