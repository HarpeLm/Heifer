//! In-loop filters (H.265 §8.7): deblocking filter, then sample adaptive offset (SAO).
//!
//! Both run on the whole picture after all coding units have been reconstructed. The boundary
//! strength of each edge (§8.7.2.4) comes from the prediction mode, the coded residual and the
//! motion on both sides.

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
    /// Bit 0: transform/coding block edge on the left of the unit; bit 1: on the top. Bits 2
    /// and 3: the same for prediction block edges.
    pub edges: Vec<u8>,
    /// The unit belongs to an intra coding unit.
    pub intra: Vec<bool>,
    /// The unit belongs to a luma transform block with non-zero coefficients.
    pub coded: Vec<bool>,
    /// Motion of the unit, for the boundary strength of inter blocks.
    pub motion: Vec<EdgeMotion>,
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
            intra: vec![true; n],
            coded: vec![false; n],
            motion: vec![EdgeMotion::default(); n],
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

/// Motion of a 4×4 unit as the deblocking filter compares it: the vectors and the POC of the
/// pictures they point to.
pub use crate::inter::ColMotion as EdgeMotion;

/// Boundary strength of the edge between units `p` and `iq` (§8.7.2.4); `transform_edge`
/// tells whether the edge is a transform block edge.
#[inline]
fn boundary_strength(info: &FilterInfo, ip: usize, iq: usize, transform_edge: bool) -> i32 {
    if info.intra[ip] || info.intra[iq] {
        return 2;
    }
    if transform_edge && (info.coded[ip] || info.coded[iq]) {
        return 1;
    }
    motion_strength(info.motion[ip], info.motion[iq])
}

/// Boundary strength between two inter blocks from their motion (§8.7.2.4): 1 when they use
/// different reference pictures, a different number of vectors, or vectors at least one
/// integer sample apart; 0 otherwise.
#[inline]
fn motion_strength(p: EdgeMotion, q: EdgeMotion) -> i32 {
    let far = |a: [i16; 2], b: [i16; 2]| {
        (i32::from(a[0]) - i32::from(b[0])).abs() >= 4
            || (i32::from(a[1]) - i32::from(b[1])).abs() >= 4
    };
    // The (POC, vector) pairs of the lists used, without allocating.
    let vectors = |m: &EdgeMotion| {
        let mut v = [(0, [0i16; 2]); 2];
        let mut n = 0;
        for l in 0..2 {
            if let Some(poc) = m.ref_poc[l] {
                v[n] = (poc, m.mv[l]);
                n += 1;
            }
        }
        (v, n)
    };
    let ((vp, np), (vq, nq)) = (vectors(&p), vectors(&q));
    match (&vp[..np], &vq[..nq]) {
        ([a], [b]) => i32::from(a.0 != b.0 || far(a.1, b.1)),
        ([a0, a1], [b0, b1]) => {
            let mut pocs_p = [a0.0, a1.0];
            let mut pocs_q = [b0.0, b1.0];
            pocs_p.sort_unstable();
            pocs_q.sort_unstable();
            if pocs_p != pocs_q {
                return 1;
            }
            if a0.0 != a1.0 {
                // Two different pictures: compare the vectors pointing at the same one.
                let (q_same, q_other) = if a0.0 == b0.0 { (b0, b1) } else { (b1, b0) };
                i32::from(far(a0.1, q_same.1) || far(a1.1, q_other.1))
            } else {
                // Both vectors point at the same picture: either pairing may match.
                i32::from(
                    (far(a0.1, b0.1) || far(a1.1, b1.1)) && (far(a0.1, b1.1) || far(a1.1, b0.1)),
                )
            }
        }
        _ => 1,
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
    if info.slices.len() == 1 && info.slices[0].deblocking_disabled {
        return;
    }
    deblock_luma::<true>(frame, info, sps, pps, layout);
    deblock_luma::<false>(frame, info, sps, pps, layout);
    if sps.chroma_array_type() != 0 {
        deblock_chroma::<true>(frame, info, sps, pps, layout);
        deblock_chroma::<false>(frame, info, sps, pps, layout);
    }
}

/// Whether the edge between unit `p` (left/above) and unit `q` (the current block) at luma
/// position (`xq`, `yq`) is filtered (§8.7.2.3, filterEdgeFlag).
#[allow(clippy::too_many_arguments)]
#[inline]
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

/// Applies the luma deblocking filter (§8.7.2.4) across vertical (`VERTICAL`) or horizontal
/// edges.
///
/// `VERTICAL` is a compile-time constant so both the sample addressing and the outer/inner
/// cache-friendly row-major traversal collapse with no per-sample branch.
fn deblock_luma<const VERTICAL: bool>(
    frame: &mut Frame,
    info: &FilterInfo,
    sps: &Sps,
    pps: &Pps,
    layout: &CtbLayout,
) {
    let (w, h) = (
        sps.pic_width_in_luma_samples,
        sps.pic_height_in_luma_samples,
    );
    let stride = frame.widths[0] as usize;
    let bit_depth = i32::from(sps.bit_depth_luma);
    let max = (1 << bit_depth) - 1;
    let plane = &mut frame.planes[0];
    let edge_bit = if VERTICAL { 1 } else { 2 };
    let any_edge = edge_bit | (edge_bit << 2);
    let all_edges_filtered = info.slices.len() == 1
        && pps
            .tiles
            .as_ref()
            .is_none_or(|t| t.loop_filter_across_tiles_enabled_flag);

    // Each segment: 4 lines across an edge on the 8×8 grid, traversed in row-major order.
    let (y_start, y_step, x_start, x_step) = if VERTICAL { (0, 4, 8, 8) } else { (8, 8, 0, 4) };
    for yq in (y_start..h).step_by(y_step) {
        for xq in (x_start..w).step_by(x_step) {
            let (xp, yp) = if VERTICAL { (xq - 1, yq) } else { (xq, yq - 1) };
            let iq = info.at(xq, yq);
            if info.edges[iq] & any_edge == 0
                || (!all_edges_filtered && !edge_filtered(info, sps, pps, layout, xq, yq, xp, yp))
            {
                continue;
            }
            let ip = info.at(xp, yp);
            let bs = boundary_strength(info, ip, iq, info.edges[iq] & edge_bit != 0);
            if bs == 0 {
                continue;
            }
            let qpl = (i32::from(info.qp_y[iq]) + i32::from(info.qp_y[ip]) + 1) >> 1;
            let slice = &info.slices[usize::from(info.slice[iq])];
            let beta = beta_prime((qpl + (i32::from(slice.beta_offset_div2) << 1)).clamp(0, 51))
                << (bit_depth - 8);
            let tc = tc_prime(
                (qpl + 2 * (bs - 1) + (i32::from(slice.tc_offset_div2) << 1)).clamp(0, 53),
            ) << (bit_depth - 8);
            if beta == 0 || tc == 0 {
                continue;
            }
            let (no_p, no_q) = (info.no_filter[ip], info.no_filter[iq]);

            // Sample p3..q3 of line `k` runs across the edge: `base` is p3 of line 0, and the
            // two strides walk along the edge and from one line to the next.
            let (base, step_i, step_k) = if VERTICAL {
                (yq as usize * stride + (xq as usize - 4), 1usize, stride)
            } else {
                ((yq as usize - 4) * stride + xq as usize, stride, 1usize)
            };
            let load = |plane: &[u16], k: usize| -> [i32; 8] {
                let b = base + k * step_k;
                if VERTICAL {
                    let row = &plane[b..b + 8];
                    [
                        i32::from(row[0]),
                        i32::from(row[1]),
                        i32::from(row[2]),
                        i32::from(row[3]),
                        i32::from(row[4]),
                        i32::from(row[5]),
                        i32::from(row[6]),
                        i32::from(row[7]),
                    ]
                } else {
                    core::array::from_fn(|j| i32::from(plane[b + j * step_i]))
                }
            };
            let l0 = load(plane, 0);
            let l3 = load(plane, 3);
            let dp0 = (l0[1] - 2 * l0[2] + l0[3]).abs();
            let dq0 = (l0[6] - 2 * l0[5] + l0[4]).abs();
            let dp3 = (l3[1] - 2 * l3[2] + l3[3]).abs();
            let dq3 = (l3[6] - 2 * l3[5] + l3[4]).abs();
            let (dpq0, dpq3) = (dp0 + dq0, dp3 + dq3);
            if dpq0 + dpq3 >= beta {
                continue;
            }
            let strong = |line: &[i32; 8], dpq: i32| {
                2 * dpq < (beta >> 2)
                    && (line[0] - line[3]).abs() + (line[4] - line[7]).abs() < (beta >> 3)
                    && (line[3] - line[4]).abs() < ((5 * tc + 1) >> 1)
            };
            let de_strong = strong(&l0, dpq0) && strong(&l3, dpq3);
            let dep = dp0 + dp3 < ((beta + (beta >> 1)) >> 3);
            let deq = dq0 + dq3 < ((beta + (beta >> 1)) >> 3);

            for k in 0..4 {
                let [p3, p2, p1, p0, q0, q1, q2, q3] = match k {
                    0 => l0,
                    3 => l3,
                    _ => load(plane, k),
                };
                let b = base + k * step_k;
                if de_strong {
                    let c = |v: i32, f: i32| f.clamp(v - 2 * tc, v + 2 * tc) as u16;
                    let np0 = c(p0, (p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3);
                    let np1 = c(p1, (p2 + p1 + p0 + q0 + 2) >> 2);
                    let np2 = c(p2, (2 * p3 + 3 * p2 + p1 + p0 + q0 + 4) >> 3);
                    let nq0 = c(q0, (p1 + 2 * p0 + 2 * q0 + 2 * q1 + q2 + 4) >> 3);
                    let nq1 = c(q1, (p0 + q0 + q1 + q2 + 2) >> 2);
                    let nq2 = c(q2, (p0 + q0 + q1 + 3 * q2 + 2 * q3 + 4) >> 3);
                    if VERTICAL {
                        let row = &mut plane[b..b + 8];
                        if !no_p {
                            row[1] = np2;
                            row[2] = np1;
                            row[3] = np0;
                        }
                        if !no_q {
                            row[4] = nq0;
                            row[5] = nq1;
                            row[6] = nq2;
                        }
                    } else {
                        if !no_p {
                            plane[b + step_i] = np2;
                            plane[b + 2 * step_i] = np1;
                            plane[b + 3 * step_i] = np0;
                        }
                        if !no_q {
                            plane[b + 4 * step_i] = nq0;
                            plane[b + 5 * step_i] = nq1;
                            plane[b + 6 * step_i] = nq2;
                        }
                    }
                } else {
                    let mut delta = (9 * (q0 - p0) - 3 * (q1 - p1) + 8) >> 4;
                    if delta.abs() < tc * 10 {
                        delta = delta.clamp(-tc, tc);
                        if !no_p {
                            let np0 = (p0 + delta).clamp(0, max) as u16;
                            plane[b + 3 * step_i] = np0;
                            if dep {
                                let dp = ((((p2 + p0 + 1) >> 1) - p1 + delta) >> 1)
                                    .clamp(-(tc >> 1), tc >> 1);
                                plane[b + 2 * step_i] = (p1 + dp).clamp(0, max) as u16;
                            }
                        }
                        if !no_q {
                            let nq0 = (q0 - delta).clamp(0, max) as u16;
                            plane[b + 4 * step_i] = nq0;
                            if deq {
                                let dq = ((((q2 + q0 + 1) >> 1) - q1 - delta) >> 1)
                                    .clamp(-(tc >> 1), tc >> 1);
                                plane[b + 5 * step_i] = (q1 + dq).clamp(0, max) as u16;
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Applies the chroma deblocking filter across vertical (`VERTICAL`) or horizontal edges.
fn deblock_chroma<const VERTICAL: bool>(
    frame: &mut Frame,
    info: &FilterInfo,
    sps: &Sps,
    pps: &Pps,
    layout: &CtbLayout,
) {
    let (sw, sh) = sps.chroma_subsampling();
    let (cw, ch) = (frame.widths[1], frame.heights[1]);
    let stride = cw as usize;
    let bit_depth = i32::from(sps.bit_depth_chroma);
    let max = (1 << bit_depth) - 1;
    let edge_bit = if VERTICAL { 1 } else { 2 };
    let any_edge = edge_bit | (edge_bit << 2);
    let chroma_type = sps.chroma_array_type();
    let all_edges_filtered = info.slices.len() == 1
        && pps
            .tiles
            .as_ref()
            .is_none_or(|t| t.loop_filter_across_tiles_enabled_flag);

    let (yc_start, yc_step, xc_start, xc_step) = if VERTICAL { (0, 1, 8, 8) } else { (8, 8, 0, 1) };
    for (c, plane) in frame.planes.iter_mut().enumerate().skip(1) {
        let pic_offset = i32::from(if c == 1 {
            pps.pps_cb_qp_offset
        } else {
            pps.pps_cr_qp_offset
        });
        // Chroma edges lie on an 8×8 grid of chroma samples, traversed row-major.
        for yc in (yc_start..ch).step_by(yc_step) {
            for xc in (xc_start..cw).step_by(xc_step) {
                let (xq, yq) = (xc * sw, yc * sh);
                let (xp, yp) = if VERTICAL { (xq - 1, yq) } else { (xq, yq - 1) };
                let iq = info.at(xq, yq);
                if info.edges[iq] & any_edge == 0
                    || (!all_edges_filtered
                        && !edge_filtered(info, sps, pps, layout, xq, yq, xp, yp))
                {
                    continue;
                }
                let ip = info.at(xp, yp);
                // Chroma is only filtered across edges of intra blocks (bS = 2).
                if !info.intra[ip] && !info.intra[iq] {
                    continue;
                }
                let qpi =
                    ((i32::from(info.qp_y[iq]) + i32::from(info.qp_y[ip]) + 1) >> 1) + pic_offset;
                let qpc = chroma_qp(qpi, chroma_type);
                let slice = &info.slices[usize::from(info.slice[iq])];
                let tc = tc_prime((qpc + 2 + (i32::from(slice.tc_offset_div2) << 1)).clamp(0, 53))
                    << (bit_depth - 8);
                if tc == 0 {
                    continue;
                }
                // p1, p0, q0, q1 are two samples each side of the edge along `step`.
                let (base, step) = if VERTICAL {
                    (yc as usize * stride + (xc as usize - 2), 1usize)
                } else {
                    ((yc as usize - 2) * stride + xc as usize, stride)
                };
                let (p1, p0, q0, q1) = if VERTICAL {
                    let s = &plane[base..base + 4];
                    (
                        i32::from(s[0]),
                        i32::from(s[1]),
                        i32::from(s[2]),
                        i32::from(s[3]),
                    )
                } else {
                    (
                        i32::from(plane[base]),
                        i32::from(plane[base + step]),
                        i32::from(plane[base + 2 * step]),
                        i32::from(plane[base + 3 * step]),
                    )
                };
                let delta = ((((q0 - p0) << 2) + p1 - q1 + 4) >> 3).clamp(-tc, tc);
                if !info.no_filter[ip] {
                    plane[base + step] = (p0 + delta).clamp(0, max) as u16;
                }
                if !info.no_filter[iq] {
                    plane[base + 2 * step] = (q0 - delta).clamp(0, max) as u16;
                }
            }
        }
    }
}

/// Applies SAO to `frame` in place (§8.7.3). `frame` must already be deblocked.
///
/// Band offset only reads the sample it writes, so a plane that uses it exclusively is processed
/// in place. Edge offset reads neighbouring samples of the *pre-SAO* picture, so a plane that
/// uses it is copied first — and only then.
pub fn sao(frame: &mut Frame, info: &FilterInfo, sps: &Sps, pps: &Pps, layout: &CtbLayout) {
    let num_comps = if sps.chroma_array_type() != 0 { 3 } else { 1 };
    let ctb = sps.ctb_size();
    let num_ctb = (layout.width * layout.height) as usize;
    for c in 0..num_comps {
        let mut any_sao = false;
        let mut has_edge = false;
        for p in &info.sao[..num_ctb] {
            let t = p.type_idx[c];
            any_sao |= t != 0;
            has_edge |= t == 2;
        }
        if !any_sao {
            continue;
        }
        let (pw, ph) = (frame.widths[c], frame.heights[c]);
        if has_edge {
            let src = frame.planes[c].clone();
            sao_plane(
                Some(&src),
                &mut frame.planes[c],
                c,
                info,
                sps,
                pps,
                layout,
                ctb,
                pw,
                ph,
            );
        } else {
            sao_plane(
                None,
                &mut frame.planes[c],
                c,
                info,
                sps,
                pps,
                layout,
                ctb,
                pw,
                ph,
            );
        }
    }
}

/// Applies SAO to one colour plane. `src` is the pre-SAO copy used by edge offset; `None` means
/// the plane holds no edge-offset CTB and may be filtered in place.
#[allow(clippy::too_many_arguments)]
fn sao_plane(
    src: Option<&[u16]>,
    dst: &mut [u16],
    c: usize,
    info: &FilterInfo,
    sps: &Sps,
    pps: &Pps,
    layout: &CtbLayout,
    ctb: u32,
    pw: u32,
    ph: u32,
) {
    let (sw, sh) = if c == 0 {
        (1, 1)
    } else {
        sps.chroma_subsampling()
    };
    let stride = pw as usize;
    let bit_depth = if c == 0 {
        sps.bit_depth_luma
    } else {
        sps.bit_depth_chroma
    };
    let max = (1i32 << bit_depth) - 1;
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
        let clean = (y0 * sh..y1 * sh).step_by(4).all(|yl| {
            (x0 * sw..x1 * sw)
                .step_by(4)
                .all(|xl| !info.no_filter[info.at(xl, yl)])
        });
        if type_idx == 1 {
            let shift = bit_depth - 5;
            let band = params.band_position[c];
            if clean {
                let (xs, xe) = (x0 as usize, x1 as usize);
                for y in y0..y1 {
                    let row = y as usize * stride;
                    if let Some(src) = src {
                        for (d, &sv) in dst[row + xs..row + xe]
                            .iter_mut()
                            .zip(&src[row + xs..row + xe])
                        {
                            let v = i32::from(sv);
                            let k = ((v >> shift) as u8).wrapping_sub(band) & 31;
                            if k < 4 {
                                *d = (v + i32::from(offsets[usize::from(k)])).clamp(0, max) as u16;
                            }
                        }
                    } else {
                        for d in &mut dst[row + xs..row + xe] {
                            let v = i32::from(*d);
                            let k = ((v >> shift) as u8).wrapping_sub(band) & 31;
                            if k < 4 {
                                *d = (v + i32::from(offsets[usize::from(k)])).clamp(0, max) as u16;
                            }
                        }
                    }
                }
            } else {
                for y in y0..y1 {
                    for x in x0..x1 {
                        if info.no_filter[info.at(x * sw, y * sh)] {
                            continue;
                        }
                        let i = y as usize * stride + x as usize;
                        let v = i32::from(src.map_or(dst[i], |s| s[i]));
                        let k = ((v >> shift) as u8).wrapping_sub(band) & 31;
                        if k < 4 {
                            dst[i] = (v + i32::from(offsets[usize::from(k)])).clamp(0, max) as u16;
                        }
                    }
                }
            }
        } else {
            let src = src.expect("edge offset needs the pre-SAO picture");
            let (dx, dy): (i32, i32) = match params.eo_class[c] {
                0 => (1, 0),
                1 => (0, 1),
                2 => (1, 1),
                _ => (-1, 1),
            };
            // Distance, in the plane, from a sample to its neighbour b (and from a to the sample).
            let step = dy as isize * stride as isize + dx as isize;
            let eo = [offsets[0], offsets[1], 0, offsets[2], offsets[3]];
            for y in y0..y1 {
                if clean && y > y0 && y + 1 < y1 && x1 > x0 + 2 {
                    let row = y as usize * stride;
                    let (xs, xe) = ((x0 + 1) as usize, (x1 - 1) as usize);
                    let len = xe - xs;
                    let a_start = ((row + xs) as isize - step) as usize;
                    let b_start = ((row + xs) as isize + step) as usize;
                    for (((d, &v), &a), &b) in dst[row + xs..row + xe]
                        .iter_mut()
                        .zip(&src[row + xs..row + xe])
                        .zip(&src[a_start..a_start + len])
                        .zip(&src[b_start..b_start + len])
                    {
                        let (v, a, b) = (i32::from(v), i32::from(a), i32::from(b));
                        let edge = (2 + (v - a).signum() + (v - b).signum()) as usize;
                        let off = eo[edge];
                        if off != 0 {
                            *d = (v + i32::from(off)).clamp(0, max) as u16;
                        }
                    }
                }
                for x in x0..x1 {
                    if clean && y > y0 && y + 1 < y1 && x > x0 && x + 1 < x1 {
                        continue;
                    }
                    let (xl, yl) = (x * sw, y * sh);
                    if info.no_filter[info.at(xl, yl)] {
                        continue;
                    }
                    let (ax, ay) = (x as i32 - dx, y as i32 - dy);
                    let (bx, by) = (x as i32 + dx, y as i32 + dy);
                    let inside =
                        |px: i32, py: i32| px >= 0 && py >= 0 && px < pw as i32 && py < ph as i32;
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
                    let edge = (2 + (v - a).signum() + (v - b).signum()) as usize;
                    let off = eo[edge];
                    if off != 0 {
                        dst[i] = (v + i32::from(off)).clamp(0, max) as u16;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn m(l0: Option<(i32, [i16; 2])>, l1: Option<(i32, [i16; 2])>) -> EdgeMotion {
        EdgeMotion {
            mv: [l0.map_or([0; 2], |v| v.1), l1.map_or([0; 2], |v| v.1)],
            ref_poc: [l0.map(|v| v.0), l1.map(|v| v.0)],
            ref_lt: [false; 2],
        }
    }

    #[test]
    fn bi_predicted_edges() {
        let bi = m(Some((0, [0, 0])), Some((8, [16, 0])));
        // Same pictures in the other list order, vectors paired by picture.
        assert_eq!(
            motion_strength(bi, m(Some((8, [17, 0])), Some((0, [3, 0])))),
            0
        );
        assert_eq!(
            motion_strength(bi, m(Some((8, [20, 0])), Some((0, [0, 0])))),
            1
        );
        // Another picture, or one vector against two.
        assert_eq!(
            motion_strength(bi, m(Some((0, [0, 0])), Some((4, [16, 0])))),
            1
        );
        assert_eq!(motion_strength(bi, m(Some((0, [0, 0])), None)), 1);
        // Both vectors on one picture: either pairing may match.
        let same = m(Some((0, [0, 0])), Some((0, [8, 0])));
        assert_eq!(
            motion_strength(same, m(Some((0, [8, 0])), Some((0, [0, 0])))),
            0
        );
        assert_eq!(
            motion_strength(same, m(Some((0, [8, 0])), Some((0, [4, 0])))),
            1
        );
        // Uni-prediction from either list compares picture and vector.
        assert_eq!(
            motion_strength(m(None, Some((8, [1, 1]))), m(Some((8, [0, 0])), None)),
            0
        );
    }
}
