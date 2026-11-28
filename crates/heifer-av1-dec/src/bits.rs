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

    /// `su(n)` (§4.10.6): an `n`-bit two's complement value.
    pub(crate) fn su(&mut self, n: u32) -> Result<i32, Error> {
        let value = self.f(n)? as i64;
        let sign = 1i64 << (n - 1);
        Ok((if value & sign != 0 {
            value - 2 * sign
        } else {
            value
        }) as i32)
    }

    /// `ns(n)` (§4.10.7): a value below `n`, `n >= 1`, with a short code for small values.
    pub(crate) fn ns(&mut self, n: u32) -> Result<u32, Error> {
        debug_assert!(n >= 1);
        let w = 32 - n.leading_zeros();
        let m = (1u32 << w) - n;
        let v = self.f(w - 1)?;
        if v < m {
            return Ok(v);
        }
        Ok((v << 1) - m + u32::from(self.bit()?))
    }

    /// `trailing_bits()` (§5.3.4): a one bit, then zero bits to the end of the data.
    pub(crate) fn trailing_bits(&mut self) -> Result<(), Error> {
        if !self.bit()? {
            return Err(Error::Invalid("trailing bits do not start with a one"));
        }
        self.zero_padding(self.data.len() * 8)
    }

    /// Checks that the bits up to `end` (a bit position) are zero, and skips them.
    fn zero_padding(&mut self, end: usize) -> Result<(), Error> {
        while self.pos < end {
            if self.bit()? {
                return Err(Error::Invalid("padding bits are not zero"));
            }
        }
        Ok(())
    }

    /// `byte_alignment()`: skips to the next byte boundary. Returns the bytes read so far.
    pub(crate) fn byte_align(&mut self) -> Result<usize, Error> {
        self.zero_padding(self.pos.div_ceil(8) * 8)?;
        Ok(self.pos / 8)
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
/// Packs `(value, bits)` fields MSB first, padded with zeros.
pub(crate) fn pack(fields: &[(u32, u32)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut acc = 0u64;
    let mut n = 0;
    for &(v, bits) in fields {
        for i in (0..bits).rev() {
            acc = acc << 1 | u64::from(v >> i & 1);
            n += 1;
            if n == 8 {
                out.push(acc as u8);
                (acc, n) = (0, 0);
            }
        }
    }
    if n > 0 {
        out.push((acc << (8 - n)) as u8);
    }
    out
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
    fn signed_and_non_symmetric_codes() {
        let mut r = BitReader::new(&[0b1111_0001, 0b1000_0000]);
        assert_eq!(r.su(4).unwrap(), -1);
        assert_eq!(r.su(4).unwrap(), 1);
        // ns(5): w = 3, m = 3. "10" → 2 and "00" → 0 are below m; "110" and "111" read one more
        // bit: (3 << 1) - 3 + bit.
        let mut r = BitReader::new(&[0b1000_1101, 0b1100_0000]);
        assert_eq!(r.ns(5).unwrap(), 2);
        assert_eq!(r.ns(5).unwrap(), 0);
        assert_eq!(r.ns(5).unwrap(), 3);
        assert_eq!(r.ns(5).unwrap(), 4);
        // ns(1) reads nothing.
        assert_eq!(r.ns(1).unwrap(), 0);
        // 10 bits read: alignment ends at byte 2.
        assert_eq!(r.byte_align(), Ok(2));
        assert_eq!(BitReader::new(&[0x01]).byte_align(), Ok(0));
        let mut r = BitReader::new(&[0b1010_0000]);
        r.f(1).unwrap();
        assert!(r.byte_align().is_err());
        let mut r = BitReader::new(&[0b0100_0000]);
        r.f(1).unwrap();
        assert!(r.clone().trailing_bits().is_ok());
        r.f(1).unwrap();
        assert!(r.trailing_bits().is_err());
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
