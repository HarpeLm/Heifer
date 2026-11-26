//! YCbCr to RGB conversion (ITU-T H.273 matrix coefficients).

use heifer_hevc_dec::decoder::{CropWindow, DecodedFrameView};
use heifer_hevc_dec::recon::Frame;

use crate::image::{Image, Sample};

/// The R, G and B contributions of chroma along a row.
#[derive(Debug, Default)]
struct ChromaTerms {
    r: Vec<i32>,
    g: Vec<i32>,
    b: Vec<i32>,
}

impl ChromaTerms {
    fn resize(&mut self, width: usize) {
        self.r.resize(width, 0);
        self.g.resize(width, 0);
        self.b.resize(width, 0);
    }

    /// Sets this row half way between `a` and `b`.
    fn average(&mut self, a: &Self, b: &Self) {
        for (out, a, b) in [
            (&mut self.r, &a.r, &b.r),
            (&mut self.g, &a.g, &b.g),
            (&mut self.b, &a.b, &b.b),
        ] {
            for ((o, &a), &b) in out.iter_mut().zip(a).zip(b) {
                *o = (a + b + 1) >> 1;
            }
        }
    }
}

/// Reusable per-thread row buffers for [`frame_to_rgba_into_with_scratch`].
#[derive(Debug, Default)]
pub(crate) struct ColorScratch {
    above: ChromaTerms,
    below: ChromaTerms,
    blended: ChromaTerms,
}

/// How to interpret YCbCr samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorParams {
    /// `matrix_coefficients` (H.273): 0 = identity (GBR), 1 = BT.709, 5/6 = BT.601, 9 = BT.2020.
    pub matrix: u16,
    /// Whether samples use the full range (otherwise "video" range, e.g. 16..235 for 8-bit).
    pub full_range: bool,
}

impl ColorParams {
    /// Colour description of an HEVC stream's VUI. When absent, HEVC specifies limited range,
    /// and the matrix is unspecified (treated as BT.601).
    pub fn from_frame(frame: &Frame) -> Self {
        Self {
            matrix: u16::from(frame.matrix_coeffs),
            full_range: frame.full_range,
        }
    }

    /// Colour description of a decoded frame view's VUI.
    pub(crate) fn from_view(view: &DecodedFrameView<'_>) -> Self {
        Self {
            matrix: u16::from(view.matrix_coeffs()),
            full_range: view.full_range(),
        }
    }
}

/// `(Kr, Kb)` for a matrix coefficients code point.
fn kr_kb(matrix: u16) -> (f64, f64) {
    match matrix {
        1 => (0.2126, 0.0722),
        4 => (0.30, 0.11),
        7 => (0.212, 0.087),
        9 | 10 => (0.2627, 0.0593),
        _ => (0.299, 0.114), // 5, 6 and unspecified: BT.601
    }
}

/// Converts a decoded frame to an RGBA image (alpha opaque). Subsampled chroma is upsampled
/// bilinearly, with the chroma samples on the even luma rows and columns, as Apple's decoder
/// does: decoded photos then match what Apple's devices show.
pub fn frame_to_rgba(frame: &Frame, params: ColorParams) -> Image {
    frame_to_rgba_as(frame, params)
}

/// Like [`frame_to_rgba`], for an image of `S` samples: `u8` halves the memory at 8 bits.
pub fn frame_to_rgba_as<S: Sample>(frame: &Frame, params: ColorParams) -> Image<S> {
    frame_to_rgba_sized(frame, params, frame.widths[0], frame.heights[0])
}

/// Like [`frame_to_rgba`], for the top-left `width`×`height` pixels of `frame` (clamped to its
/// size): what is coded past them is padding and leaves no trace in the image.
pub fn frame_to_rgba_sized<S: Sample>(
    frame: &Frame,
    params: ColorParams,
    width: u32,
    height: u32,
) -> Image<S> {
    let view = DecodedFrameView::U16(frame, frame.full_window());
    frame_view_to_rgba_sized(&view, params, width, height)
}

/// Like [`frame_to_rgba`], but writes the top-left `width`×`height` pixels of `frame` (clamped
/// to its size) into the rows `out` (4 samples per pixel, each row at least `width` pixels), so
/// that grid tiles can be converted in place in the final image.
pub fn frame_to_rgba_into<S: Sample>(
    frame: &Frame,
    params: ColorParams,
    out: &mut [&mut [S]],
    width: usize,
    height: usize,
) {
    let mut scratch = ColorScratch::default();
    frame_to_rgba_into_with_scratch(frame, params, out, width, height, &mut scratch);
}

/// Like [`frame_to_rgba_into`], reusing `scratch` row buffers across tiles of a grid.
pub(crate) fn frame_to_rgba_into_with_scratch<S: Sample>(
    frame: &Frame,
    params: ColorParams,
    out: &mut [&mut [S]],
    width: usize,
    height: usize,
    scratch: &mut ColorScratch,
) {
    let view = DecodedFrameView::U16(frame, frame.full_window());
    frame_view_to_rgba_into_with_scratch(&view, params, out, width, height, scratch);
}

pub(crate) fn frame_view_to_rgba_sized<S: Sample>(
    view: &DecodedFrameView<'_>,
    params: ColorParams,
    width: u32,
    height: u32,
) -> Image<S> {
    let (w, h) = (width.min(view.width()), height.min(view.height()));
    let mut data = vec![S::default(); w as usize * h as usize * 4];
    let stride = (w as usize * 4).max(1);
    let mut scratch = ColorScratch::default();
    view_to_rgba_rows(
        view,
        params,
        data.chunks_exact_mut(stride),
        w as usize,
        h as usize,
        &mut scratch,
    );
    let bd = view.bit_depth();
    Image {
        width: w,
        height: h,
        bit_depth: S::depth(bd[0].max(bd[1])),
        has_alpha: false,
        data,
    }
}

pub(crate) fn frame_view_to_rgba_into_with_scratch<S: Sample>(
    view: &DecodedFrameView<'_>,
    params: ColorParams,
    out: &mut [&mut [S]],
    width: usize,
    height: usize,
    scratch: &mut ColorScratch,
) {
    view_to_rgba_rows(
        view,
        params,
        out.iter_mut().map(|row| &mut **row),
        width,
        height,
        scratch,
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn frame_view_to_rgba_strided_with_scratch<S: Sample>(
    view: &DecodedFrameView<'_>,
    params: ColorParams,
    canvas: &mut [S],
    canvas_stride: usize,
    x0: usize,
    y0: usize,
    width: usize,
    height: usize,
    scratch: &mut ColorScratch,
) {
    if width == 0 || height == 0 || canvas_stride == 0 {
        return;
    }
    let rows = canvas
        .chunks_exact_mut(canvas_stride)
        .skip(y0)
        .take(height)
        .map(|row| &mut row[x0 * 4..]);
    view_to_rgba_rows(view, params, rows, width, height, scratch);
}

fn view_to_rgba_rows<'o, S: Sample + 'o>(
    view: &DecodedFrameView<'_>,
    params: ColorParams,
    out_rows: impl Iterator<Item = &'o mut [S]>,
    width: usize,
    height: usize,
    scratch: &mut ColorScratch,
) {
    match *view {
        DecodedFrameView::U8(f, crop) => planes_to_rgba_into_with_scratch(
            [&f.planes[0], &f.planes[1], &f.planes[2]],
            f.widths.map(|v| v as usize),
            crop,
            f.bit_depth,
            params,
            out_rows,
            width,
            height,
            scratch,
        ),
        DecodedFrameView::U16(f, crop) => planes_to_rgba_into_with_scratch(
            [&f.planes[0], &f.planes[1], &f.planes[2]],
            f.widths.map(|v| v as usize),
            crop,
            f.bit_depth,
            params,
            out_rows,
            width,
            height,
            scratch,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn planes_to_rgba_into_with_scratch<'o, P: Copy + Into<i32>, S: Sample + 'o>(
    planes: [&[P]; 3],
    strides: [usize; 3],
    crop: CropWindow,
    bit_depth: [u8; 2],
    params: ColorParams,
    out_rows: impl Iterator<Item = &'o mut [S]>,
    width: usize,
    height: usize,
    scratch: &mut ColorScratch,
) {
    let (w, h) = (crop.widths[0] as usize, crop.heights[0] as usize);
    let (width, height) = (width.min(w), height.min(h));
    if width == 0 || height == 0 {
        return;
    }
    let bd_y = bit_depth[0];
    let bd_c = bit_depth[1];
    let out_depth = S::depth(bd_y.max(bd_c));
    let max_out = f64::from((1u32 << out_depth) - 1);
    let alpha = S::from_u16(((1u32 << out_depth) - 1) as u16);
    let rows = out_rows.take(height).enumerate();
    let (lx0, ly0) = (crop.x[0] as usize, crop.y[0] as usize);
    let luma_stride = strides[0];
    let luma_row = |y: usize| {
        let start = (ly0 + y) * luma_stride + lx0;
        &planes[0][start..start + width]
    };

    if crop.widths[1] == 0 || crop.heights[1] == 0 {
        // Monochrome.
        let scale = max_out / f64::from((1u32 << bd_y) - 1);
        for (y, out_row) in rows {
            for (px, &v) in out_row[..width * 4].chunks_exact_mut(4).zip(luma_row(y)) {
                let g = S::from_u16((f64::from(v.into()) * scale).round() as u16);
                px.copy_from_slice(&[g, g, g, alpha]);
            }
        }
        return;
    }

    let (cw, chh) = (crop.widths[1], crop.heights[1]);
    let (sx, sy) = (crop.widths[0] / cw.max(1), crop.heights[0] / chh.max(1));
    let (kr, kb) = kr_kb(params.matrix);
    let kg = 1.0 - kr - kb;

    // Normalize luma to 0..1 and chroma to -0.5..0.5.
    let (y_off, y_scale, c_scale) = {
        let (fy, fc) = (f64::from(1u32 << (bd_y - 8)), f64::from(1u32 << (bd_c - 8)));
        if params.full_range {
            (
                0.0,
                f64::from((1u32 << bd_y) - 1),
                f64::from((1u32 << bd_c) - 1),
            )
        } else {
            (16.0 * fy, 219.0 * fy, 224.0 * fc)
        }
    };
    let c_mid = f64::from(1u32 << (bd_c - 1));
    let cwu = cw as usize;
    let (cb_x0, cb_y0) = (crop.x[1] as usize, crop.y[1] as usize);
    let (cr_x0, cr_y0) = (crop.x[2] as usize, crop.y[2] as usize);
    let (cb_stride, cr_stride) = (strides[1], strides[2]);

    if params.matrix != 0 {
        // Fixed point with FRAC fractional bits. Every step is a plain loop over a row, with the
        // chroma terms kept apart per channel: that is what the compiler vectorizes.
        const FRAC: u32 = 14;
        const HALF: i32 = 1 << (FRAC - 1);
        let scale = max_out * f64::from(1u32 << FRAC);
        let fixed = |v: f64| (v * scale).round() as i32;
        let y_base = y_off as i32;
        let y_mul = fixed(1.0 / y_scale);
        let y_bias = HALF - y_mul * y_base;
        let c_base = c_mid as i32;
        let r_mul = fixed(2.0 * (1.0 - kr) / c_scale);
        let b_mul = fixed(2.0 * (1.0 - kb) / c_scale);
        // g = (y - kr r - kb b) / kg = y - (kr (r - y) + kb (b - y)) / kg
        let gr_mul = fixed(kr * 2.0 * (1.0 - kr) / kg / c_scale);
        let gb_mul = fixed(kb * 2.0 * (1.0 - kb) / kg / c_scale);
        let max = (1i32 << out_depth) - 1;
        let to_out = |v: i32| S::from_u16((v >> FRAC).clamp(0, max) as u16);
        let last_row = (height.saturating_sub(1) / sy as usize).min(chh as usize - 1);
        let last_column = (width.saturating_sub(1) / sx as usize).min(cwu - 1);
        let chroma_cols = last_column + 1;
        let pair_count = last_column.min(width / 2);
        let done = pair_count * 2;
        // Chroma terms (R, G, B contributions) of one chroma row. The conversion is linear, so
        // interpolating the terms is interpolating the chroma samples.
        let row_terms = |yc: usize, t: &mut ChromaTerms| {
            let cb_start = (cb_y0 + yc) * cb_stride + cb_x0;
            let cr_start = (cr_y0 + yc) * cr_stride + cr_x0;
            let cb_row = &planes[1][cb_start..cb_start + chroma_cols];
            let cr_row = &planes[2][cr_start..cr_start + chroma_cols];
            for (r, &cr) in t.r.iter_mut().zip(cr_row) {
                *r = r_mul * (cr.into() - c_base);
            }
            for (b, &cb) in t.b.iter_mut().zip(cb_row) {
                *b = b_mul * (cb.into() - c_base);
            }
            for ((g, &cb), &cr) in t.g.iter_mut().zip(cb_row).zip(cr_row) {
                *g = gr_mul * (cr.into() - c_base) + gb_mul * (cb.into() - c_base);
            }
        };
        // Terms of the chroma rows above and below the current luma row.
        scratch.above.resize(chroma_cols);
        scratch.below.resize(chroma_cols);
        scratch.blended.resize(chroma_cols);
        let (mut above, mut below, blended) =
            (&mut scratch.above, &mut scratch.below, &mut scratch.blended);
        let mut loaded = (usize::MAX, usize::MAX);
        for (y, out_row) in rows {
            // Vertically: chroma rows sit on even luma rows (4:2:0); odd rows are in between.
            let j = y / sy as usize;
            let next = if sy == 2 && y % 2 == 1 {
                (j + 1).min(last_row)
            } else {
                j
            };
            if loaded.0 != j {
                if loaded.1 == j {
                    std::mem::swap(&mut above, &mut below);
                } else {
                    row_terms(j.min(chh as usize - 1), above);
                }
                loaded = (j, usize::MAX);
            }
            if next != j && loaded.1 != next {
                row_terms(next, below);
                loaded.1 = next;
            }
            let terms = if next == j {
                &*above
            } else {
                blended.average(above, below);
                &*blended
            };
            let tr_s = &terms.r;
            let tg_s = &terms.g;
            let tb_s = &terms.b;
            let luma = luma_row(y);
            if sx == 2 {
                let pairs = out_row[..done * 4]
                    .chunks_exact_mut(8)
                    .zip(luma[..done].chunks_exact(2))
                    .zip(tr_s[..=pair_count].windows(2))
                    .zip(tg_s[..=pair_count].windows(2))
                    .zip(tb_s[..=pair_count].windows(2));
                for ((((px, lv), tr), tg), tb) in pairs {
                    let yf0 = y_mul * lv[0].into() + y_bias;
                    let yf1 = y_mul * lv[1].into() + y_bias;
                    px[0] = to_out(yf0 + tr[0]);
                    px[1] = to_out(yf0 - tg[0]);
                    px[2] = to_out(yf0 + tb[0]);
                    px[3] = alpha;
                    px[4] = to_out(yf1 + ((tr[0] + tr[1] + 1) >> 1));
                    px[5] = to_out(yf1 - ((tg[0] + tg[1] + 1) >> 1));
                    px[6] = to_out(yf1 + ((tb[0] + tb[1] + 1) >> 1));
                    px[7] = alpha;
                }
                for (dx, (px, &yv)) in out_row[done * 4..width * 4]
                    .chunks_exact_mut(4)
                    .zip(&luma[done..])
                    .enumerate()
                {
                    let xc = ((done + dx) / 2).min(last_column);
                    let yf = y_mul * yv.into() + y_bias;
                    px[0] = to_out(yf + tr_s[xc]);
                    px[1] = to_out(yf - tg_s[xc]);
                    px[2] = to_out(yf + tb_s[xc]);
                    px[3] = alpha;
                }
            } else {
                let columns = out_row[..width * 4]
                    .chunks_exact_mut(4)
                    .zip(luma)
                    .zip(tr_s)
                    .zip(tg_s)
                    .zip(tb_s);
                for ((((px, &yv), &r), &g), &b) in columns {
                    let yf = y_mul * yv.into() + y_bias;
                    px[0] = to_out(yf + r);
                    px[1] = to_out(yf - g);
                    px[2] = to_out(yf + b);
                    px[3] = alpha;
                }
            }
        }
        return;
    }

    // Identity matrix: planes are G, B, R.
    let xcs: Vec<usize> = (0..width as u32)
        .map(|x| (x / sx).min(cw - 1) as usize)
        .collect();
    let chroma_rows = |y: usize| {
        let yc = (y as u32 / sy).min(chh - 1) as usize;
        let cb_start = (cb_y0 + yc) * cb_stride + cb_x0;
        let cr_start = (cr_y0 + yc) * cr_stride + cr_x0;
        (
            yc,
            &planes[1][cb_start..cb_start + cwu],
            &planes[2][cr_start..cr_start + cwu],
        )
    };
    let s = max_out / f64::from((1u32 << bd_y) - 1);
    let to_out = |v: f64| S::from_u16((v * s).round().clamp(0.0, max_out) as u16);
    for (y, out_row) in rows {
        let (_, cb_row, cr_row) = chroma_rows(y);
        let pixels = out_row[..width * 4]
            .chunks_exact_mut(4)
            .zip(luma_row(y))
            .zip(&xcs);
        for ((px, &g), &xc) in pixels {
            let (b, r) = (f64::from(cb_row[xc].into()), f64::from(cr_row[xc].into()));
            px.copy_from_slice(&[to_out(r), to_out(f64::from(g.into())), to_out(b), alpha]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(y: u16, cb: u16, cr: u16) -> Frame {
        Frame {
            widths: [2, 1, 1],
            heights: [2, 1, 1],
            planes: [vec![y; 4], vec![cb], vec![cr]],
            bit_depth: [8, 8],
            chroma_format: 1,
            full_range: false,
            matrix_coeffs: 2,
        }
    }

    #[test]
    fn chroma_past_the_picture_is_not_interpolated_into_it() {
        let padded = Frame {
            widths: [4, 2, 2],
            heights: [4, 2, 2],
            planes: [vec![128; 16], vec![128, 0, 0, 0], vec![128, 255, 255, 255]],
            bit_depth: [8, 8],
            chroma_format: 1,
            full_range: true,
            matrix_coeffs: 6,
        };
        let params = ColorParams::from_frame(&padded);
        let image: Image<u8> = frame_to_rgba_sized(&padded, params, 2, 2);
        assert_eq!(image.data, [128, 128, 128, 255].repeat(4));
    }

    #[test]
    fn grey_and_primaries() {
        let full = ColorParams {
            matrix: 6,
            full_range: true,
        };
        let img: Image = frame_to_rgba(&frame(128, 128, 128), full);
        assert_eq!(&img.data[..4], &[128, 128, 128, 255]);
        // Limited range black and white.
        let limited = ColorParams {
            matrix: 1,
            full_range: false,
        };
        assert_eq!(
            &frame_to_rgba(&frame(16, 128, 128), limited).data[..3],
            &[0, 0, 0]
        );
        assert_eq!(
            &frame_to_rgba(&frame(235, 128, 128), limited).data[..3],
            &[255, 255, 255]
        );
        // Pure red in BT.601 full range: Y=76, Cb=85, Cr=255.
        let red: Image = frame_to_rgba(&frame(76, 85, 255), full);
        assert!(
            red.data[0] >= 253 && red.data[1] <= 2 && red.data[2] <= 2,
            "{:?}",
            &red.data[..3]
        );
    }

    #[test]
    fn cropped_view_u8_and_u16_match_cropped_frame() {
        use heifer_hevc_dec::decoder::Frame8;

        fn make_frames(
            coded_w: u32,
            coded_h: u32,
            crop: CropWindow,
            chroma_format: u8,
            full_range: bool,
            matrix_coeffs: u8,
        ) -> (Frame, Frame8, Frame) {
            let (sw, sh) = match chroma_format {
                0 => (1, 1),
                1 => (2, 2),
                2 => (2, 1),
                _ => (1, 1),
            };
            let (cw, ch) = if chroma_format == 0 {
                (0, 0)
            } else {
                (coded_w / sw, coded_h / sh)
            };
            let widths = [coded_w, cw, cw];
            let heights = [coded_h, ch, ch];
            let fill = |c: usize, len: usize| -> Vec<u8> {
                (0..len)
                    .map(|i| ((i * 37 + c * 91 + 17) & 0xFF) as u8)
                    .collect()
            };
            let planes8 = [
                fill(0, (widths[0] * heights[0]) as usize),
                fill(1, (widths[1] * heights[1]) as usize),
                fill(2, (widths[2] * heights[2]) as usize),
            ];
            let frame8 = Frame8 {
                widths,
                heights,
                planes: planes8.clone(),
                bit_depth: [8, 8],
                chroma_format,
                full_range,
                matrix_coeffs,
            };
            let frame16 = Frame {
                widths,
                heights,
                planes: planes8.map(|p| p.into_iter().map(u16::from).collect()),
                bit_depth: [8, 8],
                chroma_format,
                full_range,
                matrix_coeffs,
            };
            let cropped = frame8.to_frame_cropped(crop);
            (frame16, frame8, cropped)
        }

        let cases = [
            // 4:2:0 with non-zero crop window and odd clipped width/height.
            (
                16,
                16,
                CropWindow {
                    x: [4, 2, 2],
                    y: [2, 1, 1],
                    widths: [10, 5, 5],
                    heights: [12, 6, 6],
                },
                1u8,
                false,
                1u8,
                9u32,
                11u32,
            ),
            // 4:4:4 with crop window and full matrix 0 (identity GBR).
            (
                10,
                10,
                CropWindow {
                    x: [2, 2, 2],
                    y: [3, 3, 3],
                    widths: [7, 7, 7],
                    heights: [6, 6, 6],
                },
                3u8,
                true,
                0u8,
                6u32,
                5u32,
            ),
            // 4:4:4 with BT.709.
            (
                12,
                8,
                CropWindow {
                    x: [3, 3, 3],
                    y: [1, 1, 1],
                    widths: [8, 8, 8],
                    heights: [7, 7, 7],
                },
                3u8,
                true,
                1u8,
                7u32,
                7u32,
            ),
            // Monochrome with crop window.
            (
                12,
                10,
                CropWindow {
                    x: [4, 0, 0],
                    y: [2, 0, 0],
                    widths: [7, 0, 0],
                    heights: [7, 0, 0],
                },
                0u8,
                false,
                2u8,
                5u32,
                6u32,
            ),
        ];

        for (cw, ch, crop, fmt, full_range, matrix, out_w, out_h) in cases {
            let (f16, f8, cropped) = make_frames(cw, ch, crop, fmt, full_range, matrix);
            let params = ColorParams {
                matrix: u16::from(matrix),
                full_range,
            };

            let expected_u8: Image<u8> = frame_to_rgba_sized(&cropped, params, out_w, out_h);
            let v8 = DecodedFrameView::U8(&f8, crop);
            let v16 = DecodedFrameView::U16(&f16, crop);
            assert_eq!(
                frame_view_to_rgba_sized::<u8>(&v8, params, out_w, out_h),
                expected_u8
            );
            assert_eq!(
                frame_view_to_rgba_sized::<u8>(&v16, params, out_w, out_h),
                expected_u8
            );

            let expected_u16: Image<u16> = frame_to_rgba_sized(&cropped, params, out_w, out_h);
            assert_eq!(
                frame_view_to_rgba_sized::<u16>(&v8, params, out_w, out_h),
                expected_u16
            );
            assert_eq!(
                frame_view_to_rgba_sized::<u16>(&v16, params, out_w, out_h),
                expected_u16
            );

            // Strided offset into a larger canvas also matches bit-for-bit.
            let canvas_w = out_w as usize + 5;
            let canvas_h = out_h as usize + 4;
            let stride = canvas_w * 4;
            let mut canvas = vec![0u8; stride * canvas_h];
            let mut scratch = ColorScratch::default();
            frame_view_to_rgba_strided_with_scratch(
                &v8,
                params,
                &mut canvas,
                stride,
                3,
                2,
                out_w as usize,
                out_h as usize,
                &mut scratch,
            );
            for y in 0..out_h as usize {
                let actual_row =
                    &canvas[(2 + y) * stride + 3 * 4..(2 + y) * stride + (3 + out_w as usize) * 4];
                let expected_row =
                    &expected_u8.data[y * out_w as usize * 4..(y + 1) * out_w as usize * 4];
                assert_eq!(actual_row, expected_row);
            }
        }
    }
}
