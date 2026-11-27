//! Decoding of a complete still picture from an HEVC Annex B bitstream.

use crate::Error;
use crate::nal::NalUnitType;
use crate::nal::{NalUnit, split_annex_b};
use crate::params::{ParameterSets, Sps};
pub use crate::recon::{CropWindow, DecodedFrameView, Frame, Frame8};
use crate::recon::{Reconstructor, ReconstructorBuffers};
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
    frame8: Frame8,
    sao_scratch8: Vec<u8>,
}

struct DecodeMeta {
    is_u8: bool,
    crop: CropWindow,
    has_crop: bool,
    check: HashCheck,
}

enum ReconState<'a> {
    U8 {
        recon: Reconstructor<'a, Frame8>,
        orig_frame: Frame,
        orig_sao: Vec<u16>,
    },
    U16(Reconstructor<'a, Frame>),
}

impl<'a> ReconState<'a> {
    fn start_slice(&mut self, header: &SliceHeader) {
        match self {
            Self::U8 { recon, .. } => recon.start_slice(header),
            Self::U16(recon) => recon.start_slice(header),
        }
    }

    fn decode_slice(
        &mut self,
        pic: &mut Picture<'_>,
        header: &SliceHeader,
        qp: i32,
        data: &[u8],
    ) -> Result<(), Error> {
        match self {
            Self::U8 { recon, .. } => {
                decode_slice_segment(pic, header, qp, data, None, recon)?;
                if let Some(e) = recon.error.take() {
                    return Err(e);
                }
            }
            Self::U16(recon) => {
                decode_slice_segment(pic, header, qp, data, None, recon)?;
                if let Some(e) = recon.error.take() {
                    return Err(e);
                }
            }
        }
        Ok(())
    }
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
    let meta = decode_picture_with_workspace(stream, options, &mut workspace, false)?;
    let frame = if meta.has_crop {
        workspace.cropped
    } else {
        workspace.recon.frame
    };
    Ok((frame, meta.check))
}

/// Like [`decode_picture_checked`], reusing heap allocations in `workspace` and borrowing the
/// decoded frame from it.
pub fn decode_picture_into<'w>(
    stream: &[u8],
    options: DecodeOptions,
    workspace: &'w mut DecodeWorkspace,
) -> Result<(&'w Frame, HashCheck), Error> {
    let meta = decode_picture_with_workspace(stream, options, workspace, false)?;
    let frame = if meta.has_crop {
        &workspace.cropped
    } else {
        &workspace.recon.frame
    };
    Ok((frame, meta.check))
}

/// Like [`decode_picture_into`], decoding 8-bit pictures directly into `u8` planes and returning
/// a borrowed [`DecodedFrameView`] with its conformance [`CropWindow`] instead of copying out
/// cropped planes.
pub fn decode_picture_view_into<'w>(
    stream: &[u8],
    options: DecodeOptions,
    workspace: &'w mut DecodeWorkspace,
) -> Result<(DecodedFrameView<'w>, HashCheck), Error> {
    let meta = decode_picture_with_workspace(stream, options, workspace, true)?;
    let view = if meta.is_u8 {
        DecodedFrameView::U8(&workspace.frame8, meta.crop)
    } else {
        DecodedFrameView::U16(&workspace.recon.frame, meta.crop)
    };
    Ok((view, meta.check))
}

fn check_hash(
    hash: &Option<PictureHash>,
    skip_loop_filters: bool,
    compute: impl FnOnce(&PictureHash) -> PictureHash,
) -> Result<HashCheck, Error> {
    let _ = &compute;
    match hash {
        Some(expected) if !skip_loop_filters => {
            if compute(expected) != *expected {
                return Err(Error::HashMismatch(expected.kind()));
            }
            Ok(HashCheck::Verified(expected.kind()))
        }
        _ => Ok(HashCheck::NotChecked),
    }
}

fn decode_picture_with_workspace(
    stream: &[u8],
    options: DecodeOptions,
    ws: &mut DecodeWorkspace,
    allow_u8: bool,
) -> Result<DecodeMeta, Error> {
    let nals: Vec<NalUnit<'_>> = split_annex_b(stream)
        .map(NalUnit::parse)
        .collect::<Result<_, _>>()?;
    // Parameter sets are activated in stream order: those sent before the first slice are
    // used for the first picture (later ones may redefine the same IDs for following pictures).
    let mut sets = ParameterSets::default();
    for nal in nals.iter().take_while(|n| !n.header.unit_type.is_slice()) {
        sets.add(nal)?;
    }

    let mut state: Option<(Picture<'_>, ReconState<'_>)> = None;
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
        let (pic, recon_state) = state.get_or_insert_with(|| {
            let pic = Picture::from_buffers(sps, pps, std::mem::take(&mut ws.pic));
            let recon_state = if allow_u8 && sps.bit_depth_luma == 8 && sps.bit_depth_chroma == 8 {
                let orig_frame = std::mem::take(&mut ws.recon.frame);
                let orig_sao = std::mem::take(&mut ws.recon.sao_scratch);
                ReconState::U8 {
                    recon: Reconstructor::from_buffers_8(
                        sps,
                        pps,
                        std::mem::take(&mut ws.frame8),
                        std::mem::take(&mut ws.sao_scratch8),
                        std::mem::take(&mut ws.recon),
                    ),
                    orig_frame,
                    orig_sao,
                }
            } else {
                ReconState::U16(Reconstructor::from_buffers(
                    sps,
                    pps,
                    std::mem::take(&mut ws.recon),
                ))
            };
            (pic, recon_state)
        });
        if !std::ptr::eq(pic.sps, sps) || !std::ptr::eq(pic.pps, pps) {
            return Err(Error::Invalid(
                "slices of one picture use different parameter sets",
            ));
        }
        recon_state.start_slice(&header);
        let data = &nal.rbsp[header.header_size..];
        recon_state.decode_slice(pic, &header, header.slice_qp_y(&sets)?, data)?;
        if !header.dependent_slice_segment_flag {
            previous = Some(header);
        }
    }
    let (pic, recon_state) = state.ok_or(Error::Invalid("no slice in bitstream"))?;
    if !pic.is_complete() {
        return Err(Error::Invalid("picture is incomplete"));
    }
    let sps: &Sps = pic.sps;
    let has_crop = !allow_u8 && sps.conf_win_offsets != [0; 4];
    ws.pic = pic.into_buffers();
    match recon_state {
        ReconState::U8 {
            recon,
            orig_frame,
            orig_sao,
        } => {
            let (frame8, sao8, buf) =
                recon.finish_into_buffers_8(options.skip_loop_filters, orig_frame, orig_sao);
            ws.frame8 = frame8;
            ws.sao_scratch8 = sao8;
            ws.recon = buf;
            let check = check_hash(&hash, options.skip_loop_filters, |h| {
                h.compute_like_8(&ws.frame8)
            })?;
            let crop = ws.frame8.crop_window(sps);
            Ok(DecodeMeta {
                is_u8: true,
                crop,
                has_crop: false,
                check,
            })
        }
        ReconState::U16(recon) => {
            ws.recon = recon.finish_into_buffers(options.skip_loop_filters);
            let check = check_hash(&hash, options.skip_loop_filters, |h| {
                h.compute_like(&ws.recon.frame)
            })?;
            let crop = ws.recon.frame.crop_window(sps);
            if has_crop {
                ws.recon.frame.cropped_into(sps, &mut ws.cropped);
            }
            Ok(DecodeMeta {
                is_u8: false,
                crop,
                has_crop,
                check,
            })
        }
    }
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

    fn view_to_frame(view: DecodedFrameView<'_>) -> Frame {
        match view {
            DecodedFrameView::U8(f, crop) => f.to_frame_cropped(crop),
            DecodedFrameView::U16(f, crop) => {
                let mut planes = [Vec::new(), Vec::new(), Vec::new()];
                for c in 0..3 {
                    if crop.widths[c] == 0 {
                        continue;
                    }
                    let src_w = f.widths[c] as usize;
                    let (x0, y0) = (crop.x[c] as usize, crop.y[c] as usize);
                    let (w, h) = (crop.widths[c] as usize, crop.heights[c] as usize);
                    planes[c].reserve(w * h);
                    for y in y0..y0 + h {
                        let row = y * src_w + x0;
                        planes[c].extend_from_slice(&f.planes[c][row..row + w]);
                    }
                }
                Frame {
                    widths: crop.widths,
                    heights: crop.heights,
                    planes,
                    bit_depth: f.bit_depth,
                    chroma_format: f.chroma_format,
                    full_range: f.full_range,
                    matrix_coeffs: f.matrix_coeffs,
                }
            }
        }
    }

    #[test]
    fn view_reuse_matches_fresh_decode_across_8bit_10bit_crop_and_hash() {
        let base = format!("{}/../../tests/fixtures", env!("CARGO_MANIFEST_DIR"));
        let conf = format!("{}/../../tests/conformance", env!("CARGO_MANIFEST_DIR"));
        let mut streams: Vec<(String, Vec<u8>, bool, Option<bool>)> = Vec::new();
        for (path, verify_hash, expect_u8) in [
            (format!("{base}/grid.heic"), false, Some(true)),
            (format!("{base}/alpha.heic"), false, Some(true)),
            (format!("{base}/single_image.heic"), false, Some(true)),
            (
                format!("{base}/real/heif__RGB_10__128x128.heif"),
                false,
                Some(false),
            ),
        ] {
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let f = HeifFile::parse(&bytes).unwrap();
            let primary = f.primary_item().unwrap().id;
            if let Ok(bs) = f.hevc_bitstream(primary) {
                streams.push((path.clone(), bs, verify_hash, expect_u8));
            }
            for child in f.referenced_items(primary, b"dimg") {
                if let Ok(bs) = f.hevc_bitstream(*child) {
                    streams.push((format!("{path}#child={child}"), bs, verify_hash, expect_u8));
                }
            }
        }
        for (dir_name, expect_u8) in [
            ("CONFWIN_A_Sony_1", true),
            ("CCP_8bit_RExt_QCOM_1", true),
            ("DBLK_A_MAIN10_VIXS_4", false),
        ] {
            let dir = std::path::Path::new(&conf).join(dir_name);
            let mut stack = vec![dir];
            while let Some(p) = stack.pop() {
                for f in std::fs::read_dir(&p).into_iter().flatten().flatten() {
                    let path = f.path();
                    if path.is_dir() {
                        stack.push(path);
                    } else if matches!(
                        path.extension().and_then(|e| e.to_str()),
                        Some("bit" | "bin")
                    ) && let Ok(data) = std::fs::read(&path)
                    {
                        streams.push((dir_name.to_string(), data, true, Some(expect_u8)));
                    }
                }
            }
        }
        if streams.is_empty() {
            return;
        }

        let mut ws = DecodeWorkspace::default();
        for (idx, (label, stream, verify_hash, expect_u8)) in streams.iter().enumerate() {
            let opts = DecodeOptions::new().with_verify_hash(*verify_hash);
            let (expected, expected_check) = decode_picture_checked(stream, opts).unwrap();
            let (view, actual_check) = decode_picture_view_into(stream, opts, &mut ws).unwrap();
            assert_eq!(actual_check, expected_check, "{label} #{idx}");
            if let Some(u8_expected) = expect_u8 {
                assert_eq!(
                    matches!(view, DecodedFrameView::U8(_, _)),
                    *u8_expected,
                    "{label} #{idx}"
                );
            }
            if label == "CONFWIN_A_Sony_1" {
                let crop = view.crop();
                assert!(
                    crop.x != [0; 3] || crop.y != [0; 3],
                    "expected non-zero crop window on {label}"
                );
            }
            assert_eq!(view_to_frame(view), expected, "{label} #{idx}");
        }

        // Recovery after a failed decode must keep view workspace valid for mixed 8-bit / 10-bit.
        assert!(decode_picture_view_into(&[0, 0, 0, 1, 0], DecodeOptions::new(), &mut ws).is_err());
        for (idx, (label, stream, verify_hash, _)) in streams.iter().enumerate().rev() {
            let opts = DecodeOptions::new().with_verify_hash(*verify_hash);
            let (expected, expected_check) = decode_picture_checked(stream, opts).unwrap();
            let (view, actual_check) = decode_picture_view_into(stream, opts, &mut ws).unwrap();
            assert_eq!(actual_check, expected_check, "rev {label} #{idx}");
            assert_eq!(view_to_frame(view), expected, "rev {label} #{idx}");
        }
    }
}
