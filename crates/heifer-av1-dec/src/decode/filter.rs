//! The loop filters run once all tiles are decoded (AV1 §7.14, §7.15, §7.17): deblocking, CDEF,
//! then loop restoration. Superres is rejected earlier, so the upscaling step is the identity.

use super::{FrameState, LrUnit, round2};
use crate::frame::RestorationType;
use crate::predict::Plane;
use crate::spec_tables::{SGR_PARAMS, TX_HEIGHT, TX_WIDTH};

const SEG_LVL_ALT_LF_Y_V: usize = 1;
const MAX_LOOP_FILTER: i32 = 63;

const CDEF_UV_DIR: [[[usize; 8]; 2]; 2] = [
    [[0, 1, 2, 3, 4, 5, 6, 7], [1, 2, 2, 2, 3, 4, 6, 0]],
    [[7, 0, 2, 4, 5, 6, 6, 6], [0, 1, 2, 3, 4, 5, 6, 7]],
];
const CDEF_DIRECTIONS: [[[isize; 2]; 2]; 8] = [
    [[-1, 1], [-2, 2]],
    [[0, 1], [-1, 2]],
    [[0, 1], [0, 2]],
    [[0, 1], [1, 2]],
    [[1, 1], [2, 2]],
    [[1, 0], [2, 1]],
    [[1, 0], [2, 0]],
    [[1, 0], [2, -1]],
];
const CDEF_PRI_TAPS: [[i32; 2]; 2] = [[4, 2], [3, 3]];
const CDEF_SEC_TAPS: [[i32; 2]; 2] = [[2, 1], [2, 1]];
const DIV_TABLE: [i64; 9] = [0, 840, 420, 280, 210, 168, 140, 120, 105];

const SGRPROJ_RST_BITS: u32 = 4;
const SGRPROJ_PRJ_BITS: u32 = 7;
const SGRPROJ_MTABLE_BITS: u32 = 20;
const SGRPROJ_RECIP_BITS: u32 = 12;
const SGRPROJ_SGR_BITS: u32 = 8;
const FILTER_BITS: u32 = 7;

fn round2_64(x: i64, n: u32) -> i64 {
    if n == 0 { x } else { (x + (1 << (n - 1))) >> n }
}

impl FrameState<'_> {
    /// The filtering steps of `decode_frame_wrapup()`.
    pub(super) fn apply_filters(&mut self) {
        let lf = &self.fh.loop_filter;
        if lf.level[0] != 0 || lf.level[1] != 0 {
            self.deblock();
        }
        let uses_cdef = self.cdef_idx.iter().any(|&i| i != -1);
        let uses_lr = self.lr_units.iter().any(|u| !u.is_empty());
        if !uses_cdef && !uses_lr {
            return;
        }
        let deblocked = self.planes.clone();
        if uses_cdef {
            self.cdef(&deblocked);
        }
        if uses_lr {
            let cdef = self.planes.clone();
            self.restore(&deblocked, &cdef);
        }
    }

    // §7.14: deblocking, in place.

    fn deblock(&mut self) {
        for plane in 0..self.num_planes {
            if plane > 0 && self.fh.loop_filter.level[1 + plane] == 0 {
                continue;
            }
            let (sx, sy) = self.sub(plane);
            for pass in 0..2 {
                for row in (0..self.mi_rows).step_by(1 << sy) {
                    for col in (0..self.mi_cols).step_by(1 << sx) {
                        self.deblock_edge(plane, pass, row, col);
                    }
                }
            }
        }
    }

    /// The edge loop filter process (§7.14.2).
    fn deblock_edge(&mut self, plane: usize, pass: usize, row: usize, col: usize) {
        let (sx, sy) = self.sub(plane);
        let (dx, dy) = if pass == 0 { (1, 0) } else { (0, 1) };
        let (x, y) = (col * 4, row * 4);
        let (row, col) = (row | sy, col | sx);
        if x >= self.fh.frame_width as usize
            || y >= self.fh.frame_height as usize
            || (pass == 0 && x == 0)
            || (pass == 1 && y == 0)
        {
            return;
        }
        let (xp, yp) = (x >> sx, y >> sy);
        let (prev_row, prev_col) = (row - (dy << sy), col - (dx << sx));
        let lf_tx = |r: usize, c: usize| {
            usize::from(self.lf_tx_sizes[plane][(r >> sy) * self.mi_stride + (c >> sx)])
        };
        let (tx, prev_tx) = (lf_tx(row, col), lf_tx(prev_row, prev_col));
        // isBlockEdge and skip only matter for inter blocks.
        let (pos, tx_len, prev_tx_len) = if pass == 0 {
            (xp, TX_WIDTH[tx], TX_WIDTH[prev_tx])
        } else {
            (yp, TX_HEIGHT[tx], TX_HEIGHT[prev_tx])
        };
        if pos % usize::from(tx_len) != 0 {
            return;
        }
        let base = usize::from(tx_len.min(prev_tx_len));
        let filter_size = base.min(if plane == 0 { 16 } else { 8 });
        let mut lvl = self.filter_level(row, col, plane, pass);
        if lvl == 0 {
            lvl = self.filter_level(prev_row, prev_col, plane, pass);
        }
        if lvl == 0 {
            return;
        }
        let sharpness = i32::from(self.fh.loop_filter.sharpness);
        let shift = if sharpness > 4 {
            2
        } else if sharpness > 0 {
            1
        } else {
            0
        };
        let limit = if sharpness > 0 {
            (lvl >> shift).clamp(1, 9 - sharpness)
        } else {
            (lvl >> shift).max(1)
        };
        let strength = Strength {
            limit,
            blimit: 2 * (lvl + 2) + limit,
            thresh: lvl >> 4,
        };
        let bit_depth = self.bit_depth;
        let p = &mut self.planes[plane];
        let step = if pass == 0 { 1 } else { p.stride as isize };
        for i in 0..4 {
            let pos = (yp + dx * i) * p.stride + xp + dy * i;
            filter_samples(
                &mut p.data,
                pos,
                step,
                filter_size,
                plane == 0,
                &strength,
                bit_depth,
            );
        }
    }

    /// The adaptive filter strength selection process (§7.14.4, §7.14.5): `lvl`.
    fn filter_level(&self, row: usize, col: usize, plane: usize, pass: usize) -> i32 {
        let lf = &self.fh.loop_filter;
        let mi = self.mi(row, col);
        let i = if plane == 0 { pass } else { plane + 1 };
        let delta = self.delta_lfs[mi][if self.fh.delta_lf_multi { i } else { 0 }];
        let mut lvl = (i32::from(delta) + i32::from(lf.level[i])).clamp(0, MAX_LOOP_FILTER);
        let seg = &self.fh.segmentation;
        let segment = usize::from(self.segment_ids[mi]);
        if seg.enabled && seg.feature_enabled[segment][SEG_LVL_ALT_LF_Y_V + i] {
            let data = i32::from(seg.feature_data[segment][SEG_LVL_ALT_LF_Y_V + i]);
            lvl = (lvl + data).clamp(0, MAX_LOOP_FILTER);
        }
        if lf.delta_enabled {
            // INTRA_FRAME: the mode deltas only apply to inter blocks.
            let n_shift = lvl >> 5;
            lvl = (lvl + (i32::from(lf.ref_deltas[0]) << n_shift)).clamp(0, MAX_LOOP_FILTER);
        }
        lvl
    }

    // §7.15: CDEF, from the deblocked frame into `planes`.

    fn cdef(&mut self, src: &[Plane]) {
        let stride = self.mi_stride / 16;
        for r in (0..self.mi_rows).step_by(2) {
            for c in (0..self.mi_cols).step_by(2) {
                let idx = self.cdef_idx[(r >> 4) * stride + (c >> 4)];
                if idx == -1 {
                    continue;
                }
                let skip = [(r, c), (r + 1, c), (r, c + 1), (r + 1, c + 1)]
                    .iter()
                    .all(|&(r, c)| self.skips[self.mi(r, c)]);
                if !skip {
                    self.cdef_block(src, r, c, idx as usize);
                }
            }
        }
    }

    /// The CDEF block process (§7.15.1), after the copy.
    fn cdef_block(&mut self, src: &[Plane], r: usize, c: usize, idx: usize) {
        let cdef = self.fh.cdef;
        let coeff_shift = self.bit_depth - 8;
        let (y_dir, var) = cdef_direction(&src[0], r, c, coeff_shift);
        let pri = i32::from(cdef.y_pri_strength[idx]) << coeff_shift;
        let sec = i32::from(cdef.y_sec_strength[idx]) << coeff_shift;
        let dir = if pri == 0 { 0 } else { y_dir };
        let var_str = if var >> 6 != 0 {
            (var >> 6).ilog2().min(12) as i32
        } else {
            0
        };
        let pri = if var != 0 {
            (pri * (4 + var_str) + 8) >> 4
        } else {
            0
        };
        let damping = u32::from(cdef.damping) + coeff_shift;
        self.cdef_filter(src, 0, r, c, [pri, sec], damping, dir);
        if self.num_planes == 1 {
            return;
        }
        let pri = i32::from(cdef.uv_pri_strength[idx]) << coeff_shift;
        let sec = i32::from(cdef.uv_sec_strength[idx]) << coeff_shift;
        let dir = if pri == 0 {
            0
        } else {
            CDEF_UV_DIR[self.sub_x][self.sub_y][y_dir]
        };
        for plane in 1..3 {
            self.cdef_filter(src, plane, r, c, [pri, sec], damping - 1, dir);
        }
    }

    /// The CDEF filter process (§7.15.3).
    #[allow(clippy::too_many_arguments)]
    fn cdef_filter(
        &mut self,
        src: &[Plane],
        plane: usize,
        r: usize,
        c: usize,
        [pri, sec]: [i32; 2],
        damping: u32,
        dir: usize,
    ) {
        let coeff_shift = self.bit_depth - 8;
        let (sx, sy) = self.sub(plane);
        let (x0, y0) = ((c * 4) >> sx, (r * 4) >> sy);
        let (w, h) = (8 >> sx, 8 >> sy);
        let (mi_rows, mi_cols) = (self.mi_rows as isize, self.mi_cols as isize);
        let s = &src[plane];
        // cdef_get_at(): None outside the frame's 4×4 units.
        let at = |y: isize, x: isize| {
            let (cr, cc) = ((y << sy) >> 2, (x << sx) >> 2);
            (cr >= 0 && cr < mi_rows && cc >= 0 && cc < mi_cols)
                .then(|| i32::from(s.data[y as usize * s.stride + x as usize]))
        };
        let taps = ((pri >> coeff_shift) & 1) as usize;
        let mut out = [[0u16; 8]; 8];
        for (i, out_row) in out.iter_mut().enumerate().take(h) {
            for (j, out) in out_row.iter_mut().enumerate().take(w) {
                let (y, x) = ((y0 + i) as isize, (x0 + j) as isize);
                let v = i32::from(s.data[(y0 + i) * s.stride + x0 + j]);
                let (mut sum, mut max, mut min) = (0, v, v);
                for k in 0..2 {
                    for sign in [-1, 1] {
                        let mut tap = |d: usize, strength: i32, weight: i32| {
                            let [oy, ox] = CDEF_DIRECTIONS[d][k];
                            if let Some(p) = at(y + sign * oy, x + sign * ox) {
                                sum += weight * constrain(p - v, strength, damping);
                                max = max.max(p);
                                min = min.min(p);
                            }
                        };
                        tap(dir, pri, CDEF_PRI_TAPS[taps][k]);
                        tap((dir + 6) & 7, sec, CDEF_SEC_TAPS[taps][k]);
                        tap((dir + 2) & 7, sec, CDEF_SEC_TAPS[taps][k]);
                    }
                }
                *out = (v + ((8 + sum - i32::from(sum < 0)) >> 4)).clamp(min, max) as u16;
            }
        }
        let d = &mut self.planes[plane];
        for (i, row) in out.iter().enumerate().take(h) {
            d.data[(y0 + i) * d.stride + x0..][..w].copy_from_slice(&row[..w]);
        }
    }

    // §7.17: loop restoration, from the CDEF output into `planes`.

    fn restore(&mut self, deblocked: &[Plane], cdef: &[Plane]) {
        for plane in 0..self.num_planes {
            if self.fh.restoration.types[plane] == RestorationType::None {
                continue;
            }
            for y in (0..self.fh.frame_height as usize).step_by(4) {
                for x in (0..self.fh.upscaled_width as usize).step_by(4) {
                    self.restore_block(plane, y >> 2, x >> 2, &deblocked[plane], &cdef[plane]);
                }
            }
        }
    }

    /// The loop restore block process (§7.17.1).
    fn restore_block(&mut self, plane: usize, row: usize, col: usize, cur: &Plane, cdef: &Plane) {
        let (sx, sy) = self.sub(plane);
        let stripe_num = (row * 4 + 8) / 64;
        let stripe_start = (-8 + stripe_num as isize * 64) >> sy;
        let unit_size = usize::from(self.fh.restoration.unit_size[plane]);
        let (unit_rows, unit_cols) = self.lr_unit_counts(plane);
        let unit_row = (unit_rows - 1).min(((row * 4 + 8) >> sy) / unit_size);
        let unit_col = (unit_cols - 1).min(((col * 4) >> sx) / unit_size);
        let src = Source {
            cur,
            cdef,
            end_x: round2(self.fh.upscaled_width as i32, sx as u32) as isize - 1,
            end_y: round2(self.fh.frame_height as i32, sy as u32) as isize - 1,
            stripe_start,
            stripe_end: stripe_start + (64 >> sy) - 1,
        };
        let (x, y) = ((col * 4) >> sx, (row * 4) >> sy);
        let w = (4 >> sx).min(src.end_x as usize + 1 - x);
        let h = (4 >> sy).min(src.end_y as usize + 1 - y);
        let unit = self.lr_units[plane][unit_row * unit_cols + unit_col];
        let block = Block { x, y, w, h };
        let out = match unit.kind {
            1 => wiener(&src, &unit, &block, self.bit_depth),
            2 => self_guided(&src, &unit, &block, self.bit_depth),
            _ => return,
        };
        let d = &mut self.planes[plane];
        for (i, row) in out.iter().enumerate().take(h) {
            d.data[(y + i) * d.stride + x..][..w].copy_from_slice(&row[..w]);
        }
    }
}

struct Strength {
    limit: i32,
    blimit: i32,
    thresh: i32,
}

/// The sample filtering process (§7.14.6) across the edge before `pos`; `step` goes from one
/// sample to the next across the edge.
fn filter_samples(
    d: &mut [u16],
    pos: usize,
    step: isize,
    filter_size: usize,
    luma: bool,
    st: &Strength,
    bit_depth: u32,
) {
    let idx = |k: isize| (pos as isize + k * step) as usize;
    let s = |k: isize| i32::from(d[idx(k)]);
    let (q0, q1, p0, p1) = (s(0), s(1), s(-1), s(-2));
    let shift = bit_depth - 8;
    let hev = (p1 - p0).abs() > st.thresh << shift || (q1 - q0).abs() > st.thresh << shift;
    let filter_len = match (filter_size, luma) {
        (4, _) => 4,
        (_, false) => 6,
        (8, _) => 8,
        _ => 16,
    };
    let (limit, blimit) = (st.limit << shift, st.blimit << shift);
    let mut mask = (p1 - p0).abs() > limit
        || (q1 - q0).abs() > limit
        || (p0 - q0).abs() * 2 + (p1 - q1).abs() / 2 > blimit;
    if filter_len >= 6 {
        mask |= (s(-3) - p1).abs() > limit || (s(2) - q1).abs() > limit;
    }
    if filter_len >= 8 {
        mask |= (s(-4) - s(-3)).abs() > limit || (s(3) - s(2)).abs() > limit;
    }
    if mask {
        return;
    }
    let one = 1 << shift;
    // Whether samples 1..n on both sides are within `one` of the ones next to the edge.
    let flat = |range: std::ops::Range<isize>| {
        range
            .clone()
            .all(|k| (s(-1 - k) - p0).abs() <= one && (s(k) - q0).abs() <= one)
    };
    let flat_mask = filter_size >= 8 && flat(1..if filter_len >= 8 { 4 } else { 3 });
    if filter_size == 4 || !flat_mask {
        // The narrow filter process.
        let lo = -(1 << (bit_depth - 1));
        let hi = (1 << (bit_depth - 1)) - 1;
        let c = |v: i32| v.clamp(lo, hi);
        let off = 0x80 << shift;
        let (ps1, ps0, qs0, qs1) = (p1 - off, p0 - off, q0 - off, q1 - off);
        let filter = if hev { c(ps1 - qs1) } else { 0 };
        let filter = c(filter + 3 * (qs0 - ps0));
        let filter1 = c(filter + 4) >> 3;
        let filter2 = c(filter + 3) >> 3;
        d[idx(0)] = (c(qs0 - filter1) + off) as u16;
        d[idx(-1)] = (c(ps0 + filter2) + off) as u16;
        if !hev {
            let filter = round2(filter1, 1);
            d[idx(1)] = (c(qs1 - filter) + off) as u16;
            d[idx(-2)] = (c(ps1 + filter) + off) as u16;
        }
        return;
    }
    let log2 = if filter_size == 8 || !flat(4..7) {
        3
    } else {
        4
    };
    // The wide filter process.
    let n: isize = if log2 == 4 {
        6
    } else if luma {
        3
    } else {
        2
    };
    let n2 = if log2 == 3 && luma { 0 } else { 1 };
    let mut f = [0u16; 12];
    for i in -n..n {
        let t: i32 = (-n..=n)
            .map(|j| s((i + j).clamp(-(n + 1), n)) * if j.abs() <= n2 { 2 } else { 1 })
            .sum();
        f[(i + n) as usize] = round2(t, log2) as u16;
    }
    for i in -n..n {
        d[idx(i)] = f[(i + n) as usize];
    }
}

/// The CDEF direction process (§7.15.2): `yDir` and `var`.
fn cdef_direction(p: &Plane, r: usize, c: usize, coeff_shift: u32) -> (usize, i64) {
    let mut cost = [0i64; 8];
    let mut partial = [[0i64; 15]; 8];
    let (x0, y0) = (c * 4, r * 4);
    for i in 0..8 {
        for j in 0..8 {
            let x = i64::from(p.data[(y0 + i) * p.stride + x0 + j] >> coeff_shift) - 128;
            partial[0][i + j] += x;
            partial[1][i + j / 2] += x;
            partial[2][i] += x;
            partial[3][3 + i - j / 2] += x;
            partial[4][7 + i - j] += x;
            partial[5][3 - i / 2 + j] += x;
            partial[6][j] += x;
            partial[7][i / 2 + j] += x;
        }
    }
    for i in 0..8 {
        cost[2] += partial[2][i] * partial[2][i];
        cost[6] += partial[6][i] * partial[6][i];
    }
    cost[2] *= DIV_TABLE[8];
    cost[6] *= DIV_TABLE[8];
    for i in 0..7 {
        for d in [0, 4] {
            cost[d] += (partial[d][i] * partial[d][i] + partial[d][14 - i] * partial[d][14 - i])
                * DIV_TABLE[i + 1];
        }
    }
    cost[0] += partial[0][7] * partial[0][7] * DIV_TABLE[8];
    cost[4] += partial[4][7] * partial[4][7] * DIV_TABLE[8];
    for i in (1..8).step_by(2) {
        for j in 0..5 {
            cost[i] += partial[i][3 + j] * partial[i][3 + j];
        }
        cost[i] *= DIV_TABLE[8];
        for j in 0..3 {
            cost[i] += (partial[i][j] * partial[i][j] + partial[i][10 - j] * partial[i][10 - j])
                * DIV_TABLE[2 * j + 2];
        }
    }
    let (mut best_cost, mut y_dir) = (0, 0);
    for (i, &c) in cost.iter().enumerate() {
        if c > best_cost {
            best_cost = c;
            y_dir = i;
        }
    }
    (y_dir, (best_cost - cost[(y_dir + 4) & 7]) >> 10)
}

fn constrain(diff: i32, threshold: i32, damping: u32) -> i32 {
    if threshold == 0 {
        return 0;
    }
    let adj = damping.saturating_sub(threshold.ilog2());
    let v = (threshold - (diff.abs() >> adj)).clamp(0, diff.abs());
    if diff < 0 { -v } else { v }
}

/// What the get source sample process (§7.17.6) reads from.
struct Source<'a> {
    cur: &'a Plane,
    cdef: &'a Plane,
    end_x: isize,
    end_y: isize,
    stripe_start: isize,
    stripe_end: isize,
}

impl Source<'_> {
    fn at(&self, x: isize, y: isize) -> i32 {
        let x = x.clamp(0, self.end_x) as usize;
        let y = y.clamp(0, self.end_y);
        let (p, y) = if y < self.stripe_start {
            (self.cur, y.max(self.stripe_start - 2))
        } else if y > self.stripe_end {
            (self.cur, y.min(self.stripe_end + 2))
        } else {
            (self.cdef, y)
        };
        i32::from(p.data[y as usize * p.stride + x])
    }
}

/// A block of at most 4×4 samples of one plane.
struct Block {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

/// The Wiener filter process (§7.17.4).
fn wiener(src: &Source<'_>, unit: &LrUnit, b: &Block, bit_depth: u32) -> [[u16; 4]; 4] {
    // The rounding variables with isCompound equal to 0.
    let (round0, round1) = if bit_depth == 12 { (5, 9) } else { (3, 11) };
    let coefficients = |c: [i32; 3]| {
        let mut f = [0; 7];
        f[3] = 128;
        for i in 0..3 {
            f[i] = c[i];
            f[6 - i] = c[i];
            f[3] -= 2 * c[i];
        }
        f
    };
    let (vfilter, hfilter) = (coefficients(unit.wiener[0]), coefficients(unit.wiener[1]));
    let offset = 1 << (bit_depth + FILTER_BITS - round0 - 1);
    let limit = (1 << (bit_depth + 1 + FILTER_BITS - round0)) - 1;
    let (x, y) = (b.x as isize, b.y as isize);
    let mut inter = [[0i32; 4]; 10];
    for (r, row) in inter.iter_mut().enumerate().take(b.h + 6) {
        for (c, v) in row.iter_mut().enumerate().take(b.w) {
            let s: i32 = (0..7)
                .map(|t| hfilter[t] * src.at(x + (c + t) as isize - 3, y + r as isize - 3))
                .sum();
            *v = round2(s, round0).clamp(-offset, limit - offset);
        }
    }
    let max = (1 << bit_depth) - 1;
    let mut out = [[0u16; 4]; 4];
    for r in 0..b.h {
        for c in 0..b.w {
            let s: i32 = (0..7).map(|t| vfilter[t] * inter[r + t][c]).sum();
            out[r][c] = round2(s, round1).clamp(0, max) as u16;
        }
    }
    out
}

/// The self guided filter process (§7.17.2).
fn self_guided(src: &Source<'_>, unit: &LrUnit, b: &Block, bit_depth: u32) -> [[u16; 4]; 4] {
    let set = usize::from(unit.sgr_set);
    let flt0 = box_filter(src, b, set, 0, bit_depth);
    let flt1 = box_filter(src, b, set, 1, bit_depth);
    let [w0, w1] = unit.sgr_xqd.map(i64::from);
    let w2 = (1 << SGRPROJ_PRJ_BITS) - w0 - w1;
    let (r0, r1) = (SGR_PARAMS[set][0], SGR_PARAMS[set][2]);
    let max = (1 << bit_depth) - 1;
    let mut out = [[0u16; 4]; 4];
    for i in 0..b.h {
        for j in 0..b.w {
            let u =
                i64::from(src.cdef.data[(b.y + i) * src.cdef.stride + b.x + j]) << SGRPROJ_RST_BITS;
            let mut v = w1 * u;
            v += w0 * if r0 != 0 { flt0[i][j] } else { u };
            v += w2 * if r1 != 0 { flt1[i][j] } else { u };
            out[i][j] = round2_64(v, SGRPROJ_RST_BITS + SGRPROJ_PRJ_BITS).clamp(0, max) as u16;
        }
    }
    out
}

/// The box filter process (§7.17.3); zeros when the pass is unused.
fn box_filter(
    src: &Source<'_>,
    b: &Block,
    set: usize,
    pass: usize,
    bit_depth: u32,
) -> [[i64; 4]; 4] {
    let mut f = [[0i64; 4]; 4];
    let r = isize::from(SGR_PARAMS[set][pass * 2]);
    if r == 0 {
        return f;
    }
    let eps = i64::from(SGR_PARAMS[set][pass * 2 + 1]);
    let n = ((2 * r + 1) * (2 * r + 1)) as i64;
    let n2e = n * n * eps;
    let s = ((1 << SGRPROJ_MTABLE_BITS) + n2e / 2) / n2e;
    let one_over_n = ((1 << SGRPROJ_RECIP_BITS) + n / 2) / n;
    // A and B, with one extra sample on each side.
    let mut aa = [[0i64; 6]; 6];
    let mut bb = [[0i64; 6]; 6];
    let (x, y) = (b.x as isize, b.y as isize);
    for i in 0..b.h + 2 {
        for j in 0..b.w + 2 {
            let (mut a, mut sum) = (0i64, 0i64);
            for dy in -r..=r {
                for dx in -r..=r {
                    let c = i64::from(src.at(x + j as isize - 1 + dx, y + i as isize - 1 + dy));
                    a += c * c;
                    sum += c;
                }
            }
            let a = round2_64(a, 2 * (bit_depth - 8));
            let d = round2_64(sum, bit_depth - 8);
            let p = (a * n - d * d).max(0);
            let z = round2_64(p * s, SGRPROJ_MTABLE_BITS);
            let a2 = if z >= 255 {
                256
            } else if z == 0 {
                1
            } else {
                ((z << SGRPROJ_SGR_BITS) + z / 2) / (z + 1)
            };
            let b2 = ((1 << SGRPROJ_SGR_BITS) - a2) * sum * one_over_n;
            aa[i][j] = a2;
            bb[i][j] = round2_64(b2, SGRPROJ_RECIP_BITS);
        }
    }
    for i in 0..b.h {
        let shift = if pass == 0 && i & 1 == 1 { 4 } else { 5 };
        for j in 0..b.w {
            let (mut a, mut sum) = (0i64, 0i64);
            for dy in 0..3 {
                for dx in 0..3 {
                    let weight = if pass == 0 {
                        // (i + dy - 1) is odd.
                        if (i + dy) & 1 == 0 {
                            if dx == 1 { 6 } else { 5 }
                        } else {
                            0
                        }
                    } else if dx == 1 || dy == 1 {
                        4
                    } else {
                        3
                    };
                    a += weight * aa[i + dy][j + dx];
                    sum += weight * bb[i + dy][j + dx];
                }
            }
            let v = a * i64::from(src.cdef.data[(b.y + i) * src.cdef.stride + b.x + j]) + sum;
            f[i][j] = round2_64(v, SGRPROJ_SGR_BITS + shift - SGRPROJ_RST_BITS);
        }
    }
    f
}
