//! Tile groups (AV1 §5.11.1) and the frames of an AVIF item.
//!
//! [`frames`] walks the OBUs of an item, keeps the sequence header, reads each frame header and
//! cuts the tile groups into tiles. Each tile is coded with its own symbol decoder.

use crate::Error;
use crate::bits::BitReader;
use crate::frame::FrameHeader;
use crate::obu::{ObuType, obus};
use crate::sequence::SequenceHeader;

/// The coded data of one tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Tile<'a> {
    /// Tile row.
    pub row: u32,
    /// Tile column.
    pub col: u32,
    /// The tile's symbol-coded data.
    pub data: &'a [u8],
}

/// A frame: its headers and its tiles, in raster order.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CodedFrame<'a> {
    /// The sequence header in force.
    pub sequence: SequenceHeader,
    /// The frame header.
    pub header: FrameHeader,
    /// `temporal_id` of the frame's OBUs.
    pub temporal_id: u8,
    /// `spatial_id` of the frame's OBUs.
    pub spatial_id: u8,
    /// Every tile of the frame.
    pub tiles: Vec<Tile<'a>>,
}

/// Reads the frames of an AV1 item, decoding every layer (operating point 0).
///
/// `config_obus` are the OBUs of the `av1C` box. A sequence header in `data` wins over the one
/// in `av1C`, as in dav1d.
pub fn frames<'a>(data: &'a [u8], config_obus: &[u8]) -> Result<Vec<CodedFrame<'a>>, Error> {
    let mut sequence = None;
    for obu in obus(config_obus) {
        let obu = obu?;
        if obu.obu_type == ObuType::SequenceHeader {
            sequence = Some(SequenceHeader::parse(obu.payload)?);
        }
    }
    let mut done = Vec::new();
    let mut current: Option<CodedFrame<'a>> = None;
    for obu in obus(data) {
        let obu = obu?;
        if obu.obu_type == ObuType::SequenceHeader {
            if current.is_some() {
                return Err(Error::Invalid("sequence header inside a frame"));
            }
            sequence = Some(SequenceHeader::parse(obu.payload)?);
            continue;
        }
        let is_frame_obu = matches!(
            obu.obu_type,
            ObuType::FrameHeader | ObuType::Frame | ObuType::TileGroup
        );
        if !is_frame_obu {
            continue; // temporal delimiters, metadata, padding, redundant frame headers
        }
        let seq = sequence
            .as_ref()
            .ok_or(Error::Invalid("frame before any sequence header"))?;
        // Drop the OBUs of layers outside operating point 0 (§7.1).
        let idc = seq.operating_point_idc.first().copied().unwrap_or(0);
        if obu.has_extension
            && idc != 0
            && (idc >> obu.temporal_id & 1 == 0 || idc >> (obu.spatial_id + 8) & 1 == 0)
        {
            continue;
        }
        let tile_data = match (&current, obu.obu_type) {
            // A copy of the header of the frame in progress.
            (Some(_), ObuType::FrameHeader) => continue,
            (Some(_), ObuType::Frame) => {
                return Err(Error::Invalid(
                    "frame OBU before the previous frame's tiles",
                ));
            }
            (None, ObuType::TileGroup) => {
                return Err(Error::Invalid("tile group without a frame header"));
            }
            (Some(_), _) => obu.payload,
            (None, obu_type) => {
                let (header, size) = FrameHeader::parse(&obu, seq)?;
                current = Some(CodedFrame {
                    sequence: seq.clone(),
                    header,
                    temporal_id: obu.temporal_id,
                    spatial_id: obu.spatial_id,
                    tiles: Vec::new(),
                });
                if obu_type == ObuType::FrameHeader {
                    continue;
                }
                &obu.payload[size..]
            }
        };
        let frame = current.as_mut().expect("a frame is in progress");
        if read_tile_group(tile_data, &frame.header, &mut frame.tiles)? {
            done.extend(current.take());
        }
    }
    if current.is_some() {
        return Err(Error::UnexpectedEof);
    }
    if done.is_empty() {
        return Err(Error::Invalid("no frame"));
    }
    Ok(done)
}

/// `tile_group_obu()`: appends the group's tiles to `tiles`; returns whether it ends the frame.
fn read_tile_group<'a>(
    data: &'a [u8],
    header: &FrameHeader,
    tiles: &mut Vec<Tile<'a>>,
) -> Result<bool, Error> {
    let info = &header.tile_info;
    let num_tiles = info.cols * info.rows;
    let mut r = BitReader::new(data);
    let (start, end) = if num_tiles > 1 && r.flag()? {
        let bits = info.cols_log2 + info.rows_log2;
        (r.f(bits)?, r.f(bits)?)
    } else {
        (0, num_tiles - 1)
    };
    if start as usize != tiles.len() || end < start || end >= num_tiles {
        return Err(Error::Invalid("tile group out of order"));
    }
    let mut rest = &data[r.byte_align()?..];
    for tile in start..=end {
        let size = if tile == end {
            rest.len()
        } else {
            let n = usize::from(info.tile_size_bytes);
            let field = rest.get(..n).ok_or(Error::UnexpectedEof)?;
            rest = &rest[n..];
            // le(TileSizeBytes): tile_size_minus_1, little endian.
            field
                .iter()
                .rev()
                .fold(0usize, |acc, &b| acc << 8 | usize::from(b))
                + 1
        };
        if size == 0 {
            return Err(Error::Invalid("empty tile"));
        }
        let tile_data = rest.get(..size).ok_or(Error::UnexpectedEof)?;
        rest = &rest[size..];
        tiles.push(Tile {
            row: tile / info.cols,
            col: tile % info.cols,
            data: tile_data,
        });
    }
    Ok(end == num_tiles - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::pack;
    use crate::frame::tests::{lossless_fields, still_sequence_fields};

    fn obu(obu_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![obu_type << 3 | 0b10, payload.len() as u8];
        out.extend_from_slice(payload);
        out
    }

    /// A sequence header OBU and the two-tile lossless frame header of the frame tests.
    fn stream_start() -> (Vec<u8>, Vec<u8>) {
        let mut header = lossless_fields();
        let frame_header = pack(&header);
        header.push((1, 1)); // trailing bits of a frame header OBU
        let mut out = obu(1, &pack(&still_sequence_fields()));
        out.extend(obu(3, &pack(&header)));
        (out, frame_header)
    }

    #[test]
    fn frame_obu_with_two_tiles() {
        let (start, frame_header) = stream_start();
        let sequence_obu = &start[..start.len() - 6];
        let mut payload = frame_header.clone();
        // tile_start_and_end_present_flag = 0, aligned; tile 0 is 3 bytes (le(2)), tile 1 the rest.
        payload.extend([0x00, 2, 0, 0xA, 0xB, 0xC, 0xD]);
        let mut data = sequence_obu.to_vec();
        data.extend(obu(6, &payload));
        let frames = frames(&data, &[]).unwrap();
        assert_eq!(frames.len(), 1);
        let tiles = &frames[0].tiles;
        assert_eq!(tiles.len(), 2);
        assert_eq!(
            (tiles[0].row, tiles[0].col, tiles[0].data),
            (0, 0, &[0xA, 0xB, 0xC][..])
        );
        assert_eq!(
            (tiles[1].row, tiles[1].col, tiles[1].data),
            (0, 1, &[0xD][..])
        );
        // The sequence header may come from av1C instead.
        let frame_only = obu(6, &payload);
        assert_eq!(
            super::frames(&frame_only, sequence_obu).unwrap()[0].tiles,
            *tiles
        );
        assert_eq!(
            super::frames(&frame_only, &[]),
            Err(Error::Invalid("frame before any sequence header"))
        );

        // A tile size past the end, an empty last tile, a frame without its tiles.
        let mut bad = sequence_obu.to_vec();
        let mut p = frame_header.clone();
        p.extend([0x00, 9, 0, 0xA]);
        bad.extend(obu(6, &p));
        assert_eq!(super::frames(&bad, &[]), Err(Error::UnexpectedEof));
        let mut bad = sequence_obu.to_vec();
        let mut p = frame_header.clone();
        p.extend([0x00, 0, 0, 0xA]);
        bad.extend(obu(6, &p));
        assert_eq!(super::frames(&bad, &[]), Err(Error::Invalid("empty tile")));
        assert_eq!(super::frames(&start, &[]), Err(Error::UnexpectedEof));
    }

    #[test]
    fn tile_groups_in_separate_obus() {
        let (start, _) = stream_start();
        // tile_start_and_end_present_flag = 1, tg_start and tg_end on one bit each.
        let group = |first: u32, last: u32, byte: u8| {
            let mut p = pack(&[(1, 1), (first, 1), (last, 1)]);
            p.push(byte);
            obu(4, &p)
        };
        let mut data = start.clone();
        data.extend(obu(3, &[0xFF])); // a frame header copy is skipped
        data.extend(group(0, 0, 0x11));
        data.extend(group(1, 1, 0x22));
        let frames = frames(&data, &[]).unwrap();
        let tiles = &frames[0].tiles;
        assert_eq!((tiles[0].data, tiles[1].data), (&[0x11][..], &[0x22][..]));

        let mut data = start.clone();
        data.extend(group(1, 1, 0x22));
        assert_eq!(
            super::frames(&data, &[]),
            Err(Error::Invalid("tile group out of order"))
        );
        let mut data = start[..start.len() - 6].to_vec();
        data.extend(group(0, 1, 0x22));
        assert_eq!(
            super::frames(&data, &[]),
            Err(Error::Invalid("tile group without a frame header"))
        );
    }
}
