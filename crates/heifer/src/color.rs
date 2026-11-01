//! YCbCr to RGB conversion (ITU-T H.273 matrix coefficients).

use heifer_hevc_dec::recon::Frame;

use crate::image::{Image, Sample};

/// The R, G and B contributions of chroma along a row.
struct ChromaTerms {
    r: Vec<i32>,
    g: Vec<i32>,
    b: Vec<i32>,
}

impl ChromaTerms {
    fn new(width: usize) -> Self {
        Self {
            r: vec![0; width],
            g: vec![0; width],
            b: vec![0; width],
        }
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

    /// Sets this row to `terms` at twice the width, stopping at column `last` of `terms`.
    fn widen(&mut self, terms: &Self, last: usize) {
        for (out, t) in [
            (&mut self.r, &terms.r),
            (&mut self.g, &terms.g),
            (&mut self.b, &terms.b),
        ] {
            widen_row(&t[..=last], out);
        }
    }
}

fn widen_row(terms: &[i32], out: &mut [i32]) {
    let pairs = out.chunks_exact_mut(2).zip(terms).zip(&terms[1..]);
    for ((pair, &a), &b) in pairs {
        pair[0] = a;
        pair[1] = (a + b + 1) >> 1;
    }
    let done = (terms.len() - 1).min(out.len() / 2) * 2;
    for (x, o) in out.iter_mut().enumerate().skip(done) {
        *o = terms[(x / 2).min(terms.len() - 1)];
    }
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
    let (w, h) = (width.min(frame.widths[0]), height.min(frame.heights[0]));
    let mut data = vec![S::default(); w as usize * h as usize * 4];
    let stride = (w as usize * 4).max(1);
    let mut rows: Vec<&mut [S]> = data.chunks_exact_mut(stride).collect();
    frame_to_rgba_into(frame, params, &mut rows, w as usize, h as usize);
    Image {
        width: w,
        height: h,
        bit_depth: S::depth(frame.bit_depth[0].max(frame.bit_depth[1])),
        has_alpha: false,
        data,
    }
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
    let (w, h) = (frame.widths[0] as usize, frame.heights[0] as usize);
    let (width, height) = (width.min(w), height.min(h));
    let bd_y = frame.bit_depth[0];
    let bd_c = frame.bit_depth[1];
    let out_depth = S::depth(bd_y.max(bd_c));
    let max_out = f64::from((1u32 << out_depth) - 1);
    let alpha = S::from_u16(((1u32 << out_depth) - 1) as u16);
    let rows = out.iter_mut().take(height).enumerate();
    let luma_row = |y: usize| &frame.planes[0][y * w..y * w + width];

    if frame.widths[1] == 0 {
        // Monochrome.
        let scale = max_out / f64::from((1u32 << bd_y) - 1);
        for (y, out_row) in rows {
            for (px, &v) in out_row[..width * 4].chunks_exact_mut(4).zip(luma_row(y)) {
                let g = S::from_u16((f64::from(v) * scale).round() as u16);
                px.copy_from_slice(&[g, g, g, alpha]);
            }
        }
        return;
    }

    let (cw, chh) = (frame.widths[1], frame.heights[1]);
    let (sx, sy) = (frame.widths[0] / cw.max(1), frame.heights[0] / chh.max(1));
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
    let xcs: Vec<usize> = (0..width as u32)
        .map(|x| (x / sx).min(cw - 1) as usize)
        .collect();
    let chroma_rows = |y: usize| {
        let yc = (y as u32 / sy).min(chh - 1) as usize;
        (
            yc,
            &frame.planes[1][yc * cwu..(yc + 1) * cwu],
            &frame.planes[2][yc * cwu..(yc + 1) * cwu],
        )
    };

    if params.matrix != 0 {
        // Fixed point with FRAC fractional bits. Every step is a plain loop over a row, with the
        // chroma terms kept apart per channel: that is what the compiler vectorizes.
        const FRAC: u32 = 14;
        let scale = max_out * f64::from(1u32 << FRAC);
        let fixed = |v: f64| (v * scale).round() as i32;
        let y_base = y_off as i32;
        let y_mul = fixed(1.0 / y_scale);
        let c_base = c_mid as i32;
        let r_mul = fixed(2.0 * (1.0 - kr) / c_scale);
        let b_mul = fixed(2.0 * (1.0 - kb) / c_scale);
        // g = (y - kr r - kb b) / kg = y - (kr (r - y) + kb (b - y)) / kg
        let gr_mul = fixed(kr * 2.0 * (1.0 - kr) / kg / c_scale);
        let gb_mul = fixed(kb * 2.0 * (1.0 - kb) / kg / c_scale);
        let max = (1i32 << out_depth) - 1;
        let to_out = |v: i32| S::from_u16(((v + (1 << (FRAC - 1))) >> FRAC).clamp(0, max) as u16);
        // Chroma terms (R, G, B contributions) of one chroma row. The conversion is linear, so
        // interpolating the terms is interpolating the chroma samples.
        let row_terms = |yc: usize, t: &mut ChromaTerms| {
            let cb_row = &frame.planes[1][yc * cwu..(yc + 1) * cwu];
            let cr_row = &frame.planes[2][yc * cwu..(yc + 1) * cwu];
            for (r, &cr) in t.r.iter_mut().zip(cr_row) {
                *r = r_mul * (i32::from(cr) - c_base);
            }
            for (b, &cb) in t.b.iter_mut().zip(cb_row) {
                *b = b_mul * (i32::from(cb) - c_base);
            }
            for ((g, &cb), &cr) in t.g.iter_mut().zip(cb_row).zip(cr_row) {
                *g = gr_mul * (i32::from(cr) - c_base) + gb_mul * (i32::from(cb) - c_base);
            }
        };
        let last_row = (height.saturating_sub(1) / sy as usize).min(chh as usize - 1);
        let last_column = (width.saturating_sub(1) / sx as usize).min(cwu - 1);
        // Terms of the chroma rows above and below the current luma row, of the luma row at
        // chroma width, and of its pixels.
        let (mut above, mut below) = (ChromaTerms::new(cwu), ChromaTerms::new(cwu));
        let (mut blended, mut line) = (ChromaTerms::new(cwu), ChromaTerms::new(width));
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
                    row_terms(j.min(chh as usize - 1), &mut above);
                }
                loaded = (j, usize::MAX);
            }
            if next != j && loaded.1 != next {
                row_terms(next, &mut below);
                loaded.1 = next;
            }
            let terms = if next == j {
                &above
            } else {
                blended.average(&above, &below);
                &blended
            };
            // Horizontally: chroma samples on even columns, odd columns in between.
            let pixels = if sx == 2 {
                line.widen(terms, last_column);
                &line
            } else {
                terms
            };
            let columns = out_row[..width * 4]
                .chunks_exact_mut(4)
                .zip(luma_row(y))
                .zip(&pixels.r)
                .zip(&pixels.g)
                .zip(&pixels.b);
            for ((((px, &yv), &r), &g), &b) in columns {
                let yf = y_mul * (i32::from(yv) - y_base);
                px[0] = to_out(yf + r);
                px[1] = to_out(yf - g);
                px[2] = to_out(yf + b);
                px[3] = alpha;
            }
        }
        return;
    }

    // Identity matrix: planes are G, B, R.
    let s = max_out / f64::from((1u32 << bd_y) - 1);
    let to_out = |v: f64| S::from_u16((v * s).round().clamp(0.0, max_out) as u16);
    for (y, out_row) in rows {
        let (_, cb_row, cr_row) = chroma_rows(y);
        let pixels = out_row[..width * 4]
            .chunks_exact_mut(4)
            .zip(luma_row(y))
            .zip(&xcs);
        for ((px, &g), &xc) in pixels {
            let (b, r) = (f64::from(cb_row[xc]), f64::from(cr_row[xc]));
            px.copy_from_slice(&[to_out(r), to_out(f64::from(g)), to_out(b), alpha]);
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
}
