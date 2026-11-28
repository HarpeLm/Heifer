//! Symbol decoder (AV1 §8.2): the multi-symbol arithmetic decoder that reads every tile.
//!
//! Each symbol is read with a cumulative distribution function (CDF): `N` increasing 15-bit
//! values ending with 32768, then an adaptation counter. Unless `disable_cdf_update` is set, the
//! CDF moves towards each decoded symbol.

use crate::Error;
use crate::bits::BitReader;

const EC_PROB_SHIFT: u32 = 6;
const EC_MIN_PROB: u32 = 4;

/// Reads symbols from the data of one tile.
#[derive(Debug, Clone)]
pub struct SymbolDecoder<'a> {
    bits: BitReader<'a>,
    value: u32,
    range: u32,
    /// `SymbolMaxBits`: bits left in the tile, minus the 15 already loaded. Goes negative near
    /// the end, where zero bits are read.
    max_bits: i64,
    disable_update: bool,
}

impl<'a> SymbolDecoder<'a> {
    /// `init_symbol()` on a tile's data.
    pub fn new(data: &'a [u8], disable_cdf_update: bool) -> Result<Self, Error> {
        if data.is_empty() {
            return Err(Error::Invalid("empty tile"));
        }
        let mut bits = BitReader::new(data);
        let num_bits = (data.len() * 8).min(15) as u32;
        let buf = bits.f(num_bits)?;
        let padded = buf << (15 - num_bits);
        Ok(Self {
            bits,
            value: ((1 << 15) - 1) ^ padded,
            range: 1 << 15,
            max_bits: 8 * data.len() as i64 - 15,
            disable_update: disable_cdf_update,
        })
    }

    /// `read_symbol()`: a symbol below `N = cdf.len() - 1`, adapting `cdf`.
    pub fn read_symbol(&mut self, cdf: &mut [u16]) -> Result<usize, Error> {
        let n = cdf.len() - 1;
        debug_assert!(n >= 2 && cdf[n - 1] == 1 << 15);
        let mut cur = self.range;
        let mut prev;
        let mut symbol = 0;
        loop {
            prev = cur;
            let f = (1 << 15) - u32::from(cdf[symbol]);
            cur = (((self.range >> 8) * (f >> EC_PROB_SHIFT)) >> (7 - EC_PROB_SHIFT))
                + EC_MIN_PROB * (n - symbol - 1) as u32;
            if self.value >= cur {
                break;
            }
            symbol += 1;
        }
        self.range = prev - cur;
        self.value -= cur;
        self.renormalize()?;
        if !self.disable_update {
            adapt(cdf, symbol);
        }
        Ok(symbol)
    }

    /// `read_bool()`: one bit with even odds.
    pub fn read_bool(&mut self) -> Result<bool, Error> {
        // A fixed CDF: its adaptation is thrown away.
        let mut cdf = [1 << 14, 1 << 15, 0];
        Ok(self.read_symbol(&mut cdf)? == 1)
    }

    /// `read_literal(n)`: an `n`-bit unsigned value, most significant bit first.
    pub fn read_literal(&mut self, n: u32) -> Result<u32, Error> {
        let mut x = 0;
        for _ in 0..n {
            x = x << 1 | u32::from(self.read_bool()?);
        }
        Ok(x)
    }

    /// `exit_symbol()`: checks that the tile was not read past its end.
    pub fn finish(self) -> Result<(), Error> {
        if self.max_bits < -14 {
            return Err(Error::Invalid("tile data read past its end"));
        }
        Ok(())
    }

    fn renormalize(&mut self) -> Result<(), Error> {
        let bits = 15 - (31 - self.range.leading_zeros());
        self.range <<= bits;
        let num_bits = i64::from(bits).min(self.max_bits.max(0)) as u32;
        let new_data = self.bits.f(num_bits)?;
        let padded = new_data << (bits - num_bits);
        self.value = padded ^ (((self.value + 1) << bits) - 1);
        self.max_bits -= i64::from(bits);
        Ok(())
    }
}

/// The CDF update of `read_symbol()`.
fn adapt(cdf: &mut [u16], symbol: usize) {
    let n = cdf.len() - 1;
    let count = cdf[n];
    let rate = 3 + u32::from(count > 15) + u32::from(count > 31) + (n.ilog2()).min(2);
    let mut tmp = 0u16;
    for (i, p) in cdf[..n - 1].iter_mut().enumerate() {
        if i == symbol {
            tmp = 1 << 15;
        }
        if tmp < *p {
            *p -= (*p - tmp) >> rate;
        } else {
            *p += (tmp - *p) >> rate;
        }
    }
    cdf[n] += u16::from(count < 32);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reference encoder (the inverse of §8.2, as in libaom's `od_ec_encode_q15`), to check
    /// that decoding gives back what was encoded.
    struct Encoder {
        low: u64,
        rng: u32,
        cnt: i32,
        precarry: Vec<u16>,
    }

    impl Encoder {
        fn new() -> Self {
            Self {
                low: 0,
                rng: 0x8000,
                cnt: -9,
                precarry: Vec::new(),
            }
        }

        /// Encodes `symbol` with the decoder's CDF convention (increasing, ending at 32768).
        fn encode(&mut self, cdf: &mut [u16], symbol: usize, adapt_cdf: bool) {
            let n = cdf.len() - 1;
            // libaom stores 32768 - cdf.
            let icdf = |i: usize| -> u32 { 32768 - u32::from(cdf[i]) };
            let fl = if symbol > 0 { icdf(symbol - 1) } else { 32768 };
            let fh = icdf(symbol);
            let r = self.rng;
            let nsyms = n as u32;
            let s = symbol as u32;
            let mut l = self.low;
            let scaled = |f: u32, k: u32| {
                (((r >> 8) * (f >> EC_PROB_SHIFT)) >> (7 - EC_PROB_SHIFT))
                    + EC_MIN_PROB * (nsyms - k)
            };
            let new_r = if fl < 32768 {
                let u = scaled(fl, s);
                let v = scaled(fh, s + 1);
                l += u64::from(r - u);
                u - v
            } else {
                r - scaled(fh, s + 1)
            };
            self.normalize(l, new_r);
            if adapt_cdf {
                adapt(cdf, symbol);
            }
        }

        fn normalize(&mut self, mut low: u64, rng: u32) {
            let d = 16 - (32 - rng.leading_zeros()) as i32;
            let mut c = self.cnt;
            let mut s = c + d;
            if s >= 0 {
                c += 16;
                let mut m = (1u64 << c) - 1;
                if s >= 8 {
                    self.precarry.push((low >> c) as u16);
                    low &= m;
                    c -= 8;
                    m >>= 8;
                }
                self.precarry.push((low >> c) as u16);
                s = c + d - 24;
                low &= m;
            }
            self.low = low << d;
            self.rng = rng << d;
            self.cnt = s;
        }

        fn finish(mut self) -> Vec<u8> {
            let l = self.low;
            let mut c = self.cnt;
            let mut s = 10;
            let m = 0x3FFFu64;
            let mut e = ((l + m) & !m) | (m + 1);
            s += c;
            if s > 0 {
                let mut n = (1u64 << (c + 16)) - 1;
                loop {
                    self.precarry.push((e >> (c + 16)) as u16);
                    e &= n;
                    s -= 8;
                    c -= 8;
                    n >>= 8;
                    if s <= 0 {
                        break;
                    }
                }
            }
            let mut out = vec![0u8; self.precarry.len()];
            let mut carry = 0u16;
            for i in (0..self.precarry.len()).rev() {
                let v = self.precarry[i] + carry;
                out[i] = v as u8;
                carry = v >> 8;
            }
            out
        }
    }

    /// A small deterministic generator.
    fn lcg(state: &mut u32) -> u32 {
        *state = state.wrapping_mul(1_103_515_245).wrapping_add(12345);
        *state >> 16
    }

    fn round_trip(disable_update: bool) {
        let initial: Vec<Vec<u16>> = vec![
            vec![16384, 32768, 0],
            vec![4000, 12000, 30000, 32768, 0],
            vec![1000, 2000, 3000, 4000, 5000, 6000, 7000, 32768, 0],
            vec![32000, 32700, 32768, 0],
        ];
        let mut seed = 7;
        let script: Vec<(usize, usize)> = (0..5000)
            .map(|_| {
                let which = lcg(&mut seed) as usize % initial.len();
                let n = initial[which].len() - 1;
                // Favour the likely symbols, as real data does.
                let symbol = (lcg(&mut seed) as usize % (n + 2)).min(n - 1);
                (which, symbol)
            })
            .collect();
        let mut enc_cdfs = initial.clone();
        let mut enc = Encoder::new();
        for &(which, symbol) in &script {
            enc.encode(&mut enc_cdfs[which], symbol, !disable_update);
        }
        let data = enc.finish();
        let mut dec_cdfs = initial.clone();
        let mut dec = SymbolDecoder::new(&data, disable_update).unwrap();
        for (i, &(which, symbol)) in script.iter().enumerate() {
            assert_eq!(
                dec.read_symbol(&mut dec_cdfs[which]).unwrap(),
                symbol,
                "symbol {i}"
            );
        }
        assert_eq!(enc_cdfs, dec_cdfs);
        if disable_update {
            assert_eq!(dec_cdfs, initial);
        }
        dec.finish().unwrap();
    }

    #[test]
    fn decodes_what_the_reference_encoder_wrote() {
        round_trip(false);
        round_trip(true);
    }

    #[test]
    fn bools_and_literals() {
        let mut enc = Encoder::new();
        let values = [0x2Au32, 0x3, 0xFFFF, 0];
        for (&v, n) in values.iter().zip([6, 2, 16, 5]) {
            for i in (0..n).rev() {
                enc.encode(&mut [1 << 14, 1 << 15, 0], (v >> i & 1) as usize, false);
            }
        }
        let data = enc.finish();
        let mut dec = SymbolDecoder::new(&data, false).unwrap();
        for (&v, n) in values.iter().zip([6, 2, 16, 5]) {
            assert_eq!(dec.read_literal(n).unwrap(), v);
        }
        dec.finish().unwrap();
    }

    #[test]
    fn adaptation_moves_towards_the_symbol() {
        let mut cdf = [8192, 16384, 24576, 32768, 0];
        adapt(&mut cdf, 0);
        // rate = 3 + 0 + 0 + min(log2(4), 2) = 5: each value moves 1/32 of the way to 32768.
        assert_eq!(
            cdf,
            [
                8192 + (24576 >> 5),
                16384 + (16384 >> 5),
                24576 + (8192 >> 5),
                32768,
                1
            ]
        );
        // Eight symbols or more: the size term of the rate stops at 2.
        let mut cdf = [4096, 8192, 12288, 16384, 20480, 24576, 28672, 32768, 0];
        adapt(&mut cdf, 7);
        assert_eq!(
            cdf[..3],
            [4096 - (4096 >> 5), 8192 - (8192 >> 5), 12288 - (12288 >> 5)]
        );
        let mut cdf = [8192, 16384, 24576, 32768, 40];
        adapt(&mut cdf, 3);
        // A counter above 31 gives rate 3 + 1 + 1 + 2 = 7 and is not incremented; values move to 0.
        assert_eq!(
            cdf,
            [
                8192 - (8192 >> 7),
                16384 - (16384 >> 7),
                24576 - (24576 >> 7),
                32768,
                40
            ]
        );
    }

    #[test]
    fn reading_far_past_the_end_is_an_error() {
        let mut dec = SymbolDecoder::new(&[0x00], false).unwrap();
        for _ in 0..64 {
            dec.read_bool().unwrap();
        }
        assert!(dec.finish().is_err());
        assert!(SymbolDecoder::new(&[], false).is_err());
    }

    #[test]
    fn default_cdfs_are_valid() {
        let tables = crate::cdf_tables::all();
        assert_eq!(tables.len(), 96);
        let mut rows = 0;
        for (name, values, n) in tables {
            assert!(n >= 3 && values.len() % n == 0, "{name}");
            for cdf in values.chunks(n) {
                assert_eq!(cdf[n - 1], 0, "{name}: counter");
                assert_eq!(cdf[n - 2], 1 << 15, "{name}: last value");
                assert!(
                    cdf.windows(2).take(n - 2).all(|w| w[0] <= w[1]),
                    "{name}: {cdf:?}"
                );
                rows += 1;
            }
        }
        assert_eq!(rows, 4_011, "CDFs");
        // A few values checked against the specification text by hand.
        assert_eq!(crate::cdf_tables::DEFAULT_INTRABC_CDF, [30531, 32768, 0]);
        assert_eq!(
            crate::cdf_tables::DEFAULT_FILTER_INTRA_MODE_CDF,
            [8949, 12776, 17211, 29558, 32768, 0]
        );
        assert_eq!(
            crate::cdf_tables::DEFAULT_SEGMENT_ID_PREDICTED_CDF[2],
            [16384, 32768, 0]
        );
    }
}
