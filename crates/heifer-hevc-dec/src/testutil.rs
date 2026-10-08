//! Test helpers.

/// Writes bits MSB-first, with Exp-Golomb helpers, to build test parameter sets.
#[derive(Default)]
pub(crate) struct BitWriter {
    bytes: Vec<u8>,
    n: usize,
}

impl BitWriter {
    pub(crate) fn bit(&mut self, b: bool) -> &mut Self {
        if self.n % 8 == 0 {
            self.bytes.push(0);
        }
        if b {
            *self.bytes.last_mut().unwrap() |= 0x80 >> (self.n % 8);
        }
        self.n += 1;
        self
    }
    pub(crate) fn bits(&mut self, v: u32, n: u32) -> &mut Self {
        for i in (0..n).rev() {
            self.bit(i < 32 && (v >> i) & 1 == 1);
        }
        self
    }
    pub(crate) fn ue(&mut self, v: u32) -> &mut Self {
        let x = v + 1;
        let len = 32 - x.leading_zeros();
        self.bits(0, len - 1).bits(x, len)
    }
    pub(crate) fn se(&mut self, v: i32) -> &mut Self {
        self.ue(if v > 0 {
            2 * v as u32 - 1
        } else {
            (-2 * v) as u32
        })
    }
    pub(crate) fn finish(&mut self) -> Vec<u8> {
        self.bit(true);
        while self.n % 8 != 0 {
            self.bit(false);
        }
        std::mem::take(&mut self.bytes)
    }
}
