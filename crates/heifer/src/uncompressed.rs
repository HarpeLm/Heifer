//! Decoding of uncompressed images (`unci`, ISO/IEC 23001-17).
//!
//! The subset that matters in practice is supported: one to four unsigned components of 1 to 16
//! bits, the component, pixel, mixed, row and tile-component interleave modes, tiles, subsampling
//! of 4:2:2, 4:2:0 and 4:1:1, and the row, pixel and tile alignments. Blocks, floating-point and
//! signed samples, little-endian samples and generic compression are refused.

use crate::color::{ColorParams, frame_to_rgba_sized};
use crate::heifer_isobmff::boxes::{ColorInfo, Property, UncompressedConfig};
use crate::heifer_isobmff::{HeifFile, ItemId};
use crate::{Error, Image, Options, Sample, check_size};
use heifer_hevc_dec::recon::Frame;

/// Reads samples packed most-significant bit first.
struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    fn position(&self) -> usize {
        self.bit
    }

    fn truncated() -> Error {
        Error::Invalid("uncompressed image data is truncated")
    }

    fn read(&mut self, bits: u16) -> Result<u16, Error> {
        let mut value = 0u32;
        for _ in 0..bits {
            let byte = self.data.get(self.bit / 8).ok_or_else(Self::truncated)?;
            let shift = 7 - self.bit % 8;
            value = (value << 1) | u32::from((byte >> shift) & 1);
            self.bit += 1;
        }
        Ok(value as u16)
    }

    fn skip_bits(&mut self, bits: usize) -> Result<(), Error> {
        let end = self.bit.checked_add(bits).ok_or_else(Self::truncated)?;
        let total = self.data.len().checked_mul(8).ok_or_else(Self::truncated)?;
        if end > total {
            return Err(Self::truncated());
        }
        self.bit = end;
        Ok(())
    }

    fn skip_to_byte_boundary(&mut self) -> Result<(), Error> {
        let pad = (8 - self.bit % 8) % 8;
        self.skip_bits(pad)
    }

    /// Pads the current row, from its first bit at `start`, to `alignment` bytes.
    fn align_row(&mut self, start: usize, alignment: u32) -> Result<(), Error> {
        self.skip_to_byte_boundary()?;
        self.align_from(start, alignment)
    }

    /// Pads the current pixel, from its first bit at `start`, to `pixel_size` bytes.
    fn align_pixel(&mut self, start: usize, pixel_size: u32) -> Result<(), Error> {
        if pixel_size == 0 {
            return Ok(());
        }
        let bits = self.bit - start;
        let target = usize::try_from(pixel_size)
            .map_err(|_| Self::truncated())?
            .checked_mul(8)
            .ok_or_else(Self::truncated)?;
        if bits > target {
            return Err(Error::Invalid(
                "uncompressed pixel is larger than `pixel_size`",
            ));
        }
        self.skip_bits(target - bits)
    }

    /// Pads up to the next multiple of `alignment` bytes, counted from `start`.
    fn align_from(&mut self, start: usize, alignment: u32) -> Result<(), Error> {
        if alignment == 0 {
            return Ok(());
        }
        let alignment = usize::try_from(alignment).map_err(|_| Self::truncated())?;
        let bytes = self.bit / 8 - start / 8;
        let pad = (alignment - bytes % alignment) % alignment;
        self.skip_bits(pad * 8)
    }
}

/// What one coded component holds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Mono,
    Luma,
    Cb,
    Cr,
    Red,
    Green,
    Blue,
    Alpha,
    /// A component heifer does not use (padded, depth, palette...): its samples are skipped.
    Unused,
}

impl Role {
    /// Index of the plane of this component, if it has one.
    fn plane(self) -> Option<usize> {
        match self {
            Self::Mono => Some(0),
            Self::Luma => Some(1),
            Self::Cb => Some(2),
            Self::Cr => Some(3),
            Self::Red => Some(4),
            Self::Green => Some(5),
            Self::Blue => Some(6),
            Self::Alpha => Some(7),
            Self::Unused => None,
        }
    }

    fn is_chroma(self) -> bool {
        matches!(self, Self::Cb | Self::Cr)
    }
}

fn role_of(component_type: u16) -> Role {
    match component_type {
        0 => Role::Mono,
        1 => Role::Luma,
        2 => Role::Cb,
        3 => Role::Cr,
        4 => Role::Red,
        5 => Role::Green,
        6 => Role::Blue,
        7 => Role::Alpha,
        _ => Role::Unused,
    }
}

/// A component of the frame, with the layout of its samples.
struct Entry {
    role: Role,
    bit_depth: u16,
    align_size: u8,
    /// Width of one tile, in samples of this component.
    tile_width: u32,
    /// Height of one tile, in samples of this component.
    tile_height: u32,
}

struct Plane {
    width: u32,
    height: u32,
    data: Vec<u16>,
}

impl Plane {
    fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            data: vec![0; width as usize * height as usize],
        }
    }

    fn set(&mut self, x: u32, y: u32, value: u16) {
        if x < self.width && y < self.height {
            self.data[y as usize * self.width as usize + x as usize] = value;
        }
    }

    fn get(&self, x: u32, y: u32) -> u16 {
        self.data[y as usize * self.width as usize + x as usize]
    }
}

fn unsupported(reason: &str) -> Error {
    Error::Unsupported(format!("uncompressed image {reason}"))
}

/// Decodes an uncompressed image item, as a primary image or as an auxiliary one.
pub(crate) fn decode<S: Sample>(
    file: &HeifFile<'_>,
    id: ItemId,
    options: &Options,
) -> Result<Image<S>, Error> {
    let config = file.uncompressed_config(id)?.ok_or_else(|| {
        Error::Unsupported("uncompressed image item without `uncC` configuration".into())
    })?;
    if file
        .item_properties(id)?
        .any(|p| matches!(p, Property::Unknown(t, _) if &t.0 == b"cmpC"))
    {
        return Err(unsupported("with generic compression"));
    }
    let (width, height) = file.image_size(id)?.ok_or(Error::Invalid(
        "uncompressed image without an `ispe` property",
    ))?;
    check_size(width, height, options.max_pixels())?;

    let data = file.item_data(id)?;
    let frame = layout(file, id, config, width, height, data.as_ref())?;
    let mut reader = frame.reader;
    let entries = frame.entries;
    let mut planes = frame.planes;
    let mode = frame.mode;

    let tile_width = width / config.columns;
    let tile_height = height / config.rows;
    if mode == Mode::TileComponent {
        // Tile-component interleave stores all tiles of one component before the next
        // component, unlike the other modes, which store all components of one tile.
        for entry in &entries {
            for tile_row in 0..config.rows {
                for tile_col in 0..config.columns {
                    let tile_start = reader.position();
                    decode_component_rows(
                        &mut reader,
                        entry,
                        &mut planes,
                        config.row_align_size,
                        tile_col,
                        tile_row,
                        entry.tile_width,
                        entry.tile_height,
                    )?;
                    reader.align_from(tile_start, config.tile_align_size)?;
                }
            }
        }
    } else {
        for tile_row in 0..config.rows {
            for tile_col in 0..config.columns {
                decode_tile(
                    &mut reader,
                    &entries,
                    &mut planes,
                    config,
                    mode,
                    tile_col,
                    tile_row,
                    tile_width,
                    tile_height,
                )?;
            }
        }
    }

    compose::<S>(
        file,
        id,
        width,
        height,
        config.sampling,
        &entries,
        &mut planes,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Component,
    Pixel,
    Mixed,
    Row,
    TileComponent,
}

/// A validated frame, ready to decode.
struct FrameLayout<'a> {
    reader: BitReader<'a>,
    entries: Vec<Entry>,
    planes: [Option<Plane>; 8],
    mode: Mode,
}

/// Validates the frame and returns its entries, planes and interleave mode.
fn layout<'a>(
    file: &HeifFile<'_>,
    id: ItemId,
    config: &UncompressedConfig,
    width: u32,
    height: u32,
    data: &'a [u8],
) -> Result<FrameLayout<'a>, Error> {
    if config.version != 0 {
        return Err(unsupported(&format!(
            "configuration version {}",
            config.version
        )));
    }
    if config.block_size != 0 {
        return Err(unsupported("in blocks"));
    }
    if config.components_little_endian {
        return Err(unsupported("with little-endian components"));
    }
    if config.components.is_empty() || config.components.len() > 4 {
        return Err(unsupported(&format!(
            "with {} components",
            config.components.len()
        )));
    }
    for component in &config.components {
        if component.format != 0 {
            return Err(unsupported(&format!(
                "component format {}",
                component.format
            )));
        }
        if component.bit_depth == 0 || component.bit_depth > 16 {
            return Err(unsupported(&format!(
                "component bit depth {}",
                component.bit_depth
            )));
        }
    }
    if config.columns == 0 || config.rows == 0 {
        return Err(Error::Invalid("uncompressed image with no tiles"));
    }
    if width % config.columns != 0 || height % config.rows != 0 {
        return Err(unsupported("with partial edge tiles"));
    }

    let mode = match config.interleave {
        0 => Mode::Component,
        1 => Mode::Pixel,
        2 => Mode::Mixed,
        3 => Mode::Row,
        4 => Mode::TileComponent,
        5 => return Err(unsupported("with multi-Y pixel interleave")),
        other => return Err(unsupported(&format!("interleave mode {other}"))),
    };
    if config.sampling > 3 {
        return Err(unsupported(&format!(
            "subsampling mode {}",
            config.sampling
        )));
    }
    if config.pixel_size != 0 && mode != Mode::Pixel {
        return Err(unsupported("with `pixel_size` outside pixel interleave"));
    }
    if mode == Mode::Mixed && !matches!(config.sampling, 1 | 2) {
        return Err(unsupported(
            "with mixed interleave without 4:2:2 or 4:2:0 subsampling",
        ));
    }
    if matches!(mode, Mode::Pixel | Mode::Row) && config.sampling != 0 {
        return Err(unsupported("with subsampling and pixel or row interleave"));
    }

    let descriptions = file.component_descriptions(id)?;
    let count = config.components.len();
    let mut entries = Vec::with_capacity(count);
    let mut seen = [false; 8];
    for (position, component) in config.components.iter().enumerate() {
        let role = match descriptions {
            Some(descriptions) => {
                let description = descriptions
                    .components
                    .get(usize::from(component.index))
                    .ok_or(Error::Invalid(
                        "uncompressed component index is out of range",
                    ))?;
                role_of(description.component_type)
            }
            None => fallback_role(position, count),
        };
        if let Some(plane) = role.plane() {
            if seen[plane] {
                return Err(Error::Invalid(
                    "uncompressed image with a duplicate component",
                ));
            }
            seen[plane] = true;
        }
        let (component_width, component_height) =
            component_size(role, config.sampling, width, height)?;
        if component_width % config.columns != 0 || component_height % config.rows != 0 {
            return Err(unsupported("with partial edge tiles"));
        }
        entries.push(Entry {
            role,
            bit_depth: component.bit_depth,
            align_size: component.align_size,
            tile_width: component_width / config.columns,
            tile_height: component_height / config.rows,
        });
    }

    let has_rgb = seen[4] && seen[5] && seen[6];
    let any_rgb = seen[4] || seen[5] || seen[6];
    let has_ycbcr = seen[1] && seen[2] && seen[3];
    let any_ycbcr = seen[1] || seen[2] || seen[3];
    if any_rgb && (!has_rgb || any_ycbcr || seen[0]) {
        return Err(unsupported("without a complete RGB component set"));
    }
    if any_ycbcr && (!has_ycbcr || seen[0]) {
        return Err(unsupported("without a complete YCbCr component set"));
    }
    if config.sampling != 0 && !has_ycbcr {
        return Err(unsupported(
            "with subsampling but no complete YCbCr component set",
        ));
    }
    if !has_rgb && !has_ycbcr && !seen[0] && !seen[1] {
        return Err(unsupported("without a component heifer understands"));
    }

    let mut planes: [Option<Plane>; 8] = std::array::from_fn(|_| None);
    for entry in &entries {
        if let Some(index) = entry.role.plane() {
            let (w, h) = component_size(entry.role, config.sampling, width, height)?;
            planes[index] = Some(Plane::new(w, h));
        }
    }

    Ok(FrameLayout {
        reader: BitReader::new(data),
        entries,
        planes,
        mode,
    })
}

/// Role of a component when the file has no `cmpd` box: the usual layouts.
fn fallback_role(position: usize, count: usize) -> Role {
    match (count, position) {
        (1, _) => Role::Mono,
        (2, 0) => Role::Mono,
        (2, 1) => Role::Alpha,
        (3, 0) => Role::Red,
        (3, 1) => Role::Green,
        (3, 2) => Role::Blue,
        (4, 0) => Role::Red,
        (4, 1) => Role::Green,
        (4, 2) => Role::Blue,
        (4, _) => Role::Alpha,
        _ => Role::Unused,
    }
}

/// Width and height of a component, accounting for chroma subsampling.
fn component_size(role: Role, sampling: u8, width: u32, height: u32) -> Result<(u32, u32), Error> {
    let (sw, sh) = if role.is_chroma() {
        match sampling {
            0 => (1, 1),
            1 => (2, 1),
            2 => (2, 2),
            3 => (4, 1),
            _ => return Err(unsupported(&format!("subsampling mode {sampling}"))),
        }
    } else {
        (1, 1)
    };
    if width % sw != 0 || height % sh != 0 {
        return Err(unsupported("whose subsampled size is not a whole number"));
    }
    Ok((width / sw, height / sh))
}

fn read_sample(reader: &mut BitReader<'_>, entry: &Entry) -> Result<u16, Error> {
    if entry.align_size != 0 {
        reader.skip_to_byte_boundary()?;
        let pad = usize::from(entry.align_size) * 8 - usize::from(entry.bit_depth);
        reader.skip_bits(pad)?;
    }
    reader.read(entry.bit_depth)
}

fn store(planes: &mut [Option<Plane>; 8], entry: &Entry, x: u32, y: u32, value: u16) {
    if let Some(index) = entry.role.plane() {
        if let Some(plane) = &mut planes[index] {
            plane.set(x, y, value);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_tile(
    reader: &mut BitReader<'_>,
    entries: &[Entry],
    planes: &mut [Option<Plane>; 8],
    config: &UncompressedConfig,
    mode: Mode,
    tile_col: u32,
    tile_row: u32,
    tile_width: u32,
    tile_height: u32,
) -> Result<(), Error> {
    let align = config.row_align_size;
    match mode {
        Mode::Component => {
            let tile_start = reader.position();
            for entry in entries {
                decode_component_rows(
                    reader,
                    entry,
                    planes,
                    align,
                    tile_col,
                    tile_row,
                    entry.tile_width,
                    entry.tile_height,
                )?;
            }
            reader.align_from(tile_start, config.tile_align_size)?;
        }
        Mode::TileComponent => {
            unreachable!("tile-component interleave is decoded component by component")
        }
        Mode::Row => {
            let tile_start = reader.position();
            for y in 0..tile_height {
                for entry in entries {
                    let row_start = reader.position();
                    for x in 0..tile_width {
                        let value = read_sample(reader, entry)?;
                        store(
                            planes,
                            entry,
                            tile_col * entry.tile_width + x,
                            tile_row * tile_height + y,
                            value,
                        );
                    }
                    reader.align_row(row_start, align)?;
                }
            }
            reader.align_from(tile_start, config.tile_align_size)?;
        }
        Mode::Pixel => {
            let tile_start = reader.position();
            for y in 0..tile_height {
                let row_start = reader.position();
                for x in 0..tile_width {
                    let pixel_start = reader.position();
                    for entry in entries {
                        let value = read_sample(reader, entry)?;
                        store(
                            planes,
                            entry,
                            tile_col * tile_width + x,
                            tile_row * tile_height + y,
                            value,
                        );
                    }
                    reader.align_pixel(pixel_start, config.pixel_size)?;
                }
                reader.align_row(row_start, align)?;
            }
            reader.align_from(tile_start, config.tile_align_size)?;
        }
        Mode::Mixed => {
            let tile_start = reader.position();
            let mut chroma_done = false;
            for entry in entries {
                if entry.role.is_chroma() {
                    if chroma_done {
                        continue;
                    }
                    chroma_done = true;
                    let chroma: Vec<&Entry> = entries
                        .iter()
                        .filter(|entry| entry.role.is_chroma())
                        .collect();
                    let (cwidth, cheight) = (entry.tile_width, entry.tile_height);
                    for y in 0..cheight {
                        let row_start = reader.position();
                        for x in 0..cwidth {
                            for chroma_entry in &chroma {
                                let value = read_sample(reader, chroma_entry)?;
                                store(
                                    planes,
                                    chroma_entry,
                                    tile_col * chroma_entry.tile_width + x,
                                    tile_row * chroma_entry.tile_height + y,
                                    value,
                                );
                            }
                        }
                        reader.align_row(row_start, align)?;
                    }
                } else {
                    decode_component_rows(
                        reader,
                        entry,
                        planes,
                        align,
                        tile_col,
                        tile_row,
                        entry.tile_width,
                        entry.tile_height,
                    )?;
                }
            }
            reader.align_from(tile_start, config.tile_align_size)?;
        }
    }
    Ok(())
}

/// Decodes one component of a tile, row by row, as component and mixed interleave store it.
#[allow(clippy::too_many_arguments)]
fn decode_component_rows(
    reader: &mut BitReader<'_>,
    entry: &Entry,
    planes: &mut [Option<Plane>; 8],
    row_align_size: u32,
    tile_col: u32,
    tile_row: u32,
    tile_width: u32,
    tile_height: u32,
) -> Result<(), Error> {
    for y in 0..tile_height {
        let row_start = reader.position();
        for x in 0..tile_width {
            let value = read_sample(reader, entry)?;
            store(
                planes,
                entry,
                tile_col * tile_width + x,
                tile_row * tile_height + y,
                value,
            );
        }
        reader.align_row(row_start, row_align_size)?;
    }
    Ok(())
}

/// Builds the RGBA image from the decoded planes.
fn compose<S: Sample>(
    file: &HeifFile<'_>,
    id: ItemId,
    width: u32,
    height: u32,
    sampling: u8,
    entries: &[Entry],
    planes: &mut [Option<Plane>; 8],
) -> Result<Image<S>, Error> {
    let has_rgb = entries.iter().any(|entry| entry.role == Role::Red);
    if has_rgb {
        let red = entry_of(entries, Role::Red)?;
        let green = entry_of(entries, Role::Green)?;
        let blue = entry_of(entries, Role::Blue)?;
        let coded = red.bit_depth.max(green.bit_depth).max(blue.bit_depth);
        let depth = S::depth(coded as u8);
        let mut image = Image::filled_samples(width, height, depth, [S::default(); 4]);
        let max = image.max_value();
        let alpha_entry = entry_of(entries, Role::Alpha).ok();
        for y in 0..height {
            for x in 0..width {
                let i = (y as usize * width as usize + x as usize) * 4;
                image.data[i] = scale(
                    plane_sample(planes, Role::Red, x, y),
                    red.bit_depth,
                    depth,
                    max,
                );
                image.data[i + 1] = scale(
                    plane_sample(planes, Role::Green, x, y),
                    green.bit_depth,
                    depth,
                    max,
                );
                image.data[i + 2] = scale(
                    plane_sample(planes, Role::Blue, x, y),
                    blue.bit_depth,
                    depth,
                    max,
                );
                image.data[i + 3] = match alpha_entry {
                    Some(entry) => scale(
                        plane_sample(planes, Role::Alpha, x, y),
                        entry.bit_depth,
                        depth,
                        max,
                    ),
                    None => S::from_u16(max),
                };
            }
        }
        image.has_alpha = alpha_entry.is_some();
        return Ok(image);
    }

    if entries.iter().any(|entry| entry.role == Role::Luma) {
        let luma = entry_of(entries, Role::Luma)?;
        if !entries.iter().any(|entry| entry.role == Role::Cb) {
            return Ok(mono_image::<S>(
                width,
                height,
                luma.bit_depth,
                planes,
                Role::Luma,
            ));
        }
        let cb = entry_of(entries, Role::Cb)?;
        let cr = entry_of(entries, Role::Cr)?;
        let chroma_depth = cb.bit_depth.max(cr.bit_depth);
        let mut cb_plane = planes[Role::Cb.plane().unwrap()]
            .take()
            .ok_or(Error::Invalid("missing uncompressed chroma plane"))?;
        let mut cr_plane = planes[Role::Cr.plane().unwrap()]
            .take()
            .ok_or(Error::Invalid("missing uncompressed chroma plane"))?;
        rescale_plane(&mut cb_plane, cb.bit_depth, chroma_depth);
        rescale_plane(&mut cr_plane, cr.bit_depth, chroma_depth);
        let params = color_params(file, id)?;
        let frame = Frame {
            widths: [width, cb_plane.width, cb_plane.width],
            heights: [height, cb_plane.height, cb_plane.height],
            planes: [
                planes[Role::Luma.plane().unwrap()]
                    .take()
                    .ok_or(Error::Invalid("missing uncompressed luma plane"))?
                    .data,
                cb_plane.data,
                cr_plane.data,
            ],
            bit_depth: [luma.bit_depth as u8, chroma_depth as u8],
            chroma_format: match sampling {
                1 => 2,
                2 => 1,
                _ => 3,
            },
            full_range: params.full_range,
            matrix_coeffs: params.matrix as u8,
        };
        let mut image: Image<S> = frame_to_rgba_sized(&frame, params, width, height);
        attach_alpha(&mut image, entries, planes)?;
        return Ok(image);
    }

    if let Some(mono) = entries.iter().find(|entry| entry.role == Role::Mono) {
        let mut image = mono_image::<S>(width, height, mono.bit_depth, planes, Role::Mono);
        attach_alpha(&mut image, entries, planes)?;
        return Ok(image);
    }

    Err(unsupported("without an image component"))
}

fn mono_image<S: Sample>(
    width: u32,
    height: u32,
    bit_depth: u16,
    planes: &[Option<Plane>; 8],
    role: Role,
) -> Image<S> {
    let depth = S::depth(bit_depth as u8);
    let max = ((1u32 << depth) - 1) as u16;
    let mut image = Image::filled_samples(width, height, depth, [S::default(); 4]);
    for y in 0..height {
        for x in 0..width {
            let i = (y as usize * width as usize + x as usize) * 4;
            let value = scale(plane_sample(planes, role, x, y), bit_depth, depth, max);
            image.data[i] = value;
            image.data[i + 1] = value;
            image.data[i + 2] = value;
            image.data[i + 3] = S::from_u16(max);
        }
    }
    image
}

fn attach_alpha<S: Sample>(
    image: &mut Image<S>,
    entries: &[Entry],
    planes: &[Option<Plane>; 8],
) -> Result<(), Error> {
    let Some(entry) = entry_of(entries, Role::Alpha).ok() else {
        return Ok(());
    };
    let Some(plane) = &planes[Role::Alpha.plane().unwrap()] else {
        return Ok(());
    };
    if plane.width < image.width || plane.height < image.height {
        return Err(Error::Unsupported(
            "alpha plane smaller than the image".into(),
        ));
    }
    let depth = image.bit_depth;
    let max = image.max_value();
    for y in 0..image.height {
        for x in 0..image.width {
            let i = (y as usize * image.width as usize + x as usize) * 4 + 3;
            image.data[i] = scale(plane.get(x, y), entry.bit_depth, depth, max);
        }
    }
    image.has_alpha = true;
    Ok(())
}

fn entry_of(entries: &[Entry], role: Role) -> Result<&Entry, Error> {
    entries
        .iter()
        .find(|entry| entry.role == role)
        .ok_or(Error::Invalid("missing uncompressed component"))
}

fn plane_sample(planes: &[Option<Plane>; 8], role: Role, x: u32, y: u32) -> u16 {
    planes[role.plane().unwrap()]
        .as_ref()
        .map_or(0, |plane| plane.get(x, y))
}

fn scale<S: Sample>(value: u16, source_depth: u16, target_depth: u8, max: u16) -> S {
    if u8::try_from(source_depth) == Ok(target_depth) {
        return S::from_u16(value);
    }
    let source_max = (1u32 << source_depth) - 1;
    let target_max = u32::from(max);
    let scaled = (u32::from(value) * target_max + source_max / 2) / source_max;
    S::from_u16(scaled as u16)
}

fn rescale_plane(plane: &mut Plane, source_depth: u16, target_depth: u16) {
    if source_depth == target_depth {
        return;
    }
    let source_max = (1u32 << source_depth) - 1;
    let target_max = (1u32 << target_depth) - 1;
    for value in &mut plane.data {
        *value = ((u32::from(*value) * target_max + source_max / 2) / source_max) as u16;
    }
}

fn color_params(file: &HeifFile<'_>, id: ItemId) -> Result<ColorParams, Error> {
    Ok(file
        .item_properties(id)?
        .find_map(|property| match property {
            Property::Color(ColorInfo::Nclx {
                matrix, full_range, ..
            }) => Some(ColorParams {
                matrix: *matrix,
                full_range: *full_range,
            }),
            _ => None,
        })
        .unwrap_or(ColorParams {
            matrix: 2,
            full_range: true,
        }))
}
