//! Sequence header OBU (AV1 §5.5 and §6.4).
//!
//! The sequence header gives the limits and tools of every frame that follows: profile, maximum
//! picture size, bit depth, chroma subsampling, colour description and which coding tools may be
//! used. Timing and decoder model fields are read but not kept: a still image does not need them.

use crate::Error;
use crate::bits::BitReader;

/// A parsed sequence header.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SequenceHeader {
    /// `seq_profile`: 0 = Main, 1 = High, 2 = Professional.
    pub profile: u8,
    /// `still_picture`: the bitstream holds one picture.
    pub still_picture: bool,
    /// `reduced_still_picture_header`: the short form used by some AVIF files.
    pub reduced_still_picture_header: bool,
    /// `seq_level_idx` of the first operating point.
    pub level_idx: u8,
    /// `seq_tier` of the first operating point.
    pub tier: u8,
    /// `operating_point_idc` of each operating point (which layers it decodes).
    pub operating_point_idc: Vec<u16>,
    /// `frame_width_bits_minus_1 + 1`.
    pub frame_width_bits: u8,
    /// `frame_height_bits_minus_1 + 1`.
    pub frame_height_bits: u8,
    /// Largest frame width, in pixels.
    pub max_frame_width: u32,
    /// Largest frame height, in pixels.
    pub max_frame_height: u32,
    /// `frame_id_numbers_present_flag`.
    pub frame_id_numbers_present: bool,
    /// `delta_frame_id_length_minus_2 + 2`, when frame ids are present.
    pub delta_frame_id_length: u8,
    /// `additional_frame_id_length_minus_1 + 1`, when frame ids are present.
    pub additional_frame_id_length: u8,
    /// `use_128x128_superblock`: superblocks are 128×128 instead of 64×64.
    pub use_128x128_superblock: bool,
    /// `enable_filter_intra`.
    pub enable_filter_intra: bool,
    /// `enable_intra_edge_filter`.
    pub enable_intra_edge_filter: bool,
    /// `enable_interintra_compound`.
    pub enable_interintra_compound: bool,
    /// `enable_masked_compound`.
    pub enable_masked_compound: bool,
    /// `enable_warped_motion`.
    pub enable_warped_motion: bool,
    /// `enable_dual_filter`.
    pub enable_dual_filter: bool,
    /// `enable_order_hint`.
    pub enable_order_hint: bool,
    /// `enable_jnt_comp`.
    pub enable_jnt_comp: bool,
    /// `enable_ref_frame_mvs`.
    pub enable_ref_frame_mvs: bool,
    /// `seq_force_screen_content_tools`: 0, 1, or 2 (chosen per frame).
    pub seq_force_screen_content_tools: u8,
    /// `seq_force_integer_mv`: 0, 1, or 2 (chosen per frame).
    pub seq_force_integer_mv: u8,
    /// `OrderHintBits`.
    pub order_hint_bits: u8,
    /// `enable_superres`.
    pub enable_superres: bool,
    /// `enable_cdef`.
    pub enable_cdef: bool,
    /// `enable_restoration`.
    pub enable_restoration: bool,
    /// Colour configuration.
    pub color: ColorConfig,
    /// `film_grain_params_present`.
    pub film_grain_params_present: bool,
    /// Decoder model fields that change the frame header syntax.
    pub(crate) timing: FrameTiming,
}

/// The sequence header fields that add `temporal_point_info()` and `buffer_removal_time` to frame
/// headers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct FrameTiming {
    /// `frame_presentation_time_length_minus_1 + 1` when frame headers carry a presentation time
    /// (`decoder_model_info_present_flag && !equal_picture_interval`), else 0.
    pub(crate) presentation_time_bits: u32,
    /// `buffer_removal_time_length_minus_1 + 1` when the decoder model is present, else 0.
    pub(crate) buffer_removal_time_bits: u32,
    /// `decoder_model_present_for_this_op` of each operating point.
    pub(crate) decoder_model_present: Vec<bool>,
}

/// `color_config()` (AV1 §5.5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ColorConfig {
    /// Bits per sample: 8, 10 or 12.
    pub bit_depth: u8,
    /// Luma only.
    pub monochrome: bool,
    /// `color_primaries` (ITU-T H.273), 2 when not described.
    pub color_primaries: u8,
    /// `transfer_characteristics` (ITU-T H.273), 2 when not described.
    pub transfer_characteristics: u8,
    /// `matrix_coefficients` (ITU-T H.273), 2 when not described.
    pub matrix_coefficients: u8,
    /// `color_range`: full range (`true`) or studio range.
    pub full_range: bool,
    /// Horizontal chroma subsampling.
    pub subsampling_x: bool,
    /// Vertical chroma subsampling.
    pub subsampling_y: bool,
    /// `chroma_sample_position` (0 = unknown, 1 = vertical, 2 = colocated).
    pub chroma_sample_position: u8,
    /// `separate_uv_delta_q`.
    pub separate_uv_delta_q: bool,
}

/// `SELECT_SCREEN_CONTENT_TOOLS` and `SELECT_INTEGER_MV`: the choice is made per frame.
const SELECT: u8 = 2;

impl SequenceHeader {
    /// Parses the payload of a sequence header OBU.
    pub fn parse(payload: &[u8]) -> Result<Self, Error> {
        let mut r = BitReader::new(payload);
        let profile = r.f(3)? as u8;
        if profile > 2 {
            return Err(Error::Invalid("seq_profile is reserved"));
        }
        let still_picture = r.flag()?;
        let reduced_still_picture_header = r.flag()?;
        if reduced_still_picture_header && !still_picture {
            return Err(Error::Invalid(
                "reduced_still_picture_header without still_picture",
            ));
        }
        let mut operating_point_idc = Vec::new();
        let mut timing = FrameTiming::default();
        let (level_idx, tier);
        if reduced_still_picture_header {
            operating_point_idc.push(0);
            level_idx = r.f(5)? as u8;
            tier = 0;
        } else {
            let timing_info_present = r.flag()?;
            let mut decoder_model_info_present = false;
            let mut buffer_delay_length = 0;
            let mut equal_picture_interval = false;
            let mut presentation_time_bits = 0;
            if timing_info_present {
                r.f(32)?; // num_units_in_display_tick
                r.f(32)?; // time_scale
                equal_picture_interval = r.flag()?;
                if equal_picture_interval {
                    r.uvlc()?; // num_ticks_per_picture_minus_1
                }
                decoder_model_info_present = r.flag()?;
                if decoder_model_info_present {
                    buffer_delay_length = r.f(5)? + 1;
                    r.f(32)?; // num_units_in_decoding_tick
                    timing.buffer_removal_time_bits = r.f(5)? + 1;
                    presentation_time_bits = r.f(5)? + 1;
                }
            }
            if decoder_model_info_present && !equal_picture_interval {
                timing.presentation_time_bits = presentation_time_bits;
            }
            let initial_display_delay_present = r.flag()?;
            let count = r.f(5)? + 1;
            let mut first = None;
            for _ in 0..count {
                operating_point_idc.push(r.f(12)? as u16);
                let level = r.f(5)? as u8;
                let op_tier = if level > 7 { r.f(1)? as u8 } else { 0 };
                first.get_or_insert((level, op_tier));
                let decoder_model_present = decoder_model_info_present && r.flag()?;
                timing.decoder_model_present.push(decoder_model_present);
                if decoder_model_present {
                    r.f(buffer_delay_length)?; // decoder_buffer_delay
                    r.f(buffer_delay_length)?; // encoder_buffer_delay
                    r.flag()?; // low_delay_mode_flag
                }
                if initial_display_delay_present && r.flag()? {
                    r.f(4)?; // initial_display_delay_minus_1
                }
            }
            (level_idx, tier) = first.unwrap_or((0, 0));
        }
        let frame_width_bits = r.f(4)? as u8 + 1;
        let frame_height_bits = r.f(4)? as u8 + 1;
        let max_frame_width = r.f(u32::from(frame_width_bits))? + 1;
        let max_frame_height = r.f(u32::from(frame_height_bits))? + 1;
        let frame_id_numbers_present = !reduced_still_picture_header && r.flag()?;
        let (delta_frame_id_length, additional_frame_id_length) = if frame_id_numbers_present {
            (r.f(4)? as u8 + 2, r.f(3)? as u8 + 1)
        } else {
            (0, 0)
        };
        let use_128x128_superblock = r.flag()?;
        let enable_filter_intra = r.flag()?;
        let enable_intra_edge_filter = r.flag()?;
        let mut h = Self {
            profile,
            still_picture,
            reduced_still_picture_header,
            level_idx,
            tier,
            operating_point_idc,
            frame_width_bits,
            frame_height_bits,
            max_frame_width,
            max_frame_height,
            frame_id_numbers_present,
            delta_frame_id_length,
            additional_frame_id_length,
            use_128x128_superblock,
            enable_filter_intra,
            enable_intra_edge_filter,
            enable_interintra_compound: false,
            enable_masked_compound: false,
            enable_warped_motion: false,
            enable_dual_filter: false,
            enable_order_hint: false,
            enable_jnt_comp: false,
            enable_ref_frame_mvs: false,
            seq_force_screen_content_tools: SELECT,
            seq_force_integer_mv: SELECT,
            order_hint_bits: 0,
            enable_superres: false,
            enable_cdef: false,
            enable_restoration: false,
            color: ColorConfig {
                bit_depth: 8,
                monochrome: false,
                color_primaries: 2,
                transfer_characteristics: 2,
                matrix_coefficients: 2,
                full_range: false,
                subsampling_x: true,
                subsampling_y: true,
                chroma_sample_position: 0,
                separate_uv_delta_q: false,
            },
            film_grain_params_present: false,
            timing,
        };
        if !reduced_still_picture_header {
            h.enable_interintra_compound = r.flag()?;
            h.enable_masked_compound = r.flag()?;
            h.enable_warped_motion = r.flag()?;
            h.enable_dual_filter = r.flag()?;
            h.enable_order_hint = r.flag()?;
            if h.enable_order_hint {
                h.enable_jnt_comp = r.flag()?;
                h.enable_ref_frame_mvs = r.flag()?;
            }
            h.seq_force_screen_content_tools = if r.flag()? { SELECT } else { r.f(1)? as u8 };
            h.seq_force_integer_mv = if h.seq_force_screen_content_tools > 0 {
                if r.flag()? { SELECT } else { r.f(1)? as u8 }
            } else {
                SELECT
            };
            if h.enable_order_hint {
                h.order_hint_bits = r.f(3)? as u8 + 1;
            }
        }
        h.enable_superres = r.flag()?;
        h.enable_cdef = r.flag()?;
        h.enable_restoration = r.flag()?;
        h.color = color_config(&mut r, profile)?;
        h.film_grain_params_present = r.flag()?;
        Ok(h)
    }
}

/// `color_config()` (AV1 §5.5.2).
fn color_config(r: &mut BitReader<'_>, profile: u8) -> Result<ColorConfig, Error> {
    let high_bitdepth = r.flag()?;
    let bit_depth = if profile == 2 && high_bitdepth {
        if r.flag()? { 12 } else { 10 }
    } else if high_bitdepth {
        10
    } else {
        8
    };
    let monochrome = profile != 1 && r.flag()?;
    let (color_primaries, transfer_characteristics, matrix_coefficients) = if r.flag()? {
        (r.f(8)? as u8, r.f(8)? as u8, r.f(8)? as u8)
    } else {
        (2, 2, 2)
    };
    let mut c = ColorConfig {
        bit_depth,
        monochrome,
        color_primaries,
        transfer_characteristics,
        matrix_coefficients,
        full_range: false,
        subsampling_x: true,
        subsampling_y: true,
        chroma_sample_position: 0,
        separate_uv_delta_q: false,
    };
    if monochrome {
        c.full_range = r.flag()?;
        return Ok(c);
    }
    // sRGB primaries and transfer with the identity matrix: RGB, full range, 4:4:4.
    if (
        color_primaries,
        transfer_characteristics,
        matrix_coefficients,
    ) == (1, 13, 0)
    {
        c.full_range = true;
        (c.subsampling_x, c.subsampling_y) = (false, false);
        if profile == 0 || (profile == 2 && bit_depth != 12) {
            return Err(Error::Invalid("4:4:4 is not allowed in this profile"));
        }
    } else {
        c.full_range = r.flag()?;
        (c.subsampling_x, c.subsampling_y) = match profile {
            0 => (true, true),
            1 => (false, false),
            _ if bit_depth == 12 => {
                let x = r.flag()?;
                (x, x && r.flag()?)
            }
            _ => (true, false),
        };
        if c.subsampling_x && c.subsampling_y {
            c.chroma_sample_position = r.f(2)? as u8;
        }
    }
    c.separate_uv_delta_q = r.flag()?;
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::pack;

    #[test]
    fn reduced_still_picture_header() {
        // Profile 0, still, reduced, level 8; 11-bit width 1280, 10-bit height 720; no 128×128
        // superblocks, filter intra and edge filter on; superres off, CDEF and restoration on;
        // 8-bit, colour description BT.709, studio range, chroma position 1; no film grain.
        let data = pack(&[
            (0, 3),
            (1, 1),
            (1, 1),
            (8, 5),
            (10, 4),
            (9, 4),
            (1279, 11),
            (719, 10),
            (0, 1),
            (1, 1),
            (1, 1),
            (0, 1),
            (1, 1),
            (1, 1),
            (0, 1),
            (0, 1),
            (1, 1),
            (1, 8),
            (1, 8),
            (1, 8),
            (0, 1),
            (1, 2),
            (0, 1),
            (0, 1),
        ]);
        let h = SequenceHeader::parse(&data).unwrap();
        assert!(h.still_picture && h.reduced_still_picture_header);
        assert_eq!((h.profile, h.level_idx, h.tier), (0, 8, 0));
        assert_eq!((h.max_frame_width, h.max_frame_height), (1280, 720));
        assert!(!h.use_128x128_superblock && h.enable_filter_intra && h.enable_intra_edge_filter);
        assert!(!h.enable_superres && h.enable_cdef && h.enable_restoration);
        assert_eq!(
            (h.seq_force_screen_content_tools, h.seq_force_integer_mv),
            (2, 2)
        );
        assert_eq!(h.color.bit_depth, 8);
        assert_eq!(
            (h.color.color_primaries, h.color.matrix_coefficients),
            (1, 1)
        );
        assert!(h.color.subsampling_x && h.color.subsampling_y && !h.color.full_range);
        assert_eq!(h.color.chroma_sample_position, 1);
        assert!(!h.film_grain_params_present);
    }

    #[test]
    fn full_header_with_timing_and_operating_points() {
        // Profile 2, not still; timing info with a decoder model (buffer delay 10 bits); two
        // operating points, the first at level 8 (the first with a tier bit, tier 1) and a decoder
        // model; 12-bit 4:2:2, no colour description, full range; film grain present.
        let data = pack(&[
            (2, 3),
            (0, 1),
            (0, 1),
            (1, 1),
            (1, 32),
            (25, 32),
            (0, 1),
            (1, 1),
            (9, 5),
            (1, 32),
            (4, 5),
            (4, 5),
            (0, 1),
            (1, 5),
            (0x101, 12),
            (8, 5),
            (1, 1),
            (1, 1),
            (5, 10),
            (6, 10),
            (0, 1),
            (0x103, 12),
            (4, 5),
            (0, 1),
            (3, 4),
            (3, 4),
            (15, 4),
            (15, 4),
            (0, 1),
            (1, 1),
            (0, 1),
            (1, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (1, 1),
            (0, 1),
            (1, 1),
            (1, 1),
            (1, 1),
            (6, 3),
            (0, 1),
            (1, 1),
            (0, 1),
            (1, 1),
            (1, 1),
            (0, 1),
            (0, 1),
            (1, 1),
            (1, 1),
            (0, 1),
            (0, 1),
            (1, 1),
        ]);
        let h = SequenceHeader::parse(&data).unwrap();
        assert!(!h.still_picture);
        assert_eq!((h.profile, h.level_idx, h.tier), (2, 8, 1));
        assert_eq!(h.operating_point_idc, [0x101, 0x103]);
        assert_eq!((h.max_frame_width, h.max_frame_height), (16, 16));
        assert!(h.use_128x128_superblock && h.enable_order_hint && h.enable_ref_frame_mvs);
        assert!(!h.enable_jnt_comp);
        assert_eq!(h.order_hint_bits, 7);
        assert_eq!(
            (h.seq_force_screen_content_tools, h.seq_force_integer_mv),
            (2, 2)
        );
        assert_eq!(h.color.bit_depth, 12);
        assert!(h.color.subsampling_x && !h.color.subsampling_y && h.color.full_range);
        assert_eq!(h.color.color_primaries, 2);
        assert!(h.film_grain_params_present);
    }

    #[test]
    fn display_delay_and_10_bit_422() {
        // Profile 2, still, full header; no timing info, initial display delay present; one
        // operating point at level 7 (no tier bit) with a display delay of 4 bits; 10-bit, so
        // 4:2:2 without subsampling bits; sRGB/identity colour would need 4:4:4, so BT.2020 here.
        let data = pack(&[
            (2, 3),
            (1, 1),
            (0, 1),
            (0, 1),
            (1, 1),
            (0, 5),
            (0, 12),
            (7, 5),
            (1, 1),
            (3, 4),
            (7, 4),
            (7, 4),
            (99, 8),
            (49, 8),
            (1, 1),
            (2, 4),
            (4, 3),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (1, 1),
            (0, 1),
            (0, 1),
            (1, 1),
            (9, 8),
            (16, 8),
            (9, 8),
            (0, 1),
            (1, 1),
            (0, 1),
        ]);
        let h = SequenceHeader::parse(&data).unwrap();
        assert!(h.still_picture && !h.reduced_still_picture_header);
        assert_eq!((h.level_idx, h.tier), (7, 0));
        assert_eq!((h.max_frame_width, h.max_frame_height), (100, 50));
        assert!(h.frame_id_numbers_present);
        assert_eq!(
            (h.delta_frame_id_length, h.additional_frame_id_length),
            (4, 5)
        );
        assert_eq!(
            (h.seq_force_screen_content_tools, h.seq_force_integer_mv),
            (0, 2)
        );
        assert_eq!(h.color.bit_depth, 10);
        assert!(h.color.subsampling_x && !h.color.subsampling_y && !h.color.full_range);
        assert_eq!(
            (h.color.color_primaries, h.color.transfer_characteristics),
            (9, 16)
        );
        assert!(h.color.separate_uv_delta_q);
    }

    #[test]
    fn srgb_identity_needs_444() {
        // Profile 0 (4:2:0 only) with sRGB primaries and transfer and the identity matrix.
        let data = pack(&[
            (0, 3),
            (1, 1),
            (1, 1),
            (0, 5),
            (0, 4),
            (0, 4),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (1, 1),
            (1, 8),
            (13, 8),
            (0, 8),
            (0, 1),
            (0, 1),
        ]);
        assert!(matches!(
            SequenceHeader::parse(&data),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn malformed_headers_are_errors() {
        assert_eq!(SequenceHeader::parse(&[]), Err(Error::UnexpectedEof));
        assert!(matches!(
            SequenceHeader::parse(&[0xE0]),
            Err(Error::Invalid(_))
        ));
        // reduced_still_picture_header without still_picture.
        assert!(matches!(
            SequenceHeader::parse(&[0b0000_1000]),
            Err(Error::Invalid(_))
        ));
        // Truncated after the level.
        assert_eq!(
            SequenceHeader::parse(&[0b0001_1010]),
            Err(Error::UnexpectedEof)
        );
    }
}
