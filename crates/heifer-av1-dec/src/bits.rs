//! Bit reader for AV1 syntax elements (AV1 §4.10 and §8.1).
//!
//! AV1 headers are read most significant bit first: fixed-size fields `f(n)`, variable-length
//! `uvlc()` codes, and `leb128()` sizes, which are byte-aligned.

use crate::Error;

/// Reads bits from a byte slice, most significant bit first.
#[derive(Debug, Clone)]
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    /// Position in bits from the start of `data`.
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// One bit.
    pub(crate) fn bit(&mut self) -> Result<bool, Error> {
        let byte = *self.data.get(self.pos / 8).ok_or(Error::UnexpectedEof)?;
        let bit = byte >> (7 - self.pos % 8) & 1;
        self.pos += 1;
        Ok(bit == 1)
    }

    /// `f(n)`: an `n`-bit unsigned value, `n <= 32`.
    pub(crate) fn f(&mut self, n: u32) -> Result<u32, Error> {
        debug_assert!(n <= 32);
        let mut value = 0u64;
        for _ in 0..n {
            value = value << 1 | u64::from(self.bit()?);
        }
        Ok(value as u32)
    }

    /// `f(1)` as a flag.
    pub(crate) fn flag(&mut self) -> Result<bool, Error> {
        self.bit()
    }

    /// `uvlc()` (§4.10.3): a variable-length unsigned code, saturating at 2³² − 1.
    pub(crate) fn uvlc(&mut self) -> Result<u32, Error> {
        let mut leading_zeros = 0u32;
        while !self.bit()? {
            leading_zeros += 1;
        }
        if leading_zeros >= 32 {
            return Ok(u32::MAX);
        }
        let value = self.f(leading_zeros)?;
        Ok(value + ((1u32 << leading_zeros) - 1))
    }
}

/// `leb128()` (§4.10.5) at the start of `data`: the value and the number of bytes it used.
pub(crate) fn leb128(data: &[u8]) -> Result<(u64, usize), Error> {
    let mut value = 0u64;
    for i in 0..8 {
        let byte = *data.get(i).ok_or(Error::UnexpectedEof)?;
        value |= u64::from(byte & 0x7F) << (7 * i);
        if byte & 0x80 == 0 {
            // The specification limits values to 32 bits.
            if value > u64::from(u32::MAX) {
                return Err(Error::Invalid("leb128 value larger than 32 bits"));
            }
            return Ok((value, i + 1));
        }
    }
    Err(Error::Invalid("leb128 value longer than 8 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_fields_are_read_msb_first() {
        let mut r = BitReader::new(&[0b1010_0000, 0xFF]);
        assert_eq!(r.f(3).unwrap(), 0b101);
        assert!(!r.flag().unwrap());
        assert_eq!(r.f(12).unwrap(), 0x0FF);
        assert_eq!(r.bit(), Err(Error::UnexpectedEof));
        assert_eq!(
            BitReader::new(&[0xDE, 0xAD, 0xBE, 0xEF]).f(32).unwrap(),
            0xDEAD_BEEF
        );
    }

    #[test]
    fn uvlc_codes() {
        // 1 → 0; 010 → 1; 011 → 2; 00100 → 3.
        let mut r = BitReader::new(&[0b1010_0110, 0b0100_0000]);
        assert_eq!(r.uvlc().unwrap(), 0);
        assert_eq!(r.uvlc().unwrap(), 1);
        assert_eq!(r.uvlc().unwrap(), 2);
        assert_eq!(r.uvlc().unwrap(), 3);
        // 32 leading zeros saturate.
        let mut r = BitReader::new(&[0, 0, 0, 0, 0x80]);
        assert_eq!(r.uvlc().unwrap(), u32::MAX);
        assert_eq!(BitReader::new(&[0, 0]).uvlc(), Err(Error::UnexpectedEof));
    }

    #[test]
    fn leb128_sizes() {
        assert_eq!(leb128(&[0x05]).unwrap(), (5, 1));
        assert_eq!(leb128(&[0xE5, 0x8E, 0x26, 0xAA]).unwrap(), (624_485, 3));
        // Padded encodings are allowed.
        assert_eq!(leb128(&[0x80, 0x80, 0x00]).unwrap(), (0, 3));
        assert_eq!(
            leb128(&[0xFF, 0xFF, 0xFF, 0xFF, 0x0F]).unwrap(),
            (u64::from(u32::MAX), 5)
        );
        assert!(leb128(&[0xFF, 0xFF, 0xFF, 0xFF, 0x10]).is_err());
        assert!(leb128(&[0x80; 8]).is_err());
        assert_eq!(leb128(&[0x80]), Err(Error::UnexpectedEof));
    }
}
