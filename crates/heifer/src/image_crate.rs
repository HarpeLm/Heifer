//! Integration with the [`image`](https://docs.rs/image) crate (feature `image`).
//!
//! ```no_run
//! heifer::image_crate::register_image_decoder_hooks();
//! let img = image::open("photo.heic").unwrap(); // .heic / .heif / .hif + content detection
//! img.save("photo.png").unwrap();
//! ```
//!
//! [`HeifDecoder`] can also be used directly with `image::DynamicImage::from_decoder`.
//!
//! [`HeifSequenceDecoder`] reads image sequences (bursts, animations, `.heics`) as an
//! [`image::AnimationDecoder`], frame by frame in display order:
//!
//! ```no_run
//! use image::AnimationDecoder;
//! let file = std::fs::File::open("burst.heics").unwrap();
//! let decoder = heifer::image_crate::HeifSequenceDecoder::new(file).unwrap();
//! for frame in decoder.into_frames() {
//!     let frame = frame.unwrap();
//!     println!("{:?} {}×{}", frame.delay(), frame.buffer().width(), frame.buffer().height());
//! }
//! ```
//!
//! With the `encoder` feature (default), `HeifEncoder` writes HEIC files from any image of the
//! `image` crate:
//!
//! ```ignore
//! let img = image::open("photo.png").unwrap();
//! let file = std::fs::File::create("photo.heic").unwrap();
//! img.write_with_encoder(heifer::image_crate::HeifEncoder::new(file)).unwrap();
//! ```
//!
//! (`image::save` and `DynamicImage::save` only know the formats built into `image`.)

use std::io::Read;

use image::error::{DecodingError, ImageFormatHint, ParameterError, ParameterErrorKind};
use image::hooks::GenericReader;
use image::metadata::Orientation;
use image::{ColorType, ImageDecoder, ImageError, ImageResult, Limits};

use crate::reader::{
    ReadError, decode_reader_rgba8, decode_reader_with_options, read_metadata_reader,
    read_still_info_reader,
};
use crate::{Image, ImageInfo, Metadata, Options, check_size, read_still_info};

#[cfg(feature = "encoder")]
pub use encoder::HeifEncoder;

/// File extensions handled by heifer.
const EXTENSIONS: [&str; 3] = ["heic", "heif", "hif"];

/// HEVC-based HEIF brands recognised from the file content (the `ftyp` major brand).
const BRANDS: [&[u8; 4]; 4] = [b"heic", b"heix", b"heim", b"heis"];

/// Registers heifer with the `image` crate, for the `.heic`, `.heif` and `.hif` extensions and
/// for content detection of HEVC-coded HEIF files. Returns `false` if another decoder was
/// already registered for one of these extensions.
///
/// After this call, `image::open`, `image::ImageReader` and `image::load_from_memory` decode
/// HEIC files with heifer, reading dimensions and metadata from headers without decoding pixels
/// upfront and checking `image::Limits` before allocating decode buffers.
pub fn register_image_decoder_hooks() -> bool {
    register_image_decoder_hooks_with_options(Options::default())
}

/// Like [`register_image_decoder_hooks`], decoding with `options`: for example
/// `max_threads: 1` in applications that already decode several images in parallel.
///
/// ```no_run
/// heifer::image_crate::register_image_decoder_hooks_with_options(heifer::Options {
///     max_threads: 1,
///     ..Default::default()
/// });
/// ```
pub fn register_image_decoder_hooks_with_options(options: Options) -> bool {
    let mut all = true;
    for ext in EXTENSIONS {
        all &= image::hooks::register_decoding_hook(
            ext.into(),
            Box::new(move |reader| Ok(Box::new(HookDecoder::new(reader, &options)?))),
        );
    }
    // Box size (4 bytes, any value) then `ftyp` and the major brand.
    const MASK: &[u8] = b"\x00\x00\x00\x00\xff\xff\xff\xff\xff\xff\xff\xff";
    static SIGNATURES: [[u8; 12]; 4] = {
        let mut s = [[0u8; 12]; 4];
        let mut i = 0;
        while i < 4 {
            let mut j = 0;
            while j < 4 {
                s[i][4 + j] = b"ftyp"[j];
                s[i][8 + j] = BRANDS[i][j];
                j += 1;
            }
            i += 1;
        }
        s
    };
    for signature in &SIGNATURES {
        image::hooks::register_format_detection_hook("heic".into(), signature, Some(MASK));
    }
    all
}

/// An [`image::ImageDecoder`] for HEIF/HEIC files.
///
/// Only the container headers are parsed when the decoder is created: dimensions, colour type
/// and limits (`set_limits`) are checked before pixels are decoded in `read_image`, and 8-bit
/// images decode via the packed 8-bit path ([`decode_rgba8`](crate::decode_rgba8)).
/// Crop, rotation and mirror transforms are applied by heifer, so [`ImageDecoder::orientation`]
/// reports [`Orientation::NoTransforms`].
pub struct HeifDecoder {
    bytes: Vec<u8>,
    info: ImageInfo,
    metadata: Option<Metadata>,
    options: Options,
    limits: Option<Limits>,
}

impl HeifDecoder {
    /// Reads a HEIF file and inspects its container headers (without decoding pixels yet).
    pub fn new(reader: impl Read) -> ImageResult<Self> {
        Self::with_options(reader, &Options::default())
    }

    /// Like [`new`](Self::new), with heifer options.
    pub fn with_options(mut reader: impl Read, options: &Options) -> ImageResult<Self> {
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .map_err(ImageError::IoError)?;
        Self::from_vec(bytes, options)
    }

    /// Creates a lazy decoder from a HEIF file held in memory, with heifer options.
    pub fn from_bytes(bytes: &[u8], options: &Options) -> ImageResult<Self> {
        let info = read_still_info(bytes).map_err(decoding_error)?;
        check_size(info.width, info.height, options.max_pixels()).map_err(decoding_error)?;
        Ok(Self {
            bytes: bytes.to_vec(),
            info,
            metadata: None,
            options: *options,
            limits: None,
        })
    }

    /// Like [`from_bytes`](Self::from_bytes), taking ownership of the buffer without copying it.
    pub fn from_vec(bytes: Vec<u8>, options: &Options) -> ImageResult<Self> {
        let info = read_still_info(&bytes).map_err(decoding_error)?;
        check_size(info.width, info.height, options.max_pixels()).map_err(decoding_error)?;
        Ok(Self {
            bytes,
            info,
            metadata: None,
            options: *options,
            limits: None,
        })
    }

    fn metadata(&mut self) -> ImageResult<&Metadata> {
        if self.metadata.is_none() {
            self.metadata = Some(crate::read_metadata(&self.bytes).map_err(decoding_error)?);
        }
        Ok(self.metadata.as_ref().expect("metadata populated above"))
    }
}

/// Seekable decoder backing `image::hooks` so `image::image_dimensions` and limit checks read
/// only container headers instead of buffering the whole file upfront.
struct HookDecoder<'a> {
    reader: GenericReader<'a>,
    info: ImageInfo,
    metadata: Option<Metadata>,
    options: Options,
    limits: Option<Limits>,
}

impl<'a> HookDecoder<'a> {
    fn new(mut reader: GenericReader<'a>, options: &Options) -> ImageResult<Self> {
        let info = read_still_info_reader(&mut reader).map_err(read_error)?;
        check_size(info.width, info.height, options.max_pixels()).map_err(decoding_error)?;
        Ok(Self {
            reader,
            info,
            metadata: None,
            options: *options,
            limits: None,
        })
    }

    fn metadata(&mut self) -> ImageResult<&Metadata> {
        if self.metadata.is_none() {
            let meta = read_metadata_reader(&mut self.reader).map_err(read_error)?;
            self.metadata = Some(meta);
        }
        Ok(self.metadata.as_ref().expect("metadata populated above"))
    }
}

/// An [`image::AnimationDecoder`] for the image sequence of a HEIF file (see the module
/// documentation). Frames are RGBA 8-bit, in display order, each with its duration.
pub struct HeifSequenceDecoder {
    bytes: Vec<u8>,
    options: Options,
    /// Duration of each frame, in display order.
    delays: Vec<image::Delay>,
}

impl HeifSequenceDecoder {
    /// Reads a whole HEIF file; fails if it holds no image sequence.
    pub fn new(reader: impl Read) -> ImageResult<Self> {
        Self::with_options(reader, &Options::default())
    }

    /// Like [`new`](Self::new), with heifer options.
    pub fn with_options(mut reader: impl Read, options: &Options) -> ImageResult<Self> {
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .map_err(ImageError::IoError)?;
        Self::from_bytes(bytes, options)
    }

    /// A decoder for a HEIF file held in memory.
    pub fn from_bytes(bytes: Vec<u8>, options: &Options) -> ImageResult<Self> {
        let info = crate::read_sequence(&bytes)
            .map_err(decoding_error)?
            .ok_or_else(|| decoding_error(crate::Error::Invalid("file has no image sequence")))?;
        let delays = info
            .frames
            .iter()
            .map(|f| {
                // Microseconds keep the timing of any timescale within a microsecond.
                let micros = (f.duration * 1e6).round().clamp(0.0, f64::from(u32::MAX)) as u32;
                image::Delay::from_numer_denom_ms(micros, 1000)
            })
            .collect();
        Ok(Self {
            bytes,
            options: *options,
            delays,
        })
    }

    /// Number of frames.
    pub fn len(&self) -> usize {
        self.delays.len()
    }

    /// Whether the sequence has no frame.
    pub fn is_empty(&self) -> bool {
        self.delays.is_empty()
    }
}

impl<'a> image::AnimationDecoder<'a> for HeifSequenceDecoder {
    fn into_frames(self) -> image::Frames<'a> {
        let delays = self.delays;
        let frames = match crate::sequence::decode_sequence_owned(self.bytes, &self.options) {
            Ok(frames) => frames,
            Err(e) => return image::Frames::new(Box::new(std::iter::once(Err(decoding_error(e))))),
        };
        let iter = frames.zip(delays).map(|(frame, delay)| {
            let frame = frame.map_err(decoding_error)?;
            let buffer = image::RgbaImage::from_raw(frame.width, frame.height, frame.to_rgba8())
                .ok_or_else(|| {
                    decoding_error(crate::Error::Invalid("frame of an unexpected size"))
                })?;
            Ok(image::Frame::from_parts(buffer, 0, 0, delay))
        });
        image::Frames::new(Box::new(iter))
    }
}

fn decoding_error(e: crate::Error) -> ImageError {
    ImageError::Decoding(DecodingError::new(ImageFormatHint::Name("HEIF".into()), e))
}

fn read_error(e: ReadError) -> ImageError {
    match e {
        ReadError::Io(io) => ImageError::IoError(io),
        ReadError::Heif(heif) => decoding_error(heif),
    }
}

fn info_color_type(info: &ImageInfo) -> ColorType {
    match (info.bit_depth > 8, info.has_alpha) {
        (false, false) => ColorType::Rgb8,
        (false, true) => ColorType::Rgba8,
        (true, false) => ColorType::Rgb16,
        (true, true) => ColorType::Rgba16,
    }
}

fn check_decoder_limits(info: &ImageInfo, limits: &Limits) -> ImageResult<()> {
    limits.check_support(&image::LimitSupport::default())?;
    limits.check_dimensions(info.width, info.height)?;
    Ok(())
}

fn prepare_decode(
    info: &ImageInfo,
    base_options: Options,
    limits: Option<Limits>,
    buf_len: usize,
    total_bytes: u64,
) -> ImageResult<Options> {
    if buf_len as u64 != total_bytes {
        return Err(ImageError::Parameter(ParameterError::from_kind(
            ParameterErrorKind::DimensionMismatch,
        )));
    }
    let mut options = base_options;
    if let Some(mut limits) = limits {
        limits.check_dimensions(info.width, info.height)?;
        let internal_bpp = if info.bit_depth > 8 { 8u64 } else { 4u64 };
        let internal_bytes = u64::from(info.width)
            .saturating_mul(u64::from(info.height))
            .saturating_mul(internal_bpp);
        let max_alloc_before = limits.max_alloc;
        limits.reserve(internal_bytes)?;
        if let Some(max_alloc) = max_alloc_before {
            let cap = (max_alloc / internal_bpp).max(1);
            options.max_pixels = options.max_pixels().min(cap);
        }
    }
    Ok(options)
}

fn write_image_u8(info: &ImageInfo, image: &Image<u8>, buf: &mut [u8]) -> ImageResult<()> {
    if (image.width, image.height) != (info.width, info.height) {
        return Err(decoding_error(crate::Error::Invalid(
            "decoded image dimensions do not match container header",
        )));
    }
    if info.has_alpha {
        buf.copy_from_slice(&image.data);
    } else {
        for (dst, src) in buf.chunks_exact_mut(3).zip(image.data.chunks_exact(4)) {
            dst.copy_from_slice(&src[..3]);
        }
    }
    Ok(())
}

fn write_image_u16(info: &ImageInfo, image: &Image<u16>, buf: &mut [u8]) -> ImageResult<()> {
    if (image.width, image.height) != (info.width, info.height) {
        return Err(decoding_error(crate::Error::Invalid(
            "decoded image dimensions do not match container header",
        )));
    }
    let channels = if info.has_alpha { 4 } else { 3 };
    let max = u32::from(image.max_value());
    let pixels = image.data.chunks_exact(4).map(|p| &p[..channels]);
    // 16-bit native endian, scaled to the full 0..=65535 range.
    for (out, v) in buf.chunks_exact_mut(2).zip(pixels.flatten()) {
        let v16 = ((u32::from(*v) * 65535 + max / 2) / max) as u16;
        out.copy_from_slice(&v16.to_ne_bytes());
    }
    Ok(())
}

impl ImageDecoder for HeifDecoder {
    fn dimensions(&self) -> (u32, u32) {
        (self.info.width, self.info.height)
    }

    fn color_type(&self) -> ColorType {
        info_color_type(&self.info)
    }

    fn icc_profile(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(self.metadata()?.icc_profile.clone())
    }

    fn exif_metadata(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(self.metadata()?.exif.clone())
    }

    fn xmp_metadata(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(self.metadata()?.xmp.clone())
    }

    /// HEIF transforms (`irot`/`imir`) are already applied, and the EXIF orientation is only
    /// informative in HEIF: nothing is left to do.
    fn orientation(&mut self) -> ImageResult<Orientation> {
        Ok(Orientation::NoTransforms)
    }

    fn set_limits(&mut self, limits: Limits) -> ImageResult<()> {
        check_decoder_limits(&self.info, &limits)?;
        self.limits = Some(limits);
        Ok(())
    }

    fn read_image(self, buf: &mut [u8]) -> ImageResult<()> {
        let total = self.total_bytes();
        let options = prepare_decode(&self.info, self.options, self.limits, buf.len(), total)?;
        if self.info.bit_depth <= 8 {
            let image = crate::decode_rgba8(&self.bytes, &options).map_err(decoding_error)?;
            write_image_u8(&self.info, &image, buf)
        } else {
            let image =
                crate::decode_with_options(&self.bytes, &options).map_err(decoding_error)?;
            write_image_u16(&self.info, &image, buf)
        }
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}

impl ImageDecoder for HookDecoder<'_> {
    fn dimensions(&self) -> (u32, u32) {
        (self.info.width, self.info.height)
    }

    fn color_type(&self) -> ColorType {
        info_color_type(&self.info)
    }

    fn icc_profile(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(self.metadata()?.icc_profile.clone())
    }

    fn exif_metadata(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(self.metadata()?.exif.clone())
    }

    fn xmp_metadata(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(self.metadata()?.xmp.clone())
    }

    fn orientation(&mut self) -> ImageResult<Orientation> {
        Ok(Orientation::NoTransforms)
    }

    fn set_limits(&mut self, limits: Limits) -> ImageResult<()> {
        check_decoder_limits(&self.info, &limits)?;
        self.limits = Some(limits);
        Ok(())
    }

    fn read_image(mut self, buf: &mut [u8]) -> ImageResult<()> {
        let total = self.total_bytes();
        let options = prepare_decode(&self.info, self.options, self.limits, buf.len(), total)?;
        if self.info.bit_depth <= 8 {
            let image = decode_reader_rgba8(&mut self.reader, &options).map_err(read_error)?;
            write_image_u8(&self.info, &image, buf)
        } else {
            let image =
                decode_reader_with_options(&mut self.reader, &options).map_err(read_error)?;
            write_image_u16(&self.info, &image, buf)
        }
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}

#[cfg(feature = "encoder")]
mod encoder {
    use std::io::Write;

    use image::error::{EncodingError, ImageFormatHint, UnsupportedError, UnsupportedErrorKind};
    use image::{ExtendedColorType, ImageEncoder, ImageError, ImageResult};

    use crate::{EncodeFormat, EncodeOptions, Metadata};

    /// Encodes images as HEIC files, for `DynamicImage::write_with_encoder` and the other users of
    /// [`ImageEncoder`].
    ///
    /// Accepts 8- and 16-bit grey, grey + alpha, RGB and RGBA images (16-bit images are coded in
    /// 10 bits). The ICC profile and EXIF metadata given by `image` are stored in the file; the EXIF
    /// orientation is set to 1, since the pixels are stored as given.
    #[derive(Debug)]
    pub struct HeifEncoder<W: Write> {
        writer: W,
        options: EncodeOptions,
        format: EncodeFormat,
        metadata: Metadata,
    }

    impl<W: Write> HeifEncoder<W> {
        /// An encoder with the default options (quality 80, all cores, 4:2:0).
        pub fn new(writer: W) -> Self {
            Self::with_options(writer, EncodeOptions::default())
        }

        /// An encoder with the given quality and threads, coding 4:2:0.
        pub fn with_options(writer: W, options: EncodeOptions) -> Self {
            Self::with_format(writer, options, EncodeFormat::Ycbcr420)
        }

        /// An encoder with the given chroma format and coding mode.
        pub fn with_format(writer: W, options: EncodeOptions, format: EncodeFormat) -> Self {
            Self {
                writer,
                options,
                format,
                metadata: Metadata::default(),
            }
        }
    }

    fn hint() -> ImageFormatHint {
        ImageFormatHint::Name("HEIF".into())
    }

    impl<W: Write> ImageEncoder for HeifEncoder<W> {
        fn write_image(
            mut self,
            buf: &[u8],
            width: u32,
            height: u32,
            color_type: ExtendedColorType,
        ) -> ImageResult<()> {
            use ExtendedColorType as C;
            let (channels, wide) = match color_type {
                C::L8 => (1, false),
                C::La8 => (2, false),
                C::Rgb8 => (3, false),
                C::Rgba8 => (4, false),
                C::L16 => (1, true),
                C::La16 => (2, true),
                C::Rgb16 => (3, true),
                C::Rgba16 => (4, true),
                _ => {
                    return Err(ImageError::Unsupported(
                        UnsupportedError::from_format_and_kind(
                            hint(),
                            UnsupportedErrorKind::Color(color_type),
                        ),
                    ));
                }
            };
            let bytes = if wide { 2 } else { 1 };
            let expected = u64::from(width) * u64::from(height) * channels * bytes;
            if buf.len() as u64 != expected {
                return Err(ImageError::Parameter(
                    image::error::ParameterError::from_kind(
                        image::error::ParameterErrorKind::DimensionMismatch,
                    ),
                ));
            }
            let channels = channels as usize;
            // 16-bit images are coded in 10 bits (no banding), 8-bit ones in 8 bits.
            let (bit_depth, top) = if wide { (10, 1023u16) } else { (8, 255) };
            let sample = |i: usize| -> u16 {
                if wide {
                    let v = u16::from_ne_bytes([buf[2 * i], buf[2 * i + 1]]);
                    ((u32::from(v) * 1023 + 32767) / 65535) as u16
                } else {
                    u16::from(buf[i])
                }
            };
            let pixel = |i: usize| -> [u16; 4] {
                let p = i * channels;
                match channels {
                    1 => {
                        let v = sample(p);
                        [v, v, v, top]
                    }
                    2 => {
                        let v = sample(p);
                        [v, v, v, sample(p + 1)]
                    }
                    3 => [sample(p), sample(p + 1), sample(p + 2), top],
                    _ => [sample(p), sample(p + 1), sample(p + 2), sample(p + 3)],
                }
            };
            let n = width as usize * height as usize;
            let has_alpha = channels % 2 == 0 && (0..n).any(|i| pixel(i)[3] != top);
            let heic = crate::encode::encode_pixels(
                width,
                height,
                bit_depth,
                has_alpha,
                Some(&self.metadata),
                self.format,
                &self.options,
                pixel,
            )
            .map_err(|e| ImageError::Encoding(EncodingError::new(hint(), e)))?;
            self.writer.write_all(&heic).map_err(ImageError::IoError)
        }

        fn set_icc_profile(&mut self, icc_profile: Vec<u8>) -> Result<(), UnsupportedError> {
            self.metadata.icc_profile = Some(icc_profile);
            Ok(())
        }

        fn set_exif_metadata(&mut self, exif: Vec<u8>) -> Result<(), UnsupportedError> {
            // Some callers keep the `Exif\0\0` prefix of JPEG APP1 segments.
            let tiff = exif.strip_prefix(b"Exif\0\0").unwrap_or(&exif);
            self.metadata.exif = Some(tiff.to_vec());
            Ok(())
        }
    }
}
