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
    ChromaFormat, CodingOptions, EncodedPicture, EncoderConfig, Planes16, encode_picture16,
    encode_picture16_with,
};
use heifer_isobmff::ItemId;
use heifer_isobmff::boxes::{ColorInfo, HevcConfig};
use heifer_isobmff::writer::{ALPHA_URN, HeifWriter, ImageSpec};

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

/// Encoding options.
#[derive(Debug, Clone, Copy)]
pub struct EncodeOptions {
    /// Quality, from 0 (smallest files) to 100 (best quality). Default: 80.
    pub quality: u8,
    /// Maximum number of threads used to encode tiles in parallel. `0` uses all available
    /// cores.
    pub max_threads: usize,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            quality: 80,
            max_threads: 0,
        }
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
/// let options = heifer::EncodeOptions { quality: 60, ..Default::default() };
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
        // GBR: component 0 is G, then B and R, with the identity matrix in the VUI. Bypassing
        // the transform and quantization makes the round trip exact.
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
