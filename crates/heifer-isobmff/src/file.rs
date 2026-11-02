//! High-level view of a HEIF file: items, their properties, references and data.

use std::borrow::Cow;

use crate::boxes::{
    self, ConstructionMethod, FileType, Grid, HevcConfig, ItemLocation, ItemReference, Property,
    PropertyAssociation,
};
use crate::{BoxIter, Error, FourCC};

/// Identifier of an item inside a HEIF file.
pub type ItemId = u32;

/// An item: an image, a tile, a thumbnail, EXIF metadata...
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// Item ID.
    pub id: ItemId,
    /// Item type, e.g. `hvc1`, `grid`, `Exif`.
    pub item_type: FourCC,
    /// Item name (often empty).
    pub name: String,
    /// Whether the item is hidden (e.g. grid tiles).
    pub hidden: bool,
    /// MIME content type, for `mime` items (e.g. `application/rdf+xml` for XMP).
    pub content_type: Option<String>,
    /// Associated properties (indices into [`HeifFile::properties`]).
    pub properties: Vec<PropertyAssociation>,
    /// Where the item data is stored, if it has any.
    pub location: Option<ItemLocation>,
}

/// A parsed HEIF file. Item data is borrowed from the input buffer.
#[derive(Debug, Clone)]
pub struct HeifFile<'a> {
    data: &'a [u8],
    /// The `ftyp` box.
    pub file_type: FileType,
    /// ID of the primary item (the image to display).
    pub primary_id: ItemId,
    /// All items, in declaration order.
    pub items: Vec<Item>,
    /// All properties from `ipco`, in order.
    pub properties: Vec<Property>,
    /// All item references from `iref`.
    pub references: Vec<ItemReference>,
    idat: Option<&'a [u8]>,
}

impl<'a> HeifFile<'a> {
    /// Parses the container structure of a HEIF file.
    pub fn parse(data: &'a [u8]) -> Result<Self, Error> {
        let mut file_type = None;
        let mut meta = None;
        for b in BoxIter::new(data) {
            let b = b?;
            match &b.header.box_type.0 {
                b"ftyp" => file_type = Some(FileType::parse(b.content)?),
                b"meta" => meta = Some(b.content),
                _ => {}
            }
        }
        let file_type = file_type.ok_or(Error::MissingBox(FourCC(*b"ftyp")))?;
        let meta = meta.ok_or(Error::MissingBox(FourCC(*b"meta")))?;
        // `meta` is a FullBox: skip version and flags.
        let meta = meta.get(4..).ok_or(Error::UnexpectedEof)?;

        let mut handler = None;
        let mut primary_id = None;
        let mut infos = Vec::new();
        let mut locations = Vec::new();
        let mut references = Vec::new();
        let mut properties = Vec::new();
        let mut associations = Vec::new();
        let mut idat = None;

        for b in BoxIter::new(meta) {
            let b = b?;
            match &b.header.box_type.0 {
                b"hdlr" => handler = Some(boxes::parse_handler(b.content)?),
                b"pitm" => primary_id = Some(boxes::parse_primary_item(b.content)?),
                b"iinf" => infos = boxes::parse_item_infos(b.content)?,
                b"iloc" => locations = boxes::parse_item_locations(b.content)?,
                b"iref" => references = boxes::parse_item_references(b.content)?,
                b"idat" => idat = Some(b.content),
                b"iprp" => {
                    for child in BoxIter::new(b.content) {
                        let child = child?;
                        match &child.header.box_type.0 {
                            b"ipco" => {
                                properties = boxes::parse_property_container(child.content)?;
                            }
                            b"ipma" => associations
                                .extend(boxes::parse_property_associations(child.content)?),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }

        let handler = handler.ok_or(Error::MissingBox(FourCC(*b"hdlr")))?;
        if handler.0 != *b"pict" {
            return Err(Error::InvalidBox {
                box_type: FourCC(*b"hdlr"),
                reason: "handler type is not `pict`",
            });
        }
        let primary_id = primary_id.ok_or(Error::MissingBox(FourCC(*b"pitm")))?;

        let items: Vec<Item> = infos
            .into_iter()
            .map(|info| Item {
                id: info.id,
                properties: associations
                    .iter()
                    .find(|(id, _)| *id == info.id)
                    .map(|(_, a)| a.clone())
                    .unwrap_or_default(),
                location: locations.iter().find(|l| l.id == info.id).cloned(),
                item_type: info.item_type,
                name: info.name,
                hidden: info.hidden,
                content_type: info.content_type,
            })
            .collect();

        if !items.iter().any(|i| i.id == primary_id) {
            return Err(Error::UnknownItem(primary_id));
        }
        for item in &items {
            for a in &item.properties {
                if usize::from(a.index) > properties.len() {
                    return Err(Error::InvalidBox {
                        box_type: FourCC(*b"ipma"),
                        reason: "property index out of range",
                    });
                }
            }
        }

        Ok(Self {
            data,
            file_type,
            primary_id,
            items,
            properties,
            references,
            idat,
        })
    }

    /// Returns the item with the given ID.
    pub fn item(&self, id: ItemId) -> Result<&Item, Error> {
        self.items
            .iter()
            .find(|i| i.id == id)
            .ok_or(Error::UnknownItem(id))
    }

    /// Returns the primary item.
    pub fn primary_item(&self) -> Result<&Item, Error> {
        self.item(self.primary_id)
    }

    /// Properties associated with an item, in association order.
    pub fn item_properties(&self, id: ItemId) -> Result<impl Iterator<Item = &Property>, Error> {
        let item = self.item(id)?;
        Ok(item
            .properties
            .iter()
            .filter(|a| a.index > 0)
            .map(|a| &self.properties[usize::from(a.index) - 1]))
    }

    /// Image size from the `ispe` property.
    pub fn image_size(&self, id: ItemId) -> Result<Option<(u32, u32)>, Error> {
        Ok(self.item_properties(id)?.find_map(|p| match p {
            Property::ImageSize { width, height } => Some((*width, *height)),
            _ => None,
        }))
    }

    /// HEVC decoder configuration from the `hvcC` property.
    pub fn hevc_config(&self, id: ItemId) -> Result<Option<&HevcConfig>, Error> {
        Ok(self.item_properties(id)?.find_map(|p| match p {
            Property::HevcConfig(c) => Some(c),
            _ => None,
        }))
    }

    /// Items referenced by `from` with the given reference type (e.g. `dimg` for grid tiles).
    pub fn referenced_items(&self, from: ItemId, ref_type: &[u8; 4]) -> &[ItemId] {
        self.references
            .iter()
            .find(|r| r.from == from && r.ref_type.0 == *ref_type)
            .map_or(&[], |r| &r.to)
    }

    /// Items that reference `to` with the given reference type (e.g. `thmb` to find thumbnails of `to`).
    pub fn referencing_items(&self, to: ItemId, ref_type: &[u8; 4]) -> Vec<ItemId> {
        self.references
            .iter()
            .filter(|r| r.ref_type.0 == *ref_type && r.to.contains(&to))
            .map(|r| r.from)
            .collect()
    }

    /// Item IDs of the top-level images, in declaration order: the images meant to be displayed,
    /// as opposed to their tiles, thumbnails and auxiliary planes.
    ///
    /// An item is a top-level image when its type is an image type ([`is_image_item`]), it is not
    /// hidden, it is not an input of a derived image (a `dimg` reference, e.g. a grid tile), and it
    /// is not a thumbnail (`thmb`) or an auxiliary image such as an alpha plane (`auxl`).
    ///
    /// Most files hold a single image, so the list has one entry. Bursts, "collections" and Apple
    /// dynamic HEIF hold several. [`HeifFile::primary_id`] is always listed, first when the scan
    /// above did not already include it first.
    pub fn top_level_image_ids(&self) -> Vec<ItemId> {
        let is_input = |id: ItemId| {
            self.references
                .iter()
                .any(|r| r.ref_type.0 == *b"dimg" && r.to.contains(&id))
        };
        let is_helper = |id: ItemId| {
            self.references
                .iter()
                .any(|r| r.from == id && (r.ref_type.0 == *b"thmb" || r.ref_type.0 == *b"auxl"))
        };
        let mut ids: Vec<ItemId> = self
            .items
            .iter()
            .filter(|i| {
                !i.hidden && is_image_item(i.item_type) && !is_input(i.id) && !is_helper(i.id)
            })
            .map(|i| i.id)
            .collect();
        if !ids.contains(&self.primary_id) {
            ids.insert(0, self.primary_id);
        }
        ids
    }

    /// Raw item data, with all extents concatenated.
    ///
    /// With construction method 2 (`iloc` version 1 or 2), extents point into the data of the
    /// items the item references with `iloc` references, which may themselves be built that way.
    pub fn item_data(&self, id: ItemId) -> Result<Cow<'a, [u8]>, Error> {
        let mut budget = Budget {
            items: MAX_CONSTRUCTED_ITEMS,
            bytes: self.data.len().saturating_mul(2),
        };
        self.construct_item(id, 0, &mut budget)
    }

    fn construct_item(
        &self,
        id: ItemId,
        depth: u32,
        budget: &mut Budget,
    ) -> Result<Cow<'a, [u8]>, Error> {
        let item = self.item(id)?;
        let Some(loc) = &item.location else {
            return Ok(Cow::Borrowed(&[]));
        };
        let source = match loc.construction_method {
            ConstructionMethod::File => self.data,
            ConstructionMethod::Idat => self.idat.ok_or(Error::MissingBox(FourCC(*b"idat")))?,
            ConstructionMethod::Item => return self.construct_from_items(id, loc, depth, budget),
        };
        match loc.extents.as_slice() {
            [single] => Ok(Cow::Borrowed(extent_slice(source, single)?)),
            extents => {
                let parts = extents
                    .iter()
                    .map(|e| extent_slice(source, e))
                    .collect::<Result<Vec<_>, _>>()?;
                concat(&parts, source.len(), budget).map(Cow::Owned)
            }
        }
    }

    /// Construction method 2: each extent is a range of the data of one of the items that `id`
    /// references with `iloc` references (`extent_index`, 1-based).
    fn construct_from_items(
        &self,
        id: ItemId,
        loc: &ItemLocation,
        depth: u32,
        budget: &mut Budget,
    ) -> Result<Cow<'a, [u8]>, Error> {
        // Bounds reference cycles, and the work done for items referenced many times over.
        if depth >= MAX_CONSTRUCTION_DEPTH {
            return Err(invalid_iloc("item construction nested too deeply"));
        }
        budget.items = budget
            .items
            .checked_sub(1)
            .ok_or_else(|| invalid_iloc("too many items constructed from other items"))?;

        let refs = self.referenced_items(id, b"iloc");
        let source_index = |e: &boxes::Extent| -> Result<usize, Error> {
            usize::try_from(e.index.max(1) - 1)
                .ok()
                .filter(|&i| i < refs.len())
                .ok_or_else(|| invalid_iloc("extent_index without a matching `iloc` reference"))
        };
        // Each source item is constructed once, however many extents use it.
        let mut sources: Vec<Option<Cow<'a, [u8]>>> = vec![None; refs.len()];
        for e in &loc.extents {
            let i = source_index(e)?;
            if sources[i].is_none() {
                sources[i] = Some(self.construct_item(refs[i], depth + 1, budget)?);
            }
        }
        let source = |e: &boxes::Extent| -> Result<&[u8], Error> {
            Ok(sources[source_index(e)?].as_deref().unwrap_or_default())
        };

        if let [single] = loc.extents.as_slice()
            && let Some(Cow::Borrowed(data)) = sources[source_index(single)?]
        {
            return Ok(Cow::Borrowed(extent_slice(data, single)?));
        }
        let parts = loc
            .extents
            .iter()
            .map(|e| extent_slice(source(e)?, e))
            .collect::<Result<Vec<_>, _>>()?;
        concat(&parts, self.data.len(), budget).map(Cow::Owned)
    }

    /// Metadata items of type `item_type` describing `image` (`cdsc` references). When none
    /// describes `image` itself and it is a derived image, items describing its first input
    /// (e.g. the first tile of a grid) are used.
    fn metadata_items(&self, image: ItemId, accept: impl Fn(&Item) -> bool) -> Vec<ItemId> {
        let find = |id: ItemId| -> Vec<ItemId> {
            self.referencing_items(id, b"cdsc")
                .into_iter()
                .filter(|&m| self.item(m).is_ok_and(&accept))
                .collect()
        };
        let direct = find(image);
        if !direct.is_empty() {
            return direct;
        }
        self.referenced_items(image, b"dimg")
            .first()
            .map_or_else(Vec::new, |&first| find(first))
    }

    /// EXIF metadata of an image, as TIFF data (starting with `II*\0` or `MM\0*`).
    ///
    /// Note: HEIF orientation is given by the `irot`/`imir` properties; an EXIF orientation tag,
    /// if present, is informative only and must not be applied on top of them.
    pub fn exif(&self, image: ItemId) -> Result<Option<Vec<u8>>, Error> {
        for id in self.metadata_items(image, |i| i.item_type.0 == *b"Exif") {
            let data = self.item_data(id)?;
            // ExifDataBlock: exif_tiff_header_offset (u32), then the data.
            let Some(offset) = data
                .get(..4)
                .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize)
            else {
                continue;
            };
            let Some(mut tiff) = offset.checked_add(4).and_then(|start| data.get(start..)) else {
                continue;
            };
            // Some writers keep the JPEG APP1 "Exif\0\0" prefix.
            if tiff.starts_with(b"Exif\0\0") {
                tiff = &tiff[6..];
            }
            if tiff.starts_with(b"II*\0") || tiff.starts_with(b"MM\0*") {
                return Ok(Some(tiff.to_vec()));
            }
        }
        Ok(None)
    }

    /// XMP metadata of an image (an XML packet), from a `mime` item of type
    /// `application/rdf+xml`.
    pub fn xmp(&self, image: ItemId) -> Result<Option<Vec<u8>>, Error> {
        let accept = |i: &Item| {
            i.item_type.0 == *b"mime" && i.content_type.as_deref() == Some("application/rdf+xml")
        };
        match self.metadata_items(image, accept).first() {
            Some(&id) => Ok(Some(self.item_data(id)?.into_owned())),
            None => Ok(None),
        }
    }

    /// Embedded ICC profile of an image (`colr` property of type `prof`/`rICC`). For a grid
    /// without its own profile, the profile of its first tile is returned. Empty profiles are
    /// ignored.
    pub fn icc_profile(&self, image: ItemId) -> Result<Option<&[u8]>, Error> {
        let own = self.item_properties(image)?.find_map(|p| match p {
            Property::Color(boxes::ColorInfo::Icc(icc)) if !icc.is_empty() => Some(icc.as_slice()),
            _ => None,
        });
        if own.is_some() {
            return Ok(own);
        }
        match self.referenced_items(image, b"dimg").first() {
            Some(&first) if first != image => self.icc_profile(first),
            _ => Ok(None),
        }
    }

    /// Grid description of a `grid` item.
    pub fn grid(&self, id: ItemId) -> Result<Grid, Error> {
        let item = self.item(id)?;
        if item.item_type.0 != *b"grid" {
            return Err(Error::InvalidBox {
                box_type: item.item_type,
                reason: "item is not a grid",
            });
        }
        Grid::parse(&self.item_data(id)?)
    }

    /// Builds a complete HEVC Annex B bitstream for an `hvc1` item:
    /// the parameter sets from `hvcC` followed by the item's NAL units, each preceded by a
    /// `00 00 00 01` start code. This is what an HEVC decoder (or `ffmpeg`) expects.
    pub fn hevc_bitstream(&self, id: ItemId) -> Result<Vec<u8>, Error> {
        let config = self
            .hevc_config(id)?
            .ok_or(Error::MissingBox(FourCC(*b"hvcC")))?;
        let data = self.item_data(id)?;
        let mut out = Vec::with_capacity(data.len() + 256);
        for nal in &config.nal_units {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal);
        }
        let len_size = usize::from(config.nal_length_size);
        let mut rest: &[u8] = &data;
        while !rest.is_empty() {
            let (len_bytes, tail) = rest
                .split_at_checked(len_size)
                .ok_or(Error::UnexpectedEof)?;
            let len = len_bytes
                .iter()
                .fold(0usize, |acc, &b| (acc << 8) | usize::from(b));
            let (nal, tail) = tail.split_at_checked(len).ok_or(Error::UnexpectedEof)?;
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal);
            rest = tail;
        }
        Ok(out)
    }
}

/// Whether an item type is an image, coded or derived, as opposed to metadata (`Exif`, `mime`)
/// or another kind of helper.
///
/// Covers the standard image item types a HEIF file may hold: `hvc1`/`hev1` (HEVC), `avc1` (AVC),
/// `av01` (AV1), `jpeg`, `unci` (uncompressed), and the derived `grid`, `iovl` and `iden`.
pub fn is_image_item(item_type: FourCC) -> bool {
    let t = &item_type.0;
    t == b"hvc1"
        || t == b"hev1"
        || t == b"avc1"
        || t == b"av01"
        || t == b"jpeg"
        || t == b"unci"
        || t == b"grid"
        || t == b"iovl"
        || t == b"iden"
}

/// Maximum nesting of items constructed from other items (construction method 2).
const MAX_CONSTRUCTION_DEPTH: u32 = 8;
/// Maximum number of items constructed from other items for one [`HeifFile::item_data`] call.
const MAX_CONSTRUCTED_ITEMS: u32 = 256;

/// Limits on the work done by one [`HeifFile::item_data`] call.
struct Budget {
    /// Items that may still be constructed from other items.
    items: u32,
    /// Bytes that may still be copied.
    bytes: usize,
}

fn invalid_iloc(reason: &'static str) -> Error {
    Error::InvalidBox {
        box_type: FourCC(*b"iloc"),
        reason,
    }
}

/// The bytes of `source` covered by an extent.
fn extent_slice<'s>(source: &'s [u8], e: &boxes::Extent) -> Result<&'s [u8], Error> {
    let start = usize::try_from(e.offset).map_err(|_| Error::UnexpectedEof)?;
    let end = if e.length == 0 {
        source.len()
    } else {
        usize::try_from(e.length)
            .ok()
            .and_then(|l| start.checked_add(l))
            .ok_or(Error::UnexpectedEof)?
    };
    source.get(start..end).ok_or(Error::UnexpectedEof)
}

/// Concatenates extents. The result cannot be larger than `limit` (the size of their source),
/// so that overlapping extents cannot make a small file expand to gigabytes.
fn concat(parts: &[&[u8]], limit: usize, budget: &mut Budget) -> Result<Vec<u8>, Error> {
    let total = parts
        .iter()
        .try_fold(0usize, |acc, p| acc.checked_add(p.len()))
        .filter(|&total| total <= limit && total <= budget.bytes)
        .ok_or_else(|| invalid_iloc("item data larger than its source"))?;
    budget.bytes -= total;
    Ok(parts.concat())
}
