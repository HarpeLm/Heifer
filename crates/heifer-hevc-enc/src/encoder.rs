//! Encoding of one intra picture.
//!
//! The encoder reconstructs the picture with the decoder's own [`Reconstructor`], fed with the
//! same blocks the decoder will parse: intra prediction and reconstruction are therefore
//! identical on both sides by construction. Decisions are made by rate-distortion cost
//! (`distortion + λ · bits`), with the bits counted by the CABAC encoder itself:
//!
//! - coding quadtree: each split is tried against coding the whole block (64 to 8);
//! - luma prediction modes: a coarse search over the 35 modes by SATD, refined around the best,
//!   then the best few coded for real (five per block, 4×4 blocks included);
//! - chroma mode: the two best by SATD coded for real;
//! - transform tree: split up to twice; 4×4 blocks may skip the transform;
//! - levels: RDOQ (including coding nothing), sign data hiding; then SAO on the deblocked picture.

use heifer_hevc_dec::contexts::{Contexts, ctx};
use heifer_hevc_dec::decoder::{DecodeOptions, decode_picture};
use heifer_hevc_dec::nal::NalUnit;
use heifer_hevc_dec::params::{ParameterSets, Sps};
use heifer_hevc_dec::recon::Frame;
use heifer_hevc_dec::recon::{IntraReferences, Reconstructor};
use heifer_hevc_dec::scan::{ScanType, scan_order};
use heifer_hevc_dec::slice::SliceHeader;
use heifer_hevc_dec::syntax::{CodingUnit, SaoParams, Sink, TransformBlock};

use crate::Error;
use crate::cabac::ArithmeticEncoder;
use crate::estimate::{BinLog, LogPoint, replay};
use crate::params::{NalType, StreamParams, nal_unit};
use crate::rdo::{
    RdoqScratch, hide_signs, last_ctx, last_prefix, rdoq, scan_positions, sig_ctx_table,
};
use crate::sao::{self, CtbSao};
use crate::transform::{chroma_qp, forward_transform, quantize_levels, satd};

const LOG2_CTB: u8 = 6;
/// Largest transform blocks: 32×32 (64×64 coding units split their transform).
const LOG2_MAX_TB: u8 = 5;
const LOG2_MIN_CB: u8 = 3;
/// `transform_skip_enabled_flag`: 4×4 blocks may skip the transform.
pub(crate) const TRANSFORM_SKIP: bool = true;
/// `max_transform_hierarchy_depth_intra`.
pub(crate) const MAX_TU_DEPTH: u8 = 2;

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
) -> Result<(EncodedPicture, Frame, [usize; 4]), Error> {
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
    // Decide each 32×32 block with estimated rates, keeping the bins of its chosen coding.
    let mut blocks = Vec::with_capacity((ctbs_x * ctbs_y) as usize);
    for cy in 0..ctbs_y {
        for cx in 0..ctbs_x {
            enc.quadtree(cx * ctb, cy * ctb, LOG2_CTB, 0);
            blocks.push(enc.log.take());
        }
    }
    if let Some(e) = enc.recon.error.take() {
        return Err(e.into());
    }

    // Slice data: the SAO parameters of each block, then its bins. SAO has its own contexts,
    // so the bins decided without it stay valid.
    let bit_depth = planes.bit_depth;
    let chroma = planes.chroma.is_some();
    let write = |sao: &[(CtbSao, SaoParams)]| -> Vec<u8> {
        let mut ctx = Contexts::new(params.qp);
        let mut coder = ArithmeticEncoder::new();
        for (i, bins) in blocks.iter().enumerate() {
            let (cx, cy) = (i as u32 % ctbs_x, i as u32 / ctbs_x);
            sao::write(
                &mut coder,
                &mut ctx,
                &sao[i].0,
                (cx > 0, cy > 0),
                chroma,
                bit_depth,
            );
            replay(bins, &mut ctx, &mut coder);
            let last = i + 1 == blocks.len();
            coder.encode_terminate(u8::from(last)); // end_of_slice_segment_flag
        }
        coder.finish()
    };
    // SAO is chosen on the picture as the decoder reconstructs it before SAO: decode a first
    // version without SAO.
    let off = vec![(CtbSao::Own(SaoParams::default()), SaoParams::default()); blocks.len()];
    let mut first = header_rbsp.clone();
    first.extend_from_slice(&write(&off));
    let mut stream = Vec::new();
    for nal in parameter_sets
        .iter()
        .chain([&nal_unit(NalType::IdrWRadl, &first)])
    {
        stream.extend_from_slice(&[0, 0, 0, 1]);
        stream.extend_from_slice(nal);
    }
    let deblocked = decode_picture(&stream, DecodeOptions::default())?;
    let src = [&enc.src[0], &enc.src[1], &enc.src[2]];
    let decisions = sao::decide(
        &deblocked,
        src.map(|p| &p.samples[..]),
        src.map(|p| p.width),
        chroma,
        ctb as usize,
        bit_depth,
        enc.lambda,
    );
    let mut rbsp = header_rbsp;
    rbsp.extend_from_slice(&write(&decisions));

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
    /// Position, in samples of the component.
    x: u32,
    y: u32,
    log2_size: u8,
    mode: u8,
    levels: Vec<i32>,
    /// Whether the residual is coded without transform.
    transform_skip: bool,
    sse: f64,
}

impl Tb {
    fn coded(&self) -> bool {
        !self.levels.is_empty()
    }
}

/// A transform tree: one transform unit, or four smaller trees.
enum TNode {
    /// The luma block and, from 8×8 luma up, the chroma blocks.
    Leaf(Tb, Option<[Tb; 2]>),
    /// Four quarters and, when they are 4×4 luma blocks, the chroma blocks they share.
    Split(Vec<TNode>, Option<[Tb; 2]>),
}

impl TNode {
    fn chroma(&self) -> &Option<[Tb; 2]> {
        match self {
            Self::Leaf(_, chroma) | Self::Split(_, chroma) => chroma,
        }
    }

    /// Whether Cb (`k` = 0) or Cr (1) has residual anywhere in the tree.
    fn chroma_cbf(&self, k: usize) -> bool {
        self.chroma().as_ref().is_some_and(|c| c[k].coded())
            || matches!(self, Self::Split(blocks, _) if blocks.iter().any(|b| b.chroma_cbf(k)))
    }

    fn residual(&self) -> bool {
        self.chroma_cbf(0)
            || self.chroma_cbf(1)
            || match self {
                Self::Leaf(luma, _) => luma.coded(),
                Self::Split(blocks, _) => blocks.iter().any(Self::residual),
            }
    }

    fn sse(&self) -> f64 {
        let chroma: f64 = self.chroma().iter().flatten().map(|tb| tb.sse).sum();
        chroma
            + match self {
                Self::Leaf(luma, _) => luma.sse,
                Self::Split(blocks, _) => blocks.iter().map(Self::sse).sum(),
            }
    }
}

/// Sample, depth and mode buffers of a [`Snapshot`].
type SnapshotBuffers = ([Vec<u16>; 3], Vec<u8>, Vec<u8>);

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
    /// Bins of the trials, with their estimated rates.
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
    /// Modes and SATD of the first 32×32 block of the current 64×64 block, at its position.
    quadrant_search: Option<((u32, u32), Vec<u8>, f64)>,
    /// Whether transform blocks are coded without being reconstructed (whole-block trials).
    deferred: bool,
    /// Transform skip trials and wins.
    ts_trials: (u32, u32),
    /// Buffers of snapshots given back, reused by the next ones.
    pool: Vec<SnapshotBuffers>,
    /// Chroma mode chosen for a luma mode, in the current coding unit.
    chroma_choice: Option<u8>,
    /// Coding units chosen of each kind, then transform-skipped blocks, for tests.
    kinds: [usize; 4],
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
            kinds: [0; 4],
            chroma_choice: None,
            rdoq: RdoqScratch::new(),
            last_satd: 0.0,
            mode_cache: None,
            quadrant_search: None,
            pool: Vec::new(),
            ts_trials: (0, 0),
            deferred: false,
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
    fn regions(
        &self,
        x0: u32,
        y0: u32,
        size: u32,
    ) -> impl Iterator<Item = (usize, u32, u32, u32)> + use<> {
        let n = if self.chroma { 3 } else { 1 };
        [
            (0, x0, y0, size),
            (1, x0 / 2, y0 / 2, size / 2),
            (2, x0 / 2, y0 / 2, size / 2),
        ]
        .into_iter()
        .take(n)
    }

    /// Saves the state of a block, to try another coding of it and come back. The buffers come
    /// from the ones given back with [`recycle`](Self::recycle).
    fn snapshot(&mut self, x0: u32, y0: u32, size: u32, coded: bool, log: LogPoint) -> Snapshot {
        let (mut samples, mut depth, mut modes) = self.pool.pop().unwrap_or_default();
        for v in &mut samples {
            v.clear();
        }
        depth.clear();
        modes.clear();
        for (c, x, y, n) in self.regions(x0, y0, size) {
            let stride = self.recon.frame.widths[c] as usize;
            let plane = &self.recon.frame.planes[c];
            for row in y..y + n {
                let start = row as usize * stride + x as usize;
                samples[c].extend_from_slice(&plane[start..start + n as usize]);
            }
        }
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

    /// Gives back the buffers of a snapshot no longer needed.
    fn recycle(&mut self, s: Snapshot) {
        self.pool.push((s.samples, s.depth, s.modes));
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
        // almost never wins there (measured), and trying it costs as much as the split. 64×64
        // blocks are predicted in 32×32 transform blocks: their modes are estimated on the
        // first one.
        if log2_size >= LOG2_MAX_TB {
            // The first 32×32 block of a 64×64 block is searched once for both.
            let modes = match self.quadrant_search.take() {
                Some((at, modes, satd)) if at == (x0, y0) && log2_size == LOG2_MAX_TB => {
                    self.last_satd = satd;
                    modes
                }
                _ => {
                    let mpm = self.mpm_candidates(x0, y0);
                    self.choose_modes(x0, y0, LOG2_MAX_TB, mpm, 3)
                }
            };
            if log2_size > LOG2_MAX_TB {
                self.quadrant_search = Some(((x0, y0), modes.clone(), self.last_satd));
            }
            self.mode_cache = Some(((x0, y0, log2_size), modes));
            let limit = if log2_size > LOG2_MAX_TB { 1.0 } else { 2.0 };
            if self.last_satd >= limit {
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
            self.recycle(start);
            return d_whole;
        }
        let whole = self.snapshot(x0, y0, size, true, self.log.checkpoint_since(&start.log));

        self.restore(x0, y0, size, &start);
        self.recycle(start);
        self.split_cu_flag(x0, y0, depth, true);
        let d_split: f64 = children
            .iter()
            .map(|&(dx, dy)| self.quadtree(x0 + dx, y0 + dy, log2_size - 1, depth + 1))
            .sum();
        let j_split = d_split + self.lambda * (self.log.bit_count() - start_bits);
        if j_whole <= j_split {
            self.restore(x0, y0, size, &whole);
        }
        self.recycle(whole);
        if j_whole <= j_split { d_whole } else { d_split }
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
        self.pred = pred;
        self.diff_from_prediction(c, x, y, n);
    }

    /// Like [`predict`](Self::predict), from reference samples gathered once for several modes.
    #[allow(clippy::too_many_arguments)]
    fn predict_from(
        &mut self,
        refs: &IntraReferences,
        c: usize,
        x: u32,
        y: u32,
        log2_size: u8,
        mode: u8,
    ) {
        let n = 1usize << log2_size;
        let mut pred = std::mem::take(&mut self.pred);
        self.recon.predict_from(refs, mode, &mut pred[..n * n]);
        self.pred = pred;
        self.diff_from_prediction(c, x, y, n);
    }

    /// `self.diff` = source − `self.pred`, for an `n`×`n` block.
    fn diff_from_prediction(&mut self, c: usize, x: u32, y: u32, n: usize) {
        let pred = std::mem::take(&mut self.pred);
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
        let refs = self.recon.intra_references(0, x0, y0, n);
        let eval = |enc: &mut Self, costs: &mut Vec<(f64, u8)>, mode: u8| {
            if costs.iter().any(|&(_, m)| m == mode) {
                return;
            }
            enc.predict_from(&refs, 0, x0, y0, log2_size, mode);
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
        let limit = costs[0].0 * 1.25;
        costs
            .into_iter()
            .take(count)
            .take_while(|&(c, _)| c <= limit)
            .map(|(_, m)| m)
            .collect()
    }

    /// Computes the residual levels of one transform block and reconstructs it.
    fn transform_block(&mut self, c: usize, x: u32, y: u32, log2_size: u8, mode: u8) -> Tb {
        self.predict(c, x, y, log2_size, mode);
        let w = if c == 0 { 1.0 } else { CHROMA_WEIGHT };
        let levels = self.quantize(c, log2_size, mode, false);
        let sse = self.distortion(c, x, y, log2_size, mode, &levels, false);
        let tb = Tb {
            c,
            x,
            y,
            log2_size,
            mode,
            levels,
            transform_skip: false,
            sse: sse as f64 * w,
        };
        // Tried while it wins often enough (graphics, text), not on photos where it hardly does.
        let worth = self.ts_trials.0 < 256 || self.ts_trials.1 * 32 >= self.ts_trials.0;
        if log2_size != 2 || !tb.coded() || !TRANSFORM_SKIP || !worth {
            return tb;
        }
        // 4×4 blocks can also code the residual without transform, which suits sharp edges
        // (text, graphics): keep the cheaper in rate and distortion.
        self.ts_trials.0 += 1;
        let levels = self.quantize(c, 2, mode, true);
        if levels.is_empty() {
            return tb;
        }
        let sse = self.distortion(c, x, y, 2, mode, &levels, true);
        let skip = Tb {
            levels,
            transform_skip: true,
            sse: sse as f64 * w,
            ..tb
        };
        if self.tb_cost(&skip) < self.tb_cost(&tb) {
            self.ts_trials.1 += 1;
            self.kinds[3] += 1;
            skip
        } else {
            if !self.deferred {
                self.reconstruct(c, x, y, 2, mode, &tb.levels, false);
            }
            tb
        }
    }

    /// The sum of squared errors of a block just quantized: from its reconstruction or, in
    /// deferred trials, estimated in the transform domain without reconstructing it.
    #[allow(clippy::too_many_arguments)]
    fn distortion(
        &mut self,
        c: usize,
        x: u32,
        y: u32,
        log2_size: u8,
        mode: u8,
        levels: &[i32],
        transform_skip: bool,
    ) -> i64 {
        if !self.deferred {
            return self.reconstruct(c, x, y, log2_size, mode, levels, transform_skip);
        }
        let n = 1usize << log2_size;
        if levels.is_empty() {
            // The reconstruction is the prediction.
            return self.diff[..n * n]
                .iter()
                .map(|&d| i64::from(d) * i64::from(d))
                .sum();
        }
        // The transforms preserve energy: the errors of the levels, in quantization steps.
        let qp = if c == 0 { self.qp_luma } else { self.qp_c };
        let step2 = 2f64.powf(f64::from(qp - 4) / 3.0);
        let error: f64 = self.exact[..n * n]
            .iter()
            .zip(levels)
            .map(|(&e, &l)| (f64::from(e) - f64::from(l)).powi(2))
            .sum();
        (error * step2).round() as i64
    }

    /// Reconstructs the blocks of a tree coded in a deferred trial.
    fn reconstruct_tree(&mut self, tree: &TNode) {
        let (blocks, chroma) = match tree {
            TNode::Leaf(luma, chroma) => (std::slice::from_ref(luma), chroma),
            TNode::Split(..) => unreachable!("deferred trials code whole blocks"),
        };
        for tb in blocks.iter().chain(chroma.iter().flatten()) {
            self.predict(tb.c, tb.x, tb.y, tb.log2_size, tb.mode);
            self.reconstruct(
                tb.c,
                tb.x,
                tb.y,
                tb.log2_size,
                tb.mode,
                &tb.levels,
                tb.transform_skip,
            );
        }
    }

    /// Rate-distortion cost of a coded transform block: its distortion and the estimated bits of
    /// its residual.
    fn tb_cost(&mut self, tb: &Tb) -> f64 {
        let (point, contexts) = (self.log.checkpoint(), self.ctx);
        let start = self.log.bit_count();
        self.residual_coding(tb);
        let bits = self.log.bit_count() - start;
        self.log.restore(&point);
        self.ctx = contexts;
        tb.sse + self.lambda * bits
    }

    /// Transforms (unless `transform_skip`) and quantizes the residual in `self.diff`, with
    /// rate-distortion optimized levels; returns the levels, or nothing when all are zero.
    fn quantize(&mut self, c: usize, log2_size: u8, mode: u8, transform_skip: bool) -> Vec<i32> {
        let n = 1usize << log2_size;
        let mut coeffs = std::mem::take(&mut self.coeffs);
        let qp = if c == 0 { self.qp_luma } else { self.qp_c };
        let bd = self.bit_depth;
        if transform_skip {
            // The scale of the transforms' output: 2^(15 - bit depth - log2 size).
            let shift = 15 - u32::from(bd) - u32::from(log2_size);
            for (o, &d) in coeffs[..n * n].iter_mut().zip(&self.diff[..n * n]) {
                *o = d << shift;
            }
        } else {
            let dst = c == 0 && log2_size == 2;
            forward_transform(
                &self.diff[..n * n],
                log2_size,
                dst,
                bd,
                &mut coeffs[..n * n],
            );
        }
        let mut exact = std::mem::take(&mut self.exact);
        let has_levels = quantize_levels(
            &mut coeffs[..n * n],
            log2_size,
            qp,
            bd,
            Some(&mut exact[..n * n]),
        );
        if !has_levels && exact[..n * n].iter().all(|&level| level.abs() < 0.5) {
            self.exact = exact;
            self.coeffs = coeffs;
            return Vec::new();
        }
        let scan = scan_type(log2_size, c == 0, mode);
        // λ in units of the squared quantization step, 2^((QP - 4) / 3).
        let w = if c == 0 { 1.0 } else { CHROMA_WEIGHT };
        let lambda = (self.lambda / w / 2f64.powf(f64::from(qp - 4) / 3.0)) as f32;
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
        let levels = if levels.iter().any(|&v| v != 0) {
            levels.to_vec()
        } else {
            Vec::new()
        };
        self.coeffs = coeffs;
        levels
    }

    /// Reconstructs a block (the prediction in `self.pred` plus the residual of `levels`, if
    /// any) as the decoder will; returns its sum of squared errors.
    #[allow(clippy::too_many_arguments)]
    fn reconstruct(
        &mut self,
        c: usize,
        x: u32,
        y: u32,
        log2_size: u8,
        mode: u8,
        levels: &[i32],
        transform_skip: bool,
    ) -> i64 {
        let n = 1usize << log2_size;
        let (mut cols, mut rows) = (0, 0);
        for (i, _) in levels.iter().enumerate().filter(|(_, v)| **v != 0) {
            cols = cols.max(i % n + 1);
            rows = rows.max(i / n + 1);
        }
        let prediction = std::mem::take(&mut self.pred);
        self.recon.reconstruct(
            &TransformBlock {
                c_idx: c as u8,
                x,
                y,
                log2_size,
                intra_mode: mode,
                coeffs: (!levels.is_empty()).then_some(levels),
                coeff_bounds: (cols as u8, rows as u8),
                transform_skip,
                transquant_bypass: false,
                qp_y: self.qp,
                cu_qp_offset: 0,
                res_scale_val: 0,
            },
            &prediction,
        );
        self.pred = prediction;
        self.block_sse(c, x, y, n)
    }

    /// Sum of squared errors of a reconstructed block against the source.
    fn block_sse(&self, c: usize, x: u32, y: u32, n: usize) -> i64 {
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
        sse
    }

    /// Chooses the chroma prediction mode of a coding unit whose (first) luma mode is `luma`:
    /// returns `intra_chroma_pred_mode` (4 = same as luma) and the mode it gives.
    fn choose_chroma_mode(
        &mut self,
        xc: u32,
        yc: u32,
        log2_size: u8,
        luma: u8,
        luma_coded: bool,
    ) -> (u8, u8) {
        let n = 1usize << log2_size;
        let refs = [1, 2].map(|c| self.recon.intra_references(c, xc, yc, n));
        let mut costs = Vec::with_capacity(5);
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
                self.predict_from(&refs[c - 1], c, xc, yc, log2_size, mode);
                cost += satd(&self.diff[..n * n], n) as f64;
            }
            costs.push((cost, syntax, mode));
        }
        costs.sort_by(|a, b| a.0.total_cmp(&b.0));
        let tried = 2;
        // The best few by SATD are coded (as one transform block per component), and the
        // cheapest in rate and distortion is kept.
        let log2_tb = log2_size.min(LOG2_MAX_TB - 1);
        let (x0, y0, size) = (xc * 2, yc * 2, 2u32 << log2_size);
        let start = self.snapshot(x0, y0, size, luma_coded, self.log.checkpoint());
        let contexts = self.ctx;
        let mut best = (f64::MAX, costs[0].1, costs[0].2);
        for (i, &(_, syntax, mode)) in costs.iter().take(tried).enumerate() {
            if i > 0 {
                self.restore(x0, y0, size, &start);
            }
            let bits = self.log.bit_count();
            self.bin(ctx::INTRA_CHROMA_PRED_MODE, syntax != 4);
            if syntax != 4 {
                self.log.encode_bypass_bits(0, 2);
            }
            let mut d = 0.0;
            let step = 1u32 << log2_tb;
            for y in (yc..yc + (1 << log2_size)).step_by(step as usize) {
                for x in (xc..xc + (1 << log2_size)).step_by(step as usize) {
                    for tb in self
                        .chroma_blocks(x, y, log2_tb, mode)
                        .into_iter()
                        .flatten()
                    {
                        self.bin(ctx::CBF_CHROMA, tb.coded());
                        if tb.coded() {
                            self.residual_coding(&tb);
                        }
                        d += tb.sse;
                    }
                }
            }
            let j = d + self.lambda * (self.log.bit_count() - bits);
            self.ctx = contexts;
            if j < best.0 {
                best = (j, syntax, mode);
            }
        }
        self.restore(x0, y0, size, &start);
        self.recycle(start);
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
        let size = 1u32 << log2_size;
        let start_bits = self.log.bit_count();
        let start = self.snapshot(x0, y0, size, false, self.log.checkpoint());
        let mpm = self.mpm_candidates(x0, y0);
        let modes = match self.mode_cache.take() {
            Some((key, modes)) if key == (x0, y0, log2_size) => modes,
            _ => self.choose_modes(x0, y0, log2_size.min(LOG2_MAX_TB), mpm, 5),
        };
        self.chroma_choice = None;

        // (cost, distortion, kind, mode, residual, snapshot after coding)
        let mut best = None;
        let mut current = None; // what the encoder state holds now
        type Best = Option<(f64, f64, CuKind, u8, bool, Snapshot, Option<TNode>)>;
        let try_kind = |enc: &mut Self,
                        best: &mut Best,
                        current: &mut Option<(CuKind, u8)>,
                        kind: CuKind,
                        mode: u8| {
            if current.is_some() {
                enc.restore(x0, y0, size, &start);
            }
            // Whole blocks have one transform block per component, which no other block of the
            // trial predicts from: they are reconstructed only if chosen.
            enc.deferred = kind == CuKind::Whole && log2_size <= LOG2_MAX_TB;
            let (d, residual, deferred) = enc.code_cu(x0, y0, log2_size, depth, kind, mode);
            enc.deferred = false;
            *current = Some((kind, mode));
            let j = d + enc.lambda * (enc.log.bit_count() - start_bits);
            if best.as_ref().is_none_or(|b| j < b.0) {
                let after = enc.log.checkpoint_since(&start.log);
                let snapshot = enc.snapshot(x0, y0, size, true, after);
                if let Some(old) = best.replace((j, d, kind, mode, residual, snapshot, deferred)) {
                    enc.recycle(old.5);
                }
            }
        };
        for &mode in &modes {
            try_kind(self, &mut best, &mut current, CuKind::Whole, mode);
        }
        let (_, _, _, mode, residual, _, _) = best.as_ref().expect("a mode was tried");
        // A prediction good enough to need no residual, or cheap enough, is not improved by
        // smaller blocks.
        let per_pixel = best.as_ref().map_or(0.0, |b| b.0) / (self.lambda * f64::from(size * size));
        if *residual && log2_size <= LOG2_MAX_TB && per_pixel >= small_blocks_threshold(log2_size) {
            let mode = *mode;
            try_kind(self, &mut best, &mut current, CuKind::SplitTransform, mode);
            // Where split transforms win, the next modes may win with them too.
            if best.as_ref().is_some_and(|b| b.2 == CuKind::SplitTransform) {
                for &m in modes.iter().filter(|&&m| m != mode).take(2) {
                    try_kind(self, &mut best, &mut current, CuKind::SplitTransform, m);
                }
            }
            if log2_size == LOG2_MIN_CB {
                try_kind(self, &mut best, &mut current, CuKind::FourBlocks, mode);
            }
        }
        let (_, d, kind, mode, residual, snapshot, deferred) = best.expect("a mode was tried");
        self.kinds[kind as usize] += 1;
        if current != Some((kind, mode)) {
            self.restore(x0, y0, size, &snapshot);
        }
        if let Some(tree) = deferred {
            self.reconstruct_tree(&tree);
        }
        self.recycle(snapshot);
        self.recycle(start);
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
    ) -> (f64, bool, Option<TNode>) {
        let size = 1u32 << log2_size;
        self.fill_grid(x0, y0, size, Some(depth), None);
        let four = kind == CuKind::FourBlocks;

        // Prediction modes and transform blocks, in decoding order: each block is predicted
        // from the ones reconstructed before it.
        let mut parts: Vec<(u8, [u8; 3])> = Vec::new(); // (mode, MPM candidates)
        // intra_chroma_pred_mode and the chroma mode (decided once the first luma mode is).
        let mut chroma = (4u8, mode);
        let tree = if four {
            let half = size / 2;
            let mut blocks = Vec::new();
            for i in 0..4u32 {
                let (x, y) = (x0 + (i % 2) * half, y0 + (i / 2) * half);
                let mpm = self.mpm_candidates(x, y);
                let (mode, luma) = self.small_block(x, y, mpm);
                parts.push((mode, mpm));
                blocks.push(TNode::Leaf(luma, None));
            }
            if self.chroma {
                chroma = self.choose_chroma_mode(x0 / 2, y0 / 2, log2_size - 1, parts[0].0, true);
            }
            let chroma = self.chroma_blocks(x0 / 2, y0 / 2, log2_size - 1, chroma.1);
            TNode::Split(blocks, chroma)
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
                        let choice =
                            self.choose_chroma_mode(x0 / 2, y0 / 2, log2_size - 1, mode, false);
                        self.chroma_choice = Some(choice.1);
                        choice
                    }
                };
            }
            if kind == CuKind::Whole && log2_size <= LOG2_MAX_TB {
                self.transform_leaf(x0, y0, log2_size, mode, chroma.1)
            } else {
                self.split_tree(x0, y0, log2_size, mode, chroma.1, 1)
            }
        };

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
        self.write_tree(&tree, log2_size, 0, (true, true), four);

        self.recon.coding_unit(&CodingUnit {
            x: x0,
            y: y0,
            log2_size,
            part_nxn: four,
            transquant_bypass: false,
            pcm: false,
            qp_y: self.qp,
        });
        let (sse, residual) = (tree.sse(), tree.residual());
        (sse, residual, self.deferred.then_some(tree))
    }

    /// A 4×4 luma prediction block of an N×N coding unit: tries the few best modes by SATD
    /// and keeps the one with the cheapest rate-distortion cost (mode, cbf and residual).
    fn small_block(&mut self, x: u32, y: u32, mpm: [u8; 3]) -> (u8, Tb) {
        let modes = self.choose_modes(x, y, 2, mpm, 5);
        let start = (modes.len() > 1).then(|| self.snapshot(x, y, 4, false, self.log.checkpoint()));
        let mut best: Option<(f64, u8, Tb, Snapshot)> = None;
        for &mode in &modes {
            let Some(start) = &start else { break };
            if best.is_some() {
                self.restore(x, y, 4, start);
            }
            self.fill_grid(x, y, 4, None, Some(mode));
            let tb = self.transform_block(0, x, y, 2, mode);
            let contexts = self.ctx;
            let bits = self.log.bit_count();
            self.bin(ctx::PREV_INTRA_LUMA_PRED_FLAG, mpm.contains(&mode));
            match mpm.iter().position(|&m| m == mode) {
                Some(i) => self.log.encode_bypass_bits(0, 1 + u32::from(i > 0)),
                None => self.log.encode_bypass_bits(0, 5),
            }
            self.bin(ctx::CBF_LUMA, tb.coded());
            if tb.coded() {
                self.residual_coding(&tb);
            }
            let j = tb.sse + self.lambda * (self.log.bit_count() - bits);
            self.log.restore(&start.log);
            self.ctx = contexts;
            if best.as_ref().is_none_or(|b| j < b.0) {
                let after = self.snapshot(x, y, 4, true, self.log.checkpoint());
                if let Some(old) = best.replace((j, mode, tb, after)) {
                    self.recycle(old.3);
                }
            }
        }
        if let Some(start) = start {
            self.recycle(start);
        }
        match best {
            Some((_, mode, tb, after)) => {
                self.restore(x, y, 4, &after);
                self.recycle(after);
                (mode, tb)
            }
            None => {
                let mode = modes[0];
                self.fill_grid(x, y, 4, None, Some(mode));
                (mode, self.transform_block(0, x, y, 2, mode))
            }
        }
    }

    /// One transform unit: the luma block and, from 8×8 luma up, its chroma blocks.
    fn transform_leaf(&mut self, x: u32, y: u32, log2_size: u8, mode: u8, chroma: u8) -> TNode {
        let luma = self.transform_block(0, x, y, log2_size, mode);
        let chroma = if log2_size > 2 {
            self.chroma_blocks(x / 2, y / 2, log2_size - 1, chroma)
        } else {
            None
        };
        TNode::Leaf(luma, chroma)
    }

    /// A block split into four transform trees at `depth`; 4×4 luma blocks share the chroma
    /// blocks of their parent.
    fn split_tree(
        &mut self,
        x0: u32,
        y0: u32,
        log2_size: u8,
        mode: u8,
        chroma: u8,
        depth: u8,
    ) -> TNode {
        let half = 1u32 << (log2_size - 1);
        let mut blocks = Vec::with_capacity(4);
        for i in 0..4u32 {
            let (x, y) = (x0 + (i % 2) * half, y0 + (i / 2) * half);
            blocks.push(self.transform_tree(x, y, log2_size - 1, mode, chroma, depth));
        }
        let chroma = if log2_size == 3 {
            self.chroma_blocks(x0 / 2, y0 / 2, 2, chroma)
        } else {
            None
        };
        TNode::Split(blocks, chroma)
    }

    /// A transform tree below the root: one transform unit or, when it has residual worth
    /// coding more finely and is cheaper that way, four.
    fn transform_tree(
        &mut self,
        x: u32,
        y: u32,
        log2_size: u8,
        mode: u8,
        chroma: u8,
        depth: u8,
    ) -> TNode {
        if log2_size == 2 || depth == MAX_TU_DEPTH {
            return self.transform_leaf(x, y, log2_size, mode, chroma);
        }
        let size = 1u32 << log2_size;
        let start = self.snapshot(x, y, size, false, self.log.checkpoint());
        let leaf = self.transform_leaf(x, y, log2_size, mode, chroma);
        if !leaf.residual() {
            self.recycle(start);
            return leaf;
        }
        let whole = self.tree_cost(&leaf, log2_size, depth);
        if whole / (self.lambda * f64::from(size * size)) < small_blocks_threshold(log2_size) {
            self.recycle(start);
            return leaf;
        }
        let after = self.snapshot(x, y, size, true, self.log.checkpoint());
        self.restore(x, y, size, &start);
        self.recycle(start);
        let split = self.split_tree(x, y, log2_size, mode, chroma, depth + 1);
        let tree = if self.tree_cost(&split, log2_size, depth) < whole {
            split
        } else {
            self.restore(x, y, size, &after);
            leaf
        };
        self.recycle(after);
        tree
    }

    /// The rate-distortion cost of a transform tree, its syntax estimated and then discarded.
    fn tree_cost(&mut self, tree: &TNode, log2_size: u8, depth: u8) -> f64 {
        let (point, contexts) = (self.log.checkpoint(), self.ctx);
        let start = self.log.bit_count();
        self.write_tree(tree, log2_size, depth, (true, true), false);
        let bits = self.log.bit_count() - start;
        self.log.restore(&point);
        self.ctx = contexts;
        tree.sse() + self.lambda * bits
    }

    /// `transform_tree()` (§7.3.8.8) of a coded tree: split flags, cbfs and residuals.
    /// `parent` holds the chroma cbfs of the parent, `intra_split` whether the coding unit has
    /// four prediction blocks.
    fn write_tree(
        &mut self,
        tree: &TNode,
        log2_size: u8,
        depth: u8,
        parent: (bool, bool),
        intra_split: bool,
    ) {
        // The split is inferred for 4×4 prediction blocks and blocks larger than 32×32.
        if (3..=LOG2_MAX_TB).contains(&log2_size)
            && depth < MAX_TU_DEPTH + u8::from(intra_split)
            && !(intra_split && depth == 0)
        {
            let split = matches!(tree, TNode::Split(..));
            self.bin(
                ctx::SPLIT_TRANSFORM_FLAG + usize::from(5 - log2_size),
                split,
            );
        }
        let cbf = (tree.chroma_cbf(0), tree.chroma_cbf(1));
        if self.chroma && log2_size > 2 {
            if depth == 0 || parent.0 {
                self.bin(ctx::CBF_CHROMA + usize::from(depth), cbf.0);
            }
            if depth == 0 || parent.1 {
                self.bin(ctx::CBF_CHROMA + usize::from(depth), cbf.1);
            }
        }
        let chroma = match tree {
            TNode::Split(blocks, chroma) => {
                for block in blocks {
                    self.write_tree(block, log2_size - 1, depth + 1, cbf, intra_split);
                }
                chroma
            }
            TNode::Leaf(luma, chroma) => {
                self.bin(ctx::CBF_LUMA + usize::from(depth == 0), luma.coded());
                if luma.coded() {
                    self.residual_coding(luma);
                }
                chroma
            }
        };
        // Chroma of 4×4 luma blocks comes after the last of them.
        for tb in chroma.iter().flatten().filter(|tb| tb.coded()) {
            self.residual_coding(tb);
        }
    }

    /// `residual_coding()` (§7.3.8.11) of the levels in `self.coeffs[c]`, which has at least one
    /// non-zero level: the mirror of the decoder's parsing.
    fn residual_coding(&mut self, tb: &Tb) {
        let (coeffs, c, log2_size, intra_mode) = (&tb.levels[..], tb.c, tb.log2_size, tb.mode);
        if TRANSFORM_SKIP && log2_size == 2 {
            let ctx = if c == 0 {
                ctx::TRANSFORM_SKIP_FLAG_LUMA
            } else {
                ctx::TRANSFORM_SKIP_FLAG_CHROMA
            };
            self.bin(ctx, tb.transform_skip);
        }
        let luma = c == 0;
        let scan = scan_type(log2_size, luma, intra_mode);
        let log2_sb = log2_size - 2;
        let sb_scan = scan_order(log2_sb, scan);
        let positions = scan_positions(log2_size, scan);
        let level =
            |sb: usize, pos: usize| -> i32 { coeffs[usize::from(positions[sb * 16 + pos].0)] };

        // Last significant coefficient, in scan order.
        let last_index = positions
            .iter()
            .rposition(|&(i, _, _)| coeffs[usize::from(i)] != 0)
            .expect("the block has a non-zero level");
        let (last_sb, last_pos) = (last_index / 16, last_index % 16);
        let (_, x, y) = positions[last_index];
        let mut last = [u32::from(x), u32::from(y)];
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
                let sig_ctxs = sig_ctx_table(log2_size, scan, luma, prev_csbf, xs + ys == 0);
                let start = if i == last_sb {
                    last_pos as i32 - 1
                } else {
                    15
                };
                for n in (0..=start).rev() {
                    let n = n as usize;
                    let v = level(i, n);
                    if n > 0 || !infer_sb_dc {
                        let inc = usize::from(sig_ctxs[n]);
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
    if log2_size >= LOG2_MAX_TB {
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

/// Weight of chroma distortion against luma in rate-distortion decisions: chroma errors weigh
/// more in RGB than their share of the samples (measured on RGB PSNR, photos and graphics).
const CHROMA_WEIGHT: f64 = 3.0;

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
        static KINDS: std::cell::RefCell<[usize; 4]> = const { std::cell::RefCell::new([0; 4]) };
    }

    #[test]
    fn every_kind_of_coding_unit_round_trips() {
        // The decoder reproduces the reconstruction (checked in `round_trip`) whatever the
        // encoder chose: make sure it chose every kind of coding unit, and transform skip.
        KINDS.with(|k| *k.borrow_mut() = [0; 4]);
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
