//! Encoding of intra and inter pictures.
//!
//! The encoder reconstructs the picture with the decoder's own [`Reconstructor`], fed with the
//! same blocks the decoder will parse: prediction and reconstruction are therefore identical on
//! both sides by construction. Decisions are made by rate-distortion cost
//! (`distortion + λ · bits`), with the bits counted by the CABAC encoder itself:
//!
//! - coding quadtree: each split is tried against coding the whole block (64 to 8);
//! - luma prediction modes: a coarse search over the 35 modes by SATD, refined around the best,
//!   then the best few coded for real (five per block, 4×4 blocks included);
//! - chroma mode: the two best by SATD coded for real;
//! - transform tree: split up to twice; 4×4 blocks may skip the transform;
//! - levels: RDOQ (including coding nothing), sign data hiding; then SAO on the deblocked picture.
//!
//! In a P picture, a coding unit is also tried as inter: skipped or merge-predicted with a
//! residual, with a searched motion vector (AMVP), or as a partition (2N×N, N×2N or an
//! asymmetric pair, AMP) that follows two motions with one set of headers.
//!
//! Lossless pictures bypass the transform and quantization: prediction still applies, but the
//! levels are the raw residual samples, and the in-loop filters are off.

use heifer_hevc_dec::contexts::{Contexts, ctx};
use heifer_hevc_dec::inter::{Motion, MotionSource, Mv, PartMode, PuGeom, SliceRefs};
use heifer_hevc_dec::nal::NalUnit;
use heifer_hevc_dec::params::{ParameterSets, Sps};
use heifer_hevc_dec::recon::FilterState;
use heifer_hevc_dec::recon::Frame;
use heifer_hevc_dec::recon::{IntraReferences, Reconstructor};
use heifer_hevc_dec::scan::{ScanType, scan_order};
use heifer_hevc_dec::slice::SliceHeader;
use heifer_hevc_dec::syntax::{CodingUnit, PredictionUnit, SaoParams, Sink, TransformBlock};

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

/// Half the side of the window searched exhaustively around zero motion, in quarter samples
/// (8 samples).
const SEARCH_RANGE: i16 = 32;

/// `MaxNumMergeCand` of P pictures (`five_minus_max_num_merge_cand` 0).
const MAX_MERGE_CANDIDATES: usize = 5;

/// Whether any block of a transform tree has a residual.
fn tree_coded(tree: &TNode) -> bool {
    match tree {
        TNode::Leaf(luma, chroma) => luma.coded() || chroma.iter().flatten().any(Tb::coded),
        TNode::Split(blocks, _) => blocks.iter().any(tree_coded),
    }
}

/// Weighted sum of squared errors of the blocks of a transform tree.
fn tree_sse(tree: &TNode) -> f64 {
    match tree {
        TNode::Leaf(luma, chroma) => {
            luma.sse + chroma.iter().flatten().map(|tb| tb.sse).sum::<f64>()
        }
        TNode::Split(blocks, _) => blocks.iter().map(tree_sse).sum(),
    }
}

/// Approximate bits of a motion vector difference (quarter samples), for the motion search.
fn mvd_bits(mvd: [i32; 2]) -> f64 {
    mvd.iter()
        .map(|&v| {
            let a = v.unsigned_abs();
            if a == 0 {
                1.0
            } else {
                2.0 * f64::from(32 - a.leading_zeros()) + 1.0
            }
        })
        .sum()
}
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
    /// Chroma planes: `ceil(width / 2) × ceil(height / 2)` each in 4:2:0 (see
    /// [`CodingOptions::chroma_format`]), `width × height` in 4:4:4; `None` for monochrome
    /// pictures (e.g. alpha planes).
    pub chroma: Option<(&'a [u8], &'a [u8])>,
}

/// Planes of up to 10 bits per sample, for [`encode_picture16`]: same layout as [`Planes`].
#[derive(Debug, Clone, Copy)]
pub struct Planes16<'a> {
    /// Luma, `width × height`, row-major.
    pub y: &'a [u16],
    /// Chroma planes, of the same size as `y` in 4:4:4 and half its size in 4:2:0; `None` for
    /// monochrome pictures.
    pub chroma: Option<(&'a [u16], &'a [u16])>,
    /// Bits per sample: 8 (Main profile) to 10 (Main 10). Samples must fit.
    pub bit_depth: u8,
}

/// Chroma format of the planes given to [`encode_picture_with`] and [`encode_picture16_with`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChromaFormat {
    /// One plane, e.g. an alpha channel.
    Monochrome,
    /// 4:2:0: chroma at half width and height.
    Chroma420,
    /// 4:2:2: chroma at half width and full height, coded with the Range Extensions profile.
    Chroma422,
    /// 4:4:4: chroma at full resolution, coded with the Range Extensions profile.
    Chroma444,
}

impl ChromaFormat {
    /// `chroma_format_idc`.
    pub(crate) fn idc(self) -> u8 {
        match self {
            Self::Monochrome => 0,
            Self::Chroma420 => 1,
            Self::Chroma422 => 2,
            Self::Chroma444 => 3,
        }
    }
}

/// Coding options of one picture, for [`encode_picture_with`] and [`encode_picture16_with`].
///
/// [`encode_picture`] and [`encode_picture16`] use the format implied by their planes (4:2:0
/// with chroma, monochrome without) and lossy coding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodingOptions {
    /// Format of the chroma planes; [`ChromaFormat::Monochrome`] requires `planes.chroma` to be
    /// `None`, the other formats require it.
    pub chroma_format: ChromaFormat,
    /// Code the picture losslessly: every coding unit bypasses the transform and quantization,
    /// and the deblocking filter and SAO are off. Decoding the picture then reproduces the input
    /// samples exactly. Only 4:4:4 can faithfully represent RGB, with `matrix_coeffs = 0`.
    pub lossless: bool,
}

impl Default for CodingOptions {
    fn default() -> Self {
        Self {
            chroma_format: ChromaFormat::Chroma420,
            lossless: false,
        }
    }
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
    /// `chroma_format_idc`: 0, 1 or 3.
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

/// Encodes one picture as an IDR frame, with the chroma format and lossless mode of `options`
/// (for 4:4:4 or lossless coding; [`encode_picture`] covers 4:2:0 and monochrome).
pub fn encode_picture_with(
    width: u32,
    height: u32,
    planes: Planes<'_>,
    options: &CodingOptions,
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
    encode_picture16_with(width, height, planes, options, config)
}

/// Encodes one picture of up to 10 bits per sample as an IDR frame (Main 10 profile above 8
/// bits). 10-bit coding avoids the banding of smooth gradients, e.g. in skies.
pub fn encode_picture16(
    width: u32,
    height: u32,
    planes: Planes16<'_>,
    config: &EncoderConfig,
) -> Result<EncodedPicture, Error> {
    let options = CodingOptions {
        chroma_format: if planes.chroma.is_some() {
            ChromaFormat::Chroma420
        } else {
            ChromaFormat::Monochrome
        },
        lossless: false,
    };
    encode_picture16_with(width, height, planes, &options, config)
}

/// Encodes one picture of up to 10 bits per sample as an IDR frame, with the chroma format and
/// lossless mode of `options`. 4:4:4 streams use the Format Range Extensions profile (4);
/// lossless streams code every coding unit with `cu_transquant_bypass_flag` and disable the
/// in-loop filters, so that decoding reproduces the samples exactly.
pub fn encode_picture16_with(
    width: u32,
    height: u32,
    planes: Planes16<'_>,
    options: &CodingOptions,
    config: &EncoderConfig,
) -> Result<EncodedPicture, Error> {
    encode_with_reconstruction(width, height, planes, options, config).map(|(picture, ..)| picture)
}

/// Encodes the pictures of an image sequence in order: an IDR picture every
/// `keyframe_interval` pictures, and P pictures predicted from the previous picture in
/// between (skipped blocks, merge candidates with a residual, motion vectors to a quarter
/// sample, residuals, partitioned prediction blocks 2N×N, N×2N or an asymmetric pair (AMP), or
/// intra blocks, whichever costs least). Every picture of the sequence shares the same
/// parameter sets.
///
/// ```
/// use heifer_hevc_enc::encoder::{CodingOptions, EncoderConfig, Planes16, SequenceEncoder};
///
/// let (w, h) = (64u32, 48u32);
/// let mut encoder = SequenceEncoder::new(w, h, CodingOptions::default(), EncoderConfig::default(), 30);
/// for n in 0..3u16 {
///     let y = vec![100 + 10 * n; (w * h) as usize];
///     let c = vec![128u16; (w * h / 4) as usize];
///     let planes = Planes16 { y: &y, chroma: Some((&c, &c)), bit_depth: 8 };
///     let (picture, keyframe) = encoder.encode(planes).unwrap();
///     assert_eq!(keyframe, n == 0);
///     assert_eq!(picture.slices.len(), 1);
/// }
/// ```
#[derive(Debug)]
pub struct SequenceEncoder {
    width: u32,
    height: u32,
    options: CodingOptions,
    config: EncoderConfig,
    keyframe_interval: usize,
    /// Pictures encoded so far.
    count: usize,
    /// POC of the next picture.
    poc: u32,
    /// The filtered reconstruction of the previous picture.
    reference: Option<Frame>,
}

impl SequenceEncoder {
    /// An encoder of `width`×`height` pictures, with an IDR picture every
    /// `keyframe_interval` pictures (at least 1: 1 codes every picture as an IDR picture).
    pub fn new(
        width: u32,
        height: u32,
        options: CodingOptions,
        config: EncoderConfig,
        keyframe_interval: usize,
    ) -> Self {
        Self {
            width,
            height,
            options,
            config,
            keyframe_interval: keyframe_interval.max(1),
            count: 0,
            poc: 0,
            reference: None,
        }
    }

    /// Encodes the next picture. Returns it, and whether it is an IDR picture (a sync sample:
    /// decodable without the pictures before it).
    pub fn encode(&mut self, planes: Planes16<'_>) -> Result<(EncodedPicture, bool), Error> {
        let keyframe = self.count % self.keyframe_interval == 0;
        let kind = match &self.reference {
            Some(reference) if !keyframe => Kind::P {
                poc: self.poc,
                reference,
            },
            _ => Kind::SequenceIdr,
        };
        let (picture, recon, _) = encode_kind(
            self.width,
            self.height,
            planes,
            &self.options,
            &self.config,
            kind,
        )?;
        self.poc = if keyframe { 1 } else { self.poc + 1 };
        self.reference = Some(recon);
        self.count += 1;
        Ok((picture, keyframe))
    }
}

/// Like [`encode_picture`], also returning the encoder's reconstruction of the picture (coded
/// size, before the in-loop filters) and how many coding units of each [`CuKind`] it chose.
fn encode_with_reconstruction(
    width: u32,
    height: u32,
    planes: Planes16<'_>,
    options: &CodingOptions,
    config: &EncoderConfig,
) -> Result<(EncodedPicture, Frame, [usize; 4]), Error> {
    encode_kind(width, height, planes, options, config, Kind::Still)
}

/// What to encode: a still image, or a picture of an image sequence.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Kind<'r> {
    /// A still image: an IDR picture, in a stream without reference pictures.
    Still,
    /// The first picture of a group of an image sequence: an IDR picture, in a stream whose
    /// SPS keeps one reference picture.
    SequenceIdr,
    /// A P picture of POC `poc`, predicted from `reference`, the filtered reconstruction of
    /// the previous picture (POC `poc - 1`).
    P { poc: u32, reference: &'r Frame },
}

/// Encodes a picture of the given [`Kind`]. Returns the encoded picture, the reconstruction
/// (before the in-loop filters for a still image; filtered, as decoders keep it for reference,
/// for a picture of a sequence) and how many coding units of each [`CuKind`] were chosen.
pub(crate) fn encode_kind(
    width: u32,
    height: u32,
    planes: Planes16<'_>,
    options: &CodingOptions,
    config: &EncoderConfig,
    kind: Kind<'_>,
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
    let format = options.chroma_format;
    let (cw, ch) = match format {
        ChromaFormat::Monochrome => (0, 0),
        ChromaFormat::Chroma420 => (w.div_ceil(2), h.div_ceil(2)),
        ChromaFormat::Chroma422 => (w.div_ceil(2), h),
        ChromaFormat::Chroma444 => (w, h),
    };
    match (planes.chroma, format) {
        (
            Some((cb, cr)),
            ChromaFormat::Chroma420 | ChromaFormat::Chroma422 | ChromaFormat::Chroma444,
        ) => {
            if cb.len() != cw * ch || cr.len() != cw * ch {
                return Err(Error::InvalidInput(
                    "chroma plane size does not match the picture",
                ));
            }
        }
        (None, ChromaFormat::Monochrome) => {}
        (Some(_), ChromaFormat::Monochrome) => {
            return Err(Error::InvalidInput(
                "monochrome pictures have no chroma planes",
            ));
        }
        (None, _) => {
            return Err(Error::InvalidInput(
                "chroma planes are missing from the picture",
            ));
        }
    }

    // Conformance windows crop by whole chroma samples: 2 luma samples horizontally in 4:2:0
    // and 4:2:2, vertically in 4:2:0. Odd sizes there are rounded up (HEIF crops the rest with a
    // `clap` property).
    let unit_x = match format {
        ChromaFormat::Chroma420 | ChromaFormat::Chroma422 => 2,
        _ => 1,
    };
    let unit_y = if format == ChromaFormat::Chroma420 {
        2
    } else {
        1
    };
    let params = StreamParams {
        width: width.next_multiple_of(8),
        height: height.next_multiple_of(8),
        output_width: width.next_multiple_of(unit_x),
        output_height: height.next_multiple_of(unit_y),
        chroma_format: format.idc(),
        bit_depth: planes.bit_depth,
        qp: i32::from(config.qp),
        full_range: config.full_range,
        colour: config.colour,
        lossless: options.lossless,
        inter: !matches!(kind, Kind::Still),
    };
    let parameter_sets = vec![params.vps(), params.sps(), params.pps()];

    // Parse what was written with the decoder: the reconstruction uses its structures.
    let mut sets = ParameterSets::default();
    for nal in &parameter_sets {
        sets.add(&NalUnit::parse(nal)?)?;
    }
    let (header_rbsp, nal_type) = match kind {
        Kind::P { poc, .. } => (params.p_slice_header(poc), NalType::TrailR),
        _ => (params.slice_header(), NalType::IdrWRadl),
    };
    let header_nal = nal_unit(nal_type, &header_rbsp);
    let header = SliceHeader::parse(
        &header_rbsp,
        &NalUnit::parse(&header_nal)?.header,
        &sets,
        None,
    )?;
    let (pps, sps) = sets.get(0)?;

    let mut enc = Encoder::new(sps, pps, &params, (w, h), planes, &header);
    if let Kind::P { poc, reference } = kind {
        if (reference.widths[0], reference.heights[0]) != (params.width, params.height) {
            return Err(Error::InvalidInput("reference picture of another size"));
        }
        let previous = i32::try_from(poc).unwrap_or(i32::MAX) - 1;
        enc.recon.set_references(
            [vec![(reference, previous, false)], Vec::new()],
            [None, None],
        );
        enc.reference = Some(reference);
        enc.poc = previous + 1;
        enc.ref_pocs = vec![previous];
        enc.ref_long_term = vec![false];
    }
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

    let enc_init_type = Contexts::init_type(header.slice_type, header.cabac_init_flag);
    // Slice data: the SAO parameters of each block, then its bins. SAO has its own contexts,
    // so the bins decided without it stay valid. Lossless streams disable SAO in the SPS.
    let bit_depth = planes.bit_depth;
    let chroma = format != ChromaFormat::Monochrome;
    let write = |sao: &[(CtbSao, SaoParams)]| -> Vec<u8> {
        let mut ctx = Contexts::new_with_type(params.qp, enc_init_type);
        let mut coder = ArithmeticEncoder::new();
        for (i, bins) in blocks.iter().enumerate() {
            if !sao.is_empty() {
                let (cx, cy) = (i as u32 % ctbs_x, i as u32 / ctbs_x);
                sao::write(
                    &mut coder,
                    &mut ctx,
                    &sao[i].0,
                    (cx > 0, cy > 0),
                    chroma,
                    bit_depth,
                );
            }
            replay(bins, &mut ctx, &mut coder);
            let last = i + 1 == blocks.len();
            coder.encode_terminate(u8::from(last)); // end_of_slice_segment_flag
        }
        coder.finish()
    };
    // SAO is chosen on the picture as the decoder reconstructs it before SAO: the deblocked
    // reconstruction (its block edges are those of the chosen codings).
    let decisions = if options.lossless {
        Vec::new()
    } else {
        let deblocked = enc.recon.deblocked();
        let src = [&enc.src[0], &enc.src[1], &enc.src[2]];
        sao::decide(
            &deblocked,
            src.map(|p| &p.samples[..]),
            src.map(|p| p.width),
            chroma,
            ctb as usize,
            bit_depth,
            enc.lambda,
        )
    };
    let mut rbsp = header_rbsp;
    rbsp.extend_from_slice(&write(&decisions));
    // The SAO parameters go to the reconstruction too, so that its filtered picture, the
    // reference of the next picture of a sequence, is the decoder's.
    for (i, (_, params)) in decisions.iter().enumerate() {
        let (cx, cy) = (i as u32 % ctbs_x, i as u32 / ctbs_x);
        enc.recon.sao(cx, cy, params);
    }

    let picture = EncodedPicture {
        parameter_sets,
        slices: vec![nal_unit(nal_type, &rbsp)],
        profile_idc: params.profile_idc(),
        level_idc: params.level_idc(),
        chroma_format: params.chroma_format,
        width: params.output_width,
        height: params.output_height,
    };
    let kinds = enc.kinds;
    let skip_filters = matches!(kind, Kind::Still);
    Ok((picture, enc.recon.finish(skip_filters), kinds))
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

/// How one prediction block of a partitioned coding unit is coded.
#[derive(Clone, Copy)]
enum PuCoding {
    /// Merge mode: the motion of merge candidate `idx`.
    Merge { idx: usize, motion: Motion },
    /// AMVP: an explicit vector, coded against the predictor chosen by `flag`.
    Amvp { mv: Mv, flag: bool, mvd: [i32; 2] },
}

impl PuCoding {
    /// The motion this coding predicts the block with.
    fn motion(self) -> Motion {
        match self {
            Self::Merge { motion, .. } => motion,
            Self::Amvp { mv, .. } => Motion::l0(mv, 0),
        }
    }
}

/// A transform tree: one transform unit, or four smaller trees.
enum TNode {
    /// The luma block and, from 8×8 luma up, the chroma blocks.
    Leaf(Tb, Option<Vec<Tb>>),
    /// Four quarters and, when they are 4×4 luma blocks, the chroma blocks they share.
    Split(Vec<TNode>, Option<Vec<Tb>>),
}

impl TNode {
    fn chroma(&self) -> &Option<Vec<Tb>> {
        match self {
            Self::Leaf(_, chroma) | Self::Split(_, chroma) => chroma,
        }
    }

    /// Whether Cb (`k` = 0) or Cr (1) has residual anywhere in the tree.
    fn chroma_cbf(&self, k: usize) -> bool {
        self.chroma()
            .as_ref()
            .is_some_and(|c| c.iter().any(|tb| tb.c == k + 1 && tb.coded()))
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
type SnapshotBuffers = ([Vec<u16>; 3], Vec<u8>, Vec<u8>, Vec<u8>);

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
    /// Block edge flags of the deblocking filter.
    edges: Vec<u8>,
    /// In P pictures: the deblocking filter's state of the block (intra and coded flags,
    /// motion), and which of its 4×4 blocks are skipped.
    inter: Option<(FilterState, Vec<bool>, Vec<Motion>)>,
}

struct Encoder<'a> {
    recon: Reconstructor<'a>,
    /// Whether the picture is a P picture: coding units start with `cu_skip_flag` and
    /// `pred_mode_flag`.
    p_slice: bool,
    /// Bins of the trials, with their estimated rates.
    log: BinLog,
    ctx: Contexts,
    src: [Plane; 3],
    chroma: bool,
    /// Horizontal chroma subsampling factor: 2 in 4:2:0 and 4:2:2, 1 in 4:4:4 (and for
    /// monochrome).
    csx: usize,
    /// Vertical chroma subsampling factor: 2 in 4:2:0, 1 otherwise.
    csy: usize,
    /// Whether coding units bypass the transform and quantization.
    lossless: bool,
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
    /// Whether each 4×4 block belongs to a skipped coding unit (P pictures).
    skipped: Vec<bool>,
    /// Motion of each 4×4 block (P pictures), for the candidates of the following blocks.
    motion: Vec<Motion>,
    /// Skipped and other inter coding units chosen, for tests.
    skipped_cus: usize,
    inter_cus: usize,
    /// Whether the transform blocks being coded belong to an inter coding unit: DCT only,
    /// prediction taken from the picture.
    inter_block: bool,
    /// The reference picture of a P picture (the previous picture's filtered reconstruction),
    /// the POCs of the current and reference pictures, and `Log2ParMrgLevel`.
    reference: Option<&'a Frame>,
    poc: i32,
    ref_pocs: Vec<i32>,
    ref_long_term: Vec<bool>,
    par_mrg_level: u8,
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
        let csx = if matches!(params.chroma_format, 1 | 2) {
            2
        } else {
            1
        };
        let csy = if params.chroma_format == 1 { 2 } else { 1 };
        let y = Plane::padded(planes.y, w, h, pw, ph);
        let (cb, cr) = match planes.chroma {
            Some((cb, cr)) => {
                let (cw, ch) = (w.div_ceil(csx), h.div_ceil(csy));
                (
                    Plane::padded(cb, cw, ch, pw / csx, ph / csy),
                    Plane::padded(cr, cw, ch, pw / csx, ph / csy),
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
        // The 4:2:0 table maps QPs only for 4:2:0 (§8.6.1).
        let qp_c = if params.chroma_format != 1 {
            qp.min(51)
        } else {
            chroma_qp(qp)
        };
        Self {
            recon,
            log: BinLog::new(),
            ctx: Contexts::new_with_type(
                qp,
                Contexts::init_type(header.slice_type, header.cabac_init_flag),
            ),
            p_slice: header.slice_type == heifer_hevc_dec::slice::SliceType::P,
            src: [y, cb, cr],
            chroma: planes.chroma.is_some(),
            csx,
            csy,
            lossless: params.lossless,
            width: params.width,
            height: params.height,
            qp,
            qp_luma: qp + bd_offset,
            qp_c: qp_c + bd_offset,
            bit_depth: planes.bit_depth,
            lambda,
            sqrt_lambda: lambda.sqrt(),
            depth: vec![0; grid_w * (ph / 4)],
            skipped: vec![false; grid_w * (ph / 4)],
            motion: vec![Motion::NONE; grid_w * (ph / 4)],
            skipped_cus: 0,
            inter_cus: 0,
            inter_block: false,
            reference: None,
            poc: 0,
            ref_pocs: Vec::new(),
            ref_long_term: Vec::new(),
            par_mrg_level: pps.log2_parallel_merge_level,
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

    /// Components and their regions (in component samples: x, y, width, height) covered by a
    /// luma block.
    fn regions(
        &self,
        x0: u32,
        y0: u32,
        size: u32,
    ) -> impl Iterator<Item = (usize, u32, u32, u32, u32)> + use<> {
        let n = if self.chroma { 3 } else { 1 };
        let (sx, sy) = (self.csx as u32, self.csy as u32);
        [
            (0, x0, y0, size, size),
            (1, x0 / sx, y0 / sy, size / sx, size / sy),
            (2, x0 / sx, y0 / sy, size / sx, size / sy),
        ]
        .into_iter()
        .take(n)
    }

    /// Position of a luma sample in the chroma planes.
    fn chroma_pos(&self, x: u32, y: u32) -> (u32, u32) {
        (x / self.csx as u32, y / self.csy as u32)
    }

    /// `log2` of the size of the chroma block that covers a luma transform block.
    fn chroma_log2(&self, log2_size: u8) -> u8 {
        log2_size - self.csx.trailing_zeros() as u8
    }

    /// Largest `log2` of a chroma transform block (32×32, or 16×16 in 4:2:0).
    fn max_chroma_log2(&self) -> u8 {
        LOG2_MAX_TB - self.csx.trailing_zeros() as u8
    }

    /// Weight of chroma distortion against luma in rate-distortion decisions. 4:4:4 chroma
    /// samples are as numerous as luma samples, so they weigh as much.
    fn chroma_weight(&self) -> f64 {
        if self.csx == 1 { 1.0 } else { CHROMA_WEIGHT }
    }

    /// Saves the state of a block, to try another coding of it and come back. The buffers come
    /// from the ones given back with [`recycle`](Self::recycle).
    fn snapshot(&mut self, x0: u32, y0: u32, size: u32, coded: bool, log: LogPoint) -> Snapshot {
        let (mut samples, mut depth, mut modes, mut edges) = self.pool.pop().unwrap_or_default();
        for v in &mut samples {
            v.clear();
        }
        depth.clear();
        modes.clear();
        edges.clear();
        self.recon.save_edges(x0, y0, size, size, &mut edges);
        for (c, x, y, w, h) in self.regions(x0, y0, size) {
            let stride = self.recon.frame.widths[c] as usize;
            let plane = &self.recon.frame.planes[c];
            for row in y..y + h {
                let start = row as usize * stride + x as usize;
                samples[c].extend_from_slice(&plane[start..start + w as usize]);
            }
        }
        for y in (y0..y0 + size).step_by(4) {
            for x in (x0..x0 + size).step_by(4) {
                let i = self.grid(x, y);
                depth.push(self.depth[i]);
                modes.push(self.modes[i]);
            }
        }
        let inter = self.p_slice.then(|| {
            let (mut skipped, mut motion) = (Vec::new(), Vec::new());
            for y in (y0..y0 + size).step_by(4) {
                for x in (x0..x0 + size).step_by(4) {
                    let i = self.grid(x, y);
                    skipped.push(self.skipped[i]);
                    motion.push(self.motion[i]);
                }
            }
            (
                self.recon.save_filter_state(x0, y0, size, size),
                skipped,
                motion,
            )
        });
        Snapshot {
            coded,
            log,
            ctx: self.ctx,
            samples,
            depth,
            modes,
            edges,
            inter,
        }
    }

    /// Gives back the buffers of a snapshot no longer needed.
    fn recycle(&mut self, s: Snapshot) {
        self.log.recycle(s.log);
        self.pool.push((s.samples, s.depth, s.modes, s.edges));
    }

    fn restore(&mut self, x0: u32, y0: u32, size: u32, s: &Snapshot) {
        // Intra prediction must see the block as the decoder will: coded or not yet.
        self.recon.set_reconstructed(x0, y0, size, size, s.coded);
        self.log.restore(&s.log);
        self.ctx = s.ctx;
        self.recon.restore_edges(x0, y0, size, size, &s.edges);
        for (c, x, y, w, h) in self.regions(x0, y0, size) {
            let stride = self.recon.frame.widths[c] as usize;
            let plane = &mut self.recon.frame.planes[c];
            for (i, row) in (y..y + h).enumerate() {
                let start = row as usize * stride + x as usize;
                let w = w as usize;
                plane[start..start + w].copy_from_slice(&s.samples[c][i * w..(i + 1) * w]);
            }
        }
        let mut k = 0;
        for y in (y0..y0 + size).step_by(4) {
            for x in (x0..x0 + size).step_by(4) {
                let i = self.grid(x, y);
                self.depth[i] = s.depth[k];
                self.modes[i] = s.modes[k];
                if let Some((_, skipped, motion)) = &s.inter {
                    self.skipped[i] = skipped[k];
                    self.motion[i] = motion[k];
                }
                k += 1;
            }
        }
        if let Some((filter, ..)) = &s.inter {
            self.recon.restore_filter_state(x0, y0, size, size, filter);
        }
    }

    /// `ctxInc` of `cu_skip_flag` (§9.3.4.2.2): skipped neighbours on the left and above.
    fn skip_ctx(&self, x0: u32, y0: u32) -> usize {
        usize::from(x0 > 0 && self.skipped[self.grid(x0 - 1, y0)])
            + usize::from(y0 > 0 && self.skipped[self.grid(x0, y0 - 1)])
    }

    /// The reference pictures of the current P picture, for motion vector prediction.
    fn slice_refs(&self) -> SliceRefs<'_> {
        SliceRefs {
            poc: self.poc,
            lists: [&self.ref_pocs, &[]],
            long_term: [&self.ref_long_term, &[]],
            col: None,
            collocated_from_l0: true,
            no_backward_pred: true,
            pic_size: (self.width, self.height),
            log2_ctb: LOG2_CTB,
        }
    }

    /// The single prediction block (2N×2N) of a coding unit.
    fn geom(x0: u32, y0: u32, size: u32) -> PuGeom {
        PuGeom {
            x_cb: x0,
            y_cb: y0,
            n_cb: size,
            x: x0,
            y: y0,
            w: size,
            h: size,
            part_idx: 0,
            part_mode: PartMode::Part2Nx2N,
        }
    }

    /// Prediction block `part_idx` of a coding unit split by `part_mode`.
    fn pu_geom(x0: u32, y0: u32, size: u32, part_mode: PartMode, part_idx: u8) -> PuGeom {
        let (x, y, w, h) = part_mode.blocks(x0, y0, size)[usize::from(part_idx)];
        PuGeom {
            x_cb: x0,
            y_cb: y0,
            n_cb: size,
            x,
            y,
            w,
            h,
            part_idx,
            part_mode,
        }
    }

    /// The distinct merge candidates of a prediction block, with their `merge_idx`.
    fn merge_candidates_of(&self, g: &PuGeom) -> Vec<(usize, Motion)> {
        let refs = self.slice_refs();
        let mut out: Vec<(usize, Motion)> = Vec::new();
        for idx in 0..MAX_MERGE_CANDIDATES {
            let m = heifer_hevc_dec::inter::merge(
                self,
                &refs,
                g,
                idx,
                MAX_MERGE_CANDIDATES,
                self.par_mrg_level,
                [1, 0],
            );
            if !out.iter().any(|(_, o)| *o == m) {
                out.push((idx, m));
            }
        }
        out
    }

    /// Distortion of the prediction currently in the picture, over one prediction block.
    fn pu_sse(&self, g: &PuGeom) -> f64 {
        let mut d = self.block_sse_rect(0, g.x, g.y, g.w as usize, g.h as usize) as f64;
        if self.chroma {
            let (xc, yc) = self.chroma_pos(g.x, g.y);
            let (cw, ch) = (g.w as usize / self.csx, g.h as usize / self.csy);
            let mut dc = 0i64;
            for c in 1..3 {
                dc += self.block_sse_rect(c, xc, yc, cw, ch);
            }
            d += self.chroma_weight() * dc as f64;
        }
        d
    }

    /// The distinct merge candidates of a 2N×2N coding unit, with their `merge_idx`.
    fn merge_candidates(&self, x0: u32, y0: u32, size: u32) -> Vec<(usize, Motion)> {
        self.merge_candidates_of(&Self::geom(x0, y0, size))
    }

    /// Codes the syntax of one prediction block's merge or motion, and reconstructs it.
    fn code_pu(&mut self, g: &PuGeom, option: PuCoding) {
        self.write_pu_syntax(option);
        let motion = option.motion();
        self.recon.prediction_unit(&PredictionUnit {
            x: g.x,
            y: g.y,
            w: g.w,
            h: g.h,
            motion,
        });
        for y in (g.y..g.y + g.h).step_by(4) {
            for x in (g.x..g.x + g.w).step_by(4) {
                let i = self.grid(x, y);
                self.motion[i] = motion;
            }
        }
    }

    /// `prediction_unit()` of one block of a partitioned coding unit: `merge_flag` then either
    /// `merge_idx` or the motion vector difference and predictor flag (§7.3.8.6).
    fn write_pu_syntax(&mut self, option: PuCoding) {
        match option {
            PuCoding::Merge { idx, .. } => {
                self.bin(ctx::MERGE_FLAG, true);
                self.merge_idx(idx);
            }
            PuCoding::Amvp { flag, mvd, .. } => {
                self.bin(ctx::MERGE_FLAG, false);
                self.mvd_coding(mvd);
                self.bin(ctx::MVP_LX_FLAG, flag);
            }
        }
    }

    /// The bits of a prediction block's syntax, coded and discarded.
    fn pu_syntax_bits(&mut self, option: PuCoding) -> f64 {
        let (point, contexts) = (self.log.checkpoint(), self.ctx);
        let start = self.log.bit_count();
        self.write_pu_syntax(option);
        let bits = self.log.bit_count() - start;
        self.log.restore(&point);
        self.ctx = contexts;
        bits
    }

    /// Picks how to code one prediction block of a partitioned coding unit: the cheapest of its
    /// merge candidates and a searched motion vector, by distortion and syntax bits. The block
    /// is predicted for each candidate, so its neighbours see it afterwards.
    fn choose_pu(&mut self, g: &PuGeom) -> PuCoding {
        let mut best: Option<(f64, PuCoding)> = None;
        let candidates = self.merge_candidates_of(g);
        let mut consider = |enc: &mut Self, motion: Motion, option: PuCoding| {
            enc.recon.prediction_unit(&PredictionUnit {
                x: g.x,
                y: g.y,
                w: g.w,
                h: g.h,
                motion,
            });
            let j = enc.pu_sse(g) + enc.lambda * enc.pu_syntax_bits(option);
            if best.as_ref().is_none_or(|b| j < b.0) {
                best = Some((j, option));
            }
        };
        for &(idx, motion) in &candidates {
            consider(self, motion, PuCoding::Merge { idx, motion });
        }
        if self.reference.is_some() {
            let predictors = heifer_hevc_dec::inter::amvp(self, &self.slice_refs(), g, 0, 0);
            let mut starts = vec![[0, 0], predictors[0], predictors[1]];
            starts.extend(candidates.iter().map(|(_, m)| m.mv[0]));
            let mv = self.search_motion_rect(g.x, g.y, g.w, g.h, &starts, predictors[0]);
            let mvd_of = |p: Mv| {
                [
                    i32::from(mv[0]) - i32::from(p[0]),
                    i32::from(mv[1]) - i32::from(p[1]),
                ]
            };
            let flag =
                usize::from(mvd_bits(mvd_of(predictors[1])) < mvd_bits(mvd_of(predictors[0])));
            let option = PuCoding::Amvp {
                mv,
                flag: flag == 1,
                mvd: mvd_of(predictors[flag]),
            };
            consider(self, Motion::l0(mv, 0), option);
        }
        best.expect("a merge candidate or a motion vector was tried")
            .1
    }

    /// `part_mode` of a partitioned inter coding unit (§7.3.8.5, Table 9-43), with AMP enabled:
    /// the 2N×2N bin is not coded (a partition is already chosen), then horizontal/vertical,
    /// symmetric/asymmetric, and for an asymmetric pair a bypass bin picking the near (up or
    /// left) share or the far one. Only coded above the smallest coding unit, where the third
    /// and later bins appear, so the minimum-size binarization (N×N) is never reached.
    fn write_part_mode(&mut self, part_mode: PartMode) {
        self.bin(ctx::PART_MODE, false);
        let horizontal = matches!(
            part_mode,
            PartMode::Part2NxN | PartMode::Part2NxnU | PartMode::Part2NxnD
        );
        self.bin(ctx::PART_MODE + 1, horizontal);
        let symmetric = matches!(part_mode, PartMode::Part2NxN | PartMode::PartNx2N);
        self.bin(ctx::PART_MODE + 3, symmetric);
        if !symmetric {
            let near = matches!(part_mode, PartMode::Part2NxnU | PartMode::PartnLx2N);
            self.log.encode_bypass_bits(u32::from(!near), 1);
        }
    }

    /// Codes a partitioned inter coding unit (2N×N, N×2N or an asymmetric partition): each
    /// prediction block picks its merge candidate or motion vector, then the unit's residual is
    /// coded once. Unlike a 2N×2N merge, `rqt_root_cbf` is not inferred and is written here.
    /// Returns the distortion, or `None` when a residual was asked for and none of it is worth
    /// coding.
    fn code_partition(
        &mut self,
        x0: u32,
        y0: u32,
        log2_size: u8,
        depth: u8,
        part_mode: PartMode,
        residual: bool,
    ) -> Option<f64> {
        let size = 1u32 << log2_size;
        let inc = self.skip_ctx(x0, y0);
        self.bin(ctx::CU_SKIP_FLAG + inc, false);
        self.bin(ctx::PRED_MODE_FLAG, false); // inter
        self.write_part_mode(part_mode);
        self.recon.coding_unit(&CodingUnit {
            x: x0,
            y: y0,
            log2_size,
            part_nxn: false,
            transquant_bypass: false,
            pcm: false,
            intra: false,
            qp_y: self.qp,
        });
        self.fill_grid(x0, y0, size, Some(depth), Some(1));

        // The prediction blocks, in decoding order: each one's motion is decided with the ones
        // before it already in the picture (they are its neighbours).
        let mut sse = 0.0;
        for part_idx in 0..2u8 {
            let g = Self::pu_geom(x0, y0, size, part_mode, part_idx);
            let option = self.choose_pu(&g);
            self.code_pu(&g, option);
            sse += self.pu_sse(&g);
        }

        if !residual {
            self.bin(ctx::NO_RESIDUAL_DATA_FLAG, false);
            return Some(sse);
        }
        self.inter_block = true;
        let tree = self.inter_tree(x0, y0, log2_size, true);
        self.inter_block = false;
        if !tree_coded(&tree) {
            return None;
        }
        self.bin(ctx::NO_RESIDUAL_DATA_FLAG, true);
        self.write_inter_tree(&tree, log2_size, 0, (false, false));
        Some(tree_sse(&tree))
    }

    /// Tells the reconstruction about an inter coding unit with `motion`, and records it in the
    /// grids; returns its distortion.
    fn reconstruct_inter(
        &mut self,
        x0: u32,
        y0: u32,
        log2_size: u8,
        depth: u8,
        motion: Motion,
        skipped: bool,
    ) -> f64 {
        let size = 1u32 << log2_size;
        self.recon.coding_unit(&CodingUnit {
            x: x0,
            y: y0,
            log2_size,
            part_nxn: false,
            transquant_bypass: false,
            pcm: false,
            intra: false,
            qp_y: self.qp,
        });
        self.recon.prediction_unit(&PredictionUnit {
            x: x0,
            y: y0,
            w: size,
            h: size,
            motion,
        });
        // Not intra: neighbours take DC as its intra mode (§8.4.2).
        self.fill_grid(x0, y0, size, Some(depth), Some(1));
        for y in (y0..y0 + size).step_by(4) {
            for x in (x0..x0 + size).step_by(4) {
                let i = self.grid(x, y);
                self.skipped[i] = skipped;
                self.motion[i] = motion;
            }
        }
        let mut d = self.block_sse(0, x0, y0, size as usize) as f64;
        if self.chroma {
            let (xc, yc) = self.chroma_pos(x0, y0);
            let (cw, ch) = (size as usize / self.csx, size as usize / self.csy);
            let mut dc = 0i64;
            for c in 1..3 {
                dc += self.block_sse_rect(c, xc, yc, cw, ch);
            }
            d += self.chroma_weight() * dc as f64;
        }
        d
    }

    /// `merge_idx` of `prediction_unit()` (§7.3.8.6): truncated unary, the first bin with a
    /// context, the others bypass.
    fn merge_idx(&mut self, merge_idx: usize) {
        self.bin(ctx::MERGE_IDX, merge_idx > 0);
        if merge_idx > 0 {
            for _ in 1..merge_idx {
                self.log.encode_bypass(1);
            }
            if merge_idx < MAX_MERGE_CANDIDATES - 1 {
                self.log.encode_bypass(0);
            }
        }
    }

    /// Codes a skipped coding unit: the motion of merge candidate `merge_idx`, no residual.
    /// Returns its distortion.
    fn code_skip(
        &mut self,
        x0: u32,
        y0: u32,
        log2_size: u8,
        depth: u8,
        merge_idx: usize,
        motion: Motion,
    ) -> f64 {
        let inc = self.skip_ctx(x0, y0);
        self.bin(ctx::CU_SKIP_FLAG + inc, true);
        self.merge_idx(merge_idx);
        self.reconstruct_inter(x0, y0, log2_size, depth, motion, true)
    }

    /// Codes a merge-predicted coding unit with a residual: the motion of merge candidate
    /// `merge_idx`, without the skip flag, so that the prediction error is corrected by a
    /// transform tree. `rqt_root_cbf` is inferred to be 1 for a 2N×2N merge (§7.3.8.7), so no
    /// flag is coded before the tree. Returns its distortion, or `None` when the prediction
    /// needs no residual (the skipped coding unit is then the same prediction, cheaper).
    fn code_merge(
        &mut self,
        x0: u32,
        y0: u32,
        log2_size: u8,
        depth: u8,
        merge_idx: usize,
        motion: Motion,
    ) -> Option<f64> {
        let inc = self.skip_ctx(x0, y0);
        self.bin(ctx::CU_SKIP_FLAG + inc, false);
        self.bin(ctx::PRED_MODE_FLAG, false); // inter
        self.bin(ctx::PART_MODE, true); // PART_2Nx2N
        self.bin(ctx::MERGE_FLAG, true);
        self.merge_idx(merge_idx);
        self.reconstruct_inter(x0, y0, log2_size, depth, motion, false);
        self.inter_block = true;
        let tree = self.inter_tree(x0, y0, log2_size, false);
        self.inter_block = false;
        if !tree_coded(&tree) {
            return None;
        }
        self.write_inter_tree(&tree, log2_size, 0, (false, false));
        Some(tree_sse(&tree))
    }

    /// Codes an inter coding unit with motion vector `mv` (AMVP, one reference picture), with
    /// a residual when `residual` and the residual has any level. Returns its distortion, or
    /// `None` when a residual was asked for and none is worth coding.
    fn code_amvp(
        &mut self,
        x0: u32,
        y0: u32,
        log2_size: u8,
        depth: u8,
        mv: Mv,
        residual: bool,
    ) -> Option<f64> {
        let size = 1u32 << log2_size;
        let candidates =
            heifer_hevc_dec::inter::amvp(self, &self.slice_refs(), &Self::geom(x0, y0, size), 0, 0);
        let mvd_of = |p: Mv| {
            [
                i32::from(mv[0]) - i32::from(p[0]),
                i32::from(mv[1]) - i32::from(p[1]),
            ]
        };
        let flag = usize::from(mvd_bits(mvd_of(candidates[1])) < mvd_bits(mvd_of(candidates[0])));
        let inc = self.skip_ctx(x0, y0);
        self.bin(ctx::CU_SKIP_FLAG + inc, false);
        self.bin(ctx::PRED_MODE_FLAG, false); // inter
        self.bin(ctx::PART_MODE, true); // PART_2Nx2N
        self.bin(ctx::MERGE_FLAG, false);
        self.mvd_coding(mvd_of(candidates[flag]));
        self.bin(ctx::MVP_LX_FLAG, flag == 1);
        let d = self.reconstruct_inter(x0, y0, log2_size, depth, Motion::l0(mv, 0), false);
        if !residual {
            self.bin(ctx::NO_RESIDUAL_DATA_FLAG, false); // rqt_root_cbf: no residual
            return Some(d);
        }
        // The residual of the prediction just written to the picture.
        self.inter_block = true;
        let tree = self.inter_tree(x0, y0, log2_size, false);
        self.inter_block = false;
        if !tree_coded(&tree) {
            return None;
        }
        self.bin(ctx::NO_RESIDUAL_DATA_FLAG, true); // rqt_root_cbf
        self.write_inter_tree(&tree, log2_size, 0, (false, false));
        Some(tree_sse(&tree))
    }

    /// The transform tree of an inter coding unit's residual: one transform block per
    /// component (`max_transform_hierarchy_depth_inter` is 0), split above 32×32 and, when
    /// `split` (a partitioned coding unit, where `interSplitFlag` infers it), once at the root.
    /// Each block is reconstructed as it is quantized.
    fn inter_tree(&mut self, x0: u32, y0: u32, log2_size: u8, split: bool) -> TNode {
        if log2_size > LOG2_MAX_TB || (split && log2_size > 2) {
            let half = 1u32 << (log2_size - 1);
            let blocks = [(0, 0), (half, 0), (0, half), (half, half)]
                .iter()
                .map(|&(dx, dy)| self.inter_tree(x0 + dx, y0 + dy, log2_size - 1, false))
                .collect();
            return TNode::Split(blocks, None);
        }
        let luma = self.inter_tb(0, x0, y0, log2_size);
        // 4×4 luma blocks (4:2:0 / 4:2:2) share their parent's chroma blocks (see
        // `write_inter_tree`); partition splits stop above them.
        let chroma = (self.chroma && (log2_size > 2 || self.csx == 1)).then(|| {
            let (xc, yc) = self.chroma_pos(x0, y0);
            let log2_c = log2_size - u8::from(self.csx == 2);
            (1..3)
                .map(|c| self.inter_tb(c, xc, yc, log2_c))
                .collect::<Vec<_>>()
        });
        TNode::Leaf(luma, chroma)
    }

    /// Quantizes and reconstructs one transform block of an inter coding unit, whose
    /// prediction is already in the picture.
    fn inter_tb(&mut self, c: usize, x: u32, y: u32, log2_size: u8) -> Tb {
        let n = 1usize << log2_size;
        let mut pred = std::mem::take(&mut self.pred);
        let stride = self.recon.frame.widths[c] as usize;
        for row in 0..n {
            let start = (y as usize + row) * stride + x as usize;
            for (p, &v) in pred[row * n..][..n]
                .iter_mut()
                .zip(&self.recon.frame.planes[c][start..start + n])
            {
                *p = i32::from(v);
            }
        }
        self.pred = pred;
        self.diff_from_prediction(c, x, y, n);
        // Mode 1 (DC) gives the diagonal scan of inter blocks.
        let levels = self.quantize(c, log2_size, 1, false);
        let sse = self.reconstruct(c, x, y, log2_size, 1, &levels, false);
        let w = if c == 0 { 1.0 } else { self.chroma_weight() };
        Tb {
            c,
            x,
            y,
            log2_size,
            mode: 1,
            levels,
            transform_skip: false,
            sse: sse as f64 * w,
        }
    }

    /// `transform_tree()` of an inter coding unit (§7.3.8.8): no `split_transform_flag`, the
    /// split above 32×32 is inferred, and the root's `cbf_luma` is inferred to be 1 without
    /// chroma residual.
    fn write_inter_tree(&mut self, tree: &TNode, log2_size: u8, depth: u8, parent: (bool, bool)) {
        let cbf = (tree.chroma_cbf(0), tree.chroma_cbf(1));
        if self.chroma && (log2_size > 2 || self.csx == 1) {
            for (coded, parent_cbf) in [(cbf.0, parent.0), (cbf.1, parent.1)] {
                if depth == 0 || parent_cbf {
                    self.bin(ctx::CBF_CHROMA + usize::from(depth), coded);
                }
            }
        }
        match tree {
            TNode::Split(blocks, _) => {
                for block in blocks {
                    self.write_inter_tree(block, log2_size - 1, depth + 1, cbf);
                }
            }
            TNode::Leaf(luma, chroma) => {
                if depth != 0 || cbf.0 || cbf.1 {
                    self.bin(ctx::CBF_LUMA + usize::from(depth == 0), luma.coded());
                }
                if luma.coded() {
                    self.residual_coding(luma);
                }
                for tb in chroma.iter().flatten().filter(|tb| tb.coded()) {
                    self.residual_coding(tb);
                }
            }
        }
    }

    /// `mvd_coding()` (§7.3.8.9).
    fn mvd_coding(&mut self, mvd: [i32; 2]) {
        let a = [mvd[0].unsigned_abs(), mvd[1].unsigned_abs()];
        self.bin(ctx::ABS_MVD_GREATER0_FLAG, a[0] > 0);
        self.bin(ctx::ABS_MVD_GREATER0_FLAG, a[1] > 0);
        for &v in &a {
            if v > 0 {
                // The decoder's table holds this flag's context second.
                self.bin(ctx::ABS_MVD_GREATER1_FLAG + 1, v > 1);
            }
        }
        for k in 0..2 {
            if a[k] > 0 {
                if a[k] > 1 {
                    self.exp_golomb_bypass(a[k] - 2, 1);
                }
                self.log.encode_bypass(u8::from(mvd[k] < 0));
            }
        }
    }

    /// `k`-th order Exp-Golomb code in bypass bins (§9.3.3.5).
    fn exp_golomb_bypass(&mut self, mut value: u32, mut k: u32) {
        while value >= 1 << k {
            self.log.encode_bypass(1);
            value -= 1 << k;
            k += 1;
        }
        self.log.encode_bypass(0);
        self.log.encode_bypass_bits(value, k);
    }

    /// Integer motion search of a 2N×2N block from the `starts` (quarter-sample vectors): see
    /// [`search_motion_rect`](Self::search_motion_rect).
    fn search_motion(&self, x0: u32, y0: u32, size: u32, starts: &[Mv], predictor: Mv) -> Mv {
        self.search_motion_rect(x0, y0, size, size, starts, predictor)
    }

    /// Integer motion search of a `w`×`h` block from the `starts` (quarter-sample vectors): the
    /// best start or vector of the window around zero motion ([`SEARCH_RANGE`]), then a
    /// diamond search with steps of 8, 4, 2 and 1 samples, then half and quarter samples. The cost is the
    /// luma SAD plus √λ times the vector difference's bits from `predictor`.
    fn search_motion_rect(
        &self,
        x0: u32,
        y0: u32,
        w: u32,
        h: u32,
        starts: &[Mv],
        predictor: Mv,
    ) -> Mv {
        let Some(reference) = self.reference else {
            return [0, 0];
        };
        let plane = &reference.planes[0];
        let (stride, plane_h) = (
            i64::from(reference.widths[0]),
            i64::from(reference.heights[0]),
        );
        let src = &self.src[0];
        let (nw, nh) = (w as usize, h as usize);
        let limit = 4 * 256;
        // Early-exit integer SAD once the partial sum + MVD penalty already exceeds `best_cost`.
        let eval = |mv: Mv, best_cost: f64| -> Option<f64> {
            let mvd = [
                i32::from(mv[0]) - i32::from(predictor[0]),
                i32::from(mv[1]) - i32::from(predictor[1]),
            ];
            let mvd_cost = self.sqrt_lambda * mvd_bits(mvd);
            if mvd_cost >= best_cost {
                return None;
            }
            let sad_limit = best_cost - mvd_cost;
            let (dx, dy) = (i64::from(mv[0]) >> 2, i64::from(mv[1]) >> 2);
            let (rx, ry) = (i64::from(x0) + dx, i64::from(y0) + dy);
            let inside =
                rx >= 0 && ry >= 0 && rx + nw as i64 <= stride && ry + nh as i64 <= plane_h;
            let mut total = 0u64;
            for row in 0..nh {
                let s = &src.samples[(y0 as usize + row) * src.width + x0 as usize..][..nw];
                if inside {
                    let r = &plane[((ry + row as i64) * stride + rx) as usize..][..nw];
                    total += s
                        .iter()
                        .zip(r)
                        .map(|(&a, &b)| u64::from((a - i32::from(b)).unsigned_abs()))
                        .sum::<u64>();
                } else {
                    let yy = (ry + row as i64).clamp(0, plane_h - 1) * stride;
                    for (col, &a) in s.iter().enumerate() {
                        let xx = (rx + col as i64).clamp(0, stride - 1);
                        total +=
                            u64::from((a - i32::from(plane[(yy + xx) as usize])).unsigned_abs());
                    }
                }
                if (total as f64) >= sad_limit {
                    return None;
                }
            }
            let c = total as f64 + mvd_cost;
            (c < best_cost).then_some(c)
        };
        let integer = |mv: Mv| [mv[0] & !3, mv[1] & !3];
        let mut best = [0, 0];
        let mut best_cost = eval(best, f64::INFINITY).expect("finite initial cost");
        for &s in starts {
            let s = integer(s);
            if s == [0, 0] || s[0].unsigned_abs() > limit || s[1].unsigned_abs() > limit {
                continue;
            }
            if let Some(c) = eval(s, best_cost) {
                (best, best_cost) = (s, c);
            }
        }
        // Every integer vector of a small window around zero motion: a diamond search alone
        // gets lost in textures, whose cost has no slope to follow.
        for dy in (-SEARCH_RANGE..=SEARCH_RANGE).step_by(4) {
            for dx in (-SEARCH_RANGE..=SEARCH_RANGE).step_by(4) {
                if dx == 0 && dy == 0 {
                    continue;
                }
                if let Some(c) = eval([dx, dy], best_cost) {
                    (best, best_cost) = ([dx, dy], c);
                }
            }
        }
        for step in [32i16, 16, 8, 4] {
            loop {
                let mut moved = false;
                for (dx, dy) in [(step, 0), (-step, 0), (0, step), (0, -step)] {
                    let mv = [best[0].saturating_add(dx), best[1].saturating_add(dy)];
                    if mv[0].unsigned_abs() > limit || mv[1].unsigned_abs() > limit {
                        continue;
                    }
                    if let Some(c) = eval(mv, best_cost) {
                        (best, best_cost, moved) = (mv, c, true);
                    }
                }
                if !moved {
                    break;
                }
            }
        }
        // Half then quarter samples around the best integer vector, with the luma predicted
        // as the decoder interpolates it (8-tap filter).
        let bit_depth = self.bit_depth;
        let mut pred = vec![0i32; nw * nh];
        let mut fractional = |mv: Mv, best_cost: f64| -> Option<f64> {
            let mvd = [
                i32::from(mv[0]) - i32::from(predictor[0]),
                i32::from(mv[1]) - i32::from(predictor[1]),
            ];
            let mvd_cost = self.sqrt_lambda * mvd_bits(mvd);
            if mvd_cost >= best_cost {
                return None;
            }
            let sad_limit = best_cost - mvd_cost;
            heifer_hevc_dec::inter::predict_component(
                reference,
                0,
                x0,
                y0,
                w,
                h,
                mv,
                (1, 1),
                &mut pred,
            );
            let mut total = 0u64;
            for row in 0..nh {
                let s = &src.samples[(y0 as usize + row) * src.width + x0 as usize..][..nw];
                for (&a, &p) in s.iter().zip(&pred[row * nw..][..nw]) {
                    let v = i32::from(heifer_hevc_dec::inter::weight_uni(p, bit_depth));
                    total += u64::from((a - v).unsigned_abs());
                }
                if (total as f64) >= sad_limit {
                    return None;
                }
            }
            let c = total as f64 + mvd_cost;
            (c < best_cost).then_some(c)
        };
        let mut best_cost = fractional(best, f64::INFINITY).expect("finite initial cost");
        for step in [2i16, 1] {
            let centre = best;
            for dy in [-step, 0, step] {
                for dx in [-step, 0, step] {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    let mv = [centre[0] + dx, centre[1] + dy];
                    if mv[0].unsigned_abs() > limit || mv[1].unsigned_abs() > limit {
                        continue;
                    }
                    if let Some(c) = fractional(mv, best_cost) {
                        (best, best_cost) = (mv, c);
                    }
                }
            }
        }
        best
    }

    /// Like [`block_sse`](Self::block_sse), for a `w`×`h` rectangle.
    fn block_sse_rect(&self, c: usize, x: u32, y: u32, w: usize, h: usize) -> i64 {
        let stride = self.recon.frame.widths[c] as usize;
        let rec = &self.recon.frame.planes[c];
        let src = &self.src[c];
        let mut sse = 0i64;
        for row in 0..h {
            let r = (y as usize + row) * stride + x as usize;
            let s = (y as usize + row) * src.width + x as usize;
            for col in 0..w {
                let d = i64::from(src.samples[s + col]) - i64::from(rec[r + col]);
                sse += d * d;
            }
        }
        sse
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
        // blocks are tried whole only when quite flat: elsewhere, the split wins 96% of the time. 64×64
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
            let limit = if log2_size > LOG2_MAX_TB { 0.5 } else { 2.0 };
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
        let after = self.log.checkpoint_since(&start.log);
        let whole = self.snapshot(x0, y0, size, true, after);

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
        if self.lossless {
            // Transquant bypass: the levels are the residual samples themselves, and the
            // decoder adds them to the prediction without a transform.
            let n = 1usize << log2_size;
            let levels = if self.diff[..n * n].iter().all(|&d| d == 0) {
                Vec::new()
            } else {
                self.diff[..n * n].to_vec()
            };
            self.reconstruct(c, x, y, log2_size, mode, &levels, false);
            return Tb {
                c,
                x,
                y,
                log2_size,
                mode,
                levels,
                transform_skip: false,
                sse: 0.0,
            };
        }
        let w = if c == 0 { 1.0 } else { self.chroma_weight() };
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
        let n = 1usize << log2_size;
        // Only luma is estimated: on chroma, whose errors weigh more in RGB, the estimate made
        // worse choices on graphics (measured).
        if !self.deferred || c > 0 {
            return self.reconstruct(c, x, y, log2_size, mode, levels, transform_skip);
        }
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
            let dst = c == 0 && log2_size == 2 && !self.inter_block;
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
        // 4:4:4 chroma scan follows the luma rules at 8×8 too (§7.4.9.11).
        let scan = scan_type(log2_size, c == 0 || self.csx == 1, mode);
        // λ in units of the squared quantization step, 2^((QP - 4) / 3).
        let w = if c == 0 { 1.0 } else { self.chroma_weight() };
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
                transquant_bypass: self.lossless,
                explicit_rdpcm: None,
                inter: self.inter_block,
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

    /// Chooses the chroma prediction mode of a coding unit (or prediction block) whose luma
    /// mode is `luma_mode`: returns `intra_chroma_pred_mode` (4 = same as luma) and the mode it
    /// gives. `luma` is the position and size of the luma block the chroma belongs to, `(xc,
    /// yc)` the position of the chroma block and `log2_c` its size.
    #[allow(clippy::too_many_arguments)]
    fn choose_chroma_mode(
        &mut self,
        luma: (u32, u32, u32),
        xc: u32,
        yc: u32,
        log2_c: u8,
        luma_mode: u8,
        luma_coded: bool,
    ) -> (u8, u8) {
        let n = 1usize << log2_c;
        let refs = [1, 2].map(|c| self.recon.intra_references(c, xc, yc, n));
        let mut costs = Vec::with_capacity(5);
        for syntax in [4u8, 0, 1, 2, 3] {
            let mode = match syntax {
                4 => luma_mode,
                s => {
                    let m = [0, 26, 10, 1][usize::from(s)];
                    if m == luma_mode { 34 } else { m }
                }
            };
            let mut cost = self.sqrt_lambda * if syntax == 4 { 1.0 } else { 3.0 };
            let predicted = self.chroma_mode(mode);
            for c in 1..=2 {
                self.predict_from(&refs[c - 1], c, xc, yc, log2_c, predicted);
                cost += satd(&self.diff[..n * n], n) as f64;
            }
            costs.push((cost, syntax, mode));
        }
        costs.sort_by(|a, b| a.0.total_cmp(&b.0));
        let tried = 2;
        // The best few by SATD are coded (as one transform block per component), and the
        // cheapest in rate and distortion is kept.
        let log2_tb = log2_c.min(self.max_chroma_log2());
        let (x0, y0, size) = luma;
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
            // A call of `chroma_blocks` covers a square, or two stacked ones in 4:2:2.
            let step = 1u32 << log2_tb;
            let tall = if self.is_422() { 2 } else { 1 };
            for y in (yc..yc + tall * (1 << log2_c)).step_by((tall * step) as usize) {
                for x in (xc..xc + (1 << log2_c)).step_by(step as usize) {
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

    /// Chroma transform blocks at a chroma position, if the picture has chroma, in coding
    /// order: Cb then Cr. In 4:2:2 a chroma block is twice as high as wide and each component
    /// has two square blocks, the top one then the bottom one (§7.3.8.10); `mode` is mapped to
    /// the 4:2:2 chroma prediction mode (Table 8-3).
    fn chroma_blocks(&mut self, xc: u32, yc: u32, log2_size: u8, mode: u8) -> Option<Vec<Tb>> {
        if !self.chroma {
            return None;
        }
        let mode = self.chroma_mode(mode);
        let halves = if self.is_422() { 2 } else { 1 };
        let n = 1u32 << log2_size;
        let mut tbs = Vec::with_capacity(2 * halves);
        for c in 1..=2 {
            for half in 0..halves {
                tbs.push(self.transform_block(c, xc, yc + half as u32 * n, log2_size, mode));
            }
        }
        Some(tbs)
    }

    /// Whether the chroma is 4:2:2.
    fn is_422(&self) -> bool {
        self.chroma && self.csx == 2 && self.csy == 1
    }

    /// The chroma prediction mode used for `mode` (`IntraPredModeC`): in 4:2:2, mapped by
    /// Table 8-3 to the half-width chroma block.
    fn chroma_mode(&self, mode: u8) -> u8 {
        const MAP_422: [u8; 35] = [
            0, 1, 2, 2, 2, 2, 3, 5, 7, 8, 10, 12, 13, 15, 17, 18, 19, 20, 21, 22, 23, 23, 24, 24,
            25, 25, 26, 27, 27, 28, 28, 29, 29, 30, 31,
        ];
        if self.is_422() {
            MAP_422[usize::from(mode)]
        } else {
            mode
        }
    }

    /// `coding_unit()`: tries the best few prediction modes with one transform for the whole
    /// block, then the best of them with four transforms, and (for the smallest blocks) four 4×4
    /// prediction blocks, and keeps the cheapest. Returns the distortion and whether there is
    /// any residual.
    fn coding_unit(&mut self, x0: u32, y0: u32, log2_size: u8, depth: u8) -> (f64, bool) {
        let size = 1u32 << log2_size;
        let start_bits = self.log.bit_count();
        let start = self.snapshot(x0, y0, size, false, self.log.checkpoint());
        // In a P picture, the inter codings first: skipped with each merge candidate, then a
        // searched motion vector. The best one is kept to compare with intra.
        let mut skip: Option<(f64, f64, Snapshot, bool)> = None;
        if self.p_slice && !self.lossless {
            let mut keep = |enc: &mut Self, d: f64, is_skip: bool| {
                let j = d + enc.lambda * (enc.log.bit_count() - start_bits);
                if skip.as_ref().is_none_or(|b| j < b.0) {
                    let after = enc.log.checkpoint_since(&start.log);
                    let snapshot = enc.snapshot(x0, y0, size, true, after);
                    if let Some(old) = skip.replace((j, d, snapshot, is_skip)) {
                        enc.recycle(old.2);
                    }
                }
                enc.restore(x0, y0, size, &start);
            };
            let candidates = self.merge_candidates(x0, y0, size);
            for &(idx, motion) in &candidates {
                let d = self.code_skip(x0, y0, log2_size, depth, idx, motion);
                keep(self, d, true);
            }
            // Merge with a residual: the same candidates, but correcting the prediction error.
            // The candidate that predicts best is not always the cheapest once corrected, so
            // every one is tried. 4:2:2 chroma trees are not handled (see `code_amvp`).
            if !self.is_422() {
                for &(idx, motion) in &candidates {
                    match self.code_merge(x0, y0, log2_size, depth, idx, motion) {
                        Some(d) => keep(self, d, false),
                        None => self.restore(x0, y0, size, &start),
                    }
                }
            }
            if self.reference.is_some() {
                let predictors = heifer_hevc_dec::inter::amvp(
                    &*self,
                    &self.slice_refs(),
                    &Self::geom(x0, y0, size),
                    0,
                    0,
                );
                let mut starts = vec![[0, 0], predictors[0], predictors[1]];
                starts.extend(candidates.iter().map(|(_, m)| m.mv[0]));
                let mv = self.search_motion(x0, y0, size, &starts, predictors[0]);
                let d = self
                    .code_amvp(x0, y0, log2_size, depth, mv, false)
                    .expect("always coded without residual");
                keep(self, d, false);
                // With a residual (4:2:2 chroma trees are not handled).
                if !self.is_422() {
                    match self.code_amvp(x0, y0, log2_size, depth, mv, true) {
                        Some(d) => keep(self, d, false),
                        None => self.restore(x0, y0, size, &start),
                    }
                }
            }
            // Partitions 2N×N, N×2N and the asymmetric pairs (AMP): two prediction blocks with
            // a motion each, at the cost of one transform tree split (interSplitFlag) and a
            // coded `rqt_root_cbf`. Only above the smallest coding units, where the extra
            // syntax pays for itself. 4:2:2 chroma trees are not handled, like the residual
            // paths above.
            if self.reference.is_some() && log2_size > LOG2_MIN_CB && !self.is_422() {
                for part_mode in [
                    PartMode::Part2NxN,
                    PartMode::PartNx2N,
                    PartMode::Part2NxnU,
                    PartMode::Part2NxnD,
                    PartMode::PartnLx2N,
                    PartMode::PartnRx2N,
                ] {
                    let d = self
                        .code_partition(x0, y0, log2_size, depth, part_mode, false)
                        .expect("always coded without residual");
                    keep(self, d, false);
                    match self.code_partition(x0, y0, log2_size, depth, part_mode, true) {
                        Some(d) => keep(self, d, false),
                        None => self.restore(x0, y0, size, &start),
                    }
                }
            }
        }
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
            // trial predicts from: they are reconstructed only if chosen. Lossless blocks are
            // reconstructed as they are coded.
            // Not in 4:2:2: there the bottom chroma block of a transform unit is predicted from
            // the top one, whose neighbours count as available only once the luma block is
            // reconstructed.
            enc.deferred =
                !enc.lossless && !enc.is_422() && kind == CuKind::Whole && log2_size <= LOG2_MAX_TB;
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
        let (j_intra, d, kind, mode, residual, snapshot, deferred) =
            best.expect("a mode was tried");
        if let Some((j_inter, d_inter, inter_snapshot, is_skip)) = skip {
            if j_inter <= j_intra {
                self.restore(x0, y0, size, &inter_snapshot);
                self.recycle(inter_snapshot);
                self.recycle(snapshot);
                self.recycle(start);
                if is_skip {
                    self.skipped_cus += 1;
                } else {
                    self.inter_cus += 1;
                }
                return (d_inter, false);
            }
            self.recycle(inter_snapshot);
        }
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
        // `intra_chroma_pred_mode` and the chroma mode: one per prediction block in 4:4:4
        // PART_NxN, one per coding unit otherwise.
        let mut chroma_syntaxes: Vec<(u8, u8)> = Vec::new();
        let tree = if four {
            let half = size / 2;
            let log2_half = log2_size - 1;
            let mut blocks = Vec::new();
            for i in 0..4u32 {
                let (x, y) = (x0 + (i % 2) * half, y0 + (i / 2) * half);
                let mpm = self.mpm_candidates(x, y);
                let (mode, luma) = self.small_block(x, y, mpm);
                parts.push((mode, mpm));
                let mut node = TNode::Leaf(luma, None);
                if self.chroma && self.csx == 1 {
                    // 4:4:4: each prediction block has its own chroma block, at full
                    // resolution, and its own chroma mode. It is reconstructed right after the
                    // luma block, as the decoder does: the availability of the neighbouring
                    // samples seen by the next prediction depends on it.
                    let choice = self.choose_chroma_mode((x, y, half), x, y, log2_half, mode, true);
                    chroma_syntaxes.push(choice);
                    let chroma = self.chroma_blocks(x, y, log2_half, choice.1);
                    if let TNode::Leaf(_, slot) = &mut node {
                        *slot = chroma;
                    }
                }
                blocks.push(node);
            }
            if self.chroma && self.csx != 1 {
                let (xc, yc) = self.chroma_pos(x0, y0);
                let choice =
                    self.choose_chroma_mode((x0, y0, size), xc, yc, log2_half, parts[0].0, true);
                chroma_syntaxes.push(choice);
            }
            // In 4:2:0, the four 4×4 luma blocks share their parent's chroma blocks, coded
            // after the last of them.
            let chroma = if self.chroma && self.csx != 1 {
                let (xc, yc) = self.chroma_pos(x0, y0);
                self.chroma_blocks(xc, yc, log2_half, chroma_syntaxes[0].1)
            } else {
                None
            };
            TNode::Split(blocks, chroma)
        } else {
            let mpm = self.mpm_candidates(x0, y0);
            self.fill_grid(x0, y0, size, None, Some(mode));
            parts.push((mode, mpm));
            if self.chroma {
                // The chroma mode is chosen once per coding unit, with its first luma mode, and
                // kept for the other luma modes tried when it can still be signalled.
                let (xc, yc) = self.chroma_pos(x0, y0);
                let log2_c = self.chroma_log2(log2_size).min(LOG2_MAX_TB);
                let choice = match self.chroma_choice {
                    Some(chosen) => chroma_syntax(chosen, mode),
                    None => {
                        let choice =
                            self.choose_chroma_mode((x0, y0, size), xc, yc, log2_c, mode, false);
                        self.chroma_choice = Some(choice.1);
                        choice
                    }
                };
                chroma_syntaxes.push(choice);
            }
            let chroma = chroma_syntaxes.first().map_or(1, |c| c.1);
            if kind == CuKind::Whole && log2_size <= LOG2_MAX_TB {
                self.transform_leaf(x0, y0, log2_size, mode, chroma)
            } else {
                self.split_tree(x0, y0, log2_size, mode, chroma, 1)
            }
        };

        // Syntax: part_mode, the luma modes, the chroma modes, then the transform tree.
        if self.lossless {
            // The PPS enables the flag only for lossless streams.
            self.bin(ctx::CU_TRANSQUANT_BYPASS_FLAG, true);
        }
        if self.p_slice {
            // Not skipped, and intra.
            let inc = self.skip_ctx(x0, y0);
            self.bin(ctx::CU_SKIP_FLAG + inc, false);
            self.bin(ctx::PRED_MODE_FLAG, true);
        }
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
        for &(syntax, _) in &chroma_syntaxes {
            // intra_chroma_pred_mode: 0 for "as luma", else 1 and two bypass bits.
            self.bin(ctx::INTRA_CHROMA_PRED_MODE, syntax != 4);
            if syntax != 4 {
                self.log.encode_bypass_bits(u32::from(syntax), 2);
            }
        }
        self.write_tree(&tree, log2_size, 0, (true, true), four);

        self.recon.coding_unit(&CodingUnit {
            x: x0,
            y: y0,
            log2_size,
            part_nxn: four,
            transquant_bypass: self.lossless,
            pcm: false,
            intra: true,
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

    /// One transform unit: the luma block and, from 8×8 luma up in 4:2:0 (always in 4:4:4), its
    /// chroma blocks.
    fn transform_leaf(&mut self, x: u32, y: u32, log2_size: u8, mode: u8, chroma: u8) -> TNode {
        let luma = self.transform_block(0, x, y, log2_size, mode);
        let chroma = if self.chroma && (log2_size > 2 || self.csx == 1) {
            let log2_c = self.chroma_log2(log2_size);
            let (xc, yc) = self.chroma_pos(x, y);
            self.chroma_blocks(xc, yc, log2_c, chroma)
        } else {
            None
        };
        TNode::Leaf(luma, chroma)
    }

    /// A block split into four transform trees at `depth`; in 4:2:0, the 4×4 luma blocks share
    /// the chroma blocks of their parent.
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
        let chroma = if self.chroma && self.csx == 2 && log2_size == 3 {
            let (xc, yc) = self.chroma_pos(x0, y0);
            self.chroma_blocks(xc, yc, 2, chroma)
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
        // 4:4:4 chroma has a transform block at every node, 4×4 ones included; 4:2:0 only from
        // 8×8 luma up (4×4 luma blocks share their parent's).
        if self.chroma && (log2_size > 2 || self.csx == 1) {
            // In 4:2:2, a node whose chroma blocks are its own (not split, or 8×8 luma split
            // into 4×4 blocks) has one flag per square block, the top one then the bottom one.
            let own = match tree {
                TNode::Leaf(_, c) => c.as_ref(),
                TNode::Split(_, c) if log2_size == 3 => c.as_ref(),
                TNode::Split(..) => None,
            };
            for (k, parent_cbf) in [(0, parent.0), (1, parent.1)] {
                if depth != 0 && !parent_cbf {
                    continue;
                }
                match own.filter(|_| self.is_422()) {
                    Some(tbs) => {
                        for tb in tbs.iter().filter(|tb| tb.c == k + 1) {
                            self.bin(ctx::CBF_CHROMA + usize::from(depth), tb.coded());
                        }
                    }
                    None => {
                        let coded = if k == 0 { cbf.0 } else { cbf.1 };
                        self.bin(ctx::CBF_CHROMA + usize::from(depth), coded);
                    }
                }
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
        if TRANSFORM_SKIP && log2_size == 2 && !self.lossless {
            // Not signalled with transquant bypass (and the PPS disables it for lossless).
            let ctx = if c == 0 {
                ctx::TRANSFORM_SKIP_FLAG_LUMA
            } else {
                ctx::TRANSFORM_SKIP_FLAG_CHROMA
            };
            self.bin(ctx, tb.transform_skip);
        }
        let luma = c == 0;
        let scan = scan_type(log2_size, luma || self.csx == 1, intra_mode);
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

            // Sign data hiding: the sign of the first level follows from the parity (not used
            // with transquant bypass, where the PPS disables it).
            let sign_hidden = !self.lossless && sig_pos[0] - sig_pos[n_sig - 1] > 3;
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

impl MotionSource for Encoder<'_> {
    /// Availability of a neighbouring block (§6.4.2) and its motion. A neighbour inside the
    /// current coding unit (a partitioned one) is available, like the decoder's: the motion of
    /// the prediction block before it is in the grid.
    fn neighbour(&self, g: &PuGeom, xn: i64, yn: i64) -> Option<Motion> {
        if !self.recon.is_available(g.x, g.y, xn, yn) {
            return None;
        }
        let m = self.motion[self.grid(xn as u32, yn as u32)];
        m.is_inter().then_some(m)
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
        let (picture, recon, kinds) = encode_with_reconstruction(
            w as u32,
            h as u32,
            planes,
            &CodingOptions {
                chroma_format: if monochrome {
                    ChromaFormat::Monochrome
                } else {
                    ChromaFormat::Chroma420
                },
                lossless: false,
            },
            &config,
        )
        .unwrap();
        KINDS.with(|k| {
            for (total, n) in k.borrow_mut().iter_mut().zip(kinds) {
                *total += n;
            }
        });
        let stream = picture.annex_b();

        // Without in-loop filters, the decoder reproduces the encoder's reconstruction exactly.
        let options = DecodeOptions::new()
            .with_skip_loop_filters(true)
            .with_verify_hash(false)
            .with_max_pixels(0);
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
            let (picture, recon, _) = encode_with_reconstruction(
                w as u32,
                h as u32,
                planes,
                &CodingOptions {
                    chroma_format: ChromaFormat::Chroma420,
                    lossless: false,
                },
                &config,
            )
            .unwrap();
            assert_eq!(picture.profile_idc, 2, "Main 10");
            let stream = picture.annex_b();
            let options = DecodeOptions::new()
                .with_skip_loop_filters(true)
                .with_verify_hash(false)
                .with_max_pixels(0);
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

    /// A deterministic 4:4:4 picture: gradients, edges and noise on all three components.
    fn test_picture_444(w: usize, h: usize, seed: u32, bit_depth: u8) -> [Vec<u16>; 3] {
        let mut state = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
        let mut noise = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state % 63) as i32 - 31
        };
        let max = (1i32 << bit_depth) - 1;
        let scale = max / 255;
        let plane = |k: usize, noise: &mut dyn FnMut() -> i32| -> Vec<u16> {
            (0..w * h)
                .map(|i| {
                    let (x, y) = (i % w, i / w);
                    let v = match (x * 3 / w.max(1), y * 3 / h.max(1)) {
                        (0, _) => (x * 255 / w.max(1)) as i32,
                        (1, _) => (y * 255 / h.max(1)) as i32,
                        (2, 0) => {
                            if (x / 2 + y / 2) % 2 == 0 {
                                30
                            } else {
                                225
                            }
                        }
                        (_, 0) => 128 + noise() * 2,
                        (_, _) => 255 - (x + y) as i32 * 255 / (w + h) as i32,
                    } + k as i32 * 9;
                    (v * scale).clamp(0, max) as u16
                })
                .collect()
        };
        [
            plane(0, &mut noise),
            plane(1, &mut noise),
            plane(2, &mut noise),
        ]
    }

    #[test]
    fn round_trip_444() {
        for (w, h) in [(1, 1), (7, 5), (16, 16), (64, 48), (100, 37), (33, 130)] {
            let [y, cb, cr] = test_picture_444(w, h, (w * 17 + h) as u32, 8);
            let planes = Planes16 {
                y: &y,
                chroma: Some((&cb, &cr)),
                bit_depth: 8,
            };
            let config = EncoderConfig {
                qp: 22,
                ..EncoderConfig::default()
            };
            let options = CodingOptions {
                chroma_format: ChromaFormat::Chroma444,
                lossless: false,
            };
            let (picture, recon, _) =
                encode_with_reconstruction(w as u32, h as u32, planes, &options, &config).unwrap();
            assert_eq!(picture.profile_idc, 4, "Format Range Extensions");
            assert_eq!(picture.chroma_format, 3);
            assert_eq!((picture.width, picture.height), (w as u32, h as u32));
            let stream = picture.annex_b();

            let unfiltered = decode_picture(
                &stream,
                DecodeOptions::new()
                    .with_skip_loop_filters(true)
                    .with_verify_hash(false)
                    .with_max_pixels(0),
            )
            .unwrap();
            assert_eq!(unfiltered.chroma_format, 3);
            for c in 0..3 {
                let (cw, ch) = (
                    unfiltered.widths[c] as usize,
                    unfiltered.heights[c] as usize,
                );
                assert_eq!(
                    crop(&unfiltered, c, cw, ch),
                    crop(&recon, c, cw, ch),
                    "{w}×{h}, component {c}"
                );
            }

            let frame = decode_picture(&stream, DecodeOptions::default()).unwrap();
            let psnr = |c: usize, src: &[u16]| -> f64 {
                let (cw, ch) = (frame.widths[c] as usize, frame.heights[c] as usize);
                let decoded = crop(&frame, c, cw, ch);
                let mse = decoded
                    .iter()
                    .zip(src)
                    .map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2))
                    .sum::<f64>()
                    / src.len() as f64;
                10.0 * (255.0f64.powi(2) / mse.max(1e-9)).log10()
            };
            let p = [psnr(0, &y), psnr(1, &cb), psnr(2, &cr)];
            assert!(
                p.iter().all(|&p| p > 34.0),
                "{w}×{h}: {:.1} {:.1} {:.1} dB",
                p[0],
                p[1],
                p[2]
            );
        }
    }

    #[test]
    fn round_trip_422() {
        for (w, h) in [
            (1, 1),
            (7, 5),
            (16, 16),
            (64, 48),
            (100, 37),
            (33, 130),
            (96, 80),
        ] {
            for (qp, bit_depth) in [(10, 8), (22, 8), (34, 8), (22, 10)] {
                let [y, cb444, cr444] = test_picture_444(w, h, (w * 23 + h) as u32, bit_depth);
                // Half-width chroma: every other column.
                let cw = w.div_ceil(2);
                let half = |p: &[u16]| -> Vec<u16> {
                    (0..cw * h)
                        .map(|i| p[(i / cw) * w + (i % cw) * 2])
                        .collect()
                };
                let (cb, cr) = (half(&cb444), half(&cr444));
                let planes = Planes16 {
                    y: &y,
                    chroma: Some((&cb, &cr)),
                    bit_depth,
                };
                let config = EncoderConfig {
                    qp,
                    ..EncoderConfig::default()
                };
                let options = CodingOptions {
                    chroma_format: ChromaFormat::Chroma422,
                    lossless: false,
                };
                let (picture, recon, _) =
                    encode_with_reconstruction(w as u32, h as u32, planes, &options, &config)
                        .unwrap();
                assert_eq!(picture.profile_idc, 4, "Format Range Extensions");
                assert_eq!(picture.chroma_format, 2);
                let stream = picture.annex_b();

                // The decoder reproduces the encoder's reconstruction exactly.
                let unfiltered = decode_picture(
                    &stream,
                    DecodeOptions::new()
                        .with_skip_loop_filters(true)
                        .with_verify_hash(false)
                        .with_max_pixels(0),
                )
                .unwrap();
                assert_eq!(unfiltered.chroma_format, 2);
                for c in 0..3 {
                    let (cw, ch) = (
                        unfiltered.widths[c] as usize,
                        unfiltered.heights[c] as usize,
                    );
                    assert_eq!(
                        crop(&unfiltered, c, cw, ch),
                        crop(&recon, c, cw, ch),
                        "{w}×{h} QP {qp} {bit_depth}-bit, component {c}"
                    );
                }

                let frame = decode_picture(&stream, DecodeOptions::default()).unwrap();
                let max = f64::from((1u32 << bit_depth) - 1);
                let psnr = |c: usize, src: &[u16], width: usize| -> f64 {
                    let decoded = crop(&frame, c, width, h);
                    let mse = decoded
                        .iter()
                        .zip(src)
                        .map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2))
                        .sum::<f64>()
                        / src.len() as f64;
                    10.0 * (max.powi(2) / mse.max(1e-9)).log10()
                };
                let p = [psnr(0, &y, w), psnr(1, &cb, cw), psnr(2, &cr, cw)];
                let floor = if qp >= 34 { 28.0 } else { 34.0 };
                assert!(
                    p.iter().all(|&p| p > floor),
                    "{w}×{h} QP {qp}: {:.1} {:.1} {:.1} dB",
                    p[0],
                    p[1],
                    p[2]
                );
            }
        }
    }

    #[test]
    fn lossless_444_and_monochrome_are_exact() {
        for (w, h) in [(1, 1), (8, 8), (17, 9), (64, 64), (100, 37)] {
            for bit_depth in [8u8, 10] {
                let [y, cb, cr] = test_picture_444(w, h, (w * 13 + h) as u32, bit_depth);
                let planes = Planes16 {
                    y: &y,
                    chroma: Some((&cb, &cr)),
                    bit_depth,
                };
                let options = CodingOptions {
                    chroma_format: ChromaFormat::Chroma444,
                    lossless: true,
                };
                let (picture, recon, _) = encode_with_reconstruction(
                    w as u32,
                    h as u32,
                    planes,
                    &options,
                    &EncoderConfig::default(),
                )
                .unwrap();
                assert_eq!(picture.profile_idc, 4);
                let stream = picture.annex_b();
                let frame = decode_picture(&stream, DecodeOptions::default()).unwrap();
                for c in 0..3 {
                    let src = [&y, &cb, &cr][c];
                    assert_eq!(
                        crop(&frame, c, w, h),
                        crop(&recon, c, w, h),
                        "{w}×{h}, {bit_depth} bits, component {c}"
                    );
                    assert_eq!(
                        &crop(&frame, c, w, h)[..],
                        &src[..],
                        "{w}×{h}, {bit_depth} bits, component {c}: not lossless"
                    );
                }
            }

            // Monochrome, e.g. an alpha channel.
            let [y, _, _] = test_picture_444(w, h, (w + h * 3) as u32, 8);
            let planes = Planes16 {
                y: &y,
                chroma: None,
                bit_depth: 8,
            };
            let options = CodingOptions {
                chroma_format: ChromaFormat::Monochrome,
                lossless: true,
            };
            let (picture, ..) = encode_with_reconstruction(
                w as u32,
                h as u32,
                planes,
                &options,
                &EncoderConfig::default(),
            )
            .unwrap();
            let frame = decode_picture(
                &picture.annex_b(),
                DecodeOptions::new()
                    .with_skip_loop_filters(true)
                    .with_verify_hash(false)
                    .with_max_pixels(0),
            )
            .unwrap();
            assert_eq!(crop(&frame, 0, w, h), y, "{w}×{h}: monochrome not lossless");
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

    /// Encodes `pictures` (8-bit 4:2:0 planes of `w`×`h`) as an IDR picture then P pictures,
    /// each predicted from the previous reconstruction. Returns the Annex B stream and the
    /// encoder's reconstructions, cropped to the picture.
    fn encode_sequence_stream(
        w: usize,
        h: usize,
        pictures: &[(Vec<u8>, Vec<u8>, Vec<u8>)],
        qp: u8,
    ) -> (Vec<u8>, Vec<[Vec<u16>; 3]>, Vec<usize>) {
        let options = CodingOptions::default();
        let config = EncoderConfig {
            qp,
            ..EncoderConfig::default()
        };
        let wide = |p: &[u8]| p.iter().map(|&v| u16::from(v)).collect::<Vec<u16>>();
        let mut stream = Vec::new();
        let mut recons = Vec::new();
        let mut sizes = Vec::new();
        let mut reference: Option<Frame> = None;
        for (poc, (y, cb, cr)) in pictures.iter().enumerate() {
            let (y, cb, cr) = (wide(y), wide(cb), wide(cr));
            let planes = Planes16 {
                y: &y,
                chroma: Some((&cb, &cr)),
                bit_depth: 8,
            };
            let kind = match &reference {
                None => Kind::SequenceIdr,
                Some(frame) => Kind::P {
                    poc: poc as u32,
                    reference: frame,
                },
            };
            let (picture, recon, _) =
                encode_kind(w as u32, h as u32, planes, &options, &config, kind).unwrap();
            if poc == 0 {
                for nal in &picture.parameter_sets {
                    stream.extend_from_slice(&[0, 0, 0, 1]);
                    stream.extend_from_slice(nal);
                }
            }
            for nal in &picture.slices {
                stream.extend_from_slice(&[0, 0, 0, 1]);
                stream.extend_from_slice(nal);
            }
            sizes.push(picture.slices.iter().map(Vec::len).sum());
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            recons.push([
                crop(&recon, 0, w, h),
                crop(&recon, 1, cw, ch),
                crop(&recon, 2, cw, ch),
            ]);
            reference = Some(recon);
        }
        (stream, recons, sizes)
    }

    /// Decodes `stream` and checks every picture against the encoder's reconstruction.
    fn check_sequence(stream: &[u8], recons: &[[Vec<u16>; 3]]) {
        let mut decoder = heifer_hevc_dec::SequenceDecoder::new(DecodeOptions::new());
        let units = heifer_hevc_dec::access_units(stream);
        assert_eq!(units.len(), recons.len());
        for (n, unit) in units.into_iter().enumerate() {
            let frame = decoder.decode(unit).unwrap();
            for c in 0..3 {
                let (cw, ch) = (frame.widths[c] as usize, frame.heights[c] as usize);
                assert!(
                    crop(&frame, c, cw, ch) == recons[n][c],
                    "picture {n}, plane {c}"
                );
            }
        }
    }

    #[test]
    fn a_still_scene_is_coded_with_skipped_blocks() {
        let (w, h) = (96, 72);
        let picture = test_picture(w, h, 11);
        let pictures = vec![picture.clone(), picture.clone(), picture];
        let (stream, recons, sizes) = encode_sequence_stream(w, h, &pictures, 30);
        if let Ok(path) = std::env::var("HEIFER_DUMP_P") {
            std::fs::write(path, &stream).unwrap();
        }
        check_sequence(&stream, &recons);
        // The P pictures copy the first one: a few bytes against hundreds.
        assert!(
            sizes[1] * 20 < sizes[0] && sizes[2] * 20 < sizes[0],
            "{sizes:?}"
        );
        eprintln!("slice sizes: {sizes:?}");
    }

    #[test]
    fn p_pictures_decode_as_the_encoder_reconstructs_them() {
        let (w, h) = (96, 72);
        let pictures: Vec<_> = (0..3).map(|n| test_picture(w, h, 7 + n)).collect();
        let (stream, recons, _) = encode_sequence_stream(w, h, &pictures, 30);
        let mut decoder =
            heifer_hevc_dec::SequenceDecoder::new(DecodeOptions::new().with_verify_hash(true));
        let units = heifer_hevc_dec::access_units(&stream);
        assert_eq!(units.len(), 3);
        for (n, unit) in units.into_iter().enumerate() {
            let frame = decoder.decode(unit).unwrap();
            for c in 0..3 {
                let (cw, ch) = (frame.widths[c] as usize, frame.heights[c] as usize);
                assert_eq!(
                    crop(&frame, c, cw, ch),
                    recons[n][c],
                    "picture {n}, plane {c}"
                );
            }
        }
    }

    #[test]
    fn p_pictures_monochrome_decode_as_the_encoder_reconstructs_them() {
        let (w, h) = (64usize, 48usize);
        let pictures: Vec<Vec<u16>> = (0..4)
            .map(|n| {
                (0..w * h)
                    .map(|i| {
                        let (x, y) = (i % w, i / w);
                        (((x + 3 * n) * 5 + y * 7) % 220) as u16
                    })
                    .collect()
            })
            .collect();
        let mut enc = SequenceEncoder::new(
            w as u32,
            h as u32,
            CodingOptions {
                chroma_format: ChromaFormat::Monochrome,
                lossless: false,
            },
            EncoderConfig {
                qp: 28,
                ..EncoderConfig::default()
            },
            30,
        );
        let mut decoder =
            heifer_hevc_dec::SequenceDecoder::new(DecodeOptions::new().with_verify_hash(true));
        for (n, y) in pictures.iter().enumerate() {
            let planes = Planes16 {
                y,
                chroma: None,
                bit_depth: 8,
            };
            let (picture, keyframe) = enc.encode(planes).unwrap();
            assert_eq!(keyframe, n == 0);
            let frame = decoder.decode(&picture.annex_b()).unwrap();
            assert_eq!(frame.chroma_format, 0);
            assert_eq!(crop(&frame, 0, w, h).len(), w * h);
        }
    }

    #[test]
    fn p_pictures_422_decode_as_the_encoder_reconstructs_them() {
        // 4:2:2 P pictures: merge with a residual is left out there (its chroma tree is not
        // handled, like AMVP with a residual), so this covers the skip and AMVP paths.
        let (w, h) = (64usize, 48usize);
        let cw = w.div_ceil(2);
        let pictures: Vec<[Vec<u16>; 3]> = (0..3)
            .map(|n| {
                let [y, cb, cr] = test_picture_444(w, h, 5 + n, 8);
                let half = |p: &[u16]| -> Vec<u16> {
                    (0..cw * h)
                        .map(|i| p[(i / cw) * w + (i % cw) * 2])
                        .collect()
                };
                [y, half(&cb), half(&cr)]
            })
            .collect();
        let options = CodingOptions {
            chroma_format: ChromaFormat::Chroma422,
            lossless: false,
        };
        let config = EncoderConfig {
            qp: 30,
            ..EncoderConfig::default()
        };
        let mut stream = Vec::new();
        let mut recons = Vec::new();
        let mut reference: Option<Frame> = None;
        for (poc, [y, cb, cr]) in pictures.iter().enumerate() {
            let planes = Planes16 {
                y,
                chroma: Some((cb, cr)),
                bit_depth: 8,
            };
            let kind = match &reference {
                None => Kind::SequenceIdr,
                Some(frame) => Kind::P {
                    poc: poc as u32,
                    reference: frame,
                },
            };
            let (picture, recon, _) =
                encode_kind(w as u32, h as u32, planes, &options, &config, kind).unwrap();
            if poc == 0 {
                for nal in &picture.parameter_sets {
                    stream.extend_from_slice(&[0, 0, 0, 1]);
                    stream.extend_from_slice(nal);
                }
            }
            for nal in &picture.slices {
                stream.extend_from_slice(&[0, 0, 0, 1]);
                stream.extend_from_slice(nal);
            }
            recons.push([
                crop(&recon, 0, w, h),
                crop(&recon, 1, cw, h),
                crop(&recon, 2, cw, h),
            ]);
            reference = Some(recon);
        }
        check_sequence(&stream, &recons);
    }

    #[test]
    fn skipped_and_intra_blocks_side_by_side() {
        // The left half stays, the right half changes: skipped blocks next to intra blocks,
        // and the deblocking filter between them, must match the decoder.
        let (w, h) = (128, 64);
        let base = test_picture(w, h, 3);
        let pictures: Vec<_> = (0..4u32)
            .map(|n| {
                let other = test_picture(w, h, 50 + n);
                let mix = |a: &[u8], b: &[u8], pw: usize| -> Vec<u8> {
                    a.iter()
                        .zip(b)
                        .enumerate()
                        .map(|(i, (&x, &y))| if i % pw < pw / 2 { x } else { y })
                        .collect()
                };
                let cw = w.div_ceil(2);
                (
                    mix(&base.0, &other.0, w),
                    mix(&base.1, &other.1, cw),
                    mix(&base.2, &other.2, cw),
                )
            })
            .collect();
        for qp in [22, 34] {
            let (stream, recons, sizes) = encode_sequence_stream(w, h, &pictures, qp);
            check_sequence(&stream, &recons);
            // Half of each P picture is a copy: noticeably smaller than the IDR picture.
            assert!(
                sizes[1..].iter().all(|&s| s < sizes[0] * 3 / 4),
                "qp {qp}: {sizes:?}"
            );
        }
    }

    #[test]
    fn moving_content_is_coded_with_motion_vectors() {
        // A larger scene moving 3 samples right and 2 down per picture.
        let (sw, sh) = (160, 120);
        let scene = test_picture(sw, sh, 21);
        let (w, h) = (128, 96);
        let window = |dx: usize, dy: usize| {
            let cut =
                |p: &[u8], pw: usize, x0: usize, y0: usize, ww: usize, hh: usize| -> Vec<u8> {
                    (0..hh)
                        .flat_map(|y| p[(y0 + y) * pw + x0..][..ww].to_vec())
                        .collect()
                };
            let (cw, chh, scw) = (w / 2, h / 2, sw / 2);
            (
                cut(&scene.0, sw, dx, dy, w, h),
                cut(&scene.1, scw, dx / 2, dy / 2, cw, chh),
                cut(&scene.2, scw, dx / 2, dy / 2, cw, chh),
            )
        };
        // Even steps keep the chroma aligned with the luma.
        let pictures: Vec<_> = (0..4).map(|n| window(20 - 4 * n, 16 - 2 * n)).collect();
        let (stream, recons, sizes) = encode_sequence_stream(w, h, &pictures, 30);
        if let Ok(path) = std::env::var("HEIFER_DUMP_P") {
            std::fs::write(path, &stream).unwrap();
        }
        check_sequence(&stream, &recons);
        eprintln!("slice sizes: {sizes:?}");
        assert!(sizes[1..].iter().all(|&s| s * 3 < sizes[0]), "{sizes:?}");
    }

    #[test]
    fn inter_blocks_code_a_residual() {
        // A scene moving 4 samples right and 2 down per picture, and getting brighter: the
        // motion predicts it well, but not exactly.
        let (sw, sh) = (160, 120);
        let scene = test_picture(sw, sh, 33);
        let (w, h) = (128, 96);
        let frame = |n: usize| {
            let (dx, dy) = (20 - 4 * n, 16 - 2 * n);
            let gain = 6 * n as i32;
            let cut = |p: &[u8],
                       pw: usize,
                       x0: usize,
                       y0: usize,
                       ww: usize,
                       hh: usize,
                       add: i32|
             -> Vec<u8> {
                (0..hh)
                    .flat_map(|y| {
                        p[(y0 + y) * pw + x0..][..ww]
                            .iter()
                            .map(|&v| (i32::from(v) + add).clamp(0, 255) as u8)
                            .collect::<Vec<_>>()
                    })
                    .collect()
            };
            (
                cut(&scene.0, sw, dx, dy, w, h, gain),
                cut(&scene.1, sw / 2, dx / 2, dy / 2, w / 2, h / 2, 0),
                cut(&scene.2, sw / 2, dx / 2, dy / 2, w / 2, h / 2, 0),
            )
        };
        let pictures: Vec<_> = (0..4).map(frame).collect();
        let (stream, recons, sizes) = encode_sequence_stream(w, h, &pictures, 30);
        if let Ok(path) = std::env::var("HEIFER_DUMP_P") {
            std::fs::write(path, &stream).unwrap();
        }
        check_sequence(&stream, &recons);
        eprintln!("slice sizes: {sizes:?}");
        assert!(sizes[1..].iter().all(|&s| s * 2 < sizes[0]), "{sizes:?}");
        // The brightness drift is corrected on a merge prediction (a good candidate plus a
        // residual) rather than by a new motion vector: without merge-with-residual the P
        // pictures total about 1070 bytes, more than this bound (0.36 × the IDR picture).
        let inter: usize = sizes[1..].iter().sum();
        assert!(inter * 100 < sizes[0] * 36, "{sizes:?}");
    }

    #[test]
    fn partitioned_inter_blocks_two_halves_move_apart() {
        // Two smooth bands moving in opposite directions, with the boundary inside the coding
        // units: each unit is then best predicted as two halves with a vector each, stacked
        // (2N×N) or side by side (N×2N), which a single 2N×2N vector cannot do and which
        // splitting into smaller units pays more syntax for.
        let split = 24usize;
        let margin = 32usize;
        let scene = |x: usize, y: usize| -> u8 {
            let fx = (x as f64) * std::f64::consts::TAU / 40.0;
            let fy = (y as f64) * std::f64::consts::TAU / 64.0;
            (128.0 + 60.0 * fx.sin() + 30.0 * fy.sin())
                .round()
                .clamp(0.0, 255.0) as u8
        };
        // `along` is the axis the bands move along, `across` the one the boundary crosses. The
        // frame is the longer one across, so as many units straddle the boundary either way.
        for vertical in [false, true] {
            let (w, h) = if vertical { (64, 128) } else { (128, 64) };
            let frame = |n: usize| {
                let shift = 4 * n;
                let moved = |x: usize, y: usize| -> u8 {
                    let (across, along) = if vertical { (x, y) } else { (y, x) };
                    let d = if across < split {
                        margin + shift
                    } else {
                        margin - shift
                    };
                    if vertical {
                        scene(along, across + d)
                    } else {
                        scene(along + d, across)
                    }
                };
                let luma: Vec<u8> = (0..h)
                    .flat_map(|y| (0..w).map(move |x| moved(x, y)).collect::<Vec<_>>())
                    .collect();
                let (cw, ch) = (w / 2, h / 2);
                let chroma = |off: f64| -> Vec<u8> {
                    (0..ch)
                        .flat_map(|y| {
                            (0..cw)
                                .map(move |x| {
                                    let (across, along) = if vertical { (x, y) } else { (y, x) };
                                    let d = if across < split / 2 {
                                        margin + shift
                                    } else {
                                        margin - shift
                                    };
                                    let v = if vertical {
                                        scene(2 * along, 2 * across + d)
                                    } else {
                                        scene(2 * along + d, 2 * across)
                                    };
                                    (f64::from(v) + off).round().clamp(0.0, 255.0) as u8
                                })
                                .collect::<Vec<_>>()
                        })
                        .collect()
                };
                (luma, chroma(-4.0), chroma(4.0))
            };
            let pictures: Vec<_> = (0..4).map(frame).collect();
            let (stream, recons, sizes) = encode_sequence_stream(w, h, &pictures, 32);
            check_sequence(&stream, &recons);
            // The halves keep moving apart, and the first two follow-ups are where a partition
            // pays: two prediction blocks in one unit, against a quadtree split into four with
            // their own headers (they cost about 212 (stacked) and 161 (side by side) bytes
            // here without partitions, against this bound of the intra picture's half).
            assert!(
                sizes[1] + sizes[2] < sizes[0] / 2,
                "vertical {vertical}: {sizes:?}"
            );
        }
    }

    #[test]
    fn asymmetric_inter_partition_follows_a_quarter() {
        // A 12-row band moves differently from the rest, over a fine texture so that a wrong
        // vector costs a real residual. That boundary is a quarter of a 16×16 coding unit, so
        // inside such a unit an asymmetric partition (AMP) predicts the band with its own
        // vector and the rest with another, under one set of headers, where a symmetric split
        // at the middle would not fit and splitting further pays for more headers.
        let (w, h) = (64usize, 64usize);
        let band = 12usize;
        let margin = 32usize;
        let texture = |x: usize, y: usize| -> u8 {
            let v = (x as u32)
                .wrapping_mul(2_654_435_761)
                .wrapping_add((y as u32).wrapping_mul(40_503));
            let v = v ^ (v >> 13) ^ (v >> 23);
            (v >> 7) as u8
        };
        let frame = |n: usize| {
            let shift = 8 * n;
            let moved = |x: usize, y: usize| -> u8 {
                let d = if y < band {
                    margin + shift
                } else {
                    margin - shift
                };
                texture(x + d, y)
            };
            let luma: Vec<u8> = (0..h)
                .flat_map(|y| (0..w).map(move |x| moved(x, y)).collect::<Vec<_>>())
                .collect();
            let (cw, ch) = (w / 2, h / 2);
            let chroma = |off: f64| -> Vec<u8> {
                (0..ch)
                    .flat_map(|y| {
                        (0..cw)
                            .map(move |x| {
                                let d = if y < band / 2 {
                                    margin + shift
                                } else {
                                    margin - shift
                                };
                                let v = f64::from(texture(2 * x + d, 2 * y)) + off;
                                v.round().clamp(0.0, 255.0) as u8
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect()
            };
            (luma, chroma(-4.0), chroma(4.0))
        };
        let pictures: Vec<_> = (0..4).map(frame).collect();
        let (stream, recons, sizes) = encode_sequence_stream(w, h, &pictures, 32);
        check_sequence(&stream, &recons);
        // The asymmetric partitions carry most of the gain here: the texture makes a wrong
        // vector expensive, and the 12-row band is a quarter of a 16×16 unit, so the encoder
        // codes it as 2N×nD with two vectors. The three predicted pictures total about 1469
        // bytes against this bound (two thirds of the intra picture); with only the symmetric
        // partitions they total about 2131.
        let inter: usize = sizes[1..].iter().sum();
        assert!(inter * 3 < sizes[0] * 2, "{sizes:?}");
    }

    #[test]
    fn sub_sample_motion_is_found() {
        // A smooth scene moving 1.25 samples right and 0.5 down per picture.
        let (w, h) = (128, 96);
        let value = |x: f64, y: f64, k: f64| -> u8 {
            (128.0
                + 50.0 * (x * 0.23 + k).sin()
                + 40.0 * (y * 0.19 - k).cos()
                + 20.0 * ((x + y) * 0.41).sin())
            .round()
            .clamp(0.0, 255.0) as u8
        };
        let frame = |n: usize| {
            let (ox, oy) = (1.25 * n as f64, 0.5 * n as f64);
            let luma: Vec<u8> = (0..w * h)
                .map(|i| value((i % w) as f64 - ox, (i / w) as f64 - oy, 0.0))
                .collect();
            let chroma = |k: f64| -> Vec<u8> {
                (0..(w / 2) * (h / 2))
                    .map(|i| {
                        value(
                            2.0 * (i % (w / 2)) as f64 - ox,
                            2.0 * (i / (w / 2)) as f64 - oy,
                            k,
                        )
                    })
                    .collect()
            };
            (luma, chroma(1.0), chroma(2.0))
        };
        let pictures: Vec<_> = (0..4).map(frame).collect();
        let (stream, recons, sizes) = encode_sequence_stream(w, h, &pictures, 30);
        if let Ok(path) = std::env::var("HEIFER_DUMP_P") {
            std::fs::write(path, &stream).unwrap();
        }
        check_sequence(&stream, &recons);
        eprintln!("slice sizes: {sizes:?}");
        assert!(sizes[1..].iter().all(|&s| s * 4 < sizes[0]), "{sizes:?}");
    }
}
