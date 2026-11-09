//! Auxiliary images: listing and decoding alpha planes and depth maps.

use heifer::heifer_hevc_enc::encoder::{
    ChromaFormat, CodingOptions, EncodedPicture, EncoderConfig, Planes, Planes16, encode_picture,
    encode_picture_with, encode_picture16_with,
};
use heifer::heifer_isobmff::boxes::HevcConfig;
use heifer::heifer_isobmff::writer::{ALPHA_URN, HeifWriter, ImageSpec};
use heifer::{
    DEPTH_URN, EncodeFormat, EncodeOptions, Error, decode_auxiliary, decode_auxiliary_urn,
};

fn load_real(name: &str) -> Option<Vec<u8>> {
    let path = format!(
        "{}/../../tests/fixtures/real/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let data = std::fs::read(&path).ok();
    if data.is_none() {
        eprintln!("skipping: {path} not found (run scripts/fetch-fixtures.sh)");
    }
    data
}

/// An `ImageSpec` describing `picture`, with its own size.
fn spec<'a>(picture: &'a EncodedPicture, auxiliary_type: Option<&'a str>) -> ImageSpec<'a> {
    ImageSpec {
        width: picture.width,
        height: picture.height,
        config: HevcConfig {
            profile_idc: picture.profile_idc,
            level_idc: picture.level_idc,
            chroma_format: picture.chroma_format,
            bit_depth_luma: 8,
            bit_depth_chroma: 8,
            nal_length_size: 4,
            nal_units: picture.parameter_sets.clone(),
        },
        slices: &picture.slices,
        color: None,
        auxiliary_type,
        hidden: auxiliary_type.is_some(),
        crop: None,
    }
}

/// A lossless monochrome depth picture, `side × side`, whose samples encode the position.
fn depth_picture(side: u32) -> (Vec<u8>, EncodedPicture) {
    let samples: Vec<u8> = (0..side * side)
        .map(|i| ((i % side) * 4 + (i / side) * 7) as u8)
        .collect();
    let options = CodingOptions {
        chroma_format: ChromaFormat::Monochrome,
        lossless: true,
    };
    let picture = encode_picture_with(
        side,
        side,
        Planes {
            y: &samples,
            chroma: None,
        },
        &options,
        &EncoderConfig::default(),
    )
    .unwrap();
    (samples, picture)
}

/// Layout of a synthetic uncompressed frame, mirroring the fields of `uncC`.
#[derive(Clone, Copy)]
struct Uncompressed {
    width: u32,
    height: u32,
    bit_depth: u8,
    align_size: u8,
    interleave: u8,
    pixel_size: u32,
    row_align_size: u32,
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

fn auxc(urn: &str) -> Vec<u8> {
    let mut content = urn.as_bytes().to_vec();
    content.push(0);
    full_box(b"auxC", 0, 0, &content)
}

fn uncc(layout: &Uncompressed) -> Vec<u8> {
    let mut content = Vec::new();
    content.extend_from_slice(&0u32.to_be_bytes()); // profile
    content.extend_from_slice(&1u32.to_be_bytes()); // component_count
    content.extend_from_slice(&0u16.to_be_bytes()); // component_index
    content.push(layout.bit_depth - 1); // component_bit_depth_minus_one
    content.push(0); // unsigned
    content.push(layout.align_size);
    content.push(0); // sampling
    content.push(layout.interleave);
    content.push(0); // block_size
    content.push(0); // flags
    content.extend_from_slice(&layout.pixel_size.to_be_bytes());
    content.extend_from_slice(&layout.row_align_size.to_be_bytes());
    content.extend_from_slice(&0u32.to_be_bytes()); // tile_align_size
    content.extend_from_slice(&0u32.to_be_bytes()); // num_tile_cols_minus_one
    content.extend_from_slice(&0u32.to_be_bytes()); // num_tile_rows_minus_one
    full_box(b"uncC", 0, 0, &content)
}

fn infe(id: u16, item_type: &[u8; 4], hidden: bool) -> Vec<u8> {
    let mut content = id.to_be_bytes().to_vec();
    content.extend_from_slice(&0u16.to_be_bytes()); // item_protection_index
    content.extend_from_slice(item_type);
    content.push(0); // name
    full_box(b"infe", 2, u32::from(hidden), &content)
}

/// A minimal HEIF file with one uncompressed image, stored in `idat`, either as an auxiliary
/// image of an empty `hvc1` primary image or as the primary image itself.
fn uncompressed_file(layout: &Uncompressed, data: &[u8], auxiliary: bool) -> Vec<u8> {
    let item_count = if auxiliary { 2u16 } else { 1u16 };
    let mut iloc = vec![0x44, 0x00];
    iloc.extend_from_slice(&item_count.to_be_bytes());
    if auxiliary {
        // Empty primary image.
        iloc.extend_from_slice(&1u16.to_be_bytes());
        iloc.extend_from_slice(&0u16.to_be_bytes()); // construction method 0
        iloc.extend_from_slice(&0u16.to_be_bytes()); // data_reference_index
        iloc.extend_from_slice(&0u16.to_be_bytes()); // extent_count
    }
    let id = if auxiliary { 2u16 } else { 1u16 };
    iloc.extend_from_slice(&id.to_be_bytes());
    iloc.extend_from_slice(&1u16.to_be_bytes()); // construction method 1 (idat)
    iloc.extend_from_slice(&0u16.to_be_bytes()); // data_reference_index
    iloc.extend_from_slice(&1u16.to_be_bytes()); // extent_count
    iloc.extend_from_slice(&0u32.to_be_bytes()); // extent_offset
    iloc.extend_from_slice(&(data.len() as u32).to_be_bytes());

    let mut iinf = item_count.to_be_bytes().to_vec();
    if auxiliary {
        iinf.extend(infe(1, b"hvc1", false));
    }
    iinf.extend(infe(id, b"unci", auxiliary));

    let mut ipco = Vec::new();
    let mut associations = Vec::new();
    if auxiliary {
        ipco.extend(ispe(2, 2));
        associations.push((1u16, vec![(1u8, false)]));
        ipco.extend(uncc(layout));
        ipco.extend(ispe(layout.width, layout.height));
        ipco.extend(auxc(DEPTH_URN));
        associations.push((2u16, vec![(2, true), (3, false), (4, true)]));
    } else {
        ipco.extend(uncc(layout));
        ipco.extend(ispe(layout.width, layout.height));
        associations.push((1u16, vec![(1, true), (2, false)]));
    }
    let mut ipma = (associations.len() as u32).to_be_bytes().to_vec();
    for (id, props) in associations {
        ipma.extend_from_slice(&id.to_be_bytes());
        ipma.push(props.len() as u8);
        for (index, essential) in props {
            ipma.push((u8::from(essential) << 7) | index);
        }
    }
    let iprp = bx(
        b"iprp",
        &[bx(b"ipco", &ipco), full_box(b"ipma", 0, 0, &ipma)].concat(),
    );

    let mut hdlr = vec![0; 4];
    hdlr.extend_from_slice(b"pict");
    hdlr.extend_from_slice(&[0; 13]);
    let mut meta = full_box(b"hdlr", 0, 0, &hdlr);
    meta.extend(full_box(b"pitm", 0, 0, &1u16.to_be_bytes()));
    meta.extend(full_box(b"iloc", 1, 0, &iloc));
    meta.extend(full_box(b"iinf", 0, 0, &iinf));
    meta.extend(iprp);
    if auxiliary {
        let mut auxl = 2u16.to_be_bytes().to_vec();
        auxl.extend_from_slice(&1u16.to_be_bytes());
        auxl.extend_from_slice(&1u16.to_be_bytes());
        meta.extend(full_box(b"iref", 0, 0, &bx(b"auxl", &auxl)));
    }
    meta.extend(bx(b"idat", data));

    let mut bytes = bx(b"ftyp", b"heic\0\0\0\0mif1heic");
    bytes.extend(full_box(b"meta", 0, 0, &meta));
    bytes
}

fn hvcc(config: &HevcConfig) -> Vec<u8> {
    let mut b = vec![1, config.profile_idc & 0x1F];
    let compatibility = 1u32 << (31 - u32::from(config.profile_idc.min(31)));
    b.extend_from_slice(&compatibility.to_be_bytes());
    b.extend_from_slice(&[0x90, 0, 0, 0, 0, 0]);
    b.push(config.level_idc);
    b.extend_from_slice(&0xF000u16.to_be_bytes());
    b.push(0xFC);
    b.push(0xFC | (config.chroma_format & 3));
    b.push(0xF8 | (config.bit_depth_luma.saturating_sub(8) & 7));
    b.push(0xF8 | (config.bit_depth_chroma.saturating_sub(8) & 7));
    b.extend_from_slice(&0u16.to_be_bytes());
    b.push((1 << 3) | (1 << 2) | ((config.nal_length_size.max(1) - 1) & 3));
    b.push(config.nal_units.len() as u8);
    for nal in &config.nal_units {
        b.push(0x80 | nal.first().map_or(0, |h| (h >> 1) & 0x3F));
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        b.extend_from_slice(nal);
    }
    bx(b"hvcC", &b)
}

/// A real HEVC primary image with an uncompressed alpha plane, to check that `decode` attaches
/// the plane instead of expecting an HEVC bitstream.
fn uncompressed_alpha_file(
    primary: &EncodedPicture,
    layout: &Uncompressed,
    alpha_data: &[u8],
) -> Vec<u8> {
    let config = HevcConfig {
        profile_idc: primary.profile_idc,
        level_idc: primary.level_idc,
        chroma_format: primary.chroma_format,
        bit_depth_luma: 8,
        bit_depth_chroma: 8,
        nal_length_size: 4,
        nal_units: primary.parameter_sets.clone(),
    };
    let mut primary_data = Vec::new();
    for nal in &primary.slices {
        primary_data.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        primary_data.extend_from_slice(nal);
    }
    let alpha_offset = primary_data.len() as u32;
    let mut idat = primary_data.clone();
    idat.extend_from_slice(alpha_data);

    let mut iloc = vec![0x44, 0x00, 0, 2];
    for (id, offset, len) in [
        (1u16, 0u32, primary_data.len() as u32),
        (2, alpha_offset, alpha_data.len() as u32),
    ] {
        iloc.extend_from_slice(&id.to_be_bytes());
        iloc.extend_from_slice(&1u16.to_be_bytes()); // construction method 1 (idat)
        iloc.extend_from_slice(&0u16.to_be_bytes());
        iloc.extend_from_slice(&1u16.to_be_bytes());
        iloc.extend_from_slice(&offset.to_be_bytes());
        iloc.extend_from_slice(&len.to_be_bytes());
    }

    let mut iinf = 2u16.to_be_bytes().to_vec();
    iinf.extend(infe(1, b"hvc1", false));
    iinf.extend(infe(2, b"unci", true));

    let mut pixi = vec![3, 8, 8, 8];
    pixi = full_box(b"pixi", 0, 0, &pixi);
    let mut ipco = hvcc(&config);
    ipco.extend(ispe(primary.width, primary.height));
    ipco.extend(pixi);
    ipco.extend(uncc(layout));
    ipco.extend(ispe(layout.width, layout.height));
    ipco.extend(auxc(ALPHA_URN));
    let mut ipma = 2u32.to_be_bytes().to_vec();
    for (id, props) in [
        (1u16, vec![(1u8, true), (2, false), (3, false)]),
        (2, vec![(4, true), (5, false), (6, true)]),
    ] {
        ipma.extend_from_slice(&id.to_be_bytes());
        ipma.push(props.len() as u8);
        for (index, essential) in props {
            ipma.push((u8::from(essential) << 7) | index);
        }
    }
    let iprp = bx(
        b"iprp",
        &[bx(b"ipco", &ipco), full_box(b"ipma", 0, 0, &ipma)].concat(),
    );

    let mut hdlr = vec![0; 4];
    hdlr.extend_from_slice(b"pict");
    hdlr.extend_from_slice(&[0; 13]);
    let mut meta = full_box(b"hdlr", 0, 0, &hdlr);
    meta.extend(full_box(b"pitm", 0, 0, &1u16.to_be_bytes()));
    meta.extend(full_box(b"iloc", 1, 0, &iloc));
    meta.extend(full_box(b"iinf", 0, 0, &iinf));
    meta.extend(iprp);
    let mut auxl = 2u16.to_be_bytes().to_vec();
    auxl.extend_from_slice(&1u16.to_be_bytes());
    auxl.extend_from_slice(&1u16.to_be_bytes());
    meta.extend(full_box(b"iref", 0, 0, &bx(b"auxl", &auxl)));
    meta.extend(bx(b"idat", &idat));

    let mut bytes = bx(b"ftyp", b"heic\0\0\0\0mif1heic");
    bytes.extend(full_box(b"meta", 0, 0, &meta));
    bytes
}

/// A 16×16 4:2:0 grey primary picture.
fn primary_picture() -> EncodedPicture {
    let y = [96u8; 16 * 16];
    let c = [128u8; 8 * 8];
    encode_picture(
        16,
        16,
        Planes {
            y: &y,
            chroma: Some((&c, &c)),
        },
        &EncoderConfig::default(),
    )
    .unwrap()
}

#[test]
fn alpha_plane_is_listed_and_decoded() {
    let (width, height) = (32u32, 24u32);
    let rgba: Vec<u8> = (0..width * height)
        .flat_map(|i| {
            let alpha = ((i % width) * 8) as u8;
            [40, 80, 120, alpha]
        })
        .collect();
    let bytes = heifer::encode_rgba8_with_format(
        width,
        height,
        &rgba,
        EncodeFormat::Lossless,
        &EncodeOptions::default(),
    )
    .unwrap();

    let aux = heifer::auxiliary_images(&bytes).unwrap();
    assert_eq!(aux.len(), 1, "{aux:?}");
    assert!(aux[0].is_alpha());
    assert!(!aux[0].is_depth());
    assert_eq!((aux[0].width, aux[0].height), (width, height));
    assert_eq!(aux[0].bit_depth, 8);
    assert_eq!(aux[0].item_type.to_string(), "hvc1");

    // The alpha plane is monochrome: its sample is replicated in the RGB channels.
    let plane = decode_auxiliary(&bytes, aux[0].id).unwrap();
    assert_eq!((plane.width, plane.height), (width, height));
    assert!(!plane.has_alpha);
    for y in 0..height as usize {
        for x in 0..width as usize {
            let expected = (x * 8) as u16;
            let i = (y * width as usize + x) * 4;
            assert_eq!(plane.data[i], expected, "({x}, {y})");
            assert_eq!(plane.data[i + 1], expected, "({x}, {y})");
            assert_eq!(plane.data[i + 2], expected, "({x}, {y})");
        }
    }

    // The same plane by URN, as a caller looking for alpha would do.
    let by_urn = decode_auxiliary_urn(&bytes, "urn:mpeg:hevc:2015:auxid:1")
        .unwrap()
        .expect("alpha auxiliary image");
    assert_eq!(by_urn, plane);
}

#[test]
fn uncompressed_alpha_plane_is_attached_to_the_primary_image() {
    let primary = primary_picture();
    let samples: Vec<u8> = (0..16 * 16).map(|i| (i % 256) as u8).collect();
    let layout = Uncompressed {
        width: 16,
        height: 16,
        bit_depth: 8,
        align_size: 0,
        interleave: 1,
        pixel_size: 0,
        row_align_size: 0,
    };
    let bytes = uncompressed_alpha_file(&primary, &layout, &samples);

    let aux = heifer::auxiliary_images(&bytes).unwrap();
    assert_eq!(aux.len(), 1, "{aux:?}");
    assert!(aux[0].is_alpha());
    assert_eq!(aux[0].item_type.to_string(), "unci");

    let image = heifer::decode(&bytes).unwrap();
    assert!(image.has_alpha);
    for (i, &sample) in samples.iter().enumerate() {
        assert_eq!(image.data[i * 4 + 3], u16::from(sample), "sample {i}");
    }
}

#[test]
fn depth_map_is_listed_and_decoded_losslessly() {
    let (samples, depth) = depth_picture(16);
    let primary = primary_picture();

    let mut writer = HeifWriter::new();
    let primary_id = writer.add_image(&spec(&primary, None));
    let depth_id = writer.add_image(&spec(&depth, Some(DEPTH_URN)));
    writer.add_reference(*b"auxl", depth_id, &[primary_id]);
    writer.set_primary(primary_id);
    let bytes = writer.finish();

    let aux = heifer::auxiliary_images(&bytes).unwrap();
    assert_eq!(aux.len(), 1, "{aux:?}");
    assert_eq!(aux[0].id, depth_id);
    assert!(aux[0].is_depth());
    assert!(!aux[0].is_alpha());
    assert_eq!((aux[0].width, aux[0].height), (16, 16));
    assert_eq!(aux[0].bit_depth, 8);

    let decoded = decode_auxiliary_urn(&bytes, DEPTH_URN)
        .unwrap()
        .expect("depth auxiliary image");
    assert_eq!((decoded.width, decoded.height), (16, 16));
    for (i, &sample) in samples.iter().enumerate() {
        assert_eq!(decoded.data[i * 4], u16::from(sample), "sample {i}");
    }

    // No auxiliary image of that type in an ordinary file.
    let plain =
        heifer::encode_rgb8(16, 16, &[0u8; 16 * 16 * 3], &EncodeOptions::default()).unwrap();
    assert!(heifer::auxiliary_images(&plain).unwrap().is_empty());
    assert!(decode_auxiliary_urn(&plain, DEPTH_URN).unwrap().is_none());
}

#[test]
fn uncompressed_depth_auxiliary_is_decoded() {
    let samples: Vec<u8> = (0..4 * 3).map(|i| (i * 17) as u8).collect();
    let layout = Uncompressed {
        width: 4,
        height: 3,
        bit_depth: 8,
        align_size: 0,
        interleave: 1,
        pixel_size: 0,
        row_align_size: 0,
    };
    let bytes = uncompressed_file(&layout, &samples, true);

    let aux = heifer::auxiliary_images(&bytes).unwrap();
    assert_eq!(aux.len(), 1, "{aux:?}");
    assert!(aux[0].is_depth());
    assert_eq!((aux[0].width, aux[0].height), (4, 3));
    assert_eq!(aux[0].bit_depth, 8, "bit depth comes from uncC");
    assert_eq!(aux[0].item_type.to_string(), "unci");

    let decoded = decode_auxiliary_urn(&bytes, DEPTH_URN)
        .unwrap()
        .expect("uncompressed depth image");
    assert_eq!(
        (decoded.width, decoded.height, decoded.bit_depth),
        (4, 3, 8)
    );
    assert!(!decoded.has_alpha);
    for (i, &sample) in samples.iter().enumerate() {
        let expected = u16::from(sample);
        assert_eq!(decoded.data[i * 4], expected, "sample {i}");
        assert_eq!(decoded.data[i * 4 + 1], expected, "sample {i}");
        assert_eq!(decoded.data[i * 4 + 2], expected, "sample {i}");
        assert_eq!(decoded.data[i * 4 + 3], 255, "sample {i}");
    }
}

#[test]
fn uncompressed_depth_auxiliary_honours_alignment_and_row_padding() {
    let samples = [0x100u16, 0x200, 0x300, 0xABC, 0xDEF, 0xFFF];
    let layout = Uncompressed {
        width: 3,
        height: 2,
        bit_depth: 12,
        align_size: 2,
        interleave: 0,
        pixel_size: 0,
        row_align_size: 4,
    };
    let mut data = Vec::new();
    for row in samples.chunks(3) {
        for &sample in row {
            data.extend_from_slice(&sample.to_be_bytes());
        }
        data.extend_from_slice(&[0; 2]); // pad the 6-byte row to 8 bytes
    }
    let bytes = uncompressed_file(&layout, &data, true);

    let aux = heifer::auxiliary_images(&bytes).unwrap();
    assert_eq!(aux[0].bit_depth, 12);
    let decoded = decode_auxiliary(&bytes, aux[0].id).unwrap();
    assert_eq!(decoded.bit_depth, 12);
    for (i, &sample) in samples.iter().enumerate() {
        assert_eq!(decoded.data[i * 4], sample, "sample {i}");
    }
}

#[test]
fn uncompressed_depth_auxiliary_honours_pixel_size() {
    let samples = [1u16, 200, 500, 1023, 42, 7, 900, 333];
    let layout = Uncompressed {
        width: 4,
        height: 2,
        bit_depth: 10,
        align_size: 0,
        interleave: 1,
        pixel_size: 2,
        row_align_size: 0,
    };
    let data: Vec<u8> = samples
        .iter()
        .flat_map(|&sample| (sample << 6).to_be_bytes())
        .collect();
    let bytes = uncompressed_file(&layout, &data, true);

    let aux = heifer::auxiliary_images(&bytes).unwrap();
    assert_eq!(aux[0].bit_depth, 10);
    let decoded = decode_auxiliary(&bytes, aux[0].id).unwrap();
    assert_eq!(decoded.bit_depth, 10);
    for (i, &sample) in samples.iter().enumerate() {
        assert_eq!(decoded.data[i * 4], sample, "sample {i}");
    }
}

#[test]
fn uncompressed_primary_image_is_decoded() {
    let samples: Vec<u8> = (0..4 * 3).map(|i| (i * 17) as u8).collect();
    let layout = Uncompressed {
        width: 4,
        height: 3,
        bit_depth: 8,
        align_size: 0,
        interleave: 1,
        pixel_size: 0,
        row_align_size: 0,
    };
    let bytes = uncompressed_file(&layout, &samples, false);
    assert!(heifer::auxiliary_images(&bytes).unwrap().is_empty());
    let image = heifer::decode(&bytes).expect("unci is decoded as a primary image");
    assert_eq!((image.width, image.height, image.bit_depth), (4, 3, 8));
    assert!(!image.has_alpha);
    for (i, &sample) in samples.iter().enumerate() {
        let pixel = &image.data[i * 4..i * 4 + 4];
        assert_eq!(
            pixel,
            [u16::from(sample), u16::from(sample), u16::from(sample), 255],
            "pixel {i}"
        );
    }
}

#[test]
fn real_depth_item_is_listed_but_unsupported() {
    let Some(bytes) = load_real("heif_special__unsupported_depth_image.heic") else {
        return;
    };
    let aux = heifer::auxiliary_images(&bytes).unwrap();
    assert_eq!(aux.len(), 1, "{aux:?}");
    assert!(aux[0].is_depth());
    assert_eq!(aux[0].urn, "urn:mpeg:hevc:2015:auxid:2");
    assert_eq!((aux[0].width, aux[0].height), (64, 32));
    assert_eq!(aux[0].bit_depth, 8);
    assert_eq!(aux[0].item_type.to_string(), "unci");

    // The fixture labels an HEVC payload as `unci` and has no `uncC`: the payload cannot be
    // interpreted as uncompressed samples.
    let error = decode_auxiliary(&bytes, aux[0].id).expect_err("no uncC configuration");
    assert!(
        matches!(&error, Error::Unsupported(message) if message.contains("uncC")),
        "unexpected error: {error}"
    );
}

#[test]
fn grid_auxiliary_image_reports_its_tiles_bit_depth() {
    // A 10-bit monochrome tile: the grid item itself has no `pixi` or `hvcC`, so the depth
    // must come from its first input.
    let samples = [512u16; 8 * 8];
    let picture = encode_picture16_with(
        8,
        8,
        Planes16 {
            y: &samples,
            chroma: None,
            bit_depth: 10,
        },
        &CodingOptions {
            chroma_format: ChromaFormat::Monochrome,
            lossless: false,
        },
        &EncoderConfig::default(),
    )
    .unwrap();
    let config = HevcConfig {
        profile_idc: picture.profile_idc,
        level_idc: picture.level_idc,
        chroma_format: picture.chroma_format,
        bit_depth_luma: 10,
        bit_depth_chroma: 10,
        nal_length_size: 4,
        nal_units: picture.parameter_sets.clone(),
    };
    let tile_spec = ImageSpec {
        hidden: true,
        ..ImageSpec::new(8, 8, config, &picture.slices)
    };

    let mut writer = HeifWriter::new();
    let primary = primary_picture();
    let primary_id = writer.add_image(&spec(&primary, None));
    let tiles = [writer.add_image(&tile_spec), writer.add_image(&tile_spec)];
    let alpha = writer.add_grid(1, 2, 16, 8, &tiles, None, Some(ALPHA_URN));
    writer.add_reference(*b"auxl", alpha, &[primary_id]);
    writer.set_primary(primary_id);
    let bytes = writer.finish();

    let aux = heifer::auxiliary_images(&bytes).unwrap();
    assert_eq!(aux.len(), 1, "{aux:?}");
    assert!(aux[0].is_alpha());
    assert_eq!((aux[0].width, aux[0].height), (16, 8));
    assert_eq!(aux[0].bit_depth, 10);
    assert_eq!(aux[0].item_type.to_string(), "grid");
}

#[test]
fn decode_all_still_skips_auxiliary_images() {
    let (width, height) = (32u32, 24u32);
    let rgba: Vec<u8> = (0..width * height)
        .flat_map(|i| [200, 10, 60, (i % 200) as u8])
        .collect();
    let with_alpha = heifer::encode_rgba8(width, height, &rgba, &EncodeOptions::default()).unwrap();
    assert_eq!(heifer::auxiliary_images(&with_alpha).unwrap().len(), 1);
    assert_eq!(heifer::decode_all(&with_alpha).unwrap().len(), 1);

    let (_, depth) = depth_picture(8);
    let primary = primary_picture();
    let mut writer = HeifWriter::new();
    let primary_id = writer.add_image(&spec(&primary, None));
    let depth_id = writer.add_image(&spec(&depth, Some(DEPTH_URN)));
    writer.add_reference(*b"auxl", depth_id, &[primary_id]);
    writer.set_primary(primary_id);
    let with_depth = writer.finish();
    assert_eq!(heifer::auxiliary_images(&with_depth).unwrap().len(), 1);
    assert_eq!(heifer::decode_all(&with_depth).unwrap().len(), 1);
}
