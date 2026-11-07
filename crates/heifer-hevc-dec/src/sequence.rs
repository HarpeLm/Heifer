//! Decoding of a sequence of pictures (image sequences, bursts, video): picture order count,
//! reference picture sets and the decoded picture buffer (§8.3), then each picture's slices
//! with inter prediction from the pictures kept for reference.
//!
//! [`SequenceDecoder::decode`] returns pictures in decoding order, and
//! [`SequenceDecoder::decode_in_display_order`] in display order. P and B slices are
//! supported, with short- and long-term reference pictures.

use crate::Error;
use crate::decoder::{DEFAULT_MAX_PIXELS, DecodeOptions};
use crate::inter::{MotionField, SliceRefs};
use crate::nal::{NalUnit, NalUnitType, split_annex_b};
use crate::params::ParameterSets;
use crate::recon::{Frame, Reconstructor};
use crate::refs::{PocState, RefPic, build_ref_pic_list};
use crate::scan::tiles_fit;
use crate::sei::parse_picture_hash;
use crate::slice::{LongTermRef, SliceHeader, SliceType};
use crate::syntax::{Picture, decode_slice_segment};

/// A decoded picture kept for reference.
#[derive(Debug, Clone)]
struct DecodedPicture {
    poc: i32,
    /// The filtered picture at its coded size (before cropping).
    frame: Frame,
    /// Its motion, for temporal motion vector prediction.
    motion: MotionField,
    /// Marked as used for long-term reference.
    long_term: bool,
}

/// Decodes the pictures of an HEVC sequence one at a time, in decoding order.
#[derive(Debug, Default)]
pub struct SequenceDecoder {
    options: DecodeOptions,
    sets: ParameterSets,
    poc: PocState,
    dpb: Vec<DecodedPicture>,
    /// A coded video sequence has started (an IRAP picture was decoded).
    started: bool,
    /// The last IRAP picture started a new sequence, so its RASL pictures cannot be decoded.
    skip_rasl: bool,
    /// Decoded pictures waiting to be output in display order: POC, `PicLatencyCount` and
    /// cropped picture.
    pending: Vec<(i32, u32, Frame)>,
    /// Facts about the last decoded picture, for output in display order.
    last: LastPicture,
}

/// The last decoded picture: its POC, whether it started a new coded video sequence, whether
/// it is output (`pic_output_flag`), and `sps_max_num_reorder_pics`.
#[derive(Debug, Default, Clone, Copy)]
struct LastPicture {
    poc: i32,
    new_sequence: bool,
    /// `NoOutputOfPriorPicsFlag`: the waiting pictures are dropped, not output.
    drop_prior: bool,
    output: bool,
    max_num_reorder: usize,
    max_dec_pic_buffering: usize,
    /// `SpsMaxLatencyPictures`, if limited.
    max_latency: Option<u32>,
}

impl SequenceDecoder {
    /// A decoder with no parameter sets and no reference pictures.
    pub fn new(options: DecodeOptions) -> Self {
        Self {
            options,
            ..Self::default()
        }
    }

    /// Adds the parameter sets (VPS, SPS, PPS) of an Annex B stream, such as the NAL units of
    /// an `hvcC` box. Other NAL units are ignored.
    pub fn add_parameter_sets(&mut self, stream: &[u8]) -> Result<(), Error> {
        for nal in split_annex_b(stream) {
            let nal = NalUnit::parse(nal)?;
            if !nal.header.unit_type.is_slice() {
                self.sets.add(&nal)?;
            }
        }
        Ok(())
    }

    /// Forgets the reference pictures: the next picture must be a random access point.
    pub fn reset(&mut self) {
        self.dpb.clear();
        self.poc = PocState::default();
        self.started = false;
    }

    /// Decodes the next picture. `stream` is an Annex B stream holding the NAL units of one
    /// picture (an access unit), parameter sets included if any; the picture is returned
    /// cropped to its conformance window.
    pub fn decode(&mut self, stream: &[u8]) -> Result<Frame, Error> {
        let nals: Vec<NalUnit<'_>> = split_annex_b(stream)
            .map(NalUnit::parse)
            .collect::<Result<_, _>>()?;
        for nal in nals.iter().take_while(|n| !n.header.unit_type.is_slice()) {
            self.sets.add(nal)?;
        }
        let first = nals
            .iter()
            .find(|n| n.header.unit_type.is_slice())
            .ok_or(Error::Invalid("no slice in access unit"))?;
        let nal_type = match first.header.unit_type {
            NalUnitType::IrapSlice(t) | NalUnitType::OtherSlice(t) => t,
            _ => unreachable!("a slice NAL unit"),
        };
        let irap = matches!(first.header.unit_type, NalUnitType::IrapSlice(_));
        let header = SliceHeader::parse(&first.rbsp, &first.header, &self.sets, None)?;
        // IDR and BLA pictures always start a sequence; a CRA picture does at the start. A
        // stream may also start at an intra picture that is not a random access point (image
        // sequences mark such pictures as sync samples): its POC then serves as the origin.
        let intra_start = !self.started && !irap && slices_are_intra(&nals, &self.sets, &header)?;
        let new_sequence = (irap && (nal_type != 21 || !self.started)) || intra_start;
        let output = header.pic_output_flag;
        if !self.started && !irap && !intra_start {
            return Err(Error::Invalid(
                "sequence does not start with a random access picture",
            ));
        }
        if irap || intra_start {
            self.skip_rasl = new_sequence;
            self.started = true;
        } else if (nal_type == 8 || nal_type == 9) && self.skip_rasl {
            return Err(Error::Unimplemented(
                "RASL picture after a random access point (its references are missing)",
            ));
        }

        let (pps, sps) = self
            .sets
            .get(usize::from(header.slice_pic_parameter_set_id))?;
        let max_pixels = if self.options.max_pixels == 0 {
            DEFAULT_MAX_PIXELS
        } else {
            self.options.max_pixels
        };
        if u64::from(sps.pic_width_in_luma_samples) * u64::from(sps.pic_height_in_luma_samples)
            > max_pixels
        {
            return Err(Error::Unimplemented(
                "picture larger than the configured size limit",
            ));
        }
        if let Some(tiles) = &pps.tiles {
            let (width, height) = sps.pic_size_in_ctbs();
            if !tiles_fit(tiles, width, height) {
                return Err(Error::Invalid(
                    "tile layout does not match the picture size",
                ));
            }
        }
        let poc = self.poc.next(
            header.slice_pic_order_cnt_lsb,
            sps.log2_max_pic_order_cnt_lsb,
            new_sequence,
            PocState::is_prev_tid0_candidate(nal_type, first.header.temporal_id),
        );

        // Reference picture set (§8.3.2): keep only the pictures it lists, and mark its
        // long-term pictures.
        if new_sequence {
            self.dpb.clear();
        } else {
            let mut long_term = Vec::new();
            for lt in &header.long_term_refs {
                match find_long_term(&self.dpb, lt, poc, &header, sps.log2_max_pic_order_cnt_lsb) {
                    Some(slot) => long_term.push(slot),
                    None if lt.used_by_curr_pic => {
                        return Err(Error::Invalid(
                            "long-term reference picture is not in the decoded picture buffer",
                        ));
                    }
                    None => {}
                }
            }
            let short_term: Vec<i32> = header
                .st_rps
                .negative
                .iter()
                .chain(&header.st_rps.positive)
                .map(|r| poc + r.delta_poc)
                .collect();
            let mut slot = 0;
            self.dpb.retain_mut(|p| {
                let lt = long_term.contains(&slot);
                slot += 1;
                p.long_term |= lt;
                lt || (!p.long_term && short_term.contains(&p.poc))
            });
        }
        if self.dpb.iter().any(|p| p.poc == poc) {
            return Err(Error::Invalid("two pictures with the same POC"));
        }

        let (frame, motion) = self.decode_slices(&nals, poc)?;
        // The decoded picture hash, sent in a suffix SEI after the slices (uncropped picture).
        if self.options.verify_hash && !self.options.skip_loop_filters {
            let components = if sps.chroma_format_idc == 0 { 1 } else { 3 };
            for nal in nals
                .iter()
                .skip_while(|n| !n.header.unit_type.is_slice())
                .filter(|n| n.header.unit_type == NalUnitType::SuffixSei)
            {
                if let Some(expected) = parse_picture_hash(&nal.rbsp, components)? {
                    if expected.compute_like(&frame) != expected {
                        return Err(Error::HashMismatch(expected.kind()));
                    }
                    break;
                }
            }
        }
        let cropped = if sps.conf_win_offsets == [0; 4] {
            frame.clone()
        } else {
            frame.cropped(sps)
        };
        self.last = LastPicture {
            poc,
            new_sequence,
            // For an IRAP picture that starts a sequence: always for a CRA picture, else as
            // signalled (§C.5.2.2).
            drop_prior: irap
                && new_sequence
                && (nal_type == 21 || header.no_output_of_prior_pics_flag),
            output,
            max_num_reorder: sps.max_num_reorder_pics as usize,
            max_dec_pic_buffering: sps.max_dec_pic_buffering as usize,
            max_latency: (sps.max_latency_increase_plus1 != 0)
                .then(|| sps.max_num_reorder_pics + sps.max_latency_increase_plus1 - 1),
        };
        self.dpb.push(DecodedPicture {
            poc,
            frame,
            motion,
            long_term: false,
        });
        // After an end of sequence NAL unit, the next picture starts a new sequence.
        if nals
            .iter()
            .any(|n| n.header.unit_type == NalUnitType::Other(36))
        {
            self.started = false;
        }
        Ok(cropped)
    }

    /// Like [`decode`](Self::decode), but returns pictures in display order, with the output
    /// process of §C.5.2 ("bumping"): a picture is output once too many pictures wait, a
    /// latency limit is reached, or the DPB is full. The result holds zero or more pictures;
    /// call [`flush`](Self::flush) after the last access unit. Pictures with
    /// `pic_output_flag` = 0 are not returned.
    pub fn decode_in_display_order(&mut self, stream: &[u8]) -> Result<Vec<Frame>, Error> {
        let frame = self.decode(stream)?;
        let last = self.last;
        let mut out = Vec::new();
        if last.drop_prior {
            // NoOutputOfPriorPicsFlag: the waiting pictures are dropped.
            self.pending.clear();
        } else if last.new_sequence {
            // A new coded video sequence: every waiting picture comes first.
            out = self.flush();
        } else {
            // Before the current picture is stored (§C.5.2.2). The DPB holds the reference
            // pictures (the current one included, last) and the pictures waiting for output.
            loop {
                let refs = &self.dpb[..self.dpb.len() - 1];
                let waiting_only = self
                    .pending
                    .iter()
                    .filter(|p| !refs.iter().any(|r| r.poc == p.0))
                    .count();
                let fullness = refs.len() + waiting_only;
                if self.pending.is_empty()
                    || !(self.pending.len() > last.max_num_reorder
                        || self.latency_exceeded(last.max_latency)
                        || fullness >= last.max_dec_pic_buffering)
                {
                    break;
                }
                out.push(self.pop_first());
            }
        }
        // The current picture (§C.5.2.3).
        if last.output {
            for p in &mut self.pending {
                p.1 += 1;
            }
            self.pending.push((last.poc, 0, frame));
        }
        while !self.pending.is_empty()
            && (self.pending.len() > last.max_num_reorder
                || self.latency_exceeded(last.max_latency))
        {
            out.push(self.pop_first());
        }
        Ok(out)
    }

    /// Whether a waiting picture has reached `SpsMaxLatencyPictures`.
    fn latency_exceeded(&self, max_latency: Option<u32>) -> bool {
        max_latency.is_some_and(|max| self.pending.iter().any(|p| p.1 >= max))
    }

    /// The pictures still waiting for output, in display order (see
    /// [`decode_in_display_order`](Self::decode_in_display_order)).
    pub fn flush(&mut self) -> Vec<Frame> {
        let mut out = Vec::with_capacity(self.pending.len());
        while !self.pending.is_empty() {
            out.push(self.pop_first());
        }
        out
    }

    /// Removes the waiting picture with the smallest POC.
    fn pop_first(&mut self) -> Frame {
        let i = (0..self.pending.len())
            .min_by_key(|&i| self.pending[i].0)
            .expect("a waiting picture");
        self.pending.remove(i).2
    }

    /// Decodes the slices of the current picture, whose POC is `poc`.
    fn decode_slices(&self, nals: &[NalUnit<'_>], poc: i32) -> Result<(Frame, MotionField), Error> {
        let dpb = &self.dpb;
        let pictures: Vec<RefPic> = dpb
            .iter()
            .enumerate()
            .map(|(slot, p)| RefPic {
                slot,
                poc: p.poc,
                long_term: p.long_term,
            })
            .collect();
        let mut state: Option<(Picture<'_>, Reconstructor<'_>)> = None;
        let mut previous: Option<SliceHeader> = None;
        for nal in nals.iter().filter(|n| n.header.unit_type.is_slice()) {
            let first_in_pic = nal.rbsp.first().is_some_and(|b| b & 0x80 != 0);
            if first_in_pic && state.is_some() {
                return Err(Error::Invalid("access unit holds more than one picture"));
            }
            let header = SliceHeader::parse(&nal.rbsp, &nal.header, &self.sets, previous.as_ref())?;
            let (pps, sps) = self
                .sets
                .get(usize::from(header.slice_pic_parameter_set_id))?;
            let (pic, recon) =
                state.get_or_insert_with(|| (Picture::new(sps, pps), Reconstructor::new(sps, pps)));
            if !std::ptr::eq(pic.sps, sps) || !std::ptr::eq(pic.pps, pps) {
                return Err(Error::Invalid(
                    "slices of one picture use different parameter sets",
                ));
            }
            recon.start_slice(&header);

            // Reference picture lists (§8.3.4) and the collocated picture (§8.5.3.2.8).
            let inter = header.slice_type != SliceType::I;
            let mut lists: [Vec<RefPic>; 2] = [Vec::new(), Vec::new()];
            if inter {
                let mut lt_curr = Vec::new();
                for lt in header.long_term_refs.iter().filter(|l| l.used_by_curr_pic) {
                    let slot =
                        find_long_term(dpb, lt, poc, &header, sps.log2_max_pic_order_cnt_lsb)
                            .ok_or(Error::Invalid("long-term reference picture is missing"))?;
                    lt_curr.push(pictures[slot]);
                }
                let num = [
                    usize::from(header.num_ref_idx_l0_active_minus1) + 1,
                    usize::from(header.num_ref_idx_l1_active_minus1) + 1,
                ];
                let entries = [
                    header.list_entry_l0.as_deref(),
                    header.list_entry_l1.as_deref(),
                ];
                let used = if header.slice_type == SliceType::B {
                    2
                } else {
                    1
                };
                for l in 0..used {
                    lists[l] = build_ref_pic_list(
                        l,
                        poc,
                        &header.st_rps,
                        &pictures,
                        &lt_curr,
                        num[l],
                        entries[l],
                    )?;
                }
            }
            recon.set_references(
                [0, 1].map(|l| {
                    lists[l]
                        .iter()
                        .map(|r| (&dpb[r.slot].frame, r.poc, r.long_term))
                        .collect()
                }),
                [header.pred_weights.clone(), header.pred_weights_l1.clone()],
            );
            let pocs = [0, 1].map(|l| lists[l].iter().map(|r| r.poc).collect::<Vec<i32>>());
            let long_term =
                [0, 1].map(|l| lists[l].iter().map(|r| r.long_term).collect::<Vec<bool>>());
            let mut col = None;
            if inter && header.slice_temporal_mvp_enabled_flag {
                let from = usize::from(!header.collocated_from_l0_flag);
                let r = lists[from]
                    .get(usize::from(header.collocated_ref_idx))
                    .ok_or(Error::Invalid("collocated_ref_idx out of range"))?;
                col = Some(&dpb[r.slot].motion);
            }
            let refs = SliceRefs {
                poc,
                lists: [&pocs[0], &pocs[1]],
                long_term: [&long_term[0], &long_term[1]],
                col,
                collocated_from_l0: header.collocated_from_l0_flag,
                no_backward_pred: pocs.iter().flatten().all(|&p| p <= poc),
                pic_size: (
                    sps.pic_width_in_luma_samples,
                    sps.pic_height_in_luma_samples,
                ),
                log2_ctb: sps.log2_ctb_size,
            };
            let data = &nal.rbsp[header.header_size..];
            decode_slice_segment(
                pic,
                &header,
                header.slice_qp_y(&self.sets)?,
                data,
                inter.then_some(&refs),
                recon,
            )?;
            if let Some(e) = recon.error.take() {
                return Err(e);
            }
            if !header.dependent_slice_segment_flag {
                previous = Some(header);
            }
        }
        let (pic, recon) = state.ok_or(Error::Invalid("no slice in access unit"))?;
        if !pic.is_complete() {
            return Err(Error::Invalid("picture is incomplete"));
        }
        Ok(recon.finish_with_motion(self.options.skip_loop_filters, poc))
    }
}

/// The DPB slot of the long-term picture `lt` of the current picture (POC `poc`): the
/// picture with that POC, or with those POC LSBs when its MSBs are not coded (§8.3.2).
fn find_long_term(
    dpb: &[DecodedPicture],
    lt: &LongTermRef,
    poc: i32,
    header: &SliceHeader,
    log2_max_poc_lsb: u8,
) -> Option<usize> {
    let target = lt.poc(poc, header.slice_pic_order_cnt_lsb, log2_max_poc_lsb);
    let mask = (1i32 << log2_max_poc_lsb) - 1;
    dpb.iter().position(|p| {
        if lt.delta_poc_msb_cycle.is_some() {
            p.poc == target
        } else {
            p.poc & mask == target
        }
    })
}

/// Whether every slice of the access unit is an I slice.
fn slices_are_intra(
    nals: &[NalUnit<'_>],
    sets: &ParameterSets,
    first: &SliceHeader,
) -> Result<bool, Error> {
    if first.slice_type != SliceType::I {
        return Ok(false);
    }
    let mut previous = first.clone();
    for nal in nals
        .iter()
        .filter(|n| n.header.unit_type.is_slice())
        .skip(1)
    {
        let header = SliceHeader::parse(&nal.rbsp, &nal.header, sets, Some(&previous))?;
        if header.slice_type != SliceType::I {
            return Ok(false);
        }
        if !header.dependent_slice_segment_flag {
            previous = header;
        }
    }
    Ok(true)
}
