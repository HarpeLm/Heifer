//! Decoding of frames (AV1 §5.11, §6.10, §7.9–7.13, §7.19–7.20): reads each tile's blocks,
//! predicts them (intra prediction, palettes, intra block copy, or inter prediction from the
//! reference frames, in `inter` and `mc`), adds their residuals, then runs the loop filters
//! (`filter`).

use std::sync::Arc;

use crate::Error;
use crate::cdf_tables::Cdfs;
use crate::frame::{FrameHeader, RestorationType, TxMode, relative_dist};
use crate::predict::{self, DC_PRED, IntraBlock, Plane, SMOOTH_H_PRED, SMOOTH_PRED, SMOOTH_V_PRED};
use crate::sequence::SequenceHeader;
use crate::spec_tables::*;
use crate::symbol::SymbolDecoder;
use crate::tile::{CodedFrame, Tile};
use crate::transform::{self, DCT_DCT, IDTX};

mod filter;
mod inter;
mod mc;

use inter::{INVALID_MV, MvStack, NONE};

/// A decoded picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picture {
    /// Width of the luma plane.
    pub width: u32,
    /// Height of the luma plane.
    pub height: u32,
    /// Bits per sample.
    pub bit_depth: u8,
    /// Chroma has half the luma width.
    pub subsampling_x: bool,
    /// Chroma has half the luma height.
    pub subsampling_y: bool,
    /// Y, then U and V unless the picture is monochrome. Rows are stored one after the other,
    /// without padding; chroma planes are `(width + ss_x) >> ss_x` samples wide.
    pub planes: Vec<Vec<u16>>,
}

const BLOCK_4X4: usize = 0;
const BLOCK_8X8: usize = 3;
const BLOCK_32X32: usize = 9;
const BLOCK_64X64: usize = 12;
const BLOCK_128X128: usize = 15;
const BLOCK_INVALID: u8 = 22;

const PARTITION_NONE: usize = 0;
const PARTITION_HORZ: usize = 1;
const PARTITION_VERT: usize = 2;
const PARTITION_SPLIT: usize = 3;
const PARTITION_HORZ_A: usize = 4;
const PARTITION_HORZ_B: usize = 5;
const PARTITION_VERT_A: usize = 6;
const PARTITION_VERT_B: usize = 7;
const PARTITION_HORZ_4: usize = 8;
const PARTITION_VERT_4: usize = 9;

const TX_4X4: usize = 0;
const TX_32X32: usize = 3;
const TX_16X32: usize = 9;
const TX_32X16: usize = 10;
const TX_16X64: usize = 17;
const TX_64X16: usize = 18;

const UV_CFL_PRED: u8 = 13;
/// `RefFrame[ 0 ]` of intra blocks, intra block copy included.
const INTRA_FRAME: i8 = 0;
/// `RefFrames` of a unit no block of this frame has written yet.
const REF_UNWRITTEN: i8 = -2;
const MAX_VARTX_DEPTH: usize = 2;
const MAX_REF_MV_STACK_SIZE: usize = 8;
const REF_CAT_LEVEL: u32 = 640;
const MV_BORDER: i32 = 128;
const INTRABC_DELAY_PIXELS: i32 = 256;
const MV_INTRABC_CONTEXT: usize = 1;
/// `InterpFilters` of intra block copy: `BILINEAR`.
const BILINEAR: u8 = 3;
const SWITCHABLE: u8 = 4;
const SEG_LVL_ALT_Q: usize = 0;
const SEG_LVL_SKIP: usize = 6;
const TX_CLASS_2D: usize = 0;
const TX_CLASS_HORIZ: usize = 1;
const TX_CLASS_VERT: usize = 2;

/// Decodes an intra frame on its own. Film grain is not applied. Frames that refer to others go
/// through a [`Decoder`].
pub fn decode_frame(frame: &CodedFrame<'_>) -> Result<Picture, Error> {
    if !frame.header.is_intra() || frame.header.show_existing_frame.is_some() {
        return Err(Error::Unsupported("inter frames"));
    }
    Ok(decode_coded(frame, &Default::default())?.picture)
}

/// Decodes the frames of a stream in order, keeping the eight reference slots (§7.20).
#[derive(Default)]
pub struct Decoder {
    slots: [Option<Arc<RefFrame>>; 8],
}

impl Decoder {
    /// A decoder with empty reference slots.
    pub fn new() -> Self {
        Self::default()
    }

    /// Decodes `frame`, one of the frames of [`frames`](crate::frames) in order, and returns its
    /// picture when the frame is shown. Film grain is not applied.
    pub fn decode(&mut self, frame: &CodedFrame<'_>) -> Result<Option<Picture>, Error> {
        let fh = &frame.header;
        let current = match fh.show_existing_frame {
            Some(idx) => self.slots[usize::from(idx)]
                .clone()
                .ok_or(Error::Invalid("shows an empty reference slot"))?,
            None => Arc::new(decode_coded(frame, &self.slots)?),
        };
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if fh.refresh_frame_flags >> i & 1 == 1 {
                *slot = Some(current.clone());
            }
        }
        Ok(fh.show_frame.then(|| current.picture.clone()))
    }
}

/// What a decoded frame leaves in a reference slot.
struct RefFrame {
    /// `LrFrame`.
    picture: Picture,
    /// `RefUpscaledWidth`, `RefFrameHeight`, `RefMiRows` and `RefMiCols`.
    upscaled_width: u32,
    frame_height: u32,
    mi_rows: usize,
    mi_cols: usize,
    /// `RefFrameType` is `KEY_FRAME` or `INTRA_ONLY_FRAME`.
    intra: bool,
    /// `SavedOrderHints`.
    order_hints: [u32; 8],
    /// The CDFs at the end of the frame (`save_cdfs()`).
    cdfs: Box<Cdfs>,
    /// Row stride of the per-unit arrays.
    mi_stride: usize,
    /// `SavedRefFrames`: `MfRefFrames`, `NONE` (-1) where no motion vector is kept.
    mf_ref_frames: Vec<i8>,
    /// `SavedMvs`.
    mf_mvs: Vec<[i32; 2]>,
    /// `SavedSegmentIds`.
    segment_ids: Vec<u8>,
}

/// Decodes a frame (not a shown existing one), given the reference slots.
fn decode_coded(
    frame: &CodedFrame<'_>,
    slots: &[Option<Arc<RefFrame>>; 8],
) -> Result<RefFrame, Error> {
    let (seq, fh) = (&frame.sequence, &frame.header);
    if fh.upscaled_width != fh.frame_width {
        return Err(Error::Unsupported("superres"));
    }
    if fh.quantization.qmatrix.is_some() {
        return Err(Error::Unsupported("quantizer matrices"));
    }
    let cdfs = match fh.primary_ref_frame {
        Some(p) => {
            // load_cdfs()
            let slot = &slots[usize::from(fh.ref_frame_idx[usize::from(p)])];
            let mut cdfs = slot
                .as_ref()
                .ok_or(Error::Invalid("reference to an empty slot"))?
                .cdfs
                .clone();
            cdfs.clear_counters();
            cdfs
        }
        None => Cdfs::new(match fh.quantization.base_q_idx {
            0..=20 => 0,
            21..=60 => 1,
            61..=120 => 2,
            _ => 3,
        }),
    };
    let mut f = FrameState::new(seq, fh, cdfs);
    if !fh.is_intra() {
        for (i, &idx) in fh.ref_frame_idx.iter().enumerate() {
            let slot = slots[usize::from(idx)].clone();
            f.refs[i + 1] = Some(slot.ok_or(Error::Invalid("reference to an empty slot"))?);
        }
    }
    if let Some(p) = fh.primary_ref_frame {
        // load_previous_segment_ids()
        let prev = f.refs[usize::from(p) + 1].clone();
        if let Some(prev) = prev
            .filter(|r| fh.segmentation.enabled && r.mi_rows == f.mi_rows && r.mi_cols == f.mi_cols)
        {
            f.prev_segment_ids = prev.segment_ids.clone();
        }
    }
    if fh.use_ref_frame_mvs {
        f.motion_field_estimation();
    }
    let mut saved = None;
    for (num, tile) in frame.tiles.iter().enumerate() {
        let cdfs = TileDecoder::new(&mut f, tile)?.decode()?;
        // The exit process for the symbol decoder keeps one tile's CDFs.
        if num == fh.tile_info.context_update_tile_id as usize && !fh.disable_frame_end_update_cdf {
            saved = Some(cdfs);
        }
    }
    if let Some(cdfs) = saved {
        f.cdfs = cdfs; // frame_end_update_cdf()
    }
    f.apply_filters();
    Ok(f.into_ref_frame())
}

/// State shared by the tiles of a frame: the reconstructed planes and what blocks leave behind
/// for their neighbours, per 4×4 luma unit (`MiRow`, `MiCol`).
struct FrameState<'h> {
    seq: &'h SequenceHeader,
    fh: &'h FrameHeader,
    /// The CDFs every tile starts from, then those kept at the end of the frame.
    cdfs: Box<Cdfs>,
    sub_x: usize,
    sub_y: usize,
    num_planes: usize,
    bit_depth: u32,
    mi_rows: usize,
    mi_cols: usize,
    /// Row stride of the per-unit arrays, which cover whole superblocks.
    mi_stride: usize,
    planes: Vec<Plane>,
    y_modes: Vec<u8>,
    uv_modes: Vec<u8>,
    mi_sizes: Vec<u8>,
    skips: Vec<bool>,
    segment_ids: Vec<u8>,
    /// `InterTxSizes`.
    tx_sizes: Vec<u8>,
    is_inters: Vec<bool>,
    skip_modes: Vec<bool>,
    /// `RefFrames`: `REF_UNWRITTEN` until the unit is decoded.
    ref_frames: Vec<[i8; 2]>,
    /// `Mvs`, row then column, in 1/8 samples.
    mvs: Vec<[[i32; 2]; 2]>,
    interp_filters: Vec<[u8; 2]>,
    comp_group_idxs: Vec<u8>,
    compound_idxs: Vec<u8>,
    /// The reference frames, indexed by `LAST_FRAME`…`ALTREF_FRAME`.
    refs: [Option<Arc<RefFrame>>; 8],
    /// `PrevSegmentIds`: empty when they are all 0.
    prev_segment_ids: Vec<u8>,
    /// `MotionFieldMvs` per reference frame, `mi_cols >> 1` entries per row.
    motion_field: Vec<Vec<[i32; 2]>>,
    tx_types: Vec<u8>,
    /// `PaletteSizes` and `PaletteColors` for Y and U.
    palette_sizes: [Vec<u8>; 2],
    palette_colors: [Vec<[u16; 8]>; 2],
    /// `cdef_idx` per 64×64 block.
    cdef_idx: Vec<i8>,
    /// `DeltaLFs`.
    delta_lfs: Vec<[i8; 4]>,
    /// `LoopfilterTxSizes` per plane and 4×4 unit of that plane, `mi_stride` units per row.
    lf_tx_sizes: [Vec<u8>; 3],
    /// Loop restoration units of each plane, row after row.
    lr_units: [Vec<LrUnit>; 3],
}

/// `LrType`, `LrWiener`, `LrSgrSet` and `LrSgrXqd` of a loop restoration unit.
#[derive(Debug, Clone, Copy, Default)]
struct LrUnit {
    /// `RESTORE_NONE` (0), `RESTORE_WIENER` (1) or `RESTORE_SGRPROJ` (2).
    kind: u8,
    wiener: [[i32; 3]; 2],
    sgr_set: u8,
    sgr_xqd: [i32; 2],
}

impl<'h> FrameState<'h> {
    fn new(seq: &'h SequenceHeader, fh: &'h FrameHeader, cdfs: Box<Cdfs>) -> Self {
        let (mi_rows, mi_cols) = (fh.mi_rows as usize, fh.mi_cols as usize);
        let mi_stride = mi_cols.next_multiple_of(32);
        let rows = mi_rows.next_multiple_of(32);
        let num_planes = if seq.color.monochrome { 1 } else { 3 };
        let (sub_x, sub_y) = (
            usize::from(seq.color.subsampling_x),
            usize::from(seq.color.subsampling_y),
        );
        let planes = (0..num_planes)
            .map(|p| {
                let (sx, sy) = if p == 0 { (0, 0) } else { (sub_x, sub_y) };
                let stride = (mi_stride * 4) >> sx;
                Plane {
                    data: vec![0; stride * ((rows * 4) >> sy)],
                    stride,
                }
            })
            .collect();
        let units = mi_stride * rows;
        let mut f = Self {
            seq,
            fh,
            cdfs,
            sub_x,
            sub_y,
            num_planes,
            bit_depth: u32::from(seq.color.bit_depth),
            mi_rows,
            mi_cols,
            mi_stride,
            planes,
            y_modes: vec![0; units],
            uv_modes: vec![0; units],
            mi_sizes: vec![0; units],
            skips: vec![false; units],
            segment_ids: vec![0; units],
            tx_sizes: vec![0; units],
            is_inters: vec![false; units],
            skip_modes: vec![false; units],
            ref_frames: vec![[REF_UNWRITTEN, NONE]; units],
            mvs: vec![[[0; 2]; 2]; units],
            interp_filters: vec![[0; 2]; units],
            comp_group_idxs: vec![0; units],
            compound_idxs: vec![0; units],
            refs: Default::default(),
            prev_segment_ids: Vec::new(),
            motion_field: Vec::new(),
            tx_types: vec![0; units],
            palette_sizes: [vec![0; units], vec![0; units]],
            palette_colors: [vec![[0; 8]; units], vec![[0; 8]; units]],
            cdef_idx: vec![-1; (mi_stride / 16) * (rows / 16)],
            delta_lfs: vec![[0; 4]; units],
            lf_tx_sizes: [vec![0; units], vec![0; units], vec![0; units]],
            lr_units: Default::default(),
        };
        for plane in 0..num_planes {
            if fh.restoration.types[plane] != RestorationType::None {
                let (rows, cols) = f.lr_unit_counts(plane);
                f.lr_units[plane] = vec![LrUnit::default(); rows * cols];
            }
        }
        f
    }

    fn sub(&self, plane: usize) -> (usize, usize) {
        if plane > 0 {
            (self.sub_x, self.sub_y)
        } else {
            (0, 0)
        }
    }

    /// `unitRows` and `unitCols` of a plane (`count_units_in_frame()`).
    fn lr_unit_counts(&self, plane: usize) -> (usize, usize) {
        let (sx, sy) = self.sub(plane);
        let unit = usize::from(self.fh.restoration.unit_size[plane]);
        let count = |size: usize| ((size + (unit >> 1)) / unit).max(1);
        (
            count(round2(self.fh.frame_height as i32, sy as u32) as usize),
            count(round2(self.fh.upscaled_width as i32, sx as u32) as usize),
        )
    }

    fn mi(&self, row: usize, col: usize) -> usize {
        row * self.mi_stride + col
    }

    /// The motion field motion vector storage process (§7.19) and the frame state that the
    /// reference frame update process (§7.20) keeps.
    fn into_ref_frame(mut self) -> RefFrame {
        let fh = self.fh;
        let units = self.ref_frames.len();
        let (mut mf_ref_frames, mut mf_mvs) = (vec![NONE; units], vec![[0; 2]; units]);
        for (i, refs) in self.ref_frames.iter().enumerate() {
            for list in 0..2 {
                let r = refs[list];
                if r > INTRA_FRAME
                    && relative_dist(self.seq, fh.order_hints[r as usize], fh.order_hint) < 0
                {
                    let mv = self.mvs[i][list];
                    // REFMVS_LIMIT
                    if mv[0].abs() < 1 << 12 && mv[1].abs() < 1 << 12 {
                        mf_ref_frames[i] = r;
                        mf_mvs[i] = mv;
                    }
                }
            }
        }
        let seg = &fh.segmentation;
        if seg.enabled && !seg.update_map {
            self.segment_ids = if self.prev_segment_ids.is_empty() {
                vec![0; units]
            } else {
                std::mem::take(&mut self.prev_segment_ids)
            };
        }
        let mut order_hints = fh.order_hints;
        order_hints[0] = fh.order_hint;
        RefFrame {
            upscaled_width: fh.upscaled_width,
            frame_height: fh.frame_height,
            mi_rows: self.mi_rows,
            mi_cols: self.mi_cols,
            intra: fh.is_intra(),
            order_hints,
            cdfs: self.cdfs.clone(),
            mi_stride: self.mi_stride,
            mf_ref_frames,
            mf_mvs,
            segment_ids: std::mem::take(&mut self.segment_ids),
            picture: self.picture(),
        }
    }

    /// The motion field estimation process (§7.9).
    fn motion_field_estimation(&mut self) {
        let (w8, h8) = (self.mi_cols >> 1, self.mi_rows >> 1);
        self.motion_field = vec![vec![[INVALID_MV; 2]; w8 * h8]; 8];
        let hints = self.fh.order_hints;
        let dist = |a: u32| relative_dist(self.seq, a, self.fh.order_hint);
        let last_alt = self.refs[1].as_ref().map_or(0, |r| r.order_hints[7]);
        if last_alt != hints[4] {
            self.project(1, -1);
        }
        let mut stamp = 1; // MFMV_STACK_SIZE - 2
        for src in [5, 6, 7] {
            if dist(hints[src]) > 0 && (src < 7 || stamp >= 0) && self.project(src, 1) {
                stamp -= 1;
            }
        }
        if stamp >= 0 {
            self.project(2, -1);
        }
    }

    /// The projection process (§7.9.2).
    fn project(&mut self, src: usize, dst_sign: i32) -> bool {
        let Some(rf) = self.refs[src].clone() else {
            return false;
        };
        if rf.mi_rows != self.mi_rows || rf.mi_cols != self.mi_cols || rf.intra {
            return false;
        }
        let (w8, h8) = (self.mi_cols >> 1, self.mi_rows >> 1);
        let seq = self.seq;
        let hints = self.fh.order_hints;
        let cur = self.fh.order_hint;
        // get_mv_projection()
        let projection = |mv: [i32; 2], num: i32, den: i32| {
            let num = num.clamp(-31, 31);
            let mult = i32::from(DIV_MULT[den.min(31) as usize]);
            mv.map(|v| round2_signed(v * num * mult, 14).clamp(-(1 << 14) + 1, (1 << 14) - 1))
        };
        let project = |v8: usize, delta: i32, max8: usize, max_off8: isize| {
            let base8 = ((v8 >> 3) << 3) as isize;
            let offset8 = if delta >= 0 {
                delta >> 6
            } else {
                -((-delta) >> 6)
            };
            let v8 = v8 as isize + (dst_sign * offset8) as isize;
            let valid = v8 >= 0
                && v8 < max8 as isize
                && v8 >= base8 - max_off8
                && v8 < base8 + 8 + max_off8;
            valid.then_some(v8 as usize)
        };
        for y8 in 0..h8 {
            for x8 in 0..w8 {
                let i = (2 * y8 + 1) * rf.mi_stride + 2 * x8 + 1;
                let src_ref = rf.mf_ref_frames[i];
                if src_ref <= INTRA_FRAME {
                    continue;
                }
                let ref_to_cur = relative_dist(seq, hints[src], cur);
                let ref_offset = relative_dist(seq, hints[src], rf.order_hints[src_ref as usize]);
                if ref_to_cur.abs() > 31 || ref_offset.abs() > 31 || ref_offset <= 0 {
                    continue;
                }
                let mv = rf.mf_mvs[i];
                let proj = projection(mv, ref_to_cur * dst_sign, ref_offset);
                // MAX_OFFSET_HEIGHT and MAX_OFFSET_WIDTH.
                let (Some(py), Some(px)) =
                    (project(y8, proj[0], h8, 0), project(x8, proj[1], w8, 8))
                else {
                    continue;
                };
                for dst in 1..8 {
                    let ref_to_dst = relative_dist(seq, cur, hints[dst]);
                    self.motion_field[dst][py * w8 + px] = projection(mv, ref_to_dst, ref_offset);
                }
            }
        }
        true
    }

    fn picture(self) -> Picture {
        let (w, h) = (self.fh.frame_width as usize, self.fh.frame_height as usize);
        let planes = self
            .planes
            .iter()
            .enumerate()
            .map(|(p, plane)| {
                let (sx, sy) = if p == 0 {
                    (0, 0)
                } else {
                    (self.sub_x, self.sub_y)
                };
                let (pw, ph) = ((w + sx) >> sx, (h + sy) >> sy);
                (0..ph)
                    .flat_map(|y| plane.data[y * plane.stride..][..pw].iter().copied())
                    .collect()
            })
            .collect();
        Picture {
            width: w as u32,
            height: h as u32,
            bit_depth: self.seq.color.bit_depth,
            subsampling_x: self.seq.color.subsampling_x,
            subsampling_y: self.seq.color.subsampling_y,
            planes,
        }
    }
}

/// The block being decoded (the spec's `MiRow`, `MiSize`, `YMode`…).
#[derive(Default)]
struct Block {
    row: usize,
    col: usize,
    size: usize,
    bw4: usize,
    bh4: usize,
    has_chroma: bool,
    avail_u: bool,
    avail_l: bool,
    avail_u_chroma: bool,
    avail_l_chroma: bool,
    skip: bool,
    segment_id: usize,
    lossless: bool,
    y_mode: u8,
    uv_mode: u8,
    angle_delta_y: i32,
    angle_delta_uv: i32,
    cfl_alpha_u: i32,
    cfl_alpha_v: i32,
    filter_intra_mode: Option<u8>,
    palette_size_y: usize,
    palette_size_uv: usize,
    palette_y: [u16; 8],
    palette_u: [u16; 8],
    palette_v: [u16; 8],
    tx_size: usize,
    is_inter: bool,
    /// `RefFrame`.
    ref_frame: [i8; 2],
    /// `Mv`.
    mv: [[i32; 2]; 2],
    /// `LeftRefFrame` and `AboveRefFrame`.
    left_ref: [i8; 2],
    above_ref: [i8; 2],
    skip_mode: bool,
    stack: MvStack,
    ref_mv_idx: usize,
    interp_filter: [u8; 2],
    motion_mode: u8,
    interintra: bool,
    interintra_mode: u8,
    wedge_interintra: bool,
    wedge_index: usize,
    wedge_sign: usize,
    mask_type: bool,
    compound_type: u8,
    comp_group_idx: u8,
    compound_idx: u8,
    /// `NumSamples`, `NumSamplesScanned` and `CandList` of the find warp samples process.
    num_samples: usize,
    num_samples_scanned: usize,
    cand_list: [[i32; 4]; 8],
    /// `LocalWarpParams` when `LocalValid`.
    local_warp: Option<[i32; 6]>,
    max_luma_w: usize,
    max_luma_h: usize,
}

struct TileDecoder<'h, 'f> {
    f: &'f mut FrameState<'h>,
    sd: SymbolDecoder<'h>,
    cdf: Box<Cdfs>,
    row_start: usize,
    row_end: usize,
    col_start: usize,
    col_end: usize,
    current_q_index: i32,
    delta_lf: [i32; 4],
    read_deltas: bool,
    ref_lr_wiener: [[[i32; 3]; 2]; 3],
    ref_sgr_xqd: [[i32; 2]; 3],
    /// `AboveLevelContext`, `AboveDcContext`, `LeftLevelContext`, `LeftDcContext`, per plane and
    /// 4×4 unit of that plane.
    above_level: [Vec<u8>; 3],
    above_dc: [Vec<u8>; 3],
    left_level: [Vec<u8>; 3],
    left_dc: [Vec<u8>; 3],
    /// `BlockDecoded`, offset by one so that index 0 is the spec's -1.
    block_decoded: [[[bool; 34]; 34]; 3],
    b: Block,
    quant: Vec<i32>,
    /// `ColorMapY` and `ColorMapUV`, 64 entries per row.
    color_maps: [Vec<u8>; 2],
    /// `AboveSegPredContext` and `LeftSegPredContext`.
    above_seg_pred: Vec<u8>,
    left_seg_pred: Vec<u8>,
    /// `Mask` of compound and inter-intra prediction, `mask_w` entries per row.
    mask: Vec<u8>,
    mask_w: usize,
}

/// `Wiener_Taps_Mid`, `Sgrproj_Xqd_Mid`.
const WIENER_TAPS_MID: [i32; 3] = [3, -7, 15];
const SGRPROJ_XQD_MID: [i32; 2] = [-32, 31];

fn round2(x: i32, n: u32) -> i32 {
    if n == 0 { x } else { (x + (1 << (n - 1))) >> n }
}

fn round2_signed(x: i32, n: u32) -> i32 {
    if x >= 0 { round2(x, n) } else { -round2(-x, n) }
}

fn tx_class(tx_type: u8) -> usize {
    match tx_type {
        10 | 12 | 14 => TX_CLASS_VERT,
        11 | 13 | 15 => TX_CLASS_HORIZ,
        _ => TX_CLASS_2D,
    }
}

fn is_smooth(mode: u8) -> bool {
    matches!(mode, SMOOTH_PRED | SMOOTH_V_PRED | SMOOTH_H_PRED)
}

fn partition_cdf(cdf: &mut Cdfs, bsl: u8, ctx: usize) -> &mut [u16] {
    match bsl {
        1 => &mut cdf.partition_w8[ctx],
        2 => &mut cdf.partition_w16[ctx],
        3 => &mut cdf.partition_w32[ctx],
        4 => &mut cdf.partition_w64[ctx],
        _ => &mut cdf.partition_w128[ctx],
    }
}

fn scan(tx: usize, tx_type: u8) -> &'static [u16] {
    if tx == TX_16X64 {
        return &DEFAULT_SCAN_16X32;
    }
    if tx == TX_64X16 {
        return &DEFAULT_SCAN_32X16;
    }
    if TX_SIZE_SQR_UP[tx] == 4 {
        return &DEFAULT_SCAN_32X32;
    }
    let class = if tx_type == IDTX {
        TX_CLASS_2D
    } else {
        tx_class(tx_type)
    };
    match (class, tx) {
        (TX_CLASS_VERT, 0) => &MROW_SCAN_4X4,
        (TX_CLASS_VERT, 5) => &MROW_SCAN_4X8,
        (TX_CLASS_VERT, 6) => &MROW_SCAN_8X4,
        (TX_CLASS_VERT, 1) => &MROW_SCAN_8X8,
        (TX_CLASS_VERT, 7) => &MROW_SCAN_8X16,
        (TX_CLASS_VERT, 8) => &MROW_SCAN_16X8,
        (TX_CLASS_VERT, 2) => &MROW_SCAN_16X16,
        (TX_CLASS_VERT, 13) => &MROW_SCAN_4X16,
        (TX_CLASS_VERT, _) => &MROW_SCAN_16X4,
        (TX_CLASS_HORIZ, 0) => &MCOL_SCAN_4X4,
        (TX_CLASS_HORIZ, 5) => &MCOL_SCAN_4X8,
        (TX_CLASS_HORIZ, 6) => &MCOL_SCAN_8X4,
        (TX_CLASS_HORIZ, 1) => &MCOL_SCAN_8X8,
        (TX_CLASS_HORIZ, 7) => &MCOL_SCAN_8X16,
        (TX_CLASS_HORIZ, 8) => &MCOL_SCAN_16X8,
        (TX_CLASS_HORIZ, 2) => &MCOL_SCAN_16X16,
        (TX_CLASS_HORIZ, 13) => &MCOL_SCAN_4X16,
        (TX_CLASS_HORIZ, _) => &MCOL_SCAN_16X4,
        (_, 0) => &DEFAULT_SCAN_4X4,
        (_, 5) => &DEFAULT_SCAN_4X8,
        (_, 6) => &DEFAULT_SCAN_8X4,
        (_, 1) => &DEFAULT_SCAN_8X8,
        (_, 7) => &DEFAULT_SCAN_8X16,
        (_, 8) => &DEFAULT_SCAN_16X8,
        (_, 2) => &DEFAULT_SCAN_16X16,
        (_, 9) => &DEFAULT_SCAN_16X32,
        (_, 10) => &DEFAULT_SCAN_32X16,
        (_, 13) => &DEFAULT_SCAN_4X16,
        (_, 14) => &DEFAULT_SCAN_16X4,
        (_, 15) => &DEFAULT_SCAN_8X32,
        (_, 16) => &DEFAULT_SCAN_32X8,
        _ => &DEFAULT_SCAN_32X32,
    }
}

/// `neg_deinterleave()`.
fn neg_deinterleave(diff: i32, r: i32, max: i32) -> i32 {
    if r == 0 {
        return diff;
    }
    if r >= max - 1 {
        return max - diff - 1;
    }
    let near = if 2 * r < max {
        2 * r
    } else {
        2 * (max - r - 1)
    };
    if diff <= near {
        if diff & 1 == 1 {
            r + ((diff + 1) >> 1)
        } else {
            r - (diff >> 1)
        }
    } else if 2 * r < max {
        diff
    } else {
        max - (diff + 1)
    }
}

/// `inverse_recenter()`.
fn inverse_recenter(r: i32, v: i32) -> i32 {
    if v > 2 * r {
        v
    } else if v & 1 == 1 {
        r - ((v + 1) >> 1)
    } else {
        r + (v >> 1)
    }
}

impl<'h, 'f> TileDecoder<'h, 'f> {
    fn new(f: &'f mut FrameState<'h>, tile: &Tile<'h>) -> Result<Self, Error> {
        let info = &f.fh.tile_info;
        let (row, col) = (tile.row as usize, tile.col as usize);
        let q = f.fh.quantization.base_q_idx;
        let above = f.mi_stride + 32;
        let left = f.mi_rows.next_multiple_of(32) + 32;
        Ok(Self {
            sd: SymbolDecoder::new(tile.data, f.fh.disable_cdf_update)?,
            cdf: f.cdfs.clone(),
            row_start: info.mi_row_starts[row] as usize,
            row_end: info.mi_row_starts[row + 1] as usize,
            col_start: info.mi_col_starts[col] as usize,
            col_end: info.mi_col_starts[col + 1] as usize,
            current_q_index: i32::from(q),
            delta_lf: [0; 4],
            read_deltas: false,
            ref_lr_wiener: [[WIENER_TAPS_MID; 2]; 3],
            ref_sgr_xqd: [SGRPROJ_XQD_MID; 3],
            above_level: std::array::from_fn(|_| vec![0; above]),
            above_dc: std::array::from_fn(|_| vec![0; above]),
            left_level: std::array::from_fn(|_| vec![0; left]),
            left_dc: std::array::from_fn(|_| vec![0; left]),
            block_decoded: [[[false; 34]; 34]; 3],
            b: Block::default(),
            quant: vec![0; 1024],
            color_maps: [vec![0; 64 * 64], vec![0; 64 * 64]],
            above_seg_pred: vec![0; above],
            left_seg_pred: vec![0; left],
            mask: Vec::new(),
            mask_w: 0,
            f,
        })
    }

    fn sym(&mut self, cdf: impl FnOnce(&mut Cdfs) -> &mut [u16]) -> Result<usize, Error> {
        self.sd.read_symbol(cdf(&mut self.cdf))
    }

    fn lit(&mut self, n: u32) -> Result<u32, Error> {
        self.sd.read_literal(n)
    }

    fn is_inside(&self, row: isize, col: isize) -> bool {
        col >= self.col_start as isize
            && col < self.col_end as isize
            && row >= self.row_start as isize
            && row < self.row_end as isize
    }

    fn sb_size(&self) -> usize {
        if self.f.seq.use_128x128_superblock {
            BLOCK_128X128
        } else {
            BLOCK_64X64
        }
    }

    /// `decode_tile()`; returns the tile's final CDFs.
    fn decode(mut self) -> Result<Box<Cdfs>, Error> {
        let sb_size = self.sb_size();
        let sb4 = usize::from(NUM_4X4_BLOCKS_WIDE[sb_size]);
        for r in (self.row_start..self.row_end).step_by(sb4) {
            for p in 0..3 {
                self.left_level[p].fill(0);
                self.left_dc[p].fill(0);
            }
            self.left_seg_pred.fill(0);
            for c in (self.col_start..self.col_end).step_by(sb4) {
                self.read_deltas = self.f.fh.delta_q_present;
                self.clear_cdef(r, c);
                self.clear_block_decoded_flags(r, c, sb4);
                self.read_lr(r, c, sb_size)?;
                self.decode_partition(r, c, sb_size)?;
            }
        }
        self.sd.finish()?;
        Ok(self.cdf)
    }

    fn clear_cdef(&mut self, r: usize, c: usize) {
        let stride = self.f.mi_stride / 16;
        let n = if self.f.seq.use_128x128_superblock {
            2
        } else {
            1
        };
        for i in 0..n {
            for j in 0..n {
                self.f.cdef_idx[((r >> 4) + i) * stride + (c >> 4) + j] = -1;
            }
        }
    }

    fn clear_block_decoded_flags(&mut self, r: usize, c: usize, sb4: usize) {
        for plane in 0..self.f.num_planes {
            let (sx, sy) = if plane > 0 {
                (self.f.sub_x, self.f.sub_y)
            } else {
                (0, 0)
            };
            let sb_w4 = ((self.col_end - c) >> sx) as isize;
            let sb_h4 = ((self.row_end - r) >> sy) as isize;
            let flags = &mut self.block_decoded[plane];
            for y in -1..=(sb4 >> sy) as isize {
                for x in -1..=(sb4 >> sx) as isize {
                    flags[(y + 1) as usize][(x + 1) as usize] =
                        (y < 0 && x < sb_w4) || (x < 0 && y < sb_h4);
                }
            }
            flags[(sb4 >> sy) + 1][0] = false;
        }
    }

    /// `read_lr()`: loop restoration coefficients.
    fn read_lr(&mut self, r: usize, c: usize, bsize: usize) -> Result<(), Error> {
        let fh = self.f.fh;
        if fh.allow_intrabc {
            return Ok(());
        }
        let w = usize::from(NUM_4X4_BLOCKS_WIDE[bsize]);
        let h = usize::from(NUM_4X4_BLOCKS_HIGH[bsize]);
        for plane in 0..self.f.num_planes {
            let frame_type = fh.restoration.types[plane];
            if frame_type == RestorationType::None {
                continue;
            }
            let (sx, sy) = self.f.sub(plane);
            let unit = usize::from(fh.restoration.unit_size[plane]);
            let (unit_rows, unit_cols) = self.f.lr_unit_counts(plane);
            let row_start = (r * (4 >> sy)).div_ceil(unit);
            let row_end = unit_rows.min(((r + h) * (4 >> sy)).div_ceil(unit));
            let col_start = (c * (4 >> sx)).div_ceil(unit);
            let col_end = unit_cols.min(((c + w) * (4 >> sx)).div_ceil(unit));
            for unit_row in row_start..row_end {
                for unit_col in col_start..col_end {
                    let u = self.read_lr_unit(plane, frame_type)?;
                    self.f.lr_units[plane][unit_row * unit_cols + unit_col] = u;
                }
            }
        }
        Ok(())
    }

    fn read_lr_unit(&mut self, plane: usize, frame_type: RestorationType) -> Result<LrUnit, Error> {
        let kind = match frame_type {
            RestorationType::Wiener => {
                if self.sym(|c| &mut c.use_wiener)? == 1 {
                    1
                } else {
                    0
                }
            }
            RestorationType::Sgrproj => {
                if self.sym(|c| &mut c.use_sgrproj)? == 1 {
                    2
                } else {
                    0
                }
            }
            _ => self.sym(|c| &mut c.restoration_type)?,
        };
        let mut unit = LrUnit {
            kind: kind as u8,
            ..LrUnit::default()
        };
        if kind == 1 {
            const MIN: [i32; 3] = [-5, -23, -17];
            const MAX: [i32; 3] = [10, 8, 46];
            const K: [u32; 3] = [1, 2, 3];
            for pass in 0..2 {
                for j in usize::from(plane > 0)..3 {
                    let r = self.ref_lr_wiener[plane][pass][j];
                    let v = self.signed_subexp(MIN[j], MAX[j] + 1, K[j], r)?;
                    self.ref_lr_wiener[plane][pass][j] = v;
                    unit.wiener[pass][j] = v;
                }
            }
        } else if kind == 2 {
            const MIN: [i32; 2] = [-96, -32];
            const MAX: [i32; 2] = [31, 95];
            let set = self.lit(4)? as usize;
            unit.sgr_set = set as u8;
            for i in 0..2 {
                let v = if SGR_PARAMS[set][i * 2] != 0 {
                    self.signed_subexp(MIN[i], MAX[i] + 1, 4, self.ref_sgr_xqd[plane][i])?
                } else if i == 1 {
                    ((1 << 7) - self.ref_sgr_xqd[plane][0]).clamp(MIN[i], MAX[i])
                } else {
                    0
                };
                self.ref_sgr_xqd[plane][i] = v;
                unit.sgr_xqd[i] = v;
            }
        }
        Ok(unit)
    }

    /// `decode_signed_subexp_with_ref_bool()`.
    fn signed_subexp(&mut self, low: i32, high: i32, k: u32, r: i32) -> Result<i32, Error> {
        let mx = high - low;
        let r = r - low;
        // decode_subexp_bool()
        let (mut i, mut mk) = (0u32, 0i32);
        let v = loop {
            let b2 = if i > 0 { k + i - 1 } else { k };
            let a = 1 << b2;
            if mx <= mk + 3 * a {
                break self.read_ns((mx - mk) as u32)? as i32 + mk;
            } else if self.lit(1)? == 1 {
                i += 1;
                mk += a;
            } else {
                break self.lit(b2)? as i32 + mk;
            }
        };
        let x = if (r << 1) <= mx {
            inverse_recenter(r, v)
        } else {
            mx - 1 - inverse_recenter(mx - 1 - r, v)
        };
        Ok(x + low)
    }

    /// `NS(n)`: a value below `n`, its bits read as literals.
    fn read_ns(&mut self, n: u32) -> Result<u32, Error> {
        let w = n.ilog2() + 1;
        let m = (1 << w) - n;
        let v = self.lit(w - 1)?;
        if v < m {
            return Ok(v);
        }
        Ok((v << 1) - m + self.lit(1)?)
    }

    fn decode_partition(&mut self, r: usize, c: usize, bsize: usize) -> Result<(), Error> {
        if r >= self.f.mi_rows || c >= self.f.mi_cols {
            return Ok(());
        }
        let avail_u = self.is_inside(r as isize - 1, c as isize);
        let avail_l = self.is_inside(r as isize, c as isize - 1);
        let num4x4 = usize::from(NUM_4X4_BLOCKS_WIDE[bsize]);
        let half = num4x4 >> 1;
        let quarter = half >> 1;
        let has_rows = r + half < self.f.mi_rows;
        let has_cols = c + half < self.f.mi_cols;
        let partition = if bsize < BLOCK_8X8 {
            PARTITION_NONE
        } else {
            let bsl = MI_WIDTH_LOG2[bsize];
            let above =
                avail_u && MI_WIDTH_LOG2[usize::from(self.f.mi_sizes[self.f.mi(r - 1, c)])] < bsl;
            let left =
                avail_l && MI_HEIGHT_LOG2[usize::from(self.f.mi_sizes[self.f.mi(r, c - 1)])] < bsl;
            let ctx = usize::from(left) * 2 + usize::from(above);
            if has_rows && has_cols {
                self.sym(|cdf| partition_cdf(cdf, bsl, ctx))?
            } else if has_cols || has_rows {
                let p = partition_cdf(&mut self.cdf, bsl, ctx);
                let prob = |i: usize| i32::from(p[i]) - i32::from(p[i - 1]);
                let mut psum = if has_cols {
                    // split_or_horz
                    prob(PARTITION_VERT)
                        + prob(PARTITION_SPLIT)
                        + prob(PARTITION_HORZ_A)
                        + prob(PARTITION_VERT_A)
                        + prob(PARTITION_VERT_B)
                } else {
                    // split_or_vert
                    prob(PARTITION_HORZ)
                        + prob(PARTITION_SPLIT)
                        + prob(PARTITION_HORZ_A)
                        + prob(PARTITION_HORZ_B)
                        + prob(PARTITION_VERT_A)
                };
                if bsize != BLOCK_128X128 {
                    psum += prob(if has_cols {
                        PARTITION_VERT_4
                    } else {
                        PARTITION_HORZ_4
                    });
                }
                let mut cdf = [((1 << 15) - psum) as u16, 1 << 15, 0];
                let split = self.sd.read_symbol(&mut cdf)? == 1;
                match (split, has_cols) {
                    (true, _) => PARTITION_SPLIT,
                    (false, true) => PARTITION_HORZ,
                    (false, false) => PARTITION_VERT,
                }
            } else {
                PARTITION_SPLIT
            }
        };
        let sub = usize::from(PARTITION_SUBSIZE[partition][bsize]);
        let split = usize::from(PARTITION_SUBSIZE[PARTITION_SPLIT][bsize]);
        match partition {
            PARTITION_NONE => self.decode_block(r, c, sub)?,
            PARTITION_HORZ => {
                self.decode_block(r, c, sub)?;
                if has_rows {
                    self.decode_block(r + half, c, sub)?;
                }
            }
            PARTITION_VERT => {
                self.decode_block(r, c, sub)?;
                if has_cols {
                    self.decode_block(r, c + half, sub)?;
                }
            }
            PARTITION_SPLIT => {
                self.decode_partition(r, c, sub)?;
                self.decode_partition(r, c + half, sub)?;
                self.decode_partition(r + half, c, sub)?;
                self.decode_partition(r + half, c + half, sub)?;
            }
            PARTITION_HORZ_A => {
                self.decode_block(r, c, split)?;
                self.decode_block(r, c + half, split)?;
                self.decode_block(r + half, c, sub)?;
            }
            PARTITION_HORZ_B => {
                self.decode_block(r, c, sub)?;
                self.decode_block(r + half, c, split)?;
                self.decode_block(r + half, c + half, split)?;
            }
            PARTITION_VERT_A => {
                self.decode_block(r, c, split)?;
                self.decode_block(r + half, c, split)?;
                self.decode_block(r, c + half, sub)?;
            }
            PARTITION_VERT_B => {
                self.decode_block(r, c, sub)?;
                self.decode_block(r, c + half, split)?;
                self.decode_block(r + half, c + half, split)?;
            }
            PARTITION_HORZ_4 => {
                for i in 0..4 {
                    if i < 3 || r + quarter * 3 < self.f.mi_rows {
                        self.decode_block(r + quarter * i, c, sub)?;
                    }
                }
            }
            _ => {
                for i in 0..4 {
                    if i < 3 || c + quarter * 3 < self.f.mi_cols {
                        self.decode_block(r, c + quarter * i, sub)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn decode_block(&mut self, r: usize, c: usize, size: usize) -> Result<(), Error> {
        let (sx, sy) = (self.f.sub_x, self.f.sub_y);
        let bw4 = usize::from(NUM_4X4_BLOCKS_WIDE[size]);
        let bh4 = usize::from(NUM_4X4_BLOCKS_HIGH[size]);
        if self.f.num_planes > 1 && SUBSAMPLED_SIZE[size][sx][sy] == BLOCK_INVALID {
            return Err(Error::Invalid(
                "block size not allowed by the chroma subsampling",
            ));
        }
        let has_chroma = !((bh4 == 1 && sy == 1 && r & 1 == 0)
            || (bw4 == 1 && sx == 1 && c & 1 == 0))
            && self.f.num_planes > 1;
        let avail_u = self.is_inside(r as isize - 1, c as isize);
        let avail_l = self.is_inside(r as isize, c as isize - 1);
        let (mut avail_u_chroma, mut avail_l_chroma) = (avail_u, avail_l);
        if has_chroma {
            if sy == 1 && bh4 == 1 {
                avail_u_chroma = self.is_inside(r as isize - 2, c as isize);
            }
            if sx == 1 && bw4 == 1 {
                avail_l_chroma = self.is_inside(r as isize, c as isize - 2);
            }
        } else {
            (avail_u_chroma, avail_l_chroma) = (false, false);
        }
        self.b = Block {
            row: r,
            col: c,
            size,
            bw4,
            bh4,
            has_chroma,
            avail_u,
            avail_l,
            avail_u_chroma,
            avail_l_chroma,
            ..Block::default()
        };
        let intra_frame = self.f.fh.is_intra();
        if intra_frame {
            self.intra_frame_mode_info()?;
        } else {
            self.inter_frame_mode_info()?;
        }
        self.palette_tokens()?;
        self.read_block_tx_size()?;
        let b = &self.b;
        for row in r..r + bh4 {
            for col in c..c + bw4 {
                let i = self.f.mi(row, col);
                self.f.y_modes[i] = b.y_mode;
                if b.ref_frame[0] == INTRA_FRAME && has_chroma {
                    self.f.uv_modes[i] = b.uv_mode;
                }
                self.f.ref_frames[i] = b.ref_frame;
                if b.is_inter {
                    self.f.comp_group_idxs[i] = b.comp_group_idx;
                    self.f.compound_idxs[i] = b.compound_idx;
                    self.f.interp_filters[i] = b.interp_filter;
                    self.f.mvs[i] = b.mv;
                }
            }
        }
        if b.skip {
            self.reset_block_context();
        }
        if self.b.is_inter {
            if intra_frame {
                self.predict_intrabc();
            } else {
                self.predict_inter_block()?;
            }
        }
        self.residual()?;
        let b = &self.b;
        for row in r..r + bh4 {
            for col in c..c + bw4 {
                let i = self.f.mi(row, col);
                self.f.is_inters[i] = b.is_inter;
                self.f.skip_modes[i] = b.skip_mode;
                self.f.skips[i] = b.skip;
                self.f.mi_sizes[i] = size as u8;
                self.f.segment_ids[i] = b.segment_id as u8;
                self.f.palette_sizes[0][i] = b.palette_size_y as u8;
                self.f.palette_sizes[1][i] = b.palette_size_uv as u8;
                self.f.palette_colors[0][i] = b.palette_y;
                self.f.palette_colors[1][i] = b.palette_u;
                self.f.delta_lfs[i] = self.delta_lf.map(|d| d as i8);
            }
        }
        Ok(())
    }

    fn reset_block_context(&mut self) {
        let b = &self.b;
        for plane in 0..1 + 2 * usize::from(b.has_chroma) {
            let (sx, sy) = if plane > 0 {
                (self.f.sub_x, self.f.sub_y)
            } else {
                (0, 0)
            };
            for i in b.col >> sx..(b.col + b.bw4) >> sx {
                self.above_level[plane][i] = 0;
                self.above_dc[plane][i] = 0;
            }
            for i in b.row >> sy..(b.row + b.bh4) >> sy {
                self.left_level[plane][i] = 0;
                self.left_dc[plane][i] = 0;
            }
        }
    }

    fn seg_feature_active(&self, segment_id: usize, feature: usize) -> bool {
        let seg = &self.f.fh.segmentation;
        seg.enabled && seg.feature_enabled[segment_id][feature]
    }

    fn intra_frame_mode_info(&mut self) -> Result<(), Error> {
        self.b.ref_frame = [INTRA_FRAME, NONE];
        let pre_skip = self.f.fh.segmentation.seg_id_pre_skip;
        if pre_skip {
            self.intra_segment_id()?;
        }
        self.read_skip()?;
        if !pre_skip {
            self.intra_segment_id()?;
        }
        self.read_cdef()?;
        self.read_delta_qindex()?;
        self.read_delta_lf()?;
        self.read_deltas = false;
        if self.f.fh.allow_intrabc && self.sym(|c| &mut c.intrabc)? == 1 {
            // An inter block predicted from the frame itself, with YMode and UVMode DC_PRED.
            self.b.is_inter = true;
            self.b.interp_filter = [BILINEAR; 2];
            self.b.mv[0] = self.intrabc_mv()?;
            return Ok(());
        }

        let (row, col) = (self.b.row, self.b.col);
        let above = if self.b.avail_u {
            self.f.y_modes[self.f.mi(row - 1, col)]
        } else {
            DC_PRED
        };
        let left = if self.b.avail_l {
            self.f.y_modes[self.f.mi(row, col - 1)]
        } else {
            DC_PRED
        };
        let above = usize::from(INTRA_MODE_CONTEXT[usize::from(above)]);
        let left = usize::from(INTRA_MODE_CONTEXT[usize::from(left)]);
        let y_mode = self.sym(|c| &mut c.intra_frame_y_mode[above][left])? as u8;
        self.intra_modes(y_mode)
    }

    /// `read_skip()`.
    fn read_skip(&mut self) -> Result<(), Error> {
        let pre_skip = self.f.fh.segmentation.seg_id_pre_skip;
        self.b.skip = if pre_skip && self.seg_feature_active(self.b.segment_id, SEG_LVL_SKIP) {
            true
        } else {
            let mut ctx = 0;
            if self.b.avail_u {
                ctx += usize::from(self.f.skips[self.f.mi(self.b.row - 1, self.b.col)]);
            }
            if self.b.avail_l {
                ctx += usize::from(self.f.skips[self.f.mi(self.b.row, self.b.col - 1)]);
            }
            self.sym(|c| &mut c.skip[ctx])? == 1
        };
        Ok(())
    }

    /// The modes of an intra block after `y_mode`: angles, chroma mode, palette and filter.
    fn intra_modes(&mut self, y_mode: u8) -> Result<(), Error> {
        self.b.y_mode = y_mode;
        self.b.angle_delta_y = self.angle_info(self.b.y_mode)?;
        let size = self.b.size;
        let bw = 4 * usize::from(NUM_4X4_BLOCKS_WIDE[size]);
        let bh = 4 * usize::from(NUM_4X4_BLOCKS_HIGH[size]);
        if self.b.has_chroma {
            let cfl_allowed = if self.b.lossless {
                SUBSAMPLED_SIZE[size][self.f.sub_x][self.f.sub_y] == BLOCK_4X4 as u8
            } else {
                bw.max(bh) <= 32
            };
            let y_mode = usize::from(self.b.y_mode);
            self.b.uv_mode = if cfl_allowed {
                self.sym(|c| &mut c.uv_mode_cfl_allowed[y_mode])?
            } else {
                self.sym(|c| &mut c.uv_mode_cfl_not_allowed[y_mode])?
            } as u8;
            if self.b.uv_mode == UV_CFL_PRED {
                self.read_cfl_alphas()?;
            }
            self.b.angle_delta_uv = self.angle_info(self.b.uv_mode)?;
        }
        if size >= BLOCK_8X8 && bw <= 64 && bh <= 64 && self.f.fh.allow_screen_content_tools {
            self.palette_mode_info()?;
        }
        // filter_intra_mode_info()
        if self.f.seq.enable_filter_intra
            && self.b.y_mode == DC_PRED
            && self.b.palette_size_y == 0
            && bw.max(bh) <= 32
            && self.sym(|c| &mut c.filter_intra[size])? == 1
        {
            self.b.filter_intra_mode = Some(self.sym(|c| &mut c.filter_intra_mode)? as u8);
        }
        Ok(())
    }

    fn palette_mode_info(&mut self) -> Result<(), Error> {
        let size = self.b.size;
        let bsize_ctx = usize::from(MI_WIDTH_LOG2[size] + MI_HEIGHT_LOG2[size]) - 2;
        let bd = self.f.bit_depth;
        let (row, col) = (self.b.row, self.b.col);
        if self.b.y_mode == DC_PRED {
            let ctx =
                usize::from(self.b.avail_u && self.f.palette_sizes[0][self.f.mi(row - 1, col)] > 0)
                    + usize::from(
                        self.b.avail_l && self.f.palette_sizes[0][self.f.mi(row, col - 1)] > 0,
                    );
            if self.sym(|c| &mut c.palette_y_mode[bsize_ctx][ctx])? == 1 {
                let n = self.sym(|c| &mut c.palette_y_size[bsize_ctx])? + 2;
                self.b.palette_size_y = n;
                self.b.palette_y = self.palette_colors(0, n, 1)?;
            }
        }
        if self.b.has_chroma && self.b.uv_mode == DC_PRED {
            let ctx = usize::from(self.b.palette_size_y > 0);
            if self.sym(|c| &mut c.palette_uv_mode[ctx])? == 1 {
                let n = self.sym(|c| &mut c.palette_uv_size[bsize_ctx])? + 2;
                self.b.palette_size_uv = n;
                self.b.palette_u = self.palette_colors(1, n, 0)?;
                let max = 1i32 << bd;
                if self.lit(1)? == 1 {
                    // delta_encode_palette_colors_v
                    let bits = bd - 4 + self.lit(2)?;
                    let mut prev = self.lit(bd)? as i32;
                    self.b.palette_v[0] = prev as u16;
                    for i in 1..n {
                        let mut delta = self.lit(bits)? as i32;
                        if delta != 0 && self.lit(1)? == 1 {
                            delta = -delta;
                        }
                        let mut val = prev + delta;
                        if val < 0 {
                            val += max;
                        }
                        if val >= max {
                            val -= max;
                        }
                        prev = val.clamp(0, max - 1);
                        self.b.palette_v[i] = prev as u16;
                    }
                } else {
                    for i in 0..n {
                        self.b.palette_v[i] = self.lit(bd)? as u16;
                    }
                }
            }
        }
        Ok(())
    }

    /// The colors of the Y (`plane` 0, deltas offset by 1) or U (`plane` 1) palette.
    fn palette_colors(&mut self, plane: usize, n: usize, offset: i32) -> Result<[u16; 8], Error> {
        let bd = self.f.bit_depth;
        let cache = self.palette_cache(plane);
        let mut colors = [0u16; 8];
        let mut idx = 0;
        for &c in &cache {
            if idx == n {
                break;
            }
            if self.lit(1)? == 1 {
                colors[idx] = c;
                idx += 1;
            }
        }
        if idx < n {
            colors[idx] = self.lit(bd)? as u16;
            idx += 1;
        }
        if idx < n {
            let mut bits = bd - 3 + self.lit(2)?;
            let max = (1i32 << bd) - 1;
            while idx < n {
                let delta = self.lit(bits)? as i32 + offset;
                let c = (i32::from(colors[idx - 1]) + delta).clamp(0, max);
                colors[idx] = c as u16;
                let range = (1 << bd) - c - offset;
                // CeilLog2( range )
                bits = bits.min(if range < 2 {
                    0
                } else {
                    (range - 1).ilog2() + 1
                });
                idx += 1;
            }
        }
        colors[..n].sort_unstable();
        Ok(colors)
    }

    /// `get_palette_cache()`: the above and left palettes, merged without duplicates.
    fn palette_cache(&self, plane: usize) -> Vec<u16> {
        let (row, col) = (self.b.row, self.b.col);
        let mut colors = Vec::with_capacity(16);
        if (row * 4) % 64 != 0 {
            let i = self.f.mi(row - 1, col);
            colors.extend_from_slice(
                &self.f.palette_colors[plane][i][..usize::from(self.f.palette_sizes[plane][i])],
            );
        }
        if self.b.avail_l {
            let i = self.f.mi(row, col - 1);
            colors.extend_from_slice(
                &self.f.palette_colors[plane][i][..usize::from(self.f.palette_sizes[plane][i])],
            );
        }
        colors.sort_unstable();
        colors.dedup();
        colors
    }

    /// `palette_tokens()`: the color index maps.
    fn palette_tokens(&mut self) -> Result<(), Error> {
        let b = &self.b;
        let mut block_w = 4 * b.bw4;
        let mut block_h = 4 * b.bh4;
        let mut onscreen_w = block_w.min((self.f.mi_cols - b.col) * 4);
        let mut onscreen_h = block_h.min((self.f.mi_rows - b.row) * 4);
        let (n_y, n_uv) = (b.palette_size_y, b.palette_size_uv);
        if n_y > 0 {
            self.color_map(0, n_y, block_w, block_h, onscreen_w, onscreen_h)?;
        }
        if n_uv > 0 {
            let (sx, sy) = (self.f.sub_x, self.f.sub_y);
            (block_w, block_h, onscreen_w, onscreen_h) = (
                block_w >> sx,
                block_h >> sy,
                onscreen_w >> sx,
                onscreen_h >> sy,
            );
            if block_w < 4 {
                block_w += 2;
                onscreen_w += 2;
            }
            if block_h < 4 {
                block_h += 2;
                onscreen_h += 2;
            }
            self.color_map(1, n_uv, block_w, block_h, onscreen_w, onscreen_h)?;
        }
        Ok(())
    }

    fn color_map(
        &mut self,
        map: usize,
        n: usize,
        block_w: usize,
        block_h: usize,
        onscreen_w: usize,
        onscreen_h: usize,
    ) -> Result<(), Error> {
        const STRIDE: usize = 64;
        self.color_maps[map][0] = self.read_ns(n as u32)? as u8;
        for i in 1..onscreen_h + onscreen_w - 1 {
            for j in (i.saturating_sub(onscreen_h - 1)..=i.min(onscreen_w - 1)).rev() {
                let (r, c) = (i - j, j);
                let m = &self.color_maps[map];
                // get_palette_color_context()
                let mut scores = [0u32; 8];
                let mut order: [u8; 8] = [0, 1, 2, 3, 4, 5, 6, 7];
                if c > 0 {
                    scores[usize::from(m[r * STRIDE + c - 1])] += 2;
                }
                if r > 0 && c > 0 {
                    scores[usize::from(m[(r - 1) * STRIDE + c - 1])] += 1;
                }
                if r > 0 {
                    scores[usize::from(m[(r - 1) * STRIDE + c])] += 2;
                }
                for i in 0..3 {
                    let mut max_idx = i;
                    for j in i + 1..n {
                        if scores[j] > scores[max_idx] {
                            max_idx = j;
                        }
                    }
                    scores[i..=max_idx].rotate_right(1);
                    order[i..=max_idx].rotate_right(1);
                }
                let hash: u32 = (0..3)
                    .map(|i| scores[i] * u32::from(PALETTE_COLOR_HASH_MULTIPLIERS[i]))
                    .sum();
                let ctx = usize::try_from(PALETTE_COLOR_CONTEXT[hash as usize])
                    .map_err(|_| Error::Invalid("palette color context"))?;
                let idx = match (map, n) {
                    (0, 2) => self.sym(|c| &mut c.palette_size_2_y_color[ctx])?,
                    (0, 3) => self.sym(|c| &mut c.palette_size_3_y_color[ctx])?,
                    (0, 4) => self.sym(|c| &mut c.palette_size_4_y_color[ctx])?,
                    (0, 5) => self.sym(|c| &mut c.palette_size_5_y_color[ctx])?,
                    (0, 6) => self.sym(|c| &mut c.palette_size_6_y_color[ctx])?,
                    (0, 7) => self.sym(|c| &mut c.palette_size_7_y_color[ctx])?,
                    (0, _) => self.sym(|c| &mut c.palette_size_8_y_color[ctx])?,
                    (_, 2) => self.sym(|c| &mut c.palette_size_2_uv_color[ctx])?,
                    (_, 3) => self.sym(|c| &mut c.palette_size_3_uv_color[ctx])?,
                    (_, 4) => self.sym(|c| &mut c.palette_size_4_uv_color[ctx])?,
                    (_, 5) => self.sym(|c| &mut c.palette_size_5_uv_color[ctx])?,
                    (_, 6) => self.sym(|c| &mut c.palette_size_6_uv_color[ctx])?,
                    (_, 7) => self.sym(|c| &mut c.palette_size_7_uv_color[ctx])?,
                    (_, _) => self.sym(|c| &mut c.palette_size_8_uv_color[ctx])?,
                };
                self.color_maps[map][r * STRIDE + c] = order[idx];
            }
        }
        let m = &mut self.color_maps[map];
        for i in 0..onscreen_h {
            let last = m[i * STRIDE + onscreen_w - 1];
            m[i * STRIDE + onscreen_w..i * STRIDE + block_w].fill(last);
        }
        for i in onscreen_h..block_h {
            m.copy_within(
                (onscreen_h - 1) * STRIDE..(onscreen_h - 1) * STRIDE + block_w,
                i * STRIDE,
            );
        }
        Ok(())
    }

    fn angle_info(&mut self, mode: u8) -> Result<i32, Error> {
        if self.b.size >= BLOCK_8X8 && predict::is_directional(mode) {
            let i = usize::from(mode) - 1;
            Ok(self.sym(|c| &mut c.angle_delta[i])? as i32 - 3)
        } else {
            Ok(0)
        }
    }

    fn read_cfl_alphas(&mut self) -> Result<(), Error> {
        let signs = self.sym(|c| &mut c.cfl_sign)?;
        let (sign_u, sign_v) = ((signs + 1) / 3, (signs + 1) % 3);
        let mut alpha = |sign: usize, ctx: usize| -> Result<i32, Error> {
            if sign == 0 {
                return Ok(0);
            }
            let a = 1 + self.sym(|c| &mut c.cfl_alpha[ctx])? as i32;
            Ok(if sign == 1 { -a } else { a })
        };
        let u = alpha(sign_u, (sign_u.max(1) - 1) * 3 + sign_v)?;
        let v = alpha(sign_v, (sign_v.max(1) - 1) * 3 + sign_u)?;
        (self.b.cfl_alpha_u, self.b.cfl_alpha_v) = (u, v);
        Ok(())
    }

    fn intra_segment_id(&mut self) -> Result<(), Error> {
        if self.f.fh.segmentation.enabled {
            self.read_segment_id()?;
        }
        self.b.lossless = self.f.fh.lossless[self.b.segment_id];
        Ok(())
    }

    /// `read_segment_id()`.
    fn read_segment_id(&mut self) -> Result<(), Error> {
        let seg = &self.f.fh.segmentation;
        {
            let last = i32::from(seg.last_active_seg_id);
            let (row, col) = (self.b.row, self.b.col);
            let get = |s: &Self, r: usize, c: usize| i32::from(s.f.segment_ids[s.f.mi(r, c)]);
            let prev_ul = if self.b.avail_u && self.b.avail_l {
                get(self, row - 1, col - 1)
            } else {
                -1
            };
            let prev_u = if self.b.avail_u {
                get(self, row - 1, col)
            } else {
                -1
            };
            let prev_l = if self.b.avail_l {
                get(self, row, col - 1)
            } else {
                -1
            };
            let pred = if prev_u == -1 {
                prev_l.max(0)
            } else if prev_l == -1 || prev_ul == prev_u {
                prev_u
            } else {
                prev_l
            };
            let id = if self.b.skip {
                pred
            } else {
                let ctx = if prev_ul < 0 {
                    0
                } else if prev_ul == prev_u && prev_ul == prev_l {
                    2
                } else if prev_ul == prev_u || prev_ul == prev_l || prev_u == prev_l {
                    1
                } else {
                    0
                };
                let v = self.sym(|c| &mut c.segment_id[ctx])? as i32;
                neg_deinterleave(v, pred, last + 1)
            };
            if !(0..=last).contains(&id) {
                return Err(Error::Invalid("segment_id"));
            }
            self.b.segment_id = id as usize;
        }
        Ok(())
    }

    fn read_cdef(&mut self) -> Result<(), Error> {
        let fh = self.f.fh;
        if self.b.skip || fh.coded_lossless || !self.f.seq.enable_cdef || fh.allow_intrabc {
            return Ok(());
        }
        let stride = self.f.mi_stride / 16;
        let (r, c) = (self.b.row >> 4, self.b.col >> 4);
        if self.f.cdef_idx[r * stride + c] == -1 {
            let idx = self.lit(u32::from(fh.cdef.bits))? as i8;
            let (w, h) = (self.b.bw4.div_ceil(16), self.b.bh4.div_ceil(16));
            for i in r..r + h {
                for j in c..c + w {
                    self.f.cdef_idx[i * stride + j] = idx;
                }
            }
        }
        Ok(())
    }

    /// Reads `delta_q_abs` or `delta_lf_abs` and their extra bits; 0 when absent.
    fn read_delta(&mut self, cdf: impl FnOnce(&mut Cdfs) -> &mut [u16]) -> Result<i32, Error> {
        let mut abs = self.sym(cdf)? as i32;
        if abs == 3 {
            let n = self.lit(3)? + 1;
            abs = self.lit(n)? as i32 + (1 << n) + 1;
        }
        if abs != 0 && self.lit(1)? == 1 {
            abs = -abs;
        }
        Ok(abs)
    }

    fn read_delta_qindex(&mut self) -> Result<(), Error> {
        if (self.b.size == self.sb_size() && self.b.skip) || !self.read_deltas {
            return Ok(());
        }
        let d = self.read_delta(|c| &mut c.delta_q)?;
        if d != 0 {
            let res = u32::from(self.f.fh.delta_q_res);
            self.current_q_index = (self.current_q_index + (d << res)).clamp(1, 255);
        }
        Ok(())
    }

    fn read_delta_lf(&mut self) -> Result<(), Error> {
        let fh = self.f.fh;
        if (self.b.size == self.sb_size() && self.b.skip)
            || !self.read_deltas
            || !fh.delta_lf_present
        {
            return Ok(());
        }
        let count = match (fh.delta_lf_multi, self.f.num_planes > 1) {
            (false, _) => 1,
            (true, true) => 4,
            (true, false) => 2,
        };
        for i in 0..count {
            let d = if fh.delta_lf_multi {
                self.read_delta(|c| &mut c.delta_lf_multi[i])?
            } else {
                self.read_delta(|c| &mut c.delta_lf)?
            };
            if d != 0 {
                let res = u32::from(fh.delta_lf_res);
                self.delta_lf[i] = (self.delta_lf[i] + (d << res)).clamp(-63, 63);
            }
        }
        Ok(())
    }

    /// `read_block_tx_size()`: `TxSize`, and `InterTxSizes` over the block.
    fn read_block_tx_size(&mut self) -> Result<(), Error> {
        let b = &self.b;
        let (size, row, col, bw4, bh4) = (b.size, b.row, b.col, b.bw4, b.bh4);
        if self.f.fh.tx_mode == TxMode::Select
            && size > BLOCK_4X4
            && b.is_inter
            && !b.skip
            && !b.lossless
        {
            let max = usize::from(MAX_TX_SIZE_RECT[size]);
            let (w4, h4) = (
                usize::from(TX_WIDTH[max]) >> 2,
                usize::from(TX_HEIGHT[max]) >> 2,
            );
            for r in (row..row + bh4).step_by(h4) {
                for c in (col..col + bw4).step_by(w4) {
                    self.read_var_tx_size(r, c, max, 0)?;
                }
            }
            return Ok(());
        }
        let allow_select = !b.skip || !b.is_inter;
        self.read_tx_size(allow_select)?;
        let tx = self.b.tx_size as u8;
        for r in row..row + self.b.bh4 {
            let i = self.f.mi(r, col);
            self.f.tx_sizes[i..i + self.b.bw4].fill(tx);
        }
        Ok(())
    }

    /// `read_var_tx_size()`: a transform size tree of an inter block.
    fn read_var_tx_size(
        &mut self,
        row: usize,
        col: usize,
        tx: usize,
        depth: usize,
    ) -> Result<(), Error> {
        if row >= self.f.mi_rows || col >= self.f.mi_cols {
            return Ok(());
        }
        let split = if tx == TX_4X4 || depth == MAX_VARTX_DEPTH {
            false
        } else {
            let above = self.above_tx_width(row, col) < usize::from(TX_WIDTH[tx]);
            let left = self.left_tx_height(row, col) < usize::from(TX_HEIGHT[tx]);
            let size = self.b.size;
            let bw = 4 * usize::from(NUM_4X4_BLOCKS_WIDE[size]);
            let bh = 4 * usize::from(NUM_4X4_BLOCKS_HIGH[size]);
            // find_tx_size() of a square: TX_4X4 to TX_64X64 are 0 to 4.
            let max_sq = (bw.max(bh).min(64).ilog2() - 2) as usize;
            let ctx = usize::from(usize::from(TX_SIZE_SQR_UP[tx]) != max_sq) * 3
                + (4 - max_sq) * 6
                + usize::from(above)
                + usize::from(left);
            self.sym(|c| &mut c.txfm_split[ctx])? == 1
        };
        let (w4, h4) = (
            usize::from(TX_WIDTH[tx]) >> 2,
            usize::from(TX_HEIGHT[tx]) >> 2,
        );
        if split {
            let sub = usize::from(SPLIT_TX_SIZE[tx]);
            let (step_w, step_h) = (
                usize::from(TX_WIDTH[sub]) >> 2,
                usize::from(TX_HEIGHT[sub]) >> 2,
            );
            for i in (0..h4).step_by(step_h) {
                for j in (0..w4).step_by(step_w) {
                    self.read_var_tx_size(row + i, col + j, sub, depth + 1)?;
                }
            }
        } else {
            // Units past the frame edge are written too: the arrays cover whole superblocks.
            for i in 0..h4 {
                let start = self.f.mi(row + i, col);
                self.f.tx_sizes[start..start + w4].fill(tx as u8);
            }
            self.b.tx_size = tx;
        }
        Ok(())
    }

    /// `get_above_tx_width()`.
    fn above_tx_width(&self, row: usize, col: usize) -> usize {
        if row == self.b.row {
            if !self.b.avail_u {
                return 64;
            }
            let i = self.f.mi(row - 1, col);
            if self.f.skips[i] && self.f.is_inters[i] {
                return 4 * usize::from(NUM_4X4_BLOCKS_WIDE[usize::from(self.f.mi_sizes[i])]);
            }
        }
        usize::from(TX_WIDTH[usize::from(self.f.tx_sizes[self.f.mi(row - 1, col)])])
    }

    /// `get_left_tx_height()`.
    fn left_tx_height(&self, row: usize, col: usize) -> usize {
        if col == self.b.col {
            if !self.b.avail_l {
                return 64;
            }
            let i = self.f.mi(row, col - 1);
            if self.f.skips[i] && self.f.is_inters[i] {
                return 4 * usize::from(NUM_4X4_BLOCKS_HIGH[usize::from(self.f.mi_sizes[i])]);
            }
        }
        usize::from(TX_HEIGHT[usize::from(self.f.tx_sizes[self.f.mi(row, col - 1)])])
    }

    /// `read_tx_size( allowSelect )`.
    fn read_tx_size(&mut self, allow_select: bool) -> Result<(), Error> {
        let size = self.b.size;
        if self.b.lossless {
            self.b.tx_size = TX_4X4;
            return Ok(());
        }
        let max_rect = usize::from(MAX_TX_SIZE_RECT[size]);
        let max_depth = MAX_TX_DEPTH[size];
        self.b.tx_size = max_rect;
        if size > BLOCK_4X4 && allow_select && self.f.fh.tx_mode == TxMode::Select {
            let (row, col) = (self.b.row, self.b.col);
            let block = |i: usize| self.f.mi_sizes[i] as usize;
            let above_w = if self.b.avail_u {
                let i = self.f.mi(row - 1, col);
                if self.f.is_inters[i] {
                    4 * NUM_4X4_BLOCKS_WIDE[block(i)]
                } else {
                    self.above_tx_width(row, col) as u8
                }
            } else {
                0
            };
            let left_h = if self.b.avail_l {
                let i = self.f.mi(row, col - 1);
                if self.f.is_inters[i] {
                    4 * NUM_4X4_BLOCKS_HIGH[block(i)]
                } else {
                    self.left_tx_height(row, col) as u8
                }
            } else {
                0
            };
            let ctx = usize::from(above_w >= TX_WIDTH[max_rect])
                + usize::from(left_h >= TX_HEIGHT[max_rect]);
            let depth = match max_depth {
                4 => self.sym(|c| &mut c.tx_64x64[ctx])?,
                3 => self.sym(|c| &mut c.tx_32x32[ctx])?,
                2 => self.sym(|c| &mut c.tx_16x16[ctx])?,
                _ => self.sym(|c| &mut c.tx_8x8[ctx])?,
            };
            for _ in 0..depth {
                self.b.tx_size = usize::from(SPLIT_TX_SIZE[self.b.tx_size]);
            }
        }
        Ok(())
    }

    fn plane_sub(&self, plane: usize) -> (usize, usize) {
        self.f.sub(plane)
    }

    /// `get_plane_residual_size()`.
    fn residual_size(&self, size: usize, plane: usize) -> usize {
        let (sx, sy) = self.plane_sub(plane);
        usize::from(SUBSAMPLED_SIZE[size][sx][sy])
    }

    /// `get_tx_size()`.
    fn plane_tx_size(&self, plane: usize) -> usize {
        if plane == 0 {
            return self.b.tx_size;
        }
        let uv = usize::from(MAX_TX_SIZE_RECT[self.residual_size(self.b.size, plane)]);
        if TX_WIDTH[uv] == 64 || TX_HEIGHT[uv] == 64 {
            if TX_WIDTH[uv] == 16 {
                return TX_16X32;
            }
            if TX_HEIGHT[uv] == 16 {
                return TX_32X16;
            }
            return TX_32X32;
        }
        uv
    }

    fn residual(&mut self) -> Result<(), Error> {
        let size = self.b.size;
        let bw = 4 * self.b.bw4;
        let bh = 4 * self.b.bh4;
        let width_chunks = (bw >> 6).max(1);
        let height_chunks = (bh >> 6).max(1);
        let size_chunk = if width_chunks > 1 || height_chunks > 1 {
            BLOCK_64X64
        } else {
            size
        };
        for chunk_y in 0..height_chunks {
            for chunk_x in 0..width_chunks {
                for plane in 0..1 + 2 * usize::from(self.b.has_chroma) {
                    let tx = if self.b.lossless {
                        TX_4X4
                    } else {
                        self.plane_tx_size(plane)
                    };
                    let step_x = usize::from(TX_WIDTH[tx]) >> 2;
                    let step_y = usize::from(TX_HEIGHT[tx]) >> 2;
                    let plane_size = self.residual_size(size_chunk, plane);
                    let num4x4_w = usize::from(NUM_4X4_BLOCKS_WIDE[plane_size]);
                    let num4x4_h = usize::from(NUM_4X4_BLOCKS_HIGH[plane_size]);
                    let (sx, sy) = self.plane_sub(plane);
                    if self.b.is_inter && !self.b.lossless && plane == 0 {
                        let x = (self.b.col + (chunk_x << 4)) * 4;
                        let y = (self.b.row + (chunk_y << 4)) * 4;
                        self.transform_tree(x, y, num4x4_w * 4, num4x4_h * 4)?;
                        continue;
                    }
                    let base_x = (self.b.col >> sx) * 4;
                    let base_y = (self.b.row >> sy) * 4;
                    for y in (0..num4x4_h).step_by(step_y) {
                        for x in (0..num4x4_w).step_by(step_x) {
                            self.transform_block(
                                plane,
                                base_x,
                                base_y,
                                tx,
                                x + ((chunk_x << 4) >> sx),
                                y + ((chunk_y << 4) >> sy),
                            )?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// `transform_tree()`: the luma transform blocks of an inter block, per `InterTxSizes`.
    fn transform_tree(&mut self, x: usize, y: usize, w: usize, h: usize) -> Result<(), Error> {
        if x >= self.f.mi_cols * 4 || y >= self.f.mi_rows * 4 {
            return Ok(());
        }
        let luma = usize::from(self.f.tx_sizes[self.f.mi(y >> 2, x >> 2)]);
        if w <= usize::from(TX_WIDTH[luma]) && h <= usize::from(TX_HEIGHT[luma]) {
            // find_tx_size()
            let tx = (0..TX_WIDTH.len())
                .find(|&t| usize::from(TX_WIDTH[t]) == w && usize::from(TX_HEIGHT[t]) == h)
                .ok_or(Error::Invalid("transform tree size"))?;
            return self.transform_block(0, x, y, tx, 0, 0);
        }
        if w > h {
            self.transform_tree(x, y, w / 2, h)?;
            self.transform_tree(x + w / 2, y, w / 2, h)
        } else if w < h {
            self.transform_tree(x, y, w, h / 2)?;
            self.transform_tree(x, y + h / 2, w, h / 2)
        } else {
            self.transform_tree(x, y, w / 2, h / 2)?;
            self.transform_tree(x + w / 2, y, w / 2, h / 2)?;
            self.transform_tree(x, y + h / 2, w / 2, h / 2)?;
            self.transform_tree(x + w / 2, y + h / 2, w / 2, h / 2)
        }
    }

    /// The prediction of an intra block copy block (`compute_prediction()`): every plane copies
    /// the block's own motion vector, since all its neighbours are intra blocks.
    fn predict_intrabc(&mut self) {
        let b = &self.b;
        let bd = self.f.bit_depth;
        // Rounding variables of single prediction.
        let rounds = if bd == 12 { [5, 9] } else { [3, 11] };
        let max = (1 << bd) - 1;
        for plane in 0..1 + 2 * usize::from(b.has_chroma) {
            let plane_size = self.residual_size(b.size, plane);
            let w = 4 * usize::from(NUM_4X4_BLOCKS_WIDE[plane_size]);
            let h = 4 * usize::from(NUM_4X4_BLOCKS_HIGH[plane_size]);
            let (sx, sy) = self.plane_sub(plane);
            let (x, y) = ((b.col >> sx) * 4, (b.row >> sy) * 4);
            // The motion vector scaling process with the current frame as reference: no
            // scaling, positions in 1/1024 samples.
            let pos = |base: usize, mv: i32, sub: usize| {
                ((((base as i32) << 4) + ((2 * mv) >> sub)) << 6) + 32
            };
            // Clamped to the decoded area, which covers whole 4×4 units.
            let last_x = (((self.f.mi_cols * 4) + sx) >> sx) as i32 - 1;
            let last_y = (((self.f.mi_rows * 4) + sy) >> sy) as i32 - 1;
            let src = &self.f.planes[plane];
            let pred = predict::block_inter(
                &src.data,
                src.stride,
                last_x,
                last_y,
                pos(x, b.mv[0][1], sx),
                pos(y, b.mv[0][0], sy),
                1 << 10,
                1 << 10,
                w,
                h,
                [usize::from(BILINEAR); 2],
                rounds,
            );
            let out = &mut self.f.planes[plane];
            for i in 0..h {
                for j in 0..w {
                    out.set(x + j, y + i, pred[i * w + j].clamp(0, max));
                }
            }
        }
    }

    fn filter_type(&self, plane: usize) -> bool {
        let b = &self.b;
        let (sx, sy) = (self.f.sub_x == 1, self.f.sub_y == 1);
        let smooth = |r: usize, c: usize| {
            let i = self.f.mi(r, c);
            if plane == 0 {
                is_smooth(self.f.y_modes[i])
            } else {
                self.f.ref_frames[i][0] <= INTRA_FRAME && is_smooth(self.f.uv_modes[i])
            }
        };
        let mut above = false;
        if if plane == 0 {
            b.avail_u
        } else {
            b.avail_u_chroma
        } {
            let (mut r, mut c) = (b.row - 1, b.col);
            if plane > 0 {
                if sx && b.col & 1 == 0 {
                    c += 1;
                }
                if sy && b.row & 1 == 1 {
                    r -= 1;
                }
            }
            above = smooth(r, c);
        }
        let mut left = false;
        if if plane == 0 {
            b.avail_l
        } else {
            b.avail_l_chroma
        } {
            let (mut r, mut c) = (b.row, b.col - 1);
            if plane > 0 {
                if sx && b.col & 1 == 1 {
                    c -= 1;
                }
                if sy && b.row & 1 == 0 {
                    r += 1;
                }
            }
            left = smooth(r, c);
        }
        above || left
    }

    fn transform_block(
        &mut self,
        plane: usize,
        base_x: usize,
        base_y: usize,
        tx: usize,
        x: usize,
        y: usize,
    ) -> Result<(), Error> {
        let start_x = base_x + 4 * x;
        let start_y = base_y + 4 * y;
        let (sx, sy) = self.plane_sub(plane);
        let row = (start_y << sy) >> 2;
        let col = (start_x << sx) >> 2;
        let sb_mask = if self.f.seq.use_128x128_superblock {
            31
        } else {
            15
        };
        let (sub_row, sub_col) = ((row & sb_mask) >> sy, (col & sb_mask) >> sx);
        let step_x = usize::from(TX_WIDTH[tx]) >> 2;
        let step_y = usize::from(TX_HEIGHT[tx]) >> 2;
        let max_x = (self.f.mi_cols * 4) >> sx;
        let max_y = (self.f.mi_rows * 4) >> sy;
        if start_x >= max_x || start_y >= max_y {
            return Ok(());
        }
        let b = &self.b;
        let palette = if plane == 0 {
            b.palette_size_y
        } else {
            b.palette_size_uv
        };
        if b.is_inter {
            // Predicted with the whole block, before its residual.
        } else if palette > 0 {
            // The palette prediction process.
            let colors = match plane {
                0 => &b.palette_y,
                1 => &b.palette_u,
                _ => &b.palette_v,
            };
            let map = &self.color_maps[usize::from(plane > 0)];
            let (w, h) = (usize::from(TX_WIDTH[tx]), usize::from(TX_HEIGHT[tx]));
            let out = &mut self.f.planes[plane];
            for i in 0..h {
                for j in 0..w {
                    let index = map[(y * 4 + i) * 64 + x * 4 + j];
                    out.set(
                        start_x + j,
                        start_y + i,
                        i32::from(colors[usize::from(index)]),
                    );
                }
            }
        } else {
            self.predict(plane, start_x, start_y, tx, x, y, max_x, max_y);
        }
        if plane == 0 {
            self.b.max_luma_w = start_x + step_x * 4;
            self.b.max_luma_h = start_y + step_y * 4;
        }
        if !self.b.skip {
            let (eob, tx_type) = self.coeffs(plane, start_x, start_y, tx)?;
            if eob > 0 {
                self.reconstruct(plane, start_x, start_y, tx, tx_type);
            }
        }
        for row in &mut self.block_decoded[plane][sub_row + 1..sub_row + 1 + step_y] {
            row[sub_col + 1..sub_col + 1 + step_x].fill(true);
        }
        let stride = self.f.mi_stride;
        for i in 0..step_y {
            let start = ((row >> sy) + i) * stride + (col >> sx);
            self.f.lf_tx_sizes[plane][start..start + step_x].fill(tx as u8);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn predict(
        &mut self,
        plane: usize,
        start_x: usize,
        start_y: usize,
        tx: usize,
        x: usize,
        y: usize,
        max_x: usize,
        max_y: usize,
    ) {
        let b = &self.b;
        let is_cfl = plane > 0 && b.uv_mode == UV_CFL_PRED;
        let mode = match plane {
            0 => b.y_mode,
            _ if is_cfl => DC_PRED,
            _ => b.uv_mode,
        };
        let (sx, sy) = self.plane_sub(plane);
        let sb_mask = if self.f.seq.use_128x128_superblock {
            31
        } else {
            15
        };
        let (sub_row, sub_col) = (
            (((start_y << sy) >> 2) & sb_mask) >> sy,
            (((start_x << sx) >> 2) & sb_mask) >> sx,
        );
        let step_x = usize::from(TX_WIDTH[tx]) >> 2;
        let step_y = usize::from(TX_HEIGHT[tx]) >> 2;
        let flags = &self.block_decoded[plane];
        let intra = IntraBlock {
            x: start_x,
            y: start_y,
            log2w: u32::from(TX_WIDTH_LOG2[tx]),
            log2h: u32::from(TX_HEIGHT_LOG2[tx]),
            have_left: (if plane == 0 {
                b.avail_l
            } else {
                b.avail_l_chroma
            }) || x > 0,
            have_above: (if plane == 0 {
                b.avail_u
            } else {
                b.avail_u_chroma
            }) || y > 0,
            have_above_right: flags[sub_row][sub_col + step_x + 1],
            have_below_left: flags[sub_row + step_y + 1][sub_col],
            mode,
            angle_delta: if plane == 0 {
                b.angle_delta_y
            } else {
                b.angle_delta_uv
            },
            filter_intra_mode: if plane == 0 {
                b.filter_intra_mode
            } else {
                None
            },
            edge_filter: self.f.seq.enable_intra_edge_filter,
            smooth_neighbour: self.f.seq.enable_intra_edge_filter && self.filter_type(plane),
            max_x: max_x - 1,
            max_y: max_y - 1,
            bit_depth: self.f.bit_depth,
        };
        predict::predict_intra(&mut self.f.planes[plane], &intra);
        if is_cfl {
            self.predict_cfl(plane, start_x, start_y, tx);
        }
    }

    /// The predict chroma from luma process.
    fn predict_cfl(&mut self, plane: usize, start_x: usize, start_y: usize, tx: usize) {
        let (log2w, log2h) = (u32::from(TX_WIDTH_LOG2[tx]), u32::from(TX_HEIGHT_LOG2[tx]));
        let (w, h) = (1usize << log2w, 1usize << log2h);
        let (sx, sy) = (self.f.sub_x, self.f.sub_y);
        let alpha = if plane == 1 {
            self.b.cfl_alpha_u
        } else {
            self.b.cfl_alpha_v
        };
        let luma = &self.f.planes[0];
        let mut l = vec![0i32; w * h];
        let mut sum = 0i32;
        for i in 0..h {
            let luma_y = ((start_y + i) << sy).min(self.b.max_luma_h - (1 << sy));
            for j in 0..w {
                let luma_x = ((start_x + j) << sx).min(self.b.max_luma_w - (1 << sx));
                let mut t = 0;
                for dy in 0..=sy {
                    for dx in 0..=sx {
                        t += luma.at(luma_x + dx, luma_y + dy);
                    }
                }
                let v = t << (3 - sx - sy);
                l[i * w + j] = v;
                sum += v;
            }
        }
        let avg = round2(sum, log2w + log2h);
        let max = (1 << self.f.bit_depth) - 1;
        let out = &mut self.f.planes[plane];
        for i in 0..h {
            for j in 0..w {
                let dc = out.at(start_x + j, start_y + i);
                let scaled = round2_signed(alpha * (l[i * w + j] - avg), 6);
                out.set(start_x + j, start_y + i, (dc + scaled).clamp(0, max));
            }
        }
    }

    /// `get_tx_set()`.
    fn tx_set(&self, tx: usize) -> usize {
        let (sqr, sqr_up) = (TX_SIZE_SQR[tx], TX_SIZE_SQR_UP[tx]);
        if sqr_up > 3 {
            0
        } else if self.b.is_inter {
            if self.f.fh.reduced_tx_set || sqr_up == 3 {
                3
            } else if sqr == 2 {
                2
            } else {
                1
            }
        } else if sqr_up == 3 {
            0
        } else if self.f.fh.reduced_tx_set || sqr == 2 {
            2
        } else {
            1
        }
    }

    /// `get_qindex()`.
    fn qindex(&self, ignore_delta_q: bool, segment_id: usize) -> i32 {
        let fh = self.f.fh;
        let base = i32::from(fh.quantization.base_q_idx);
        let use_delta = !ignore_delta_q && fh.delta_q_present;
        if self.seg_feature_active(segment_id, SEG_LVL_ALT_Q) {
            let data = i32::from(fh.segmentation.feature_data[segment_id][SEG_LVL_ALT_Q]);
            ((if use_delta {
                self.current_q_index
            } else {
                base
            }) + data)
                .clamp(0, 255)
        } else if use_delta {
            self.current_q_index
        } else {
            base
        }
    }

    /// `transform_type()`: the luma transform type, stored for the block's 4×4 units.
    fn transform_type(&mut self, x4: usize, y4: usize, tx: usize) -> Result<(), Error> {
        let set = self.tx_set(tx);
        let seg = &self.f.fh.segmentation;
        let q = if seg.enabled {
            self.qindex(true, self.b.segment_id)
        } else {
            i32::from(self.f.fh.quantization.base_q_idx)
        };
        let tx_type = if set > 0 && q > 0 && self.b.is_inter {
            let sqr = usize::from(TX_SIZE_SQR[tx]);
            match set {
                1 => TX_TYPE_INTER_INV_SET1[self.sym(|c| &mut c.inter_tx_type_set1[sqr])?],
                2 => TX_TYPE_INTER_INV_SET2[self.sym(|c| &mut c.inter_tx_type_set2)?],
                _ => TX_TYPE_INTER_INV_SET3[self.sym(|c| &mut c.inter_tx_type_set3[sqr])?],
            }
        } else if set > 0 && q > 0 {
            let dir = match self.b.filter_intra_mode {
                Some(m) => FILTER_INTRA_MODE_TO_INTRA_DIR[usize::from(m)],
                None => self.b.y_mode,
            };
            let (sqr, dir) = (usize::from(TX_SIZE_SQR[tx]), usize::from(dir));
            if set == 1 {
                TX_TYPE_INTRA_INV_SET1[self.sym(|c| &mut c.intra_tx_type_set1[sqr][dir])?]
            } else {
                TX_TYPE_INTRA_INV_SET2[self.sym(|c| &mut c.intra_tx_type_set2[sqr][dir])?]
            }
        } else {
            DCT_DCT
        };
        self.set_tx_types(x4, y4, tx, tx_type);
        Ok(())
    }

    fn set_tx_types(&mut self, x4: usize, y4: usize, tx: usize, tx_type: u8) {
        let (w4, h4) = (
            usize::from(TX_WIDTH[tx]) >> 2,
            usize::from(TX_HEIGHT[tx]) >> 2,
        );
        for j in 0..h4 {
            let i = self.f.mi(y4 + j, x4);
            self.f.tx_types[i..i + w4].fill(tx_type);
        }
    }

    /// `compute_tx_type()`.
    fn compute_tx_type(&self, plane: usize, tx: usize, x4: usize, y4: usize) -> u8 {
        if self.b.lossless || TX_SIZE_SQR_UP[tx] > 3 {
            return DCT_DCT;
        }
        if plane == 0 {
            return self.f.tx_types[self.f.mi(y4, x4)];
        }
        if self.b.is_inter {
            // The luma transform type at the same place.
            let x = self.b.col.max(x4 << self.f.sub_x);
            let y = self.b.row.max(y4 << self.f.sub_y);
            let tx_type = self.f.tx_types[self.f.mi(y, x)];
            return if TX_TYPE_IN_SET_INTER[self.tx_set(tx)][usize::from(tx_type)] == 1 {
                tx_type
            } else {
                DCT_DCT
            };
        }
        let tx_type = MODE_TO_TXFM[usize::from(self.b.uv_mode)];
        if TX_TYPE_IN_SET_INTRA[self.tx_set(tx)][usize::from(tx_type)] == 1 {
            tx_type
        } else {
            DCT_DCT
        }
    }

    /// `coeffs()`: returns the end of block and the transform type.
    fn coeffs(
        &mut self,
        plane: usize,
        start_x: usize,
        start_y: usize,
        tx: usize,
    ) -> Result<(usize, u8), Error> {
        let (x4, y4) = (start_x >> 2, start_y >> 2);
        let (w4, h4) = (
            usize::from(TX_WIDTH[tx]) >> 2,
            usize::from(TX_HEIGHT[tx]) >> 2,
        );
        let tx_ctx = usize::from(TX_SIZE_SQR[tx] + TX_SIZE_SQR_UP[tx]).div_ceil(2);
        let ptype = usize::from(plane > 0);
        let seg_eob = if tx == TX_16X64 || tx == TX_64X16 {
            512
        } else {
            (16 * w4 * h4).min(1024)
        };
        self.quant[..seg_eob].fill(0);
        let mut eob = 0;
        let mut cul_level = 0u32;
        let mut dc_category = 0u8;
        let mut tx_type = DCT_DCT;
        let ctx = self.all_zero_ctx(plane, tx, x4, y4, w4, h4);
        if self.sym(|c| &mut c.txb_skip[tx_ctx][ctx])? == 1 {
            if plane == 0 {
                self.set_tx_types(x4, y4, tx, DCT_DCT);
            }
        } else {
            if plane == 0 {
                self.transform_type(x4, y4, tx)?;
            }
            tx_type = self.compute_tx_type(plane, tx, x4, y4);
            let class = tx_class(tx_type);
            let scan = scan(tx, tx_type);
            let multisize = u32::from(TX_WIDTH_LOG2[tx].min(5) + TX_HEIGHT_LOG2[tx].min(5)) - 4;
            let ctx = usize::from(class != TX_CLASS_2D);
            let eob_pt = 1 + match multisize {
                0 => self.sym(|c| &mut c.eob_pt_16[ptype][ctx])?,
                1 => self.sym(|c| &mut c.eob_pt_32[ptype][ctx])?,
                2 => self.sym(|c| &mut c.eob_pt_64[ptype][ctx])?,
                3 => self.sym(|c| &mut c.eob_pt_128[ptype][ctx])?,
                4 => self.sym(|c| &mut c.eob_pt_256[ptype][ctx])?,
                5 => self.sym(|c| &mut c.eob_pt_512[ptype])?,
                _ => self.sym(|c| &mut c.eob_pt_1024[ptype])?,
            };
            eob = if eob_pt < 2 {
                eob_pt
            } else {
                (1 << (eob_pt - 2)) + 1
            };
            if eob_pt >= 3 {
                if self.sym(|c| &mut c.eob_extra[tx_ctx][ptype][eob_pt - 3])? == 1 {
                    eob += 1 << (eob_pt - 3);
                }
                for i in 1..eob_pt - 2 {
                    if self.lit(1)? == 1 {
                        eob += 1 << (eob_pt - 3 - i);
                    }
                }
            }
            for c in (0..eob).rev() {
                let pos = usize::from(scan[c]);
                let mut level = if c == eob - 1 {
                    let ctx = self.coeff_base_eob_ctx(tx, c);
                    self.sym(|cdf| &mut cdf.coeff_base_eob[tx_ctx][ptype][ctx])? as i32 + 1
                } else {
                    let ctx = self.coeff_base_ctx(tx, class, pos);
                    self.sym(|cdf| &mut cdf.coeff_base[tx_ctx][ptype][ctx])? as i32
                };
                if level > 2 {
                    let ctx = self.coeff_br_ctx(tx, class, pos);
                    for _ in 0..4 {
                        let br = self.sym(|c| &mut c.coeff_br[tx_ctx.min(3)][ptype][ctx])?;
                        level += br as i32;
                        if br < 3 {
                            break;
                        }
                    }
                }
                self.quant[pos] = level;
            }
            for (c, &pos) in scan[..eob].iter().enumerate() {
                let pos = usize::from(pos);
                let mut q = self.quant[pos] as u32;
                let sign = if q == 0 {
                    false
                } else if c == 0 {
                    let ctx = self.dc_sign_ctx(plane, x4, y4, w4, h4);
                    self.sym(|cdf| &mut cdf.dc_sign[ptype][ctx])? == 1
                } else {
                    self.lit(1)? == 1
                };
                if q > 14 {
                    let mut length = 0;
                    loop {
                        length += 1;
                        if self.lit(1)? == 1 {
                            break;
                        }
                        if length == 32 {
                            return Err(Error::Invalid("coefficient golomb length"));
                        }
                    }
                    let mut x = 1u64;
                    for _ in 0..length - 1 {
                        x = (x << 1) | u64::from(self.lit(1)?);
                    }
                    q = ((x + 14) & 0xFFFFF) as u32;
                }
                if pos == 0 && q > 0 {
                    dc_category = if sign { 1 } else { 2 };
                }
                q &= 0xFFFFF;
                cul_level += q;
                self.quant[pos] = if sign { -(q as i32) } else { q as i32 };
            }
            cul_level = cul_level.min(63);
        }
        self.above_level[plane][x4..x4 + w4].fill(cul_level as u8);
        self.above_dc[plane][x4..x4 + w4].fill(dc_category);
        self.left_level[plane][y4..y4 + h4].fill(cul_level as u8);
        self.left_dc[plane][y4..y4 + h4].fill(dc_category);
        Ok((eob, tx_type))
    }

    fn all_zero_ctx(
        &self,
        plane: usize,
        tx: usize,
        x4: usize,
        y4: usize,
        w4: usize,
        h4: usize,
    ) -> usize {
        let (sx, sy) = self.plane_sub(plane);
        let (max_x4, max_y4) = (self.f.mi_cols >> sx, self.f.mi_rows >> sy);
        let (w, h) = (usize::from(TX_WIDTH[tx]), usize::from(TX_HEIGHT[tx]));
        let bsize = self.residual_size(self.b.size, plane);
        let bw = 4 * usize::from(NUM_4X4_BLOCKS_WIDE[bsize]);
        let bh = 4 * usize::from(NUM_4X4_BLOCKS_HIGH[bsize]);
        let above = (x4..x4 + w4).filter(|&x| x < max_x4);
        let left = (y4..y4 + h4).filter(|&y| y < max_y4);
        if plane == 0 {
            let top = above.map(|x| self.above_level[0][x]).max().unwrap_or(0);
            let left = left.map(|y| self.left_level[0][y]).max().unwrap_or(0);
            if bw == w && bh == h {
                0
            } else if top == 0 && left == 0 {
                1
            } else if top == 0 || left == 0 {
                2 + usize::from(top.max(left) > 3)
            } else if top.max(left) <= 3 {
                4
            } else if top.min(left) <= 3 {
                5
            } else {
                6
            }
        } else {
            let a = above.fold(0, |acc, x| {
                acc | self.above_level[plane][x] | self.above_dc[plane][x]
            });
            let l = left.fold(0, |acc, y| {
                acc | self.left_level[plane][y] | self.left_dc[plane][y]
            });
            7 + usize::from(a != 0) + usize::from(l != 0) + if bw * bh > w * h { 3 } else { 0 }
        }
    }

    fn coeff_base_eob_ctx(&self, tx: usize, c: usize) -> usize {
        let adj = usize::from(ADJUSTED_TX_SIZE[tx]);
        let area = usize::from(TX_HEIGHT[adj]) << TX_WIDTH_LOG2[adj];
        if c == 0 {
            0
        } else if c <= area / 8 {
            1
        } else if c <= area / 4 {
            2
        } else {
            3
        }
    }

    fn coeff_base_ctx(&self, tx: usize, class: usize, pos: usize) -> usize {
        let adj = usize::from(ADJUSTED_TX_SIZE[tx]);
        let bwl = TX_WIDTH_LOG2[adj];
        let (width, height) = (1usize << bwl, usize::from(TX_HEIGHT[adj]));
        let (row, col) = (pos >> bwl, pos & (width - 1));
        let mut mag = 0;
        for [dr, dc] in SIG_REF_DIFF_OFFSET[class] {
            let (rr, rc) = (row + usize::from(dr), col + usize::from(dc));
            if rr < height && rc < width {
                mag += self.quant[(rr << bwl) + rc].unsigned_abs().min(3) as usize;
            }
        }
        let ctx = ((mag + 1) >> 1).min(4);
        if class == TX_CLASS_2D {
            if row == 0 && col == 0 {
                return 0;
            }
            return ctx + usize::from(COEFF_BASE_CTX_OFFSET[tx][row.min(4)][col.min(4)]);
        }
        let idx = if class == TX_CLASS_VERT { row } else { col };
        ctx + usize::from(COEFF_BASE_POS_CTX_OFFSET[idx.min(2)])
    }

    fn coeff_br_ctx(&self, tx: usize, class: usize, pos: usize) -> usize {
        let adj = usize::from(ADJUSTED_TX_SIZE[tx]);
        let bwl = TX_WIDTH_LOG2[adj];
        let (txw, txh) = (usize::from(TX_WIDTH[adj]), usize::from(TX_HEIGHT[adj]));
        let (row, col) = (pos >> bwl, pos & (txw - 1));
        let mut mag = 0;
        for [dr, dc] in MAG_REF_OFFSET_WITH_TX_CLASS[class] {
            let (rr, rc) = (row + usize::from(dr), col + usize::from(dc));
            if rr < txh && rc < txw {
                mag += self.quant[rr * txw + rc].min(15) as usize;
            }
        }
        let mag = ((mag + 1) >> 1).min(6);
        let near = match class {
            TX_CLASS_2D => row < 2 && col < 2,
            TX_CLASS_HORIZ => col == 0,
            _ => row == 0,
        };
        if pos == 0 {
            mag
        } else if near {
            mag + 7
        } else {
            mag + 14
        }
    }

    fn dc_sign_ctx(&self, plane: usize, x4: usize, y4: usize, w4: usize, h4: usize) -> usize {
        let (sx, sy) = self.plane_sub(plane);
        let (max_x4, max_y4) = (self.f.mi_cols >> sx, self.f.mi_rows >> sy);
        let weight = |s: u8| match s {
            1 => -1,
            2 => 1,
            _ => 0,
        };
        let dc_sign: i32 = (x4..x4 + w4)
            .filter(|&x| x < max_x4)
            .map(|x| weight(self.above_dc[plane][x]))
            .chain(
                (y4..y4 + h4)
                    .filter(|&y| y < max_y4)
                    .map(|y| weight(self.left_dc[plane][y])),
            )
            .sum();
        match dc_sign {
            ..0 => 1,
            0 => 0,
            _ => 2,
        }
    }

    /// The reconstruct process: dequantization, inverse transform, then adding the residual.
    fn reconstruct(&mut self, plane: usize, x: usize, y: usize, tx: usize, tx_type: u8) {
        let bd = self.f.bit_depth;
        let dq_denom = match tx {
            3 | 9 | 10 | 17 | 18 => 2,
            4 | 11 | 12 => 4,
            _ => 1,
        };
        let (log2w, log2h) = (u32::from(TX_WIDTH_LOG2[tx]), u32::from(TX_HEIGHT_LOG2[tx]));
        let (w, h) = (1usize << log2w, 1usize << log2h);
        let (tw, th) = (w.min(32), h.min(32));
        let q = &self.f.fh.quantization;
        let (dc_delta, ac_delta) = match plane {
            0 => (q.delta_q_y_dc, 0),
            1 => (q.delta_q_u_dc, q.delta_q_u_ac),
            _ => (q.delta_q_v_dc, q.delta_q_v_ac),
        };
        let qindex = self.qindex(false, self.b.segment_id);
        let table = ((bd - 8) >> 1) as usize;
        let lookup = |t: &[[u16; 256]; 3], delta: i8| {
            i64::from(t[table][(qindex + i32::from(delta)).clamp(0, 255) as usize])
        };
        let (dc_q, ac_q) = (lookup(&DC_QLOOKUP, dc_delta), lookup(&AC_QLOOKUP, ac_delta));
        let limit = 1i64 << (7 + bd);
        let dequant: Vec<i32> = (0..tw * th)
            .map(|i| {
                let dq = i64::from(self.quant[i]) * if i == 0 { dc_q } else { ac_q };
                let dq2 = ((dq.abs() & 0xFF_FFFF) / dq_denom) * dq.signum();
                dq2.clamp(-limit, limit - 1) as i32
            })
            .collect();
        let residual = transform::inverse_2d(
            &dequant,
            log2w,
            log2h,
            tx_type,
            self.b.lossless,
            u32::from(TRANSFORM_ROW_SHIFT[tx]),
            bd,
        );
        let flip_ud = matches!(tx_type, 4 | 8 | 14 | 6);
        let flip_lr = matches!(tx_type, 5 | 7 | 15 | 6);
        let max = (1 << bd) - 1;
        let out = &mut self.f.planes[plane];
        for i in 0..h {
            for j in 0..w {
                let xx = if flip_lr { w - j - 1 } else { j };
                let yy = if flip_ud { h - i - 1 } else { i };
                let v = out.at(x + xx, y + yy) + residual[i * w + j];
                out.set(x + xx, y + yy, v.clamp(0, max));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neg_deinterleave_inverts_the_encoder_mapping() {
        // libaom's av1_neg_interleave, the encoder side.
        fn neg_interleave(x: i32, r: i32, max: i32) -> i32 {
            if r == 0 {
                return x;
            }
            if r >= max - 1 {
                return -x + max - 1;
            }
            let diff = x - r;
            if 2 * r < max {
                if diff.abs() <= r {
                    return if diff > 0 {
                        (diff << 1) - 1
                    } else {
                        (-diff) << 1
                    };
                }
                return x;
            }
            if diff.abs() < max - r {
                return if diff > 0 {
                    (diff << 1) - 1
                } else {
                    (-diff) << 1
                };
            }
            (max - x) - 1
        }
        for max in 1..=8 {
            for r in 0..max {
                for x in 0..max {
                    let coded = neg_interleave(x, r, max);
                    assert_eq!(
                        neg_deinterleave(coded, r, max),
                        x,
                        "x={x} ref={r} max={max}"
                    );
                }
            }
        }
    }

    #[test]
    fn inverse_recenter_covers_the_range() {
        for r in 0..10 {
            let mut seen: Vec<i32> = (0..20).map(|v| inverse_recenter(r, v)).collect();
            seen.sort_unstable();
            assert_eq!(seen, (0..20).collect::<Vec<_>>(), "r={r}");
        }
    }
}
