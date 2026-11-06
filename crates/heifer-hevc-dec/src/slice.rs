//! Slice segment header (H.265 §7.3.6.1).
//!
//! A picture is split into one or more slices; each starts with this header, followed by the
//! CABAC-coded slice data. HEIF still images only contain intra (I) slices, so P and B slices
//! are rejected.

use crate::Error;
use crate::bitreader::BitReader;
use crate::nal::{NalHeader, NalUnitType};
use crate::params::{ParameterSets, ShortTermRps, parse_st_ref_pic_set};

/// `slice_type` (Table 7-7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceType {
    /// Bi-predictive (0).
    B,
    /// Predictive (1).
    P,
    /// Intra (2).
    I,
}

/// A parsed slice segment header.
///
/// For dependent slice segments, the fields after `slice_segment_address` are copied from the
/// preceding independent slice segment, as required by the spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceHeader {
    /// `first_slice_segment_in_pic_flag`.
    pub first_slice_segment_in_pic_flag: bool,
    /// `no_output_of_prior_pics_flag` (IRAP pictures only).
    pub no_output_of_prior_pics_flag: bool,
    /// `slice_pic_parameter_set_id`.
    pub slice_pic_parameter_set_id: u8,
    /// `dependent_slice_segment_flag`.
    pub dependent_slice_segment_flag: bool,
    /// `slice_segment_address`: address of the first CTB, in raster scan order.
    pub slice_segment_address: u32,
    /// `slice_type`.
    pub slice_type: SliceType,
    /// `pic_output_flag` (true when absent).
    pub pic_output_flag: bool,
    /// `colour_plane_id` (only with separate colour planes).
    pub colour_plane_id: u8,
    /// `slice_pic_order_cnt_lsb` (0 for IDR pictures).
    pub slice_pic_order_cnt_lsb: u32,
    /// Short-term reference picture set of this slice.
    pub st_rps: ShortTermRps,
    /// `short_term_ref_pic_set_sps_flag`: whether `st_rps` comes from the SPS (rather than
    /// being coded in this header).
    pub st_rps_from_sps: bool,
    /// `slice_temporal_mvp_enabled_flag`.
    pub slice_temporal_mvp_enabled_flag: bool,
    /// `num_ref_idx_l0_active_minus1`.
    pub num_ref_idx_l0_active_minus1: u8,
    /// `num_ref_idx_l1_active_minus1` (B slices; 0 for P and I slices).
    pub num_ref_idx_l1_active_minus1: u8,
    /// `cabac_init_flag`.
    pub cabac_init_flag: bool,
    /// `collocated_from_l0_flag` (inferred as true for P slices).
    pub collocated_from_l0_flag: bool,
    /// `collocated_ref_idx`.
    pub collocated_ref_idx: u8,
    /// `five_minus_max_num_merge_cand`.
    pub five_minus_max_num_merge_cand: u8,
    /// `list_entry_l0` when `ref_pic_list_modification_flag_l0` is set.
    pub list_entry_l0: Option<Vec<u8>>,
    /// `slice_sao_luma_flag`.
    pub slice_sao_luma_flag: bool,
    /// `slice_sao_chroma_flag`.
    pub slice_sao_chroma_flag: bool,
    /// `slice_qp_delta`.
    pub slice_qp_delta: i8,
    /// `slice_cb_qp_offset`.
    pub slice_cb_qp_offset: i8,
    /// `slice_cr_qp_offset`.
    pub slice_cr_qp_offset: i8,
    /// `cu_chroma_qp_offset_enabled_flag`.
    pub cu_chroma_qp_offset_enabled_flag: bool,
    /// `slice_deblocking_filter_disabled_flag` (inherited from the PPS when not overridden).
    pub slice_deblocking_filter_disabled_flag: bool,
    /// `slice_beta_offset_div2` (inherited from the PPS when not overridden).
    pub slice_beta_offset_div2: i8,
    /// `slice_tc_offset_div2` (inherited from the PPS when not overridden).
    pub slice_tc_offset_div2: i8,
    /// `slice_loop_filter_across_slices_enabled_flag` (inherited from the PPS when absent).
    pub slice_loop_filter_across_slices_enabled_flag: bool,
    /// `entry_point_offset_minus1[i] + 1`: sizes in bytes of each substream (tile or CTB row),
    /// counted in the NAL unit *including* emulation prevention bytes (§7.4.7.1).
    pub entry_point_offsets: Vec<u32>,
    /// Size of the header in bytes, i.e. offset of the slice data in the RBSP.
    pub header_size: usize,
}

impl SliceHeader {
    /// Parses the slice segment header at the start of a slice NAL unit's RBSP.
    ///
    /// `previous` is the last independent slice segment header of the same picture, needed for
    /// dependent slice segments.
    pub fn parse(
        rbsp: &[u8],
        nal: &NalHeader,
        sets: &ParameterSets,
        previous: Option<&SliceHeader>,
    ) -> Result<Self, Error> {
        let nal_type = match nal.unit_type {
            NalUnitType::IrapSlice(t) | NalUnitType::OtherSlice(t) => t,
            _ => return Err(Error::Invalid("not a slice NAL unit")),
        };
        let mut r = BitReader::new(rbsp);
        let first_slice_segment_in_pic_flag = r.flag()?;
        let no_output_of_prior_pics_flag =
            matches!(nal.unit_type, NalUnitType::IrapSlice(_)) && r.flag()?;
        let pps_id = r.ue_max(63, "slice_pic_parameter_set_id")?;
        let (pps, sps) = sets.get(pps_id as usize)?;

        let mut dependent_slice_segment_flag = false;
        let mut slice_segment_address = 0;
        if !first_slice_segment_in_pic_flag {
            if pps.dependent_slice_segments_enabled_flag {
                dependent_slice_segment_flag = r.flag()?;
            }
            let (w, h) = sps.pic_size_in_ctbs();
            let pic_size_in_ctbs = w * h;
            let bits = u32::BITS - (pic_size_in_ctbs - 1).leading_zeros(); // Ceil(Log2(PicSizeInCtbsY))
            slice_segment_address = r.bits(bits)?;
            if slice_segment_address >= pic_size_in_ctbs {
                return Err(Error::Invalid("slice_segment_address out of range"));
            }
        }

        let mut h = if dependent_slice_segment_flag {
            let prev = previous.ok_or(Error::Invalid(
                "dependent slice segment without a preceding slice",
            ))?;
            Self {
                first_slice_segment_in_pic_flag,
                dependent_slice_segment_flag,
                slice_segment_address,
                entry_point_offsets: Vec::new(),
                header_size: 0,
                ..prev.clone()
            }
        } else {
            r.skip(usize::from(pps.num_extra_slice_header_bits))?; // slice_reserved_flag
            let slice_type = match r.ue_max(2, "slice_type")? {
                0 => SliceType::B,
                1 => SliceType::P,
                _ => SliceType::I,
            };
            if slice_type == SliceType::B {
                return Err(Error::Unimplemented("B slices (bi-prediction)"));
            }
            let pic_output_flag = !pps.output_flag_present_flag || r.flag()?;
            let colour_plane_id = if sps.separate_colour_plane_flag {
                r.bits(2)? as u8
            } else {
                0
            };

            let mut slice_pic_order_cnt_lsb = 0;
            let mut st_rps = ShortTermRps::default();
            let mut st_rps_from_sps = false;
            let mut slice_temporal_mvp_enabled_flag = false;
            let is_idr = nal_type == 19 || nal_type == 20;
            if !is_idr {
                slice_pic_order_cnt_lsb = r.bits(u32::from(sps.log2_max_pic_order_cnt_lsb))?;
                let num_sets = sps.st_rps.len();
                st_rps_from_sps = r.flag()?;
                if st_rps_from_sps {
                    let idx = if num_sets > 1 {
                        let bits = usize::BITS - (num_sets - 1).leading_zeros();
                        r.bits(bits)? as usize
                    } else {
                        0
                    };
                    st_rps = sps
                        .st_rps
                        .get(idx)
                        .ok_or(Error::Invalid("short_term_ref_pic_set_idx out of range"))?
                        .clone();
                } else {
                    st_rps = parse_st_ref_pic_set(&mut r, num_sets, &sps.st_rps, true)?;
                }
                let mut num_long_term = 0;
                if sps.long_term_ref_pics_present_flag {
                    num_long_term = skip_long_term_refs(&mut r, sps)?;
                }
                if slice_type != SliceType::I && num_long_term > 0 {
                    return Err(Error::Unimplemented("long-term reference pictures"));
                }
                if sps.sps_temporal_mvp_enabled_flag {
                    slice_temporal_mvp_enabled_flag = r.flag()?;
                }
            }

            let (mut slice_sao_luma_flag, mut slice_sao_chroma_flag) = (false, false);
            if sps.sample_adaptive_offset_enabled_flag {
                slice_sao_luma_flag = r.flag()?;
                if sps.chroma_array_type() != 0 {
                    slice_sao_chroma_flag = r.flag()?;
                }
            }

            // Inter prediction fields of the header, coded before `slice_qp_delta` (§7.3.6.1).
            let mut num_ref_idx_l0_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
            let mut cabac_init_flag = false;
            let collocated_from_l0_flag = true;
            let mut collocated_ref_idx = 0;
            let mut five_minus_max_num_merge_cand = 0;
            let mut list_entry_l0 = None;
            if slice_type == SliceType::P {
                if r.flag()? {
                    // num_ref_idx_active_override_flag
                    num_ref_idx_l0_active_minus1 =
                        r.ue_max(14, "num_ref_idx_l0_active_minus1")? as u8;
                }
                let num_l0 = usize::from(num_ref_idx_l0_active_minus1) + 1;
                if pps.lists_modification_present_flag && st_rps.used_count() > 1 {
                    let bits = usize::BITS - (st_rps.used_count() - 1).leading_zeros();
                    if r.flag()? {
                        // ref_pic_list_modification_flag_l0
                        let mut entries = Vec::with_capacity(num_l0);
                        for _ in 0..num_l0 {
                            entries.push(r.bits(bits)? as u8);
                        }
                        list_entry_l0 = Some(entries);
                    }
                }
                if pps.cabac_init_present_flag {
                    cabac_init_flag = r.flag()?;
                }
                if slice_temporal_mvp_enabled_flag && num_ref_idx_l0_active_minus1 > 0 {
                    collocated_ref_idx = r.ue_max(14, "collocated_ref_idx")? as u8;
                    if usize::from(collocated_ref_idx) >= num_l0 {
                        return Err(Error::Invalid("collocated_ref_idx out of range"));
                    }
                }
                if pps.weighted_pred_flag {
                    return Err(Error::Unimplemented("weighted prediction for P slices"));
                }
                five_minus_max_num_merge_cand = r.ue_max(4, "five_minus_max_num_merge_cand")? as u8;
            }

            let qp_bd_offset = 6 * (i32::from(sps.bit_depth_luma) - 8);
            let init_qp = 26 + i32::from(pps.init_qp_minus26);
            let slice_qp_delta =
                r.se_range(-qp_bd_offset - init_qp, 51 - init_qp, "slice_qp_delta")? as i8;

            let (mut slice_cb_qp_offset, mut slice_cr_qp_offset) = (0, 0);
            if pps.pps_slice_chroma_qp_offsets_present_flag {
                slice_cb_qp_offset = r.se_range(-12, 12, "slice_cb_qp_offset")? as i8;
                slice_cr_qp_offset = r.se_range(-12, 12, "slice_cr_qp_offset")? as i8;
                if !(-12..=12).contains(&(pps.pps_cb_qp_offset + slice_cb_qp_offset))
                    || !(-12..=12).contains(&(pps.pps_cr_qp_offset + slice_cr_qp_offset))
                {
                    return Err(Error::Invalid("chroma QP offset out of range"));
                }
            }
            let cu_chroma_qp_offset_enabled_flag =
                pps.range_extension.diff_cu_chroma_qp_offset_depth.is_some() && r.flag()?;

            let deblocking_filter_override_flag =
                pps.deblocking_filter_override_enabled_flag && r.flag()?;
            let mut slice_deblocking_filter_disabled_flag = pps.pps_deblocking_filter_disabled_flag;
            let mut slice_beta_offset_div2 = pps.pps_beta_offset_div2;
            let mut slice_tc_offset_div2 = pps.pps_tc_offset_div2;
            if deblocking_filter_override_flag {
                slice_deblocking_filter_disabled_flag = r.flag()?;
                if !slice_deblocking_filter_disabled_flag {
                    slice_beta_offset_div2 = r.se_range(-6, 6, "slice_beta_offset_div2")? as i8;
                    slice_tc_offset_div2 = r.se_range(-6, 6, "slice_tc_offset_div2")? as i8;
                }
            }
            let mut slice_loop_filter_across_slices_enabled_flag =
                pps.pps_loop_filter_across_slices_enabled_flag;
            if pps.pps_loop_filter_across_slices_enabled_flag
                && (slice_sao_luma_flag
                    || slice_sao_chroma_flag
                    || !slice_deblocking_filter_disabled_flag)
            {
                slice_loop_filter_across_slices_enabled_flag = r.flag()?;
            }

            Self {
                first_slice_segment_in_pic_flag,
                no_output_of_prior_pics_flag,
                slice_pic_parameter_set_id: pps_id as u8,
                dependent_slice_segment_flag,
                slice_segment_address,
                slice_type,
                pic_output_flag,
                colour_plane_id,
                slice_pic_order_cnt_lsb,
                st_rps,
                st_rps_from_sps,
                slice_temporal_mvp_enabled_flag,
                num_ref_idx_l0_active_minus1,
                num_ref_idx_l1_active_minus1: 0,
                cabac_init_flag,
                collocated_from_l0_flag,
                collocated_ref_idx,
                five_minus_max_num_merge_cand,
                list_entry_l0,
                slice_sao_luma_flag,
                slice_sao_chroma_flag,
                slice_qp_delta,
                slice_cb_qp_offset,
                slice_cr_qp_offset,
                cu_chroma_qp_offset_enabled_flag,
                slice_deblocking_filter_disabled_flag,
                slice_beta_offset_div2,
                slice_tc_offset_div2,
                slice_loop_filter_across_slices_enabled_flag,
                entry_point_offsets: Vec::new(),
                header_size: 0,
            }
        };

        if pps.tiles.is_some() || pps.entropy_coding_sync_enabled_flag {
            let (w, hc) = sps.pic_size_in_ctbs();
            let max = match (&pps.tiles, pps.entropy_coding_sync_enabled_flag) {
                (None, true) => hc - 1,
                (Some(t), false) => t.num_columns * t.num_rows - 1,
                (Some(t), true) => t.num_columns * hc - 1,
                (None, false) => unreachable!(),
            };
            let n = r.ue_max(max.min(w * hc), "num_entry_point_offsets")?;
            if n > 0 {
                let offset_len = r.ue_max(31, "offset_len_minus1")? + 1;
                h.entry_point_offsets = (0..n)
                    .map(|_| r.bits(offset_len).map(|v| v.saturating_add(1)))
                    .collect::<Result<_, _>>()?;
            }
        }

        if pps.slice_segment_header_extension_present_flag {
            let len = r.ue_max(256, "slice_segment_header_extension_length")?;
            r.skip(len as usize * 8)?;
        }

        // byte_alignment(): a one bit, then zero bits up to the byte boundary.
        r.rbsp_trailing_bits()
            .map_err(|_| Error::Invalid("bad slice header byte alignment"))?;
        h.header_size = r.position() / 8;
        Ok(h)
    }

    /// `SliceQpY = 26 + init_qp_minus26 + slice_qp_delta`.
    pub fn slice_qp_y(&self, sets: &ParameterSets) -> Result<i32, Error> {
        let (pps, _) = sets.get(usize::from(self.slice_pic_parameter_set_id))?;
        Ok(26 + i32::from(pps.init_qp_minus26) + i32::from(self.slice_qp_delta))
    }
}

/// Skips the long-term reference picture part of a slice header and returns how many pictures
/// it adds to the current reference picture set.
fn skip_long_term_refs(r: &mut BitReader<'_>, sps: &crate::params::Sps) -> Result<u32, Error> {
    let num_lt_sps = u32::from(sps.num_long_term_ref_pics_sps);
    let num_long_term_sps = if num_lt_sps > 0 {
        r.ue_max(num_lt_sps, "num_long_term_sps")?
    } else {
        0
    };
    let num_long_term_pics = r.ue_max(32, "num_long_term_pics")?;
    for i in 0..num_long_term_sps + num_long_term_pics {
        if i < num_long_term_sps {
            if num_lt_sps > 1 {
                r.skip((u32::BITS - (num_lt_sps - 1).leading_zeros()) as usize)?; // lt_idx_sps
            }
        } else {
            r.skip(usize::from(sps.log2_max_pic_order_cnt_lsb) + 1)?; // poc_lsb_lt, used_by_curr_pic_lt_flag
        }
        if r.flag()? {
            r.ue()?; // delta_poc_msb_cycle_lt
        }
    }
    Ok(num_long_term_sps + num_long_term_pics)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nal::NalUnit;
    use crate::params::tests::small_sps;
    use crate::testutil::BitWriter;

    /// SPS from the params tests (2×2 CTBs of 32, one short-term RPS, SAO on) and a PPS with
    /// dependent slices, slice-level chroma QP offsets and deblocking override.
    fn sets() -> ParameterSets {
        let mut w = BitWriter::default();
        w.ue(0)
            .ue(0)
            .bit(true)
            .bit(false)
            .bits(0, 3)
            .bit(true)
            .bit(true);
        w.ue(0).ue(0).se(0).bit(false).bit(false).bit(false);
        w.se(0).se(0).bit(true).bit(false).bit(false).bit(false);
        w.bit(false).bit(false); // no tiles, no WPP
        w.bit(true).bit(true).bit(true).bit(false).se(1).se(-1);
        w.bit(false).bit(false).ue(0).bit(false).bit(false);
        let pps = w.finish();

        let mut sets = ParameterSets::default();
        for (t, rbsp) in [(33u8, small_sps()), (34, pps)] {
            let mut nal = vec![t << 1, 1];
            nal.extend_from_slice(&rbsp);
            sets.add(&NalUnit::parse(&nal).unwrap()).unwrap();
        }
        sets
    }

    fn cra() -> NalHeader {
        NalHeader::parse(&[21 << 1, 1]).unwrap()
    }

    fn cra_slice() -> Vec<u8> {
        let mut w = BitWriter::default();
        w.bit(true).bit(false).ue(0).ue(2); // first, no_output, pps 0, I slice
        w.bits(5, 8); // slice_pic_order_cnt_lsb
        // st_ref_pic_set coded in the slice, predicted from SPS set 0 (which has 1 picture).
        w.bit(false).bit(true).ue(0).bit(false).ue(0);
        w.bit(true).bit(false).bit(false);
        w.bit(true).bit(false); // SAO luma on, chroma off
        w.se(-3).se(2).se(-1); // slice_qp_delta, cb, cr offsets
        w.bit(true).bit(false).se(3).se(-2); // deblocking override
        w.bit(false); // slice_loop_filter_across_slices_enabled_flag
        w.finish()
    }

    #[test]
    fn independent_cra_slice() {
        let sets = sets();
        let h = SliceHeader::parse(&cra_slice(), &cra(), &sets, None).unwrap();
        assert_eq!(h.slice_type, SliceType::I);
        assert_eq!(h.slice_pic_order_cnt_lsb, 5);
        assert!(h.slice_sao_luma_flag && !h.slice_sao_chroma_flag);
        assert_eq!(h.slice_qp_delta, -3);
        assert_eq!(h.slice_qp_y(&sets), Ok(23));
        assert_eq!((h.slice_cb_qp_offset, h.slice_cr_qp_offset), (2, -1));
        assert_eq!((h.slice_beta_offset_div2, h.slice_tc_offset_div2), (3, -2));
        assert!(!h.slice_loop_filter_across_slices_enabled_flag);
        assert_eq!(h.header_size, cra_slice().len());
    }

    #[test]
    fn dependent_slice_copies_previous_header() {
        let sets = sets();
        let first = SliceHeader::parse(&cra_slice(), &cra(), &sets, None).unwrap();
        let mut w = BitWriter::default();
        w.bit(false).bit(false).ue(0).bit(true).bits(3, 2); // not first, dependent, address 3
        let h = SliceHeader::parse(&w.finish(), &cra(), &sets, Some(&first)).unwrap();
        assert!(h.dependent_slice_segment_flag);
        assert_eq!(h.slice_segment_address, 3);
        assert_eq!(h.slice_qp_delta, -3);
        assert_eq!(h.slice_beta_offset_div2, 3);
        assert!(SliceHeader::parse(&w.finish(), &cra(), &sets, None).is_err());
    }

    #[test]
    fn rejects_bad_slice_addresses() {
        let sets = sets();
        let mut w = BitWriter::default();
        w.bit(false).bit(false).ue(0).bit(false).bits(3, 2).ue(2); // address 3 is valid...
        assert!(SliceHeader::parse(&w.finish(), &cra(), &sets, None).is_err()); // ...but header truncated
    }

    #[test]
    fn parses_p_slice_header() {
        let sets = sets();
        let pps = sets.pps[0].as_ref().unwrap();
        assert!(pps.cabac_init_present_flag && !pps.lists_modification_present_flag);
        let mut w = BitWriter::default();
        w.bit(true).bit(false).ue(0).ue(1); // first, no_output, pps 0, P slice
        w.bits(7, 8); // slice_pic_order_cnt_lsb
        // RPS coded in the slice, predicted from the SPS set: one negative picture at -2.
        w.bit(false).bit(true).ue(0).bit(true).ue(0);
        w.bit(true).bit(false).bit(false);
        w.bit(true).bit(false); // SAO luma on, chroma off
        w.bit(true).ue(1); // num_ref_idx_active_override_flag, l0 active minus1
        w.bit(true); // cabac_init_flag
        w.ue(2); // five_minus_max_num_merge_cand
        w.se(0).se(0).se(0); // slice_qp_delta, cb, cr offsets
        w.bit(false); // deblocking override
        w.bit(false); // slice_loop_filter_across_slices_enabled_flag
        let h = SliceHeader::parse(&w.finish(), &cra(), &sets, None).unwrap();
        assert_eq!(h.slice_type, SliceType::P);
        assert_eq!(h.slice_pic_order_cnt_lsb, 7);
        assert_eq!(h.st_rps.negative.len(), 1);
        assert_eq!(h.st_rps.negative[0].delta_poc, -2);
        assert!(h.st_rps.negative[0].used_by_curr_pic);
        assert_eq!(h.st_rps.used_count(), 1);
        assert!(!h.st_rps_from_sps);
        assert_eq!(h.num_ref_idx_l0_active_minus1, 1);
        assert!(h.cabac_init_flag);
        assert_eq!(h.five_minus_max_num_merge_cand, 2);
        assert!(!h.slice_temporal_mvp_enabled_flag);
        assert!(h.list_entry_l0.is_none());
    }
}
