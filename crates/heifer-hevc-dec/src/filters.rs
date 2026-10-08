//! In-loop filters (H.265 §8.7): deblocking filter, then sample adaptive offset (SAO).
//!
//! Both run on the whole picture after all coding units have been reconstructed. Only intra
//! pictures are supported, so every filtered edge has boundary strength 2.

use crate::params::{Pps, Sps};
use crate::recon::Frame;
use crate::scan::CtbLayout;
use crate::syntax::SaoParams;

/// Per-slice parameters used by the filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceFilterParams {
    /// `slice_deblocking_filter_disabled_flag`.
    pub deblocking_disabled: bool,
    /// `slice_beta_offset_div2`.
    pub beta_offset_div2: i8,
    /// `slice_tc_offset_div2`.
    pub tc_offset_div2: i8,
    /// `slice_loop_filter_across_slices_enabled_flag`.
    pub loop_filter_across_slices: bool,
}

/// Information collected during reconstruction, at 4×4 luma granularity.
#[derive(Debug, Clone)]
pub struct FilterInfo {
    /// Width of the grid (in 4×4 units).
    pub w4: usize,
    /// Height of the grid (in 4×4 units).
    pub h4: usize,
    /// Bit 0: transform/coding block edge on the left of the unit; bit 1: on the top.
    pub edges: Vec<u8>,
    /// `QpY` of the coding unit covering each unit.
    pub qp_y: Vec<i8>,
    /// Samples that must not be modified by filters (PCM with loop filter disabled, or
    /// transquant bypass).
    pub no_filter: Vec<bool>,
    /// Index into `slices` of the slice covering each unit (in decoding order).
    pub slice: Vec<u16>,
    /// Parameters of each slice, in decoding order.
    pub slices: Vec<SliceFilterParams>,
    /// SAO parameters per CTB, raster order.
    pub sao: Vec<SaoParams>,
}

impl FilterInfo {
    /// Creates empty information for a picture.
    pub fn new(sps: &Sps, layout: &CtbLayout) -> Self {
        let w4 = sps.pic_width_in_luma_samples.div_ceil(4) as usize;
        let h4 = sps.pic_height_in_luma_samples.div_ceil(4) as usize;
        let n = w4 * h4;
        Self {
            w4,
            h4,
            edges: vec![0; n],
            qp_y: vec![0; n],
            no_filter: vec![false; n],
            slice: vec![0; n],
            slices: Vec::new(),
            sao: vec![SaoParams::default(); (layout.width * layout.height) as usize],
        }
    }

    fn at(&self, x: u32, y: u32) -> usize {
        (y as usize >> 2) * self.w4 + (x as usize >> 2)
    }
}

/// `β′` for Q = 16..=51 (Table 8-12); 0 below 16.
const BETA_TABLE: [i32; 36] = [
    6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 20, 22, 24, 26, 28, 30, 32, 34, 36, 38, 40, 42,
    44, 46, 48, 50, 52, 54, 56, 58, 60, 62, 64,
];

/// `tC′` for Q = 18..=53 (Table 8-12); 0 below 18.
const TC_TABLE: [i32; 36] = [
    1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 5, 5, 6, 6, 7, 8, 9, 10, 11, 13,
    14, 16, 18, 20, 22, 24,
];

fn beta_prime(q: i32) -> i32 {
    if q < 16 {
        0
    } else {
        BETA_TABLE[(q - 16) as usize]
    }
}

fn tc_prime(q: i32) -> i32 {
    if q < 18 {
        0
    } else {
        TC_TABLE[(q - 18) as usize]
    }
}

/// Chroma QP mapping (Table 8-10) for `ChromaArrayType` 1, `Min(qPi, 51)` otherwise.
pub(crate) fn chroma_qp(qpi: i32, chroma_array_type: u8) -> i32 {
    if chroma_array_type != 1 {
        return qpi.min(51);
    }
    match qpi {
        ..30 => qpi,
        30..=43 => [29, 30, 31, 32, 33, 33, 34, 34, 35, 35, 36, 36, 37, 37][(qpi - 30) as usize],
        _ => qpi - 6,
    }
}

/// Applies the deblocking filter to `frame` in place (§8.7.2).
pub fn deblock(frame: &mut Frame, info: &FilterInfo, sps: &Sps, pps: &Pps, layout: &CtbLayout) {
    for vertical in [true, false] {
        deblock_luma(frame, info, sps, pps, layout, vertical);
        if sps.chroma_array_type() != 0 {
            deblock_chroma(frame, info, sps, pps, layout, vertical);
        }
    }
}

/// Whether the edge between unit `p` (left/above) and unit `q` (the current block) at luma
/// position (`xq`, `yq`) is filtered (§8.7.2.3, filterEdgeFlag).
#[allow(clippy::too_many_arguments)]
fn edge_filtered(
    info: &FilterInfo,
    sps: &Sps,
    pps: &Pps,
    layout: &CtbLayout,
    xq: u32,
    yq: u32,
    xp: u32,
    yp: u32,
) -> bool {
    let (iq, ip) = (info.at(xq, yq), info.at(xp, yp));
    let slice_q = &info.slices[usize::from(info.slice[iq])];
    if slice_q.deblocking_disabled {
        return false;
    }
    if info.slice[iq] != info.slice[ip] && !slice_q.loop_filter_across_slices {
        return false;
    }
    let l = sps.log2_ctb_size;
    let ctb = |x: u32, y: u32| (y >> l) * layout.width + (x >> l);
    if let Some(t) = &pps.tiles
        && !t.loop_filter_across_tiles_enabled_flag
        && layout.tile_of_rs(ctb(xq, yq)) != layout.tile_of_rs(ctb(xp, yp))
    {
        return false;
    }
    true
}

fn deblock_luma(
    frame: &mut Frame,
    info: &FilterInfo,
    sps: &Sps,
    pps: &Pps,
    layout: &CtbLayout,
    vertical: bool,
) {
    let (w, h) = (
        sps.pic_width_in_luma_samples,
        sps.pic_height_in_luma_samples,
    );
    let stride = frame.widths[0] as usize;
    let bit_depth = i32::from(sps.bit_depth_luma);
    let max = (1 << bit_depth) - 1;
    let plane = &mut frame.planes[0];
    let edge_bit = if vertical { 1 } else { 2 };

    // Each segment: 4 lines across an edge on the 8×8 grid.
    let (outer, inner) = if vertical {
        ((0..h).step_by(4), w)
    } else {
        ((0..w).step_by(4), h)
    };
    for a in outer {
        for b in (8..inner).step_by(8) {
            let (xq, yq) = if vertical { (b, a) } else { (a, b) };
            let (xp, yp) = if vertical { (b - 1, a) } else { (a, b - 1) };
            let iq = info.at(xq, yq);
            if info.edges[iq] & edge_bit == 0
                || !edge_filtered(info, sps, pps, layout, xq, yq, xp, yp)
            {
                continue;
            }
            let ip = info.at(xp, yp);
            let qpl = (i32::from(info.qp_y[iq]) + i32::from(info.qp_y[ip]) + 1) >> 1;
            let slice = &info.slices[usize::from(info.slice[iq])];
            let beta = beta_prime((qpl + (i32::from(slice.beta_offset_div2) << 1)).clamp(0, 51))
                << (bit_depth - 8);
            let bs = 2;
            let tc = tc_prime(
                (qpl + 2 * (bs - 1) + (i32::from(slice.tc_offset_div2) << 1)).clamp(0, 53),
            ) << (bit_depth - 8);
            let (no_p, no_q) = (info.no_filter[ip], info.no_filter[iq]);

            // Sample accessor: line k (0..4) across the edge, offset i (-4..4; <0 is the P side).
            let idx = |k: u32, i: i32| -> usize {
                if vertical {
                    ((yq + k) as usize) * stride + (xq as i32 + i) as usize
                } else {
                    ((yq as i32 + i) as usize) * stride + (xq + k) as usize
                }
            };
            let s = |plane: &[u16], k: u32, i: i32| i32::from(plane[idx(k, i)]);

            let dp =
                |plane: &[u16], k| (s(plane, k, -3) - 2 * s(plane, k, -2) + s(plane, k, -1)).abs();
            let dq =
                |plane: &[u16], k| (s(plane, k, 2) - 2 * s(plane, k, 1) + s(plane, k, 0)).abs();
            let (dp0, dp3, dq0, dq3) = (dp(plane, 0), dp(plane, 3), dq(plane, 0), dq(plane, 3));
            let (dpq0, dpq3) = (dp0 + dq0, dp3 + dq3);
            let d = dpq0 + dpq3;
            if d >= beta {
                continue;
            }
            let strong = |plane: &[u16], k, dpq: i32| {
                2 * dpq < (beta >> 2)
                    && (s(plane, k, -4) - s(plane, k, -1)).abs()
                        + (s(plane, k, 0) - s(plane, k, 3)).abs()
                        < (beta >> 3)
                    && (s(plane, k, -1) - s(plane, k, 0)).abs() < ((5 * tc + 1) >> 1)
            };
            let de_strong = strong(plane, 0, dpq0) && strong(plane, 3, dpq3);
            let dep = dp0 + dp3 < ((beta + (beta >> 1)) >> 3);
            let deq = dq0 + dq3 < ((beta + (beta >> 1)) >> 3);

            for k in 0..4 {
                let [p3, p2, p1, p0, q0, q1, q2, q3] =
                    core::array::from_fn(|j| s(plane, k, j as i32 - 4));
                let mut out = [(p0, -1), (q0, 0), (p1, -2), (q1, 1), (p2, -3), (q2, 2)];
                let mut n = 0;
                if de_strong {
                    let c = |v: i32, f: i32| f.clamp(v - 2 * tc, v + 2 * tc);
                    out = [
                        (c(p0, (p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3), -1),
                        (c(q0, (p1 + 2 * p0 + 2 * q0 + 2 * q1 + q2 + 4) >> 3), 0),
                        (c(p1, (p2 + p1 + p0 + q0 + 2) >> 2), -2),
                        (c(q1, (p0 + q0 + q1 + q2 + 2) >> 2), 1),
                        (c(p2, (2 * p3 + 3 * p2 + p1 + p0 + q0 + 4) >> 3), -3),
                        (c(q2, (p0 + q0 + q1 + 3 * q2 + 2 * q3 + 4) >> 3), 2),
                    ];
                    n = 6;
                } else {
                    let mut delta = (9 * (q0 - p0) - 3 * (q1 - p1) + 8) >> 4;
                    if delta.abs() < tc * 10 {
                        delta = delta.clamp(-tc, tc);
                        out[0].0 = (p0 + delta).clamp(0, max);
                        out[1].0 = (q0 - delta).clamp(0, max);
                        if dep {
                            let dp = ((((p2 + p0 + 1) >> 1) - p1 + delta) >> 1)
                                .clamp(-(tc >> 1), tc >> 1);
                            out[2].0 = (p1 + dp).clamp(0, max);
                        }
                        if deq {
                            let dq = ((((q2 + q0 + 1) >> 1) - q1 - delta) >> 1)
                                .clamp(-(tc >> 1), tc >> 1);
                            out[3].0 = (q1 + dq).clamp(0, max);
                        }
                        n = 4;
                    }
                }
                for &(v, i) in &out[..n] {
                    if (i < 0 && no_p) || (i >= 0 && no_q) {
                        continue;
                    }
                    plane[idx(k, i)] = v as u16;
                }
            }
        }
    }
}

fn deblock_chroma(
    frame: &mut Frame,
    info: &FilterInfo,
    sps: &Sps,
    pps: &Pps,
    layout: &CtbLayout,
    vertical: bool,
) {
    let (sw, sh) = sps.chroma_subsampling();
    let (cw, ch) = (frame.widths[1], frame.heights[1]);
    let stride = cw as usize;
    let bit_depth = i32::from(sps.bit_depth_chroma);
    let max = (1 << bit_depth) - 1;
    let edge_bit = if vertical { 1 } else { 2 };
    let chroma_type = sps.chroma_array_type();

    for c in 1..3 {
        let pic_offset = i32::from(if c == 1 {
            pps.pps_cb_qp_offset
        } else {
            pps.pps_cr_qp_offset
        });
        let plane = &mut frame.planes[c];
        // Chroma edges lie on an 8×8 grid of chroma samples.
        let (outer, inner) = if vertical { (ch, cw) } else { (cw, ch) };
        for a in 0..outer {
            for b in (8..inner).step_by(8) {
                let (xc, yc) = if vertical { (b, a) } else { (a, b) };
                let (xq, yq) = (xc * sw, yc * sh);
                let (xp, yp) = if vertical { (xq - 1, yq) } else { (xq, yq - 1) };
                let iq = info.at(xq, yq);
                if info.edges[iq] & edge_bit == 0
                    || !edge_filtered(info, sps, pps, layout, xq, yq, xp, yp)
                {
                    continue;
                }
                let ip = info.at(xp, yp);
                let qpi =
                    ((i32::from(info.qp_y[iq]) + i32::from(info.qp_y[ip]) + 1) >> 1) + pic_offset;
                let qpc = chroma_qp(qpi, chroma_type);
                let slice = &info.slices[usize::from(info.slice[iq])];
                let tc = tc_prime((qpc + 2 + (i32::from(slice.tc_offset_div2) << 1)).clamp(0, 53))
                    << (bit_depth - 8);
                if tc == 0 {
                    continue;
                }
                let idx = |i: i32| -> usize {
                    if vertical {
                        yc as usize * stride + (xc as i32 + i) as usize
                    } else {
                        (yc as i32 + i) as usize * stride + xc as usize
                    }
                };
                let [p1, p0, q0, q1] =
                    core::array::from_fn(|j| i32::from(plane[idx(j as i32 - 2)]));
                let delta = ((((q0 - p0) << 2) + p1 - q1 + 4) >> 3).clamp(-tc, tc);
                if !info.no_filter[ip] {
                    plane[idx(-1)] = (p0 + delta).clamp(0, max) as u16;
                }
                if !info.no_filter[iq] {
                    plane[idx(0)] = (q0 - delta).clamp(0, max) as u16;
                }
            }
        }
    }
}

/// Applies SAO to `frame` in place (§8.7.3). `frame` must already be deblocked.
pub fn sao(frame: &mut Frame, info: &FilterInfo, sps: &Sps, pps: &Pps, layout: &CtbLayout) {
    let deblocked = frame.clone();
    let num_comps = if sps.chroma_array_type() != 0 { 3 } else { 1 };
    let ctb = sps.ctb_size();
    for c in 0..num_comps {
        let (sw, sh) = if c == 0 {
            (1, 1)
        } else {
            sps.chroma_subsampling()
        };
        let (pw, ph) = (frame.widths[c], frame.heights[c]);
        let stride = pw as usize;
        let bit_depth = if c == 0 {
            sps.bit_depth_luma
        } else {
            sps.bit_depth_chroma
        };
        let max = (1i32 << bit_depth) - 1;
        let src = &deblocked.planes[c];
        let dst = &mut frame.planes[c];
        for rs in 0..layout.width * layout.height {
            let params = &info.sao[rs as usize];
            let type_idx = params.type_idx[c];
            if type_idx == 0 {
                continue;
            }
            let x0 = (rs % layout.width) * ctb / sw;
            let y0 = (rs / layout.width) * ctb / sh;
            let (x1, y1) = ((x0 + ctb / sw).min(pw), (y0 + ctb / sh).min(ph));
            let offsets = params.offsets[c];
            if type_idx == 1 {
                let shift = bit_depth - 5;
                let band = params.band_position[c];
                for y in y0..y1 {
                    for x in x0..x1 {
                        if info.no_filter[info.at(x * sw, y * sh)] {
                            continue;
                        }
                        let i = y as usize * stride + x as usize;
                        let v = i32::from(src[i]);
                        let k = ((v >> shift) as u8).wrapping_sub(band) & 31;
                        if k < 4 {
                            dst[i] = (v + i32::from(offsets[usize::from(k)])).clamp(0, max) as u16;
                        }
                    }
                }
            } else {
                let (dx, dy): (i32, i32) = match params.eo_class[c] {
                    0 => (1, 0),
                    1 => (0, 1),
                    2 => (1, 1),
                    _ => (-1, 1),
                };
                for y in y0..y1 {
                    for x in x0..x1 {
                        let (xl, yl) = (x * sw, y * sh);
                        if info.no_filter[info.at(xl, yl)] {
                            continue;
                        }
                        let (ax, ay) = (x as i32 - dx, y as i32 - dy);
                        let (bx, by) = (x as i32 + dx, y as i32 + dy);
                        let inside = |px: i32, py: i32| {
                            px >= 0 && py >= 0 && px < pw as i32 && py < ph as i32
                        };
                        if !inside(ax, ay) || !inside(bx, by) {
                            continue;
                        }
                        if !sao_neighbour_ok(
                            info,
                            sps,
                            pps,
                            layout,
                            xl,
                            yl,
                            ax as u32 * sw,
                            ay as u32 * sh,
                        ) || !sao_neighbour_ok(
                            info,
                            sps,
                            pps,
                            layout,
                            xl,
                            yl,
                            bx as u32 * sw,
                            by as u32 * sh,
                        ) {
                            continue;
                        }
                        let i = y as usize * stride + x as usize;
                        let v = i32::from(src[i]);
                        let a = i32::from(src[ay as usize * stride + ax as usize]);
                        let b = i32::from(src[by as usize * stride + bx as usize]);
                        let edge = 2 + (v - a).signum() + (v - b).signum();
                        // edgeIdx 0, 1, 2 are remapped to 1, 2, 0 (§8.7.3.2).
                        let edge_idx = match edge {
                            0 => 1,
                            1 => 2,
                            2 => 0,
                            e => e,
                        };
                        if edge_idx != 0 {
                            dst[i] = (v + i32::from(offsets[edge_idx as usize - 1])).clamp(0, max)
                                as u16;
                        }
                    }
                }
            }
        }
    }
}

/// Whether a neighbouring sample may be used by edge offset (§8.7.3.2): same slice, or the
/// slice decoded later allows filtering across slices; same tile, or tiles allow it.
#[allow(clippy::too_many_arguments)]
fn sao_neighbour_ok(
    info: &FilterInfo,
    sps: &Sps,
    pps: &Pps,
    layout: &CtbLayout,
    x: u32,
    y: u32,
    xn: u32,
    yn: u32,
) -> bool {
    let (s, sn) = (info.slice[info.at(x, y)], info.slice[info.at(xn, yn)]);
    if s != sn {
        let later = s.max(sn);
        if !info.slices[usize::from(later)].loop_filter_across_slices {
            return false;
        }
    }
    let l = sps.log2_ctb_size;
    let ctb = |x: u32, y: u32| (y >> l) * layout.width + (x >> l);
    if let Some(t) = &pps.tiles
        && !t.loop_filter_across_tiles_enabled_flag
        && layout.tile_of_rs(ctb(x, y)) != layout.tile_of_rs(ctb(xn, yn))
    {
        return false;
    }
    true
}
