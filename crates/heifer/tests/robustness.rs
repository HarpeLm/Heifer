//! Robustness against malformed files: structural inconsistencies in the container must be
//! reported as errors, never as panics (see `SECURITY.md`).

use heifer_hevc_enc::encoder::{EncodedPicture, EncoderConfig, Planes, encode_picture};
use heifer_isobmff::boxes::HevcConfig;
use heifer_isobmff::writer::{ALPHA_URN, HeifWriter, ImageSpec};

/// An 8-bit 4:2:0 picture filled with mid-grey.
fn grey(w: u32, h: u32) -> EncodedPicture {
    let y = vec![128u8; (w * h) as usize];
    let c = vec![128u8; (w / 2 * h / 2) as usize];
    encode_picture(
        w,
        h,
        Planes {
            y: &y,
            chroma: Some((&c, &c)),
        },
        &EncoderConfig::default(),
    )
    .unwrap()
}

/// An `ImageSpec` with its own size, which may disagree with the coded picture.
fn spec<'a>(
    width: u32,
    height: u32,
    picture: &'a EncodedPicture,
    auxiliary_type: Option<&'a str>,
) -> ImageSpec<'a> {
    ImageSpec {
        width,
        height,
        config: HevcConfig {
            profile_idc: picture.profile_idc,
            level_idc: picture.level_idc,
            chroma_format: picture.chroma_format,
            bit_depth_luma: 8,
            bit_depth_chroma: 8,
            nal_length_size: 4,
            nal_units: picture.parameter_sets.clone(),
        },
        slices: &picture.slices,
        color: None,
        auxiliary_type,
        hidden: auxiliary_type.is_some(),
        crop: None,
    }
}

/// A `grid` whose tile `ispe` is 0×0 used to divide by zero when placing tiles.
#[test]
fn grid_tile_with_zero_size_is_rejected() {
    let picture = grey(64, 32);
    let mut writer = HeifWriter::new();
    let tile = writer.add_image(&spec(0, 0, &picture, None));
    let grid = writer.add_grid(1, 1, 64, 32, &[tile], None, None);
    writer.set_primary(grid);

    let error = heifer::decode(&writer.finish()).expect_err("a 0×0 tile must be rejected");
    assert!(
        matches!(error, heifer::Error::Invalid(_)),
        "unexpected error: {error}"
    );
}

/// An alpha plane with the right width but a smaller height used to be read out of bounds.
#[test]
fn alpha_plane_shorter_than_the_image_is_rejected() {
    let image = grey(64, 32);
    let alpha = grey(64, 16);
    let mut writer = HeifWriter::new();
    let id = writer.add_image(&spec(64, 32, &image, None));
    writer.add_alpha(id, &spec(64, 16, &alpha, Some(ALPHA_URN)));
    writer.set_primary(id);

    let error =
        heifer::decode(&writer.finish()).expect_err("a smaller alpha plane must be rejected");
    assert!(
        matches!(error, heifer::Error::Unsupported(_)),
        "unexpected error: {error}"
    );
}

/// The same width and height as the image: the alpha plane is applied.
#[test]
fn exact_size_alpha_plane_is_accepted() {
    let image = grey(64, 32);
    let alpha = grey(64, 32);
    let mut writer = HeifWriter::new();
    let id = writer.add_image(&spec(64, 32, &image, None));
    writer.add_alpha(id, &spec(64, 32, &alpha, Some(ALPHA_URN)));
    writer.set_primary(id);

    let decoded = heifer::decode(&writer.finish()).expect("valid file");
    assert!(decoded.has_alpha);
}

/// A tile `ispe` larger than any picture the decoder would accept is rejected before placing it.
#[test]
fn absurd_tile_size_is_rejected() {
    let picture = grey(64, 32);
    let mut writer = HeifWriter::new();
    let tile = writer.add_image(&spec(1, 300_000_000, &picture, None));
    let grid = writer.add_grid(1, 1, 1, 1, &[tile], None, None);
    writer.set_primary(grid);

    let error = heifer::decode(&writer.finish()).expect_err("an absurd tile size must be rejected");
    assert!(
        matches!(error, heifer::Error::Invalid(_)),
        "unexpected error: {error}"
    );
}

#[test]
fn oversized_lossless_levels_do_not_overflow() {
    // Found by fuzzing: a transquant-bypass block codes coefficient levels near 2^31, which
    // reached the residual unchanged and overflowed when added to the prediction. Levels are
    // now limited to the range the spec allows (CoeffMinY..=CoeffMaxY).
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/bypass_level_overflow.heic");
    let data = std::fs::read(path).unwrap();
    let options = heifer::Options::new()
        .with_max_threads(1)
        .with_max_pixels(1 << 17);
    let _ = heifer::decode_with_options(&data, &options);
    let _ = heifer::decode_reader_with_options(std::io::Cursor::new(&data), &options);
}

#[test]
fn slice_and_reader_agree_on_a_short_junk_tail() {
    // A few bytes after the last box (padding, a truncated copy) are ignored by both paths:
    // the slice path used to reject the whole file while the reader decoded it.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/bframes.mp4");
    let mut data = std::fs::read(path).unwrap();
    assert!(heifer::read_sequence(&data).unwrap().is_some());
    data.extend_from_slice(&[0xAB; 7]);
    let seq = heifer::read_sequence(&data)
        .unwrap()
        .expect("sequence still found");
    assert_eq!(seq.frames.len(), 24);
    assert!(heifer::decode_sequence_frame(&data, 3).is_ok());
    if let Ok(still) = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/single_image.heic"),
    ) {
        let mut padded = still.clone();
        padded.extend_from_slice(&[0xAB; 7]);
        let from_slice = heifer::decode(&padded).unwrap();
        let from_reader = heifer::decode_reader(std::io::Cursor::new(&padded)).unwrap();
        assert_eq!(from_slice, from_reader);
        assert_eq!(from_slice, heifer::decode(&still).unwrap());
    }
}

#[test]
fn option_constructors_match_the_defaults() {
    let (o, d) = (heifer::Options::new(), heifer::Options::default());
    assert_eq!((o.max_threads, o.max_pixels), (d.max_threads, d.max_pixels));
    let (o, d) = (
        heifer::EncodeOptions::new(),
        heifer::EncodeOptions::default(),
    );
    assert_eq!(
        (o.quality, o.max_threads, o.keyframe_interval, o.b_frames),
        (d.quality, d.max_threads, d.keyframe_interval, d.b_frames)
    );
    let o = heifer::EncodeOptions::new()
        .with_keyframe_interval(12)
        .with_b_frames(0);
    assert_eq!((o.keyframe_interval, o.b_frames), (12, 0));
    let (o, d) = (
        heifer::heifer_hevc_dec::decoder::DecodeOptions::new(),
        heifer::heifer_hevc_dec::decoder::DecodeOptions::default(),
    );
    assert_eq!(
        (o.skip_loop_filters, o.verify_hash, o.max_pixels),
        (d.skip_loop_filters, d.verify_hash, d.max_pixels)
    );
    let o = heifer::Options::new()
        .with_max_threads(3)
        .with_max_pixels(7);
    assert_eq!((o.max_threads, o.max_pixels), (3, 7));
}
