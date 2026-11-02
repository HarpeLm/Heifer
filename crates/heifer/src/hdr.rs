//! HDR photos with a gain map, or a PQ/HLG transfer.
//!
//! Recent iPhones store a normal (SDR) image plus a *gain map*: a small grayscale auxiliary
//! image (`urn:com:apple:photo:2020:aux:hdrgainmap`) telling how much brighter each area may be
//! shown on an HDR screen. The maximum boost, the *headroom*, comes from the EXIF MakerNote.
//! This module follows Apple's published method ("Applying Apple HDR effect to your photos"):
//!
//! `hdr = sdr × (1 + (headroom − 1) × gain)`, in linear light, where 1.0 is SDR white.
//!
//! Two details follow Apple's own decoder (ImageIO) rather than that document, so that the result
//! matches what Apple devices show: gain map samples are linearized with a 2.2 gamma (the
//! document says Rec.709), and [`decode_hdr`] limits the headroom to [`APPLE_MAX_HEADROOM`].
//! On an iPhone 12 Pro photo, heifer then matches ImageIO's HDR decoding within 0.3% on average.
//!
//! Files with an ISO 21496-1 gain map (`tmap` item) are decoded by the `iso21496` submodule instead.
//!
//! Files whose `nclx` transfer characteristics are PQ (16) or HLG (18) hold an HDR signal instead
//! of an SDR one. [`decode_hdr`] linearises them with SMPTE ST 2084 or the BT.2100 HLG OOTF, on a
//! scale where 1.0 is the BT.2408 reference white of 203 cd/m²: a PQ signal reaches
//! `10000 / 203` and HLG `1000 / 203`. No tone mapping is applied.

mod iso21496;

pub(crate) use iso21496::payload as tmap_payload;

use heifer_isobmff::HeifFile;
use heifer_isobmff::boxes::Property;

use crate::{Error, Exif, Options, decode_item_with};

/// Auxiliary type of Apple HDR gain maps.
pub const APPLE_GAIN_MAP: &str = "urn:com:apple:photo:2020:aux:hdrgainmap";

/// Headroom limit applied by Apple's decoder (ImageIO) when decoding to HDR.
pub const APPLE_MAX_HEADROOM: f32 = 4.0;

/// BT.2408 reference white for HDR, in cd/m²: the linear value 1.0 of [`HdrImage`].
const REFERENCE_WHITE: f32 = 203.0;

/// Peak luminance of the PQ (SMPTE ST 2084) signal, in cd/m².
const PQ_PEAK: f32 = 10_000.0;

/// Nominal peak luminance of the HLG reference OOTF, in cd/m².
const HLG_PEAK: f32 = 1_000.0;

/// The transfer function of the base image.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Transfer {
    /// sRGB or Rec.709: 1.0 is SDR white. This is also the fallback when the file has no
    /// `nclx` colour information, as before HDR transfers were read.
    Sdr,
    /// SMPTE ST 2084, up to 10 000 cd/m².
    Pq,
    /// BT.2100 HLG with the reference 1 000 cd/m² OOTF.
    Hlg,
}

impl Transfer {
    /// The transfer of an `nclx` transfer characteristic code point.
    pub(super) fn from_nclx(transfer: Option<u16>) -> Self {
        match transfer {
            Some(16) => Self::Pq,
            Some(18) => Self::Hlg,
            _ => Self::Sdr,
        }
    }

    /// The ratio of the transfer's peak to SDR white.
    fn headroom(self) -> f32 {
        match self {
            Self::Sdr => 1.0,
            Self::Pq => PQ_PEAK / REFERENCE_WHITE,
            Self::Hlg => HLG_PEAK / REFERENCE_WHITE,
        }
    }
}

/// Linearises coded samples with a transfer function, on a scale where 1.0 is SDR white.
pub(super) struct Linearizer {
    transfer: Transfer,
    max: f32,
    /// sRGB table for SDR transfers, much cheaper than a power per sample.
    lut: Vec<f32>,
}

impl Linearizer {
    pub(super) fn new(transfer: Transfer, max: u16) -> Self {
        let lut = match transfer {
            Transfer::Sdr => (0..=max)
                .map(|v| srgb_to_linear(f32::from(v) / f32::from(max)))
                .collect(),
            Transfer::Pq | Transfer::Hlg => Vec::new(),
        };
        Self {
            transfer,
            max: f32::from(max),
            lut,
        }
    }

    /// The linear RGB value of one coded pixel.
    pub(super) fn apply(&self, rgb: [u16; 3]) -> [f32; 3] {
        match self.transfer {
            Transfer::Sdr => [
                self.lut[usize::from(rgb[0])],
                self.lut[usize::from(rgb[1])],
                self.lut[usize::from(rgb[2])],
            ],
            Transfer::Pq => [
                pq_to_linear(f32::from(rgb[0]) / self.max),
                pq_to_linear(f32::from(rgb[1]) / self.max),
                pq_to_linear(f32::from(rgb[2]) / self.max),
            ],
            Transfer::Hlg => hlg_to_linear([
                f32::from(rgb[0]) / self.max,
                f32::from(rgb[1]) / self.max,
                f32::from(rgb[2]) / self.max,
            ]),
        }
    }
}

/// SMPTE ST 2084 (PQ) to linear light, on the SDR-white scale.
fn pq_to_linear(value: f32) -> f32 {
    const M1: f32 = 2610.0 / 16384.0;
    const M2: f32 = 2523.0 / 4096.0 * 128.0;
    const C1: f32 = 3424.0 / 4096.0;
    const C2: f32 = 2413.0 / 4096.0 * 32.0;
    const C3: f32 = 2392.0 / 4096.0 * 32.0;
    let value = value.clamp(0.0, 1.0).powf(1.0 / M2);
    let numerator = (value - C1).max(0.0);
    let denominator = C2 - C3 * value;
    (numerator / denominator).powf(1.0 / M1) * (PQ_PEAK / REFERENCE_WHITE)
}

/// BT.2100 HLG to linear light, on the SDR-white scale: inverse OETF, then the 1 000 cd/m²
/// reference OOTF with BT.2020 luma.
fn hlg_to_linear(rgb: [f32; 3]) -> [f32; 3] {
    const A: f32 = 0.178_832_77;
    const B: f32 = 0.284_668_92;
    const C: f32 = 0.559_910_7;
    let scene = rgb.map(|value| {
        let value = value.clamp(0.0, 1.0);
        if value <= 0.5 {
            value * value / 3.0
        } else {
            (((value - C) / A).exp() + B) / 12.0
        }
    });
    let luma = 0.2627 * scene[0] + 0.6780 * scene[1] + 0.0593 * scene[2];
    let scale = luma.powf(0.2) * (HLG_PEAK / REFERENCE_WHITE);
    scene.map(|value| value * scale)
}

/// An HDR gain map, ready to be applied.
#[derive(Debug, Clone, PartialEq)]
pub struct GainMap {
    /// Width of the gain map (usually half the image width), after rotation and mirroring.
    pub width: u32,
    /// Height of the gain map.
    pub height: u32,
    /// Linear gains in `0.0..=1.0`, one per gain map pixel, row-major.
    pub data: Vec<f32>,
    /// Ratio of the brightest HDR white to SDR white (at least 1), from the photo's metadata.
    pub headroom: f32,
}

impl GainMap {
    /// The gain at image position (`x`, `y`) of an image of size `width`×`height`, with
    /// bilinear interpolation.
    pub fn sample(&self, x: u32, y: u32, width: u32, height: u32) -> f32 {
        let coord = |p: u32, out: u32, size: u32| {
            let f =
                ((p as f32 + 0.5) * size as f32 / out as f32 - 0.5).clamp(0.0, (size - 1) as f32);
            let i = (f as u32).min(size - 1);
            (i, (i + 1).min(size - 1), f - i as f32)
        };
        let (x0, x1, fx) = coord(x, width, self.width);
        let (y0, y1, fy) = coord(y, height, self.height);
        let at = |x: u32, y: u32| self.data[(y * self.width + x) as usize];
        let top = at(x0, y0) * (1.0 - fx) + at(x1, y0) * fx;
        let bottom = at(x0, y1) * (1.0 - fx) + at(x1, y1) * fx;
        top * (1.0 - fy) + bottom * fy
    }
}

/// A decoded HDR image.
#[derive(Debug, Clone, PartialEq)]
pub struct HdrImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Display headroom ratio represented by `data`: 1.0 for an SDR transfer without a gain map,
    /// about 49.3 for PQ and 4.9 for HLG without one.
    pub headroom: f32,
    /// Linear RGB, 3 values per pixel, row-major. 1.0 is SDR white (203 cd/m² under BT.2408);
    /// highlights go up to `headroom`. The primaries are those of the image (Display P3 for
    /// iPhone photos).
    pub data: Vec<f32>,
}

/// Reads the HDR gain map of the primary image, if the file has a valid Apple gain map.
pub fn read_gain_map(bytes: &[u8]) -> Result<Option<GainMap>, Error> {
    read_gain_map_with_options(bytes, &Options::default())
}

/// Like [`read_gain_map`], with options.
pub fn read_gain_map_with_options(
    bytes: &[u8],
    options: &Options,
) -> Result<Option<GainMap>, Error> {
    gain_map(&HeifFile::parse(bytes)?, options)
}

/// Decodes an ISO 21496-1 or Apple gain-mapped image, or an image with a PQ or HLG transfer, in
/// linear light. ISO 21496-1 metadata is preferred when present; the default display headroom
/// limit is [`APPLE_MAX_HEADROOM`]. Files without a usable gain map or HDR transfer give the base
/// image in linear light, with a headroom of 1.
pub fn decode_hdr(bytes: &[u8]) -> Result<HdrImage, Error> {
    decode_hdr_with_options(bytes, &Options::default(), APPLE_MAX_HEADROOM)
}

/// Like [`decode_hdr`], with options and a headroom limit, for example the headroom of the
/// target display (`f32::INFINITY` for the full headroom stored in the photo). For ISO gain maps,
/// this selects an interpolation between the base and alternate renditions.
///
/// The limit applies to gain maps. PQ and HLG transfers are returned over their full range
/// ([`HdrImage::headroom`] is then about 49.3 and 4.9), with no tone mapping: a display pipeline
/// that cannot show that range has to tone-map the result.
pub fn decode_hdr_with_options(
    bytes: &[u8],
    options: &Options,
    max_headroom: f32,
) -> Result<HdrImage, Error> {
    let file = HeifFile::parse(bytes)?;
    if let Some(hdr) = iso21496::decode(&file, options, max_headroom)? {
        return Ok(hdr);
    }
    let transfer = Transfer::from_nclx(crate::nclx(&file, file.primary_id)?.map(|(_, t)| t));
    let sdr = decode_item_with::<u16>(&file, file.primary_id, 0, options)?;
    let gain = gain_map(&file, options)?.map(|mut g| {
        g.headroom = g.headroom.min(max_headroom).max(1.0);
        g
    });

    let linearizer = Linearizer::new(transfer, sdr.max_value());
    let (w, h) = (sdr.width, sdr.height);
    let mut data = Vec::with_capacity(w as usize * h as usize * 3);
    for (i, px) in sdr.data.chunks_exact(4).enumerate() {
        let factor = gain.as_ref().map_or(1.0, |g| {
            let (x, y) = (i as u32 % w, i as u32 / w);
            1.0 + (g.headroom - 1.0) * g.sample(x, y, w, h)
        });
        let linear = linearizer.apply([px[0], px[1], px[2]]);
        data.extend(linear.map(|value| value * factor));
    }
    let base_headroom = transfer.headroom();
    Ok(HdrImage {
        width: w,
        height: h,
        headroom: gain
            .as_ref()
            .map_or(base_headroom, |g| base_headroom * g.headroom),
        data,
    })
}

fn gain_map(file: &HeifFile<'_>, options: &Options) -> Result<Option<GainMap>, Error> {
    let primary = file.primary_id;
    let mut aux_items = file.referencing_items(primary, b"auxl").into_iter();
    let found = aux_items.try_fold(None, |found, aux| -> Result<_, Error> {
        if found.is_some() {
            return Ok(found);
        }
        let is_gain_map = file
            .item_properties(aux)?
            .any(|p| matches!(p, Property::AuxiliaryType(t) if t == APPLE_GAIN_MAP));
        Ok(is_gain_map.then_some(aux))
    })?;
    let Some(aux) = found else {
        return Ok(None);
    };
    // Apple: the gain map is valid only with the `HDRGainMapVersion` key and both MakerNote tags.
    let has_version = file
        .xmp(aux)?
        .is_some_and(|x| x.windows(17).any(|w| w == b"HDRGainMapVersion"));
    let exif = file.exif(primary)?;
    let headroom = exif
        .as_deref()
        .and_then(Exif::parse)
        .and_then(|e| e.apple_hdr_headroom());
    let (true, Some(headroom)) = (has_version, headroom) else {
        return Ok(None);
    };

    let image = decode_item_with::<u16>(file, aux, 1, options)?;
    // Grayscale: the red channel holds the gain map samples, encoded with a 2.2 gamma.
    let max = f32::from(image.max_value());
    let data = image
        .data
        .chunks_exact(4)
        .map(|p| (f32::from(p[0]) / max).powf(2.2))
        .collect();
    Ok(Some(GainMap {
        width: image.width,
        height: image.height,
        data,
        headroom,
    }))
}

/// sRGB transfer function to linear light.
pub(crate) fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_functions() {
        assert!((srgb_to_linear(1.0) - 1.0).abs() < 1e-6);
        assert!((srgb_to_linear(0.5) - 0.214).abs() < 1e-3);
    }

    #[test]
    fn pq_and_hlg_linearization() {
        // PQ 0.58 is the BT.2408 reference white; 1.0 is the 10 000 cd/m² peak.
        assert_eq!(pq_to_linear(0.0), 0.0);
        assert!((pq_to_linear(0.58) - 1.0).abs() < 0.01);
        assert!((pq_to_linear(1.0) - 49.261).abs() < 0.01);

        // HLG 0.75 is the reference white; 1.0 is the nominal 1 000 cd/m² peak.
        assert_eq!(hlg_to_linear([0.0; 3]), [0.0; 3]);
        assert!((hlg_to_linear([0.75; 3])[0] - 1.0).abs() < 0.01);
        assert!((hlg_to_linear([1.0; 3])[0] - 4.926).abs() < 0.01);
    }

    #[test]
    fn linearizer_scales_by_coded_depth() {
        let pq = Linearizer::new(Transfer::Pq, 1023);
        assert_eq!(pq.apply([0, 0, 0]), [0.0; 3]);
        assert!((pq.apply([1023, 0, 0])[0] - 49.261).abs() < 0.02);
        let sdr = Linearizer::new(Transfer::Sdr, 255);
        assert!((sdr.apply([255, 0, 0])[0] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn bilinear_sampling() {
        let g = GainMap {
            width: 2,
            height: 1,
            data: vec![0.0, 1.0],
            headroom: 2.0,
        };
        // A 4-pixel-wide image over a 2-pixel gain map: edges clamp, the middle interpolates.
        let row: Vec<f32> = (0..4).map(|x| g.sample(x, 0, 4, 1)).collect();
        assert_eq!(row, vec![0.0, 0.25, 0.75, 1.0]);
    }
}
