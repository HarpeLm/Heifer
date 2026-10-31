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
use crate::cabac::ArithmeticEncoder;
use crate::estimate::{BinLog, LogPoint};
use crate::params::{NalType, StreamParams, nal_unit};
use crate::rdo::{RdoqScratch, hide_signs, last_ctx, last_prefix, rdoq, sig_ctx_inc};
use crate::transform::{chroma_qp, forward_transform, quantize_levels, satd};

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

/// Planes of up to 10 bits per sample, for [`encode_picture16`]: same layout as [`Planes`].
#[derive(Debug, Clone, Copy)]
pub struct Planes16<'a> {
    /// Luma, `width × height`, row-major.
    pub y: &'a [u16],
    /// Chroma planes of 4:2:0 pictures, `ceil(width / 2) × ceil(height / 2)` each; `None` for
    /// monochrome pictures.
    pub chroma: Option<(&'a [u16], &'a [u16])>,
    /// Bits per sample: 8 (Main profile) to 10 (Main 10). Samples must fit.
    pub bit_depth: u8,
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
    let wide = |p: &[u8]| p.iter().map(|&v| u16::from(v)).collect::<Vec<u16>>();
    let (y, chroma) = (
        wide(planes.y),
        planes.chroma.map(|(cb, cr)| (wide(cb), wide(cr))),
    );
    let planes = Planes16 {
        y: &y,
        chroma: chroma.as_ref().map(|(cb, cr)| (&cb[..], &cr[..])),
        bit_depth: 8,
    };
    encode_picture16(width, height, planes, config)
}

/// Encodes one picture of up to 10 bits per sample as an IDR frame (Main 10 profile above 8
/// bits). 10-bit coding avoids the banding of smooth gradients, e.g. in skies.
pub fn encode_picture16(
    width: u32,
    height: u32,
    planes: Planes16<'_>,
    config: &EncoderConfig,
) -> Result<EncodedPicture, Error> {
    encode_with_reconstruction(width, height, planes, config).map(|(picture, ..)| picture)
}

/// Like [`encode_picture`], also returning the encoder's reconstruction of the picture (coded
/// size, before the in-loop filters) and how many coding units of each [`CuKind`] it chose.
fn encode_with_reconstruction(
    width: u32,
    height: u32,
    planes: Planes16<'_>,
    config: &EncoderConfig,
) -> Result<(EncodedPicture, Frame, [usize; 3]), Error> {
    if width == 0 || height == 0 || width > 16384 || height > 16384 {
        return Err(Error::InvalidInput(
            "picture size must be 1 to 16384 pixels",
        ));
    }
    if config.qp > 51 {
        return Err(Error::InvalidInput("QP must be 0 to 51"));
    }
    if !(8..=10).contains(&planes.bit_depth) {
        return Err(Error::InvalidInput("bit depth must be 8 to 10"));
    }
    let max = (1u16 << planes.bit_depth) - 1;
    let all = planes.y.iter().chain(
        planes
            .chroma
            .into_iter()
            .flat_map(|(cb, cr)| cb.iter().chain(cr)),
    );
    if all.into_iter().any(|&v| v > max) {
        return Err(Error::InvalidInput(
            "sample larger than the bit depth allows",
        ));
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
        bit_depth: planes.bit_depth,
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
            // Decide the block with estimated rates, then code the chosen bins for real.
            let contexts = enc.ctx;
            enc.quadtree(cx * ctb, cy * ctb, LOG2_CTB, 0);
            let decided = enc.ctx;
            enc.ctx = contexts;
            enc.log.replay(&mut enc.ctx, &mut enc.cabac);
            debug_assert_eq!(enc.ctx, decided);
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
        profile_idc: params.profile_idc(),
        level_idc: params.level_idc(),
        chroma_format: params.chroma_format,
        width: params.output_width,
        height: params.output_height,
    };
    let kinds = enc.kinds;
    Ok((picture, enc.recon.finish(true), kinds))
}

/// The ways a coding unit can be coded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CuKind {
    /// One prediction block, one transform block.
    Whole,
    /// One prediction block, four transform blocks of half size.
    SplitTransform,
    /// Four 4×4 prediction and transform blocks (8×8 coding units only).
    FourBlocks,
}

/// A transform block of one component, with its levels (empty when all are zero).
struct Tb {
    c: usize,
    log2_size: u8,
    mode: u8,
    levels: Vec<i32>,
    sse: f64,
}

/// A source plane, padded to the coded size by repeating the last column and row.
struct Plane {
    width: usize,
    samples: Vec<i32>,
}

impl Plane {
    fn padded(src: &[u16], w: usize, h: usize, pw: usize, ph: usize) -> Self {
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
    /// Whether the block was coded when the snapshot was taken.
    coded: bool,
    log: LogPoint,
    ctx: Contexts,
    /// Reconstructed samples of the block, per component.
    samples: [Vec<u16>; 3],
    depth: Vec<u8>,
    modes: Vec<u8>,
}

struct Encoder<'a> {
    recon: Reconstructor<'a>,
    /// The arithmetic coder, fed from `log` once each 32×32 block is decided.
    cabac: ArithmeticEncoder,
    log: BinLog,
    ctx: Contexts,
    src: [Plane; 3],
    chroma: bool,
    width: u32,
    height: u32,
    /// `QpY`, and the quantization parameters `Qp′Y` and `Qp′C` (with `QpBdOffset`).
    qp: i32,
    qp_luma: i32,
    qp_c: i32,
    bit_depth: u8,
    lambda: f64,
    sqrt_lambda: f64,
    /// Coding tree depth and luma intra mode of each 4×4 block.
    depth: Vec<u8>,
    modes: Vec<u8>,
    grid_w: usize,
    // Scratch buffers.
    diff: Vec<i32>,
    pred: Vec<i32>,
    coeffs: Vec<i32>,
    exact: Vec<f32>,
    rdoq: RdoqScratch,
    /// Best SATD cost per pixel (in units of √λ) found by the last `choose_modes`.
    last_satd: f64,
    /// Modes chosen for a block by `quadtree` before `coding_unit`, to reuse.
    mode_cache: Option<((u32, u32, u8), Vec<u8>)>,
    /// Chroma mode chosen for a luma mode, in the current coding unit.
    chroma_choice: Option<u8>,
    /// Coding units chosen of each kind, for tests and statistics.
    kinds: [usize; 3],
    /// Rate cost of a level made significant or zero, for sign data hiding, in units of the
    /// squared quantization step.
    sdh_rate: f32,
}

impl<'a> Encoder<'a> {
    fn new(
        sps: &'a Sps,
        pps: &'a heifer_hevc_dec::params::Pps,
        params: &StreamParams,
        (w, h): (usize, usize),
        planes: Planes16<'_>,
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
        let bd_offset = 6 * i32::from(planes.bit_depth - 8);
        // λ of the HM reference encoder for intra pictures, for distortions measured on
        // `bit_depth`-bit samples (4× larger per extra bit, squared).
        let lambda =
            0.57 * 2f64.powf(f64::from(qp - 12) / 3.0) * 4f64.powi(i32::from(planes.bit_depth - 8));
        let grid_w = pw / 4;
        Self {
            recon,
            cabac: ArithmeticEncoder::new(),
            log: BinLog::new(),
            ctx: Contexts::new(qp),
            src: [y, cb, cr],
            chroma: planes.chroma.is_some(),
            width: params.width,
            height: params.height,
            qp,
            qp_luma: qp + bd_offset,
            qp_c: chroma_qp(qp) + bd_offset,
            bit_depth: planes.bit_depth,
            lambda,
            sqrt_lambda: lambda.sqrt(),
            depth: vec![0; grid_w * (ph / 4)],
            modes: vec![1; grid_w * (ph / 4)],
            grid_w,
            diff: vec![0; 32 * 32],
            pred: vec![0; 32 * 32],
            coeffs: vec![0; 32 * 32],
            exact: vec![0.0; 32 * 32],
            kinds: [0; 3],
            chroma_choice: None,
            rdoq: RdoqScratch::new(),
            last_satd: 0.0,
            mode_cache: None,
            // About 3 bits, at λ / step² (step = 2^((Qp′ - 4) / 6)).
            sdh_rate: (3.0 * lambda / 2f64.powf(f64::from(qp + bd_offset - 4) / 3.0)) as f32,
        }
    }

    fn bin(&mut self, i: usize, bin: bool) {
        self.log.encode(&mut self.ctx[i], i, u8::from(bin));
    }

    fn grid(&self, x: u32, y: u32) -> usize {
        (y as usize / 4) * self.grid_w + x as usize / 4
    }

    fn fill_grid(&mut self, x0: u32, y0: u32, size: u32, depth: Option<u8>, mode: Option<u8>) {
        for y in (y0..y0 + size).step_by(4) {
            for x in (x0..x0 + size).step_by(4) {
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

    fn snapshot(&self, x0: u32, y0: u32, size: u32, coded: bool, log: LogPoint) -> Snapshot {
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
        for y in (y0..y0 + size).step_by(4) {
            for x in (x0..x0 + size).step_by(4) {
                let i = self.grid(x, y);
                depth.push(self.depth[i]);
                modes.push(self.modes[i]);
            }
        }
        Snapshot {
            coded,
            log,
            ctx: self.ctx,
            samples,
            depth,
            modes,
        }
    }

    fn restore(&mut self, x0: u32, y0: u32, size: u32, s: &Snapshot) {
        // Intra prediction must see the block as the decoder will: coded or not yet.
        self.recon.set_reconstructed(x0, y0, size, size, s.coded);
        self.log.restore(&s.log);
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
        for y in (y0..y0 + size).step_by(4) {
            for x in (x0..x0 + size).step_by(4) {
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
            return self.coding_unit(x0, y0, log2_size, depth).0;
        }

        // Very detailed 32×32 blocks are split without trying them whole: the whole block
        // almost never wins there (measured), and trying it costs as much as the split.
        if log2_size == LOG2_CTB {
            let mpm = self.mpm_candidates(x0, y0);
            let modes = self.choose_modes(x0, y0, log2_size, mpm, 3);
            self.mode_cache = Some(((x0, y0, log2_size), modes));
            if self.last_satd >= 2.0 {
                self.mode_cache = None;
                self.split_cu_flag(x0, y0, depth, true);
                return children
                    .iter()
                    .map(|&(dx, dy)| self.quadtree(x0 + dx, y0 + dy, log2_size - 1, depth + 1))
                    .sum();
            }
        }
        // The whole block first: when it needs no residual, smaller blocks cannot do better.
        let start_bits = self.log.bit_count();
        let start = self.snapshot(x0, y0, size, false, self.log.checkpoint());
        self.split_cu_flag(x0, y0, depth, false);
        let (d_whole, residual) = self.coding_unit(x0, y0, log2_size, depth);
        let j_whole = d_whole + self.lambda * (self.log.bit_count() - start_bits);
        // Blocks that cost little per pixel are smooth enough: splitting them almost never pays.
        let per_pixel = j_whole / (self.lambda * f64::from(size * size));
        if !residual || per_pixel < split_threshold(log2_size) {
            return d_whole;
        }
        let whole = self.snapshot(x0, y0, size, true, self.log.checkpoint_since(&start.log));

        self.restore(x0, y0, size, &start);
        self.split_cu_flag(x0, y0, depth, true);
        let d_split: f64 = children
            .iter()
            .map(|&(dx, dy)| self.quadtree(x0 + dx, y0 + dy, log2_size - 1, depth + 1))
            .sum();
        let j_split = d_split + self.lambda * (self.log.bit_count() - start_bits);
        if j_whole <= j_split {
            self.restore(x0, y0, size, &whole);
            d_whole
        } else {
            d_split
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

    /// Predicts a block with the decoder's intra prediction, without writing it to the picture,
    /// and puts the difference between the source and the prediction in `self.diff`.
    fn predict(&mut self, c: usize, x: u32, y: u32, log2_size: u8, mode: u8) {
        let n = 1usize << log2_size;
        let mut pred = std::mem::take(&mut self.pred);
        self.recon
            .predict_block(c, x, y, n, mode, &mut pred[..n * n]);
        let src = &self.src[c];
        for row in 0..n {
            let s = (y as usize + row) * src.width + x as usize;
            let (d, p) = (&mut self.diff[row * n..][..n], &pred[row * n..][..n]);
            for ((d, &p), &v) in d.iter_mut().zip(p).zip(&src.samples[s..s + n]) {
                *d = v - p;
            }
        }
        self.pred = pred;
    }

    /// The `count` most promising luma intra modes of a block, best first, by SATD and estimated
    /// mode bits: a coarse search over the 35 modes, refined around the best angle, plus the
    /// most probable modes (cheap to signal).
    fn choose_modes(
        &mut self,
        x0: u32,
        y0: u32,
        log2_size: u8,
        mpm: [u8; 3],
        count: usize,
    ) -> Vec<u8> {
        let n = 1usize << log2_size;
        let mut costs: Vec<(f64, u8)> = Vec::with_capacity(20);
        let eval = |enc: &mut Self, costs: &mut Vec<(f64, u8)>, mode: u8| {
            if costs.iter().any(|&(_, m)| m == mode) {
                return;
            }
            enc.predict(0, x0, y0, log2_size, mode);
            let bits = match mpm.iter().position(|&m| m == mode) {
                Some(0) => 2.0,
                Some(_) => 3.0,
                None => 6.0,
            };
            costs.push((
                satd(&enc.diff[..n * n], n) as f64 + enc.sqrt_lambda * bits,
                mode,
            ));
        };
        for &mode in [0, 1, 2, 6, 10, 14, 18, 22, 26, 30, 34].iter().chain(&mpm) {
            eval(self, &mut costs, mode);
        }
        let best_angle = costs
            .iter()
            .filter(|&&(_, m)| m >= 2)
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map_or(26, |&(_, m)| m);
        for mode in [
            best_angle.saturating_sub(2),
            best_angle.saturating_sub(1),
            best_angle + 1,
            best_angle + 2,
        ] {
            if (2..=34).contains(&mode) {
                eval(self, &mut costs, mode);
            }
        }
        costs.sort_by(|a, b| a.0.total_cmp(&b.0));
        self.last_satd = costs[0].0 / f64::from(1u32 << (2 * log2_size)) / self.sqrt_lambda;
        // Only modes whose estimate is close to the best one are worth coding for real.
        let limit = costs[0].0 * 1.1;
        costs
            .into_iter()
            .take(count)
            .take_while(|&(c, _)| c <= limit)
            .map(|(_, m)| m)
            .collect()
    }

    /// Computes the residual levels of one transform block and reconstructs it.
    fn transform_block(&mut self, c: usize, x: u32, y: u32, log2_size: u8, mode: u8) -> Tb {
        let n = 1usize << log2_size;
        self.predict(c, x, y, log2_size, mode);
        let mut coeffs = std::mem::take(&mut self.coeffs);
        let qp = if c == 0 { self.qp_luma } else { self.qp_c };
        let dst = c == 0 && log2_size == 2;
        forward_transform(
            &self.diff[..n * n],
            log2_size,
            dst,
            self.bit_depth,
            &mut coeffs[..n * n],
        );
        let mut exact = std::mem::take(&mut self.exact);
        let bd = self.bit_depth;
        quantize_levels(
            &mut coeffs[..n * n],
            log2_size,
            qp,
            bd,
            Some(&mut exact[..n * n]),
        );
        let scan = scan_type(log2_size, c == 0, mode);
        // λ in units of the squared quantization step, 2^((QP - 4) / 3).
        let lambda = (self.lambda / 2f64.powf(f64::from(qp - 4) / 3.0)) as f32;
        let levels = &mut coeffs[..n * n];
        rdoq(
            levels,
            &exact[..n * n],
            log2_size,
            c == 0,
            scan,
            &self.ctx,
            lambda,
            &mut self.rdoq,
        );
        hide_signs(levels, &exact[..n * n], log2_size, scan, self.sdh_rate);
        self.exact = exact;
        let coded = levels.iter().any(|&v| v != 0);
        // Reconstruct the block (prediction plus residual, if any) as the decoder will.
        {
            let (mut cols, mut rows) = (0, 0);
            for (i, _) in levels.iter().enumerate().filter(|(_, v)| **v != 0) {
                cols = cols.max(i % n + 1);
                rows = rows.max(i / n + 1);
            }
            self.recon.transform_block(&TransformBlock {
                c_idx: c as u8,
                x,
                y,
                log2_size,
                intra_mode: mode,
                coeffs: coded.then_some(&*levels),
                coeff_bounds: (cols as u8, rows as u8),
                transform_skip: false,
                transquant_bypass: false,
                qp_y: self.qp,
                cu_qp_offset: 0,
                res_scale_val: 0,
            });
        }
        let levels = if coded {
            coeffs[..n * n].to_vec()
        } else {
            Vec::new()
        };
        self.coeffs = coeffs;

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
        Tb {
            c,
            log2_size,
            mode,
            levels,
            sse: sse as f64,
        }
    }

    /// Chooses the chroma prediction mode of a coding unit whose (first) luma mode is `luma`:
    /// returns `intra_chroma_pred_mode` (4 = same as luma) and the mode it gives.
    fn choose_chroma_mode(&mut self, xc: u32, yc: u32, log2_size: u8, luma: u8) -> (u8, u8) {
        let n = 1usize << log2_size;
        let mut best = (f64::MAX, 4, luma);
        for syntax in [4u8, 0, 1, 2, 3] {
            let mode = match syntax {
                4 => luma,
                s => {
                    let m = [0, 26, 10, 1][usize::from(s)];
                    if m == luma { 34 } else { m }
                }
            };
            let mut cost = self.sqrt_lambda * if syntax == 4 { 1.0 } else { 3.0 };
            for c in 1..=2 {
                self.predict(c, xc, yc, log2_size, mode);
                cost += satd(&self.diff[..n * n], n) as f64;
            }
            if cost < best.0 {
                best = (cost, syntax, mode);
            }
        }
        (best.1, best.2)
    }

    /// Chroma transform blocks (Cb, Cr) at a chroma position, if the picture has chroma.
    fn chroma_blocks(&mut self, xc: u32, yc: u32, log2_size: u8, mode: u8) -> Option<[Tb; 2]> {
        self.chroma.then(|| {
            [
                self.transform_block(1, xc, yc, log2_size, mode),
                self.transform_block(2, xc, yc, log2_size, mode),
            ]
        })
    }

    /// `coding_unit()`: tries the best few prediction modes with one transform for the whole
    /// block, then the best of them with four transforms, and (for the smallest blocks) four 4×4
    /// prediction blocks, and keeps the cheapest. Returns the distortion and whether there is
    /// any residual.
    fn coding_unit(&mut self, x0: u32, y0: u32, log2_size: u8, depth: u8) -> (f64, bool) {
        const MODES_TRIED: usize = 3;
        let size = 1u32 << log2_size;
        let start_bits = self.log.bit_count();
        let start = self.snapshot(x0, y0, size, false, self.log.checkpoint());
        let mpm = self.mpm_candidates(x0, y0);
        let modes = match self.mode_cache.take() {
            Some((key, modes)) if key == (x0, y0, log2_size) => modes,
            _ => self.choose_modes(x0, y0, log2_size, mpm, MODES_TRIED),
        };
        self.chroma_choice = None;

        // (cost, distortion, kind, mode, residual, snapshot after coding)
        let mut best = None;
        let mut current = None; // what the encoder state holds now
        type Best = Option<(f64, f64, CuKind, u8, bool, Snapshot)>;
        let try_kind = |enc: &mut Self,
                        best: &mut Best,
                        current: &mut Option<(CuKind, u8)>,
                        kind: CuKind,
                        mode: u8| {
            if current.is_some() {
                enc.restore(x0, y0, size, &start);
            }
            let (d, residual) = enc.code_cu(x0, y0, log2_size, depth, kind, mode);
            *current = Some((kind, mode));
            let j = d + enc.lambda * (enc.log.bit_count() - start_bits);
            if best.as_ref().is_none_or(|b| j < b.0) {
                let after = enc.log.checkpoint_since(&start.log);
                *best = Some((
                    j,
                    d,
                    kind,
                    mode,
                    residual,
                    enc.snapshot(x0, y0, size, true, after),
                ));
            }
        };
        for &mode in &modes {
            try_kind(self, &mut best, &mut current, CuKind::Whole, mode);
        }
        let (_, _, _, mode, residual, _) = best.as_ref().expect("a mode was tried");
        // A prediction good enough to need no residual, or cheap enough, is not improved by
        // smaller blocks.
        let per_pixel = best.as_ref().map_or(0.0, |b| b.0) / (self.lambda * f64::from(size * size));
        if *residual && per_pixel >= small_blocks_threshold(log2_size) {
            let mode = *mode;
            try_kind(self, &mut best, &mut current, CuKind::SplitTransform, mode);
            if log2_size == LOG2_MIN_CB {
                try_kind(self, &mut best, &mut current, CuKind::FourBlocks, mode);
            }
        }
        let (_, d, kind, mode, residual, snapshot) = best.expect("a mode was tried");
        self.kinds[kind as usize] += 1;
        if current != Some((kind, mode)) {
            self.restore(x0, y0, size, &snapshot);
        }
        (d, residual)
    }

    /// Codes a coding unit of the given kind, with `mode` for 2N×2N prediction; returns its
    /// distortion and whether it has any residual.
    fn code_cu(
        &mut self,
        x0: u32,
        y0: u32,
        log2_size: u8,
        depth: u8,
        kind: CuKind,
        mode: u8,
    ) -> (f64, bool) {
        let size = 1u32 << log2_size;
        self.fill_grid(x0, y0, size, Some(depth), None);
        let four = kind == CuKind::FourBlocks;

        // Prediction modes and transform blocks, in decoding order: each block is predicted
        // from the ones reconstructed before it.
        let mut parts: Vec<(u8, [u8; 3])> = Vec::new(); // (mode, MPM candidates)
        let mut leaves: Vec<(Tb, Option<[Tb; 2]>)> = Vec::new(); // luma, chroma
        let mut small_chroma = None; // chroma of 4×4 luma blocks, coded after the last one
        // intra_chroma_pred_mode and the chroma mode (decided once the first luma mode is).
        let mut chroma = (4u8, mode);
        if four {
            let half = size / 2;
            for i in 0..4u32 {
                let (x, y) = (x0 + (i % 2) * half, y0 + (i / 2) * half);
                let mpm = self.mpm_candidates(x, y);
                let mode = self.choose_modes(x, y, log2_size - 1, mpm, 1)[0];
                self.fill_grid(x, y, half, None, Some(mode));
                parts.push((mode, mpm));
                leaves.push((self.transform_block(0, x, y, log2_size - 1, mode), None));
            }
            if self.chroma {
                chroma = self.choose_chroma_mode(x0 / 2, y0 / 2, log2_size - 1, parts[0].0);
            }
            small_chroma = self.chroma_blocks(x0 / 2, y0 / 2, log2_size - 1, chroma.1);
        } else {
            let mpm = self.mpm_candidates(x0, y0);
            self.fill_grid(x0, y0, size, None, Some(mode));
            parts.push((mode, mpm));
            if self.chroma {
                // The chroma mode is chosen once per coding unit, with its first luma mode, and
                // kept for the other luma modes tried when it can still be signalled.
                chroma = match self.chroma_choice {
                    Some(chosen) => chroma_syntax(chosen, mode),
                    None => {
                        let choice = self.choose_chroma_mode(x0 / 2, y0 / 2, log2_size - 1, mode);
                        self.chroma_choice = Some(choice.1);
                        choice
                    }
                };
            }
            if kind == CuKind::Whole {
                let luma = self.transform_block(0, x0, y0, log2_size, mode);
                let chroma = self.chroma_blocks(x0 / 2, y0 / 2, log2_size - 1, chroma.1);
                leaves.push((luma, chroma));
            } else {
                let half = size / 2;
                for i in 0..4u32 {
                    let (x, y) = (x0 + (i % 2) * half, y0 + (i / 2) * half);
                    let luma = self.transform_block(0, x, y, log2_size - 1, mode);
                    let chroma = if log2_size - 1 > 2 {
                        self.chroma_blocks(x / 2, y / 2, log2_size - 2, chroma.1)
                    } else {
                        None
                    };
                    leaves.push((luma, chroma));
                }
                if log2_size - 1 == 2 {
                    small_chroma = self.chroma_blocks(x0 / 2, y0 / 2, 2, chroma.1);
                }
            }
        }

        // Syntax: part_mode, the luma modes, the chroma mode, then the transform tree.
        if log2_size == LOG2_MIN_CB {
            self.bin(ctx::PART_MODE, !four); // 1: PART_2Nx2N, 0: PART_NxN
        }
        for &(mode, mpm) in &parts {
            self.bin(ctx::PREV_INTRA_LUMA_PRED_FLAG, mpm.contains(&mode));
        }
        for &(mode, mpm) in &parts {
            match mpm.iter().position(|&m| m == mode) {
                Some(i) => {
                    // mpm_idx: truncated unary, maximum 2.
                    self.log.encode_bypass(u8::from(i > 0));
                    if i > 0 {
                        self.log.encode_bypass(u8::from(i > 1));
                    }
                }
                None => {
                    let below = mpm.iter().filter(|&&m| m < mode).count() as u8;
                    self.log.encode_bypass_bits(u32::from(mode - below), 5);
                }
            }
        }
        if self.chroma {
            // intra_chroma_pred_mode: 0 for "as luma", else 1 and two bypass bits.
            self.bin(ctx::INTRA_CHROMA_PRED_MODE, chroma.0 != 4);
            if chroma.0 != 4 {
                self.log.encode_bypass_bits(u32::from(chroma.0), 2);
            }
        }

        let split = kind != CuKind::Whole;
        if !four {
            // split_transform_flag (max_transform_hierarchy_depth_intra = 1).
            self.bin(
                ctx::SPLIT_TRANSFORM_FLAG + usize::from(5 - log2_size),
                split,
            );
        }
        let cbf = |tb: &Tb| !tb.levels.is_empty();
        // Chroma cbfs of the root: is there any chroma residual in the tree? With split
        // transforms of 8×8 or more, each transform unit then has its own.
        let any = |k: usize| {
            small_chroma.as_ref().is_some_and(|c| cbf(&c[k]))
                || leaves
                    .iter()
                    .any(|(_, c)| c.as_ref().is_some_and(|c| cbf(&c[k])))
        };
        let (cb_any, cr_any) = (any(0), any(1));
        if self.chroma {
            self.bin(ctx::CBF_CHROMA, cb_any);
            self.bin(ctx::CBF_CHROMA, cr_any);
        }
        let mut sse = 0.0;
        let n_leaves = leaves.len();
        for (i, (luma, chroma)) in leaves.iter().enumerate() {
            if split && let Some(chroma) = chroma {
                if cb_any {
                    self.bin(ctx::CBF_CHROMA + 1, cbf(&chroma[0]));
                }
                if cr_any {
                    self.bin(ctx::CBF_CHROMA + 1, cbf(&chroma[1]));
                }
            }
            self.bin(ctx::CBF_LUMA + usize::from(!split), cbf(luma));
            if cbf(luma) {
                self.residual_coding(&luma.levels, 0, luma.log2_size, luma.mode);
            }
            sse += luma.sse;
            // Chroma of 4×4 luma blocks comes with the last of them.
            let chroma = match chroma {
                Some(c) => Some(c),
                None if i + 1 == n_leaves => small_chroma.as_ref(),
                None => None,
            };
            for tb in chroma.into_iter().flatten() {
                if cbf(tb) {
                    self.residual_coding(&tb.levels, tb.c, tb.log2_size, tb.mode);
                }
                sse += tb.sse;
            }
        }

        self.recon.coding_unit(&CodingUnit {
            x: x0,
            y: y0,
            log2_size,
            part_nxn: four,
            transquant_bypass: false,
            pcm: false,
            qp_y: self.qp,
        });
        (
            sse,
            cb_any || cr_any || leaves.iter().any(|(luma, _)| cbf(luma)),
        )
    }

    /// `residual_coding()` (§7.3.8.11) of the levels in `self.coeffs[c]`, which has at least one
    /// non-zero level: the mirror of the decoder's parsing.
    fn residual_coding(&mut self, coeffs: &[i32], c: usize, log2_size: u8, intra_mode: u8) {
        let size = 1usize << log2_size;
        let luma = c == 0;
        let scan = scan_type(log2_size, luma, intra_mode);
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
            let mut sig_pos = [0usize; 16];
            let mut n_sig = 0;
            if i == last_sb {
                sig[0] = level(i, last_pos);
                sig_pos[0] = last_pos;
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
                        sig_pos[n_sig] = n;
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

            // Sign data hiding: the sign of the first level follows from the parity.
            let sign_hidden = sig_pos[0] - sig_pos[n_sig - 1] > 3;
            let coded_signs = if sign_hidden { n_sig - 1 } else { n_sig };
            for &v in &sig[..coded_signs] {
                self.log.encode_bypass(u8::from(v < 0));
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
    }

    /// `last_sig_coeff_{x,y}_{prefix,suffix}` for the (scan-swapped) position `last`.
    fn last_sig_coeff(&mut self, last: [u32; 2], log2_size: u8, luma: bool) {
        let (ctx_offset, ctx_shift) = last_ctx(log2_size, luma);
        let c_max = (u32::from(log2_size) << 1) - 1;
        let parts = last.map(last_prefix);
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
                self.log.encode_bypass_bits(value, n);
            }
        }
    }

    /// `coeff_abs_level_remaining` (§9.3.3.11): Rice code, with an Exp-Golomb escape.
    fn coeff_abs_level_remaining(&mut self, value: u32, rice: u32) {
        if value < (4 << rice) {
            let prefix = value >> rice;
            for _ in 0..prefix {
                self.log.encode_bypass(1);
            }
            self.log.encode_bypass(0);
            self.log.encode_bypass_bits(value & ((1 << rice) - 1), rice);
        } else {
            let mut prefix = 4;
            while value >= (((2u32 << (prefix - 3)) + 2) << rice) {
                prefix += 1;
            }
            let base = ((1u32 << (prefix - 3)) + 2) << rice;
            for _ in 0..prefix {
                self.log.encode_bypass(1);
            }
            self.log.encode_bypass(0);
            self.log.encode_bypass_bits(value - base, prefix - 3 + rice);
        }
    }
}

/// `intra_chroma_pred_mode` giving chroma mode `chroma` with luma mode `luma`, and the chroma
/// mode it gives: `chroma` itself when possible, else the luma mode (`intra_chroma_pred_mode` 4).
fn chroma_syntax(chroma: u8, luma: u8) -> (u8, u8) {
    if chroma == luma {
        return (4, luma);
    }
    match [0, 26, 10, 1].iter().position(|&m| m == chroma) {
        Some(i) => (i as u8, chroma),
        // Mode 34 replaces a listed mode equal to the luma mode.
        None if chroma == 34 => match [0, 26, 10, 1].iter().position(|&m| m == luma) {
            Some(i) => (i as u8, 34),
            None => (4, luma),
        },
        None => (4, luma),
    }
}

/// Cost per pixel (in units of λ) of a whole block below which splitting it is not tried:
/// splits almost never win there (measured on photos and text).
fn split_threshold(log2_size: u8) -> f64 {
    if log2_size == LOG2_CTB {
        1.0 / 16.0
    } else {
        1.0 / 8.0
    }
}

/// Cost per pixel (in units of λ) of a coding unit below which split transforms and 4×4
/// blocks are not tried.
fn small_blocks_threshold(log2_size: u8) -> f64 {
    match log2_size {
        3 => 0.21,  // 2^-2.25
        4 => 0.088, // 2^-3.5
        _ => 0.044, // 2^-4.5
    }
}

/// Scan order of a block (§7.4.9.11): mode-dependent for small blocks, diagonal otherwise.
fn scan_type(log2_size: u8, luma: bool, intra_mode: u8) -> ScanType {
    if log2_size == 2 || (log2_size == 3 && luma) {
        match intra_mode {
            6..=14 => ScanType::Vertical,
            22..=30 => ScanType::Horizontal,
            _ => ScanType::Diagonal,
        }
    } else {
        ScanType::Diagonal
    }
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
        let wide = |p: &[u8]| p.iter().map(|&v| u16::from(v)).collect::<Vec<u16>>();
        let (y16, cb16, cr16) = (wide(&y), wide(&cb), wide(&cr));
        let planes = Planes16 {
            y: &y16,
            chroma: (!monochrome).then_some((&cb16[..], &cr16[..])),
            bit_depth: 8,
        };
        let config = EncoderConfig {
            qp,
            ..EncoderConfig::default()
        };
        let (picture, recon, kinds) =
            encode_with_reconstruction(w as u32, h as u32, planes, &config).unwrap();
        KINDS.with(|k| {
            for (total, n) in k.borrow_mut().iter_mut().zip(kinds) {
                *total += n;
            }
        });
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

    thread_local! {
        static KINDS: std::cell::RefCell<[usize; 3]> = const { std::cell::RefCell::new([0; 3]) };
    }

    #[test]
    fn every_kind_of_coding_unit_round_trips() {
        // The decoder reproduces the reconstruction (checked in `round_trip`) whatever the
        // encoder chose: make sure it chose every kind of coding unit.
        KINDS.with(|k| *k.borrow_mut() = [0; 3]);
        for qp in [10, 22, 34] {
            round_trip(96, 80, qp, false);
            round_trip(48, 40, qp, true);
        }
        let kinds = KINDS.with(|k| *k.borrow());
        assert!(kinds.iter().all(|&n| n > 0), "{kinds:?}");
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
    fn ten_bit_round_trip() {
        // A smooth 10-bit gradient (which 8 bits would turn into steps), with some texture.
        let (w, h) = (100usize, 70usize);
        let sample = |x: usize, y: usize, k: usize| -> u16 {
            let v = 64 + x * 8 + y * 2 + if (x / 5 + y / 7 + k) % 3 == 0 { 12 } else { 0 };
            v.min(1023) as u16
        };
        let y: Vec<u16> = (0..w * h).map(|i| sample(i % w, i / w, 0)).collect();
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let cb: Vec<u16> = (0..cw * ch)
            .map(|i| 300 + sample(i % cw, i / cw, 1) / 3)
            .collect();
        let cr: Vec<u16> = (0..cw * ch)
            .map(|i| 700 - sample(i % cw, i / cw, 2) / 4)
            .collect();
        for qp in [4u8, 22, 37] {
            let planes = Planes16 {
                y: &y,
                chroma: Some((&cb, &cr)),
                bit_depth: 10,
            };
            let config = EncoderConfig {
                qp,
                ..EncoderConfig::default()
            };
            let (picture, recon, _) =
                encode_with_reconstruction(w as u32, h as u32, planes, &config).unwrap();
            assert_eq!(picture.profile_idc, 2, "Main 10");
            let stream = picture.annex_b();
            let options = DecodeOptions {
                skip_loop_filters: true,
                verify_hash: false,
                max_pixels: 0,
            };
            let unfiltered = decode_picture(&stream, options).unwrap();
            assert_eq!(unfiltered.bit_depth, [10, 10]);
            for c in 0..3 {
                let (cw, ch) = (
                    unfiltered.widths[c] as usize,
                    unfiltered.heights[c] as usize,
                );
                assert_eq!(
                    crop(&unfiltered, c, cw, ch),
                    crop(&recon, c, cw, ch),
                    "QP {qp}, component {c}"
                );
            }
            let frame = decode_picture(&stream, DecodeOptions::default()).unwrap();
            let decoded = crop(&frame, 0, w, h);
            let mse = decoded
                .iter()
                .zip(&y)
                .map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2))
                .sum::<f64>()
                / (w * h) as f64;
            let psnr = 10.0 * (1023.0f64.powi(2) / mse.max(1e-9)).log10();
            assert!(
                psnr > if qp == 4 { 50.0 } else { 36.0 },
                "QP {qp}: {psnr:.1} dB"
            );
        }
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
        let big = [1024u16; 4];
        let planes16 = Planes16 {
            y: &big,
            chroma: None,
            bit_depth: 10,
        };
        assert!(encode_picture16(2, 2, planes16, &config).is_err());
        let planes16 = Planes16 {
            y: &big,
            chroma: None,
            bit_depth: 12,
        };
        assert!(encode_picture16(2, 2, planes16, &config).is_err());
    }
}
