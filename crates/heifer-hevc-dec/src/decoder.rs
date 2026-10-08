//! Decoding of a complete still picture from an HEVC Annex B bitstream.

use crate::Error;
use crate::nal::{NalUnit, split_annex_b};
use crate::params::ParameterSets;
use crate::recon::{Frame, Reconstructor};
use crate::slice::SliceHeader;
use crate::syntax::{Picture, decode_slice_segment};

/// Options for [`decode_picture`].
#[derive(Debug, Clone, Copy, Default)]
pub struct DecodeOptions {
    /// Skip the in-loop filters (deblocking and SAO). Useful for debugging.
    ///
    /// The filters are not implemented yet: pictures are currently always returned unfiltered.
    pub skip_loop_filters: bool,
}

/// Decodes the first picture of an HEVC Annex B bitstream (as produced by
/// `heifer_isobmff::HeifFile::hevc_bitstream`), cropped to its conformance window.
pub fn decode_picture(stream: &[u8], options: DecodeOptions) -> Result<Frame, Error> {
    let nals: Vec<NalUnit<'_>> = split_annex_b(stream)
        .map(NalUnit::parse)
        .collect::<Result<_, _>>()?;
    let mut sets = ParameterSets::default();
    for nal in &nals {
        sets.add(nal)?;
    }
    let _ = options;

    let mut state: Option<(Picture<'_>, Reconstructor<'_>)> = None;
    let mut previous: Option<SliceHeader> = None;
    for nal in nals.iter().filter(|n| n.header.unit_type.is_slice()) {
        let header = SliceHeader::parse(&nal.rbsp, &nal.header, &sets, previous.as_ref())?;
        if header.first_slice_segment_in_pic_flag && state.is_some() {
            break; // Only the first picture is decoded.
        }
        let (pps, sps) = sets.get(usize::from(header.slice_pic_parameter_set_id))?;
        let (pic, recon) =
            state.get_or_insert_with(|| (Picture::new(sps, pps), Reconstructor::new(sps, pps)));
        recon.start_slice(&header);
        let data = &nal.rbsp[header.header_size..];
        decode_slice_segment(pic, &header, header.slice_qp_y(&sets)?, data, recon)?;
        if let Some(e) = recon.error.take() {
            return Err(e);
        }
        if !header.dependent_slice_segment_flag {
            previous = Some(header);
        }
    }
    let (pic, recon) = state.ok_or(Error::Invalid("no slice in bitstream"))?;
    if !pic.is_complete() {
        return Err(Error::Invalid("picture is incomplete"));
    }
    Ok(recon.frame.cropped(pic.sps))
}
