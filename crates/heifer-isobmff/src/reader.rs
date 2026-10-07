//! Bounds-checked big-endian reader over a byte slice.

use crate::{Error, FourCC};

pub(crate) struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub(crate) fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    pub(crate) fn bytes(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.pos.checked_add(n).ok_or(Error::UnexpectedEof)?;
        let out = self.data.get(self.pos..end).ok_or(Error::UnexpectedEof)?;
        self.pos = end;
        Ok(out)
    }

    pub(crate) fn rest(&mut self) -> &'a [u8] {
        let out = &self.data[self.pos..];
        self.pos = self.data.len();
        out
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        self.bytes(N)
            .map(|b| b.try_into().expect("slice has length N"))
    }

    pub(crate) fn u8(&mut self) -> Result<u8, Error> {
        self.array::<1>().map(|b| b[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16, Error> {
        self.array().map(u16::from_be_bytes)
    }

    pub(crate) fn u32(&mut self) -> Result<u32, Error> {
        self.array().map(u32::from_be_bytes)
    }

    pub(crate) fn i32(&mut self) -> Result<i32, Error> {
        self.array().map(i32::from_be_bytes)
    }

    pub(crate) fn u64(&mut self) -> Result<u64, Error> {
        self.array().map(u64::from_be_bytes)
    }

    pub(crate) fn fourcc(&mut self) -> Result<FourCC, Error> {
        self.array().map(FourCC)
    }

    /// Reads an unsigned integer stored on `size` bytes (0, 1, 2, 4 or 8). A size of 0 yields 0.
    pub(crate) fn uint(&mut self, size: u8) -> Result<u64, Error> {
        Ok(match size {
            0 => 0,
            1 => self.u8()?.into(),
            2 => self.u16()?.into(),
            4 => self.u32()?.into(),
            8 => self.u64()?,
            _ => return Err(Error::Unimplemented("integer field size")),
        })
    }

    /// Reads the version and flags of a FullBox.
    pub(crate) fn full_box(&mut self) -> Result<(u8, u32), Error> {
        let v = self.u32()?;
        Ok(((v >> 24) as u8, v & 0x00FF_FFFF))
    }

    /// Reads a NUL-terminated UTF-8 string (lossy). A missing terminator ends the string at the end of data.
    pub(crate) fn cstring(&mut self) -> String {
        let rest = &self.data[self.pos..];
        let len = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        self.pos += (len + 1).min(rest.len());
        String::from_utf8_lossy(&rest[..len]).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_big_endian() {
        let mut r = Reader::new(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07]);
        assert_eq!(r.u16(), Ok(0x0102));
        assert_eq!(r.u32(), Ok(0x0304_0506));
        assert_eq!(r.u16(), Err(Error::UnexpectedEof));
        assert_eq!(r.u8(), Ok(0x07));
    }

    #[test]
    fn variable_size_ints() {
        let mut r = Reader::new(&[0xAB, 0, 0, 0, 0x10]);
        assert_eq!(r.uint(0), Ok(0));
        assert_eq!(r.uint(1), Ok(0xAB));
        assert_eq!(r.uint(4), Ok(0x10));
        assert_eq!(r.uint(3), Err(Error::Unimplemented("integer field size")));
    }

    #[test]
    fn cstrings() {
        let mut r = Reader::new(b"abc\0de");
        assert_eq!(r.cstring(), "abc");
        assert_eq!(r.cstring(), "de");
        assert_eq!(r.remaining(), 0);
    }
}
