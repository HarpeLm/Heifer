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
#[cfg(test)]
pub fn forward_dct(residual: &[i32], log2_size: u8, out: &mut [i32]) {
    forward_transform(residual, log2_size, false, 8, out);
}

/// Forward transform of an `n`×`n` residual of `bit_depth`-bit samples: the DCT, or the 4×4
/// DST-VII when `dst` is set (4×4 intra luma blocks).
pub fn forward_transform(
    residual: &[i32],
    log2_size: u8,
    dst: bool,
    bit_depth: u8,
    out: &mut [i32],
) {
    let bd = i32::from(bit_depth);
    match log2_size {
        2 if dst => forward(residual, &DST4, bd, out),
        2 => forward_dct_sym::<4, 2>(residual, &DCT4, bd, out),
        3 => forward_dct_sym::<8, 4>(residual, &DCT8, bd, out),
        4 => forward_dct_sym::<16, 8>(residual, &DCT16, bd, out),
        _ => forward_dct_sym::<32, 16>(residual, &DCT32, bd, out),
    }
}

/// Even/odd butterfly forward 2-D DCT (bit-exact with [`forward`], half the multiplications).
fn forward_dct_sym<const N: usize, const H: usize>(
    residual: &[i32],
    basis: &[[i32; N]; N],
    bit_depth: i32,
    out: &mut [i32],
) {
    let log2_size = N.trailing_zeros() as i32;
    let shift1 = log2_size + bit_depth - 9;
    let add1 = 1 << shift1 >> 1;
    let shift2 = log2_size + 6;
    let add2 = 1 << (shift2 - 1);

    // Column pass: fold symmetric pairs of rows first.
    let mut sum_rows = [[0i32; N]; H];
    let mut diff_rows = [[0i32; N]; H];
    for y in 0..H {
        let top = &residual[y * N..y * N + N];
        let bot = &residual[(N - 1 - y) * N..(N - 1 - y) * N + N];
        for x in 0..N {
            sum_rows[y][x] = top[x] + bot[x];
            diff_rows[y][x] = top[x] - bot[x];
        }
    }

    let mut tmp = [[0i32; N]; N];
    for k in (0..N).step_by(2) {
        let mut acc_e = [0i32; N];
        let mut acc_o = [0i32; N];
        let be = &basis[k];
        let bo = &basis[k + 1];
        for y in 0..H {
            let we = be[y];
            let wo = bo[y];
            let sr = &sum_rows[y];
            let dr = &diff_rows[y];
            for x in 0..N {
                acc_e[x] += we * sr[x];
                acc_o[x] += wo * dr[x];
            }
        }
        for x in 0..N {
            tmp[k][x] = (acc_e[x] + add1) >> shift1;
            tmp[k + 1][x] = (acc_o[x] + add1) >> shift1;
        }
    }

    // Row pass: fold symmetric pairs within each intermediate row.
    for (k, row) in tmp.iter().enumerate() {
        let mut e = [0i32; H];
        let mut o = [0i32; H];
        for x in 0..H {
            e[x] = row[x] + row[N - 1 - x];
            o[x] = row[x] - row[N - 1 - x];
        }
        let dst = &mut out[k * N..k * N + N];
        for l in (0..N).step_by(2) {
            let mut se = 0i32;
            let mut so = 0i32;
            let be = &basis[l];
            let bo = &basis[l + 1];
            for x in 0..H {
                se += be[x] * e[x];
                so += bo[x] * o[x];
            }
            dst[l] = (se + add2) >> shift2;
            dst[l + 1] = (so + add2) >> shift2;
        }
    }
}

/// Separable forward transform with `basis[frequency][position]`: columns first, then rows.
/// The sums fit in `i32`: residuals of up to 10 bits, basis values of at most 90 in magnitude.
fn forward<const N: usize>(
    residual: &[i32],
    basis: &[[i32; N]; N],
    bit_depth: i32,
    out: &mut [i32],
) {
    let log2_size = N.trailing_zeros() as i32;
    let shift1 = log2_size + bit_depth - 9;
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
#[cfg(test)]
pub fn quantize(coeffs: &mut [i32], log2_size: u8, qp: i32) -> bool {
    quantize_levels(coeffs, log2_size, qp, 8, None)
}

/// Quantization of the coefficients of `bit_depth`-bit samples with `qp` = `Qp′`, the QP plus
/// `QpBdOffset` (6 per bit above 8). Optionally gives the unrounded levels in `exact`.
pub fn quantize_levels(
    coeffs: &mut [i32],
    log2_size: u8,
    qp: i32,
    bit_depth: u8,
    exact: Option<&mut [f32]>,
) -> bool {
    let transform_shift = 15 - i32::from(bit_depth) - i32::from(log2_size);
    let qbits = 14 + qp / 6 + transform_shift;
    let scale = QUANT_SCALE[(qp % 6) as usize];
    if let Some(exact) = exact {
        let exact_scale = (scale as f64 / f64::from(1u32 << qbits)) as f32;
        for (e, &c) in exact.iter_mut().zip(coeffs.iter()) {
            *e = c as f32 * exact_scale;
        }
    }
    let offset = 171i64 << (qbits - 9);
    let mut any_bits = 0i32;
    for c in coeffs.iter_mut() {
        let level = ((i64::from(c.unsigned_abs()) * scale + offset) >> qbits).min(32767) as i32;
        *c = if *c < 0 { -level } else { level };
        any_bits |= level;
    }
    any_bits != 0
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
            let mut block_sum = 0u32;
            for x in 0..4 {
                let (a, b) = (m0[x] + m3[x], m0[x] - m3[x]);
                let (c, e) = (m1[x] + m2[x], m1[x] - m2[x]);
                block_sum += (a + c).unsigned_abs()
                    + (b + e).unsigned_abs()
                    + (a - c).unsigned_abs()
                    + (b - e).unsigned_abs();
            }
            total += u64::from(block_sum);
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
