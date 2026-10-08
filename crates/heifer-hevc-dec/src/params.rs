//! Parameter sets: VPS (§7.3.2.1), SPS (§7.3.2.2) and PPS (§7.3.2.3) of ITU-T H.265.
//!
//! Field names follow the specification so they can be looked up directly.
//! Syntax that only matters for video (reference picture sets, HRD, timing) is parsed
//! so that the following fields can be reached, but only kept where it may be useful.

use crate::Error;
use crate::bitreader::BitReader;

/// `profile_tier_level()` (§7.3.3), general part only.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProfileTierLevel {
    /// `general_profile_space`.
    pub general_profile_space: u8,
    /// `general_tier_flag`.
    pub general_tier_flag: bool,
    /// `general_profile_idc` (1 = Main, 2 = Main 10, 3 = Main Still Picture, 4 = Range Extensions).
    pub general_profile_idc: u8,
    /// `general_profile_compatibility_flag[j]`, bit `31 - j`.
    pub general_profile_compatibility_flags: u32,
    /// `general_level_idc` (30 × level number, e.g. 186 = level 6.2).
    pub general_level_idc: u8,
}

fn profile_tier_level(
    r: &mut BitReader<'_>,
    max_sub_layers_minus1: u32,
) -> Result<ProfileTierLevel, Error> {
    let ptl = ProfileTierLevel {
        general_profile_space: r.bits(2)? as u8,
        general_tier_flag: r.flag()?,
        general_profile_idc: r.bits(5)? as u8,
        general_profile_compatibility_flags: r.bits(32)?,
        general_level_idc: {
            // progressive, interlaced, non_packed, frame_only + 43 constraint bits + 1 bit.
            r.skip(4 + 43 + 1)?;
            r.bits(8)? as u8
        },
    };
    let mut profile_present = [false; 8];
    let mut level_present = [false; 8];
    for i in 0..max_sub_layers_minus1 as usize {
        profile_present[i] = r.flag()?;
        level_present[i] = r.flag()?;
    }
    if max_sub_layers_minus1 > 0 {
        r.skip(2 * (8 - max_sub_layers_minus1 as usize))?; // reserved_zero_2bits
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if profile_present[i] {
            r.skip(88)?;
        }
        if level_present[i] {
            r.skip(8)?;
        }
    }
    Ok(ptl)
}

/// Video parameter set. Only the fields useful to a single-layer still image decoder are kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vps {
    /// `vps_video_parameter_set_id`.
    pub vps_video_parameter_set_id: u8,
    /// `vps_max_sub_layers_minus1`.
    pub vps_max_sub_layers_minus1: u8,
    /// Profile, tier and level.
    pub profile_tier_level: ProfileTierLevel,
}

impl Vps {
    /// Parses a VPS RBSP.
    pub fn parse(rbsp: &[u8]) -> Result<Self, Error> {
        let mut r = BitReader::new(rbsp);
        let vps_video_parameter_set_id = r.bits(4)? as u8;
        r.skip(2)?; // base_layer_internal_flag, base_layer_available_flag
        r.bits(6)?; // vps_max_layers_minus1
        let max_sub_layers_minus1 = r.bits(3)?;
        if max_sub_layers_minus1 > 6 {
            return Err(Error::Invalid("vps_max_sub_layers_minus1 > 6"));
        }
        r.flag()?; // vps_temporal_id_nesting_flag
        if r.bits(16)? != 0xFFFF {
            return Err(Error::Invalid("vps_reserved_0xffff_16bits"));
        }
        let profile_tier_level = profile_tier_level(&mut r, max_sub_layers_minus1)?;
        Ok(Self {
            vps_video_parameter_set_id,
            vps_max_sub_layers_minus1: max_sub_layers_minus1 as u8,
            profile_tier_level,
        })
    }
}

/// Scaling matrices (`scaling_list_data()`, §7.3.4), indexed `[sizeId][matrixId]`.
///
/// `sizeId` 0..=3 is 4×4, 8×8, 16×16, 32×32; `matrixId` 0..=2 are intra Y/Cb/Cr, 3..=5 inter.
/// Coefficients are stored in coded order (up-right diagonal scan), as in the spec's
/// `ScalingList[sizeId][matrixId][i]`; the decoder maps them to positions later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScalingList {
    /// `ScalingList[sizeId][matrixId][i]`, 16 coefficients for sizeId 0, 64 otherwise.
    pub lists: [[Vec<u8>; 6]; 4],
    /// `scaling_list_dc_coef_minus8 + 8` for sizeId 2 and 3 (16×16 and 32×32).
    pub dc: [[u8; 6]; 2],
}

/// Default 8×8 intra scaling list (Table 7-6), in up-right diagonal order.
#[rustfmt::skip]
const DEFAULT_INTRA_8X8: [u8; 64] = [
    16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 16, 17, 16, 17, 18,
    17, 18, 18, 17, 18, 21, 19, 20, 21, 20, 19, 21, 24, 22, 22, 24,
    24, 22, 22, 24, 25, 25, 27, 30, 27, 25, 25, 29, 31, 35, 35, 31,
    29, 36, 41, 44, 41, 36, 47, 54, 54, 47, 65, 70, 65, 88, 88, 115,
];

/// Default 8×8 inter scaling list (Table 7-6), in up-right diagonal order.
#[rustfmt::skip]
const DEFAULT_INTER_8X8: [u8; 64] = [
    16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 17, 17, 17, 17, 18,
    18, 18, 18, 18, 18, 20, 20, 20, 20, 20, 20, 20, 24, 24, 24, 24,
    24, 24, 24, 24, 25, 25, 25, 25, 25, 25, 25, 28, 28, 28, 28, 28,
    28, 33, 33, 33, 33, 33, 41, 41, 41, 41, 54, 54, 54, 71, 71, 91,
];

impl ScalingList {
    /// Default lists (Table 7-5 and 7-6), used when scaling lists are enabled but not transmitted.
    pub fn default_lists() -> Self {
        let lists = core::array::from_fn(|size_id| {
            core::array::from_fn(|matrix_id| Self::default_list(size_id, matrix_id))
        });
        Self {
            lists,
            dc: [[16; 6]; 2],
        }
    }

    fn default_list(size_id: usize, matrix_id: usize) -> Vec<u8> {
        match (size_id, matrix_id) {
            (0, _) => vec![16; 16],
            (_, 0..=2) => DEFAULT_INTRA_8X8.to_vec(),
            _ => DEFAULT_INTER_8X8.to_vec(),
        }
    }

    fn parse(r: &mut BitReader<'_>) -> Result<Self, Error> {
        let mut s = Self::default_lists();
        for size_id in 0..4 {
            let step = if size_id == 3 { 3 } else { 1 };
            for matrix_id in (0..6).step_by(step) {
                let pred_mode_flag = r.flag()?;
                if !pred_mode_flag {
                    let delta = r.ue_max(
                        matrix_id as u32 / step as u32,
                        "scaling_list_pred_matrix_id_delta",
                    )? as usize;
                    if delta == 0 {
                        s.lists[size_id][matrix_id] = Self::default_list(size_id, matrix_id);
                        if size_id > 1 {
                            s.dc[size_id - 2][matrix_id] = 16;
                        }
                    } else {
                        let ref_id = matrix_id - delta * step;
                        s.lists[size_id][matrix_id] = s.lists[size_id][ref_id].clone();
                        if size_id > 1 {
                            s.dc[size_id - 2][matrix_id] = s.dc[size_id - 2][ref_id];
                        }
                    }
                } else {
                    let coef_num = if size_id == 0 { 16 } else { 64 };
                    let mut next_coef: i32 = 8;
                    if size_id > 1 {
                        let dc = r.se_range(-7, 247, "scaling_list_dc_coef_minus8")? + 8;
                        next_coef = dc;
                        s.dc[size_id - 2][matrix_id] = dc as u8;
                    }
                    let list = &mut s.lists[size_id][matrix_id];
                    list.clear();
                    for _ in 0..coef_num {
                        let delta = r.se_range(-128, 127, "scaling_list_delta_coef")?;
                        next_coef = (next_coef + delta + 256) % 256;
                        if next_coef == 0 {
                            return Err(Error::Invalid("scaling list coefficient is 0"));
                        }
                        list.push(next_coef as u8);
                    }
                }
            }
            if size_id == 3 {
                // Chroma 32×32 lists (only used with 4:4:4) are copied from the 16×16 ones (§7.4.5).
                for matrix_id in [1, 2, 4, 5] {
                    s.lists[3][matrix_id] = s.lists[2][matrix_id].clone();
                    s.dc[1][matrix_id] = s.dc[0][matrix_id];
                }
            }
        }
        Ok(s)
    }
}

/// Reads one `st_ref_pic_set(stRpsIdx)` (§7.3.7) and returns its `NumDeltaPocs`.
/// Contents are discarded: they only matter for inter prediction.
///
/// `num_delta_pocs` holds `NumDeltaPocs` of the sets decoded so far. In a slice header
/// (`in_slice_header`), `delta_idx_minus1` selects the reference set; in the SPS it is always
/// the previous set.
pub(crate) fn skip_st_ref_pic_set(
    r: &mut BitReader<'_>,
    idx: usize,
    num_delta_pocs: &[u32],
    in_slice_header: bool,
) -> Result<u32, Error> {
    let inter_ref_pic_set_prediction_flag = idx != 0 && r.flag()?;
    if inter_ref_pic_set_prediction_flag {
        let delta_idx = if in_slice_header {
            r.ue_max(idx as u32 - 1, "delta_idx_minus1")? as usize + 1
        } else {
            1
        };
        let ref_num = num_delta_pocs[idx - delta_idx];
        r.flag()?; // delta_rps_sign
        r.ue_max(32767, "abs_delta_rps_minus1")?;
        let mut count = 0;
        for _ in 0..=ref_num {
            let used_by_curr_pic_flag = r.flag()?;
            let use_delta_flag = used_by_curr_pic_flag || r.flag()?;
            if use_delta_flag {
                count += 1;
            }
        }
        Ok(count)
    } else {
        let num_negative_pics = r.ue_max(16, "num_negative_pics")?;
        let num_positive_pics = r.ue_max(16, "num_positive_pics")?;
        for _ in 0..num_negative_pics + num_positive_pics {
            r.ue_max(32767, "delta_poc_minus1")?;
            r.flag()?; // used_by_curr_pic_flag
        }
        Ok(num_negative_pics + num_positive_pics)
    }
}

fn skip_sub_layer_hrd(r: &mut BitReader<'_>, cpb_cnt: u32, sub_pic: bool) -> Result<(), Error> {
    for _ in 0..cpb_cnt {
        r.ue()?; // bit_rate_value_minus1
        r.ue()?; // cpb_size_value_minus1
        if sub_pic {
            r.ue()?; // cpb_size_du_value_minus1
            r.ue()?; // bit_rate_du_value_minus1
        }
        r.flag()?; // cbr_flag
    }
    Ok(())
}

/// Skips `hrd_parameters(commonInfPresentFlag, maxNumSubLayersMinus1)` (§E.2.2).
fn skip_hrd_parameters(
    r: &mut BitReader<'_>,
    common: bool,
    max_sub_layers_minus1: u32,
) -> Result<(), Error> {
    let (mut nal, mut vcl, mut sub_pic) = (false, false, false);
    if common {
        nal = r.flag()?;
        vcl = r.flag()?;
        if nal || vcl {
            sub_pic = r.flag()?;
            if sub_pic {
                r.skip(8 + 5 + 1 + 5)?;
            }
            r.skip(4 + 4)?; // bit_rate_scale, cpb_size_scale
            if sub_pic {
                r.skip(4)?; // cpb_size_du_scale
            }
            r.skip(5 + 5 + 5)?;
        }
    }
    for _ in 0..=max_sub_layers_minus1 {
        let fixed_pic_rate_general_flag = r.flag()?;
        let fixed_pic_rate_within_cvs_flag = fixed_pic_rate_general_flag || r.flag()?;
        let mut low_delay_hrd_flag = false;
        if fixed_pic_rate_within_cvs_flag {
            r.ue()?; // elemental_duration_in_tc_minus1
        } else {
            low_delay_hrd_flag = r.flag()?;
        }
        let mut cpb_cnt = 1;
        if !low_delay_hrd_flag {
            cpb_cnt = r.ue_max(31, "cpb_cnt_minus1")? + 1;
        }
        if nal {
            skip_sub_layer_hrd(r, cpb_cnt, sub_pic)?;
        }
        if vcl {
            skip_sub_layer_hrd(r, cpb_cnt, sub_pic)?;
        }
    }
    Ok(())
}

/// Video usability information (§E.2.1). Only colour-related fields are kept.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Vui {
    /// `video_full_range_flag`.
    pub video_full_range_flag: bool,
    /// `colour_primaries` (2 = unspecified).
    pub colour_primaries: u8,
    /// `transfer_characteristics` (2 = unspecified).
    pub transfer_characteristics: u8,
    /// `matrix_coeffs` (2 = unspecified).
    pub matrix_coeffs: u8,
}

fn parse_vui(r: &mut BitReader<'_>, max_sub_layers_minus1: u32) -> Result<Vui, Error> {
    let mut vui = Vui {
        colour_primaries: 2,
        transfer_characteristics: 2,
        matrix_coeffs: 2,
        ..Vui::default()
    };
    if r.flag()? {
        // aspect_ratio_info_present_flag
        if r.bits(8)? == 255 {
            r.skip(32)?; // sar_width, sar_height
        }
    }
    if r.flag()? {
        r.flag()?; // overscan_appropriate_flag
    }
    if r.flag()? {
        // video_signal_type_present_flag
        r.bits(3)?; // video_format
        vui.video_full_range_flag = r.flag()?;
        if r.flag()? {
            vui.colour_primaries = r.bits(8)? as u8;
            vui.transfer_characteristics = r.bits(8)? as u8;
            vui.matrix_coeffs = r.bits(8)? as u8;
        }
    }
    if r.flag()? {
        r.ue()?; // chroma_sample_loc_type_top_field
        r.ue()?; // chroma_sample_loc_type_bottom_field
    }
    r.skip(3)?; // neutral_chroma_indication_flag, field_seq_flag, frame_field_info_present_flag
    if r.flag()? {
        // default_display_window_flag
        for _ in 0..4 {
            r.ue()?;
        }
    }
    if r.flag()? {
        // vui_timing_info_present_flag
        r.skip(64)?; // num_units_in_tick, time_scale
        if r.flag()? {
            r.ue()?; // num_ticks_poc_diff_one_minus1
        }
        if r.flag()? {
            skip_hrd_parameters(r, true, max_sub_layers_minus1)?;
        }
    }
    if r.flag()? {
        // bitstream_restriction_flag
        r.skip(3)?;
        for _ in 0..5 {
            r.ue()?;
        }
    }
    Ok(vui)
}

/// `sps_range_extension()` (§7.3.2.2.2), used by Range Extensions profiles (4:4:4, high bit depths).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpsRangeExtension {
    /// `transform_skip_rotation_enabled_flag`.
    pub transform_skip_rotation_enabled_flag: bool,
    /// `transform_skip_context_enabled_flag`.
    pub transform_skip_context_enabled_flag: bool,
    /// `implicit_rdpcm_enabled_flag`.
    pub implicit_rdpcm_enabled_flag: bool,
    /// `explicit_rdpcm_enabled_flag`.
    pub explicit_rdpcm_enabled_flag: bool,
    /// `extended_precision_processing_flag`.
    pub extended_precision_processing_flag: bool,
    /// `intra_smoothing_disabled_flag`.
    pub intra_smoothing_disabled_flag: bool,
    /// `high_precision_offsets_enabled_flag`.
    pub high_precision_offsets_enabled_flag: bool,
    /// `persistent_rice_adaptation_enabled_flag`.
    pub persistent_rice_adaptation_enabled_flag: bool,
    /// `cabac_bypass_alignment_enabled_flag`.
    pub cabac_bypass_alignment_enabled_flag: bool,
}

/// PCM parameters, present when `pcm_enabled_flag` is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pcm {
    /// `pcm_sample_bit_depth_luma_minus1 + 1`.
    pub bit_depth_luma: u8,
    /// `pcm_sample_bit_depth_chroma_minus1 + 1`.
    pub bit_depth_chroma: u8,
    /// `Log2MinIpcmCbSizeY`.
    pub log2_min_size: u8,
    /// `Log2MaxIpcmCbSizeY`.
    pub log2_max_size: u8,
    /// `pcm_loop_filter_disabled_flag`.
    pub loop_filter_disabled_flag: bool,
}

/// Sequence parameter set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sps {
    /// `sps_video_parameter_set_id`.
    pub sps_video_parameter_set_id: u8,
    /// `sps_max_sub_layers_minus1`.
    pub sps_max_sub_layers_minus1: u8,
    /// Profile, tier and level.
    pub profile_tier_level: ProfileTierLevel,
    /// `sps_seq_parameter_set_id` (0..=15).
    pub sps_seq_parameter_set_id: u8,
    /// `chroma_format_idc`: 0 = monochrome, 1 = 4:2:0, 2 = 4:2:2, 3 = 4:4:4.
    pub chroma_format_idc: u8,
    /// `separate_colour_plane_flag`.
    pub separate_colour_plane_flag: bool,
    /// `pic_width_in_luma_samples` (coded width, before cropping).
    pub pic_width_in_luma_samples: u32,
    /// `pic_height_in_luma_samples` (coded height, before cropping).
    pub pic_height_in_luma_samples: u32,
    /// Conformance window `[left, right, top, bottom]`, in chroma sample units.
    pub conf_win_offsets: [u32; 4],
    /// `BitDepthY`.
    pub bit_depth_luma: u8,
    /// `BitDepthC`.
    pub bit_depth_chroma: u8,
    /// `log2_max_pic_order_cnt_lsb_minus4 + 4`, needed to parse slice headers.
    pub log2_max_pic_order_cnt_lsb: u8,
    /// `MinCbLog2SizeY`.
    pub log2_min_luma_coding_block_size: u8,
    /// `CtbLog2SizeY`.
    pub log2_ctb_size: u8,
    /// `MinTbLog2SizeY`.
    pub log2_min_luma_transform_block_size: u8,
    /// `MaxTbLog2SizeY`.
    pub log2_max_luma_transform_block_size: u8,
    /// `max_transform_hierarchy_depth_intra`.
    pub max_transform_hierarchy_depth_intra: u8,
    /// Scaling lists in use, if `scaling_list_enabled_flag` is set.
    pub scaling_list: Option<ScalingList>,
    /// `amp_enabled_flag`.
    pub amp_enabled_flag: bool,
    /// `sample_adaptive_offset_enabled_flag`.
    pub sample_adaptive_offset_enabled_flag: bool,
    /// PCM parameters, if `pcm_enabled_flag` is set.
    pub pcm: Option<Pcm>,
    /// `NumDeltaPocs` of each short-term reference picture set; its length is
    /// `num_short_term_ref_pic_sets`. Needed to parse slice headers.
    pub st_rps_num_delta_pocs: Vec<u32>,
    /// `long_term_ref_pics_present_flag`.
    pub long_term_ref_pics_present_flag: bool,
    /// `num_long_term_ref_pics_sps`, needed to parse slice headers.
    pub num_long_term_ref_pics_sps: u8,
    /// `sps_temporal_mvp_enabled_flag`.
    pub sps_temporal_mvp_enabled_flag: bool,
    /// `strong_intra_smoothing_enabled_flag`.
    pub strong_intra_smoothing_enabled_flag: bool,
    /// Video usability information, if present.
    pub vui: Option<Vui>,
    /// Range extension flags (all false when absent).
    pub range_extension: SpsRangeExtension,
}

impl Sps {
    /// Parses an SPS RBSP.
    pub fn parse(rbsp: &[u8]) -> Result<Self, Error> {
        let mut r = BitReader::new(rbsp);
        let sps_video_parameter_set_id = r.bits(4)? as u8;
        let max_sub_layers_minus1 = r.bits(3)?;
        if max_sub_layers_minus1 > 6 {
            return Err(Error::Invalid("sps_max_sub_layers_minus1 > 6"));
        }
        r.flag()?; // sps_temporal_id_nesting_flag
        let profile_tier_level = profile_tier_level(&mut r, max_sub_layers_minus1)?;
        let sps_seq_parameter_set_id = r.ue_max(15, "sps_seq_parameter_set_id")? as u8;
        let chroma_format_idc = r.ue_max(3, "chroma_format_idc")? as u8;
        let separate_colour_plane_flag = chroma_format_idc == 3 && r.flag()?;
        let pic_width_in_luma_samples = r.ue()?;
        let pic_height_in_luma_samples = r.ue()?;
        if pic_width_in_luma_samples == 0 || pic_height_in_luma_samples == 0 {
            return Err(Error::Invalid("picture size is 0"));
        }
        let mut conf_win_offsets = [0; 4];
        if r.flag()? {
            for o in &mut conf_win_offsets {
                *o = r.ue()?;
            }
        }
        let bit_depth_luma = r.ue_max(8, "bit_depth_luma_minus8")? as u8 + 8;
        let bit_depth_chroma = r.ue_max(8, "bit_depth_chroma_minus8")? as u8 + 8;
        let log2_max_pic_order_cnt_lsb =
            r.ue_max(12, "log2_max_pic_order_cnt_lsb_minus4")? as u8 + 4;
        let sub_layer_ordering_info_present_flag = r.flag()?;
        let first = if sub_layer_ordering_info_present_flag {
            0
        } else {
            max_sub_layers_minus1
        };
        for _ in first..=max_sub_layers_minus1 {
            r.ue()?; // sps_max_dec_pic_buffering_minus1
            r.ue()?; // sps_max_num_reorder_pics
            r.ue()?; // sps_max_latency_increase_plus1
        }
        let log2_min_cb = r.ue_max(3, "log2_min_luma_coding_block_size_minus3")? + 3;
        let log2_ctb = log2_min_cb + r.ue_max(3, "log2_diff_max_min_luma_coding_block_size")?;
        let log2_min_tb = r.ue_max(3, "log2_min_luma_transform_block_size_minus2")? + 2;
        let log2_max_tb =
            log2_min_tb + r.ue_max(3, "log2_diff_max_min_luma_transform_block_size")?;
        if !(4..=6).contains(&log2_ctb) {
            return Err(Error::Invalid("CTB size must be 16, 32 or 64"));
        }
        if log2_min_tb >= log2_min_cb || log2_max_tb > log2_ctb.min(5) {
            return Err(Error::Invalid("inconsistent transform block sizes"));
        }
        if pic_width_in_luma_samples % (1 << log2_min_cb) != 0
            || pic_height_in_luma_samples % (1 << log2_min_cb) != 0
        {
            return Err(Error::Invalid(
                "picture size is not a multiple of the minimum coding block size",
            ));
        }
        let depth_max = log2_ctb - log2_min_tb;
        r.ue_max(depth_max, "max_transform_hierarchy_depth_inter")?;
        let max_transform_hierarchy_depth_intra =
            r.ue_max(depth_max, "max_transform_hierarchy_depth_intra")? as u8;

        let scaling_list = if r.flag()? {
            Some(if r.flag()? {
                ScalingList::parse(&mut r)?
            } else {
                ScalingList::default_lists()
            })
        } else {
            None
        };
        let amp_enabled_flag = r.flag()?;
        let sample_adaptive_offset_enabled_flag = r.flag()?;
        let pcm = if r.flag()? {
            let bit_depth_luma_pcm = r.bits(4)? as u8 + 1;
            let bit_depth_chroma_pcm = r.bits(4)? as u8 + 1;
            let log2_min_size =
                r.ue_max(2, "log2_min_pcm_luma_coding_block_size_minus3")? as u8 + 3;
            let log2_max_size =
                log2_min_size + r.ue_max(2, "log2_diff_max_min_pcm_luma_coding_block_size")? as u8;
            if bit_depth_luma_pcm > bit_depth_luma || bit_depth_chroma_pcm > bit_depth_chroma {
                return Err(Error::Invalid(
                    "PCM bit depth larger than picture bit depth",
                ));
            }
            Some(Pcm {
                bit_depth_luma: bit_depth_luma_pcm,
                bit_depth_chroma: bit_depth_chroma_pcm,
                log2_min_size,
                log2_max_size,
                loop_filter_disabled_flag: r.flag()?,
            })
        } else {
            None
        };

        let num_short_term_ref_pic_sets = r.ue_max(64, "num_short_term_ref_pic_sets")? as usize;
        let mut num_delta_pocs = Vec::with_capacity(num_short_term_ref_pic_sets);
        for i in 0..num_short_term_ref_pic_sets {
            let n = skip_st_ref_pic_set(&mut r, i, &num_delta_pocs, false)?;
            num_delta_pocs.push(n);
        }
        let long_term_ref_pics_present_flag = r.flag()?;
        let mut num_long_term_ref_pics_sps = 0;
        if long_term_ref_pics_present_flag {
            num_long_term_ref_pics_sps = r.ue_max(32, "num_long_term_ref_pics_sps")? as u8;
            for _ in 0..num_long_term_ref_pics_sps {
                r.skip(usize::from(log2_max_pic_order_cnt_lsb) + 1)?;
            }
        }
        let sps_temporal_mvp_enabled_flag = r.flag()?;
        let strong_intra_smoothing_enabled_flag = r.flag()?;
        let vui = if r.flag()? {
            Some(parse_vui(&mut r, max_sub_layers_minus1)?)
        } else {
            None
        };

        let mut range_extension = SpsRangeExtension::default();
        if r.flag()? {
            // sps_extension_present_flag
            let sps_range_extension_flag = r.flag()?;
            r.skip(3 + 4)?; // multilayer, 3d, scc, extension_4bits: ignored
            if sps_range_extension_flag {
                range_extension = SpsRangeExtension {
                    transform_skip_rotation_enabled_flag: r.flag()?,
                    transform_skip_context_enabled_flag: r.flag()?,
                    implicit_rdpcm_enabled_flag: r.flag()?,
                    explicit_rdpcm_enabled_flag: r.flag()?,
                    extended_precision_processing_flag: r.flag()?,
                    intra_smoothing_disabled_flag: r.flag()?,
                    high_precision_offsets_enabled_flag: r.flag()?,
                    persistent_rice_adaptation_enabled_flag: r.flag()?,
                    cabac_bypass_alignment_enabled_flag: r.flag()?,
                };
            }
        }

        let sps = Self {
            sps_video_parameter_set_id,
            sps_max_sub_layers_minus1: max_sub_layers_minus1 as u8,
            profile_tier_level,
            sps_seq_parameter_set_id,
            chroma_format_idc,
            separate_colour_plane_flag,
            pic_width_in_luma_samples,
            pic_height_in_luma_samples,
            conf_win_offsets,
            bit_depth_luma,
            bit_depth_chroma,
            log2_max_pic_order_cnt_lsb,
            log2_min_luma_coding_block_size: log2_min_cb as u8,
            log2_ctb_size: log2_ctb as u8,
            log2_min_luma_transform_block_size: log2_min_tb as u8,
            log2_max_luma_transform_block_size: log2_max_tb as u8,
            max_transform_hierarchy_depth_intra,
            scaling_list,
            amp_enabled_flag,
            sample_adaptive_offset_enabled_flag,
            pcm,
            st_rps_num_delta_pocs: num_delta_pocs,
            long_term_ref_pics_present_flag,
            num_long_term_ref_pics_sps,
            sps_temporal_mvp_enabled_flag,
            strong_intra_smoothing_enabled_flag,
            vui,
            range_extension,
        };
        let (w, h) = sps.output_size();
        if w == 0 || h == 0 {
            return Err(Error::Invalid("conformance window crops the whole picture"));
        }
        Ok(sps)
    }

    /// `ChromaArrayType`: 0 for monochrome or separate colour planes, else `chroma_format_idc`.
    pub fn chroma_array_type(&self) -> u8 {
        if self.separate_colour_plane_flag {
            0
        } else {
            self.chroma_format_idc
        }
    }

    /// (`SubWidthC`, `SubHeightC`) from Table 6-1.
    pub fn chroma_subsampling(&self) -> (u32, u32) {
        match self.chroma_array_type() {
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        }
    }

    /// CTB size in luma samples (`CtbSizeY`).
    pub fn ctb_size(&self) -> u32 {
        1 << self.log2_ctb_size
    }

    /// Picture size in CTBs (`PicWidthInCtbsY`, `PicHeightInCtbsY`).
    pub fn pic_size_in_ctbs(&self) -> (u32, u32) {
        (
            self.pic_width_in_luma_samples.div_ceil(self.ctb_size()),
            self.pic_height_in_luma_samples.div_ceil(self.ctb_size()),
        )
    }

    /// Displayed size, after applying the conformance window.
    pub fn output_size(&self) -> (u32, u32) {
        let (sw, sh) = self.chroma_subsampling();
        let [l, r, t, b] = self.conf_win_offsets;
        let crop_w = sw.saturating_mul(l.saturating_add(r));
        let crop_h = sh.saturating_mul(t.saturating_add(b));
        (
            self.pic_width_in_luma_samples.saturating_sub(crop_w),
            self.pic_height_in_luma_samples.saturating_sub(crop_h),
        )
    }
}

/// Tile layout (§6.5.1), present when `tiles_enabled_flag` is set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tiles {
    /// Column widths in CTBs, explicit or derived from uniform spacing later.
    pub column_widths: Option<Vec<u32>>,
    /// Row heights in CTBs, explicit or derived from uniform spacing later.
    pub row_heights: Option<Vec<u32>>,
    /// `num_tile_columns_minus1 + 1`.
    pub num_columns: u32,
    /// `num_tile_rows_minus1 + 1`.
    pub num_rows: u32,
    /// `loop_filter_across_tiles_enabled_flag`.
    pub loop_filter_across_tiles_enabled_flag: bool,
}

/// `pps_range_extension()` (§7.3.2.3.2).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PpsRangeExtension {
    /// `log2_max_transform_skip_block_size_minus2 + 2`.
    pub log2_max_transform_skip_block_size: u8,
    /// `cross_component_prediction_enabled_flag`.
    pub cross_component_prediction_enabled_flag: bool,
    /// `diff_cu_chroma_qp_offset_depth`, if `chroma_qp_offset_list_enabled_flag`.
    pub diff_cu_chroma_qp_offset_depth: Option<u8>,
    /// `(cb_qp_offset_list[i], cr_qp_offset_list[i])`.
    pub chroma_qp_offset_list: Vec<(i8, i8)>,
    /// `log2_sao_offset_scale_luma`.
    pub log2_sao_offset_scale_luma: u8,
    /// `log2_sao_offset_scale_chroma`.
    pub log2_sao_offset_scale_chroma: u8,
}

/// Picture parameter set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pps {
    /// `pps_pic_parameter_set_id` (0..=63).
    pub pps_pic_parameter_set_id: u8,
    /// `pps_seq_parameter_set_id` (0..=15).
    pub pps_seq_parameter_set_id: u8,
    /// `dependent_slice_segments_enabled_flag`.
    pub dependent_slice_segments_enabled_flag: bool,
    /// `output_flag_present_flag`.
    pub output_flag_present_flag: bool,
    /// `num_extra_slice_header_bits`.
    pub num_extra_slice_header_bits: u8,
    /// `sign_data_hiding_enabled_flag`.
    pub sign_data_hiding_enabled_flag: bool,
    /// `cabac_init_present_flag`.
    pub cabac_init_present_flag: bool,
    /// `num_ref_idx_l0_default_active_minus1`.
    pub num_ref_idx_l0_default_active_minus1: u8,
    /// `num_ref_idx_l1_default_active_minus1`.
    pub num_ref_idx_l1_default_active_minus1: u8,
    /// `init_qp_minus26`.
    pub init_qp_minus26: i8,
    /// `constrained_intra_pred_flag`.
    pub constrained_intra_pred_flag: bool,
    /// `transform_skip_enabled_flag`.
    pub transform_skip_enabled_flag: bool,
    /// `diff_cu_qp_delta_depth`, if `cu_qp_delta_enabled_flag` is set.
    pub diff_cu_qp_delta_depth: Option<u8>,
    /// `pps_cb_qp_offset`.
    pub pps_cb_qp_offset: i8,
    /// `pps_cr_qp_offset`.
    pub pps_cr_qp_offset: i8,
    /// `pps_slice_chroma_qp_offsets_present_flag`.
    pub pps_slice_chroma_qp_offsets_present_flag: bool,
    /// `weighted_pred_flag`.
    pub weighted_pred_flag: bool,
    /// `weighted_bipred_flag`.
    pub weighted_bipred_flag: bool,
    /// `transquant_bypass_enabled_flag`.
    pub transquant_bypass_enabled_flag: bool,
    /// Tile layout, if `tiles_enabled_flag` is set.
    pub tiles: Option<Tiles>,
    /// `entropy_coding_sync_enabled_flag` (wavefront parallel processing).
    pub entropy_coding_sync_enabled_flag: bool,
    /// `pps_loop_filter_across_slices_enabled_flag`.
    pub pps_loop_filter_across_slices_enabled_flag: bool,
    /// `deblocking_filter_override_enabled_flag`.
    pub deblocking_filter_override_enabled_flag: bool,
    /// `pps_deblocking_filter_disabled_flag`.
    pub pps_deblocking_filter_disabled_flag: bool,
    /// `pps_beta_offset_div2`.
    pub pps_beta_offset_div2: i8,
    /// `pps_tc_offset_div2`.
    pub pps_tc_offset_div2: i8,
    /// Scaling lists overriding the SPS ones, if `pps_scaling_list_data_present_flag` is set.
    pub scaling_list: Option<ScalingList>,
    /// `lists_modification_present_flag`.
    pub lists_modification_present_flag: bool,
    /// `log2_parallel_merge_level_minus2 + 2`.
    pub log2_parallel_merge_level: u8,
    /// `slice_segment_header_extension_present_flag`.
    pub slice_segment_header_extension_present_flag: bool,
    /// Range extension (defaults when absent).
    pub range_extension: PpsRangeExtension,
}

impl Pps {
    /// Parses a PPS RBSP. The SPS it refers to is needed to validate some ranges.
    pub fn parse(rbsp: &[u8], sps_list: &[Option<Sps>; 16]) -> Result<Self, Error> {
        let mut r = BitReader::new(rbsp);
        let pps_pic_parameter_set_id = r.ue_max(63, "pps_pic_parameter_set_id")? as u8;
        let pps_seq_parameter_set_id = r.ue_max(15, "pps_seq_parameter_set_id")? as u8;
        let sps = sps_list[usize::from(pps_seq_parameter_set_id)]
            .as_ref()
            .ok_or(Error::Invalid("PPS refers to a missing SPS"))?;
        let dependent_slice_segments_enabled_flag = r.flag()?;
        let output_flag_present_flag = r.flag()?;
        let num_extra_slice_header_bits = r.bits(3)? as u8;
        let sign_data_hiding_enabled_flag = r.flag()?;
        let cabac_init_present_flag = r.flag()?;
        let num_ref_idx_l0_default_active_minus1 =
            r.ue_max(14, "num_ref_idx_l0_default_active_minus1")? as u8;
        let num_ref_idx_l1_default_active_minus1 =
            r.ue_max(14, "num_ref_idx_l1_default_active_minus1")? as u8;
        let qp_bd_offset = 6 * (i32::from(sps.bit_depth_luma) - 8);
        let init_qp_minus26 = r.se_range(-(26 + qp_bd_offset), 25, "init_qp_minus26")? as i8;
        let constrained_intra_pred_flag = r.flag()?;
        let transform_skip_enabled_flag = r.flag()?;
        let diff_cu_qp_delta_depth = if r.flag()? {
            let max = u32::from(sps.log2_ctb_size - sps.log2_min_luma_coding_block_size);
            Some(r.ue_max(max, "diff_cu_qp_delta_depth")? as u8)
        } else {
            None
        };
        let pps_cb_qp_offset = r.se_range(-12, 12, "pps_cb_qp_offset")? as i8;
        let pps_cr_qp_offset = r.se_range(-12, 12, "pps_cr_qp_offset")? as i8;
        let pps_slice_chroma_qp_offsets_present_flag = r.flag()?;
        let weighted_pred_flag = r.flag()?;
        let weighted_bipred_flag = r.flag()?;
        let transquant_bypass_enabled_flag = r.flag()?;
        let tiles_enabled_flag = r.flag()?;
        let entropy_coding_sync_enabled_flag = r.flag()?;

        let tiles = if tiles_enabled_flag {
            let (w_ctbs, h_ctbs) = sps.pic_size_in_ctbs();
            let num_columns = r.ue_max(w_ctbs - 1, "num_tile_columns_minus1")? + 1;
            let num_rows = r.ue_max(h_ctbs - 1, "num_tile_rows_minus1")? + 1;
            let uniform_spacing_flag = r.flag()?;
            let (mut column_widths, mut row_heights) = (None, None);
            if !uniform_spacing_flag {
                let mut read_sizes = |n: u32, total: u32, what| -> Result<Vec<u32>, Error> {
                    let mut sizes = Vec::with_capacity(n as usize);
                    let mut used = 0u32;
                    for _ in 0..n - 1 {
                        let s = r.ue()? + 1;
                        used = used
                            .checked_add(s)
                            .filter(|&u| u < total)
                            .ok_or(Error::Invalid(what))?;
                        sizes.push(s);
                    }
                    sizes.push(total - used);
                    Ok(sizes)
                };
                column_widths = Some(read_sizes(
                    num_columns,
                    w_ctbs,
                    "tile columns exceed picture width",
                )?);
                row_heights = Some(read_sizes(
                    num_rows,
                    h_ctbs,
                    "tile rows exceed picture height",
                )?);
            }
            Some(Tiles {
                column_widths,
                row_heights,
                num_columns,
                num_rows,
                loop_filter_across_tiles_enabled_flag: r.flag()?,
            })
        } else {
            None
        };

        let pps_loop_filter_across_slices_enabled_flag = r.flag()?;
        let (mut deblocking_filter_override_enabled_flag, mut pps_deblocking_filter_disabled_flag) =
            (false, false);
        let (mut pps_beta_offset_div2, mut pps_tc_offset_div2) = (0, 0);
        if r.flag()? {
            // deblocking_filter_control_present_flag
            deblocking_filter_override_enabled_flag = r.flag()?;
            pps_deblocking_filter_disabled_flag = r.flag()?;
            if !pps_deblocking_filter_disabled_flag {
                pps_beta_offset_div2 = r.se_range(-6, 6, "pps_beta_offset_div2")? as i8;
                pps_tc_offset_div2 = r.se_range(-6, 6, "pps_tc_offset_div2")? as i8;
            }
        }
        let scaling_list = if r.flag()? {
            Some(ScalingList::parse(&mut r)?)
        } else {
            None
        };
        let lists_modification_present_flag = r.flag()?;
        let log2_parallel_merge_level = r.ue_max(
            u32::from(sps.log2_ctb_size) - 2,
            "log2_parallel_merge_level_minus2",
        )? as u8
            + 2;
        let slice_segment_header_extension_present_flag = r.flag()?;

        let mut range_extension = PpsRangeExtension {
            log2_max_transform_skip_block_size: 2,
            ..Default::default()
        };
        if r.flag()? {
            // pps_extension_present_flag
            let pps_range_extension_flag = r.flag()?;
            r.skip(3 + 4)?; // multilayer, 3d, scc, extension_4bits: ignored
            if pps_range_extension_flag {
                if transform_skip_enabled_flag {
                    range_extension.log2_max_transform_skip_block_size =
                        r.ue_max(3, "log2_max_transform_skip_block_size_minus2")? as u8 + 2;
                }
                range_extension.cross_component_prediction_enabled_flag = r.flag()?;
                if r.flag()? {
                    // chroma_qp_offset_list_enabled_flag
                    let max = u32::from(sps.log2_ctb_size - sps.log2_min_luma_coding_block_size);
                    range_extension.diff_cu_chroma_qp_offset_depth =
                        Some(r.ue_max(max, "diff_cu_chroma_qp_offset_depth")? as u8);
                    let len = r.ue_max(5, "chroma_qp_offset_list_len_minus1")? + 1;
                    for _ in 0..len {
                        let cb = r.se_range(-12, 12, "cb_qp_offset_list")? as i8;
                        let cr = r.se_range(-12, 12, "cr_qp_offset_list")? as i8;
                        range_extension.chroma_qp_offset_list.push((cb, cr));
                    }
                }
                let max_luma = u32::from(sps.bit_depth_luma.saturating_sub(10));
                let max_chroma = u32::from(sps.bit_depth_chroma.saturating_sub(10));
                range_extension.log2_sao_offset_scale_luma =
                    r.ue_max(max_luma, "log2_sao_offset_scale_luma")? as u8;
                range_extension.log2_sao_offset_scale_chroma =
                    r.ue_max(max_chroma, "log2_sao_offset_scale_chroma")? as u8;
            }
        }

        Ok(Self {
            pps_pic_parameter_set_id,
            pps_seq_parameter_set_id,
            dependent_slice_segments_enabled_flag,
            output_flag_present_flag,
            num_extra_slice_header_bits,
            sign_data_hiding_enabled_flag,
            cabac_init_present_flag,
            num_ref_idx_l0_default_active_minus1,
            num_ref_idx_l1_default_active_minus1,
            init_qp_minus26,
            constrained_intra_pred_flag,
            transform_skip_enabled_flag,
            diff_cu_qp_delta_depth,
            pps_cb_qp_offset,
            pps_cr_qp_offset,
            pps_slice_chroma_qp_offsets_present_flag,
            weighted_pred_flag,
            weighted_bipred_flag,
            transquant_bypass_enabled_flag,
            tiles,
            entropy_coding_sync_enabled_flag,
            pps_loop_filter_across_slices_enabled_flag,
            deblocking_filter_override_enabled_flag,
            pps_deblocking_filter_disabled_flag,
            pps_beta_offset_div2,
            pps_tc_offset_div2,
            scaling_list,
            lists_modification_present_flag,
            log2_parallel_merge_level,
            slice_segment_header_extension_present_flag,
            range_extension,
        })
    }
}

/// All active parameter sets, indexed by their ID.
#[derive(Debug, Clone, Default)]
pub struct ParameterSets {
    /// VPS by `vps_video_parameter_set_id`.
    pub vps: [Option<Vps>; 16],
    /// SPS by `sps_seq_parameter_set_id`.
    pub sps: [Option<Sps>; 16],
    /// PPS by `pps_pic_parameter_set_id`.
    pub pps: Vec<Option<Pps>>,
}

impl ParameterSets {
    /// Parses a parameter set NAL unit and stores it. Other NAL units are ignored.
    /// Returns whether the NAL unit was a parameter set.
    pub fn add(&mut self, nal: &crate::nal::NalUnit<'_>) -> Result<bool, Error> {
        use crate::nal::NalUnitType;
        match nal.header.unit_type {
            NalUnitType::Vps => {
                let vps = Vps::parse(&nal.rbsp)?;
                let id = usize::from(vps.vps_video_parameter_set_id);
                self.vps[id] = Some(vps);
            }
            NalUnitType::Sps => {
                let sps = Sps::parse(&nal.rbsp)?;
                let id = usize::from(sps.sps_seq_parameter_set_id);
                self.sps[id] = Some(sps);
            }
            NalUnitType::Pps => {
                let pps = Pps::parse(&nal.rbsp, &self.sps)?;
                let id = usize::from(pps.pps_pic_parameter_set_id);
                if self.pps.len() <= id {
                    self.pps.resize(id + 1, None);
                }
                self.pps[id] = Some(pps);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Returns a PPS and the SPS it refers to.
    pub fn get(&self, pps_id: usize) -> Result<(&Pps, &Sps), Error> {
        let pps = self
            .pps
            .get(pps_id)
            .and_then(Option::as_ref)
            .ok_or(Error::Invalid("missing PPS"))?;
        let sps = self.sps[usize::from(pps.pps_seq_parameter_set_id)]
            .as_ref()
            .ok_or(Error::Invalid("missing SPS"))?;
        Ok((pps, sps))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::testutil::BitWriter;

    pub(crate) fn ptl(w: &mut BitWriter) {
        w.bits(0, 2).bit(false).bits(1, 5).bits(0x6000_0000, 32);
        w.bits(0, 4 + 43 + 1).bits(93, 8);
    }

    /// A small SPS: 4:2:0, 64×48 cropped to 62×46, 8 bits, CTB 32, no tools.
    pub(crate) fn small_sps() -> Vec<u8> {
        let mut w = BitWriter::default();
        w.bits(0, 4).bits(0, 3).bit(true);
        ptl(&mut w);
        w.ue(0).ue(1).ue(64).ue(48);
        w.bit(true).ue(0).ue(1).ue(0).ue(1); // conformance window: right 1, bottom 1 (×2)
        w.ue(0).ue(0).ue(4);
        w.bit(true).ue(0).ue(0).ue(0);
        w.ue(0).ue(2).ue(0).ue(3).ue(1).ue(1); // CB 8..32, TB 4..32
        w.bit(false).bit(false).bit(true).bit(false); // scaling, amp, sao, pcm
        w.ue(1).ue(1).ue(0).ue(0).bit(true); // one st_ref_pic_set: 1 negative pic, used
        w.bit(false).bit(false).bit(true); // long term, tmvp, strong smoothing
        w.bit(false).bit(false); // vui, extension
        w.finish()
    }

    #[test]
    fn parses_small_sps() {
        let sps = Sps::parse(&small_sps()).unwrap();
        assert_eq!(sps.chroma_format_idc, 1);
        assert_eq!(
            (
                sps.pic_width_in_luma_samples,
                sps.pic_height_in_luma_samples
            ),
            (64, 48)
        );
        assert_eq!(sps.output_size(), (62, 46));
        assert_eq!(sps.ctb_size(), 32);
        assert_eq!(sps.pic_size_in_ctbs(), (2, 2));
        assert_eq!(
            (
                sps.log2_min_luma_transform_block_size,
                sps.log2_max_luma_transform_block_size
            ),
            (2, 5)
        );
        assert!(sps.sample_adaptive_offset_enabled_flag);
        assert!(sps.strong_intra_smoothing_enabled_flag);
        assert_eq!(sps.st_rps_num_delta_pocs, vec![1]);
    }

    #[test]
    fn parses_pps_with_tiles() {
        let mut sps_list: [Option<Sps>; 16] = Default::default();
        sps_list[0] = Some(Sps::parse(&small_sps()).unwrap());
        let mut w = BitWriter::default();
        w.ue(3)
            .ue(0)
            .bit(false)
            .bit(false)
            .bits(0, 3)
            .bit(true)
            .bit(false);
        w.ue(0).ue(0).se(-4).bit(false).bit(true);
        w.bit(true).ue(1); // cu_qp_delta, depth 1
        w.se(2).se(-3).bit(false).bit(false).bit(false).bit(false);
        w.bit(true).bit(false); // tiles, no wpp
        w.ue(1).ue(0).bit(false).ue(0).bit(true); // 2 columns (1 + 1 CTB), 1 row
        w.bit(true).bit(true).bit(false).bit(false).se(-2).se(3); // deblocking
        w.bit(false).bit(false).ue(0).bit(false).bit(false);
        let pps = Pps::parse(&w.finish(), &sps_list).unwrap();
        assert_eq!(pps.pps_pic_parameter_set_id, 3);
        assert_eq!(pps.init_qp_minus26, -4);
        assert_eq!(pps.diff_cu_qp_delta_depth, Some(1));
        assert_eq!((pps.pps_cb_qp_offset, pps.pps_cr_qp_offset), (2, -3));
        let tiles = pps.tiles.unwrap();
        assert_eq!(tiles.column_widths, Some(vec![1, 1]));
        assert_eq!(tiles.row_heights, Some(vec![2]));
        assert_eq!((pps.pps_beta_offset_div2, pps.pps_tc_offset_div2), (-2, 3));
    }

    #[test]
    fn pps_requires_its_sps() {
        let mut w = BitWriter::default();
        w.ue(0).ue(5);
        let sps_list: [Option<Sps>; 16] = Default::default();
        assert!(Pps::parse(&w.finish(), &sps_list).is_err());
    }

    #[test]
    fn scaling_list_prediction_and_explicit_values() {
        let mut w = BitWriter::default();
        // sizeId 0: matrix 0 explicit (16 deltas: first +2 -> 10, then 0), others copy previous.
        w.bit(true).se(2);
        for _ in 1..16 {
            w.se(0);
        }
        for _ in 1..6 {
            w.bit(false).ue(1);
        }
        // sizeId 1, 2: all default (delta 0).
        for _ in 0..12 {
            w.bit(false).ue(0);
        }
        // sizeId 3: matrix 0 default, matrix 3 copies matrix 0.
        w.bit(false).ue(0).bit(false).ue(1);
        let s = ScalingList::parse(&mut BitReader::new(&w.finish())).unwrap();
        assert_eq!(s.lists[0][0], vec![10; 16]);
        assert_eq!(s.lists[0][5], vec![10; 16]);
        assert_eq!(s.lists[1][3], DEFAULT_INTER_8X8.to_vec());
        assert_eq!(s.lists[3][3], DEFAULT_INTRA_8X8.to_vec());
    }

    #[test]
    fn rejects_bad_ctb_size() {
        let mut w = BitWriter::default();
        w.bits(0, 4).bits(0, 3).bit(true);
        ptl(&mut w);
        w.ue(0).ue(1).ue(64).ue(48).bit(false);
        w.ue(0).ue(0).ue(4).bit(true).ue(0).ue(0).ue(0);
        w.ue(0).ue(0).ue(0).ue(1); // CTB 8x8: forbidden
        assert!(Sps::parse(&w.finish()).is_err());
    }
}
