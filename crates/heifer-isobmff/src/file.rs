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
                let mut out = Vec::new();
                for e in extents {
                    out.extend_from_slice(slice(e)?);
                }
                Ok(Cow::Owned(out))
            }
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
