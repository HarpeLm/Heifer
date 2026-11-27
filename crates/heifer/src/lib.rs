//! Pure-Rust HEIC/HEIF (and `.heics`) decoder and encoder — `#![forbid(unsafe_code)]`, zero
//! C/`libheif` dependency, WASM-ready, and optional [`image`](https://docs.rs/image) crate support.
//!
//! ```no_run
//! let bytes = std::fs::read("photo.heic").unwrap();
//! let image = heifer::decode(&bytes).unwrap();
//! let rgba: Vec<u8> = image.to_rgba8();
//! println!("{}x{}, alpha: {}", image.width, image.height, image.has_alpha);
//!
//! # #[cfg(feature = "encoder")]
//! # {
//! let heic = heifer::encode(&image, &heifer::EncodeOptions::default()).unwrap();
//! std::fs::write("copy.heic", heic).unwrap();
//! # }
//!
//! # #[cfg(feature = "image")]
//! # {
//! heifer::image_crate::register_image_decoder_hooks();
//! let img = ::image::open("photo.heic").unwrap();
//! img.save("photo.png").unwrap();
//! # }
//! ```
//!
//! This crate ties together the container ([`heifer_isobmff`]) and the HEVC codec
//! ([`heifer_hevc_dec`], and `heifer_hevc_enc` with the default `encoder` feature).
//!
//! ## Migrating from `libheif-rs`
//!
//! | `libheif-rs` | `heifer` |
//! |---|---|
//! | `HeifContext::read_from_bytes` + `decode` | [`decode`] / [`decode_rgba8`] / [`decode_srgb`] |
//! | `check_file_type` / `FileTypeResult` | [`check_file_type`] / [`FileTypeResult`] |
//! | `metadata_block_ids` / `metadata` | [`read_metadata`] / [`read_iptc`] |
//! | `HeifContext::new` + `encode_image` | `encode` / `encode_rgb8` / `encode_rgba8` |
//! | Image sequences (`.heics`) | [`read_sequence`] / [`decode_sequence`] / `encode_sequence` |
//! | `integration::image::register_all_decoding_hooks` | `image_crate::register_image_decoder_hooks` |
//!
//! ## Features
//!
//! - `encoder` (default): enables HEIC and `.heics` encoding via `heifer_hevc_enc`. Disable
//!   default features (`default-features = false`) for decode-only builds.
//! - `image`: enables [`image`](https://docs.rs/image) crate integration (`image_crate` module:
//!   `HeifDecoder`, `HeifSequenceDecoder`, `HeifEncoder`, and `image::open` hooks).

pub mod color;
pub mod depth;
#[cfg(feature = "encoder")]
pub mod encode;
pub mod hdr;
mod icc;
pub mod image;
#[cfg(feature = "image")]
pub mod image_crate;
pub mod iptc;
pub mod metadata;
pub mod preview;
pub mod reader;
pub mod sequence;
mod uncompressed;

pub use depth::{DepthInfo, DepthRepresentation};
#[cfg(feature = "encoder")]
pub use encode::{
    EncodeFormat, EncodeOptions, EncodeSpeed, encode, encode_hdr, encode_hdr_with_metadata,
    encode_rgb8, encode_rgb8_with_format, encode_rgba8, encode_rgba8_with_format, encode_sequence,
    encode_sequence_to_size, encode_sequence_to_size_with_format, encode_sequence_with_format,
    encode_to_size, encode_with_format, encode_with_metadata, encode_with_metadata_and_format,
    fit_to_size,
};
pub use hdr::{
    APPLE_GAIN_MAP, GainMap, HdrImage, decode_hdr, decode_hdr_with_options, read_gain_map,
    read_gain_map_with_options,
};
pub use heifer_hevc_dec;
#[cfg(feature = "encoder")]
pub use heifer_hevc_enc;
pub use heifer_isobmff;
pub use heifer_isobmff::FourCC;
pub use heifer_isobmff::region::{Geometry, RegionItem, UserDescription};
pub use heifer_isobmff::writer::{ALPHA_URN, DEPTH_URN};
pub use image::{Image, Sample};
pub use iptc::{Dataset, Iptc};
pub use metadata::{
    Exif, Metadata, Regions, item_iptc, item_metadata, item_regions, read_iptc, read_metadata,
    read_regions,
};
pub use preview::{PreviewOptions, decode_preview_rgba8};
pub use reader::{
    ReadError, decode_reader, decode_reader_preview_rgba8, decode_reader_rgba8,
    decode_reader_srgb_rgba8, decode_reader_with_options, read_info_reader, read_metadata_reader,
};
pub use sequence::{
    FrameInfo, SequenceFrames, SequenceInfo, decode_sequence, decode_sequence_frame,
    decode_sequence_frame_with_options, decode_sequence_with_options, read_sequence,
};

use color::{
    ColorParams, ColorScratch, frame_view_to_rgba_into_with_scratch, frame_view_to_rgba_sized,
    frame_view_to_rgba_strided_with_scratch,
};
use heifer_hevc_dec::decoder::{
    DecodeOptions, DecodeWorkspace, DecodedFrameView, decode_picture_view_into,
};
use heifer_hevc_dec::nal::{NalUnit, split_annex_b};
use heifer_hevc_dec::params::ParameterSets;
use heifer_hevc_dec::recon::{CropWindow, Frame};
use heifer_hevc_dec::sequence::SequenceDecoder;
use heifer_isobmff::boxes::{ColorInfo, Property};
use heifer_isobmff::{HeifFile, ItemId};

/// Errors produced while decoding or encoding a HEIF file.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The HEIF container is invalid.
    #[error("container: {0}")]
    Container(#[from] heifer_isobmff::Error),
    /// An HEVC bitstream could not be decoded.
    #[error("HEVC: {0}")]
    Hevc(#[from] heifer_hevc_dec::Error),
    /// An image could not be encoded.
    #[cfg(feature = "encoder")]
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
pub(crate) const MAX_PIXELS: u64 = 1 << 28;
const MAX_DEPTH: u32 = 4;

/// Decoding options.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Options {
    /// Maximum number of threads used to decode grid tiles, and the groups of pictures of an
    /// image sequence, in parallel. `0` uses all available cores; `1` decodes sequentially.
    pub max_threads: usize,
    /// Maximum size, in pixels, of any decoded or assembled image (protects against malicious
    /// files). `0` means the default, 2^28 pixels (e.g. a 16384×16384 image).
    pub max_pixels: u64,
    /// When `true`, sRGB-converting entry points ([`decode_srgb_with_options`],
    /// [`decode_srgb_rgba8`], [`decode_reader_srgb_rgba8`], [`decode_all_srgb_rgba8`],
    /// [`decode_preview_rgba8`]) return the already-decoded image as coded instead of failing
    /// with [`Error::Unsupported`] if its ICC profile or `nclx` colour space cannot be converted
    /// to sRGB (for example a non-RGB ICC profile or a PQ/HLG transfer). Defaults to `false`.
    pub fallback_to_coded_color: bool,
}

/// Construction methods: they keep working when new options are added, unlike a struct
/// literal, so prefer them to setting the fields directly.
///
/// ```
/// let options = heifer::Options::new().with_max_threads(1).with_max_pixels(1 << 24);
/// assert_eq!((options.max_threads, options.max_pixels), (1, 1 << 24));
/// ```
impl Options {
    /// The default options: all cores, the default size limit, strict sRGB conversion.
    pub const fn new() -> Self {
        Self {
            max_threads: 0,
            max_pixels: 0,
            fallback_to_coded_color: false,
        }
    }

    /// Sets [`max_threads`](Self::max_threads): `0` uses all available cores.
    #[must_use]
    pub const fn with_max_threads(mut self, max_threads: usize) -> Self {
        self.max_threads = max_threads;
        self
    }

    /// Sets [`max_pixels`](#structfield.max_pixels): `0` means the default limit.
    #[must_use]
    pub const fn with_max_pixels(mut self, max_pixels: u64) -> Self {
        self.max_pixels = max_pixels;
        self
    }

    /// Sets [`fallback_to_coded_color`](Self::fallback_to_coded_color).
    #[must_use]
    pub const fn with_fallback_to_coded_color(mut self, fallback: bool) -> Self {
        self.fallback_to_coded_color = fallback;
        self
    }
}

impl Options {
    pub(crate) fn max_pixels(&self) -> u64 {
        if self.max_pixels == 0 {
            MAX_PIXELS
        } else {
            self.max_pixels
        }
    }

    pub(crate) fn threads(&self) -> usize {
        let available = std::thread::available_parallelism().map_or(1, |n| n.get());
        if self.max_threads == 0 {
            available
        } else {
            self.max_threads.min(available)
        }
    }
}

/// The result of [`check_file_type`], matching `libheif-rs`'s `FileTypeResult` so that a caller
/// can switch from `libheif` without changing its validation logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileTypeResult {
    /// A HEVC-coded HEIF file (`heic`/`heix`), which `heifer` decodes.
    Supported,
    /// Not a HEIF file.
    No,
    /// A HEIF file whose codec `heifer` does not support (AV1/AVIF, JPEG, JPEG 2000, image
    /// sequences, …).
    Unsupported,
    /// Too few bytes to decide, or a generic HEIF brand (`mif1`/`mif2`) whose codec is not
    /// known from the major brand alone.
    MayBe,
}

/// Checks the type of a file from its leading bytes, without decoding it: the equivalent of
/// `libheif`'s `heif_check_filetype`, for gating or validation before a full decode.
///
/// The first 12 bytes are enough (box size, `ftyp`, major brand); any more are ignored. Only
/// the major brand is read, as in `libheif`: HEVC images and image sequences (`heic`, `heix`,
/// `hevc`, `hevx`) are [`FileTypeResult::Supported`], and a generic `mif1`/`mif2`/`msf1` file
/// is [`FileTypeResult::MayBe`] even when it holds HEVC.
///
/// ```
/// let heic = b"\0\0\0\x18ftypheic";
/// assert_eq!(heifer::check_file_type(heic), heifer::FileTypeResult::Supported);
/// assert_eq!(
///     heifer::check_file_type(b"\0\0\0\x18ftypavif"),
///     heifer::FileTypeResult::Unsupported
/// );
/// assert_eq!(heifer::check_file_type(b"not a heif file"), heifer::FileTypeResult::No);
/// ```
pub fn check_file_type(data: &[u8]) -> FileTypeResult {
    if data.len() < 8 {
        return FileTypeResult::MayBe;
    }
    if &data[4..8] != b"ftyp" {
        return FileTypeResult::No;
    }
    if data.len() < 12 {
        return FileTypeResult::MayBe;
    }
    match &data[8..12] {
        // HEVC images, and HEVC image sequences.
        b"heic" | b"heix" | b"hevc" | b"hevx" => FileTypeResult::Supported,
        // Generic images and sequences: the codec is not known from the brand.
        b"mif1" | b"mif2" | b"msf1" => FileTypeResult::MayBe,
        _ => FileTypeResult::Unsupported,
    }
}

/// Kind of HEIF container, inferred from the container structure and `ftyp` major brand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FileKind {
    /// HEVC-coded still image (`heic` / `heix`, no image sequence track).
    Heic,
    /// Generic or uncompressed HEIF still image (`mif1` / `mif2`, no image sequence track).
    Heif,
    /// Image sequence (`.heics` or MP4/HEIF file with a sequence track).
    Sequence,
}

impl FileKind {
    /// Short extension / format name (`"heic"`, `"heif"`, or `"heics"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Heic => "heic",
            Self::Heif => "heif",
            Self::Sequence => "heics",
        }
    }

    /// MIME media type (`"image/heic"`, `"image/heif"`, or `"image/heic-sequence"`).
    pub const fn mime_type(self) -> &'static str {
        match self {
            Self::Heic => "image/heic",
            Self::Heif => "image/heif",
            Self::Sequence => "image/heic-sequence",
        }
    }
}

/// Lightweight container metadata read without decoding pixel data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ImageInfo {
    /// Displayed image width in pixels, after `clap`, `irot` and `imir` transforms.
    pub width: u32,
    /// Displayed image height in pixels, after `clap`, `irot` and `imir` transforms.
    pub height: u32,
    /// Bits per colour sample (typically 8, 10 or 12).
    pub bit_depth: u8,
    /// Whether the image carries an alpha channel or transparency.
    pub has_alpha: bool,
    /// Container kind ([`FileKind::Heic`], [`FileKind::Heif`], or [`FileKind::Sequence`]).
    pub kind: FileKind,
    /// Major brand from the `ftyp` box (e.g. `heic`, `mif1`, `msf1`).
    pub major_brand: FourCC,
    /// Primary item type (e.g. `hvc1`, `grid`, `iovl`, `unci`), or sequence sample coding
    /// (`hvc1` / `hev1`) when the file has no still image.
    pub item_type: FourCC,
    /// Whether the file has a still primary image item (`pitm` in `meta`).
    pub has_primary_image: bool,
    /// Whether the file contains an image sequence track (`moov`/`trak`).
    pub has_sequence: bool,
}

pub(crate) fn classify_file_kind(major_brand: FourCC, has_sequence: bool) -> FileKind {
    if has_sequence || matches!(&major_brand.0, b"hevc" | b"hevx" | b"msf1") {
        FileKind::Sequence
    } else if matches!(&major_brand.0, b"heic" | b"heix") {
        FileKind::Heic
    } else {
        FileKind::Heif
    }
}

/// Reads container dimensions, bit depth, alpha presence and format kind without decoding
/// pixel data (supports still images and sequence-only `.heics` / MP4 files).
pub fn read_info(bytes: &[u8]) -> Result<ImageInfo, Error> {
    match HeifFile::parse(bytes) {
        Ok(file) => {
            let mut info = item_info(&file, file.primary_id)?;
            let has_sequence = sequence::has_sequence_track(bytes);
            info.has_sequence = has_sequence;
            info.kind = classify_file_kind(info.major_brand, has_sequence);
            Ok(info)
        }
        Err(parse_err) => match read_sequence_only_info(bytes) {
            Ok(Some(info)) => Ok(info),
            Ok(None) => Err(parse_err.into()),
            Err(err) => Err(err),
        },
    }
}

/// Like [`read_info`], requiring a still primary image item (skipping sequence track lookup).
#[cfg(feature = "image")]
pub(crate) fn read_still_info(bytes: &[u8]) -> Result<ImageInfo, Error> {
    let file = HeifFile::parse(bytes)?;
    item_info(&file, file.primary_id)
}

/// Reads container info from a parsed sequence track when no `meta` still image exists.
pub(crate) fn read_sequence_only_info(bytes: &[u8]) -> Result<Option<ImageInfo>, Error> {
    let mut file_type = None;
    for b in heifer_isobmff::BoxIter::new(bytes) {
        let b = b?;
        if &b.header.box_type.0 == b"ftyp" {
            file_type = Some(heifer_isobmff::boxes::FileType::parse(b.content)?);
            break;
        }
    }
    let file_type = file_type.ok_or(heifer_isobmff::Error::MissingBox(FourCC(*b"ftyp")))?;
    let Some(summary) = sequence::sequence_summary(bytes)? else {
        return Ok(None);
    };
    Ok(Some(ImageInfo {
        width: summary.width,
        height: summary.height,
        bit_depth: summary.bit_depth,
        has_alpha: summary.has_alpha,
        kind: FileKind::Sequence,
        major_brand: file_type.major_brand,
        item_type: summary.coding,
        has_primary_image: false,
        has_sequence: true,
    }))
}

/// Reads lightweight info for one still image item without decoding its pixel payload.
pub fn item_info(file: &HeifFile<'_>, id: ItemId) -> Result<ImageInfo, Error> {
    let item = file.item(id)?;
    let (width, height, bit_depth, has_alpha) = item_core_info(file, id, 0)?;
    Ok(ImageInfo {
        width,
        height,
        bit_depth,
        has_alpha,
        kind: classify_file_kind(file.file_type.major_brand, false),
        major_brand: file.file_type.major_brand,
        item_type: item.item_type,
        has_primary_image: true,
        has_sequence: false,
    })
}

fn sps_from_config(
    config: &heifer_isobmff::boxes::HevcConfig,
) -> Result<Option<(u32, u32, u8)>, Error> {
    let mut sets = ParameterSets::default();
    for nal in &config.nal_units {
        sets.add(&NalUnit::parse(nal)?)?;
    }
    let sps = sets
        .pps
        .iter()
        .flatten()
        .find_map(|pps| sets.sps[usize::from(pps.pps_seq_parameter_set_id)].as_ref())
        .or_else(|| sets.sps.iter().flatten().next());
    Ok(sps.map(|sps| {
        let (w, h) = sps.output_size();
        (w, h, sps.bit_depth_luma.max(sps.bit_depth_chroma))
    }))
}

pub(crate) fn hevc_config_bit_depth(
    config: &heifer_isobmff::boxes::HevcConfig,
) -> Result<Option<u8>, Error> {
    if let Some((_, _, bit_depth)) = sps_from_config(config)? {
        return Ok(Some(bit_depth));
    }
    let bit_depth = config.bit_depth_luma.max(config.bit_depth_chroma);
    Ok((bit_depth > 0).then_some(bit_depth))
}

fn has_alpha_aux(file: &HeifFile<'_>, id: ItemId) -> Result<bool, Error> {
    for aux in file.referencing_items(id, b"auxl") {
        if is_alpha_item(file, aux)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn item_has_alpha(file: &HeifFile<'_>, id: ItemId, depth: u32) -> Result<bool, Error> {
    if depth > MAX_DEPTH {
        return Err(Error::Invalid("derived images nested too deeply"));
    }
    let item = file.item(id)?;
    if has_alpha_aux(file, id)? {
        return Ok(true);
    }
    match &item.item_type.0 {
        b"hvc1" => Ok(false),
        b"unci" => Ok(uncompressed::info(file, id)?.3),
        b"grid" => {
            for &tile in file.referenced_items(id, b"dimg") {
                if item_has_alpha(file, tile, depth + 1)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        b"iovl" => {
            let data = file.item_data(id)?;
            let inputs = file.referenced_items(id, b"dimg");
            Ok(parse_iovl_header(&data, inputs.len())?.fill[3] != 65535)
        }
        b"iden" => match file.referenced_items(id, b"dimg") {
            [source] => item_has_alpha(file, *source, depth + 1),
            _ => Err(Error::Invalid("iden item without exactly one source")),
        },
        _ => Err(Error::Unsupported(format!(
            "image item type `{}`",
            item.item_type
        ))),
    }
}

fn hvc1_info(file: &HeifFile<'_>, id: ItemId) -> Result<(u32, u32, u8, bool), Error> {
    let config = file
        .hevc_config(id)?
        .ok_or(heifer_isobmff::Error::MissingBox(FourCC(*b"hvcC")))?;
    let sps = sps_from_config(config)?;
    let (width, height) = match (file.image_size(id)?, sps) {
        // `ispe` is authoritative unless malformed/oversized; decoded pixels never exceed SPS bounds.
        (Some((w, h)), Some((sw, sh, _))) => (w.min(sw), h.min(sh)),
        (Some((w, h)), None) => (w, h),
        (None, Some((sw, sh, _))) => (sw, sh),
        (None, None) => return Err(Error::Invalid("image has no size")),
    };
    if width == 0 || height == 0 {
        return Err(Error::Invalid("image size is zero or too large"));
    }
    let bit_depth = sps
        .map(|(_, _, bd)| bd)
        .unwrap_or_else(|| config.bit_depth_luma.max(config.bit_depth_chroma))
        .max(8);
    let has_alpha = has_alpha_aux(file, id)?;
    Ok((width, height, bit_depth, has_alpha))
}

fn item_core_info(
    file: &HeifFile<'_>,
    id: ItemId,
    depth: u32,
) -> Result<(u32, u32, u8, bool), Error> {
    if depth > MAX_DEPTH {
        return Err(Error::Invalid("derived images nested too deeply"));
    }
    let item = file.item(id)?;
    let (mut w, mut h, bit_depth, mut has_alpha) = match &item.item_type.0 {
        b"hvc1" => hvc1_info(file, id)?,
        b"unci" => uncompressed::info(file, id)?,
        b"grid" => {
            let grid = file.grid(id)?;
            let tiles = file.referenced_items(id, b"dimg");
            if tiles.is_empty() || tiles.len() != usize::from(grid.rows) * usize::from(grid.columns)
            {
                return Err(Error::Invalid(
                    "grid tile count does not match its rows and columns",
                ));
            }
            if grid.output_width == 0 || grid.output_height == 0 {
                return Err(Error::Invalid("image size is zero or too large"));
            }
            let bit_depth = match file.hevc_config(tiles[0])? {
                Some(c) => hevc_config_bit_depth(c)?.unwrap_or(8),
                None => item_core_info(file, tiles[0], depth + 1)?.2,
            };
            let mut tile_alpha = false;
            for &tile in tiles {
                if item_has_alpha(file, tile, depth + 1)? {
                    tile_alpha = true;
                    break;
                }
            }
            (grid.output_width, grid.output_height, bit_depth, tile_alpha)
        }
        b"iovl" => {
            let data = file.item_data(id)?;
            let inputs = file.referenced_items(id, b"dimg");
            let header = parse_iovl_header(&data, inputs.len())?;
            if header.width == 0 || header.height == 0 {
                return Err(Error::Invalid("image size is zero or too large"));
            }
            let mut bit_depth = 8u8;
            for &input in inputs {
                bit_depth = bit_depth.max(item_core_info(file, input, depth + 1)?.2);
            }
            (
                header.width,
                header.height,
                bit_depth,
                header.fill[3] != 65535,
            )
        }
        b"iden" => match file.referenced_items(id, b"dimg")[..] {
            [source] => item_core_info(file, source, depth + 1)?,
            _ => return Err(Error::Invalid("iden item without exactly one source")),
        },
        _ => {
            return Err(Error::Unsupported(format!(
                "image item type `{}`",
                item.item_type
            )));
        }
    };
    if item.item_type.0 != *b"hvc1" {
        has_alpha |= has_alpha_aux(file, id)?;
    }
    for property in file.item_properties(id)? {
        match *property {
            Property::CleanAperture {
                width,
                height,
                horiz_offset,
                vert_offset,
            } => {
                let (_, _, cw, ch) = clap_rect(w, h, width, height, horiz_offset, vert_offset)?;
                w = cw;
                h = ch;
            }
            Property::Rotation(q) if q % 2 == 1 => std::mem::swap(&mut w, &mut h),
            _ => {}
        }
    }
    Ok((w, h, bit_depth, has_alpha))
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
/// This is what a viewer without colour management expects. ICC profiles are read whether they
/// are matrix/TRC or LUT-based (`A2B0`/`A2B1`, of type `lut8`, `lut16` or `lutAtoB`), as long as
/// they describe an RGB input. Files in a colour space heifer cannot convert (another ICC input
/// space, or PQ/HLG transfers, which are HDR and belong to [`decode_hdr`]) are refused with
/// [`Error::Unsupported`].
pub fn decode_srgb(bytes: &[u8]) -> Result<Image, Error> {
    decode_srgb_with_options(bytes, &Options::default())
}

/// Like [`decode_srgb`], with options.
pub fn decode_srgb_with_options(bytes: &[u8], options: &Options) -> Result<Image, Error> {
    let file = HeifFile::parse(bytes)?;
    let id = file.primary_id;
    let image = decode_item_with(&file, id, 0, options)?;
    to_srgb_with_options(&file, id, image, options)
}

/// Like [`decode_srgb`], at 8 bits per sample whatever the coded depth: half the memory, which
/// counts on large photos (see [`decode_rgba8`] for the caveat on 10- and 12-bit files).
pub fn decode_srgb_rgba8(bytes: &[u8], options: &Options) -> Result<Image<u8>, Error> {
    let file = HeifFile::parse(bytes)?;
    let id = file.primary_id;
    let image = decode_item_with(&file, id, 0, options)?;
    to_srgb_with_options(&file, id, image, options)
}

/// Decodes every top-level image of a HEIF/HEIC file, not just the primary one.
///
/// Most files hold a single image, so the result has one entry, equal to [`decode`]. Files that
/// hold several (bursts, "collections", Apple dynamic HEIF) yield one image per picture, skipping
/// their thumbnails, tiles and auxiliary planes; see
/// [`HeifFile::top_level_image_ids`](heifer_isobmff::HeifFile::top_level_image_ids).
pub fn decode_all(bytes: &[u8]) -> Result<Vec<Image>, Error> {
    decode_all_with_options(bytes, &Options::default())
}

/// Like [`decode_all`], with options.
pub fn decode_all_with_options(bytes: &[u8], options: &Options) -> Result<Vec<Image>, Error> {
    let file = HeifFile::parse(bytes)?;
    decode_all_images(&file, options, false)
}

/// Like [`decode_all`], at 8 bits per sample and with the samples converted to sRGB, as
/// [`decode_srgb_rgba8`] does for the primary image. A file in a colour space heifer cannot
/// convert (an ICC profile that is not an RGB input profile, or a PQ/HLG transfer) is refused
/// with [`Error::Unsupported`].
pub fn decode_all_srgb_rgba8(bytes: &[u8], options: &Options) -> Result<Vec<Image<u8>>, Error> {
    let file = HeifFile::parse(bytes)?;
    decode_all_images(&file, options, true)
}

/// An auxiliary image of a HEIF image: a depth map, an alpha plane, a gain map or another
/// refinement stored next to the image and referencing it.
///
/// Obtain the list with [`auxiliary_images`] for the primary image, or
/// [`item_auxiliary_images`] for any image.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuxiliaryImage {
    /// Item ID, to pass to [`decode_auxiliary`].
    pub id: ItemId,
    /// Item type, e.g. `hvc1` or `grid`.
    pub item_type: FourCC,
    /// Auxiliary type (`auxC` URN), e.g. `urn:mpeg:hevc:2015:auxid:2` for a depth map.
    pub urn: String,
    /// Width in pixels from the `ispe` property, `0` when the item has none.
    pub width: u32,
    /// Height in pixels from the `ispe` property, `0` when the item has none.
    pub height: u32,
    /// Bits per sample, from the `pixi` property, else the HEVC configuration, else 8.
    pub bit_depth: u8,
}

impl AuxiliaryImage {
    /// Whether this is an alpha plane.
    pub fn is_alpha(&self) -> bool {
        is_alpha_urn(&self.urn)
    }

    /// Whether this is a depth map.
    pub fn is_depth(&self) -> bool {
        is_depth_urn(&self.urn)
    }

    /// Whether this is an HDR gain map, such as Apple's or Samsung's.
    pub fn is_gain_map(&self) -> bool {
        self.urn == hdr::APPLE_GAIN_MAP || self.urn.ends_with(":aux:hdrgainmap")
    }
}

/// Lists the auxiliary images of the primary image: depth maps of Portrait photos, gain maps,
/// alpha planes and any other refinement.
///
/// The primary image itself is not listed, and neither are thumbnails or grid tiles. Use
/// [`decode_auxiliary`] to decode one of them.
pub fn auxiliary_images(bytes: &[u8]) -> Result<Vec<AuxiliaryImage>, Error> {
    let file = HeifFile::parse(bytes)?;
    item_auxiliary_images(&file, file.primary_id)
}

/// Lists the auxiliary images of one image item (an `auxl` reference), in file order.
pub fn item_auxiliary_images(
    file: &HeifFile<'_>,
    id: ItemId,
) -> Result<Vec<AuxiliaryImage>, Error> {
    let mut images = Vec::new();
    for aux in file.auxiliary_items(id) {
        let item = file.item(aux)?;
        let properties: Vec<&Property> = file.item_properties(aux)?.collect();
        // Without an `auxC` type the item cannot be interpreted: skip it.
        let Some(urn) = properties.iter().find_map(|p| match p {
            Property::AuxiliaryType(urn) => Some(urn.clone()),
            _ => None,
        }) else {
            continue;
        };
        let (width, height) = file.image_size(aux)?.unwrap_or((0, 0));
        let bit_depth = item_bit_depth(file, aux)?.unwrap_or(8);
        images.push(AuxiliaryImage {
            id: aux,
            item_type: item.item_type,
            urn,
            width,
            height,
            bit_depth,
        });
    }
    Ok(images)
}

/// Bits per sample of an item: from its `pixi` property, else its HEVC configuration, else from
/// the first input of a derived image (a grid's first tile).
fn item_bit_depth(file: &HeifFile<'_>, id: ItemId) -> Result<Option<u8>, Error> {
    let mut current = Some(id);
    for _ in 0..MAX_DEPTH {
        let Some(item) = current else { return Ok(None) };
        let properties: Vec<&Property> = file.item_properties(item)?.collect();
        if let Some(bits) = properties.iter().find_map(|p| match p {
            Property::PixelInfo(channels) => channels.first().copied(),
            _ => None,
        }) {
            return Ok(Some(bits));
        }
        if let Some(config) = file.hevc_config(item)? {
            return Ok(Some(config.bit_depth_luma));
        }
        if let Some(config) = file.uncompressed_config(item)? {
            if let Some(component) = config.components.first() {
                return Ok(Some(u8::try_from(component.bit_depth).unwrap_or(8)));
            }
        }
        current = file
            .referenced_items(item, b"dimg")
            .first()
            .copied()
            .filter(|&first| first != item);
    }
    Ok(None)
}

/// Decodes an auxiliary image by item ID, with its own crop, rotation and mirror transforms
/// applied (the transforms of the primary image are not applied on top).
///
/// The result is an RGBA [`Image`] like [`decode`]; monochrome auxiliary images such as depth
/// maps have their sample replicated in the red, green and blue channels.
pub fn decode_auxiliary(bytes: &[u8], id: ItemId) -> Result<Image, Error> {
    decode_auxiliary_with_options(bytes, id, &Options::default())
}

/// Like [`decode_auxiliary`], with options.
pub fn decode_auxiliary_with_options(
    bytes: &[u8],
    id: ItemId,
    options: &Options,
) -> Result<Image, Error> {
    let file = HeifFile::parse(bytes)?;
    decode_item_with(&file, id, 0, options)
}

/// Decodes the auxiliary image of the primary image whose type is `urn`, or `None` when the
/// file has no such auxiliary image.
///
/// For example [`DEPTH_URN`] selects the depth map. When several auxiliary images share a type,
/// the first one is decoded.
pub fn decode_auxiliary_urn(bytes: &[u8], urn: &str) -> Result<Option<Image>, Error> {
    decode_auxiliary_urn_with_options(bytes, urn, &Options::default())
}

/// Like [`decode_auxiliary_urn`], with options.
pub fn decode_auxiliary_urn_with_options(
    bytes: &[u8],
    urn: &str,
    options: &Options,
) -> Result<Option<Image>, Error> {
    let file = HeifFile::parse(bytes)?;
    let Some(aux) = item_auxiliary_images(&file, file.primary_id)?
        .into_iter()
        .find(|aux| aux.urn == urn)
    else {
        return Ok(None);
    };
    Ok(Some(decode_item_with(&file, aux.id, 0, options)?))
}

fn decode_all_images<S: Sample>(
    file: &HeifFile<'_>,
    options: &Options,
    srgb: bool,
) -> Result<Vec<Image<S>>, Error> {
    file.top_level_image_ids()
        .into_iter()
        .map(|id| {
            let image = decode_item_with::<S>(file, id, 0, options)?;
            if srgb {
                to_srgb_with_options(file, id, image, options)
            } else {
                Ok(image)
            }
        })
        .collect()
}

/// Converts a decoded image to sRGB from its ICC profile, else from its `nclx` colour
/// information, else leaves it untouched. When `options.fallback_to_coded_color` is set, an
/// unsupported ICC profile or transfer leaves the decoded image untouched instead of failing.
pub(crate) fn to_srgb_with_options<S: Sample>(
    file: &HeifFile<'_>,
    id: ItemId,
    image: Image<S>,
    options: &Options,
) -> Result<Image<S>, Error> {
    if let Some(icc) = file.icc_profile(id)? {
        return match icc::Profile::parse(icc) {
            Ok(profile) => Ok(profile.to_srgb(&image)),
            Err(Error::Unsupported(_)) if options.fallback_to_coded_color => Ok(image),
            Err(e) => Err(e),
        };
    }
    match nclx(file, id)? {
        Some((primaries, transfer)) => match icc::from_primaries(&image, primaries, transfer) {
            Ok(converted) => Ok(converted),
            Err(Error::Unsupported(_)) if options.fallback_to_coded_color => Ok(image),
            Err(e) => Err(e),
        },
        None => Ok(image),
    }
}

/// The colour primaries and transfer characteristics of an item, or of the first source of a
/// derived image (a grid's tiles), following the `dimg` references: the `nclx` colour
/// information when present, else the HEVC VUI of a coded item.
fn nclx(file: &HeifFile<'_>, id: ItemId) -> Result<Option<(u16, u16)>, Error> {
    let mut current = Some(id);
    for _ in 0..MAX_DEPTH {
        let Some(item) = current else { return Ok(None) };
        if let Some(colour) = item_nclx(file, item)? {
            return Ok(Some(colour));
        }
        if let Some(colour) = vui_colour(file, item)? {
            return Ok(Some(colour));
        }
        current = file
            .referenced_items(item, b"dimg")
            .first()
            .copied()
            .filter(|&first| first != item);
    }
    Ok(None)
}

/// The `nclx` primaries and transfer characteristics of an item, if it has a `colr` property.
fn item_nclx(file: &HeifFile<'_>, id: ItemId) -> Result<Option<(u16, u16)>, Error> {
    Ok(file.item_properties(id)?.find_map(|p| match p {
        Property::Color(ColorInfo::Nclx {
            primaries,
            transfer,
            ..
        }) => Some((*primaries, *transfer)),
        _ => None,
    }))
}

/// The colour primaries and transfer characteristics declared by the HEVC VUI of a coded item,
/// when both are specified (not 2, "unspecified"). Some encoders write them only there, e.g.
/// for HDR HEIC files that have no `nclx` property.
fn vui_colour(file: &HeifFile<'_>, id: ItemId) -> Result<Option<(u16, u16)>, Error> {
    let item_type = file.item(id)?.item_type;
    if item_type != FourCC(*b"hvc1") && item_type != FourCC(*b"hev1") {
        return Ok(None);
    }
    let Some(config) = file.hevc_config(id)? else {
        return Ok(None);
    };
    // Parameter sets are normally in `hvcC`; `hev1` streams may also carry them in-band, in
    // which case the reconstructed Annex B stream is the only place to find the SPS.
    let mut sets = ParameterSets::default();
    for nal in &config.nal_units {
        sets.add(&NalUnit::parse(nal)?)?;
    }
    if let Some(colour) = sps_colour(&sets) {
        return Ok(Some(colour));
    }
    if item_type == FourCC(*b"hvc1") {
        return Ok(None);
    }
    let mut sets = ParameterSets::default();
    for nal in split_annex_b(&file.hevc_bitstream(id)?) {
        sets.add(&NalUnit::parse(nal)?)?;
    }
    Ok(sps_colour(&sets))
}

/// The colour primaries and transfer characteristics declared by the VUI of a parsed SPS, when
/// both are specified.
fn sps_colour(sets: &ParameterSets) -> Option<(u16, u16)> {
    sets.sps.iter().flatten().find_map(|sps| {
        let vui = sps.vui.as_ref()?;
        (vui.colour_primaries != 2 && vui.transfer_characteristics != 2).then_some((
            u16::from(vui.colour_primaries),
            u16::from(vui.transfer_characteristics),
        ))
    })
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
    let mut alpha_id = None;
    if item.item_type.0 != *b"hvc1" {
        for aux in file.referencing_items(id, b"auxl") {
            if is_alpha_item(file, aux)? {
                alpha_id = Some(aux);
                break;
            }
        }
    }
    let mut fused_transforms = false;
    let mut image = match &item.item_type.0 {
        b"hvc1" => decode_hvc1(file, id, options)?,
        b"unci" => uncompressed::decode(file, id, options)?,
        b"grid" => {
            if alpha_id.is_none() {
                if let Some((crop, xform)) = fusable_grid_transforms(file, id)? {
                    if !xform.is_identity() || crop.is_some() {
                        let (img, applied) =
                            decode_grid_with_transform(file, id, depth, options, crop, xform)?;
                        fused_transforms = applied;
                        img
                    } else {
                        decode_grid_with_transform(
                            file,
                            id,
                            depth,
                            options,
                            None,
                            GridTransform::default(),
                        )?
                        .0
                    }
                } else {
                    decode_grid_with_transform(
                        file,
                        id,
                        depth,
                        options,
                        None,
                        GridTransform::default(),
                    )?
                    .0
                }
            } else {
                decode_grid_with_transform(
                    file,
                    id,
                    depth,
                    options,
                    None,
                    GridTransform::default(),
                )?
                .0
            }
        }
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
    if let Some(aux) = alpha_id {
        attach_alpha_image(
            &mut image,
            &decode_item_with(file, aux, depth + 1, options)?,
        )?;
    }
    if fused_transforms {
        return Ok(image);
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
fn color_params_view(
    file: &HeifFile<'_>,
    id: ItemId,
    view: &DecodedFrameView<'_>,
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
        .unwrap_or_else(|| ColorParams::from_view(view)))
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
    let mut bitstream = Vec::new();
    let mut workspace = DecodeWorkspace::default();
    let predicted;
    let mut image = {
        let view = match file.referenced_items(id, b"pred") {
            [] => {
                file.hevc_bitstream_into(id, &mut bitstream)?;
                decode_picture_view_into(&bitstream, hevc_options, &mut workspace)?.0
            }
            refs => {
                predicted = decode_predicted(file, refs, id, hevc_options, &mut bitstream)?;
                let crop = CropWindow::full(predicted.widths, predicted.heights);
                DecodedFrameView::U16(&predicted, crop)
            }
        };
        // `ispe` is authoritative; HEVC may code a slightly larger picture.
        let (width, height) = match file.image_size(id)? {
            Some((w, h)) => (w.min(view.width()), h.min(view.height())),
            None => (view.width(), view.height()),
        };
        check_size(width, height, options.max_pixels())?;
        let params = color_params_view(file, id, &view)?;
        frame_view_to_rgba_sized(&view, params, width, height)
    };

    // Alpha channel: an auxiliary image of type alpha referencing this item.
    for aux in file.referencing_items(id, b"auxl") {
        if !is_alpha_item(file, aux)? {
            continue;
        }
        // An uncompressed alpha plane is samples, not an HEVC bitstream.
        if file.item(aux)?.item_type.0 == *b"unci" {
            let alpha: Image<S> = uncompressed::decode(file, aux, options)?;
            attach_alpha_image(&mut image, &alpha)?;
            break;
        }
        file.hevc_bitstream_into(aux, &mut bitstream)?;
        let (alpha, _) = decode_picture_view_into(&bitstream, hevc_options, &mut workspace)?;
        // The alpha plane is the luma of the auxiliary image, with its own range.
        let range = color_params_view(file, aux, &alpha)?;
        attach_alpha_view(&mut image, &alpha, range.full_range)?;
        break;
    }
    Ok(image)
}

/// A predictively coded image item (`pred` references, ISO/IEC 23008-12 §6.4.12): the items it
/// references, listed in decoding order, then the item itself, decoded as one HEVC sequence.
fn decode_predicted(
    file: &HeifFile<'_>,
    refs: &[ItemId],
    id: ItemId,
    options: DecodeOptions,
    bitstream: &mut Vec<u8>,
) -> Result<Frame, Error> {
    let mut decoder = SequenceDecoder::new(options);
    for &item in refs {
        file.hevc_bitstream_into(item, bitstream)?;
        decoder.decode(bitstream)?;
    }
    file.hevc_bitstream_into(id, bitstream)?;
    Ok(decoder.decode(bitstream)?)
}

/// Whether an `auxC` / `auxi` type denotes an alpha plane.
pub(crate) fn is_alpha_urn(urn: &str) -> bool {
    matches!(
        urn,
        "urn:mpeg:hevc:2015:auxid:1" | "urn:mpeg:mpegB:cicp:systems:auxiliary:alpha"
    )
}

/// Whether an `auxC` type denotes a depth map.
fn is_depth_urn(urn: &str) -> bool {
    matches!(
        urn,
        "urn:mpeg:hevc:2015:auxid:2" | "urn:mpeg:mpegB:cicp:systems:auxiliary:depth"
    )
}

/// Whether an item is an auxiliary image of type alpha.
fn is_alpha_item(file: &HeifFile<'_>, id: ItemId) -> Result<bool, Error> {
    Ok(file
        .item_properties(id)?
        .any(|p| matches!(p, Property::AuxiliaryType(t) if is_alpha_urn(t))))
}

/// Attaches the luma plane of a decoded auxiliary `alpha` frame to `image`.
pub(crate) fn attach_alpha_frame<S: Sample>(
    image: &mut Image<S>,
    alpha: &Frame,
    full_range: bool,
) -> Result<(), Error> {
    let view = DecodedFrameView::U16(alpha, alpha.full_window());
    attach_alpha_view(image, &view, full_range)
}

fn copy_alpha_plane<P: Copy + Into<f64>, S: Sample>(
    image: &mut Image<S>,
    plane: &[P],
    stride: usize,
    crop: heifer_hevc_dec::decoder::CropWindow,
    bd: u8,
    full_range: bool,
) {
    let (offset, span) = if full_range {
        (0.0, f64::from((1u32 << bd) - 1))
    } else {
        (f64::from(16u32 << (bd - 8)), f64::from(219u32 << (bd - 8)))
    };
    let max = f64::from(image.max_value());
    let img_w = image.width as usize;
    let img_h = image.height as usize;
    let (ax0, ay0) = (crop.x[0] as usize, crop.y[0] as usize);
    for y in 0..img_h {
        let src_row = &plane[(ay0 + y) * stride + ax0..];
        for x in 0..img_w {
            let a = src_row[x].into();
            let a = ((a - offset) / span * max).round().clamp(0.0, max);
            image.data[(y * img_w + x) * 4 + 3] = S::from_u16(a as u16);
        }
    }
}

/// Attaches the luma plane of a decoded auxiliary `alpha` view to `image`.
pub(crate) fn attach_alpha_view<S: Sample>(
    image: &mut Image<S>,
    alpha: &DecodedFrameView<'_>,
    full_range: bool,
) -> Result<(), Error> {
    let crop = alpha.crop();
    // The alpha plane must cover the image: anything smaller would be read out of bounds.
    if crop.widths[0] < image.width || crop.heights[0] < image.height {
        return Err(Error::Unsupported(
            "alpha plane smaller than the image".into(),
        ));
    }
    let bd = alpha.bit_depth()[0];
    match *alpha {
        DecodedFrameView::U8(f, _) => {
            copy_alpha_plane(
                image,
                &f.planes[0],
                f.widths[0] as usize,
                crop,
                bd,
                full_range,
            );
        }
        DecodedFrameView::U16(f, _) => {
            copy_alpha_plane(
                image,
                &f.planes[0],
                f.widths[0] as usize,
                crop,
                bd,
                full_range,
            );
        }
    }
    image.has_alpha = true;
    Ok(())
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

pub(crate) fn check_size(w: u32, h: u32, max_pixels: u64) -> Result<(), Error> {
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > max_pixels {
        return Err(Error::Invalid("image size is zero or too large"));
    }
    Ok(())
}

/// Any composition of 90° rotations and reflections (`D4`) applied as flip-then-swap from
/// source `(sx, sy)` to destination `(dx, dy)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct GridTransform {
    pub(crate) swap_xy: bool,
    pub(crate) flip_x: bool,
    pub(crate) flip_y: bool,
}

impl GridTransform {
    pub(crate) fn is_identity(self) -> bool {
        !self.swap_xy && !self.flip_x && !self.flip_y
    }

    fn then_rotate_ccw(&mut self, quarters: u8) {
        for _ in 0..quarters % 4 {
            if !self.swap_xy {
                self.swap_xy = true;
                self.flip_x = !self.flip_x;
            } else {
                self.swap_xy = false;
                self.flip_y = !self.flip_y;
            }
        }
    }

    fn then_mirror(&mut self, top_bottom: bool) {
        if top_bottom ^ self.swap_xy {
            self.flip_y = !self.flip_y;
        } else {
            self.flip_x = !self.flip_x;
        }
    }

    pub(crate) fn output_size(self, w: u32, h: u32) -> (u32, u32) {
        if self.swap_xy { (h, w) } else { (w, h) }
    }

    pub(crate) fn map_point(self, cx: u32, cy: u32, crop_w: u32, crop_h: u32) -> (u32, u32) {
        let fx = if self.flip_x { crop_w - 1 - cx } else { cx };
        let fy = if self.flip_y { crop_h - 1 - cy } else { cy };
        if self.swap_xy { (fy, fx) } else { (fx, fy) }
    }
}

/// Leading `clap` rectangle (before any rotation/mirror) followed by composed `irot`/`imir`
/// transforms for a grid item, or `None` if a `clap` follows a non-identity rotation/mirror.
#[allow(clippy::type_complexity)]
pub(crate) fn fusable_grid_transforms(
    file: &HeifFile<'_>,
    id: ItemId,
) -> Result<Option<(Option<(u32, u32, u32, u32)>, GridTransform)>, Error> {
    let grid = file.grid(id)?;
    let (mut cw, mut ch) = (grid.output_width, grid.output_height);
    let mut crop: Option<(u32, u32, u32, u32)> = None;
    let mut xform = GridTransform::default();
    for property in file.item_properties(id)? {
        match *property {
            Property::CleanAperture {
                width,
                height,
                horiz_offset,
                vert_offset,
            } => {
                if !xform.is_identity() {
                    return Ok(None);
                }
                let (x, y, w, h) = clap_rect(cw, ch, width, height, horiz_offset, vert_offset)?;
                crop = Some(match crop {
                    Some((cx, cy, _, _)) => (cx + x, cy + y, w, h),
                    None => (x, y, w, h),
                });
                cw = w;
                ch = h;
            }
            Property::Rotation(q) => xform.then_rotate_ccw(q),
            Property::Mirror(top_bottom) => xform.then_mirror(top_bottom),
            _ => {}
        }
    }
    Ok(Some((crop, xform)))
}

/// Reusable per-worker decode buffers for plain grid tiles (`48`–`768` tiles per photo).
pub(crate) struct GridWorkspace<S> {
    pub(crate) bitstream: Vec<u8>,
    pub(crate) hevc: DecodeWorkspace,
    pub(crate) color: ColorScratch,
    pub(crate) tile_rgba: Vec<S>,
}

impl<S> Default for GridWorkspace<S> {
    fn default() -> Self {
        Self {
            bitstream: Vec::new(),
            hevc: DecodeWorkspace::default(),
            color: ColorScratch::default(),
            tile_rgba: Vec::new(),
        }
    }
}

/// Writes an `h × w` sub-rectangle at `(tx0, ty0)` from `src` (row stride `src_w * 4`) into `dst`
/// (`dw × dh`, row stride `dst_stride`) applying `xform` in 32×32 cache blocks.
#[allow(clippy::too_many_arguments)]
fn copy_tile_transformed<S: Copy>(
    src: &[S],
    src_w: usize,
    tx0: usize,
    ty0: usize,
    w: usize,
    h: usize,
    dst: &mut [S],
    dst_stride: usize,
    dx0: usize,
    dy0: usize,
    xform: GridTransform,
) {
    let (dw, dh) = if xform.swap_xy { (h, w) } else { (w, h) };
    let src_stride = src_w * 4;
    if !xform.swap_xy {
        if !xform.flip_x {
            for ldy in 0..dh {
                let sy = ty0 + if xform.flip_y { dh - 1 - ldy } else { ldy };
                let s = sy * src_stride + tx0 * 4;
                let d = (dy0 + ldy) * dst_stride + dx0 * 4;
                dst[d..d + dw * 4].copy_from_slice(&src[s..s + dw * 4]);
            }
            return;
        }
        for ldy in 0..dh {
            let sy = ty0 + if xform.flip_y { dh - 1 - ldy } else { ldy };
            let src_row = &src[sy * src_stride + tx0 * 4..sy * src_stride + (tx0 + dw) * 4];
            let dst_row = &mut dst
                [(dy0 + ldy) * dst_stride + dx0 * 4..(dy0 + ldy) * dst_stride + (dx0 + dw) * 4];
            for ldx in 0..dw {
                let s = (dw - 1 - ldx) * 4;
                let d = ldx * 4;
                dst_row[d..d + 4].copy_from_slice(&src_row[s..s + 4]);
            }
        }
        return;
    }

    const BLOCK: usize = 32;
    for by in (0..dh).step_by(BLOCK) {
        let y_end = (by + BLOCK).min(dh);
        for bx in (0..dw).step_by(BLOCK) {
            let x_end = (bx + BLOCK).min(dw);
            for ldy in by..y_end {
                let sx = tx0 + if xform.flip_x { dh - 1 - ldy } else { ldy };
                let dst_row = &mut dst[(dy0 + ldy) * dst_stride + dx0 * 4..];
                for ldx in bx..x_end {
                    let sy = ty0 + if xform.flip_y { dw - 1 - ldx } else { ldx };
                    let s = sy * src_stride + sx * 4;
                    let d = ldx * 4;
                    dst_row[d..d + 4].copy_from_slice(&src[s..s + 4]);
                }
            }
        }
    }
}

/// Like [`copy_tile_transformed`], targeting disjoint row segments owned by a single tile.
#[allow(clippy::too_many_arguments)]
fn copy_tile_transformed_segments<S: Copy>(
    src: &[S],
    src_w: usize,
    tx0: usize,
    ty0: usize,
    w: usize,
    h: usize,
    dst_rows: &mut [&mut [S]],
    xform: GridTransform,
) {
    let (dw, dh) = if xform.swap_xy { (h, w) } else { (w, h) };
    let src_stride = src_w * 4;
    if !xform.swap_xy {
        if !xform.flip_x {
            for (ldy, dst_row) in dst_rows.iter_mut().enumerate().take(dh) {
                let sy = ty0 + if xform.flip_y { dh - 1 - ldy } else { ldy };
                let s = sy * src_stride + tx0 * 4;
                dst_row[..dw * 4].copy_from_slice(&src[s..s + dw * 4]);
            }
            return;
        }
        for (ldy, dst_row) in dst_rows.iter_mut().enumerate().take(dh) {
            let sy = ty0 + if xform.flip_y { dh - 1 - ldy } else { ldy };
            let src_row = &src[sy * src_stride + tx0 * 4..sy * src_stride + (tx0 + dw) * 4];
            for ldx in 0..dw {
                let s = (dw - 1 - ldx) * 4;
                let d = ldx * 4;
                dst_row[d..d + 4].copy_from_slice(&src_row[s..s + 4]);
            }
        }
        return;
    }

    const BLOCK: usize = 32;
    for by in (0..dh).step_by(BLOCK) {
        let y_end = (by + BLOCK).min(dh);
        for bx in (0..dw).step_by(BLOCK) {
            let x_end = (bx + BLOCK).min(dw);
            for (dy, dst_row) in dst_rows[by..y_end].iter_mut().enumerate() {
                let ldy = by + dy;
                let sx = tx0 + if xform.flip_x { dh - 1 - ldy } else { ldy };
                for ldx in bx..x_end {
                    let sy = ty0 + if xform.flip_y { dw - 1 - ldx } else { ldx };
                    let s = sy * src_stride + sx * 4;
                    let d = ldx * 4;
                    dst_row[d..d + 4].copy_from_slice(&src[s..s + 4]);
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_plain_tile<'w>(
    file: &HeifFile<'_>,
    tile_id: ItemId,
    tw: u32,
    th: u32,
    bit_depth: u8,
    sample_depth: fn(u8) -> u8,
    hevc_options: DecodeOptions,
    bitstream: &mut Vec<u8>,
    hevc: &'w mut DecodeWorkspace,
) -> Result<(DecodedFrameView<'w>, ColorParams), Error> {
    file.hevc_bitstream_into(tile_id, bitstream)?;
    let (view, _) = decode_picture_view_into(bitstream, hevc_options, hevc)?;
    if view.width() < tw
        || view.height() < th
        || sample_depth(view.bit_depth()[0].max(view.bit_depth()[1])) != bit_depth
    {
        return Err(Error::Invalid(
            "grid tiles have different sizes or bit depths",
        ));
    }
    let params = color_params_view(file, tile_id, &view)?;
    Ok((view, params))
}

#[allow(clippy::too_many_arguments)]
fn decode_grid_with_transform<S: Sample>(
    file: &HeifFile<'_>,
    id: ItemId,
    depth: u32,
    options: &Options,
    crop: Option<(u32, u32, u32, u32)>,
    xform: GridTransform,
) -> Result<(Image<S>, bool), Error> {
    let grid = file.grid(id)?;
    let tiles = file.referenced_items(id, b"dimg");
    let columns = u32::from(grid.columns);
    let rows = u32::from(grid.rows);
    if tiles.is_empty() || tiles.len() != rows as usize * columns as usize {
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
    // Plain tiles are converted straight into the canvas, without an intermediate image.
    let plain = first_tile.is_none()
        && tiles
            .iter()
            .all(|&t| is_plain_coded_image(file, t).unwrap_or(false));
    let tiles_cover_grid = u64::from(columns) * u64::from(tw) >= u64::from(grid.output_width)
        && u64::from(rows) * u64::from(th) >= u64::from(grid.output_height);
    let apply_xform = plain && tiles_cover_grid && (!xform.is_identity() || crop.is_some());
    if apply_xform {
        let (cx, cy, cw, ch) = crop.unwrap_or((0, 0, grid.output_width, grid.output_height));
        let (final_w, final_h) = xform.output_size(cw, ch);
        if final_w == 0 || final_h == 0 {
            return Ok((
                Image::filled_samples(final_w, final_h, bit_depth, [S::default(); 4]),
                true,
            ));
        }
        let mut canvas = Image::filled_samples(final_w, final_h, bit_depth, [S::default(); 4]);
        let stride = final_w as usize * 4;

        let active_cols: Vec<(u32, usize, usize, usize)> = (0..columns)
            .filter_map(|c| {
                let x0 = c * tw;
                let ix0 = x0.max(cx);
                let ix1 = (x0 + tw).min(grid.output_width).min(cx + cw);
                (ix0 < ix1).then(|| {
                    (
                        c,
                        (ix0 - x0) as usize,
                        (ix0 - cx) as usize,
                        (ix1 - ix0) as usize,
                    )
                })
            })
            .collect();
        let active_rows: Vec<(u32, usize, usize, usize)> = (0..rows)
            .filter_map(|r| {
                let y0 = r * th;
                let iy0 = y0.max(cy);
                let iy1 = (y0 + th).min(grid.output_height).min(cy + ch);
                (iy0 < iy1).then(|| {
                    (
                        r,
                        (iy0 - y0) as usize,
                        (iy0 - cy) as usize,
                        (iy1 - iy0) as usize,
                    )
                })
            })
            .collect();
        let (n_c, n_r) = (active_cols.len(), active_rows.len());
        struct TileJob {
            tile_id: ItemId,
            tx0: usize,
            ty0: usize,
            w: usize,
            h: usize,
            dx0: usize,
            dy0: usize,
            slot: usize,
        }
        let mut jobs = Vec::with_capacity(n_c * n_r);
        let n_xb = if xform.swap_xy { n_r } else { n_c };
        for (r_idx, &(r, ty0, sy0, h)) in active_rows.iter().enumerate() {
            let py_idx = if xform.flip_y { n_r - 1 - r_idx } else { r_idx };
            let ry0 = if xform.flip_y {
                ch as usize - sy0 - h
            } else {
                sy0
            };
            for (c_idx, &(c, tx0, sx0, w)) in active_cols.iter().enumerate() {
                let px_idx = if xform.flip_x { n_c - 1 - c_idx } else { c_idx };
                let rx0 = if xform.flip_x {
                    cw as usize - sx0 - w
                } else {
                    sx0
                };
                let (dst_xb, dst_yb, dx0, dy0) = if xform.swap_xy {
                    (py_idx, px_idx, ry0, rx0)
                } else {
                    (px_idx, py_idx, rx0, ry0)
                };
                jobs.push(TileJob {
                    tile_id: tiles[r as usize * columns as usize + c as usize],
                    tx0,
                    ty0,
                    w,
                    h,
                    dx0,
                    dy0,
                    slot: dst_yb * n_xb + dst_xb,
                });
            }
        }
        let hevc_options = DecodeOptions {
            max_pixels: options.max_pixels(),
            ..Default::default()
        };
        let threads = options.threads().min(jobs.len());
        if threads <= 1 {
            let mut ws = GridWorkspace::<S>::default();
            for job in &jobs {
                let (view, params) = decode_plain_tile(
                    file,
                    job.tile_id,
                    tw,
                    th,
                    bit_depth,
                    S::depth,
                    hevc_options,
                    &mut ws.bitstream,
                    &mut ws.hevc,
                )?;
                if xform.is_identity() && job.tx0 == 0 && job.ty0 == 0 {
                    frame_view_to_rgba_strided_with_scratch(
                        &view,
                        params,
                        &mut canvas.data,
                        stride,
                        job.dx0,
                        job.dy0,
                        job.w,
                        job.h,
                        &mut ws.color,
                    );
                } else {
                    let src_w = job.tx0 + job.w;
                    let src_h = job.ty0 + job.h;
                    ws.tile_rgba.resize(src_w * src_h * 4, S::default());
                    frame_view_to_rgba_strided_with_scratch(
                        &view,
                        params,
                        &mut ws.tile_rgba,
                        src_w * 4,
                        0,
                        0,
                        src_w,
                        src_h,
                        &mut ws.color,
                    );
                    copy_tile_transformed(
                        &ws.tile_rgba,
                        src_w,
                        job.tx0,
                        job.ty0,
                        job.w,
                        job.h,
                        &mut canvas.data,
                        stride,
                        job.dx0,
                        job.dy0,
                        xform,
                    );
                }
            }
            return Ok((canvas, true));
        }

        let col_widths: Vec<usize> = if xform.swap_xy {
            if xform.flip_y {
                active_rows.iter().rev().map(|t| t.3).collect()
            } else {
                active_rows.iter().map(|t| t.3).collect()
            }
        } else if xform.flip_x {
            active_cols.iter().rev().map(|t| t.3).collect()
        } else {
            active_cols.iter().map(|t| t.3).collect()
        };
        let row_heights: Vec<usize> = if xform.swap_xy {
            if xform.flip_x {
                active_cols.iter().rev().map(|t| t.3).collect()
            } else {
                active_cols.iter().map(|t| t.3).collect()
            }
        } else if xform.flip_y {
            active_rows.iter().rev().map(|t| t.3).collect()
        } else {
            active_rows.iter().map(|t| t.3).collect()
        };
        let mut segments: Vec<Vec<&mut [S]>> = (0..jobs.len())
            .map(|slot| Vec::with_capacity(row_heights[slot / n_xb]))
            .collect();
        let mut canvas_rows = canvas.data.chunks_exact_mut(stride);
        for (dst_yb, &band_h) in row_heights.iter().enumerate() {
            let base_slot = dst_yb * n_xb;
            for _ in 0..band_h {
                let mut rest = canvas_rows
                    .next()
                    .expect("row heights sum to canvas height");
                for (dst_xb, &band_w) in col_widths.iter().enumerate() {
                    let (seg, tail) = rest.split_at_mut(band_w * 4);
                    rest = tail;
                    segments[base_slot + dst_xb].push(seg);
                }
            }
        }
        let segments: Vec<std::sync::Mutex<Vec<&mut [S]>>> =
            segments.into_iter().map(std::sync::Mutex::new).collect();
        let next = std::sync::atomic::AtomicUsize::new(0);
        let error = std::sync::Mutex::new(None);
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    let mut ws = GridWorkspace::<S>::default();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if i >= jobs.len() {
                            break;
                        }
                        let job = &jobs[i];
                        let step = (|| -> Result<(), Error> {
                            let (view, params) = decode_plain_tile(
                                file,
                                job.tile_id,
                                tw,
                                th,
                                bit_depth,
                                S::depth,
                                hevc_options,
                                &mut ws.bitstream,
                                &mut ws.hevc,
                            )?;
                            if xform.is_identity() && job.tx0 == 0 && job.ty0 == 0 {
                                let mut out =
                                    segments[job.slot].lock().unwrap_or_else(|e| e.into_inner());
                                frame_view_to_rgba_into_with_scratch(
                                    &view,
                                    params,
                                    &mut out,
                                    job.w,
                                    job.h,
                                    &mut ws.color,
                                );
                            } else {
                                let src_w = job.tx0 + job.w;
                                let src_h = job.ty0 + job.h;
                                ws.tile_rgba.resize(src_w * src_h * 4, S::default());
                                frame_view_to_rgba_strided_with_scratch(
                                    &view,
                                    params,
                                    &mut ws.tile_rgba,
                                    src_w * 4,
                                    0,
                                    0,
                                    src_w,
                                    src_h,
                                    &mut ws.color,
                                );
                                let mut out =
                                    segments[job.slot].lock().unwrap_or_else(|e| e.into_inner());
                                copy_tile_transformed_segments(
                                    &ws.tile_rgba,
                                    src_w,
                                    job.tx0,
                                    job.ty0,
                                    job.w,
                                    job.h,
                                    &mut out,
                                    xform,
                                );
                            }
                            Ok(())
                        })();
                        if let Err(e) = step {
                            error
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .get_or_insert(e);
                            next.store(jobs.len(), std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        drop(segments);
        if let Some(e) = error.into_inner().unwrap_or_else(|e| e.into_inner()) {
            return Err(e);
        }
        return Ok((canvas, true));
    }

    let mut canvas = Image::filled_samples(
        grid.output_width,
        grid.output_height,
        bit_depth,
        [S::default(); 4],
    );
    let (canvas_w, canvas_h) = (canvas.width, canvas.height);
    let stride = canvas_w as usize * 4;
    let start = usize::from(first_tile.is_some());
    let threads = options.threads().min(tiles.len() - start);
    if threads <= 1 && plain && first_tile.is_none() {
        let mut ws = GridWorkspace::<S>::default();
        let hevc_options = DecodeOptions {
            max_pixels: options.max_pixels(),
            ..Default::default()
        };
        for (i, &tile_id) in tiles.iter().enumerate() {
            let x0 =
                (u64::from(i as u32 % columns) * u64::from(tw)).min(u64::from(u32::MAX)) as u32;
            let y0 =
                (u64::from(i as u32 / columns) * u64::from(th)).min(u64::from(u32::MAX)) as u32;
            let (w, h) = (
                tw.min(canvas_w.saturating_sub(x0)) as usize,
                th.min(canvas_h.saturating_sub(y0)) as usize,
            );
            let (view, params) = decode_plain_tile(
                file,
                tile_id,
                tw,
                th,
                bit_depth,
                S::depth,
                hevc_options,
                &mut ws.bitstream,
                &mut ws.hevc,
            )?;
            frame_view_to_rgba_strided_with_scratch(
                &view,
                params,
                &mut canvas.data,
                stride,
                x0 as usize,
                y0 as usize,
                w,
                h,
                &mut ws.color,
            );
        }
        return Ok((canvas, false));
    }

    let tile_stride = tw.min(canvas_w) as usize * 4;
    let tile_rows = (th as usize).min(canvas_h as usize);
    // Each tile gets its own row segments of the canvas, so tiles are written concurrently.
    let mut segments: Vec<Vec<&mut [S]>> = (0..tiles.len())
        .map(|_| Vec::with_capacity(tile_rows))
        .collect();
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
    let place = |i: usize,
                 tile: Option<&Image<S>>,
                 ws: Option<&mut GridWorkspace<S>>|
     -> Result<(), Error> {
        let x0 = (u64::from(i as u32 % columns) * u64::from(tw)).min(u64::from(u32::MAX)) as u32;
        let y0 = (u64::from(i as u32 / columns) * u64::from(th)).min(u64::from(u32::MAX)) as u32;
        let (w, h) = (
            tw.min(canvas_w.saturating_sub(x0)) as usize,
            th.min(canvas_h.saturating_sub(y0)) as usize,
        );
        if tile.is_none() && plain {
            let mut fallback = None;
            let ws = match ws {
                Some(ws) => ws,
                None => fallback.get_or_insert_with(GridWorkspace::<S>::default),
            };
            let hevc_options = DecodeOptions {
                max_pixels: options.max_pixels(),
                ..Default::default()
            };
            let (view, params) = decode_plain_tile(
                file,
                tiles[i],
                tw,
                th,
                bit_depth,
                S::depth,
                hevc_options,
                &mut ws.bitstream,
                &mut ws.hevc,
            )?;
            let mut out = segments[i].lock().unwrap_or_else(|e| e.into_inner());
            frame_view_to_rgba_into_with_scratch(&view, params, &mut out, w, h, &mut ws.color);
            return Ok(());
        }
        let tile_storage;
        let tile = match tile {
            Some(t) => t,
            None => {
                tile_storage = decode_item_with(file, tiles[i], depth + 1, options)?;
                &tile_storage
            }
        };
        if (tile.width, tile.height, tile.bit_depth) != (tw, th, bit_depth) {
            return Err(Error::Invalid(
                "grid tiles have different sizes or bit depths",
            ));
        }
        let mut out = segments[i].lock().unwrap_or_else(|e| e.into_inner());
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
        Ok(())
    };
    if let Some(tile) = &first_tile {
        place(0, Some(tile), None)?;
    }

    // Tiles are independent HEVC streams: decode them on several threads.
    if threads <= 1 {
        let mut ws = plain.then(GridWorkspace::<S>::default);
        for i in start..tiles.len() {
            place(i, None, ws.as_mut())?;
        }
    } else {
        let next = std::sync::atomic::AtomicUsize::new(start);
        let error = std::sync::Mutex::new(None);
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    let mut ws = plain.then(GridWorkspace::<S>::default);
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if i >= tiles.len() {
                            break;
                        }
                        if let Err(e) = place(i, None, ws.as_mut()) {
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
    Ok((canvas, false))
}

/// Whether `id` is a coded image without transformative properties or alpha plane, so that
/// its decoded picture can be used as is.
pub(crate) fn is_plain_coded_image(file: &HeifFile<'_>, id: ItemId) -> Result<bool, Error> {
    if file.item(id)?.item_type.0 != *b"hvc1"
        || !file.referencing_items(id, b"auxl").is_empty()
        || !file.referenced_items(id, b"pred").is_empty()
    {
        return Ok(false);
    }
    Ok(!file.item_properties(id)?.any(|p| {
        matches!(
            p,
            Property::CleanAperture { .. } | Property::Rotation(_) | Property::Mirror(_)
        )
    }))
}

struct IovlHeader {
    width: u32,
    height: u32,
    fill: [u16; 4],
    offsets: Vec<(i64, i64)>,
}

fn parse_iovl_header(data: &[u8], input_count: usize) -> Result<IovlHeader, Error> {
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
    let (width, height) = (field(false)? as u32, field(false)? as u32);
    let offsets = (0..input_count)
        .map(|_| Ok((field(true)?, field(true)?)))
        .collect::<Result<Vec<_>, Error>>()?;
    Ok(IovlHeader {
        width,
        height,
        fill,
        offsets,
    })
}

fn decode_overlay<S: Sample>(
    file: &HeifFile<'_>,
    id: ItemId,
    depth: u32,
    options: &Options,
) -> Result<Image<S>, Error> {
    let data = file.item_data(id)?;
    let inputs = file.referenced_items(id, b"dimg");
    let header = parse_iovl_header(&data, inputs.len())?;
    check_size(header.width, header.height, options.max_pixels())?;

    let images = inputs
        .iter()
        .map(|&i| decode_item_with::<S>(file, i, depth + 1, options))
        .collect::<Result<Vec<_>, _>>()?;
    let bit_depth = images.iter().map(|i| i.bit_depth).max().unwrap_or(8);
    // canvas_fill_value is given on 16 bits.
    let scale = |v: u16| ((u32::from(v) * ((1u32 << bit_depth) - 1) + 32767) / 65535) as u16;
    let mut canvas = Image::filled_samples(
        header.width,
        header.height,
        bit_depth,
        header.fill.map(|v| S::from_u16(scale(v))),
    );
    canvas.has_alpha = header.fill[3] != 65535;
    for (img, (x, y)) in images.iter().zip(header.offsets) {
        canvas.draw(img, x, y);
    }
    let opaque = canvas.max_value();
    canvas.has_alpha = canvas.data.chunks_exact(4).any(|p| p[3].to_u16() != opaque);
    Ok(canvas)
}

fn clap_rect(
    img_w: u32,
    img_h: u32,
    width: (u32, u32),
    height: (u32, u32),
    horiz: (i32, u32),
    vert: (i32, u32),
) -> Result<(u32, u32, u32, u32), Error> {
    if width.1 == 0 || height.1 == 0 || horiz.1 == 0 || vert.1 == 0 {
        return Err(Error::Invalid("clap with a zero denominator"));
    }
    let cw = f64::from(width.0) / f64::from(width.1);
    let ch = f64::from(height.0) / f64::from(height.1);
    let cx = f64::from(horiz.0) / f64::from(horiz.1) + (f64::from(img_w) - 1.0) / 2.0;
    let cy = f64::from(vert.0) / f64::from(vert.1) + (f64::from(img_h) - 1.0) / 2.0;
    let left = (cx - (cw - 1.0) / 2.0).round().max(0.0) as u32;
    let top = (cy - (ch - 1.0) / 2.0).round().max(0.0) as u32;
    let x = left.min(img_w);
    let y = top.min(img_h);
    let w = (cw.round() as u32).min(img_w - x);
    let h = (ch.round() as u32).min(img_h - y);
    Ok((x, y, w, h))
}

/// Applies a `clap` (clean aperture) crop, centred as specified by ISO/IEC 14496-12.
fn clean_aperture<S: Sample>(
    img: &Image<S>,
    width: (u32, u32),
    height: (u32, u32),
    horiz: (i32, u32),
    vert: (i32, u32),
) -> Result<Image<S>, Error> {
    let (x, y, w, h) = clap_rect(img.width, img.height, width, height, horiz, vert)?;
    Ok(img.crop(x, y, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_transform_matches_image_ops_over_d4() {
        let base = Image::<u8> {
            width: 5,
            height: 3,
            bit_depth: 8,
            has_alpha: false,
            data: (0..15)
                .flat_map(|i| [i as u8, i as u8 + 20, i as u8 + 40, 255])
                .collect(),
        };
        let all_d4 = [false, true].into_iter().flat_map(|swap_xy| {
            [false, true].into_iter().flat_map(move |flip_x| {
                [false, true].into_iter().map(move |flip_y| GridTransform {
                    swap_xy,
                    flip_x,
                    flip_y,
                })
            })
        });
        for start in all_d4 {
            let mut img = Image::filled_samples(
                start.output_size(base.width, base.height).0,
                start.output_size(base.width, base.height).1,
                8,
                [0u8; 4],
            );
            copy_tile_transformed(
                &base.data,
                base.width as usize,
                0,
                0,
                base.width as usize,
                base.height as usize,
                &mut img.data,
                img.width as usize * 4,
                0,
                0,
                start,
            );
            for op in 0..5 {
                let mut xf = start;
                let expected = match op {
                    0 => {
                        xf.then_rotate_ccw(1);
                        img.rotate_ccw(1)
                    }
                    1 => {
                        xf.then_rotate_ccw(2);
                        img.rotate_ccw(2)
                    }
                    2 => {
                        xf.then_rotate_ccw(3);
                        img.rotate_ccw(3)
                    }
                    3 => {
                        xf.then_mirror(true);
                        img.mirror(true)
                    }
                    _ => {
                        xf.then_mirror(false);
                        img.mirror(false)
                    }
                };
                let (ow, oh) = xf.output_size(base.width, base.height);
                let mut got = Image::filled_samples(ow, oh, 8, [0u8; 4]);
                copy_tile_transformed(
                    &base.data,
                    base.width as usize,
                    0,
                    0,
                    base.width as usize,
                    base.height as usize,
                    &mut got.data,
                    ow as usize * 4,
                    0,
                    0,
                    xf,
                );
                assert_eq!(got, expected, "start={start:?}, op={op}");

                let mut segs: Vec<Vec<u8>> = (0..oh as usize)
                    .map(|_| vec![0u8; ow as usize * 4])
                    .collect();
                let mut refs: Vec<&mut [u8]> = segs.iter_mut().map(Vec::as_mut_slice).collect();
                copy_tile_transformed_segments(
                    &base.data,
                    base.width as usize,
                    0,
                    0,
                    base.width as usize,
                    base.height as usize,
                    &mut refs,
                    xf,
                );
                let flat: Vec<u8> = segs.into_iter().flatten().collect();
                assert_eq!(flat, expected.data, "segments start={start:?}, op={op}");
            }
        }
    }
}
