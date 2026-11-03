//! Writing of HEIF files: HEVC-coded image items, an optional alpha plane, and their properties.
//!
//! ```
//! use heifer_isobmff::boxes::HevcConfig;
//! use heifer_isobmff::writer::{HeifWriter, ImageSpec};
//!
//! # let (vps, sps, pps, slice) = (vec![0x40, 1], vec![0x42, 1], vec![0x44, 1], vec![0x26, 1]);
//! let config = HevcConfig {
//!     profile_idc: 1,
//!     level_idc: 93,
//!     chroma_format: 1,
//!     bit_depth_luma: 8,
//!     bit_depth_chroma: 8,
//!     nal_length_size: 4,
//!     nal_units: vec![vps, sps, pps],
//! };
//! let mut writer = HeifWriter::new();
//! let image = writer.add_image(&ImageSpec::new(64, 48, config, &[slice]));
//! writer.set_primary(image);
//! let bytes = writer.finish();
//! let file = heifer_isobmff::HeifFile::parse(&bytes).unwrap();
//! assert_eq!(file.image_size(image).unwrap(), Some((64, 48)));
//! ```

use crate::ItemId;
use crate::boxes::{ColorInfo, HevcConfig};

/// `auxC` type of alpha planes.
pub const ALPHA_URN: &str = "urn:mpeg:hevc:2015:auxid:1";

/// `auxC` type of depth maps.
pub const DEPTH_URN: &str = "urn:mpeg:hevc:2015:auxid:2";

/// An HEVC-coded image to add to a file.
#[derive(Debug, Clone)]
pub struct ImageSpec<'a> {
    /// Width in pixels (`ispe`).
    pub width: u32,
    /// Height in pixels (`ispe`).
    pub height: u32,
    /// Decoder configuration (`hvcC`): parameter sets and stream properties. Its
    /// `nal_length_size` must be 4.
    pub config: HevcConfig,
    /// Slice segment NAL units of the picture, without start codes or length prefixes.
    pub slices: &'a [Vec<u8>],
    /// Colour information (`colr`), if any.
    pub color: Option<ColorInfo>,
    /// Auxiliary image type (`auxC`), e.g. [`ALPHA_URN`]; `None` for a regular image.
    pub auxiliary_type: Option<&'a str>,
    /// Whether the item is hidden (e.g. an alpha plane or a thumbnail).
    pub hidden: bool,
    /// Size to crop the image to, from its top-left corner (`clap`), when the coded picture is
    /// larger than the image (e.g. odd sizes in 4:2:0).
    pub crop: Option<(u32, u32)>,
}

impl<'a> ImageSpec<'a> {
    /// A visible image without colour information.
    pub fn new(width: u32, height: u32, config: HevcConfig, slices: &'a [Vec<u8>]) -> Self {
        Self {
            width,
            height,
            config,
            slices,
            color: None,
            auxiliary_type: None,
            hidden: false,
            crop: None,
        }
    }
}

#[derive(Debug)]
struct WriterItem {
    id: ItemId,
    item_type: [u8; 4],
    hidden: bool,
    /// Indices into `HeifWriter::properties` (1-based in `ipma`), with their `essential` flag.
    properties: Vec<(u16, bool)>,
    /// MIME type of `mime` items.
    content_type: Option<&'static str>,
    data: Vec<u8>,
}

/// Builds a HEIF file item by item.
#[derive(Debug, Default)]
pub struct HeifWriter {
    items: Vec<WriterItem>,
    /// Property boxes of `ipco`, complete (header included).
    properties: Vec<Vec<u8>>,
    /// `iref` entries: type, from, to.
    references: Vec<([u8; 4], ItemId, Vec<ItemId>)>,
    /// `grpl` entity groups (`altr`), in order.
    groups: Vec<Vec<ItemId>>,
    primary: Option<ItemId>,
    /// Whether the file has a `tmap` item, which adds the `tmap` compatible brand.
    has_tmap: bool,
}

impl HeifWriter {
    /// An empty file.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an `hvc1` image item and returns its ID. The first image added is the primary
    /// image unless [`set_primary`](Self::set_primary) says otherwise.
    pub fn add_image(&mut self, image: &ImageSpec<'_>) -> ItemId {
        let mut properties = vec![
            (self.property(hvcc_box(&image.config)), true),
            (
                self.property(full_box(b"ispe", 0, 0, |b| {
                    b.extend_from_slice(&image.width.to_be_bytes());
                    b.extend_from_slice(&image.height.to_be_bytes());
                })),
                false,
            ),
        ];
        let channels: &[u8] = if image.config.chroma_format == 0 {
            &[image.config.bit_depth_luma]
        } else {
            &[
                image.config.bit_depth_luma,
                image.config.bit_depth_chroma,
                image.config.bit_depth_chroma,
            ]
        };
        properties.push((
            self.property(full_box(b"pixi", 0, 0, |b| {
                b.push(channels.len() as u8);
                b.extend_from_slice(channels);
            })),
            false,
        ));
        if let Some(color) = &image.color {
            properties.push((self.property(colr_box(color)), false));
        }
        if let Some(urn) = image.auxiliary_type {
            properties.push((
                self.property(full_box(b"auxC", 0, 0, |b| {
                    b.extend_from_slice(urn.as_bytes());
                    b.push(0);
                })),
                true,
            ));
        }
        if let Some((w, h)) = image.crop {
            // Offsets of the centre of the kept area from the centre of the image, in halves.
            let clap = make_box(b"clap", |b| {
                for v in [w, 1, h, 1] {
                    b.extend_from_slice(&v.to_be_bytes());
                }
                for (kept, full) in [(w, image.width), (h, image.height)] {
                    let offset = i64::from(kept) - i64::from(full);
                    b.extend_from_slice(&(offset as i32).to_be_bytes());
                    b.extend_from_slice(&2u32.to_be_bytes());
                }
            });
            properties.push((self.property(clap), true));
        }
        let mut data = Vec::new();
        for nal in image.slices {
            data.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            data.extend_from_slice(nal);
        }
        self.add_item(*b"hvc1", image.hidden, properties, data)
    }

    /// Adds a `grid` derived image made of `tiles` (row by row, all of the same size), cropped
    /// to `width × height`, and returns its ID. For an alpha plane of another grid, pass
    /// [`ALPHA_URN`] as `auxiliary_type`, then [`add_reference`](Self::add_reference) `auxl`.
    #[allow(clippy::too_many_arguments)]
    pub fn add_grid(
        &mut self,
        rows: u16,
        columns: u16,
        width: u32,
        height: u32,
        tiles: &[ItemId],
        color: Option<&ColorInfo>,
        auxiliary_type: Option<&str>,
    ) -> ItemId {
        self.add_grid_item(
            rows,
            columns,
            width,
            height,
            tiles,
            color,
            auxiliary_type,
            auxiliary_type.is_some(),
            auxiliary_type.is_none(),
        )
    }

    /// Adds a hidden `grid` derived image without an auxiliary type, e.g. a gain map. The grid
    /// does not become the primary item.
    pub fn add_hidden_grid(
        &mut self,
        rows: u16,
        columns: u16,
        width: u32,
        height: u32,
        tiles: &[ItemId],
        color: Option<&ColorInfo>,
    ) -> ItemId {
        self.add_grid_item(
            rows, columns, width, height, tiles, color, None, true, false,
        )
    }

    /// Builds a `grid` item. `hidden` flags the item, and `primary` makes it the file's image.
    #[allow(clippy::too_many_arguments)]
    fn add_grid_item(
        &mut self,
        rows: u16,
        columns: u16,
        width: u32,
        height: u32,
        tiles: &[ItemId],
        color: Option<&ColorInfo>,
        auxiliary_type: Option<&str>,
        hidden: bool,
        primary: bool,
    ) -> ItemId {
        let mut properties = vec![(
            self.property(full_box(b"ispe", 0, 0, |b| {
                b.extend_from_slice(&width.to_be_bytes());
                b.extend_from_slice(&height.to_be_bytes());
            })),
            false,
        )];
        if let Some(color) = color {
            properties.push((self.property(colr_box(color)), false));
        }
        if let Some(urn) = auxiliary_type {
            properties.push((
                self.property(full_box(b"auxC", 0, 0, |b| {
                    b.extend_from_slice(urn.as_bytes());
                    b.push(0);
                })),
                true,
            ));
        }
        let large = width > 0xFFFF || height > 0xFFFF;
        let mut data = vec![0, u8::from(large), (rows - 1) as u8, (columns - 1) as u8];
        if large {
            data.extend_from_slice(&width.to_be_bytes());
            data.extend_from_slice(&height.to_be_bytes());
        } else {
            data.extend_from_slice(&(width as u16).to_be_bytes());
            data.extend_from_slice(&(height as u16).to_be_bytes());
        }
        let id = self.add_item(*b"grid", hidden, properties, data);
        self.add_reference(*b"dimg", id, tiles);
        if primary {
            self.primary = Some(id); // the grid, not its tiles, is the image
        }
        id
    }

    /// Adds a `tmap` derived image item (an ISO 21496-1 gain map) referencing `base` and `map`,
    /// and returns its ID. `payload` is the item's data (the tmap payload, version byte first).
    /// The item takes the size and colour of the base image, is grouped with it in an `altr`
    /// entity group, and makes `tmap` a compatible brand of the file.
    pub fn add_tmap(
        &mut self,
        base: ItemId,
        map: ItemId,
        width: u32,
        height: u32,
        color: Option<&ColorInfo>,
        payload: &[u8],
    ) -> ItemId {
        let mut properties = vec![(
            self.property(full_box(b"ispe", 0, 0, |b| {
                b.extend_from_slice(&width.to_be_bytes());
                b.extend_from_slice(&height.to_be_bytes());
            })),
            false,
        )];
        if let Some(color) = color {
            properties.push((self.property(colr_box(color)), false));
        }
        let id = self.add_item(*b"tmap", false, properties, payload.to_vec());
        self.add_reference(*b"dimg", id, &[base, map]);
        // The tone-mapped rendition and the base image are alternates of each other.
        self.add_alternative_group(&[id, base]);
        self.has_tmap = true;
        id
    }

    /// Adds an `altr` entity group (`grpl`) listing `items` in preference order.
    pub fn add_alternative_group(&mut self, items: &[ItemId]) {
        if !items.is_empty() {
            self.groups.push(items.to_vec());
        }
    }

    /// Adds an alpha plane (a hidden auxiliary image) to `image`.
    pub fn add_alpha(&mut self, image: ItemId, alpha: &ImageSpec<'_>) -> ItemId {
        let alpha = self.add_image(&ImageSpec {
            auxiliary_type: Some(ALPHA_URN),
            hidden: true,
            ..alpha.clone()
        });
        self.add_reference(*b"auxl", alpha, &[image]);
        alpha
    }

    /// Adds an `Exif` metadata item describing `image`. `tiff` is the TIFF data, starting with
    /// `II*\0` or `MM\0*`.
    pub fn add_exif(&mut self, image: ItemId, tiff: &[u8]) -> ItemId {
        let mut data = 0u32.to_be_bytes().to_vec(); // offset to the TIFF header
        data.extend_from_slice(tiff);
        let id = self.add_item(*b"Exif", true, Vec::new(), data);
        self.add_reference(*b"cdsc", id, &[image]);
        id
    }

    /// Adds an XMP packet (a `mime` item of type `application/rdf+xml`) describing `image`.
    pub fn add_xmp(&mut self, image: ItemId, xmp: &[u8]) -> ItemId {
        let id = self.add_item(*b"mime", true, Vec::new(), xmp.to_vec());
        if let Some(item) = self.items.last_mut() {
            item.content_type = Some("application/rdf+xml");
        }
        self.add_reference(*b"cdsc", id, &[image]);
        id
    }

    /// Adds an ICC profile (`colr` of type `prof`) to an image or grid item already added.
    /// Unknown items are ignored.
    pub fn add_icc_profile(&mut self, item: ItemId, icc: &[u8]) {
        let index = self.property(colr_box(&ColorInfo::Icc(icc.to_vec())));
        let properties = &self.properties;
        if let Some(item) = self.items.iter_mut().find(|i| i.id == item) {
            // Descriptive properties go before transformative ones such as `clap`.
            let position = item
                .properties
                .iter()
                .position(|&(i, _)| properties[usize::from(i) - 1][4..8] == *b"clap")
                .unwrap_or(item.properties.len());
            item.properties.insert(position, (index, false));
        }
    }

    /// Adds a reference of type `ref_type` from `from` to `to` (`iref`).
    pub fn add_reference(&mut self, ref_type: [u8; 4], from: ItemId, to: &[ItemId]) {
        self.references.push((ref_type, from, to.to_vec()));
    }

    /// Sets the primary item (`pitm`).
    pub fn set_primary(&mut self, id: ItemId) {
        self.primary = Some(id);
    }

    fn property(&mut self, b: Vec<u8>) -> u16 {
        // Identical properties (e.g. the `ispe` of an image and its alpha plane) are shared.
        let index = match self.properties.iter().position(|p| *p == b) {
            Some(i) => i,
            None => {
                self.properties.push(b);
                self.properties.len() - 1
            }
        };
        index as u16 + 1
    }

    fn add_item(
        &mut self,
        item_type: [u8; 4],
        hidden: bool,
        properties: Vec<(u16, bool)>,
        data: Vec<u8>,
    ) -> ItemId {
        let id = self.items.len() as ItemId + 1;
        self.items.push(WriterItem {
            id,
            item_type,
            hidden,
            properties,
            content_type: None,
            data,
        });
        if self.primary.is_none() && item_type == *b"hvc1" && !hidden {
            self.primary = Some(id);
        }
        id
    }

    /// Writes the file: `ftyp`, `meta`, then all item data in one `mdat`.
    pub fn finish(self) -> Vec<u8> {
        let data_size: u64 = self.items.iter().map(|i| i.data.len() as u64).sum();
        // 64-bit offsets and lengths only when needed.
        let field_size: u8 = if data_size > u64::from(u32::MAX) - (1 << 20) {
            8
        } else {
            4
        };

        let ftyp = make_box(b"ftyp", |b| {
            b.extend_from_slice(b"heic");
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(b"mif1heic");
            if self.has_tmap {
                b.extend_from_slice(b"tmap");
            }
        });
        // The `iloc` offsets depend on the size of `meta`, which does not depend on their
        // values: write it once with placeholder offsets, then again with the real ones.
        let meta_size = self.meta_box(0, field_size).len() as u64;
        let mdat_header = if data_size + 8 > u64::from(u32::MAX) {
            16
        } else {
            8
        };
        let first_offset = ftyp.len() as u64 + meta_size + mdat_header;
        let meta = self.meta_box(first_offset, field_size);

        let mut out = Vec::with_capacity((first_offset + data_size) as usize);
        out.extend_from_slice(&ftyp);
        out.extend_from_slice(&meta);
        if mdat_header == 16 {
            out.extend_from_slice(&1u32.to_be_bytes());
            out.extend_from_slice(b"mdat");
            out.extend_from_slice(&(data_size + 16).to_be_bytes());
        } else {
            out.extend_from_slice(&((data_size + 8) as u32).to_be_bytes());
            out.extend_from_slice(b"mdat");
        }
        for item in &self.items {
            out.extend_from_slice(&item.data);
        }
        out
    }

    fn meta_box(&self, first_offset: u64, field_size: u8) -> Vec<u8> {
        let large_ids = self.items.len() > usize::from(u16::MAX);
        let id = |b: &mut Vec<u8>, id: ItemId| {
            if large_ids {
                b.extend_from_slice(&id.to_be_bytes());
            } else {
                b.extend_from_slice(&(id as u16).to_be_bytes());
            }
        };
        full_box(b"meta", 0, 0, |meta| {
            meta.extend(full_box(b"hdlr", 0, 0, |b| {
                b.extend_from_slice(&[0; 4]); // pre_defined
                b.extend_from_slice(b"pict");
                b.extend_from_slice(&[0; 12]); // reserved
                b.push(0); // empty name
            }));
            let primary = self.primary.unwrap_or(1);
            meta.extend(full_box(b"pitm", u8::from(large_ids), 0, |b| {
                id(b, primary)
            }));

            // `iloc` version 0 (or 2 with 32-bit IDs); one extent per item, in the `mdat`.
            meta.extend(full_box(b"iloc", if large_ids { 2 } else { 0 }, 0, |b| {
                b.push((field_size << 4) | field_size);
                b.push(0); // base_offset_size 0, reserved
                if large_ids {
                    b.extend_from_slice(&(self.items.len() as u32).to_be_bytes());
                } else {
                    b.extend_from_slice(&(self.items.len() as u16).to_be_bytes());
                }
                let mut offset = first_offset;
                for item in &self.items {
                    id(b, item.id);
                    b.extend_from_slice(&0u16.to_be_bytes()); // data_reference_index
                    b.extend_from_slice(&1u16.to_be_bytes()); // extent_count
                    let len = item.data.len() as u64;
                    if field_size == 8 {
                        b.extend_from_slice(&offset.to_be_bytes());
                        b.extend_from_slice(&len.to_be_bytes());
                    } else {
                        b.extend_from_slice(&(offset as u32).to_be_bytes());
                        b.extend_from_slice(&(len as u32).to_be_bytes());
                    }
                    offset += len;
                }
            }));

            meta.extend(full_box(b"iinf", u8::from(large_ids), 0, |b| {
                if large_ids {
                    b.extend_from_slice(&(self.items.len() as u32).to_be_bytes());
                } else {
                    b.extend_from_slice(&(self.items.len() as u16).to_be_bytes());
                }
                for item in &self.items {
                    let version = if large_ids { 3 } else { 2 };
                    b.extend(full_box(b"infe", version, u32::from(item.hidden), |e| {
                        id(e, item.id);
                        e.extend_from_slice(&0u16.to_be_bytes()); // item_protection_index
                        e.extend_from_slice(&item.item_type);
                        e.push(0); // empty name
                        if let Some(content_type) = item.content_type {
                            e.extend_from_slice(content_type.as_bytes());
                            e.push(0);
                        }
                    }));
                }
            }));

            if !self.references.is_empty() {
                meta.extend(full_box(b"iref", u8::from(large_ids), 0, |b| {
                    for (ref_type, from, to) in &self.references {
                        b.extend(make_box(ref_type, |r| {
                            id(r, *from);
                            r.extend_from_slice(&(to.len() as u16).to_be_bytes());
                            for &t in to {
                                id(r, t);
                            }
                        }));
                    }
                }));
            }

            if !self.groups.is_empty() {
                // Entity group IDs share the item ID namespace (ISO/IEC 23008-12): a group
                // whose ID is an item ID makes decoders such as ImageIO ignore it. Number the
                // groups after the last item.
                let first = self.items.iter().map(|i| i.id).max().unwrap_or(0) + 1;
                meta.extend(make_box(b"grpl", |b| {
                    for (index, group) in self.groups.iter().enumerate() {
                        b.extend(make_box(b"altr", |g| {
                            g.extend_from_slice(&0u32.to_be_bytes()); // version 0, flags 0
                            g.extend_from_slice(&(first + index as u32).to_be_bytes());
                            g.extend_from_slice(&(group.len() as u32).to_be_bytes());
                            for &item in group {
                                g.extend_from_slice(&item.to_be_bytes());
                            }
                        }));
                    }
                }));
            }

            meta.extend(make_box(b"iprp", |iprp| {
                iprp.extend(make_box(b"ipco", |b| {
                    for p in &self.properties {
                        b.extend_from_slice(p);
                    }
                }));
                // Property indices fit in 7 bits unless there are more than 127 properties.
                let large_index = self.properties.len() > 127;
                iprp.extend(full_box(
                    b"ipma",
                    u8::from(large_ids),
                    u32::from(large_index),
                    |b| {
                        let with_props: Vec<_> = self
                            .items
                            .iter()
                            .filter(|i| !i.properties.is_empty())
                            .collect();
                        b.extend_from_slice(&(with_props.len() as u32).to_be_bytes());
                        for item in with_props {
                            id(b, item.id);
                            b.push(item.properties.len() as u8);
                            for &(index, essential) in &item.properties {
                                if large_index {
                                    let v = (u16::from(essential) << 15) | index;
                                    b.extend_from_slice(&v.to_be_bytes());
                                } else {
                                    b.push((u8::from(essential) << 7) | index as u8);
                                }
                            }
                        }
                    },
                ));
            }));
        })
    }
}

/// A box with its 32-bit size header.
fn make_box(box_type: &[u8; 4], content: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut b = vec![0; 4];
    b.extend_from_slice(box_type);
    content(&mut b);
    let size = b.len() as u32;
    b[..4].copy_from_slice(&size.to_be_bytes());
    b
}

/// A full box: version and flags, then the content.
fn full_box(
    box_type: &[u8; 4],
    version: u8,
    flags: u32,
    content: impl FnOnce(&mut Vec<u8>),
) -> Vec<u8> {
    make_box(box_type, |b| {
        b.extend_from_slice(&((u32::from(version) << 24) | (flags & 0xFF_FFFF)).to_be_bytes());
        content(b);
    })
}

/// `hvcC` (ISO/IEC 14496-15 §8.3.3). Profile compatibility and constraint flags are those of a
/// progressive, frame-only stream compatible with its own profile.
fn hvcc_box(config: &HevcConfig) -> Vec<u8> {
    make_box(b"hvcC", |b| {
        b.push(1); // configurationVersion
        b.push(config.profile_idc & 0x1F); // profile_space 0, tier 0
        let compat = 1u32 << (31 - u32::from(config.profile_idc.min(31)));
        b.extend_from_slice(&compat.to_be_bytes());
        b.extend_from_slice(&[0x90, 0, 0, 0, 0, 0]); // progressive_source, frame_only
        b.push(config.level_idc);
        b.extend_from_slice(&0xF000u16.to_be_bytes()); // min_spatial_segmentation_idc 0
        b.push(0xFC); // parallelismType 0
        b.push(0xFC | (config.chroma_format & 3));
        b.push(0xF8 | (config.bit_depth_luma.saturating_sub(8) & 7));
        b.push(0xF8 | (config.bit_depth_chroma.saturating_sub(8) & 7));
        b.extend_from_slice(&0u16.to_be_bytes()); // avgFrameRate
        // constantFrameRate 0, numTemporalLayers 1, temporalIdNested 1, lengthSizeMinusOne.
        b.push((1 << 3) | (1 << 2) | ((config.nal_length_size.max(1) - 1) & 3));
        // One array per NAL unit type, in first-appearance order.
        let mut types: Vec<u8> = Vec::new();
        for nal in &config.nal_units {
            let t = nal.first().map_or(0, |h| (h >> 1) & 0x3F);
            if !types.contains(&t) {
                types.push(t);
            }
        }
        b.push(types.len() as u8);
        for t in types {
            b.push(0x80 | t); // array_completeness
            let nals: Vec<_> = config
                .nal_units
                .iter()
                .filter(|n| n.first().map_or(0, |h| (h >> 1) & 0x3F) == t)
                .collect();
            b.extend_from_slice(&(nals.len() as u16).to_be_bytes());
            for nal in nals {
                b.extend_from_slice(&(nal.len() as u16).to_be_bytes());
                b.extend_from_slice(nal);
            }
        }
    })
}

fn colr_box(color: &ColorInfo) -> Vec<u8> {
    make_box(b"colr", |b| match color {
        ColorInfo::Nclx {
            primaries,
            transfer,
            matrix,
            full_range,
        } => {
            b.extend_from_slice(b"nclx");
            b.extend_from_slice(&primaries.to_be_bytes());
            b.extend_from_slice(&transfer.to_be_bytes());
            b.extend_from_slice(&matrix.to_be_bytes());
            b.push(u8::from(*full_range) << 7);
        }
        ColorInfo::Icc(icc) => {
            b.extend_from_slice(b"prof");
            b.extend_from_slice(icc);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HeifFile;
    use crate::boxes::Property;

    fn config(chroma_format: u8) -> HevcConfig {
        HevcConfig {
            profile_idc: 1,
            level_idc: 93,
            chroma_format,
            bit_depth_luma: 8,
            bit_depth_chroma: 8,
            nal_length_size: 4,
            nal_units: vec![
                vec![0x40, 1, 0xAA],
                vec![0x42, 1, 0xBB],
                vec![0x44, 1, 0xCC],
            ],
        }
    }

    #[test]
    fn grid_round_trips() {
        let slices = [vec![0x26, 1, 1]];
        let mut w = HeifWriter::new();
        let tiles: Vec<ItemId> = (0..6)
            .map(|_| {
                w.add_image(&ImageSpec {
                    hidden: true,
                    ..ImageSpec::new(512, 512, config(1), &slices)
                })
            })
            .collect();
        let grid = w.add_grid(2, 3, 1500, 1000, &tiles, None, None);
        let bytes = w.finish();
        let file = HeifFile::parse(&bytes).unwrap();
        assert_eq!(file.primary_id, grid);
        let g = file.grid(grid).unwrap();
        assert_eq!(
            (g.rows, g.columns, g.output_width, g.output_height),
            (2, 3, 1500, 1000)
        );
        assert_eq!(file.referenced_items(grid, b"dimg"), &tiles[..]);
        assert_eq!(file.image_size(grid).unwrap(), Some((1500, 1000)));
        // The tiles share one `hvcC` and one `ispe`.
        assert_eq!(file.properties.len(), 4);
    }

    #[test]
    fn tmap_round_trips_with_a_hidden_gain_map_grid() {
        let slices = [vec![0x26, 1, 1]];
        let mut w = HeifWriter::new();
        let base = w.add_image(&ImageSpec::new(640, 480, config(1), &slices));
        let tile_ids: Vec<ItemId> = (0..2)
            .map(|_| {
                w.add_image(&ImageSpec {
                    hidden: true,
                    ..ImageSpec::new(512, 512, config(0), &slices)
                })
            })
            .collect();
        let map = w.add_hidden_grid(1, 2, 320, 240, &tile_ids, None);
        let payload: Vec<u8> = (0..62).collect();
        let tmap = w.add_tmap(base, map, 640, 480, None, &payload);
        let bytes = w.finish();

        let file = HeifFile::parse(&bytes).unwrap();
        assert_eq!(file.primary_id, base);
        assert!(file.file_type.has_brand(b"tmap"));
        assert!(!file.item(tmap).unwrap().hidden);
        assert_eq!(file.referenced_items(tmap, b"dimg"), &[base, map]);
        assert_eq!(file.image_size(tmap).unwrap(), Some((640, 480)));
        assert_eq!(file.item_data(tmap).unwrap(), &payload[..]);
        assert!(file.item(map).unwrap().hidden);
        assert_eq!(file.image_size(map).unwrap(), Some((320, 240)));
        assert_eq!(file.referenced_items(map, b"dimg"), &tile_ids[..]);
        // The tone-mapped item and the base image are alternates, and the group ID does not
        // collide with an item ID.
        let at = bytes.windows(4).position(|w| w == b"altr").unwrap();
        let group_id = u32::from_be_bytes(bytes[at + 8..at + 12].try_into().unwrap());
        let last_item = file.items.iter().map(|item| item.id).max().unwrap();
        assert!(
            group_id > last_item,
            "group {group_id}, last item {last_item}"
        );
    }

    #[test]
    fn image_with_alpha_and_exif_round_trips() {
        let slices = [vec![0x26, 1, 1, 2, 3], vec![0x02, 1, 4]];
        let alpha_slices = [vec![0x26, 1, 9]];
        let color = ColorInfo::Nclx {
            primaries: 1,
            transfer: 13,
            matrix: 6,
            full_range: true,
        };
        let mut w = HeifWriter::new();
        let image = w.add_image(&ImageSpec {
            color: Some(color.clone()),
            crop: Some((639, 479)),
            ..ImageSpec::new(640, 480, config(1), &slices)
        });
        let alpha = w.add_alpha(image, &ImageSpec::new(640, 480, config(0), &alpha_slices));
        let exif = w.add_exif(image, b"MM\0*\0\0\0\x08");
        let xmp = w.add_xmp(image, b"<x:xmpmeta/>");
        w.add_icc_profile(image, b"icc profile");
        let bytes = w.finish();

        let file = HeifFile::parse(&bytes).unwrap();
        assert!(file.file_type.has_brand(b"heic"));
        assert_eq!(file.primary_id, image);
        assert_eq!(file.image_size(image).unwrap(), Some((640, 480)));
        assert_eq!(file.hevc_config(image).unwrap().unwrap(), &config(1));
        assert_eq!(file.hevc_config(alpha).unwrap().unwrap(), &config(0));
        assert!(
            file.item_properties(image)
                .unwrap()
                .any(|p| *p == Property::Color(color.clone()))
        );
        assert!(file.item_properties(image).unwrap().any(|p| *p
            == Property::CleanAperture {
                width: (639, 1),
                height: (479, 1),
                horiz_offset: (-1, 2),
                vert_offset: (-1, 2),
            }));
        assert!(file.item(alpha).unwrap().hidden);
        assert_eq!(file.referencing_items(image, b"auxl"), vec![alpha]);
        assert_eq!(file.exif(image).unwrap().unwrap(), b"MM\0*\0\0\0\x08");
        assert_eq!(file.referencing_items(image, b"cdsc"), vec![exif, xmp]);
        assert_eq!(file.xmp(image).unwrap().unwrap(), b"<x:xmpmeta/>");
        assert_eq!(file.icc_profile(image).unwrap().unwrap(), b"icc profile");
        // `clap` stays the last property of the image.
        assert!(matches!(
            file.item_properties(image).unwrap().last(),
            Some(Property::CleanAperture { .. })
        ));

        let stream = file.hevc_bitstream(image).unwrap();
        let mut expected = Vec::new();
        for nal in config(1).nal_units.iter().chain(&slices) {
            expected.extend_from_slice(&[0, 0, 0, 1]);
            expected.extend_from_slice(nal);
        }
        assert_eq!(stream, expected);
        // The `ispe` property is shared by the image and its alpha plane.
        let ispe = file
            .properties
            .iter()
            .filter(|p| matches!(p, Property::ImageSize { .. }))
            .count();
        assert_eq!(ispe, 1);
    }
}
