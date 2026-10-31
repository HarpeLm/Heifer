//! Rate-distortion refinements of quantized levels.

use heifer_hevc_dec::cabac::ContextModel;
use heifer_hevc_dec::contexts::{Contexts, ctx};
use heifer_hevc_dec::scan::{ScanType, scan_order};

/// Context offset and shift of `last_sig_coeff_{x,y}_prefix` (§9.3.4.2.3).
pub(crate) fn last_ctx(log2_size: u8, luma: bool) -> (u8, u8) {
    if luma {
        (
            3 * (log2_size - 2) + ((log2_size - 1) >> 2),
            (log2_size + 1) >> 2,
        )
    } else {
        (15, log2_size - 2)
    }
}

/// `last_sig_coeff_*_prefix` of a position, and its suffix (value, bit count) if any.
pub(crate) fn last_prefix(pos: u32) -> (u32, Option<(u32, u32)>) {
    if pos < 4 {
        return (pos, None);
    }
    let n = 31 - pos.leading_zeros() - 1;
    let b = (pos >> n) - 2;
    (2 * (n + 1) + b, Some((pos - ((2 + b) << n), n)))
}

/// Context increment of `sig_coeff_flag` (§9.3.4.2.5), as in the decoder.
#[allow(clippy::too_many_arguments)]
pub(crate) fn sig_ctx_inc(
    luma: bool,
    log2_size: u8,
    xc: usize,
    yc: usize,
    xp: u8,
    yp: u8,
    prev_csbf: u8,
    scan: ScanType,
) -> usize {
    const CTX_IDX_MAP: [u8; 16] = [0, 1, 4, 5, 2, 3, 4, 5, 6, 6, 8, 8, 7, 7, 8, 8];
    let sig_ctx: usize = if log2_size == 2 {
        usize::from(CTX_IDX_MAP[(yc << 2) + xc])
    } else if xc + yc == 0 {
        0
    } else {
        let (xp, yp) = (usize::from(xp), usize::from(yp));
        let mut s = match prev_csbf {
            0 => {
                if xp + yp == 0 {
                    2
                } else if xp + yp < 3 {
                    1
                } else {
                    0
                }
            }
            1 => match yp {
                0 => 2,
                1 => 1,
                _ => 0,
            },
            2 => match xp {
                0 => 2,
                1 => 1,
                _ => 0,
            },
            _ => 2,
        };
        if luma {
            if (xc >> 2) + (yc >> 2) > 0 {
                s += 3;
            }
            s += if log2_size == 3 {
                if scan == ScanType::Diagonal { 9 } else { 15 }
            } else {
                21
            };
        } else {
            s += if log2_size == 3 { 9 } else { 12 };
        }
        s
    };
    if luma { sig_ctx } else { 27 + sig_ctx }
}

/// Buffers and tables of [`rdoq`], allocated once per picture.
pub(crate) struct RdoqScratch {
    /// Estimated cost in bits of a bin per context state: [MPS, LPS].
    bits: [[f32; 2]; 64],
    chosen: Vec<u32>,
    cost_level: Vec<f32>,
    cost_sig1: Vec<f32>,
    cost_zero: Vec<f32>,
    zero_after: Vec<f32>,
    positions: Vec<(u16, u8, u8)>,
    magnitudes: Vec<f32>,
}

impl RdoqScratch {
    pub(crate) fn new() -> Self {
        Self {
            bits: crate::estimate::entropy_bits(),
            chosen: vec![0; 1024],
            cost_level: vec![0.0; 1024],
            cost_sig1: vec![0.0; 1024],
            cost_zero: vec![0.0; 1024],
            zero_after: vec![0.0; 1025],
            positions: vec![(0, 0, 0); 1024],
            magnitudes: vec![0.0; 1024],
        }
    }
}

/// Estimated cost in bits of coding `bin` with a context, from its probability state.
fn bits(table: &[[f32; 2]; 64], model: &ContextModel, bin: bool) -> f32 {
    table[usize::from(model.state.min(63))][usize::from(u8::from(bin) != model.mps)]
}

/// Bits of `coeff_abs_level_remaining` for `value` with Rice parameter `rice`.
fn remaining_bits(value: u32, rice: u32) -> f32 {
    if value < (4 << rice) {
        ((value >> rice) + 1 + rice) as f32
    } else {
        let mut prefix = 4;
        while value >= (((2u32 << (prefix - 3)) + 2) << rice) {
            prefix += 1;
        }
        (prefix + 1 + prefix - 3 + rice) as f32
    }
}

/// Rate-distortion optimized quantization, after the HM reference encoder: chooses each level
/// among the rounded-up and rounded-down values of `exact` (unrounded levels) and zero, then
/// whether to drop whole 4×4 sub-blocks, then the last significant position, minimizing
/// `error² + lambda · bits` (errors in quantization steps, `lambda` = λ / step²). Bits are
/// estimated from the current probabilities of the contexts. Writes the signed levels.
#[allow(clippy::too_many_arguments)]
pub(crate) fn rdoq(
    levels: &mut [i32],
    exact: &[f32],
    log2_size: u8,
    luma: bool,
    scan: ScanType,
    contexts: &Contexts,
    lambda: f32,
    scratch: &mut RdoqScratch,
) {
    let size = 1usize << log2_size;
    let log2_sb = log2_size - 2;
    let sb_scan = scan_order(log2_sb, scan);
    let pos_scan = scan_order(2, scan);
    let sb_width = 1usize << log2_sb;
    let n = 1usize << (2 * log2_size);
    let table = scratch.bits;
    // Per scan position: chosen magnitude, cost when coded, cost of the sig flag alone, cost
    // of zero (error² plus the sig flag). Every position up to `last_start` is written below
    // before being read.
    let RdoqScratch {
        chosen,
        cost_level,
        cost_sig1,
        cost_zero,
        zero_after,
        positions,
        magnitudes,
        ..
    } = scratch;
    // Coefficient index and position of each scan position (sub-block · 16 + position), and
    // its unrounded magnitude.
    for (sb, &(xs, ys)) in sb_scan.iter().enumerate() {
        for (p, &(xp, yp)) in pos_scan.iter().enumerate() {
            let (x, y) = (xs * 4 + xp, ys * 4 + yp);
            let i = usize::from(y) * size + usize::from(x);
            positions[sb * 16 + p] = (i as u16, x, y);
            magnitudes[sb * 16 + p] = exact[i].abs();
        }
    }
    let (positions, magnitudes) = (&*positions, &*magnitudes);
    let index = |g: usize| -> (usize, usize, usize) {
        let (i, x, y) = positions[g];
        (usize::from(i), usize::from(x), usize::from(y))
    };
    let magnitude = |g: usize| magnitudes[g];
    levels[..n].fill(0);
    let Some(last_start) = magnitudes[..n].iter().rposition(|&m| m >= 0.5) else {
        return;
    };
    let ctx_bits = |i: usize, bin: bool| bits(&table, &contexts[i], bin);
    let mut coded_sb = [[false; 8]; 8];
    let mut sb_cost = [0f32; 64]; // chosen cost of each sub-block (with its flag)
    let mut sb_zero = [0f32; 64]; // cost of dropping it
    let mut greater1_ctx_state: u8 = 1;

    for sb in (0..=last_start / 16).rev() {
        let (xs, ys) = sb_scan[sb];
        let (xs, ys) = (usize::from(xs), usize::from(ys));
        let prev_csbf = u8::from(xs + 1 < sb_width && coded_sb[xs + 1][ys])
            | (u8::from(ys + 1 < sb_width && coded_sb[xs][ys + 1]) << 1);
        let mut ctx_set = if sb == 0 || !luma { 0 } else { 2 };
        if greater1_ctx_state == 0 {
            ctx_set += 1;
        }
        let mut state: u8 = 1; // greater1Ctx within the sub-block
        let (mut count, mut seen_g1, mut rice) = (0usize, false, 0u32);
        let top = if sb == last_start / 16 {
            last_start % 16
        } else {
            15
        };
        for pos in (0..=top).rev() {
            let g = sb * 16 + pos;
            let (_, x, y) = index(g);
            let e = magnitude(g);
            let (xp, yp) = pos_scan[pos];
            let sig_ctx =
                ctx::SIG_COEFF_FLAG + sig_ctx_inc(luma, log2_size, x, y, xp, yp, prev_csbf, scan);
            let is_last = g == last_start;
            cost_sig1[g] = if is_last {
                0.0
            } else {
                lambda * ctx_bits(sig_ctx, true)
            };
            cost_zero[g] = e * e
                + if is_last {
                    0.0
                } else {
                    lambda * ctx_bits(sig_ctx, false)
                };

            let rounded = (e + 0.5) as u32;
            let g1_ctx = ctx::COEFF_ABS_LEVEL_GREATER1_FLAG
                + ctx_set * 4
                + usize::from(state.min(3))
                + if luma { 0 } else { 16 };
            let g2_ctx = ctx::COEFF_ABS_LEVEL_GREATER2_FLAG + ctx_set + if luma { 0 } else { 4 };
            let rate = |level: u32| -> f32 {
                let mut r = 1.0; // sign
                if count < 8 {
                    r += ctx_bits(g1_ctx, level > 1);
                    let first_g1 = level > 1 && !seen_g1;
                    if first_g1 {
                        r += ctx_bits(g2_ctx, level > 2);
                    }
                    let base = 1 + u32::from(level > 1) + u32::from(first_g1 && level > 2);
                    let threshold = if first_g1 { 3 } else { 2 };
                    if base == threshold {
                        r += remaining_bits(level - base, rice);
                    }
                } else {
                    r += remaining_bits(level - 1, rice);
                }
                r
            };
            let mut best = (cost_zero[g], 0u32);
            if is_last {
                best.0 = f32::MAX; // the last position is significant by definition
            }
            for level in [rounded, rounded.saturating_sub(1)] {
                if level == 0 {
                    continue;
                }
                let d = e - level as f32;
                let cost = d * d + cost_sig1[g] + lambda * rate(level);
                if cost < best.0 {
                    best = (cost, level);
                }
            }
            if best.1 == 0 && is_last {
                best = (cost_zero[g], 1.max(rounded));
                let d = e - best.1 as f32;
                best.0 = d * d + lambda * rate(best.1);
            }
            chosen[g] = best.1;
            cost_level[g] = best.0;
            if best.1 > 0 {
                if count < 8 {
                    if best.1 > 1 {
                        state = 0;
                        seen_g1 = true;
                    } else if state > 0 && state < 3 {
                        state += 1;
                    }
                }
                let base = if count < 8 {
                    1 + u32::from(best.1 > 1) + u32::from(best.1 > 2)
                } else {
                    1
                };
                if best.1 >= base && best.1 > 3 * (1 << rice) {
                    rice = (rice + 1).min(4);
                }
                count += 1;
            }
        }
        if count > 0 {
            greater1_ctx_state = state;
        }

        // Dropping the whole sub-block (not possible for the first and last ones).
        let range = sb * 16..=sb * 16 + top;
        let coded_cost: f32 = range.clone().map(|g| cost_level[g]).sum();
        let zero_cost: f32 = range.clone().map(|g| magnitude(g).powi(2)).sum();
        let inc = usize::from(prev_csbf != 0) + if luma { 0 } else { 2 };
        let flag = ctx::CODED_SUB_BLOCK_FLAG + inc;
        let any = range.clone().any(|g| chosen[g] > 0);
        if sb > 0 && sb < last_start / 16 {
            sb_cost[sb] = coded_cost + lambda * ctx_bits(flag, true);
            sb_zero[sb] = zero_cost + lambda * ctx_bits(flag, false);
            if !any || sb_zero[sb] < sb_cost[sb] {
                for g in range {
                    chosen[g] = 0;
                    cost_level[g] = magnitude(g).powi(2);
                }
                sb_cost[sb] = sb_zero[sb];
            }
        } else {
            sb_cost[sb] = coded_cost;
        }
        coded_sb[xs][ys] = range_any(chosen, sb * 16, sb * 16 + top);
    }

    // Last significant position: keep the levels up to it, drop the ones after.
    let (ctx_offset, ctx_shift) = last_ctx(log2_size, luma);
    let c_max = (u32::from(log2_size) << 1) - 1;
    let last_bits = |g: usize| -> f32 {
        let (_, x, y) = index(g);
        let mut pos = [x as u32, y as u32];
        if scan == ScanType::Vertical {
            pos.swap(0, 1);
        }
        let mut r = 0.0;
        for (k, base) in [ctx::LAST_SIG_COEFF_X_PREFIX, ctx::LAST_SIG_COEFF_Y_PREFIX]
            .into_iter()
            .enumerate()
        {
            let (prefix, suffix) = last_prefix(pos[k]);
            for i in 0..prefix.min(c_max) {
                r += ctx_bits(
                    base + usize::from(ctx_offset) + (i as usize >> ctx_shift),
                    true,
                );
            }
            if prefix < c_max {
                r += ctx_bits(
                    base + usize::from(ctx_offset) + (prefix as usize >> ctx_shift),
                    false,
                );
            }
            r += suffix.map_or(0.0, |(_, bits)| bits as f32);
        }
        r
    };
    // Cost of everything up to and including position g when g is the last one: the sum of the
    // chosen costs before it, its own cost without sig flag, the dropped errors after it.
    let mut best: Option<(f32, usize)> = None;
    let mut before = 0.0f32;
    let total_zero_after = {
        zero_after[last_start + 1] = 0.0;
        for g in (0..=last_start).rev() {
            zero_after[g] = zero_after[g + 1] + magnitude(g).powi(2);
        }
        &*zero_after
    };
    for g in 0..=last_start {
        if chosen[g] > 0 {
            let cost = before + cost_level[g] - cost_sig1[g]
                + total_zero_after[g + 1]
                + lambda * last_bits(g);
            if best.is_none_or(|(c, _)| cost < c) {
                best = Some((cost, g));
            }
        }
        before += if chosen[g] > 0 {
            cost_level[g]
        } else {
            cost_zero[g]
        };
    }
    let Some((cost, last)) = best else { return };
    // Coding nothing at all (cbf 0) may be cheaper.
    let cbf = if luma {
        ctx::CBF_LUMA + 1
    } else {
        ctx::CBF_CHROMA
    };
    if total_zero_after[0] + lambda * ctx_bits(cbf, false) <= cost + lambda * ctx_bits(cbf, true) {
        return;
    }
    for (g, &level) in chosen.iter().enumerate().take(last + 1) {
        let (i, _, _) = index(g);
        let level = level as i32;
        levels[i] = if exact[i] < 0.0 { -level } else { level };
    }
}

fn range_any(chosen: &[u32], from: usize, to: usize) -> bool {
    chosen[from..=to].iter().any(|&l| l > 0)
}

/// Sign data hiding (H.265 §7.4.9.11): in each 4×4 sub-block whose first and last non-zero
/// levels are more than 3 scan positions apart, the decoder infers the sign of the first one
/// from the parity of the sum of the levels. Adjusts the levels by ±1 where needed, choosing
/// the change that costs the least distortion (plus `rate_weight` per level made significant or
/// zero, in units of the squared quantization step).
pub(crate) fn hide_signs(
    levels: &mut [i32],
    exact: &[f32],
    log2_size: u8,
    scan: ScanType,
    rate_weight: f32,
) {
    let size = 1usize << log2_size;
    let sb_scan = scan_order(log2_size - 2, scan);
    let pos_scan = scan_order(2, scan);
    for &(xs, ys) in sb_scan {
        let index: [usize; 16] = core::array::from_fn(|n| {
            let (xp, yp) = pos_scan[n];
            (usize::from(ys) * 4 + usize::from(yp)) * size + usize::from(xs) * 4 + usize::from(xp)
        });
        // A few passes: a change can move the first or last level, which changes the rule.
        for _ in 0..4 {
            let first = (0..16).find(|&n| levels[index[n]] != 0);
            let last = (0..16).rev().find(|&n| levels[index[n]] != 0);
            let (Some(first), Some(last)) = (first, last) else {
                break;
            };
            if last - first <= 3 {
                break;
            }
            let sum: i32 = index.iter().map(|&i| levels[i].abs()).sum();
            let negative = levels[index[first]] < 0;
            if (sum & 1 == 1) == negative {
                break;
            }
            // Cheapest ±1 change of one level (`step` in the direction of its magnitude).
            let mut best = (f32::MAX, 0usize, 0i32);
            for (n, &i) in index.iter().enumerate() {
                let (level, e) = (levels[i], exact[i]);
                let error = e.abs() - level.abs() as f32; // unrounded minus rounded magnitude
                let mut consider = |cost: f32, step: i32| {
                    if cost < best.0 {
                        best = (cost, n, step);
                    }
                };
                if level != 0 {
                    consider(1.0 - 2.0 * error, 1);
                    if level.abs() > 1 {
                        consider(1.0 + 2.0 * error, -1);
                    } else if n != first {
                        consider(1.0 + 2.0 * error - rate_weight, -1);
                    }
                } else if e != 0.0 && n < last {
                    // A new level takes the sign of its coefficient. Below the first level, it
                    // becomes the hidden one: its sign must be the one the new parity gives.
                    if n > first || (e < 0.0) == negative {
                        consider(1.0 - 2.0 * error + rate_weight, 1);
                    }
                }
            }
            let (cost, n, step) = best;
            let i = index[n];
            if cost == f32::MAX {
                // Always possible, and keeps the positions: raise the last level.
                let i = index[last];
                levels[i] += levels[i].signum();
                continue;
            }
            levels[i] = if levels[i] == 0 {
                if exact[i] < 0.0 { -1 } else { 1 }
            } else {
                levels[i] + step * levels[i].signum()
            };
        }
    }
}

/// Whether the decoder will infer a sign in each sub-block, and whether levels match: used by
/// the tests.
#[cfg(test)]
pub(crate) fn signs_consistent(levels: &[i32], log2_size: u8, scan: ScanType) -> bool {
    let size = 1usize << log2_size;
    let pos_scan = scan_order(2, scan);
    scan_order(log2_size - 2, scan).iter().all(|&(xs, ys)| {
        let v: Vec<i32> = (0..16)
            .map(|n| {
                let (xp, yp) = pos_scan[n];
                levels[(usize::from(ys) * 4 + usize::from(yp)) * size
                    + usize::from(xs) * 4
                    + usize::from(xp)]
            })
            .collect();
        let nz: Vec<usize> = (0..16).filter(|&n| v[n] != 0).collect();
        match (nz.first(), nz.last()) {
            (Some(&f), Some(&l)) if l - f > 3 => {
                (v.iter().map(|x| x.abs()).sum::<i32>() & 1 == 1) == (v[f] < 0)
            }
            _ => true,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_hiding_fixes_parity_with_small_changes() {
        let mut state = 7u32;
        for log2 in 2..=5u8 {
            for scan in [ScanType::Diagonal, ScanType::Horizontal, ScanType::Vertical] {
                if log2 > 3 && scan != ScanType::Diagonal {
                    continue;
                }
                for _ in 0..50 {
                    let n = 1usize << (2 * log2);
                    let exact: Vec<f32> = (0..n)
                        .map(|_| {
                            state ^= state << 13;
                            state ^= state >> 17;
                            state ^= state << 5;
                            let v = (state % 1000) as f32 / 100.0 - 5.0;
                            if state % 3 == 0 { 0.0 } else { v }
                        })
                        .collect();
                    let original: Vec<i32> = exact
                        .iter()
                        .map(|&e| (e + 0.5f32.copysign(e) * 0.66) as i32)
                        .collect();
                    let mut levels = original.clone();
                    hide_signs(&mut levels, &exact, log2, scan, 0.3);
                    assert!(signs_consistent(&levels, log2, scan));
                    // At most one change per sub-block (or a few when the rule moved).
                    let changed: i32 = levels
                        .iter()
                        .zip(&original)
                        .map(|(a, b)| (a - b).abs())
                        .sum();
                    assert!(changed as usize <= 4 * (n / 16));
                }
            }
        }
    }
}
