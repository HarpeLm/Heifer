//! Differential check: decoding with several threads must give exactly the same image (or the
//! same failure) as sequential decoding.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let opts = |max_threads| heifer::Options { max_threads, max_pixels: 1 << 17 };
    let sequential = heifer::decode_with_options(data, &opts(1));
    let parallel = heifer::decode_with_options(data, &opts(4));
    match (sequential, parallel) {
        (Ok(a), Ok(b)) => assert_eq!(a, b, "parallel decoding differs"),
        (Err(_), Err(_)) => {}
        (a, b) => panic!("sequential {:?} vs parallel {:?}", a.map(|_| ()), b.map(|_| ())),
    }
});
