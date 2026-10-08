//! Image metadata: EXIF, XMP and ICC profile, plus a minimal EXIF (TIFF) reader for the most
//! common fields.

use heifer_isobmff::{HeifFile, ItemId};

use crate::Error;

/// Metadata attached to an image.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Metadata {
    /// Raw EXIF data (TIFF structure, starting with `II*\0` or `MM\0*`).
    pub exif: Option<Vec<u8>>,
    /// XMP packet (XML).
    pub xmp: Option<Vec<u8>>,
    /// Embedded ICC colour profile.
    pub icc_profile: Option<Vec<u8>>,
}

impl Metadata {
    /// Parsed view of the EXIF data, if any.
    pub fn exif_fields(&self) -> Option<Exif<'_>> {
        self.exif.as_deref().and_then(Exif::parse)
    }
}

/// Reads the metadata of the primary image of a HEIF file, without decoding pixels.
pub fn read_metadata(bytes: &[u8]) -> Result<Metadata, Error> {
    let file = HeifFile::parse(bytes)?;
    item_metadata(&file, file.primary_id)
}

/// Reads the metadata of one image item.
pub fn item_metadata(file: &HeifFile<'_>, id: ItemId) -> Result<Metadata, Error> {
    Ok(Metadata {
        exif: file.exif(id)?,
        xmp: file.xmp(id)?,
        icc_profile: file.icc_profile(id)?.map(<[u8]>::to_vec),
    })
}

/// A minimal, bounds-checked reader for EXIF (TIFF) data.
#[derive(Debug, Clone, Copy)]
pub struct Exif<'a> {
    data: &'a [u8],
    big_endian: bool,
    ifd0: usize,
}

/// EXIF tag numbers used by [`Exif`].
mod tag {
    pub const MAKE: u16 = 0x010F;
    pub const MODEL: u16 = 0x0110;
    pub const ORIENTATION: u16 = 0x0112;
    pub const SOFTWARE: u16 = 0x0131;
    pub const DATE_TIME: u16 = 0x0132;
    pub const EXIF_IFD: u16 = 0x8769;
    pub const GPS_IFD: u16 = 0x8825;
    pub const DATE_TIME_ORIGINAL: u16 = 0x9003;
}

impl<'a> Exif<'a> {
    /// Parses the TIFF header. Returns `None` if the data is not valid TIFF.
    pub fn parse(data: &'a [u8]) -> Option<Self> {
        let big_endian = match data.get(..4)? {
            b"II*\0" => false,
            b"MM\0*" => true,
            _ => return None,
        };
        let mut e = Self {
            data,
            big_endian,
            ifd0: 0,
        };
        e.ifd0 = e.u32(4)? as usize;
        Some(e)
    }

    fn u16(&self, at: usize) -> Option<u16> {
        let b: [u8; 2] = self.data.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(if self.big_endian {
            u16::from_be_bytes(b)
        } else {
            u16::from_le_bytes(b)
        })
    }

    fn u32(&self, at: usize) -> Option<u32> {
        let b: [u8; 4] = self.data.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(if self.big_endian {
            u32::from_be_bytes(b)
        } else {
            u32::from_le_bytes(b)
        })
    }

    /// Finds a tag in the IFD at `ifd`: returns (type, count, value offset field position).
    fn find(&self, ifd: usize, wanted: u16) -> Option<(u16, u32, usize)> {
        let n = self.u16(ifd)? as usize;
        (0..n.min(1024)).find_map(|i| {
            let entry = ifd.checked_add(2 + 12 * i)?;
            (self.u16(entry)? == wanted)
                .then(|| Some((self.u16(entry + 2)?, self.u32(entry + 4)?, entry + 8)))?
        })
    }

    fn short(&self, ifd: usize, wanted: u16) -> Option<u16> {
        let (kind, _, at) = self.find(ifd, wanted)?;
        match kind {
            3 => self.u16(at),
            4 => self.u32(at).and_then(|v| u16::try_from(v).ok()),
            _ => None,
        }
    }

    fn long(&self, ifd: usize, wanted: u16) -> Option<u32> {
        let (kind, _, at) = self.find(ifd, wanted)?;
        match kind {
            4 | 13 => self.u32(at),
            3 => self.u16(at).map(u32::from),
            _ => None,
        }
    }

    fn ascii(&self, ifd: usize, wanted: u16) -> Option<&'a str> {
        let (kind, count, at) = self.find(ifd, wanted)?;
        if kind != 2 {
            return None;
        }
        let count = count as usize;
        let start = if count <= 4 {
            at
        } else {
            self.u32(at)? as usize
        };
        let bytes = self.data.get(start..start.checked_add(count)?)?;
        let bytes = bytes.split(|&b| b == 0).next()?;
        std::str::from_utf8(bytes)
            .ok()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    fn exif_ifd(&self) -> Option<usize> {
        self.long(self.ifd0, tag::EXIF_IFD).map(|v| v as usize)
    }

    /// EXIF orientation (1 to 8). Informative only in HEIF: heifer already applies the
    /// `irot`/`imir` transforms, so do not rotate the decoded image again.
    pub fn orientation(&self) -> Option<u16> {
        self.short(self.ifd0, tag::ORIENTATION)
            .filter(|v| (1..=8).contains(v))
    }

    /// Camera manufacturer.
    pub fn make(&self) -> Option<&'a str> {
        self.ascii(self.ifd0, tag::MAKE)
    }

    /// Camera model.
    pub fn model(&self) -> Option<&'a str> {
        self.ascii(self.ifd0, tag::MODEL)
    }

    /// Software that produced the file.
    pub fn software(&self) -> Option<&'a str> {
        self.ascii(self.ifd0, tag::SOFTWARE)
    }

    /// Capture date and time (`DateTimeOriginal`, else `DateTime`), as `YYYY:MM:DD HH:MM:SS`.
    pub fn date_time(&self) -> Option<&'a str> {
        self.exif_ifd()
            .and_then(|ifd| self.ascii(ifd, tag::DATE_TIME_ORIGINAL))
            .or_else(|| self.ascii(self.ifd0, tag::DATE_TIME))
    }

    /// Whether the image has GPS information.
    pub fn has_gps(&self) -> bool {
        self.long(self.ifd0, tag::GPS_IFD).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Little-endian TIFF with IFD0: Make (ASCII, out of line), Orientation = 6.
    fn sample() -> Vec<u8> {
        let mut d = b"II*\0".to_vec();
        d.extend_from_slice(&8u32.to_le_bytes());
        d.extend_from_slice(&2u16.to_le_bytes());
        // Make: type 2, count 7, offset 38
        d.extend_from_slice(&tag::MAKE.to_le_bytes());
        d.extend_from_slice(&2u16.to_le_bytes());
        d.extend_from_slice(&7u32.to_le_bytes());
        d.extend_from_slice(&38u32.to_le_bytes());
        // Orientation: type 3, count 1, value 6
        d.extend_from_slice(&tag::ORIENTATION.to_le_bytes());
        d.extend_from_slice(&3u16.to_le_bytes());
        d.extend_from_slice(&1u32.to_le_bytes());
        d.extend_from_slice(&[6, 0, 0, 0]);
        d.extend_from_slice(&0u32.to_le_bytes()); // next IFD
        d.extend_from_slice(b"Apple\0\0");
        d
    }

    #[test]
    fn reads_fields() {
        let data = sample();
        let e = Exif::parse(&data).unwrap();
        assert_eq!(e.orientation(), Some(6));
        assert_eq!(e.make(), Some("Apple"));
        assert_eq!(e.model(), None);
        assert!(!e.has_gps());
    }

    #[test]
    fn rejects_garbage_and_truncation() {
        assert!(Exif::parse(b"JUNK").is_none());
        let data = sample();
        for len in 0..data.len() {
            if let Some(e) = Exif::parse(&data[..len]) {
                let _ = (e.orientation(), e.make(), e.date_time(), e.has_gps());
            }
        }
    }
}
