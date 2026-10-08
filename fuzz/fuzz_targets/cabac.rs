//! CABAC round trip: any sequence of bins encoded by heifer's encoder must be decoded back
//! exactly by heifer's decoder, with identical context states.
#![no_main]
use heifer_hevc_dec::cabac::{ArithmeticDecoder, ContextModel};
use heifer_hevc_enc::cabac::ArithmeticEncoder;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&qp, ops)) = data.split_first() else { return };
    let init: Vec<ContextModel> = (0..8u8).map(|i| ContextModel::new(i.wrapping_mul(37), i32::from(qp % 52))).collect();
    let mut enc = ArithmeticEncoder::new();
    let mut enc_ctx = init.clone();
    for &op in ops {
        let bin = op & 1;
        match (op >> 1) % 4 {
            0 | 1 => enc.encode(&mut enc_ctx[usize::from(op >> 4) % 8], bin),
            2 => enc.encode_bypass(bin),
            _ => enc.encode_terminate(0),
        }
    }
    enc.encode_terminate(1);
    let bytes = enc.finish();

    let mut dec = ArithmeticDecoder::new(&bytes).expect("encoder output must start validly");
    let mut dec_ctx = init;
    for &op in ops {
        let bin = op & 1;
        let got = match (op >> 1) % 4 {
            0 | 1 => dec.decode(&mut dec_ctx[usize::from(op >> 4) % 8]),
            2 => dec.decode_bypass(),
            _ => dec.decode_terminate(),
        };
        let want = if (op >> 1) % 4 == 3 { 0 } else { bin };
        assert_eq!(got, want);
    }
    assert_eq!(dec.decode_terminate(), 1);
    assert_eq!(dec_ctx, enc_ctx);
    assert_eq!(dec.aligned_position_after_terminate(), bytes.len());
});
