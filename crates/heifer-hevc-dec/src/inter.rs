//! Inter prediction (H.265 §8.5.3): motion vector prediction (merge mode, AMVP, temporal
//! candidates) and fractional sample interpolation.
//!
//! The derivations here are pure: neighbouring motion comes through [`MotionSource`], which the
//! parser implements with the availability rules of §6.4.2, and the collocated picture through
//! [`SliceRefs`]. Only short-term reference pictures exist (long-term ones are rejected while
//! parsing the slice header), so every `LongTermRefPic()` test of the spec is false.

use crate::recon::Frame;

/// A motion vector, in quarter luma samples.
pub type Mv = [i16; 2];

/// Motion of a prediction block: a vector and a reference index per reference picture list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Motion {
    /// `MvL0`, `MvL1`.
    pub mv: [Mv; 2],
    /// `RefIdxL0`, `RefIdxL1`; -1 when the list is not used (`PredFlagLX` = 0).
    pub ref_idx: [i8; 2],
}

impl Motion {
    /// No motion: an intra block, or a block not decoded yet.
    pub const NONE: Self = Self {
        mv: [[0; 2]; 2],
        ref_idx: [-1; 2],
    };

    /// Uni-prediction from `RefPicList0`.
    pub fn l0(mv: Mv, ref_idx: i8) -> Self {
        Self {
            mv: [mv, [0; 2]],
            ref_idx: [ref_idx, -1],
        }
    }

    /// `PredFlagLX`.
    pub fn uses(&self, list: usize) -> bool {
        self.ref_idx[list] >= 0
    }

    /// Whether any list is used, i.e. the block is inter predicted.
    pub fn is_inter(&self) -> bool {
        self.uses(0) || self.uses(1)
    }
}

/// `PartMode` of an inter coding unit (Table 7-10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartMode {
    /// One prediction block.
    Part2Nx2N,
    /// Two blocks, top and bottom.
    Part2NxN,
    /// Two blocks, left and right.
    PartNx2N,
    /// Four blocks.
    PartNxN,
    /// Top quarter and bottom three quarters.
    Part2NxnU,
    /// Top three quarters and bottom quarter.
    Part2NxnD,
    /// Left quarter and right three quarters.
    PartnLx2N,
    /// Left three quarters and right quarter.
    PartnRx2N,
}

impl PartMode {
    /// The prediction blocks of a coding unit of size `n` at (`x`, `y`): position and size.
    pub fn blocks(self, x: u32, y: u32, n: u32) -> Vec<(u32, u32, u32, u32)> {
        let (h, q) = (n / 2, n / 4);
        match self {
            Self::Part2Nx2N => vec![(x, y, n, n)],
            Self::Part2NxN => vec![(x, y, n, h), (x, y + h, n, h)],
            Self::PartNx2N => vec![(x, y, h, n), (x + h, y, h, n)],
            Self::PartNxN => vec![
                (x, y, h, h),
                (x + h, y, h, h),
                (x, y + h, h, h),
                (x + h, y + h, h, h),
            ],
            Self::Part2NxnU => vec![(x, y, n, q), (x, y + q, n, n - q)],
            Self::Part2NxnD => vec![(x, y, n, n - q), (x, y + n - q, n, q)],
            Self::PartnLx2N => vec![(x, y, q, n), (x + q, y, n - q, n)],
            Self::PartnRx2N => vec![(x, y, n - q, n), (x + n - q, y, q, n)],
        }
    }

    fn is_vertical_split(self) -> bool {
        matches!(self, Self::PartNx2N | Self::PartnLx2N | Self::PartnRx2N)
    }

    fn is_horizontal_split(self) -> bool {
        matches!(self, Self::Part2NxN | Self::Part2NxnU | Self::Part2NxnD)
    }
}

/// A prediction block and the coding block it belongs to, in luma samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PuGeom {
    /// Top-left of the coding block.
    pub x_cb: u32,
    /// Top-left of the coding block.
    pub y_cb: u32,
    /// Size of the coding block.
    pub n_cb: u32,
    /// Top-left of the prediction block.
    pub x: u32,
    /// Top-left of the prediction block.
    pub y: u32,
    /// Width of the prediction block.
    pub w: u32,
    /// Height of the prediction block.
    pub h: u32,
    /// `partIdx`.
    pub part_idx: u8,
    /// `PartMode` of the coding unit.
    pub part_mode: PartMode,
}

/// Motion of the neighbouring prediction blocks of the picture being decoded.
pub trait MotionSource {
    /// Motion of the prediction block covering luma position (`xn`, `yn`), or `None` when it
    /// is unavailable for the prediction block `g` (§6.4.2) or intra coded.
    fn neighbour(&self, g: &PuGeom, xn: i64, yn: i64) -> Option<Motion>;
}

/// Motion of a decoded picture, kept to serve as collocated picture (§8.5.3.2.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MotionField {
    w4: usize,
    /// One entry per 4×4 luma block.
    blocks: Vec<ColMotion>,
    /// `PicOrderCntVal` of the picture.
    pub poc: i32,
}

/// Motion of one 4×4 block of a collocated picture: the vectors, and the POC of the pictures
/// they point to (the reference lists of the collocated picture are gone by then).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ColMotion {
    /// `MvL0`, `MvL1`.
    pub mv: [Mv; 2],
    /// POC of the reference picture of each list, `None` when the list is not used.
    pub ref_poc: [Option<i32>; 2],
    /// Whether the reference picture of each list was a long-term one.
    pub ref_lt: [bool; 2],
}

impl ColMotion {
    /// An intra block.
    pub const INTRA: Self = Self {
        mv: [[0; 2]; 2],
        ref_poc: [None; 2],
        ref_lt: [false; 2],
    };
}

impl MotionField {
    /// A field of intra blocks for a `width`×`height` picture.
    pub fn new(width: u32, height: u32, poc: i32) -> Self {
        let w4 = width.div_ceil(4) as usize;
        let h4 = height.div_ceil(4) as usize;
        Self {
            w4,
            blocks: vec![ColMotion::INTRA; w4 * h4],
            poc,
        }
    }

    /// Sets the motion of a luma area.
    pub fn fill(&mut self, x: u32, y: u32, w: u32, h: u32, m: ColMotion) {
        let h4 = self.blocks.len() / self.w4;
        for yy in (y as usize >> 2)..((y + h) as usize).div_ceil(4).min(h4) {
            let row = yy * self.w4;
            for xx in (x as usize >> 2)..((x + w) as usize).div_ceil(4).min(self.w4) {
                self.blocks[row + xx] = m;
            }
        }
    }

    /// A field from the motion of every 4×4 block, `w4` blocks per row.
    pub fn from_blocks(w4: usize, blocks: Vec<ColMotion>, poc: i32) -> Self {
        Self { w4, blocks, poc }
    }

    /// Motion at luma position (`x`, `y`).
    pub fn at(&self, x: u32, y: u32) -> ColMotion {
        self.blocks
            .get((y as usize >> 2) * self.w4 + (x as usize >> 2))
            .copied()
            .unwrap_or(ColMotion::INTRA)
    }
}

/// Reference pictures of the slice being decoded, as needed by motion vector prediction.
#[derive(Debug, Clone, Copy)]
pub struct SliceRefs<'a> {
    /// `PicOrderCntVal` of the current picture.
    pub poc: i32,
    /// POC of each entry of `RefPicList0` and `RefPicList1`.
    pub lists: [&'a [i32]; 2],
    /// Whether each entry of the lists is a long-term reference picture.
    pub long_term: [&'a [bool]; 2],
    /// Motion of the collocated picture, when `slice_temporal_mvp_enabled_flag` is set.
    pub col: Option<&'a MotionField>,
    /// `collocated_from_l0_flag`.
    pub collocated_from_l0: bool,
    /// `NoBackwardPredFlag`: no reference picture follows the current one in output order.
    pub no_backward_pred: bool,
    /// Picture size in luma samples.
    pub pic_size: (u32, u32),
    /// `CtbLog2SizeY`.
    pub log2_ctb: u8,
}

impl SliceRefs<'_> {
    fn poc_of(&self, list: usize, ref_idx: i8) -> Option<i32> {
        usize::try_from(ref_idx)
            .ok()
            .and_then(|i| self.lists[list].get(i))
            .copied()
    }

    /// `LongTermRefPic()`: whether `RefPicListX[ref_idx]` is a long-term picture.
    fn is_long_term(&self, list: usize, ref_idx: i8) -> bool {
        usize::try_from(ref_idx)
            .ok()
            .and_then(|i| self.long_term[list].get(i))
            .copied()
            .unwrap_or(false)
    }
}

/// Scales a motion vector by the ratio of two POC distances (8-205 to 8-208).
fn scale(mv: Mv, td: i32, tb: i32) -> Mv {
    let td = td.clamp(-128, 127);
    let tb = tb.clamp(-128, 127);
    if td == 0 {
        return mv;
    }
    let tx = (16384 + (td.abs() >> 1)) / td;
    let factor = ((tb * tx + 32) >> 6).clamp(-4096, 4095);
    mv.map(|v| {
        let p = factor * i32::from(v);
        (p.signum() * ((p.abs() + 127) >> 8)).clamp(-32768, 32767) as i16
    })
}

/// Temporal luma motion vector prediction (§8.5.3.2.8): the vector of the collocated block,
/// scaled to point at `RefPicListX[ref_idx]`. A collocated vector is used only if it points
/// at a long-term picture exactly when the target is one; long-term vectors are not scaled.
pub fn temporal(refs: &SliceRefs<'_>, g: &PuGeom, ref_idx: i8, list: usize) -> Option<Mv> {
    let col = refs.col?;
    let target = refs.poc_of(list, ref_idx)?;
    let target_lt = refs.is_long_term(list, ref_idx);
    let (pw, ph) = refs.pic_size;
    let collocated = |x: u32, y: u32| -> Option<Mv> {
        let c = col.at((x >> 4) << 4, (y >> 4) << 4);
        let n = match c.ref_poc {
            [None, None] => return None,
            [None, Some(_)] => 1,
            [Some(_), None] => 0,
            [Some(_), Some(_)] if refs.no_backward_pred => list,
            [Some(_), Some(_)] => usize::from(refs.collocated_from_l0),
        };
        let (mv, ref_poc) = (c.mv[n], c.ref_poc[n]?);
        if c.ref_lt[n] != target_lt {
            return None;
        }
        let col_diff = col.poc - ref_poc;
        let curr_diff = refs.poc - target;
        Some(if target_lt || col_diff == curr_diff {
            mv
        } else {
            scale(mv, col_diff, curr_diff)
        })
    };
    let (xbr, ybr) = (g.x + g.w, g.y + g.h);
    if (g.y >> refs.log2_ctb) == (ybr >> refs.log2_ctb)
        && ybr < ph
        && xbr < pw
        && let Some(mv) = collocated(xbr, ybr)
    {
        return Some(mv);
    }
    collocated(g.x + (g.w >> 1), g.y + (g.h >> 1))
}

/// Merge mode (§8.5.3.2.2): the motion of candidate `merge_idx` of the merge candidate list.
///
/// `num_ref_idx` is the number of active entries of each reference list (0 for
/// `RefPicList1` in P slices).
pub fn merge(
    src: &impl MotionSource,
    refs: &SliceRefs<'_>,
    geom: &PuGeom,
    merge_idx: usize,
    max_num_merge_cand: usize,
    log2_par_mrg_level: u8,
    num_ref_idx: [usize; 2],
) -> Motion {
    let mut g = *geom;
    // Parallel merge level: all prediction blocks of an 8×8 coding unit share one list.
    if log2_par_mrg_level > 2 && g.n_cb == 8 {
        g = PuGeom {
            x: g.x_cb,
            y: g.y_cb,
            w: 8,
            h: 8,
            part_idx: 0,
            ..g
        };
    }
    let l = log2_par_mrg_level;
    let (x, y, w, h) = (
        i64::from(g.x),
        i64::from(g.y),
        i64::from(g.w),
        i64::from(g.h),
    );
    let same_region = |xn: i64, yn: i64| (x >> l) == (xn >> l) && (y >> l) == (yn >> l);
    let spatial = |xn: i64, yn: i64| {
        if same_region(xn, yn) {
            None
        } else {
            src.neighbour(&g, xn, yn)
        }
    };

    // Each candidate is compared with the neighbours it may duplicate as they are available,
    // whether or not those were themselves pruned from the list.
    let a1 = if g.part_mode.is_vertical_split() && g.part_idx == 1 {
        None
    } else {
        spatial(x - 1, y + h - 1)
    };
    let b1 = if g.part_mode.is_horizontal_split() && g.part_idx == 1 {
        None
    } else {
        spatial(x + w - 1, y - 1)
    };
    let b0 = spatial(x + w, y - 1);
    let a0 = spatial(x - 1, y + h);
    let b2 = spatial(x - 1, y - 1);
    let differs = |m: &Motion, other: Option<Motion>| other != Some(*m);
    let a1_flag = a1;
    let b1_flag = b1.filter(|m| differs(m, a1));
    let b0_flag = b0.filter(|m| differs(m, b1));
    let a0_flag = a0.filter(|m| differs(m, a1));
    let b2_flag = if [a0_flag, a1_flag, b0_flag, b1_flag]
        .iter()
        .all(Option::is_some)
    {
        None
    } else {
        b2.filter(|m| differs(m, a1) && differs(m, b1))
    };
    let (a1, b1, b0, a0, b2) = (a1_flag, b1_flag, b0_flag, a0_flag, b2_flag);

    let mut list: Vec<Motion> = [a1, b1, b0, a0, b2].into_iter().flatten().collect();
    let is_b = num_ref_idx[1] > 0;
    // 8×4 and 4×8 blocks are not bi-predicted: their candidate keeps only RefPicList0
    // (8-161), using the size of the block itself, not of the shared 8×8 list.
    let restrict = |m: Motion| {
        if geom.w + geom.h == 12 && m.uses(0) && m.uses(1) {
            Motion {
                mv: [m.mv[0], [0; 2]],
                ref_idx: [m.ref_idx[0], -1],
            }
        } else {
            m
        }
    };
    if list.len() > merge_idx {
        return restrict(list[merge_idx]);
    }
    // Temporal candidate, reference index 0 of each list (§8.5.3.2.2 step 4).
    let mut col = Motion::NONE;
    for l in 0..if is_b { 2 } else { 1 } {
        if let Some(mv) = temporal(refs, &g, 0, l) {
            col.mv[l] = mv;
            col.ref_idx[l] = 0;
        }
    }
    if col.is_inter() {
        list.push(col);
    }
    // Combined bi-predictive candidates (§8.5.3.2.4): L0 motion of one candidate with the L1
    // motion of another, in the order of Table 8-7.
    let num_orig = list.len();
    if is_b && num_orig > 1 && num_orig < max_num_merge_cand {
        const L0: [usize; 12] = [0, 1, 0, 2, 1, 2, 0, 3, 1, 3, 2, 3];
        const L1: [usize; 12] = [1, 0, 2, 0, 2, 1, 3, 0, 3, 1, 3, 2];
        for comb in 0..num_orig * (num_orig - 1) {
            let (c0, c1) = (list[L0[comb]], list[L1[comb]]);
            if c0.uses(0)
                && c1.uses(1)
                && (refs.poc_of(0, c0.ref_idx[0]) != refs.poc_of(1, c1.ref_idx[1])
                    || c0.mv[0] != c1.mv[1])
            {
                list.push(Motion {
                    mv: [c0.mv[0], c1.mv[1]],
                    ref_idx: [c0.ref_idx[0], c1.ref_idx[1]],
                });
                if list.len() == max_num_merge_cand {
                    break;
                }
            }
        }
    }
    // Zero candidates (§8.5.3.2.5), on both lists in B slices.
    let num_zero_ref = if is_b {
        num_ref_idx[0].min(num_ref_idx[1])
    } else {
        num_ref_idx[0]
    };
    let mut zero_idx = 0;
    while list.len() < max_num_merge_cand.max(merge_idx + 1) {
        let r = if zero_idx < num_zero_ref { zero_idx } else { 0 } as i8;
        list.push(Motion {
            mv: [[0; 2]; 2],
            ref_idx: [r, if is_b { r } else { -1 }],
        });
        zero_idx += 1;
    }
    restrict(list[merge_idx])
}

/// Luma motion vector prediction (§8.5.3.2.6): the two candidates of `mvpListLX` for a
/// prediction block that refers to `RefPicListX[ref_idx]`.
pub fn amvp(
    src: &impl MotionSource,
    refs: &SliceRefs<'_>,
    g: &PuGeom,
    ref_idx: i8,
    list: usize,
) -> [Mv; 2] {
    let other = 1 - list;
    let Some(target) = refs.poc_of(list, ref_idx) else {
        return [[0; 2]; 2];
    };
    let (x, y, w, h) = (
        i64::from(g.x),
        i64::from(g.y),
        i64::from(g.w),
        i64::from(g.h),
    );
    // A vector of a neighbour that points at the target picture, from list X then list Y.
    let same_pic = |m: &Motion| {
        [list, other]
            .into_iter()
            .find(|&l| m.uses(l) && refs.poc_of(l, m.ref_idx[l]) == Some(target))
            .map(|l| m.mv[l])
    };
    // A vector of the neighbour pointing at a picture of the same kind (short- or long-term)
    // as the target, scaled to the target's distance when both are short-term.
    let target_lt = refs.is_long_term(list, ref_idx);
    let scaled = |m: &Motion| {
        [list, other]
            .into_iter()
            .find(|&l| m.uses(l) && refs.is_long_term(l, m.ref_idx[l]) == target_lt)
            .map(|l| {
                let poc = refs.poc_of(l, m.ref_idx[l]).unwrap_or(target);
                if target_lt || poc == target {
                    m.mv[l]
                } else {
                    scale(m.mv[l], refs.poc - poc, refs.poc - target)
                }
            })
    };

    let a = [(x - 1, y + h), (x - 1, y + h - 1)].map(|(xn, yn)| src.neighbour(g, xn, yn));
    let is_scaled = a.iter().any(Option::is_some);
    let mut mv_a = a.iter().flatten().find_map(same_pic);
    if mv_a.is_none() {
        mv_a = a.iter().flatten().find_map(scaled);
    }

    let b = [(x + w, y - 1), (x + w - 1, y - 1), (x - 1, y - 1)]
        .map(|(xn, yn)| src.neighbour(g, xn, yn));
    let mut mv_b = b.iter().flatten().find_map(same_pic);
    if !is_scaled {
        if mv_a.is_none() {
            mv_a = mv_b;
        }
        mv_b = b.iter().flatten().find_map(scaled);
    }

    let mut candidates: Vec<Mv> = Vec::with_capacity(3);
    candidates.extend(mv_a);
    if let Some(b) = mv_b
        && mv_a != Some(b)
    {
        candidates.push(b);
    }
    if candidates.len() < 2
        && let Some(col) = temporal(refs, g, ref_idx, list)
    {
        candidates.push(col);
    }
    candidates.resize(2, [0; 2]);
    [candidates[0], candidates[1]]
}

/// Luma interpolation filter coefficients `fL` (Table 8-11), by fractional position.
const LUMA_FILTER: [[i32; 8]; 4] = [
    [0, 0, 0, 64, 0, 0, 0, 0],
    [-1, 4, -10, 58, 17, -5, 1, 0],
    [-1, 4, -11, 40, 40, -11, 4, -1],
    [0, 1, -5, 17, 58, -10, 4, -1],
];

/// Chroma interpolation filter coefficients `fC` (Table 8-12), by fractional position.
const CHROMA_FILTER: [[i32; 4]; 8] = [
    [0, 64, 0, 0],
    [-2, 58, 10, -2],
    [-4, 54, 16, -2],
    [-6, 46, 28, -4],
    [-4, 36, 36, -4],
    [-4, 28, 46, -6],
    [-2, 16, 54, -4],
    [-2, 10, 58, -2],
];

/// Fractional sample interpolation of one block (§8.5.3.3.3) at 14-bit intermediate
/// precision, into `out` (`w * h` values, row-major).
///
/// `TAPS` is 8 for luma and 4 for chroma; (`xi`, `yi`) is the integer sample position of the
/// block in the reference plane and (`fx`, `fy`) the fraction. Positions outside the plane are
/// clamped to its edges, as the spec does.
#[allow(clippy::too_many_arguments)]
fn interpolate<const TAPS: usize>(
    plane: &[u16],
    stride: usize,
    plane_h: usize,
    xi: i64,
    yi: i64,
    fx: usize,
    fy: usize,
    w: usize,
    h: usize,
    bit_depth: u8,
    filters: &[[i32; TAPS]],
    out: &mut [i32],
) {
    let shift1 = (i32::from(bit_depth) - 8).min(4);
    let shift3 = (14 - i32::from(bit_depth)).max(2);
    let before = (TAPS / 2 - 1) as i64;
    // The source area: the block and the filter margins. When it lies inside the plane
    // (most blocks), the plane is read in place; otherwise the area is copied once with its
    // coordinates clamped to the plane, as the spec does.
    let (sw, sh) = (w + TAPS - 1, h + TAPS - 1);
    let (x0, y0) = (xi - before, yi - before);
    let padded: Vec<u16>;
    let (src, src_stride) =
        if x0 >= 0 && y0 >= 0 && x0 as usize + sw <= stride && y0 as usize + sh <= plane_h {
            (&plane[y0 as usize * stride + x0 as usize..], stride)
        } else {
            let (max_x, max_y) = (stride as i64 - 1, plane_h as i64 - 1);
            padded = (0..sh as i64)
                .flat_map(|r| {
                    let row = (y0 + r).clamp(0, max_y) as usize * stride;
                    (0..sw as i64).map(move |c| plane[row + (x0 + c).clamp(0, max_x) as usize])
                })
                .collect();
            (&padded[..], sw)
        };
    let b = before as usize;
    let row = |r: usize| &src[r * src_stride..r * src_stride + sw];
    let (cx, cy) = (&filters[fx], &filters[fy]);
    let horizontal = |row: &[u16], out: &mut [i32]| {
        for (xx, o) in out.iter_mut().enumerate() {
            let taps = &row[xx..xx + TAPS];
            let s: i32 = cx.iter().zip(taps).map(|(&c, &v)| c * i32::from(v)).sum();
            *o = s >> shift1;
        }
    };
    match (fx, fy) {
        (0, 0) => {
            for (yy, out) in out.chunks_exact_mut(w).take(h).enumerate() {
                for (o, &v) in out.iter_mut().zip(&row(yy + b)[b..b + w]) {
                    *o = i32::from(v) << shift3;
                }
            }
        }
        (_, 0) => {
            for (yy, out) in out.chunks_exact_mut(w).take(h).enumerate() {
                horizontal(row(yy + b), out);
            }
        }
        (0, _) => {
            for (yy, out) in out.chunks_exact_mut(w).take(h).enumerate() {
                out.fill(0);
                for (t, &c) in cy.iter().enumerate() {
                    for (o, &v) in out.iter_mut().zip(&row(yy + t)[b..b + w]) {
                        *o += c * i32::from(v);
                    }
                }
                for o in out.iter_mut() {
                    *o >>= shift1;
                }
            }
        }
        _ => {
            // Horizontal pass over the rows the vertical filter needs, then vertical pass.
            let mut tmp = [0i32; 64 * (64 + 7)];
            let tmp = &mut tmp[..sh * w];
            for (r, t) in tmp.chunks_exact_mut(w).enumerate() {
                horizontal(row(r), t);
            }
            for (yy, out) in out.chunks_exact_mut(w).take(h).enumerate() {
                out.fill(0);
                for (t, &c) in cy.iter().enumerate() {
                    for (o, &v) in out.iter_mut().zip(&tmp[(yy + t) * w..(yy + t + 1) * w]) {
                        *o += c * v;
                    }
                }
                for o in out.iter_mut() {
                    *o >>= 6;
                }
            }
        }
    }
}

/// Prediction samples of component `c` of a prediction block from one reference picture
/// (§8.5.3.3.3), at 14-bit intermediate precision. (`x`, `y`, `w`, `h`) is the block in
/// luma samples.
#[allow(clippy::too_many_arguments)]
pub fn predict_component(
    reference: &Frame,
    c: usize,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    mv: Mv,
    sub: (u32, u32),
    out: &mut [i32],
) {
    let stride = reference.widths[c] as usize;
    let plane_h = reference.heights[c] as usize;
    let plane = &reference.planes[c];
    if c == 0 {
        let (mx, my) = (i64::from(mv[0]), i64::from(mv[1]));
        interpolate::<8>(
            plane,
            stride,
            plane_h,
            i64::from(x) + (mx >> 2),
            i64::from(y) + (my >> 2),
            (mx & 3) as usize,
            (my & 3) as usize,
            w as usize,
            h as usize,
            reference.bit_depth[0],
            &LUMA_FILTER,
            out,
        );
    } else {
        let (sw, sh) = sub;
        // Chroma vectors are in 1/8 chroma samples (8-228, 8-229).
        let mx = i64::from(mv[0]) * 2 / i64::from(sw);
        let my = i64::from(mv[1]) * 2 / i64::from(sh);
        interpolate::<4>(
            plane,
            stride,
            plane_h,
            i64::from(x / sw) + (mx >> 3),
            i64::from(y / sh) + (my >> 3),
            (mx & 7) as usize,
            (my & 7) as usize,
            (w / sw) as usize,
            (h / sh) as usize,
            reference.bit_depth[1],
            &CHROMA_FILTER,
            out,
        );
    }
}

/// Default weighted sample prediction for one list (8-252): back to the sample bit depth.
pub fn weight_uni(pred: i32, bit_depth: u8) -> u16 {
    let shift = 14 - i32::from(bit_depth);
    let offset = if shift > 0 { 1 << (shift - 1) } else { 0 };
    ((pred + offset) >> shift).clamp(0, (1 << bit_depth) - 1) as u16
}

/// Explicit weighted sample prediction for one list (8-252): `weight` and `offset` (already
/// scaled to the bit depth) with denominator `2^log2_denom`.
pub fn weight_explicit(pred: i32, bit_depth: u8, weight: i32, offset: i32, log2_denom: u8) -> u16 {
    let log2_wd = i32::from(log2_denom) + 14 - i32::from(bit_depth);
    let max = (1 << bit_depth) - 1;
    let v = if log2_wd >= 1 {
        ((pred * weight + (1 << (log2_wd - 1))) >> log2_wd) + offset
    } else {
        pred * weight + offset
    };
    v.clamp(0, max) as u16
}

/// Default weighted sample prediction of a bi-predicted block (8-264): the average of the
/// two predictions.
pub fn weight_bi(p0: i32, p1: i32, bit_depth: u8) -> u16 {
    let shift = 15 - i32::from(bit_depth);
    ((p0 + p1 + (1 << (shift - 1))) >> shift).clamp(0, (1 << bit_depth) - 1) as u16
}

/// Explicit weighted sample prediction of a bi-predicted block (8-270): the two
/// predictions weighted, with the average of the two offsets.
pub fn weight_explicit_bi(
    pred: [i32; 2],
    bit_depth: u8,
    weight: [i32; 2],
    offset: [i32; 2],
    log2_denom: u8,
) -> u16 {
    let log2_wd = i32::from(log2_denom) + 14 - i32::from(bit_depth);
    let v = (pred[0] * weight[0] + pred[1] * weight[1] + ((offset[0] + offset[1] + 1) << log2_wd))
        >> (log2_wd + 1);
    v.clamp(0, (1 << bit_depth) - 1) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Neighbours: A1 (left) uses L0, B1 (above) uses L1; the others are unavailable.
    struct TwoNeighbours;
    impl MotionSource for TwoNeighbours {
        fn neighbour(&self, g: &PuGeom, xn: i64, yn: i64) -> Option<Motion> {
            let (x, y) = (i64::from(g.x), i64::from(g.y));
            if xn == x - 1 && yn == y + i64::from(g.h) - 1 {
                Some(Motion::l0([4, 0], 0))
            } else if yn == y - 1 && xn == x + i64::from(g.w) - 1 {
                Some(Motion {
                    mv: [[0; 2], [-8, 2]],
                    ref_idx: [-1, 0],
                })
            } else {
                None
            }
        }
    }

    fn b_refs<'a>(l0: &'a [i32], l1: &'a [i32]) -> SliceRefs<'a> {
        SliceRefs {
            poc: 4,
            lists: [l0, l1],
            long_term: [&[], &[]],
            col: None,
            collocated_from_l0: true,
            no_backward_pred: false,
            pic_size: (64, 64),
            log2_ctb: 4,
        }
    }

    fn geom(w: u32, h: u32) -> PuGeom {
        PuGeom {
            x_cb: 16,
            y_cb: 16,
            n_cb: 16,
            x: 16,
            y: 16,
            w,
            h,
            part_idx: 0,
            part_mode: PartMode::Part2Nx2N,
        }
    }

    #[test]
    fn b_merge_list_combines_and_fills_with_zero_on_both_lists() {
        let (l0, l1) = ([0, 2], [8]);
        let refs = b_refs(&l0, &l1);
        let g = geom(16, 16);
        let m = |i| merge(&TwoNeighbours, &refs, &g, i, 5, 2, [2, 1]);
        assert_eq!(m(0), Motion::l0([4, 0], 0));
        assert_eq!(m(1).ref_idx, [-1, 0]);
        // A1's L0 motion with B1's L1 motion; the reverse pair has neither.
        assert_eq!(
            m(2),
            Motion {
                mv: [[4, 0], [-8, 2]],
                ref_idx: [0, 0],
            }
        );
        // Zero candidates: min(2, 1) = 1 reference index on both lists.
        let zero = Motion {
            mv: [[0; 2]; 2],
            ref_idx: [0, 0],
        };
        assert_eq!(m(3), zero);
        assert_eq!(m(4), zero);
    }

    #[test]
    fn small_blocks_are_not_bi_predicted() {
        let (l0, l1) = ([0], [8]);
        let refs = b_refs(&l0, &l1);
        let g = geom(8, 4);
        let m = merge(&TwoNeighbours, &refs, &g, 2, 5, 2, [1, 1]);
        assert_eq!(m, Motion::l0([4, 0], 0));
        let p = merge(
            &TwoNeighbours,
            &b_refs(&l0, &[]),
            &geom(8, 4),
            1,
            5,
            2,
            [1, 0],
        );
        assert_eq!(
            p.ref_idx,
            [-1, 0],
            "spatial candidates are copied as they are"
        );
    }

    #[test]
    fn bi_prediction_averages_and_weights() {
        // 8-bit samples at 14-bit precision: 100 << 6 and 103 << 6.
        let (p0, p1) = (100 << 6, 103 << 6);
        assert_eq!(weight_bi(p0, p1, 8), 102); // (100 + 103 + 1) / 2, rounded up
        assert_eq!(weight_bi(p0, p0, 8), 100);
        // Weights 1/1 with denominator 0 and no offset give the plain average.
        assert_eq!(weight_explicit_bi([p0, p1], 8, [1, 1], [0, 0], 0), 102);
        // Each weight is over 2^denom and the sum is halved: 6 and 2 over 4 blend 100 and
        // 200 as 3/4 and 1/4, plus the average of offsets 4 and 6.
        let p = [100 << 6, 200 << 6];
        assert_eq!(weight_explicit_bi(p, 8, [6, 2], [4, 6], 2), 130);
        assert_eq!(weight_bi(0, 0, 10), 0);
        assert_eq!(weight_bi(i32::from(u16::MAX), 1 << 14, 8), 255);
    }

    #[test]
    fn scaling_matches_the_spec_formula() {
        // Same distance: unchanged.
        assert_eq!(scale([8, -8], 2, 2), [8, -8]);
        // Twice the distance: doubled (with the spec's rounding).
        assert_eq!(scale([8, -8], 1, 2), [16, -16]);
        // Half the distance.
        assert_eq!(scale([9, -9], 2, 1), [4, -4]);
        // Opposite direction.
        assert_eq!(scale([4, 0], 1, -1), [-4, 0]);
    }

    #[test]
    fn partition_blocks_cover_the_coding_unit() {
        for mode in [
            PartMode::Part2Nx2N,
            PartMode::Part2NxN,
            PartMode::PartNx2N,
            PartMode::PartNxN,
            PartMode::Part2NxnU,
            PartMode::Part2NxnD,
            PartMode::PartnLx2N,
            PartMode::PartnRx2N,
        ] {
            let area: u32 = mode.blocks(16, 32, 16).iter().map(|b| b.2 * b.3).sum();
            assert_eq!(area, 256, "{mode:?}");
        }
        assert_eq!(
            PartMode::Part2NxnU.blocks(0, 0, 32),
            vec![(0, 0, 32, 8), (0, 8, 32, 24)]
        );
    }

    #[test]
    fn integer_vectors_copy_samples() {
        let frame = Frame {
            widths: [8, 4, 4],
            heights: [8, 4, 4],
            planes: [(0..64).collect(), vec![100; 16], vec![50; 16]],
            bit_depth: [8, 8],
            chroma_format: 1,
            full_range: false,
            matrix_coeffs: 2,
        };
        let mut out = vec![0; 4];
        predict_component(&frame, 0, 2, 2, 2, 2, [4, 4], (2, 2), &mut out);
        // Vector (1, 1) in whole samples: samples (3, 3), (4, 3), (3, 4), (4, 4).
        let got: Vec<u16> = out.iter().map(|&p| weight_uni(p, 8)).collect();
        assert_eq!(got, vec![27, 28, 35, 36]);
        // Outside the picture: clamped to the edge.
        predict_component(&frame, 0, 0, 0, 2, 2, [-64, -64], (2, 2), &mut out);
        let got: Vec<u16> = out.iter().map(|&p| weight_uni(p, 8)).collect();
        assert_eq!(got, vec![0, 0, 0, 0]);
        // A flat plane stays flat at any fraction.
        let mut c = vec![0; 1];
        predict_component(&frame, 1, 0, 0, 2, 2, [3, 5], (2, 2), &mut c);
        assert_eq!(weight_uni(c[0], 8), 100);
    }
}
