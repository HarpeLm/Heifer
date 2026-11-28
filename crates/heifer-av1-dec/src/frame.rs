//! Frame header (AV1 §5.9 and §6.8), for intra frames.
//!
//! A still image is one key frame (or, rarely, an intra-only frame). Inter frames need reference
//! frames and are rejected with [`Error::Unsupported`].

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
    /// `frame_type`: [`FrameType::Key`] or [`FrameType::IntraOnly`].
    pub frame_type: FrameType,
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

impl FrameHeader {
    /// Parses the frame header at the start of a frame header or frame OBU.
    ///
    /// Returns the header and the number of bytes it takes, up to the byte alignment: in a frame
    /// OBU, the tile group starts there.
    pub fn parse(obu: &Obu<'_>, seq: &SequenceHeader) -> Result<(Self, usize), Error> {
        let mut r = BitReader::new(obu.payload);
        let (frame_type, show_frame, showable_frame, error_resilient_mode);
        if seq.reduced_still_picture_header {
            (frame_type, show_frame, showable_frame, error_resilient_mode) =
                (FrameType::Key, true, false, true);
        } else {
            if r.flag()? {
                return Err(Error::Unsupported("show_existing_frame"));
            }
            frame_type = match r.f(2)? {
                0 => FrameType::Key,
                1 => FrameType::Inter,
                2 => FrameType::IntraOnly,
                _ => FrameType::Switch,
            };
            if !matches!(frame_type, FrameType::Key | FrameType::IntraOnly) {
                return Err(Error::Unsupported("inter frames"));
            }
            show_frame = r.flag()?;
            if show_frame {
                r.f(seq.timing.presentation_time_bits)?; // temporal_point_info()
            }
            showable_frame = if show_frame {
                frame_type != FrameType::Key
            } else {
                r.flag()?
            };
            error_resilient_mode = (frame_type == FrameType::Key && show_frame) || r.flag()?;
        }
        let disable_cdf_update = r.flag()?;
        let allow_screen_content_tools = match seq.seq_force_screen_content_tools {
            2 => r.flag()?,
            v => v == 1,
        };
        // Intra frames force integer motion vectors, so `force_integer_mv` is not read.
        if allow_screen_content_tools && seq.seq_force_integer_mv == 2 {
            r.flag()?;
        }
        let current_frame_id = if seq.frame_id_numbers_present {
            r.f(u32::from(
                seq.delta_frame_id_length + seq.additional_frame_id_length,
            ))?
        } else {
            0
        };
        let frame_size_override = !seq.reduced_still_picture_header && r.flag()?;
        let order_hint = r.f(u32::from(seq.order_hint_bits))?;
        // Intra frames have no primary reference frame (`PRIMARY_REF_NONE`).
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
        let refresh_frame_flags = if frame_type == FrameType::Key && show_frame {
            ALL_FRAMES
        } else {
            r.f(8)? as u8
        };
        if frame_type == FrameType::IntraOnly && refresh_frame_flags == ALL_FRAMES {
            return Err(Error::Invalid("intra-only frame refreshes every reference"));
        }
        if refresh_frame_flags != ALL_FRAMES && error_resilient_mode && seq.enable_order_hint {
            for _ in 0..8 {
                r.f(u32::from(seq.order_hint_bits))?; // ref_order_hint
            }
        }

        // frame_size(), superres_params(), compute_image_size(), render_size().
        let (frame_width, frame_height) = if frame_size_override {
            (
                r.f(u32::from(seq.frame_width_bits))? + 1,
                r.f(u32::from(seq.frame_height_bits))? + 1,
            )
        } else {
            (seq.max_frame_width, seq.max_frame_height)
        };
        if frame_width > seq.max_frame_width || frame_height > seq.max_frame_height {
            return Err(Error::Invalid(
                "frame larger than the sequence header allows",
            ));
        }
        let superres_denom = if seq.enable_superres && r.flag()? {
            r.f(3)? as u8 + 9
        } else {
            8
        };
        let upscaled_width = frame_width;
        let frame_width =
            (upscaled_width * 8 + u32::from(superres_denom) / 2) / u32::from(superres_denom);
        let mi_cols = 2 * frame_width.div_ceil(8);
        let mi_rows = 2 * frame_height.div_ceil(8);
        let (render_width, render_height) = if r.flag()? {
            (r.f(16)? + 1, r.f(16)? + 1)
        } else {
            (upscaled_width, frame_height)
        };
        let allow_intrabc =
            allow_screen_content_tools && upscaled_width == frame_width && r.flag()?;

        let disable_frame_end_update_cdf =
            seq.reduced_still_picture_header || disable_cdf_update || r.flag()?;
        let tile_info = tile_info(&mut r, seq, mi_cols, mi_rows)?;
        let num_planes = if seq.color.monochrome { 1 } else { 3 };
        let quantization = quantization_params(&mut r, seq, num_planes)?;
        let segmentation = segmentation_params(&mut r)?;
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
            loop_filter_params(&mut r, num_planes)?
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
        // Intra frames: no reference_select, skip mode, warped motion or global motion.
        let reduced_tx_set = r.flag()?;
        let film_grain = if seq.film_grain_params_present && (show_frame || showable_frame) {
            film_grain_params(&mut r, seq)?
        } else {
            None
        };
        // A frame header OBU ends with trailing bits; in a frame OBU the tile group starts at the
        // next byte.
        let size = if obu.obu_type == ObuType::FrameHeader {
            r.trailing_bits()?;
            obu.payload.len()
        } else {
            r.byte_align()?
        };
        Ok((
            Self {
                frame_type,
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
            },
            size,
        ))
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

/// `segmentation_params()` (§5.9.14), for a frame without a primary reference frame.
fn segmentation_params(r: &mut BitReader<'_>) -> Result<Segmentation, Error> {
    let mut s = Segmentation {
        enabled: r.flag()?,
        feature_enabled: [[false; SEG_LVL_MAX]; MAX_SEGMENTS],
        feature_data: [[0; SEG_LVL_MAX]; MAX_SEGMENTS],
        seg_id_pre_skip: false,
        last_active_seg_id: 0,
    };
    if !s.enabled {
        return Ok(s);
    }
    // Without a primary reference frame the map and the data are always updated.
    for segment in 0..MAX_SEGMENTS {
        for feature in 0..SEG_LVL_MAX {
            if !r.flag()? {
                continue;
            }
            s.feature_enabled[segment][feature] = true;
            let bits = SEGMENTATION_FEATURE_BITS[feature];
            let limit = SEGMENTATION_FEATURE_MAX[feature];
            s.feature_data[segment][feature] = if SEGMENTATION_FEATURE_SIGNED[feature] {
                (r.su(1 + bits)? as i16).clamp(-limit, limit)
            } else {
                (r.f(bits)? as i16).clamp(0, limit)
            };
            s.last_active_seg_id = segment as u8;
            if feature >= SEG_LVL_REF_FRAME {
                s.seg_id_pre_skip = true;
            }
        }
    }
    Ok(s)
}

/// `loop_filter_params()` (§5.9.11), when the filter may be used.
fn loop_filter_params(r: &mut BitReader<'_>, num_planes: usize) -> Result<LoopFilter, Error> {
    let mut lf = LoopFilter {
        level: [r.f(6)? as u8, r.f(6)? as u8, 0, 0],
        sharpness: 0,
        delta_enabled: false,
        ref_deltas: DEFAULT_REF_DELTAS,
        mode_deltas: [0; 2],
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

/// `film_grain_params()` (§5.9.30) of an intra frame: `None` when grain is not applied.
fn film_grain_params(
    r: &mut BitReader<'_>,
    seq: &SequenceHeader,
) -> Result<Option<FilmGrain>, Error> {
    if !r.flag()? {
        return Ok(None); // apply_grain
    }
    let grain_seed = r.f(16)? as u16;
    // Intra frames always carry new parameters (update_grain = 1).
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
    fn unsupported_and_invalid_frames() {
        let seq = full_sequence();
        let parse = |fields: &[(u32, u32)]| FrameHeader::parse(&header_obu(&pack(fields)), &seq);
        assert_eq!(
            parse(&[(1, 1), (0, 3)]),
            Err(Error::Unsupported("show_existing_frame"))
        );
        assert_eq!(
            parse(&[(0, 1), (1, 2)]),
            Err(Error::Unsupported("inter frames"))
        );
        assert_eq!(
            parse(&[(0, 1), (3, 2)]),
            Err(Error::Unsupported("inter frames"))
        );
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
