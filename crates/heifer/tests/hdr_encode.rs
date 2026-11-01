//! Encoding of HDR images with an ISO 21496-1 gain map.

use heifer::heifer_isobmff::HeifFile;
use heifer::{EncodeOptions, HdrImage, Image, encode_hdr, encode_hdr_with_metadata};

fn load(name: &str) -> Option<Vec<u8>> {
    let path = format!("{}/../../tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let data = std::fs::read(&path).ok();
    if data.is_none() {
        eprintln!("skipping: {path} not found (run scripts/fetch-fixtures.sh)");
    }
    data
}

fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// A grey gradient whose central square is four times brighter in the HDR rendition.
fn synthetic(w: u32, h: u32) -> (Image, HdrImage) {
    let mut base = Image::filled(w, h, 8, [0, 0, 0, 255]);
    let mut hdr = HdrImage {
        width: w,
        height: h,
        headroom: 4.0,
        data: vec![0.0; w as usize * h as usize * 3],
    };
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) as usize;
            let v = (40 + 160 * x / (w - 1)) as u8;
            base.data[i * 4] = u16::from(v);
            base.data[i * 4 + 1] = u16::from(v);
            base.data[i * 4 + 2] = u16::from(v);
            let patch = (w / 4..w / 2).contains(&x) && (h / 4..h / 2).contains(&y);
            let gain = if patch { 4.0 } else { 1.0 };
            for c in 0..3 {
                hdr.data[i * 3 + c] = srgb_to_linear(f32::from(v) / 255.0) * gain;
            }
        }
    }
    (base, hdr)
}

#[test]
fn gain_map_round_trips() {
    let (base, hdr) = synthetic(64, 48);
    let options = EncodeOptions {
        quality: 90,
        ..EncodeOptions::default()
    };
    let bytes = encode_hdr(&base, &hdr, &options).unwrap();

    let file = HeifFile::parse(&bytes).unwrap();
    assert!(file.file_type.has_brand(b"tmap"));
    let tmap = file
        .items
        .iter()
        .find(|item| item.item_type.0 == *b"tmap")
        .unwrap();
    let references = file.referenced_items(tmap.id, b"dimg");
    let [base_id, map_id] = references else {
        panic!("expected two dimg references, got {references:?}");
    };
    assert_eq!(*base_id, file.primary_id);
    assert_eq!(file.image_size(*map_id).unwrap(), Some((32, 24)));
    assert!(file.item(*map_id).unwrap().hidden);
    assert!(!file.item(tmap.id).unwrap().hidden);
    assert_eq!(file.hevc_config(*map_id).unwrap().unwrap().chroma_format, 0);
    let payload = file.item_data(tmap.id).unwrap();
    assert_eq!(payload.len(), 62);
    assert_eq!(payload[5], 0x40); // use_base_colour_space, one channel
    assert!(bytes.windows(4).any(|w| w == b"altr"));

    let decoded =
        heifer::hdr::decode_hdr_with_options(&bytes, &Default::default(), f32::INFINITY).unwrap();
    assert_eq!((decoded.width, decoded.height), (64, 48));
    assert!((decoded.headroom - 4.0).abs() < 0.01);
    let base_back = heifer::decode(&bytes).unwrap();
    for y in 0..48 {
        for x in 0..64 {
            // The half-resolution gain map cannot represent the two-pixel ramp at a hard edge.
            let in_patch = (18..30).contains(&x) && (14..22).contains(&y);
            let flat = !((14..34).contains(&x) && (10..26).contains(&y));
            if !(in_patch || flat) {
                continue;
            }
            let i = (y * 64 + x) as usize;
            let gain = if in_patch { 4.0 } else { 1.0 };
            for c in 0..3 {
                let sample = f32::from(base_back.data[i * 4 + c]) / 255.0;
                let expected = srgb_to_linear(sample) * gain;
                let got = decoded.data[i * 3 + c];
                let error = (got - expected).abs();
                assert!(
                    error <= 0.02 * expected + 0.001,
                    "({x}, {y}) channel {c}: expected {expected}, got {got}"
                );
            }
        }
    }
}

#[test]
fn gain_map_of_a_grid_is_a_hidden_half_size_grid() {
    let (base, hdr) = synthetic(1100, 700);
    let options = EncodeOptions {
        quality: 60,
        ..EncodeOptions::default()
    };
    let bytes = encode_hdr(&base, &hdr, &options).unwrap();

    let file = HeifFile::parse(&bytes).unwrap();
    let tmap = file
        .items
        .iter()
        .find(|item| item.item_type.0 == *b"tmap")
        .unwrap();
    let references = file.referenced_items(tmap.id, b"dimg");
    let [base_id, map_id] = references else {
        panic!("expected two dimg references, got {references:?}");
    };
    assert_eq!(*base_id, file.primary_id);
    assert_eq!(file.image_size(tmap.id).unwrap(), Some((1100, 700)));
    assert_eq!(file.image_size(*map_id).unwrap(), Some((550, 350)));
    assert!(file.item(*map_id).unwrap().hidden);
    let grid = file.grid(*map_id).unwrap();
    assert_eq!((grid.rows, grid.columns), (1, 2));
    for &tile in file.referenced_items(*map_id, b"dimg") {
        assert_eq!(file.hevc_config(tile).unwrap().unwrap().chroma_format, 0);
    }

    let decoded =
        heifer::hdr::decode_hdr_with_options(&bytes, &Default::default(), f32::INFINITY).unwrap();
    assert_eq!((decoded.width, decoded.height), (1100, 700));
    assert!((decoded.headroom - 4.0).abs() < 0.01);
}

#[test]
fn hdr_metadata_is_kept() {
    let (base, hdr) = synthetic(64, 48);
    let metadata = heifer::Metadata {
        exif: Some(b"II*\0\x08\0\0\0\0\0".to_vec()),
        xmp: Some(b"<x:xmpmeta/>".to_vec()),
        icc_profile: Some(b"icc".to_vec()),
    };
    let bytes =
        encode_hdr_with_metadata(&base, &hdr, &metadata, &EncodeOptions::default()).unwrap();
    assert_eq!(heifer::read_metadata(&bytes).unwrap(), metadata);
    assert!(
        heifer::hdr::decode_hdr_with_options(&bytes, &Default::default(), f32::INFINITY).is_ok()
    );
}

#[test]
fn rejects_bad_hdr_input() {
    let (base, hdr) = synthetic(64, 48);
    let options = EncodeOptions::default();

    let mut wrong_size = hdr.clone();
    wrong_size.width = 32;
    assert!(encode_hdr(&base, &wrong_size, &options).is_err());

    let mut truncated = hdr.clone();
    truncated.data.pop();
    assert!(encode_hdr(&base, &truncated, &options).is_err());

    for headroom in [1.0, 0.5, f32::NAN, f32::INFINITY] {
        let mut flat = hdr.clone();
        flat.headroom = headroom;
        assert!(encode_hdr(&base, &flat, &options).is_err(), "{headroom}");
    }
}

#[test]
fn reencodes_a_real_samsung_gain_map() {
    let Some(bytes) = load("real/heif_special__aux_YCbCr.heic") else {
        return;
    };
    let base = heifer::decode(&bytes).unwrap();
    let hdr =
        heifer::hdr::decode_hdr_with_options(&bytes, &Default::default(), f32::INFINITY).unwrap();

    // A 512×512 crop around the brightest highlights keeps the test fast.
    let (x0, y0, size) = (704usize, 192usize, 512usize);
    let base = base.crop(x0 as u32, y0 as u32, size as u32, size as u32);
    let mut data = Vec::with_capacity(size * size * 3);
    for y in 0..size {
        let start = ((y + y0) * hdr.width as usize + x0) * 3;
        data.extend_from_slice(&hdr.data[start..start + size * 3]);
    }
    let crop = HdrImage {
        width: size as u32,
        height: size as u32,
        headroom: hdr.headroom,
        data,
    };

    let options = EncodeOptions {
        quality: 90,
        ..EncodeOptions::default()
    };
    let out = encode_hdr(&base, &crop, &options).unwrap();
    let back =
        heifer::hdr::decode_hdr_with_options(&out, &Default::default(), f32::INFINITY).unwrap();
    assert!((back.headroom - crop.headroom).abs() < 0.01);
    assert_eq!((back.width, back.height), (size as u32, size as u32));

    let (mut sum, mut max) = (0.0f64, 0.0f32);
    for (&expected, &got) in crop.data.iter().zip(&back.data) {
        let error = (got - expected).abs();
        sum += f64::from(error);
        max = max.max(error);
    }
    let mean = sum / crop.data.len() as f64;
    assert!(mean < 0.01, "mean error {mean}");
    assert!(max < 0.15, "max error {max}");
}
