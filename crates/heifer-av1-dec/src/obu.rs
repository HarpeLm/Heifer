//! Open Bitstream Units (AV1 §5.3 and §6.2).
//!
//! An AV1 bitstream is a sequence of OBUs. Each starts with a one- or two-byte header (type,
//! optional temporal and spatial layer ids) and usually a `leb128` size. AVIF stores OBUs in
//! the "low overhead bitstream format", where every OBU has a size field.

use crate::Error;
use crate::bits::leb128;

/// Type of an OBU (`obu_type`, AV1 §6.2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ObuType {
    /// Sequence header: picture size limits, bit depth, colour configuration, enabled tools.
    SequenceHeader,
    /// Temporal delimiter: start of a temporal unit (no payload).
    TemporalDelimiter,
    /// Frame header without its tiles.
    FrameHeader,
    /// Tile group: coded tiles of the current frame.
    TileGroup,
    /// Metadata (HDR, scalability, timecode...).
    Metadata,
    /// Frame header followed by its first tile group.
    Frame,
    /// Copy of a frame header, for error resilience.
    RedundantFrameHeader,
    /// Tile list (large scale tile decoding).
    TileList,
    /// Padding.
    Padding,
    /// A reserved type, to be ignored.
    Reserved(u8),
}

impl ObuType {
    fn from_bits(t: u8) -> Self {
        match t {
            1 => Self::SequenceHeader,
            2 => Self::TemporalDelimiter,
            3 => Self::FrameHeader,
            4 => Self::TileGroup,
            5 => Self::Metadata,
            6 => Self::Frame,
            7 => Self::RedundantFrameHeader,
            8 => Self::TileList,
            15 => Self::Padding,
            t => Self::Reserved(t),
        }
    }
}

/// One OBU: its header fields and payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Obu<'a> {
    /// `obu_type`.
    pub obu_type: ObuType,
    /// `temporal_id`, 0 without an extension header.
    pub temporal_id: u8,
    /// `spatial_id`, 0 without an extension header.
    pub spatial_id: u8,
    /// Whether the OBU has an extension header (layer ids).
    pub has_extension: bool,
    /// The payload, after the header and size field.
    pub payload: &'a [u8],
}

/// Splits `data` into OBUs. An OBU without a size field takes the rest of `data`.
pub fn obus(data: &[u8]) -> Obus<'_> {
    Obus { rest: data }
}

/// Iterator returned by [`obus`]. Stops after the first error.
#[derive(Debug, Clone)]
pub struct Obus<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for Obus<'a> {
    type Item = Result<Obu<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        let result = parse_obu(self.rest);
        match result {
            Ok((obu, used)) => {
                self.rest = &self.rest[used..];
                Some(Ok(obu))
            }
            Err(e) => {
                self.rest = &[];
                Some(Err(e))
            }
        }
    }
}

/// Parses the OBU at the start of `data`: the OBU and the number of bytes it takes.
fn parse_obu(data: &[u8]) -> Result<(Obu<'_>, usize), Error> {
    let header = *data.first().ok_or(Error::UnexpectedEof)?;
    if header & 0x80 != 0 {
        return Err(Error::Invalid("obu_forbidden_bit is set"));
    }
    let obu_type = ObuType::from_bits(header >> 3 & 0x0F);
    let has_extension = header & 0x04 != 0;
    let has_size = header & 0x02 != 0;
    let mut pos = 1;
    let (temporal_id, spatial_id) = if has_extension {
        let ext = *data.get(1).ok_or(Error::UnexpectedEof)?;
        pos += 1;
        (ext >> 5, ext >> 3 & 0x03)
    } else {
        (0, 0)
    };
    let size = if has_size {
        let (size, used) = leb128(&data[pos..])?;
        pos += used;
        usize::try_from(size).map_err(|_| Error::UnexpectedEof)?
    } else {
        data.len() - pos
    };
    let end = pos.checked_add(size).ok_or(Error::UnexpectedEof)?;
    let payload = data.get(pos..end).ok_or(Error::UnexpectedEof)?;
    Ok((
        Obu {
            obu_type,
            temporal_id,
            spatial_id,
            has_extension,
            payload,
        },
        end,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn obus_are_split_by_size() {
        // Temporal delimiter (size 0), sequence header with 2 payload bytes, then an OBU with an
        // extension header (temporal 5, spatial 2) and no size field, taking the rest.
        let data = [
            0x12,
            0x00,
            0x0A,
            0x02,
            0xAA,
            0xBB,
            0x34,
            0b1011_0000,
            0xCC,
            0xDD,
        ];
        let all: Vec<Obu> = obus(&data).collect::<Result<_, _>>().unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].obu_type, ObuType::TemporalDelimiter);
        assert!(all[0].payload.is_empty());
        assert_eq!(all[1].obu_type, ObuType::SequenceHeader);
        assert_eq!(all[1].payload, [0xAA, 0xBB]);
        assert_eq!(all[2].obu_type, ObuType::Frame);
        assert!(all[2].has_extension);
        assert_eq!((all[2].temporal_id, all[2].spatial_id), (5, 2));
        assert_eq!(all[2].payload, [0xCC, 0xDD]);
    }

    #[test]
    fn malformed_obus_are_errors() {
        // Forbidden bit.
        assert!(obus(&[0x80]).next().unwrap().is_err());
        // Size larger than the data, then iteration stops.
        let mut it = obus(&[0x0A, 0x05, 0x00]);
        assert_eq!(it.next(), Some(Err(Error::UnexpectedEof)));
        assert_eq!(it.next(), None);
        // Missing extension byte, truncated size.
        assert_eq!(obus(&[0x0C]).next(), Some(Err(Error::UnexpectedEof)));
        assert_eq!(obus(&[0x0A, 0x80]).next(), Some(Err(Error::UnexpectedEof)));
        assert_eq!(
            obus(&[0x7A, 0x00]).next().unwrap().unwrap().obu_type,
            ObuType::Padding
        );
        assert_eq!(
            obus(&[0x4A, 0x00]).next().unwrap().unwrap().obu_type,
            ObuType::Reserved(9)
        );
    }
}
