//! Encoding of HEIC files: RGB(A) images to HEVC-coded HEIF, in 8 or 10 bits.
//!
//! ```
//! let (width, height) = (64, 48);
//! let rgba = vec![200u8; width * height * 4];
//! let heic = heifer::encode_rgba8(width as u32, height as u32, &rgba, &Default::default()).unwrap();
//! let image = heifer::decode(&heic).unwrap();
//! assert_eq!((image.width, image.height), (64, 48));
//! ```
//!
//! By default, images are converted to YCbCr 4:2:0 (BT.601 matrix, full range, sRGB colour
//! space), in 10 bits (HEVC Main 10) when the image has more than 8 bits per sample, and coded
//! as 512×512 tiles of a `grid` image, encoded in parallel, like the photos of iPhones. Images
//! of at most 512×512 pixels are coded as a single picture. The alpha channel, if any sample is
//! not opaque, is coded as a monochrome auxiliary image.
//!
//! [`EncodeFormat`] selects full-resolution chroma (4:4:4) or lossless GBR coding instead; the
//! functions without a `_with_format` suffix keep writing 4:2:0.

use heifer_hevc_enc::encoder::{
    ChromaFormat, CodingOptions, EncodedPicture, EncodedSequenceFrame, EncoderConfig, Planes16,
    SequenceEncoder, SpeedPreset, encode_picture16, encode_picture16_with,
};
use heifer_isobmff::ItemId;
use heifer_isobmff::boxes::{ColorInfo, HevcConfig};
use heifer_isobmff::writer::{ALPHA_URN, HeifWriter, ImageSpec, SequenceTrack};

use crate::{Error, HdrImage, Image, Metadata};

/// Size of grid tiles.
const TILE: u32 = 512;

/// Chroma format and coding mode of an encoded image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum EncodeFormat {
    /// YCbCr 4:2:0, BT.601 matrix, lossy: the usual HEIC photos, with the smallest files.
    #[default]
    Ycbcr420,
    /// YCbCr 4:2:2, BT.601 matrix, lossy: chroma at half width and full height, as in
    /// broadcast and many cameras (Range Extensions profile).
    Ycbcr422,
    /// YCbCr 4:4:4, BT.601 matrix, lossy: full-resolution chroma for graphics and text.
    Ycbcr444,
    /// GBR 4:4:4 without transform or quantization: decoding reproduces every sample exactly.
    /// Lossless files are much larger than lossy ones.
    Lossless,
}

/// Encoding speed/compression trade-off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum EncodeSpeed {
    /// Faster encoding: smaller motion search window and fewer mode/partition trials.
    Fast,
    /// Balanced speed and compression efficiency (default).
    #[default]
    Medium,
    /// Thorough search: wider motion search window and full partition trials.
    Slow,
}

impl EncodeSpeed {
    const fn preset(self) -> SpeedPreset {
        match self {
            Self::Fast => SpeedPreset::Fast,
            Self::Medium => SpeedPreset::Medium,
            Self::Slow => SpeedPreset::Slow,
        }
    }
}

/// Encoding options.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct EncodeOptions {
    /// Quality, from 0 (smallest files) to 100 (best quality). Default: 80.
    pub quality: u8,
    /// Maximum number of threads used to encode tiles in parallel. `0` uses all available
    /// cores.
    pub max_threads: usize,
    /// Maximum number of frames from one keyframe (IDR picture) to the next in an encoded
    /// image sequence. `0` uses the default (`30`); `1` codes every frame as a keyframe.
    pub keyframe_interval: usize,
    /// Maximum number of consecutive B-frames between reference pictures (IDR or P) in an
    /// encoded image sequence. `0` disables B-frames and writes IDR + P-frames only.
    /// Default: `2`.
    pub b_frames: usize,
    /// Speed/compression preset for intra and sequence encoding. Default: [`EncodeSpeed::Medium`].
    pub speed: EncodeSpeed,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Construction methods: they keep working when new options are added, unlike a struct
/// literal, so prefer them to setting the fields directly.
///
/// ```
/// let options = heifer::EncodeOptions::new().with_quality(70).with_max_threads(2);
/// assert_eq!((options.quality, options.max_threads), (70, 2));
/// ```
impl EncodeOptions {
    /// The default options: quality 80, all cores, keyframe every 30 frames, 2 consecutive
    /// B-frames, medium speed.
    pub const fn new() -> Self {
        Self {
            quality: 80,
            max_threads: 0,
            keyframe_interval: SEQUENCE_KEYFRAME_INTERVAL,
            b_frames: 2,
            speed: EncodeSpeed::Medium,
        }
    }

    /// Sets [`quality`](Self::quality), from 0 (smallest files) to 100 (best quality).
    #[must_use]
    pub const fn with_quality(mut self, quality: u8) -> Self {
        self.quality = quality;
        self
    }

    /// Sets [`max_threads`](Self::max_threads): `0` uses all available cores.
    #[must_use]
    pub const fn with_max_threads(mut self, max_threads: usize) -> Self {
        self.max_threads = max_threads;
        self
    }

    /// Sets [`keyframe_interval`](Self::keyframe_interval): `0` uses the default (`30`); `1`
    /// codes every frame as a keyframe.
    #[must_use]
    pub const fn with_keyframe_interval(mut self, keyframe_interval: usize) -> Self {
        self.keyframe_interval = keyframe_interval;
        self
    }

    /// Sets [`b_frames`](Self::b_frames): `0` disables B-frames; larger values allow hierarchical
    /// B-frame runs between P/IDR references.
    #[must_use]
    pub const fn with_b_frames(mut self, b_frames: usize) -> Self {
        self.b_frames = b_frames;
        self
    }

    /// Sets [`speed`](Self::speed).
    #[must_use]
    pub const fn with_speed(mut self, speed: EncodeSpeed) -> Self {
        self.speed = speed;
        self
    }
}

impl EncodeOptions {
    /// HEVC quantization parameter for the quality: 100 → 8, 80 → 20, 50 → 38, 0 → 51.
    fn qp(&self) -> u8 {
        let q = f64::from(self.quality.min(100));
        (68.0 - 0.6 * q).round().clamp(8.0, 51.0) as u8
    }

    fn threads(&self) -> usize {
        let available = std::thread::available_parallelism().map_or(1, |n| n.get());
        if self.max_threads == 0 {
            available
        } else {
            self.max_threads.min(available)
        }
    }

    fn sequence_keyframe_interval(&self) -> usize {
        if self.keyframe_interval == 0 {
            SEQUENCE_KEYFRAME_INTERVAL
        } else {
            self.keyframe_interval
        }
    }
}

/// Encodes 8-bit RGBA pixels (row-major, 4 bytes per pixel) as a HEIC file. The alpha channel
/// is stored only if some pixel is not fully opaque.
pub fn encode_rgba8(
    width: u32,
    height: u32,
    rgba: &[u8],
    options: &EncodeOptions,
) -> Result<Vec<u8>, Error> {
    encode_rgba8_with_format(width, height, rgba, EncodeFormat::Ycbcr420, options)
}

/// Like [`encode_rgba8`], with the chroma format and coding mode of `format`. With
/// [`EncodeFormat::Lossless`], the decoded pixels are exactly the input pixels, alpha included.
pub fn encode_rgba8_with_format(
    width: u32,
    height: u32,
    rgba: &[u8],
    format: EncodeFormat,
    options: &EncodeOptions,
) -> Result<Vec<u8>, Error> {
    check_input(width, height, rgba.len(), 4)?;
    let has_alpha = rgba.chunks_exact(4).any(|p| p[3] != 255);
    encode_pixels(width, height, 8, has_alpha, None, format, options, |i| {
        let p = &rgba[i * 4..i * 4 + 4];
        [p[0], p[1], p[2], p[3]].map(u16::from)
    })
}

/// Encodes 8-bit RGB pixels (row-major, 3 bytes per pixel) as a HEIC file.
pub fn encode_rgb8(
    width: u32,
    height: u32,
    rgb: &[u8],
    options: &EncodeOptions,
) -> Result<Vec<u8>, Error> {
    encode_rgb8_with_format(width, height, rgb, EncodeFormat::Ycbcr420, options)
}

/// Like [`encode_rgb8`], with the chroma format and coding mode of `format`.
pub fn encode_rgb8_with_format(
    width: u32,
    height: u32,
    rgb: &[u8],
    format: EncodeFormat,
    options: &EncodeOptions,
) -> Result<Vec<u8>, Error> {
    check_input(width, height, rgb.len(), 3)?;
    encode_pixels(width, height, 8, false, None, format, options, |i| {
        let p = &rgb[i * 3..i * 3 + 3];
        [p[0], p[1], p[2], 255].map(u16::from)
    })
}

/// Encodes an [`Image`] (e.g. one decoded by [`decode`](crate::decode)) as a HEIC file: in
/// 8 bits, or in 10 bits (HEVC Main 10) for images of more bits per sample, which keeps smooth
/// gradients free of banding.
pub fn encode(image: &Image, options: &EncodeOptions) -> Result<Vec<u8>, Error> {
    encode_with_format(image, EncodeFormat::Ycbcr420, options)
}

/// Like [`encode`], with the chroma format and coding mode of `format`.
pub fn encode_with_format(
    image: &Image,
    format: EncodeFormat,
    options: &EncodeOptions,
) -> Result<Vec<u8>, Error> {
    encode_with_metadata_and_format(image, &Metadata::default(), format, options)
}

/// Like [`encode`], also storing EXIF, XMP and the ICC profile, e.g. those read by
/// [`read_metadata`](crate::read_metadata) from the original file:
///
/// ```no_run
/// let bytes = std::fs::read("photo.heic").unwrap();
/// let (image, metadata) = (heifer::decode(&bytes).unwrap(), heifer::read_metadata(&bytes).unwrap());
/// let options = heifer::EncodeOptions::new().with_quality(60);
/// let smaller = heifer::encode_with_metadata(&image, &metadata, &options).unwrap();
/// ```
///
/// The pixels of a decoded image are already upright: the EXIF orientation tag, if any, is set
/// to 1 so that no viewer rotates them again.
pub fn encode_with_metadata(
    image: &Image,
    metadata: &Metadata,
    options: &EncodeOptions,
) -> Result<Vec<u8>, Error> {
    encode_with_metadata_and_format(image, metadata, EncodeFormat::Ycbcr420, options)
}

/// Like [`encode_with_metadata`], with the chroma format and coding mode of `format`.
pub fn encode_with_metadata_and_format(
    image: &Image,
    metadata: &Metadata,
    format: EncodeFormat,
    options: &EncodeOptions,
) -> Result<Vec<u8>, Error> {
    check_input(image.width, image.height, image.data.len(), 4)?;
    let max = u32::from(image.max_value());
    let bit_depth = if image.bit_depth > 8 { 10 } else { 8 };
    let target = (1u32 << bit_depth) - 1;
    let scale = |v: u16| ((u32::from(v) * target + max / 2) / max) as u16;
    encode_pixels(
        image.width,
        image.height,
        bit_depth,
        image.has_alpha,
        Some(metadata),
        format,
        options,
        |i| {
            let p = &image.data[i * 4..i * 4 + 4];
            [scale(p[0]), scale(p[1]), scale(p[2]), scale(p[3])]
        },
    )
}

/// Encodes at the highest quality whose file fits in `max_bytes`, like
/// [`encode_with_metadata_and_format`]; returns the file and the quality used.
///
/// Quality is searched by dichotomy (about six encodes), and every candidate is measured, so the
/// result never exceeds `max_bytes`. Files usually grow with quality, but not strictly: the
/// quality found is a high one that fits, not always the highest. `options.quality` is ignored.
/// A lossless file is encoded once. Fails with [`Error::Unsupported`] when even quality 0 is
/// too large.
pub fn encode_to_size(
    image: &Image,
    metadata: &Metadata,
    format: EncodeFormat,
    max_bytes: usize,
    options: &EncodeOptions,
) -> Result<(Vec<u8>, u8), Error> {
    let encode = |quality: u8| {
        encode_with_metadata_and_format(
            image,
            metadata,
            format,
            &EncodeOptions {
                quality,
                ..*options
            },
        )
    };
    if format == EncodeFormat::Lossless {
        let heic = encode(100)?;
        return if heic.len() <= max_bytes {
            Ok((heic, 100))
        } else {
            Err(too_large(heic.len(), max_bytes))
        };
    }
    fit_to_size(max_bytes, encode)?.ok_or_else(|| {
        Error::Unsupported(format!(
            "the image does not fit in {max_bytes} bytes, even at quality 0"
        ))
    })
}

/// Units per second of the durations of an encoded sequence: milliseconds.
const SEQUENCE_TIMESCALE: u32 = 1000;

/// Frames from one keyframe (IDR picture) to the next in an encoded sequence: about a second
/// at 30 frames per second, so that seeking decodes few frames.
const SEQUENCE_KEYFRAME_INTERVAL: usize = 30;

/// Encodes an image sequence (a burst or an animation) as a HEIF file in YCbCr 4:2:0:
/// `frames`, all the same size and bit depth, each shown for the matching entry of `durations`,
/// in seconds. See [`encode_sequence_with_format`] to choose 4:2:2, 4:4:4 or lossless GBR.
///
/// Every `options.keyframe_interval` frames (default `30`) is a keyframe coded on its own; the
/// frames between are predicted from nearby references (P-frames and, when
/// [`EncodeOptions::b_frames`] > 0, bi-directionally predicted B-frames with L0/L1 motion,
/// skipped blocks, merge predictions with a residual, motion vectors to a quarter sample, and
/// asymmetric/symmetric partitions), which makes files far smaller than coding each frame
/// alone. Groups of pictures are encoded in parallel on [`EncodeOptions::max_threads`] threads.
/// The first frame is also stored as the primary image, for viewers of still images. When any
/// frame has transparency, an auxiliary monochrome alpha track (`auxl`) is stored alongside
/// the colour track.
///
/// ```
/// let red = heifer::Image::filled(64, 48, 8, [255, 0, 0, 255]);
/// let blue = heifer::Image::filled(64, 48, 8, [0, 0, 255, 255]);
/// let bytes = heifer::encode_sequence(&[red, blue], &[0.5, 0.5], &heifer::EncodeOptions::new())
///     .unwrap();
/// let sequence = heifer::read_sequence(&bytes).unwrap().unwrap();
/// assert_eq!(sequence.frames.len(), 2);
/// ```
pub fn encode_sequence(
    frames: &[Image],
    durations: &[f64],
    options: &EncodeOptions,
) -> Result<Vec<u8>, Error> {
    encode_sequence_with_format(frames, durations, EncodeFormat::Ycbcr420, options)
}

/// Like [`encode_sequence`], with the chroma format and coding mode of `format`.
pub fn encode_sequence_with_format(
    frames: &[Image],
    durations: &[f64],
    format: EncodeFormat,
    options: &EncodeOptions,
) -> Result<Vec<u8>, Error> {
    let first = frames
        .first()
        .ok_or(Error::Invalid("image sequence without frames"))?;
    if durations.len() != frames.len() {
        return Err(Error::Invalid("one duration per frame is needed"));
    }
    if durations.iter().any(|d| !(d.is_finite() && *d >= 0.0)) {
        return Err(Error::Invalid(
            "frame durations must be finite and not negative",
        ));
    }
    let (width, height, src_bit_depth) = (first.width, first.height, first.bit_depth);
    let bit_depth = if src_bit_depth > 8 { 10 } else { 8 };
    let src_max = u32::from(first.max_value());
    let target_max = (1u32 << bit_depth) - 1;
    let scale = |v: u16| ((u32::from(v) * target_max + src_max / 2) / src_max) as u16;
    let mut has_alpha = false;
    for frame in frames {
        if (frame.width, frame.height, frame.bit_depth) != (width, height, src_bit_depth) {
            return Err(Error::Invalid(
                "the frames of a sequence must have the same size and bit depth",
            ));
        }
        check_input(width, height, frame.data.len(), 4)?;
        if frame.has_alpha
            && frame
                .data
                .chunks_exact(4)
                .any(|p| u32::from(p[3]) < src_max)
        {
            has_alpha = true;
        }
    }
    let pixel_at = |frame: &Image, i: usize| {
        let p = rgba_at(frame, i);
        let a = if frame.has_alpha {
            scale(p[3])
        } else {
            target_max as u16
        };
        [scale(p[0]), scale(p[1]), scale(p[2]), a]
    };

    let mut writer = HeifWriter::new();
    // The first frame as a still image, coded like a photo (tiles, thumbnail).
    encode_into(
        &mut writer,
        width,
        height,
        bit_depth,
        has_alpha,
        None,
        format,
        options,
        |i| pixel_at(first, i),
    )?;

    // Groups of pictures, encoded in parallel: an IDR picture, then reordered P and B pictures.
    let interval = options.sequence_keyframe_interval();
    let b_frames = options.b_frames;
    let lossless = format == EncodeFormat::Lossless;
    let chroma_format = match format {
        EncodeFormat::Ycbcr420 => ChromaFormat::Chroma420,
        EncodeFormat::Ycbcr422 => ChromaFormat::Chroma422,
        EncodeFormat::Ycbcr444 | EncodeFormat::Lossless => ChromaFormat::Chroma444,
    };
    let speed = options.speed.preset();
    let config = EncoderConfig {
        qp: options.qp(),
        colour: if lossless { (1, 13, 0) } else { (1, 13, 6) },
        speed,
        ..EncoderConfig::default()
    };
    let alpha_config = EncoderConfig {
        qp: options.qp(),
        speed,
        ..EncoderConfig::default()
    };
    let coding = CodingOptions {
        chroma_format,
        lossless,
    };
    let alpha_coding = CodingOptions {
        chroma_format: ChromaFormat::Monochrome,
        lossless,
    };
    let groups: Vec<(usize, &[Image])> = frames
        .chunks(interval)
        .enumerate()
        .map(|(g, chunk)| (g * interval, chunk))
        .collect();
    type CodedPair = (EncodedSequenceFrame, Option<EncodedSequenceFrame>);
    let encode_chunk = |chunk: &[(usize, &[Image])]| -> Vec<Result<CodedPair, Error>> {
        let mut out = Vec::new();
        for &(group_start, group) in chunk {
            let mut encoder = SequenceEncoder::new(width, height, coding, config, interval)
                .with_b_frames(b_frames);
            let mut alpha_encoder = has_alpha.then(|| {
                SequenceEncoder::new(width, height, alpha_coding, alpha_config, interval)
                    .with_b_frames(b_frames)
            });
            let mut push_batch =
                |colour_batch: Vec<EncodedSequenceFrame>,
                 alpha_batch: Option<Vec<EncodedSequenceFrame>>| {
                    match alpha_batch {
                        Some(alphas) => {
                            debug_assert_eq!(colour_batch.len(), alphas.len());
                            for (mut c, mut a) in colour_batch.into_iter().zip(alphas) {
                                debug_assert_eq!(c.display_index, a.display_index);
                                c.display_index += group_start;
                                a.display_index += group_start;
                                out.push(Ok((c, Some(a))));
                            }
                        }
                        None => {
                            for mut c in colour_batch {
                                c.display_index += group_start;
                                out.push(Ok((c, None)));
                            }
                        }
                    }
                };
            for frame in group {
                let at = |x: usize, y: usize| pixel_at(frame, y * width as usize + x);
                let (c0, c1, c2) = if lossless {
                    gbr_planes(&at, width as usize, height as usize)
                } else {
                    ycbcr_planes(&at, width as usize, height as usize, bit_depth, format)
                };
                let planes = Planes16 {
                    y: &c0,
                    chroma: Some((&c1, &c2)),
                    bit_depth,
                };
                let colour_batch = match encoder.push(planes) {
                    Ok(batch) => batch,
                    Err(e) => {
                        out.push(Err(Error::from(e)));
                        return out;
                    }
                };
                let alpha_batch = match &mut alpha_encoder {
                    Some(enc) => {
                        let alpha_y: Vec<u16> = (0..(width as usize * height as usize))
                            .map(|i| pixel_at(frame, i)[3])
                            .collect();
                        let alpha_planes = Planes16 {
                            y: &alpha_y,
                            chroma: None,
                            bit_depth,
                        };
                        match enc.push(alpha_planes) {
                            Ok(batch) => Some(batch),
                            Err(e) => {
                                out.push(Err(Error::from(e)));
                                return out;
                            }
                        }
                    }
                    None => None,
                };
                push_batch(colour_batch, alpha_batch);
            }
            let colour_rest = match encoder.flush() {
                Ok(batch) => batch,
                Err(e) => {
                    out.push(Err(Error::from(e)));
                    return out;
                }
            };
            let alpha_rest = match &mut alpha_encoder {
                Some(enc) => match enc.flush() {
                    Ok(batch) => Some(batch),
                    Err(e) => {
                        out.push(Err(Error::from(e)));
                        return out;
                    }
                },
                None => None,
            };
            push_batch(colour_rest, alpha_rest);
        }
        out
    };
    let threads = options.threads().clamp(1, groups.len());
    let coded: Vec<Result<CodedPair, Error>> = if threads <= 1 {
        // No thread at all: WebAssembly without threads cannot spawn any.
        encode_chunk(&groups)
    } else {
        let per_thread = groups.len().div_ceil(threads);
        let encode_chunk = &encode_chunk;
        std::thread::scope(|scope| {
            let handles: Vec<_> = groups
                .chunks(per_thread)
                .map(|chunk| scope.spawn(move || encode_chunk(chunk)))
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().expect("encoding thread panicked"))
                .collect()
        })
    };
    let coded = coded.into_iter().collect::<Result<Vec<_>, _>>()?;
    let colour_coded: Vec<EncodedSequenceFrame> = coded.iter().map(|(c, _)| c.clone()).collect();
    let mut track = build_sequence_track(width, height, bit_depth, durations, &colour_coded)?;
    if has_alpha {
        let alpha_coded: Vec<EncodedSequenceFrame> = coded
            .into_iter()
            .map(|(_, a)| a.expect("alpha coded for every frame"))
            .collect();
        let alpha_track = build_sequence_track(width, height, bit_depth, durations, &alpha_coded)?;
        track.set_alpha(alpha_track);
    }
    writer.set_sequence(track);
    Ok(writer.finish())
}

/// Encodes an image sequence at a high quality whose file fits in `max_bytes`; returns the file
/// and the quality used.
///
/// Quality is searched by dichotomy (like [`encode_to_size`]); `options.quality` is ignored,
/// while [`EncodeOptions::keyframe_interval`], [`EncodeOptions::b_frames`],
/// [`EncodeOptions::speed`] and [`EncodeOptions::max_threads`] apply. Fails with
/// [`Error::Unsupported`] when even quality 0 is too large.
pub fn encode_sequence_to_size(
    frames: &[Image],
    durations: &[f64],
    max_bytes: usize,
    options: &EncodeOptions,
) -> Result<(Vec<u8>, u8), Error> {
    encode_sequence_to_size_with_format(
        frames,
        durations,
        EncodeFormat::Ycbcr420,
        max_bytes,
        options,
    )
}

/// Like [`encode_sequence_to_size`], with the chroma format and coding mode of `format`.
pub fn encode_sequence_to_size_with_format(
    frames: &[Image],
    durations: &[f64],
    format: EncodeFormat,
    max_bytes: usize,
    options: &EncodeOptions,
) -> Result<(Vec<u8>, u8), Error> {
    let encode = |quality: u8| {
        encode_sequence_with_format(
            frames,
            durations,
            format,
            &EncodeOptions {
                quality,
                ..*options
            },
        )
    };
    if format == EncodeFormat::Lossless {
        let heic = encode(100)?;
        return if heic.len() <= max_bytes {
            Ok((heic, 100))
        } else {
            Err(too_large(heic.len(), max_bytes))
        };
    }
    fit_to_size(max_bytes, encode)?.ok_or_else(|| {
        Error::Unsupported(format!(
            "the sequence does not fit in {max_bytes} bytes, even at quality 0"
        ))
    })
}

fn build_sequence_track(
    width: u32,
    height: u32,
    bit_depth: u8,
    durations: &[f64],
    coded: &[EncodedSequenceFrame],
) -> Result<SequenceTrack, Error> {
    let first = &coded[0].picture;
    if coded
        .iter()
        .any(|f| f.picture.parameter_sets != first.parameter_sets)
    {
        return Err(Error::Invalid("frames coded with different parameter sets"));
    }
    let hevc = hevc_config(first, bit_depth);
    let mut track = SequenceTrack::new(width, height, SEQUENCE_TIMESCALE, hevc);
    let mut display_ticks = Vec::with_capacity(durations.len());
    let mut display_starts = Vec::with_capacity(durations.len());
    let mut display_time = 0i64;
    for &duration in durations {
        let ticks = (duration * f64::from(SEQUENCE_TIMESCALE))
            .round()
            .min(f64::from(u32::MAX)) as u32;
        display_starts.push(display_time);
        display_ticks.push(ticks);
        display_time += i64::from(ticks);
    }
    let mut decode_time = 0i64;
    for frame in coded {
        let dur = display_ticks[frame.display_index];
        let offset = (display_starts[frame.display_index] - decode_time)
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
        track.add_sample_with_offset(&frame.picture.slices, dur, frame.keyframe, offset);
        decode_time += i64::from(dur);
    }
    Ok(track)
}

/// Pixel `i` of an RGBA image.
fn rgba_at(image: &Image, i: usize) -> [u16; 4] {
    let p = &image.data[4 * i..4 * i + 4];
    [p[0], p[1], p[2], p[3]]
}

fn too_large(size: usize, max_bytes: usize) -> Error {
    Error::Unsupported(format!(
        "the lossless file takes {size} bytes, more than {max_bytes}"
    ))
}

/// Finds a high quality whose output of `encode` fits in `max_bytes`, by dichotomy over the
/// qualities that give distinct quantization parameters (44 of them, so about six calls).
/// Returns the output and its quality, or `None` when even quality 0 is too large.
///
/// For encoders other than [`encode_to_size`]'s, such as the `image` crate `HeifEncoder`
/// with metadata.
pub fn fit_to_size<E>(
    max_bytes: usize,
    mut encode: impl FnMut(u8) -> Result<Vec<u8>, E>,
) -> Result<Option<(Vec<u8>, u8)>, E> {
    // The highest quality of each quantization parameter, in increasing quality.
    let mut qualities: Vec<u8> = Vec::new();
    for q in 0..=100u8 {
        let qp = EncodeOptions {
            quality: q,
            ..EncodeOptions::default()
        }
        .qp();
        let same = qualities.last().is_some_and(|&last| {
            EncodeOptions {
                quality: last,
                ..EncodeOptions::default()
            }
            .qp()
                == qp
        });
        if same {
            *qualities.last_mut().expect("not empty") = q;
        } else {
            qualities.push(q);
        }
    }
    let (mut lo, mut hi) = (0usize, qualities.len());
    let mut best = None;
    while lo < hi {
        let mid = (lo + hi) / 2;
        let out = encode(qualities[mid])?;
        if out.len() <= max_bytes {
            best = Some((out, qualities[mid]));
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(best)
}

/// Encodes an HDR image as a HEIC file with an ISO 21496-1 gain map: `base` is the SDR image
/// shown by viewers that ignore the gain map, and `hdr` is the linear-light rendition (see
/// [`HdrImage`]) to reconstruct on HDR displays. Both must have the same size, and `hdr` must
/// describe the same primaries as `base` (`use_base_colour_space`).
///
/// The gain map is a monochrome image at half resolution, stored at `quality`; the declared
/// headroom is `hdr.headroom`, so values above it are clipped. A headroom of 1 (or less) is an
/// error: there is nothing to gain.
pub fn encode_hdr(base: &Image, hdr: &HdrImage, options: &EncodeOptions) -> Result<Vec<u8>, Error> {
    encode_hdr_with_metadata(base, hdr, &Metadata::default(), options)
}

/// Like [`encode_hdr`], also storing EXIF, XMP and the ICC profile of the base image.
pub fn encode_hdr_with_metadata(
    base: &Image,
    hdr: &HdrImage,
    metadata: &Metadata,
    options: &EncodeOptions,
) -> Result<Vec<u8>, Error> {
    check_input(base.width, base.height, base.data.len(), 4)?;
    if (base.width, base.height) != (hdr.width, hdr.height) {
        return Err(Error::Invalid(
            "HDR image size does not match the base image",
        ));
    }
    if hdr.data.len() != base.width as usize * base.height as usize * 3 {
        return Err(Error::Invalid(
            "HDR pixel buffer size does not match the image size",
        ));
    }
    // Validates the headroom before any expensive work.
    let payload = crate::hdr::tmap_payload(hdr.headroom)?;

    let max = u32::from(base.max_value());
    let bit_depth = if base.bit_depth > 8 { 10 } else { 8 };
    let target = (1u32 << bit_depth) - 1;
    let scale = |v: u16| ((u32::from(v) * target + max / 2) / max) as u16;
    let mut writer = HeifWriter::new();
    let image = encode_into(
        &mut writer,
        base.width,
        base.height,
        bit_depth,
        base.has_alpha,
        Some(metadata),
        EncodeFormat::Ycbcr420,
        options,
        |i| {
            let p = &base.data[i * 4..i * 4 + 4];
            [scale(p[0]), scale(p[1]), scale(p[2]), scale(p[3])]
        },
    )?;

    let map = gain_map(base, hdr);
    let (width, height) = (base.width.div_ceil(2), base.height.div_ceil(2));
    let map = encode_monochrome(&mut writer, width, height, &map, options)?;
    let icc = metadata.icc_profile.as_deref();
    let nclx = icc.is_none().then_some(&COLOR);
    let tmap = writer.add_tmap(image, map, base.width, base.height, nclx, &payload);
    if let Some(icc) = icc {
        writer.add_icc_profile(tmap, icc);
    }
    Ok(writer.finish())
}

/// The monochrome gain map of `hdr` relative to `base`, at half resolution: one byte per sample,
/// where 255 is the headroom. The gain is the brightest channel ratio, averaged in log2 over
/// each 2×2 block.
fn gain_map(base: &Image, hdr: &HdrImage) -> Vec<u8> {
    let (w, h) = (base.width as usize, base.height as usize);
    let max = base.max_value();
    let lut: Vec<f32> = (0..=max)
        .map(|v| crate::hdr::srgb_to_linear(f32::from(v) / f32::from(max)))
        .collect();
    let log2_headroom = hdr.headroom.log2();
    let (mw, mh) = (w.div_ceil(2), h.div_ceil(2));
    let mut map = Vec::with_capacity(mw * mh);
    for my in 0..mh {
        for mx in 0..mw {
            let mut sum = 0.0;
            let mut count = 0;
            for y in 2 * my..(2 * my + 2).min(h) {
                for x in 2 * mx..(2 * mx + 2).min(w) {
                    let i = y * w + x;
                    let base_px = &base.data[i * 4..i * 4 + 3];
                    let hdr_px = &hdr.data[i * 3..i * 3 + 3];
                    let mut ratio: f32 = 1.0;
                    for (&sample, &value) in base_px.iter().zip(hdr_px) {
                        let sample = lut[usize::from(sample).min(lut.len() - 1)];
                        // With a black base sample no multiplier can reconstruct a highlight.
                        let r = if sample > 0.0 {
                            value.max(0.0) / sample
                        } else if value > 0.0 {
                            f32::INFINITY
                        } else {
                            1.0
                        };
                        ratio = ratio.max(r);
                    }
                    sum += ratio.max(1.0).log2().min(log2_headroom);
                    count += 1;
                }
            }
            let value = sum / count as f32 / log2_headroom;
            map.push((value.clamp(0.0, 1.0) * 255.0).round() as u8);
        }
    }
    map
}

/// Encodes `map` (one byte per sample) as a hidden monochrome image or grid of 512×512 tiles.
fn encode_monochrome(
    writer: &mut HeifWriter,
    width: u32,
    height: u32,
    map: &[u8],
    options: &EncodeOptions,
) -> Result<ItemId, Error> {
    let source = Source {
        width,
        height,
        bit_depth: 8,
        pixel: |i: usize| [0, 0, 0, u16::from(map[i])],
    };
    // The gain map shapes every tone: code it with the same quality as the base image.
    let config = EncoderConfig {
        qp: options.qp(),
        speed: options.speed.preset(),
        ..EncoderConfig::default()
    };
    if width <= TILE && height <= TILE {
        let picture = encode_region(
            &source,
            (0, 0),
            (width, height),
            true,
            &config,
            EncodeFormat::Ycbcr420,
        )?;
        return Ok(add_picture(
            writer,
            &picture,
            (width, height),
            true,
            8,
            None,
            None,
        ));
    }
    let (columns, rows) = (width.div_ceil(TILE), height.div_ceil(TILE));
    if columns > 256 || rows > 256 {
        return Err(Error::Invalid(
            "image too large for a grid of 512×512 tiles",
        ));
    }
    let pictures = encode_tiles(
        &source,
        columns,
        rows,
        true,
        &config,
        EncodeFormat::Ycbcr420,
        options,
    )?;
    let ids: Vec<ItemId> = pictures
        .iter()
        .map(|p| add_picture(writer, p, (TILE, TILE), true, 8, None, None))
        .collect();
    Ok(writer.add_hidden_grid(rows as u16, columns as u16, width, height, &ids, None))
}

fn check_input(width: u32, height: u32, len: usize, channels: usize) -> Result<(), Error> {
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 1 << 28 {
        return Err(Error::Invalid("image size is zero or too large"));
    }
    if len != width as usize * height as usize * channels {
        return Err(Error::Invalid(
            "pixel buffer size does not match the image size",
        ));
    }
    Ok(())
}

/// sRGB colour space with the BT.601 matrix, full range: the usual colours of HEIC photos.
const COLOR: ColorInfo = ColorInfo::Nclx {
    primaries: 1,
    transfer: 13,
    matrix: 6,
    full_range: true,
};

/// sRGB colour space with the identity matrix (GBR planes), as lossless images are coded.
const COLOR_GBR: ColorInfo = ColorInfo::Nclx {
    primaries: 1,
    transfer: 13,
    matrix: 0,
    full_range: true,
};

/// Encodes an image given by a function returning the RGBA pixel at an index, of `bit_depth`
/// (8 or 10) bits per sample, with its metadata.
#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_pixels(
    width: u32,
    height: u32,
    bit_depth: u8,
    has_alpha: bool,
    metadata: Option<&Metadata>,
    format: EncodeFormat,
    options: &EncodeOptions,
    pixel: impl Fn(usize) -> [u16; 4] + Sync,
) -> Result<Vec<u8>, Error> {
    let mut writer = HeifWriter::new();
    encode_into(
        &mut writer,
        width,
        height,
        bit_depth,
        has_alpha,
        metadata,
        format,
        options,
        pixel,
    )?;
    Ok(writer.finish())
}

/// Encodes an image into `writer`, with a thumbnail and its metadata; returns the image's ID.
#[allow(clippy::too_many_arguments)]
fn encode_into(
    writer: &mut HeifWriter,
    width: u32,
    height: u32,
    bit_depth: u8,
    has_alpha: bool,
    metadata: Option<&Metadata>,
    format: EncodeFormat,
    options: &EncodeOptions,
    pixel: impl Fn(usize) -> [u16; 4] + Sync,
) -> Result<ItemId, Error> {
    let source = Source {
        width,
        height,
        bit_depth,
        pixel,
    };
    // With an ICC profile the colour space is the profile's: it is attached to every image
    // item, and no `nclx` is written next to it (as in iPhone photos).
    let icc = metadata.and_then(|m| m.icc_profile.as_deref());
    let image = encode_image(&source, has_alpha, icc, options, format, writer)?;
    // A thumbnail, as in iPhone photos: viewers show it without decoding the whole image.
    if width.max(height) > TILE {
        let (tw, th, pixels) = thumbnail(&source);
        let thumb = Source {
            width: tw,
            height: th,
            bit_depth,
            pixel: |i: usize| pixels[i],
        };
        // A preview needs less quality: at most 60 (QP 32), a few kilobytes.
        let preview = EncodeOptions {
            quality: options.quality.min(60),
            ..*options
        };
        let id = encode_image(
            &thumb,
            has_alpha,
            icc,
            &preview,
            EncodeFormat::Ycbcr420,
            writer,
        )?;
        writer.add_reference(*b"thmb", id, &[image]);
    }
    if let Some(m) = metadata {
        if let Some(exif) = &m.exif {
            writer.add_exif(image, &crate::metadata::exif_upright(exif));
        }
        if let Some(xmp) = &m.xmp {
            writer.add_xmp(image, xmp);
        }
    }
    Ok(image)
}

/// The image to encode: its size, bits per sample, and RGBA pixels by index.
struct Source<F> {
    width: u32,
    height: u32,
    bit_depth: u8,
    pixel: F,
}

/// Longest side of thumbnails, in pixels (as in iPhone photos).
const THUMBNAIL: u32 = 320;

/// A thumbnail of the image: its size and RGBA pixels, each the average of the pixels it
/// covers.
fn thumbnail<F: Fn(usize) -> [u16; 4]>(source: &Source<F>) -> (u32, u32, Vec<[u16; 4]>) {
    let (w, h) = (source.width as usize, source.height as usize);
    let scale = f64::from(THUMBNAIL) / w.max(h) as f64;
    let tw = ((w as f64 * scale).round() as usize).max(1);
    let th = ((h as f64 * scale).round() as usize).max(1);
    let span = |o: usize, n: usize, t: usize| (o * n / t, ((o + 1) * n / t).max(o * n / t + 1));
    let mut pixels = Vec::with_capacity(tw * th);
    for oy in 0..th {
        let (y0, y1) = span(oy, h, th);
        for ox in 0..tw {
            let (x0, x1) = span(ox, w, tw);
            let mut sum = [0u64; 4];
            for y in y0..y1 {
                for x in x0..x1 {
                    for (s, v) in sum.iter_mut().zip((source.pixel)(y * w + x)) {
                        *s += u64::from(v);
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u64;
            pixels.push(sum.map(|s| ((s + n / 2) / n) as u16));
        }
    }
    (tw as u32, th as u32, pixels)
}

/// Encodes the pixels into `writer`; returns the ID of the image (picture or grid). `icc`, if
/// given, is attached to every image item and replaces the `nclx` colour information.
fn encode_image<F: Fn(usize) -> [u16; 4] + Sync>(
    source: &Source<F>,
    has_alpha: bool,
    icc: Option<&[u8]>,
    options: &EncodeOptions,
    format: EncodeFormat,
    writer: &mut HeifWriter,
) -> Result<ItemId, Error> {
    let (width, height) = (source.width, source.height);
    let qp = options.qp();
    let color = EncoderConfig {
        qp,
        colour: match format {
            EncodeFormat::Lossless => (1, 13, 0),
            _ => (1, 13, 6),
        },
        speed: options.speed.preset(),
        ..EncoderConfig::default()
    };
    // Alpha edges are visible: code the alpha plane a little better.
    let alpha = EncoderConfig {
        qp: qp.saturating_sub(4),
        colour: (2, 2, 2),
        ..color
    };
    let nclx = icc.is_none().then_some(match format {
        EncodeFormat::Lossless => &COLOR_GBR,
        _ => &COLOR,
    });

    if width <= TILE && height <= TILE {
        let picture = encode_region(source, (0, 0), (width, height), false, &color, format)?;
        let image = add_picture(
            writer,
            &picture,
            (width, height),
            false,
            source.bit_depth,
            nclx,
            icc,
        );
        if has_alpha {
            // Same coded size as the colour picture, so that both are cropped alike.
            let coded = (picture.width, picture.height);
            let alpha = encode_region(source, (0, 0), coded, true, &alpha, format)?;
            add_alpha_picture(writer, image, &alpha, (width, height), source.bit_depth);
        }
        return Ok(image);
    }

    let (columns, rows) = (width.div_ceil(TILE), height.div_ceil(TILE));
    if columns > 256 || rows > 256 {
        return Err(Error::Invalid(
            "image too large for a grid of 512×512 tiles",
        ));
    }
    let tiles = encode_tiles(source, columns, rows, false, &color, format, options)?;
    let ids: Vec<ItemId> = tiles
        .iter()
        .map(|t| add_picture(writer, t, (TILE, TILE), true, source.bit_depth, nclx, icc))
        .collect();
    let grid = writer.add_grid(rows as u16, columns as u16, width, height, &ids, nclx, None);
    if let Some(icc) = icc {
        writer.add_icc_profile(grid, icc);
    }
    if has_alpha {
        let tiles = encode_tiles(source, columns, rows, true, &alpha, format, options)?;
        let ids: Vec<ItemId> = tiles
            .iter()
            .map(|t| add_picture(writer, t, (TILE, TILE), true, source.bit_depth, None, None))
            .collect();
        let alpha = writer.add_grid(
            rows as u16,
            columns as u16,
            width,
            height,
            &ids,
            None,
            Some(ALPHA_URN),
        );
        writer.add_reference(*b"auxl", alpha, &[grid]);
    }
    writer.set_primary(grid);
    Ok(grid)
}

/// Encodes all the tiles of a grid, in parallel. Returns them row by row.
fn encode_tiles<F: Fn(usize) -> [u16; 4] + Sync>(
    source: &Source<F>,
    columns: u32,
    rows: u32,
    alpha: bool,
    config: &EncoderConfig,
    format: EncodeFormat,
    options: &EncodeOptions,
) -> Result<Vec<EncodedPicture>, Error> {
    let count = (columns * rows) as usize;
    let tile = |i: usize| {
        let origin = ((i as u32 % columns) * TILE, (i as u32 / columns) * TILE);
        encode_region(source, origin, (TILE, TILE), alpha, config, format)
    };
    let threads = options.threads().min(count);
    if threads <= 1 {
        // No thread at all: WebAssembly without threads cannot spawn any.
        return (0..count).map(tile).collect();
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<std::sync::Mutex<Option<Result<EncodedPicture, Error>>>> =
        (0..count).map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if i >= count {
                        break;
                    }
                    *results[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(tile(i));
                }
            });
        }
    });
    results
        .into_iter()
        .map(|r| {
            r.into_inner()
                .unwrap_or_else(|e| e.into_inner())
                .unwrap_or(Err(Error::Invalid("tile not encoded")))
        })
        .collect()
}

/// Encodes the `size` region at `origin` of the image: its colour, in the chroma format and
/// coding mode of `format`, or its alpha channel (monochrome, lossless when `format` is).
/// Pixels outside the image repeat the nearest edge.
fn encode_region<F: Fn(usize) -> [u16; 4]>(
    source: &Source<F>,
    origin: (u32, u32),
    size: (u32, u32),
    alpha: bool,
    config: &EncoderConfig,
    format: EncodeFormat,
) -> Result<EncodedPicture, Error> {
    let (w, h) = (size.0 as usize, size.1 as usize);
    let (width, height) = (source.width as usize, source.height as usize);
    let at = |x: usize, y: usize| -> [u16; 4] {
        let x = (origin.0 as usize + x).min(width - 1);
        let y = (origin.1 as usize + y).min(height - 1);
        (source.pixel)(y * width + x)
    };
    let bit_depth = source.bit_depth;
    let lossless = format == EncodeFormat::Lossless;
    if alpha {
        let a: Vec<u16> = (0..w * h).map(|i| at(i % w, i / w)[3]).collect();
        let planes = Planes16 {
            y: &a,
            chroma: None,
            bit_depth,
        };
        let options = CodingOptions {
            chroma_format: ChromaFormat::Monochrome,
            lossless,
        };
        return Ok(encode_picture16_with(
            size.0, size.1, planes, &options, config,
        )?);
    }
    if lossless {
        let (g, b, r) = gbr_planes(&at, w, h);
        let planes = Planes16 {
            y: &g,
            chroma: Some((&b, &r)),
            bit_depth,
        };
        let options = CodingOptions {
            chroma_format: ChromaFormat::Chroma444,
            lossless: true,
        };
        return Ok(encode_picture16_with(
            size.0, size.1, planes, &options, config,
        )?);
    }

    let (y, cb, cr) = ycbcr_planes(&at, w, h, bit_depth, format);
    let planes = Planes16 {
        y: &y,
        chroma: Some((&cb, &cr)),
        bit_depth,
    };
    if matches!(format, EncodeFormat::Ycbcr422 | EncodeFormat::Ycbcr444) {
        let options = CodingOptions {
            chroma_format: if format == EncodeFormat::Ycbcr422 {
                ChromaFormat::Chroma422
            } else {
                ChromaFormat::Chroma444
            },
            lossless: false,
        };
        Ok(encode_picture16_with(
            size.0, size.1, planes, &options, config,
        )?)
    } else {
        Ok(encode_picture16(size.0, size.1, planes, config)?)
    }
}

/// Converts RGBA pixels into GBR planes (G = Y, B = Cb, R = Cr) with identity colour matrix for
/// lossless coding.
fn gbr_planes(
    at: &impl Fn(usize, usize) -> [u16; 4],
    w: usize,
    h: usize,
) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
    let (mut g, mut b, mut r) = (
        Vec::with_capacity(w * h),
        Vec::with_capacity(w * h),
        Vec::with_capacity(w * h),
    );
    for row in 0..h {
        for col in 0..w {
            let [red, green, blue, _] = at(col, row);
            g.push(green);
            b.push(blue);
            r.push(red);
        }
    }
    (g, b, r)
}

/// Converts RGBA pixels to YCbCr planes: BT.601, full range, with the chroma subsampled for
/// `format` (4:4:4, or 4:2:2 and 4:2:0 with a [1 2 1] filter around the even positions).
/// `at(x, y)` is the pixel at a position of the `w`×`h` area.
fn ycbcr_planes(
    at: &impl Fn(usize, usize) -> [u16; 4],
    w: usize,
    h: usize,
    bit_depth: u8,
    format: EncodeFormat,
) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
    // BT.601, full range, in 16-bit fixed point.
    let mut y = Vec::with_capacity(w * h);
    let mut cb_full = Vec::with_capacity(w * h);
    let mut cr_full = Vec::with_capacity(w * h);
    for row in 0..h {
        for col in 0..w {
            let [r, g, b, _] = at(col, row).map(i32::from);
            y.push(((19595 * r + 38470 * g + 7471 * b + 32768) >> 16) as u16);
            cb_full.push(-11059 * r - 21709 * g + 32768 * b);
            cr_full.push(32768 * r - 27439 * g - 5329 * b);
        }
    }
    let (mid, max) = (1i64 << (bit_depth - 1), (1i64 << bit_depth) - 1);
    let full = |full: &[i32]| -> Vec<u16> {
        let n = 1i64 << 16;
        full.iter()
            .map(|&v| ((i64::from(v) + n * mid + n / 2).div_euclid(n)).clamp(0, max) as u16)
            .collect()
    };
    let (cb, cr) = if format == EncodeFormat::Ycbcr444 {
        (full(&cb_full), full(&cr_full))
    } else if format == EncodeFormat::Ycbcr422 {
        // 4:2:2 with the chroma samples on the even columns: a [1 2 1] filter horizontally.
        let cw = w.div_ceil(2);
        let filtered = |full: &[i32]| -> Vec<u16> {
            let mut out = Vec::with_capacity(cw * h);
            for row in 0..h {
                for cx in 0..cw {
                    let x = 2 * cx;
                    let cols = [x.saturating_sub(1), x, (x + 1).min(w - 1)];
                    let sum: i64 = cols
                        .into_iter()
                        .zip([1i64, 2, 1])
                        .map(|(c, wx)| wx * i64::from(full[row * w + c]))
                        .sum();
                    let n = 4i64 << 16;
                    out.push(((sum + n * mid + n / 2).div_euclid(n)).clamp(0, max) as u16);
                }
            }
            out
        };
        (filtered(&cb_full), filtered(&cr_full))
    } else {
        // 4:2:0 with the chroma samples on the even rows and columns, where Apple's decoder
        // (and heifer's) puts them: a [1 2 1] filter in both directions around each of them.
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let filtered = |full: &[i32]| -> Vec<u16> {
            let mut out = Vec::with_capacity(cw * ch);
            for cy in 0..ch {
                let rows = [(2 * cy).saturating_sub(1), 2 * cy, (2 * cy + 1).min(h - 1)];
                for cx in 0..cw {
                    let x = 2 * cx;
                    let cols = [x.saturating_sub(1), x, (x + 1).min(w - 1)];
                    let mut sum = 0i64;
                    for (r, wy) in rows.into_iter().zip([1i64, 2, 1]) {
                        for (c, wx) in cols.into_iter().zip([1i64, 2, 1]) {
                            sum += wy * wx * i64::from(full[r * w + c]);
                        }
                    }
                    let n = 16i64 << 16;
                    out.push(((sum + n * mid + n / 2).div_euclid(n)).clamp(0, max) as u16);
                }
            }
            out
        };
        (filtered(&cb_full), filtered(&cr_full))
    };
    (y, cb, cr)
}

fn hevc_config(picture: &EncodedPicture, bit_depth: u8) -> HevcConfig {
    HevcConfig {
        profile_idc: picture.profile_idc,
        level_idc: picture.level_idc,
        chroma_format: picture.chroma_format,
        bit_depth_luma: bit_depth,
        bit_depth_chroma: bit_depth,
        nal_length_size: 4,
        nal_units: picture.parameter_sets.clone(),
    }
}

/// Adds a coded picture of an image of `size` (the picture may be larger: odd sizes are
/// rounded up in 4:2:0, then cropped with `clap`). `color` is written as `nclx` for colour
/// pictures; `icc` is attached to the item.
fn add_picture(
    writer: &mut HeifWriter,
    picture: &EncodedPicture,
    size: (u32, u32),
    hidden: bool,
    bit_depth: u8,
    color: Option<&ColorInfo>,
    icc: Option<&[u8]>,
) -> ItemId {
    let color = color.filter(|_| picture.chroma_format != 0).cloned();
    let id = writer.add_image(&ImageSpec {
        color,
        hidden,
        crop: (size != (picture.width, picture.height)).then_some(size),
        ..ImageSpec::new(
            picture.width,
            picture.height,
            hevc_config(picture, bit_depth),
            &picture.slices,
        )
    });
    if let Some(icc) = icc {
        writer.add_icc_profile(id, icc);
    }
    id
}

fn add_alpha_picture(
    writer: &mut HeifWriter,
    image: ItemId,
    picture: &EncodedPicture,
    size: (u32, u32),
    bit_depth: u8,
) -> ItemId {
    writer.add_alpha(
        image,
        &ImageSpec {
            crop: (size != (picture.width, picture.height)).then_some(size),
            ..ImageSpec::new(
                picture.width,
                picture.height,
                hevc_config(picture, bit_depth),
                &picture.slices,
            )
        },
    )
}
