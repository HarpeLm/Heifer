//! Slice segment data: coding tree units, coding units, transform trees and residuals
//! (H.265 §7.3.8), decoded with CABAC (context selection in §9.3.4.2).
//!
//! This module only *parses*: decoded blocks are handed to a [`Sink`], which will perform the
//! reconstruction (prediction, inverse transform, filters). Parsing still needs a few derived
//! values that depend on previously decoded blocks: intra prediction modes (for the most
//! probable mode list and the coefficient scan order), coding tree depths (for contexts),
//! and luma QPs (prediction of the QP from neighbours).

use crate::Error;
use crate::bitreader::BitReader;
use crate::cabac::{ArithmeticDecoder, ContextModel};
use crate::contexts::{Contexts, ctx};
use crate::params::{Pps, Sps};
use crate::scan::{CtbLayout, ScanType, scan_order};
use crate::slice::SliceHeader;

/// SAO parameters of one CTB (§7.3.8.3), per colour component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SaoParams {
    /// `SaoTypeIdx`: 0 = off, 1 = band offset, 2 = edge offset.
    pub type_idx: [u8; 3],
    /// `SaoOffsetVal[1..=4]`, already signed and scaled.
    pub offsets: [[i16; 4]; 3],
    /// `sao_band_position` (band offset).
    pub band_position: [u8; 3],
    /// `SaoEoClass` (edge offset): 0 = horizontal, 1 = vertical, 2 = 135°, 3 = 45°.
    pub eo_class: [u8; 3],
}

/// A transform block of one colour component, in decoding order. Emitted for every block,
/// including those without coded residual, because intra prediction is done per block.
#[derive(Debug)]
pub struct TransformBlock<'a> {
    /// Colour component: 0 = Y, 1 = Cb, 2 = Cr.
    pub c_idx: u8,
    /// Top-left position, in samples of this component.
    pub x: u32,
    /// Top-left position, in samples of this component.
    pub y: u32,
    /// Block size (log2), in samples of this component.
    pub log2_size: u8,
    /// Intra prediction mode (0 = planar, 1 = DC, 2..=34 = angular).
    pub intra_mode: u8,
    /// `TransCoeffLevel`, row-major (`[y * size + x]`), or `None` when the block has no residual.
    pub coeffs: Option<&'a [i32]>,
    /// `transform_skip_flag`.
    pub transform_skip: bool,
    /// `cu_transquant_bypass_flag` of the coding unit.
    pub transquant_bypass: bool,
    /// Luma QP of the coding unit (`QpY`).
    pub qp_y: i32,
    /// `CuQpOffsetCb` / `CuQpOffsetCr` (range extension), for chroma blocks.
    pub cu_qp_offset: i8,
    /// `ResScaleVal` for cross-component prediction (chroma, 4:4:4 only), 0 when unused.
    pub res_scale_val: i8,
}

/// A coding unit, emitted once fully parsed (after its transform blocks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodingUnit {
    /// Top-left luma position.
    pub x: u32,
    /// Top-left luma position.
    pub y: u32,
    /// `log2CbSize`.
    pub log2_size: u8,
    /// `PartMode` is `PART_NxN` (four prediction blocks).
    pub part_nxn: bool,
    /// `cu_transquant_bypass_flag`.
    pub transquant_bypass: bool,
    /// `pcm_flag`.
    pub pcm: bool,
    /// Final `QpY` of the coding unit.
    pub qp_y: i32,
}

/// Receives the parsed content of a slice, in decoding order.
pub trait Sink {
    /// SAO parameters of a CTB.
    fn sao(&mut self, _ctb_x: u32, _ctb_y: u32, _params: &SaoParams) {}
    /// A transform block (prediction + optional residual).
    fn transform_block(&mut self, _tb: &TransformBlock<'_>) {}
    /// PCM samples of a coding unit (raw, at PCM bit depth), luma then Cb then Cr.
    fn pcm(&mut self, _cu_x: u32, _cu_y: u32, _log2_size: u8, _samples: [&[u16]; 3]) {}
    /// A coding unit is complete.
    fn coding_unit(&mut self, _cu: &CodingUnit) {}
}

/// Per-picture parsing state, shared by all slice segments of a picture.
#[derive(Debug)]
pub struct Picture<'a> {
    /// Active SPS.
    pub sps: &'a Sps,
    /// Active PPS.
    pub pps: &'a Pps,
    /// CTB scan conversions and tiles.
    pub layout: CtbLayout,
    /// SAO parameters per CTB (raster order).
    pub sao: Vec<SaoParams>,
    /// Number of CTBs decoded so far.
    pub decoded_ctbs: u32,
    // Grids at 4×4 luma granularity.
    w4: usize,
    /// `SliceAddrRs` of the slice that decoded each 4×4 block (`u32::MAX` = not decoded yet).
    slice_addr: Vec<u32>,
    ct_depth: Vec<u8>,
    intra_mode: Vec<u8>,
    pcm: Vec<bool>,
    qp_y: Vec<i8>,
    wpp_saved: Option<Contexts>,
    ds_saved: Option<Contexts>,
    slice_addr_rs: u32,
    /// `QpY` of the last coding unit, carried to dependent slice segments.
    last_qp_y: i32,
}

impl<'a> Picture<'a> {
    /// Creates the state for a new picture.
    pub fn new(sps: &'a Sps, pps: &'a Pps) -> Self {
        let layout = CtbLayout::new(sps, pps);
        let w4 = sps.pic_width_in_luma_samples.div_ceil(4) as usize;
        let h4 = sps.pic_height_in_luma_samples.div_ceil(4) as usize;
        let n = w4 * h4;
        Self {
            sao: vec![SaoParams::default(); (layout.width * layout.height) as usize],
            layout,
            sps,
            pps,
            decoded_ctbs: 0,
            w4,
            slice_addr: vec![u32::MAX; n],
            ct_depth: vec![0; n],
            intra_mode: vec![1; n],
            pcm: vec![false; n],
            qp_y: vec![0; n],
            wpp_saved: None,
            ds_saved: None,
            slice_addr_rs: 0,
            last_qp_y: 0,
        }
    }

    /// Whether all CTBs of the picture have been decoded.
    pub fn is_complete(&self) -> bool {
        self.decoded_ctbs == self.layout.width * self.layout.height
    }

    fn idx(&self, x: u32, y: u32) -> usize {
        (y as usize >> 2) * self.w4 + (x as usize >> 2)
    }

    fn ctb_rs(&self, x: u32, y: u32) -> u32 {
        let l = self.sps.log2_ctb_size;
        (y >> l) * self.layout.width + (x >> l)
    }

    /// Availability of a neighbouring luma position (§6.4.1): inside the picture, already
    /// decoded, in the same slice and the same tile.
    fn available(&self, x: u32, y: u32, xn: i64, yn: i64) -> bool {
        if xn < 0
            || yn < 0
            || xn >= i64::from(self.sps.pic_width_in_luma_samples)
            || yn >= i64::from(self.sps.pic_height_in_luma_samples)
        {
            return false;
        }
        let (xn, yn) = (xn as u32, yn as u32);
        self.slice_addr[self.idx(xn, yn)] == self.slice_addr_rs
            && self.layout.tile_of_rs(self.ctb_rs(xn, yn))
                == self.layout.tile_of_rs(self.ctb_rs(x, y))
    }

    fn fill<T: Copy>(
        grid_sel: fn(&mut Self) -> &mut Vec<T>,
        pic: &mut Self,
        x: u32,
        y: u32,
        size: u32,
        v: T,
    ) {
        let (w4, x4, y4, n4) = (
            pic.w4,
            x as usize >> 2,
            y as usize >> 2,
            (size as usize).div_ceil(4),
        );
        let h4 = pic.slice_addr.len() / w4;
        let grid = grid_sel(pic);
        for yy in y4..(y4 + n4).min(h4) {
            let row = yy * w4;
            for xx in x4..(x4 + n4).min(w4) {
                grid[row + xx] = v;
            }
        }
    }
}

/// Result of parsing a slice segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceStats {
    /// Number of CTUs in the slice segment.
    pub ctus: u32,
    /// Byte position (in the slice data) right after the final `end_of_slice_segment_flag`.
    pub end_position: usize,
}

/// Parses the data of one slice segment. `data` is the RBSP after the slice header.
pub fn decode_slice_segment(
    pic: &mut Picture<'_>,
    header: &SliceHeader,
    slice_qp_y: i32,
    data: &[u8],
    sink: &mut impl Sink,
) -> Result<SliceStats, Error> {
    if pic
        .sps
        .range_extension
        .persistent_rice_adaptation_enabled_flag
        || pic.sps.range_extension.cabac_bypass_alignment_enabled_flag
        || pic.sps.range_extension.implicit_rdpcm_enabled_flag
        || pic.sps.range_extension.extended_precision_processing_flag
    {
        return Err(Error::Unimplemented("range extension coding tools"));
    }
    let (sps, pps) = (pic.sps, pic.pps);
    let mut p = Parser {
        sps,
        pps,
        pic,
        data,
        pos: 0,
        engine: ArithmeticDecoder::new(data)?,
        ctx: Contexts::new(slice_qp_y),
        slice_qp_y,
        qp_bd_offset: 6 * (i32::from(sps.bit_depth_luma) - 8),
        is_cu_qp_delta_coded: false,
        cu_qp_delta_val: 0,
        is_cu_chroma_qp_offset_coded: false,
        cu_qp_offset: (0, 0),
        qp_y_pred: slice_qp_y,
        last_qp_y: slice_qp_y,
        first_qg_in_substream: true,
        cu_transquant_bypass: false,
        coeffs: vec![0; 32 * 32],
        sao_luma: header.slice_sao_luma_flag,
        sao_chroma: header.slice_sao_chroma_flag,
        cu_chroma_qp_offset_enabled: header.cu_chroma_qp_offset_enabled_flag,
    };
    if header.dependent_slice_segment_flag {
        // QP prediction continues across the segments of a slice (§8.6.1).
        p.first_qg_in_substream = false;
        p.last_qp_y = p.pic.last_qp_y;
    }
    let stats = p.run(header, sink);
    p.pic.last_qp_y = p.last_qp_y;
    stats
}

struct Parser<'p, 'a> {
    pic: &'p mut Picture<'a>,
    sps: &'a Sps,
    pps: &'a Pps,
    data: &'p [u8],
    /// Start of the current substream in `data`.
    pos: usize,
    engine: ArithmeticDecoder<'p>,
    ctx: Contexts,
    slice_qp_y: i32,
    qp_bd_offset: i32,
    is_cu_qp_delta_coded: bool,
    cu_qp_delta_val: i32,
    is_cu_chroma_qp_offset_coded: bool,
    cu_qp_offset: (i8, i8),
    qp_y_pred: i32,
    last_qp_y: i32,
    first_qg_in_substream: bool,
    cu_transquant_bypass: bool,
    coeffs: Vec<i32>,
    sao_luma: bool,
    sao_chroma: bool,
    cu_chroma_qp_offset_enabled: bool,
}

/// Helpers to decode bins with a context index.
impl Parser<'_, '_> {
    fn bin(&mut self, i: usize) -> u8 {
        let c: &mut ContextModel = &mut self.ctx[i];
        self.engine.decode(c)
    }

    fn flag(&mut self, i: usize) -> bool {
        self.bin(i) == 1
    }

    fn bypass(&mut self) -> u8 {
        self.engine.decode_bypass()
    }

    /// Truncated unary bypass value with maximum `c_max`.
    fn tr_bypass(&mut self, c_max: u32) -> u32 {
        let mut v = 0;
        while v < c_max && self.bypass() == 1 {
            v += 1;
        }
        v
    }

    /// k-th order Exp-Golomb bypass value (§9.3.3.3).
    fn eg_bypass(&mut self, mut k: u32) -> Result<u32, Error> {
        let mut abs = 0u32;
        while self.bypass() == 1 {
            abs = abs
                .checked_add(1 << k)
                .ok_or(Error::Invalid("Exp-Golomb bypass overflow"))?;
            k += 1;
            if k > 31 {
                return Err(Error::Invalid("Exp-Golomb bypass too long"));
            }
        }
        Ok(abs + self.engine.decode_bypass_bits(k))
    }
}

impl Parser<'_, '_> {
    fn run(&mut self, header: &SliceHeader, sink: &mut impl Sink) -> Result<SliceStats, Error> {
        let layout_w = self.pic.layout.width;
        let mut ts = self.pic.layout.rs_to_ts[header.slice_segment_address as usize];
        if !header.dependent_slice_segment_flag {
            self.pic.slice_addr_rs = header.slice_segment_address;
        }
        let wpp = self.pps.entropy_coding_sync_enabled_flag;
        let ctb_size = self.sps.ctb_size();
        let mut ctus = 0;
        let mut first_in_segment = true;

        loop {
            let rs = self.pic.layout.ts_to_rs[ts as usize];
            let (cx, cy) = (rs % layout_w, rs / layout_w);
            let (x0, y0) = (cx * ctb_size, cy * ctb_size);
            let first_in_tile = ts == 0
                || self.pic.layout.tile_id[ts as usize] != self.pic.layout.tile_id[ts as usize - 1];
            let row_start = cx == self.pic.layout.tile_column_start(cx);

            // Context initialization / synchronization (§9.3.1).
            if first_in_tile {
                if !first_in_segment {
                    self.ctx = Contexts::new(self.slice_qp_y);
                }
                self.first_qg_in_substream = true;
            } else if wpp && row_start {
                let tr_available = self.pic.available(
                    x0,
                    y0,
                    i64::from(x0 + ctb_size),
                    i64::from(y0) - i64::from(ctb_size),
                );
                // The availability check needs this CTB marked as part of the current slice.
                self.ctx = match (tr_available, self.pic.wpp_saved) {
                    (true, Some(saved)) => saved,
                    _ => Contexts::new(self.slice_qp_y),
                };
                self.first_qg_in_substream = true;
            } else if first_in_segment && header.dependent_slice_segment_flag {
                if let Some(saved) = self.pic.ds_saved {
                    self.ctx = saved;
                }
            }
            first_in_segment = false;

            self.coding_tree_unit(x0, y0, cx, cy, rs, sink)?;
            ctus += 1;
            self.pic.decoded_ctbs += 1;

            let end_of_slice_segment = self.engine.decode_terminate() == 1;
            if wpp && cx == self.pic.layout.tile_column_start(cx) + 1 {
                self.pic.wpp_saved = Some(self.ctx);
            }
            ts += 1;
            if end_of_slice_segment {
                if self.pps.dependent_slice_segments_enabled_flag {
                    self.pic.ds_saved = Some(self.ctx);
                }
                self.engine.check_overrun()?;
                return Ok(SliceStats {
                    ctus,
                    end_position: self.pos + self.engine.aligned_position_after_terminate(),
                });
            }
            if ts as usize >= self.pic.layout.ts_to_rs.len() {
                return Err(Error::Invalid("slice extends past the end of the picture"));
            }
            let next_rs = self.pic.layout.ts_to_rs[ts as usize];
            let next_x = next_rs % layout_w;
            let new_tile =
                self.pic.layout.tile_id[ts as usize] != self.pic.layout.tile_id[ts as usize - 1];
            if (self.pps.tiles.is_some() && new_tile)
                || (wpp && (next_x == self.pic.layout.tile_column_start(next_x) || new_tile))
            {
                if self.engine.decode_terminate() != 1 {
                    return Err(Error::Invalid("end_of_subset_one_bit must be 1"));
                }
                self.pos += self.engine.aligned_position_after_terminate();
                let rest = self.data.get(self.pos..).ok_or(Error::UnexpectedEof)?;
                self.engine = ArithmeticDecoder::new(rest)?;
            }
        }
    }

    fn coding_tree_unit(
        &mut self,
        x0: u32,
        y0: u32,
        cx: u32,
        cy: u32,
        rs: u32,
        sink: &mut impl Sink,
    ) -> Result<(), Error> {
        if self.sao_luma || self.sao_chroma {
            let params = self.sao(cx, cy, rs)?;
            self.pic.sao[rs as usize] = params;
            sink.sao(cx, cy, &params);
        }
        self.coding_quadtree(x0, y0, self.sps.log2_ctb_size, 0, sink)
    }

    /// `sao()` (§7.3.8.3).
    fn sao(&mut self, cx: u32, cy: u32, rs: u32) -> Result<SaoParams, Error> {
        let layout = &self.pic.layout;
        let tile = layout.tile_of_rs(rs);
        let slice_addr = self.pic.slice_addr_rs;
        let left_ok = cx > 0 && rs > slice_addr && layout.tile_of_rs(rs - 1) == tile;
        let up = rs.wrapping_sub(layout.width);
        let up_ok = cy > 0 && up >= slice_addr && layout.tile_of_rs(up) == tile;
        if left_ok && self.flag(ctx::SAO_MERGE_FLAG) {
            return Ok(self.pic.sao[rs as usize - 1]);
        }
        if up_ok && self.flag(ctx::SAO_MERGE_FLAG) {
            return Ok(self.pic.sao[up as usize]);
        }

        let mut p = SaoParams::default();
        let num_comps = if self.sps.chroma_array_type() != 0 {
            3
        } else {
            1
        };
        for c in 0..num_comps {
            let enabled = if c == 0 {
                self.sao_luma
            } else {
                self.sao_chroma
            };
            if !enabled {
                continue;
            }
            if c < 2 {
                p.type_idx[c] = if !self.flag(ctx::SAO_TYPE_IDX) {
                    0
                } else if self.bypass() == 0 {
                    1
                } else {
                    2
                };
            } else {
                p.type_idx[2] = p.type_idx[1];
            }
            if p.type_idx[c] == 0 {
                continue;
            }
            let bit_depth = if c == 0 {
                self.sps.bit_depth_luma
            } else {
                self.sps.bit_depth_chroma
            };
            let c_max = (1u32 << (u32::from(bit_depth.min(10)) - 5)) - 1;
            let mut abs = [0u32; 4];
            for a in &mut abs {
                *a = self.tr_bypass(c_max);
            }
            let log2_scale = if c == 0 {
                self.pps.range_extension.log2_sao_offset_scale_luma
            } else {
                self.pps.range_extension.log2_sao_offset_scale_chroma
            };
            if p.type_idx[c] == 1 {
                for (o, &a) in p.offsets[c].iter_mut().zip(&abs) {
                    let negative = a != 0 && self.bypass() == 1;
                    let v = (a as i16) << log2_scale;
                    *o = if negative { -v } else { v };
                }
                p.band_position[c] = self.engine.decode_bypass_bits(5) as u8;
            } else {
                for (i, (o, &a)) in p.offsets[c].iter_mut().zip(&abs).enumerate() {
                    let v = (a as i16) << log2_scale;
                    *o = if i < 2 { v } else { -v };
                }
                if c == 0 {
                    p.eo_class[0] = self.engine.decode_bypass_bits(2) as u8;
                } else if c == 1 {
                    p.eo_class[1] = self.engine.decode_bypass_bits(2) as u8;
                } else {
                    p.eo_class[2] = p.eo_class[1];
                }
            }
        }
        Ok(p)
    }

    /// `coding_quadtree()` (§7.3.8.4).
    fn coding_quadtree(
        &mut self,
        x0: u32,
        y0: u32,
        log2_size: u8,
        depth: u8,
        sink: &mut impl Sink,
    ) -> Result<(), Error> {
        let size = 1u32 << log2_size;
        let (pw, ph) = (
            self.sps.pic_width_in_luma_samples,
            self.sps.pic_height_in_luma_samples,
        );
        let min_cb = self.sps.log2_min_luma_coding_block_size;
        let split = if x0 + size <= pw && y0 + size <= ph && log2_size > min_cb {
            // ctxInc from the depth of the left and above coding units (§9.3.4.2.2).
            let mut inc = 0;
            if self.pic.available(x0, y0, i64::from(x0) - 1, i64::from(y0))
                && self.pic.ct_depth[self.pic.idx(x0 - 1, y0)] > depth
            {
                inc += 1;
            }
            if self.pic.available(x0, y0, i64::from(x0), i64::from(y0) - 1)
                && self.pic.ct_depth[self.pic.idx(x0, y0 - 1)] > depth
            {
                inc += 1;
            }
            self.flag(ctx::SPLIT_CU_FLAG + inc)
        } else {
            log2_size > min_cb
        };

        let log2_min_qg = self.sps.log2_ctb_size - self.pps.diff_cu_qp_delta_depth.unwrap_or(0);
        if log2_size >= log2_min_qg {
            self.start_quantization_group(x0, y0);
        }
        if self.cu_chroma_qp_offset_enabled {
            let depth = self
                .pps
                .range_extension
                .diff_cu_chroma_qp_offset_depth
                .unwrap_or(0);
            if log2_size >= self.sps.log2_ctb_size - depth {
                self.is_cu_chroma_qp_offset_coded = false;
            }
        }

        if split {
            let half = size / 2;
            for (dx, dy) in [(0, 0), (half, 0), (0, half), (half, half)] {
                if x0 + dx < pw && y0 + dy < ph {
                    self.coding_quadtree(x0 + dx, y0 + dy, log2_size - 1, depth + 1, sink)?;
                }
            }
            Ok(())
        } else {
            self.coding_unit(x0, y0, log2_size, depth, sink)
        }
    }

    /// Start of a quantization group: QP prediction (§8.6.1).
    fn start_quantization_group(&mut self, xq: u32, yq: u32) {
        self.is_cu_qp_delta_coded = false;
        self.cu_qp_delta_val = 0;
        let prev = if self.first_qg_in_substream {
            self.slice_qp_y
        } else {
            self.last_qp_y
        };
        self.first_qg_in_substream = false;
        let ctb_mask = !((1u32 << self.sps.log2_ctb_size) - 1);
        let same_ctb = |xn: u32, yn: u32| {
            (xn & ctb_mask) == (xq & ctb_mask) && (yn & ctb_mask) == (yq & ctb_mask)
        };
        let qp_a = if xq > 0
            && self.pic.available(xq, yq, i64::from(xq) - 1, i64::from(yq))
            && same_ctb(xq - 1, yq)
        {
            i32::from(self.pic.qp_y[self.pic.idx(xq - 1, yq)])
        } else {
            prev
        };
        let qp_b = if yq > 0
            && self.pic.available(xq, yq, i64::from(xq), i64::from(yq) - 1)
            && same_ctb(xq, yq - 1)
        {
            i32::from(self.pic.qp_y[self.pic.idx(xq, yq - 1)])
        } else {
            prev
        };
        self.qp_y_pred = (qp_a + qp_b + 1) >> 1;
    }

    fn current_qp_y(&self) -> i32 {
        let b = self.qp_bd_offset;
        ((self.qp_y_pred + self.cu_qp_delta_val + 52 + 2 * b) % (52 + b)) - b
    }

    /// `coding_unit()` (§7.3.8.5), intra only.
    fn coding_unit(
        &mut self,
        x0: u32,
        y0: u32,
        log2_size: u8,
        depth: u8,
        sink: &mut impl Sink,
    ) -> Result<(), Error> {
        let size = 1u32 << log2_size;
        // Mark the area as decoded by this slice (neighbours inside the CU are decoded in z-order).
        let slice_addr = self.pic.slice_addr_rs;
        Picture::fill(|p| &mut p.slice_addr, self.pic, x0, y0, size, slice_addr);
        Picture::fill(|p| &mut p.ct_depth, self.pic, x0, y0, size, depth);
        Picture::fill(|p| &mut p.intra_mode, self.pic, x0, y0, size, 1);
        Picture::fill(|p| &mut p.pcm, self.pic, x0, y0, size, false);

        self.cu_transquant_bypass =
            self.pps.transquant_bypass_enabled_flag && self.flag(ctx::CU_TRANSQUANT_BYPASS_FLAG);
        // part_mode: one bin, only at the minimum CB size (1 = 2Nx2N, 0 = NxN).
        let part_nxn =
            log2_size == self.sps.log2_min_luma_coding_block_size && !self.flag(ctx::PART_MODE);

        let mut pcm = false;
        if let Some(pcm_params) = self.sps.pcm
            && !part_nxn
            && (pcm_params.log2_min_size..=pcm_params.log2_max_size).contains(&log2_size)
        {
            pcm = self.engine.decode_terminate() == 1;
            if pcm {
                Picture::fill(|p| &mut p.pcm, self.pic, x0, y0, size, true);
                self.pcm_sample(x0, y0, log2_size, pcm_params, sink)?;
            }
        }

        if !pcm {
            let (luma_modes, chroma_modes) = self.intra_modes(x0, y0, log2_size, part_nxn)?;
            let ctx = TreeCtx {
                x_cu: x0,
                y_cu: y0,
                log2_cb: log2_size,
                part_nxn,
                luma_modes,
                chroma_modes,
                max_depth: self.sps.max_transform_hierarchy_depth_intra + u8::from(part_nxn),
            };
            self.transform_tree(
                &ctx, x0, y0, x0, y0, log2_size, 0, 0, [false; 2], [false; 2], sink,
            )?;
        }

        let qp_y = self.current_qp_y();
        Picture::fill(|p| &mut p.qp_y, self.pic, x0, y0, size, qp_y as i8);
        self.last_qp_y = qp_y;
        sink.coding_unit(&CodingUnit {
            x: x0,
            y: y0,
            log2_size,
            part_nxn,
            transquant_bypass: self.cu_transquant_bypass,
            pcm,
            qp_y,
        });
        Ok(())
    }

    /// `pcm_sample()` (§7.3.8.7): raw samples after byte alignment, then CABAC restarts.
    fn pcm_sample(
        &mut self,
        x0: u32,
        y0: u32,
        log2_size: u8,
        params: crate::params::Pcm,
        sink: &mut impl Sink,
    ) -> Result<(), Error> {
        let start = self.pos + self.engine.aligned_position_after_terminate();
        let data = self.data.get(start..).ok_or(Error::UnexpectedEof)?;
        let mut r = BitReader::new(data);
        let n_luma = 1usize << (2 * log2_size);
        let luma = (0..n_luma)
            .map(|_| r.bits(u32::from(params.bit_depth_luma)).map(|v| v as u16))
            .collect::<Result<Vec<_>, _>>()?;
        let (cb, cr) = if self.sps.chroma_array_type() != 0 {
            let (sw, sh) = self.sps.chroma_subsampling();
            let n = n_luma / (sw * sh) as usize;
            let mut read = || {
                (0..n)
                    .map(|_| r.bits(u32::from(params.bit_depth_chroma)).map(|v| v as u16))
                    .collect::<Result<Vec<_>, _>>()
            };
            (read()?, read()?)
        } else {
            (Vec::new(), Vec::new())
        };
        sink.pcm(x0, y0, log2_size, [&luma, &cb, &cr]);
        if !r.is_byte_aligned() {
            return Err(Error::Invalid("PCM samples do not end on a byte boundary"));
        }
        self.pos = start + r.position() / 8;
        self.engine =
            ArithmeticDecoder::new(self.data.get(self.pos..).ok_or(Error::UnexpectedEof)?)?;
        Ok(())
    }

    /// Intra prediction modes of a CU (§7.3.8.5 syntax, §8.4.2 and §8.4.3 derivations).
    /// Returns the luma mode of each prediction block and the chroma mode(s).
    fn intra_modes(
        &mut self,
        x0: u32,
        y0: u32,
        log2_size: u8,
        part_nxn: bool,
    ) -> Result<([u8; 4], [u8; 4]), Error> {
        let n_parts = if part_nxn { 4 } else { 1 };
        let pb_size = if part_nxn {
            1u32 << (log2_size - 1)
        } else {
            1 << log2_size
        };
        let mut prev_flags = [false; 4];
        for f in prev_flags.iter_mut().take(n_parts) {
            *f = self.flag(ctx::PREV_INTRA_LUMA_PRED_FLAG);
        }
        let mut luma = [1u8; 4];
        for (i, &prev) in prev_flags.iter().enumerate().take(n_parts) {
            let (xp, yp) = (x0 + (i as u32 % 2) * pb_size, y0 + (i as u32 / 2) * pb_size);
            let candidates = self.mpm_candidates(xp, yp);
            let mode = if prev {
                let mpm_idx = self.tr_bypass(2) as usize;
                candidates[mpm_idx]
            } else {
                let mut sorted = candidates;
                sorted.sort_unstable();
                let mut mode = self.engine.decode_bypass_bits(5) as u8;
                for c in sorted {
                    if mode >= c {
                        mode += 1;
                    }
                }
                mode
            };
            luma[i] = mode;
            Picture::fill(|p| &mut p.intra_mode, self.pic, xp, yp, pb_size, mode);
        }
        if !part_nxn {
            luma = [luma[0]; 4];
        }

        let mut chroma = [0u8; 4];
        match self.sps.chroma_array_type() {
            0 => {}
            3 if part_nxn => {
                for i in 0..4 {
                    chroma[i] = self.chroma_mode(luma[i]);
                }
            }
            _ => chroma = [self.chroma_mode(luma[0]); 4],
        }
        Ok((luma, chroma))
    }

    /// Most probable mode candidates (§8.4.2).
    fn mpm_candidates(&self, xp: u32, yp: u32) -> [u8; 3] {
        let cand = |xn: i64, yn: i64, above: bool| -> u8 {
            if !self.pic.available(xp, yp, xn, yn) {
                return 1;
            }
            let (xn, yn) = (xn as u32, yn as u32);
            let i = self.pic.idx(xn, yn);
            if self.pic.pcm[i] {
                return 1;
            }
            // The above candidate must be in the same CTB row.
            if above && yn < ((yp >> self.sps.log2_ctb_size) << self.sps.log2_ctb_size) {
                return 1;
            }
            self.pic.intra_mode[i]
        };
        let a = cand(i64::from(xp) - 1, i64::from(yp), false);
        let b = cand(i64::from(xp), i64::from(yp) - 1, true);
        if a == b {
            if a < 2 {
                [0, 1, 26]
            } else {
                [a, 2 + ((a + 29) % 32), 2 + ((a - 2 + 1) % 32)]
            }
        } else {
            let c = if a != 0 && b != 0 {
                0
            } else if a != 1 && b != 1 {
                1
            } else {
                26
            };
            [a, b, c]
        }
    }

    /// `intra_chroma_pred_mode` and derivation of `IntraPredModeC` (§8.4.3).
    fn chroma_mode(&mut self, luma: u8) -> u8 {
        let syntax = if !self.flag(ctx::INTRA_CHROMA_PRED_MODE) {
            4
        } else {
            self.engine.decode_bypass_bits(2) as u8
        };
        let mode = match syntax {
            4 => luma,
            s => {
                let m = [0, 26, 10, 1][usize::from(s)];
                if m == luma { 34 } else { m }
            }
        };
        if self.sps.chroma_array_type() == 2 {
            // Table 8-3: 4:2:2 mode mapping.
            const MAP_422: [u8; 35] = [
                0, 1, 2, 2, 2, 2, 3, 5, 7, 8, 10, 11, 13, 15, 16, 18, 19, 20, 21, 22, 23, 23, 24,
                24, 25, 25, 26, 27, 27, 28, 28, 29, 29, 30, 31,
            ];
            MAP_422[usize::from(mode)]
        } else {
            mode
        }
    }

    /// `transform_tree()` (§7.3.8.8).
    #[allow(clippy::too_many_arguments)]
    fn transform_tree(
        &mut self,
        t: &TreeCtx,
        x0: u32,
        y0: u32,
        x_base: u32,
        y_base: u32,
        log2_size: u8,
        depth: u8,
        blk_idx: u8,
        parent_cbf_cb: [bool; 2],
        parent_cbf_cr: [bool; 2],
        sink: &mut impl Sink,
    ) -> Result<(), Error> {
        let chroma_type = self.sps.chroma_array_type();
        let split = if log2_size <= self.sps.log2_max_luma_transform_block_size
            && log2_size > self.sps.log2_min_luma_transform_block_size
            && depth < t.max_depth
            && !(t.part_nxn && depth == 0)
        {
            self.flag(ctx::SPLIT_TRANSFORM_FLAG + usize::from(5 - log2_size))
        } else {
            log2_size > self.sps.log2_max_luma_transform_block_size || (t.part_nxn && depth == 0)
        };

        let mut cbf_cb = [false; 2];
        let mut cbf_cr = [false; 2];
        if (log2_size > 2 && chroma_type != 0) || chroma_type == 3 {
            let second = chroma_type == 2 && (!split || log2_size == 3);
            for (cbf, parent) in [(&mut cbf_cb, parent_cbf_cb), (&mut cbf_cr, parent_cbf_cr)] {
                if depth == 0 || parent[0] || parent[1] {
                    cbf[0] = self
                        .engine
                        .decode(&mut self.ctx[ctx::CBF_CHROMA + usize::from(depth)])
                        == 1;
                    if second {
                        cbf[1] = self
                            .engine
                            .decode(&mut self.ctx[ctx::CBF_CHROMA + usize::from(depth)])
                            == 1;
                    }
                }
            }
        }

        if split {
            let half = 1u32 << (log2_size - 1);
            for (i, (dx, dy)) in [(0, 0), (half, 0), (0, half), (half, half)]
                .into_iter()
                .enumerate()
            {
                self.transform_tree(
                    t,
                    x0 + dx,
                    y0 + dy,
                    x0,
                    y0,
                    log2_size - 1,
                    depth + 1,
                    i as u8,
                    cbf_cb,
                    cbf_cr,
                    sink,
                )?;
            }
            Ok(())
        } else {
            // Intra: cbf_luma is always coded.
            let cbf_luma = self.flag(ctx::CBF_LUMA + usize::from(depth == 0));
            self.transform_unit(
                t,
                x0,
                y0,
                x_base,
                y_base,
                log2_size,
                blk_idx,
                cbf_luma,
                cbf_cb,
                cbf_cr,
                parent_cbf_cb,
                parent_cbf_cr,
                sink,
            )
        }
    }

    /// `transform_unit()` (§7.3.8.10).
    #[allow(clippy::too_many_arguments)]
    fn transform_unit(
        &mut self,
        t: &TreeCtx,
        x0: u32,
        y0: u32,
        x_base: u32,
        y_base: u32,
        log2_size: u8,
        blk_idx: u8,
        cbf_luma: bool,
        cbf_cb: [bool; 2],
        cbf_cr: [bool; 2],
        parent_cbf_cb: [bool; 2],
        parent_cbf_cr: [bool; 2],
        sink: &mut impl Sink,
    ) -> Result<(), Error> {
        let chroma_type = self.sps.chroma_array_type();
        let small_420 = chroma_type != 3 && log2_size == 2;
        // Chroma of 4×4 luma blocks (4:2:0 / 4:2:2) is coded with the last of the four blocks.
        let (eff_cb, eff_cr) = if small_420 {
            (parent_cbf_cb, parent_cbf_cr)
        } else {
            (cbf_cb, cbf_cr)
        };
        let cbf_chroma = chroma_type != 0 && (eff_cb[0] || eff_cr[0] || eff_cb[1] || eff_cr[1]);

        if cbf_luma || cbf_chroma {
            if self.pps.diff_cu_qp_delta_depth.is_some() && !self.is_cu_qp_delta_coded {
                let mut abs = 0;
                while abs < 5 && self.flag(ctx::CU_QP_DELTA_ABS + usize::from(abs > 0)) {
                    abs += 1;
                }
                if abs == 5 {
                    abs += self.eg_bypass(0)?;
                }
                let negative = abs > 0 && self.bypass() == 1;
                self.is_cu_qp_delta_coded = true;
                self.cu_qp_delta_val = if negative { -(abs as i32) } else { abs as i32 };
                let b = self.qp_bd_offset;
                if !(-(26 + b / 2)..=25 + b / 2).contains(&self.cu_qp_delta_val) {
                    return Err(Error::Invalid("CuQpDeltaVal out of range"));
                }
            }
            if self.cu_chroma_qp_offset_enabled
                && cbf_chroma
                && !self.cu_transquant_bypass
                && !self.is_cu_chroma_qp_offset_coded
            {
                let list = &self.pps.range_extension.chroma_qp_offset_list;
                let flag = self.flag(ctx::CU_CHROMA_QP_OFFSET_FLAG);
                let mut idx = 0;
                if flag && list.len() > 1 {
                    while idx < list.len() - 1 && self.flag(ctx::CU_CHROMA_QP_OFFSET_IDX) {
                        idx += 1;
                    }
                }
                self.is_cu_chroma_qp_offset_coded = true;
                self.cu_qp_offset = if flag { list[idx] } else { (0, 0) };
            }
        }

        let qp_y = self.current_qp_y();
        let luma_mode = self.pic.intra_mode[self.pic.idx(x0, y0)];
        let b = BlockInfo {
            c_idx: 0,
            x: x0,
            y: y0,
            log2_size,
            intra_mode: luma_mode,
            qp_y,
            cu_qp_offset: 0,
            res_scale: 0,
        };
        self.block(b, cbf_luma, sink)?;

        if chroma_type == 0 {
            return Ok(());
        }
        let (sw, sh) = self.sps.chroma_subsampling();
        let log2_c = if chroma_type == 3 {
            log2_size
        } else {
            log2_size.max(3) - 1
        };
        let (xc, yc) = if small_420 {
            if blk_idx != 3 {
                return Ok(());
            }
            (x_base, y_base)
        } else {
            (x0, y0)
        };
        let pb = if t.part_nxn && chroma_type == 3 {
            let half = 1u32 << (t.log2_cb - 1);
            usize::from(xc >= t.x_cu + half) + 2 * usize::from(yc >= t.y_cu + half)
        } else {
            0
        };
        let chroma_mode = t.chroma_modes[pb];
        let n_sub = if chroma_type == 2 { 2 } else { 1 };
        for (c, cbf) in [(1u8, eff_cb), (2u8, eff_cr)] {
            let res_scale = if self
                .pps
                .range_extension
                .cross_component_prediction_enabled_flag
                && cbf_luma
                && t.chroma_modes[pb] == t.luma_modes[pb]
            {
                self.cross_comp_pred(c - 1)
            } else {
                0
            };
            let offset = if c == 1 {
                self.cu_qp_offset.0
            } else {
                self.cu_qp_offset.1
            };
            for i in 0..n_sub {
                let (x, y) = (xc / sw, yc / sh + (i << log2_c));
                let b = BlockInfo {
                    c_idx: c,
                    x,
                    y,
                    log2_size: log2_c,
                    intra_mode: chroma_mode,
                    qp_y,
                    cu_qp_offset: offset,
                    res_scale,
                };
                self.block(b, cbf[i as usize], sink)?;
            }
        }
        Ok(())
    }

    /// `cross_comp_pred()` (§7.3.8.12): returns `ResScaleVal`.
    fn cross_comp_pred(&mut self, c: u8) -> i8 {
        let base = ctx::LOG2_RES_SCALE_ABS_PLUS1 + 4 * usize::from(c);
        let mut v = 0;
        while v < 4 && self.flag(base + v) {
            v += 1;
        }
        if v == 0 {
            return 0;
        }
        let negative = self.flag(ctx::RES_SCALE_SIGN_FLAG + usize::from(c));
        let s = 1i8 << (v - 1);
        if negative { -s } else { s }
    }

    /// Parses the residual of one block if coded, then emits it.
    fn block(&mut self, b: BlockInfo, cbf: bool, sink: &mut impl Sink) -> Result<(), Error> {
        let n = 1usize << (2 * b.log2_size);
        let transform_skip = cbf && self.residual_coding(b.log2_size, b.c_idx, b.intra_mode)?;
        let coeffs = std::mem::take(&mut self.coeffs);
        sink.transform_block(&TransformBlock {
            c_idx: b.c_idx,
            x: b.x,
            y: b.y,
            log2_size: b.log2_size,
            intra_mode: b.intra_mode,
            coeffs: cbf.then(|| &coeffs[..n]),
            transform_skip,
            transquant_bypass: self.cu_transquant_bypass,
            qp_y: b.qp_y,
            cu_qp_offset: b.cu_qp_offset,
            res_scale_val: b.res_scale,
        });
        self.coeffs = coeffs;
        Ok(())
    }

    /// `residual_coding()` (§7.3.8.11). Fills `self.coeffs[..size²]` and returns
    /// `transform_skip_flag`.
    fn residual_coding(&mut self, log2_size: u8, c_idx: u8, intra_mode: u8) -> Result<bool, Error> {
        let size = 1usize << log2_size;
        self.coeffs[..size * size].fill(0);
        let luma = c_idx == 0;

        let transform_skip = self.pps.transform_skip_enabled_flag
            && !self.cu_transquant_bypass
            && log2_size <= self.pps.range_extension.log2_max_transform_skip_block_size
            && self.flag(if luma {
                ctx::TRANSFORM_SKIP_FLAG_LUMA
            } else {
                ctx::TRANSFORM_SKIP_FLAG_CHROMA
            });

        // last_sig_coeff_{x,y}_prefix (§9.3.4.2.3).
        let (ctx_offset, ctx_shift) = if luma {
            (
                3 * (log2_size - 2) + ((log2_size - 1) >> 2),
                (log2_size + 1) >> 2,
            )
        } else {
            (15, log2_size - 2)
        };
        let c_max = (u32::from(log2_size) << 1) - 1;
        let mut prefix = [0u32; 2];
        for (k, base) in [ctx::LAST_SIG_COEFF_X_PREFIX, ctx::LAST_SIG_COEFF_Y_PREFIX]
            .into_iter()
            .enumerate()
        {
            while prefix[k] < c_max
                && self.flag(base + usize::from(ctx_offset) + (prefix[k] as usize >> ctx_shift))
            {
                prefix[k] += 1;
            }
        }
        let mut last = [0u32; 2];
        for k in 0..2 {
            last[k] = if prefix[k] > 3 {
                let n = (prefix[k] >> 1) - 1;
                let suffix = self.engine.decode_bypass_bits(n);
                (1 << n) * (2 + (prefix[k] & 1)) + suffix
            } else {
                prefix[k]
            };
        }

        // Scan order (§7.4.9.11).
        let scan =
            if log2_size == 2 || (log2_size == 3 && (luma || self.sps.chroma_array_type() == 3)) {
                match intra_mode {
                    6..=14 => ScanType::Vertical,
                    22..=30 => ScanType::Horizontal,
                    _ => ScanType::Diagonal,
                }
            } else {
                ScanType::Diagonal
            };
        if scan == ScanType::Vertical {
            last.swap(0, 1);
        }
        let (last_x, last_y) = (last[0] as u8, last[1] as u8);

        let log2_sb = log2_size - 2;
        let sb_scan = scan_order(log2_sb, scan);
        let pos_scan = scan_order(2, scan);
        let num_sb = 1usize << (2 * log2_sb);

        // Locate the last significant position in scan order.
        let mut last_sub_block = num_sb - 1;
        let mut last_scan_pos = 16;
        loop {
            if last_scan_pos == 0 {
                if last_sub_block == 0 {
                    return Err(Error::Invalid("last significant coefficient not found"));
                }
                last_scan_pos = 16;
                last_sub_block -= 1;
            }
            last_scan_pos -= 1;
            let (xs, ys) = sb_scan[last_sub_block];
            let (xp, yp) = pos_scan[last_scan_pos];
            if (xs << 2) + xp == last_x && (ys << 2) + yp == last_y {
                break;
            }
        }

        let sb_width = 1usize << log2_sb;
        let mut coded_sb = [[false; 8]; 8];
        let sign_hiding_allowed =
            self.pps.sign_data_hiding_enabled_flag && !self.cu_transquant_bypass;
        let ts_ctx = self.sps.range_extension.transform_skip_context_enabled_flag
            && (transform_skip || self.cu_transquant_bypass);
        // `greater1Ctx` of the last coded sub-block; starts at 1 so the first sub-block is unaffected.
        let mut greater1_ctx_state: u8 = 1;

        for i in (0..=last_sub_block).rev() {
            let (xs, ys) = sb_scan[i];
            let (xs, ys) = (usize::from(xs), usize::from(ys));
            let mut infer_sb_dc = false;
            if i < last_sub_block && i > 0 {
                let mut csbf_ctx = 0;
                if xs + 1 < sb_width {
                    csbf_ctx += usize::from(coded_sb[xs + 1][ys]);
                }
                if ys + 1 < sb_width {
                    csbf_ctx += usize::from(coded_sb[xs][ys + 1]);
                }
                let inc = csbf_ctx.min(1) + if luma { 0 } else { 2 };
                coded_sb[xs][ys] = self.flag(ctx::CODED_SUB_BLOCK_FLAG + inc);
                infer_sb_dc = true;
            } else {
                coded_sb[xs][ys] = true;
            }

            // Significant positions, in decreasing scan position.
            let mut sig = [0u8; 16];
            let mut n_sig = 0;
            if i == last_sub_block {
                sig[0] = last_scan_pos as u8;
                n_sig = 1;
            }
            if coded_sb[xs][ys] {
                let prev_csbf = u8::from(xs + 1 < sb_width && coded_sb[xs + 1][ys])
                    | (u8::from(ys + 1 < sb_width && coded_sb[xs][ys + 1]) << 1);
                let start = if i == last_sub_block {
                    last_scan_pos as i32 - 1
                } else {
                    15
                };
                for n in (0..=start).rev() {
                    let n = n as usize;
                    let (xp, yp) = pos_scan[n];
                    let significant = if n > 0 || !infer_sb_dc {
                        let xc = (xs << 2) + usize::from(xp);
                        let yc = (ys << 2) + usize::from(yp);
                        let inc = self
                            .sig_ctx_inc(c_idx, log2_size, xc, yc, xp, yp, prev_csbf, scan, ts_ctx);
                        let s = self.flag(ctx::SIG_COEFF_FLAG + inc);
                        if s {
                            infer_sb_dc = false;
                        }
                        s
                    } else {
                        // DC of a coded sub-block with no other significant coefficient.
                        true
                    };
                    if significant {
                        sig[n_sig] = n as u8;
                        n_sig += 1;
                    }
                }
            }
            if n_sig == 0 {
                continue;
            }

            // coeff_abs_level_greater1_flag (§9.3.4.2.6), first 8 coefficients.
            let mut ctx_set = if i == 0 || !luma { 0 } else { 2 };
            if greater1_ctx_state == 0 {
                ctx_set += 1;
            }
            greater1_ctx_state = 1;
            let mut greater1 = [false; 16];
            let mut first_g1: Option<usize> = None;
            for (k, g1) in greater1.iter_mut().enumerate().take(n_sig.min(8)) {
                let inc = ctx_set * 4
                    + usize::from(greater1_ctx_state.min(3))
                    + if luma { 0 } else { 16 };
                *g1 = self.flag(ctx::COEFF_ABS_LEVEL_GREATER1_FLAG + inc);
                if *g1 {
                    greater1_ctx_state = 0;
                    first_g1.get_or_insert(k);
                } else if greater1_ctx_state > 0 && greater1_ctx_state < 3 {
                    greater1_ctx_state += 1;
                }
            }
            let mut greater2 = false;
            if first_g1.is_some() {
                greater2 = self
                    .flag(ctx::COEFF_ABS_LEVEL_GREATER2_FLAG + ctx_set + if luma { 0 } else { 4 });
            }

            let last_sig_pos = usize::from(sig[0]);
            let first_sig_pos = usize::from(sig[n_sig - 1]);
            let sign_hidden = sign_hiding_allowed && last_sig_pos - first_sig_pos > 3;

            let mut signs = [false; 16];
            for (k, s) in signs.iter_mut().enumerate().take(n_sig) {
                if !(sign_hidden && k == n_sig - 1) {
                    *s = self.bypass() == 1;
                }
            }

            // coeff_abs_level_remaining and final levels.
            let mut rice: u32 = 0;
            let mut sum_abs: i64 = 0;
            for k in 0..n_sig {
                let base = 1 + i32::from(greater1[k]) + i32::from(first_g1 == Some(k) && greater2);
                let threshold = if k < 8 {
                    if first_g1 == Some(k) { 3 } else { 2 }
                } else {
                    1
                };
                let mut abs = base;
                if base == threshold {
                    let rem = self.coeff_abs_level_remaining(rice)?;
                    abs = base
                        .checked_add(rem as i32)
                        .ok_or(Error::Invalid("coefficient overflow"))?;
                    if abs > 3 * (1 << rice) {
                        rice = (rice + 1).min(4);
                    }
                }
                let (xp, yp) = pos_scan[usize::from(sig[k])];
                let xc = (xs << 2) + usize::from(xp);
                let yc = (ys << 2) + usize::from(yp);
                let mut level = if signs[k] { -abs } else { abs };
                if sign_hidden {
                    sum_abs += i64::from(abs);
                    if k == n_sig - 1 && sum_abs % 2 == 1 {
                        level = -level;
                    }
                }
                self.coeffs[yc * size + xc] = level;
            }
        }
        Ok(transform_skip)
    }

    /// Context increment of `sig_coeff_flag` (§9.3.4.2.5).
    #[allow(clippy::too_many_arguments)]
    fn sig_ctx_inc(
        &self,
        c_idx: u8,
        log2_size: u8,
        xc: usize,
        yc: usize,
        xp: u8,
        yp: u8,
        prev_csbf: u8,
        scan: ScanType,
        ts_ctx: bool,
    ) -> usize {
        const CTX_IDX_MAP: [u8; 16] = [0, 1, 4, 5, 2, 3, 4, 5, 6, 6, 8, 8, 7, 7, 8, 8];
        let luma = c_idx == 0;
        let sig_ctx: usize = if ts_ctx {
            if luma { 42 } else { 16 }
        } else if log2_size == 2 {
            usize::from(CTX_IDX_MAP[(yc << 2) + xc])
        } else if xc + yc == 0 {
            0
        } else {
            let (xp, yp) = (usize::from(xp), usize::from(yp));
            let mut s = match prev_csbf {
                0 => {
                    if xp + yp == 0 {
                        2
                    } else if xp + yp < 3 {
                        1
                    } else {
                        0
                    }
                }
                1 => match yp {
                    0 => 2,
                    1 => 1,
                    _ => 0,
                },
                2 => match xp {
                    0 => 2,
                    1 => 1,
                    _ => 0,
                },
                _ => 2,
            };
            if luma {
                if (xc >> 2) + (yc >> 2) > 0 {
                    s += 3;
                }
                s += if log2_size == 3 {
                    if scan == ScanType::Diagonal { 9 } else { 15 }
                } else {
                    21
                };
            } else {
                s += if log2_size == 3 { 9 } else { 12 };
            }
            s
        };
        if luma { sig_ctx } else { 27 + sig_ctx }
    }

    /// `coeff_abs_level_remaining` (§9.3.3.11): Rice prefix/suffix, then Exp-Golomb escape.
    fn coeff_abs_level_remaining(&mut self, rice: u32) -> Result<u32, Error> {
        let mut prefix = 0;
        while self.bypass() == 1 {
            prefix += 1;
            if prefix > 32 {
                return Err(Error::Invalid("coeff_abs_level_remaining prefix too long"));
            }
        }
        if prefix <= 3 {
            Ok((prefix << rice) + self.engine.decode_bypass_bits(rice))
        } else {
            let n = prefix - 3 + rice;
            if n > 32 {
                return Err(Error::Invalid("coeff_abs_level_remaining suffix too long"));
            }
            let base = ((1u64 << (prefix - 3)) + 2) << rice;
            let v = base + u64::from(self.engine.decode_bypass_bits(n));
            u32::try_from(v).map_err(|_| Error::Invalid("coeff_abs_level_remaining overflow"))
        }
    }
}

/// Position and parameters of one transform block.
#[derive(Clone, Copy)]
struct BlockInfo {
    c_idx: u8,
    x: u32,
    y: u32,
    log2_size: u8,
    intra_mode: u8,
    qp_y: i32,
    cu_qp_offset: i8,
    res_scale: i8,
}

/// Values shared by the transform tree of one coding unit.
struct TreeCtx {
    x_cu: u32,
    y_cu: u32,
    log2_cb: u8,
    part_nxn: bool,
    luma_modes: [u8; 4],
    chroma_modes: [u8; 4],
    max_depth: u8,
}
