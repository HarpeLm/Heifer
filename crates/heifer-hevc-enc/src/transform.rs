//! Forward transform and quantization: the counterparts of the decoder's scaling and inverse
//! transform (H.265 §8.6.2 to §8.6.4), for 8-bit samples and flat scaling.

use heifer_hevc_dec::recon::{DCT32, DST4};

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

/// Forward 2-D DCT of an `n`×`n` residual (`n` = 4..=32), row-major. The output has the scale
/// that the decoder's inverse transform expects, as in the HM reference encoder.
pub fn forward_dct(residual: &[i32], log2_size: u8, out: &mut [i32]) {
    match log2_size {
        2 => forward(residual, &DCT4, out),
        3 => forward(residual, &DCT8, out),
        4 => forward(residual, &DCT16, out),
        _ => forward(residual, &DCT32, out),
    }
}

/// Forward 4×4 DST-VII, used instead of the DCT for 4×4 intra luma blocks.
pub fn forward_dst4(residual: &[i32], out: &mut [i32]) {
    forward(residual, &DST4, out);
}

/// Separable forward transform with `basis[frequency][position]`: columns first, then rows.
/// The sums fit in `i32`: 8-bit residuals, basis values of at most 90 in magnitude.
fn forward<const N: usize>(residual: &[i32], basis: &[[i32; N]; N], out: &mut [i32]) {
    let log2_size = N.trailing_zeros() as i32;
    let shift1 = log2_size + 8 - 9;
    let shift2 = log2_size + 6;
    let mut tmp = [[0i32; N]; N];
    for (row, b) in tmp.iter_mut().zip(basis) {
        // Whole rows at once, which the compiler vectorizes.
        let mut acc = [0i32; N];
        for (y, &weight) in b.iter().enumerate() {
            for (a, &r) in acc.iter_mut().zip(&residual[y * N..y * N + N]) {
                *a += weight * r;
            }
        }
        for (t, a) in row.iter_mut().zip(acc) {
            *t = (a + (1 << shift1 >> 1)) >> shift1;
        }
    }
    for (k, row) in tmp.iter().enumerate() {
        for (l, b) in basis.iter().enumerate() {
            let s: i32 = b.iter().zip(row).map(|(&w, &t)| w * t).sum();
            out[k * N + l] = (s + (1 << (shift2 - 1))) >> shift2;
        }
    }
}

/// Quantization multipliers, `2^14 / levelScale` (with `levelScale` = 40, 45, 51, 57, 64, 72).
const QUANT_SCALE: [i64; 6] = [26214, 23302, 20560, 18396, 16384, 14564];

/// Quantizes transform coefficients in place for quantization parameter `qp` (0..=51), with a
/// dead zone of 1/3 as usual for intra blocks. Returns whether any level is non-zero.
pub fn quantize(coeffs: &mut [i32], log2_size: u8, qp: i32) -> bool {
    let transform_shift = 15 - 8 - i32::from(log2_size);
    let qbits = 14 + qp / 6 + transform_shift;
    let scale = QUANT_SCALE[(qp % 6) as usize];
    let offset = 171i64 << (qbits - 9);
    let mut any = false;
    for c in coeffs.iter_mut() {
        let level = ((i64::from(c.unsigned_abs()) * scale + offset) >> qbits).min(32767) as i32;
        *c = if *c < 0 { -level } else { level };
        any |= level != 0;
    }
    any
}

/// Like [`quantize`], also giving the unrounded level of each coefficient (signed, in units of
/// the quantization step) in `exact`, for rate-distortion refinements of the levels.
pub fn quantize_exact(coeffs: &mut [i32], log2_size: u8, qp: i32, exact: &mut [f32]) -> bool {
    let transform_shift = 15 - 8 - i32::from(log2_size);
    let qbits = 14 + qp / 6 + transform_shift;
    let unit = 1.0 / f64::from(1u32 << qbits);
    let scale = QUANT_SCALE[(qp % 6) as usize];
    for (e, &c) in exact.iter_mut().zip(coeffs.iter()) {
        *e = (f64::from(c) * scale as f64 * unit) as f32;
    }
    quantize(coeffs, log2_size, qp)
}

/// `QpC` of 4:2:0 chroma for a luma QP (Table 8-10), without chroma QP offsets.
pub fn chroma_qp(qp_y: i32) -> i32 {
    match qp_y {
        ..30 => qp_y,
        30..=43 => i32::from(
            [29u8, 30, 31, 32, 33, 33, 34, 34, 35, 35, 36, 36, 37, 37][(qp_y - 30) as usize],
        ),
        _ => qp_y - 6,
    }
}

/// Sum of absolute Hadamard-transformed differences, on 4×4 blocks (`n` ≥ 4).
pub fn satd(diff: &[i32], n: usize) -> u64 {
    let mut total = 0u64;
    for by in (0..n).step_by(4) {
        for bx in (0..n).step_by(4) {
            let mut m = [[0i32; 4]; 4];
            for (y, row) in m.iter_mut().enumerate() {
                let d = &diff[(by + y) * n + bx..];
                let (a, b) = (d[0] + d[3], d[0] - d[3]);
                let (c, e) = (d[1] + d[2], d[1] - d[2]);
                *row = [a + c, b + e, a - c, b - e];
            }
            let [m0, m1, m2, m3] = m;
            for x in 0..4 {
                let (a, b) = (m0[x] + m3[x], m0[x] - m3[x]);
                let (c, e) = (m1[x] + m2[x], m1[x] - m2[x]);
                total += u64::from((a + c).unsigned_abs())
                    + u64::from((b + e).unsigned_abs())
                    + u64::from((a - c).unsigned_abs())
                    + u64::from((b - e).unsigned_abs());
            }
        }
    }
    total / 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_residual_has_only_dc() {
        for log2 in 2..=5u8 {
            let n = 1usize << log2;
            let mut out = vec![0; n * n];
            forward_dct(&vec![10; n * n], log2, &mut out);
            assert!(out[0] > 0);
            assert!(out[1..].iter().all(|&c| c == 0), "log2 {log2}");
        }
    }

    #[test]
    fn quantization_dead_zone() {
        let mut c = [1000, -1000, 10, 0];
        assert!(quantize(&mut c, 3, 22));
        assert_eq!(c[0], -c[1]);
        assert_eq!(&c[2..], &[0, 0]);
    }

    #[test]
    fn chroma_qp_table() {
        assert_eq!(chroma_qp(29), 29);
        assert_eq!(chroma_qp(35), 33);
        assert_eq!(chroma_qp(51), 45);
    }
}
