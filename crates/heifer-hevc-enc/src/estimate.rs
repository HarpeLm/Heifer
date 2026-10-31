//! Rate estimation for the encoder's trials.
//!
//! While the encoder compares ways of coding a block, the bins go to a [`BinLog`]: it adds up
//! their estimated cost from the probability of their context, updates the contexts exactly as
//! the arithmetic coder would, and records them. Once a 32×32 block is decided, the log of the
//! chosen coding is replayed into the real arithmetic coder. Trials thus avoid the arithmetic
//! coding itself and the copies of its output.

use heifer_hevc_dec::cabac::ContextModel;
use heifer_hevc_dec::contexts::Contexts;

use crate::cabac::ArithmeticEncoder;

/// Fixed-point unit of the estimated costs: 1/65536 bit.
const ONE_BIT: u64 = 1 << 16;
/// Marks a bypass bin in the log (otherwise: context index << 1 | bin).
const BYPASS: u16 = 0x8000;

/// Estimated cost in bits of a bin per context state, as [MPS, LPS].
pub(crate) fn entropy_bits() -> [[f32; 2]; 64] {
    // pLPS(state) = 0.5 · α^state, with α = (0.01875 / 0.5)^(1/63) (§9.3.3.2.1 design).
    let alpha = (0.01875f64 / 0.5).powf(1.0 / 63.0);
    core::array::from_fn(|s| {
        let lps = 0.5 * alpha.powi(s as i32);
        [-(1.0 - lps).log2() as f32, -lps.log2() as f32]
    })
}

/// A position in a [`BinLog`], to come back to.
#[derive(Debug, Clone)]
pub(crate) struct LogPoint {
    len: usize,
    bits: u64,
    /// For points taken after a trial: the bins logged since the trial started.
    tail: Option<(usize, Vec<u16>)>,
}

/// Records bins and estimates their cost (see the module documentation).
#[derive(Debug)]
pub(crate) struct BinLog {
    ops: Vec<u16>,
    bits: u64,
    cost: [[u32; 2]; 64],
}

impl BinLog {
    pub(crate) fn new() -> Self {
        let table = entropy_bits();
        Self {
            ops: Vec::with_capacity(1 << 16),
            bits: 0,
            cost: table.map(|c| c.map(|b| (f64::from(b) * ONE_BIT as f64).round() as u32)),
        }
    }

    /// A bin coded with context `index` (whose model is `ctx`).
    #[inline]
    pub(crate) fn encode(&mut self, ctx: &mut ContextModel, index: usize, bin: u8) {
        let lps = usize::from(bin != ctx.mps);
        self.bits += u64::from(self.cost[usize::from(ctx.state.min(63))][lps]);
        ctx.update(bin);
        self.ops.push(((index as u16) << 1) | u16::from(bin));
    }

    /// An equiprobable bin.
    #[inline]
    pub(crate) fn encode_bypass(&mut self, bin: u8) {
        self.bits += ONE_BIT;
        self.ops.push(BYPASS | u16::from(bin));
    }

    /// `n` bypass bins holding `value`, most significant first.
    pub(crate) fn encode_bypass_bits(&mut self, value: u32, n: u32) {
        for i in (0..n).rev() {
            self.encode_bypass(((value >> i) & 1) as u8);
        }
    }

    /// Estimated bits logged so far.
    pub(crate) fn bit_count(&self) -> f64 {
        self.bits as f64 / ONE_BIT as f64
    }

    pub(crate) fn checkpoint(&self) -> LogPoint {
        LogPoint {
            len: self.ops.len(),
            bits: self.bits,
            tail: None,
        }
    }

    /// Like [`checkpoint`](Self::checkpoint), keeping the bins logged since `since`, so that
    /// this state can be restored after going back to `since`.
    pub(crate) fn checkpoint_since(&self, since: &LogPoint) -> LogPoint {
        LogPoint {
            tail: Some((since.len, self.ops[since.len..].to_vec())),
            ..self.checkpoint()
        }
    }

    pub(crate) fn restore(&mut self, point: &LogPoint) {
        match &point.tail {
            None => self.ops.truncate(point.len),
            Some((start, tail)) => {
                self.ops.truncate(*start);
                self.ops.extend_from_slice(tail);
            }
        }
        self.bits = point.bits;
    }

    /// Takes the logged bins, emptying the log, to replay them later with [`replay`].
    pub(crate) fn take(&mut self) -> Vec<u16> {
        self.bits = 0;
        // A copy, so that the log keeps its capacity.
        let ops = self.ops.clone();
        self.ops.clear();
        ops
    }
}

/// Codes logged bins for real, with `contexts` as they were when they were logged. The
/// contexts end as the logging left them.
pub(crate) fn replay(ops: &[u16], contexts: &mut Contexts, coder: &mut ArithmeticEncoder) {
    for &op in ops {
        let bin = (op & 1) as u8;
        if op & BYPASS != 0 {
            coder.encode_bypass(bin);
        } else {
            coder.encode(&mut contexts[usize::from(op >> 1)], bin);
        }
    }
}
