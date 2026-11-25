//! Picture reconstruction: intra prediction (§8.4.4.2), scaling and inverse transforms
//! (§8.6), and adding the residual. In-loop filters (deblocking, SAO) are applied afterwards.

use crate::Error;
use crate::filters::{self, EdgeMotion, FilterInfo, SliceFilterParams};
use crate::inter;
use crate::params::{Pps, ScalingList, Sps};
use crate::scan::CtbLayout;
use crate::syntax::{CodingUnit, PredictionUnit, SaoParams, Sink, TransformBlock};

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
pub const DCT32: [[i32; 32]; 32] = {
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
pub const DST4: [[i32; 4]; 4] = [
    [29, 55, 74, 84],
    [74, 74, 0, -74],
    [84, -29, -74, 55],
    [55, -84, 74, -29],
];

/// The `N`-point DCT matrix: rows `k * 32 / N` of [`DCT32`], first `N` columns.
const fn dct_matrix<const N: usize>() -> [[i32; N]; N] {
    let mut m = [[0; N]; N];
    let mut k = 0;
    while k < N {
        let mut n = 0;
        while n < N {
            m[k][n] = DCT32[k * (32 / N)][n];
            n += 1;
        }
        k += 1;
    }
    m
}

const DCT4: [[i32; 4]; 4] = dct_matrix();
const DCT8: [[i32; 8]; 8] = dct_matrix();
const DCT16: [[i32; 16]; 16] = dct_matrix();

/// One 1-D inverse transform stage: `out[i] = sum_j m[j][i] * src[j]`, summing only the first
/// `terms` entries of `src`.
///
/// Accumulating over `j` on the outside keeps the inner loop a fixed `N` multiply-accumulate
/// over two contiguous slices, which the compiler vectorizes. A zero term inside that range is
/// skipped; the skipped terms add nothing, so dropping them is exact.
///
/// Inputs are 16-bit and coefficients at most 90 in magnitude, so the sums fit in `i32`
/// (`32 * 32768 * 90` is well inside it).
#[inline]
fn mul_1d<const N: usize>(m: &[[i32; N]; N], src: &[i32; N], terms: usize) -> [i32; N] {
    let mut acc = [0i32; N];
    for j in 0..terms.min(N) {
        let c = src[j];
        if c == 0 {
            continue;
        }
        let row = &m[j];
        for i in 0..N {
            acc[i] += row[i] * c;
        }
    }
    acc
}

/// Odd half of the `N`-point DCT matrix: `odd[i][j] = DCT_N[2i + 1][j]` for `j < N / 2`,
/// the rows that the partial butterfly multiplies by the odd coefficients.
const fn odd_rows<const N: usize, const H: usize>() -> [[i32; H]; H] {
    let mut m = [[0i32; H]; H];
    let mut i = 0;
    while i < H {
        let mut j = 0;
        while j < H {
            m[i][j] = DCT32[(2 * i + 1) * (32 / N)][j];
            j += 1;
        }
        i += 1;
    }
    m
}

const ODD8: [[i32; 4]; 4] = odd_rows::<8, 4>();
const ODD16: [[i32; 8]; 8] = odd_rows::<16, 8>();
const ODD32: [[i32; 16]; 16] = odd_rows::<32, 16>();

/// One butterfly level of the inverse DCT: the `N`-point output from the `N / 2`-point
/// inverse of the even coefficients (`even`) and the odd coefficients of `src`, of which the
/// first `terms` entries may be non-zero.
///
/// The DCT basis is symmetric, `DCT_N[k][N - 1 - n] = (-1)^k DCT_N[k][n]`, and its even rows
/// are the `N / 2`-point basis. So with `E` the even part and `O` the odd part of
/// `sum_k DCT_N[k][n] * src[k]`, the outputs are `E[n] + O[n]` and `E[n] - O[n]` mirrored:
/// the same integer sums as the matrix product, regrouped, so the result is identical.
#[inline]
fn butterfly<const N: usize, const H: usize>(
    odd: &[[i32; H]; H],
    even: [i32; H],
    src: &[i32; N],
    terms: usize,
) -> [i32; N] {
    let mut o = [0i32; H];
    for (i, row) in odd.iter().enumerate().take(terms.min(N) / 2) {
        let c = src[2 * i + 1];
        if c == 0 {
            continue;
        }
        for j in 0..H {
            o[j] += row[j] * c;
        }
    }
    let mut out = [0i32; N];
    for j in 0..H {
        out[j] = even[j] + o[j];
        out[N - 1 - j] = even[j] - o[j];
    }
    out
}

/// The even coefficients of `src` (`src[2i]`), and how many of them may be non-zero.
#[inline]
fn evens<const N: usize, const H: usize>(src: &[i32; N], terms: usize) -> ([i32; H], usize) {
    let t = terms.min(N).div_ceil(2);
    let mut e = [0i32; H];
    for i in 0..t {
        e[i] = src[2 * i];
    }
    (e, t)
}

#[inline]
fn idct4(src: &[i32; 4], terms: usize) -> [i32; 4] {
    let s0 = if terms > 0 { src[0] } else { 0 };
    let s1 = if terms > 1 { src[1] } else { 0 };
    let s2 = if terms > 2 { src[2] } else { 0 };
    let s3 = if terms > 3 { src[3] } else { 0 };
    let (e0, e1) = (64 * (s0 + s2), 64 * (s0 - s2));
    let (o0, o1) = (83 * s1 + 36 * s3, 36 * s1 - 83 * s3);
    [e0 + o0, e1 + o1, e1 - o1, e0 - o0]
}

/// 4×4 inverse DST-VII by partial factorization (7 multiplications instead of 16).
#[inline]
fn idst4(src: &[i32; 4], terms: usize) -> [i32; 4] {
    let a = if terms > 0 { src[0] } else { 0 };
    let b = if terms > 1 { src[1] } else { 0 };
    let c = if terms > 2 { src[2] } else { 0 };
    let d = if terms > 3 { src[3] } else { 0 };
    let c0 = a + c;
    let c1 = c + d;
    let c2 = a - d;
    let c3 = 74 * b;
    [
        29 * c0 + 55 * c1 + c3,
        55 * c2 - 29 * c1 + c3,
        74 * (a - c + d),
        55 * c0 + 29 * c2 - c3,
    ]
}

#[inline]
fn idct8(src: &[i32; 8], terms: usize) -> [i32; 8] {
    let (e, t) = evens::<8, 4>(src, terms);
    butterfly(&ODD8, idct4(&e, t), src, terms)
}

#[inline]
fn idct16(src: &[i32; 16], terms: usize) -> [i32; 16] {
    let (e, t) = evens::<16, 8>(src, terms);
    butterfly(&ODD16, idct8(&e, t), src, terms)
}

#[inline]
fn idct32(src: &[i32; 32], terms: usize) -> [i32; 32] {
    let (e, t) = evens::<32, 16>(src, terms);
    butterfly(&ODD32, idct16(&e, t), src, terms)
}

/// One 1-D inverse DCT stage of size `N`, by partial butterfly: the same result as
/// `mul_1d(&DCT_N, src, terms)` with about half the multiplications.
#[inline]
fn idct_1d<const N: usize>(src: &[i32; N], terms: usize) -> [i32; N] {
    let mut out = [0i32; N];
    match N {
        4 => out.copy_from_slice(&idct4(src[..].try_into().expect("N = 4"), terms)),
        8 => out.copy_from_slice(&idct8(src[..].try_into().expect("N = 8"), terms)),
        16 => out.copy_from_slice(&idct16(src[..].try_into().expect("N = 16"), terms)),
        _ => out.copy_from_slice(&idct32(src[..].try_into().expect("N = 32"), terms)),
    }
    out
}

/// One 1-D stage of the inverse transform: the partial butterfly for the DCT (`FLAT_ROW`), the
/// fast DST-VII for 4×4 (`N = 4`) or matrix product for general sizes.
#[inline]
fn inverse_1d<const N: usize, const FLAT_ROW: bool>(
    _m: &[[i32; N]; N],
    src: &[i32; N],
    terms: usize,
) -> [i32; N] {
    if FLAT_ROW {
        idct_1d::<N>(src, terms)
    } else if N == 4 {
        let mut out = [0i32; N];
        out.copy_from_slice(&idst4(src[..].try_into().expect("N = 4"), terms));
        out
    } else {
        mul_1d::<N>(_m, src, terms)
    }
}

/// 2-D inverse transform (§8.6.4.2) of an `N`×`N` block of scaled coefficients `d` whose
/// non-zero values all lie in the top-left `rows`×`cols` corner: a column pass into `g` with
/// clipping to 16 bits, then a row pass shifted by `bd_shift`.
///
/// Both passes sum only over that rectangle, whose outside is never read: the parser already
/// gives the tight bounds and `inverse_1d` ignores entries at or past `terms`, so `g` does not
/// need to be pre-zeroed.
///
/// With `FLAT_ROW` (the DCT, whose first basis row `m[0][i]` is constant) the one-row and
/// one-column shapes are handled directly; the DST's first row is not flat, so it only uses
/// the general path.
fn inverse_2d<const N: usize, const FLAT_ROW: bool>(
    m: &[[i32; N]; N],
    d: &[i32],
    g: &mut [i32],
    rows: usize,
    cols: usize,
    bd_shift: i32,
    out: &mut [i32],
) {
    let b = &d[..N * N];
    let g = &mut g[..N * N];
    let (rows, cols) = (rows.min(N), cols.min(N));
    let rnd = 1 << (bd_shift - 1);
    if FLAT_ROW && rows == 1 {
        // Only the first coefficient row is non-zero, so the column pass gives every row the
        // same values `m[0][0] * d[x]`; the row pass then produces one residual row, repeated
        // down the block.
        let mut row_in = [0i32; N];
        for (dst, &c) in row_in.iter_mut().zip(b).take(cols) {
            *dst = ((m[0][0] * c + 64) >> 7).clamp(-32768, 32767);
        }
        let mut row = inverse_1d::<N, FLAT_ROW>(m, &row_in, cols);
        for v in row.iter_mut() {
            *v = (*v + rnd) >> bd_shift;
        }
        for o in out.chunks_exact_mut(N) {
            o.copy_from_slice(&row);
        }
        return;
    }
    // Column pass on the first `cols` columns (later columns have an all-zero source, so they
    // stay zero and the row pass never sums over them).
    let mut col = [0i32; N];
    for x in 0..cols {
        for j in 0..rows {
            col[j] = b[j * N + x];
        }
        let e = inverse_1d::<N, FLAT_ROW>(m, &col, rows);
        for y in 0..N {
            g[y * N + x] = ((e[y] + 64) >> 7).clamp(-32768, 32767);
        }
    }
    // Row pass: one row of `g` at a time.
    for (grow, o) in g.chunks_exact(N).zip(out.chunks_exact_mut(N)) {
        if FLAT_ROW && cols == 1 {
            // One column of `g` means every sample of the row is `m[0][i] * g[y][0]`.
            o.fill((m[0][0] * grow[0] + rnd) >> bd_shift);
            continue;
        }
        let r = inverse_1d::<N, FLAT_ROW>(m, grow.try_into().expect("N elements"), cols);
        for (dst, &val) in o.iter_mut().zip(&r) {
            *dst = (val + rnd) >> bd_shift;
        }
    }
}

/// Position in the up-right diagonal scan of each raster position of an `N`×`N` block, for
/// looking up scaling list coefficients (stored in scan order).
const fn diagonal_index<const N: usize, const NN: usize>() -> [u8; NN] {
    let mut index = [0u8; NN];
    let (mut i, mut x, mut y) = (0, 0usize, 0usize);
    // §6.5.3: walk the anti-diagonals from bottom-left to top-right.
    while i < NN {
        while y < N && x < N {
            index[y * N + x] = i as u8;
            i += 1;
            if y == 0 {
                break;
            }
            y -= 1;
            x += 1;
        }
        let d = x + y + 1;
        y = if d < N { d } else { N - 1 };
        x = d - y;
    }
    index
}

const DIAGONAL_INDEX_4: [u8; 16] = diagonal_index::<4, 16>();
const DIAGONAL_INDEX_8: [u8; 64] = diagonal_index::<8, 64>();

/// Reference samples of an intra-predicted block, from
/// [`Reconstructor::intra_references`].
#[derive(Debug, Clone)]
pub struct IntraReferences {
    samples: [i32; 4 * 32 + 1],
    c: usize,
    n: usize,
}

/// Reconstructs a picture from the blocks emitted by the parser.
#[derive(Debug)]
pub struct Reconstructor<'a> {
    sps: &'a Sps,
    pps: &'a Pps,
    layout: CtbLayout,
    /// Whether the picture is a single tile: intra prediction then never crosses a tile.
    single_tile: bool,
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
    /// coefficients, and 2-D inverse transform intermediate column pass), 32×32 each.
    scratch: Vec<i32>,
    /// Information for the in-loop filters.
    filter_info: FilterInfo,
    /// `RefPicList0` and `RefPicList1` of the current slice: each picture and its POC.
    refs: [Vec<(&'a Frame, i32, bool)>; 2],
    /// Explicit weighted prediction of the current slice, per entry of each list.
    weights: [Option<Vec<crate::slice::PredWeight>>; 2],
    /// Prediction buffers of inter blocks (two lists), reused across blocks.
    inter_scratch: Vec<i32>,
    /// Deferred error (the sink interface cannot return errors).
    pub error: Option<Error>,
}

/// The deblocking filter's state of an area, saved by [`Reconstructor::save_filter_state`].
#[derive(Debug, Clone, Default)]
pub struct FilterState {
    edges: Vec<u8>,
    intra: Vec<bool>,
    coded: Vec<bool>,
    motion: Vec<EdgeMotion>,
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
            single_tile: layout.col_bd.len() == 2 && layout.row_bd.len() == 2,
            layout,
            frame: Frame::new(sps),
            decoded: vec![u32::MAX; w4 * h4],
            w4,
            slice_addr: 0,
            slice_cb_qp_offset: 0,
            slice_cr_qp_offset: 0,
            luma_residual: vec![0; 32 * 32],
            scratch: vec![0; 4 * 32 * 32],
            refs: [Vec::new(), Vec::new()],
            weights: [None, None],
            inter_scratch: Vec::new(),
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

    /// Sets `RefPicList0` and `RefPicList1` of the slice whose blocks follow: the reference
    /// pictures, already filtered, with their POC and whether they are long-term pictures,
    /// and the explicit prediction weights of
    /// each list, if any. `RefPicList1` is empty for P slices.
    pub fn set_references(
        &mut self,
        lists: [Vec<(&'a Frame, i32, bool)>; 2],
        weights: [Option<Vec<crate::slice::PredWeight>>; 2],
    ) {
        self.refs = lists;
        self.weights = weights;
    }

    /// Like [`finish`](Self::finish), also returning the motion of every 4×4 luma block, for
    /// the pictures that use this one as collocated picture.
    pub fn finish_with_motion(self, skip_filters: bool, poc: i32) -> (Frame, inter::MotionField) {
        let w4 = self.filter_info.w4;
        let motion = self.filter_info.motion.clone();
        (
            self.finish(skip_filters),
            inter::MotionField::from_blocks(w4, motion, poc),
        )
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

    /// The deblocked picture, leaving this one as it is: for encoders that choose SAO parameters
    /// on it.
    pub fn deblocked(&self) -> Frame {
        let mut frame = self.frame.clone();
        filters::deblock(
            &mut frame,
            &self.filter_info,
            self.sps,
            self.pps,
            &self.layout,
        );
        frame
    }

    /// Appends the block edge flags of a luma area to `out`, row by row of 4×4 units, to put
    /// them back with [`restore_edges`](Self::restore_edges): for encoders that try several
    /// codings of the same area.
    pub fn save_edges(&self, x: u32, y: u32, w: u32, h: u32, out: &mut Vec<u8>) {
        let info = &self.filter_info;
        let (x0, x1) = (x as usize >> 2, ((x + w) as usize).div_ceil(4).min(info.w4));
        for yy in (y as usize >> 2)..((y + h) as usize).div_ceil(4).min(info.h4) {
            out.extend_from_slice(&info.edges[yy * info.w4 + x0..yy * info.w4 + x1]);
        }
    }

    /// Puts back edge flags saved by [`save_edges`](Self::save_edges) for the same area.
    pub fn restore_edges(&mut self, x: u32, y: u32, w: u32, h: u32, saved: &[u8]) {
        let info = &mut self.filter_info;
        let (x0, x1) = (x as usize >> 2, ((x + w) as usize).div_ceil(4).min(info.w4));
        let mut k = 0;
        for yy in (y as usize >> 2)..((y + h) as usize).div_ceil(4).min(info.h4) {
            let row = &mut info.edges[yy * info.w4 + x0..yy * info.w4 + x1];
            row.copy_from_slice(&saved[k..k + row.len()]);
            k += row.len();
        }
    }

    /// Saves what the deblocking filter knows of a luma area: edge flags, intra and coded
    /// flags and motion of each 4×4 block. For encoders that try inter and intra codings of
    /// the same area, with [`restore_filter_state`](Self::restore_filter_state).
    pub fn save_filter_state(&self, x: u32, y: u32, w: u32, h: u32) -> FilterState {
        let info = &self.filter_info;
        let (x0, x1) = (x as usize >> 2, ((x + w) as usize).div_ceil(4).min(info.w4));
        let mut state = FilterState::default();
        for yy in (y as usize >> 2)..((y + h) as usize).div_ceil(4).min(info.h4) {
            let r = yy * info.w4 + x0..yy * info.w4 + x1;
            state.edges.extend_from_slice(&info.edges[r.clone()]);
            state.intra.extend_from_slice(&info.intra[r.clone()]);
            state.coded.extend_from_slice(&info.coded[r.clone()]);
            state.motion.extend_from_slice(&info.motion[r]);
        }
        state
    }

    /// Puts back a state saved by [`save_filter_state`](Self::save_filter_state) for the same
    /// area.
    pub fn restore_filter_state(&mut self, x: u32, y: u32, w: u32, h: u32, state: &FilterState) {
        let info = &mut self.filter_info;
        let (x0, x1) = (x as usize >> 2, ((x + w) as usize).div_ceil(4).min(info.w4));
        let mut k = 0;
        for yy in (y as usize >> 2)..((y + h) as usize).div_ceil(4).min(info.h4) {
            let r = yy * info.w4 + x0..yy * info.w4 + x1;
            let n = r.len();
            info.edges[r.clone()].copy_from_slice(&state.edges[k..k + n]);
            info.intra[r.clone()].copy_from_slice(&state.intra[k..k + n]);
            info.coded[r.clone()].copy_from_slice(&state.coded[k..k + n]);
            info.motion[r].copy_from_slice(&state.motion[k..k + n]);
            k += n;
        }
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

    /// Marks a luma area as reconstructed or not, which decides whether intra prediction may
    /// use its samples. For encoders that try several codings of the same area: before trying
    /// again, the area must be marked as not reconstructed.
    pub fn set_reconstructed(&mut self, x: u32, y: u32, w: u32, h: u32, reconstructed: bool) {
        if reconstructed {
            self.mark_decoded(x, y, w, h);
        } else {
            let h4 = self.decoded.len() / self.w4;
            for yy in (y as usize >> 2)..((y + h) as usize).div_ceil(4).min(h4) {
                for xx in (x as usize >> 2)..((x + w) as usize).div_ceil(4).min(self.w4) {
                    self.decoded[yy * self.w4 + xx] = u32::MAX;
                }
            }
        }
    }

    #[inline]
    fn mark_decoded(&mut self, x: u32, y: u32, w: u32, h: u32) {
        let h4 = self.decoded.len() / self.w4;
        let x4 = x as usize >> 2;
        let x4_end = ((x + w) as usize).div_ceil(4).min(self.w4);
        if x4 >= x4_end {
            return;
        }
        for yy in (y as usize >> 2)..((y + h) as usize).div_ceil(4).min(h4) {
            let row = yy * self.w4;
            self.decoded[row + x4..row + x4_end].fill(self.slice_addr);
        }
    }

    /// Availability of a luma sample position for intra prediction of the block at (`x`, `y`).
    /// Whether the luma sample (`xn`, `yn`) may be used to predict the block at (`x`, `y`)
    /// (§6.4.1): inside the picture, already reconstructed, in the same slice and tile. For
    /// encoders deriving the motion vector candidates of a block.
    pub fn is_available(&self, x: u32, y: u32, xn: i64, yn: i64) -> bool {
        self.available(x, y, xn, yn)
    }

    #[inline]
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
        if self.single_tile {
            return true;
        }
        let l = self.sps.log2_ctb_size;
        let w = self.layout.width;
        self.layout.tile_of_rs((yn >> l) * w + (xn >> l))
            == self.layout.tile_of_rs((y >> l) * w + (x >> l))
    }

    /// Intra prediction of an `n`×`n` block of component `c` at (`x0`, `y0`) (in samples of
    /// that component) with `mode`, from the samples reconstructed so far, into `pred`
    /// (row-major). Nothing is written to the picture: for encoders comparing modes.
    pub fn predict_block(&self, c: usize, x0: u32, y0: u32, n: usize, mode: u8, pred: &mut [i32]) {
        self.predict(c, x0, y0, n, mode, false, pred);
    }

    /// The reference samples of the intra prediction of a block (see
    /// [`predict_block`](Self::predict_block)), to predict it with several modes at once with
    /// [`predict_from`](Self::predict_from).
    pub fn intra_references(&self, c: usize, x0: u32, y0: u32, n: usize) -> IntraReferences {
        IntraReferences {
            samples: self.reference_samples(c, x0, y0, n),
            c,
            n,
        }
    }

    /// Intra prediction with `mode` from reference samples given by
    /// [`intra_references`](Self::intra_references), into `pred` (row-major).
    pub fn predict_from(&self, refs: &IntraReferences, mode: u8, pred: &mut [i32]) {
        self.predict_with(refs.c, refs.n, mode, false, &refs.samples, pred);
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
        let refs = self.reference_samples(c, x0, y0, n);
        self.predict_with(c, n, mode, transquant_bypass, &refs, pred);
    }

    /// Reference samples of a block, substituted where unavailable (§8.4.4.2.2): index 0 =
    /// p[-1][2n-1] (bottom of the left column) ... index 2n = p[-1][-1] ... index 4n =
    /// p[2n-1][-1] (right end of the top row).
    fn reference_samples(&self, c: usize, x0: u32, y0: u32, n: usize) -> [i32; 4 * 32 + 1] {
        let (sw, sh) = if c == 0 {
            (1, 1)
        } else {
            self.sps.chroma_subsampling()
        };
        let bit_depth = self.frame.bit_depth[usize::from(c > 0)];
        let plane = &self.frame.planes[c];
        let stride = self.frame.widths[c] as usize;
        let ph_c = self.frame.heights[c] as usize;

        // Reference samples: index 0 = p[-1][2n-1] (bottom of the left column) ... index 2n =
        // p[-1][-1] ... index 4n = p[2n-1][-1] (right end of the top row), as in the
        // substitution order of §8.4.4.2.2.
        let total = 4 * n + 1;
        let mut all_refs = [0i32; 4 * 32 + 1];
        let mut avail = [false; 4 * 32 + 1];
        let (refs, avail) = (&mut all_refs[..total], &mut avail[..total]);
        let mut any = false;
        // Availability is decided per 4×4 luma block, i.e. per run of `unit_x` (`unit_y`)
        // samples of this component along the top row (left column).
        let (unit_x, unit_y) = ((4 / sw).max(1) as usize, (4 / sh).max(1) as usize);
        let (x0u, y0u) = (x0 as usize, y0 as usize);
        if self.single_tile && x0u > 0 && y0u > 0 && x0u + 2 * n <= stride && y0u + 2 * n <= ph_c {
            let mut all_avail = true;
            let x4_left = ((x0u - 1) * sw as usize) >> 2;
            let y4_above = ((y0u - 1) * sh as usize) >> 2;
            let mut k = 0;
            while k < 2 * n {
                let y = y0u + k;
                let len = (unit_y - y % unit_y).min(2 * n - k);
                if self.decoded[((y * sh as usize) >> 2) * self.w4 + x4_left] == self.slice_addr {
                    for j in 0..len {
                        refs[2 * n - 1 - k - j] = i32::from(plane[(y + j) * stride + x0u - 1]);
                        avail[2 * n - 1 - k - j] = true;
                    }
                    any = true;
                } else {
                    all_avail = false;
                }
                k += len;
            }
            if self.decoded[y4_above * self.w4 + x4_left] == self.slice_addr {
                refs[2 * n] = i32::from(plane[(y0u - 1) * stride + x0u - 1]);
                avail[2 * n] = true;
                any = true;
            } else {
                all_avail = false;
            }
            let mut k = 0;
            while k < 2 * n {
                let x = x0u + k;
                let len = (unit_x - x % unit_x).min(2 * n - k);
                if self.decoded[y4_above * self.w4 + ((x * sw as usize) >> 2)] == self.slice_addr {
                    let row = (y0u - 1) * stride + x;
                    for ((r, a), &v) in refs[2 * n + 1 + k..2 * n + 1 + k + len]
                        .iter_mut()
                        .zip(&mut avail[2 * n + 1 + k..])
                        .zip(&plane[row..row + len])
                    {
                        *r = i32::from(v);
                        *a = true;
                    }
                    any = true;
                } else {
                    all_avail = false;
                }
                k += len;
            }
            if all_avail {
                return all_refs;
            }
        } else {
            let (xl, yl) = (x0 * sw, y0 * sh);
            let (sw64, sh64) = (i64::from(sw), i64::from(sh));
            let available = |xc: i64, yc: i64| self.available(xl, yl, xc * sw64, yc * sh64);
            // Left column, rows y0..y0+2n, stored from index 2n-1 downwards.
            let mut k = 0;
            while k < 2 * n {
                let y = y0u + k;
                let len = (unit_y - y % unit_y).min(2 * n - k);
                if available(i64::from(x0) - 1, y as i64) {
                    for j in 0..len {
                        refs[2 * n - 1 - k - j] = i32::from(plane[(y + j) * stride + x0u - 1]);
                        avail[2 * n - 1 - k - j] = true;
                    }
                    any = true;
                }
                k += len;
            }
            // Corner.
            if available(i64::from(x0) - 1, i64::from(y0) - 1) {
                refs[2 * n] = i32::from(plane[(y0u - 1) * stride + x0u - 1]);
                avail[2 * n] = true;
                any = true;
            }
            // Top row, columns x0..x0+2n, contiguous in the plane.
            let mut k = 0;
            while k < 2 * n {
                let x = x0u + k;
                let len = (unit_x - x % unit_x).min(2 * n - k);
                if available(x as i64, i64::from(y0) - 1) {
                    let row = (y0u - 1) * stride + x;
                    for ((r, a), &v) in refs[2 * n + 1 + k..2 * n + 1 + k + len]
                        .iter_mut()
                        .zip(&mut avail[2 * n + 1 + k..])
                        .zip(&plane[row..row + len])
                    {
                        *r = i32::from(v);
                        *a = true;
                    }
                    any = true;
                }
                k += len;
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
        all_refs
    }

    /// Intra prediction from reference samples (§8.4.4.2.3 to §8.4.4.2.6).
    fn predict_with(
        &self,
        c: usize,
        n: usize,
        mode: u8,
        transquant_bypass: bool,
        all_refs: &[i32; 4 * 32 + 1],
        pred: &mut [i32],
    ) {
        // disableIntraBoundaryFilter (range extension).
        let boundary_filters = c == 0
            && n < 32
            && !(self.sps.range_extension.implicit_rdpcm_enabled_flag && transquant_bypass);
        let bit_depth = self.frame.bit_depth[usize::from(c > 0)];
        let total = 4 * n + 1;
        let mut smoothed = [0i32; 4 * 32 + 1];
        let mut refs: &[i32] = &all_refs[..total];
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
                let f = &mut smoothed[..total];
                f[0] = refs[0];
                f[total - 1] = refs[total - 1];
                if bi_int {
                    f[2 * n] = corner;
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
                refs = &smoothed[..total];
            }
        }

        let max = (1 << bit_depth) - 1;
        let n_i = n as i32;
        let log2n = n.trailing_zeros();
        match mode {
            0 => {
                // Planar.
                let (tr, bl) = (top(refs, n_i), left(refs, n_i));
                let tops = &refs[2 * n + 1..3 * n + 1];
                for (y, row) in pred.chunks_exact_mut(n).enumerate() {
                    let y = y as i32;
                    let l = left(refs, y);
                    let row_base = (n_i - 1) * l + tr + (y + 1) * bl + n_i;
                    let dl = tr - l;
                    let ym = n_i - 1 - y;
                    for (x, (o, &t)) in row.iter_mut().zip(tops).enumerate() {
                        *o = (row_base + (x as i32) * dl + ym * t) >> (log2n + 1);
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
            26 => {
                let top_row = &refs[2 * n + 1..3 * n + 1];
                for line in pred.chunks_exact_mut(n) {
                    line.copy_from_slice(top_row);
                }
                if boundary_filters {
                    let corner = refs[2 * n];
                    let top0 = top_row[0];
                    for y in 0..n {
                        pred[y * n] = (top0 + ((refs[2 * n - 1 - y] - corner) >> 1)).clamp(0, max);
                    }
                }
            }
            10 => {
                for (y, line) in pred.chunks_exact_mut(n).enumerate() {
                    line.fill(refs[2 * n - 1 - y]);
                }
                if boundary_filters {
                    let corner = refs[2 * n];
                    let left0 = refs[2 * n - 1];
                    let top_row = &refs[2 * n + 1..3 * n + 1];
                    for (o, &t) in pred[..n].iter_mut().zip(top_row) {
                        *o = (left0 + ((t - corner) >> 1)).clamp(0, max);
                    }
                }
            }
            _ => {
                let angle = INTRA_PRED_ANGLE[usize::from(mode) - 2];
                let vertical = mode >= 18;
                // ref[] with offset n so that negative indices are representable, or slice
                // `refs[2*n..=4*n]` directly for vertical modes with non-negative angle.
                let mut r = [0i32; 3 * 32 + 1];
                let (ref_base, ref_slice): (usize, &[i32]) = if vertical && angle >= 0 {
                    (2 * n, refs)
                } else {
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
                    (n, &r)
                };
                // j runs along the prediction direction: rows for vertical modes. Rows are
                // computed contiguously (vectorizable), then transposed for horizontal modes.
                for (j, line) in pred.chunks_exact_mut(n).enumerate() {
                    let j = j as i32;
                    let idx = ((j + 1) * angle) >> 5;
                    let fact = ((j + 1) * angle) & 31;
                    let src = &ref_slice[(idx + 1 + ref_base as i32) as usize..];
                    if fact == 0 {
                        line.copy_from_slice(&src[..n]);
                    } else {
                        for ((o, &a), &b) in line.iter_mut().zip(src).zip(&src[1..]) {
                            *o = ((32 - fact) * a + fact * b + 16) >> 5;
                        }
                    }
                }
                if !vertical {
                    for y in 0..n {
                        for x in y + 1..n {
                            pred.swap(y * n + x, x * n + y);
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
            return i32::from(list.lists[0][matrix_id][usize::from(DIAGONAL_INDEX_4[y * 4 + x])]);
        }
        let ratio = 1usize << (size_id - 1); // 1 for 8×8, 2 for 16×16, 4 for 32×32
        if size_id >= 2 && x == 0 && y == 0 {
            return i32::from(list.dc[size_id - 2][matrix_id]);
        }
        let (xs, ys) = (x / ratio, y / ratio);
        i32::from(list.lists[size_id][matrix_id][usize::from(DIAGONAL_INDEX_8[ys * 8 + xs])])
    }

    /// Range extension tools for blocks coded without transform (transform skip or
    /// transquant bypass): 180° rotation of intra 4×4 blocks, then RDPCM, the residual
    /// accumulated along a direction: implicit for intra horizontal/vertical blocks,
    /// explicit (signalled) for inter blocks.
    fn rotate_and_rdpcm(&self, tb: &TransformBlock<'_>, r: &mut [i32]) {
        let n = 1usize << tb.log2_size;
        let ext = &self.sps.range_extension;
        let vertical = if tb.inter {
            tb.explicit_rdpcm
        } else {
            if ext.transform_skip_rotation_enabled_flag && n == 4 {
                r.reverse();
            }
            match tb.intra_mode {
                10 if ext.implicit_rdpcm_enabled_flag => Some(false),
                26 if ext.implicit_rdpcm_enabled_flag => Some(true),
                _ => None,
            }
        };
        match vertical {
            Some(false) => {
                for y in 0..n {
                    for x in 1..n {
                        r[y * n + x] += r[y * n + x - 1];
                    }
                }
            }
            Some(true) => {
                for y in 1..n {
                    for x in 0..n {
                        r[y * n + x] += r[(y - 1) * n + x];
                    }
                }
            }
            None => {}
        }
    }

    /// Computes the residual of a block from its coefficients (§8.6.2 to §8.6.4).
    /// `d` and `g_buf` are scratch buffers of at least `n * n` values.
    fn residual(
        &self,
        tb: &TransformBlock<'_>,
        coeffs: &[i32],
        out: &mut [i32],
        d: &mut [i32],
        g_buf: &mut [i32],
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
        // matrixId: intra Y, Cb, Cr then inter Y, Cb, Cr (Table 7-4).
        let matrix_id = c + if tb.inter { 3 } else { 0 };
        // Only the rectangle holding the non-zero coefficients is scaled, padded with zeros to
        // the width the transform reads; transform skip uses the whole block.
        let (cols, rows) = if tb.transform_skip {
            (n, n)
        } else {
            let (c, r) = tb.coeff_bounds;
            (usize::from(c).clamp(1, n), usize::from(r).clamp(1, n))
        };
        let width = if tb.transform_skip {
            n
        } else {
            (cols.div_ceil(8) * 8).min(n)
        };
        let d = &mut d[..n * n];
        let rnd = 1i64 << (bd_shift - 1);
        let flat_factor = 16 * scale;
        if flat {
            if flat_factor <= 65000 {
                let ff = flat_factor as i32;
                let rnd32 = rnd as i32;
                for (src, dst) in coeffs.chunks_exact(n).zip(d.chunks_exact_mut(n)).take(rows) {
                    dst[cols..width].fill(0);
                    for (&level, o) in src[..cols].iter().zip(&mut dst[..cols]) {
                        *o = ((level * ff + rnd32) >> bd_shift).clamp(-32768, 32767);
                    }
                }
            } else {
                for (src, dst) in coeffs.chunks_exact(n).zip(d.chunks_exact_mut(n)).take(rows) {
                    dst[cols..width].fill(0);
                    for (&level, o) in src[..cols].iter().zip(&mut dst[..cols]) {
                        *o = ((i64::from(level) * flat_factor + rnd) >> bd_shift)
                            .clamp(-32768, 32767) as i32;
                    }
                }
            }
        } else {
            let scaling_list = list.unwrap();
            for (y, (src, dst)) in coeffs
                .chunks_exact(n)
                .zip(d.chunks_exact_mut(n))
                .take(rows)
                .enumerate()
            {
                dst[cols..width].fill(0);
                for (x, (&level, o)) in src[..cols].iter().zip(&mut dst[..cols]).enumerate() {
                    let factor = i64::from(Self::scaling_factor(
                        scaling_list,
                        tb.log2_size,
                        matrix_id,
                        x,
                        y,
                    )) * scale;
                    // A zero level scales to `rnd >> bd_shift`, which is zero for every `bd_shift`
                    // in range, so no special case is needed here.
                    *o =
                        ((i64::from(level) * factor + rnd) >> bd_shift).clamp(-32768, 32767) as i32;
                }
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

        // Inverse transform (§8.6.4.2): columns, clip, then rows, over the non-zero rectangle.
        let dst = c == 0 && n == 4 && !tb.inter;
        let rnd2 = 1 << (bd_shift2 - 1);
        if cols == 1 && rows == 1 && !dst {
            // DC only: every DCT basis row 0 coefficient is 64, so the residual is constant.
            let t = ((64 * d[0] + 64) >> 7).clamp(-32768, 32767);
            out[..n * n].fill((64 * t + rnd2) >> bd_shift2);
            return;
        }
        let out = &mut out[..n * n];
        match n {
            4 if dst => inverse_2d::<4, false>(&DST4, d, g_buf, rows, cols, bd_shift2, out),
            4 => inverse_2d::<4, true>(&DCT4, d, g_buf, rows, cols, bd_shift2, out),
            8 => inverse_2d::<8, true>(&DCT8, d, g_buf, rows, cols, bd_shift2, out),
            16 => inverse_2d::<16, true>(&DCT16, d, g_buf, rows, cols, bd_shift2, out),
            _ => inverse_2d::<32, true>(&DCT32, d, g_buf, rows, cols, bd_shift2, out),
        }
    }

    /// Like [`Sink::transform_block`], with the intra prediction of the block already
    /// computed (by [`predict_block`](Self::predict_block) or
    /// [`predict_from`](Self::predict_from)): for encoders, which predict before coding.
    pub fn reconstruct(&mut self, tb: &TransformBlock<'_>, prediction: &[i32]) {
        if self.error.is_none() {
            self.transform_block_impl(tb, Some(prediction));
        }
    }

    fn transform_block_impl(&mut self, tb: &TransformBlock<'_>, prediction: Option<&[i32]>) {
        let n = 1usize << tb.log2_size;
        let c = usize::from(tb.c_idx);
        let ccp = self
            .pps
            .range_extension
            .cross_component_prediction_enabled_flag;
        if tb.coeffs.is_none() && (c == 0 || tb.res_scale_val == 0) {
            if c == 0 && ccp {
                self.luma_residual[..n * n].fill(0);
            }
            if !(tb.inter && prediction.is_none()) {
                let mut scratch = std::mem::take(&mut self.scratch);
                let pred = &mut scratch[..n * n];
                match prediction {
                    Some(p) => pred.copy_from_slice(&p[..n * n]),
                    None => {
                        self.predict(c, tb.x, tb.y, n, tb.intra_mode, tb.transquant_bypass, pred)
                    }
                }
                let max = (1i32 << self.frame.bit_depth[usize::from(c > 0)]) - 1;
                let stride = self.frame.widths[c] as usize;
                let plane = &mut self.frame.planes[c];
                for (y, p) in pred.chunks_exact(n).enumerate() {
                    let row = (tb.y as usize + y) * stride + tb.x as usize;
                    for (o, &p) in plane[row..row + n].iter_mut().zip(p) {
                        *o = p.clamp(0, max) as u16;
                    }
                }
                self.scratch = scratch;
            }
            if c == 0 {
                if !(tb.inter && self.pps.constrained_intra_pred_flag) {
                    self.mark_decoded(tb.x, tb.y, n as u32, n as u32);
                }
                self.mark_edges(tb.x, tb.y, n as u32, n as u32);
            }
            return;
        }
        let mut scratch = std::mem::take(&mut self.scratch);
        let (pred, rest) = scratch.split_at_mut(32 * 32);
        let (residual, rest) = rest.split_at_mut(32 * 32);
        let (d, g_buf) = rest.split_at_mut(32 * 32);
        let (pred, residual) = (&mut pred[..n * n], &mut residual[..n * n]);
        match prediction {
            Some(p) => pred.copy_from_slice(&p[..n * n]),
            None if tb.inter => {
                // Motion compensation already wrote the prediction into the picture.
                let stride = self.frame.widths[c] as usize;
                for (y, row) in pred.chunks_exact_mut(n).enumerate() {
                    let start = (tb.y as usize + y) * stride + tb.x as usize;
                    for (o, &v) in row.iter_mut().zip(&self.frame.planes[c][start..start + n]) {
                        *o = i32::from(v);
                    }
                }
            }
            None => self.predict(c, tb.x, tb.y, n, tb.intra_mode, tb.transquant_bypass, pred),
        }

        if let Some(coeffs) = tb.coeffs {
            self.residual(tb, coeffs, residual, d, g_buf);
        } else {
            residual.fill(0);
        }
        if c == 0 && ccp {
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
        for (y, (p, r)) in pred
            .chunks_exact(n)
            .zip(residual.chunks_exact(n))
            .enumerate()
        {
            let row = (tb.y as usize + y) * stride + tb.x as usize;
            for ((o, &p), &r) in plane[row..row + n].iter_mut().zip(p).zip(r) {
                *o = (p + r).clamp(0, max) as u16;
            }
        }
        self.scratch = scratch;
        if c == 0 {
            // With constrained intra prediction, inter samples are never intra references.
            if !(tb.inter && self.pps.constrained_intra_pred_flag) {
                self.mark_decoded(tb.x, tb.y, n as u32, n as u32);
            }
            self.mark_edges(tb.x, tb.y, n as u32, n as u32);
            if tb.coeffs.is_some() {
                let info = &mut self.filter_info;
                let x4 = tb.x as usize >> 2;
                let x4_end = (tb.x as usize + n).div_ceil(4).min(info.w4);
                if x4 < x4_end {
                    for yy in (tb.y as usize >> 2)..((tb.y as usize + n).div_ceil(4)).min(info.h4) {
                        let row = yy * info.w4;
                        info.coded[row + x4..row + x4_end].fill(true);
                    }
                }
            }
        }
    }

    /// Motion compensated prediction of an inter block (§8.5.3.3), written into the picture.
    fn predict_inter(&mut self, pu: &PredictionUnit) -> Result<(), Error> {
        let m = pu.motion;
        // The reference picture, its POC and its explicit weights, for each list used.
        let mut used = [None, None];
        let mut ref_lt = [false; 2];
        for (list, slot) in used.iter_mut().enumerate() {
            if !m.uses(list) {
                continue;
            }
            let i = m.ref_idx[list] as usize;
            let (reference, poc, lt) = *self.refs[list]
                .get(i)
                .ok_or(Error::Invalid("reference index outside the reference list"))?;
            if reference.widths != self.frame.widths || reference.heights != self.frame.heights {
                return Err(Error::Invalid("reference picture of another size"));
            }
            let weight = match &self.weights[list] {
                Some(w) => Some(
                    *w.get(i)
                        .ok_or(Error::Invalid("reference index outside the weight table"))?,
                ),
                None => None,
            };
            *slot = Some((reference, poc, weight));
            ref_lt[list] = lt;
        }
        if used == [None, None] {
            return Err(Error::Invalid("inter block without a reference list"));
        }
        let sub = self.sps.chroma_subsampling();
        let planes = if self.sps.chroma_array_type() == 0 {
            1
        } else {
            3
        };
        let n = (pu.w * pu.h) as usize;
        let mut buf = std::mem::take(&mut self.inter_scratch);
        buf.resize(2 * n, 0);
        let (buf0, buf1) = buf.split_at_mut(n);
        for c in 0..planes {
            let (x, y, w, h) = if c == 0 {
                (pu.x, pu.y, pu.w, pu.h)
            } else {
                (pu.x / sub.0, pu.y / sub.1, pu.w / sub.0, pu.h / sub.1)
            };
            let len = (w * h) as usize;
            let (out0, out1) = (&mut buf0[..len], &mut buf1[..len]);
            for (list, out) in [(0, &mut *out0), (1, &mut *out1)] {
                if let Some((reference, _, _)) = used[list] {
                    inter::predict_component(
                        reference, c, pu.x, pu.y, pu.w, pu.h, m.mv[list], sub, out,
                    );
                }
            }
            let bit_depth = self.frame.bit_depth[usize::from(c > 0)];
            let denom_c = usize::from(c > 0);
            let stride = self.frame.widths[c] as usize;
            let plane = &mut self.frame.planes[c];
            for yy in 0..h as usize {
                let start = (y as usize + yy) * stride + x as usize;
                let dst = &mut plane[start..start + w as usize];
                let row = yy * w as usize..(yy + 1) * w as usize;
                let (r0, r1) = (&out0[row.clone()], &out1[row]);
                match (used[0], used[1]) {
                    (Some((_, _, w0)), Some((_, _, w1))) => match (w0, w1) {
                        (Some(w0), Some(w1)) => {
                            for (o, (&p0, &p1)) in dst.iter_mut().zip(r0.iter().zip(r1)) {
                                *o = inter::weight_explicit_bi(
                                    [p0, p1],
                                    bit_depth,
                                    [w0.weight[c], w1.weight[c]],
                                    [w0.offset[c], w1.offset[c]],
                                    w0.log2_denom[denom_c],
                                );
                            }
                        }
                        _ => {
                            for (o, (&p0, &p1)) in dst.iter_mut().zip(r0.iter().zip(r1)) {
                                *o = inter::weight_bi(p0, p1, bit_depth);
                            }
                        }
                    },
                    (Some((_, _, wt)), None) | (None, Some((_, _, wt))) => {
                        let r = if used[0].is_some() { r0 } else { r1 };
                        match wt {
                            None => {
                                for (o, &p) in dst.iter_mut().zip(r) {
                                    *o = inter::weight_uni(p, bit_depth);
                                }
                            }
                            Some(wt) => {
                                for (o, &p) in dst.iter_mut().zip(r) {
                                    *o = inter::weight_explicit(
                                        p,
                                        bit_depth,
                                        wt.weight[c],
                                        wt.offset[c],
                                        wt.log2_denom[denom_c],
                                    );
                                }
                            }
                        }
                    }
                    (None, None) => unreachable!("checked above"),
                }
            }
        }
        self.inter_scratch = buf;
        if !self.pps.constrained_intra_pred_flag {
            self.mark_decoded(pu.x, pu.y, pu.w, pu.h);
        }
        // Prediction block edges, and the motion the deblocking filter compares.
        let info = &mut self.filter_info;
        let (x4, y4) = (pu.x as usize >> 2, pu.y as usize >> 2);
        let motion = EdgeMotion {
            mv: [0, 1].map(|l| if m.uses(l) { m.mv[l] } else { [0; 2] }),
            ref_poc: used.map(|u| u.map(|(_, poc, _)| poc)),
            ref_lt,
        };
        for yy in y4..((pu.y + pu.h) as usize).div_ceil(4).min(info.h4) {
            info.edges[yy * info.w4 + x4] |= 4;
            for xx in x4..((pu.x + pu.w) as usize).div_ceil(4).min(info.w4) {
                info.motion[yy * info.w4 + xx] = motion;
            }
        }
        for xx in x4..((pu.x + pu.w) as usize).div_ceil(4).min(info.w4) {
            info.edges[y4 * info.w4 + xx] |= 8;
        }
        Ok(())
    }
}

impl Sink for Reconstructor<'_> {
    fn transform_block(&mut self, tb: &TransformBlock<'_>) {
        if self.error.is_none() {
            self.transform_block_impl(tb, None);
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
        let intra = cu.intra;
        let no_filter = cu.transquant_bypass
            || (cu.pcm && self.sps.pcm.is_some_and(|p| p.loop_filter_disabled_flag));
        let slice = (self.filter_info.slices.len().max(1) - 1) as u16;
        let info = &mut self.filter_info;
        let x4 = cu.x as usize >> 2;
        let x4_end = ((cu.x + size) as usize).div_ceil(4).min(info.w4);
        if x4 >= x4_end {
            return;
        }
        let y4_end = ((cu.y + size) as usize).div_ceil(4).min(info.h4);
        let qp = cu.qp_y as i8;
        for yy in (cu.y as usize >> 2)..y4_end {
            let range = yy * info.w4 + x4..yy * info.w4 + x4_end;
            info.qp_y[range].fill(qp);
        }
        if !intra {
            for yy in (cu.y as usize >> 2)..y4_end {
                let range = yy * info.w4 + x4..yy * info.w4 + x4_end;
                info.intra[range].fill(false);
            }
        }
        if no_filter {
            for yy in (cu.y as usize >> 2)..y4_end {
                let range = yy * info.w4 + x4..yy * info.w4 + x4_end;
                info.no_filter[range].fill(true);
            }
        }
        if slice != 0 {
            for yy in (cu.y as usize >> 2)..y4_end {
                let range = yy * info.w4 + x4..yy * info.w4 + x4_end;
                info.slice[range].fill(slice);
            }
        }
    }

    fn prediction_unit(&mut self, pu: &PredictionUnit) {
        if self.error.is_none()
            && let Err(e) = self.predict_inter(pu)
        {
            self.error = Some(e);
        }
    }

    fn sao(&mut self, ctb_x: u32, ctb_y: u32, params: &SaoParams) {
        self.filter_info.sao[(ctb_y * self.layout.width + ctb_x) as usize] = *params;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The partial butterfly gives exactly the matrix product, for every size and number of
    /// non-zero inputs, including the 16-bit extremes.
    #[test]
    fn butterfly_matches_matrix_product() {
        fn check<const N: usize>(m: &[[i32; N]; N]) {
            let mut seed = 0x9e37_79b9_u32;
            let mut next = || {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed
            };
            for round in 0..2000 {
                let mut src = [0i32; N];
                for v in &mut src {
                    *v = match round % 4 {
                        0 => i32::from(next() as i16),
                        1 => [-32768, 32767, 0][(next() % 3) as usize],
                        2 => (next() % 9) as i32 - 4,
                        _ => {
                            if next() % 4 == 0 {
                                i32::from(next() as i16)
                            } else {
                                0
                            }
                        }
                    };
                }
                for terms in 0..=N {
                    assert_eq!(
                        idct_1d::<N>(&src, terms),
                        mul_1d::<N>(m, &src, terms),
                        "N = {N}, terms = {terms}"
                    );
                }
            }
        }
        check::<4>(&DCT4);
        check::<8>(&DCT8);
        check::<16>(&DCT16);
        check::<32>(&DCT32);
    }

    #[test]
    fn fast_dst4_and_unzeroed_scratch_match_matrix_product() {
        let mut seed = 0x517c_c1b7_u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for _ in 0..2000 {
            let src: [i32; 4] = core::array::from_fn(|_| i32::from(next() as i16));
            for terms in 0..=4 {
                assert_eq!(idst4(&src, terms), mul_1d::<4>(&DST4, &src, terms));
            }
        }
        let d: Vec<i32> = (0..64).map(|_| i32::from(next() as i16)).collect();
        let mut zero_g = vec![0i32; 64];
        let mut dirty_g = vec![0x1234_5678_i32; 64];
        let mut out_clean = vec![0i32; 64];
        let mut out_dirty = vec![0i32; 64];
        inverse_2d::<8, true>(&DCT8, &d, &mut zero_g, 3, 5, 12, &mut out_clean);
        inverse_2d::<8, true>(&DCT8, &d, &mut dirty_g, 3, 5, 12, &mut out_dirty);
        assert_eq!(out_clean, out_dirty);
    }

    use crate::scan::{ScanType, scan_order};

    #[test]
    fn diagonal_index_matches_scan_order() {
        for (log2, table) in [(2u8, &DIAGONAL_INDEX_4[..]), (3, &DIAGONAL_INDEX_8[..])] {
            let n = 1usize << log2;
            for (i, &(x, y)) in scan_order(log2, ScanType::Diagonal).iter().enumerate() {
                assert_eq!(usize::from(table[usize::from(y) * n + usize::from(x)]), i);
            }
        }
    }
}
