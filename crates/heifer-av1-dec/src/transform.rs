//! Inverse transforms (AV1 §7.13): DCT, ADST, identity and Walsh-Hadamard, in rows then
//! columns.
//!
//! Products are computed on 64 bits and stored back truncated, so a stream outside the ranges the
//! specification requires gives wrong pixels instead of an overflow panic.

use crate::spec_tables::COS128_LOOKUP;

pub(crate) const DCT_DCT: u8 = 0;
pub(crate) const IDTX: u8 = 9;

fn round2(x: i64, n: u32) -> i64 {
    if n == 0 { x } else { (x + (1 << (n - 1))) >> n }
}

fn brev(bits: u32, x: usize) -> usize {
    x.reverse_bits() >> (usize::BITS - bits)
}

fn cos128(angle: i32) -> i64 {
    let a = (angle & 255) as usize;
    let c = |i: usize| i64::from(COS128_LOOKUP[i]);
    match a {
        0..=64 => c(a),
        65..=128 => -c(128 - a),
        129..=192 => -c(a - 128),
        _ => c(256 - a),
    }
}

fn sin128(angle: i32) -> i64 {
    cos128(angle - 64)
}

/// The butterfly rotation `B( a, b, angle, flip )`.
fn bf(t: &mut [i32], a: usize, b: usize, angle: i32, flip: bool) {
    let (ta, tb) = (i64::from(t[a]), i64::from(t[b]));
    let x = ta * cos128(angle) - tb * sin128(angle);
    let y = ta * sin128(angle) + tb * cos128(angle);
    t[a] = round2(x, 12) as i32;
    t[b] = round2(y, 12) as i32;
    if flip {
        t.swap(a, b);
    }
}

/// The Hadamard rotation `H( a, b, flip, r )`.
fn hd(t: &mut [i32], a: usize, b: usize, flip: bool, r: u32) {
    let (a, b) = if flip { (b, a) } else { (a, b) };
    let (x, y) = (i64::from(t[a]), i64::from(t[b]));
    let (lo, hi) = (-(1i64 << (r - 1)), (1i64 << (r - 1)) - 1);
    t[a] = (x + y).clamp(lo, hi) as i32;
    t[b] = (x - y).clamp(lo, hi) as i32;
}

/// The inverse DCT process on `t` of length `1 << n`.
fn dct(t: &mut [i32], n: u32, r: u32) {
    let copy: Vec<i32> = t.to_vec();
    for (i, v) in t.iter_mut().enumerate() {
        *v = copy[brev(n, i)];
    }
    let b = |t: &mut [i32], a: usize, b: usize, angle: usize, flip: bool| {
        bf(t, a, b, angle as i32, flip)
    };
    if n == 6 {
        for i in 0..16 {
            b(t, 32 + i, 63 - i, 63 - 4 * brev(4, i), false);
        }
    }
    if n >= 5 {
        for i in 0..8 {
            b(t, 16 + i, 31 - i, 6 + (brev(3, 7 - i) << 3), false);
        }
    }
    if n == 6 {
        for i in 0..16 {
            hd(t, 32 + i * 2, 33 + i * 2, i & 1 == 1, r);
        }
    }
    if n >= 4 {
        for i in 0..4 {
            b(t, 8 + i, 15 - i, 12 + (brev(2, 3 - i) << 4), false);
        }
    }
    if n >= 5 {
        for i in 0..8 {
            hd(t, 16 + 2 * i, 17 + 2 * i, i & 1 == 1, r);
        }
    }
    if n == 6 {
        for i in 0..4 {
            for j in 0..2 {
                b(
                    t,
                    62 - i * 4 - j,
                    33 + i * 4 + j,
                    60 - 16 * brev(2, i) + 64 * j,
                    true,
                );
            }
        }
    }
    if n >= 3 {
        for i in 0..2 {
            b(t, 4 + i, 7 - i, 56 - 32 * i, false);
        }
    }
    if n >= 4 {
        for i in 0..4 {
            hd(t, 8 + 2 * i, 9 + 2 * i, i & 1 == 1, r);
        }
    }
    if n >= 5 {
        for i in 0..2 {
            for j in 0..2 {
                b(
                    t,
                    30 - 4 * i - j,
                    17 + 4 * i + j,
                    24 + (j << 6) + ((1 - i) << 5),
                    true,
                );
            }
        }
    }
    if n == 6 {
        for i in 0..8 {
            for j in 0..2 {
                hd(t, 32 + i * 4 + j, 35 + i * 4 - j, i & 1 == 1, r);
            }
        }
    }
    for i in 0..2 {
        b(t, 2 * i, 2 * i + 1, 32 + 16 * i, i == 0);
    }
    if n >= 3 {
        for i in 0..2 {
            hd(t, 4 + 2 * i, 5 + 2 * i, i == 1, r);
        }
    }
    if n >= 4 {
        for i in 0..2 {
            b(t, 14 - i, 9 + i, 48 + 64 * i, true);
        }
    }
    if n >= 5 {
        for i in 0..4 {
            for j in 0..2 {
                hd(t, 16 + 4 * i + j, 19 + 4 * i - j, i & 1 == 1, r);
            }
        }
    }
    if n == 6 {
        for i in 0..2 {
            for j in 0..4 {
                b(
                    t,
                    61 - i * 8 - j,
                    34 + i * 8 + j,
                    56 - i * 32 + (j >> 1) * 64,
                    true,
                );
            }
        }
    }
    for i in 0..2 {
        hd(t, i, 3 - i, false, r);
    }
    if n >= 3 {
        b(t, 6, 5, 32, true);
    }
    if n >= 4 {
        for i in 0..2 {
            for j in 0..2 {
                hd(t, 8 + 4 * i + j, 11 + 4 * i - j, i == 1, r);
            }
        }
    }
    if n >= 5 {
        for i in 0..4 {
            b(t, 29 - i, 18 + i, 48 + (i >> 1) * 64, true);
        }
    }
    if n == 6 {
        for i in 0..4 {
            for j in 0..4 {
                hd(t, 32 + 8 * i + j, 39 + 8 * i - j, i & 1 == 1, r);
            }
        }
    }
    if n >= 3 {
        for i in 0..4 {
            hd(t, i, 7 - i, false, r);
        }
    }
    if n >= 4 {
        for i in 0..2 {
            b(t, 13 - i, 10 + i, 32, true);
        }
    }
    if n >= 5 {
        for i in 0..2 {
            for j in 0..4 {
                hd(t, 16 + i * 8 + j, 23 + i * 8 - j, i == 1, r);
            }
        }
    }
    if n == 6 {
        for i in 0..8 {
            b(t, 59 - i, 36 + i, if i < 4 { 48 } else { 112 }, true);
        }
    }
    if n >= 4 {
        for i in 0..8 {
            hd(t, i, 15 - i, false, r);
        }
    }
    if n >= 5 {
        for i in 0..4 {
            b(t, 27 - i, 20 + i, 32, true);
        }
    }
    if n == 6 {
        for i in 0..8 {
            hd(t, 32 + i, 47 - i, false, r);
            hd(t, 48 + i, 63 - i, true, r);
        }
    }
    if n >= 5 {
        for i in 0..16 {
            hd(t, i, 31 - i, false, r);
        }
    }
    if n == 6 {
        for i in 0..8 {
            b(t, 55 - i, 40 + i, 32, true);
        }
    }
    if n == 6 {
        for i in 0..32 {
            hd(t, i, 63 - i, false, r);
        }
    }
}

fn adst_in_permute(t: &mut [i32]) {
    let copy: Vec<i32> = t.to_vec();
    let n0 = t.len();
    for (i, v) in t.iter_mut().enumerate() {
        *v = copy[if i & 1 == 1 { i - 1 } else { n0 - i - 1 }];
    }
}

fn adst_out_permute(t: &mut [i32], n: u32) {
    let copy: Vec<i32> = t.to_vec();
    for (i, v) in t.iter_mut().enumerate() {
        let a = (i >> 3) & 1;
        let b = ((i >> 2) & 1) ^ ((i >> 3) & 1);
        let c = ((i >> 1) & 1) ^ ((i >> 2) & 1);
        let d = (i & 1) ^ ((i >> 1) & 1);
        let idx = ((d << 3) | (c << 2) | (b << 1) | a) >> (4 - n);
        *v = if i & 1 == 1 {
            copy[idx].wrapping_neg()
        } else {
            copy[idx]
        };
    }
}

fn adst4(t: &mut [i32]) {
    const SINPI: [i64; 4] = [1321, 2482, 3344, 3803];
    let t64: [i64; 4] = [t[0].into(), t[1].into(), t[2].into(), t[3].into()];
    let mut s = [
        SINPI[0] * t64[0],
        SINPI[1] * t64[0],
        SINPI[2] * t64[1],
        SINPI[3] * t64[2],
        SINPI[0] * t64[2],
        SINPI[1] * t64[3],
        SINPI[3] * t64[3],
    ];
    let a7 = t64[0] - t64[2];
    let b7 = a7 + t64[3];
    s[0] += s[3];
    s[1] -= s[4];
    s[3] = s[2];
    s[2] = SINPI[2] * b7;
    s[0] += s[5];
    s[1] -= s[6];
    let x = [s[0] + s[3], s[1] + s[3], s[2], s[0] + s[1] - s[3]];
    for (v, x) in t.iter_mut().zip(x) {
        *v = round2(x, 12) as i32;
    }
}

fn adst8(t: &mut [i32], r: u32) {
    adst_in_permute(t);
    for i in 0..4 {
        bf(t, 2 * i, 2 * i + 1, 60 - 16 * i as i32, true);
    }
    for i in 0..4 {
        hd(t, i, 4 + i, false, r);
    }
    for i in 0..2 {
        bf(t, 4 + 3 * i, 5 + i, 48 - 32 * i as i32, true);
    }
    for i in 0..2 {
        for j in 0..2 {
            hd(t, 4 * j + i, 2 + 4 * j + i, false, r);
        }
    }
    for i in 0..2 {
        bf(t, 2 + 4 * i, 3 + 4 * i, 32, true);
    }
    adst_out_permute(t, 3);
}

fn adst16(t: &mut [i32], r: u32) {
    adst_in_permute(t);
    for i in 0..8 {
        bf(t, 2 * i, 2 * i + 1, 62 - 8 * i as i32, true);
    }
    for i in 0..8 {
        hd(t, i, 8 + i, false, r);
    }
    for i in 0..2 {
        bf(t, 8 + 2 * i, 9 + 2 * i, 56 - 32 * i as i32, true);
        bf(t, 13 + 2 * i, 12 + 2 * i, 8 + 32 * i as i32, true);
    }
    for i in 0..4 {
        for j in 0..2 {
            hd(t, 8 * j + i, 4 + 8 * j + i, false, r);
        }
    }
    for i in 0..2 {
        for j in 0..2 {
            bf(
                t,
                4 + 8 * j + 3 * i,
                5 + 8 * j + i,
                48 - 32 * i as i32,
                true,
            );
        }
    }
    for i in 0..2 {
        for j in 0..4 {
            hd(t, 4 * j + i, 2 + 4 * j + i, false, r);
        }
    }
    for i in 0..4 {
        bf(t, 2 + 4 * i, 3 + 4 * i, 32, true);
    }
    adst_out_permute(t, 4);
}

fn identity(t: &mut [i32], n: u32) {
    for v in t.iter_mut() {
        let x = i64::from(*v);
        *v = match n {
            2 => round2(x * 5793, 12),
            3 => x * 2,
            4 => round2(x * 11586, 12),
            _ => x * 4,
        } as i32;
    }
}

fn wht(t: &mut [i32], shift: u32) {
    let mut a = t[0] >> shift;
    let mut c = t[1] >> shift;
    let mut d = t[2] >> shift;
    let mut b = t[3] >> shift;
    a = a.wrapping_add(c);
    d = d.wrapping_sub(b);
    let e = (a.wrapping_sub(d)) >> 1;
    b = e.wrapping_sub(b);
    c = e.wrapping_sub(c);
    a = a.wrapping_sub(b);
    d = d.wrapping_add(c);
    t[..4].copy_from_slice(&[a, b, c, d]);
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Dct,
    Adst,
    Identity,
}

fn transform_1d(t: &mut [i32], kind: Kind, n: u32, r: u32) {
    match kind {
        Kind::Dct => dct(t, n, r),
        Kind::Adst => match n {
            2 => adst4(t),
            3 => adst8(t, r),
            _ => adst16(t, r),
        },
        Kind::Identity => identity(t, n),
    }
}

/// The 2D inverse transform process: `dequant` holds the `min(32, w)` by `min(32, h)` dequantized
/// coefficients row by row, the result is the `w` by `h` residual.
pub(crate) fn inverse_2d(
    dequant: &[i32],
    log2w: u32,
    log2h: u32,
    tx_type: u8,
    lossless: bool,
    row_shift: u32,
    bit_depth: u32,
) -> Vec<i32> {
    let (w, h) = (1usize << log2w, 1usize << log2h);
    let tw = w.min(32);
    let (row_shift, col_shift) = if lossless { (0, 0) } else { (row_shift, 4) };
    let row_range = bit_depth + 8;
    let col_range = (bit_depth + 6).max(16);
    let row_kind = match tx_type {
        0 | 1 | 4 | 11 => Kind::Dct,
        2 | 3 | 5..=8 | 13 | 15 => Kind::Adst,
        _ => Kind::Identity,
    };
    let col_kind = match tx_type {
        0 | 2 | 5 | 10 => Kind::Dct,
        1 | 3 | 4 | 6..=8 | 12 | 14 => Kind::Adst,
        _ => Kind::Identity,
    };
    let mut residual = vec![0i32; w * h];
    let mut t = vec![0i32; w.max(h)];
    let (lo, hi) = (-(1i32 << (col_range - 1)), (1i32 << (col_range - 1)) - 1);
    for i in 0..h.min(32) {
        let row = &mut t[..w];
        for (j, v) in row.iter_mut().enumerate() {
            *v = if j < 32 {
                dequant.get(i * tw + j).copied().unwrap_or(0)
            } else {
                0
            };
        }
        if row.iter().all(|&v| v == 0) {
            continue;
        }
        if log2w.abs_diff(log2h) == 1 {
            for v in row.iter_mut() {
                *v = round2(i64::from(*v) * 2896, 12) as i32;
            }
        }
        if lossless {
            wht(row, 2);
        } else {
            transform_1d(row, row_kind, log2w, row_range);
        }
        for (j, &v) in row.iter().enumerate() {
            residual[i * w + j] = (round2(i64::from(v), row_shift) as i32).clamp(lo, hi);
        }
    }
    for j in 0..w {
        let col = &mut t[..h];
        for (i, v) in col.iter_mut().enumerate() {
            *v = residual[i * w + j];
        }
        if lossless {
            wht(col, 0);
        } else {
            transform_1d(col, col_kind, log2h, col_range);
        }
        for (i, &v) in col.iter().enumerate() {
            residual[i * w + j] = round2(i64::from(v), col_shift) as i32;
        }
    }
    residual
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Floating-point inverse DCT-II, scaled as AV1's: `X[0]` weight 1/√2.
    fn reference_idct(input: &[f64]) -> Vec<f64> {
        let n = input.len();
        (0..n)
            .map(|x| {
                input
                    .iter()
                    .enumerate()
                    .map(|(k, &c)| {
                        let scale = if k == 0 {
                            std::f64::consts::FRAC_1_SQRT_2
                        } else {
                            1.0
                        };
                        scale
                            * c
                            * (std::f64::consts::PI * (2 * x + 1) as f64 * k as f64
                                / (2 * n) as f64)
                                .cos()
                    })
                    .sum::<f64>()
            })
            .collect()
    }

    #[test]
    fn dct_matches_the_continuous_transform() {
        for n in 2..=6 {
            let len = 1usize << n;
            let input: Vec<i32> = (0..len)
                .map(|i| ((i * 37 + 11) % 200) as i32 * 8 - 800)
                .collect();
            let mut t = input.clone();
            dct(&mut t, n, 24);
            let expected = reference_idct(&input.iter().map(|&v| f64::from(v)).collect::<Vec<_>>());
            for (i, (&got, want)) in t.iter().zip(expected).enumerate() {
                // Each of the n stages rounds once.
                assert!(
                    (f64::from(got) - want).abs() <= f64::from(n),
                    "n={n} i={i}: {got} vs {want}"
                );
            }
        }
    }

    #[test]
    fn adst4_matches_the_continuous_transform() {
        // AV1's ADST4 is the DST-VII: out[x] = Σ in[k]·sin(π(2x+1)(k+1)/9)·√(4/9)·√2 / √2.
        let input = [400, -300, 200, -100];
        let mut t = input;
        adst4(&mut t);
        for (x, &got) in t.iter().enumerate() {
            let want: f64 = input
                .iter()
                .enumerate()
                .map(|(k, &c)| {
                    f64::from(c)
                        * (std::f64::consts::PI * ((x + 1) * (2 * k + 1)) as f64 / 9.0).sin()
                        * (8.0f64 / 9.0).sqrt()
                        * std::f64::consts::FRAC_1_SQRT_2
                        * 2.0f64.sqrt()
                })
                .sum();
            assert!(
                (f64::from(got) - want).abs() < 2.0,
                "x={x}: {got} vs {want}"
            );
        }
    }

    #[test]
    fn adst_is_orthogonal() {
        // Energy is kept up to the transform's scale: √(N/2) for AV1's integer ADSTs.
        for n in 3..=4u32 {
            let len = 1usize << n;
            for k in 0..len {
                let mut t = vec![0; len];
                t[k] = 4096;
                adst_in_out(&mut t, n);
                let energy: f64 = t.iter().map(|&v| f64::from(v).powi(2)).sum();
                let want = 4096f64.powi(2) * len as f64 / 2.0;
                assert!(
                    (energy / want - 1.0).abs() < 0.01,
                    "n={n} k={k}: {energy} vs {want}"
                );
            }
        }
    }

    fn adst_in_out(t: &mut [i32], n: u32) {
        if n == 3 { adst8(t, 24) } else { adst16(t, 24) }
    }

    #[test]
    fn dc_only_blocks_are_flat() {
        for (log2w, log2h, shift) in [
            (2, 2, 0),
            (3, 3, 1),
            (4, 4, 2),
            (5, 5, 2),
            (6, 6, 2),
            (3, 4, 1),
        ] {
            let tw = 1usize << log2w.min(5);
            let th = 1usize << log2h.min(5);
            let mut dequant = vec![0; tw * th];
            dequant[0] = 1024;
            let res = inverse_2d(&dequant, log2w, log2h, DCT_DCT, false, shift, 8);
            assert!(res.iter().all(|&v| v == res[0]), "{log2w}x{log2h}: {res:?}");
            assert!(res[0] > 0);
        }
    }

    #[test]
    fn walsh_hadamard_round_trips() {
        // Lossless: forward WHT (libaom's av1_fwht4x4) of a residual, then the inverse.
        let block = [[3, -1, 0, 2], [0, 5, -4, 1], [7, 0, 0, -2], [1, 1, 1, 1]];
        let coeffs = fwht4x4(&block);
        let res = inverse_2d(&coeffs, 2, 2, DCT_DCT, true, 0, 8);
        for i in 0..4 {
            for j in 0..4 {
                assert_eq!(res[i * 4 + j], block[i][j], "({i}, {j})");
            }
        }
    }

    /// libaom's `av1_fwht4x4_c` with the AV1 `UNIT_QUANT_SHIFT` of 2.
    fn fwht4x4(input: &[[i32; 4]; 4]) -> Vec<i32> {
        let mut tmp = [[0i32; 4]; 4];
        let mut out = vec![0i32; 16];
        for i in 0..4 {
            let (mut a1, mut b1, mut c1, mut d1) =
                (input[0][i], input[1][i], input[2][i], input[3][i]);
            a1 += b1;
            d1 -= c1;
            let e1 = (a1 - d1) >> 1;
            b1 = e1 - b1;
            c1 = e1 - c1;
            a1 -= c1;
            d1 += b1;
            tmp[0][i] = a1;
            tmp[1][i] = c1;
            tmp[2][i] = d1;
            tmp[3][i] = b1;
        }
        for i in 0..4 {
            let (mut a1, mut b1, mut c1, mut d1) = (tmp[i][0], tmp[i][1], tmp[i][2], tmp[i][3]);
            a1 += b1;
            d1 -= c1;
            let e1 = (a1 - d1) >> 1;
            b1 = e1 - b1;
            c1 = e1 - c1;
            a1 -= c1;
            d1 += b1;
            out[i * 4] = a1 * 4;
            out[i * 4 + 1] = c1 * 4;
            out[i * 4 + 2] = d1 * 4;
            out[i * 4 + 3] = b1 * 4;
        }
        out
    }
}
