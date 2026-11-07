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

/// Boundary strength of the edge between units `ip` and `iq` (§8.7.2.4); `transform_edge`
/// tells whether the edge is a transform block edge.
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
fn motion_strength(p: EdgeMotion, q: EdgeMotion) -> i32 {
    let far = |a: [i16; 2], b: [i16; 2]| {
        (i32::from(a[0]) - i32::from(b[0])).abs() >= 4
            || (i32::from(a[1]) - i32::from(b[1])).abs() >= 4
    };
    let vectors = |m: &EdgeMotion| {
        (0..2)
            .filter(|&l| m.ref_poc[l].is_some())
            .map(|l| (m.ref_poc[l].unwrap_or(0), m.mv[l]))
            .collect::<Vec<_>>()
    };
    let (vp, vq) = (vectors(&p), vectors(&q));
    match (vp.as_slice(), vq.as_slice()) {
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
/// `VERTICAL` is a compile-time constant so the sample addressing below collapses to a base
/// offset and two strides, with no per-sample branch.
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

    // Each segment: 4 lines across an edge on the 8×8 grid.
    let (outer, inner) = if VERTICAL { (h, w) } else { (w, h) };
    for a in (0..outer).step_by(4) {
        for b in (8..inner).step_by(8) {
            let (xq, yq) = if VERTICAL { (b, a) } else { (a, b) };
            let (xp, yp) = if VERTICAL { (b - 1, a) } else { (a, b - 1) };
            let iq = info.at(xq, yq);
            if info.edges[iq] & any_edge == 0
                || !edge_filtered(info, sps, pps, layout, xq, yq, xp, yp)
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
            let (no_p, no_q) = (info.no_filter[ip], info.no_filter[iq]);

            // Sample p3..q3 of line `k` runs across the edge: `base` is p3 of line 0, and the
            // two strides walk along the edge and from one line to the next.
            let (base, step_i, step_k) = if VERTICAL {
                (yq as usize * stride + (xq as usize - 4), 1usize, stride)
            } else {
                ((yq as usize - 4) * stride + xq as usize, stride, 1usize)
            };
            let at = |k: usize, i: isize| base + k * step_k + (i + 4) as usize * step_i;
            let s = |plane: &[u16], k: usize, i: isize| i32::from(plane[at(k, i)]);

            let dp = |plane: &[u16], k: usize| {
                (s(plane, k, -3) - 2 * s(plane, k, -2) + s(plane, k, -1)).abs()
            };
            let dq = |plane: &[u16], k: usize| {
                (s(plane, k, 2) - 2 * s(plane, k, 1) + s(plane, k, 0)).abs()
            };
            let (dp0, dp3, dq0, dq3) = (dp(plane, 0), dp(plane, 3), dq(plane, 0), dq(plane, 3));
            let (dpq0, dpq3) = (dp0 + dq0, dp3 + dq3);
            let d = dpq0 + dpq3;
            if d >= beta {
                continue;
            }
            let strong = |plane: &[u16], k: usize, dpq: i32| {
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
                    core::array::from_fn(|j| s(plane, k, j as isize - 4));
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
                    plane[at(k, i)] = v as u16;
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
    let chroma_type = sps.chroma_array_type();

    for c in 1..3 {
        let pic_offset = i32::from(if c == 1 {
            pps.pps_cb_qp_offset
        } else {
            pps.pps_cr_qp_offset
        });
        let plane = &mut frame.planes[c];
        // Chroma edges lie on an 8×8 grid of chroma samples.
        let (outer, inner) = if VERTICAL { (ch, cw) } else { (cw, ch) };
        for a in 0..outer {
            for b in (8..inner).step_by(8) {
                let (xc, yc) = if VERTICAL { (b, a) } else { (a, b) };
                let (xq, yq) = (xc * sw, yc * sh);
                let (xp, yp) = if VERTICAL { (xq - 1, yq) } else { (xq, yq - 1) };
                let iq = info.at(xq, yq);
                if info.edges[iq] & (edge_bit | (edge_bit << 2)) == 0
                    || !edge_filtered(info, sps, pps, layout, xq, yq, xp, yp)
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
                let at = |i: isize| base + (i + 2) as usize * step;
                let [p1, p0, q0, q1] =
                    core::array::from_fn(|j| i32::from(plane[at(j as isize - 2)]));
                let delta = ((((q0 - p0) << 2) + p1 - q1 + 4) >> 3).clamp(-tc, tc);
                if !info.no_filter[ip] {
                    plane[at(-1)] = (p0 + delta).clamp(0, max) as u16;
                }
                if !info.no_filter[iq] {
                    plane[at(0)] = (q0 - delta).clamp(0, max) as u16;
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
        let has_edge = info.sao[..num_ctb].iter().any(|p| p.type_idx[c] == 2);
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
        if type_idx == 1 {
            let shift = bit_depth - 5;
            let band = params.band_position[c];
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
        } else {
            let src = src.expect("edge offset needs the pre-SAO picture");
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
                    let edge = 2 + (v - a).signum() + (v - b).signum();
                    // edgeIdx 0, 1, 2 are remapped to 1, 2, 0 (§8.7.3.2).
                    let edge_idx = match edge {
                        0 => 1,
                        1 => 2,
                        2 => 0,
                        e => e,
                    };
                    if edge_idx != 0 {
                        dst[i] =
                            (v + i32::from(offsets[edge_idx as usize - 1])).clamp(0, max) as u16;
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
