//! Differential check: decoding with several threads must give exactly the same image (or the
//! same failure) as sequential decoding.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let opts = |max_threads| {
        heifer::Options::new()
            .with_max_threads(max_threads)
            .with_max_pixels(1 << 17)
    };
    let sequential = heifer::decode_with_options(data, &opts(1));
    let parallel = heifer::decode_with_options(data, &opts(4));
    match (sequential, parallel) {
        (Ok(a), Ok(b)) => assert_eq!(a, b, "parallel decoding differs"),
        (Err(_), Err(_)) => {}
        (a, b) => panic!(
            "sequential {:?} vs parallel {:?}",
            a.map(|_| ()),
            b.map(|_| ())
        ),
    }
    if let Ok(Some(_)) = heifer::read_sequence(data) {
        match (
            heifer::decode_sequence_with_options(data, &opts(1)),
            heifer::decode_sequence_with_options(data, &opts(4)),
        ) {
            (Ok(s1), Ok(s4)) => {
                for pair in s1.take(8).zip(s4.take(8)) {
                    match pair {
                        (Ok(a), Ok(b)) => assert_eq!(a, b, "parallel sequence decoding differs"),
                        (Err(_), Err(_)) => {}
                        (a, b) => panic!(
                            "sequential frame {:?} vs parallel frame {:?}",
                            a.map(|_| ()),
                            b.map(|_| ())
                        ),
                    }
                }
            }
            (Err(_), Err(_)) => {}
            (a, b) => panic!(
                "sequential sequence {:?} vs parallel sequence {:?}",
                a.map(|_| ()),
                b.map(|_| ())
            ),
        }
    }
});
