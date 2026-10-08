//! CABAC arithmetic encoder: the mirror of the decoder in `heifer_hevc_dec::cabac`.
//!
//! The specification only defines decoding; this follows the classic encoder design
//! (as in the H.264 specification §9.3.4): a 10-bit `low` register, a 9-bit `range`, and
//! "outstanding" bits for carries that are not resolved yet.

use heifer_hevc_dec::cabac::{ContextModel, RANGE_TAB_LPS};

/// The arithmetic encoding engine. Produces RBSP bytes (no emulation prevention).
#[derive(Debug, Clone)]
pub struct ArithmeticEncoder {
    out: Vec<u8>,
    /// Number of bits written in the last byte of `out` (0..8, 0 = byte complete).
    bit_pos: u8,
    low: u32,
    range: u32,
    bits_outstanding: u32,
    first_bit: bool,
}

impl Default for ArithmeticEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl ArithmeticEncoder {
    /// Creates an encoder with the initial state of §9.3.2.5's decoder counterpart.
    pub fn new() -> Self {
        Self {
            out: Vec::new(),
            bit_pos: 0,
            low: 0,
            range: 510,
            bits_outstanding: 0,
            first_bit: true,
        }
    }

    fn write_bit(&mut self, bit: u32) {
        if self.bit_pos == 0 {
            self.out.push(0);
        }
        if bit != 0 {
            *self.out.last_mut().expect("a byte was pushed") |= 0x80 >> self.bit_pos;
        }
        self.bit_pos = (self.bit_pos + 1) % 8;
    }

    fn put_bit(&mut self, bit: u32) {
        if self.first_bit {
            self.first_bit = false;
        } else {
            self.write_bit(bit);
        }
        while self.bits_outstanding > 0 {
            self.write_bit(1 - bit);
            self.bits_outstanding -= 1;
        }
    }

    fn renormalize(&mut self) {
        while self.range < 256 {
            if self.low < 256 {
                self.put_bit(0);
            } else if self.low >= 512 {
                self.low -= 512;
                self.put_bit(1);
            } else {
                self.low -= 256;
                self.bits_outstanding += 1;
            }
            self.range <<= 1;
            self.low <<= 1;
        }
    }

    /// Encodes one bin with a context, and updates the context.
    pub fn encode(&mut self, ctx: &mut ContextModel, bin: u8) {
        let q = ((self.range >> 6) & 3) as usize;
        let lps = u32::from(RANGE_TAB_LPS[usize::from(ctx.state)][q]);
        self.range -= lps;
        if bin != ctx.mps {
            self.low += self.range;
            self.range = lps;
        }
        ctx.update(bin);
        self.renormalize();
    }

    /// Encodes one equiprobable bin.
    pub fn encode_bypass(&mut self, bin: u8) {
        self.low <<= 1;
        if bin != 0 {
            self.low += self.range;
        }
        if self.low >= 1024 {
            self.put_bit(1);
            self.low -= 1024;
        } else if self.low < 512 {
            self.put_bit(0);
        } else {
            self.low -= 512;
            self.bits_outstanding += 1;
        }
    }

    /// Encodes `n` bits of `value` in bypass mode, most significant first.
    pub fn encode_bypass_bits(&mut self, value: u32, n: u32) {
        for i in (0..n).rev() {
            self.encode_bypass(((value >> i) & 1) as u8);
        }
    }

    /// Encodes a terminating bin. A 1 flushes the engine; the last bit written is the
    /// `rbsp_stop_one_bit` (or the bit before PCM alignment / the next substream).
    pub fn encode_terminate(&mut self, bin: u8) {
        self.range -= 2;
        if bin != 0 {
            self.low += self.range;
            // Flush (H.264 §9.3.4.5).
            self.range = 2;
            self.renormalize();
            self.put_bit((self.low >> 9) & 1);
            self.write_bit((self.low >> 8) & 1);
            self.write_bit(1); // stop bit
        } else {
            self.renormalize();
        }
    }

    /// Pads with zero bits to the next byte boundary and returns the bytes.
    pub fn finish(mut self) -> Vec<u8> {
        self.bit_pos = 0;
        self.out
    }

    /// Pads with zero bits to the next byte boundary and appends raw bytes (e.g. PCM samples),
    /// then restarts a fresh arithmetic engine after them.
    pub fn append_aligned(&mut self, bytes: &[u8]) {
        self.bit_pos = 0;
        self.out.extend_from_slice(bytes);
        let out = std::mem::take(&mut self.out);
        *self = Self::new();
        self.out = out;
    }
}
