//! Picture reconstruction: intra prediction (§8.4.4.2), scaling and inverse transforms
//! (§8.6), and adding the residual. In-loop filters (deblocking, SAO) are applied afterwards.

use crate::Error;
use crate::filters::{self, EdgeMotion, FilterInfo, SliceFilterParams};
use crate::inter;
use crate::params::{Pps, ScalingList, Sps};
use crate::scan::CtbLayout;
use crate::syntax::{CodingUnit, PredictionUnit, SaoParams, Sink, TransformBlock};

/// Sample storage abstraction shared between 8-bit (`u8`) and wide (`u16`) decoded pictures.
pub(crate) trait PlaneSample: Copy + Default + Send + Sync + 'static {
    fn to_i32(self) -> i32;
    fn from_clamped(v: i32, max: i32) -> Self;
    fn from_in_range(v: i32) -> Self;
    fn mid(bit_depth: u8) -> Self;
    fn from_u16_sample(v: u16) -> Self;
    fn sao_eo_row(dst: &mut [Self], src: &[Self], a: &[Self], b: &[Self], eo: [i16; 4], max: i32);
}

#[inline]
fn sao_eo_row_scalar<T: PlaneSample>(
    dst: &mut [T],
    src: &[T],
    a: &[T],
    b: &[T],
    eo: [i16; 4],
    max: i32,
) {
    let [e0, e1, e2, e3] = eo.map(i32::from);
    for (((d, &v), &av), &bv) in dst.iter_mut().zip(src).zip(a).zip(b) {
        let vi = v.to_i32();
        let ai = av.to_i32();
        let bi = bv.to_i32();
        let c =
            (i32::from(vi > ai) - i32::from(vi < ai)) + (i32::from(vi > bi) - i32::from(vi < bi));
        let mut off = 0i32;
        if c == -2 {
            off = e0;
        }
        if c == -1 {
            off = e1;
        }
        if c == 1 {
            off = e2;
        }
        if c == 2 {
            off = e3;
        }
        *d = T::from_clamped(vi + off, max);
    }
}

impl PlaneSample for u8 {
    #[inline(always)]
    fn to_i32(self) -> i32 {
        i32::from(self)
    }

    #[inline(always)]
    fn from_clamped(v: i32, _max: i32) -> Self {
        v.clamp(0, 255) as u8
    }

    #[inline(always)]
    fn from_in_range(v: i32) -> Self {
        v as u8
    }

    #[inline(always)]
    fn mid(_bit_depth: u8) -> Self {
        128
    }

    #[inline(always)]
    fn from_u16_sample(v: u16) -> Self {
        v as u8
    }

    #[inline]
    fn sao_eo_row(dst: &mut [Self], src: &[Self], a: &[Self], b: &[Self], eo: [i16; 4], max: i32) {
        if max == 255 {
            if let (Ok(e0), Ok(e1), Ok(e2), Ok(e3)) = (
                i8::try_from(eo[0]),
                i8::try_from(eo[1]),
                i8::try_from(eo[2]),
                i8::try_from(eo[3]),
            ) {
                for (((d, &v), &av), &bv) in dst.iter_mut().zip(src).zip(a).zip(b) {
                    let c = (i8::from(v > av) - i8::from(v < av))
                        + (i8::from(v > bv) - i8::from(v < bv));
                    let mut off = 0i8;
                    if c == -2 {
                        off = e0;
                    }
                    if c == -1 {
                        off = e1;
                    }
                    if c == 1 {
                        off = e2;
                    }
                    if c == 2 {
                        off = e3;
                    }
                    *d = v.saturating_add_signed(off);
                }
                return;
            }
        }
        sao_eo_row_scalar(dst, src, a, b, eo, max);
    }
}

impl PlaneSample for u16 {
    #[inline(always)]
    fn to_i32(self) -> i32 {
        i32::from(self)
    }

    #[inline(always)]
    fn from_clamped(v: i32, max: i32) -> Self {
        v.clamp(0, max) as u16
    }

    #[inline(always)]
    fn from_in_range(v: i32) -> Self {
        v as u16
    }

    #[inline(always)]
    fn mid(bit_depth: u8) -> Self {
        1u16 << (bit_depth - 1)
    }

    #[inline(always)]
    fn from_u16_sample(v: u16) -> Self {
        v
    }

    #[inline]
    fn sao_eo_row(dst: &mut [Self], src: &[Self], a: &[Self], b: &[Self], eo: [i16; 4], max: i32) {
        sao_eo_row_scalar(dst, src, a, b, eo, max);
    }
}

/// Conformance crop window of a decoded picture, in per-plane sample coordinates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CropWindow {
    /// Left crop offset in samples for `[Y, Cb, Cr]`.
    pub x: [u32; 3],
    /// Top crop offset in samples for `[Y, Cb, Cr]`.
    pub y: [u32; 3],
    /// Cropped width in samples for `[Y, Cb, Cr]`.
    pub widths: [u32; 3],
    /// Cropped height in samples for `[Y, Cb, Cr]`.
    pub heights: [u32; 3],
}

impl CropWindow {
    /// Uncropped window covering all samples of `widths` × `heights`.
    pub fn full(widths: [u32; 3], heights: [u32; 3]) -> Self {
        Self {
            x: [0; 3],
            y: [0; 3],
            widths,
            heights,
        }
    }

    /// Conformance crop window derived from `sps.conf_win_offsets`.
    pub fn from_sps(widths: [u32; 3], heights: [u32; 3], sps: &Sps) -> Self {
        let (sw, sh) = sps.chroma_subsampling();
        let [l, r, t, b] = sps.conf_win_offsets;
        let mut out = Self {
            x: [0; 3],
            y: [0; 3],
            widths,
            heights,
        };
        for c in 0..3 {
            if widths[c] == 0 {
                out.x[c] = 0;
                out.y[c] = 0;
                out.widths[c] = 0;
                out.heights[c] = 0;
                continue;
            }
            let (fx, fy) = if c == 0 { (sw, sh) } else { (1, 1) };
            out.x[c] = l * fx;
            out.y[c] = t * fy;
            out.widths[c] = widths[c] - (l + r) * fx;
            out.heights[c] = heights[c] - (t + b) * fy;
        }
        out
    }
}

#[allow(clippy::too_many_arguments)]
fn reset_plane_storage<T: PlaneSample>(
    sps: &Sps,
    widths: &mut [u32; 3],
    heights: &mut [u32; 3],
    planes: &mut [Vec<T>; 3],
    bit_depth: &mut [u8; 2],
    chroma_format: &mut u8,
    full_range: &mut bool,
    matrix_coeffs: &mut u8,
) {
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
    *widths = [w, cw, cw];
    *heights = [h, ch, ch];
    for (c, (len, fill)) in [
        ((w * h) as usize, T::mid(sps.bit_depth_luma)),
        ((cw * ch) as usize, T::mid(sps.bit_depth_chroma)),
        ((cw * ch) as usize, T::mid(sps.bit_depth_chroma)),
    ]
    .into_iter()
    .enumerate()
    {
        planes[c].clear();
        planes[c].resize(len, fill);
    }
    *bit_depth = [sps.bit_depth_luma, sps.bit_depth_chroma];
    *chroma_format = sps.chroma_format_idc;
    *full_range = sps.vui.as_ref().is_some_and(|v| v.video_full_range_flag);
    *matrix_coeffs = sps.vui.as_ref().map_or(2, |v| v.matrix_coeffs);
}

/// A decoded picture: three planes of samples (only the first one for monochrome).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
    pub(crate) fn reset(&mut self, sps: &Sps) {
        reset_plane_storage(
            sps,
            &mut self.widths,
            &mut self.heights,
            &mut self.planes,
            &mut self.bit_depth,
            &mut self.chroma_format,
            &mut self.full_range,
            &mut self.matrix_coeffs,
        );
    }

    /// Uncropped conformance window covering all samples of `self`.
    pub fn full_window(&self) -> CropWindow {
        CropWindow::full(self.widths, self.heights)
    }

    /// Conformance crop window of `self` derived from `sps`.
    pub fn crop_window(&self, sps: &Sps) -> CropWindow {
        CropWindow::from_sps(self.widths, self.heights, sps)
    }

    /// Returns a copy cropped to the conformance window of `sps`.
    pub fn cropped(&self, sps: &Sps) -> Self {
        let mut out = Self::default();
        self.cropped_into(sps, &mut out);
        out
    }

    pub(crate) fn cropped_into(&self, sps: &Sps, out: &mut Self) {
        let crop = self.crop_window(sps);
        out.widths = crop.widths;
        out.heights = crop.heights;
        out.bit_depth = self.bit_depth;
        out.chroma_format = self.chroma_format;
        out.full_range = self.full_range;
        out.matrix_coeffs = self.matrix_coeffs;
        for c in 0..3 {
            if crop.widths[c] == 0 {
                out.planes[c].clear();
                continue;
            }
            let src_w = self.widths[c] as usize;
            let (x0, y0) = (crop.x[c] as usize, crop.y[c] as usize);
            let (w, h) = (crop.widths[c] as usize, crop.heights[c] as usize);
            out.planes[c].clear();
            out.planes[c].reserve(w * h);
            for y in y0..y0 + h {
                let row = y * src_w + x0;
                out.planes[c].extend_from_slice(&self.planes[c][row..row + w]);
            }
        }
    }
}

/// An 8-bit decoded picture: three `u8` planes (halves memory bandwidth compared to [`Frame`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frame8 {
    /// Width of each plane, in samples.
    pub widths: [u32; 3],
    /// Height of each plane, in samples.
    pub heights: [u32; 3],
    /// Samples, row-major, one `Vec<u8>` per plane.
    pub planes: [Vec<u8>; 3],
    /// Bit depth of luma and chroma samples (always `[8, 8]`).
    pub bit_depth: [u8; 2],
    /// `chroma_format_idc` (0 = monochrome, 1 = 4:2:0, 2 = 4:2:2, 3 = 4:4:4).
    pub chroma_format: u8,
    /// `video_full_range_flag` from the VUI (false when the VUI is absent).
    pub full_range: bool,
    /// `matrix_coeffs` from the VUI (2 = unspecified when absent).
    pub matrix_coeffs: u8,
}

impl Frame8 {
    pub(crate) fn reset(&mut self, sps: &Sps) {
        reset_plane_storage(
            sps,
            &mut self.widths,
            &mut self.heights,
            &mut self.planes,
            &mut self.bit_depth,
            &mut self.chroma_format,
            &mut self.full_range,
            &mut self.matrix_coeffs,
        );
    }

    /// Uncropped conformance window covering all samples of `self`.
    pub fn full_window(&self) -> CropWindow {
        CropWindow::full(self.widths, self.heights)
    }

    /// Conformance crop window of `self` derived from `sps`.
    pub fn crop_window(&self, sps: &Sps) -> CropWindow {
        CropWindow::from_sps(self.widths, self.heights, sps)
    }

    /// Copies the cropped region `crop` into a 16-bit [`Frame`] (for wide callers/tests).
    pub fn to_frame_cropped(&self, crop: CropWindow) -> Frame {
        let mut planes = [Vec::new(), Vec::new(), Vec::new()];
        for c in 0..3 {
            if crop.widths[c] == 0 {
                continue;
            }
            let src_w = self.widths[c] as usize;
            let (x0, y0) = (crop.x[c] as usize, crop.y[c] as usize);
            let (w, h) = (crop.widths[c] as usize, crop.heights[c] as usize);
            planes[c].reserve(w * h);
            for y in y0..y0 + h {
                let row = y * src_w + x0;
                planes[c].extend(self.planes[c][row..row + w].iter().copied().map(u16::from));
            }
        }
        Frame {
            widths: crop.widths,
            heights: crop.heights,
            planes,
            bit_depth: self.bit_depth,
            chroma_format: self.chroma_format,
            full_range: self.full_range,
            matrix_coeffs: self.matrix_coeffs,
        }
    }
}

/// Borrowed view of a decoded picture and its conformance crop window.
#[derive(Debug, Clone, Copy)]
pub enum DecodedFrameView<'a> {
    /// 8-bit picture (`bit_depth == [8, 8]`) stored directly in `u8` planes.
    U8(&'a Frame8, CropWindow),
    /// General picture (8–16 bit) stored in `u16` planes.
    U16(&'a Frame, CropWindow),
}

impl DecodedFrameView<'_> {
    /// Conformance crop window of this picture.
    pub fn crop(&self) -> CropWindow {
        match *self {
            Self::U8(_, crop) | Self::U16(_, crop) => crop,
        }
    }

    /// Cropped luma width in pixels.
    pub fn width(&self) -> u32 {
        self.crop().widths[0]
    }

    /// Cropped luma height in pixels.
    pub fn height(&self) -> u32 {
        self.crop().heights[0]
    }

    /// Bit depth of luma and chroma samples.
    pub fn bit_depth(&self) -> [u8; 2] {
        match *self {
            Self::U8(f, _) => f.bit_depth,
            Self::U16(f, _) => f.bit_depth,
        }
    }

    /// `chroma_format_idc` (0 = monochrome, 1 = 4:2:0, 2 = 4:2:2, 3 = 4:4:4).
    pub fn chroma_format(&self) -> u8 {
        match *self {
            Self::U8(f, _) => f.chroma_format,
            Self::U16(f, _) => f.chroma_format,
        }
    }

    /// `video_full_range_flag` from the VUI.
    pub fn full_range(&self) -> bool {
        match *self {
            Self::U8(f, _) => f.full_range,
            Self::U16(f, _) => f.full_range,
        }
    }

    /// `matrix_coeffs` from the VUI.
    pub fn matrix_coeffs(&self) -> u8 {
        match *self {
            Self::U8(f, _) => f.matrix_coeffs,
            Self::U16(f, _) => f.matrix_coeffs,
        }
    }
}

pub(crate) trait ReconFrame {
    type Sample: PlaneSample;
    fn widths(&self) -> &[u32; 3];
    fn heights(&self) -> &[u32; 3];
    fn bit_depth(&self) -> &[u8; 2];
    fn plane(&self, c: usize) -> &[Self::Sample];
    fn plane_mut(&mut self, c: usize) -> &mut [Self::Sample];
    fn reset(&mut self, sps: &Sps);
}

impl ReconFrame for Frame {
    type Sample = u16;
    fn widths(&self) -> &[u32; 3] {
        &self.widths
    }
    fn heights(&self) -> &[u32; 3] {
        &self.heights
    }
    fn bit_depth(&self) -> &[u8; 2] {
        &self.bit_depth
    }
    fn plane(&self, c: usize) -> &[u16] {
        &self.planes[c]
    }
    fn plane_mut(&mut self, c: usize) -> &mut [u16] {
        &mut self.planes[c]
    }
    fn reset(&mut self, sps: &Sps) {
        Frame::reset(self, sps);
    }
}

impl ReconFrame for Frame8 {
    type Sample = u8;
    fn widths(&self) -> &[u32; 3] {
        &self.widths
    }
    fn heights(&self) -> &[u32; 3] {
        &self.heights
    }
    fn bit_depth(&self) -> &[u8; 2] {
        &self.bit_depth
    }
    fn plane(&self, c: usize) -> &[u8] {
        &self.planes[c]
    }
    fn plane_mut(&mut self, c: usize) -> &mut [u8] {
        &mut self.planes[c]
    }
    fn reset(&mut self, sps: &Sps) {
        Frame8::reset(self, sps);
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
#[cfg(test)]
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

/// `[x]` stores the four columns `c0..c3` at index `x`.
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
    let t = terms.min(8).div_ceil(2);
    let e = [src[0], src[2], src[4], src[6]];
    butterfly(&ODD8, idct4(&e, t), src, terms)
}

#[inline]
fn idct16(src: &[i32; 16], terms: usize) -> [i32; 16] {
    let t = terms.min(16).div_ceil(2);
    let e = [
        src[0], src[2], src[4], src[6], src[8], src[10], src[12], src[14],
    ];
    butterfly(&ODD16, idct8(&e, t), src, terms)
}

#[inline]
fn idct32(src: &[i32; 32], terms: usize) -> [i32; 32] {
    let t = terms.min(32).div_ceil(2);
    let mut e = [0i32; 16];
    for (dst, s) in e.iter_mut().zip(src.chunks_exact(2)) {
        *dst = s[0];
    }
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

/// One 4-lane vector across four adjacent columns or rows of a transform pass.
type V4 = [i32; 4];

#[inline(always)]
fn vadd(a: V4, b: V4) -> V4 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2], a[3] + b[3]]
}

#[inline(always)]
fn vsub(a: V4, b: V4) -> V4 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2], a[3] - b[3]]
}

#[inline(always)]
fn vmul(a: V4, k: i32) -> V4 {
    [a[0] * k, a[1] * k, a[2] * k, a[3] * k]
}

#[inline(always)]
fn vmla(acc: V4, a: V4, k: i32) -> V4 {
    [
        acc[0] + a[0] * k,
        acc[1] + a[1] * k,
        acc[2] + a[2] * k,
        acc[3] + a[3] * k,
    ]
}

#[inline(always)]
fn vclip16(a: V4) -> V4 {
    [
        ((a[0] + 64) >> 7).clamp(-32768, 32767),
        ((a[1] + 64) >> 7).clamp(-32768, 32767),
        ((a[2] + 64) >> 7).clamp(-32768, 32767),
        ((a[3] + 64) >> 7).clamp(-32768, 32767),
    ]
}

#[inline(always)]
fn vshift(a: V4, rnd: i32, shift: i32) -> V4 {
    [
        (a[0] + rnd) >> shift,
        (a[1] + rnd) >> shift,
        (a[2] + rnd) >> shift,
        (a[3] + rnd) >> shift,
    ]
}

#[inline(always)]
fn transpose4x4(r: [V4; 4]) -> [V4; 4] {
    [
        [r[0][0], r[1][0], r[2][0], r[3][0]],
        [r[0][1], r[1][1], r[2][1], r[3][1]],
        [r[0][2], r[1][2], r[2][2], r[3][2]],
        [r[0][3], r[1][3], r[2][3], r[3][3]],
    ]
}

#[inline]
fn butterfly_x4<const N: usize, const H: usize>(
    odd: &[[i32; H]; H],
    even: [V4; H],
    src: &[V4],
    terms: usize,
) -> [V4; N] {
    let mut o = [[0i32; 4]; H];
    for (i, row) in odd.iter().enumerate().take(terms.min(N) / 2) {
        let c = src[2 * i + 1];
        for j in 0..H {
            o[j] = vmla(o[j], c, row[j]);
        }
    }
    let mut out = [[0i32; 4]; N];
    for j in 0..H {
        out[j] = vadd(even[j], o[j]);
        out[N - 1 - j] = vsub(even[j], o[j]);
    }
    out
}

#[inline]
fn idct4_x4(src: &[V4], terms: usize) -> [V4; 4] {
    let z = [0i32; 4];
    let s0 = if terms > 0 { src[0] } else { z };
    let s1 = if terms > 1 { src[1] } else { z };
    let s2 = if terms > 2 { src[2] } else { z };
    let s3 = if terms > 3 { src[3] } else { z };
    let (e0, e1) = (vmul(vadd(s0, s2), 64), vmul(vsub(s0, s2), 64));
    let o0 = vmla(vmul(s1, 83), s3, 36);
    let o1 = vsub(vmul(s1, 36), vmul(s3, 83));
    [vadd(e0, o0), vadd(e1, o1), vsub(e1, o1), vsub(e0, o0)]
}

#[inline]
fn idct8_x4(src: &[V4], terms: usize) -> [V4; 8] {
    let t = terms.min(8).div_ceil(2);
    let e = [src[0], src[2], src[4], src[6]];
    butterfly_x4(&ODD8, idct4_x4(&e, t), src, terms)
}

#[inline]
fn idct16_x4(src: &[V4], terms: usize) -> [V4; 16] {
    let t = terms.min(16).div_ceil(2);
    let mut e = [[0i32; 4]; 8];
    for (dst, s) in e[..t].iter_mut().zip(src.chunks_exact(2)) {
        *dst = s[0];
    }
    butterfly_x4(&ODD16, idct8_x4(&e, t), src, terms)
}

#[inline]
fn idct32_x4(src: &[V4], terms: usize) -> [V4; 32] {
    let t = terms.min(32).div_ceil(2);
    let mut e = [[0i32; 4]; 16];
    for (dst, s) in e[..t].iter_mut().zip(src.chunks_exact(2)) {
        *dst = s[0];
    }
    butterfly_x4(&ODD32, idct16_x4(&e, t), src, terms)
}

trait Idct4Col<const N: usize> {
    fn idct_x4(src: &[V4], terms: usize) -> [V4; N];
}

struct IdctDispatch;

impl Idct4Col<8> for IdctDispatch {
    #[inline(always)]
    fn idct_x4(src: &[V4], terms: usize) -> [V4; 8] {
        idct8_x4(src, terms)
    }
}

impl Idct4Col<16> for IdctDispatch {
    #[inline(always)]
    fn idct_x4(src: &[V4], terms: usize) -> [V4; 16] {
        idct16_x4(src, terms)
    }
}

impl Idct4Col<32> for IdctDispatch {
    #[inline(always)]
    fn idct_x4(src: &[V4], terms: usize) -> [V4; 32] {
        idct32_x4(src, terms)
    }
}

#[inline]
fn load_col_tile<const N: usize>(b: &[i32], x: usize, rows: usize, cols: usize, out: &mut [V4; N]) {
    let rem = cols - x;
    if rem >= 4 {
        for j in 0..rows {
            let row = &b[j * N + x..j * N + x + 4];
            out[j] = [row[0], row[1], row[2], row[3]];
        }
    } else {
        for j in 0..rows {
            let row = &b[j * N + x..j * N + x + rem];
            let mut v = [0i32; 4];
            v[..rem].copy_from_slice(row);
            out[j] = v;
        }
    }
}

#[inline]
fn inverse_2d_dct_x4<const N: usize>(
    b: &[i32],
    g: &mut [i32],
    rows: usize,
    cols: usize,
    bd_shift: i32,
    out: &mut [i32],
) where
    IdctDispatch: Idct4Col<N>,
{
    let rnd = 1 << (bd_shift - 1);
    // Column pass: 4 columns at a time, storing transposed 4×4 blocks so row strips are contiguous.
    let mut col_vecs = [[0i32; 4]; N];
    for x in (0..cols).step_by(4) {
        load_col_tile::<N>(b, x, rows, cols, &mut col_vecs);
        let e = IdctDispatch::idct_x4(&col_vecs, rows);
        for y0 in (0..N).step_by(4) {
            let tile = transpose4x4([
                vclip16(e[y0]),
                vclip16(e[y0 + 1]),
                vclip16(e[y0 + 2]),
                vclip16(e[y0 + 3]),
            ]);
            for (r, tv) in tile.iter().enumerate() {
                g[(x + r) * N + y0..(x + r) * N + y0 + 4].copy_from_slice(tv);
            }
        }
    }
    // Row pass: 4 rows at a time, writing transposed 4×4 blocks to `out`.
    let mut row_vecs = [[0i32; 4]; N];
    for y0 in (0..N).step_by(4) {
        for k in 0..cols {
            let s = &g[k * N + y0..k * N + y0 + 4];
            row_vecs[k] = [s[0], s[1], s[2], s[3]];
        }
        let r = IdctDispatch::idct_x4(&row_vecs, cols);
        for x0 in (0..N).step_by(4) {
            let tile = transpose4x4([
                vshift(r[x0], rnd, bd_shift),
                vshift(r[x0 + 1], rnd, bd_shift),
                vshift(r[x0 + 2], rnd, bd_shift),
                vshift(r[x0 + 3], rnd, bd_shift),
            ]);
            out[y0 * N + x0..y0 * N + x0 + 4].copy_from_slice(&tile[0]);
            out[(y0 + 1) * N + x0..(y0 + 1) * N + x0 + 4].copy_from_slice(&tile[1]);
            out[(y0 + 2) * N + x0..(y0 + 2) * N + x0 + 4].copy_from_slice(&tile[2]);
            out[(y0 + 3) * N + x0..(y0 + 3) * N + x0 + 4].copy_from_slice(&tile[3]);
        }
    }
}

#[inline]
fn inverse_1d_scalar<const N: usize, const FLAT_ROW: bool>(
    src: &[i32; N],
    terms: usize,
) -> [i32; N] {
    if FLAT_ROW {
        idct_1d::<N>(src, terms)
    } else {
        let mut out = [0i32; N];
        out[..4].copy_from_slice(&idst4(src[..4].try_into().expect("N = 4"), terms));
        out
    }
}

fn inverse_2d_scalar<const N: usize, const FLAT_ROW: bool>(
    b: &[i32],
    g: &mut [i32],
    rows: usize,
    cols: usize,
    bd_shift: i32,
    out: &mut [i32],
) {
    let rnd = 1 << (bd_shift - 1);
    let mut col = [0i32; N];
    for x in 0..cols {
        for j in 0..rows {
            col[j] = b[j * N + x];
        }
        let e = inverse_1d_scalar::<N, FLAT_ROW>(&col, rows);
        for y in 0..N {
            g[y * N + x] = ((e[y] + 64) >> 7).clamp(-32768, 32767);
        }
    }
    for (grow, o) in g.chunks_exact(N).zip(out.chunks_exact_mut(N)) {
        let r = inverse_1d_scalar::<N, FLAT_ROW>(grow.try_into().expect("N elements"), cols);
        for (dst, &val) in o.iter_mut().zip(&r) {
            *dst = (val + rnd) >> bd_shift;
        }
    }
}

/// 2-D inverse transform (§8.6.4.2) of an `N`×`N` block (`N` in `{4, 8, 16, 32}`) whose
/// non-zero scaled coefficients lie in `rows`×`cols`, dispatching sparse/4×4 blocks to scalar
/// kernels and dense `N >= 8` blocks (`cols >= 3`) to 4-column vectorized kernels.
fn inverse_2d<const N: usize, const FLAT_ROW: bool>(
    _m: &[[i32; N]; N],
    d: &[i32],
    g: &mut [i32],
    rows: usize,
    cols: usize,
    bd_shift: i32,
    out: &mut [i32],
) {
    let (rows, cols) = (rows.min(N), cols.min(N));
    let b = &d[..N * N];
    let g = &mut g[..N * N];
    let out = &mut out[..N * N];
    let rnd = 1 << (bd_shift - 1);
    if FLAT_ROW && rows == 1 {
        let mut row_in = [0i32; N];
        for (dst, &c) in row_in.iter_mut().zip(b).take(cols) {
            *dst = ((64 * c + 64) >> 7).clamp(-32768, 32767);
        }
        let mut row = idct_1d::<N>(&row_in, cols);
        for v in row.iter_mut() {
            *v = (*v + rnd) >> bd_shift;
        }
        for o in out.chunks_exact_mut(N) {
            o.copy_from_slice(&row);
        }
        return;
    }
    if FLAT_ROW && cols == 1 {
        let mut col = [0i32; N];
        for j in 0..rows {
            col[j] = b[j * N];
        }
        let e = idct_1d::<N>(&col, rows);
        for (&ev, o) in e.iter().zip(out.chunks_exact_mut(N)) {
            let g0 = ((ev + 64) >> 7).clamp(-32768, 32767);
            o.fill((64 * g0 + rnd) >> bd_shift);
        }
        return;
    }
    if N == 4 || cols < 3 {
        inverse_2d_scalar::<N, FLAT_ROW>(b, g, rows, cols, bd_shift, out);
        return;
    }
    match N {
        8 => inverse_2d_dct_x4::<8>(b, g, rows, cols, bd_shift, out),
        16 => inverse_2d_dct_x4::<16>(b, g, rows, cols, bd_shift, out),
        _ => inverse_2d_dct_x4::<32>(b, g, rows, cols, bd_shift, out),
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

#[derive(Debug)]
enum SaoScratch {
    U8(Vec<u8>),
    U16(Vec<u16>),
}

/// Reconstructs a picture from the blocks emitted by the parser.
#[derive(Debug)]
pub struct Reconstructor<'a, F = Frame> {
    sps: &'a Sps,
    pps: &'a Pps,
    layout: CtbLayout,
    /// Whether the picture is a single tile: intra prediction then never crosses a tile.
    single_tile: bool,
    /// The picture being reconstructed (coded size, before cropping).
    pub frame: F,
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
    /// Pre-SAO plane copy buffer, reused across components and frames.
    sao_scratch: SaoScratch,
    /// Deferred error (the sink interface cannot return errors).
    pub error: Option<Error>,
}

/// Reusable backing storage for [`Reconstructor`], avoiding allocations across repeated decodes.
#[derive(Debug, Default)]
pub struct ReconstructorBuffers {
    pub(crate) frame: Frame,
    decoded: Vec<u32>,
    luma_residual: Vec<i32>,
    scratch: Vec<i32>,
    filter_info: FilterInfo,
    inter_scratch: Vec<i32>,
    pub(crate) sao_scratch: Vec<u16>,
}

/// The deblocking filter's state of an area, saved by [`Reconstructor::save_filter_state`].
#[derive(Debug, Clone, Default)]
pub struct FilterState {
    edges: Vec<u8>,
    intra: Vec<bool>,
    coded: Vec<bool>,
    motion: Vec<EdgeMotion>,
}

fn init_reconstructor<'a, F: ReconFrame>(
    sps: &'a Sps,
    pps: &'a Pps,
    mut frame: F,
    sao_scratch: SaoScratch,
    mut buf: ReconstructorBuffers,
) -> Reconstructor<'a, F> {
    let w4 = sps.pic_width_in_luma_samples.div_ceil(4) as usize;
    let h4 = sps.pic_height_in_luma_samples.div_ceil(4) as usize;
    let layout = CtbLayout::new(sps, pps);
    frame.reset(sps);
    buf.filter_info.reset(sps, &layout);
    buf.decoded.clear();
    buf.decoded.resize(w4 * h4, u32::MAX);
    buf.luma_residual.clear();
    buf.luma_residual.resize(32 * 32, 0);
    buf.scratch.clear();
    buf.scratch.resize(4 * 32 * 32 + 2 * (4 * 32 + 1), 0);
    buf.inter_scratch.clear();
    Reconstructor {
        sps,
        pps,
        filter_info: buf.filter_info,
        single_tile: layout.col_bd.len() == 2 && layout.row_bd.len() == 2,
        layout,
        frame,
        decoded: buf.decoded,
        w4,
        slice_addr: 0,
        slice_cb_qp_offset: 0,
        slice_cr_qp_offset: 0,
        luma_residual: buf.luma_residual,
        scratch: buf.scratch,
        refs: [Vec::new(), Vec::new()],
        weights: [None, None],
        inter_scratch: buf.inter_scratch,
        sao_scratch,
        error: None,
    }
}

impl<'a> Reconstructor<'a, Frame> {
    /// Creates a reconstructor for a picture.
    pub fn new(sps: &'a Sps, pps: &'a Pps) -> Self {
        Self::from_buffers(sps, pps, ReconstructorBuffers::default())
    }

    /// Creates a reconstructor for a picture, reusing heap allocations from `buf`.
    pub fn from_buffers(sps: &'a Sps, pps: &'a Pps, mut buf: ReconstructorBuffers) -> Self {
        let frame = std::mem::take(&mut buf.frame);
        let mut sao_scratch = std::mem::take(&mut buf.sao_scratch);
        sao_scratch.clear();
        init_reconstructor(sps, pps, frame, SaoScratch::U16(sao_scratch), buf)
    }

    /// Like [`finish`](Self::finish), also returning the motion of every 4×4 luma block, for
    /// the pictures that use this one as collocated picture.
    pub fn finish_with_motion(self, skip_filters: bool, poc: i32) -> (Frame, inter::MotionField) {
        let w4 = self.filter_info.w4;
        let mut motion = self.filter_info.motion.clone();
        if motion.is_empty() {
            motion.resize(w4 * self.filter_info.h4, EdgeMotion::default());
        }
        (
            self.finish(skip_filters),
            inter::MotionField::from_blocks(w4, motion, poc),
        )
    }

    /// Applies the in-loop filters (unless `skip_filters`) and returns the decoded picture,
    /// uncropped (see [`Frame::cropped`]).
    pub fn finish(self, skip_filters: bool) -> Frame {
        self.finish_into_buffers(skip_filters).frame
    }

    /// Applies the in-loop filters (unless `skip_filters`) and returns all internal buffers
    /// (including `frame`) so callers can reuse their heap allocations across pictures.
    pub fn finish_into_buffers(mut self, skip_filters: bool) -> ReconstructorBuffers {
        let mut sao_scratch =
            match std::mem::replace(&mut self.sao_scratch, SaoScratch::U16(Vec::new())) {
                SaoScratch::U16(v) => v,
                SaoScratch::U8(_) => Vec::new(),
            };
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
                &mut sao_scratch,
            );
        }
        ReconstructorBuffers {
            frame: self.frame,
            decoded: self.decoded,
            luma_residual: self.luma_residual,
            scratch: self.scratch,
            filter_info: self.filter_info,
            inter_scratch: self.inter_scratch,
            sao_scratch,
        }
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
        let mut samples = [0i32; 4 * 32 + 1];
        self.reference_samples_into(c, x0, y0, n, &mut samples);
        IntraReferences { samples, c, n }
    }

    /// Intra prediction with `mode` from reference samples given by
    /// [`intra_references`](Self::intra_references), into `pred` (row-major).
    pub fn predict_from(&self, refs: &IntraReferences, mode: u8, pred: &mut [i32]) {
        let mut smooth = [0i32; 4 * 32 + 1];
        self.predict_with(
            refs.c,
            refs.n,
            mode,
            false,
            &refs.samples,
            &mut smooth,
            pred,
        );
    }

    /// Like [`Sink::transform_block`], with the intra prediction of the block already
    /// computed (by [`predict_block`](Self::predict_block) or
    /// [`predict_from`](Self::predict_from)): for encoders, which predict before coding.
    pub fn reconstruct(&mut self, tb: &TransformBlock<'_>, prediction: &[i32]) {
        if self.error.is_none() {
            self.transform_block_impl(tb, Some(prediction));
        }
    }
}

impl<'a> Reconstructor<'a, Frame8> {
    pub(crate) fn from_buffers_8(
        sps: &'a Sps,
        pps: &'a Pps,
        frame8: Frame8,
        mut sao_scratch8: Vec<u8>,
        buf: ReconstructorBuffers,
    ) -> Self {
        sao_scratch8.clear();
        init_reconstructor(sps, pps, frame8, SaoScratch::U8(sao_scratch8), buf)
    }

    pub(crate) fn finish_into_buffers_8(
        mut self,
        skip_filters: bool,
        orig_frame: Frame,
        orig_sao: Vec<u16>,
    ) -> (Frame8, Vec<u8>, ReconstructorBuffers) {
        let mut sao_scratch8 =
            match std::mem::replace(&mut self.sao_scratch, SaoScratch::U8(Vec::new())) {
                SaoScratch::U8(v) => v,
                SaoScratch::U16(_) => Vec::new(),
            };
        if !skip_filters {
            filters::deblock_8(
                &mut self.frame,
                &self.filter_info,
                self.sps,
                self.pps,
                &self.layout,
            );
            filters::sao_8(
                &mut self.frame,
                &self.filter_info,
                self.sps,
                self.pps,
                &self.layout,
                &mut sao_scratch8,
            );
        }
        let buf = ReconstructorBuffers {
            frame: orig_frame,
            decoded: self.decoded,
            luma_residual: self.luma_residual,
            scratch: self.scratch,
            filter_info: self.filter_info,
            inter_scratch: self.inter_scratch,
            sao_scratch: orig_sao,
        };
        (self.frame, sao_scratch8, buf)
    }
}

impl<'a, F> Reconstructor<'a, F> {
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
            let len = r.len();
            state.edges.extend_from_slice(&info.edges[r.clone()]);
            state.intra.extend_from_slice(&info.intra[r.clone()]);
            state.coded.extend_from_slice(&info.coded[r.clone()]);
            if info.motion.is_empty() {
                state
                    .motion
                    .resize(state.motion.len() + len, EdgeMotion::default());
            } else {
                state.motion.extend_from_slice(&info.motion[r]);
            }
        }
        state
    }

    /// Puts back a state saved by [`save_filter_state`](Self::save_filter_state) for the same
    /// area.
    pub fn restore_filter_state(&mut self, x: u32, y: u32, w: u32, h: u32, state: &FilterState) {
        let info = &mut self.filter_info;
        if info.motion.is_empty() {
            info.motion.resize(info.w4 * info.h4, EdgeMotion::default());
        }
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
}

// Only private methods live in this impl block.
#[allow(private_bounds)]
impl<'a, F: ReconFrame> Reconstructor<'a, F> {
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
        let mut refs = [0i32; 4 * 32 + 1];
        let mut smooth = [0i32; 4 * 32 + 1];
        self.reference_samples_into(c, x0, y0, n, &mut refs);
        self.predict_with(c, n, mode, transquant_bypass, &refs, &mut smooth, pred);
    }

    /// Reference samples of a block, substituted where unavailable (§8.4.4.2.2): index 0 =
    /// p[-1][2n-1] (bottom of the left column) ... index 2n = p[-1][-1] ... index 4n =
    /// p[2n-1][-1] (right end of the top row). Writes `out[..4 * n + 1]`.
    fn reference_samples_into(
        &self,
        c: usize,
        x0: u32,
        y0: u32,
        n: usize,
        out: &mut [i32; 4 * 32 + 1],
    ) {
        let (sw, sh) = if c == 0 {
            (1, 1)
        } else {
            self.sps.chroma_subsampling()
        };
        let bit_depth = self.frame.bit_depth()[usize::from(c > 0)];
        let plane = self.frame.plane(c);
        let stride = self.frame.widths()[c] as usize;
        let ph_c = self.frame.heights()[c] as usize;

        // Reference samples: index 0 = p[-1][2n-1] (bottom of the left column) ... index 2n =
        // p[-1][-1] ... index 4n = p[2n-1][-1] (right end of the top row), as in the
        // substitution order of §8.4.4.2.2.
        let total = 4 * n + 1;
        let refs = &mut out[..total];
        let mut avail_lo: u128 = 0;
        let mut avail_hi = false;
        // Availability is decided per 4×4 luma block, i.e. per run of `unit_x` (`unit_y`)
        // samples of this component along the top row (left column).
        let (unit_x, unit_y) = ((4 / sw).max(1) as usize, (4 / sh).max(1) as usize);
        let (x0u, y0u) = (x0 as usize, y0 as usize);
        if self.single_tile && x0u > 0 && y0u > 0 && x0u + 2 * n <= stride && y0u + 2 * n <= ph_c {
            let swu = sw as usize;
            let shu = sh as usize;
            let x4_left = ((x0u - 1) * swu) >> 2;
            let y4_above = ((y0u - 1) * shu) >> 2;
            let addr = self.slice_addr;
            let try_fast = |refs: &mut [i32]| -> bool {
                if self.decoded[y4_above * self.w4 + x4_left] != addr {
                    return false;
                }
                let tx0 = (x0u * swu) >> 2;
                let tx1 = ((x0u + n - 1) * swu) >> 2;
                let top_row_decoded = &self.decoded[y4_above * self.w4..];
                if top_row_decoded[tx0..=tx1].iter().any(|&v| v != addr) {
                    return false;
                }
                let ty0 = (y0u * shu) >> 2;
                let ty1 = ((y0u + n - 1) * shu) >> 2;
                for y4 in ty0..=ty1 {
                    if self.decoded[y4 * self.w4 + x4_left] != addr {
                        return false;
                    }
                }
                let tx2 = ((x0u + n) * swu) >> 2;
                let tx3 = ((x0u + 2 * n - 1) * swu) >> 2;
                let mut tr_all = true;
                let mut tr_none = true;
                for &v in &top_row_decoded[tx2..=tx3] {
                    let is = v == addr;
                    tr_all &= is;
                    tr_none &= !is;
                }
                if !tr_all && !tr_none {
                    return false;
                }
                let ty2 = ((y0u + n) * shu) >> 2;
                let ty3 = ((y0u + 2 * n - 1) * shu) >> 2;
                let mut bl_all = true;
                let mut bl_none = true;
                for y4 in ty2..=ty3 {
                    let is = self.decoded[y4 * self.w4 + x4_left] == addr;
                    bl_all &= is;
                    bl_none &= !is;
                }
                if !bl_all && !bl_none {
                    return false;
                }
                for j in 0..n {
                    refs[2 * n - 1 - j] = plane[(y0u + j) * stride + x0u - 1].to_i32();
                }
                if bl_all {
                    for j in n..2 * n {
                        refs[2 * n - 1 - j] = plane[(y0u + j) * stride + x0u - 1].to_i32();
                    }
                } else {
                    let fill_bl = refs[n];
                    refs[..n].fill(fill_bl);
                }
                refs[2 * n] = plane[(y0u - 1) * stride + x0u - 1].to_i32();
                let row_above = (y0u - 1) * stride + x0u;
                for (dst, &v) in refs[2 * n + 1..3 * n + 1]
                    .iter_mut()
                    .zip(&plane[row_above..row_above + n])
                {
                    *dst = v.to_i32();
                }
                if tr_all {
                    for (dst, &v) in refs[3 * n + 1..4 * n + 1]
                        .iter_mut()
                        .zip(&plane[row_above + n..row_above + 2 * n])
                    {
                        *dst = v.to_i32();
                    }
                } else {
                    let fill_tr = refs[3 * n];
                    refs[3 * n + 1..4 * n + 1].fill(fill_tr);
                }
                true
            };
            if try_fast(refs) {
                return;
            }
            let mut all_avail = true;
            let mut k = 0;
            while k < 2 * n {
                let y = y0u + k;
                let len = (unit_y - y % unit_y).min(2 * n - k);
                if self.decoded[((y * sh as usize) >> 2) * self.w4 + x4_left] == self.slice_addr {
                    for j in 0..len {
                        refs[2 * n - 1 - k - j] = plane[(y + j) * stride + x0u - 1].to_i32();
                    }
                    avail_lo |= ((1u128 << len) - 1) << (2 * n - k - len);
                } else {
                    all_avail = false;
                }
                k += len;
            }
            if self.decoded[y4_above * self.w4 + x4_left] == self.slice_addr {
                refs[2 * n] = plane[(y0u - 1) * stride + x0u - 1].to_i32();
                avail_lo |= 1u128 << (2 * n);
            } else {
                all_avail = false;
            }
            let mut k = 0;
            while k < 2 * n {
                let x = x0u + k;
                let len = (unit_x - x % unit_x).min(2 * n - k);
                if self.decoded[y4_above * self.w4 + ((x * sw as usize) >> 2)] == self.slice_addr {
                    let row = (y0u - 1) * stride + x;
                    let start = 2 * n + 1 + k;
                    for (r, &v) in refs[start..start + len]
                        .iter_mut()
                        .zip(&plane[row..row + len])
                    {
                        *r = v.to_i32();
                    }
                    let end = start + len;
                    if end == 129 {
                        avail_hi = true;
                    }
                    let lo_len = end.min(128) - start;
                    if lo_len > 0 {
                        avail_lo |= ((1u128 << lo_len) - 1) << start;
                    }
                } else {
                    all_avail = false;
                }
                k += len;
            }
            if all_avail {
                return;
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
                        refs[2 * n - 1 - k - j] = plane[(y + j) * stride + x0u - 1].to_i32();
                    }
                    avail_lo |= ((1u128 << len) - 1) << (2 * n - k - len);
                }
                k += len;
            }
            // Corner.
            if available(i64::from(x0) - 1, i64::from(y0) - 1) {
                refs[2 * n] = plane[(y0u - 1) * stride + x0u - 1].to_i32();
                avail_lo |= 1u128 << (2 * n);
            }
            // Top row, columns x0..x0+2n, contiguous in the plane.
            let mut k = 0;
            while k < 2 * n {
                let x = x0u + k;
                let len = (unit_x - x % unit_x).min(2 * n - k);
                if available(x as i64, i64::from(y0) - 1) {
                    let row = (y0u - 1) * stride + x;
                    let start = 2 * n + 1 + k;
                    for (r, &v) in refs[start..start + len]
                        .iter_mut()
                        .zip(&plane[row..row + len])
                    {
                        *r = v.to_i32();
                    }
                    let end = start + len;
                    if end == 129 {
                        avail_hi = true;
                    }
                    let lo_len = end.min(128) - start;
                    if lo_len > 0 {
                        avail_lo |= ((1u128 << lo_len) - 1) << start;
                    }
                }
                k += len;
            }
        }
        if avail_lo == 0 && !avail_hi {
            refs.fill(1 << (bit_depth - 1));
        } else {
            if (avail_lo & 1) == 0 {
                let first = if avail_lo != 0 {
                    avail_lo.trailing_zeros() as usize
                } else {
                    128
                };
                refs[0] = refs[first];
            }
            for i in 1..total {
                let set = if i < 128 {
                    (avail_lo >> i) & 1 != 0
                } else {
                    avail_hi
                };
                if !set {
                    refs[i] = refs[i - 1];
                }
            }
        }
    }

    /// Intra prediction from reference samples (§8.4.4.2.3 to §8.4.4.2.6).
    #[allow(clippy::too_many_arguments)]
    fn predict_with(
        &self,
        c: usize,
        n: usize,
        mode: u8,
        transquant_bypass: bool,
        all_refs: &[i32; 4 * 32 + 1],
        smoothed: &mut [i32; 4 * 32 + 1],
        pred: &mut [i32],
    ) {
        // disableIntraBoundaryFilter (range extension).
        let boundary_filters = c == 0
            && n < 32
            && !(self.sps.range_extension.implicit_rdpcm_enabled_flag && transquant_bypass);
        let bit_depth = self.frame.bit_depth()[usize::from(c > 0)];
        let total = 4 * n + 1;
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
        let bit_depth = i32::from(self.frame.bit_depth()[usize::from(c > 0)]);

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
                let (pred_buf, rest) = scratch.split_at_mut(32 * 32);
                let (_, rest) = rest.split_at_mut(3 * 32 * 32);
                let (refs_buf, smooth_buf) = rest.split_at_mut(4 * 32 + 1);
                let pred = &mut pred_buf[..n * n];
                match prediction {
                    Some(p) => pred.copy_from_slice(&p[..n * n]),
                    None => {
                        let refs_arr: &mut [i32; 4 * 32 + 1] =
                            (&mut refs_buf[..4 * 32 + 1]).try_into().unwrap();
                        let smooth_arr: &mut [i32; 4 * 32 + 1] =
                            (&mut smooth_buf[..4 * 32 + 1]).try_into().unwrap();
                        self.reference_samples_into(c, tb.x, tb.y, n, refs_arr);
                        self.predict_with(
                            c,
                            n,
                            tb.intra_mode,
                            tb.transquant_bypass,
                            refs_arr,
                            smooth_arr,
                            pred,
                        );
                    }
                }
                let max = (1i32 << self.frame.bit_depth()[usize::from(c > 0)]) - 1;
                let stride = self.frame.widths()[c] as usize;
                let plane = self.frame.plane_mut(c);
                for (y, p) in pred.chunks_exact(n).enumerate() {
                    let row = (tb.y as usize + y) * stride + tb.x as usize;
                    for (o, &p) in plane[row..row + n].iter_mut().zip(p) {
                        *o = F::Sample::from_clamped(p, max);
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
        let (d, rest) = rest.split_at_mut(32 * 32);
        let (g_buf, rest) = rest.split_at_mut(32 * 32);
        let (refs_buf, smooth_buf) = rest.split_at_mut(4 * 32 + 1);
        let (pred, residual) = (&mut pred[..n * n], &mut residual[..n * n]);
        match prediction {
            Some(p) => pred.copy_from_slice(&p[..n * n]),
            None if tb.inter => {
                // Motion compensation already wrote the prediction into the picture.
                let stride = self.frame.widths()[c] as usize;
                let plane = self.frame.plane(c);
                for (y, row) in pred.chunks_exact_mut(n).enumerate() {
                    let start = (tb.y as usize + y) * stride + tb.x as usize;
                    for (o, &v) in row.iter_mut().zip(&plane[start..start + n]) {
                        *o = v.to_i32();
                    }
                }
            }
            None => {
                let refs_arr: &mut [i32; 4 * 32 + 1] =
                    (&mut refs_buf[..4 * 32 + 1]).try_into().unwrap();
                let smooth_arr: &mut [i32; 4 * 32 + 1] =
                    (&mut smooth_buf[..4 * 32 + 1]).try_into().unwrap();
                self.reference_samples_into(c, tb.x, tb.y, n, refs_arr);
                self.predict_with(
                    c,
                    n,
                    tb.intra_mode,
                    tb.transquant_bypass,
                    refs_arr,
                    smooth_arr,
                    pred,
                );
            }
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
                i32::from(self.frame.bit_depth()[0]),
                i32::from(self.frame.bit_depth()[1]),
            );
            for (r, &ry) in residual.iter_mut().zip(&self.luma_residual) {
                *r += (i32::from(tb.res_scale_val) * ((ry << bd_c) >> bd_y)) >> 3;
            }
        }

        let max = (1i32 << self.frame.bit_depth()[usize::from(c > 0)]) - 1;
        let stride = self.frame.widths()[c] as usize;
        let plane = self.frame.plane_mut(c);
        for (y, (p, r)) in pred
            .chunks_exact(n)
            .zip(residual.chunks_exact(n))
            .enumerate()
        {
            let row = (tb.y as usize + y) * stride + tb.x as usize;
            for ((o, &p), &r) in plane[row..row + n].iter_mut().zip(p).zip(r) {
                *o = F::Sample::from_clamped(p + r, max);
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
            if reference.widths != *self.frame.widths()
                || reference.heights != *self.frame.heights()
            {
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
            let bit_depth = self.frame.bit_depth()[usize::from(c > 0)];
            let denom_c = usize::from(c > 0);
            let stride = self.frame.widths()[c] as usize;
            let plane = self.frame.plane_mut(c);
            for yy in 0..h as usize {
                let start = (y as usize + yy) * stride + x as usize;
                let dst = &mut plane[start..start + w as usize];
                let row = yy * w as usize..(yy + 1) * w as usize;
                let (r0, r1) = (&out0[row.clone()], &out1[row]);
                match (used[0], used[1]) {
                    (Some((_, _, w0)), Some((_, _, w1))) => match (w0, w1) {
                        (Some(w0), Some(w1)) => {
                            for (o, (&p0, &p1)) in dst.iter_mut().zip(r0.iter().zip(r1)) {
                                *o = F::Sample::from_u16_sample(inter::weight_explicit_bi(
                                    [p0, p1],
                                    bit_depth,
                                    [w0.weight[c], w1.weight[c]],
                                    [w0.offset[c], w1.offset[c]],
                                    w0.log2_denom[denom_c],
                                ));
                            }
                        }
                        _ => {
                            for (o, (&p0, &p1)) in dst.iter_mut().zip(r0.iter().zip(r1)) {
                                *o =
                                    F::Sample::from_u16_sample(inter::weight_bi(p0, p1, bit_depth));
                            }
                        }
                    },
                    (Some((_, _, wt)), None) | (None, Some((_, _, wt))) => {
                        let r = if used[0].is_some() { r0 } else { r1 };
                        match wt {
                            None => {
                                for (o, &p) in dst.iter_mut().zip(r) {
                                    *o =
                                        F::Sample::from_u16_sample(inter::weight_uni(p, bit_depth));
                                }
                            }
                            Some(wt) => {
                                for (o, &p) in dst.iter_mut().zip(r) {
                                    *o = F::Sample::from_u16_sample(inter::weight_explicit(
                                        p,
                                        bit_depth,
                                        wt.weight[c],
                                        wt.offset[c],
                                        wt.log2_denom[denom_c],
                                    ));
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
        if info.motion.is_empty() {
            info.motion.resize(info.w4 * info.h4, EdgeMotion::default());
        }
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

    fn sink_transform_block(&mut self, tb: &TransformBlock<'_>) {
        if self.error.is_none() {
            self.transform_block_impl(tb, None);
        }
    }

    fn sink_pcm(&mut self, cu_x: u32, cu_y: u32, log2_size: u8, samples: [&[u16]; 3]) {
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
                    self.frame.widths()[0],
                )
            } else {
                (
                    self.sps.bit_depth_chroma - pcm.bit_depth_chroma,
                    self.frame.widths()[c],
                )
            };
            let plane = self.frame.plane_mut(c);
            for y in 0..h {
                for x in 0..w {
                    plane[((y0 + y) * stride + x0 + x) as usize] =
                        F::Sample::from_u16_sample(s[(y * w + x) as usize] << shift);
                }
            }
        }
        self.mark_decoded(cu_x, cu_y, size, size);
    }

    fn sink_coding_unit(&mut self, cu: &CodingUnit) {
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

    fn sink_prediction_unit(&mut self, pu: &PredictionUnit) {
        if self.error.is_none()
            && let Err(e) = self.predict_inter(pu)
        {
            self.error = Some(e);
        }
    }

    fn sink_sao(&mut self, ctb_x: u32, ctb_y: u32, params: &SaoParams) {
        self.filter_info.sao[(ctb_y * self.layout.width + ctb_x) as usize] = *params;
    }
}

impl Sink for Reconstructor<'_, Frame> {
    fn transform_block(&mut self, tb: &TransformBlock<'_>) {
        self.sink_transform_block(tb);
    }

    fn pcm(&mut self, cu_x: u32, cu_y: u32, log2_size: u8, samples: [&[u16]; 3]) {
        self.sink_pcm(cu_x, cu_y, log2_size, samples);
    }

    fn coding_unit(&mut self, cu: &CodingUnit) {
        self.sink_coding_unit(cu);
    }

    fn prediction_unit(&mut self, pu: &PredictionUnit) {
        self.sink_prediction_unit(pu);
    }

    fn sao(&mut self, ctb_x: u32, ctb_y: u32, params: &SaoParams) {
        self.sink_sao(ctb_x, ctb_y, params);
    }
}

impl Sink for Reconstructor<'_, Frame8> {
    fn transform_block(&mut self, tb: &TransformBlock<'_>) {
        self.sink_transform_block(tb);
    }

    fn pcm(&mut self, cu_x: u32, cu_y: u32, log2_size: u8, samples: [&[u16]; 3]) {
        self.sink_pcm(cu_x, cu_y, log2_size, samples);
    }

    fn coding_unit(&mut self, cu: &CodingUnit) {
        self.sink_coding_unit(cu);
    }

    fn prediction_unit(&mut self, pu: &PredictionUnit) {
        self.sink_prediction_unit(pu);
    }

    fn sao(&mut self, ctb_x: u32, ctb_y: u32, params: &SaoParams) {
        self.sink_sao(ctb_x, ctb_y, params);
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

    #[test]
    fn vectorized_inverse_2d_matches_scalar_reference() {
        fn ref_2d<const N: usize>(
            m: &[[i32; N]; N],
            d: &[i32],
            rows: usize,
            cols: usize,
            bd_shift: i32,
        ) -> Vec<i32> {
            let mut g = vec![0i32; N * N];
            let mut col = [0i32; N];
            for x in 0..cols {
                for j in 0..rows {
                    col[j] = d[j * N + x];
                }
                let e = mul_1d::<N>(m, &col, rows);
                for y in 0..N {
                    g[y * N + x] = ((e[y] + 64) >> 7).clamp(-32768, 32767);
                }
            }
            let rnd = 1 << (bd_shift - 1);
            let mut out = vec![0i32; N * N];
            for (grow, o) in g.chunks_exact(N).zip(out.chunks_exact_mut(N)) {
                let r = mul_1d::<N>(m, grow.try_into().unwrap(), cols);
                for (dst, &val) in o.iter_mut().zip(&r) {
                    *dst = (val + rnd) >> bd_shift;
                }
            }
            out
        }
        fn check<const N: usize, const FLAT: bool>(m: &[[i32; N]; N], shapes: &[(usize, usize)]) {
            let mut seed = 0x243f_6a88_u32;
            let mut next = || {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed
            };
            let mut g = vec![0x7777_7777_i32; N * N];
            let mut out = vec![0i32; N * N];
            let d: Vec<i32> = (0..N * N).map(|_| i32::from(next() as i16)).collect();
            for &(rows, cols) in shapes {
                inverse_2d::<N, FLAT>(m, &d, &mut g, rows, cols, 12, &mut out);
                let expected = ref_2d::<N>(m, &d, rows, cols, 12);
                assert_eq!(out, expected, "N={N} FLAT={FLAT} rows={rows} cols={cols}");
            }
        }
        check::<4, false>(&DST4, &[(1, 1), (1, 3), (3, 1), (2, 4), (4, 3), (4, 4)]);
        check::<4, true>(&DCT4, &[(1, 1), (1, 4), (4, 1), (2, 3), (3, 4), (4, 4)]);
        check::<8, true>(&DCT8, &[(1, 1), (1, 7), (7, 1), (3, 5), (4, 8), (8, 8)]);
        check::<16, true>(
            &DCT16,
            &[(1, 1), (1, 9), (9, 1), (5, 7), (12, 13), (16, 16)],
        );
        check::<32, true>(
            &DCT32,
            &[(1, 1), (1, 17), (17, 1), (5, 9), (16, 25), (32, 32)],
        );
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
