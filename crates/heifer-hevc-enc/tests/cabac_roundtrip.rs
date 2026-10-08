//! Encodes random bin sequences with the CABAC encoder and checks that the decoder from
//! `heifer-hevc-dec` returns exactly the same bins and context states.

use heifer_hevc_dec::cabac::{ArithmeticDecoder, ContextModel};
use heifer_hevc_enc::cabac::ArithmeticEncoder;

/// Small deterministic PRNG (xorshift64*), to avoid a dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[derive(Debug, Clone, Copy)]
enum Op {
    Ctx(usize, u8),
    Bypass(u8),
    Terminate0,
}

fn random_ops(rng: &mut Rng, n: usize, num_ctx: usize) -> Vec<Op> {
    // Per-context bias so that contexts actually adapt (skewed probabilities).
    let bias: Vec<u64> = (0..num_ctx).map(|_| rng.below(100)).collect();
    (0..n)
        .map(|_| match rng.below(10) {
            0..=5 => {
                let c = rng.below(num_ctx as u64) as usize;
                Op::Ctx(c, u8::from(rng.below(100) < bias[c]))
            }
            6..=8 => Op::Bypass(rng.below(2) as u8),
            _ => Op::Terminate0,
        })
        .collect()
}

fn roundtrip(seed: u64, n: usize) {
    let mut rng = Rng(seed);
    let num_ctx = 8;
    let qp = rng.below(52) as i32;
    let init: Vec<ContextModel> = (0..num_ctx)
        .map(|_| ContextModel::new(rng.below(256) as u8, qp))
        .collect();
    let ops = random_ops(&mut rng, n, num_ctx);

    let mut enc = ArithmeticEncoder::new();
    let mut enc_ctx = init.clone();
    for &op in &ops {
        match op {
            Op::Ctx(c, b) => enc.encode(&mut enc_ctx[c], b),
            Op::Bypass(b) => enc.encode_bypass(b),
            Op::Terminate0 => enc.encode_terminate(0),
        }
    }
    enc.encode_terminate(1);
    // Data after the substream must be found at the right byte position.
    enc.append_aligned(&[0xAB, 0xCD]);
    let data = enc.finish();

    let mut dec = ArithmeticDecoder::new(&data).unwrap();
    let mut dec_ctx = init.clone();
    for (i, &op) in ops.iter().enumerate() {
        let (got, want) = match op {
            Op::Ctx(c, b) => (dec.decode(&mut dec_ctx[c]), b),
            Op::Bypass(b) => (dec.decode_bypass(), b),
            Op::Terminate0 => (dec.decode_terminate(), 0),
        };
        assert_eq!(got, want, "seed {seed}: bin {i} ({op:?}) differs");
    }
    assert_eq!(dec.decode_terminate(), 1, "seed {seed}: final terminate");
    assert_eq!(dec_ctx, enc_ctx, "seed {seed}: context states differ");
    let pos = dec.aligned_position_after_terminate();
    assert_eq!(
        &data[pos..],
        &[0xAB, 0xCD],
        "seed {seed}: wrong position after terminate"
    );
}

#[test]
fn random_sequences_roundtrip() {
    for seed in 1..=2000 {
        roundtrip(seed, (seed as usize * 37) % 3000);
    }
}

#[test]
fn empty_sequence() {
    roundtrip(42, 0);
}

#[test]
fn long_runs_of_the_same_bin() {
    // Highly predictable data drives contexts to state 62 and creates long carry chains.
    for bin in [0u8, 1] {
        let mut enc = ArithmeticEncoder::new();
        let mut ctx = ContextModel::new(154, 26);
        for _ in 0..100_000 {
            enc.encode(&mut ctx, bin);
        }
        enc.encode_terminate(1);
        let data = enc.finish();
        assert!(
            data.len() < 1000,
            "100k predictable bins should compress well, got {} bytes",
            data.len()
        );
        let mut dec = ArithmeticDecoder::new(&data).unwrap();
        let mut ctx = ContextModel::new(154, 26);
        for _ in 0..100_000 {
            assert_eq!(dec.decode(&mut ctx), bin);
        }
        assert_eq!(dec.decode_terminate(), 1);
    }
}

#[test]
fn bypass_bits() {
    let mut enc = ArithmeticEncoder::new();
    enc.encode_bypass_bits(0b1011_0011_1000_1111, 16);
    enc.encode_terminate(1);
    let data = enc.finish();
    let mut dec = ArithmeticDecoder::new(&data).unwrap();
    assert_eq!(dec.decode_bypass_bits(16), 0b1011_0011_1000_1111);
}
