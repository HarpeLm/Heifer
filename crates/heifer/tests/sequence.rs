//! Image sequences: track structure and keyframe decoding on the Nokia samples.

fn fixture(name: &str) -> Option<Vec<u8>> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name);
    std::fs::read(path).ok()
}

#[test]
fn sequences_describe_and_decode_keyframes() {
    // (file, frames, keyframes, width, height, seconds)
    let cases = [
        ("candle_animation.heic", 120, vec![0, 60], 256, 144, 4.8),
        (
            "sea1_animation.heic",
            120,
            (0..8).map(|i| i * 16).collect(),
            256,
            144,
            4.8,
        ),
        ("rally_burst.heic", 60, vec![0], 640, 360, 2.4),
    ];
    for (name, frames, keyframes, width, height, seconds) in cases {
        let Some(bytes) = fixture(name) else {
            eprintln!("skipping {name}: run scripts/fetch-fixtures.sh");
            continue;
        };
        let seq = heifer::read_sequence(&bytes).unwrap().expect(name);
        assert_eq!(seq.frames.len(), frames, "{name}");
        assert_eq!((seq.width, seq.height), (width, height), "{name}");
        assert!((seq.duration() - seconds).abs() < 1e-9, "{name}");
        let keys: Vec<usize> = (0..frames).filter(|&i| seq.frames[i].keyframe).collect();
        assert_eq!(keys, keyframes, "{name}");
        for &k in &keys {
            let image = heifer::decode_sequence_frame(&bytes, k).unwrap();
            assert_eq!(
                (image.width, image.height),
                (width, height),
                "{name} frame {k}"
            );
            // Real pictures, not a flat fill.
            let first = &image.data[..4];
            assert!(
                image.data.chunks_exact(4).any(|p| p != first),
                "{name} frame {k}"
            );
        }
        // Every frame decodes in order, and decoding one frame alone (from its keyframe)
        // gives the same image.
        let all: Vec<heifer::Image> = heifer::decode_sequence(&bytes)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(all.len(), frames, "{name}");
        for (i, image) in all.iter().enumerate() {
            assert_eq!(
                (image.width, image.height),
                (width, height),
                "{name} frame {i}"
            );
        }
        for i in [1, frames / 2 + 1, frames - 1] {
            let single = heifer::decode_sequence_frame(&bytes, i).unwrap();
            assert_eq!(single, all[i], "{name} frame {i}");
        }
        // The sequence moves: some predicted frame differs from the first one.
        assert!(all[1..].iter().any(|f| f != &all[0]), "{name}");
        assert!(heifer::decode_sequence_frame(&bytes, frames).is_err());
    }
}

#[test]
fn still_images_have_no_sequence() {
    let rgba = vec![90u8; 32 * 32 * 4];
    let bytes = heifer::encode_rgba8(32, 32, &rgba, &Default::default()).unwrap();
    assert_eq!(heifer::read_sequence(&bytes).unwrap(), None);
    assert!(heifer::decode_sequence_frame(&bytes, 0).is_err());
}

#[test]
fn damaged_sequences_return_errors() {
    let Some(bytes) = fixture("candle_animation.heic") else {
        return;
    };
    // Truncated files and flipped bytes in the track boxes: errors, never panics.
    for end in (0..4000.min(bytes.len())).step_by(3) {
        let _ = heifer::read_sequence(&bytes[..end]);
        let _ = heifer::decode_sequence_frame(&bytes[..end], 0);
    }
    for at in 0..1900.min(bytes.len()) {
        let mut damaged = bytes.clone();
        damaged[at] ^= 0xA5;
        let _ = heifer::read_sequence(&damaged);
    }
}
