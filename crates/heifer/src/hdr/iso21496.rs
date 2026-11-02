//! HDR images with an ISO 21496-1 gain map.
//!
//! A `tmap` derived image item references a base image and a gain map image (`dimg`) and holds
//! the gain map metadata. The gain is applied in linear light, per channel:
//!
//! `alternate = (base + base_offset) × 2^(log_gain × weight) − alternate_offset`, where
//! `log_gain = min + (max − min) × map^(1 / gamma)` and `weight` moves from 0 (base rendition)
//! to ±1 (alternate rendition) with the display headroom.

use heifer_isobmff::HeifFile;
use heifer_isobmff::boxes::{ColorInfo, Property};

use super::{HdrImage, Linearizer, Transfer};
use crate::{Error, Options, decode_item_with};

/// Decodes the image of the file's `tmap` item for a display of `max_headroom`, or `None` when
/// the file has no ISO 21496-1 gain map.
pub(super) fn decode(
    file: &HeifFile<'_>,
    options: &Options,
    max_headroom: f32,
) -> Result<Option<HdrImage>, Error> {
    let Some(gain) = find(file)? else {
        return Ok(None);
    };
    let transfer = Transfer::from_nclx(crate::nclx(file, gain.base)?.map(|(_, t)| t));
    let base = decode_item_with(file, gain.base, 0, options)?;
    let map = decode_item_with(file, gain.map, 1, options)?;
    apply(
        file,
        gain.base,
        base,
        map,
        gain.metadata,
        max_headroom,
        transfer,
    )
    .map(Some)
}

/// The gain map metadata of a `tmap` item. Headrooms and gains are log2 values.
#[derive(Clone, Copy)]
struct Metadata {
    base_headroom: f32,
    alternate_headroom: f32,
    multichannel: bool,
    use_base_color_space: bool,
    min: [f32; 3],
    max: [f32; 3],
    gamma: [f32; 3],
    base_offset: [f32; 3],
    alternate_offset: [f32; 3],
}

/// A `tmap` item: its base image, its gain map image and its metadata.
struct GainMap {
    base: u32,
    map: u32,
    metadata: Metadata,
}

/// The `tmap` item derived from the primary image (or else the first one), if any.
fn find(file: &HeifFile<'_>) -> Result<Option<GainMap>, Error> {
    let tone_map = file
        .items
        .iter()
        .filter(|item| item.item_type.0 == *b"tmap")
        .find(|item| file.referenced_items(item.id, b"dimg").first() == Some(&file.primary_id))
        .or_else(|| file.items.iter().find(|item| item.item_type.0 == *b"tmap"));
    let Some(tone_map) = tone_map else {
        return Ok(None);
    };
    let references = file.referenced_items(tone_map.id, b"dimg");
    let [base, map] = references else {
        return Err(Error::Invalid("ISO 21496-1 tmap must reference two images"));
    };
    let payload = file.item_data(tone_map.id)?;
    if payload.len() > 4096 {
        return Err(Error::Invalid("ISO 21496-1 tmap metadata is too large"));
    }
    let metadata = parse_metadata(&payload)?;

    Ok(Some(GainMap {
        base: *base,
        map: *map,
        metadata,
    }))
}

/// Parses the `tmap` payload: versions, flags, headrooms, then per channel the gain range,
/// gamma and offsets, as rationals.
fn parse_metadata(data: &[u8]) -> Result<Metadata, Error> {
    let invalid = || Error::Invalid("invalid ISO 21496-1 tmap metadata");
    let mut r = Reader { data, pos: 0 };
    if r.u8()? != 0 {
        return Err(Error::Unsupported(
            "unknown ISO 21496-1 tmap version".into(),
        ));
    }
    if r.u16()? != 0 {
        return Err(Error::Unsupported(
            "unknown ISO 21496-1 gain-map metadata version".into(),
        ));
    }
    let _writer_version = r.u16()?;
    let flags = r.u8()?;
    if flags & 0x3f != 0 {
        return Err(Error::Unsupported("unknown ISO 21496-1 tmap flags".into()));
    }
    let multichannel = flags & 0x80 != 0;
    let use_base_color_space = flags & 0x40 != 0;
    let base_headroom = r.unsigned_ratio()?;
    let alternate_headroom = r.unsigned_ratio()?;
    let channels = if multichannel { 3 } else { 1 };
    let mut min = [0.0; 3];
    let mut max = [0.0; 3];
    let mut gamma = [1.0; 3];
    let mut base_offset = [0.0; 3];
    let mut alternate_offset = [0.0; 3];
    for channel in 0..channels {
        min[channel] = r.signed_ratio()?;
        max[channel] = r.signed_ratio()?;
        gamma[channel] = r.unsigned_ratio()?;
        base_offset[channel] = r.signed_ratio()?;
        alternate_offset[channel] = r.signed_ratio()?;
        if max[channel] < min[channel] || gamma[channel] <= 0.0 {
            return Err(invalid());
        }
    }
    if !r.is_empty() || base_headroom == alternate_headroom {
        return Err(invalid());
    }
    if base_headroom >= 127.0 || alternate_headroom >= 127.0 {
        return Err(invalid());
    }
    if !multichannel {
        for channel in 1..3 {
            min[channel] = min[0];
            max[channel] = max[0];
            gamma[channel] = gamma[0];
            base_offset[channel] = base_offset[0];
            alternate_offset[channel] = alternate_offset[0];
        }
    }
    Ok(Metadata {
        base_headroom,
        alternate_headroom,
        multichannel,
        use_base_color_space,
        min,
        max,
        gamma,
        base_offset,
        alternate_offset,
    })
}

/// The `tmap` item payload for a monochrome gain map with the given headroom: version 0,
/// `use_base_colour_space`, no base headroom, no gain below 1, gamma 1 and no offsets.
pub(crate) fn payload(headroom: f32) -> Result<Vec<u8>, Error> {
    let log2 = headroom.log2();
    if !(log2 > 0.0 && log2 < 127.0) {
        return Err(Error::Invalid("HDR headroom must be greater than 1"));
    }
    // Headrooms are rationals; 2^22 keeps the log2 value to about seven decimal digits.
    let denominator = 1u32 << 22;
    let numerator = (log2 * denominator as f32).round() as u32;
    let max = i32::try_from(numerator).map_err(|_| Error::Invalid("HDR headroom is too large"))?;
    let mut data = Vec::with_capacity(62);
    data.push(0); // tmap version
    data.extend_from_slice(&0u16.to_be_bytes()); // minimum gain map metadata version
    data.extend_from_slice(&0u16.to_be_bytes()); // writer version
    data.push(0x40); // one channel, use_base_colour_space, reserved bits zero
    data.extend_from_slice(&0u32.to_be_bytes()); // base headroom 0/1
    data.extend_from_slice(&1u32.to_be_bytes());
    data.extend_from_slice(&numerator.to_be_bytes()); // alternate headroom
    data.extend_from_slice(&denominator.to_be_bytes());
    data.extend_from_slice(&0i32.to_be_bytes()); // gain min 0/1
    data.extend_from_slice(&1u32.to_be_bytes());
    data.extend_from_slice(&max.to_be_bytes()); // gain max: the headroom
    data.extend_from_slice(&denominator.to_be_bytes());
    data.extend_from_slice(&1u32.to_be_bytes()); // gamma 1/1
    data.extend_from_slice(&1u32.to_be_bytes());
    data.extend_from_slice(&0i32.to_be_bytes()); // base offset 0/1
    data.extend_from_slice(&1u32.to_be_bytes());
    data.extend_from_slice(&0i32.to_be_bytes()); // alternate offset 0/1
    data.extend_from_slice(&1u32.to_be_bytes());
    Ok(data)
}

/// Big-endian reader of the `tmap` payload.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let end = self
            .pos
            .checked_add(N)
            .ok_or(Error::Invalid("truncated ISO 21496-1 tmap metadata"))?;
        let bytes = self
            .data
            .get(self.pos..end)
            .ok_or(Error::Invalid("truncated ISO 21496-1 tmap metadata"))?;
        self.pos = end;
        bytes
            .try_into()
            .map_err(|_| Error::Invalid("truncated ISO 21496-1 tmap metadata"))
    }

    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.bytes::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_be_bytes(self.bytes()?))
    }

    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_be_bytes(self.bytes()?))
    }

    fn signed_ratio(&mut self) -> Result<f32, Error> {
        let numerator = i32::from_be_bytes(self.bytes()?);
        self.ratio(f64::from(numerator))
    }

    fn unsigned_ratio(&mut self) -> Result<f32, Error> {
        let numerator = self.u32()?;
        self.ratio(f64::from(numerator))
    }

    fn ratio(&mut self, numerator: f64) -> Result<f32, Error> {
        let denominator = self.u32()?;
        if denominator == 0 {
            return Err(Error::Invalid("zero denominator in ISO 21496-1 tmap"));
        }
        let value = (numerator / f64::from(denominator)) as f32;
        if !value.is_finite() {
            return Err(Error::Invalid("non-finite ISO 21496-1 tmap value"));
        }
        Ok(value)
    }

    fn is_empty(&self) -> bool {
        self.pos == self.data.len()
    }
}

/// Applies the gain map to the base image, towards the alternate rendition, as far as
/// `max_headroom` allows. `transfer` is the transfer function of the base image.
fn apply(
    file: &HeifFile<'_>,
    base_id: u32,
    base: crate::Image,
    map: crate::Image,
    metadata: Metadata,
    max_headroom: f32,
    transfer: Transfer,
) -> Result<HdrImage, Error> {
    let base_log = metadata.base_headroom;
    let alternate_log = metadata.alternate_headroom;
    let low_log = base_log.min(alternate_log);
    let high_log = base_log.max(alternate_log);
    let requested_log = max_headroom.max(1.0).log2();
    let target_log = requested_log.clamp(low_log, high_log);
    let progress = ((target_log - base_log) / (alternate_log - base_log)).clamp(0.0, 1.0);
    let gain_weight = if alternate_log > base_log {
        progress
    } else {
        -progress
    };
    let headroom = 2.0_f32.powf(target_log);
    if !headroom.is_finite() {
        return Err(Error::Invalid("ISO 21496-1 headroom is out of range"));
    }
    let map_max = f32::from(map.max_value());
    let linearizer = Linearizer::new(transfer, base.max_value());
    let primaries = file
        .item_properties(base_id)?
        .find_map(|property| match property {
            Property::Color(ColorInfo::Nclx { primaries, .. }) => Some(*primaries),
            _ => None,
        })
        .unwrap_or(1);
    let mut data = Vec::with_capacity(base.width as usize * base.height as usize * 3);
    for (index, pixel) in base.data.chunks_exact(4).enumerate() {
        let (x, y) = (index as u32 % base.width, index as u32 / base.width);
        let source = linearizer.apply([pixel[0], pixel[1], pixel[2]]);
        let mut working = if metadata.use_base_color_space {
            source
        } else {
            convert_primaries(source, primaries, true)?
        };
        if gain_weight != 0.0 {
            for (channel, output) in working.iter_mut().enumerate() {
                let map_channel = if metadata.multichannel { channel } else { 0 };
                let map_sample =
                    sample_map(&map, x, y, base.width, base.height, map_channel) / map_max;
                let log_gain = metadata.min[channel]
                    + (metadata.max[channel] - metadata.min[channel])
                        * map_sample.powf(1.0 / metadata.gamma[channel]);
                *output = (*output + metadata.base_offset[channel])
                    * 2.0_f32.powf(log_gain * gain_weight)
                    - metadata.alternate_offset[channel];
            }
        }
        if !metadata.use_base_color_space {
            working = convert_primaries(working, primaries, false)?;
        }
        if !working.iter().all(|value| value.is_finite()) {
            return Err(Error::Invalid(
                "ISO 21496-1 gain-map output is out of range",
            ));
        }
        data.extend_from_slice(&working);
    }
    Ok(HdrImage {
        width: base.width,
        height: base.height,
        headroom: transfer.headroom() * headroom,
        data,
    })
}

/// A gain map channel at image position (`x`, `y`), with bilinear interpolation.
fn sample_map(
    image: &crate::Image,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    channel: usize,
) -> f32 {
    let coord = |p: u32, out: u32, size: u32| {
        let position =
            ((p as f32 + 0.5) * size as f32 / out as f32 - 0.5).clamp(0.0, (size - 1) as f32);
        let first = position as u32;
        (first, (first + 1).min(size - 1), position - first as f32)
    };
    let (x0, x1, fx) = coord(x, width, image.width);
    let (y0, y1, fy) = coord(y, height, image.height);
    let at = |x: u32, y: u32| image.data[(y * image.width + x) as usize * 4 + channel] as f32;
    let top = at(x0, y0) * (1.0 - fx) + at(x1, y0) * fx;
    let bottom = at(x0, y1) * (1.0 - fx) + at(x1, y1) * fx;
    top * (1.0 - fy) + bottom * fy
}

/// Converts linear RGB between the image primaries (nclx code) and sRGB/BT.709 primaries, the
/// gain map working space when it does not use the base colour space.
fn convert_primaries(rgb: [f32; 3], primaries: u16, to_srgb: bool) -> Result<[f32; 3], Error> {
    let (to, from): ([[f32; 3]; 3], [[f32; 3]; 3]) = match primaries {
        1 | 2 => (
            [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        ),
        9 => (
            [
                [0.627_404, 0.329_282, 0.043_314],
                [0.069_097, 0.919_540, 0.011_362],
                [0.016_391, 0.088_013, 0.895_595],
            ],
            [
                [1.660_491, -0.587_641, -0.072_850],
                [-0.124_550, 1.132_9, -0.008_349],
                [-0.018_151, -0.100_579, 1.118_73],
            ],
        ),
        12 => (
            [
                [1.224_745, -0.224_904, 0.0],
                [-0.042_058, 1.042_081, 0.0],
                [-0.019_642, -0.078_655, 1.098_297],
            ],
            [
                [0.822_592, 0.177_534, 0.0],
                [0.033_200, 0.966_783, 0.0],
                [0.017_085, 0.072_396, 0.910_302],
            ],
        ),
        _ => {
            return Err(Error::Unsupported(format!(
                "ISO 21496-1 gain map in unsupported base primaries {primaries}"
            )));
        }
    };
    let matrix = if to_srgb { to } else { from };
    Ok(std::array::from_fn(|row| {
        matrix[row][0] * rgb[0] + matrix[row][1] * rgb[1] + matrix[row][2] * rgb[2]
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hdr::srgb_to_linear;

    fn ratio(numerator: u32, denominator: u32) -> [u8; 8] {
        [
            (numerator >> 24) as u8,
            (numerator >> 16) as u8,
            (numerator >> 8) as u8,
            numerator as u8,
            (denominator >> 24) as u8,
            (denominator >> 16) as u8,
            (denominator >> 8) as u8,
            denominator as u8,
        ]
    }

    fn signed_ratio(numerator: i32, denominator: u32) -> [u8; 8] {
        let mut bytes = ratio(numerator as u32, denominator);
        bytes[..4].copy_from_slice(&numerator.to_be_bytes());
        bytes
    }

    fn tmap_payload() -> Vec<u8> {
        let mut data = vec![0, 0, 0, 0, 0, 0]; // tmap version, metadata versions, flags
        data.extend_from_slice(&ratio(0, 1));
        data.extend_from_slice(&ratio(2, 1));
        data.extend_from_slice(&signed_ratio(0, 1));
        data.extend_from_slice(&signed_ratio(2, 1));
        data.extend_from_slice(&ratio(1, 1));
        data.extend_from_slice(&signed_ratio(0, 1));
        data.extend_from_slice(&signed_ratio(0, 1));
        data
    }

    fn box_bytes(kind: &[u8; 4], content: &[u8]) -> Vec<u8> {
        let mut bx = ((content.len() + 8) as u32).to_be_bytes().to_vec();
        bx.extend_from_slice(kind);
        bx.extend_from_slice(content);
        bx
    }

    fn file_with_tmap(payload: &[u8]) -> Vec<u8> {
        let mut iinf = vec![0, 0, 0, 0, 0, 3];
        for (id, kind) in [(1u16, b"hvc1"), (2, b"hvc1"), (3, b"tmap")] {
            let mut infe = vec![2, 0, 0, 0];
            infe.extend_from_slice(&id.to_be_bytes());
            infe.extend_from_slice(&[0, 0]);
            infe.extend_from_slice(kind);
            infe.push(0);
            iinf.extend(box_bytes(b"infe", &infe));
        }

        let mut iref = vec![0, 0, 0, 0];
        iref.extend(box_bytes(b"dimg", &[0, 3, 0, 2, 0, 1, 0, 2]));
        let mut iloc = vec![1, 0, 0, 0, 0x44, 0, 0, 1, 0, 3, 0, 1, 0, 0, 0, 1];
        iloc.extend_from_slice(&0u32.to_be_bytes());
        iloc.extend_from_slice(&(payload.len() as u32).to_be_bytes());

        let mut hdlr = vec![0; 8];
        hdlr.extend_from_slice(b"pict");
        hdlr.extend_from_slice(&[0; 13]);
        let mut meta = vec![0, 0, 0, 0];
        meta.extend(box_bytes(b"hdlr", &hdlr));
        meta.extend(box_bytes(b"pitm", &[0, 0, 0, 0, 0, 1]));
        meta.extend(box_bytes(b"iinf", &iinf));
        meta.extend(box_bytes(b"iref", &iref));
        meta.extend(box_bytes(b"iloc", &iloc));
        meta.extend(box_bytes(b"idat", payload));

        let mut data = box_bytes(b"ftyp", b"heic\0\0\0\0mif1heic");
        data.extend(box_bytes(b"meta", &meta));
        data
    }

    #[test]
    fn parses_iso_21496_tmap_metadata_and_rejects_invalid_values() {
        let metadata = parse_metadata(&tmap_payload()).unwrap();
        assert_eq!(metadata.base_headroom, 0.0);
        assert_eq!(metadata.alternate_headroom, 2.0);
        assert_eq!(metadata.min, [0.0; 3]);
        assert_eq!(metadata.max, [2.0; 3]);
        assert_eq!(metadata.gamma, [1.0; 3]);
        assert!(!metadata.multichannel);

        let mut truncated = tmap_payload();
        truncated.pop();
        assert!(parse_metadata(&truncated).is_err());

        let mut zero_denominator = tmap_payload();
        zero_denominator[18..22].fill(0);
        assert!(parse_metadata(&zero_denominator).is_err());
    }

    #[test]
    fn serialises_monochrome_tmap_metadata() {
        let data = payload(4.0).unwrap();
        assert_eq!(data.len(), 62);
        assert_eq!(data[5], 0x40);

        let metadata = parse_metadata(&data).unwrap();
        assert!(!metadata.multichannel);
        assert!(metadata.use_base_color_space);
        assert_eq!(metadata.base_headroom, 0.0);
        assert!((metadata.alternate_headroom - 2.0).abs() < 1e-6);
        assert_eq!(metadata.min, [0.0; 3]);
        assert!((metadata.max[0] - 2.0).abs() < 1e-6);
        assert_eq!(metadata.gamma, [1.0; 3]);
        assert_eq!(metadata.base_offset, [0.0; 3]);
        assert_eq!(metadata.alternate_offset, [0.0; 3]);

        assert!(payload(1.0).is_err());
        assert!(payload(0.5).is_err());
        assert!(payload(f32::NAN).is_err());
        assert!(payload(f32::INFINITY).is_err());
    }

    #[test]
    fn parses_multichannel_and_backward_iso_metadata() {
        let mut payload = tmap_payload();
        payload[5] = 0x80;
        payload[6..14].copy_from_slice(&ratio(2, 1));
        payload[14..22].copy_from_slice(&ratio(0, 1));
        let single_channel = payload[22..].to_vec();
        payload.extend_from_slice(&single_channel);
        payload.extend_from_slice(&single_channel);

        let metadata = parse_metadata(&payload).unwrap();
        assert!(metadata.multichannel);
        assert_eq!(
            (metadata.base_headroom, metadata.alternate_headroom),
            (2.0, 0.0)
        );
    }

    #[test]
    fn finds_tmap_item_and_references_from_heif() {
        let data = file_with_tmap(&tmap_payload());
        let file = HeifFile::parse(&data).unwrap();
        assert!(file.item(3).unwrap().location.is_some(), "{:?}", file.items);
        assert_eq!(file.item_data(3).unwrap(), tmap_payload());
        let gain = find(&file).unwrap().unwrap();
        assert_eq!((gain.base, gain.map), (1, 2));
        assert_eq!(gain.metadata.alternate_headroom, 2.0);
    }

    #[test]
    fn applies_iso_gain_map_with_display_headroom_limit() {
        let data = file_with_tmap(&tmap_payload());
        let file = HeifFile::parse(&data).unwrap();
        let base = crate::Image {
            width: 1,
            height: 1,
            bit_depth: 8,
            has_alpha: false,
            data: vec![128, 128, 128, 255],
        };
        let map = crate::Image {
            width: 1,
            height: 1,
            bit_depth: 8,
            has_alpha: false,
            data: vec![255, 255, 255, 255],
        };
        let metadata = parse_metadata(&tmap_payload()).unwrap();

        let hdr = apply(
            &file,
            1,
            base.clone(),
            map.clone(),
            metadata,
            4.0,
            Transfer::Sdr,
        )
        .unwrap();
        assert!((hdr.data[0] - srgb_to_linear(128.0 / 255.0) * 4.0).abs() < 1e-5);
        assert_eq!(hdr.headroom, 4.0);

        let sdr = apply(&file, 1, base, map, metadata, 1.0, Transfer::Sdr).unwrap();
        assert!((sdr.data[0] - srgb_to_linear(128.0 / 255.0)).abs() < 1e-6);
        assert_eq!(sdr.headroom, 1.0);
    }

    #[test]
    fn applies_backward_iso_gain_map_from_hdr_base_to_sdr() {
        let data = file_with_tmap(&tmap_payload());
        let file = HeifFile::parse(&data).unwrap();
        let base = crate::Image {
            width: 1,
            height: 1,
            bit_depth: 8,
            has_alpha: false,
            data: vec![128, 128, 128, 255],
        };
        let map = crate::Image {
            width: 1,
            height: 1,
            bit_depth: 8,
            has_alpha: false,
            data: vec![255, 255, 255, 255],
        };
        let mut metadata = parse_metadata(&tmap_payload()).unwrap();
        metadata.base_headroom = 2.0;
        metadata.alternate_headroom = 0.0;
        metadata.min = [2.0; 3];
        metadata.max = [2.0; 3];

        let sdr = apply(&file, 1, base, map, metadata, 1.0, Transfer::Sdr).unwrap();
        assert!((sdr.data[0] - srgb_to_linear(128.0 / 255.0) / 4.0).abs() < 1e-6);
        assert_eq!(sdr.headroom, 1.0);
    }

    #[test]
    fn applies_per_channel_iso_gain_values() {
        let data = file_with_tmap(&tmap_payload());
        let file = HeifFile::parse(&data).unwrap();
        let base = crate::Image {
            width: 1,
            height: 1,
            bit_depth: 8,
            has_alpha: false,
            data: vec![255, 255, 255, 255],
        };
        let map = crate::Image {
            width: 1,
            height: 1,
            bit_depth: 8,
            has_alpha: false,
            data: vec![0, 128, 255, 255],
        };
        let mut metadata = parse_metadata(&tmap_payload()).unwrap();
        metadata.multichannel = true;
        metadata.alternate_headroom = 1.0;
        metadata.max = [0.0, 1.0, 2.0];

        let hdr = apply(&file, 1, base, map, metadata, 2.0, Transfer::Sdr).unwrap();
        assert!((hdr.data[0] - 1.0).abs() < 1e-6);
        assert!((hdr.data[1] - 2.0_f32.powf(128.0 / 255.0)).abs() < 1e-5);
        assert!((hdr.data[2] - 4.0).abs() < 1e-5);
    }
}
