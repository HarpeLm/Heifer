//! Sample adaptive offset (H.265 §8.7.3): choice of the offsets of each CTB.
//!
//! SAO corrects the deblocked picture with offsets per CTB and component: per band of sample
//! values (band offset), or per local shape along one direction (edge offset). The encoder
//! measures, on the picture as the decoder reconstructs it before SAO, the error of the samples
//! of each band and each edge category, and keeps the choice of lowest rate-distortion cost.

use heifer_hevc_dec::recon::Frame;
use heifer_hevc_dec::syntax::SaoParams;

/// Error statistics of one CTB component: count and sum of (source − reconstruction) of the
/// samples of each band (32) and of each edge category (4 classes × 4 categories).
struct Stats {
    band: [(i64, i64); 32],
    edge: [[(i64, i64); 4]; 4],
}

/// Gathers the statistics of the CTB area (`x0`, `y0`, `w`, `h`) of a component.
#[allow(clippy::too_many_arguments)]
fn stats(
    rec: &[u16],
    rec_stride: usize,
    src: &[i32],
    src_stride: usize,
    (pw, ph): (usize, usize),
    (x0, y0, w, h): (usize, usize, usize, usize),
    bit_depth: u8,
) -> Stats {
    let mut s = Stats {
        band: [(0, 0); 32],
        edge: [[(0, 0); 4]; 4],
    };
    let shift = bit_depth - 5;
    for y in y0..y0 + h {
        for x in x0..x0 + w {
            let v = i32::from(rec[y * rec_stride + x]);
            let err = i64::from(src[y * src_stride + x] - v);
            let b = &mut s.band[(v >> shift) as usize & 31];
            b.0 += 1;
            b.1 += err;
            for (class, (dx, dy)) in [(1i32, 0i32), (0, 1), (1, 1), (-1, 1)]
                .into_iter()
                .enumerate()
            {
                let (ax, ay) = (x as i32 - dx, y as i32 - dy);
                let (bx, by) = (x as i32 + dx, y as i32 + dy);
                if ax < 0 || ay < 0 || bx < 0 || by < 0 {
                    continue;
                }
                let (ax, ay, bx, by) = (ax as usize, ay as usize, bx as usize, by as usize);
                if ax >= pw || ay >= ph || bx >= pw || by >= ph {
                    continue;
                }
                let a = i32::from(rec[ay * rec_stride + ax]);
                let b = i32::from(rec[by * rec_stride + bx]);
                // edgeIdx 0, 1, 2, 3, 4 → categories 1, 2, none, 3, 4 (§8.7.3.2).
                let category = match 2 + (v - a).signum() + (v - b).signum() {
                    0 => 0,
                    1 => 1,
                    3 => 2,
                    4 => 3,
                    _ => continue,
                };
                let e = &mut s.edge[class][category];
                e.0 += 1;
                e.1 += err;
            }
        }
    }
    s
}

/// Change of the squared error of `n` samples whose errors sum to `e` when `o` is added.
fn delta(n: i64, e: i64, o: i64) -> f64 {
    (n * o * o - 2 * o * e) as f64
}

/// Bits of `sao_offset_abs` (truncated unary, maximum `max`).
fn offset_bits(abs: i64, max: i64) -> f64 {
    (abs + i64::from(abs < max)) as f64
}

/// Best offset for samples of statistics (`n`, `e`) within `range`, with `sign_bit` when the
/// sign is coded (band offset). Returns (offset, cost change including its bits · λ).
fn best_offset(n: i64, e: i64, range: (i64, i64), sign_bit: bool, lambda: f64) -> (i64, f64) {
    let max = range.1.max(-range.0);
    let mut best = (0, lambda * offset_bits(0, max));
    if n == 0 {
        return best;
    }
    let target = ((e as f64) / (n as f64)).round() as i64;
    let target = target.clamp(range.0, range.1);
    // From the unbiased offset towards zero: a smaller offset can cost fewer bits.
    let step = -target.signum();
    let mut o = target;
    while o != 0 {
        let bits = offset_bits(o.abs(), max) + f64::from(u8::from(sign_bit));
        let cost = delta(n, e, o) + lambda * bits;
        if cost < best.1 {
            best = (o, cost);
        }
        o += step;
    }
    best
}

/// One component's choice: type (0 off, 1 band, 2 edge), offsets, band position or class, and
/// its cost (distortion change + λ · bits, excluding the type bits).
#[derive(Clone, Copy)]
struct Choice {
    type_idx: u8,
    offsets: [i16; 4],
    band: u8,
    class: u8,
    cost: f64,
}

const OFF: Choice = Choice {
    type_idx: 0,
    offsets: [0; 4],
    band: 0,
    class: 0,
    cost: 0.0,
};

fn band_choice(s: &Stats, lambda: f64, max: i64) -> Choice {
    let mut best = OFF;
    best.cost = f64::MAX;
    for start in 0..32 {
        let mut offsets = [0i16; 4];
        let mut cost = lambda * 5.0; // sao_band_position
        for (k, offset) in offsets.iter_mut().enumerate() {
            let (n, e) = s.band[(start + k) % 32];
            let (o, c) = best_offset(n, e, (-max, max), true, lambda);
            *offset = o as i16;
            cost += c;
        }
        if cost < best.cost {
            best = Choice {
                type_idx: 1,
                offsets,
                band: start as u8,
                class: 0,
                cost,
            };
        }
    }
    best
}

fn edge_choice(s: &Stats, class: usize, lambda: f64, class_bits: bool, max: i64) -> Choice {
    let mut offsets = [0i16; 4];
    let mut cost = if class_bits { lambda * 2.0 } else { 0.0 }; // sao_eo_class
    for (k, o_out) in offsets.iter_mut().enumerate() {
        let (n, e) = s.edge[class][k];
        let range = if k < 2 { (0, max) } else { (-max, 0) };
        let (o, c) = best_offset(n, e, range, false, lambda);
        *o_out = o as i16;
        cost += c;
    }
    Choice {
        type_idx: 2,
        offsets,
        band: 0,
        class: class as u8,
        cost,
    }
}

/// Cost change of applying `params` (of a neighbouring CTB) to component `c` of this CTB.
fn apply_cost(s: &Stats, params: &SaoParams, c: usize) -> f64 {
    let o = params.offsets[c];
    match params.type_idx[c] {
        1 => (0..4)
            .map(|k| {
                let (n, e) = s.band[(usize::from(params.band_position[c]) + k) % 32];
                delta(n, e, i64::from(o[k]))
            })
            .sum(),
        2 => (0..4)
            .map(|k| {
                let (n, e) = s.edge[usize::from(params.eo_class[c])][k];
                delta(n, e, i64::from(o[k]))
            })
            .sum(),
        _ => 0.0,
    }
}

/// The SAO decision of one CTB: its parameters, or a merge with the CTB on the left or above.
#[derive(Debug, Clone, Copy)]
pub(crate) enum CtbSao {
    MergeLeft,
    MergeUp,
    Own(SaoParams),
}

/// Chooses the SAO parameters of every CTB of a picture, from the picture reconstructed
/// before SAO (`rec`, cropped to `width`×`height`) and the source planes (`src[c]`, of width
/// `src_widths[c]`). Returns them in raster order, with the parameters each one ends up with.
#[allow(clippy::too_many_arguments)]
pub(crate) fn decide(
    rec: &Frame,
    src: [&[i32]; 3],
    src_widths: [usize; 3],
    chroma: bool,
    ctb: usize,
    bit_depth: u8,
    lambda: f64,
) -> Vec<(CtbSao, SaoParams)> {
    let (w, h) = (rec.widths[0] as usize, rec.heights[0] as usize);
    let (cols, rows) = (w.div_ceil(ctb), h.div_ceil(ctb));
    let comps = if chroma { 3 } else { 1 };
    // Largest offset: (1 << (min(bit depth, 10) − 5)) − 1.
    let max = (1i64 << (bit_depth.min(10) - 5)) - 1;
    let mut out: Vec<(CtbSao, SaoParams)> = Vec::with_capacity(cols * rows);
    for cy in 0..rows {
        for cx in 0..cols {
            let all: Vec<Stats> = (0..comps)
                .map(|c| {
                    let (sw, sh) = if c == 0 { (1, 1) } else { (2, 2) };
                    let (pw, ph) = (rec.widths[c] as usize, rec.heights[c] as usize);
                    let (x0, y0) = (cx * ctb / sw, cy * ctb / sh);
                    let (bw, bh) = ((ctb / sw).min(pw - x0), (ctb / sh).min(ph - y0));
                    stats(
                        &rec.planes[c],
                        pw,
                        src[c],
                        src_widths[c],
                        (pw, ph),
                        (x0, y0, bw, bh),
                        bit_depth,
                    )
                })
                .collect();

            // Own parameters: luma alone; Cb and Cr share the type and the edge class.
            let mut p = SaoParams::default();
            let type_bits = |t: u8| if t == 0 { 1.0 } else { 2.0 };
            let mut cost = 0.0;
            {
                let s = &all[0];
                let mut best = OFF;
                best.cost = lambda * type_bits(0);
                let band = band_choice(s, lambda, max);
                if band.cost + lambda * type_bits(1) < best.cost {
                    best = Choice {
                        cost: band.cost + lambda * type_bits(1),
                        ..band
                    };
                }
                for class in 0..4 {
                    let e = edge_choice(s, class, lambda, true, max);
                    if e.cost + lambda * type_bits(2) < best.cost {
                        best = Choice {
                            cost: e.cost + lambda * type_bits(2),
                            ..e
                        };
                    }
                }
                p.type_idx[0] = best.type_idx;
                p.offsets[0] = best.offsets;
                p.band_position[0] = best.band;
                p.eo_class[0] = best.class;
                cost += best.cost;
            }
            if chroma {
                let (sb, sr) = (&all[1], &all[2]);
                let mut best = (lambda * type_bits(0), OFF, OFF);
                let (bb, br) = (band_choice(sb, lambda, max), band_choice(sr, lambda, max));
                let c = bb.cost + br.cost + lambda * type_bits(1);
                if c < best.0 {
                    best = (c, bb, br);
                }
                for class in 0..4 {
                    let (eb, er) = (
                        edge_choice(sb, class, lambda, true, max),
                        edge_choice(sr, class, lambda, false, max),
                    );
                    let c = eb.cost + er.cost + lambda * type_bits(2);
                    if c < best.0 {
                        best = (c, eb, er);
                    }
                }
                for (k, choice) in [(1, best.1), (2, best.2)] {
                    p.type_idx[k] = choice.type_idx;
                    p.offsets[k] = choice.offsets;
                    p.band_position[k] = choice.band;
                    p.eo_class[k] = best.1.class;
                }
                cost += best.0;
            }

            // Merging costs one bin and applies the neighbour's parameters.
            let merge_cost = |params: &SaoParams| -> f64 {
                (0..comps)
                    .map(|c| apply_cost(&all[c], params, c))
                    .sum::<f64>()
                    + lambda
            };
            let mut decision = (
                CtbSao::Own(p),
                p,
                cost + lambda * f64::from(u8::from(cx > 0) + u8::from(cy > 0)),
            );
            if cx > 0 {
                let left = out[out.len() - 1].1;
                let c = merge_cost(&left);
                if c < decision.2 {
                    decision = (CtbSao::MergeLeft, left, c);
                }
            }
            if cy > 0 {
                let up = out[out.len() - cols].1;
                let c = merge_cost(&up) + lambda * f64::from(u8::from(cx > 0));
                if c < decision.2 {
                    decision = (CtbSao::MergeUp, up, c);
                }
            }
            out.push((decision.0, decision.1));
        }
    }
    out
}

/// Writes `sao()` (§7.3.8.3) of one CTB, the mirror of the decoder's parsing.
pub(crate) fn write(
    coder: &mut crate::cabac::ArithmeticEncoder,
    ctx: &mut heifer_hevc_dec::contexts::Contexts,
    decision: &CtbSao,
    (left_ok, up_ok): (bool, bool),
    chroma: bool,
    bit_depth: u8,
) {
    use heifer_hevc_dec::contexts::ctx as c;
    if left_ok {
        let merge = matches!(decision, CtbSao::MergeLeft);
        coder.encode(&mut ctx[c::SAO_MERGE_FLAG], u8::from(merge));
        if merge {
            return;
        }
    }
    if up_ok {
        let merge = matches!(decision, CtbSao::MergeUp);
        coder.encode(&mut ctx[c::SAO_MERGE_FLAG], u8::from(merge));
        if merge {
            return;
        }
    }
    let CtbSao::Own(p) = decision else {
        unreachable!("merges are coded above");
    };
    let max = (1u32 << (bit_depth.min(10) - 5)) - 1;
    for comp in 0..if chroma { 3 } else { 1 } {
        let t = p.type_idx[comp];
        if comp < 2 {
            coder.encode(&mut ctx[c::SAO_TYPE_IDX], u8::from(t != 0));
            if t != 0 {
                coder.encode_bypass(u8::from(t == 2));
            }
        }
        if t == 0 {
            continue;
        }
        for &o in &p.offsets[comp] {
            let a = u32::from(o.unsigned_abs());
            for _ in 0..a {
                coder.encode_bypass(1);
            }
            if a < max {
                coder.encode_bypass(0);
            }
        }
        if t == 1 {
            for &o in &p.offsets[comp] {
                if o != 0 {
                    coder.encode_bypass(u8::from(o < 0));
                }
            }
            coder.encode_bypass_bits(u32::from(p.band_position[comp]), 5);
        } else if comp < 2 {
            coder.encode_bypass_bits(u32::from(p.eo_class[comp]), 2);
        }
    }
}
