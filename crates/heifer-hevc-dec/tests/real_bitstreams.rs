//! Reads the start of real parameter sets with the bit reader and checks the values
//! against what ffprobe reports. Run `scripts/fetch-fixtures.sh` first; missing files are skipped.

use heifer_hevc_dec::bitreader::BitReader;
use heifer_hevc_dec::nal::{NalUnit, NalUnitType, split_annex_b};
use heifer_isobmff::HeifFile;

fn bitstream(file: &str, item: u32) -> Option<Vec<u8>> {
    let path = format!("{}/../../tests/fixtures/{file}", env!("CARGO_MANIFEST_DIR"));
    let Ok(data) = std::fs::read(&path) else {
        eprintln!("skipping: {path} not found (run scripts/fetch-fixtures.sh)");
        return None;
    };
    Some(
        HeifFile::parse(&data)
            .unwrap()
            .hevc_bitstream(item)
            .unwrap(),
    )
}

/// Reads the beginning of an SPS (H.265 §7.3.2.2) up to the picture size.
/// Returns (chroma_format_idc, width, height).
fn sps_size(rbsp: &[u8]) -> (u32, u32, u32) {
    let mut r = BitReader::new(rbsp);
    r.bits(4).unwrap(); // sps_video_parameter_set_id
    let max_sub_layers_minus1 = r.bits(3).unwrap();
    assert_eq!(
        max_sub_layers_minus1, 0,
        "still images have a single sub-layer"
    );
    r.flag().unwrap(); // sps_temporal_id_nesting_flag
    // profile_tier_level(1, 0): 2+1+5 bits, 32 compatibility flags, 48 constraint bits, level u(8).
    r.skip(8 + 32 + 48 + 8).unwrap();
    assert_eq!(r.ue().unwrap(), 0, "sps_seq_parameter_set_id");
    let chroma_format_idc = r.ue_max(3, "chroma_format_idc").unwrap();
    if chroma_format_idc == 3 {
        r.flag().unwrap(); // separate_colour_plane_flag
    }
    let width = r.ue().unwrap();
    let height = r.ue().unwrap();
    (chroma_format_idc, width, height)
}

fn check(file: &str, item: u32, expected: (u32, u32, u32)) {
    let Some(stream) = bitstream(file, item) else {
        return;
    };
    let nals: Vec<_> = split_annex_b(&stream)
        .map(|n| NalUnit::parse(n).unwrap())
        .collect();

    let types: Vec<_> = nals.iter().map(|n| n.header.unit_type).collect();
    assert_eq!(
        &types[..3],
        &[NalUnitType::Vps, NalUnitType::Sps, NalUnitType::Pps]
    );
    assert!(
        types.iter().any(|t| matches!(t, NalUnitType::IrapSlice(_))),
        "{types:?}"
    );

    // VPS: 4+1+1+6+3+1 bits, then `vps_reserved_0xffff_16bits`.
    let mut r = BitReader::new(&nals[0].rbsp);
    r.skip(16).unwrap();
    assert_eq!(r.bits(16).unwrap(), 0xFFFF);

    assert_eq!(sps_size(&nals[1].rbsp), expected);
}

#[test]
fn single_image_sps() {
    check("single_image.heic", 1002, (1, 1440, 960));
}

#[test]
fn grid_tile_sps() {
    check("grid.heic", 1002, (1, 480, 320));
}

#[test]
fn libheif_example_sps() {
    // ffprobe reports 1280x854; HEVC codes it padded to a multiple of 8 and crops with the
    // conformance window, so the coded height is 856.
    check("libheif_example.heic", 20004, (1, 1280, 856));
}
