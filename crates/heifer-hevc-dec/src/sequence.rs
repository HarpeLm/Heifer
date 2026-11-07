//! Decoding of a sequence of pictures (image sequences, bursts, video): picture order count,
//! reference picture sets and the decoded picture buffer (§8.3), then each picture's slices
//! with inter prediction from the pictures kept for reference.
//!
//! Pictures are returned in decoding order. P slices are supported; B slices, long-term
//! reference pictures and weighted prediction are rejected with [`Error::Unimplemented`].

use crate::Error;
use crate::decoder::{DEFAULT_MAX_PIXELS, DecodeOptions};
use crate::inter::{MotionField, SliceRefs};
use crate::nal::{NalUnit, NalUnitType, split_annex_b};
use crate::params::ParameterSets;
use crate::recon::{Frame, Reconstructor};
use crate::refs::{PocState, RefPic, build_ref_pic_list0};
use crate::scan::tiles_fit;
use crate::sei::parse_picture_hash;
use crate::slice::{SliceHeader, SliceType};
use crate::syntax::{Picture, decode_slice_segment};

/// A decoded picture kept for reference.
#[derive(Debug, Clone)]
struct DecodedPicture {
    poc: i32,
    /// The filtered picture at its coded size (before cropping).
    frame: Frame,
    /// Its motion, for temporal motion vector prediction.
    motion: MotionField,
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
            first.header.temporal_id,
        );

        // Reference picture set (§8.3.2): keep only the pictures it lists.
        if new_sequence {
            self.dpb.clear();
        } else {
            let kept: Vec<i32> = header
                .st_rps
                .negative
                .iter()
                .chain(&header.st_rps.positive)
                .map(|r| poc + r.delta_poc)
                .collect();
            self.dpb.retain(|p| kept.contains(&p.poc));
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
        self.dpb.push(DecodedPicture { poc, frame, motion });
        // After an end of sequence NAL unit, the next picture starts a new sequence.
        if nals
            .iter()
            .any(|n| n.header.unit_type == NalUnitType::Other(36))
        {
            self.started = false;
        }
        Ok(cropped)
    }

    /// Decodes the slices of the current picture, whose POC is `poc`.
    fn decode_slices(&self, nals: &[NalUnit<'_>], poc: i32) -> Result<(Frame, MotionField), Error> {
        let dpb = &self.dpb;
        let pictures: Vec<RefPic> = dpb
            .iter()
            .enumerate()
            .map(|(slot, p)| RefPic { slot, poc: p.poc })
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

            let mut l0_pocs = Vec::new();
            let mut col = None;
            if header.slice_type == SliceType::P {
                let num_active = usize::from(header.num_ref_idx_l0_active_minus1) + 1;
                let l0 = build_ref_pic_list0(
                    poc,
                    &header.st_rps,
                    &pictures,
                    num_active,
                    header.list_entry_l0.as_deref(),
                )?;
                recon.set_references(
                    l0.iter().map(|r| (&dpb[r.slot].frame, r.poc)).collect(),
                    header.pred_weights.clone(),
                );
                l0_pocs = l0.iter().map(|r| r.poc).collect();
                if header.slice_temporal_mvp_enabled_flag {
                    let r = l0
                        .get(usize::from(header.collocated_ref_idx))
                        .ok_or(Error::Invalid("collocated_ref_idx out of range"))?;
                    col = Some(&dpb[r.slot].motion);
                }
            }
            let refs = SliceRefs {
                poc,
                lists: [&l0_pocs, &[]],
                col,
                collocated_from_l0: header.collocated_from_l0_flag,
                no_backward_pred: l0_pocs.iter().all(|&p| p <= poc),
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
                (header.slice_type == SliceType::P).then_some(&refs),
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
