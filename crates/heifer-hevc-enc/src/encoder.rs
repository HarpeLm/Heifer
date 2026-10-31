//! Encoding of one intra picture.
//!
//! The encoder reconstructs the picture with the decoder's own [`Reconstructor`], fed with the
//! same blocks the decoder will parse: intra prediction and reconstruction are therefore
//! identical on both sides by construction. Decisions are made by rate-distortion cost
//! (`distortion + λ · bits`), with the bits counted by the CABAC encoder itself:
//!
//! - coding quadtree: each split is tried against coding the whole block (32, 16 and 8);
//! - luma prediction mode: a coarse search over the 35 modes by SATD, refined around the best;
//! - chroma uses the luma mode (`intra_chroma_pred_mode` 4); one transform per coding block.

use heifer_hevc_dec::contexts::{Contexts, ctx};
use heifer_hevc_dec::nal::NalUnit;
use heifer_hevc_dec::params::{ParameterSets, Sps};
use heifer_hevc_dec::recon::Frame;
use heifer_hevc_dec::recon::Reconstructor;
use heifer_hevc_dec::scan::{ScanType, scan_order};
use heifer_hevc_dec::slice::SliceHeader;
use heifer_hevc_dec::syntax::{CodingUnit, Sink, TransformBlock};

use crate::Error;
use crate::cabac::{ArithmeticEncoder, Checkpoint};
use crate::params::{NalType, PROFILE_MAIN, StreamParams, nal_unit};
use crate::transform::{chroma_qp, forward_dct, quantize, satd};

const LOG2_CTB: u8 = 5;
const LOG2_MIN_CB: u8 = 3;

/// 8-bit planes of the picture to encode.
#[derive(Debug, Clone, Copy)]
pub struct Planes<'a> {
    /// Luma, `width × height`, row-major.
    pub y: &'a [u8],
    /// Chroma planes of 4:2:0 pictures, `ceil(width / 2) × ceil(height / 2)` each; `None` for
    /// monochrome pictures (e.g. alpha planes).
    pub chroma: Option<(&'a [u8], &'a [u8])>,
}

/// Encoding parameters.
#[derive(Debug, Clone, Copy)]
pub struct EncoderConfig {
    /// Quantization parameter, 0 (best quality) to 51.
    pub qp: u8,
    /// Whether samples use the full range (0..=255) rather than the video range.
    pub full_range: bool,
    /// `colour_primaries`, `transfer_characteristics` and `matrix_coeffs` (ITU-T H.273) written
    /// in the VUI.
    pub colour: (u8, u8, u8),
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            qp: 24,
            full_range: true,
            colour: (1, 13, 6), // BT.709 primaries, sRGB transfer, BT.601 matrix
        }
    }
}

/// An encoded picture: parameter sets and slice, as NAL units without start codes.
#[derive(Debug, Clone)]
pub struct EncodedPicture {
    /// VPS, SPS and PPS.
    pub parameter_sets: Vec<Vec<u8>>,
    /// The slice segments of the picture (one).
    pub slices: Vec<Vec<u8>>,
    /// `general_profile_idc`.
    pub profile_idc: u8,
    /// `general_level_idc`.
    pub level_idc: u8,
    /// `chroma_format_idc`: 0 or 1.
    pub chroma_format: u8,
    /// Size of the decoded picture: the input size, rounded up to even numbers in 4:2:0.
    pub width: u32,
    /// Size of the decoded picture: the input size, rounded up to even numbers in 4:2:0.
    pub height: u32,
}

impl EncodedPicture {
    /// The picture as an Annex B byte stream (start codes before each NAL unit).
    pub fn annex_b(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in self.parameter_sets.iter().chain(&self.slices) {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal);
        }
        out
    }
}

/// Encodes one picture as an IDR frame.
pub fn encode_picture(
    width: u32,
    height: u32,
    planes: Planes<'_>,
    config: &EncoderConfig,
) -> Result<EncodedPicture, Error> {
    encode_with_reconstruction(width, height, planes, config).map(|(picture, _)| picture)
}

/// Like [`encode_picture`], also returning the encoder's reconstruction of the picture (coded
/// size, before the in-loop filters).
fn encode_with_reconstruction(
    width: u32,
    height: u32,
    planes: Planes<'_>,
    config: &EncoderConfig,
) -> Result<(EncodedPicture, Frame), Error> {
    if width == 0 || height == 0 || width > 16384 || height > 16384 {
        return Err(Error::InvalidInput(
            "picture size must be 1 to 16384 pixels",
        ));
    }
    if config.qp > 51 {
        return Err(Error::InvalidInput("QP must be 0 to 51"));
    }
    let (w, h) = (width as usize, height as usize);
    if planes.y.len() != w * h {
        return Err(Error::InvalidInput(
            "luma plane size does not match the picture",
        ));
    }
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    if let Some((cb, cr)) = planes.chroma
        && (cb.len() != cw * ch || cr.len() != cw * ch)
    {
        return Err(Error::InvalidInput(
            "chroma plane size does not match the picture",
        ));
    }

    // 4:2:0 conformance windows crop by 2 samples: odd sizes are rounded up (HEIF crops the
    // rest with a `clap` property).
    let unit = if planes.chroma.is_some() { 2 } else { 1 };
    let params = StreamParams {
        width: width.next_multiple_of(8),
        height: height.next_multiple_of(8),
        output_width: width.next_multiple_of(unit),
        output_height: height.next_multiple_of(unit),
        chroma_format: u8::from(planes.chroma.is_some()),
        qp: i32::from(config.qp),
        full_range: config.full_range,
        colour: config.colour,
    };
    let parameter_sets = vec![params.vps(), params.sps(), params.pps()];

    // Parse what was written with the decoder: the reconstruction uses its structures.
    let mut sets = ParameterSets::default();
    for nal in &parameter_sets {
        sets.add(&NalUnit::parse(nal)?)?;
    }
    let header_rbsp = params.slice_header();
    let header_nal = nal_unit(NalType::IdrWRadl, &header_rbsp);
    let header = SliceHeader::parse(
        &header_rbsp,
        &NalUnit::parse(&header_nal)?.header,
        &sets,
        None,
    )?;
    let (pps, sps) = sets.get(0)?;

    let mut enc = Encoder::new(sps, pps, &params, (w, h), planes, &header);
    let (pw, ph) = (params.width, params.height);
    let ctb = 1u32 << LOG2_CTB;
    let (ctbs_x, ctbs_y) = (pw.div_ceil(ctb), ph.div_ceil(ctb));
    for cy in 0..ctbs_y {
        for cx in 0..ctbs_x {
            enc.quadtree(cx * ctb, cy * ctb, LOG2_CTB, 0);
            let last = cy + 1 == ctbs_y && cx + 1 == ctbs_x;
            enc.cabac.encode_terminate(u8::from(last)); // end_of_slice_segment_flag
        }
    }
    if let Some(e) = enc.recon.error.take() {
        return Err(e.into());
    }
    let mut rbsp = header_rbsp;
    rbsp.extend_from_slice(&enc.cabac.finish());

    let picture = EncodedPicture {
        parameter_sets,
        slices: vec![nal_unit(NalType::IdrWRadl, &rbsp)],
        profile_idc: PROFILE_MAIN,
        level_idc: params.level_idc(),
        chroma_format: params.chroma_format,
        width: params.output_width,
        height: params.output_height,
    };
    Ok((picture, enc.recon.finish(true)))
}

/// A source plane, padded to the coded size by repeating the last column and row.
struct Plane {
    width: usize,
    samples: Vec<i32>,
}

impl Plane {
    fn padded(src: &[u8], w: usize, h: usize, pw: usize, ph: usize) -> Self {
        let mut samples = Vec::with_capacity(pw * ph);
        for y in 0..ph {
            let row = &src[y.min(h - 1) * w..][..w];
            samples.extend(row.iter().map(|&v| i32::from(v)));
            samples.extend(std::iter::repeat_n(i32::from(row[w - 1]), pw - w));
        }
        Self { width: pw, samples }
    }
}

/// Everything needed to go back to an earlier encoder state for one block.
struct Snapshot {
    cabac: Checkpoint,
    ctx: Contexts,
    /// Reconstructed samples of the block, per component.
    samples: [Vec<u16>; 3],
    depth: Vec<u8>,
    modes: Vec<u8>,
}

struct Encoder<'a> {
    recon: Reconstructor<'a>,
    cabac: ArithmeticEncoder,
    ctx: Contexts,
    src: [Plane; 3],
    chroma: bool,
    width: u32,
    height: u32,
    qp: i32,
    qp_c: i32,
    lambda: f64,
    sqrt_lambda: f64,
    /// Coding tree depth and luma intra mode of each 8×8 block.
    depth: Vec<u8>,
    modes: Vec<u8>,
    grid_w: usize,
    // Scratch buffers.
    diff: Vec<i32>,
    coeffs: [Vec<i32>; 3],
}

impl<'a> Encoder<'a> {
    fn new(
        sps: &'a Sps,
        pps: &'a heifer_hevc_dec::params::Pps,
        params: &StreamParams,
        (w, h): (usize, usize),
        planes: Planes<'_>,
        header: &SliceHeader,
    ) -> Self {
        let (pw, ph) = (params.width as usize, params.height as usize);
        let y = Plane::padded(planes.y, w, h, pw, ph);
        let (cb, cr) = match planes.chroma {
            Some((cb, cr)) => {
                let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
                (
                    Plane::padded(cb, cw, ch, pw / 2, ph / 2),
                    Plane::padded(cr, cw, ch, pw / 2, ph / 2),
                )
            }
            None => (
                Plane {
                    width: 0,
                    samples: Vec::new(),
                },
                Plane {
                    width: 0,
                    samples: Vec::new(),
                },
            ),
        };
        let mut recon = Reconstructor::new(sps, pps);
        recon.start_slice(header);
        let qp = params.qp;
        // λ of the HM reference encoder for intra pictures.
        let lambda = 0.57 * 2f64.powf(f64::from(qp - 12) / 3.0);
        let grid_w = pw / 8;
        Self {
            recon,
            cabac: ArithmeticEncoder::new(),
            ctx: Contexts::new(qp),
            src: [y, cb, cr],
            chroma: planes.chroma.is_some(),
            width: params.width,
            height: params.height,
            qp,
            qp_c: chroma_qp(qp),
            lambda,
            sqrt_lambda: lambda.sqrt(),
            depth: vec![0; grid_w * (ph / 8)],
            modes: vec![1; grid_w * (ph / 8)],
            grid_w,
            diff: vec![0; 32 * 32],
            coeffs: [vec![0; 32 * 32], vec![0; 16 * 16], vec![0; 16 * 16]],
        }
    }

    fn bin(&mut self, i: usize, bin: bool) {
        self.cabac.encode(&mut self.ctx[i], u8::from(bin));
    }

    fn grid(&self, x: u32, y: u32) -> usize {
        (y as usize / 8) * self.grid_w + x as usize / 8
    }

    fn fill_grid(&mut self, x0: u32, y0: u32, size: u32, depth: Option<u8>, mode: Option<u8>) {
        for y in (y0..y0 + size).step_by(8) {
            for x in (x0..x0 + size).step_by(8) {
                let i = self.grid(x, y);
                if let Some(d) = depth {
                    self.depth[i] = d;
                }
                if let Some(m) = mode {
                    self.modes[i] = m;
                }
            }
        }
    }

    /// Components and their regions (in component samples) covered by a luma block.
    fn regions(&self, x0: u32, y0: u32, size: u32) -> Vec<(usize, u32, u32, u32)> {
        let n = if self.chroma { 3 } else { 1 };
        (0..n)
            .map(|c| {
                if c == 0 {
                    (0, x0, y0, size)
                } else {
                    (c, x0 / 2, y0 / 2, size / 2)
                }
            })
            .collect()
    }

    fn snapshot(&self, x0: u32, y0: u32, size: u32, cabac: Checkpoint) -> Snapshot {
        let mut samples: [Vec<u16>; 3] = Default::default();
        for (c, x, y, n) in self.regions(x0, y0, size) {
            let stride = self.recon.frame.widths[c] as usize;
            let plane = &self.recon.frame.planes[c];
            for row in y..y + n {
                let start = row as usize * stride + x as usize;
                samples[c].extend_from_slice(&plane[start..start + n as usize]);
            }
        }
        let mut depth = Vec::new();
        let mut modes = Vec::new();
        for y in (y0..y0 + size).step_by(8) {
            for x in (x0..x0 + size).step_by(8) {
                let i = self.grid(x, y);
                depth.push(self.depth[i]);
                modes.push(self.modes[i]);
            }
        }
        Snapshot {
            cabac,
            ctx: self.ctx,
            samples,
            depth,
            modes,
        }
    }

    fn restore(&mut self, x0: u32, y0: u32, size: u32, s: &Snapshot) {
        self.cabac.restore(&s.cabac);
        self.ctx = s.ctx;
        for (c, x, y, n) in self.regions(x0, y0, size) {
            let stride = self.recon.frame.widths[c] as usize;
            let plane = &mut self.recon.frame.planes[c];
            for (i, row) in (y..y + n).enumerate() {
                let start = row as usize * stride + x as usize;
                let n = n as usize;
                plane[start..start + n].copy_from_slice(&s.samples[c][i * n..(i + 1) * n]);
            }
        }
        let mut k = 0;
        for y in (y0..y0 + size).step_by(8) {
            for x in (x0..x0 + size).step_by(8) {
                let i = self.grid(x, y);
                self.depth[i] = s.depth[k];
                self.modes[i] = s.modes[k];
                k += 1;
            }
        }
    }

    /// `coding_quadtree()`: chooses and encodes the coding tree of a block. Returns its
    /// distortion (sum of squared errors).
    fn quadtree(&mut self, x0: u32, y0: u32, log2_size: u8, depth: u8) -> f64 {
        let size = 1u32 << log2_size;
        let half = size / 2;
        let children = [(0, 0), (half, 0), (0, half), (half, half)];
        if x0 + size > self.width || y0 + size > self.height {
            // Split inferred at the picture boundary (the coded size is a multiple of 8).
            let mut d = 0.0;
            for (dx, dy) in children {
                if x0 + dx < self.width && y0 + dy < self.height {
                    d += self.quadtree(x0 + dx, y0 + dy, log2_size - 1, depth + 1);
                }
            }
            return d;
        }
        if log2_size == LOG2_MIN_CB {
            return self.coding_unit(x0, y0, log2_size, depth);
        }

        // Try the split first: coding the whole block next only looks outside the block, so
        // the samples left inside by the split do not matter.
        let start_bits = self.cabac.bit_count();
        let start = self.snapshot(x0, y0, size, self.cabac.checkpoint());
        self.split_cu_flag(x0, y0, depth, true);
        let d_split: f64 = children
            .iter()
            .map(|&(dx, dy)| self.quadtree(x0 + dx, y0 + dy, log2_size - 1, depth + 1))
            .sum();
        let j_split = d_split + self.lambda * (self.cabac.bit_count() - start_bits) as f64;
        let split = self.snapshot(x0, y0, size, self.cabac.checkpoint_since(&start.cabac));

        self.restore(x0, y0, size, &start);
        self.split_cu_flag(x0, y0, depth, false);
        let d_whole = self.coding_unit(x0, y0, log2_size, depth);
        let j_whole = d_whole + self.lambda * (self.cabac.bit_count() - start_bits) as f64;
        if j_split < j_whole {
            self.restore(x0, y0, size, &split);
            d_split
        } else {
            d_whole
        }
    }

    fn split_cu_flag(&mut self, x0: u32, y0: u32, depth: u8, split: bool) {
        let mut inc = 0;
        if x0 > 0 && self.depth[self.grid(x0 - 1, y0)] > depth {
            inc += 1;
        }
        if y0 > 0 && self.depth[self.grid(x0, y0 - 1)] > depth {
            inc += 1;
        }
        self.bin(ctx::SPLIT_CU_FLAG + inc, split);
    }

    /// Most probable modes (§8.4.2).
    fn mpm_candidates(&self, x: u32, y: u32) -> [u8; 3] {
        let a = if x > 0 {
            self.modes[self.grid(x - 1, y)]
        } else {
            1
        };
        // The above candidate must be in the same CTB row.
        let b = if y > 0 && (y - 1) >> LOG2_CTB == y >> LOG2_CTB {
            self.modes[self.grid(x, y - 1)]
        } else {
            1
        };
        if a == b {
            if a < 2 {
                [0, 1, 26]
            } else {
                [a, 2 + ((a + 29) % 32), 2 + ((a - 2 + 1) % 32)]
            }
        } else {
            let c = if a != 0 && b != 0 {
                0
            } else if a != 1 && b != 1 {
                1
            } else {
                26
            };
            [a, b, c]
        }
    }

    /// Predicts a block with the decoder (a transform block without residual) and returns the
    /// difference between the source and the prediction in `self.diff`.
    fn predict(&mut self, c: usize, x: u32, y: u32, log2_size: u8, mode: u8) {
        self.recon.transform_block(&TransformBlock {
            c_idx: c as u8,
            x,
            y,
            log2_size,
            intra_mode: mode,
            coeffs: None,
            coeff_bounds: (0, 0),
            transform_skip: false,
            transquant_bypass: false,
            qp_y: self.qp,
            cu_qp_offset: 0,
            res_scale_val: 0,
        });
        let n = 1usize << log2_size;
        let stride = self.recon.frame.widths[c] as usize;
        let pred = &self.recon.frame.planes[c];
        let src = &self.src[c];
        for row in 0..n {
            let p = (y as usize + row) * stride + x as usize;
            let s = (y as usize + row) * src.width + x as usize;
            for col in 0..n {
                self.diff[row * n + col] = src.samples[s + col] - i32::from(pred[p + col]);
            }
        }
    }

    /// Chooses the luma intra mode of a block by SATD and estimated mode bits.
    fn choose_mode(&mut self, x0: u32, y0: u32, log2_size: u8, mpm: [u8; 3]) -> u8 {
        let n = 1usize << log2_size;
        let cost = |enc: &mut Self, mode: u8| -> f64 {
            enc.predict(0, x0, y0, log2_size, mode);
            let bits = match mpm.iter().position(|&m| m == mode) {
                Some(0) => 2.0,
                Some(_) => 3.0,
                None => 6.0,
            };
            satd(&enc.diff[..n * n], n) as f64 + enc.sqrt_lambda * bits
        };
        let mut best = (f64::MAX, 0u8);
        for mode in [0, 1, 2, 6, 10, 14, 18, 22, 26, 30, 34] {
            let c = cost(self, mode);
            if c < best.0 {
                best = (c, mode);
            }
        }
        if best.1 >= 2 {
            let center = best.1;
            for mode in [
                center.saturating_sub(2),
                center.saturating_sub(1),
                center + 1,
                center + 2,
            ] {
                if (2..=34).contains(&mode) && mode != center {
                    let c = cost(self, mode);
                    if c < best.0 {
                        best = (c, mode);
                    }
                }
            }
        }
        best.1
    }

    /// Computes the residual levels of one block (in `self.coeffs[c]`) and reconstructs it.
    /// Returns whether it has non-zero levels, and the squared error of the reconstruction.
    fn transform_block(
        &mut self,
        c: usize,
        x: u32,
        y: u32,
        log2_size: u8,
        mode: u8,
    ) -> (bool, f64) {
        let n = 1usize << log2_size;
        self.predict(c, x, y, log2_size, mode);
        let mut coeffs = std::mem::take(&mut self.coeffs[c]);
        let qp = if c == 0 { self.qp } else { self.qp_c };
        forward_dct(&self.diff[..n * n], log2_size, &mut coeffs[..n * n]);
        let coded = quantize(&mut coeffs[..n * n], log2_size, qp);
        if coded {
            let (mut cols, mut rows) = (0, 0);
            for (i, _) in coeffs[..n * n].iter().enumerate().filter(|(_, v)| **v != 0) {
                cols = cols.max(i % n + 1);
                rows = rows.max(i / n + 1);
            }
            self.recon.transform_block(&TransformBlock {
                c_idx: c as u8,
                x,
                y,
                log2_size,
                intra_mode: mode,
                coeffs: Some(&coeffs[..n * n]),
                coeff_bounds: (cols as u8, rows as u8),
                transform_skip: false,
                transquant_bypass: false,
                qp_y: self.qp,
                cu_qp_offset: 0,
                res_scale_val: 0,
            });
        }
        self.coeffs[c] = coeffs;

        let stride = self.recon.frame.widths[c] as usize;
        let rec = &self.recon.frame.planes[c];
        let src = &self.src[c];
        let mut sse = 0i64;
        for row in 0..n {
            let r = (y as usize + row) * stride + x as usize;
            let s = (y as usize + row) * src.width + x as usize;
            for col in 0..n {
                let d = i64::from(src.samples[s + col]) - i64::from(rec[r + col]);
                sse += d * d;
            }
        }
        (coded, sse as f64)
    }

    /// `coding_unit()` with one 2N×2N prediction block and one transform unit. Returns the
    /// distortion.
    fn coding_unit(&mut self, x0: u32, y0: u32, log2_size: u8, depth: u8) -> f64 {
        let size = 1u32 << log2_size;
        self.fill_grid(x0, y0, size, Some(depth), None);
        if log2_size == LOG2_MIN_CB {
            self.bin(ctx::PART_MODE, true); // PART_2Nx2N
        }

        let mpm = self.mpm_candidates(x0, y0);
        let mode = self.choose_mode(x0, y0, log2_size, mpm);
        match mpm.iter().position(|&m| m == mode) {
            Some(i) => {
                self.bin(ctx::PREV_INTRA_LUMA_PRED_FLAG, true);
                // mpm_idx: truncated unary, maximum 2.
                self.cabac.encode_bypass(u8::from(i > 0));
                if i > 0 {
                    self.cabac.encode_bypass(u8::from(i > 1));
                }
            }
            None => {
                self.bin(ctx::PREV_INTRA_LUMA_PRED_FLAG, false);
                let below = mpm.iter().filter(|&&m| m < mode).count() as u8;
                self.cabac.encode_bypass_bits(u32::from(mode - below), 5);
            }
        }
        if self.chroma {
            self.bin(ctx::INTRA_CHROMA_PRED_MODE, false); // same mode as luma
        }
        self.fill_grid(x0, y0, size, None, Some(mode));

        // transform_tree() at depth 0, not split: luma, then Cb and Cr of half size.
        let (cbf_y, d_y) = self.transform_block(0, x0, y0, log2_size, mode);
        let (mut cbf_cb, mut cbf_cr, mut d_c) = (false, false, 0.0);
        if self.chroma {
            let d;
            (cbf_cb, d) = self.transform_block(1, x0 / 2, y0 / 2, log2_size - 1, mode);
            d_c += d;
            let d;
            (cbf_cr, d) = self.transform_block(2, x0 / 2, y0 / 2, log2_size - 1, mode);
            d_c += d;
            self.bin(ctx::CBF_CHROMA, cbf_cb);
            self.bin(ctx::CBF_CHROMA, cbf_cr);
        }
        self.bin(ctx::CBF_LUMA + 1, cbf_y);
        if cbf_y {
            self.residual_coding(0, log2_size, mode);
        }
        if cbf_cb {
            self.residual_coding(1, log2_size - 1, mode);
        }
        if cbf_cr {
            self.residual_coding(2, log2_size - 1, mode);
        }

        self.recon.coding_unit(&CodingUnit {
            x: x0,
            y: y0,
            log2_size,
            part_nxn: false,
            transquant_bypass: false,
            pcm: false,
            qp_y: self.qp,
        });
        d_y + d_c
    }

    /// `residual_coding()` (§7.3.8.11) of the levels in `self.coeffs[c]`, which has at least one
    /// non-zero level: the mirror of the decoder's parsing.
    fn residual_coding(&mut self, c: usize, log2_size: u8, intra_mode: u8) {
        let coeffs = std::mem::take(&mut self.coeffs[c]);
        let size = 1usize << log2_size;
        let luma = c == 0;
        let scan = if log2_size == 2 || (log2_size == 3 && luma) {
            match intra_mode {
                6..=14 => ScanType::Vertical,
                22..=30 => ScanType::Horizontal,
                _ => ScanType::Diagonal,
            }
        } else {
            ScanType::Diagonal
        };
        let log2_sb = log2_size - 2;
        let sb_scan = scan_order(log2_sb, scan);
        let pos_scan = scan_order(2, scan);
        let level = |sb: usize, pos: usize| -> i32 {
            let (xs, ys) = sb_scan[sb];
            let (xp, yp) = pos_scan[pos];
            let (x, y) = (
                usize::from(xs) * 4 + usize::from(xp),
                usize::from(ys) * 4 + usize::from(yp),
            );
            coeffs[y * size + x]
        };

        // Last significant coefficient, in scan order.
        let num_sb = 1usize << (2 * log2_sb);
        let (last_sb, last_pos) = (0..num_sb * 16)
            .rev()
            .map(|i| (i / 16, i % 16))
            .find(|&(sb, pos)| level(sb, pos) != 0)
            .expect("the block has a non-zero level");
        let (xs, ys) = sb_scan[last_sb];
        let (xp, yp) = pos_scan[last_pos];
        let mut last = [u32::from(xs * 4 + xp), u32::from(ys * 4 + yp)];
        if scan == ScanType::Vertical {
            last.swap(0, 1);
        }
        self.last_sig_coeff(last, log2_size, luma);

        let sb_width = 1usize << log2_sb;
        let mut coded_sb = [[false; 8]; 8];
        let mut greater1_ctx_state: u8 = 1;
        for i in (0..=last_sb).rev() {
            let (xs, ys) = sb_scan[i];
            let (xs, ys) = (usize::from(xs), usize::from(ys));
            let mut infer_sb_dc = false;
            if i < last_sb && i > 0 {
                let mut csbf_ctx = 0;
                if xs + 1 < sb_width {
                    csbf_ctx += usize::from(coded_sb[xs + 1][ys]);
                }
                if ys + 1 < sb_width {
                    csbf_ctx += usize::from(coded_sb[xs][ys + 1]);
                }
                let inc = csbf_ctx.min(1) + if luma { 0 } else { 2 };
                let flag = (0..16).any(|n| level(i, n) != 0);
                self.bin(ctx::CODED_SUB_BLOCK_FLAG + inc, flag);
                coded_sb[xs][ys] = flag;
                infer_sb_dc = true;
            } else {
                coded_sb[xs][ys] = true;
            }

            // Significant positions, in decreasing scan position, and their levels.
            let mut sig = [0i32; 16];
            let mut n_sig = 0;
            if i == last_sb {
                sig[0] = level(i, last_pos);
                n_sig = 1;
            }
            if coded_sb[xs][ys] {
                let prev_csbf = u8::from(xs + 1 < sb_width && coded_sb[xs + 1][ys])
                    | (u8::from(ys + 1 < sb_width && coded_sb[xs][ys + 1]) << 1);
                let start = if i == last_sb {
                    last_pos as i32 - 1
                } else {
                    15
                };
                for n in (0..=start).rev() {
                    let n = n as usize;
                    let v = level(i, n);
                    if n > 0 || !infer_sb_dc {
                        let (xp, yp) = pos_scan[n];
                        let xc = (xs << 2) + usize::from(xp);
                        let yc = (ys << 2) + usize::from(yp);
                        let inc = sig_ctx_inc(luma, log2_size, xc, yc, xp, yp, prev_csbf, scan);
                        self.bin(ctx::SIG_COEFF_FLAG + inc, v != 0);
                        if v != 0 {
                            infer_sb_dc = false;
                        }
                    }
                    // Otherwise: the DC of a coded sub-block whose other levels are all zero,
                    // inferred significant (and therefore non-zero).
                    if v != 0 {
                        sig[n_sig] = v;
                        n_sig += 1;
                    }
                }
            }
            if n_sig == 0 {
                continue;
            }

            // coeff_abs_level_greater1_flag for the first 8, greater2 for the first > 1.
            let mut ctx_set = if i == 0 || !luma { 0 } else { 2 };
            if greater1_ctx_state == 0 {
                ctx_set += 1;
            }
            greater1_ctx_state = 1;
            let mut greater1 = [false; 16];
            let mut first_g1: Option<usize> = None;
            for k in 0..n_sig.min(8) {
                let inc = ctx_set * 4
                    + usize::from(greater1_ctx_state.min(3))
                    + if luma { 0 } else { 16 };
                greater1[k] = sig[k].abs() > 1;
                self.bin(ctx::COEFF_ABS_LEVEL_GREATER1_FLAG + inc, greater1[k]);
                if greater1[k] {
                    greater1_ctx_state = 0;
                    first_g1.get_or_insert(k);
                } else if greater1_ctx_state > 0 && greater1_ctx_state < 3 {
                    greater1_ctx_state += 1;
                }
            }
            let mut greater2 = false;
            if let Some(k) = first_g1 {
                greater2 = sig[k].abs() > 2;
                self.bin(
                    ctx::COEFF_ABS_LEVEL_GREATER2_FLAG + ctx_set + if luma { 0 } else { 4 },
                    greater2,
                );
            }

            for &v in &sig[..n_sig] {
                self.cabac.encode_bypass(u8::from(v < 0));
            }

            let mut rice = 0u32;
            for k in 0..n_sig {
                let base = 1 + i32::from(greater1[k]) + i32::from(first_g1 == Some(k) && greater2);
                let threshold = if k < 8 {
                    if first_g1 == Some(k) { 3 } else { 2 }
                } else {
                    1
                };
                let abs = sig[k].abs();
                if base == threshold {
                    self.coeff_abs_level_remaining((abs - base) as u32, rice);
                    if abs > 3 * (1 << rice) {
                        rice = (rice + 1).min(4);
                    }
                }
            }
        }
        self.coeffs[c] = coeffs;
    }

    /// `last_sig_coeff_{x,y}_{prefix,suffix}` for the (scan-swapped) position `last`.
    fn last_sig_coeff(&mut self, last: [u32; 2], log2_size: u8, luma: bool) {
        let (ctx_offset, ctx_shift) = if luma {
            (
                3 * (log2_size - 2) + ((log2_size - 1) >> 2),
                (log2_size + 1) >> 2,
            )
        } else {
            (15, log2_size - 2)
        };
        let c_max = (u32::from(log2_size) << 1) - 1;
        let split = |pos: u32| -> (u32, Option<(u32, u32)>) {
            if pos < 4 {
                return (pos, None);
            }
            let n = 31 - pos.leading_zeros() - 1;
            let b = (pos >> n) - 2;
            let prefix = 2 * (n + 1) + b;
            (prefix, Some((pos - ((2 + b) << n), n)))
        };
        let parts = last.map(split);
        for (k, base) in [ctx::LAST_SIG_COEFF_X_PREFIX, ctx::LAST_SIG_COEFF_Y_PREFIX]
            .into_iter()
            .enumerate()
        {
            let prefix = parts[k].0;
            for i in 0..prefix.min(c_max) {
                self.bin(
                    base + usize::from(ctx_offset) + (i as usize >> ctx_shift),
                    true,
                );
            }
            if prefix < c_max {
                self.bin(
                    base + usize::from(ctx_offset) + (prefix as usize >> ctx_shift),
                    false,
                );
            }
        }
        for (_, suffix) in parts {
            if let Some((value, n)) = suffix {
                self.cabac.encode_bypass_bits(value, n);
            }
        }
    }

    /// `coeff_abs_level_remaining` (§9.3.3.11): Rice code, with an Exp-Golomb escape.
    fn coeff_abs_level_remaining(&mut self, value: u32, rice: u32) {
        if value < (4 << rice) {
            let prefix = value >> rice;
            for _ in 0..prefix {
                self.cabac.encode_bypass(1);
            }
            self.cabac.encode_bypass(0);
            self.cabac
                .encode_bypass_bits(value & ((1 << rice) - 1), rice);
        } else {
            let mut prefix = 4;
            while value >= (((2u32 << (prefix - 3)) + 2) << rice) {
                prefix += 1;
            }
            let base = ((1u32 << (prefix - 3)) + 2) << rice;
            for _ in 0..prefix {
                self.cabac.encode_bypass(1);
            }
            self.cabac.encode_bypass(0);
            self.cabac
                .encode_bypass_bits(value - base, prefix - 3 + rice);
        }
    }
}

/// Context increment of `sig_coeff_flag` (§9.3.4.2.5), as in the decoder.
#[allow(clippy::too_many_arguments)]
fn sig_ctx_inc(
    luma: bool,
    log2_size: u8,
    xc: usize,
    yc: usize,
    xp: u8,
    yp: u8,
    prev_csbf: u8,
    scan: ScanType,
) -> usize {
    const CTX_IDX_MAP: [u8; 16] = [0, 1, 4, 5, 2, 3, 4, 5, 6, 6, 8, 8, 7, 7, 8, 8];
    let sig_ctx: usize = if log2_size == 2 {
        usize::from(CTX_IDX_MAP[(yc << 2) + xc])
    } else if xc + yc == 0 {
        0
    } else {
        let (xp, yp) = (usize::from(xp), usize::from(yp));
        let mut s = match prev_csbf {
            0 => {
                if xp + yp == 0 {
                    2
                } else if xp + yp < 3 {
                    1
                } else {
                    0
                }
            }
            1 => match yp {
                0 => 2,
                1 => 1,
                _ => 0,
            },
            2 => match xp {
                0 => 2,
                1 => 1,
                _ => 0,
            },
            _ => 2,
        };
        if luma {
            if (xc >> 2) + (yc >> 2) > 0 {
                s += 3;
            }
            s += if log2_size == 3 {
                if scan == ScanType::Diagonal { 9 } else { 15 }
            } else {
                21
            };
        } else {
            s += if log2_size == 3 { 9 } else { 12 };
        }
        s
    };
    if luma { sig_ctx } else { 27 + sig_ctx }
}

#[cfg(test)]
mod tests {
    use super::*;
    use heifer_hevc_dec::decoder::{DecodeOptions, decode_picture};

    /// A deterministic test picture: gradients, sharp edges, a checkerboard and noise.
    fn test_picture(w: usize, h: usize, seed: u32) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let mut state = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
        let mut noise = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state % 41) as i32 - 20
        };
        let sample = |x: usize, y: usize, n: i32| -> u8 {
            let v = match (x * 4 / w.max(1), y * 3 / h.max(1)) {
                (0, _) => (x * 255 / w.max(1)) as i32,
                (1, 0) => {
                    if (x / 3 + y / 3) % 2 == 0 {
                        20
                    } else {
                        235
                    }
                }
                (1, _) => (y * 255 / h.max(1)) as i32,
                (2, _) => 128 + n * 3,
                _ => {
                    if x > y {
                        250
                    } else {
                        5
                    }
                }
            };
            v.clamp(0, 255) as u8
        };
        let y: Vec<u8> = (0..w * h).map(|i| sample(i % w, i / w, noise())).collect();
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let cb = (0..cw * ch)
            .map(|i| sample(i % cw * 2, i / cw * 2, noise() / 4).wrapping_add(7))
            .collect();
        let cr = (0..cw * ch)
            .map(|i| 255 - sample(i % cw * 2, i / cw * 2, 0))
            .collect();
        (y, cb, cr)
    }

    fn psnr(a: &[u16], b: &[u8]) -> f64 {
        let mse = a
            .iter()
            .zip(b)
            .map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2))
            .sum::<f64>()
            / b.len() as f64;
        10.0 * (255.0 * 255.0 / mse.max(1e-9)).log10()
    }

    /// The plane of a decoded frame, cropped to `w × h`.
    fn crop(frame: &Frame, c: usize, w: usize, h: usize) -> Vec<u16> {
        let stride = frame.widths[c] as usize;
        (0..h)
            .flat_map(|y| frame.planes[c][y * stride..][..w].to_vec())
            .collect()
    }

    fn round_trip(w: usize, h: usize, qp: u8, monochrome: bool) -> f64 {
        let (y, cb, cr) = test_picture(w, h, (w * 31 + h) as u32);
        let planes = Planes {
            y: &y,
            chroma: (!monochrome).then_some((&cb[..], &cr[..])),
        };
        let config = EncoderConfig {
            qp,
            ..EncoderConfig::default()
        };
        let (picture, recon) =
            encode_with_reconstruction(w as u32, h as u32, planes, &config).unwrap();
        let stream = picture.annex_b();

        // Without in-loop filters, the decoder reproduces the encoder's reconstruction exactly.
        let options = DecodeOptions {
            skip_loop_filters: true,
            verify_hash: false,
            max_pixels: 0,
        };
        let unfiltered = decode_picture(&stream, options).unwrap();
        let unit = if monochrome { 1 } else { 2 };
        assert_eq!(
            (unfiltered.widths[0], unfiltered.heights[0]),
            (
                (w as u32).next_multiple_of(unit),
                (h as u32).next_multiple_of(unit)
            )
        );
        assert_eq!(
            (picture.width, picture.height),
            (unfiltered.widths[0], unfiltered.heights[0])
        );
        let components = if monochrome { 1 } else { 3 };
        for c in 0..components {
            let (cw, ch) = (
                unfiltered.widths[c] as usize,
                unfiltered.heights[c] as usize,
            );
            assert_eq!(
                crop(&unfiltered, c, cw, ch),
                crop(&recon, c, cw, ch),
                "component {c}"
            );
        }

        let frame = decode_picture(&stream, DecodeOptions::default()).unwrap();
        let p = psnr(&crop(&frame, 0, w, h), &y);
        if !monochrome {
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let p_cb = psnr(&crop(&frame, 1, cw, ch), &cb);
            let p_cr = psnr(&crop(&frame, 2, cw, ch), &cr);
            assert!(
                p_cb.min(p_cr) > p.min(40.0) - 8.0,
                "{w}×{h}: {p:.1} {p_cb:.1} {p_cr:.1} dB"
            );
        }
        p
    }

    #[test]
    fn round_trip_sizes() {
        for (w, h) in [(1, 1), (8, 8), (17, 9), (64, 64), (100, 37), (33, 130)] {
            let p = round_trip(w, h, 22, false);
            assert!(p > 34.0, "{w}×{h}: PSNR {p:.1} dB");
        }
    }

    #[test]
    fn round_trip_monochrome() {
        assert!(round_trip(70, 45, 22, true) > 34.0);
    }

    #[test]
    fn quality_follows_qp() {
        let p: Vec<f64> = [4, 22, 37, 51]
            .iter()
            .map(|&qp| round_trip(96, 64, qp, false))
            .collect();
        assert!(p.windows(2).all(|w| w[0] > w[1]), "{p:?}");
        assert!(p[0] > 45.0, "{p:?}");
    }

    #[test]
    fn rejects_invalid_input() {
        let y = [0u8; 4];
        let planes = Planes {
            y: &y,
            chroma: None,
        };
        let config = EncoderConfig::default();
        assert!(encode_picture(0, 4, planes, &config).is_err());
        assert!(encode_picture(3, 1, planes, &config).is_err());
        assert!(encode_picture(2, 2, planes, &EncoderConfig { qp: 52, ..config }).is_err());
    }
}
