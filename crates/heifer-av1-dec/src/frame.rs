//! Frame header (AV1 §5.9 and §6.8).
//!
//! A still image is one key frame (or, rarely, an intra-only frame). Layered images add inter
//! frames, whose headers depend on the headers saved in the eight reference slots.

use crate::Error;
use crate::bits::BitReader;
use crate::obu::{Obu, ObuType};
use crate::sequence::SequenceHeader;

/// `frame_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FrameType {
    /// `KEY_FRAME`: resets the decoder state.
    Key,
    /// `INTER_FRAME`.
    Inter,
    /// `INTRA_ONLY_FRAME`: intra, but keeps some reference frames.
    IntraOnly,
    /// `SWITCH_FRAME`.
    Switch,
}

/// A parsed frame header (`uncompressed_header()`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FrameHeader {
    /// `frame_type`.
    pub frame_type: FrameType,
    /// `frame_to_show_map_idx` when the header only shows a saved frame (`show_existing_frame`):
    /// the other fields are then those of that frame.
    pub show_existing_frame: Option<u8>,
    /// `show_frame`.
    pub show_frame: bool,
    /// `showable_frame`.
    pub showable_frame: bool,
    /// `error_resilient_mode`.
    pub error_resilient_mode: bool,
    /// `disable_cdf_update`: symbol probabilities stay at their initial values.
    pub disable_cdf_update: bool,
    /// `allow_screen_content_tools`.
    pub allow_screen_content_tools: bool,
    /// `current_frame_id`, 0 when frame ids are absent.
    pub current_frame_id: u32,
    /// `order_hint`.
    pub order_hint: u32,
    /// `refresh_frame_flags`.
    pub refresh_frame_flags: u8,
    /// `FrameWidth`: coded width, after the superres downscale.
    pub frame_width: u32,
    /// `FrameHeight`.
    pub frame_height: u32,
    /// `UpscaledWidth`: width after superres upscaling.
    pub upscaled_width: u32,
    /// `SuperresDenom`: 8 without superres, else 9 to 16.
    pub superres_denom: u8,
    /// `RenderWidth`: intended display width.
    pub render_width: u32,
    /// `RenderHeight`.
    pub render_height: u32,
    /// `MiCols`: width in 4×4 blocks, rounded up to 8 pixels.
    pub mi_cols: u32,
    /// `MiRows`.
    pub mi_rows: u32,
    /// `allow_intrabc`: blocks may copy from already decoded parts of the frame.
    pub allow_intrabc: bool,
    /// `disable_frame_end_update_cdf`.
    pub disable_frame_end_update_cdf: bool,
    /// Tile layout.
    pub tile_info: TileInfo,
    /// Quantizer parameters.
    pub quantization: Quantization,
    /// Segmentation parameters.
    pub segmentation: Segmentation,
    /// `delta_q_present`.
    pub delta_q_present: bool,
    /// `delta_q_res` (log2 of the step).
    pub delta_q_res: u8,
    /// `delta_lf_present`.
    pub delta_lf_present: bool,
    /// `delta_lf_res`.
    pub delta_lf_res: u8,
    /// `delta_lf_multi`.
    pub delta_lf_multi: bool,
    /// `LosslessArray`: whether each segment is lossless.
    pub lossless: [bool; MAX_SEGMENTS],
    /// `CodedLossless`: every segment is lossless.
    pub coded_lossless: bool,
    /// `AllLossless`: coded lossless and no superres.
    pub all_lossless: bool,
    /// Deblocking filter parameters.
    pub loop_filter: LoopFilter,
    /// CDEF parameters.
    pub cdef: Cdef,
    /// Loop restoration parameters.
    pub restoration: Restoration,
    /// `TxMode`.
    pub tx_mode: TxMode,
    /// `reduced_tx_set`.
    pub reduced_tx_set: bool,
    /// Film grain parameters, when grain is applied to this frame.
    pub film_grain: Option<FilmGrain>,
    /// `force_integer_mv`.
    pub force_integer_mv: bool,
    /// `primary_ref_frame`, `None` for `PRIMARY_REF_NONE`.
    pub primary_ref_frame: Option<u8>,
    /// `ref_frame_idx`: the slots of `LAST_FRAME` to `ALTREF_FRAME`.
    pub ref_frame_idx: [u8; 7],
    /// `OrderHints`, indexed by reference frame (`LAST_FRAME` = 1 to `ALTREF_FRAME` = 7).
    pub order_hints: [u32; 8],
    /// `RefFrameSignBias`, indexed like `order_hints`.
    pub ref_frame_sign_bias: [bool; 8],
    /// `allow_high_precision_mv`.
    pub allow_high_precision_mv: bool,
    /// `interpolation_filter`: `EIGHTTAP` 0, `SMOOTH` 1, `SHARP` 2, `BILINEAR` 3, `SWITCHABLE` 4.
    pub interpolation_filter: u8,
    /// `is_motion_mode_switchable`.
    pub is_motion_mode_switchable: bool,
    /// `use_ref_frame_mvs`.
    pub use_ref_frame_mvs: bool,
    /// `reference_select`.
    pub reference_select: bool,
    /// `SkipModeFrame`, when `skip_mode_present`.
    pub skip_mode_frames: Option<[u8; 2]>,
    /// `allow_warped_motion`.
    pub allow_warped_motion: bool,
    /// `GmType` per reference frame: `IDENTITY` 0, `TRANSLATION` 1, `ROTZOOM` 2, `AFFINE` 3.
    pub gm_types: [u8; 8],
    /// `gm_params` per reference frame.
    pub gm_params: [[i32; 6]; 8],
}

/// `MAX_SEGMENTS`.
pub const MAX_SEGMENTS: usize = 8;
/// `SEG_LVL_MAX`: number of segmentation features.
pub const SEG_LVL_MAX: usize = 8;

/// `tile_info()` (§5.9.15).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TileInfo {
    /// `TileCols`.
    pub cols: u32,
    /// `TileRows`.
    pub rows: u32,
    /// `TileColsLog2`.
    pub cols_log2: u32,
    /// `TileRowsLog2`.
    pub rows_log2: u32,
    /// `MiColStarts`: first 4×4 column of each tile, then `MiCols`.
    pub mi_col_starts: Vec<u32>,
    /// `MiRowStarts`: first 4×4 row of each tile, then `MiRows`.
    pub mi_row_starts: Vec<u32>,
    /// `context_update_tile_id`: the tile whose final probabilities are kept.
    pub context_update_tile_id: u32,
    /// `TileSizeBytes`: size of the tile size fields in tile groups.
    pub tile_size_bytes: u8,
}

/// `quantization_params()` (§5.9.12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Quantization {
    /// `base_q_idx`.
    pub base_q_idx: u8,
    /// `DeltaQYDc`.
    pub delta_q_y_dc: i8,
    /// `DeltaQUDc`.
    pub delta_q_u_dc: i8,
    /// `DeltaQUAc`.
    pub delta_q_u_ac: i8,
    /// `DeltaQVDc`.
    pub delta_q_v_dc: i8,
    /// `DeltaQVAc`.
    pub delta_q_v_ac: i8,
    /// `qm_y`, `qm_u`, `qm_v` when `using_qmatrix`.
    pub qmatrix: Option<[u8; 3]>,
}

/// `segmentation_params()` (§5.9.14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Segmentation {
    /// `segmentation_enabled`.
    pub enabled: bool,
    /// `FeatureEnabled[segment][feature]`.
    pub feature_enabled: [[bool; SEG_LVL_MAX]; MAX_SEGMENTS],
    /// `FeatureData[segment][feature]`, clipped.
    pub feature_data: [[i16; SEG_LVL_MAX]; MAX_SEGMENTS],
    /// `SegIdPreSkip`.
    pub seg_id_pre_skip: bool,
    /// `LastActiveSegId`.
    pub last_active_seg_id: u8,
    /// `segmentation_update_map`.
    pub update_map: bool,
    /// `segmentation_temporal_update`.
    pub temporal_update: bool,
}

/// `loop_filter_params()` (§5.9.11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct LoopFilter {
    /// `loop_filter_level`: luma vertical, luma horizontal, U, V.
    pub level: [u8; 4],
    /// `loop_filter_sharpness`.
    pub sharpness: u8,
    /// `loop_filter_delta_enabled`.
    pub delta_enabled: bool,
    /// `loop_filter_ref_deltas`.
    pub ref_deltas: [i8; 8],
    /// `loop_filter_mode_deltas`.
    pub mode_deltas: [i8; 2],
}

/// `cdef_params()` (§5.9.19).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Cdef {
    /// `CdefDamping`.
    pub damping: u8,
    /// `cdef_bits`: `1 << bits` strength presets.
    pub bits: u8,
    /// `cdef_y_pri_strength`.
    pub y_pri_strength: [u8; 8],
    /// `cdef_y_sec_strength` (3 is read as 4).
    pub y_sec_strength: [u8; 8],
    /// `cdef_uv_pri_strength`.
    pub uv_pri_strength: [u8; 8],
    /// `cdef_uv_sec_strength` (3 is read as 4).
    pub uv_sec_strength: [u8; 8],
}

/// Loop restoration filter of a plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RestorationType {
    /// `RESTORE_NONE`.
    None,
    /// `RESTORE_WIENER`.
    Wiener,
    /// `RESTORE_SGRPROJ`: self-guided.
    Sgrproj,
    /// `RESTORE_SWITCHABLE`: chosen per unit.
    Switchable,
}

/// `lr_params()` (§5.9.20).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Restoration {
    /// `FrameRestorationType` of each plane.
    pub types: [RestorationType; 3],
    /// `LoopRestorationSize` of each plane, in pixels (0 when unused).
    pub unit_size: [u16; 3],
}

/// `TxMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TxMode {
    /// `ONLY_4X4`: lossless frames.
    Only4x4,
    /// `TX_MODE_LARGEST`.
    Largest,
    /// `TX_MODE_SELECT`: chosen per block.
    Select,
}

/// `film_grain_params()` (§5.9.30), when `apply_grain` is set.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FilmGrain {
    /// `grain_seed`.
    pub grain_seed: u16,
    /// `(point_y_value, point_y_scaling)`.
    pub y_points: Vec<(u8, u8)>,
    /// `chroma_scaling_from_luma`.
    pub chroma_scaling_from_luma: bool,
    /// `(point_cb_value, point_cb_scaling)`.
    pub cb_points: Vec<(u8, u8)>,
    /// `(point_cr_value, point_cr_scaling)`.
    pub cr_points: Vec<(u8, u8)>,
    /// `grain_scaling_minus_8 + 8`.
    pub grain_scaling: u8,
    /// `ar_coeff_lag`.
    pub ar_coeff_lag: u8,
    /// `ar_coeffs_y_plus_128 - 128`.
    pub ar_coeffs_y: Vec<i8>,
    /// `ar_coeffs_cb_plus_128 - 128`.
    pub ar_coeffs_cb: Vec<i8>,
    /// `ar_coeffs_cr_plus_128 - 128`.
    pub ar_coeffs_cr: Vec<i8>,
    /// `ar_coeff_shift_minus_6 + 6`.
    pub ar_coeff_shift: u8,
    /// `grain_scale_shift`.
    pub grain_scale_shift: u8,
    /// `(cb_mult, cb_luma_mult, cb_offset)`.
    pub cb_mult: (u8, u8, u16),
    /// `(cr_mult, cr_luma_mult, cr_offset)`.
    pub cr_mult: (u8, u8, u16),
    /// `overlap_flag`.
    pub overlap: bool,
    /// `clip_to_restricted_range`.
    pub clip_to_restricted_range: bool,
}

const ALL_FRAMES: u8 = 0xFF;
const MAX_TILE_WIDTH: u32 = 4096;
const MAX_TILE_AREA: u32 = 4096 * 2304;
const MAX_TILE_COLS: u32 = 64;
const MAX_TILE_ROWS: u32 = 64;
const SEGMENTATION_FEATURE_BITS: [u32; SEG_LVL_MAX] = [8, 6, 6, 6, 6, 3, 0, 0];
const SEGMENTATION_FEATURE_SIGNED: [bool; SEG_LVL_MAX] =
    [true, true, true, true, true, false, false, false];
const SEGMENTATION_FEATURE_MAX: [i16; SEG_LVL_MAX] = [255, 63, 63, 63, 63, 7, 0, 0];
/// `SEG_LVL_REF_FRAME`.
const SEG_LVL_REF_FRAME: usize = 5;
/// Defaults of `loop_filter_ref_deltas` (`setup_past_independence()`).
const DEFAULT_REF_DELTAS: [i8; 8] = [1, 0, 0, 0, -1, 0, -1, -1];
const PRIMARY_REF_NONE: u32 = 7;
const SWITCHABLE: u8 = 4;
const WARPEDMODEL_PREC_BITS: u32 = 16;
/// `gm_params` of the identity model (`IDENTITY`).
pub(crate) const DEFAULT_GM_PARAMS: [i32; 6] = [
    0,
    0,
    1 << WARPEDMODEL_PREC_BITS,
    0,
    0,
    1 << WARPEDMODEL_PREC_BITS,
];
const TRANSLATION: u8 = 1;
const ROTZOOM: u8 = 2;
const AFFINE: u8 = 3;

/// The headers saved in the eight reference slots (`RefValid` is `Some`).
pub(crate) type RefHeaders = [Option<FrameHeader>; 8];

/// `get_relative_dist()`.
pub(crate) fn relative_dist(seq: &SequenceHeader, a: u32, b: u32) -> i32 {
    if !seq.enable_order_hint {
        return 0;
    }
    let diff = a as i32 - b as i32;
    let m = 1 << (seq.order_hint_bits - 1);
    (diff & (m - 1)) - (diff & m)
}

impl FrameHeader {
    /// Parses the frame header at the start of a frame header or frame OBU, with empty reference
    /// slots: right for key frames, while inter frames fail.
    ///
    /// Returns the header and the number of bytes it takes, up to the byte alignment: in a frame
    /// OBU, the tile group starts there.
    pub fn parse(obu: &Obu<'_>, seq: &SequenceHeader) -> Result<(Self, usize), Error> {
        Self::parse_with_refs(obu, seq, &RefHeaders::default())
    }

    /// `FrameIsIntra`.
    pub fn is_intra(&self) -> bool {
        matches!(self.frame_type, FrameType::Key | FrameType::IntraOnly)
    }

    /// Parses a frame header, given the headers saved in the reference slots.
    pub(crate) fn parse_with_refs(
        obu: &Obu<'_>,
        seq: &SequenceHeader,
        refs: &RefHeaders,
    ) -> Result<(Self, usize), Error> {
        let mut r = BitReader::new(obu.payload);
        let end = |r: &mut BitReader<'_>| -> Result<usize, Error> {
            // A frame header OBU ends with trailing bits; in a frame OBU the tile group starts at
            // the next byte.
            if obu.obu_type == ObuType::FrameHeader {
                r.trailing_bits()?;
                Ok(obu.payload.len())
            } else {
                r.byte_align()
            }
        };
        let id_len = u32::from(seq.delta_frame_id_length + seq.additional_frame_id_length);
        let (frame_type, show_frame, showable_frame, error_resilient_mode);
        if seq.reduced_still_picture_header {
            (frame_type, show_frame, showable_frame, error_resilient_mode) =
                (FrameType::Key, true, false, true);
        } else {
            if r.flag()? {
                // show_existing_frame
                let idx = r.f(3)? as u8;
                r.f(seq.timing.presentation_time_bits)?; // temporal_point_info()
                if seq.frame_id_numbers_present {
                    r.f(id_len)?; // display_frame_id
                }
                let mut h = refs[usize::from(idx)]
                    .clone()
                    .ok_or(Error::Invalid("shows an empty reference slot"))?;
                h.show_existing_frame = Some(idx);
                h.show_frame = true;
                h.refresh_frame_flags = if h.frame_type == FrameType::Key {
                    ALL_FRAMES
                } else {
                    0
                };
                return Ok((h, end(&mut r)?));
            }
            frame_type = match r.f(2)? {
                0 => FrameType::Key,
                1 => FrameType::Inter,
                2 => FrameType::IntraOnly,
                _ => FrameType::Switch,
            };
            show_frame = r.flag()?;
            if show_frame {
                r.f(seq.timing.presentation_time_bits)?; // temporal_point_info()
            }
            showable_frame = if show_frame {
                frame_type != FrameType::Key
            } else {
                r.flag()?
            };
            error_resilient_mode = frame_type == FrameType::Switch
                || (frame_type == FrameType::Key && show_frame)
                || r.flag()?;
        }
        let frame_is_intra = matches!(frame_type, FrameType::Key | FrameType::IntraOnly);
        // A shown key frame empties every slot (RefValid and RefOrderHint are reset).
        let no_refs = RefHeaders::default();
        let refs = if frame_type == FrameType::Key && show_frame {
            &no_refs
        } else {
            refs
        };
        let slot = |idx: u8| -> Result<&FrameHeader, Error> {
            refs[usize::from(idx)]
                .as_ref()
                .ok_or(Error::Invalid("reference to an empty slot"))
        };
        let disable_cdf_update = r.flag()?;
        let allow_screen_content_tools = match seq.seq_force_screen_content_tools {
            2 => r.flag()?,
            v => v == 1,
        };
        let force_integer_mv = (allow_screen_content_tools
            && match seq.seq_force_integer_mv {
                2 => r.flag()?,
                v => v == 1,
            })
            || frame_is_intra;
        // shortcut: mark_ref_frames() is skipped, so frame ids never invalidate a slot; only error
        // concealment needs it.
        let current_frame_id = if seq.frame_id_numbers_present {
            r.f(id_len)?
        } else {
            0
        };
        let frame_size_override = if frame_type == FrameType::Switch {
            true
        } else {
            !seq.reduced_still_picture_header && r.flag()?
        };
        let order_hint = r.f(u32::from(seq.order_hint_bits))?;
        let primary_ref_frame = if frame_is_intra || error_resilient_mode {
            None
        } else {
            match r.f(3)? {
                PRIMARY_REF_NONE => None,
                v => Some(v as u8),
            }
        };
        if seq.timing.buffer_removal_time_bits > 0 && r.flag()? {
            for (op, &idc) in seq.operating_point_idc.iter().enumerate() {
                if !seq
                    .timing
                    .decoder_model_present
                    .get(op)
                    .copied()
                    .unwrap_or(false)
                {
                    continue;
                }
                let in_temporal = idc >> obu.temporal_id & 1 == 1;
                let in_spatial = idc >> (obu.spatial_id + 8) & 1 == 1;
                if idc == 0 || (in_temporal && in_spatial) {
                    r.f(seq.timing.buffer_removal_time_bits)?; // buffer_removal_time
                }
            }
        }
        let refresh_frame_flags =
            if frame_type == FrameType::Switch || (frame_type == FrameType::Key && show_frame) {
                ALL_FRAMES
            } else {
                r.f(8)? as u8
            };
        if frame_type == FrameType::IntraOnly && refresh_frame_flags == ALL_FRAMES {
            return Err(Error::Invalid("intra-only frame refreshes every reference"));
        }
        if (!frame_is_intra || refresh_frame_flags != ALL_FRAMES)
            && error_resilient_mode
            && seq.enable_order_hint
        {
            // shortcut: ref_order_hint only invalidates slots, for error concealment; skipped.
            for _ in 0..8 {
                r.f(u32::from(seq.order_hint_bits))?;
            }
        }

        let mut ref_frame_idx = [0; 7];
        let mut found_ref = None;
        if !frame_is_intra {
            let short_signaling = seq.enable_order_hint && r.flag()?;
            if short_signaling {
                let (last, gold) = (r.f(3)? as u8, r.f(3)? as u8);
                ref_frame_idx = set_frame_refs(seq, refs, order_hint, last, gold);
            }
            for idx in &mut ref_frame_idx {
                if !short_signaling {
                    *idx = r.f(3)? as u8;
                }
                if seq.frame_id_numbers_present {
                    r.f(u32::from(seq.delta_frame_id_length))?; // delta_frame_id_minus_1
                }
            }
            for &idx in &ref_frame_idx {
                slot(idx)?;
            }
            if frame_size_override && !error_resilient_mode {
                // frame_size_with_refs()
                for &idx in &ref_frame_idx {
                    if r.flag()? {
                        found_ref = Some(slot(idx)?);
                        break;
                    }
                }
            }
        }
        // frame_size() or the size of the found reference, superres_params(), render_size().
        let (upscaled_width, frame_height) = match found_ref {
            Some(h) => (h.upscaled_width, h.frame_height),
            None if frame_size_override => (
                r.f(u32::from(seq.frame_width_bits))? + 1,
                r.f(u32::from(seq.frame_height_bits))? + 1,
            ),
            None => (seq.max_frame_width, seq.max_frame_height),
        };
        if upscaled_width > seq.max_frame_width || frame_height > seq.max_frame_height {
            return Err(Error::Invalid(
                "frame larger than the sequence header allows",
            ));
        }
        let superres_denom = if seq.enable_superres && r.flag()? {
            r.f(3)? as u8 + 9
        } else {
            8
        };
        let frame_width =
            (upscaled_width * 8 + u32::from(superres_denom) / 2) / u32::from(superres_denom);
        let mi_cols = 2 * frame_width.div_ceil(8);
        let mi_rows = 2 * frame_height.div_ceil(8);
        let (render_width, render_height) = match found_ref {
            Some(h) => (h.render_width, h.render_height),
            None if r.flag()? => (r.f(16)? + 1, r.f(16)? + 1),
            None => (upscaled_width, frame_height),
        };
        let allow_intrabc = frame_is_intra
            && allow_screen_content_tools
            && upscaled_width == frame_width
            && r.flag()?;
        let mut order_hints = [0; 8];
        let mut ref_frame_sign_bias = [false; 8];
        let (mut allow_high_precision_mv, mut interpolation_filter) = (false, 0);
        let (mut is_motion_mode_switchable, mut use_ref_frame_mvs) = (false, false);
        if !frame_is_intra {
            allow_high_precision_mv = !force_integer_mv && r.flag()?;
            interpolation_filter = if r.flag()? { SWITCHABLE } else { r.f(2)? as u8 };
            is_motion_mode_switchable = r.flag()?;
            use_ref_frame_mvs = !error_resilient_mode && seq.enable_ref_frame_mvs && r.flag()?;
            for (i, &idx) in ref_frame_idx.iter().enumerate() {
                let hint = slot(idx)?.order_hint;
                order_hints[i + 1] = hint;
                ref_frame_sign_bias[i + 1] = relative_dist(seq, hint, order_hint) > 0;
            }
        }

        let disable_frame_end_update_cdf =
            seq.reduced_still_picture_header || disable_cdf_update || r.flag()?;
        // load_previous()
        let prev = primary_ref_frame
            .map(|p| slot(ref_frame_idx[usize::from(p)]))
            .transpose()?;
        let tile_info = tile_info(&mut r, seq, mi_cols, mi_rows)?;
        let num_planes = if seq.color.monochrome { 1 } else { 3 };
        let quantization = quantization_params(&mut r, seq, num_planes)?;
        let segmentation = segmentation_params(&mut r, prev.map(|h| &h.segmentation))?;
        let delta_q_present = quantization.base_q_idx > 0 && r.flag()?;
        let delta_q_res = if delta_q_present { r.f(2)? as u8 } else { 0 };
        let delta_lf_present = delta_q_present && !allow_intrabc && r.flag()?;
        let (delta_lf_res, delta_lf_multi) = if delta_lf_present {
            (r.f(2)? as u8, r.flag()?)
        } else {
            (0, false)
        };

        let q = &quantization;
        let mut lossless = [false; MAX_SEGMENTS];
        for (segment, lossless) in lossless.iter_mut().enumerate() {
            let qindex = if segmentation.enabled && segmentation.feature_enabled[segment][0] {
                (i16::from(q.base_q_idx) + segmentation.feature_data[segment][0]).clamp(0, 255)
            } else {
                i16::from(q.base_q_idx)
            };
            *lossless = qindex == 0
                && q.delta_q_y_dc == 0
                && q.delta_q_u_ac == 0
                && q.delta_q_u_dc == 0
                && q.delta_q_v_ac == 0
                && q.delta_q_v_dc == 0;
        }
        let coded_lossless = lossless.iter().all(|&l| l);
        let all_lossless = coded_lossless && frame_width == upscaled_width;

        let loop_filter = if coded_lossless || allow_intrabc {
            LoopFilter {
                level: [0; 4],
                sharpness: 0,
                delta_enabled: false,
                ref_deltas: DEFAULT_REF_DELTAS,
                mode_deltas: [0; 2],
            }
        } else {
            loop_filter_params(&mut r, num_planes, prev.map(|h| &h.loop_filter))?
        };
        let cdef = if coded_lossless || allow_intrabc || !seq.enable_cdef {
            Cdef {
                damping: 3,
                bits: 0,
                y_pri_strength: [0; 8],
                y_sec_strength: [0; 8],
                uv_pri_strength: [0; 8],
                uv_sec_strength: [0; 8],
            }
        } else {
            cdef_params(&mut r, num_planes)?
        };
        let restoration = if all_lossless || allow_intrabc || !seq.enable_restoration {
            Restoration {
                types: [RestorationType::None; 3],
                unit_size: [0; 3],
            }
        } else {
            lr_params(&mut r, seq, num_planes)?
        };
        let tx_mode = if coded_lossless {
            TxMode::Only4x4
        } else if r.flag()? {
            TxMode::Select
        } else {
            TxMode::Largest
        };
        let reference_select = !frame_is_intra && r.flag()?;
        let skip_mode_allowed = if reference_select {
            skip_mode_frames(seq, &order_hints, order_hint)
        } else {
            None
        };
        let skip_mode_frames = match skip_mode_allowed {
            Some(frames) if r.flag()? => Some(frames), // skip_mode_present
            _ => None,
        };
        let allow_warped_motion =
            !frame_is_intra && !error_resilient_mode && seq.enable_warped_motion && r.flag()?;
        let reduced_tx_set = r.flag()?;
        let (gm_types, gm_params) = if frame_is_intra {
            ([0; 8], [DEFAULT_GM_PARAMS; 8])
        } else {
            let prev_gm = prev.map_or([DEFAULT_GM_PARAMS; 8], |h| h.gm_params);
            global_motion_params(&mut r, allow_high_precision_mv, &prev_gm)?
        };
        let film_grain = if seq.film_grain_params_present && (show_frame || showable_frame) {
            film_grain_params(&mut r, seq, frame_type, refs)?
        } else {
            None
        };
        let size = end(&mut r)?;
        Ok((
            Self {
                frame_type,
                show_existing_frame: None,
                show_frame,
                showable_frame,
                error_resilient_mode,
                disable_cdf_update,
                allow_screen_content_tools,
                current_frame_id,
                order_hint,
                refresh_frame_flags,
                frame_width,
                frame_height,
                upscaled_width,
                superres_denom,
                render_width,
                render_height,
                mi_cols,
                mi_rows,
                allow_intrabc,
                disable_frame_end_update_cdf,
                tile_info,
                quantization,
                segmentation,
                delta_q_present,
                delta_q_res,
                delta_lf_present,
                delta_lf_res,
                delta_lf_multi,
                lossless,
                coded_lossless,
                all_lossless,
                loop_filter,
                cdef,
                restoration,
                tx_mode,
                reduced_tx_set,
                film_grain,
                force_integer_mv,
                primary_ref_frame,
                ref_frame_idx,
                order_hints,
                ref_frame_sign_bias,
                allow_high_precision_mv,
                interpolation_filter,
                is_motion_mode_switchable,
                use_ref_frame_mvs,
                reference_select,
                skip_mode_frames,
                allow_warped_motion,
                gm_types,
                gm_params,
            },
            size,
        ))
    }
}

/// The set frame refs process (§7.8): `ref_frame_idx` from `last_frame_idx` and `gold_frame_idx`.
fn set_frame_refs(
    seq: &SequenceHeader,
    refs: &RefHeaders,
    order_hint: u32,
    last: u8,
    gold: u8,
) -> [u8; 7] {
    let mut idx = [-1i32; 7];
    idx[0] = i32::from(last);
    idx[3] = i32::from(gold); // GOLDEN_FRAME
    let mut used = [false; 8];
    used[usize::from(last)] = true;
    used[usize::from(gold)] = true;
    let cur = 1i32 << (seq.order_hint_bits - 1);
    let shifted: [i32; 8] = std::array::from_fn(|i| {
        cur + relative_dist(
            seq,
            refs[i].as_ref().map_or(0, |h| h.order_hint),
            order_hint,
        )
    });
    // The unused slot backward (hint >= cur) or forward of the current frame with the latest or
    // the earliest hint; ties go to the last slot when looking for the latest, the first otherwise.
    let find = |used: &[bool; 8], backward: bool, latest: bool| -> i32 {
        let mut best: i32 = -1;
        for i in 0..8 {
            let hint = shifted[i];
            if used[i] || (hint >= cur) != backward {
                continue;
            }
            if best < 0
                || if latest {
                    hint >= shifted[best as usize]
                } else {
                    hint < shifted[best as usize]
                }
            {
                best = i as i32;
            }
        }
        best
    };
    // ALTREF_FRAME, then BWDREF_FRAME and ALTREF2_FRAME.
    for (slot, latest) in [(6, true), (4, false), (5, false)] {
        let r = find(&used, true, latest);
        if r >= 0 {
            idx[slot] = r;
            used[r as usize] = true;
        }
    }
    // Ref_Frame_List: LAST2_FRAME, LAST3_FRAME, BWDREF_FRAME, ALTREF2_FRAME, ALTREF_FRAME.
    for slot in [1, 2, 4, 5, 6] {
        if idx[slot] < 0 {
            let r = find(&used, false, true);
            if r >= 0 {
                idx[slot] = r;
                used[r as usize] = true;
            }
        }
    }
    let mut earliest = 0;
    for i in 1..8 {
        if shifted[i] < shifted[earliest] {
            earliest = i;
        }
    }
    idx.map(|i| if i < 0 { earliest as u8 } else { i as u8 })
}

/// `skip_mode_params()` up to `skip_mode_present`: `SkipModeFrame` when skip mode is allowed.
fn skip_mode_frames(
    seq: &SequenceHeader,
    order_hints: &[u32; 8],
    order_hint: u32,
) -> Option<[u8; 2]> {
    if !seq.enable_order_hint {
        return None;
    }
    let dist = |a: u32, b: u32| relative_dist(seq, a, b);
    let hint = |i: usize| order_hints[i + 1];
    let (mut forward, mut backward): (Option<usize>, Option<usize>) = (None, None);
    for i in 0..7 {
        if dist(hint(i), order_hint) < 0 {
            if forward.is_none_or(|f| dist(hint(i), hint(f)) > 0) {
                forward = Some(i);
            }
        } else if dist(hint(i), order_hint) > 0
            && backward.is_none_or(|b| dist(hint(i), hint(b)) < 0)
        {
            backward = Some(i);
        }
    }
    let forward = forward?;
    let other = match backward {
        Some(b) => b,
        None => {
            let mut second: Option<usize> = None;
            for i in 0..7 {
                if dist(hint(i), hint(forward)) < 0
                    && second.is_none_or(|s| dist(hint(i), hint(s)) > 0)
                {
                    second = Some(i);
                }
            }
            second?
        }
    };
    Some([1 + forward.min(other) as u8, 1 + forward.max(other) as u8])
}

/// `global_motion_params()` of an inter frame: `GmType` and `gm_params` per reference frame.
fn global_motion_params(
    r: &mut BitReader<'_>,
    allow_high_precision_mv: bool,
    prev: &[[i32; 6]; 8],
) -> Result<([u8; 8], [[i32; 6]; 8]), Error> {
    let mut types = [0; 8];
    let mut params = [DEFAULT_GM_PARAMS; 8];
    for rf in 1..8 {
        let kind = if !r.flag()? {
            0
        } else if r.flag()? {
            ROTZOOM
        } else if r.flag()? {
            TRANSLATION
        } else {
            AFFINE
        };
        types[rf] = kind;
        let p = &mut params[rf];
        let hp = allow_high_precision_mv;
        if kind >= ROTZOOM {
            p[2] = read_global_param(r, kind, 2, hp, prev[rf][2])?;
            p[3] = read_global_param(r, kind, 3, hp, prev[rf][3])?;
            if kind == AFFINE {
                p[4] = read_global_param(r, kind, 4, hp, prev[rf][4])?;
                p[5] = read_global_param(r, kind, 5, hp, prev[rf][5])?;
            } else {
                p[4] = -p[3];
                p[5] = p[2];
            }
        }
        if kind >= TRANSLATION {
            p[0] = read_global_param(r, kind, 0, hp, prev[rf][0])?;
            p[1] = read_global_param(r, kind, 1, hp, prev[rf][1])?;
        }
    }
    Ok((types, params))
}

/// `read_global_param()`.
fn read_global_param(
    r: &mut BitReader<'_>,
    kind: u8,
    idx: usize,
    allow_high_precision_mv: bool,
    prev: i32,
) -> Result<i32, Error> {
    let lowp = u32::from(!allow_high_precision_mv);
    let (abs_bits, prec_bits) = match (idx < 2, kind == TRANSLATION) {
        (true, true) => (9 - lowp, 3 - lowp), // GM_ABS_TRANS_ONLY_BITS, GM_TRANS_ONLY_PREC_BITS
        (true, false) => (12, 6),             // GM_ABS_TRANS_BITS, GM_TRANS_PREC_BITS
        _ => (12, 15),                        // GM_ABS_ALPHA_BITS, GM_ALPHA_PREC_BITS
    };
    let prec_diff = WARPEDMODEL_PREC_BITS - prec_bits;
    let (round, sub) = if idx % 3 == 2 {
        (1 << WARPEDMODEL_PREC_BITS, 1 << prec_bits)
    } else {
        (0, 0)
    };
    let mx = 1i32 << abs_bits;
    let reference = (prev >> prec_diff) - sub;
    // decode_signed_subexp_with_ref(-mx, mx + 1, reference)
    let x = unsigned_subexp_with_ref(r, (2 * mx + 1) as u32, reference + mx)? - mx;
    Ok((x << prec_diff) + round)
}

/// `decode_unsigned_subexp_with_ref()`.
fn unsigned_subexp_with_ref(r: &mut BitReader<'_>, mx: u32, reference: i32) -> Result<i32, Error> {
    let v = subexp(r, mx)? as i32;
    let mx = mx as i32;
    Ok(if (reference << 1) <= mx {
        inverse_recenter(reference, v)
    } else {
        mx - 1 - inverse_recenter(mx - 1 - reference, v)
    })
}

/// `decode_subexp()`.
fn subexp(r: &mut BitReader<'_>, num_syms: u32) -> Result<u32, Error> {
    let (mut i, mut mk, k) = (0, 0, 3);
    loop {
        let b2 = if i > 0 { k + i - 1 } else { k };
        let a = 1 << b2;
        if num_syms <= mk + 3 * a {
            return Ok(r.ns(num_syms - mk)? + mk);
        }
        if !r.flag()? {
            return Ok(r.f(b2)? + mk);
        }
        i += 1;
        mk += a;
    }
}

/// `inverse_recenter()`.
pub(crate) fn inverse_recenter(r: i32, v: i32) -> i32 {
    if v > 2 * r {
        v
    } else if v & 1 == 1 {
        r - ((v + 1) >> 1)
    } else {
        r + (v >> 1)
    }
}

/// `tile_log2()`: the smallest `k` with `blk_size << k >= target`.
fn tile_log2(blk_size: u32, target: u32) -> u32 {
    let mut k = 0;
    while (blk_size << k) < target {
        k += 1;
    }
    k
}

/// `tile_info()` (§5.9.15).
fn tile_info(
    r: &mut BitReader<'_>,
    seq: &SequenceHeader,
    mi_cols: u32,
    mi_rows: u32,
) -> Result<TileInfo, Error> {
    let sb_shift = if seq.use_128x128_superblock { 5 } else { 4 };
    let sb_cols = (mi_cols + (1 << sb_shift) - 1) >> sb_shift;
    let sb_rows = (mi_rows + (1 << sb_shift) - 1) >> sb_shift;
    let sb_size = sb_shift + 2;
    let max_tile_width_sb = MAX_TILE_WIDTH >> sb_size;
    let mut max_tile_area_sb = MAX_TILE_AREA >> (2 * sb_size);
    let min_log2_tile_cols = tile_log2(max_tile_width_sb, sb_cols);
    let max_log2_tile_cols = tile_log2(1, sb_cols.min(MAX_TILE_COLS));
    let max_log2_tile_rows = tile_log2(1, sb_rows.min(MAX_TILE_ROWS));
    let min_log2_tiles = min_log2_tile_cols.max(tile_log2(max_tile_area_sb, sb_rows * sb_cols));

    let mut mi_col_starts = Vec::new();
    let mut mi_row_starts = Vec::new();
    let (cols_log2, rows_log2);
    if r.flag()? {
        // uniform_tile_spacing_flag
        let mut log2 = min_log2_tile_cols;
        while log2 < max_log2_tile_cols && r.flag()? {
            log2 += 1;
        }
        cols_log2 = log2;
        let tile_width_sb = (sb_cols + (1 << cols_log2) - 1) >> cols_log2;
        mi_col_starts.extend(
            (0..sb_cols)
                .step_by(tile_width_sb as usize)
                .map(|sb| sb << sb_shift),
        );
        let mut log2 = min_log2_tiles.saturating_sub(cols_log2);
        while log2 < max_log2_tile_rows && r.flag()? {
            log2 += 1;
        }
        rows_log2 = log2;
        let tile_height_sb = (sb_rows + (1 << rows_log2) - 1) >> rows_log2;
        mi_row_starts.extend(
            (0..sb_rows)
                .step_by(tile_height_sb as usize)
                .map(|sb| sb << sb_shift),
        );
    } else {
        let mut widest_tile_sb = 0;
        let mut start_sb = 0;
        while start_sb < sb_cols {
            mi_col_starts.push(start_sb << sb_shift);
            let size_sb = r.ns((sb_cols - start_sb).min(max_tile_width_sb))? + 1;
            widest_tile_sb = widest_tile_sb.max(size_sb);
            start_sb += size_sb;
        }
        cols_log2 = tile_log2(1, mi_col_starts.len() as u32);
        max_tile_area_sb = if min_log2_tiles > 0 {
            (sb_rows * sb_cols) >> (min_log2_tiles + 1)
        } else {
            sb_rows * sb_cols
        };
        let max_tile_height_sb = (max_tile_area_sb / widest_tile_sb).max(1);
        let mut start_sb = 0;
        while start_sb < sb_rows {
            mi_row_starts.push(start_sb << sb_shift);
            start_sb += r.ns((sb_rows - start_sb).min(max_tile_height_sb))? + 1;
        }
        rows_log2 = tile_log2(1, mi_row_starts.len() as u32);
    }
    let (cols, rows) = (mi_col_starts.len() as u32, mi_row_starts.len() as u32);
    if cols > MAX_TILE_COLS || rows > MAX_TILE_ROWS {
        return Err(Error::Invalid("more than 64 tile columns or rows"));
    }
    mi_col_starts.push(mi_cols);
    mi_row_starts.push(mi_rows);
    let (context_update_tile_id, tile_size_bytes) = if cols_log2 > 0 || rows_log2 > 0 {
        let id = r.f(cols_log2 + rows_log2)?;
        if id >= cols * rows {
            return Err(Error::Invalid("context_update_tile_id is not a tile"));
        }
        (id, r.f(2)? as u8 + 1)
    } else {
        (0, 4)
    };
    Ok(TileInfo {
        cols,
        rows,
        cols_log2,
        rows_log2,
        mi_col_starts,
        mi_row_starts,
        context_update_tile_id,
        tile_size_bytes,
    })
}

/// `read_delta_q()`.
fn read_delta_q(r: &mut BitReader<'_>) -> Result<i8, Error> {
    Ok(if r.flag()? { r.su(7)? as i8 } else { 0 })
}

/// `quantization_params()` (§5.9.12).
fn quantization_params(
    r: &mut BitReader<'_>,
    seq: &SequenceHeader,
    num_planes: usize,
) -> Result<Quantization, Error> {
    let base_q_idx = r.f(8)? as u8;
    let delta_q_y_dc = read_delta_q(r)?;
    let (mut u_dc, mut u_ac, mut v_dc, mut v_ac) = (0, 0, 0, 0);
    if num_planes > 1 {
        let diff_uv_delta = seq.color.separate_uv_delta_q && r.flag()?;
        u_dc = read_delta_q(r)?;
        u_ac = read_delta_q(r)?;
        (v_dc, v_ac) = if diff_uv_delta {
            (read_delta_q(r)?, read_delta_q(r)?)
        } else {
            (u_dc, u_ac)
        };
    }
    let qmatrix = if r.flag()? {
        let y = r.f(4)? as u8;
        let u = r.f(4)? as u8;
        let v = if seq.color.separate_uv_delta_q {
            r.f(4)? as u8
        } else {
            u
        };
        Some([y, u, v])
    } else {
        None
    };
    Ok(Quantization {
        base_q_idx,
        delta_q_y_dc,
        delta_q_u_dc: u_dc,
        delta_q_u_ac: u_ac,
        delta_q_v_dc: v_dc,
        delta_q_v_ac: v_ac,
        qmatrix,
    })
}

/// `segmentation_params()` (§5.9.14); `prev` holds the parameters of the primary reference frame.
fn segmentation_params(
    r: &mut BitReader<'_>,
    prev: Option<&Segmentation>,
) -> Result<Segmentation, Error> {
    let mut s = Segmentation {
        enabled: r.flag()?,
        feature_enabled: [[false; SEG_LVL_MAX]; MAX_SEGMENTS],
        feature_data: [[0; SEG_LVL_MAX]; MAX_SEGMENTS],
        seg_id_pre_skip: false,
        last_active_seg_id: 0,
        update_map: false,
        temporal_update: false,
    };
    if !s.enabled {
        return Ok(s);
    }
    let update_data = match prev {
        None => {
            s.update_map = true;
            true
        }
        Some(p) => {
            s.update_map = r.flag()?;
            s.temporal_update = s.update_map && r.flag()?;
            s.feature_enabled = p.feature_enabled;
            s.feature_data = p.feature_data;
            r.flag()?
        }
    };
    if update_data {
        for segment in 0..MAX_SEGMENTS {
            for feature in 0..SEG_LVL_MAX {
                let enabled = r.flag()?;
                s.feature_enabled[segment][feature] = enabled;
                let bits = SEGMENTATION_FEATURE_BITS[feature];
                let limit = SEGMENTATION_FEATURE_MAX[feature];
                s.feature_data[segment][feature] =
                    match (enabled, SEGMENTATION_FEATURE_SIGNED[feature]) {
                        (false, _) => 0,
                        (true, true) => (r.su(1 + bits)? as i16).clamp(-limit, limit),
                        (true, false) => (r.f(bits)? as i16).clamp(0, limit),
                    };
            }
        }
    }
    for segment in 0..MAX_SEGMENTS {
        for feature in 0..SEG_LVL_MAX {
            if s.feature_enabled[segment][feature] {
                s.last_active_seg_id = segment as u8;
                s.seg_id_pre_skip |= feature >= SEG_LVL_REF_FRAME;
            }
        }
    }
    Ok(s)
}

/// `loop_filter_params()` (§5.9.11), when the filter may be used; the deltas start from those
/// of the primary reference frame, `prev`.
fn loop_filter_params(
    r: &mut BitReader<'_>,
    num_planes: usize,
    prev: Option<&LoopFilter>,
) -> Result<LoopFilter, Error> {
    let mut lf = LoopFilter {
        level: [r.f(6)? as u8, r.f(6)? as u8, 0, 0],
        sharpness: 0,
        delta_enabled: false,
        ref_deltas: prev.map_or(DEFAULT_REF_DELTAS, |p| p.ref_deltas),
        mode_deltas: prev.map_or([0; 2], |p| p.mode_deltas),
    };
    if num_planes > 1 && (lf.level[0] != 0 || lf.level[1] != 0) {
        lf.level[2] = r.f(6)? as u8;
        lf.level[3] = r.f(6)? as u8;
    }
    lf.sharpness = r.f(3)? as u8;
    lf.delta_enabled = r.flag()?;
    if lf.delta_enabled && r.flag()? {
        // loop_filter_delta_update
        for delta in lf.ref_deltas.iter_mut().chain(lf.mode_deltas.iter_mut()) {
            if r.flag()? {
                *delta = r.su(7)? as i8;
            }
        }
    }
    Ok(lf)
}

/// `cdef_params()` (§5.9.19), when CDEF may be used.
fn cdef_params(r: &mut BitReader<'_>, num_planes: usize) -> Result<Cdef, Error> {
    let mut c = Cdef {
        damping: r.f(2)? as u8 + 3,
        bits: r.f(2)? as u8,
        y_pri_strength: [0; 8],
        y_sec_strength: [0; 8],
        uv_pri_strength: [0; 8],
        uv_sec_strength: [0; 8],
    };
    let sec = |r: &mut BitReader<'_>| -> Result<u8, Error> {
        Ok(match r.f(2)? as u8 {
            3 => 4,
            v => v,
        })
    };
    for i in 0..1 << c.bits {
        c.y_pri_strength[i] = r.f(4)? as u8;
        c.y_sec_strength[i] = sec(r)?;
        if num_planes > 1 {
            c.uv_pri_strength[i] = r.f(4)? as u8;
            c.uv_sec_strength[i] = sec(r)?;
        }
    }
    Ok(c)
}

/// `lr_params()` (§5.9.20), when loop restoration may be used.
fn lr_params(
    r: &mut BitReader<'_>,
    seq: &SequenceHeader,
    num_planes: usize,
) -> Result<Restoration, Error> {
    let mut lr = Restoration {
        types: [RestorationType::None; 3],
        unit_size: [0; 3],
    };
    for t in &mut lr.types[..num_planes] {
        // Remap_Lr_Type.
        *t = match r.f(2)? {
            0 => RestorationType::None,
            1 => RestorationType::Switchable,
            2 => RestorationType::Wiener,
            _ => RestorationType::Sgrproj,
        };
    }
    let uses_lr = lr.types.iter().any(|&t| t != RestorationType::None);
    let uses_chroma_lr = lr.types[1..].iter().any(|&t| t != RestorationType::None);
    if uses_lr {
        let mut shift = r.f(1)?;
        if seq.use_128x128_superblock {
            shift += 1;
        } else if shift == 1 {
            shift += r.f(1)?;
        }
        let luma = 256 >> (2 - shift);
        let uv_shift = if seq.color.subsampling_x && seq.color.subsampling_y && uses_chroma_lr {
            r.f(1)?
        } else {
            0
        };
        lr.unit_size = [luma, luma >> uv_shift, luma >> uv_shift];
    }
    Ok(lr)
}

/// `film_grain_params()` (§5.9.30): `None` when grain is not applied.
fn film_grain_params(
    r: &mut BitReader<'_>,
    seq: &SequenceHeader,
    frame_type: FrameType,
    refs: &RefHeaders,
) -> Result<Option<FilmGrain>, Error> {
    if !r.flag()? {
        return Ok(None); // apply_grain
    }
    let grain_seed = r.f(16)? as u16;
    if frame_type == FrameType::Inter && !r.flag()? {
        // update_grain = 0: load_grain_params(film_grain_params_ref_idx), with this seed.
        let idx = r.f(3)? as usize;
        let mut grain = refs[idx]
            .as_ref()
            .and_then(|h| h.film_grain.clone())
            .ok_or(Error::Invalid("film grain from a reference without grain"))?;
        grain.grain_seed = grain_seed;
        return Ok(Some(grain));
    }
    let points = |r: &mut BitReader<'_>, max: u32| -> Result<Vec<(u8, u8)>, Error> {
        let n = r.f(4)?;
        if n > max {
            return Err(Error::Invalid("too many film grain points"));
        }
        let points: Vec<(u8, u8)> = (0..n)
            .map(|_| Ok((r.f(8)? as u8, r.f(8)? as u8)))
            .collect::<Result<_, Error>>()?;
        if points.windows(2).any(|w| w[0].0 >= w[1].0) {
            return Err(Error::Invalid("film grain points are not increasing"));
        }
        Ok(points)
    };
    let y_points = points(r, 14)?;
    let mono = seq.color.monochrome;
    let chroma_scaling_from_luma = !mono && r.flag()?;
    let (cb_points, cr_points) = if mono
        || chroma_scaling_from_luma
        || (seq.color.subsampling_x && seq.color.subsampling_y && y_points.is_empty())
    {
        (Vec::new(), Vec::new())
    } else {
        (points(r, 10)?, points(r, 10)?)
    };
    let grain_scaling = r.f(2)? as u8 + 8;
    let ar_coeff_lag = r.f(2)? as u8;
    let num_pos_luma = 2 * usize::from(ar_coeff_lag) * (usize::from(ar_coeff_lag) + 1);
    let coeffs = |r: &mut BitReader<'_>, n: usize| -> Result<Vec<i8>, Error> {
        (0..n).map(|_| Ok((r.f(8)? as i32 - 128) as i8)).collect()
    };
    let (ar_coeffs_y, num_pos_chroma) = if y_points.is_empty() {
        (Vec::new(), num_pos_luma)
    } else {
        (coeffs(r, num_pos_luma)?, num_pos_luma + 1)
    };
    let ar_coeffs_cb = if chroma_scaling_from_luma || !cb_points.is_empty() {
        coeffs(r, num_pos_chroma)?
    } else {
        Vec::new()
    };
    let ar_coeffs_cr = if chroma_scaling_from_luma || !cr_points.is_empty() {
        coeffs(r, num_pos_chroma)?
    } else {
        Vec::new()
    };
    let ar_coeff_shift = r.f(2)? as u8 + 6;
    let grain_scale_shift = r.f(2)? as u8;
    let mult = |r: &mut BitReader<'_>, present: bool| -> Result<(u8, u8, u16), Error> {
        Ok(if present {
            (r.f(8)? as u8, r.f(8)? as u8, r.f(9)? as u16)
        } else {
            (0, 0, 0)
        })
    };
    let cb_mult = mult(r, !cb_points.is_empty())?;
    let cr_mult = mult(r, !cr_points.is_empty())?;
    Ok(Some(FilmGrain {
        grain_seed,
        y_points,
        chroma_scaling_from_luma,
        cb_points,
        cr_points,
        grain_scaling,
        ar_coeff_lag,
        ar_coeffs_y,
        ar_coeffs_cb,
        ar_coeffs_cr,
        ar_coeff_shift,
        grain_scale_shift,
        cb_mult,
        cr_mult,
        overlap: r.flag()?,
        clip_to_restricted_range: r.flag()?,
    }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::bits::pack;

    /// A reduced still picture header: 100×50, 8-bit 4:2:0, CDEF and loop restoration on.
    pub(crate) fn still_sequence_fields() -> Vec<(u32, u32)> {
        vec![
            (0, 3),  // seq_profile
            (1, 1),  // still_picture
            (1, 1),  // reduced_still_picture_header
            (8, 5),  // seq_level_idx
            (15, 4), // frame_width_bits_minus_1
            (15, 4), // frame_height_bits_minus_1
            (99, 16),
            (49, 16),
            (0, 1), // use_128x128_superblock
            (1, 1), // enable_filter_intra
            (1, 1), // enable_intra_edge_filter
            (0, 1), // enable_superres
            (1, 1), // enable_cdef
            (1, 1), // enable_restoration
            (0, 1), // high_bitdepth
            (0, 1), // mono_chrome
            (0, 1), // color_description_present_flag
            (0, 1), // color_range
            (0, 2), // chroma_sample_position
            (0, 1), // separate_uv_delta_q
            (0, 1), // film_grain_params_present
        ]
    }

    fn still_sequence() -> SequenceHeader {
        SequenceHeader::parse(&pack(&still_sequence_fields())).unwrap()
    }

    /// A full sequence header: 100×50, screen content tools chosen per frame, no tools.
    fn full_sequence() -> SequenceHeader {
        SequenceHeader::parse(&pack(&[
            (0, 3),
            (0, 1),
            (0, 1),
            (0, 1),  // timing_info_present_flag
            (0, 1),  // initial_display_delay_present_flag
            (0, 5),  // operating_points_cnt_minus_1
            (0, 12), // operating_point_idc
            (0, 5),  // seq_level_idx
            (15, 4),
            (15, 4),
            (99, 16),
            (49, 16),
            (0, 1), // frame_id_numbers_present_flag
            (0, 7), // use_128x128_superblock … enable_dual_filter
            (0, 1), // enable_order_hint
            (1, 1), // seq_choose_screen_content_tools
            (1, 1), // seq_choose_integer_mv
            (0, 3), // enable_superres, enable_cdef, enable_restoration
            (0, 4), // high_bitdepth, mono_chrome, color_description_present_flag, color_range
            (0, 2), // chroma_sample_position
            (0, 2), // separate_uv_delta_q, film_grain_params_present
        ]))
        .unwrap()
    }

    fn header_obu(payload: &[u8]) -> Obu<'_> {
        Obu {
            obu_type: ObuType::FrameHeader,
            temporal_id: 0,
            spatial_id: 0,
            has_extension: false,
            payload,
        }
    }

    /// Header fields up to and including `tile_info()` for [`still_sequence`]: screen content
    /// tools on, two tile columns.
    pub(crate) const STILL_START: [(u32, u32); 9] = [
        (0, 1), // disable_cdf_update
        (1, 1), // allow_screen_content_tools
        (0, 1), // force_integer_mv
        (0, 1), // render_and_frame_size_different
        (0, 1), // allow_intrabc
        (1, 1), // uniform_tile_spacing_flag
        (1, 1), // increment_tile_cols_log2
        (1, 1), // context_update_tile_id
        (1, 2), // tile_size_bytes_minus_1
    ];

    /// A whole lossless frame header for [`still_sequence`], 24 bits, without trailing bits.
    pub(crate) fn lossless_fields() -> Vec<(u32, u32)> {
        let mut fields = STILL_START.to_vec();
        fields.extend([
            (0, 8), // base_q_idx
            (0, 3), // DeltaQYDc, DeltaQUDc, DeltaQUAc not coded
            (0, 1), // using_qmatrix
            (0, 1), // segmentation_enabled
            (1, 1), // reduced_tx_set
        ]);
        fields
    }

    #[test]
    fn lossless_still_frame() {
        let seq = still_sequence();
        let mut fields = lossless_fields();
        fields.push((1, 1)); // trailing one bit
        let data = pack(&fields);
        let (h, size) = FrameHeader::parse(&header_obu(&data), &seq).unwrap();
        assert_eq!(size, data.len());
        assert_eq!(h.frame_type, FrameType::Key);
        assert!(h.show_frame && h.allow_screen_content_tools && !h.allow_intrabc);
        assert!(h.disable_frame_end_update_cdf);
        assert_eq!((h.frame_width, h.frame_height), (100, 50));
        assert_eq!((h.render_width, h.render_height), (100, 50));
        assert_eq!((h.mi_cols, h.mi_rows), (26, 14));
        let t = &h.tile_info;
        assert_eq!((t.cols, t.rows, t.cols_log2, t.rows_log2), (2, 1, 1, 0));
        assert_eq!(
            (t.mi_col_starts.as_slice(), t.mi_row_starts.as_slice()),
            (&[0, 16, 26][..], &[0, 14][..])
        );
        assert_eq!((t.context_update_tile_id, t.tile_size_bytes), (1, 2));
        assert!(h.coded_lossless && h.all_lossless && h.lossless.iter().all(|&l| l));
        assert_eq!(h.tx_mode, TxMode::Only4x4);
        assert_eq!(h.loop_filter.level, [0; 4]);
        assert_eq!(h.loop_filter.ref_deltas, DEFAULT_REF_DELTAS);
        assert_eq!(h.cdef.damping, 3);
        assert_eq!(h.restoration.types, [RestorationType::None; 3]);
        assert!(h.reduced_tx_set && h.film_grain.is_none());

        // The header takes exactly 3 bytes: in a frame OBU the tile group starts after them.
        let mut frame = header_obu(&data);
        frame.obu_type = ObuType::Frame;
        assert_eq!(FrameHeader::parse(&frame, &seq).unwrap().1, 3);
        // Wrong trailing bits, and a header cut short.
        let mut bad = fields.clone();
        *bad.last_mut().unwrap() = (0, 1);
        assert!(FrameHeader::parse(&header_obu(&pack(&bad)), &seq).is_err());
        assert_eq!(
            FrameHeader::parse(&header_obu(&data[..2]), &seq),
            Err(Error::UnexpectedEof)
        );
    }

    #[test]
    fn quantizer_segmentation_and_filters() {
        let seq = still_sequence();
        let mut fields = STILL_START.to_vec();
        fields.extend([
            (100, 8), // base_q_idx
            (1, 1),
            (0b111_1101, 7), // DeltaQYDc = -3
            (0, 2),          // DeltaQUDc, DeltaQUAc not coded
            (1, 1),          // using_qmatrix
            (5, 4),          // qm_y
            (7, 4),          // qm_u (qm_v copies it)
            (1, 1),          // segmentation_enabled
        ]);
        for segment in 0..8 {
            for feature in 0..8 {
                match (segment, feature) {
                    (2, 0) => fields.extend([(1, 1), (0x100, 9)]), // -256, clipped to -255
                    (3, 5) => fields.extend([(1, 1), (7, 3)]),     // reference frame feature
                    _ => fields.push((0, 1)),
                }
            }
        }
        fields.extend([
            (1, 1), // delta_q_present
            (2, 2), // delta_q_res
            (1, 1), // delta_lf_present
            (1, 2), // delta_lf_res
            (1, 1), // delta_lf_multi
            (10, 6),
            (0, 6),
            (5, 6),
            (6, 6), // loop_filter_level
            (2, 3), // loop_filter_sharpness
            (1, 1), // loop_filter_delta_enabled
            (1, 1), // loop_filter_delta_update
            (1, 1),
            (0b111_1011, 7), // ref delta 0 = -5
        ]);
        fields.extend([(0, 1); 7]);
        fields.extend([(0, 1), (1, 1), (3, 7)]); // mode delta 1 = 3
        fields.extend([
            (2, 2), // cdef_damping_minus_3
            (1, 2), // cdef_bits
            (3, 4),
            (3, 2),
            (1, 4),
            (0, 2),
            (15, 4),
            (1, 2),
            (2, 4),
            (3, 2),
            (2, 2), // lr_type Y: Wiener
            (0, 2), // U: none
            (1, 2), // V: switchable
            (1, 1), // lr_unit_shift
            (1, 1), // lr_unit_extra_shift
            (1, 1), // lr_uv_shift
            (1, 1), // tx_mode_select
            (0, 1), // reduced_tx_set
            (1, 1), // trailing one bit
        ]);
        let data = pack(&fields);
        let (h, _) = FrameHeader::parse(&header_obu(&data), &seq).unwrap();
        let q = h.quantization;
        assert_eq!((q.base_q_idx, q.delta_q_y_dc, q.delta_q_u_ac), (100, -3, 0));
        assert_eq!(q.qmatrix, Some([5, 7, 7]));
        let s = h.segmentation;
        assert!(s.enabled && s.feature_enabled[2][0] && s.feature_enabled[3][5]);
        assert_eq!((s.feature_data[2][0], s.feature_data[3][5]), (-255, 7));
        assert!(s.seg_id_pre_skip);
        assert_eq!(s.last_active_seg_id, 3);
        assert!(h.delta_q_present && h.delta_lf_present && h.delta_lf_multi);
        assert_eq!((h.delta_q_res, h.delta_lf_res), (2, 1));
        // Segment 2 reaches qindex 0, but DeltaQYDc is not zero.
        assert!(!h.coded_lossless && !h.lossless[2]);
        let lf = h.loop_filter;
        assert_eq!((lf.level, lf.sharpness), ([10, 0, 5, 6], 2));
        assert_eq!(lf.ref_deltas, [-5, 0, 0, 0, -1, 0, -1, -1]);
        assert_eq!(lf.mode_deltas, [0, 3]);
        let c = h.cdef;
        assert_eq!((c.damping, c.bits), (5, 1));
        assert_eq!(
            (&c.y_pri_strength[..2], &c.y_sec_strength[..2]),
            (&[3, 15][..], &[4, 1][..])
        );
        assert_eq!(
            (&c.uv_pri_strength[..2], &c.uv_sec_strength[..2]),
            (&[1, 2][..], &[0, 4][..])
        );
        let lr = h.restoration;
        use RestorationType::*;
        assert_eq!(lr.types, [Wiener, None, Switchable]);
        assert_eq!(lr.unit_size, [256, 128, 128]);
        assert_eq!(h.tx_mode, TxMode::Select);
    }

    #[test]
    fn a_lossless_segment_in_a_lossy_frame() {
        let seq = still_sequence();
        let mut fields = STILL_START.to_vec();
        fields.extend([(100, 8), (0, 3), (0, 1), (1, 1)]); // base_q_idx 100, segmentation on
        for segment in 0..8 {
            fields.push((u32::from(segment == 1), 1));
            if segment == 1 {
                fields.push((512 - 100, 9)); // alternative quantizer -100
            }
            fields.extend([(0, 1); 7]);
        }
        fields.extend([
            (0, 1),  // delta_q_present
            (0, 12), // loop_filter_level[0], [1]
            (0, 3),  // loop_filter_sharpness
            (0, 1),  // loop_filter_delta_enabled
            (0, 4),  // cdef_damping_minus_3, cdef_bits
            (0, 12), // cdef strengths
            (0, 6),  // lr_type of the three planes
            (0, 1),  // tx_mode_select
            (0, 1),  // reduced_tx_set
            (1, 1),  // trailing one bit
        ]);
        let (h, _) = FrameHeader::parse(&header_obu(&pack(&fields)), &seq).unwrap();
        let mut expected = [false; MAX_SEGMENTS];
        expected[1] = true;
        assert_eq!(h.lossless, expected);
        assert!(!h.coded_lossless && !h.all_lossless);
        assert_eq!(h.tx_mode, TxMode::Largest);
        assert_eq!(h.restoration.types, [RestorationType::None; 3]);
    }

    #[test]
    fn explicit_tile_sizes_of_a_large_frame() {
        // 8192×4608: 128×72 superblocks of 64×64, at least 4 tiles of at most 64 columns.
        let seq = still_sequence();
        let mut fields = vec![(0, 1)]; // uniform_tile_spacing_flag
        fields.extend([(63, 6); 2]); // two columns of ns(64) = 63 + 1 superblocks
        // At most (128 × 72 >> 3) / 64 = 18 rows each: ns(18) = 17 is "1111" then "1".
        for _ in 0..4 {
            fields.extend([(15, 4), (1, 1)]);
        }
        fields.extend([(5, 3), (3, 2)]); // context_update_tile_id, tile_size_bytes_minus_1
        let data = pack(&fields);
        let t = tile_info(&mut BitReader::new(&data), &seq, 2048, 1152).unwrap();
        assert_eq!((t.cols, t.rows, t.cols_log2, t.rows_log2), (2, 4, 1, 2));
        assert_eq!(t.mi_col_starts, [0, 1024, 2048]);
        assert_eq!(t.mi_row_starts, [0, 288, 576, 864, 1152]);
        assert_eq!((t.context_update_tile_id, t.tile_size_bytes), (5, 4));
    }

    #[test]
    fn invalid_frames() {
        let seq = full_sequence();
        let parse = |fields: &[(u32, u32)]| FrameHeader::parse(&header_obu(&pack(fields)), &seq);
        // Frames that need reference slots, which are empty here.
        assert_eq!(
            parse(&[(1, 1), (0, 3), (1, 1)]),
            Err(Error::Invalid("shows an empty reference slot"))
        );
        for frame_type in [1, 3] {
            let mut fields = vec![(0, 1), (frame_type, 2)];
            fields.extend([(0, 32); 4]);
            assert_eq!(
                parse(&fields),
                Err(Error::Invalid("reference to an empty slot"))
            );
        }
        // A hidden key frame that is wider than the sequence allows.
        let hidden_key = [
            (0, 1), // show_existing_frame
            (0, 2), // KEY_FRAME
            (0, 1), // show_frame
            (1, 1), // showable_frame
            (0, 1), // error_resilient_mode
            (0, 1), // disable_cdf_update
            (0, 1), // allow_screen_content_tools
            (1, 1), // frame_size_override_flag
            (1, 8), // refresh_frame_flags
            (200, 16),
            (49, 16),
        ];
        assert_eq!(
            parse(&hidden_key),
            Err(Error::Invalid(
                "frame larger than the sequence header allows"
            ))
        );
        // An intra-only frame may not refresh every reference.
        let intra_only = [
            (0, 1),
            (2, 2),
            (1, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0xFF, 8),
        ];
        assert_eq!(
            parse(&intra_only),
            Err(Error::Invalid("intra-only frame refreshes every reference"))
        );
    }
}
