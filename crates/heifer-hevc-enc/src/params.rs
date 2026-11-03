//! Parameter sets, slice header and NAL units of the encoded stream (H.265 §7.3).
//!
//! The stream is a single IDR picture with one slice: 8 to 10 bits, 4:2:0, 4:4:4 or 4:0:0, CTBs of
//! 64×64, coding blocks of 8×8 to 64×64 (with 4×4 prediction blocks), transform blocks of 4×4 to
//! 32×32 split twice at most, sign data hiding, deblocking and SAO. Lossless streams code every
//! coding unit with `cu_transquant_bypass_flag` and disable the in-loop filters.

/// Writes bits MSB-first, with Exp-Golomb codes.
#[derive(Debug, Default)]
pub(crate) struct BitWriter {
    bytes: Vec<u8>,
    n: usize,
}

impl BitWriter {
    pub(crate) fn bit(&mut self, b: bool) -> &mut Self {
        if self.n % 8 == 0 {
            self.bytes.push(0);
        }
        if b {
            *self.bytes.last_mut().expect("a byte was pushed") |= 0x80 >> (self.n % 8);
        }
        self.n += 1;
        self
    }

    pub(crate) fn bits(&mut self, v: u32, n: u32) -> &mut Self {
        for i in (0..n).rev() {
            self.bit((v >> i) & 1 == 1);
        }
        self
    }

    /// `ue(v)`.
    pub(crate) fn ue(&mut self, v: u32) -> &mut Self {
        let x = u64::from(v) + 1;
        let len = 64 - x.leading_zeros();
        self.bits(0, len - 1);
        for i in (0..len).rev() {
            self.bit((x >> i) & 1 == 1);
        }
        self
    }

    /// `se(v)`.
    pub(crate) fn se(&mut self, v: i32) -> &mut Self {
        self.ue(if v > 0 {
            2 * v.unsigned_abs() - 1
        } else {
            2 * v.unsigned_abs()
        })
    }

    /// `rbsp_trailing_bits()` (or `byte_alignment()`): a one bit, then zeros to a byte boundary.
    pub(crate) fn finish(mut self) -> Vec<u8> {
        self.bit(true);
        while self.n % 8 != 0 {
            self.bit(false);
        }
        self.bytes
    }
}

/// NAL unit types used by the encoder.
#[derive(Debug, Clone, Copy)]
pub(crate) enum NalType {
    IdrWRadl = 19,
    Vps = 32,
    Sps = 33,
    Pps = 34,
}

/// A NAL unit: the two-byte header, then the RBSP with emulation prevention bytes.
pub(crate) fn nal_unit(t: NalType, rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + rbsp.len() / 64 + 2);
    out.extend_from_slice(&[(t as u8) << 1, 1]); // layer 0, temporal ID 0
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// Stream parameters chosen by the encoder.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StreamParams {
    /// Coded size, a multiple of 8.
    pub width: u32,
    pub height: u32,
    /// Size of the image, before padding to the coded size.
    pub output_width: u32,
    pub output_height: u32,
    /// `chroma_format_idc`: 0 = monochrome, 1 = 4:2:0, 3 = 4:4:4.
    pub chroma_format: u8,
    /// Bits per sample, 8 to 10.
    pub bit_depth: u8,
    pub qp: i32,
    pub full_range: bool,
    /// `colour_primaries`, `transfer_characteristics`, `matrix_coeffs` of the VUI.
    pub colour: (u8, u8, u8),
    /// Whether coding units bypass the transform and quantization (`cu_transquant_bypass_flag`).
    pub lossless: bool,
}

impl StreamParams {
    /// `general_profile_idc`: Main (1) for 8 bits, Main 10 (2) above, Range Extensions (4) for
    /// 4:4:4.
    pub(crate) fn profile_idc(&self) -> u8 {
        if self.chroma_format == 3 {
            4
        } else if self.bit_depth > 8 {
            2
        } else {
            1
        }
    }

    /// `general_level_idc` (×30): the smallest level whose maximum picture size fits.
    pub(crate) fn level_idc(&self) -> u8 {
        let pixels = u64::from(self.width) * u64::from(self.height);
        const LEVELS: [(u64, u8); 7] = [
            (36_864, 30),
            (122_880, 60),
            (245_760, 63),
            (552_960, 90),
            (983_040, 93),
            (2_228_224, 120),
            (8_912_896, 150),
        ];
        LEVELS
            .iter()
            .find(|&&(max, _)| pixels <= max)
            .map_or(if pixels <= 35_651_584 { 180 } else { 186 }, |&(_, l)| l)
    }

    fn profile_tier_level(&self, w: &mut BitWriter) {
        // Main streams are also Main 10 streams; Main 10 streams are only that. 4:4:4 streams
        // are Format Range Extensions (4), whose compatibility flag is bit 27.
        let profile = self.profile_idc();
        let compat = match profile {
            1 => (1u32 << 30) | (1 << 29),
            2 => 1 << 29,
            4 => 1 << 27,
            _ => 0,
        };
        w.bits(0, 2)
            .bit(false)
            .bits(u32::from(profile), 5)
            .bits(compat, 32);
        // progressive_source, interlaced, non_packed_constraint, frame_only_constraint.
        w.bit(true).bit(false).bit(false).bit(true);
        w.bits(0, 32).bits(0, 11).bit(false); // 43 reserved bits, general_inbld_flag
        w.bits(u32::from(self.level_idc()), 8);
    }

    pub(crate) fn vps(&self) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.bits(0, 4)
            .bit(true)
            .bit(true)
            .bits(0, 6)
            .bits(0, 3)
            .bit(true);
        w.bits(0xFFFF, 16);
        self.profile_tier_level(&mut w);
        w.bit(true).ue(0).ue(0).ue(0); // sub-layer ordering info
        w.bits(0, 6).ue(0); // vps_max_layer_id, vps_num_layer_sets_minus1
        w.bit(false).bit(false); // vps_timing_info_present_flag, vps_extension_flag
        nal_unit(NalType::Vps, &w.finish())
    }

    pub(crate) fn sps(&self) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.bits(0, 4).bits(0, 3).bit(true); // VPS 0, one sub-layer, temporal ID nesting
        self.profile_tier_level(&mut w);
        w.ue(0).ue(u32::from(self.chroma_format));
        if self.chroma_format == 3 {
            w.bit(false); // separate_colour_plane_flag
        }
        w.ue(self.width).ue(self.height);
        let (right, bottom) = (
            self.width - self.output_width,
            self.height - self.output_height,
        );
        if right == 0 && bottom == 0 {
            w.bit(false);
        } else {
            // In units of chroma samples (2 luma samples in 4:2:0, 1 otherwise).
            let unit = if self.chroma_format == 1 { 2 } else { 1 };
            w.bit(true).ue(0).ue(right / unit).ue(0).ue(bottom / unit);
        }
        let bd = u32::from(self.bit_depth - 8);
        w.ue(bd).ue(bd); // bit_depth_luma_minus8, bit_depth_chroma_minus8
        w.ue(4); // log2_max_pic_order_cnt_lsb_minus4
        w.bit(true).ue(0).ue(0).ue(0); // sub-layer ordering info
        w.ue(0).ue(3); // coding blocks 8×8 to 64×64
        w.ue(0).ue(3); // transform blocks 4×4 to 32×32
        w.ue(0).ue(u32::from(crate::encoder::MAX_TU_DEPTH)); // max_transform_hierarchy_depth_inter / intra
        w.bit(false).bit(false).bit(!self.lossless).bit(false); // scaling lists, AMP, SAO, PCM
        w.ue(0).bit(false).bit(false); // no RPS, no long-term pictures, no TMVP
        w.bit(true); // strong_intra_smoothing_enabled_flag
        w.bit(true); // vui_parameters_present_flag
        {
            w.bit(false).bit(false); // aspect ratio, overscan
            w.bit(true).bits(5, 3).bit(self.full_range); // video_format: unspecified
            w.bit(true);
            w.bits(u32::from(self.colour.0), 8);
            w.bits(u32::from(self.colour.1), 8);
            w.bits(u32::from(self.colour.2), 8);
            w.bit(false); // chroma_loc_info_present_flag
            w.bit(false).bit(false).bit(false); // neutral chroma, field_seq, frame_field_info
            w.bit(false).bit(false).bit(false); // default display window, timing, restrictions
        }
        w.bit(false); // sps_extension_present_flag
        nal_unit(NalType::Sps, &w.finish())
    }

    pub(crate) fn pps(&self) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.ue(0).ue(0); // PPS 0, SPS 0
        w.bit(false).bit(false).bits(0, 3); // dependent slices, output flag, extra header bits
        w.bit(!self.lossless).bit(false); // sign_data_hiding, cabac_init_present
        w.ue(0).ue(0); // num_ref_idx_l{0,1}_default_active_minus1
        w.se(self.qp - 26); // init_qp_minus26
        let skip = crate::encoder::TRANSFORM_SKIP && !self.lossless;
        w.bit(false).bit(skip).bit(false); // constrained intra, transform skip, cu_qp_delta
        w.se(0).se(0).bit(false); // chroma QP offsets, not in slice headers
        w.bit(false).bit(false).bit(self.lossless); // weighted pred / bipred, transquant bypass
        w.bit(false).bit(false); // tiles, entropy_coding_sync
        w.bit(false); // pps_loop_filter_across_slices_enabled_flag
        if self.lossless {
            // Deblocking filter control present, overrides disabled, filter disabled.
            w.bit(true).bit(false).bit(true);
        } else {
            w.bit(false); // deblocking_filter_control_present_flag: deblocking on, offsets 0
        }
        w.bit(false).bit(false); // pps_scaling_list_data_present, lists_modification_present
        w.ue(0).bit(false); // log2_parallel_merge_level_minus2, header extension
        w.bit(false); // pps_extension_present_flag
        nal_unit(NalType::Pps, &w.finish())
    }

    /// The slice segment header, up to and including `byte_alignment()`.
    pub(crate) fn slice_header(&self) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.bit(true).bit(false); // first_slice_segment_in_pic, no_output_of_prior_pics
        w.ue(0); // slice_pic_parameter_set_id
        w.ue(2); // slice_type: I
        if !self.lossless {
            w.bit(true); // slice_sao_luma_flag
            if self.chroma_format != 0 {
                w.bit(true); // slice_sao_chroma_flag
            }
        }
        w.se(0); // slice_qp_delta
        w.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb() {
        let mut w = BitWriter::default();
        w.ue(0).ue(1).ue(4).se(-1).se(2);
        // 1 010 00101 011 00100, then the stop bit.
        assert_eq!(w.finish(), [0b1010_0010, 0b1011_0010, 0b0100_0000]);
    }

    #[test]
    fn emulation_prevention() {
        assert_eq!(
            nal_unit(NalType::Pps, &[0, 0, 1, 0, 0, 0, 0, 5]),
            [0x44, 1, 0, 0, 3, 1, 0, 0, 3, 0, 0, 5]
        );
    }
}
