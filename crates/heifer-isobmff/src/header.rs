//! Box headers and iteration over consecutive boxes (ISO/IEC 14496-12 §4.2).

use core::fmt;

use crate::Error;

/// A four-character box type, e.g. `ftyp`, `meta`, `iloc`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FourCC(pub [u8; 4]);

impl fmt::Display for FourCC {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &b in &self.0 {
            if b.is_ascii_graphic() || b == b' ' {
                write!(f, "{}", b as char)?;
            } else {
                write!(f, "\\x{b:02x}")?;
            }
        }
        Ok(())
    }
}

impl fmt::Debug for FourCC {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FourCC(\"{self}\")")
    }
}

/// The header that starts every box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxHeader {
    /// Box type.
    pub box_type: FourCC,
    /// Size of the header itself: 8, or 16 with a 64-bit size, plus 16 for a `uuid` box.
    pub header_size: u64,
    /// Total size of the box (header included), or `None` if the box extends to the end of the file.
    pub size: Option<u64>,
    /// Extended type, only present when `box_type` is `uuid`.
    pub user_type: Option<[u8; 16]>,
}

impl BoxHeader {
    /// Size of the box content (everything after the header), if known.
    pub fn content_size(&self) -> Option<u64> {
        self.size.map(|s| s - self.header_size)
    }
}

/// Reads a box header at the beginning of `data`.
///
/// Handles the special cases of ISO/IEC 14496-12 §4.2:
/// `size == 1` (a 64-bit `largesize` follows the type),
/// `size == 0` (the box extends to the end of the file),
/// and `uuid` boxes (a 16-byte extended type follows).
pub fn read_box_header(data: &[u8]) -> Result<BoxHeader, Error> {
    let size32 = read_u32(data, 0)?;
    let box_type = FourCC(read_array(data, 4)?);
    let mut header_size: u64 = 8;

    let size = match size32 {
        0 => None,
        1 => {
            header_size += 8;
            Some(read_u64(data, 8)?)
        }
        n => Some(u64::from(n)),
    };

    let user_type = if box_type.0 == *b"uuid" {
        let user_type = read_array(data, header_size as usize)?;
        header_size += 16;
        Some(user_type)
    } else {
        None
    };

    if let Some(size) = size
        && size < header_size
    {
        return Err(Error::InvalidBoxSize {
            box_type,
            size,
            header_size,
        });
    }

    Ok(BoxHeader {
        box_type,
        header_size,
        size,
        user_type,
    })
}

/// A box located inside a byte buffer.
#[derive(Debug, Clone, Copy)]
pub struct RawBox<'a> {
    /// The parsed header.
    pub header: BoxHeader,
    /// Offset of the box start, relative to the buffer given to [`BoxIter::new`].
    pub offset: usize,
    /// The box content (everything after the header).
    pub content: &'a [u8],
}

/// Iterates over consecutive boxes in a buffer (a file, or the content of a container box).
///
/// Stops after the first error.
pub struct BoxIter<'a> {
    data: &'a [u8],
    pos: usize,
    failed: bool,
}

impl<'a> BoxIter<'a> {
    /// Creates an iterator over the boxes in `data`.
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            failed: false,
        }
    }
}

impl<'a> Iterator for BoxIter<'a> {
    type Item = Result<RawBox<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.pos >= self.data.len() {
            return None;
        }
        let rest = &self.data[self.pos..];
        let result = read_box_header(rest).and_then(|header| {
            let end = match header.size {
                None => rest.len(),
                Some(size) => usize::try_from(size)
                    .ok()
                    .filter(|&s| s <= rest.len())
                    .ok_or(Error::UnexpectedEof)?,
            };
            let start = header.header_size as usize;
            Ok(RawBox {
                header,
                offset: self.pos,
                content: &rest[start..end],
            })
        });
        match &result {
            Ok(b) => self.pos += b.header.header_size as usize + b.content.len(),
            Err(_) => self.failed = true,
        }
        Some(result)
    }
}

fn read_array<const N: usize>(data: &[u8], at: usize) -> Result<[u8; N], Error> {
    data.get(at..at + N)
        .and_then(|s| s.try_into().ok())
        .ok_or(Error::UnexpectedEof)
}

fn read_u32(data: &[u8], at: usize) -> Result<u32, Error> {
    read_array(data, at).map(u32::from_be_bytes)
}

fn read_u64(data: &[u8], at: usize) -> Result<u64, Error> {
    read_array(data, at).map(u64::from_be_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_box() {
        let data = [0, 0, 0, 24, b'f', b't', b'y', b'p', 0xAA];
        let h = read_box_header(&data).unwrap();
        assert_eq!(h.box_type, FourCC(*b"ftyp"));
        assert_eq!(h.header_size, 8);
        assert_eq!(h.size, Some(24));
        assert_eq!(h.content_size(), Some(16));
    }

    #[test]
    fn large_size() {
        let mut data = vec![0, 0, 0, 1];
        data.extend_from_slice(b"mdat");
        data.extend_from_slice(&5_000_000_000u64.to_be_bytes());
        let h = read_box_header(&data).unwrap();
        assert_eq!(h.header_size, 16);
        assert_eq!(h.size, Some(5_000_000_000));
    }

    #[test]
    fn size_zero_extends_to_end() {
        let data = [0, 0, 0, 0, b'm', b'd', b'a', b't'];
        let h = read_box_header(&data).unwrap();
        assert_eq!(h.size, None);
        assert_eq!(h.content_size(), None);
    }

    #[test]
    fn uuid_box() {
        let mut data = vec![0, 0, 0, 40];
        data.extend_from_slice(b"uuid");
        data.extend_from_slice(&[7; 16]);
        let h = read_box_header(&data).unwrap();
        assert_eq!(h.header_size, 24);
        assert_eq!(h.user_type, Some([7; 16]));
    }

    #[test]
    fn truncated() {
        assert_eq!(read_box_header(&[0, 0, 0]), Err(Error::UnexpectedEof));
        assert_eq!(
            read_box_header(&[0, 0, 0, 1, b'm', b'd', b'a', b't', 0]),
            Err(Error::UnexpectedEof)
        );
    }

    #[test]
    fn iterates_boxes() {
        let mut data = vec![0, 0, 0, 10];
        data.extend_from_slice(b"free");
        data.extend_from_slice(&[1, 2]);
        data.extend_from_slice(&[0, 0, 0, 0]);
        data.extend_from_slice(b"mdat");
        data.extend_from_slice(&[3, 4, 5]);
        let boxes: Vec<_> = BoxIter::new(&data).map(Result::unwrap).collect();
        assert_eq!(boxes.len(), 2);
        assert_eq!(boxes[0].content, &[1, 2]);
        assert_eq!(boxes[1].offset, 10);
        assert_eq!(boxes[1].content, &[3, 4, 5]);
    }

    #[test]
    fn iter_stops_on_overflowing_box() {
        let mut data = vec![0, 0, 0, 100];
        data.extend_from_slice(b"free");
        let mut it = BoxIter::new(&data);
        assert!(matches!(it.next(), Some(Err(Error::UnexpectedEof))));
        assert!(it.next().is_none());
    }

    #[test]
    fn size_smaller_than_header() {
        let data = [0, 0, 0, 4, b'f', b'r', b'e', b'e'];
        assert!(matches!(
            read_box_header(&data),
            Err(Error::InvalidBoxSize { size: 4, .. })
        ));
    }
}
