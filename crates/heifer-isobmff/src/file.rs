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

    /// Raw item data, with all extents concatenated.
    pub fn item_data(&self, id: ItemId) -> Result<Cow<'a, [u8]>, Error> {
        let item = self.item(id)?;
        let Some(loc) = &item.location else {
            return Ok(Cow::Borrowed(&[]));
        };
        let source = match loc.construction_method {
            ConstructionMethod::File => self.data,
            ConstructionMethod::Idat => self.idat.ok_or(Error::MissingBox(FourCC(*b"idat")))?,
            ConstructionMethod::Item => {
                return Err(Error::Unimplemented("iloc construction method 2 (item)"));
            }
        };
        let slice = |e: &boxes::Extent| -> Result<&'a [u8], Error> {
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
        };
        match loc.extents.as_slice() {
            [single] => Ok(Cow::Borrowed(slice(single)?)),
            extents => {
                // Resolve every extent first: the concatenated data cannot be larger than the
                // source (overlapping extents could otherwise make a small file expand to
                // gigabytes).
                let parts = extents.iter().map(slice).collect::<Result<Vec<_>, _>>()?;
                let total = parts
                    .iter()
                    .try_fold(0usize, |acc, p| acc.checked_add(p.len()));
                match total {
                    Some(total) if total <= source.len() => Ok(Cow::Owned(parts.concat())),
                    _ => Err(Error::InvalidBox {
                        box_type: FourCC(*b"iloc"),
                        reason: "item data larger than its source",
                    }),
                }
            }
        }
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
