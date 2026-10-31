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
//! Images are converted to YCbCr 4:2:0 (BT.601 matrix, full range, sRGB colour space), in
//! 10 bits (HEVC Main 10) when the image has more than 8 bits per sample, and
//! coded as 512×512 tiles of a `grid` image, encoded in parallel, like the photos of iPhones.
//! Images of at most 512×512 pixels are coded as a single picture. The alpha channel, if any
//! sample is not opaque, is coded as a monochrome auxiliary image.

use heifer_hevc_enc::encoder::{EncodedPicture, EncoderConfig, Planes16, encode_picture16};
use heifer_isobmff::ItemId;
use heifer_isobmff::boxes::{ColorInfo, HevcConfig};
use heifer_isobmff::writer::{ALPHA_URN, HeifWriter, ImageSpec};

use crate::{Error, Image, Metadata};

/// Size of grid tiles.
const TILE: u32 = 512;

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
    check_input(width, height, rgba.len(), 4)?;
    let has_alpha = rgba.chunks_exact(4).any(|p| p[3] != 255);
    encode_pixels(width, height, 8, has_alpha, None, options, |i| {
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
    check_input(width, height, rgb.len(), 3)?;
    encode_pixels(width, height, 8, false, None, options, |i| {
        let p = &rgb[i * 3..i * 3 + 3];
        [p[0], p[1], p[2], 255].map(u16::from)
    })
}

/// Encodes an [`Image`] (e.g. one decoded by [`decode`](crate::decode)) as a HEIC file: in
/// 8 bits, or in 10 bits (HEVC Main 10) for images of more bits per sample, which keeps smooth
/// gradients free of banding.
pub fn encode(image: &Image, options: &EncodeOptions) -> Result<Vec<u8>, Error> {
    encode_with_metadata(image, &Metadata::default(), options)
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
        options,
        |i| {
            let p = &image.data[i * 4..i * 4 + 4];
            [scale(p[0]), scale(p[1]), scale(p[2]), scale(p[3])]
        },
    )
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

/// Encodes an image given by a function returning the RGBA pixel at an index, of `bit_depth`
/// (8 or 10) bits per sample, with its metadata.
pub(crate) fn encode_pixels(
    width: u32,
    height: u32,
    bit_depth: u8,
    has_alpha: bool,
    metadata: Option<&Metadata>,
    options: &EncodeOptions,
    pixel: impl Fn(usize) -> [u16; 4] + Sync,
) -> Result<Vec<u8>, Error> {
    let source = Source {
        width,
        height,
        bit_depth,
        pixel,
    };
    let (mut writer, image) = encode_image(&source, has_alpha, options)?;
    if let Some(m) = metadata {
        if let Some(icc) = &m.icc_profile {
            writer.add_icc_profile(image, icc);
        }
        if let Some(exif) = &m.exif {
            writer.add_exif(image, &crate::metadata::exif_upright(exif));
        }
        if let Some(xmp) = &m.xmp {
            writer.add_xmp(image, xmp);
        }
    }
    Ok(writer.finish())
}

/// The image to encode: its size, bits per sample, and RGBA pixels by index.
struct Source<F> {
    width: u32,
    height: u32,
    bit_depth: u8,
    pixel: F,
}

/// Encodes the pixels into a writer; returns it with the ID of the image (picture or grid).
fn encode_image<F: Fn(usize) -> [u16; 4] + Sync>(
    source: &Source<F>,
    has_alpha: bool,
    options: &EncodeOptions,
) -> Result<(HeifWriter, ItemId), Error> {
    let (width, height) = (source.width, source.height);
    let qp = options.qp();
    let color = EncoderConfig {
        qp,
        ..EncoderConfig::default()
    };
    // Alpha edges are visible: code the alpha plane a little better.
    let alpha = EncoderConfig {
        qp: qp.saturating_sub(4),
        colour: (2, 2, 2),
        ..color
    };
    let mut writer = HeifWriter::new();

    if width <= TILE && height <= TILE {
        let picture = encode_region(source, (0, 0), (width, height), false, &color)?;
        let image = add_picture(&mut writer, &picture, (width, height), false);
        if has_alpha {
            // Same coded size as the colour picture, so that both are cropped alike.
            let coded = (picture.width, picture.height);
            let alpha = encode_region(source, (0, 0), coded, true, &alpha)?;
            add_alpha_picture(&mut writer, image, &alpha, (width, height));
        }
        return Ok((writer, image));
    }

    let (columns, rows) = (width.div_ceil(TILE), height.div_ceil(TILE));
    if columns > 256 || rows > 256 {
        return Err(Error::Invalid(
            "image too large for a grid of 512×512 tiles",
        ));
    }
    let tiles = encode_tiles(source, columns, rows, false, &color, options)?;
    let ids: Vec<ItemId> = tiles
        .iter()
        .map(|t| add_picture(&mut writer, t, (TILE, TILE), true))
        .collect();
    let grid = writer.add_grid(
        rows as u16,
        columns as u16,
        width,
        height,
        &ids,
        Some(&COLOR),
        None,
    );
    if has_alpha {
        let tiles = encode_tiles(source, columns, rows, true, &alpha, options)?;
        let ids: Vec<ItemId> = tiles
            .iter()
            .map(|t| add_picture(&mut writer, t, (TILE, TILE), true))
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
    Ok((writer, grid))
}

/// Encodes all the tiles of a grid, in parallel. Returns them row by row.
fn encode_tiles<F: Fn(usize) -> [u16; 4] + Sync>(
    source: &Source<F>,
    columns: u32,
    rows: u32,
    alpha: bool,
    config: &EncoderConfig,
    options: &EncodeOptions,
) -> Result<Vec<EncodedPicture>, Error> {
    let count = (columns * rows) as usize;
    let tile = |i: usize| {
        let origin = ((i as u32 % columns) * TILE, (i as u32 / columns) * TILE);
        encode_region(source, origin, (TILE, TILE), alpha, config)
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

/// Encodes the `size` region at `origin` of the image: its colour (YCbCr 4:2:0) or its alpha
/// channel (monochrome). Pixels outside the image repeat the nearest edge.
fn encode_region<F: Fn(usize) -> [u16; 4]>(
    source: &Source<F>,
    origin: (u32, u32),
    size: (u32, u32),
    alpha: bool,
    config: &EncoderConfig,
) -> Result<EncodedPicture, Error> {
    let (w, h) = (size.0 as usize, size.1 as usize);
    let (width, height) = (source.width as usize, source.height as usize);
    let at = |x: usize, y: usize| -> [u16; 4] {
        let x = (origin.0 as usize + x).min(width - 1);
        let y = (origin.1 as usize + y).min(height - 1);
        (source.pixel)(y * width + x)
    };
    let bit_depth = source.bit_depth;
    if alpha {
        let a: Vec<u16> = (0..w * h).map(|i| at(i % w, i / w)[3]).collect();
        let planes = Planes16 {
            y: &a,
            chroma: None,
            bit_depth,
        };
        return Ok(encode_picture16(size.0, size.1, planes, config)?);
    }

    // BT.601, full range, in 16-bit fixed point. Chroma is the average of each 2×2 block.
    let mut y = Vec::with_capacity(w * h);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let mut cb_sum = vec![0i32; cw * ch];
    let mut cr_sum = vec![0i32; cw * ch];
    for row in 0..h {
        for col in 0..w {
            let [r, g, b, _] = at(col, row).map(i32::from);
            y.push(((19595 * r + 38470 * g + 7471 * b + 32768) >> 16) as u16);
            let i = (row / 2) * cw + col / 2;
            cb_sum[i] += -11059 * r - 21709 * g + 32768 * b;
            cr_sum[i] += 32768 * r - 27439 * g - 5329 * b;
        }
    }
    let count = |i: usize| -> i32 {
        let (cx, cy) = (i % cw, i / cw);
        ((w - 2 * cx).min(2) * (h - 2 * cy).min(2)) as i32
    };
    let (mid, max) = (1i64 << (bit_depth - 1), (1i64 << bit_depth) - 1);
    let chroma = |sums: &[i32]| -> Vec<u16> {
        sums.iter()
            .enumerate()
            .map(|(i, &s)| {
                let n = i64::from(count(i)) << 16;
                ((i64::from(s) + n * mid + n / 2).div_euclid(n)).clamp(0, max) as u16
            })
            .collect()
    };
    let (cb, cr) = (chroma(&cb_sum), chroma(&cr_sum));
    let planes = Planes16 {
        y: &y,
        chroma: Some((&cb, &cr)),
        bit_depth,
    };
    Ok(encode_picture16(size.0, size.1, planes, config)?)
}

fn hevc_config(picture: &EncodedPicture) -> HevcConfig {
    HevcConfig {
        profile_idc: picture.profile_idc,
        level_idc: picture.level_idc,
        chroma_format: picture.chroma_format,
        // Main (1) is 8 bits, Main 10 (2) is 10 bits here.
        bit_depth_luma: if picture.profile_idc == 2 { 10 } else { 8 },
        bit_depth_chroma: if picture.profile_idc == 2 { 10 } else { 8 },
        nal_length_size: 4,
        nal_units: picture.parameter_sets.clone(),
    }
}

/// Adds a coded picture of an image of `size` (the picture may be larger: odd sizes are
/// rounded up in 4:2:0, then cropped with `clap`).
fn add_picture(
    writer: &mut HeifWriter,
    picture: &EncodedPicture,
    size: (u32, u32),
    hidden: bool,
) -> ItemId {
    let color = (picture.chroma_format != 0).then_some(COLOR);
    writer.add_image(&ImageSpec {
        color,
        hidden,
        crop: (size != (picture.width, picture.height)).then_some(size),
        ..ImageSpec::new(
            picture.width,
            picture.height,
            hevc_config(picture),
            &picture.slices,
        )
    })
}

fn add_alpha_picture(
    writer: &mut HeifWriter,
    image: ItemId,
    picture: &EncodedPicture,
    size: (u32, u32),
) -> ItemId {
    writer.add_alpha(
        image,
        &ImageSpec {
            crop: (size != (picture.width, picture.height)).then_some(size),
            ..ImageSpec::new(
                picture.width,
                picture.height,
                hevc_config(picture),
                &picture.slices,
            )
        },
    )
}
