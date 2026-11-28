//! Inter prediction (§7.11.3): motion vector scaling, translation and warp filters, compound
//! masks, inter-intra blending and overlapped motion compensation.

use std::sync::OnceLock;

use super::inter::*;
use super::*;
use crate::frame::relative_dist;

fn round2_64(x: i64, n: u32) -> i64 {
    if n == 0 { x } else { (x + (1 << (n - 1))) >> n }
}

fn round2_signed_64(x: i64, n: u32) -> i64 {
    if x >= 0 {
        round2_64(x, n)
    } else {
        -round2_64(-x, n)
    }
}

fn round2_signed_128(x: i128, n: u32) -> i128 {
    let r = |x: i128| if n == 0 { x } else { (x + (1 << (n - 1))) >> n };
    if x >= 0 { r(x) } else { -r(-x) }
}

/// The resolve divisor process (§7.11.3.7): `(divShift, divFactor)`.
fn resolve_divisor(d: i64) -> (u32, i64) {
    let a = d.unsigned_abs();
    let n = 63 - a.leading_zeros();
    let e = (a - (1 << n)) as i64;
    let f = if n > 8 {
        round2_64(e, n - 8)
    } else {
        e << (8 - n)
    };
    let factor = i64::from(DIV_LUT[f as usize]);
    (n + 14, if d < 0 { -factor } else { factor })
}

/// The setup shear process (§7.11.3.6): `alpha`, `beta`, `gamma`, `delta`, when `warpValid`.
fn setup_shear(p: &[i32; 6]) -> Option<[i32; 4]> {
    if p[2] == 0 {
        return None;
    }
    let clamp16 = |v: i64| v.clamp(-32768, 32767);
    let alpha0 = clamp16(i64::from(p[2]) - (1 << 16));
    let beta0 = clamp16(i64::from(p[3]));
    let (shift, factor) = resolve_divisor(i64::from(p[2]));
    let v = i64::from(p[4]) << 16;
    let gamma0 = clamp16(round2_signed_64(v * factor, shift));
    let w = i64::from(p[3]) * i64::from(p[4]);
    let delta0 = clamp16(i64::from(p[5]) - round2_signed_64(w * factor, shift) - (1 << 16));
    let reduce = |v: i64| (round2_signed_64(v, 6) << 6) as i32;
    let s = [
        reduce(alpha0),
        reduce(beta0),
        reduce(gamma0),
        reduce(delta0),
    ];
    let valid =
        4 * s[0].abs() + 7 * s[1].abs() < 1 << 16 && 4 * s[2].abs() + 4 * s[3].abs() < 1 << 16;
    valid.then_some(s)
}

/// A reference plane and its last valid coordinates.
struct RefPlane<'a> {
    data: &'a [u16],
    stride: usize,
    last_x: i32,
    last_y: i32,
}

impl<'a> RefPlane<'a> {
    fn new(rf: &'a RefFrame, plane: usize, (sx, sy): (usize, usize)) -> Self {
        let w = (rf.upscaled_width as usize + sx) >> sx;
        let h = (rf.frame_height as usize + sy) >> sy;
        RefPlane {
            data: &rf.picture.planes[plane],
            stride: w,
            last_x: w as i32 - 1,
            last_y: h as i32 - 1,
        }
    }

    fn at(&self, x: i64, y: i64) -> i32 {
        let x = x.clamp(0, i64::from(self.last_x)) as usize;
        let y = y.clamp(0, i64::from(self.last_y)) as usize;
        i32::from(self.data[y * self.stride + x])
    }
}

/// The block warp process (§7.11.3.5) over a whole `w`×`h` block, 8×8 samples at a time.
#[allow(clippy::too_many_arguments)]
fn block_warp(
    src: &RefPlane<'_>,
    (sx, sy): (usize, usize),
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    p: &[i32; 6],
    rounds: [u32; 2],
) -> Vec<i32> {
    // Only called with parameters that passed the setup shear process.
    let [alpha, beta, gamma, delta] = setup_shear(p).unwrap_or_default();
    let round2 = |v: i32, n: u32| (v + (1 << (n - 1))) >> n;
    let mut pred = vec![0; w * h];
    for i8 in 0..=(h - 1) >> 3 {
        for j8 in 0..=(w - 1) >> 3 {
            let src_x = (
                ((x + j8 * 8 + 4) << sx) as i64,
                ((y + i8 * 8 + 4) << sy) as i64,
            );
            let dst_x = i64::from(p[2]) * src_x.0 + i64::from(p[3]) * src_x.1 + i64::from(p[0]);
            let dst_y = i64::from(p[4]) * src_x.0 + i64::from(p[5]) * src_x.1 + i64::from(p[1]);
            let (x4, y4) = (dst_x >> sx, dst_y >> sy);
            let (ix4, sx4) = (x4 >> 16, (x4 & 0xffff) as i32);
            let (iy4, sy4) = (y4 >> 16, (y4 & 0xffff) as i32);
            let mut inter = [[0i32; 8]; 15];
            for i1 in -7i32..8 {
                for i2 in -4i32..4 {
                    let s = sx4 + alpha * i2 + beta * i1;
                    let taps = &WARPED_FILTERS[(round2(s, 10) + 64) as usize];
                    let sum: i32 = (0..8)
                        .map(|t| {
                            i32::from(taps[t])
                                * src.at(ix4 + i64::from(i2) - 3 + t as i64, iy4 + i64::from(i1))
                        })
                        .sum();
                    inter[(i1 + 7) as usize][(i2 + 4) as usize] = round2(sum, rounds[0]);
                }
            }
            let rows = 4.min(h as i32 - i8 as i32 * 8 - 4);
            let cols = 4.min(w as i32 - j8 as i32 * 8 - 4);
            for i1 in -4..rows {
                for i2 in -4..cols {
                    let s = sy4 + gamma * i2 + delta * i1;
                    let taps = &WARPED_FILTERS[(round2(s, 10) + 64) as usize];
                    let sum: i32 = (0..8)
                        .map(|t| {
                            i32::from(taps[t])
                                * inter[(i1 + t as i32 + 4) as usize][(i2 + 4) as usize]
                        })
                        .sum();
                    let (r, c) = (
                        (i8 as i32 * 8 + i1 + 4) as usize,
                        (j8 as i32 * 8 + i2 + 4) as usize,
                    );
                    pred[r * w + c] = round2(sum, rounds[1]);
                }
            }
        }
    }
    pred
}

/// `MasterMask` of `initialise_wedge_mask_table()`, per wedge direction.
fn master_masks() -> &'static [[[u8; 64]; 64]; 6] {
    const HORIZONTAL: usize = 0;
    const VERTICAL: usize = 1;
    const OBLIQUE27: usize = 2;
    const OBLIQUE63: usize = 3;
    const OBLIQUE117: usize = 4;
    const OBLIQUE153: usize = 5;
    static MASKS: OnceLock<Box<[[[u8; 64]; 64]; 6]>> = OnceLock::new();
    MASKS.get_or_init(|| {
        let mut m = Box::new([[[0u8; 64]; 64]; 6]);
        let at = |t: &[u8; 64], i: isize| t[i.clamp(0, 63) as usize];
        for j in 0..64 {
            let mut shift = 16;
            for i in (0..64).step_by(2) {
                m[OBLIQUE63][i][j] = at(&WEDGE_MASTER_OBLIQUE_EVEN, j as isize - shift);
                shift -= 1;
                m[OBLIQUE63][i + 1][j] = at(&WEDGE_MASTER_OBLIQUE_ODD, j as isize - shift);
                m[VERTICAL][i][j] = WEDGE_MASTER_VERTICAL[j];
                m[VERTICAL][i + 1][j] = WEDGE_MASTER_VERTICAL[j];
            }
        }
        for i in 0..64 {
            for j in 0..64 {
                let msk = m[OBLIQUE63][i][j];
                m[OBLIQUE27][j][i] = msk;
                m[OBLIQUE117][i][63 - j] = 64 - msk;
                m[OBLIQUE153][63 - j][i] = 64 - msk;
                m[HORIZONTAL][j][i] = m[VERTICAL][i][j];
            }
        }
        m
    })
}

/// `WedgeMasks[ size ][ sign ][ index ]`.
fn wedge_mask(size: usize, sign: usize, index: usize) -> Vec<u8> {
    let w4 = NUM_4X4_BLOCKS_WIDE[size];
    let h4 = NUM_4X4_BLOCKS_HIGH[size];
    let shape = match h4.cmp(&w4) {
        std::cmp::Ordering::Greater => 0,
        std::cmp::Ordering::Less => 1,
        std::cmp::Ordering::Equal => 2,
    };
    let (w, h) = (4 * usize::from(w4), 4 * usize::from(h4));
    let [dir, xo, yo] = WEDGE_CODEBOOK[shape][index].map(usize::from);
    let (xoff, yoff) = (32 - ((xo * w) >> 3), 32 - ((yo * h) >> 3));
    let m = &master_masks()[dir];
    let sum: usize = (0..w)
        .map(|i| usize::from(m[yoff][xoff + i]))
        .sum::<usize>()
        + (1..h)
            .map(|i| usize::from(m[yoff + i][xoff]))
            .sum::<usize>();
    let avg = (sum + (w + h - 1) / 2) / (w + h - 1);
    let flip = usize::from(avg < 32);
    let mut mask = Vec::with_capacity(w * h);
    for i in 0..h {
        for j in 0..w {
            let v = m[yoff + i][xoff + j];
            mask.push(if sign == flip { v } else { 64 - v });
        }
    }
    mask
}

fn obmc_mask(len: usize) -> &'static [u8] {
    match len {
        2 => &OBMC_MASK_2,
        4 => &OBMC_MASK_4,
        8 => &OBMC_MASK_8,
        16 => &OBMC_MASK_16,
        _ => &OBMC_MASK_32,
    }
}

impl FrameState<'_> {
    fn reference(&self, r: i8) -> Result<&RefFrame, Error> {
        self.refs
            .get(r as usize)
            .and_then(|r| r.as_deref())
            .ok_or(Error::Invalid("reference to an empty slot"))
    }

    /// `xScale` and `yScale` of a reference frame.
    fn ref_scale(&self, rf: &RefFrame) -> (i64, i64) {
        let (fw, fh) = (
            i64::from(self.fh.frame_width),
            i64::from(self.fh.frame_height),
        );
        (
            ((i64::from(rf.upscaled_width) << 14) + fw / 2) / fw,
            ((i64::from(rf.frame_height) << 14) + fh / 2) / fh,
        )
    }

    /// `is_scaled( refFrame )`.
    pub(super) fn is_scaled(&self, r: i8) -> bool {
        self.reference(r)
            .is_ok_and(|rf| self.ref_scale(rf) != (1 << 14, 1 << 14))
    }

    /// The motion vector scaling process (§7.11.3.3): `[startX, startY, stepX, stepY]`.
    fn scale_mv(
        &self,
        rf: &RefFrame,
        plane: usize,
        x: usize,
        y: usize,
        mv: [i32; 2],
    ) -> Result<[i32; 4], Error> {
        let (fw, fh) = (
            i64::from(self.fh.frame_width),
            i64::from(self.fh.frame_height),
        );
        let (rw, rh) = (i64::from(rf.upscaled_width), i64::from(rf.frame_height));
        if 2 * fw < rw || 2 * fh < rh || fw > 16 * rw || fh > 16 * rh {
            return Err(Error::Invalid("reference frame size"));
        }
        let (x_scale, y_scale) = self.ref_scale(rf);
        let (sx, sy) = self.sub(plane);
        let orig_x = ((x as i64) << 4) + ((2 * i64::from(mv[1])) >> sx) + 8;
        let orig_y = ((y as i64) << 4) + ((2 * i64::from(mv[0])) >> sy) + 8;
        let start_x = round2_signed_64(orig_x * x_scale - (8 << 14), 8) + 32;
        let start_y = round2_signed_64(orig_y * y_scale - (8 << 14), 8) + 32;
        Ok([
            start_x as i32,
            start_y as i32,
            round2_signed_64(x_scale, 4) as i32,
            round2_signed_64(y_scale, 4) as i32,
        ])
    }

    /// The block inter prediction of `w`×`h` samples at `x`, `y` of `plane` from reference `r`
    /// with motion vector `mv` and the filters of unit `cand`.
    #[allow(clippy::too_many_arguments)]
    fn block_inter(
        &self,
        r: i8,
        plane: usize,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        mv: [i32; 2],
        cand: usize,
        rounds: [u32; 2],
    ) -> Result<Vec<i32>, Error> {
        let rf = self.reference(r)?;
        let [start_x, start_y, step_x, step_y] = self.scale_mv(rf, plane, x, y, mv)?;
        let src = RefPlane::new(rf, plane, self.sub(plane));
        Ok(predict::block_inter(
            src.data,
            src.stride,
            src.last_x,
            src.last_y,
            start_x,
            start_y,
            step_x,
            step_y,
            w,
            h,
            self.interp_filters[cand].map(usize::from),
            rounds,
        ))
    }
}

impl TileDecoder<'_, '_> {
    /// `compute_prediction()` of an inter block (of an inter frame).
    pub(super) fn predict_inter_block(&mut self) -> Result<(), Error> {
        let (row, col, size) = (self.b.row, self.b.col, self.b.size);
        let sb_mask = if self.f.seq.use_128x128_superblock {
            31
        } else {
            15
        };
        for plane in 0..1 + 2 * usize::from(self.b.has_chroma) {
            let plane_size = self.residual_size(size, plane);
            let n4w = usize::from(NUM_4X4_BLOCKS_WIDE[plane_size]);
            let n4h = usize::from(NUM_4X4_BLOCKS_HIGH[plane_size]);
            let (sx, sy) = self.plane_sub(plane);
            let (base_x, base_y) = ((col >> sx) * 4, (row >> sy) * 4);
            if self.b.ref_frame[1] == INTRA_FRAME {
                // The intra half of an inter-intra block.
                let (sr, sc) = ((row & sb_mask) >> sy, (col & sb_mask) >> sx);
                let flags = &self.block_decoded[plane];
                let b = &self.b;
                let intra = IntraBlock {
                    x: base_x,
                    y: base_y,
                    log2w: 2 + u32::from(MI_WIDTH_LOG2[plane_size]),
                    log2h: 2 + u32::from(MI_HEIGHT_LOG2[plane_size]),
                    have_left: if plane == 0 {
                        b.avail_l
                    } else {
                        b.avail_l_chroma
                    },
                    have_above: if plane == 0 {
                        b.avail_u
                    } else {
                        b.avail_u_chroma
                    },
                    have_above_right: flags[sr][sc + n4w + 1],
                    have_below_left: flags[sr + n4h + 1][sc],
                    // II_DC_PRED, II_V_PRED, II_H_PRED, II_SMOOTH_PRED.
                    mode: [DC_PRED, 1, 2, SMOOTH_PRED][usize::from(b.interintra_mode)],
                    angle_delta: 0,
                    filter_intra_mode: None,
                    edge_filter: self.f.seq.enable_intra_edge_filter,
                    smooth_neighbour: self.f.seq.enable_intra_edge_filter
                        && self.filter_type(plane),
                    max_x: ((self.f.mi_cols * 4) >> sx) - 1,
                    max_y: ((self.f.mi_rows * 4) >> sy) - 1,
                    bit_depth: self.f.bit_depth,
                };
                predict::predict_intra(&mut self.f.planes[plane], &intra);
            }
            let (mut cand_row, mut cand_col) = ((row >> sy) << sy, (col >> sx) << sx);
            let (mut pred_w, mut pred_h) = ((4 * self.b.bw4) >> sx, (4 * self.b.bh4) >> sy);
            let some_use_intra = (0..n4h << sy).any(|r| {
                (0..n4w << sx).any(|c| {
                    self.f.ref_frames[self.f.mi(cand_row + r, cand_col + c)][0] == INTRA_FRAME
                })
            });
            if some_use_intra {
                (pred_w, pred_h) = (n4w * 4, n4h * 4);
                (cand_row, cand_col) = (row, col);
            }
            for (r, y) in (0..n4h * 4).step_by(pred_h).enumerate() {
                for (c, x) in (0..n4w * 4).step_by(pred_w).enumerate() {
                    self.predict_inter(
                        plane,
                        base_x + x,
                        base_y + y,
                        pred_w,
                        pred_h,
                        cand_row + r,
                        cand_col + c,
                    )?;
                }
            }
        }
        Ok(())
    }

    /// The inter prediction process (§7.11.3.1).
    #[allow(clippy::too_many_arguments)]
    fn predict_inter(
        &mut self,
        plane: usize,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        cand_row: usize,
        cand_col: usize,
    ) -> Result<(), Error> {
        let cand = self.f.mi(cand_row, cand_col);
        let refs = self.f.ref_frames[cand];
        let compound = refs[1] > INTRA_FRAME;
        let bd = self.f.bit_depth;
        // The rounding variables derivation process.
        let rounds = match (bd == 12, compound) {
            (false, false) => [3, 11],
            (false, true) => [3, 7],
            (true, false) => [5, 9],
            (true, true) => [5, 7],
        };
        let post = 14 - (rounds[0] + rounds[1]);
        let fh = self.f.fh;
        if plane == 0 && self.b.motion_mode == LOCALWARP {
            self.b.local_warp = self.warp_estimation().filter(|p| setup_shear(p).is_some());
        }
        let global = self.b.y_mode == GLOBALMV || self.b.y_mode == GLOBAL_GLOBALMV;
        let sub = self.plane_sub(plane);
        let mut preds = [Vec::new(), Vec::new()];
        for list in 0..1 + usize::from(compound) {
            let rf = refs[list];
            let gm = &fh.gm_params[rf as usize];
            let gm_warp = global && fh.gm_types[rf as usize] > TRANSLATION;
            let warp = if w < 8 || h < 8 || fh.force_integer_mv {
                None
            } else if self.b.motion_mode == LOCALWARP && self.b.local_warp.is_some() {
                self.b.local_warp
            } else if gm_warp && !self.f.is_scaled(rf) && setup_shear(gm).is_some() {
                Some(*gm)
            } else {
                None
            };
            preds[list] = match warp {
                Some(p) => {
                    let src = RefPlane::new(self.f.reference(rf)?, plane, sub);
                    block_warp(&src, sub, x, y, w, h, &p, rounds)
                }
                None => {
                    let mv = self.f.mvs[cand][list];
                    self.f
                        .block_inter(rf, plane, x, y, w, h, mv, cand, rounds)?
                }
            };
        }
        let b = &self.b;
        let interintra = b.ref_frame[1] == INTRA_FRAME;
        if b.compound_type == COMPOUND_WEDGE && plane == 0 {
            self.mask = wedge_mask(b.size, b.wedge_sign, b.wedge_index);
            self.mask_w = w;
        } else if b.compound_type == COMPOUND_INTRA {
            let scale = 128 / w.max(h);
            let mode = b.interintra_mode;
            self.mask = (0..h)
                .flat_map(|i| {
                    (0..w).map(move |j| match mode {
                        1 => II_WEIGHTS_1D[i * scale],
                        2 => II_WEIGHTS_1D[j * scale],
                        3 => II_WEIGHTS_1D[i.min(j) * scale],
                        _ => 32,
                    })
                })
                .collect();
            self.mask_w = w;
        } else if b.compound_type == COMPOUND_DIFFWTD && plane == 0 {
            let shift = (bd - 8) + post;
            let mask_type = b.mask_type;
            self.mask = preds[0]
                .iter()
                .zip(&preds[1])
                .map(|(p0, p1)| {
                    let diff = round2((p0 - p1).abs(), shift);
                    let m = (38 + diff / 16).clamp(0, 64) as u8;
                    if mask_type { 64 - m } else { m }
                })
                .collect();
            self.mask_w = w;
        }
        let max = (1 << bd) - 1;
        let out = &mut self.f.planes[plane];
        if !compound && !interintra {
            for i in 0..h {
                for j in 0..w {
                    out.set(x + j, y + i, preds[0][i * w + j].clamp(0, max));
                }
            }
        } else if b.compound_type == COMPOUND_AVERAGE || b.compound_type == COMPOUND_DISTANCE {
            let (fwd, bck) = if b.compound_type == COMPOUND_DISTANCE {
                self.distance_weights(cand)
            } else {
                (1, 1)
            };
            let shift = post
                + if b.compound_type == COMPOUND_DISTANCE {
                    4
                } else {
                    1
                };
            let out = &mut self.f.planes[plane];
            for i in 0..h {
                for j in 0..w {
                    let v = fwd * preds[0][i * w + j] + bck * preds[1][i * w + j];
                    out.set(x + j, y + i, round2(v, shift).clamp(0, max));
                }
            }
        } else {
            // The mask blend process (§7.11.3.14).
            let (sx, sy) = sub;
            let per_plane = interintra && !b.wedge_interintra;
            let (mask, mw) = (&self.mask, self.mask_w);
            for i in 0..h {
                for j in 0..w {
                    let m = |r: usize, c: usize| i32::from(mask[r * mw + c]);
                    let m = if (sx == 0 && sy == 0) || per_plane {
                        m(i, j)
                    } else if sy == 0 {
                        round2(m(i, 2 * j) + m(i, 2 * j + 1), 1)
                    } else {
                        round2(
                            m(2 * i, 2 * j)
                                + m(2 * i, 2 * j + 1)
                                + m(2 * i + 1, 2 * j)
                                + m(2 * i + 1, 2 * j + 1),
                            2,
                        )
                    };
                    let p = i * w + j;
                    let v = if interintra {
                        let pred0 = round2(preds[0][p], post).clamp(0, max);
                        let pred1 = out.at(x + j, y + i);
                        round2(m * pred1 + (64 - m) * pred0, 6)
                    } else {
                        round2(m * preds[0][p] + (64 - m) * preds[1][p], 6 + post).clamp(0, max)
                    };
                    out.set(x + j, y + i, v);
                }
            }
        }
        if self.b.motion_mode == OBMC {
            self.overlapped(plane, w, h)?;
        }
        Ok(())
    }

    /// The distance weights process (§7.11.3.15): `(FwdWeight, BckWeight)`.
    fn distance_weights(&self, cand: usize) -> (i32, i32) {
        let refs = self.f.ref_frames[cand];
        let fh = self.f.fh;
        let dist = |l: usize| {
            relative_dist(self.f.seq, fh.order_hints[refs[l] as usize], fh.order_hint)
                .abs()
                .clamp(0, 31)
        };
        let (d0, d1) = (dist(1), dist(0));
        let order = usize::from(d0 <= d1);
        let i = if d0 == 0 || d1 == 0 {
            3
        } else {
            (0..3)
                .find(|&i| {
                    let c0 = i32::from(QUANT_DIST_WEIGHT[i][order]);
                    let c1 = i32::from(QUANT_DIST_WEIGHT[i][1 - order]);
                    if order == 1 {
                        d0 * c0 > d1 * c1
                    } else {
                        d0 * c0 < d1 * c1
                    }
                })
                .unwrap_or(3)
        };
        (
            i32::from(QUANT_DIST_LOOKUP[i][order]),
            i32::from(QUANT_DIST_LOOKUP[i][1 - order]),
        )
    }

    /// The warp estimation process (§7.11.3.8): `LocalWarpParams` when `LocalValid`.
    fn warp_estimation(&self) -> Option<[i32; 6]> {
        let b = &self.b;
        let mut a = [[0i64; 2]; 2];
        let (mut bx, mut by) = ([0i64; 2], [0i64; 2]);
        let mid_y = (b.row * 4 + b.bh4 * 2) as i64 - 1;
        let mid_x = (b.col * 4 + b.bw4 * 2) as i64 - 1;
        let (suy, sux) = (mid_y * 8, mid_x * 8);
        let (duy, dux) = (suy + i64::from(b.mv[0][0]), sux + i64::from(b.mv[0][1]));
        let ls = |a: i64, b: i64| ((a * b) >> 2) + (a + b);
        for c in &b.cand_list[..b.num_samples] {
            let (sy, sx) = (i64::from(c[0]) - suy, i64::from(c[1]) - sux);
            let (dy, dx) = (i64::from(c[2]) - duy, i64::from(c[3]) - dux);
            if (sx - dx).abs() < 256 && (sy - dy).abs() < 256 {
                a[0][0] += ls(sx, sx) + 8;
                a[0][1] += ls(sx, sy) + 4;
                a[1][1] += ls(sy, sy) + 8;
                bx[0] += ls(sx, dx) + 8;
                bx[1] += ls(sy, dx) + 4;
                by[0] += ls(sx, dy) + 4;
                by[1] += ls(sy, dy) + 8;
            }
        }
        let det = a[0][0] * a[1][1] - a[0][1] * a[0][1];
        if det == 0 {
            return None;
        }
        let (shift, mut factor) = resolve_divisor(det);
        let mut shift = shift as i32 - 16;
        if shift < 0 {
            factor <<= -shift;
            shift = 0;
        }
        let div = |v: i64| round2_signed_128(i128::from(v) * i128::from(factor), shift as u32);
        const CLAMP: i128 = 1 << 13;
        let nondiag = |v: i64| div(v).clamp(-CLAMP + 1, CLAMP - 1) as i32;
        let diag = |v: i64| div(v).clamp((1 << 16) - CLAMP + 1, (1 << 16) + CLAMP - 1) as i32;
        let mut p = [0i32; 6];
        p[2] = diag(a[1][1] * bx[0] - a[0][1] * bx[1]);
        p[3] = nondiag(-a[0][1] * bx[0] + a[0][0] * bx[1]);
        p[4] = nondiag(a[1][1] * by[0] - a[0][1] * by[1]);
        p[5] = diag(-a[0][1] * by[0] + a[0][0] * by[1]);
        let (mvx, mvy) = (i64::from(b.mv[0][1]), i64::from(b.mv[0][0]));
        let vx =
            mvx * (1 << 13) - (mid_x * (i64::from(p[2]) - (1 << 16)) + mid_y * i64::from(p[3]));
        let vy =
            mvy * (1 << 13) - (mid_x * i64::from(p[4]) + mid_y * (i64::from(p[5]) - (1 << 16)));
        p[0] = vx.clamp(-(1 << 23), (1 << 23) - 1) as i32;
        p[1] = vy.clamp(-(1 << 23), (1 << 23) - 1) as i32;
        Some(p)
    }

    /// The overlapped motion compensation process (§7.11.3.9).
    fn overlapped(&mut self, plane: usize, w: usize, h: usize) -> Result<(), Error> {
        let (row, col, size) = (self.b.row, self.b.col, self.b.size);
        let (sx, sy) = self.plane_sub(plane);
        let rounds = if self.f.bit_depth == 12 {
            [5, 9]
        } else {
            [3, 11]
        };
        if self.b.avail_u && self.residual_size(size, plane) >= BLOCK_8X8 {
            let n_limit = 4.min(usize::from(MI_WIDTH_LOG2[size]));
            let (mut n, mut x4) = (0, col);
            while n < n_limit && x4 < self.f.mi_cols.min(col + self.b.bw4) {
                let cand = self.f.mi(row - 1, x4 | 1);
                let step4 = usize::from(NUM_4X4_BLOCKS_WIDE[usize::from(self.f.mi_sizes[cand])])
                    .clamp(2, 16);
                if self.f.ref_frames[cand][0] > INTRA_FRAME {
                    n += 1;
                    let pred_w = w.min((step4 * 4) >> sx);
                    let pred_h = (h >> 1).min(32 >> sy);
                    self.predict_overlap(plane, cand, x4, row, pred_w, pred_h, 0, rounds)?;
                }
                x4 += step4;
            }
        }
        if self.b.avail_l {
            let n_limit = 4.min(usize::from(MI_HEIGHT_LOG2[size]));
            let (mut n, mut y4) = (0, row);
            while n < n_limit && y4 < self.f.mi_rows.min(row + self.b.bh4) {
                let cand = self.f.mi(y4 | 1, col - 1);
                let step4 = usize::from(NUM_4X4_BLOCKS_HIGH[usize::from(self.f.mi_sizes[cand])])
                    .clamp(2, 16);
                if self.f.ref_frames[cand][0] > INTRA_FRAME {
                    n += 1;
                    let pred_w = (w >> 1).min(32 >> sx);
                    let pred_h = h.min((step4 * 4) >> sy);
                    self.predict_overlap(plane, cand, col, y4, pred_w, pred_h, 1, rounds)?;
                }
                y4 += step4;
            }
        }
        Ok(())
    }

    /// `predict_overlap()` and the overlap blending process (§7.11.3.10).
    #[allow(clippy::too_many_arguments)]
    fn predict_overlap(
        &mut self,
        plane: usize,
        cand: usize,
        x4: usize,
        y4: usize,
        w: usize,
        h: usize,
        pass: usize,
        rounds: [u32; 2],
    ) -> Result<(), Error> {
        let (sx, sy) = self.plane_sub(plane);
        let (x, y) = ((x4 * 4) >> sx, (y4 * 4) >> sy);
        let mv = self.f.mvs[cand][0];
        let pred = self.f.block_inter(
            self.f.ref_frames[cand][0],
            plane,
            x,
            y,
            w,
            h,
            mv,
            cand,
            rounds,
        )?;
        let max = (1 << self.f.bit_depth) - 1;
        let mask = obmc_mask(if pass == 0 { h } else { w });
        let out = &mut self.f.planes[plane];
        for i in 0..h {
            for j in 0..w {
                let m = i32::from(mask[if pass == 0 { i } else { j }]);
                let v = m * out.at(x + j, y + i) + (64 - m) * pred[i * w + j].clamp(0, max);
                out.set(x + j, y + i, round2(v, 6));
            }
        }
        Ok(())
    }
}
