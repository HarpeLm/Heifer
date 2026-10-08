//! Picture reconstruction: intra prediction (§8.4.4.2), scaling and inverse transforms
//! (§8.6), and adding the residual. In-loop filters (deblocking, SAO) are applied afterwards.

use crate::Error;
use crate::filters::{self, FilterInfo, SliceFilterParams};
use crate::params::{Pps, ScalingList, Sps};
use crate::scan::{CtbLayout, ScanType, scan_order};
use crate::syntax::{CodingUnit, SaoParams, Sink, TransformBlock};

/// A decoded picture: three planes of samples (only the first one for monochrome).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Width of each plane, in samples.
    pub widths: [u32; 3],
    /// Height of each plane, in samples.
    pub heights: [u32; 3],
    /// Samples, row-major, one `Vec` per plane.
    pub planes: [Vec<u16>; 3],
    /// Bit depth of luma and chroma samples.
    pub bit_depth: [u8; 2],
    /// `chroma_format_idc` (0 = monochrome, 1 = 4:2:0, 2 = 4:2:2, 3 = 4:4:4).
    pub chroma_format: u8,
    /// `video_full_range_flag` from the VUI (false when the VUI is absent).
    pub full_range: bool,
    /// `matrix_coeffs` from the VUI (2 = unspecified when absent).
    pub matrix_coeffs: u8,
}

impl Frame {
    fn new(sps: &Sps) -> Self {
        let (w, h) = (
            sps.pic_width_in_luma_samples,
            sps.pic_height_in_luma_samples,
        );
        let (sw, sh) = sps.chroma_subsampling();
        let (cw, ch) = if sps.chroma_array_type() == 0 && sps.chroma_format_idc == 0 {
            (0, 0)
        } else {
            (w / sw, h / sh)
        };
        let mid = |d: u8| 1u16 << (d - 1);
        Self {
            widths: [w, cw, cw],
            heights: [h, ch, ch],
            planes: [
                vec![mid(sps.bit_depth_luma); (w * h) as usize],
                vec![mid(sps.bit_depth_chroma); (cw * ch) as usize],
                vec![mid(sps.bit_depth_chroma); (cw * ch) as usize],
            ],
            bit_depth: [sps.bit_depth_luma, sps.bit_depth_chroma],
            chroma_format: sps.chroma_format_idc,
            full_range: sps.vui.as_ref().is_some_and(|v| v.video_full_range_flag),
            matrix_coeffs: sps.vui.as_ref().map_or(2, |v| v.matrix_coeffs),
        }
    }

    /// Returns a copy cropped to the conformance window of `sps`.
    pub fn cropped(&self, sps: &Sps) -> Self {
        let (sw, sh) = sps.chroma_subsampling();
        let [l, r, t, b] = sps.conf_win_offsets;
        let mut out = Self {
            planes: Default::default(),
            ..*self
        };
        for c in 0..3 {
            if self.widths[c] == 0 {
                continue;
            }
            let (fx, fy) = if c == 0 { (sw, sh) } else { (1, 1) };
            let (x0, y0) = (l * fx, t * fy);
            let w = self.widths[c] - (l + r) * fx;
            let h = self.heights[c] - (t + b) * fy;
            let src_w = self.widths[c] as usize;
            let mut plane = Vec::with_capacity(w as usize * h as usize);
            for y in y0..y0 + h {
                let row = y as usize * src_w + x0 as usize;
                plane.extend_from_slice(&self.planes[c][row..row + w as usize]);
            }
            out.planes[c] = plane;
            out.widths[c] = w;
            out.heights[c] = h;
        }
        out
    }
}

/// `intraPredAngle` for modes 2..=34 (Table 8-4).
const INTRA_PRED_ANGLE: [i32; 33] = [
    32, 26, 21, 17, 13, 9, 5, 2, 0, -2, -5, -9, -13, -17, -21, -26, -32, -26, -21, -17, -13, -9,
    -5, -2, 0, 2, 5, 9, 13, 17, 21, 26, 32,
];

/// `invAngle` for modes 11..=25 (Table 8-5).
const INV_ANGLE: [i32; 15] = [
    -4096, -1638, -910, -630, -482, -390, -315, -256, -315, -390, -482, -630, -910, -1638, -4096,
];

/// Magnitudes of the 32-point DCT coefficients, indexed by `a = k(2n+1) mod 128` folded to 0..=32.
const DCT_C: [i32; 33] = [
    64, 90, 90, 90, 89, 88, 87, 85, 83, 82, 80, 78, 75, 73, 70, 67, 64, 61, 57, 54, 50, 46, 43, 38,
    36, 31, 25, 22, 18, 13, 9, 4, 0,
];

/// `transMatrix` coefficient for frequency `k` and position `n` of the 32-point DCT (§8.6.4.2).
const fn dct_coef(k: usize, n: usize) -> i32 {
    if k == 0 {
        return 64;
    }
    let a = (k * (2 * n + 1)) % 128;
    match a {
        0..=32 => DCT_C[a],
        33..=64 => -DCT_C[64 - a],
        65..=96 => -DCT_C[a - 64],
        _ => DCT_C[128 - a],
    }
}

/// The 32-point DCT matrix, `DCT32[k][n]`; smaller sizes use every `32 / size`-th row.
const DCT32: [[i32; 32]; 32] = {
    let mut m = [[0; 32]; 32];
    let mut k = 0;
    while k < 32 {
        let mut n = 0;
        while n < 32 {
            m[k][n] = dct_coef(k, n);
            n += 1;
        }
        k += 1;
    }
    m
};

/// 4×4 DST-VII used for intra luma 4×4 blocks (8-317).
const DST4: [[i32; 4]; 4] = [
    [29, 55, 74, 84],
    [74, 74, 0, -74],
    [84, -29, -74, 55],
    [55, -84, 74, -29],
];

/// Inverse 1-D transform: `output[i] = Σ_k M[k][i] · input[k]`, with `output.len()` points.
///
/// `input` may be shorter than the transform size when its tail is known to be zero. Inputs are
/// clipped to 16 bits and coefficients are at most 90, so the sums fit in `i32`.
fn inverse_1d(input: &[i32], output: &mut [i32], dst: bool) {
    let n = output.len();
    if dst {
        output.fill(0);
        for (k, &c) in input.iter().enumerate().filter(|&(_, &c)| c != 0) {
            for (o, &m) in output.iter_mut().zip(&DST4[k]) {
                *o += m * c;
            }
        }
        return;
    }
    // DCT rows are symmetric for even frequencies and antisymmetric for odd ones: compute the
    // first half of each part, then out[i] = even[i] + odd[i] and out[n-1-i] = even[i] - odd[i].
    let half = n / 2;
    let (mut even, mut odd) = ([0i32; 16], [0i32; 16]);
    for (k, &c) in input.iter().enumerate().filter(|&(_, &c)| c != 0) {
        let acc = if k % 2 == 0 { &mut even } else { &mut odd };
        for (a, &m) in acc[..half].iter_mut().zip(&DCT32[k * (32 / n)][..half]) {
            *a += m * c;
        }
    }
    for i in 0..half {
        output[i] = even[i] + odd[i];
        output[n - 1 - i] = even[i] - odd[i];
    }
}

/// Reconstructs a picture from the blocks emitted by the parser.
#[derive(Debug)]
pub struct Reconstructor<'a> {
    sps: &'a Sps,
    pps: &'a Pps,
    layout: CtbLayout,
    /// The picture being reconstructed (coded size, before cropping).
    pub frame: Frame,
    /// Slice that reconstructed each 4×4 luma block (`u32::MAX` = not yet).
    decoded: Vec<u32>,
    w4: usize,
    slice_addr: u32,
    slice_cb_qp_offset: i32,
    slice_cr_qp_offset: i32,
    /// Luma residual of the current transform unit, for cross-component prediction (4:4:4).
    luma_residual: Vec<i32>,
    /// Scratch buffers reused across transform blocks (prediction, residual, dequantized
    /// coefficients, intermediate transform), 32×32 each.
    scratch: Vec<i32>,
    /// Information for the in-loop filters.
    filter_info: FilterInfo,
    /// Deferred error (the sink interface cannot return errors).
    pub error: Option<Error>,
}

impl<'a> Reconstructor<'a> {
    /// Creates a reconstructor for a picture.
    pub fn new(sps: &'a Sps, pps: &'a Pps) -> Self {
        let w4 = sps.pic_width_in_luma_samples.div_ceil(4) as usize;
        let h4 = sps.pic_height_in_luma_samples.div_ceil(4) as usize;
        let layout = CtbLayout::new(sps, pps);
        Self {
            sps,
            pps,
            filter_info: FilterInfo::new(sps, &layout),
            layout,
            frame: Frame::new(sps),
            decoded: vec![u32::MAX; w4 * h4],
            w4,
            slice_addr: 0,
            slice_cb_qp_offset: 0,
            slice_cr_qp_offset: 0,
            luma_residual: vec![0; 32 * 32],
            scratch: vec![0; 4 * 32 * 32],
            error: None,
        }
    }

    /// Must be called before the blocks of each slice segment.
    pub fn start_slice(&mut self, header: &crate::slice::SliceHeader) {
        if !header.dependent_slice_segment_flag {
            self.slice_addr = header.slice_segment_address;
        }
        self.slice_cb_qp_offset = i32::from(header.slice_cb_qp_offset);
        self.slice_cr_qp_offset = i32::from(header.slice_cr_qp_offset);
        self.filter_info.slices.push(SliceFilterParams {
            deblocking_disabled: header.slice_deblocking_filter_disabled_flag,
            beta_offset_div2: header.slice_beta_offset_div2,
            tc_offset_div2: header.slice_tc_offset_div2,
            loop_filter_across_slices: header.slice_loop_filter_across_slices_enabled_flag,
        });
    }

    /// Applies the in-loop filters (unless `skip_filters`) and returns the decoded picture,
    /// uncropped (see [`Frame::cropped`]).
    pub fn finish(mut self, skip_filters: bool) -> Frame {
        if !skip_filters {
            filters::deblock(
                &mut self.frame,
                &self.filter_info,
                self.sps,
                self.pps,
                &self.layout,
            );
            filters::sao(
                &mut self.frame,
                &self.filter_info,
                self.sps,
                self.pps,
                &self.layout,
            );
        }
        self.frame
    }

    /// Marks block edges (left and top) for the deblocking filter.
    fn mark_edges(&mut self, x: u32, y: u32, w: u32, h: u32) {
        let info = &mut self.filter_info;
        let (x4, y4) = (x as usize >> 2, y as usize >> 2);
        for yy in y4..((y + h) as usize).div_ceil(4).min(info.h4) {
            info.edges[yy * info.w4 + x4] |= 1;
        }
        for xx in x4..((x + w) as usize).div_ceil(4).min(info.w4) {
            info.edges[y4 * info.w4 + xx] |= 2;
        }
    }

    fn mark_decoded(&mut self, x: u32, y: u32, w: u32, h: u32) {
        let h4 = self.decoded.len() / self.w4;
        for yy in (y as usize >> 2)..((y + h) as usize).div_ceil(4).min(h4) {
            for xx in (x as usize >> 2)..((x + w) as usize).div_ceil(4).min(self.w4) {
                self.decoded[yy * self.w4 + xx] = self.slice_addr;
            }
        }
    }

    /// Availability of a luma sample position for intra prediction of the block at (`x`, `y`).
    fn available(&self, x: u32, y: u32, xn: i64, yn: i64) -> bool {
        let (pw, ph) = (
            self.sps.pic_width_in_luma_samples,
            self.sps.pic_height_in_luma_samples,
        );
        if xn < 0 || yn < 0 || xn >= i64::from(pw) || yn >= i64::from(ph) {
            return false;
        }
        let (xn, yn) = (xn as u32, yn as u32);
        if self.decoded[(yn as usize >> 2) * self.w4 + (xn as usize >> 2)] != self.slice_addr {
            return false;
        }
        let l = self.sps.log2_ctb_size;
        let w = self.layout.width;
        self.layout.tile_of_rs((yn >> l) * w + (xn >> l))
            == self.layout.tile_of_rs((y >> l) * w + (x >> l))
    }

    /// Intra prediction of an `n`×`n` block of component `c` at (`x0`, `y0`) (§8.4.4.2).
    #[allow(clippy::too_many_arguments)]
    fn predict(
        &self,
        c: usize,
        x0: u32,
        y0: u32,
        n: usize,
        mode: u8,
        transquant_bypass: bool,
        pred: &mut [i32],
    ) {
        // disableIntraBoundaryFilter (range extension).
        let boundary_filters = c == 0
            && n < 32
            && !(self.sps.range_extension.implicit_rdpcm_enabled_flag && transquant_bypass);
        let (sw, sh) = if c == 0 {
            (1, 1)
        } else {
            self.sps.chroma_subsampling()
        };
        let bit_depth = self.frame.bit_depth[usize::from(c > 0)];
        let plane = &self.frame.planes[c];
        let stride = self.frame.widths[c] as usize;
        let (xl, yl) = (x0 * sw, y0 * sh);

        // Reference samples: index 0 = p[-1][2n-1] (bottom of the left column) ... index 2n =
        // p[-1][-1] ... index 4n = p[2n-1][-1] (right end of the top row), as in the
        // substitution order of §8.4.4.2.2.
        let total = 4 * n + 1;
        let mut refs = [0i32; 4 * 32 + 1];
        let mut avail = [false; 4 * 32 + 1];
        let (refs, avail) = (&mut refs[..total], &mut avail[..total]);
        let pos = |i: usize| -> (i64, i64) {
            if i < 2 * n {
                (i64::from(x0) - 1, i64::from(y0) + (2 * n - 1 - i) as i64)
            } else {
                (i64::from(x0) + (i - 2 * n) as i64 - 1, i64::from(y0) - 1)
            }
        };
        let mut any = false;
        // Availability only changes between 4×4 luma blocks: reuse it within a block.
        let mut cached: Option<((i64, i64), bool)> = None;
        for i in 0..total {
            let (xc, yc) = pos(i);
            let (xn, yn) = (xc * i64::from(sw), yc * i64::from(sh));
            let key = (xn >> 2, yn >> 2);
            let ok = match cached {
                Some((k, ok)) if k == key => ok,
                _ => {
                    let ok = self.available(xl, yl, xn, yn);
                    cached = Some((key, ok));
                    ok
                }
            };
            if ok {
                refs[i] = i32::from(plane[yc as usize * stride + xc as usize]);
                avail[i] = true;
                any = true;
            }
        }
        if !any {
            refs.fill(1 << (bit_depth - 1));
        } else {
            if !avail[0] {
                let first = avail.iter().position(|&a| a).unwrap();
                refs[0] = refs[first];
            }
            for i in 1..total {
                if !avail[i] {
                    refs[i] = refs[i - 1];
                }
            }
        }
        // p[-1][y] = left(y), p[x][-1] = top(x), with -1 meaning the corner.
        let left = |r: &[i32], y: i32| r[(2 * n as i32 - 1 - y) as usize];
        let top = |r: &[i32], x: i32| r[(2 * n as i32 + 1 + x) as usize];

        // Filtering of neighbouring samples (§8.4.4.2.3).
        let filter_allowed = (c == 0 || self.sps.chroma_array_type() == 3)
            && !self.sps.range_extension.intra_smoothing_disabled_flag;
        if filter_allowed && mode != 1 && n != 4 {
            let min_dist = (i32::from(mode) - 26)
                .abs()
                .min((i32::from(mode) - 10).abs());
            let thres = match n {
                8 => 7,
                16 => 1,
                _ => 0,
            };
            if min_dist > thres {
                let corner = refs[2 * n];
                let n2 = 2 * n as i32;
                let bi_int = self.sps.strong_intra_smoothing_enabled_flag
                    && c == 0
                    && n == 32
                    && (corner + top(refs, n2 - 1) - 2 * top(refs, n as i32 - 1)).abs()
                        < (1 << (bit_depth - 5))
                    && (corner + left(refs, n2 - 1) - 2 * left(refs, n as i32 - 1)).abs()
                        < (1 << (bit_depth - 5));
                let mut f = [0i32; 4 * 32 + 1];
                let f = &mut f[..total];
                f.copy_from_slice(refs);
                if bi_int {
                    let (l63, t63) = (left(refs, 63), top(refs, 63));
                    for y in 0..63 {
                        f[(63 - y) as usize] = ((63 - y) * corner + (y + 1) * l63 + 32) >> 6;
                    }
                    for x in 0..63 {
                        f[(65 + x) as usize] = ((63 - x) * corner + (x + 1) * t63 + 32) >> 6;
                    }
                } else {
                    for i in 1..total - 1 {
                        f[i] = (refs[i - 1] + 2 * refs[i] + refs[i + 1] + 2) >> 2;
                    }
                }
                refs.copy_from_slice(f);
            }
        }

        let max = (1 << bit_depth) - 1;
        let n_i = n as i32;
        let log2n = n.trailing_zeros();
        match mode {
            0 => {
                // Planar.
                let (tr, bl) = (top(refs, n_i), left(refs, n_i));
                for y in 0..n_i {
                    for x in 0..n_i {
                        pred[(y * n_i + x) as usize] = ((n_i - 1 - x) * left(refs, y)
                            + (x + 1) * tr
                            + (n_i - 1 - y) * top(refs, x)
                            + (y + 1) * bl
                            + n_i)
                            >> (log2n + 1);
                    }
                }
            }
            1 => {
                // DC.
                let sum: i32 = (0..n_i).map(|i| top(refs, i) + left(refs, i)).sum();
                let dc = (sum + n_i) >> (log2n + 1);
                pred[..n * n].fill(dc);
                if boundary_filters {
                    pred[0] = (left(refs, 0) + 2 * dc + top(refs, 0) + 2) >> 2;
                    for x in 1..n_i {
                        pred[x as usize] = (top(refs, x) + 3 * dc + 2) >> 2;
                    }
                    for y in 1..n_i {
                        pred[(y * n_i) as usize] = (left(refs, y) + 3 * dc + 2) >> 2;
                    }
                }
            }
            _ => {
                let angle = INTRA_PRED_ANGLE[usize::from(mode) - 2];
                let vertical = mode >= 18;
                // ref[] with offset n so that negative indices are representable.
                let mut r = [0i32; 3 * 32 + 1];
                let main = |i: i32| {
                    if vertical {
                        top(refs, i - 1)
                    } else {
                        left(refs, i - 1)
                    }
                };
                let side = |i: i32| {
                    if vertical {
                        left(refs, i - 1)
                    } else {
                        top(refs, i - 1)
                    }
                };
                for x in 0..=n_i {
                    r[(x + n_i) as usize] = main(x);
                }
                if angle < 0 {
                    let inv = INV_ANGLE[usize::from(mode) - 11];
                    let last = (n_i * angle) >> 5;
                    if last < -1 {
                        for x in last..=-1 {
                            r[(x + n_i) as usize] = side((x * inv + 128) >> 8);
                        }
                    }
                } else {
                    for x in n_i + 1..=2 * n_i {
                        r[(x + n_i) as usize] = main(x);
                    }
                }
                for j in 0..n_i {
                    let idx = ((j + 1) * angle) >> 5;
                    let fact = ((j + 1) * angle) & 31;
                    for i in 0..n_i {
                        let a = r[(i + idx + 1 + n_i) as usize];
                        let v = if fact != 0 {
                            let b = r[(i + idx + 2 + n_i) as usize];
                            ((32 - fact) * a + fact * b + 16) >> 5
                        } else {
                            a
                        };
                        // j runs along the prediction direction: rows for vertical modes.
                        let (x, y) = if vertical { (i, j) } else { (j, i) };
                        pred[(y * n_i + x) as usize] = v;
                    }
                }
                if boundary_filters {
                    let corner = refs[2 * n];
                    if mode == 26 {
                        for y in 0..n_i {
                            pred[(y * n_i) as usize] =
                                (top(refs, 0) + ((left(refs, y) - corner) >> 1)).clamp(0, max);
                        }
                    } else if mode == 10 {
                        for x in 0..n_i {
                            pred[x as usize] =
                                (left(refs, 0) + ((top(refs, x) - corner) >> 1)).clamp(0, max);
                        }
                    }
                }
            }
        }
    }

    /// Scaling factor `m` for a coefficient (§8.6.4.1), from the active scaling list.
    fn scaling_factor(
        list: &ScalingList,
        log2_size: u8,
        matrix_id: usize,
        x: usize,
        y: usize,
    ) -> i32 {
        let size_id = usize::from(log2_size - 2);
        if size_id == 0 {
            let scan = scan_order(2, ScanType::Diagonal);
            let i = scan
                .iter()
                .position(|&(sx, sy)| usize::from(sx) == x && usize::from(sy) == y)
                .unwrap();
            return i32::from(list.lists[0][matrix_id][i]);
        }
        let ratio = 1usize << (size_id - 1); // 1 for 8×8, 2 for 16×16, 4 for 32×32
        if size_id >= 2 && x == 0 && y == 0 {
            return i32::from(list.dc[size_id - 2][matrix_id]);
        }
        let (xs, ys) = (x / ratio, y / ratio);
        let scan = scan_order(3, ScanType::Diagonal);
        let i = scan
            .iter()
            .position(|&(sx, sy)| usize::from(sx) == xs && usize::from(sy) == ys)
            .unwrap();
        i32::from(list.lists[size_id][matrix_id][i])
    }

    /// Range extension tools for blocks coded without transform (transform skip or
    /// transquant bypass): 180° rotation of 4×4 blocks, then implicit RDPCM for intra
    /// horizontal/vertical blocks (residual accumulated along the prediction direction).
    fn rotate_and_rdpcm(&self, tb: &TransformBlock<'_>, r: &mut [i32]) {
        let n = 1usize << tb.log2_size;
        let ext = &self.sps.range_extension;
        if ext.transform_skip_rotation_enabled_flag && n == 4 {
            r.reverse();
        }
        if ext.implicit_rdpcm_enabled_flag {
            match tb.intra_mode {
                10 => {
                    for y in 0..n {
                        for x in 1..n {
                            r[y * n + x] += r[y * n + x - 1];
                        }
                    }
                }
                26 => {
                    for y in 1..n {
                        for x in 0..n {
                            r[y * n + x] += r[(y - 1) * n + x];
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Computes the residual of a block from its coefficients (§8.6.2 to §8.6.4).
    /// `d` and `tmp` are scratch buffers of at least `n * n` values.
    fn residual(
        &self,
        tb: &TransformBlock<'_>,
        coeffs: &[i32],
        out: &mut [i32],
        d: &mut [i32],
        tmp: &mut [i32],
    ) {
        let n = 1usize << tb.log2_size;
        let c = usize::from(tb.c_idx);
        if tb.transquant_bypass {
            out[..n * n].copy_from_slice(&coeffs[..n * n]);
            self.rotate_and_rdpcm(tb, &mut out[..n * n]);
            return;
        }
        let bit_depth = i32::from(self.frame.bit_depth[usize::from(c > 0)]);

        // Quantization parameter (§8.6.1).
        let qp_bd_y = 6 * (i32::from(self.sps.bit_depth_luma) - 8);
        let qp_bd_c = 6 * (i32::from(self.sps.bit_depth_chroma) - 8);
        let qp = if c == 0 {
            tb.qp_y + qp_bd_y
        } else {
            let (pps_off, slice_off) = if c == 1 {
                (
                    i32::from(self.pps.pps_cb_qp_offset),
                    self.slice_cb_qp_offset,
                )
            } else {
                (
                    i32::from(self.pps.pps_cr_qp_offset),
                    self.slice_cr_qp_offset,
                )
            };
            let qpi =
                (tb.qp_y + pps_off + slice_off + i32::from(tb.cu_qp_offset)).clamp(-qp_bd_c, 57);
            let qpc = if self.sps.chroma_array_type() == 1 {
                match qpi {
                    ..30 => qpi,
                    30..=43 => [29, 30, 31, 32, 33, 33, 34, 34, 35, 35, 36, 36, 37, 37]
                        [(qpi - 30) as usize],
                    _ => qpi - 6,
                }
            } else {
                qpi.min(51)
            };
            qpc + qp_bd_c
        };

        // Scaling (§8.6.4.1).
        const LEVEL_SCALE: [i64; 6] = [40, 45, 51, 57, 64, 72];
        let bd_shift = bit_depth + i32::from(tb.log2_size) + 10 - 15;
        let scale = LEVEL_SCALE[(qp % 6) as usize] << (qp / 6);
        let list = self
            .pps
            .scaling_list
            .as_ref()
            .or(self.sps.scaling_list.as_ref());
        let flat = list.is_none() || (tb.transform_skip && n > 4);
        let d = &mut d[..n * n];
        d.fill(0);
        // Bounding box of the non-zero coefficients: the transform only needs that part.
        let (mut max_x, mut max_y) = (0, 0);
        for y in 0..n {
            for x in 0..n {
                let level = coeffs[y * n + x];
                if level == 0 {
                    continue;
                }
                max_x = max_x.max(x);
                max_y = max_y.max(y);
                let m = if flat {
                    16
                } else {
                    Self::scaling_factor(list.unwrap(), tb.log2_size, c, x, y)
                };
                let v =
                    (i64::from(level) * i64::from(m) * scale + (1 << (bd_shift - 1))) >> bd_shift;
                d[y * n + x] = v.clamp(-32768, 32767) as i32;
            }
        }

        let bd_shift2 = 20 - bit_depth;
        if tb.transform_skip {
            let ts_shift = 5 + i32::from(tb.log2_size);
            for (o, &v) in out.iter_mut().zip(d.iter()) {
                *o = ((v << ts_shift) + (1 << (bd_shift2 - 1))) >> bd_shift2;
            }
            self.rotate_and_rdpcm(tb, &mut out[..n * n]);
            return;
        }

        // Inverse transform (§8.6.4.2): columns, clip, then rows. Coefficients outside the
        // bounding box are zero, so only its columns, and their first `max_y + 1` values, are used.
        let dst = c == 0 && n == 4;
        let rnd2 = 1 << (bd_shift2 - 1);
        if max_x == 0 && max_y == 0 && !dst {
            // DC only: every DCT basis row 0 coefficient is 64, so the residual is constant.
            let t = ((64 * d[0] + 64) >> 7).clamp(-32768, 32767);
            out[..n * n].fill((64 * t + rnd2) >> bd_shift2);
            return;
        }
        let tmp = &mut tmp[..n * n];
        let mut col = [0i32; 32];
        let mut res = [0i32; 32];
        for x in 0..=max_x {
            for y in 0..=max_y {
                col[y] = d[y * n + x];
            }
            inverse_1d(&col[..=max_y], &mut res[..n], dst);
            for y in 0..n {
                tmp[y * n + x] = ((res[y] + 64) >> 7).clamp(-32768, 32767);
            }
        }
        for y in 0..n {
            inverse_1d(&tmp[y * n..=y * n + max_x], &mut res[..n], dst);
            for x in 0..n {
                out[y * n + x] = (res[x] + rnd2) >> bd_shift2;
            }
        }
    }

    fn transform_block_impl(&mut self, tb: &TransformBlock<'_>) {
        let n = 1usize << tb.log2_size;
        let c = usize::from(tb.c_idx);
        let mut scratch = std::mem::take(&mut self.scratch);
        let (pred, rest) = scratch.split_at_mut(32 * 32);
        let (residual, rest) = rest.split_at_mut(32 * 32);
        let (d, tmp) = rest.split_at_mut(32 * 32);
        let (pred, residual) = (&mut pred[..n * n], &mut residual[..n * n]);
        self.predict(c, tb.x, tb.y, n, tb.intra_mode, tb.transquant_bypass, pred);

        if let Some(coeffs) = tb.coeffs {
            self.residual(tb, coeffs, residual, d, tmp);
        } else {
            residual.fill(0);
        }
        if c == 0
            && self
                .pps
                .range_extension
                .cross_component_prediction_enabled_flag
        {
            self.luma_residual[..n * n].copy_from_slice(residual);
        }
        if c > 0 && tb.res_scale_val != 0 {
            let (bd_y, bd_c) = (
                i32::from(self.frame.bit_depth[0]),
                i32::from(self.frame.bit_depth[1]),
            );
            for (r, &ry) in residual.iter_mut().zip(&self.luma_residual) {
                *r += (i32::from(tb.res_scale_val) * ((ry << bd_c) >> bd_y)) >> 3;
            }
        }

        let max = (1i32 << self.frame.bit_depth[usize::from(c > 0)]) - 1;
        let stride = self.frame.widths[c] as usize;
        let plane = &mut self.frame.planes[c];
        for y in 0..n {
            let row = (tb.y as usize + y) * stride + tb.x as usize;
            for x in 0..n {
                plane[row + x] = (pred[y * n + x] + residual[y * n + x]).clamp(0, max) as u16;
            }
        }
        self.scratch = scratch;
        if c == 0 {
            self.mark_decoded(tb.x, tb.y, n as u32, n as u32);
            self.mark_edges(tb.x, tb.y, n as u32, n as u32);
        }
    }
}

impl Sink for Reconstructor<'_> {
    fn transform_block(&mut self, tb: &TransformBlock<'_>) {
        if self.error.is_none() {
            self.transform_block_impl(tb);
        }
    }

    fn pcm(&mut self, cu_x: u32, cu_y: u32, log2_size: u8, samples: [&[u16]; 3]) {
        let Some(pcm) = self.sps.pcm else { return };
        let (sw, sh) = self.sps.chroma_subsampling();
        let size = 1u32 << log2_size;
        for (c, s) in samples.iter().enumerate() {
            if s.is_empty() {
                continue;
            }
            let (w, h, x0, y0) = if c == 0 {
                (size, size, cu_x, cu_y)
            } else {
                (size / sw, size / sh, cu_x / sw, cu_y / sh)
            };
            let (shift, stride) = if c == 0 {
                (
                    self.sps.bit_depth_luma - pcm.bit_depth_luma,
                    self.frame.widths[0],
                )
            } else {
                (
                    self.sps.bit_depth_chroma - pcm.bit_depth_chroma,
                    self.frame.widths[c],
                )
            };
            for y in 0..h {
                for x in 0..w {
                    self.frame.planes[c][((y0 + y) * stride + x0 + x) as usize] =
                        s[(y * w + x) as usize] << shift;
                }
            }
        }
        self.mark_decoded(cu_x, cu_y, size, size);
    }

    fn coding_unit(&mut self, cu: &CodingUnit) {
        let size = 1u32 << cu.log2_size;
        self.mark_edges(cu.x, cu.y, size, size);
        let no_filter = cu.transquant_bypass
            || (cu.pcm && self.sps.pcm.is_some_and(|p| p.loop_filter_disabled_flag));
        let slice = (self.filter_info.slices.len().max(1) - 1) as u16;
        let info = &mut self.filter_info;
        for yy in (cu.y as usize >> 2)..((cu.y + size) as usize).div_ceil(4).min(info.h4) {
            for xx in (cu.x as usize >> 2)..((cu.x + size) as usize).div_ceil(4).min(info.w4) {
                let i = yy * info.w4 + xx;
                info.qp_y[i] = cu.qp_y as i8;
                info.no_filter[i] = no_filter;
                info.slice[i] = slice;
            }
        }
    }

    fn sao(&mut self, ctb_x: u32, ctb_y: u32, params: &SaoParams) {
        self.filter_info.sao[(ctb_y * self.layout.width + ctb_x) as usize] = *params;
    }
}
