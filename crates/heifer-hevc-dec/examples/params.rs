//! Prints the parameter sets and slice headers of an HEVC Annex B file (`.h265`) using the spec's field names,
//! in the same `name = value` form as `ffmpeg -bsf:v trace_headers`, for comparison.
//!
//! Usage: cargo run -p heifer-hevc-dec --example params -- <file.h265>

use heifer_hevc_dec::nal::{NalUnit, split_annex_b};
use heifer_hevc_dec::params::ParameterSets;
use heifer_hevc_dec::slice::{SliceHeader, SliceType};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: params <file.h265>")?;
    let stream = std::fs::read(path)?;
    let mut sets = ParameterSets::default();
    let mut slices = Vec::new();
    for nal in split_annex_b(&stream) {
        let nal = NalUnit::parse(nal)?;
        if !sets.add(&nal)? && nal.header.unit_type.is_slice() {
            let previous = slices
                .iter()
                .rev()
                .find(|s: &&SliceHeader| !s.dependent_slice_segment_flag);
            let header = SliceHeader::parse(&nal.rbsp, &nal.header, &sets, previous)?;
            slices.push(header);
        }
    }

    let p = |name: &str, value: &dyn std::fmt::Display| println!("{name} = {value}");
    let b = |v: bool| u8::from(v);

    for sps in sets.sps.iter().flatten() {
        let ptl = &sps.profile_tier_level;
        p(
            "sps_video_parameter_set_id",
            &sps.sps_video_parameter_set_id,
        );
        p("sps_max_sub_layers_minus1", &sps.sps_max_sub_layers_minus1);
        p("general_profile_space", &ptl.general_profile_space);
        p("general_tier_flag", &b(ptl.general_tier_flag));
        p("general_profile_idc", &ptl.general_profile_idc);
        p("general_level_idc", &ptl.general_level_idc);
        p("sps_seq_parameter_set_id", &sps.sps_seq_parameter_set_id);
        p("chroma_format_idc", &sps.chroma_format_idc);
        p("pic_width_in_luma_samples", &sps.pic_width_in_luma_samples);
        p(
            "pic_height_in_luma_samples",
            &sps.pic_height_in_luma_samples,
        );
        for (name, v) in ["left", "right", "top", "bottom"]
            .iter()
            .zip(sps.conf_win_offsets)
        {
            p(&format!("conf_win_{name}_offset"), &v);
        }
        p("bit_depth_luma_minus8", &(sps.bit_depth_luma - 8));
        p("bit_depth_chroma_minus8", &(sps.bit_depth_chroma - 8));
        p(
            "log2_max_pic_order_cnt_lsb_minus4",
            &(sps.log2_max_pic_order_cnt_lsb - 4),
        );
        p(
            "log2_min_luma_coding_block_size_minus3",
            &(sps.log2_min_luma_coding_block_size - 3),
        );
        p(
            "log2_diff_max_min_luma_coding_block_size",
            &(sps.log2_ctb_size - sps.log2_min_luma_coding_block_size),
        );
        p(
            "log2_min_luma_transform_block_size_minus2",
            &(sps.log2_min_luma_transform_block_size - 2),
        );
        p(
            "log2_diff_max_min_luma_transform_block_size",
            &(sps.log2_max_luma_transform_block_size - sps.log2_min_luma_transform_block_size),
        );
        p(
            "max_transform_hierarchy_depth_intra",
            &sps.max_transform_hierarchy_depth_intra,
        );
        p("scaling_list_enabled_flag", &b(sps.scaling_list.is_some()));
        p("amp_enabled_flag", &b(sps.amp_enabled_flag));
        p(
            "sample_adaptive_offset_enabled_flag",
            &b(sps.sample_adaptive_offset_enabled_flag),
        );
        p("pcm_enabled_flag", &b(sps.pcm.is_some()));
        p(
            "num_short_term_ref_pic_sets",
            &sps.st_rps_num_delta_pocs.len(),
        );
        p(
            "long_term_ref_pics_present_flag",
            &b(sps.long_term_ref_pics_present_flag),
        );
        p(
            "sps_temporal_mvp_enabled_flag",
            &b(sps.sps_temporal_mvp_enabled_flag),
        );
        p(
            "strong_intra_smoothing_enabled_flag",
            &b(sps.strong_intra_smoothing_enabled_flag),
        );
        p("vui_parameters_present_flag", &b(sps.vui.is_some()));
        if let Some(vui) = &sps.vui {
            p("video_full_range_flag", &b(vui.video_full_range_flag));
            p("colour_primaries", &vui.colour_primaries);
            p("transfer_characteristics", &vui.transfer_characteristics);
            p("matrix_coefficients", &vui.matrix_coeffs);
        }
    }

    for pps in sets.pps.iter().flatten() {
        p("pps_pic_parameter_set_id", &pps.pps_pic_parameter_set_id);
        p("pps_seq_parameter_set_id", &pps.pps_seq_parameter_set_id);
        p(
            "dependent_slice_segments_enabled_flag",
            &b(pps.dependent_slice_segments_enabled_flag),
        );
        p("output_flag_present_flag", &b(pps.output_flag_present_flag));
        p(
            "num_extra_slice_header_bits",
            &pps.num_extra_slice_header_bits,
        );
        p(
            "sign_data_hiding_enabled_flag",
            &b(pps.sign_data_hiding_enabled_flag),
        );
        p("cabac_init_present_flag", &b(pps.cabac_init_present_flag));
        p(
            "num_ref_idx_l0_default_active_minus1",
            &pps.num_ref_idx_l0_default_active_minus1,
        );
        p(
            "num_ref_idx_l1_default_active_minus1",
            &pps.num_ref_idx_l1_default_active_minus1,
        );
        p("init_qp_minus26", &pps.init_qp_minus26);
        p(
            "constrained_intra_pred_flag",
            &b(pps.constrained_intra_pred_flag),
        );
        p(
            "transform_skip_enabled_flag",
            &b(pps.transform_skip_enabled_flag),
        );
        p(
            "cu_qp_delta_enabled_flag",
            &b(pps.diff_cu_qp_delta_depth.is_some()),
        );
        if let Some(d) = pps.diff_cu_qp_delta_depth {
            p("diff_cu_qp_delta_depth", &d);
        }
        p("pps_cb_qp_offset", &pps.pps_cb_qp_offset);
        p("pps_cr_qp_offset", &pps.pps_cr_qp_offset);
        p(
            "pps_slice_chroma_qp_offsets_present_flag",
            &b(pps.pps_slice_chroma_qp_offsets_present_flag),
        );
        p("weighted_pred_flag", &b(pps.weighted_pred_flag));
        p("weighted_bipred_flag", &b(pps.weighted_bipred_flag));
        p(
            "transquant_bypass_enabled_flag",
            &b(pps.transquant_bypass_enabled_flag),
        );
        p("tiles_enabled_flag", &b(pps.tiles.is_some()));
        p(
            "entropy_coding_sync_enabled_flag",
            &b(pps.entropy_coding_sync_enabled_flag),
        );
        p(
            "pps_loop_filter_across_slices_enabled_flag",
            &b(pps.pps_loop_filter_across_slices_enabled_flag),
        );
        p(
            "deblocking_filter_override_enabled_flag",
            &b(pps.deblocking_filter_override_enabled_flag),
        );
        p(
            "pps_deblocking_filter_disabled_flag",
            &b(pps.pps_deblocking_filter_disabled_flag),
        );
        p(
            "pps_scaling_list_data_present_flag",
            &b(pps.scaling_list.is_some()),
        );
        p(
            "lists_modification_present_flag",
            &b(pps.lists_modification_present_flag),
        );
        p(
            "log2_parallel_merge_level_minus2",
            &(pps.log2_parallel_merge_level - 2),
        );
        p(
            "slice_segment_header_extension_present_flag",
            &b(pps.slice_segment_header_extension_present_flag),
        );
    }
    for s in &slices {
        p(
            "first_slice_segment_in_pic_flag",
            &b(s.first_slice_segment_in_pic_flag),
        );
        p(
            "no_output_of_prior_pics_flag",
            &b(s.no_output_of_prior_pics_flag),
        );
        p("slice_pic_parameter_set_id", &s.slice_pic_parameter_set_id);
        let slice_type = match s.slice_type {
            SliceType::B => 0,
            SliceType::P => 1,
            SliceType::I => 2,
        };
        p("slice_type", &slice_type);
        p("slice_sao_luma_flag", &b(s.slice_sao_luma_flag));
        p("slice_sao_chroma_flag", &b(s.slice_sao_chroma_flag));
        p("slice_qp_delta", &s.slice_qp_delta);
        p(
            "slice_loop_filter_across_slices_enabled_flag",
            &b(s.slice_loop_filter_across_slices_enabled_flag),
        );
        if !s.entry_point_offsets.is_empty() {
            p("num_entry_point_offsets", &s.entry_point_offsets.len());
            for (i, o) in s.entry_point_offsets.iter().enumerate() {
                p(&format!("entry_point_offset_minus1[{i}]"), &(o - 1));
            }
        }
        p("# SliceQpY", &s.slice_qp_y(&sets)?);
        p("# header_size_bytes", &s.header_size);
    }
    Ok(())
}
