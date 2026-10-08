//! Pure-Rust HEIF/HEIC image decoder (and, later, encoder).
//!
//! ```no_run
//! let bytes = std::fs::read("photo.heic").unwrap();
//! let image = heifer::decode(&bytes).unwrap();
//! let rgba: Vec<u8> = image.to_rgba8();
//! println!("{}x{}, alpha: {}", image.width, image.height, image.has_alpha);
//! ```
//!
//! This crate ties together the container ([`heifer_isobmff`]) and the HEVC codec
//! ([`heifer_hevc_dec`], [`heifer_hevc_enc`]).

pub mod color;
pub mod image;

pub use heifer_hevc_dec;
pub use heifer_hevc_enc;
pub use heifer_isobmff;
pub use image::Image;

use color::{ColorParams, frame_to_rgba};
use heifer_hevc_dec::decoder::{DecodeOptions, decode_picture};
use heifer_isobmff::boxes::{ColorInfo, Property};
use heifer_isobmff::{HeifFile, ItemId};

/// Errors produced while decoding a HEIF file.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The HEIF container is invalid.
    #[error("container: {0}")]
    Container(#[from] heifer_isobmff::Error),
    /// An HEVC bitstream could not be decoded.
    #[error("HEVC: {0}")]
    Hevc(#[from] heifer_hevc_dec::Error),
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

/// Decodes one image item: coded (`hvc1`) or derived (`grid`, `iovl`), then applies its
/// transformative properties.
pub fn decode_item(file: &HeifFile<'_>, id: ItemId, depth: u32) -> Result<Image, Error> {
    decode_item_with(file, id, depth, &Options::default())
}

fn decode_item_with(
    file: &HeifFile<'_>,
    id: ItemId,
    depth: u32,
    options: &Options,
) -> Result<Image, Error> {
    if depth > MAX_DEPTH {
        return Err(Error::Invalid("derived images nested too deeply"));
    }
    let item = file.item(id)?;
    let mut image = match &item.item_type.0 {
        b"hvc1" => decode_hvc1(file, id, options)?,
        b"grid" => decode_grid(file, id, depth, options)?,
        b"iovl" => decode_overlay(file, id, depth, options)?,
        _ => {
            return Err(Error::Unsupported(format!(
                "image item type `{}`",
                item.item_type
            )));
        }
    };
    // Transformative properties, in association order (clap, then irot, then imir).
    for property in file.item_properties(id)? {
        image = match *property {
            Property::CleanAperture {
                width,
                height,
                horiz_offset,
                vert_offset,
            } => clean_aperture(&image, width, height, horiz_offset, vert_offset)?,
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

fn decode_hvc1(file: &HeifFile<'_>, id: ItemId, options: &Options) -> Result<Image, Error> {
    let hevc_options = DecodeOptions {
        max_pixels: options.max_pixels(),
        ..Default::default()
    };
    let frame = decode_picture(&file.hevc_bitstream(id)?, hevc_options)?;
    let mut image = frame_to_rgba(&frame, color_params(file, id, &frame)?);
    if let Some((w, h)) = file.image_size(id)?
        && (w, h) != (image.width, image.height)
    {
        // `ispe` is authoritative; HEVC may code a slightly larger picture.
        image = image.crop(0, 0, w, h);
    }

    // Alpha channel: an auxiliary image of type alpha referencing this item.
    for aux in file.referencing_items(id, b"auxl") {
        let is_alpha = file.item_properties(aux)?.any(|p| {
            matches!(p, Property::AuxiliaryType(t)
                if t == "urn:mpeg:hevc:2015:auxid:1" || t == "urn:mpeg:mpegB:cicp:systems:auxiliary:alpha")
        });
        if !is_alpha {
            continue;
        }
        let alpha = decode_picture(&file.hevc_bitstream(aux)?, hevc_options)?;
        if (alpha.widths[0], alpha.heights[0]) != (image.width, image.height)
            && alpha.widths[0] < image.width
        {
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
                image.data[(y * image.width as usize + x) * 4 + 3] = a as u16;
            }
        }
        image.has_alpha = true;
        break;
    }
    Ok(image)
}

fn check_size(w: u32, h: u32, max_pixels: u64) -> Result<(), Error> {
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > max_pixels {
        return Err(Error::Invalid("image size is zero or too large"));
    }
    Ok(())
}

fn decode_grid(
    file: &HeifFile<'_>,
    id: ItemId,
    depth: u32,
    options: &Options,
) -> Result<Image, Error> {
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
        (Some((w, h)), Some(c)) => (w, h, c.bit_depth_luma.max(c.bit_depth_chroma)),
        _ => {
            let tile = decode_item_with(file, tiles[0], depth + 1, options)?;
            let size = (tile.width, tile.height, tile.bit_depth);
            first_tile = Some(tile);
            size
        }
    };
    let canvas = std::sync::Mutex::new(Image::filled(
        grid.output_width,
        grid.output_height,
        bit_depth,
        [0; 4],
    ));
    let place = |i: usize, tile: &Image| -> Result<(), Error> {
        if (tile.width, tile.height, tile.bit_depth) != (tw, th, bit_depth) {
            return Err(Error::Invalid(
                "grid tiles have different sizes or bit depths",
            ));
        }
        let (x0, y0) = ((i as u32 % columns) * tw, (i as u32 / columns) * th);
        let mut canvas = canvas.lock().unwrap_or_else(|e| e.into_inner());
        // Copy rows (no blending): tiles are opaque unless they carry alpha.
        let w = tw.min(canvas.width.saturating_sub(x0)) as usize;
        for y in 0..th.min(canvas.height.saturating_sub(y0)) {
            let s = (y * tw) as usize * 4;
            let d = ((y0 + y) * canvas.width + x0) as usize * 4;
            canvas.data[d..d + w * 4].copy_from_slice(&tile.data[s..s + w * 4]);
        }
        canvas.has_alpha |= tile.has_alpha;
        Ok(())
    };
    let start = if let Some(tile) = first_tile {
        place(0, &tile)?;
        1
    } else {
        0
    };

    // Tiles are independent HEVC streams: decode them on several threads.
    let threads = options.threads().min(tiles.len() - start);
    if threads <= 1 {
        for (i, &tile_id) in tiles.iter().enumerate().skip(start) {
            place(i, &decode_item_with(file, tile_id, depth + 1, options)?)?;
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
                        let result = decode_item_with(file, tiles[i], depth + 1, options)
                            .and_then(|t| place(i, &t));
                        if let Err(e) = result {
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
    Ok(canvas.into_inner().unwrap_or_else(|e| e.into_inner()))
}

fn decode_overlay(
    file: &HeifFile<'_>,
    id: ItemId,
    depth: u32,
    options: &Options,
) -> Result<Image, Error> {
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
        .map(|&i| decode_item_with(file, i, depth + 1, options))
        .collect::<Result<Vec<_>, _>>()?;
    let bit_depth = images.iter().map(|i| i.bit_depth).max().unwrap_or(8);
    // canvas_fill_value is given on 16 bits.
    let scale = |v: u16| ((u32::from(v) * ((1u32 << bit_depth) - 1) + 32767) / 65535) as u16;
    let mut canvas = Image::filled(w, h, bit_depth, fill.map(scale));
    canvas.has_alpha = fill[3] != 65535;
    for (img, (x, y)) in images.iter().zip(offsets) {
        canvas.draw(img, x, y);
    }
    let opaque = canvas.max_value();
    canvas.has_alpha = canvas.data.chunks_exact(4).any(|p| p[3] != opaque);
    Ok(canvas)
}

/// Applies a `clap` (clean aperture) crop, centred as specified by ISO/IEC 14496-12.
fn clean_aperture(
    img: &Image,
    width: (u32, u32),
    height: (u32, u32),
    horiz: (i32, u32),
    vert: (i32, u32),
) -> Result<Image, Error> {
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
