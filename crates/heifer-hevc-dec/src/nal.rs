//! NAL units (H.265 §7.3.1): splitting an Annex B stream, removing emulation prevention
//! bytes, and parsing the 2-byte NAL unit header.

use std::borrow::Cow;

use crate::Error;

/// NAL unit types used by HEIF still images (H.265 Table 7-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NalUnitType {
    /// Coded slice of a random access point picture (`BLA_W_LP`..`CRA_NUT`, 16..=21).
    IrapSlice(u8),
    /// Coded slice of any other picture (0..=9).
    OtherSlice(u8),
    /// Video parameter set (32).
    Vps,
    /// Sequence parameter set (33).
    Sps,
    /// Picture parameter set (34).
    Pps,
    /// Access unit delimiter (35).
    AccessUnitDelimiter,
    /// Prefix supplemental enhancement information (39).
    PrefixSei,
    /// Suffix supplemental enhancement information (40).
    SuffixSei,
    /// Any other type (reserved, unspecified, end of sequence...).
    Other(u8),
}

impl NalUnitType {
    /// Converts the 6-bit `nal_unit_type` field.
    pub fn from_raw(t: u8) -> Self {
        match t {
            0..=9 => Self::OtherSlice(t),
            16..=21 => Self::IrapSlice(t),
            32 => Self::Vps,
            33 => Self::Sps,
            34 => Self::Pps,
            35 => Self::AccessUnitDelimiter,
            39 => Self::PrefixSei,
            40 => Self::SuffixSei,
            _ => Self::Other(t),
        }
    }

    /// Whether this NAL unit contains a coded slice.
    pub fn is_slice(self) -> bool {
        matches!(self, Self::IrapSlice(_) | Self::OtherSlice(_))
    }
}

/// The 2-byte NAL unit header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NalHeader {
    /// Kind of payload.
    pub unit_type: NalUnitType,
    /// Layer (0 for single-layer streams).
    pub layer_id: u8,
    /// Temporal sub-layer (0 for still images).
    pub temporal_id: u8,
}

impl NalHeader {
    /// Parses the first two bytes of a NAL unit.
    pub fn parse(nal: &[u8]) -> Result<Self, Error> {
        let [b0, b1, ..] = *nal else {
            return Err(Error::UnexpectedEof);
        };
        if b0 & 0x80 != 0 {
            return Err(Error::Invalid("forbidden_zero_bit is set"));
        }
        let temporal_id_plus1 = b1 & 0x07;
        if temporal_id_plus1 == 0 {
            return Err(Error::Invalid("nuh_temporal_id_plus1 is 0"));
        }
        Ok(Self {
            unit_type: NalUnitType::from_raw((b0 >> 1) & 0x3F),
            layer_id: ((b0 & 1) << 5) | (b1 >> 3),
            temporal_id: temporal_id_plus1 - 1,
        })
    }
}

/// A NAL unit: its header and its RBSP payload (emulation prevention bytes removed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NalUnit<'a> {
    /// Parsed header.
    pub header: NalHeader,
    /// Payload after the 2-byte header, ready for [`crate::bitreader::BitReader`].
    pub rbsp: Cow<'a, [u8]>,
}

impl<'a> NalUnit<'a> {
    /// Parses a NAL unit (without start code).
    pub fn parse(nal: &'a [u8]) -> Result<Self, Error> {
        Ok(Self {
            header: NalHeader::parse(nal)?,
            rbsp: unescape_rbsp(&nal[2..]),
        })
    }
}

/// Removes emulation prevention bytes: every `00 00 03` becomes `00 00` (H.265 §7.4.2).
///
/// Encoders insert a `03` byte so that the payload never contains a start code (`00 00 01`).
/// Borrows the input when there is nothing to remove.
pub fn unescape_rbsp(data: &[u8]) -> Cow<'_, [u8]> {
    let needs_unescape = data.windows(3).any(|w| w == [0, 0, 3]);
    if !needs_unescape {
        return Cow::Borrowed(data);
    }
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    Cow::Owned(out)
}

/// Splits an Annex B byte stream (NAL units separated by `00 00 01` or `00 00 00 01`)
/// into NAL units, without start codes.
pub fn split_annex_b(stream: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= stream.len() {
        if stream[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let ends: Vec<usize> = starts
        .iter()
        .skip(1)
        .map(|&s| s - 3)
        .chain(std::iter::once(stream.len()))
        .collect();
    starts.into_iter().zip(ends).map(move |(s, e)| {
        // Trailing zero bytes belong to the next 4-byte start code (or are padding).
        let mut e = e;
        while e > s && stream[e - 1] == 0 {
            e -= 1;
        }
        &stream[s..e]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescape_removes_emulation_bytes() {
        assert_eq!(
            &*unescape_rbsp(&[1, 0, 0, 3, 1, 0, 0, 3, 0]),
            &[1, 0, 0, 1, 0, 0, 0]
        );
        // `00 00 03 03`: only the first 03 is an emulation byte.
        assert_eq!(&*unescape_rbsp(&[0, 0, 3, 3]), &[0, 0, 3]);
    }

    #[test]
    fn unescape_borrows_when_clean() {
        assert!(matches!(unescape_rbsp(&[1, 2, 0, 0, 1]), Cow::Borrowed(_)));
    }

    #[test]
    fn nal_header_vps() {
        // 0x40 0x01: type 32 (VPS), layer 0, temporal_id_plus1 = 1.
        let h = NalHeader::parse(&[0x40, 0x01]).unwrap();
        assert_eq!(h.unit_type, NalUnitType::Vps);
        assert_eq!((h.layer_id, h.temporal_id), (0, 0));
    }

    #[test]
    fn nal_header_errors() {
        assert_eq!(NalHeader::parse(&[0x40]), Err(Error::UnexpectedEof));
        assert!(NalHeader::parse(&[0xC0, 0x01]).is_err());
        assert!(NalHeader::parse(&[0x40, 0x00]).is_err());
    }

    #[test]
    fn split_three_and_four_byte_start_codes() {
        let stream = [
            0, 0, 0, 1, 0x40, 0x01, 0xAA, 0, 0, 1, 0x42, 0x01, 0, 0, 0, 1, 0x44, 0x01, 0xBB,
        ];
        let nals: Vec<_> = split_annex_b(&stream).collect();
        assert_eq!(
            nals,
            vec![&[0x40, 0x01, 0xAA][..], &[0x42, 0x01], &[0x44, 0x01, 0xBB]]
        );
    }
}
