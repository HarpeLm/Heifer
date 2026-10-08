//! CABAC: context-adaptive binary arithmetic coding (H.265 §9.3).
//!
//! Almost every syntax element below the slice header is coded as a sequence of binary
//! decisions ("bins"). Each bin is decoded either:
//! - with a **context** ([`ContextModel`]): an adaptive probability estimate of the bin being
//!   0 or 1, updated after every use; or
//! - in **bypass** mode: probability 1/2, used for nearly random bits (signs, suffixes);
//! - with the **terminate** procedure, for `end_of_slice_segment_flag` and similar.
//!
//! The arithmetic engine keeps a 9-bit interval (`range`) and the current position in it
//! (`offset`). Probabilities are represented by 64 states per context, with fixed transition
//! and range tables shared with H.264.

use crate::Error;

/// `rangeTabLps[pStateIdx][qRangeIdx]` (Table 9-52).
#[rustfmt::skip]
pub const RANGE_TAB_LPS: [[u8; 4]; 64] = [
    [128, 176, 208, 240], [128, 167, 197, 227], [128, 158, 187, 216], [123, 150, 178, 205],
    [116, 142, 169, 195], [111, 135, 160, 185], [105, 128, 152, 175], [100, 122, 144, 166],
    [95, 116, 137, 158], [90, 110, 130, 150], [85, 104, 123, 142], [81, 99, 117, 135],
    [77, 94, 111, 128], [73, 89, 105, 122], [69, 85, 100, 116], [66, 80, 95, 110],
    [62, 76, 90, 104], [59, 72, 86, 99], [56, 69, 81, 94], [53, 65, 77, 89],
    [51, 62, 73, 85], [48, 59, 69, 80], [46, 56, 66, 76], [43, 53, 63, 72],
    [41, 50, 59, 69], [39, 48, 56, 65], [37, 45, 54, 62], [35, 43, 51, 59],
    [33, 41, 48, 56], [32, 39, 46, 53], [30, 37, 43, 50], [29, 35, 41, 48],
    [27, 33, 39, 45], [26, 31, 37, 43], [24, 30, 35, 41], [23, 28, 33, 39],
    [22, 27, 32, 37], [21, 26, 30, 35], [20, 24, 29, 33], [19, 23, 27, 31],
    [18, 22, 26, 30], [17, 21, 25, 28], [16, 20, 23, 27], [15, 19, 22, 25],
    [14, 18, 21, 24], [14, 17, 20, 23], [13, 16, 19, 22], [12, 15, 18, 21],
    [12, 14, 17, 20], [11, 14, 16, 19], [11, 13, 15, 18], [10, 12, 15, 17],
    [10, 12, 14, 16], [9, 11, 13, 15], [9, 11, 12, 14], [8, 10, 12, 14],
    [8, 9, 11, 13], [7, 9, 11, 12], [7, 9, 10, 12], [7, 8, 10, 11],
    [6, 8, 9, 11], [6, 7, 9, 10], [6, 7, 8, 9], [2, 2, 2, 2],
];

/// `transIdxLps[pStateIdx]` (Table 9-53): next state after a least probable symbol.
#[rustfmt::skip]
pub const TRANS_IDX_LPS: [u8; 64] = [
    0, 0, 1, 2, 2, 4, 4, 5, 6, 7, 8, 9, 9, 11, 11, 12,
    13, 13, 15, 15, 16, 16, 18, 18, 19, 19, 21, 21, 22, 22, 23, 24,
    24, 25, 26, 26, 27, 27, 28, 29, 29, 30, 30, 30, 31, 32, 32, 33,
    33, 33, 34, 34, 35, 35, 35, 36, 36, 36, 37, 37, 37, 38, 38, 63,
];

/// One context variable: a probability state and the value of the most probable symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ContextModel {
    /// `pStateIdx` (0..=62; 63 is reserved for the terminate procedure).
    pub state: u8,
    /// `valMps`: the most probable bin value.
    pub mps: u8,
}

impl ContextModel {
    /// Initializes a context from its `initValue` and the slice QP (§9.3.2.2).
    pub fn new(init_value: u8, slice_qp_y: i32) -> Self {
        let slope_idx = i32::from(init_value >> 4);
        let offset_idx = i32::from(init_value & 15);
        let m = slope_idx * 5 - 45;
        let n = (offset_idx << 3) - 16;
        let pre_ctx_state = (((m * slice_qp_y.clamp(0, 51)) >> 4) + n).clamp(1, 126);
        if pre_ctx_state <= 63 {
            Self {
                state: (63 - pre_ctx_state) as u8,
                mps: 0,
            }
        } else {
            Self {
                state: (pre_ctx_state - 64) as u8,
                mps: 1,
            }
        }
    }

    /// State update after coding `bin` (§9.3.4.3.2.2).
    #[inline]
    pub fn update(&mut self, bin: u8) {
        if bin == self.mps {
            self.state = (self.state + 1).min(62);
        } else {
            if self.state == 0 {
                self.mps = 1 - self.mps;
            }
            self.state = TRANS_IDX_LPS[usize::from(self.state)];
        }
    }
}

/// The arithmetic decoding engine (§9.3.4.3).
///
/// Reads RBSP data (emulation prevention bytes already removed). Bits past the end of the
/// data are read as zeros, as can legitimately happen at the very end of a slice; reading far
/// beyond is reported as an error by [`ArithmeticDecoder::check_overrun`].
#[derive(Debug, Clone)]
pub struct ArithmeticDecoder<'a> {
    data: &'a [u8],
    /// Position of the next bit to read.
    pos: usize,
    range: u32,
    offset: u32,
}

impl<'a> ArithmeticDecoder<'a> {
    /// Initializes the engine at the start of `data` (§9.3.2.5): reads 9 bits.
    pub fn new(data: &'a [u8]) -> Result<Self, Error> {
        let mut d = Self {
            data,
            pos: 0,
            range: 510,
            offset: 0,
        };
        for _ in 0..9 {
            d.offset = (d.offset << 1) | d.read_bit();
        }
        if d.offset >= 510 {
            return Err(Error::Invalid("CABAC initial offset must be < 510"));
        }
        Ok(d)
    }

    #[inline]
    fn read_bit(&mut self) -> u32 {
        let bit = self
            .data
            .get(self.pos / 8)
            .map_or(0, |b| u32::from(b >> (7 - self.pos % 8)) & 1);
        self.pos += 1;
        bit
    }

    #[inline]
    fn renormalize(&mut self) {
        while self.range < 256 {
            self.range <<= 1;
            self.offset = (self.offset << 1) | self.read_bit();
        }
    }

    /// Decodes one bin with a context (`DecodeDecision`, §9.3.4.3.2).
    #[inline]
    pub fn decode(&mut self, ctx: &mut ContextModel) -> u8 {
        let q = ((self.range >> 6) & 3) as usize;
        let lps = u32::from(RANGE_TAB_LPS[usize::from(ctx.state)][q]);
        self.range -= lps;
        let bin = if self.offset >= self.range {
            self.offset -= self.range;
            self.range = lps;
            1 - ctx.mps
        } else {
            ctx.mps
        };
        ctx.update(bin);
        self.renormalize();
        bin
    }

    /// Decodes one equiprobable bin (`DecodeBypass`, §9.3.4.3.4).
    #[inline]
    pub fn decode_bypass(&mut self) -> u8 {
        self.offset = (self.offset << 1) | self.read_bit();
        if self.offset >= self.range {
            self.offset -= self.range;
            1
        } else {
            0
        }
    }

    /// Decodes `n` bypass bins as an unsigned integer, most significant first (`n <= 32`).
    pub fn decode_bypass_bits(&mut self, n: u32) -> u32 {
        (0..n).fold(0, |acc, _| (acc << 1) | u32::from(self.decode_bypass()))
    }

    /// Decodes a terminating bin (`DecodeTerminate`, §9.3.4.3.5), used for
    /// `end_of_slice_segment_flag`, `end_of_subset_one_bit` and `pcm_flag`.
    ///
    /// When it returns 1, the arithmetic decoding of the substream is finished; use
    /// [`ArithmeticDecoder::aligned_position_after_terminate`] to find the following data.
    pub fn decode_terminate(&mut self) -> u8 {
        self.range -= 2;
        if self.offset >= self.range {
            1
        } else {
            self.renormalize();
            0
        }
    }

    /// After a terminating bin equal to 1: byte position of the data that follows, i.e. after
    /// the `rbsp_stop_one_bit` / alignment bits (for PCM samples or the next substream).
    ///
    /// When the terminating bin is decoded, the last bit read by the engine is the last bit
    /// written by the encoder's flush, i.e. the stop bit (§9.3.4.3.5). The following data
    /// starts at the next byte boundary.
    pub fn aligned_position_after_terminate(&self) -> usize {
        self.pos.div_ceil(8)
    }

    /// Returns an error if the engine read clearly beyond the end of the data.
    pub fn check_overrun(&self) -> Result<(), Error> {
        if self.pos > self.data.len() * 8 + 16 {
            Err(Error::UnexpectedEof)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_init_examples() {
        // initValue 154 is "equiprobable": m = 0, n = 64 -> preCtxState 64 -> state 0, MPS 1.
        assert_eq!(
            ContextModel::new(154, 26),
            ContextModel { state: 0, mps: 1 }
        );
        // initValue 139 (split_cu_flag ctx 0) at QP 22: slope 8 -> m = -5; offset 11 -> n = 72.
        // preCtxState = ((-5 * 22) >> 4) + 72 = -7 + 72 = 65 -> state 1, MPS 1.
        assert_eq!(
            ContextModel::new(139, 22),
            ContextModel { state: 1, mps: 1 }
        );
        // QP is clipped to 0..=51 and the state to 1..=126.
        assert_eq!(ContextModel::new(0, -10), ContextModel::new(0, 0));
        assert_eq!(ContextModel::new(0, 0), ContextModel { state: 62, mps: 0 });
    }

    #[test]
    fn state_transitions() {
        let mut c = ContextModel { state: 0, mps: 0 };
        c.update(1); // LPS at state 0 flips the MPS.
        assert_eq!(c, ContextModel { state: 0, mps: 1 });
        c.update(1);
        assert_eq!(c.state, 1);
        let mut c = ContextModel { state: 62, mps: 1 };
        c.update(1);
        assert_eq!(c.state, 62, "MPS state saturates at 62");
        c.update(0);
        assert_eq!(c.state, 38);
    }

    #[test]
    fn rejects_invalid_initial_offset() {
        assert!(ArithmeticDecoder::new(&[0xFF, 0x80]).is_err()); // 511
        assert!(ArithmeticDecoder::new(&[0x00, 0x00]).is_ok());
    }
}
