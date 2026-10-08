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

fn parameter_sets(file: &str, item: u32) -> Option<heifer_hevc_dec::params::ParameterSets> {
    let stream = bitstream(file, item)?;
    let mut sets = heifer_hevc_dec::params::ParameterSets::default();
    for nal in split_annex_b(&stream) {
        sets.add(&NalUnit::parse(nal).unwrap()).unwrap();
    }
    Some(sets)
}

#[test]
fn single_image_parameter_sets() {
    // Values checked against `ffmpeg -bsf:v trace_headers`.
    let Some(sets) = parameter_sets("single_image.heic", 1002) else {
        return;
    };
    let (pps, sps) = sets.get(0).unwrap();
    assert_eq!(sps.profile_tier_level.general_profile_idc, 1);
    assert_eq!(sps.profile_tier_level.general_level_idc, 186);
    assert_eq!(sps.output_size(), (1440, 960));
    assert_eq!(sps.ctb_size(), 64);
    assert_eq!(sps.pic_size_in_ctbs(), (23, 15));
    assert_eq!(sps.log2_min_luma_coding_block_size, 3);
    assert_eq!(
        (
            sps.log2_min_luma_transform_block_size,
            sps.log2_max_luma_transform_block_size
        ),
        (2, 5)
    );
    assert_eq!(sps.st_rps_num_delta_pocs, vec![0, 0]);
    assert!(sps.sample_adaptive_offset_enabled_flag && sps.strong_intra_smoothing_enabled_flag);
    assert!(sps.vui.is_none());
    assert!(pps.sign_data_hiding_enabled_flag && pps.cabac_init_present_flag);
    assert_eq!(pps.init_qp_minus26, 0);
    assert!(pps.tiles.is_none());
}

#[test]
fn libheif_example_parameter_sets() {
    let Some(sets) = parameter_sets("libheif_example.heic", 20004) else {
        return;
    };
    let (pps, sps) = sets.get(0).unwrap();
    assert_eq!(
        (
            sps.pic_width_in_luma_samples,
            sps.pic_height_in_luma_samples
        ),
        (1280, 856)
    );
    assert_eq!(
        sps.output_size(),
        (1280, 854),
        "conformance window crops 2 lines"
    );
    assert!(sps.vui.is_some());
    assert!(pps.diff_cu_qp_delta_depth.is_some());
}

fn slice_headers(
    file: &str,
    item: u32,
) -> Option<Vec<(heifer_hevc_dec::slice::SliceHeader, usize)>> {
    use heifer_hevc_dec::slice::SliceHeader;
    let stream = bitstream(file, item)?;
    let mut sets = heifer_hevc_dec::params::ParameterSets::default();
    let mut out = Vec::new();
    for raw in split_annex_b(&stream) {
        let nal = NalUnit::parse(raw).unwrap();
        if !sets.add(&nal).unwrap() && nal.header.unit_type.is_slice() {
            let h = SliceHeader::parse(&nal.rbsp, &nal.header, &sets, None).unwrap();
            assert_eq!(
                h.slice_qp_y(&sets).unwrap(),
                26 + i32::from(h.slice_qp_delta)
                    + i32::from(sets.get(0).unwrap().0.init_qp_minus26)
            );
            out.push((h, raw.len()));
        }
    }
    Some(out)
}

#[test]
fn single_image_slice_header() {
    use heifer_hevc_dec::slice::SliceType;
    // Values checked against `ffmpeg -bsf:v trace_headers`.
    let Some(slices) = slice_headers("single_image.heic", 1002) else {
        return;
    };
    assert_eq!(slices.len(), 1);
    let (h, _) = &slices[0];
    assert!(h.first_slice_segment_in_pic_flag);
    assert_eq!(h.slice_type, SliceType::I);
    assert!(h.slice_sao_luma_flag && h.slice_sao_chroma_flag);
    assert_eq!(h.slice_qp_delta, 2);
    assert!(h.entry_point_offsets.is_empty());
}

#[test]
fn libheif_example_wavefront_entry_points() {
    let Some(slices) = slice_headers("libheif_example.heic", 20004) else {
        return;
    };
    let (h, nal_len) = &slices[0];
    assert_eq!(h.slice_qp_delta, -9);
    // One entry point per CTB row after the first: 856 / 64 = 14 rows.
    assert_eq!(h.entry_point_offsets.len(), 13);
    assert_eq!(h.entry_point_offsets[0], 8266);
    // Substreams must fit inside the NAL unit.
    let total: usize = h.entry_point_offsets.iter().map(|&o| o as usize).sum();
    assert!(total + h.header_size < *nal_len);
}

#[test]
fn cabac_starts_on_real_slice_data() {
    use heifer_hevc_dec::cabac::ArithmeticDecoder;
    use heifer_hevc_dec::contexts::Contexts;
    use heifer_hevc_dec::slice::SliceHeader;
    for (file, item) in [
        ("single_image.heic", 1002),
        ("grid.heic", 1011),
        ("libheif_example.heic", 20004),
    ] {
        let Some(stream) = bitstream(file, item) else {
            continue;
        };
        let mut sets = heifer_hevc_dec::params::ParameterSets::default();
        for raw in split_annex_b(&stream) {
            let nal = NalUnit::parse(raw).unwrap();
            if !sets.add(&nal).unwrap() && nal.header.unit_type.is_slice() {
                let h = SliceHeader::parse(&nal.rbsp, &nal.header, &sets, None).unwrap();
                let _contexts = Contexts::new(h.slice_qp_y(&sets).unwrap());
                // The first 9 bits of slice data must form a valid initial CABAC offset (< 510).
                ArithmeticDecoder::new(&nal.rbsp[h.header_size..]).unwrap();
            }
        }
    }
}
