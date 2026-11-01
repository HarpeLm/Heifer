//! Colour management to sRGB, from an ICC profile or from `nclx` colour information.
//!
//! Most phones write a *matrix/TRC* RGB ICC profile (`Display P3`, `sRGB`, `Adobe RGB`…):
//! a per-channel tone curve and a 3×3 matrix to the XYZ profile connection space. This module
//! reads those and converts the pixel values to sRGB, which is what a viewer without colour
//! management expects. LUT-based profiles (`A2B0`/`B2A0`) are refused with
//! [`Error::Unsupported`](crate::Error::Unsupported).
//!
//! Files that carry no ICC profile but `nclx` colour information are converted from those
//! primaries and transfer function instead.

use crate::Error;
use crate::image::{Image, Sample};

/// The sRGB white point (D65), as XYZ with Y = 1.
const D65: [f32; 3] = [0.950_456, 1.0, 1.089_058];

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
    // The sRGB curve is continuous but not polynomial; a fine table with linear interpolation
    // stays within a fraction of a 16-bit step.
    const N: usize = 4096;
    let encode: Vec<f32> = (0..=N)
        .map(|i| linear_to_srgb(i as f32 / N as f32) * max as f32)
        .collect();
    let out = |value: f32| -> u16 {
        let position = value.clamp(0.0, 1.0) * N as f32;
        let low = (position as usize).min(N - 1);
        let fraction = position - low as f32;
        let value = encode[low] * (1.0 - fraction) + encode[low + 1] * fraction;
        value.round().clamp(0.0, max as f32) as u16
    };

    let mut data = Vec::with_capacity(image.data.len());
    for pixel in image.data.chunks_exact(4) {
        let linear = [
            luts[0][usize::from(pixel[0].to_u16())],
            luts[1][usize::from(pixel[1].to_u16())],
            luts[2][usize::from(pixel[2].to_u16())],
        ];
        let mixed = mul(matrix, linear);
        data.push(S::from_u16(out(mixed[0])));
        data.push(S::from_u16(out(mixed[1])));
        data.push(S::from_u16(out(mixed[2])));
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
    let curve = transfer_curve(transfer)
        .ok_or_else(|| Error::Unsupported(format!("transfer characteristics {transfer}")))?;
    if matrix == IDENTITY && curve == Curve::Srgb {
        // Already sRGB: do not pay for a conversion, and leave the samples untouched.
        return Ok(image.clone());
    }
    Ok(convert(image, matrix, |_, x| curve.apply(x)))
}

/// A parsed matrix/TRC RGB ICC profile.
pub(crate) struct Profile {
    curves: [Curve; 3],
    /// Linear RGB in the profile's primaries to linear sRGB.
    matrix: [[f32; 3]; 3],
}

impl Profile {
    /// Parses a matrix/TRC RGB ICC profile. Other profiles (LUT-based, non-RGB, …) are
    /// reported as unsupported.
    pub(crate) fn parse(data: &[u8]) -> Result<Self, Error> {
        if data.len() < 132 || data.get(36..40) != Some(b"acsp") {
            return Err(Error::Unsupported("malformed ICC profile".into()));
        }
        if data.get(16..20) != Some(b"RGB ") || data.get(20..24) != Some(b"XYZ ") {
            return Err(Error::Unsupported(
                "ICC profile that is not an XYZ-connected RGB profile".into(),
            ));
        }
        let count = u32::from_be_bytes(data[128..132].try_into().unwrap()) as usize;
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
    pub(crate) fn to_srgb<S: Sample>(&self, image: &Image<S>) -> Image<S> {
        convert(image, self.matrix, |c, x| self.curves[c].apply(x))
    }
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

/// An `s15Fixed16Number` at `offset`.
fn fixed(data: &[u8], offset: usize) -> f32 {
    let value = data
        .get(offset..offset + 4)
        .and_then(|b| b.try_into().ok())
        .map(i32::from_be_bytes)
        .unwrap_or(0);
    value as f32 / 65536.0
}

/// Parses a `curv` or `para` tag.
fn parse_curve(data: &[u8]) -> Option<Curve> {
    if data.len() < 12 {
        return None;
    }
    match data.get(..4)? {
        b"curv" => {
            let count = u32::from_be_bytes(data.get(8..12)?.try_into().ok()?) as usize;
            match count {
                0 => Some(Curve::Identity),
                1 => {
                    let gamma = u16::from_be_bytes(data.get(12..14)?.try_into().ok()?);
                    Some(Curve::Gamma(gamma as f32 / 256.0))
                }
                n if n > 1 && n <= (data.len() - 12) / 2 => {
                    let values = (0..n)
                        .map(|i| {
                            let v = u16::from_be_bytes(
                                data.get(12 + 2 * i..14 + 2 * i)?.try_into().ok()?,
                            );
                            Some(v as f32 / 65535.0)
                        })
                        .collect::<Option<Vec<f32>>>()?;
                    Some(Curve::Sampled(values))
                }
                _ => None,
            }
        }
        b"para" => {
            let function = u16::from_be_bytes(data.get(8..10)?.try_into().ok()?);
            if function > 4 {
                return None;
            }
            let mut params = [0.0f32; 7];
            for (i, p) in params.iter_mut().enumerate() {
                *p = fixed(data, 12 + 4 * i);
            }
            Some(Curve::Parametric { function, params })
        }
        _ => None,
    }
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
        let xyz = |v: [f32; 3]| {
            let mut out = b"XYZ \0\0\0\0".to_vec();
            for value in v {
                out.extend_from_slice(&((value * 65536.0) as i32).to_be_bytes());
            }
            out
        };
        // Columns hold the colourant vectors: tag data is each column transposed back.
        let tags: [(&[u8; 4], Vec<u8>); 7] = [
            (b"wtpt", xyz(white)),
            (b"rXYZ", xyz([columns[0][0], columns[1][0], columns[2][0]])),
            (b"gXYZ", xyz([columns[0][1], columns[1][1], columns[2][1]])),
            (b"bXYZ", xyz([columns[0][2], columns[1][2], columns[2][2]])),
            (b"rTRC", curve.to_vec()),
            (b"gTRC", curve.to_vec()),
            (b"bTRC", curve.to_vec()),
        ];
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
