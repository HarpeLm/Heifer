//! Bit-level reader for HEVC syntax elements (H.265 §7.2 and §9.2).
//!
//! HEVC headers are read bit by bit, most significant bit first. Besides fixed-size
//! fields (`u(n)`), many values use Exp-Golomb codes (`ue(v)`, `se(v)`), which are short
//! for small numbers:
//!
//! | value | `ue(v)` code |
//! |-------|--------------|
//! | 0     | `1`          |
//! | 1     | `010`        |
//! | 2     | `011`        |
//! | 3     | `00100`      |
//! | 4     | `00101`      |
//!
//! The number of leading zeros gives the length of the suffix.
//!
//! The reader works on RBSP data, i.e. after emulation prevention bytes have been
//! removed (see [`crate::nal::unescape_rbsp`]).

use crate::Error;

/// Reads bits from a byte slice, most significant bit first.
#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Position in bits from the start of `data`.
    pos: usize,
}

impl<'a> BitReader<'a> {
    /// Creates a reader positioned at the first bit of `data`.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Current position, in bits.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Number of bits left.
    pub fn bits_left(&self) -> usize {
        self.data.len() * 8 - self.pos
    }

    /// Whether the position is at a byte boundary (`byte_aligned()` in the spec).
    pub fn is_byte_aligned(&self) -> bool {
        self.pos % 8 == 0
    }

    /// Reads one bit.
    pub fn bit(&mut self) -> Result<u8, Error> {
        let byte = *self.data.get(self.pos / 8).ok_or(Error::UnexpectedEof)?;
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Ok(bit)
    }

    /// Reads one bit as a boolean (`u(1)` used as a flag).
    pub fn flag(&mut self) -> Result<bool, Error> {
        self.bit().map(|b| b == 1)
    }

    /// Reads `n` bits as an unsigned integer (`u(n)`), with `n <= 32`.
    pub fn bits(&mut self, n: u32) -> Result<u32, Error> {
        assert!(n <= 32, "cannot read more than 32 bits at once");
        if (n as usize) > self.bits_left() {
            return Err(Error::UnexpectedEof);
        }
        let mut value: u64 = 0;
        let mut remaining = n;
        while remaining > 0 {
            let byte = self.data[self.pos / 8];
            let offset = (self.pos % 8) as u32;
            let take = remaining.min(8 - offset);
            let chunk = (u32::from(byte) >> (8 - offset - take)) & ((1 << take) - 1);
            value = (value << take) | u64::from(chunk);
            self.pos += take as usize;
            remaining -= take;
        }
        Ok(value as u32)
    }

    /// Skips `n` bits.
    pub fn skip(&mut self, n: usize) -> Result<(), Error> {
        if n > self.bits_left() {
            return Err(Error::UnexpectedEof);
        }
        self.pos += n;
        Ok(())
    }

    /// Reads an unsigned Exp-Golomb code (`ue(v)`, H.265 §9.2).
    ///
    /// Values up to `2^32 - 2` are allowed by the spec; longer codes are rejected.
    pub fn ue(&mut self) -> Result<u32, Error> {
        let mut leading_zeros = 0u32;
        while self.bit()? == 0 {
            leading_zeros += 1;
            if leading_zeros > 31 {
                return Err(Error::Invalid("Exp-Golomb code longer than 32 bits"));
            }
        }
        if leading_zeros == 0 {
            return Ok(0);
        }
        let suffix = self.bits(leading_zeros)?;
        // 2^lz - 1 + suffix, computed in u64 to avoid overflow for lz == 31... then checked.
        let value = (1u64 << leading_zeros) - 1 + u64::from(suffix);
        u32::try_from(value).map_err(|_| Error::Invalid("Exp-Golomb value out of range"))
    }

    /// Reads a signed Exp-Golomb code (`se(v)`, H.265 §9.2.2).
    ///
    /// Mapping: 0 → 0, 1 → 1, 2 → -1, 3 → 2, 4 → -2...
    pub fn se(&mut self) -> Result<i32, Error> {
        let k = i64::from(self.ue()?);
        let value = if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) };
        Ok(value as i32)
    }

    /// Reads `ue(v)` and checks that it is at most `max`.
    pub fn ue_max(&mut self, max: u32, what: &'static str) -> Result<u32, Error> {
        let v = self.ue()?;
        if v > max {
            Err(Error::Invalid(what))
        } else {
            Ok(v)
        }
    }

    /// Reads `se(v)` and checks that it lies in `min..=max`.
    pub fn se_range(&mut self, min: i32, max: i32, what: &'static str) -> Result<i32, Error> {
        let v = self.se()?;
        if (min..=max).contains(&v) {
            Ok(v)
        } else {
            Err(Error::Invalid(what))
        }
    }

    /// `more_rbsp_data()` (H.265 §7.2): true if there is data before the RBSP trailing bits
    /// (a final `1` bit followed by zero bits up to the end).
    pub fn more_rbsp_data(&self) -> bool {
        let Some(last) = self.data.iter().rposition(|&b| b != 0) else {
            return false;
        };
        // Bit position of the stop bit (the last `1` in the data).
        let stop_bit = last * 8 + 7 - self.data[last].trailing_zeros() as usize;
        self.pos < stop_bit
    }

    /// Reads `rbsp_trailing_bits()`: one `1` bit then zero bits up to the next byte boundary.
    pub fn rbsp_trailing_bits(&mut self) -> Result<(), Error> {
        if self.bit()? != 1 {
            return Err(Error::Invalid("rbsp_stop_one_bit must be 1"));
        }
        while !self.is_byte_aligned() {
            if self.bit()? != 0 {
                return Err(Error::Invalid("rbsp_alignment_zero_bit must be 0"));
            }
        }
        Ok(())
    }

    /// Skips bits up to the next byte boundary.
    pub fn align(&mut self) {
        self.pos = self.pos.next_multiple_of(8).min(self.data.len() * 8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_bits_msb_first() {
        let mut r = BitReader::new(&[0b1010_0000]);
        assert_eq!(r.bit(), Ok(1));
        assert_eq!(r.bit(), Ok(0));
        assert_eq!(r.flag(), Ok(true));
        assert_eq!(r.position(), 3);
        assert_eq!(r.bits_left(), 5);
    }

    #[test]
    fn multi_bit_reads_across_bytes() {
        let mut r = BitReader::new(&[0xAB, 0xCD, 0xEF, 0x12, 0x34]);
        assert_eq!(r.bits(4), Ok(0xA));
        assert_eq!(r.bits(8), Ok(0xBC));
        assert_eq!(r.bits(0), Ok(0));
        assert_eq!(r.bits(28), Ok(0xDEF_1234));
        assert_eq!(r.bits(1), Err(Error::UnexpectedEof));
    }

    #[test]
    fn full_32_bits() {
        let mut r = BitReader::new(&[0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(r.bits(32), Ok(u32::MAX));
    }

    #[test]
    fn unsigned_exp_golomb_table() {
        // 1 | 010 | 011 | 00100 | 00101 | 0001000 => values 0,1,2,3,4,7
        // bits: 1010 0110 0100 0010 1000 1000 (+ padding)
        let mut r = BitReader::new(&[0b1010_0110, 0b0100_0010, 0b1000_1000]);
        for expected in [0, 1, 2, 3, 4, 7] {
            assert_eq!(r.ue(), Ok(expected));
        }
    }

    #[test]
    fn signed_exp_golomb() {
        // ue codes for 0,1,2,3,4 => se 0, 1, -1, 2, -2
        let mut r = BitReader::new(&[0b1010_0110, 0b0100_0010, 0b1000_0000]);
        for expected in [0, 1, -1, 2, -2] {
            assert_eq!(r.se(), Ok(expected));
        }
    }

    #[test]
    fn largest_exp_golomb_value() {
        // 31 zeros, a 1, then 31 suffix bits all set => 2^32 - 2.
        let mut bits = vec![0u8; 3];
        bits.extend_from_slice(&[0b0000_0001, 0xFF, 0xFF, 0xFF, 0xFE]);
        // 24 + 7 = 31 zeros, then '1', then 31 ones, then a final 0.
        let mut r = BitReader::new(&bits);
        assert_eq!(r.ue(), Ok(u32::MAX - 1));
    }

    #[test]
    fn exp_golomb_too_long() {
        let mut r = BitReader::new(&[0, 0, 0, 0, 0xFF]);
        assert!(matches!(r.ue(), Err(Error::Invalid(_))));
    }

    #[test]
    fn exp_golomb_truncated() {
        let mut r = BitReader::new(&[0b0000_0001]);
        assert_eq!(r.ue(), Err(Error::UnexpectedEof));
    }

    #[test]
    fn range_checks() {
        let mut r = BitReader::new(&[0b0010_0000]); // ue = 3
        assert_eq!(r.ue_max(2, "too big"), Err(Error::Invalid("too big")));
    }

    #[test]
    fn trailing_bits_and_more_data() {
        // payload "1" (one bit: flag=1), then stop bit 1, then zeros.
        let data = [0b1100_0000];
        let mut r = BitReader::new(&data);
        assert!(r.more_rbsp_data());
        assert_eq!(r.flag(), Ok(true));
        assert!(!r.more_rbsp_data());
        assert_eq!(r.rbsp_trailing_bits(), Ok(()));
        assert!(r.is_byte_aligned());
    }

    #[test]
    fn more_rbsp_data_ignores_trailing_zero_bytes() {
        let data = [0b0110_0000, 0x80, 0x00, 0x00];
        let mut r = BitReader::new(&data);
        r.skip(8).unwrap();
        assert!(!r.more_rbsp_data());
    }

    #[test]
    fn align() {
        let mut r = BitReader::new(&[0xFF, 0x0F]);
        r.skip(3).unwrap();
        r.align();
        assert_eq!(r.bits(8), Ok(0x0F));
        r.align();
        assert_eq!(r.bits_left(), 0);
    }
}
