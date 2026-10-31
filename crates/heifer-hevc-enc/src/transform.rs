//! Forward transform and quantization: the counterparts of the decoder's scaling and inverse
//! transform (H.265 §8.6.2 to §8.6.4), for 8-bit samples and flat scaling.

use heifer_hevc_dec::recon::DCT32;

/// Forward 2-D DCT of an `n`×`n` residual (`n` = 4..=32), row-major. The output has the scale
/// that the decoder's inverse transform expects, as in the HM reference encoder.
pub fn forward_dct(residual: &[i32], log2_size: u8, out: &mut [i32]) {
    let n = 1usize << log2_size;
    let step = 32 / n;
    let bit_depth = 8;
    // First stage: columns (vertical frequencies), then rows.
    let shift1 = u32::from(log2_size) + bit_depth - 9;
    let shift2 = u32::from(log2_size) + 6;
    let mut tmp = vec![0i32; n * n];
    for k in 0..n {
        let basis = &DCT32[k * step];
        for x in 0..n {
            let s: i64 = (0..n)
                .map(|y| i64::from(basis[y]) * i64::from(residual[y * n + x]))
                .sum();
            tmp[k * n + x] = ((s + (1 << shift1 >> 1)) >> shift1) as i32;
        }
    }
    for k in 0..n {
        for l in 0..n {
            let basis = &DCT32[l * step];
            let s: i64 = (0..n)
                .map(|x| i64::from(basis[x]) * i64::from(tmp[k * n + x]))
                .sum();
            out[k * n + l] = ((s + (1 << (shift2 - 1))) >> shift2) as i32;
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
