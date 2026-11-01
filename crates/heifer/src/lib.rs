//! Pure-Rust HEIF/HEIC image decoder and encoder.
//!
//! ```no_run
//! let bytes = std::fs::read("photo.heic").unwrap();
//! let image = heifer::decode(&bytes).unwrap();
//! let rgba: Vec<u8> = image.to_rgba8();
//! println!("{}x{}, alpha: {}", image.width, image.height, image.has_alpha);
//!
//! let heic = heifer::encode(&image, &heifer::EncodeOptions::default()).unwrap();
//! std::fs::write("copy.heic", heic).unwrap();
//! ```
//!
//! This crate ties together the container ([`heifer_isobmff`]) and the HEVC codec
//! ([`heifer_hevc_dec`], [`heifer_hevc_enc`]).

pub mod color;
pub mod encode;
pub mod hdr;
mod icc;
pub mod image;
#[cfg(feature = "image")]
pub mod image_crate;
pub mod metadata;

pub use encode::{
    EncodeOptions, encode, encode_hdr, encode_hdr_with_metadata, encode_rgb8, encode_rgba8,
    encode_with_metadata,
};
pub use hdr::{GainMap, HdrImage, decode_hdr, read_gain_map};
pub use heifer_hevc_dec;
pub use heifer_hevc_enc;
pub use heifer_isobmff;
pub use image::{Image, Sample};
pub use metadata::{Exif, Metadata, item_metadata, read_metadata};

use color::{ColorParams, frame_to_rgba_into, frame_to_rgba_sized};
use heifer_hevc_dec::decoder::{DecodeOptions, decode_picture};
use heifer_hevc_dec::recon::Frame;
use heifer_isobmff::boxes::{ColorInfo, Property};
use heifer_isobmff::{HeifFile, ItemId};

/// Errors produced while decoding or encoding a HEIF file.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The HEIF container is invalid.
    #[error("container: {0}")]
    Container(#[from] heifer_isobmff::Error),
    /// An HEVC bitstream could not be decoded.
    #[error("HEVC: {0}")]
    Hevc(#[from] heifer_hevc_dec::Error),
    /// An image could not be encoded.
    #[error("HEVC encoding: {0}")]
    Encode(#[from] heifer_hevc_enc::Error),
    /// The file uses a feature heifer does not support yet.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// The file is inconsistent.
    #[error("invalid file: {0}")]
    Invalid(&'static str),
}

/// Limits protecting against malicious files.
const MAX_PIXELS: u64 = 1 << 28;
const MAX_DEPTH: u32 = 4;

/// Decoding options.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Maximum number of threads used to decode grid tiles in parallel. `0` uses all
    /// available cores; `1` decodes sequentially.
    pub max_threads: usize,
    /// Maximum size, in pixels, of any decoded or assembled image (protects against malicious
    /// files). `0` means the default, 2^28 pixels (e.g. a 16384×16384 image).
    pub max_pixels: u64,
}

impl Options {
    fn max_pixels(&self) -> u64 {
        if self.max_pixels == 0 {
            MAX_PIXELS
        } else {
            self.max_pixels
        }
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

/// Decodes the primary image of a HEIF/HEIC file, with its alpha channel if any, and applies
/// the crop, rotation and mirror transforms stored in the file. Grid tiles are decoded in
/// parallel on all available cores.
pub fn decode(bytes: &[u8]) -> Result<Image, Error> {
    decode_with_options(bytes, &Options::default())
}

/// Like [`decode`], with options.
pub fn decode_with_options(bytes: &[u8], options: &Options) -> Result<Image, Error> {
    let file = HeifFile::parse(bytes)?;
    decode_item_with(&file, file.primary_id, 0, options)
}

/// Like [`decode_with_options`], at 8 bits per sample whatever the coded depth: half the memory,
/// which counts on large photos.
///
/// On files coded at 8 bits the result is identical to [`Image::to_rgba8`] on the wide image. On
/// 10- and 12-bit files the conversion happens at 8 bits rather than after it, so samples can
/// differ by one.
pub fn decode_rgba8(bytes: &[u8], options: &Options) -> Result<Image<u8>, Error> {
    let file = HeifFile::parse(bytes)?;
    decode_item_with(&file, file.primary_id, 0, options)
}

/// Like [`decode`], with the samples converted to sRGB: from the image's embedded ICC profile
/// when it has one (as iPhone and many camera photos do, in `Display P3`), otherwise from its
/// `nclx` colour information. Without either, the samples are returned as coded (as [`decode`]
/// does).
///
/// This is what a viewer without colour management expects. Files in a colour space heifer
/// cannot convert (LUT-based ICC profiles, or PQ/HLG transfers, which are HDR and belong to
/// [`decode_hdr`]) are refused with [`Error::Unsupported`].
pub fn decode_srgb(bytes: &[u8]) -> Result<Image, Error> {
    decode_srgb_with_options(bytes, &Options::default())
}

/// Like [`decode_srgb`], with options.
pub fn decode_srgb_with_options(bytes: &[u8], options: &Options) -> Result<Image, Error> {
    let file = HeifFile::parse(bytes)?;
    let id = file.primary_id;
    let image = decode_item_with(&file, id, 0, options)?;
    to_srgb(&file, id, image)
}

/// Like [`decode_srgb`], at 8 bits per sample whatever the coded depth: half the memory, which
/// counts on large photos (see [`decode_rgba8`] for the caveat on 10- and 12-bit files).
pub fn decode_srgb_rgba8(bytes: &[u8], options: &Options) -> Result<Image<u8>, Error> {
    let file = HeifFile::parse(bytes)?;
    let id = file.primary_id;
    let image = decode_item_with(&file, id, 0, options)?;
    to_srgb(&file, id, image)
}

/// Converts a decoded image to sRGB from its ICC profile, else from its `nclx` colour
/// information, else leaves it untouched.
fn to_srgb<S: Sample>(file: &HeifFile<'_>, id: ItemId, image: Image<S>) -> Result<Image<S>, Error> {
    if let Some(icc) = file.icc_profile(id)? {
        return Ok(icc::Profile::parse(icc)?.to_srgb(&image));
    }
    match nclx(file, id)? {
        Some((primaries, transfer)) => icc::from_primaries(&image, primaries, transfer),
        None => Ok(image),
    }
}

/// The `nclx` colour primaries and transfer characteristics of an item, or of the first source
/// of a derived image (a grid's tiles), following the `dimg` references.
fn nclx(file: &HeifFile<'_>, id: ItemId) -> Result<Option<(u16, u16)>, Error> {
    let mut current = Some(id);
    for _ in 0..MAX_DEPTH {
        let Some(item) = current else { return Ok(None) };
        let own = file.item_properties(item)?.find_map(|p| match p {
            Property::Color(ColorInfo::Nclx {
                primaries,
                transfer,
                ..
            }) => Some((*primaries, *transfer)),
            _ => None,
        });
        if own.is_some() {
            return Ok(own);
        }
        current = file
            .referenced_items(item, b"dimg")
            .first()
            .copied()
            .filter(|&first| first != item);
    }
    Ok(None)
}

/// Decodes the thumbnail of the primary image (a `thmb` item, such as the 320×240 preview
/// of iPhone photos), or `None` if the file has none: much faster than decoding the image.
pub fn decode_thumbnail(bytes: &[u8]) -> Result<Option<Image>, Error> {
    let file = HeifFile::parse(bytes)?;
    // The smallest one when there are several.
    let thumbnails = file.referencing_items(file.primary_id, b"thmb");
    let smallest = thumbnails.iter().min_by_key(|&&id| {
        file.image_size(id)
            .ok()
            .flatten()
            .map_or(u64::MAX, |(w, h)| u64::from(w) * u64::from(h))
    });
    match smallest {
        Some(&id) => Ok(Some(decode_item(&file, id, 0)?)),
        None => Ok(None),
    }
}

/// Decodes one image item: coded (`hvc1`) or derived (`grid`, `iovl`, `iden`), then applies its
/// transformative properties.
pub fn decode_item(file: &HeifFile<'_>, id: ItemId, depth: u32) -> Result<Image, Error> {
    decode_item_with(file, id, depth, &Options::default())
}

fn decode_item_with<S: Sample>(
    file: &HeifFile<'_>,
    id: ItemId,
    depth: u32,
    options: &Options,
) -> Result<Image<S>, Error> {
    if depth > MAX_DEPTH {
        return Err(Error::Invalid("derived images nested too deeply"));
    }
    let item = file.item(id)?;
    let mut image = match &item.item_type.0 {
        b"hvc1" => decode_hvc1(file, id, options)?,
        b"grid" => decode_grid(file, id, depth, options)?,
        b"iovl" => decode_overlay(file, id, depth, options)?,
        b"iden" => match file.referenced_items(id, b"dimg")[..] {
            [source] => decode_item_with(file, source, depth + 1, options)?,
            _ => return Err(Error::Invalid("iden item without exactly one source")),
        },
        _ => {
            return Err(Error::Unsupported(format!(
                "image item type `{}`",
                item.item_type
            )));
        }
    };
    // Alpha plane of a derived image (coded images handle theirs when decoded).
    if item.item_type.0 != *b"hvc1" {
        for aux in file.referencing_items(id, b"auxl") {
            if is_alpha_item(file, aux)? {
                attach_alpha_image(
                    &mut image,
                    &decode_item_with(file, aux, depth + 1, options)?,
                )?;
                break;
            }
        }
    }
    // Transformative properties, in association order (clap, then irot, then imir).
    for property in file.item_properties(id)? {
        image = match *property {
            Property::CleanAperture {
                width,
                height,
                horiz_offset,
                vert_offset,
            } => clean_aperture(&image, width, height, horiz_offset, vert_offset)?,
            Property::Rotation(0) => image,
            Property::Rotation(q) => image.rotate_ccw(q),
            Property::Mirror(top_bottom) => image.mirror(top_bottom),
            _ => image,
        };
    }
    Ok(image)
}

/// Colour parameters of an item: its `colr` (nclx) property if present, otherwise those of the
/// HEVC stream (ISO/IEC 23008-12 defers to the coded bitstream).
fn color_params(
    file: &HeifFile<'_>,
    id: ItemId,
    frame: &heifer_hevc_dec::recon::Frame,
) -> Result<ColorParams, Error> {
    Ok(file
        .item_properties(id)?
        .find_map(|p| match p {
            Property::Color(ColorInfo::Nclx {
                matrix, full_range, ..
            }) => Some(ColorParams {
                matrix: *matrix,
                full_range: *full_range,
            }),
            _ => None,
        })
        .unwrap_or_else(|| ColorParams::from_frame(frame)))
}

/// Decodes the HEVC picture of a coded image item.
fn decode_hvc1_frame(file: &HeifFile<'_>, id: ItemId, options: &Options) -> Result<Frame, Error> {
    let hevc_options = DecodeOptions {
        max_pixels: options.max_pixels(),
        ..Default::default()
    };
    Ok(decode_picture(&file.hevc_bitstream(id)?, hevc_options)?)
}

fn decode_hvc1<S: Sample>(
    file: &HeifFile<'_>,
    id: ItemId,
    options: &Options,
) -> Result<Image<S>, Error> {
    let hevc_options = DecodeOptions {
        max_pixels: options.max_pixels(),
        ..Default::default()
    };
    let frame = decode_hvc1_frame(file, id, options)?;
    // `ispe` is authoritative; HEVC may code a slightly larger picture.
    let (width, height) = file
        .image_size(id)?
        .unwrap_or((frame.widths[0], frame.heights[0]));
    let params = color_params(file, id, &frame)?;
    let mut image = frame_to_rgba_sized(&frame, params, width, height);

    // Alpha channel: an auxiliary image of type alpha referencing this item.
    for aux in file.referencing_items(id, b"auxl") {
        if !is_alpha_item(file, aux)? {
            continue;
        }
        let alpha = decode_picture(&file.hevc_bitstream(aux)?, hevc_options)?;
        // The alpha plane must cover the image: anything smaller would be read out of bounds.
        if alpha.widths[0] < image.width || alpha.heights[0] < image.height {
            return Err(Error::Unsupported(
                "alpha plane smaller than the image".into(),
            ));
        }
        // The alpha plane is the luma of the auxiliary image, with its own range.
        let range = color_params(file, aux, &alpha)?;
        let bd = alpha.bit_depth[0];
        let (offset, span) = if range.full_range {
            (0.0, f64::from((1u32 << bd) - 1))
        } else {
            (f64::from(16u32 << (bd - 8)), f64::from(219u32 << (bd - 8)))
        };
        let max = f64::from(image.max_value());
        for y in 0..image.height as usize {
            for x in 0..image.width as usize {
                let a = f64::from(alpha.planes[0][y * alpha.widths[0] as usize + x]);
                let a = ((a - offset) / span * max).round().clamp(0.0, max);
                image.data[(y * image.width as usize + x) * 4 + 3] = S::from_u16(a as u16);
            }
        }
        image.has_alpha = true;
        break;
    }
    Ok(image)
}

/// Whether an item is an auxiliary image of type alpha.
fn is_alpha_item(file: &HeifFile<'_>, id: ItemId) -> Result<bool, Error> {
    Ok(file.item_properties(id)?.any(|p| {
        matches!(p, Property::AuxiliaryType(t)
            if t == "urn:mpeg:hevc:2015:auxid:1" || t == "urn:mpeg:mpegB:cicp:systems:auxiliary:alpha")
    }))
}

/// Uses the first channel of a decoded alpha image (e.g. a grid of monochrome tiles) as the
/// alpha channel of `image`.
fn attach_alpha_image<S: Sample>(image: &mut Image<S>, alpha: &Image<S>) -> Result<(), Error> {
    if alpha.width < image.width || alpha.height < image.height {
        return Err(Error::Unsupported(
            "alpha plane smaller than the image".into(),
        ));
    }
    let (from, to) = (u32::from(alpha.max_value()), u32::from(image.max_value()));
    for y in 0..image.height as usize {
        for x in 0..image.width as usize {
            let a = u32::from(alpha.data[(y * alpha.width as usize + x) * 4].to_u16());
            image.data[(y * image.width as usize + x) * 4 + 3] =
                S::from_u16(((a * to + from / 2) / from) as u16);
        }
    }
    image.has_alpha = true;
    Ok(())
}

fn check_size(w: u32, h: u32, max_pixels: u64) -> Result<(), Error> {
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > max_pixels {
        return Err(Error::Invalid("image size is zero or too large"));
    }
    Ok(())
}

fn decode_grid<S: Sample>(
    file: &HeifFile<'_>,
    id: ItemId,
    depth: u32,
    options: &Options,
) -> Result<Image<S>, Error> {
    let grid = file.grid(id)?;
    let tiles = file.referenced_items(id, b"dimg");
    let columns = u32::from(grid.columns);
    if tiles.len() != usize::from(grid.rows) * usize::from(grid.columns) {
        return Err(Error::Invalid(
            "grid tile count does not match its rows and columns",
        ));
    }
    check_size(grid.output_width, grid.output_height, options.max_pixels())?;

    // Tile size and bit depth come from the tiles' `ispe` and `hvcC` properties, so that all
    // tiles can be decoded in parallel. Otherwise, decode the first tile to find out.
    let mut first_tile = None;
    let (tw, th, bit_depth) = match (file.image_size(tiles[0])?, file.hevc_config(tiles[0])?) {
        (Some((w, h)), Some(c)) => (w, h, S::depth(c.bit_depth_luma.max(c.bit_depth_chroma))),
        _ => {
            let tile: Image<S> = decode_item_with(file, tiles[0], depth + 1, options)?;
            let size = (tile.width, tile.height, tile.bit_depth);
            first_tile = Some(tile);
            size
        }
    };
    // The tile size comes from the file: reject values no real picture could have, and keep the
    // row stride within the canvas so that it cannot overflow on 32-bit targets.
    let tile_pixels = u64::from(tw) * u64::from(th);
    if tile_pixels == 0 || tile_pixels > options.max_pixels() {
        return Err(Error::Invalid("grid tile has an invalid size"));
    }
    let mut canvas = Image::filled_samples(
        grid.output_width,
        grid.output_height,
        bit_depth,
        [S::default(); 4],
    );
    let (canvas_w, canvas_h) = (canvas.width, canvas.height);
    let stride = canvas_w as usize * 4;
    let tile_stride = tw.min(canvas_w) as usize * 4;
    // Each tile gets its own row segments of the canvas, so tiles are written concurrently.
    let mut segments: Vec<Vec<&mut [S]>> = (0..tiles.len()).map(|_| Vec::new()).collect();
    for (y, row) in canvas.data.chunks_exact_mut(stride).enumerate() {
        let first = (y / th as usize).saturating_mul(columns as usize);
        for (column, segment) in row.chunks_mut(tile_stride).enumerate() {
            if let Some(rows) = segments.get_mut(first.saturating_add(column)) {
                rows.push(segment);
            }
        }
    }
    let segments: Vec<std::sync::Mutex<Vec<&mut [S]>>> =
        segments.into_iter().map(std::sync::Mutex::new).collect();
    let has_alpha = std::sync::atomic::AtomicBool::new(false);
    // Plain tiles are converted straight into the canvas, without an intermediate image.
    let plain = first_tile.is_none()
        && tiles
            .iter()
            .all(|&t| is_plain_coded_image(file, t).unwrap_or(false));
    let place = |i: usize, tile: Option<&Image<S>>| -> Result<(), Error> {
        let x0 = (u64::from(i as u32 % columns) * u64::from(tw)).min(u64::from(u32::MAX)) as u32;
        let y0 = (u64::from(i as u32 / columns) * u64::from(th)).min(u64::from(u32::MAX)) as u32;
        let (w, h) = (
            tw.min(canvas_w.saturating_sub(x0)) as usize,
            th.min(canvas_h.saturating_sub(y0)) as usize,
        );
        let tile_image;
        let tile = match tile {
            Some(t) => Some(t),
            None if plain => None,
            None => {
                tile_image = decode_item_with(file, tiles[i], depth + 1, options)?;
                Some(&tile_image)
            }
        };
        let frame = match tile {
            Some(_) => None,
            None => {
                let frame = decode_hvc1_frame(file, tiles[i], options)?;
                let params = color_params(file, tiles[i], &frame)?;
                Some((frame, params))
            }
        };
        let size_ok = match (tile, &frame) {
            (Some(t), _) => (t.width, t.height, t.bit_depth) == (tw, th, bit_depth),
            (None, Some((f, _))) => {
                f.widths[0] >= tw
                    && f.heights[0] >= th
                    && S::depth(f.bit_depth[0].max(f.bit_depth[1])) == bit_depth
            }
            (None, None) => false,
        };
        if !size_ok {
            return Err(Error::Invalid(
                "grid tiles have different sizes or bit depths",
            ));
        }
        let mut out = segments[i].lock().unwrap_or_else(|e| e.into_inner());
        match (tile, frame) {
            (Some(tile), _) => {
                // Copy rows (no blending): tiles are opaque unless they carry alpha.
                for (dst, src) in out
                    .iter_mut()
                    .zip(tile.data.chunks_exact(tile.width as usize * 4))
                    .take(h)
                {
                    dst[..w * 4].copy_from_slice(&src[..w * 4]);
                }
                if tile.has_alpha {
                    has_alpha.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
            (None, Some((frame, params))) => frame_to_rgba_into(&frame, params, &mut out, w, h),
            (None, None) => unreachable!("checked above"),
        }
        Ok(())
    };
    let start = if let Some(tile) = &first_tile {
        place(0, Some(tile))?;
        1
    } else {
        0
    };

    // Tiles are independent HEVC streams: decode them on several threads.
    let threads = options.threads().min(tiles.len() - start);
    if threads <= 1 {
        for i in start..tiles.len() {
            place(i, None)?;
        }
    } else {
        let next = std::sync::atomic::AtomicUsize::new(start);
        let error = std::sync::Mutex::new(None);
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if i >= tiles.len() {
                            break;
                        }
                        if let Err(e) = place(i, None) {
                            error
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .get_or_insert(e);
                            next.store(tiles.len(), std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        if let Some(e) = error.into_inner().unwrap_or_else(|e| e.into_inner()) {
            return Err(e);
        }
    }
    drop(segments);
    canvas.has_alpha = has_alpha.into_inner();
    Ok(canvas)
}

/// Whether `id` is a coded image without transformative properties or alpha plane, so that
/// its decoded picture can be used as is.
fn is_plain_coded_image(file: &HeifFile<'_>, id: ItemId) -> Result<bool, Error> {
    if file.item(id)?.item_type.0 != *b"hvc1" || !file.referencing_items(id, b"auxl").is_empty() {
        return Ok(false);
    }
    Ok(!file.item_properties(id)?.any(|p| {
        matches!(
            p,
            Property::CleanAperture { .. } | Property::Rotation(_) | Property::Mirror(_)
        )
    }))
}

fn decode_overlay<S: Sample>(
    file: &HeifFile<'_>,
    id: ItemId,
    depth: u32,
    options: &Options,
) -> Result<Image<S>, Error> {
    let data = file.item_data(id)?;
    let inputs = file.referenced_items(id, b"dimg");
    let mut r = data.iter().copied();
    let mut next = || r.next().ok_or(Error::Invalid("truncated iovl data"));
    if next()? != 0 {
        return Err(Error::Unsupported("iovl version".into()));
    }
    let large = next()? & 1 != 0;
    let mut u16v = || -> Result<u16, Error> { Ok(u16::from_be_bytes([next()?, next()?])) };
    let fill = [u16v()?, u16v()?, u16v()?, u16v()?];
    drop(u16v);
    let mut field = |signed: bool| -> Result<i64, Error> {
        Ok(if large {
            let v = [next()?, next()?, next()?, next()?];
            if signed {
                i64::from(i32::from_be_bytes(v))
            } else {
                i64::from(u32::from_be_bytes(v))
            }
        } else {
            let v = [next()?, next()?];
            if signed {
                i64::from(i16::from_be_bytes(v))
            } else {
                i64::from(u16::from_be_bytes(v))
            }
        })
    };
    let (w, h) = (field(false)? as u32, field(false)? as u32);
    check_size(w, h, options.max_pixels())?;
    let offsets = (0..inputs.len())
        .map(|_| Ok((field(true)?, field(true)?)))
        .collect::<Result<Vec<_>, Error>>()?;

    let images = inputs
        .iter()
        .map(|&i| decode_item_with::<S>(file, i, depth + 1, options))
        .collect::<Result<Vec<_>, _>>()?;
    let bit_depth = images.iter().map(|i| i.bit_depth).max().unwrap_or(8);
    // canvas_fill_value is given on 16 bits.
    let scale = |v: u16| ((u32::from(v) * ((1u32 << bit_depth) - 1) + 32767) / 65535) as u16;
    let mut canvas = Image::filled_samples(w, h, bit_depth, fill.map(|v| S::from_u16(scale(v))));
    canvas.has_alpha = fill[3] != 65535;
    for (img, (x, y)) in images.iter().zip(offsets) {
        canvas.draw(img, x, y);
    }
    let opaque = canvas.max_value();
    canvas.has_alpha = canvas.data.chunks_exact(4).any(|p| p[3].to_u16() != opaque);
    Ok(canvas)
}

/// Applies a `clap` (clean aperture) crop, centred as specified by ISO/IEC 14496-12.
fn clean_aperture<S: Sample>(
    img: &Image<S>,
    width: (u32, u32),
    height: (u32, u32),
    horiz: (i32, u32),
    vert: (i32, u32),
) -> Result<Image<S>, Error> {
    if width.1 == 0 || height.1 == 0 || horiz.1 == 0 || vert.1 == 0 {
        return Err(Error::Invalid("clap with a zero denominator"));
    }
    let cw = f64::from(width.0) / f64::from(width.1);
    let ch = f64::from(height.0) / f64::from(height.1);
    let cx = f64::from(horiz.0) / f64::from(horiz.1) + (f64::from(img.width) - 1.0) / 2.0;
    let cy = f64::from(vert.0) / f64::from(vert.1) + (f64::from(img.height) - 1.0) / 2.0;
    let left = (cx - (cw - 1.0) / 2.0).round().max(0.0) as u32;
    let top = (cy - (ch - 1.0) / 2.0).round().max(0.0) as u32;
    Ok(img.crop(left, top, cw.round() as u32, ch.round() as u32))
}
