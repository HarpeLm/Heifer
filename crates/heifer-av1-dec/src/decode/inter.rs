//! The mode info of inter frames (§5.11.7, §5.11.19–5.11.32) and the motion vector prediction
//! processes (§7.10.2–7.10.4).

use super::*;
use crate::frame::relative_dist;

pub(super) const NONE: i8 = -1;
pub(super) const LAST_FRAME: i8 = 1;
const LAST2_FRAME: i8 = 2;
const LAST3_FRAME: i8 = 3;
const GOLDEN_FRAME: i8 = 4;
const BWDREF_FRAME: i8 = 5;
const ALTREF2_FRAME: i8 = 6;
const ALTREF_FRAME: i8 = 7;

pub(super) const NEARESTMV: u8 = 13;
const NEARMV: u8 = 14;
pub(super) const GLOBALMV: u8 = 15;
const NEWMV: u8 = 16;
const NEAREST_NEARESTMV: u8 = 17;
const NEAR_NEARMV: u8 = 18;
const NEAREST_NEWMV: u8 = 19;
const NEW_NEARESTMV: u8 = 20;
const NEAR_NEWMV: u8 = 21;
const NEW_NEARMV: u8 = 22;
pub(super) const GLOBAL_GLOBALMV: u8 = 23;
const NEW_NEWMV: u8 = 24;

pub(super) const SIMPLE: u8 = 0;
pub(super) const OBMC: u8 = 1;
pub(super) const LOCALWARP: u8 = 2;

pub(super) const COMPOUND_WEDGE: u8 = 0;
pub(super) const COMPOUND_DIFFWTD: u8 = 1;
pub(super) const COMPOUND_AVERAGE: u8 = 2;
pub(super) const COMPOUND_INTRA: u8 = 3;
pub(super) const COMPOUND_DISTANCE: u8 = 4;

pub(super) const TRANSLATION: u8 = 1;
const SEG_LVL_REF_FRAME: usize = 5;
const SEG_LVL_GLOBALMV: usize = 7;
const LEAST_SQUARES_SAMPLES_MAX: usize = 8;
/// The invalid motion vector of the motion field.
pub(super) const INVALID_MV: i32 = -1 << 15;

/// `RefStackMv`, `WeightStack`, `NumMvFound` and what the find MV stack process leaves for the
/// syntax elements that follow.
#[derive(Default)]
pub(super) struct MvStack {
    pub(super) mvs: [[[i32; 2]; 2]; MAX_REF_MV_STACK_SIZE],
    weights: [u32; MAX_REF_MV_STACK_SIZE],
    pub(super) count: usize,
    found_match: bool,
    new_mv_count: u32,
    pub(super) global_mvs: [[i32; 2]; 2],
    zero_mv_ctx: usize,
    new_mv_ctx: usize,
    ref_mv_ctx: usize,
    drl_ctx: [usize; MAX_REF_MV_STACK_SIZE],
}

impl MvStack {
    /// The sorting process (§7.10.2.11): a stable sort by decreasing weight.
    fn sort(&mut self, start: usize, mut end: usize) {
        while end > start {
            let mut new_end = start;
            for i in start + 1..end {
                if self.weights[i - 1] < self.weights[i] {
                    self.weights.swap(i - 1, i);
                    self.mvs.swap(i - 1, i);
                    new_end = i;
                }
            }
            end = new_end;
        }
    }

    /// Adds `mvs` (both lists when `compound`) with `weight`, or adds the weight to an equal
    /// entry.
    fn add(&mut self, mvs: [[i32; 2]; 2], compound: bool, weight: u32) {
        let lists = 1 + usize::from(compound);
        if let Some(i) = (0..self.count).find(|&i| self.mvs[i][..lists] == mvs[..lists]) {
            self.weights[i] += weight;
        } else if self.count < MAX_REF_MV_STACK_SIZE {
            self.mvs[self.count] = mvs;
            self.weights[self.count] = weight;
            self.count += 1;
        }
    }
}

fn has_newmv(mode: u8) -> bool {
    matches!(
        mode,
        NEWMV | NEW_NEWMV | NEAR_NEWMV | NEW_NEARMV | NEAREST_NEWMV | NEW_NEARESTMV
    )
}

fn check_backward(r: i8) -> bool {
    (BWDREF_FRAME..=ALTREF_FRAME).contains(&r)
}

fn is_samedir_ref_pair(a: i8, b: i8) -> bool {
    (a >= BWDREF_FRAME) == (b >= BWDREF_FRAME)
}

fn ref_count_ctx(a: u32, b: u32) -> usize {
    match a.cmp(&b) {
        std::cmp::Ordering::Less => 0,
        std::cmp::Ordering::Equal => 1,
        std::cmp::Ordering::Greater => 2,
    }
}

impl TileDecoder<'_, '_> {
    fn gm_type(&self, r: i8) -> u8 {
        if r > INTRA_FRAME {
            self.f.fh.gm_types[r as usize]
        } else {
            0
        }
    }

    /// `inter_frame_mode_info()`.
    pub(super) fn inter_frame_mode_info(&mut self) -> Result<(), Error> {
        let (row, col) = (self.b.row, self.b.col);
        self.b.left_ref = if self.b.avail_l {
            self.f.ref_frames[self.f.mi(row, col - 1)]
        } else {
            [INTRA_FRAME, NONE]
        };
        self.b.above_ref = if self.b.avail_u {
            self.f.ref_frames[self.f.mi(row - 1, col)]
        } else {
            [INTRA_FRAME, NONE]
        };
        self.inter_segment_id(true)?;
        self.read_skip_mode()?;
        if self.b.skip_mode {
            self.b.skip = true;
        } else {
            self.read_skip()?;
        }
        if !self.f.fh.segmentation.seg_id_pre_skip {
            self.inter_segment_id(false)?;
        }
        self.b.lossless = self.f.fh.lossless[self.b.segment_id];
        self.read_cdef()?;
        self.read_delta_qindex()?;
        self.read_delta_lf()?;
        self.read_deltas = false;
        self.read_is_inter()?;
        if self.b.is_inter {
            self.inter_block_mode_info()
        } else {
            // intra_block_mode_info()
            let ctx = usize::from(SIZE_GROUP[self.b.size]);
            let y_mode = self.sym(|c| &mut c.y_mode[ctx])? as u8;
            self.intra_modes(y_mode)
        }
    }

    /// `inter_segment_id( preSkip )`.
    fn inter_segment_id(&mut self, pre_skip: bool) -> Result<(), Error> {
        let seg = &self.f.fh.segmentation;
        if !seg.enabled {
            self.b.segment_id = 0;
            return Ok(());
        }
        let predicted = self.predicted_segment_id();
        if !seg.update_map {
            self.b.segment_id = predicted;
            return Ok(());
        }
        if pre_skip && !seg.seg_id_pre_skip {
            self.b.segment_id = 0;
            return Ok(());
        }
        if !pre_skip && self.b.skip {
            self.set_seg_pred_context(0);
            return self.read_segment_id();
        }
        if seg.temporal_update {
            let ctx = usize::from(self.left_seg_pred[self.b.row] + self.above_seg_pred[self.b.col]);
            let flag = self.sym(|c| &mut c.segment_id_predicted[ctx])? as u8;
            if flag == 1 {
                self.b.segment_id = predicted;
            } else {
                self.read_segment_id()?;
            }
            self.set_seg_pred_context(flag);
            Ok(())
        } else {
            self.read_segment_id()
        }
    }

    fn set_seg_pred_context(&mut self, flag: u8) {
        let b = &self.b;
        self.above_seg_pred[b.col..b.col + b.bw4].fill(flag);
        self.left_seg_pred[b.row..b.row + b.bh4].fill(flag);
    }

    /// `get_segment_id()`: the smallest segment of the previous map under the block.
    fn predicted_segment_id(&self) -> usize {
        let b = &self.b;
        if self.f.prev_segment_ids.is_empty() {
            return 0;
        }
        let x_mis = (self.f.mi_cols - b.col).min(b.bw4);
        let y_mis = (self.f.mi_rows - b.row).min(b.bh4);
        let mut seg = 7;
        for y in 0..y_mis {
            for x in 0..x_mis {
                seg = seg.min(self.f.prev_segment_ids[self.f.mi(b.row + y, b.col + x)]);
            }
        }
        usize::from(seg)
    }

    /// `read_skip_mode()`.
    fn read_skip_mode(&mut self) -> Result<(), Error> {
        let seg = self.b.segment_id;
        if self.seg_feature_active(seg, SEG_LVL_SKIP)
            || self.seg_feature_active(seg, SEG_LVL_REF_FRAME)
            || self.seg_feature_active(seg, SEG_LVL_GLOBALMV)
            || self.f.fh.skip_mode_frames.is_none()
            || self.b.bw4 < 2
            || self.b.bh4 < 2
        {
            self.b.skip_mode = false;
            return Ok(());
        }
        let mut ctx = 0;
        if self.b.avail_u {
            ctx += usize::from(self.f.skip_modes[self.f.mi(self.b.row - 1, self.b.col)]);
        }
        if self.b.avail_l {
            ctx += usize::from(self.f.skip_modes[self.f.mi(self.b.row, self.b.col - 1)]);
        }
        self.b.skip_mode = self.sym(|c| &mut c.skip_mode[ctx])? == 1;
        Ok(())
    }

    /// `read_is_inter()`.
    fn read_is_inter(&mut self) -> Result<(), Error> {
        let seg = self.b.segment_id;
        self.b.is_inter = if self.b.skip_mode {
            true
        } else if self.seg_feature_active(seg, SEG_LVL_REF_FRAME) {
            self.f.fh.segmentation.feature_data[seg][SEG_LVL_REF_FRAME] != 0
        } else if self.seg_feature_active(seg, SEG_LVL_GLOBALMV) {
            true
        } else {
            let b = &self.b;
            let left_intra = b.left_ref[0] <= INTRA_FRAME;
            let above_intra = b.above_ref[0] <= INTRA_FRAME;
            let ctx = match (b.avail_u, b.avail_l) {
                (true, true) if left_intra && above_intra => 3,
                (true, true) => usize::from(left_intra || above_intra),
                (true, false) => 2 * usize::from(above_intra),
                (false, true) => 2 * usize::from(left_intra),
                (false, false) => 0,
            };
            self.sym(|c| &mut c.is_inter[ctx])? == 1
        };
        Ok(())
    }

    /// `inter_block_mode_info()`.
    fn inter_block_mode_info(&mut self) -> Result<(), Error> {
        self.read_ref_frames()?;
        let compound = self.b.ref_frame[1] > INTRA_FRAME;
        self.find_mv_stack(compound);
        let seg = self.b.segment_id;
        let s = &self.b.stack;
        let (new_ctx, zero_ctx, ref_ctx) = (s.new_mv_ctx, s.zero_mv_ctx, s.ref_mv_ctx);
        self.b.y_mode = if self.b.skip_mode {
            NEAREST_NEARESTMV
        } else if self.seg_feature_active(seg, SEG_LVL_SKIP)
            || self.seg_feature_active(seg, SEG_LVL_GLOBALMV)
        {
            GLOBALMV
        } else if compound {
            let ctx = usize::from(COMPOUND_MODE_CTX_MAP[ref_ctx >> 1][new_ctx.min(4)]);
            NEAREST_NEARESTMV + self.sym(|c| &mut c.compound_mode[ctx])? as u8
        } else if self.sym(|c| &mut c.new_mv[new_ctx])? == 0 {
            NEWMV
        } else if self.sym(|c| &mut c.zero_mv[zero_ctx])? == 0 {
            GLOBALMV
        } else if self.sym(|c| &mut c.ref_mv[ref_ctx])? == 0 {
            NEARESTMV
        } else {
            NEARMV
        };
        let y_mode = self.b.y_mode;
        let has_nearmv = matches!(y_mode, NEARMV | NEAR_NEARMV | NEAR_NEWMV | NEW_NEARMV);
        let range = if y_mode == NEWMV || y_mode == NEW_NEWMV {
            0..2
        } else if has_nearmv {
            1..3
        } else {
            0..0
        };
        self.b.ref_mv_idx = range.start;
        for idx in range {
            if self.b.stack.count > idx + 1 {
                let ctx = self.b.stack.drl_ctx[idx];
                if self.sym(|c| &mut c.drl_mode[ctx])? == 0 {
                    self.b.ref_mv_idx = idx;
                    break;
                }
                self.b.ref_mv_idx = idx + 1;
            }
        }
        self.assign_mv(compound)?;
        self.read_interintra_mode(compound)?;
        self.read_motion_mode(compound)?;
        self.read_compound_type(compound)?;
        let fh = self.f.fh;
        if fh.interpolation_filter == SWITCHABLE {
            let dual = self.f.seq.enable_dual_filter;
            for dir in 0..1 + usize::from(dual) {
                self.b.interp_filter[dir] = if self.needs_interp_filter() {
                    let ctx = self.interp_filter_ctx(dir);
                    self.sym(|c| &mut c.interp_filter[ctx])? as u8
                } else {
                    0 // EIGHTTAP
                };
            }
            if !dual {
                self.b.interp_filter[1] = self.b.interp_filter[0];
            }
        } else {
            self.b.interp_filter = [fh.interpolation_filter; 2];
        }
        Ok(())
    }

    fn needs_interp_filter(&self) -> bool {
        let b = &self.b;
        let large = b.bw4.min(b.bh4) >= 2;
        if b.skip_mode || b.motion_mode == LOCALWARP {
            false
        } else if large && b.y_mode == GLOBALMV {
            self.gm_type(b.ref_frame[0]) == TRANSLATION
        } else if large && b.y_mode == GLOBAL_GLOBALMV {
            self.gm_type(b.ref_frame[0]) == TRANSLATION
                || self.gm_type(b.ref_frame[1]) == TRANSLATION
        } else {
            true
        }
    }

    fn interp_filter_ctx(&self, dir: usize) -> usize {
        let b = &self.b;
        let mut ctx = ((dir & 1) * 2 + usize::from(b.ref_frame[1] > INTRA_FRAME)) * 4;
        let neighbour = |r: usize, c: usize| {
            let i = self.f.mi(r, c);
            if self.f.ref_frames[i].contains(&b.ref_frame[0]) {
                usize::from(self.f.interp_filters[i][dir])
            } else {
                3
            }
        };
        let left = if b.avail_l {
            neighbour(b.row, b.col - 1)
        } else {
            3
        };
        let above = if b.avail_u {
            neighbour(b.row - 1, b.col)
        } else {
            3
        };
        ctx += if left == above {
            left
        } else if left == 3 {
            above
        } else if above == 3 {
            left
        } else {
            3
        };
        ctx
    }

    /// `count_refs()` summed over `frames`.
    fn count_refs(&self, frames: &[i8]) -> u32 {
        let b = &self.b;
        let mut c = 0;
        for &f in frames {
            if b.avail_u {
                c += b.above_ref.iter().filter(|&&r| r == f).count() as u32;
            }
            if b.avail_l {
                c += b.left_ref.iter().filter(|&&r| r == f).count() as u32;
            }
        }
        c
    }

    fn ref_ctx(&self, a: &[i8], b: &[i8]) -> usize {
        ref_count_ctx(self.count_refs(a), self.count_refs(b))
    }

    /// `read_ref_frames()`.
    fn read_ref_frames(&mut self) -> Result<(), Error> {
        const FWD: [i8; 4] = [LAST_FRAME, LAST2_FRAME, LAST3_FRAME, GOLDEN_FRAME];
        const BWD: [i8; 3] = [BWDREF_FRAME, ALTREF2_FRAME, ALTREF_FRAME];
        let seg = self.b.segment_id;
        if self.b.skip_mode {
            let frames = self.f.fh.skip_mode_frames.unwrap_or_default();
            self.b.ref_frame = frames.map(|f| f as i8);
            return Ok(());
        }
        if self.seg_feature_active(seg, SEG_LVL_REF_FRAME) {
            let data = self.f.fh.segmentation.feature_data[seg][SEG_LVL_REF_FRAME];
            self.b.ref_frame = [data as i8, NONE];
            return Ok(());
        }
        if self.seg_feature_active(seg, SEG_LVL_SKIP)
            || self.seg_feature_active(seg, SEG_LVL_GLOBALMV)
        {
            self.b.ref_frame = [LAST_FRAME, NONE];
            return Ok(());
        }
        let compound = self.f.fh.reference_select && self.b.bw4.min(self.b.bh4) >= 2 && {
            let ctx = self.comp_mode_ctx();
            self.sym(|c| &mut c.comp_mode[ctx])? == 1
        };
        // The bits of comp_ref, single_ref_p1… (`cdf` picks the CDF from its context).
        let ctx_l12_l3g = self.ref_ctx(&[LAST_FRAME, LAST2_FRAME], &[LAST3_FRAME, GOLDEN_FRAME]);
        let ctx_l_l2 = self.ref_ctx(&[LAST_FRAME], &[LAST2_FRAME]);
        let ctx_l3_g = self.ref_ctx(&[LAST3_FRAME], &[GOLDEN_FRAME]);
        let ctx_b_a2_a = self.ref_ctx(&[BWDREF_FRAME, ALTREF2_FRAME], &[ALTREF_FRAME]);
        let ctx_b_a2 = self.ref_ctx(&[BWDREF_FRAME], &[ALTREF2_FRAME]);
        let ctx_fwd_bwd = self.ref_ctx(&FWD, &BWD);
        self.b.ref_frame = if compound {
            let ctx = self.comp_ref_type_ctx();
            if self.sym(|c| &mut c.comp_ref_type[ctx])? == 0 {
                // UNIDIR_COMP_REFERENCE
                if self.sym(|c| &mut c.uni_comp_ref[ctx_fwd_bwd][0])? == 1 {
                    [BWDREF_FRAME, ALTREF_FRAME]
                } else {
                    let ctx = self.ref_ctx(&[LAST2_FRAME], &[LAST3_FRAME, GOLDEN_FRAME]);
                    if self.sym(|c| &mut c.uni_comp_ref[ctx][1])? == 1 {
                        if self.sym(|c| &mut c.uni_comp_ref[ctx_l3_g][2])? == 1 {
                            [LAST_FRAME, GOLDEN_FRAME]
                        } else {
                            [LAST_FRAME, LAST3_FRAME]
                        }
                    } else {
                        [LAST_FRAME, LAST2_FRAME]
                    }
                }
            } else {
                let first = if self.sym(|c| &mut c.comp_ref[ctx_l12_l3g][0])? == 0 {
                    if self.sym(|c| &mut c.comp_ref[ctx_l_l2][1])? == 1 {
                        LAST2_FRAME
                    } else {
                        LAST_FRAME
                    }
                } else if self.sym(|c| &mut c.comp_ref[ctx_l3_g][2])? == 1 {
                    GOLDEN_FRAME
                } else {
                    LAST3_FRAME
                };
                let second = if self.sym(|c| &mut c.comp_bwd_ref[ctx_b_a2_a][0])? == 0 {
                    if self.sym(|c| &mut c.comp_bwd_ref[ctx_b_a2][1])? == 1 {
                        ALTREF2_FRAME
                    } else {
                        BWDREF_FRAME
                    }
                } else {
                    ALTREF_FRAME
                };
                [first, second]
            }
        } else {
            let single = |s: &mut Self, ctx: usize, n: usize| -> Result<bool, Error> {
                Ok(s.sym(|c| &mut c.single_ref[ctx][n])? == 1)
            };
            let r = if single(self, ctx_fwd_bwd, 0)? {
                if !single(self, ctx_b_a2_a, 1)? {
                    if single(self, ctx_b_a2, 5)? {
                        ALTREF2_FRAME
                    } else {
                        BWDREF_FRAME
                    }
                } else {
                    ALTREF_FRAME
                }
            } else if single(self, ctx_l12_l3g, 2)? {
                if single(self, ctx_l3_g, 4)? {
                    GOLDEN_FRAME
                } else {
                    LAST3_FRAME
                }
            } else if single(self, ctx_l_l2, 3)? {
                LAST2_FRAME
            } else {
                LAST_FRAME
            };
            [r, NONE]
        };
        Ok(())
    }

    fn comp_mode_ctx(&self) -> usize {
        let b = &self.b;
        let (above, left) = (b.above_ref, b.left_ref);
        let (above_single, left_single) = (above[1] <= INTRA_FRAME, left[1] <= INTRA_FRAME);
        let (above_intra, left_intra) = (above[0] <= INTRA_FRAME, left[0] <= INTRA_FRAME);
        match (b.avail_u, b.avail_l) {
            (true, true) => {
                if above_single && left_single {
                    usize::from(check_backward(above[0]) ^ check_backward(left[0]))
                } else if above_single {
                    2 + usize::from(check_backward(above[0]) || above_intra)
                } else if left_single {
                    2 + usize::from(check_backward(left[0]) || left_intra)
                } else {
                    4
                }
            }
            (true, false) if above_single => usize::from(check_backward(above[0])),
            (false, true) if left_single => usize::from(check_backward(left[0])),
            (true, false) | (false, true) => 3,
            (false, false) => 1,
        }
    }

    fn comp_ref_type_ctx(&self) -> usize {
        let b = &self.b;
        let ([above0, above1], [left0, left1]) = (b.above_ref, b.left_ref);
        let (above_intra, left_intra) = (above0 <= INTRA_FRAME, left0 <= INTRA_FRAME);
        let above_comp = b.avail_u && !above_intra && above1 > INTRA_FRAME;
        let left_comp = b.avail_l && !left_intra && left1 > INTRA_FRAME;
        let above_uni = above_comp && is_samedir_ref_pair(above0, above1);
        let left_uni = left_comp && is_samedir_ref_pair(left0, left1);
        if b.avail_u && !above_intra && b.avail_l && !left_intra {
            let samedir = usize::from(is_samedir_ref_pair(above0, left0));
            if !above_comp && !left_comp {
                1 + 2 * samedir
            } else if !above_comp {
                if left_uni { 3 + samedir } else { 1 }
            } else if !left_comp {
                if above_uni { 3 + samedir } else { 1 }
            } else if !above_uni && !left_uni {
                0
            } else if !above_uni || !left_uni {
                2
            } else {
                3 + usize::from((above0 == BWDREF_FRAME) == (left0 == BWDREF_FRAME))
            }
        } else if b.avail_u && b.avail_l {
            if above_comp {
                1 + 2 * usize::from(above_uni)
            } else if left_comp {
                1 + 2 * usize::from(left_uni)
            } else {
                2
            }
        } else if above_comp {
            4 * usize::from(above_uni)
        } else if left_comp {
            4 * usize::from(left_uni)
        } else {
            2
        }
    }

    /// `get_mode( refList )`.
    fn get_mode(&self, list: usize) -> u8 {
        let y = self.b.y_mode;
        if list == 0 {
            match y {
                _ if y < NEAREST_NEARESTMV => y,
                NEW_NEWMV | NEW_NEARESTMV | NEW_NEARMV => NEWMV,
                NEAREST_NEARESTMV | NEAREST_NEWMV => NEARESTMV,
                NEAR_NEARMV | NEAR_NEWMV => NEARMV,
                _ => GLOBALMV,
            }
        } else {
            match y {
                NEW_NEWMV | NEAREST_NEWMV | NEAR_NEWMV => NEWMV,
                NEAREST_NEARESTMV | NEW_NEARESTMV => NEARESTMV,
                NEAR_NEARMV | NEW_NEARMV => NEARMV,
                _ => GLOBALMV,
            }
        }
    }

    /// `assign_mv( isCompound )` of an inter frame's block.
    fn assign_mv(&mut self, compound: bool) -> Result<(), Error> {
        for i in 0..1 + usize::from(compound) {
            let mode = self.get_mode(i);
            let s = &self.b.stack;
            let pred = if mode == GLOBALMV {
                s.global_mvs[i]
            } else {
                let pos = if mode == NEARESTMV || (mode == NEWMV && s.count <= 1) {
                    0
                } else {
                    self.b.ref_mv_idx
                };
                s.mvs[pos][i]
            };
            self.b.mv[i] = if mode == NEWMV {
                let diff = self.read_mv(0)?;
                [pred[0] + diff[0], pred[1] + diff[1]]
            } else {
                pred
            };
        }
        Ok(())
    }

    /// `find_mv_stack( 0 )` and `assign_mv( 0 )` of an intra block copy block.
    pub(super) fn intrabc_mv(&mut self) -> Result<[i32; 2], Error> {
        self.find_mv_stack(false);
        let s = &self.b.stack;
        let mut pred = s.mvs[0][0];
        if pred == [0, 0] {
            pred = s.mvs[1][0];
        }
        if pred == [0, 0] {
            let sb4 = usize::from(NUM_4X4_BLOCKS_HIGH[self.sb_size()]) as i32;
            pred = if (self.b.row as i32) - sb4 < self.row_start as i32 {
                [0, -(sb4 * 4 + INTRABC_DELAY_PIXELS) * 8]
            } else {
                [-(sb4 * 4 * 8), 0]
            };
        }
        let diff = self.read_mv(MV_INTRABC_CONTEXT)?;
        Ok([pred[0] + diff[0], pred[1] + diff[1]])
    }

    /// `read_mv()`: the difference to the predicted motion vector.
    fn read_mv(&mut self, ctx: usize) -> Result<[i32; 2], Error> {
        let joint = self.sym(|c| &mut c.mv_joint[ctx])?;
        let mut diff = [0; 2];
        // MV_JOINT_HZVNZ (2) and MV_JOINT_HNZVNZ (3) code a row; MV_JOINT_HNZVZ (1) and 3 a column.
        if joint == 2 || joint == 3 {
            diff[0] = self.read_mv_component(ctx, 0)?;
        }
        if joint == 1 || joint == 3 {
            diff[1] = self.read_mv_component(ctx, 1)?;
        }
        Ok(diff)
    }

    fn read_mv_component(&mut self, ctx: usize, comp: usize) -> Result<i32, Error> {
        let (integer, hp) = (
            self.f.fh.force_integer_mv,
            self.f.fh.allow_high_precision_mv,
        );
        let sign = self.sym(|c| &mut c.mv_sign[ctx][comp])? == 1;
        let class = self.sym(|c| &mut c.mv_class[ctx][comp])?;
        let mag = if class == 0 {
            let bit = self.sym(|c| &mut c.mv_class0_bit[ctx][comp])?;
            let fr = if integer {
                3
            } else {
                self.sym(|c| &mut c.mv_class0_fr[ctx][comp][bit])?
            };
            let hp = if hp {
                self.sym(|c| &mut c.mv_class0_hp[ctx][comp])?
            } else {
                1
            };
            (((bit << 3) | (fr << 1) | hp) + 1) as i32
        } else {
            let mut d = 0;
            for i in 0..class {
                d |= self.sym(|c| &mut c.mv_bit[ctx][comp][i])? << i;
            }
            let fr = if integer {
                3
            } else {
                self.sym(|c| &mut c.mv_fr[ctx][comp])?
            };
            let hp = if hp {
                self.sym(|c| &mut c.mv_hp[ctx][comp])?
            } else {
                1
            };
            ((2 << (class + 2)) + ((d << 3) | (fr << 1) | hp) + 1) as i32
        };
        Ok(if sign { -mag } else { mag })
    }

    /// `read_interintra_mode( isCompound )`.
    fn read_interintra_mode(&mut self, compound: bool) -> Result<(), Error> {
        let size = self.b.size;
        if !self.b.skip_mode
            && self.f.seq.enable_interintra_compound
            && !compound
            && (BLOCK_8X8..=BLOCK_32X32).contains(&size)
        {
            let ctx = usize::from(SIZE_GROUP[size]) - 1;
            self.b.interintra = self.sym(|c| &mut c.inter_intra[ctx])? == 1;
            if self.b.interintra {
                self.b.interintra_mode = self.sym(|c| &mut c.inter_intra_mode[ctx])? as u8;
                self.b.ref_frame[1] = INTRA_FRAME;
                self.b.wedge_interintra = self.sym(|c| &mut c.wedge_inter_intra[size])? == 1;
                if self.b.wedge_interintra {
                    self.b.wedge_index = self.sym(|c| &mut c.wedge_index[size])?;
                    self.b.wedge_sign = 0;
                }
            }
        }
        Ok(())
    }

    /// `read_motion_mode( isCompound )`.
    fn read_motion_mode(&mut self, compound: bool) -> Result<(), Error> {
        self.b.motion_mode = SIMPLE;
        let fh = self.f.fh;
        let b = &self.b;
        if b.skip_mode || !fh.is_motion_mode_switchable || b.bw4.min(b.bh4) < 2 {
            return Ok(());
        }
        if !fh.force_integer_mv
            && (b.y_mode == GLOBALMV || b.y_mode == GLOBAL_GLOBALMV)
            && self.gm_type(b.ref_frame[0]) > TRANSLATION
        {
            return Ok(());
        }
        if compound || b.ref_frame[1] == INTRA_FRAME || !self.has_overlappable_candidates() {
            return Ok(());
        }
        self.find_warp_samples();
        let size = self.b.size;
        if fh.force_integer_mv
            || self.b.num_samples == 0
            || !fh.allow_warped_motion
            || self.f.is_scaled(self.b.ref_frame[0])
        {
            if self.sym(|c| &mut c.use_obmc[size])? == 1 {
                self.b.motion_mode = OBMC;
            }
        } else {
            self.b.motion_mode = self.sym(|c| &mut c.motion_mode[size])? as u8;
        }
        Ok(())
    }

    /// `read_compound_type( isCompound )`.
    fn read_compound_type(&mut self, compound: bool) -> Result<(), Error> {
        self.b.comp_group_idx = 0;
        self.b.compound_idx = 1;
        self.b.compound_type = COMPOUND_AVERAGE;
        if self.b.skip_mode {
            return Ok(());
        }
        if !compound {
            if self.b.interintra {
                self.b.compound_type = if self.b.wedge_interintra {
                    COMPOUND_WEDGE
                } else {
                    COMPOUND_INTRA
                };
            }
            return Ok(());
        }
        let size = self.b.size;
        let seq = self.f.seq;
        if seq.enable_masked_compound {
            let ctx = self.comp_group_idx_ctx();
            self.b.comp_group_idx = self.sym(|c| &mut c.comp_group_idx[ctx])? as u8;
        }
        if self.b.comp_group_idx == 0 {
            if seq.enable_jnt_comp {
                let ctx = self.compound_idx_ctx();
                self.b.compound_idx = self.sym(|c| &mut c.compound_idx[ctx])? as u8;
                if self.b.compound_idx == 0 {
                    self.b.compound_type = COMPOUND_DISTANCE;
                }
            }
        } else if WEDGE_BITS[size] == 0 {
            self.b.compound_type = COMPOUND_DIFFWTD;
        } else {
            self.b.compound_type = self.sym(|c| &mut c.compound_type[size])? as u8;
        }
        if self.b.compound_type == COMPOUND_WEDGE {
            self.b.wedge_index = self.sym(|c| &mut c.wedge_index[size])?;
            self.b.wedge_sign = self.lit(1)? as usize;
        } else if self.b.compound_type == COMPOUND_DIFFWTD {
            self.b.mask_type = self.lit(1)? == 1;
        }
        Ok(())
    }

    fn comp_group_idx_ctx(&self) -> usize {
        let b = &self.b;
        let mut ctx = 0;
        let mut add = |avail: bool, refs: [i8; 2], r: usize, c: usize| {
            if avail {
                if refs[1] > INTRA_FRAME {
                    ctx += usize::from(self.f.comp_group_idxs[self.f.mi(r, c)]);
                } else if refs[0] == ALTREF_FRAME {
                    ctx += 3;
                }
            }
        };
        add(b.avail_u, b.above_ref, b.row.wrapping_sub(1), b.col);
        add(b.avail_l, b.left_ref, b.row, b.col.wrapping_sub(1));
        ctx.min(5)
    }

    fn compound_idx_ctx(&self) -> usize {
        let b = &self.b;
        let fh = self.f.fh;
        let dist =
            |r: i8| relative_dist(self.f.seq, fh.order_hints[r as usize], fh.order_hint).abs();
        let mut ctx = if dist(b.ref_frame[0]) == dist(b.ref_frame[1]) {
            3
        } else {
            0
        };
        let mut add = |avail: bool, refs: [i8; 2], r: usize, c: usize| {
            if avail {
                if refs[1] > INTRA_FRAME {
                    ctx += usize::from(self.f.compound_idxs[self.f.mi(r, c)]);
                } else if refs[0] == ALTREF_FRAME {
                    ctx += 1;
                }
            }
        };
        add(b.avail_u, b.above_ref, b.row.wrapping_sub(1), b.col);
        add(b.avail_l, b.left_ref, b.row, b.col.wrapping_sub(1));
        ctx
    }

    // §7.10.2: motion vector prediction.

    /// `lower_mv_precision()`.
    fn lower_precision(&self, mv: &mut [i32; 2]) {
        let fh = self.f.fh;
        if fh.allow_high_precision_mv {
            return;
        }
        for v in mv {
            if fh.force_integer_mv {
                let a = ((v.abs() + 3) >> 3) << 3;
                *v = if *v > 0 { a } else { -a };
            } else if *v & 1 != 0 {
                *v += if *v > 0 { -1 } else { 1 };
            }
        }
    }

    /// The setup global mv process (§7.10.2.1).
    fn global_mv(&self, r: i8) -> [i32; 2] {
        let typ = self.gm_type(r);
        let mut mv = if typ == 0 {
            [0, 0]
        } else {
            let p = &self.f.fh.gm_params[r as usize];
            if typ == TRANSLATION {
                [p[0] >> 13, p[1] >> 13]
            } else {
                let b = &self.b;
                let x = (b.col * 4 + b.bw4 * 2) as i32 - 1;
                let y = (b.row * 4 + b.bh4 * 2) as i32 - 1;
                let xc = (p[2] - (1 << 16)) * x + p[3] * y + p[0];
                let yc = p[4] * x + (p[5] - (1 << 16)) * y + p[1];
                if self.f.fh.allow_high_precision_mv {
                    [round2_signed(yc, 13), round2_signed(xc, 13)]
                } else {
                    [round2_signed(yc, 14) * 2, round2_signed(xc, 14) * 2]
                }
            }
        };
        self.lower_precision(&mut mv);
        mv
    }

    /// The find MV stack process (§7.10.2), into `self.b.stack`.
    fn find_mv_stack(&mut self, compound: bool) {
        let (bw4, bh4) = (self.b.bw4, self.b.bh4);
        let mut s = MvStack::default();
        s.global_mvs[0] = self.global_mv(self.b.ref_frame[0]);
        if compound {
            s.global_mvs[1] = self.global_mv(self.b.ref_frame[1]);
        }
        self.scan_row(&mut s, -1, compound);
        let mut found_above = std::mem::take(&mut s.found_match);
        self.scan_col(&mut s, -1, compound);
        let mut found_left = std::mem::take(&mut s.found_match);
        if bw4.max(bh4) <= 16 {
            self.scan_point(&mut s, -1, bw4 as isize, compound);
        }
        found_above |= std::mem::take(&mut s.found_match);
        let close_matches = usize::from(found_above) + usize::from(found_left);
        let num_nearest = s.count;
        let num_new = s.new_mv_count;
        for w in &mut s.weights[..num_nearest] {
            *w += REF_CAT_LEVEL;
        }
        if self.f.fh.use_ref_frame_mvs {
            self.temporal_scan(&mut s, compound);
        }
        self.scan_point(&mut s, -1, -1, compound);
        found_above |= std::mem::take(&mut s.found_match);
        self.scan_row(&mut s, -3, compound);
        found_above |= std::mem::take(&mut s.found_match);
        self.scan_col(&mut s, -3, compound);
        found_left |= std::mem::take(&mut s.found_match);
        if bh4 > 1 {
            self.scan_row(&mut s, -5, compound);
        }
        found_above |= std::mem::take(&mut s.found_match);
        if bw4 > 1 {
            self.scan_col(&mut s, -5, compound);
        }
        found_left |= std::mem::take(&mut s.found_match);
        let total_matches = usize::from(found_above) + usize::from(found_left);
        s.sort(0, num_nearest);
        s.sort(num_nearest, s.count);
        if s.count < 2 {
            self.extra_search(&mut s, compound);
        }
        // The context and clamping process (§7.10.2.14).
        for idx in 0..s.count {
            s.drl_ctx[idx] = if idx + 1 < s.count {
                match (
                    s.weights[idx] >= REF_CAT_LEVEL,
                    s.weights[idx + 1] >= REF_CAT_LEVEL,
                ) {
                    (true, false) => 1,
                    (true, true) => 0,
                    (false, _) => 2,
                }
            } else {
                0
            };
        }
        let (bw, bh) = (4 * bw4 as i32, 4 * bh4 as i32);
        for entry in &mut s.mvs[..s.count] {
            for mv in &mut entry[..1 + usize::from(compound)] {
                mv[0] = self.clamp_mv_row(mv[0], MV_BORDER + bh * 8);
                mv[1] = self.clamp_mv_col(mv[1], MV_BORDER + bw * 8);
            }
        }
        (s.new_mv_ctx, s.ref_mv_ctx) = match close_matches {
            0 => (total_matches.min(1), total_matches),
            1 => (3 - usize::from(num_new > 0), 2 + total_matches),
            _ => (5 - usize::from(num_new > 0), 5),
        };
        self.b.stack = s;
    }

    /// The scan row process (§7.10.2.2).
    fn scan_row(&self, s: &mut MvStack, delta_row: isize, compound: bool) {
        let (row, col, bw4) = (self.b.row as isize, self.b.col as isize, self.b.bw4);
        let end4 = bw4.min(self.f.mi_cols - self.b.col).min(16);
        let (mut delta_row, mut delta_col) = (delta_row, 0);
        let use_step16 = bw4 >= 16;
        if delta_row.abs() > 1 {
            delta_row += row & 1;
            delta_col = 1 - (col & 1);
        }
        let mut i = 0;
        while i < end4 {
            let (r, c) = (row + delta_row, col + delta_col + i as isize);
            if !self.is_inside(r, c) {
                break;
            }
            let cand = self.f.mi(r as usize, c as usize);
            let mut len = bw4.min(usize::from(
                NUM_4X4_BLOCKS_WIDE[usize::from(self.f.mi_sizes[cand])],
            ));
            if delta_row.abs() > 1 {
                len = len.max(2);
            }
            if use_step16 {
                len = len.max(4);
            }
            self.add_ref_mv_candidate(s, cand, compound, 2 * len as u32);
            i += len;
        }
    }

    /// The scan col process (§7.10.2.3).
    fn scan_col(&self, s: &mut MvStack, delta_col: isize, compound: bool) {
        let (row, col, bh4) = (self.b.row as isize, self.b.col as isize, self.b.bh4);
        let end4 = bh4.min(self.f.mi_rows - self.b.row).min(16);
        let (mut delta_row, mut delta_col) = (0, delta_col);
        let use_step16 = bh4 >= 16;
        if delta_col.abs() > 1 {
            delta_row = 1 - (row & 1);
            delta_col += col & 1;
        }
        let mut i = 0;
        while i < end4 {
            let (r, c) = (row + delta_row + i as isize, col + delta_col);
            if !self.is_inside(r, c) {
                break;
            }
            let cand = self.f.mi(r as usize, c as usize);
            let mut len = bh4.min(usize::from(
                NUM_4X4_BLOCKS_HIGH[usize::from(self.f.mi_sizes[cand])],
            ));
            if delta_col.abs() > 1 {
                len = len.max(2);
            }
            if use_step16 {
                len = len.max(4);
            }
            self.add_ref_mv_candidate(s, cand, compound, 2 * len as u32);
            i += len;
        }
    }

    /// The scan point process (§7.10.2.4): only where a block of this frame was decoded.
    fn scan_point(&self, s: &mut MvStack, delta_row: isize, delta_col: isize, compound: bool) {
        let (r, c) = (
            self.b.row as isize + delta_row,
            self.b.col as isize + delta_col,
        );
        if self.is_inside(r, c) {
            let cand = self.f.mi(r as usize, c as usize);
            if self.f.ref_frames[cand][0] != REF_UNWRITTEN {
                self.add_ref_mv_candidate(s, cand, compound, 4);
            }
        }
    }

    /// The temporal scan process (§7.10.2.5).
    fn temporal_scan(&self, s: &mut MvStack, compound: bool) {
        let (bw4, bh4) = (self.b.bw4 as isize, self.b.bh4 as isize);
        let step_w = if bw4 >= 16 { 4 } else { 2 };
        let step_h = if bh4 >= 16 { 4 } else { 2 };
        for dr in (0..bh4.min(16)).step_by(step_h) {
            for dc in (0..bw4.min(16)).step_by(step_w) {
                self.add_tpl_ref_mv(s, dr, dc, compound);
            }
        }
        if (2..16).contains(&bh4) && (2..16).contains(&bw4) {
            for (dr, dc) in [(bh4, -2), (bh4, bw4), (bh4 - 2, bw4)] {
                let r = (self.b.row & 15) as isize + dr;
                let c = (self.b.col & 15) as isize + dc;
                if (0..16).contains(&r) && (0..16).contains(&c) {
                    self.add_tpl_ref_mv(s, dr, dc, compound);
                }
            }
        }
    }

    /// The temporal sample process (§7.10.2.6).
    fn add_tpl_ref_mv(&self, s: &mut MvStack, dr: isize, dc: isize, compound: bool) {
        let mv_row = (self.b.row as isize + dr) | 1;
        let mv_col = (self.b.col as isize + dc) | 1;
        if !self.is_inside(mv_row, mv_col) {
            return;
        }
        let at = (mv_row as usize >> 1) * (self.f.mi_cols >> 1) + (mv_col as usize >> 1);
        let center = dr == 0 && dc == 0;
        if center {
            s.zero_mv_ctx = 1;
        }
        let mut mvs = [[0; 2]; 2];
        for list in 0..1 + usize::from(compound) {
            let mv = self.f.motion_field[self.b.ref_frame[list] as usize][at];
            if mv[0] == INVALID_MV {
                return;
            }
            mvs[list] = mv;
            self.lower_precision(&mut mvs[list]);
        }
        if center {
            let far = |l: usize| {
                (mvs[l][0] - s.global_mvs[l][0]).abs() >= 16
                    || (mvs[l][1] - s.global_mvs[l][1]).abs() >= 16
            };
            s.zero_mv_ctx = usize::from(far(0) || (compound && far(1)));
        }
        s.add(mvs, compound, 2);
    }

    /// The add reference motion vector process (§7.10.2.7) and the search stack processes.
    fn add_ref_mv_candidate(&self, s: &mut MvStack, cand: usize, compound: bool, weight: u32) {
        if !self.f.is_inters[cand] {
            return;
        }
        let refs = self.f.ref_frames[cand];
        let mode = self.f.y_modes[cand];
        let rf = self.b.ref_frame;
        if !compound {
            for list in 0..2 {
                if refs[list] == rf[0] {
                    // The search stack process (§7.10.2.8).
                    let size = usize::from(self.f.mi_sizes[cand]);
                    let large = NUM_4X4_BLOCKS_WIDE[size].min(NUM_4X4_BLOCKS_HIGH[size]) >= 2;
                    let mut mv = if (mode == GLOBALMV || mode == GLOBAL_GLOBALMV)
                        && self.gm_type(rf[0]) > TRANSLATION
                        && large
                    {
                        s.global_mvs[0]
                    } else {
                        self.f.mvs[cand][list]
                    };
                    self.lower_precision(&mut mv);
                    if has_newmv(mode) {
                        s.new_mv_count += 1;
                    }
                    s.found_match = true;
                    s.add([mv, [0; 2]], false, weight);
                }
            }
        } else if refs == rf {
            // The compound search stack process (§7.10.2.9).
            let mut mvs = self.f.mvs[cand];
            for (list, mv) in mvs.iter_mut().enumerate() {
                if mode == GLOBAL_GLOBALMV && self.gm_type(rf[list]) > TRANSLATION {
                    *mv = s.global_mvs[list];
                }
                self.lower_precision(mv);
            }
            s.found_match = true;
            s.add(mvs, true, weight);
            if has_newmv(mode) {
                s.new_mv_count += 1;
            }
        }
    }

    /// The extra search process (§7.10.2.12).
    fn extra_search(&self, s: &mut MvStack, compound: bool) {
        let mut id = [[[0; 2]; 2]; 2];
        let mut diff = [[[0; 2]; 2]; 2];
        let (mut id_count, mut diff_count) = ([0; 2], [0; 2]);
        let b = &self.b;
        let w4 = b.bw4.min(16).min(self.f.mi_cols - b.col);
        let h4 = b.bh4.min(16).min(self.f.mi_rows - b.row);
        let num4x4 = w4.min(h4);
        let bias = &self.f.fh.ref_frame_sign_bias;
        for pass in 0..2 {
            let mut idx = 0;
            while idx < num4x4 && s.count < 2 {
                let (r, c) = if pass == 0 {
                    (b.row as isize - 1, (b.col + idx) as isize)
                } else {
                    ((b.row + idx) as isize, b.col as isize - 1)
                };
                if !self.is_inside(r, c) {
                    break;
                }
                let cand = self.f.mi(r as usize, c as usize);
                // The add extra mv candidate process (§7.10.2.13).
                for cand_list in 0..2 {
                    let cand_ref = self.f.ref_frames[cand][cand_list];
                    if cand_ref <= INTRA_FRAME {
                        continue;
                    }
                    let cand_mv = self.f.mvs[cand][cand_list];
                    let flipped = |list_ref: i8| {
                        if bias[cand_ref as usize] != bias[list_ref as usize] {
                            [-cand_mv[0], -cand_mv[1]]
                        } else {
                            cand_mv
                        }
                    };
                    if compound {
                        for list in 0..2 {
                            if cand_ref == b.ref_frame[list] && id_count[list] < 2 {
                                id[list][id_count[list]] = cand_mv;
                                id_count[list] += 1;
                            } else if diff_count[list] < 2 {
                                diff[list][diff_count[list]] = flipped(b.ref_frame[list]);
                                diff_count[list] += 1;
                            }
                        }
                    } else {
                        let mv = flipped(b.ref_frame[0]);
                        if !s.mvs[..s.count].iter().any(|m| m[0] == mv) {
                            s.mvs[s.count][0] = mv;
                            s.weights[s.count] = 2;
                            s.count += 1;
                        }
                    }
                }
                let size = usize::from(self.f.mi_sizes[cand]);
                idx += usize::from(if pass == 0 {
                    NUM_4X4_BLOCKS_WIDE[size]
                } else {
                    NUM_4X4_BLOCKS_HIGH[size]
                });
            }
        }
        if compound {
            let mut combined = [[[0; 2]; 2]; 2];
            for list in 0..2 {
                let mut n = 0;
                for &mv in &id[list][..id_count[list]] {
                    combined[n][list] = mv;
                    n += 1;
                }
                for &mv in &diff[list][..diff_count[list]] {
                    if n == 2 {
                        break;
                    }
                    combined[n][list] = mv;
                    n += 1;
                }
                while n < 2 {
                    combined[n][list] = s.global_mvs[list];
                    n += 1;
                }
            }
            if s.count == 1 {
                s.mvs[1] = if combined[0] == s.mvs[0] {
                    combined[1]
                } else {
                    combined[0]
                };
                s.weights[1] = 2;
                s.count = 2;
            } else {
                for c in combined {
                    s.mvs[s.count] = c;
                    s.weights[s.count] = 2;
                    s.count += 1;
                }
            }
        } else {
            for idx in s.count..2 {
                s.mvs[idx][0] = s.global_mvs[0];
            }
        }
    }

    /// `clamp_mv_row()`.
    fn clamp_mv_row(&self, mv: i32, border: i32) -> i32 {
        let (row, bh4) = (self.b.row as i32, self.b.bh4 as i32);
        let to_top = -(row * 4 * 8);
        let to_bottom = (self.f.mi_rows as i32 - bh4 - row) * 4 * 8;
        mv.clamp(to_top - border, to_bottom + border)
    }

    /// `clamp_mv_col()`.
    fn clamp_mv_col(&self, mv: i32, border: i32) -> i32 {
        let (col, bw4) = (self.b.col as i32, self.b.bw4 as i32);
        let to_left = -(col * 4 * 8);
        let to_right = (self.f.mi_cols as i32 - bw4 - col) * 4 * 8;
        mv.clamp(to_left - border, to_right + border)
    }

    /// The has overlappable candidates process (§7.10.3).
    fn has_overlappable_candidates(&self) -> bool {
        let b = &self.b;
        let inter = |r: usize, c: usize| self.f.ref_frames[self.f.mi(r, c)][0] > INTRA_FRAME;
        (b.avail_u
            && (b.col..self.f.mi_cols.min(b.col + b.bw4))
                .step_by(2)
                .any(|x4| inter(b.row - 1, x4 | 1)))
            || (b.avail_l
                && (b.row..self.f.mi_rows.min(b.row + b.bh4))
                    .step_by(2)
                    .any(|y4| inter(y4 | 1, b.col - 1)))
    }

    /// The find warp samples process (§7.10.4).
    fn find_warp_samples(&mut self) {
        self.b.num_samples = 0;
        self.b.num_samples_scanned = 0;
        let (row, col, w4, h4) = (self.b.row, self.b.col, self.b.bw4, self.b.bh4);
        let (mut do_top_left, mut do_top_right) = (true, true);
        if self.b.avail_u {
            let src_w = usize::from(
                NUM_4X4_BLOCKS_WIDE[usize::from(self.f.mi_sizes[self.f.mi(row - 1, col)])],
            );
            if w4 <= src_w {
                let col_offset = -((col & (src_w - 1)) as isize);
                if col_offset < 0 {
                    do_top_left = false;
                }
                if col_offset + src_w as isize > w4 as isize {
                    do_top_right = false;
                }
                self.add_sample(-1, 0);
            } else {
                let mut i = 0;
                while i < w4.min(self.f.mi_cols - col) {
                    let size = usize::from(self.f.mi_sizes[self.f.mi(row - 1, col + i)]);
                    self.add_sample(-1, i as isize);
                    i += w4.min(usize::from(NUM_4X4_BLOCKS_WIDE[size]));
                }
            }
        }
        if self.b.avail_l {
            let src_h = usize::from(
                NUM_4X4_BLOCKS_HIGH[usize::from(self.f.mi_sizes[self.f.mi(row, col - 1)])],
            );
            if h4 <= src_h {
                if row & (src_h - 1) != 0 {
                    do_top_left = false;
                }
                self.add_sample(0, -1);
            } else {
                let mut i = 0;
                while i < h4.min(self.f.mi_rows - row) {
                    let size = usize::from(self.f.mi_sizes[self.f.mi(row + i, col - 1)]);
                    self.add_sample(i as isize, -1);
                    i += h4.min(usize::from(NUM_4X4_BLOCKS_HIGH[size]));
                }
            }
        }
        if do_top_left {
            self.add_sample(-1, -1);
        }
        if do_top_right && w4.max(h4) <= 16 {
            self.add_sample(-1, w4 as isize);
        }
        if self.b.num_samples == 0 && self.b.num_samples_scanned > 0 {
            self.b.num_samples = 1;
        }
    }

    /// The add sample process (§7.10.4.2).
    fn add_sample(&mut self, delta_row: isize, delta_col: isize) {
        if self.b.num_samples_scanned >= LEAST_SQUARES_SAMPLES_MAX {
            return;
        }
        let (r, c) = (
            self.b.row as isize + delta_row,
            self.b.col as isize + delta_col,
        );
        if !self.is_inside(r, c) {
            return;
        }
        let refs = self.f.ref_frames[self.f.mi(r as usize, c as usize)];
        if refs[0] == REF_UNWRITTEN || refs[0] != self.b.ref_frame[0] || refs[1] != NONE {
            return;
        }
        let size = usize::from(self.f.mi_sizes[self.f.mi(r as usize, c as usize)]);
        let cand_w4 = usize::from(NUM_4X4_BLOCKS_WIDE[size]);
        let cand_h4 = usize::from(NUM_4X4_BLOCKS_HIGH[size]);
        let cand_row = r as usize & !(cand_h4 - 1);
        let cand_col = c as usize & !(cand_w4 - 1);
        let mid_y = (cand_row * 4 + cand_h4 * 2) as i32 - 1;
        let mid_x = (cand_col * 4 + cand_w4 * 2) as i32 - 1;
        let threshold = ((4 * self.b.bw4.max(self.b.bh4)) as i32).clamp(16, 112);
        let cand_mv = self.f.mvs[self.f.mi(cand_row, cand_col)][0];
        let mv = self.b.mv[0];
        let valid = (cand_mv[0] - mv[0]).abs() + (cand_mv[1] - mv[1]).abs() <= threshold;
        self.b.num_samples_scanned += 1;
        if !valid && self.b.num_samples_scanned > 1 {
            return;
        }
        self.b.cand_list[self.b.num_samples] = [
            mid_y * 8,
            mid_x * 8,
            mid_y * 8 + cand_mv[0],
            mid_x * 8 + cand_mv[1],
        ];
        if valid {
            self.b.num_samples += 1;
        }
    }
}
