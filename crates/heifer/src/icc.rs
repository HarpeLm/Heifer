//! Colour management to sRGB, from an ICC profile or from `nclx` colour information.
//!
//! Most phones write a *matrix/TRC* RGB ICC profile (`Display P3`, `sRGB`, `Adobe RGB`…):
//! a per-channel tone curve and a 3×3 matrix to the XYZ profile connection space. Some
//! cameras and capture tools write a *LUT* profile instead (`A2B1`/`A2B0`, of type `lut8`,
//! `lut16` or `lutAtoB`). This module reads both and converts the pixel values to sRGB,
//! which is what a viewer without colour management expects.
//!
//! LUT profiles are read for an RGB input profile and an XYZ or Lab connection space. Output
//! (`B2A`) tables are not needed to decode. Profiles in another input space (CMYK, Lab, XYZ)
//! are refused with [`Error::Unsupported`](crate::Error::Unsupported).
//!
//! Files that carry no ICC profile but `nclx` colour information are converted from those
//! primaries and transfer function instead.

use crate::Error;
use crate::image::{Image, Sample};

/// The sRGB white point (D65), as XYZ with Y = 1.
const D65: [f32; 3] = [0.950_456, 1.0, 1.089_058];

/// The ICC profile connection space illuminant (D50), as XYZ with Y = 1.
const D50: [f32; 3] = [0.964_2, 1.0, 0.824_9];

/// The largest PCS XYZ value an ICC profile encodes: `u1Fixed15Number` `0xFFFF`.
const MAX_ENCODEABLE_XYZ: f32 = 1.0 + 32767.0 / 32768.0;

/// XYZ (D65) to linear sRGB.
const XYZ_TO_SRGB: [[f32; 3]; 3] = [
    [3.240_454, -1.537_139, -0.498_531],
    [-0.969_266, 1.876_011, 0.041_556],
    [0.055_643, -0.204_026, 1.057_225],
];

/// Bradford chromatic adaptation, and its inverse.
const BRADFORD: [[f32; 3]; 3] = [
    [0.8951, 0.2664, -0.1614],
    [-0.7502, 1.7135, 0.0367],
    [0.0389, -0.0685, 1.0296],
];
const BRADFORD_INV: [[f32; 3]; 3] = [
    [0.986_993, -0.147_054, 0.159_963],
    [0.432_305, 0.518_360, 0.049_291],
    [-0.008_529, 0.040_043, 0.968_487],
];

/// The identity 3×3 matrix.
const IDENTITY: [[f32; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

/// A per-channel tone curve, encoded value to linear light.
#[derive(Debug, Clone, PartialEq)]
enum Curve {
    /// Linear: the encoded value is already linear.
    Identity,
    /// A pure power law.
    Gamma(f32),
    /// The sRGB transfer function.
    Srgb,
    /// The Rec. 709 / Rec. 601 / Rec. 2020 transfer function.
    Rec709,
    /// An ICC `curv` curve: linear values at evenly spaced encoded values.
    Sampled(Vec<f32>),
    /// An ICC `para` curve: function type and up to seven parameters.
    Parametric { function: u16, params: [f32; 7] },
}

impl Curve {
    /// Linear light for the encoded value `x`, in `0.0..=1.0`.
    fn apply(&self, x: f32) -> f32 {
        let x = x.clamp(0.0, 1.0);
        match self {
            Self::Identity => x,
            Self::Gamma(g) => x.powf(*g),
            Self::Srgb => {
                if x <= 0.040_45 {
                    x / 12.92
                } else {
                    ((x + 0.055) / 1.055).powf(2.4)
                }
            }
            Self::Rec709 => {
                if x < 0.081 {
                    x / 4.5
                } else {
                    ((x + 0.099) / 1.099).powf(1.0 / 0.45)
                }
            }
            Self::Sampled(values) => {
                let position = x * (values.len() - 1) as f32;
                let low = position as usize;
                let high = (low + 1).min(values.len() - 1);
                let fraction = position - low as f32;
                values[low] * (1.0 - fraction) + values[high] * fraction
            }
            Self::Parametric { function, params } => {
                let [g, a, b, c, d, e, f] = *params;
                match function {
                    0 => x.powf(g),
                    1 => {
                        if x >= -b / a {
                            (a * x + b).powf(g)
                        } else {
                            0.0
                        }
                    }
                    2 => {
                        if x >= -b / a {
                            (a * x + b).powf(g) + c
                        } else {
                            c
                        }
                    }
                    3 => {
                        if x >= d {
                            (a * x + b).powf(g)
                        } else {
                            c * x
                        }
                    }
                    _ => {
                        if x >= d {
                            (a * x + b).powf(g) + e
                        } else {
                            c * x + f
                        }
                    }
                }
            }
        }
    }
}

/// The `nclx` transfer characteristics of a code point, as a curve to linear light.
fn transfer_curve(transfer: u16) -> Option<Curve> {
    match transfer {
        0 | 2 | 13 => Some(Curve::Srgb), // unspecified, sRGB, IEC 61966-2-1
        1 | 6 | 14 | 15 => Some(Curve::Rec709), // Rec. 709, 601, 2020 10/12-bit
        4 => Some(Curve::Gamma(2.2)),
        5 => Some(Curve::Gamma(2.8)),
        8 => Some(Curve::Identity),
        _ => None, // PQ, HLG and the rest are HDR; see `crate::hdr`.
    }
}

/// The product `a × b` of two 3×3 matrices.
fn compose(a: [[f32; 3]; 3], b: [[f32; 3]; 3]) -> [[f32; 3]; 3] {
    std::array::from_fn(|i| {
        std::array::from_fn(|j| {
            let sum: f32 = (0..3).map(|k| a[i][k] * b[k][j]).sum();
            sum
        })
    })
}

/// Applies the matrix `m` to the vector `v`.
fn mul(m: [[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
    std::array::from_fn(|i| m[i][0] * v[0] + m[i][1] * v[1] + m[i][2] * v[2])
}

/// Bradford-adapts colour from the white point `source` to `target`.
fn adapt(source: [f32; 3], target: [f32; 3]) -> [[f32; 3]; 3] {
    let (from, to) = (mul(BRADFORD, source), mul(BRADFORD, target));
    let diagonal: [[f32; 3]; 3] = std::array::from_fn(|i| {
        std::array::from_fn(|j| if i == j { to[i] / from[i] } else { 0.0 })
    });
    compose(BRADFORD_INV, compose(diagonal, BRADFORD))
}

/// Solves `m x = b` for a 3×3 matrix (used to scale colourants to the white point).
fn solve3(m: [[f32; 3]; 3], b: [f32; 3]) -> [f32; 3] {
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    // The cofactors, then the transposed cofactor matrix times `b`.
    let minor = |a: usize, c: usize, d: usize, e: usize| m[a][c] * m[d][e] - m[a][e] * m[d][c];
    let cofactor = |i: usize, j: usize| {
        let sign = if (i + j) % 2 == 0 { 1.0 } else { -1.0 };
        let value = match (i, j) {
            (0, 0) => minor(1, 1, 2, 2),
            (0, 1) => minor(1, 0, 2, 2),
            (0, 2) => minor(1, 0, 2, 1),
            (1, 0) => minor(0, 1, 2, 2),
            (1, 1) => minor(0, 0, 2, 2),
            (1, 2) => minor(0, 0, 2, 1),
            (2, 0) => minor(0, 1, 1, 2),
            (2, 1) => minor(0, 0, 1, 2),
            _ => minor(0, 0, 1, 1),
        };
        sign * value / det
    };
    [
        cofactor(0, 0) * b[0] + cofactor(1, 0) * b[1] + cofactor(2, 0) * b[2],
        cofactor(0, 1) * b[0] + cofactor(1, 1) * b[1] + cofactor(2, 1) * b[2],
        cofactor(0, 2) * b[0] + cofactor(1, 2) * b[1] + cofactor(2, 2) * b[2],
    ]
}

/// Builds an RGB→XYZ matrix from xy chromaticities (Y = 1).
fn rgb_to_xyz(
    (rx, ry): (f32, f32),
    (gx, gy): (f32, f32),
    (bx, by): (f32, f32),
    (wx, wy): (f32, f32),
) -> [[f32; 3]; 3] {
    let column = |x: f32, y: f32| [x / y, 1.0, (1.0 - x - y) / y];
    let (white, r, g, b) = (
        column(wx, wy),
        column(rx, ry),
        column(gx, gy),
        column(bx, by),
    );
    let matrix = [[r[0], g[0], b[0]], [r[1], g[1], b[1]], [r[2], g[2], b[2]]];
    // Scale the colourant columns so that R = G = B = 1 maps to the white point.
    let scale = solve3(matrix, white);
    [
        [r[0] * scale[0], g[0] * scale[1], b[0] * scale[2]],
        [r[1] * scale[0], g[1] * scale[1], b[1] * scale[2]],
        [r[2] * scale[0], g[2] * scale[1], b[2] * scale[2]],
    ]
}

/// A linear-RGB (source primaries) to linear-sRGB matrix for an `nclx` code point, or `None`
/// for primaries heifer does not know.
fn primaries_matrix(primaries: u16) -> Option<[[f32; 3]; 3]> {
    // Code points are H.273 colour primaries.
    let (r, g, b, w) = match primaries {
        1 | 2 => return Some(IDENTITY),
        5 => (
            (0.640, 0.330),
            (0.290, 0.600),
            (0.150, 0.060),
            (0.3127, 0.3290),
        ),
        6 => (
            (0.630, 0.340),
            (0.310, 0.595),
            (0.155, 0.070),
            (0.3127, 0.3290),
        ),
        9 => (
            (0.708, 0.292),
            (0.170, 0.797),
            (0.131, 0.046),
            (0.3127, 0.3290),
        ),
        11 => (
            (0.680, 0.320),
            (0.265, 0.690),
            (0.150, 0.060),
            (0.3140, 0.3510),
        ),
        12 => (
            (0.680, 0.320),
            (0.265, 0.690),
            (0.150, 0.060),
            (0.3127, 0.3290),
        ),
        _ => return None,
    };
    let to_xyz = rgb_to_xyz(r, g, b, w);
    let white = mul(to_xyz, [1.0, 1.0, 1.0]);
    Some(compose(XYZ_TO_SRGB, compose(adapt(white, D65), to_xyz)))
}

/// The sRGB transfer function, linear light to encoded value.
fn linear_to_srgb(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    if x <= 0.003_130_8 {
        x * 12.92
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// Encodes linear sRGB to `max`-bit samples through a fine table with linear interpolation.
/// The sRGB curve is continuous but not polynomial; 4096 steps stay within a fraction of a
/// 16-bit sample.
struct SrgbEncoder {
    max: u16,
    table: Vec<f32>,
}

impl SrgbEncoder {
    const STEPS: usize = 4096;

    fn new(max: u16) -> Self {
        let table = (0..=Self::STEPS)
            .map(|i| linear_to_srgb(i as f32 / Self::STEPS as f32) * f32::from(max))
            .collect();
        Self { max, table }
    }

    fn encode(&self, value: f32) -> u16 {
        let position = value.clamp(0.0, 1.0) * Self::STEPS as f32;
        let low = (position as usize).min(Self::STEPS - 1);
        let fraction = position - low as f32;
        let value = self.table[low] * (1.0 - fraction) + self.table[low + 1] * fraction;
        value.round().clamp(0.0, f32::from(self.max)) as u16
    }
}

/// Converts an image to sRGB: linearise with `curve`, mix with `matrix`, encode with sRGB.
fn convert<S: Sample>(
    image: &Image<S>,
    matrix: [[f32; 3]; 3],
    curve: impl Fn(usize, f32) -> f32,
) -> Image<S> {
    let max = u32::from(image.max_value());
    let in_scale = 1.0 / max as f32;
    // Linearising the code values through a table is much cheaper than a power per sample,
    // and exact: there are only `max + 1` of them.
    let luts: [Vec<f32>; 3] =
        std::array::from_fn(|c| (0..=max).map(|v| curve(c, v as f32 * in_scale)).collect());
    let encode = SrgbEncoder::new(image.max_value());

    let mut data = Vec::with_capacity(image.data.len());
    for pixel in image.data.chunks_exact(4) {
        let linear = [
            luts[0][usize::from(pixel[0].to_u16())],
            luts[1][usize::from(pixel[1].to_u16())],
            luts[2][usize::from(pixel[2].to_u16())],
        ];
        let mixed = mul(matrix, linear);
        data.push(S::from_u16(encode.encode(mixed[0])));
        data.push(S::from_u16(encode.encode(mixed[1])));
        data.push(S::from_u16(encode.encode(mixed[2])));
        data.push(pixel[3]);
    }
    Image {
        width: image.width,
        height: image.height,
        bit_depth: image.bit_depth,
        has_alpha: image.has_alpha,
        data,
    }
}

/// Converts an image to sRGB using `nclx` colour information.
pub(crate) fn from_primaries<S: Sample>(
    image: &Image<S>,
    primaries: u16,
    transfer: u16,
) -> Result<Image<S>, Error> {
    let matrix = primaries_matrix(primaries)
        .ok_or_else(|| Error::Unsupported(format!("colour primaries {primaries}")))?;
    let curve = transfer_curve(transfer).ok_or_else(|| {
        let hint = match transfer {
            16 | 18 => " (HDR: use decode_hdr)",
            _ => "",
        };
        Error::Unsupported(format!("transfer characteristics {transfer}{hint}"))
    })?;
    if matrix == IDENTITY && curve == Curve::Srgb {
        // Already sRGB: do not pay for a conversion, and leave the samples untouched.
        return Ok(image.clone());
    }
    Ok(convert(image, matrix, |_, x| curve.apply(x)))
}

/// A parsed RGB ICC profile: matrix/TRC, or a LUT-based transform to the PCS.
pub(crate) enum Profile {
    MatrixTrc(MatrixTrc),
    Lut(LutProfile),
}

/// A matrix/TRC RGB profile, converted to sRGB through the profile connection space.
pub(crate) struct MatrixTrc {
    curves: [Curve; 3],
    /// Linear RGB in the profile's primaries to linear sRGB.
    matrix: [[f32; 3]; 3],
}

impl Profile {
    /// Parses an RGB ICC profile: the relative-colorimetric LUT (`A2B1`) when it has one, else
    /// the perceptual one (`A2B0`), else its matrix/TRC tags. Profiles in another input space,
    /// or without any usable transform, are reported as unsupported.
    pub(crate) fn parse(data: &[u8]) -> Result<Self, Error> {
        if data.len() < 132 || data.get(36..40) != Some(b"acsp") {
            return Err(Error::Unsupported("malformed ICC profile".into()));
        }
        if data.get(16..20) != Some(b"RGB ") {
            return Err(Error::Unsupported(
                "ICC profile that is not an RGB profile".into(),
            ));
        }
        let pcs = match data.get(20..24) {
            Some(b"XYZ ") => Some(Pcs::Xyz),
            Some(b"Lab ") => Some(Pcs::Lab),
            _ => None,
        };
        let count = u32::from_be_bytes(data[128..132].try_into().unwrap()) as usize;
        if count > (data.len() - 132) / 12 {
            return Err(Error::Unsupported("malformed ICC profile".into()));
        }
        // Device to PCS: relative colorimetric first (what a viewer uses for photographs),
        // then perceptual. Matrix/TRC is the fallback when there is no usable LUT.
        let mut lut_error = None;
        for signature in [b"A2B1", b"A2B0"] {
            let Some(tag) = tag(data, count, signature) else {
                continue;
            };
            match pcs {
                Some(pcs) => match parse_lut(tag, pcs) {
                    Ok(lut) => return Ok(Self::Lut(lut)),
                    Err(error) => lut_error = Some(error),
                },
                None => {
                    lut_error = Some(Error::Unsupported(
                        "ICC profile with an unknown connection space".into(),
                    ))
                }
            }
        }
        let matrix = MatrixTrc::parse(data, count).map_err(|error| lut_error.unwrap_or(error))?;
        Ok(Self::MatrixTrc(matrix))
    }

    /// The image with its samples converted to sRGB.
    pub(crate) fn to_srgb<S: Sample>(&self, image: &Image<S>) -> Image<S> {
        match self {
            Self::MatrixTrc(profile) => profile.to_srgb(image),
            Self::Lut(profile) => profile.to_srgb(image),
        }
    }
}

impl MatrixTrc {
    fn parse(data: &[u8], count: usize) -> Result<Self, Error> {
        if data.get(20..24) != Some(b"XYZ ") {
            return Err(Error::Unsupported(
                "ICC profile that is not an XYZ-connected RGB profile".into(),
            ));
        }
        let xyz = |signature: &[u8; 4]| -> Option<[f32; 3]> {
            let tag = tag(data, count, signature)?;
            if tag.get(..4) != Some(b"XYZ ") {
                return None;
            }
            Some(std::array::from_fn(|i| fixed(tag, 8 + 4 * i)))
        };
        let matrix = |signature: &[u8; 4]| {
            xyz(signature).ok_or_else(|| Error::Unsupported("ICC profile without matrix".into()))
        };
        let (r, g, b) = (matrix(b"rXYZ")?, matrix(b"gXYZ")?, matrix(b"bXYZ")?);
        let to_xyz = [[r[0], g[0], b[0]], [r[1], g[1], b[1]], [r[2], g[2], b[2]]];
        // The colourants map RGB to the profile connection space, and their sum is the white
        // that R = G = B = 1 maps to: D50 for a matrix adapted to it (the norm in v4, and in
        // many v2 profiles), the media white otherwise. Adapting from that white to D65 is what
        // a relative-colourimetric conversion to sRGB does; the `wtpt` tag, which describes the
        // media white in absolute terms, is not needed for it.
        let white = mul(to_xyz, [1.0, 1.0, 1.0]);
        let matrix = compose(XYZ_TO_SRGB, compose(adapt(white, D65), to_xyz));

        let curve = |signature: &[u8; 4]| {
            tag(data, count, signature)
                .and_then(parse_curve)
                .ok_or_else(|| Error::Unsupported("ICC profile without tone curves".into()))
        };
        let curves = [curve(b"rTRC")?, curve(b"gTRC")?, curve(b"bTRC")?];
        Ok(Self { curves, matrix })
    }

    /// The image with its samples converted to sRGB.
    fn to_srgb<S: Sample>(&self, image: &Image<S>) -> Image<S> {
        convert(image, self.matrix, |c, x| self.curves[c].apply(x))
    }
}

/// The profile connection space of a LUT-based profile.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pcs {
    Xyz,
    Lab,
}

/// A parsed LUT-based RGB profile, from device values to the connection space.
pub(crate) struct LutProfile {
    /// The LUT stages, in execution order.
    stages: Vec<Stage>,
    pcs: Pcs,
    /// `lut16` tags with a Lab connection space use the legacy v2 Lab encoding.
    lab_v2: bool,
}

impl LutProfile {
    /// The image with its samples converted to sRGB.
    fn to_srgb<S: Sample>(&self, image: &Image<S>) -> Image<S> {
        let max = image.max_value();
        let scale = 1.0 / f32::from(max);
        let encode = SrgbEncoder::new(max);
        // The PCS is D50; sRGB is D65.
        let to_srgb = compose(XYZ_TO_SRGB, adapt(D50, D65));

        let mut data = Vec::with_capacity(image.data.len());
        for pixel in image.data.chunks_exact(4) {
            let mut value = [
                f32::from(pixel[0].to_u16()) * scale,
                f32::from(pixel[1].to_u16()) * scale,
                f32::from(pixel[2].to_u16()) * scale,
            ];
            for stage in &self.stages {
                value = stage.apply(value);
            }
            let linear = mul(to_srgb, self.pcs_to_xyz(value));
            data.push(S::from_u16(encode.encode(linear[0])));
            data.push(S::from_u16(encode.encode(linear[1])));
            data.push(S::from_u16(encode.encode(linear[2])));
            data.push(pixel[3]);
        }
        Image {
            width: image.width,
            height: image.height,
            bit_depth: image.bit_depth,
            has_alpha: image.has_alpha,
            data,
        }
    }

    /// Decodes a PCS value from the LUT's normalized encoding.
    fn pcs_to_xyz(&self, value: [f32; 3]) -> [f32; 3] {
        match self.pcs {
            Pcs::Xyz => [
                value[0] * MAX_ENCODEABLE_XYZ,
                value[1] * MAX_ENCODEABLE_XYZ,
                value[2] * MAX_ENCODEABLE_XYZ,
            ],
            Pcs::Lab => {
                let value = if self.lab_v2 {
                    // A v2 lut16 encodes L* over 0..65280 and a*/b* over 0..65535.
                    let scale = 65535.0 / 65280.0;
                    [value[0] * scale, value[1] * scale, value[2] * scale]
                } else {
                    value
                };
                lab_to_xyz([
                    value[0] * 100.0,
                    value[1] * 255.0 - 128.0,
                    value[2] * 255.0 - 128.0,
                ])
            }
        }
    }
}

/// One stage of a LUT-based transform.
enum Stage {
    /// Per-channel curves.
    Curves([Curve; 3]),
    /// A 3×3 matrix, optionally with an offset.
    Matrix {
        matrix: [[f32; 3]; 3],
        offset: [f32; 3],
    },
    /// A three-dimensional lookup table.
    Clut(Clut),
}

impl Stage {
    fn apply(&self, value: [f32; 3]) -> [f32; 3] {
        match self {
            Self::Curves(curves) => [
                curves[0].apply(value[0]),
                curves[1].apply(value[1]),
                curves[2].apply(value[2]),
            ],
            Self::Matrix { matrix, offset } => {
                let mixed = mul(*matrix, value);
                [
                    mixed[0] + offset[0],
                    mixed[1] + offset[1],
                    mixed[2] + offset[2],
                ]
            }
            Self::Clut(clut) => clut.apply(value),
        }
    }
}

/// A three-dimensional CLUT: the first input channel varies slowest, and the three output
/// values of a grid point are consecutive.
struct Clut {
    grid: [usize; 3],
    values: Vec<f32>,
}

impl Clut {
    /// Tetrahedral interpolation, as littleCMS and printer profiles use: unlike trilinear
    /// interpolation it does not shift neutrals on a coarsely sampled CLUT.
    fn apply(&self, value: [f32; 3]) -> [f32; 3] {
        let [g0, g1, g2] = self.grid;
        let position = [
            value[0].clamp(0.0, 1.0) * (g0 - 1) as f32,
            value[1].clamp(0.0, 1.0) * (g1 - 1) as f32,
            value[2].clamp(0.0, 1.0) * (g2 - 1) as f32,
        ];
        // The low corner is clamped one short of the end so that a position of exactly 1.0
        // still has a cell, with a fraction of 1.0.
        let low = [
            (position[0].floor() as usize).min(g0 - 2),
            (position[1].floor() as usize).min(g1 - 2),
            (position[2].floor() as usize).min(g2 - 2),
        ];
        let high = [
            (low[0] + 1).min(g0 - 1),
            (low[1] + 1).min(g1 - 1),
            (low[2] + 1).min(g2 - 1),
        ];
        let fraction = [
            position[0] - low[0] as f32,
            position[1] - low[1] as f32,
            position[2] - low[2] as f32,
        ];
        let at =
            |i: usize, j: usize, k: usize, c: usize| self.values[((i * g1 + j) * g2 + k) * 3 + c];
        let [rx, ry, rz] = fraction;
        let (x0, y0, z0) = (low[0], low[1], low[2]);
        let (x1, y1, z1) = (high[0], high[1], high[2]);
        let mut out = [0.0f32; 3];
        for (c, channel) in out.iter_mut().enumerate() {
            let c0 = at(x0, y0, z0, c);
            let (c1, c2, c3) = if rx >= ry && ry >= rz {
                (
                    at(x1, y0, z0, c) - c0,
                    at(x1, y1, z0, c) - at(x1, y0, z0, c),
                    at(x1, y1, z1, c) - at(x1, y1, z0, c),
                )
            } else if rx >= rz && rz >= ry {
                (
                    at(x1, y0, z0, c) - c0,
                    at(x1, y1, z1, c) - at(x1, y0, z1, c),
                    at(x1, y0, z1, c) - at(x1, y0, z0, c),
                )
            } else if rz >= rx && rx >= ry {
                (
                    at(x1, y0, z1, c) - at(x0, y0, z1, c),
                    at(x1, y1, z1, c) - at(x1, y0, z1, c),
                    at(x0, y0, z1, c) - c0,
                )
            } else if ry >= rx && rx >= rz {
                (
                    at(x1, y1, z0, c) - at(x0, y1, z0, c),
                    at(x0, y1, z0, c) - c0,
                    at(x1, y1, z1, c) - at(x1, y1, z0, c),
                )
            } else if ry >= rz && rz >= rx {
                (
                    at(x1, y1, z1, c) - at(x0, y1, z1, c),
                    at(x0, y1, z0, c) - c0,
                    at(x0, y1, z1, c) - at(x0, y1, z0, c),
                )
            } else if rz >= ry && ry >= rx {
                (
                    at(x1, y1, z1, c) - at(x0, y1, z1, c),
                    at(x0, y1, z1, c) - at(x0, y0, z1, c),
                    at(x0, y0, z1, c) - c0,
                )
            } else {
                (0.0, 0.0, 0.0)
            };
            *channel = c0 + c1 * rx + c2 * ry + c3 * rz;
        }
        out
    }
}

/// Converts a Lab colour (D50) to XYZ.
fn lab_to_xyz([l, a, b]: [f32; 3]) -> [f32; 3] {
    let fy = (l + 16.0) / 116.0;
    let fx = fy + a / 500.0;
    let fz = fy - b / 200.0;
    let inverse = |t: f32| {
        const DELTA: f32 = 6.0 / 29.0;
        if t > DELTA {
            t * t * t
        } else {
            3.0 * DELTA * DELTA * (t - 4.0 / 29.0)
        }
    };
    [
        inverse(fx) * D50[0],
        inverse(fy) * D50[1],
        inverse(fz) * D50[2],
    ]
}

/// Parses an `A2B` tag into a transform to the connection space `pcs`.
fn parse_lut(tag: &[u8], pcs: Pcs) -> Result<LutProfile, Error> {
    match tag.get(..4) {
        Some(b"mft1") => parse_lut8(tag, pcs),
        Some(b"mft2") => parse_lut16(tag, pcs),
        Some(b"mAB ") => parse_lut_a_to_b(tag, pcs),
        _ => Err(Error::Unsupported(
            "ICC profile with an unsupported LUT type".into(),
        )),
    }
}

fn malformed_lut() -> Error {
    Error::Unsupported("malformed LUT in ICC profile".into())
}

/// Parses a `lut8` (`mft1`) tag: matrix, input tables, CLUT, output tables.
fn parse_lut8(tag: &[u8], pcs: Pcs) -> Result<LutProfile, Error> {
    if tag.len() < 48 {
        return Err(malformed_lut());
    }
    let (input, output, grid) = (tag[8], tag[9], usize::from(tag[10]));
    if input != 3 || output != 3 {
        return Err(Error::Unsupported(
            "ICC LUT that does not map RGB to a three-channel connection space".into(),
        ));
    }
    if grid == 1 {
        return Err(malformed_lut());
    }
    let matrix = matrix_at(tag, 12)?;
    let mut offset = 48;
    let input_curves = curves_u8(tag, &mut offset, 3)?;
    // The specification leaves the 8-bit PCSXYZ encoding implementation-specific. Follow
    // littleCMS: the CLUT bytes are the PCS XYZ values normalised over `MAX_ENCODEABLE_XYZ`.
    let clut = if grid == 0 {
        None
    } else {
        Some(clut_u8(tag, &mut offset, [grid; 3], 3)?)
    };
    let output_curves = curves_u8(tag, &mut offset, 3)?;
    let mut stages = Vec::with_capacity(4);
    if matrix != IDENTITY {
        stages.push(Stage::Matrix {
            matrix,
            offset: [0.0; 3],
        });
    }
    stages.push(Stage::Curves(input_curves));
    if let Some(clut) = clut {
        stages.push(Stage::Clut(clut));
    }
    stages.push(Stage::Curves(output_curves));
    Ok(LutProfile {
        stages,
        pcs,
        lab_v2: false,
    })
}

/// Parses a `lut16` (`mft2`) tag: matrix, input tables, CLUT, output tables.
fn parse_lut16(tag: &[u8], pcs: Pcs) -> Result<LutProfile, Error> {
    if tag.len() < 52 {
        return Err(malformed_lut());
    }
    let (input, output, grid) = (tag[8], tag[9], usize::from(tag[10]));
    if input != 3 || output != 3 {
        return Err(Error::Unsupported(
            "ICC LUT that does not map RGB to a three-channel connection space".into(),
        ));
    }
    if grid == 1 {
        return Err(malformed_lut());
    }
    let matrix = matrix_at(tag, 12)?;
    let input_entries = usize::from(u16_at(tag, 48).ok_or_else(malformed_lut)?);
    let output_entries = usize::from(u16_at(tag, 50).ok_or_else(malformed_lut)?);
    let mut offset = 52;
    let input_curves = curves_u16(tag, &mut offset, 3, input_entries)?;
    let clut = if grid == 0 {
        None
    } else {
        Some(clut_u16(tag, &mut offset, [grid; 3], 3)?)
    };
    let output_curves = curves_u16(tag, &mut offset, 3, output_entries)?;
    let mut stages = Vec::with_capacity(4);
    if matrix != IDENTITY {
        stages.push(Stage::Matrix {
            matrix,
            offset: [0.0; 3],
        });
    }
    stages.push(Stage::Curves(input_curves));
    if let Some(clut) = clut {
        stages.push(Stage::Clut(clut));
    }
    stages.push(Stage::Curves(output_curves));
    Ok(LutProfile {
        stages,
        pcs,
        // A `lut16` tag with a Lab PCS uses the legacy v2 Lab encoding, as littleCMS reads it.
        lab_v2: pcs == Pcs::Lab,
    })
}

/// Parses a `lutAtoB` (`mAB `) tag: A curves, CLUT, M curves, matrix, B curves.
fn parse_lut_a_to_b(tag: &[u8], pcs: Pcs) -> Result<LutProfile, Error> {
    if tag.len() < 32 {
        return Err(malformed_lut());
    }
    let (input, output) = (tag[8], tag[9]);
    if input != 3 || output != 3 {
        return Err(Error::Unsupported(
            "ICC LUT that does not map RGB to a three-channel connection space".into(),
        ));
    }
    let offset_b = u32_at(tag, 12).ok_or_else(malformed_lut)? as usize;
    let offset_matrix = u32_at(tag, 16).ok_or_else(malformed_lut)? as usize;
    let offset_m = u32_at(tag, 20).ok_or_else(malformed_lut)? as usize;
    let offset_c = u32_at(tag, 24).ok_or_else(malformed_lut)? as usize;
    let offset_a = u32_at(tag, 28).ok_or_else(malformed_lut)? as usize;

    let mut stages = Vec::with_capacity(5);
    if offset_a != 0 {
        stages.push(Stage::Curves(curve_set(tag, offset_a, 3)?));
    }
    if offset_c != 0 {
        stages.push(Stage::Clut(clut_a_to_b(tag, offset_c, 3)?));
    }
    if offset_m != 0 {
        stages.push(Stage::Curves(curve_set(tag, offset_m, 3)?));
    }
    if offset_matrix != 0 {
        let (matrix, offset) = matrix_and_offset_at(tag, offset_matrix)?;
        stages.push(Stage::Matrix { matrix, offset });
    }
    if offset_b != 0 {
        stages.push(Stage::Curves(curve_set(tag, offset_b, 3)?));
    }
    if stages.is_empty() {
        return Err(malformed_lut());
    }
    Ok(LutProfile {
        stages,
        pcs,
        lab_v2: false,
    })
}

/// The number of `f32` values in a CLUT, with a cap against malicious profiles.
fn clut_len(grid: [usize; 3], outputs: usize) -> Result<usize, Error> {
    let points = grid
        .iter()
        .try_fold(1usize, |product, &g| product.checked_mul(g))
        .ok_or_else(malformed_lut)?;
    let count = points.checked_mul(outputs).ok_or_else(malformed_lut)?;
    if count > (1 << 23) {
        return Err(Error::Unsupported("ICC CLUT that is too large".into()));
    }
    Ok(count)
}

fn matrix_at(data: &[u8], offset: usize) -> Result<[[f32; 3]; 3], Error> {
    let mut matrix = [[0.0; 3]; 3];
    for (i, row) in matrix.iter_mut().enumerate() {
        for (j, value) in row.iter_mut().enumerate() {
            let at = offset
                .checked_add(4 * (3 * i + j))
                .ok_or_else(malformed_lut)?;
            *value = fixed_at(data, at).ok_or_else(malformed_lut)?;
        }
    }
    Ok(matrix)
}

fn matrix_and_offset_at(data: &[u8], offset: usize) -> Result<([[f32; 3]; 3], [f32; 3]), Error> {
    let matrix = matrix_at(data, offset)?;
    let offset = [
        fixed_at(data, offset.checked_add(36).ok_or_else(malformed_lut)?)
            .ok_or_else(malformed_lut)?,
        fixed_at(data, offset.checked_add(40).ok_or_else(malformed_lut)?)
            .ok_or_else(malformed_lut)?,
        fixed_at(data, offset.checked_add(44).ok_or_else(malformed_lut)?)
            .ok_or_else(malformed_lut)?,
    ];
    Ok((matrix, offset))
}

/// Reads three 256-entry byte tables.
fn curves_u8(data: &[u8], offset: &mut usize, channels: usize) -> Result<[Curve; 3], Error> {
    let mut curves = Vec::with_capacity(channels);
    for _ in 0..channels {
        let end = (*offset).checked_add(256).ok_or_else(malformed_lut)?;
        let bytes = data.get(*offset..end).ok_or_else(malformed_lut)?;
        *offset = end;
        curves.push(Curve::Sampled(
            bytes.iter().map(|&v| f32::from(v) / 255.0).collect(),
        ));
    }
    curves.try_into().map_err(|_| malformed_lut())
}

/// Reads three `entries`-entry 16-bit tables.
fn curves_u16(
    data: &[u8],
    offset: &mut usize,
    channels: usize,
    entries: usize,
) -> Result<[Curve; 3], Error> {
    if entries == 0 {
        return Ok(std::array::from_fn(|_| Curve::Identity));
    }
    if entries < 2 {
        return Err(malformed_lut());
    }
    let mut curves = Vec::with_capacity(channels);
    for _ in 0..channels {
        let mut values = Vec::with_capacity(entries);
        for _ in 0..entries {
            let value = u16_at(data, *offset).ok_or_else(malformed_lut)?;
            *offset += 2;
            values.push(f32::from(value) / 65535.0);
        }
        curves.push(Curve::Sampled(values));
    }
    curves.try_into().map_err(|_| malformed_lut())
}

fn clut_u8(
    data: &[u8],
    offset: &mut usize,
    grid: [usize; 3],
    outputs: usize,
) -> Result<Clut, Error> {
    let count = clut_len(grid, outputs)?;
    let end = (*offset).checked_add(count).ok_or_else(malformed_lut)?;
    let bytes = data.get(*offset..end).ok_or_else(malformed_lut)?;
    *offset = end;
    Ok(Clut {
        grid,
        values: bytes.iter().map(|&v| f32::from(v) / 255.0).collect(),
    })
}

fn clut_u16(
    data: &[u8],
    offset: &mut usize,
    grid: [usize; 3],
    outputs: usize,
) -> Result<Clut, Error> {
    let count = clut_len(grid, outputs)?;
    let end = (*offset)
        .checked_add(count.checked_mul(2).ok_or_else(malformed_lut)?)
        .ok_or_else(malformed_lut)?;
    let bytes = data.get(*offset..end).ok_or_else(malformed_lut)?;
    *offset = end;
    Ok(Clut {
        grid,
        values: bytes
            .chunks_exact(2)
            .map(|pair| f32::from(u16::from_be_bytes([pair[0], pair[1]])) / 65535.0)
            .collect(),
    })
}

fn clut_a_to_b(data: &[u8], offset: usize, outputs: usize) -> Result<Clut, Error> {
    let start = offset.checked_add(20).ok_or_else(malformed_lut)?;
    let header = data.get(offset..start).ok_or_else(malformed_lut)?;
    let grid = [
        usize::from(header[0]),
        usize::from(header[1]),
        usize::from(header[2]),
    ];
    if grid.iter().any(|&g| g < 2) {
        return Err(malformed_lut());
    }
    let precision = header[16];
    let count = clut_len(grid, outputs)?;
    let values = match precision {
        1 => {
            let end = start.checked_add(count).ok_or_else(malformed_lut)?;
            let bytes = data.get(start..end).ok_or_else(malformed_lut)?;
            bytes.iter().map(|&v| f32::from(v) / 255.0).collect()
        }
        2 => {
            let end = start
                .checked_add(count.checked_mul(2).ok_or_else(malformed_lut)?)
                .ok_or_else(malformed_lut)?;
            let bytes = data.get(start..end).ok_or_else(malformed_lut)?;
            bytes
                .chunks_exact(2)
                .map(|pair| f32::from(u16::from_be_bytes([pair[0], pair[1]])) / 65535.0)
                .collect()
        }
        _ => {
            return Err(Error::Unsupported(
                "ICC CLUT with an unknown precision".into(),
            ));
        }
    };
    Ok(Clut { grid, values })
}

/// Reads a set of embedded `curv`/`para` curves, each padded to a four-byte boundary.
fn curve_set(data: &[u8], offset: usize, channels: usize) -> Result<[Curve; 3], Error> {
    let mut at = offset;
    let mut curves = Vec::with_capacity(channels);
    for _ in 0..channels {
        let (curve, size) = parse_curve_at(data, at).ok_or_else(malformed_lut)?;
        curves.push(curve);
        at = at
            .checked_add(size)
            .and_then(|end| end.checked_add(3))
            .ok_or_else(malformed_lut)?
            & !3;
    }
    curves.try_into().map_err(|_| malformed_lut())
}

/// The data of a tag, or `None` if absent or out of bounds.
fn tag<'a>(data: &'a [u8], count: usize, signature: &[u8; 4]) -> Option<&'a [u8]> {
    for k in 0..count {
        let entry = 132 + 12 * k;
        if data.get(entry..entry + 4)? != signature {
            continue;
        }
        let offset = u32::from_be_bytes(data.get(entry + 4..entry + 8)?.try_into().ok()?) as usize;
        let size = u32::from_be_bytes(data.get(entry + 8..entry + 12)?.try_into().ok()?) as usize;
        return data.get(offset..offset.checked_add(size)?);
    }
    None
}

/// A big-endian `u16` at `offset`, or `None` if out of bounds.
fn u16_at(data: &[u8], offset: usize) -> Option<u16> {
    let end = offset.checked_add(2)?;
    Some(u16::from_be_bytes(data.get(offset..end)?.try_into().ok()?))
}

/// A big-endian `u32` at `offset`, or `None` if out of bounds.
fn u32_at(data: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    Some(u32::from_be_bytes(data.get(offset..end)?.try_into().ok()?))
}

/// An `s15Fixed16Number` at `offset`, or `None` if out of bounds.
fn fixed_at(data: &[u8], offset: usize) -> Option<f32> {
    let end = offset.checked_add(4)?;
    let value = i32::from_be_bytes(data.get(offset..end)?.try_into().ok()?);
    Some(value as f32 / 65536.0)
}

/// An `s15Fixed16Number` at `offset`, zero when out of bounds (for tags where the parameter
/// count is known).
fn fixed(data: &[u8], offset: usize) -> f32 {
    fixed_at(data, offset).unwrap_or(0.0)
}

/// Parses the `curv` or `para` tag at `offset`, returning the curve and the number of bytes it
/// occupies, without alignment padding.
fn parse_curve_at(data: &[u8], offset: usize) -> Option<(Curve, usize)> {
    let data = data.get(offset..)?;
    if data.len() < 12 {
        return None;
    }
    match data.get(..4)? {
        b"curv" => {
            let count = u32::from_be_bytes(data.get(8..12)?.try_into().ok()?) as usize;
            match count {
                0 => Some((Curve::Identity, 12)),
                1 => {
                    let gamma = u16::from_be_bytes(data.get(12..14)?.try_into().ok()?);
                    Some((Curve::Gamma(gamma as f32 / 256.0), 14))
                }
                n => {
                    if n > (data.len() - 12) / 2 {
                        return None;
                    }
                    let values = (0..n)
                        .map(|i| {
                            let v = u16::from_be_bytes(
                                data.get(12 + 2 * i..14 + 2 * i)?.try_into().ok()?,
                            );
                            Some(v as f32 / 65535.0)
                        })
                        .collect::<Option<Vec<f32>>>()?;
                    Some((Curve::Sampled(values), 12 + 2 * n))
                }
            }
        }
        b"para" => {
            let function = u16::from_be_bytes(data.get(8..10)?.try_into().ok()?);
            let count = match function {
                0 => 1,
                1 => 3,
                2 => 4,
                3 => 5,
                4 => 7,
                _ => return None,
            };
            let mut params = [0.0f32; 7];
            for (i, p) in params.iter_mut().take(count).enumerate() {
                *p = fixed_at(data, 12 + 4 * i)?;
            }
            Some((Curve::Parametric { function, params }, 12 + 4 * count))
        }
        _ => None,
    }
}

/// Parses a `curv` or `para` tag.
fn parse_curve(data: &[u8]) -> Option<Curve> {
    parse_curve_at(data, 0).map(|(curve, _)| curve)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Display P3 colourants adapted to D50, as phones and cameras write them: the columns
    /// are the rXYZ, gXYZ and bXYZ tags.
    const P3: [[f32; 3]; 3] = [
        [0.515_076, 0.291_946, 0.157_181],
        [0.241_165, 0.692_230, 0.066_589],
        [-0.001_053, 0.041_885, 0.784_546],
    ];

    /// The `para` type 3 tag of the sRGB curve (`g`, `a`, `b`, `c`, `d`).
    fn para3() -> Vec<u8> {
        let mut out = b"para\0\0\0\0\0\x03\0\0".to_vec();
        for value in [2.4f32, 1.0 / 1.055, 0.055 / 1.055, 0.077_399, 0.040_45] {
            out.extend_from_slice(&((value * 65536.0) as i32).to_be_bytes());
        }
        out
    }

    /// Builds a minimal matrix/TRC ICC profile in memory. `white` is written as the `wtpt` tag;
    /// the conversion ignores it (the colourants' sum is what matters).
    fn profile(columns: [[f32; 3]; 3], white: [f32; 3], curve: &[u8]) -> Vec<u8> {
        profile_with_tags(columns, white, curve, &[])
    }

    /// Like [`profile`], with extra tags appended to the table.
    fn profile_with_tags(
        columns: [[f32; 3]; 3],
        white: [f32; 3],
        curve: &[u8],
        extra: &[(&[u8; 4], Vec<u8>)],
    ) -> Vec<u8> {
        let xyz = |v: [f32; 3]| {
            let mut out = b"XYZ \0\0\0\0".to_vec();
            for value in v {
                out.extend_from_slice(&((value * 65536.0) as i32).to_be_bytes());
            }
            out
        };
        // Columns hold the colourant vectors: tag data is each column transposed back.
        let mut tags: Vec<(&[u8; 4], Vec<u8>)> = vec![
            (b"wtpt", xyz(white)),
            (b"rXYZ", xyz([columns[0][0], columns[1][0], columns[2][0]])),
            (b"gXYZ", xyz([columns[0][1], columns[1][1], columns[2][1]])),
            (b"bXYZ", xyz([columns[0][2], columns[1][2], columns[2][2]])),
            (b"rTRC", curve.to_vec()),
            (b"gTRC", curve.to_vec()),
            (b"bTRC", curve.to_vec()),
        ];
        tags.extend_from_slice(extra);
        let mut header = vec![0u8; 128];
        header[16..20].copy_from_slice(b"RGB ");
        header[20..24].copy_from_slice(b"XYZ ");
        header[36..40].copy_from_slice(b"acsp");
        let mut out = header;
        out.extend_from_slice(&(tags.len() as u32).to_be_bytes());
        let mut offset = 132 + 12 * tags.len();
        for (signature, data) in &tags {
            out.extend_from_slice(*signature);
            out.extend_from_slice(&(offset as u32).to_be_bytes());
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            offset += data.len();
        }
        for (_, data) in &tags {
            out.extend_from_slice(data);
        }
        out
    }

    fn image(bit_depth: u8, pixels: &[[u16; 3]]) -> Image {
        let mut data = Vec::with_capacity(pixels.len() * 4);
        for p in pixels {
            data.extend_from_slice(&[p[0], p[1], p[2], 65535]);
        }
        Image {
            width: pixels.len() as u32,
            height: 1,
            bit_depth,
            has_alpha: false,
            data,
        }
    }

    /// A big-endian `s15Fixed16Number`.
    fn s15(value: f32) -> [u8; 4] {
        ((value * 65536.0) as i32).to_be_bytes()
    }

    /// An ICC `XYZ ` tag.
    fn xyz_tag(value: [f32; 3]) -> Vec<u8> {
        let mut out = b"XYZ \0\0\0\0".to_vec();
        for v in value {
            out.extend_from_slice(&s15(v));
        }
        out
    }

    /// A minimal `desc` tag: the ASCII name, then empty Unicode and ScriptCode fields.
    fn desc_tag(text: &str) -> Vec<u8> {
        let mut out = b"desc\0\0\0\0".to_vec();
        out.extend_from_slice(&(text.len() as u32 + 1).to_be_bytes());
        out.extend_from_slice(text.as_bytes());
        out.push(0);
        out.extend_from_slice(&0u32.to_be_bytes()); // Unicode language code
        out.extend_from_slice(&0u32.to_be_bytes()); // Unicode count
        out.extend_from_slice(&0u16.to_be_bytes()); // ScriptCode code
        out.push(0); // ScriptCode count
        out.extend_from_slice(&[0u8; 67]); // ScriptCode string
        out
    }

    /// A `text` tag.
    fn text_tag(text: &str) -> Vec<u8> {
        let mut out = b"text\0\0\0\0".to_vec();
        out.extend_from_slice(text.as_bytes());
        out.push(0);
        out
    }

    /// Builds an RGB ICC profile with `tags`, version `(major, minor)`, and the given PCS.
    /// Tag data is padded to the four-byte boundary the format requires.
    fn icc_profile(version: (u8, u8), pcs: &[u8; 4], tags: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let padded: Vec<Vec<u8>> = tags
            .iter()
            .map(|(_, data)| {
                let mut data = data.clone();
                while data.len() % 4 != 0 {
                    data.push(0);
                }
                data
            })
            .collect();
        let mut out = vec![0u8; 128];
        out[4..8].copy_from_slice(b"lcms");
        out[8] = version.0;
        out[9] = version.1 << 4;
        out[12..16].copy_from_slice(b"mntr");
        out[16..20].copy_from_slice(b"RGB ");
        out[20..24].copy_from_slice(pcs);
        out[36..40].copy_from_slice(b"acsp");
        for (i, v) in [0.9642f32, 1.0, 0.8249].iter().enumerate() {
            out[68 + 4 * i..72 + 4 * i].copy_from_slice(&s15(*v));
        }
        out[80..84].copy_from_slice(b"lcms");
        out.extend_from_slice(&(tags.len() as u32).to_be_bytes());
        let mut offset = 132 + 12 * tags.len();
        for ((signature, _), data) in tags.iter().zip(&padded) {
            out.extend_from_slice(*signature);
            out.extend_from_slice(&(offset as u32).to_be_bytes());
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            offset += data.len();
        }
        for data in &padded {
            out.extend_from_slice(data);
        }
        let size = out.len() as u32;
        out[..4].copy_from_slice(&size.to_be_bytes());
        out
    }

    /// An RGB profile with the usual descriptive tags plus the supplied LUT tables.
    fn lut_profile(version: (u8, u8), pcs: &[u8; 4], tables: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut tags: Vec<(&[u8; 4], Vec<u8>)> = vec![
            (b"desc", desc_tag("heifer LUT test")),
            (b"wtpt", xyz_tag([0.9642, 1.0, 0.8249])),
            (b"cprt", text_tag("public domain")),
        ];
        tags.extend_from_slice(tables);
        icc_profile(version, pcs, &tags)
    }

    /// A 256-entry identity byte table.
    fn identity_u8() -> Vec<u8> {
        (0..=255).collect()
    }

    /// A 256-entry identity 16-bit table.
    fn identity_u16() -> Vec<u16> {
        (0..=255).map(|i| i * 257).collect()
    }

    fn curve_set(curves: &[Vec<u8>; 3]) -> Vec<u8> {
        let mut out = Vec::new();
        for curve in curves {
            out.extend_from_slice(curve);
            while out.len() % 4 != 0 {
                out.push(0);
            }
        }
        out
    }

    /// A `lut8` (`mft1`) tag.
    fn lut8_tag(
        grid: u8,
        matrix: [[f32; 3]; 3],
        input: &[Vec<u8>; 3],
        clut: &[u8],
        output: &[Vec<u8>; 3],
    ) -> Vec<u8> {
        let mut out = b"mft1\0\0\0\0".to_vec();
        out.extend_from_slice(&[3, 3, grid, 0]);
        for row in matrix {
            for value in row {
                out.extend_from_slice(&s15(value));
            }
        }
        for table in input {
            out.extend_from_slice(table);
        }
        out.extend_from_slice(clut);
        for table in output {
            out.extend_from_slice(table);
        }
        out
    }

    /// A `lut16` (`mft2`) tag.
    fn lut16_tag(
        grid: u8,
        matrix: [[f32; 3]; 3],
        input: &[Vec<u16>; 3],
        clut: &[u16],
        output: &[Vec<u16>; 3],
    ) -> Vec<u8> {
        let mut out = b"mft2\0\0\0\0".to_vec();
        out.extend_from_slice(&[3, 3, grid, 0]);
        for row in matrix {
            for value in row {
                out.extend_from_slice(&s15(value));
            }
        }
        out.extend_from_slice(&(input[0].len() as u16).to_be_bytes());
        out.extend_from_slice(&(output[0].len() as u16).to_be_bytes());
        for table in input {
            for value in table {
                out.extend_from_slice(&value.to_be_bytes());
            }
        }
        for value in clut {
            out.extend_from_slice(&value.to_be_bytes());
        }
        for table in output {
            for value in table {
                out.extend_from_slice(&value.to_be_bytes());
            }
        }
        out
    }

    /// A `lutAtoB` (`mAB `) tag.
    fn mab_tag(
        grid: [u8; 3],
        a: Option<&[Vec<u8>; 3]>,
        clut: Option<(u8, &[u8])>,
        m: Option<&[Vec<u8>; 3]>,
        matrix: Option<([[f32; 3]; 3], [f32; 3])>,
        b: Option<&[Vec<u8>; 3]>,
    ) -> Vec<u8> {
        let mut out = b"mAB \0\0\0\0".to_vec();
        out.extend_from_slice(&[3, 3, 0, 0]);
        out.extend_from_slice(&[0u8; 20]);
        let mut offsets = [0u32; 5]; // B, matrix, M, CLUT, A
        if let Some(a) = a {
            offsets[4] = out.len() as u32;
            out.extend_from_slice(&curve_set(a));
        }
        if let Some((precision, values)) = clut {
            offsets[3] = out.len() as u32;
            out.extend_from_slice(&grid);
            out.extend_from_slice(&[0u8; 13]);
            out.push(precision);
            out.extend_from_slice(&[0u8; 3]);
            out.extend_from_slice(values);
            while out.len() % 4 != 0 {
                out.push(0);
            }
        }
        if let Some(m) = m {
            offsets[2] = out.len() as u32;
            out.extend_from_slice(&curve_set(m));
        }
        if let Some((matrix, offset)) = matrix {
            offsets[1] = out.len() as u32;
            for row in matrix {
                for value in row {
                    out.extend_from_slice(&s15(value));
                }
            }
            for value in offset {
                out.extend_from_slice(&s15(value));
            }
        }
        if let Some(b) = b {
            offsets[0] = out.len() as u32;
            out.extend_from_slice(&curve_set(b));
        }
        for (i, offset) in offsets.iter().enumerate() {
            out[12 + 4 * i..16 + 4 * i].copy_from_slice(&offset.to_be_bytes());
        }
        out
    }

    /// A `para` type 0 (pure gamma) curve.
    fn gamma_para(gamma: f32) -> Vec<u8> {
        let mut out = b"para\0\0\0\0\0\0\0\0".to_vec();
        out.extend_from_slice(&s15(gamma));
        out
    }

    /// A non-linear RGB to Lab CLUT, sampled on `grid` points per axis, for a v4 Lab PCS.
    fn lab_clut(grid: usize) -> Vec<u8> {
        let mut values = Vec::new();
        for i in 0..grid {
            for j in 0..grid {
                for k in 0..grid {
                    let r = i as f32 / (grid - 1) as f32;
                    let g = j as f32 / (grid - 1) as f32;
                    let b = k as f32 / (grid - 1) as f32;
                    // Cross terms make tetrahedral and trilinear interpolation differ.
                    let l = 0.25 + 0.5 * r + 0.15 * g + 0.05 * b + 0.05 * r * g;
                    let a = 0.5 + 0.25 * (r - g) + 0.1 * (g - b) + 0.05 * r * b;
                    let bb = 0.5 + 0.2 * (b - r) + 0.1 * (r - g) + 0.05 * g * b;
                    for value in [l, a, bb] {
                        values.push((value.clamp(0.0, 1.0) * 255.0).round() as u8);
                    }
                }
            }
        }
        values
    }

    /// The same CLUT at 16-bit precision.
    fn lab_clut_u16(grid: usize) -> Vec<u16> {
        lab_clut(grid)
            .into_iter()
            .map(|value| u16::from(value) * 257)
            .collect()
    }

    /// XYZ (D50) to the normalized PCS XYZ encoding.
    fn encode_xyz(xyz: [f32; 3]) -> [f32; 3] {
        [
            xyz[0] / MAX_ENCODEABLE_XYZ,
            xyz[1] / MAX_ENCODEABLE_XYZ,
            xyz[2] / MAX_ENCODEABLE_XYZ,
        ]
    }

    /// The sRGB to XYZ (D50) matrix as eight CLUT corners, for `lut8` tests.
    fn srgb_clut_u8() -> Vec<u8> {
        let mut clut = Vec::with_capacity(8 * 3);
        for i in 0..2 {
            for j in 0..2 {
                for k in 0..2 {
                    let xyz = mul(
                        [
                            [0.436_075, 0.385_065, 0.143_080],
                            [0.222_505, 0.716_879, 0.060_617],
                            [0.013_932, 0.097_105, 0.714_173],
                        ],
                        [i as f32, j as f32, k as f32],
                    );
                    for value in encode_xyz(xyz) {
                        clut.push((value * 255.0).round() as u8);
                    }
                }
            }
        }
        clut
    }

    /// The pixels the LUT tests convert, as 16-bit samples.
    const LUT_PIXELS: [[u16; 3]; 8] = [
        [65535, 0, 0],
        [0, 65535, 0],
        [0, 0, 65535],
        [13107, 32896, 52428],
        [40092, 20046, 50115],
        [65535, 65535, 65535],
        [32896, 32896, 32896],
        [2570, 5140, 7710],
    ];

    /// Converts `pixels` through the parsed profile, as 8-bit RGB.
    fn convert(bytes: &[u8], pixels: &[[u16; 3]]) -> Vec<u8> {
        Profile::parse(bytes)
            .unwrap()
            .to_srgb(&image(16, pixels))
            .to_rgb8()
    }

    /// Asserts the conversion of `pixels` against `expected`, within two 8-bit levels.
    fn assert_srgb(bytes: &[u8], pixels: &[[u16; 3]], expected: &[[u8; 3]]) {
        let out = convert(bytes, pixels);
        assert_eq!(out.len(), expected.len() * 3);
        for (i, want) in expected.iter().enumerate() {
            let got = &out[3 * i..3 * i + 3];
            for c in 0..3 {
                assert!(
                    (i32::from(got[c]) - i32::from(want[c])).abs() <= 2,
                    "pixel {i} channel {c}: got {got:?}, want {want:?}"
                );
            }
        }
    }

    #[test]
    fn lut8_profile_converts_to_srgb() {
        let table = identity_u8();
        let bytes = lut_profile(
            (2, 1),
            b"XYZ ",
            &[(
                b"A2B0",
                lut8_tag(
                    2,
                    IDENTITY,
                    &[table.clone(), table.clone(), table],
                    &srgb_clut_u8(),
                    &[identity_u8(), identity_u8(), identity_u8()],
                ),
            )],
        );
        // References from littleCMS 2.19 (`transicc -i<profile> -o*sRGB -t1 -w`).
        let expected = [
            [255, 0, 11],
            [13, 254, 0],
            [0, 18, 255],
            [122, 188, 231],
            [205, 151, 227],
            [255, 255, 255],
            [188, 188, 188],
            [55, 79, 96],
        ];
        assert_srgb(&bytes, &LUT_PIXELS, &expected);
    }

    #[test]
    fn lut16_profile_converts_to_srgb() {
        let input = identity_u16();
        let bytes = lut_profile(
            (2, 1),
            b"Lab ",
            &[(
                b"A2B0",
                lut16_tag(
                    3,
                    IDENTITY,
                    &[input.clone(), input.clone(), input],
                    &lab_clut_u16(3),
                    &[identity_u16(), identity_u16(), identity_u16()],
                ),
            )],
        );
        // References from littleCMS 2.19; it reads a `lut16` Lab tag as the legacy v2 encoding.
        let expected = [
            [255, 132, 235],
            [0, 111, 136],
            [43, 81, 0],
            [80, 123, 63],
            [192, 149, 125],
            [255, 247, 232],
            [157, 146, 143],
            [66, 70, 62],
        ];
        assert_srgb(&bytes, &LUT_PIXELS, &expected);
    }

    #[test]
    fn mab_profile_converts_to_srgb() {
        let a = [gamma_para(2.2), gamma_para(2.2), gamma_para(2.2)];
        let m = [gamma_para(0.9), gamma_para(0.9), gamma_para(0.9)];
        let b = [gamma_para(1.1), gamma_para(1.1), gamma_para(1.1)];
        let bytes = lut_profile(
            (4, 3),
            b"Lab ",
            &[(
                b"A2B0",
                mab_tag(
                    [3, 3, 3],
                    Some(&a),
                    Some((1, &lab_clut(3))),
                    Some(&m),
                    Some((
                        [[0.9, 0.05, 0.05], [0.05, 0.9, 0.05], [0.05, 0.05, 0.9]],
                        [0.01, 0.0, -0.01],
                    )),
                    Some(&b),
                ),
            )],
        );
        // References from littleCMS 2.19, through the A, CLUT, M, matrix and B stages.
        let expected = [
            [255, 135, 220],
            [0, 113, 143],
            [55, 89, 0],
            [58, 93, 54],
            [133, 110, 91],
            [255, 232, 216],
            [103, 102, 105],
            [61, 70, 75],
        ];
        assert_srgb(&bytes, &LUT_PIXELS, &expected);
    }

    #[test]
    fn relative_colorimetric_lut_is_preferred() {
        let clut = srgb_clut_u8();
        let table = |value| vec![value; 256];
        let black = lut8_tag(
            2,
            IDENTITY,
            &[table(0), table(0), table(0)],
            &clut,
            &[identity_u8(), identity_u8(), identity_u8()],
        );
        let white = lut8_tag(
            2,
            IDENTITY,
            &[table(255), table(255), table(255)],
            &clut,
            &[identity_u8(), identity_u8(), identity_u8()],
        );
        let red = [[65535, 0, 0]];
        let bytes = lut_profile(
            (2, 1),
            b"XYZ ",
            &[(b"A2B0", white.clone()), (b"A2B1", black)],
        );
        assert_srgb(&bytes, &red, &[[0, 0, 0]]);

        // A relative-colorimetric tag we cannot read falls back to the perceptual one.
        let mut broken = white.clone();
        broken[..4].copy_from_slice(b"junk");
        let bytes = lut_profile((2, 1), b"XYZ ", &[(b"A2B0", white), (b"A2B1", broken)]);
        assert_srgb(&bytes, &red, &[[255, 255, 255]]);
    }

    #[test]
    fn unusable_lut_falls_back_to_matrix_trc() {
        let mut tag = lut8_tag(
            2,
            IDENTITY,
            &[identity_u8(), identity_u8(), identity_u8()],
            &[],
            &[identity_u8(), identity_u8(), identity_u8()],
        );
        tag[..4].copy_from_slice(b"junk");
        let bytes = profile_with_tags(P3, [0.9642, 1.0, 0.8249], &para3(), &[(b"A2B0", tag)]);
        assert!(matches!(Profile::parse(&bytes), Ok(Profile::MatrixTrc(_))));
    }

    #[test]
    fn lut_profiles_reject_unsupported_inputs() {
        let table = identity_u8();
        let identity = || [identity_u8(), identity_u8(), identity_u8()];

        // A LUT that does not map three channels to three channels.
        let mut tag = lut8_tag(
            2,
            IDENTITY,
            &[table.clone(), table.clone(), table.clone()],
            &srgb_clut_u8(),
            &identity(),
        );
        tag[8] = 4;
        let bytes = lut_profile((2, 1), b"XYZ ", &[(b"A2B0", tag)]);
        assert!(Profile::parse(&bytes).is_err());

        // A profile in another input space is refused before its LUT is read.
        let bytes = lut_profile(
            (2, 1),
            b"XYZ ",
            &[(
                b"A2B0",
                lut8_tag(
                    2,
                    IDENTITY,
                    &[table.clone(), table.clone(), table.clone()],
                    &srgb_clut_u8(),
                    &identity(),
                ),
            )],
        );
        let mut bytes = bytes;
        bytes[16..20].copy_from_slice(b"CMYK");
        assert!(Profile::parse(&bytes).is_err());

        // A truncated 16-bit lookup table.
        let input = identity_u16();
        let mut tag = lut16_tag(
            2,
            IDENTITY,
            &[input.clone(), input.clone(), input],
            &[],
            &[identity_u16(), identity_u16(), identity_u16()],
        );
        tag.truncate(55);
        let bytes = lut_profile((2, 1), b"Lab ", &[(b"A2B0", tag)]);
        assert!(Profile::parse(&bytes).is_err());

        // A CLUT whose grid is too large to allocate.
        let tag = lut8_tag(
            255,
            IDENTITY,
            &[table.clone(), table.clone(), table],
            &[],
            &identity(),
        );
        let bytes = lut_profile((2, 1), b"XYZ ", &[(b"A2B0", tag)]);
        assert!(Profile::parse(&bytes).is_err());

        // An unknown LUT type.
        let mut tag = lut8_tag(
            2,
            IDENTITY,
            &[identity_u8(), identity_u8(), identity_u8()],
            &srgb_clut_u8(),
            &identity(),
        );
        tag[..4].copy_from_slice(b"junk");
        let bytes = lut_profile((2, 1), b"XYZ ", &[(b"A2B0", tag)]);
        assert!(Profile::parse(&bytes).is_err());

        // An `mAB` with no processing element at all.
        let tag = mab_tag([3, 3, 3], None, None, None, None, None);
        let bytes = lut_profile((4, 3), b"Lab ", &[(b"A2B0", tag)]);
        assert!(Profile::parse(&bytes).is_err());
    }

    #[test]
    fn p3_profile_converts_to_srgb() {
        let bytes = profile(P3, [0.964_203, 1.0, 0.824_905], &para3());
        let profile = Profile::parse(&bytes).unwrap();
        // Reference values from ColorSync's own conversion; our run over a real photo agreed
        // with ColorSync to 0.2/255 on average.
        let pixels = [
            [65535, 65535, 65535],
            [32768, 32768, 32768],
            [13107, 32768, 52428],
            [58982, 6554, 19661],
        ];
        let out = profile.to_srgb(&image(16, &pixels));
        let expected = [
            [65531, 65535, 65535],
            [32766, 32768, 32772],
            [0, 33301, 54021],
            [64433, 0, 18419],
        ];
        for (i, want) in expected.iter().enumerate() {
            for (c, want) in want.iter().enumerate() {
                let got = i32::from(out.data[i * 4 + c]);
                assert!(
                    (got - want).abs() <= 8,
                    "pixel {i} channel {c}: got {got}, want {want}"
                );
            }
        }
        assert_eq!(out.data[3], 65535);
    }

    #[test]
    fn primaries_match_the_profile_chain() {
        // Display P3 through `nclx` and through an equivalent ICC profile agree.
        let bytes = profile(P3, [0.964_203, 1.0, 0.824_905], &para3());
        let profile = Profile::parse(&bytes).unwrap();
        let pixels = [[13107u16, 32768, 52428]];
        let from_icc = profile.to_srgb(&image(16, &pixels));
        let from_nclx = from_primaries(&image(16, &pixels), 12, 13).unwrap();
        for c in 0..3 {
            let (a, b) = (i32::from(from_icc.data[c]), i32::from(from_nclx.data[c]));
            assert!((a - b).abs() <= 800, "channel {c}: {a} vs {b}");
        }
    }

    #[test]
    fn gamma_curves_are_read_from_curv_and_para() {
        // `curv` with a single value is a pure gamma, as in Adobe RGB profiles.
        let mut gamma = b"curv\0\0\0\0\0\0\0\x01".to_vec();
        gamma.extend_from_slice(&563u16.to_be_bytes()); // 563 / 256 ≈ 2.199
        match parse_curve(&gamma) {
            Some(curve) => assert!((curve.apply(0.5) - 0.5f32.powf(2.199)).abs() < 1e-4),
            None => panic!("a gamma curve was not read"),
        }
        // `curv` with no value is linear.
        assert_eq!(parse_curve(b"curv\0\0\0\0\0\0\0\0"), Some(Curve::Identity));
        // `para` function 0 stores a single gamma, as in DCI-P3 profiles.
        let mut para = b"para\0\0\0\0\0\0\0\0".to_vec();
        para.extend_from_slice(&((2.2f32 * 65536.0) as i32).to_be_bytes());
        match parse_curve(&para) {
            Some(curve) => assert!((curve.apply(0.5) - 0.5f32.powf(2.2)).abs() < 1e-4),
            None => panic!("a parametric gamma curve was not read"),
        }
    }

    #[test]
    fn srgb_and_unsupported_inputs() {
        let pixels = [[10u16, 20, 30]];
        let identity = from_primaries(&image(16, &pixels), 1, 13).unwrap();
        assert_eq!(&identity.data[..3], &[10, 20, 30]);

        assert!(from_primaries(&image(16, &pixels), 12, 16).is_err()); // PQ
        assert!(from_primaries(&image(16, &pixels), 42, 13).is_err());
        assert!(Profile::parse(b"not a profile").is_err());
        assert!(Profile::parse(&[]).is_err());
    }
}
