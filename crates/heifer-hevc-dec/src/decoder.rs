//! Decoding of a complete still picture from an HEVC Annex B bitstream.

use crate::Error;
use crate::nal::NalUnitType;
use crate::nal::{NalUnit, split_annex_b};
use crate::params::ParameterSets;
use crate::recon::{Frame, Reconstructor, ReconstructorBuffers};
use crate::scan::tiles_fit;
use crate::sei::{PictureHash, parse_picture_hash};
use crate::slice::SliceHeader;
use crate::syntax::{Picture, PictureBuffers, decode_slice_segment};

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

/// Construction methods: they keep working when new options are added, unlike a struct
/// literal, so prefer them to setting the fields directly.
impl DecodeOptions {
    /// The default options: loop filters on, no hash check, the default size limit.
    pub const fn new() -> Self {
        Self {
            skip_loop_filters: false,
            verify_hash: false,
            max_pixels: 0,
        }
    }

    /// Sets [`skip_loop_filters`](Self::skip_loop_filters).
    #[must_use]
    pub const fn with_skip_loop_filters(mut self, skip: bool) -> Self {
        self.skip_loop_filters = skip;
        self
    }

    /// Sets [`verify_hash`](Self::verify_hash).
    #[must_use]
    pub const fn with_verify_hash(mut self, verify: bool) -> Self {
        self.verify_hash = verify;
        self
    }

    /// Sets [`max_pixels`](Self::max_pixels): `0` means [`DEFAULT_MAX_PIXELS`].
    #[must_use]
    pub const fn with_max_pixels(mut self, max_pixels: u64) -> Self {
        self.max_pixels = max_pixels;
        self
    }
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

/// Reusable scratch buffers for decoding many pictures (such as tiles of a grid) on one
/// thread without repeating picture/plane heap allocations.
#[derive(Debug, Default)]
pub struct DecodeWorkspace {
    pic: PictureBuffers,
    recon: ReconstructorBuffers,
    cropped: Frame,
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
    let mut workspace = DecodeWorkspace::default();
    let (cropped, check) = decode_picture_with_workspace(stream, options, &mut workspace)?;
    let frame = if cropped {
        workspace.cropped
    } else {
        workspace.recon.frame
    };
    Ok((frame, check))
}

/// Like [`decode_picture_checked`], reusing heap allocations in `workspace` and borrowing the
/// decoded frame from it.
pub fn decode_picture_into<'w>(
    stream: &[u8],
    options: DecodeOptions,
    workspace: &'w mut DecodeWorkspace,
) -> Result<(&'w Frame, HashCheck), Error> {
    let (cropped, check) = decode_picture_with_workspace(stream, options, workspace)?;
    let frame = if cropped {
        &workspace.cropped
    } else {
        &workspace.recon.frame
    };
    Ok((frame, check))
}

fn decode_picture_with_workspace(
    stream: &[u8],
    options: DecodeOptions,
    ws: &mut DecodeWorkspace,
) -> Result<(bool, HashCheck), Error> {
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
        if let Some(tiles) = &pps.tiles {
            let (width, height) = sps.pic_size_in_ctbs();
            if !tiles_fit(tiles, width, height) {
                return Err(Error::Invalid(
                    "tile layout does not match the picture size",
                ));
            }
        }
        let (pic, recon) = state.get_or_insert_with(|| {
            (
                Picture::from_buffers(sps, pps, std::mem::take(&mut ws.pic)),
                Reconstructor::from_buffers(sps, pps, std::mem::take(&mut ws.recon)),
            )
        });
        recon.start_slice(&header);
        let data = &nal.rbsp[header.header_size..];
        decode_slice_segment(pic, &header, header.slice_qp_y(&sets)?, data, None, recon)?;
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
    let has_crop = sps.conf_win_offsets != [0; 4];
    ws.pic = pic.into_buffers();
    ws.recon = recon.finish_into_buffers(options.skip_loop_filters);
    let check = match &hash {
        Some(expected) if !options.skip_loop_filters => {
            if expected.compute_like(&ws.recon.frame) != *expected {
                return Err(Error::HashMismatch(expected.kind()));
            }
            HashCheck::Verified(expected.kind())
        }
        _ => HashCheck::NotChecked,
    };
    if has_crop {
        ws.recon.frame.cropped_into(sps, &mut ws.cropped);
    }
    Ok((has_crop, check))
}

#[cfg(test)]
mod tests {
    use super::*;
    use heifer_isobmff::HeifFile;

    #[test]
    fn workspace_reuse_matches_fresh_decode_across_sizes_and_errors() {
        let base = format!("{}/../../tests/fixtures", env!("CARGO_MANIFEST_DIR"));
        let Ok(grid_bytes) = std::fs::read(format!("{base}/grid.heic")) else {
            return;
        };
        let Ok(alpha_bytes) = std::fs::read(format!("{base}/alpha.heic")) else {
            return;
        };
        let Ok(single_bytes) = std::fs::read(format!("{base}/single_image.heic")) else {
            return;
        };

        let mut streams = Vec::new();
        for (file_bytes, rel) in [(&grid_bytes, b"dimg"), (&alpha_bytes, b"dimg")] {
            let f = HeifFile::parse(file_bytes).unwrap();
            let primary = f.primary_item().unwrap().id;
            if let Ok(bs) = f.hevc_bitstream(primary) {
                streams.push(bs);
            }
            for child in f.referenced_items(primary, rel) {
                if let Ok(bs) = f.hevc_bitstream(*child) {
                    streams.push(bs);
                }
            }
            for aux in f.referencing_items(primary, b"auxl") {
                if let Ok(bs) = f.hevc_bitstream(aux) {
                    streams.push(bs);
                }
            }
        }
        let single = HeifFile::parse(&single_bytes).unwrap();
        streams.push(
            single
                .hevc_bitstream(single.primary_item().unwrap().id)
                .unwrap(),
        );
        assert!(streams.len() >= 3);

        let mut ws = DecodeWorkspace::default();
        let opts = DecodeOptions::new();
        for (idx, stream) in streams.iter().enumerate() {
            let expected = decode_picture(stream, opts).unwrap();
            let (actual, _) = decode_picture_into(stream, opts, &mut ws).unwrap();
            assert_eq!(actual, &expected, "mismatch before error at stream {idx}");
        }

        // A failing decode must leave the workspace usable for subsequent pictures.
        assert!(decode_picture_into(&[0, 0, 0, 1, 0], opts, &mut ws).is_err());
        for (idx, stream) in streams.iter().enumerate().rev() {
            let expected = decode_picture(stream, opts).unwrap();
            let (actual, _) = decode_picture_into(stream, opts, &mut ws).unwrap();
            assert_eq!(actual, &expected, "mismatch after error at stream {idx}");
        }
    }
}
