//! `check_file_type`: type sniffing from the leading bytes, without decoding.

use heifer::{FileTypeResult, check_file_type};

/// A 12-byte prefix: a box header followed by `ftyp` and a major brand.
fn ftyp(brand: &[u8; 4]) -> [u8; 12] {
    let mut bytes = [0u8; 12];
    bytes[4..8].copy_from_slice(b"ftyp");
    bytes[8..12].copy_from_slice(brand);
    bytes
}

#[test]
fn hevc_image_brands_are_supported() {
    assert_eq!(check_file_type(&ftyp(b"heic")), FileTypeResult::Supported);
    assert_eq!(check_file_type(&ftyp(b"heix")), FileTypeResult::Supported);
    // HEVC image sequences.
    assert_eq!(check_file_type(&ftyp(b"hevc")), FileTypeResult::Supported);
    assert_eq!(check_file_type(&ftyp(b"hevx")), FileTypeResult::Supported);
}

#[test]
fn generic_heif_brands_are_maybe() {
    // The codec is in the items, not in the major brand.
    assert_eq!(check_file_type(&ftyp(b"mif1")), FileTypeResult::MayBe);
    assert_eq!(check_file_type(&ftyp(b"mif2")), FileTypeResult::MayBe);
    assert_eq!(check_file_type(&ftyp(b"msf1")), FileTypeResult::MayBe);
}

#[test]
fn codecs_heifer_does_not_decode_are_unsupported() {
    // Other codecs, and multi-layer HEVC (scalable, multiview).
    for brand in [b"avif", b"avis", b"jpeg", b"j2ki", b"heim", b"heis"] {
        assert_eq!(
            check_file_type(&ftyp(brand)),
            FileTypeResult::Unsupported,
            "brand {}",
            String::from_utf8_lossy(brand)
        );
    }
}

#[test]
fn too_few_bytes_is_maybe() {
    assert_eq!(check_file_type(&[]), FileTypeResult::MayBe);
    assert_eq!(check_file_type(b"\0\0\0\x18fty"), FileTypeResult::MayBe);
    // A `ftyp` box header without its major brand.
    assert_eq!(check_file_type(b"\0\0\0\x18ftyp"), FileTypeResult::MayBe);
}

#[test]
fn not_an_isobmff_file_is_no() {
    // A JPEG start of image, and arbitrary text.
    assert_eq!(
        check_file_type(b"\xff\xd8\xff\xe0\x00\x10JFIF"),
        FileTypeResult::No
    );
    assert_eq!(check_file_type(b"not a heif file"), FileTypeResult::No);
}

#[test]
fn bytes_after_the_major_brand_are_ignored() {
    let mut bytes = ftyp(b"heic").to_vec();
    bytes.extend_from_slice(b"mif1heic\0\0\0\0");
    assert_eq!(check_file_type(&bytes), FileTypeResult::Supported);
}
