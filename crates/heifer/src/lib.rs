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

/// Decodes the primary image of a HEIF/HEIC file, with its alpha channel if any, and applies
/// the crop, rotation and mirror transforms stored in the file.
pub fn decode(bytes: &[u8]) -> Result<Image, Error> {
    let file = HeifFile::parse(bytes)?;
    decode_item(&file, file.primary_id, 0)
}

/// Decodes one image item: coded (`hvc1`) or derived (`grid`, `iovl`), then applies its
/// transformative properties.
pub fn decode_item(file: &HeifFile<'_>, id: ItemId, depth: u32) -> Result<Image, Error> {
    if depth > MAX_DEPTH {
        return Err(Error::Invalid("derived images nested too deeply"));
    }
    let item = file.item(id)?;
    let mut image = match &item.item_type.0 {
        b"hvc1" => decode_hvc1(file, id)?,
        b"grid" => decode_grid(file, id, depth)?,
        b"iovl" => decode_overlay(file, id, depth)?,
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

fn decode_hvc1(file: &HeifFile<'_>, id: ItemId) -> Result<Image, Error> {
    let frame = decode_picture(&file.hevc_bitstream(id)?, DecodeOptions::default())?;
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
        let alpha = decode_picture(&file.hevc_bitstream(aux)?, DecodeOptions::default())?;
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

fn check_size(w: u32, h: u32) -> Result<(), Error> {
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err(Error::Invalid("image size is zero or too large"));
    }
    Ok(())
}

fn decode_grid(file: &HeifFile<'_>, id: ItemId, depth: u32) -> Result<Image, Error> {
    let grid = file.grid(id)?;
    let tiles = file.referenced_items(id, b"dimg");
    if tiles.len() != usize::from(grid.rows) * usize::from(grid.columns) {
        return Err(Error::Invalid(
            "grid tile count does not match its rows and columns",
        ));
    }
    check_size(grid.output_width, grid.output_height)?;
    let mut canvas: Option<Image> = None;
    let (mut tw, mut th) = (0, 0);
    for (i, &tile_id) in tiles.iter().enumerate() {
        let tile = decode_item(file, tile_id, depth + 1)?;
        let canvas = canvas.get_or_insert_with(|| {
            (tw, th) = (tile.width, tile.height);
            Image::filled(
                grid.output_width,
                grid.output_height,
                tile.bit_depth,
                [0; 4],
            )
        });
        if (tile.width, tile.height) != (tw, th) {
            return Err(Error::Invalid("grid tiles have different sizes"));
        }
        let (col, row) = (
            i as u32 % u32::from(grid.columns),
            i as u32 / u32::from(grid.columns),
        );
        let (x0, y0) = (i64::from(col * tw), i64::from(row * th));
        // Copy (no blending): tiles are opaque unless they carry alpha.
        for y in 0..th {
            let cy = y0 + i64::from(y);
            if cy >= i64::from(canvas.height) {
                break;
            }
            for x in 0..tw {
                let cx = x0 + i64::from(x);
                if cx >= i64::from(canvas.width) {
                    break;
                }
                let s = ((y * tw + x) * 4) as usize;
                let d = ((cy as u32 * canvas.width + cx as u32) * 4) as usize;
                canvas.data[d..d + 4].copy_from_slice(&tile.data[s..s + 4]);
            }
        }
        canvas.has_alpha |= tile.has_alpha;
    }
    canvas.ok_or(Error::Invalid("grid without tiles"))
}

fn decode_overlay(file: &HeifFile<'_>, id: ItemId, depth: u32) -> Result<Image, Error> {
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
    check_size(w, h)?;
    let offsets = (0..inputs.len())
        .map(|_| Ok((field(true)?, field(true)?)))
        .collect::<Result<Vec<_>, Error>>()?;

    let images = inputs
        .iter()
        .map(|&i| decode_item(file, i, depth + 1))
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
