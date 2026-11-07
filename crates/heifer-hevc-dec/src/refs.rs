//! Reference picture management: picture order count derivation (§8.3.1) and the
//! construction of `RefPicList0` and `RefPicList1` (§8.3.4) from the short-term pictures of
//! the RPS and the long-term pictures of the slice.

use crate::Error;
use crate::params::ShortTermRps;

/// A picture kept for reference, identified by its slot in the decoder's DPB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefPic {
    /// Index of the picture in the decoder's decoded picture buffer.
    pub slot: usize,
    /// `PicOrderCntVal` of the picture.
    pub poc: i32,
    /// Whether the picture is marked as used for long-term reference.
    pub long_term: bool,
}

/// Derives the picture order count of the pictures, in decoding order (§8.3.1).
///
/// Only the previous picture with `TemporalId` 0 is kept, as the spec requires, so pictures of
/// higher temporal sub-layers do not disturb the sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PocState {
    prev_lsb: u32,
    prev_msb: i32,
    /// Whether no temporal sub-layer 0 picture has been seen yet.
    prev_none: bool,
}

impl Default for PocState {
    fn default() -> Self {
        Self {
            prev_lsb: 0,
            prev_msb: 0,
            prev_none: true,
        }
    }
}

impl PocState {
    /// Whether a picture can be `prevTid0Pic` (§8.3.1): `TemporalId` 0, and neither a RADL
    /// or RASL picture nor a sub-layer non-reference picture (even NAL unit types below 16).
    pub fn is_prev_tid0_candidate(nal_unit_type: u8, temporal_id: u8) -> bool {
        temporal_id == 0
            && !(6..=9).contains(&nal_unit_type)
            && !(nal_unit_type < 16 && nal_unit_type % 2 == 0)
    }

    /// Derives `PicOrderCntVal` of a picture and updates the state.
    ///
    /// `is_irap` resets the state. Only pictures that may serve as `prevTid0Pic` (see
    /// [`is_prev_tid0_candidate`](Self::is_prev_tid0_candidate)) update it; the others are
    /// derived from the last such picture.
    pub fn next(
        &mut self,
        slice_pic_order_cnt_lsb: u32,
        log2_max_poc_lsb: u8,
        is_irap: bool,
        prev_tid0_candidate: bool,
    ) -> i32 {
        let max_lsb = 1i32 << log2_max_poc_lsb;
        let lsb = slice_pic_order_cnt_lsb as i32;
        let (prev_lsb, prev_msb) = if is_irap || self.prev_none {
            (0, 0)
        } else {
            (self.prev_lsb as i32, self.prev_msb)
        };
        let msb = if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
            prev_msb + max_lsb
        } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
            prev_msb - max_lsb
        } else {
            prev_msb
        };
        if prev_tid0_candidate {
            self.prev_lsb = slice_pic_order_cnt_lsb;
            self.prev_msb = msb;
            self.prev_none = false;
        }
        msb + lsb
    }
}

/// Builds `RefPicList0` of a P or B slice (§8.3.4) from the slice's RPS and the pictures
/// present in the DPB.
///
/// `dpb` must contain every picture the RPS refers to, otherwise the bitstream is invalid.
/// `num_ref_idx_active` is `num_ref_idx_l0_active_minus1 + 1`; when it exceeds the number of
/// pictures in the RPS, entries are repeated cyclically.
pub fn build_ref_pic_list0(
    curr_poc: i32,
    rps: &ShortTermRps,
    dpb: &[RefPic],
    num_ref_idx_active: usize,
    list_entry_l0: Option<&[u8]>,
) -> Result<Vec<RefPic>, Error> {
    build_ref_pic_list(
        0,
        curr_poc,
        rps,
        dpb,
        &[],
        num_ref_idx_active,
        list_entry_l0,
    )
}

/// Builds `RefPicList1` of a B slice (§8.3.4): like [`build_ref_pic_list0`], but the
/// pictures after the current one (positive delta POC) come first.
pub fn build_ref_pic_list1(
    curr_poc: i32,
    rps: &ShortTermRps,
    dpb: &[RefPic],
    num_ref_idx_active: usize,
    list_entry_l1: Option<&[u8]>,
) -> Result<Vec<RefPic>, Error> {
    build_ref_pic_list(
        1,
        curr_poc,
        rps,
        dpb,
        &[],
        num_ref_idx_active,
        list_entry_l1,
    )
}

/// Builds `RefPicList0` (`list` 0) or `RefPicList1` (`list` 1): the short-term pictures of
/// the RPS found among the short-term pictures of `dpb`, then `lt_curr`, the long-term
/// pictures used by the current one (`RefPicSetLtCurr`).
pub fn build_ref_pic_list(
    list: usize,
    curr_poc: i32,
    rps: &ShortTermRps,
    dpb: &[RefPic],
    lt_curr: &[RefPic],
    num_ref_idx_active: usize,
    list_entry: Option<&[u8]>,
) -> Result<Vec<RefPic>, Error> {
    let after_first = list == 1;
    // StCurrBefore (negative delta POC) and StCurrAfter (positive delta POC), each nearest
    // first; RefPicList0 starts with StCurrBefore, RefPicList1 with StCurrAfter.
    let (first, second) = if after_first {
        (&rps.positive, &rps.negative)
    } else {
        (&rps.negative, &rps.positive)
    };
    let mut temp = Vec::with_capacity(8);
    for pic in first.iter().chain(second).filter(|p| p.used_by_curr_pic) {
        let poc = curr_poc + pic.delta_poc;
        let entry = dpb
            .iter()
            .find(|r| r.poc == poc && !r.long_term)
            .copied()
            .ok_or(Error::Invalid(
                "reference picture of the RPS is not in the decoded picture buffer",
            ))?;
        temp.push(entry);
    }
    temp.extend_from_slice(lt_curr);
    if temp.is_empty() {
        return Err(Error::Invalid(
            "inter slice without a used reference picture",
        ));
    }

    let mut list = Vec::with_capacity(num_ref_idx_active);
    for i in 0..num_ref_idx_active {
        let index = match list_entry {
            Some(entries) => {
                let entry = usize::from(
                    *entries
                        .get(i)
                        .ok_or(Error::Invalid("list_entry out of range"))?,
                );
                if entry >= temp.len() {
                    return Err(Error::Invalid("list_entry out of range"));
                }
                entry
            }
            None => i % temp.len(),
        };
        list.push(temp[index]);
    }
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::ShortTermRefPic;

    fn rps(negative: &[(i32, bool)], positive: &[(i32, bool)]) -> ShortTermRps {
        ShortTermRps {
            negative: negative
                .iter()
                .map(|&(delta_poc, used_by_curr_pic)| ShortTermRefPic {
                    delta_poc,
                    used_by_curr_pic,
                })
                .collect(),
            positive: positive
                .iter()
                .map(|&(delta_poc, used_by_curr_pic)| ShortTermRefPic {
                    delta_poc,
                    used_by_curr_pic,
                })
                .collect(),
        }
    }

    fn dpb(pocs: &[i32]) -> Vec<RefPic> {
        pocs.iter()
            .enumerate()
            .map(|(slot, &poc)| RefPic {
                slot,
                poc,
                long_term: false,
            })
            .collect()
    }

    #[test]
    fn poc_wraps_after_half_the_lsb_range() {
        let mut state = PocState::default();
        assert_eq!(state.next(0, 4, true, true), 0);
        assert_eq!(state.next(1, 4, false, true), 1);
        assert_eq!(state.next(2, 4, false, true), 2);
        // A small step back does not cross half the range.
        assert_eq!(state.next(1, 4, false, true), 1);
        assert_eq!(state.next(9, 4, false, true), 9);
        // 9 - 1 >= 8: the LSB wrapped, so the MSB increases by the range.
        assert_eq!(state.next(1, 4, false, true), 17);
    }

    #[test]
    fn poc_ignores_higher_temporal_layers() {
        let mut state = PocState::default();
        assert_eq!(state.next(0, 8, true, true), 0);
        assert_eq!(state.next(8, 8, false, true), 8);
        assert_eq!(state.next(4, 8, false, false), 4);
        // The state still refers to POC 8, so 16 continues from it.
        assert_eq!(state.next(16, 8, false, true), 16);
    }

    #[test]
    fn list_is_negative_then_positive_and_repeats() {
        let rps = rps(&[(-1, true), (-2, true)], &[]);
        let list = build_ref_pic_list0(5, &rps, &dpb(&[3, 4]), 3, None).unwrap();
        assert_eq!(
            list.iter().map(|r| r.poc).collect::<Vec<_>>(),
            [4, 3, 4],
            "nearest first, then cycled"
        );
    }

    #[test]
    fn list_skips_unused_and_uses_positive_refs() {
        let rps = rps(&[(-1, false), (-2, true)], &[(1, true)]);
        let list = build_ref_pic_list0(5, &rps, &dpb(&[3, 6]), 4, None).unwrap();
        assert_eq!(list.iter().map(|r| r.poc).collect::<Vec<_>>(), [3, 6, 3, 6]);
    }

    #[test]
    fn list_applies_modification() {
        let rps = rps(&[(-1, true), (-2, true)], &[]);
        let list = build_ref_pic_list0(5, &rps, &dpb(&[3, 4]), 2, Some(&[1, 0])).unwrap();
        assert_eq!(list.iter().map(|r| r.poc).collect::<Vec<_>>(), [3, 4]);
    }

    #[test]
    fn non_reference_pictures_are_not_prev_tid0() {
        // TRAIL_R, CRA, IDR: yes; TRAIL_N, RADL_N, RADL_R, RASL_R, or TemporalId 1: no.
        for t in [1, 21, 19] {
            assert!(PocState::is_prev_tid0_candidate(t, 0), "{t}");
        }
        for t in [0, 6, 7, 9] {
            assert!(!PocState::is_prev_tid0_candidate(t, 0), "{t}");
        }
        assert!(!PocState::is_prev_tid0_candidate(1, 1));
        // POC_A: IDR 0, leading RADL_N -7, TRAIL_R 8, TRAIL_N 16, then 1 derived from POC 8
        // (the TRAIL_N picture is not prevTid0Pic).
        let mut state = PocState::default();
        let pics = [(0, 19), (9, 7), (8, 1), (0, 0), (1, 0)];
        let pocs: Vec<i32> = pics
            .iter()
            .map(|&(lsb, t)| state.next(lsb, 4, t == 19, PocState::is_prev_tid0_candidate(t, 0)))
            .collect();
        assert_eq!(pocs, [0, -7, 8, 16, 1]);
    }

    #[test]
    fn list1_starts_with_the_following_pictures() {
        let rps = rps(&[(-1, true), (-3, true)], &[(2, true), (4, false)]);
        let dpb = dpb(&[7, 5, 10, 12]);
        let l1 = build_ref_pic_list1(8, &rps, &dpb, 4, None).unwrap();
        let pocs: Vec<i32> = l1.iter().map(|r| r.poc).collect();
        assert_eq!(pocs, [10, 7, 5, 10]);
        let l0 = build_ref_pic_list0(8, &rps, &dpb, 3, None).unwrap();
        let pocs: Vec<i32> = l0.iter().map(|r| r.poc).collect();
        assert_eq!(pocs, [7, 5, 10]);
        let l1 = build_ref_pic_list1(8, &rps, &dpb, 2, Some(&[2, 0])).unwrap();
        let pocs: Vec<i32> = l1.iter().map(|r| r.poc).collect();
        assert_eq!(pocs, [5, 10]);
    }

    #[test]
    fn long_term_pictures_follow_the_short_term_ones() {
        let rps = rps(&[(-1, true)], &[(2, true)]);
        let mut pics = dpb(&[7, 10, 3]);
        pics[2].long_term = true;
        let lt = [pics[2]];
        let pocs = |l: Vec<RefPic>| l.iter().map(|r| r.poc).collect::<Vec<_>>();
        let l0 = build_ref_pic_list(0, 8, &rps, &pics, &lt, 4, None).unwrap();
        assert_eq!(pocs(l0), [7, 10, 3, 7]);
        let l1 = build_ref_pic_list(1, 8, &rps, &pics, &lt, 3, None).unwrap();
        assert_eq!(pocs(l1), [10, 7, 3]);
        let l1 = build_ref_pic_list(1, 8, &rps, &pics, &lt, 1, Some(&[2])).unwrap();
        assert!(l1[0].long_term);
        // A short-term entry of the RPS never matches a long-term picture.
        let st_only = rps_neg_only();
        assert!(build_ref_pic_list(0, 8, &st_only, &pics, &[], 1, None).is_err());
    }

    fn rps_neg_only() -> ShortTermRps {
        rps(&[(-5, true)], &[])
    }

    #[test]
    fn missing_reference_is_rejected() {
        let rps = rps(&[(-3, true)], &[]);
        assert!(build_ref_pic_list0(5, &rps, &dpb(&[4]), 1, None).is_err());
    }
}
