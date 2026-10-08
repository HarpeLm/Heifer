//! Decoding of a complete still picture from an HEVC Annex B bitstream.

use crate::Error;
use crate::nal::NalUnitType;
use crate::nal::{NalUnit, split_annex_b};
use crate::params::ParameterSets;
use crate::recon::{Frame, Reconstructor};
use crate::sei::{PictureHash, parse_picture_hash};
use crate::slice::SliceHeader;
use crate::syntax::{Picture, decode_slice_segment};

/// Options for [`decode_picture`].
#[derive(Debug, Clone, Copy, Default)]
pub struct DecodeOptions {
    /// Skip the in-loop filters (deblocking and SAO). Useful for debugging.
    pub skip_loop_filters: bool,
    /// Check the decoded picture against the hash SEI message, when the stream has one, and
    /// fail with [`Error::HashMismatch`] if it differs.
    pub verify_hash: bool,
    /// Maximum picture size, in luma samples, accepted before allocating memory (protects
    /// against malicious streams). `0` means [`DEFAULT_MAX_PIXELS`].
    pub max_pixels: u64,
}

/// Default limit on the picture size: 2^27 luma samples (e.g. 16384×8192), well above the
/// largest HEVC level (8192×4320) and HEIF tiles.
pub const DEFAULT_MAX_PIXELS: u64 = 1 << 27;

/// Result of the decoded picture hash check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashCheck {
    /// The stream has no decoded picture hash for this picture, or checking was not requested.
    NotChecked,
    /// The decoded picture matches the hash (MD5, CRC or checksum).
    Verified(&'static str),
}

/// Decodes the first picture of an HEVC Annex B bitstream (as produced by
/// `heifer_isobmff::HeifFile::hevc_bitstream`), cropped to its conformance window.
pub fn decode_picture(stream: &[u8], options: DecodeOptions) -> Result<Frame, Error> {
    decode_picture_checked(stream, options).map(|(frame, _)| frame)
}

/// Like [`decode_picture`], also reporting whether the decoded picture hash was verified.
pub fn decode_picture_checked(
    stream: &[u8],
    options: DecodeOptions,
) -> Result<(Frame, HashCheck), Error> {
    let nals: Vec<NalUnit<'_>> = split_annex_b(stream)
        .map(NalUnit::parse)
        .collect::<Result<_, _>>()?;
    // Parameter sets are activated in stream order: those sent before the first slice are
    // used for the first picture (later ones may redefine the same IDs for following pictures).
    let mut sets = ParameterSets::default();
    for nal in nals.iter().take_while(|n| !n.header.unit_type.is_slice()) {
        sets.add(nal)?;
    }

    let mut state: Option<(Picture<'_>, Reconstructor<'_>)> = None;
    let mut previous: Option<SliceHeader> = None;
    let mut hash: Option<PictureHash> = None;
    for nal in &nals {
        match nal.header.unit_type {
            // The hash of a picture is sent in a suffix SEI after its slices.
            NalUnitType::SuffixSei if state.is_some() && hash.is_none() && options.verify_hash => {
                let components = if sets.sps.iter().flatten().any(|s| s.chroma_format_idc == 0) {
                    1
                } else {
                    3
                };
                hash = parse_picture_hash(&nal.rbsp, components)?;
                continue;
            }
            t if !t.is_slice() => continue,
            _ => {}
        }
        // first_slice_segment_in_pic_flag is the first bit: stop before parsing the next
        // picture's header, which may use inter prediction.
        let first_in_pic = nal.rbsp.first().is_some_and(|b| b & 0x80 != 0);
        if first_in_pic && state.is_some() {
            break; // Only the first picture is decoded.
        }
        let header = SliceHeader::parse(&nal.rbsp, &nal.header, &sets, previous.as_ref())?;
        let (pps, sps) = sets.get(usize::from(header.slice_pic_parameter_set_id))?;
        let max_pixels = if options.max_pixels == 0 {
            DEFAULT_MAX_PIXELS
        } else {
            options.max_pixels
        };
        if u64::from(sps.pic_width_in_luma_samples) * u64::from(sps.pic_height_in_luma_samples)
            > max_pixels
        {
            return Err(Error::Unimplemented(
                "picture larger than the configured size limit",
            ));
        }
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
    let sps = pic.sps;
    let frame = recon.finish(options.skip_loop_filters);
    let check = match &hash {
        Some(expected) if !options.skip_loop_filters => {
            if expected.compute_like(&frame) != *expected {
                return Err(Error::HashMismatch(expected.kind()));
            }
            HashCheck::Verified(expected.kind())
        }
        _ => HashCheck::NotChecked,
    };
    // Most pictures have no conformance window: avoid copying them.
    let frame = if sps.conf_win_offsets == [0; 4] {
        frame
    } else {
        frame.cropped(sps)
    };
    Ok((frame, check))
}
