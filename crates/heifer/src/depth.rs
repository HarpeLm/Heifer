//! Depth map metadata: the depth representation information carried by an auxiliary image.
//!
//! HEIF depth maps store their scale in an HEVC SEI message in the subtype bytes of the `auxC`
//! property. The values are optional: a depth map can only say that it uses uniform disparity,
//! or give its near and far planes and the disparity range it codes.

use std::fmt;

use heifer_isobmff::{HeifFile, ItemId};

use crate::Error;

/// How the samples of a depth map map to depth or disparity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepthRepresentation {
    /// Samples are uniform in inverse Z.
    UniformInverseZ,
    /// Samples are uniform in disparity.
    UniformDisparity,
    /// Samples are uniform in Z.
    UniformZ,
    /// Disparity is coded with a non-uniform response curve.
    NonuniformDisparity,
}

impl fmt::Display for DepthRepresentation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UniformInverseZ => "uniform inverse Z",
            Self::UniformDisparity => "uniform disparity",
            Self::UniformZ => "uniform Z",
            Self::NonuniformDisparity => "non-uniform disparity",
        })
    }
}

/// Depth representation information of a depth auxiliary image.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct DepthInfo {
    /// Near clipping plane, when the file gives one.
    pub z_near: Option<f64>,
    /// Far clipping plane, when the file gives one.
    pub z_far: Option<f64>,
    /// Smallest coded disparity, when the file gives one.
    pub d_min: Option<f64>,
    /// Largest coded disparity, when the file gives one.
    pub d_max: Option<f64>,
    /// How the depth map samples represent depth or disparity.
    pub representation: DepthRepresentation,
    /// View used as the disparity reference, when the file gives a disparity range.
    pub disparity_reference_view: Option<u32>,
}

impl DepthInfo {
    /// Reads the depth representation information of an auxiliary item.
    ///
    /// Returns `None` when the item has no `auxC` subtype data or when that data is not an HEVC
    /// depth representation information SEI message.
    ///
    /// ```no_run
    /// # let bytes = std::fs::read("portrait.heic").unwrap();
    /// for aux in heifer::auxiliary_images(&bytes).unwrap() {
    ///     if let Some(depth) = heifer::DepthInfo::read(&bytes, aux.id).unwrap() {
    ///         println!("{}", depth.representation);
    ///     }
    /// }
    /// ```
    pub fn read(bytes: &[u8], id: ItemId) -> Result<Option<Self>, Error> {
        let file = HeifFile::parse(bytes)?;
        let Some(data) = file.auxiliary_subtypes(id)? else {
            return Ok(None);
        };
        Ok(parse_auxiliary_subtypes(data))
    }
}

/// Parses the `auxC` subtype bytes of a depth map.
fn parse_auxiliary_subtypes(data: &[u8]) -> Option<DepthInfo> {
    let mut pos = 0;
    while pos + 4 <= data.len() {
        let group_len = u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
        pos += 4;
        let end = pos.checked_add(group_len)?;
        if end > data.len() {
            return None;
        }
        while pos + 4 <= end {
            let nal_size = u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
            pos += 4;
            let nal_end = pos.checked_add(nal_size)?;
            if nal_end > end {
                return None;
            }
            let nal = &data[pos..nal_end];
            pos = nal_end;
            // Prefix and suffix SEI NAL units, with a two-byte NAL header.
            if nal.len() > 2
                && matches!(nal[0] >> 1, 39 | 40)
                && let Some(info) = parse_sei_messages(&nal[2..])
            {
                return Some(info);
            }
        }
        if pos < end {
            return None;
        }
    }
    None
}

/// Reads the SEI messages of a payload, looking for depth representation information (177).
fn parse_sei_messages(mut data: &[u8]) -> Option<DepthInfo> {
    while !data.is_empty() {
        let (payload_type, used) = read_sei_header(data)?;
        data = &data[used..];
        let (payload_size, used) = read_sei_header(data)?;
        data = &data[used..];
        let payload = data.get(..payload_size)?;
        data = &data[payload_size..];
        if payload_type == 177 {
            return parse_depth_representation(payload);
        }
    }
    None
}

/// Reads a SEI payload type or size, which is a run of `0xFF` bytes and a final byte.
fn read_sei_header(data: &[u8]) -> Option<(usize, usize)> {
    let mut value = 0usize;
    for (i, &byte) in data.iter().enumerate() {
        if byte == 0xFF {
            value = value.checked_add(0xFF)?;
        } else {
            return Some((value.checked_add(usize::from(byte))?, i + 1));
        }
    }
    None
}

/// Parses a depth representation information SEI payload.
fn parse_depth_representation(data: &[u8]) -> Option<DepthInfo> {
    let mut bits = Bits::new(data);
    let has_z_near = bits.read(1)? != 0;
    let has_z_far = bits.read(1)? != 0;
    let has_d_min = bits.read(1)? != 0;
    let has_d_max = bits.read(1)? != 0;
    let representation = match bits.uvlc()? {
        0 => DepthRepresentation::UniformInverseZ,
        1 => DepthRepresentation::UniformDisparity,
        2 => DepthRepresentation::UniformZ,
        3 => DepthRepresentation::NonuniformDisparity,
        _ => return None,
    };
    let disparity_reference_view = if has_d_min || has_d_max {
        Some(u32::try_from(bits.uvlc()?).ok()?)
    } else {
        None
    };
    let element = |bits: &mut Bits<'_>| bits.element();
    let z_near = if has_z_near {
        Some(element(&mut bits)?)
    } else {
        None
    };
    let z_far = if has_z_far {
        Some(element(&mut bits)?)
    } else {
        None
    };
    let d_min = if has_d_min {
        Some(element(&mut bits)?)
    } else {
        None
    };
    let d_max = if has_d_max {
        Some(element(&mut bits)?)
    } else {
        None
    };
    Some(DepthInfo {
        z_near,
        z_far,
        d_min,
        d_max,
        representation,
        disparity_reference_view,
    })
}

/// A bounded bit reader for the SEI payload.
struct Bits<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    fn read(&mut self, count: u32) -> Option<u64> {
        if count > 64 || self.bit.checked_add(count as usize)? > self.data.len() * 8 {
            return None;
        }
        let mut value = 0u64;
        for _ in 0..count {
            let byte = self.data[self.bit / 8];
            let bit = (byte >> (7 - self.bit % 8)) & 1;
            value = (value << 1) | u64::from(bit);
            self.bit += 1;
        }
        Some(value)
    }

    /// Unsigned variable-length code: leading zero bits, a one bit, then that many suffix bits.
    fn uvlc(&mut self) -> Option<u64> {
        let mut zeros = 0u32;
        while self.read(1)? == 0 {
            zeros += 1;
            if zeros > 32 {
                return None;
            }
        }
        let suffix = self.read(zeros)?;
        ((1u64 << zeros) - 1).checked_add(suffix)
    }

    /// A depth representation element: sign, 7-bit exponent, 5-bit mantissa length and mantissa.
    fn element(&mut self) -> Option<f64> {
        let negative = self.read(1)? != 0;
        let exponent = self.read(7)? as i32;
        let mantissa_len = self.read(5)? as u32 + 1;
        let mantissa = self.read(mantissa_len)?;
        let scale = 2f64.powi(mantissa_len as i32);
        let value = if exponent > 0 {
            2f64.powi(exponent - 31) * (1.0 + mantissa as f64 / scale)
        } else {
            2f64.powi(-(30 + mantissa_len as i32)) * mantissa as f64
        };
        Some(if negative { -value } else { value })
    }
}
