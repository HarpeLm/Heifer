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
            magnitudes: vec![0.0; 1024],
        }
    }
}

/// Coefficient index and position of each scan position (sub-block · 16 + position) of a block.
pub(crate) fn scan_positions(log2_size: u8, scan: ScanType) -> &'static [(u16, u8, u8)] {
    type Tables = [[Vec<(u16, u8, u8)>; 3]; 4];
    static TABLES: std::sync::LazyLock<Tables> = std::sync::LazyLock::new(|| {
        std::array::from_fn(|l| {
            let log2_size = l as u8 + 2;
            let size = 1usize << log2_size;
            [ScanType::Diagonal, ScanType::Horizontal, ScanType::Vertical].map(|scan| {
                let pos_scan = scan_order(2, scan);
                scan_order(log2_size - 2, scan)
                    .iter()
                    .flat_map(|&(xs, ys)| {
                        pos_scan.iter().map(move |&(xp, yp)| {
                            let (x, y) = (xs * 4 + xp, ys * 4 + yp);
                            ((usize::from(y) * size + usize::from(x)) as u16, x, y)
                        })
                    })
                    .collect()
            })
        })
    });
    &TABLES[usize::from(log2_size - 2)][scan as usize]
}

/// [`sig_ctx_inc`] of the 16 scan positions of a sub-block, which depend on the block size, the
/// scan, the component, the coded sub-block flags on the right and below, and whether the
/// sub-block is the first one.
pub(crate) fn sig_ctx_table(
    log2_size: u8,
    scan: ScanType,
    luma: bool,
    prev_csbf: u8,
    first: bool,
) -> &'static [u8; 16] {
    type Tables = Vec<[u8; 16]>;
    static TABLES: std::sync::LazyLock<Tables> = std::sync::LazyLock::new(|| {
        let mut tables = Vec::with_capacity(4 * 3 * 2 * 4 * 2);
        for log2_size in 2..=5u8 {
            for scan in [ScanType::Diagonal, ScanType::Horizontal, ScanType::Vertical] {
                for luma in [false, true] {
                    for prev_csbf in 0..4u8 {
                        for first in [false, true] {
                            // Any sub-block other than the first one gives the same contexts.
                            let xs = usize::from(!first && log2_size > 2);
                            let pos_scan = scan_order(2, scan);
                            tables.push(std::array::from_fn(|pos| {
                                let (xp, yp) = pos_scan[pos];
                                let (x, y) = (xs * 4 + usize::from(xp), usize::from(yp));
                                sig_ctx_inc(luma, log2_size, x, y, xp, yp, prev_csbf, scan) as u8
                            }));
                        }
                    }
                }
            }
        }
        tables
    });
    let index = (((usize::from(log2_size - 2) * 3 + scan as usize) * 2 + usize::from(luma)) * 4
        + usize::from(prev_csbf))
        * 2
        + usize::from(first);
    &TABLES[index]
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
    let log2_sb = log2_size - 2;
    let sb_scan = scan_order(log2_sb, scan);
    let sb_width = 1usize << log2_sb;
    let n = 1usize << (2 * log2_size);
    // Per scan position: chosen magnitude, cost when coded, cost of the sig flag alone, cost
    // of zero (error² plus the sig flag). Every position up to `last_start` is written below
    // before being read.
    let RdoqScratch {
        bits: table,
        chosen,
        cost_level,
        cost_sig1,
        cost_zero,
        zero_after,
        magnitudes,
    } = scratch;
    let positions = scan_positions(log2_size, scan);
    levels[..n].fill(0);
    // The last position worth coding, then the unrounded magnitudes up to it.
    let Some(last_start) = positions[..n]
        .iter()
        .rposition(|&(i, _, _)| exact[usize::from(i)].abs() >= 0.5)
    else {
        return;
    };
    for (m, &(i, _, _)) in magnitudes.iter_mut().zip(&positions[..=last_start]) {
        *m = exact[usize::from(i)].abs();
    }
    let magnitudes = &*magnitudes;
    let ctx_bits = |i: usize, bin: bool| bits(table, &contexts[i], bin);
    let mut coded_sb: u64 = 0;
    let mut greater1_ctx_state: u8 = 1;
    let last_sb = last_start / 16;

    for sb in (0..=last_sb).rev() {
        let (xs, ys) = sb_scan[sb];
        let (xs, ys) = (usize::from(xs), usize::from(ys));
        let right = xs + 1 < sb_width && (coded_sb & (1u64 << ((ys << 3) | (xs + 1)))) != 0;
        let below = ys + 1 < sb_width && (coded_sb & (1u64 << (((ys + 1) << 3) | xs))) != 0;
        let prev_csbf = u8::from(right) | (u8::from(below) << 1);
        let mut ctx_set = if sb == 0 || !luma { 0 } else { 2 };
        if greater1_ctx_state == 0 {
            ctx_set += 1;
        }
        let g1_base = ctx::COEFF_ABS_LEVEL_GREATER1_FLAG + ctx_set * 4 + if luma { 0 } else { 16 };
        let g1_l: [(f32, f32); 4] = core::array::from_fn(|s| {
            let m = &contexts[g1_base + s];
            (
                lambda * bits(table, m, false),
                lambda * bits(table, m, true),
            )
        });
        let g2_model =
            &contexts[ctx::COEFF_ABS_LEVEL_GREATER2_FLAG + ctx_set + if luma { 0 } else { 4 }];
        let g2_0_l = lambda * bits(table, g2_model, false);
        let g2_1_l = lambda * bits(table, g2_model, true);

        let mut state: u8 = 1; // greater1Ctx within the sub-block
        let (mut count, mut seen_g1, mut rice) = (0usize, false, 0u32);
        let sig_ctxs = sig_ctx_table(log2_size, scan, luma, prev_csbf, xs + ys == 0);
        let top = if sb == last_sb { last_start % 16 } else { 15 };
        let sb_base = sb * 16;
        let mut coded_cost = 0.0f32;
        let mut zero_cost = 0.0f32;
        let mut any = false;

        for pos in (0..=top).rev() {
            let g = sb_base + pos;
            let e = magnitudes[g];
            let e_sq = e * e;
            zero_cost += e_sq;
            let is_last = g == last_start;
            let sig_model = &contexts[ctx::SIG_COEFF_FLAG + usize::from(sig_ctxs[pos])];
            let sig0_l = if is_last {
                0.0
            } else {
                lambda * bits(table, sig_model, false)
            };
            let c_zero = e_sq + sig0_l;
            cost_zero[g] = c_zero;

            // Fast path for the ~75% of coefficients below 0.5 (never rounded above 0).
            if e < 0.5 && !is_last {
                cost_sig1[g] = 0.0;
                chosen[g] = 0;
                cost_level[g] = c_zero;
                coded_cost += c_zero;
                continue;
            }

            let sig1_l = if is_last {
                0.0
            } else {
                lambda * bits(table, sig_model, true)
            };
            cost_sig1[g] = sig1_l;

            let rounded = (e + 0.5) as u32;
            let mut best = if is_last {
                (f32::MAX, 0u32)
            } else {
                (c_zero, 0u32)
            };

            if rounded == 1 {
                // Only candidate level is 1: no greater2 or remaining_bits loop needed.
                let rate_l = if count < 8 {
                    lambda + g1_l[usize::from(state.min(3))].0
                } else {
                    lambda * (2 + rice) as f32
                };
                let d = e - 1.0;
                let cost = d * d + sig1_l + rate_l;
                if cost < best.0 {
                    best = (cost, 1);
                }
            } else {
                let g1_pair = g1_l[usize::from(state.min(3))];
                let rate_l = |level: u32| -> f32 {
                    let mut r_l = lambda; // sign
                    if count < 8 {
                        let gt1 = level > 1;
                        r_l += if gt1 { g1_pair.1 } else { g1_pair.0 };
                        let first_g1 = gt1 && !seen_g1;
                        if first_g1 {
                            r_l += if level > 2 { g2_1_l } else { g2_0_l };
                        }
                        let base = 1 + u32::from(gt1) + u32::from(first_g1 && level > 2);
                        let threshold = if first_g1 { 3 } else { 2 };
                        if base == threshold {
                            r_l += lambda * remaining_bits(level - base, rice);
                        }
                    } else {
                        r_l += lambda * remaining_bits(level - 1, rice);
                    }
                    r_l
                };
                for level in [rounded, rounded.saturating_sub(1)] {
                    if level == 0 {
                        continue;
                    }
                    let d = e - level as f32;
                    let cost = d * d + sig1_l + rate_l(level);
                    if cost < best.0 {
                        best = (cost, level);
                    }
                }
                if best.1 == 0 && is_last {
                    let level = 1.max(rounded);
                    let d = e - level as f32;
                    best = (d * d + rate_l(level), level);
                }
            }
            if best.1 == 0 && is_last {
                let rate_l = if count < 8 {
                    lambda + g1_l[usize::from(state.min(3))].0
                } else {
                    lambda * (2 + rice) as f32
                };
                let d = e - 1.0;
                best = (d * d + rate_l, 1);
            }
            chosen[g] = best.1;
            cost_level[g] = best.0;
            coded_cost += best.0;
            if best.1 > 0 {
                any = true;
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
        if sb > 0 && sb < last_sb {
            let inc = usize::from(prev_csbf != 0) + if luma { 0 } else { 2 };
            let flag = ctx::CODED_SUB_BLOCK_FLAG + inc;
            let sb_c = coded_cost + lambda * ctx_bits(flag, true);
            let sb_z = zero_cost + lambda * ctx_bits(flag, false);
            if !any || sb_z < sb_c {
                for g in sb_base..=sb_base + top {
                    chosen[g] = 0;
                    let e = magnitudes[g];
                    cost_level[g] = e * e;
                }
                any = false;
            }
        }
        if any {
            coded_sb |= 1u64 << ((ys << 3) | xs);
        }
    }

    // Last significant position: keep the levels up to it, drop the ones after.
    let (ctx_offset, ctx_shift) = last_ctx(log2_size, luma);
    let c_max = (u32::from(log2_size) << 1) - 1;
    let size = 1usize << log2_size;
    let prefix_bits = |k: usize, bin: bool, i: usize| {
        let base = [ctx::LAST_SIG_COEFF_X_PREFIX, ctx::LAST_SIG_COEFF_Y_PREFIX][k];
        ctx_bits(base + usize::from(ctx_offset) + (i >> ctx_shift), bin)
    };
    let mut first_axis = [0f32; 32];
    let mut second_axis = [0f32; 32];
    for coord in 0..size {
        let (prefix, suffix) = last_prefix(coord as u32);
        let suf = suffix.map_or(0.0, |(_, b)| b as f32);
        let mut r0 = 0.0;
        let mut r1 = 0.0;
        for i in 0..prefix.min(c_max) as usize {
            r0 += prefix_bits(0, true, i);
            r1 += prefix_bits(1, true, i);
        }
        if prefix < c_max {
            r0 += prefix_bits(0, false, prefix as usize);
            r1 += prefix_bits(1, false, prefix as usize);
        }
        first_axis[coord] = r0 + suf;
        second_axis[coord] = r1 + suf;
    }
    let vertical = scan == ScanType::Vertical;
    // Cost of everything up to and including position g when g is the last one: the sum of the
    // chosen costs before it, its own cost without sig flag, the dropped errors after it.
    let mut best: Option<(f32, usize)> = None;
    let mut before = 0.0f32;
    let total_zero_after = {
        zero_after[last_start + 1] = 0.0;
        for g in (0..=last_start).rev() {
            let e = magnitudes[g];
            zero_after[g] = zero_after[g + 1] + e * e;
        }
        &*zero_after
    };
    for g in 0..=last_start {
        if chosen[g] > 0 {
            let (_, x, y) = positions[g];
            let (a, b) = if vertical {
                (usize::from(y), usize::from(x))
            } else {
                (usize::from(x), usize::from(y))
            };
            let cost = before + cost_level[g] - cost_sig1[g]
                + total_zero_after[g + 1]
                + lambda * (first_axis[a] + second_axis[b]);
            if best.is_none_or(|(c, _)| cost < c) {
                best = Some((cost, g));
            }
            before += cost_level[g];
        } else {
            before += cost_zero[g];
        }
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
    for (&(i, _, _), &level) in positions.iter().zip(chosen.iter()).take(last + 1) {
        if level != 0 {
            let i = usize::from(i);
            let level = level as i32;
            levels[i] = if exact[i] < 0.0 { -level } else { level };
        }
    }
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
    for positions in scan_positions(log2_size, scan).chunks_exact(16) {
        let mut v = [0i32; 16];
        let mut first = 16usize;
        let mut last = 0usize;
        let mut sum = 0i32;
        for (n, &(i, _, _)) in positions.iter().enumerate() {
            let lv = levels[usize::from(i)];
            v[n] = lv;
            if lv != 0 {
                if first == 16 {
                    first = n;
                }
                last = n;
                sum += lv.abs();
            }
        }
        if first == 16 || last - first <= 3 {
            continue;
        }
        let mut changed = false;
        // A few passes: a change can move the first or last level, which changes the rule.
        for pass in 0..4 {
            if pass > 0 {
                first = 16;
                last = 0;
                sum = 0;
                for (n, &lv) in v.iter().enumerate() {
                    if lv != 0 {
                        if first == 16 {
                            first = n;
                        }
                        last = n;
                        sum += lv.abs();
                    }
                }
                if first == 16 || last - first <= 3 {
                    break;
                }
            }
            let negative = v[first] < 0;
            if (sum & 1 == 1) == negative {
                break;
            }
            // Cheapest ±1 change of one level (`step` in the direction of its magnitude).
            let mut best = (f32::MAX, 0usize, 0i32);
            for (n, &lv) in v.iter().enumerate() {
                let e = exact[usize::from(positions[n].0)];
                let error = e.abs() - lv.abs() as f32; // unrounded minus rounded magnitude
                if lv != 0 {
                    let c_up = 1.0 - 2.0 * error;
                    if c_up < best.0 {
                        best = (c_up, n, 1);
                    }
                    if lv.abs() > 1 {
                        let c_dn = 1.0 + 2.0 * error;
                        if c_dn < best.0 {
                            best = (c_dn, n, -1);
                        }
                    } else if n != first {
                        let c_dn = 1.0 + 2.0 * error - rate_weight;
                        if c_dn < best.0 {
                            best = (c_dn, n, -1);
                        }
                    }
                } else if e != 0.0 && n < last {
                    // A new level takes the sign of its coefficient. Below the first level, it
                    // becomes the hidden one: its sign must be the one the new parity gives.
                    if n > first || (e < 0.0) == negative {
                        let c_up = 1.0 - 2.0 * error + rate_weight;
                        if c_up < best.0 {
                            best = (c_up, n, 1);
                        }
                    }
                }
            }
            let (cost, n, step) = best;
            if cost == f32::MAX {
                // Always possible, and keeps the positions: raise the last level.
                v[last] += v[last].signum();
            } else if v[n] == 0 {
                let e = exact[usize::from(positions[n].0)];
                v[n] = if e < 0.0 { -1 } else { 1 };
            } else {
                v[n] += step * v[n].signum();
            }
            changed = true;
        }
        if changed {
            for (n, &(i, _, _)) in positions.iter().enumerate() {
                levels[usize::from(i)] = v[n];
            }
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
