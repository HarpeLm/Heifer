//! PQ and HLG transfer characteristics, in `decode_hdr`.

use heifer::{EncodeOptions, Error, HdrImage, Image, decode_hdr, decode_srgb, encode, encode_hdr};

/// Sets the transfer characteristics of every `nclx` box: the primaries are two bytes after the
/// box type, the transfer the next two.
fn patch_transfer(bytes: &mut [u8], transfer: u16) {
    let mut patched = false;
    for at in 0..bytes.len().saturating_sub(8) {
        if &bytes[at..at + 4] == b"nclx" {
            bytes[at + 6..at + 8].copy_from_slice(&transfer.to_be_bytes());
            patched = true;
        }
    }
    assert!(patched, "the encoded file has no nclx box");
}

fn grey(value: u16) -> Image {
    Image::filled(32, 32, 8, [value, value, value, 255])
}

fn encode_grey(value: u16, transfer: u16) -> Vec<u8> {
    let mut bytes = encode(&grey(value), &EncodeOptions::default()).unwrap();
    patch_transfer(&mut bytes, transfer);
    bytes
}

fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

#[test]
fn pq_transfer_is_linearised_to_reference_white() {
    // 148/255 is the PQ code 0.58, the BT.2408 reference white of 203 cd/m².
    let bytes = encode_grey(148, 16);
    let hdr = decode_hdr(&bytes).unwrap();
    assert_eq!((hdr.width, hdr.height), (32, 32));
    assert!((hdr.headroom - 10_000.0 / 203.0).abs() < 1e-3);
    for &value in &hdr.data {
        assert!((value - 1.0).abs() < 0.05, "{value}");
    }
    assert!(matches!(
        decode_srgb(&bytes),
        Err(Error::Unsupported(message)) if message.contains("decode_hdr")
    ));
}

#[test]
fn hlg_transfer_is_linearised_to_reference_white() {
    // 192/255 is close to the HLG signal 0.75, the BT.2408 reference white.
    let bytes = encode_grey(192, 18);
    let hdr = decode_hdr(&bytes).unwrap();
    assert_eq!((hdr.width, hdr.height), (32, 32));
    assert!((hdr.headroom - 1_000.0 / 203.0).abs() < 1e-3);
    for &value in &hdr.data {
        assert!((value - 1.0).abs() < 0.05, "{value}");
    }
    assert!(matches!(
        decode_srgb(&bytes),
        Err(Error::Unsupported(message)) if message.contains("decode_hdr")
    ));
}

#[test]
fn gain_map_is_applied_in_the_base_transfer() {
    let base = grey(128);
    let linear = srgb_to_linear(128.0 / 255.0);
    let hdr = HdrImage {
        width: 32,
        height: 32,
        headroom: 4.0,
        data: vec![linear * 4.0; 32 * 32 * 3],
    };
    let mut with_gain = encode_hdr(&base, &hdr, &EncodeOptions::default()).unwrap();
    patch_transfer(&mut with_gain, 16);
    let mut plain = encode(&base, &EncodeOptions::default()).unwrap();
    patch_transfer(&mut plain, 16);

    let with_gain = decode_hdr(&with_gain).unwrap();
    let plain = decode_hdr(&plain).unwrap();
    assert!((with_gain.headroom - 4.0 * 10_000.0 / 203.0).abs() < 0.01);
    for (i, (&gain, &base)) in with_gain.data.iter().zip(&plain.data).enumerate() {
        let expected = 4.0 * base;
        assert!(
            (gain - expected).abs() <= 0.02 * expected + 1e-3,
            "sample {i}: expected {expected}, got {gain}"
        );
    }
}
