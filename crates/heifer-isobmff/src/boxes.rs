//! Typed parsers for the boxes used by HEIF (ISO/IEC 23008-12) and ISOBMFF (ISO/IEC 14496-12).
//!
//! Each `parse` function takes the box *content* (everything after the box header).

use crate::reader::Reader;
use crate::{Error, FourCC};

fn invalid(box_type: &[u8; 4], reason: &'static str) -> Error {
    Error::InvalidBox {
        box_type: FourCC(*box_type),
        reason,
    }
}

/// `ftyp`: file type and compatible brands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileType {
    /// Main brand, e.g. `heic` or `mif1`.
    pub major_brand: FourCC,
    /// Version of the major brand.
    pub minor_version: u32,
    /// Other brands the file conforms to.
    pub compatible_brands: Vec<FourCC>,
}

impl FileType {
    /// Parses the content of an `ftyp` box.
    pub fn parse(content: &[u8]) -> Result<Self, Error> {
        let mut r = Reader::new(content);
        let major_brand = r.fourcc()?;
        let minor_version = r.u32()?;
        let mut compatible_brands = Vec::new();
        while r.remaining() >= 4 {
            compatible_brands.push(r.fourcc()?);
        }
        Ok(Self {
            major_brand,
            minor_version,
            compatible_brands,
        })
    }

    /// Whether the file declares `brand` as major or compatible brand.
    pub fn has_brand(&self, brand: &[u8; 4]) -> bool {
        self.major_brand.0 == *brand || self.compatible_brands.iter().any(|b| b.0 == *brand)
    }
}

/// `hdlr`: returns the handler type (`pict` for HEIF images).
pub fn parse_handler(content: &[u8]) -> Result<FourCC, Error> {
    let mut r = Reader::new(content);
    r.full_box()?;
    r.u32()?; // pre_defined
    r.fourcc()
}

/// `pitm`: returns the primary item ID.
pub fn parse_primary_item(content: &[u8]) -> Result<u32, Error> {
    let mut r = Reader::new(content);
    let (version, _) = r.full_box()?;
    if version == 0 {
        r.u16().map(u32::from)
    } else {
        r.u32()
    }
}

/// One `infe` entry of the `iinf` box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemInfo {
    /// Item ID.
    pub id: u32,
    /// Item type, e.g. `hvc1`, `grid`, `Exif`, `mime`.
    pub item_type: FourCC,
    /// Human-readable name (often empty).
    pub name: String,
    /// MIME content type, only for `mime` items.
    pub content_type: Option<String>,
    /// Whether the item is hidden (not meant to be displayed on its own).
    pub hidden: bool,
}

/// `iinf`: parses all `infe` entries. Only `infe` versions 2 and 3 are supported (required by HEIF).
pub fn parse_item_infos(content: &[u8]) -> Result<Vec<ItemInfo>, Error> {
    let mut r = Reader::new(content);
    let (version, _) = r.full_box()?;
    let count = if version == 0 {
        u32::from(r.u16()?)
    } else {
        r.u32()?
    };
    let children = r.rest();
    let mut items = Vec::with_capacity(count.min(4096) as usize);
    for child in crate::BoxIter::new(children) {
        let child = child?;
        if child.header.box_type.0 == *b"infe" {
            items.push(parse_item_info_entry(child.content)?);
        }
    }
    Ok(items)
}

fn parse_item_info_entry(content: &[u8]) -> Result<ItemInfo, Error> {
    let mut r = Reader::new(content);
    let (version, flags) = r.full_box()?;
    let id = match version {
        2 => u32::from(r.u16()?),
        3 => r.u32()?,
        _ => return Err(Error::Unimplemented("infe version < 2")),
    };
    r.u16()?; // item_protection_index
    let item_type = r.fourcc()?;
    let name = r.cstring();
    let content_type = (item_type.0 == *b"mime").then(|| r.cstring());
    Ok(ItemInfo {
        id,
        item_type,
        name,
        content_type,
        hidden: flags & 1 != 0,
    })
}

/// Where the bytes of an item are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstructionMethod {
    /// Offsets are relative to the start of the file.
    File,
    /// Offsets are relative to the content of the `idat` box.
    Idat,
    /// Data comes from another item (rarely used, not supported yet).
    Item,
}

/// One contiguous chunk of an item's data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    /// Absolute offset (base offset already added).
    pub offset: u64,
    /// Length in bytes. 0 means "until the end of the file / idat".
    pub length: u64,
}

/// One entry of the `iloc` box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemLocation {
    /// Item ID.
    pub id: u32,
    /// Where offsets point to.
    pub construction_method: ConstructionMethod,
    /// Data chunks, to be concatenated in order.
    pub extents: Vec<Extent>,
}

/// `iloc`: item locations. Field sizes are declared in the box header (4-bit values: 0, 4 or 8 bytes).
pub fn parse_item_locations(content: &[u8]) -> Result<Vec<ItemLocation>, Error> {
    let mut r = Reader::new(content);
    let (version, _) = r.full_box()?;
    if version > 2 {
        return Err(invalid(b"iloc", "unknown version"));
    }
    let sizes = r.u8()?;
    let (offset_size, length_size) = (sizes >> 4, sizes & 0x0F);
    let sizes = r.u8()?;
    let base_offset_size = sizes >> 4;
    let index_size = if version >= 1 { sizes & 0x0F } else { 0 };
    let count = if version < 2 {
        u32::from(r.u16()?)
    } else {
        r.u32()?
    };

    let mut items = Vec::with_capacity(count.min(4096) as usize);
    for _ in 0..count {
        let id = if version < 2 {
            u32::from(r.u16()?)
        } else {
            r.u32()?
        };
        let construction_method = if version >= 1 {
            match r.u16()? & 0x0F {
                0 => ConstructionMethod::File,
                1 => ConstructionMethod::Idat,
                2 => ConstructionMethod::Item,
                _ => return Err(invalid(b"iloc", "unknown construction method")),
            }
        } else {
            ConstructionMethod::File
        };
        r.u16()?; // data_reference_index: 0 = this file
        let base_offset = r.uint(base_offset_size)?;
        let extent_count = r.u16()?;
        let mut extents = Vec::with_capacity(usize::from(extent_count));
        for _ in 0..extent_count {
            r.uint(index_size)?; // extent_index, only used with construction method 2
            let offset = r.uint(offset_size)?;
            let length = r.uint(length_size)?;
            let offset = base_offset
                .checked_add(offset)
                .ok_or_else(|| invalid(b"iloc", "offset overflow"))?;
            extents.push(Extent { offset, length });
        }
        items.push(ItemLocation {
            id,
            construction_method,
            extents,
        });
    }
    Ok(items)
}

/// One reference of the `iref` box: `from` points to each item of `to`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemReference {
    /// Reference type: `dimg` (derived image), `thmb` (thumbnail), `cdsc` (metadata), `auxl` (auxiliary)...
    pub ref_type: FourCC,
    /// Referencing item.
    pub from: u32,
    /// Referenced items, in order (tile order for grids).
    pub to: Vec<u32>,
}

/// `iref`: item references.
pub fn parse_item_references(content: &[u8]) -> Result<Vec<ItemReference>, Error> {
    let mut r = Reader::new(content);
    let (version, _) = r.full_box()?;
    let mut refs = Vec::new();
    for child in crate::BoxIter::new(r.rest()) {
        let child = child?;
        let mut c = Reader::new(child.content);
        let id = |c: &mut Reader<'_>| {
            if version == 0 {
                c.u16().map(u32::from)
            } else {
                c.u32()
            }
        };
        let from = id(&mut c)?;
        let count = c.u16()?;
        let to = (0..count)
            .map(|_| id(&mut c))
            .collect::<Result<Vec<_>, _>>()?;
        refs.push(ItemReference {
            ref_type: child.header.box_type,
            from,
            to,
        });
    }
    Ok(refs)
}

/// One association of the `ipma` box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PropertyAssociation {
    /// 1-based index into the `ipco` property list (0 means "no property").
    pub index: u16,
    /// Whether a reader must understand this property to display the item.
    pub essential: bool,
}

/// `ipma`: for each item ID, the list of associated properties.
pub fn parse_property_associations(
    content: &[u8],
) -> Result<Vec<(u32, Vec<PropertyAssociation>)>, Error> {
    let mut r = Reader::new(content);
    let (version, flags) = r.full_box()?;
    let count = r.u32()?;
    let mut out = Vec::with_capacity(count.min(4096) as usize);
    for _ in 0..count {
        let id = if version < 1 {
            u32::from(r.u16()?)
        } else {
            r.u32()?
        };
        let n = r.u8()?;
        let mut assocs = Vec::with_capacity(usize::from(n));
        for _ in 0..n {
            let (essential, index) = if flags & 1 != 0 {
                let v = r.u16()?;
                (v & 0x8000 != 0, v & 0x7FFF)
            } else {
                let v = r.u8()?;
                (v & 0x80 != 0, u16::from(v & 0x7F))
            };
            assocs.push(PropertyAssociation { index, essential });
        }
        out.push((id, assocs));
    }
    Ok(out)
}

/// `hvcC`: HEVC decoder configuration (ISO/IEC 14496-15 §8.3.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HevcConfig {
    /// `general_profile_idc` (1 = Main, 2 = Main 10, 3 = Main Still Picture, 4 = RExt).
    pub profile_idc: u8,
    /// `general_level_idc`.
    pub level_idc: u8,
    /// 0 = monochrome, 1 = 4:2:0, 2 = 4:2:2, 3 = 4:4:4.
    pub chroma_format: u8,
    /// Luma bit depth (8, 10...).
    pub bit_depth_luma: u8,
    /// Chroma bit depth.
    pub bit_depth_chroma: u8,
    /// Size in bytes of the length prefix of each NAL unit in the item data (1, 2 or 4).
    pub nal_length_size: u8,
    /// Parameter sets (VPS, SPS, PPS, SEI...), without start codes, in declaration order.
    pub nal_units: Vec<Vec<u8>>,
}

impl HevcConfig {
    /// Parses the content of an `hvcC` box.
    pub fn parse(content: &[u8]) -> Result<Self, Error> {
        let mut r = Reader::new(content);
        if r.u8()? != 1 {
            return Err(invalid(b"hvcC", "configurationVersion must be 1"));
        }
        let profile_idc = r.u8()? & 0x1F;
        r.bytes(4 + 6)?; // compatibility flags + constraint flags
        let level_idc = r.u8()?;
        r.bytes(2 + 1)?; // min_spatial_segmentation_idc, parallelismType
        let chroma_format = r.u8()? & 0x03;
        let bit_depth_luma = (r.u8()? & 0x07) + 8;
        let bit_depth_chroma = (r.u8()? & 0x07) + 8;
        r.bytes(2)?; // avgFrameRate
        let nal_length_size = (r.u8()? & 0x03) + 1;
        if nal_length_size == 3 {
            return Err(invalid(b"hvcC", "NAL length size of 3 bytes is forbidden"));
        }
        let arrays = r.u8()?;
        let mut nal_units = Vec::new();
        for _ in 0..arrays {
            r.u8()?; // array_completeness + NAL_unit_type
            let n = r.u16()?;
            for _ in 0..n {
                let len = r.u16()?;
                nal_units.push(r.bytes(usize::from(len))?.to_vec());
            }
        }
        Ok(Self {
            profile_idc,
            level_idc,
            chroma_format,
            bit_depth_luma,
            bit_depth_chroma,
            nal_length_size,
            nal_units,
        })
    }
}

/// `colr`: colour information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColorInfo {
    /// Coded colour parameters (ITU-T H.273 code points).
    Nclx {
        /// Colour primaries (1 = BT.709, 9 = BT.2020, 12 = Display P3).
        primaries: u16,
        /// Transfer characteristics (13 = sRGB, 16 = PQ, 18 = HLG).
        transfer: u16,
        /// Matrix coefficients (1 = BT.709, 5/6 = BT.601, 9 = BT.2020).
        matrix: u16,
        /// Whether sample values use the full range.
        full_range: bool,
    },
    /// Embedded ICC profile (`prof` or `rICC`).
    Icc(Vec<u8>),
}

/// An item property, from the `ipco` box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Property {
    /// `hvcC`: HEVC decoder configuration.
    HevcConfig(HevcConfig),
    /// `ispe`: image width and height.
    ImageSize {
        /// Width in pixels.
        width: u32,
        /// Height in pixels.
        height: u32,
    },
    /// `colr`: colour information.
    Color(ColorInfo),
    /// `pixi`: bits per channel.
    PixelInfo(Vec<u8>),
    /// `irot`: anti-clockwise rotation, in multiples of 90°.
    Rotation(u8),
    /// `imir`: mirroring. `true` = horizontal axis (top-bottom flip), `false` = vertical axis (left-right flip).
    Mirror(bool),
    /// `clap`: clean aperture (crop), as fractions.
    CleanAperture {
        /// Width numerator / denominator.
        width: (u32, u32),
        /// Height numerator / denominator.
        height: (u32, u32),
        /// Horizontal offset numerator / denominator.
        horiz_offset: (i32, u32),
        /// Vertical offset numerator / denominator.
        vert_offset: (i32, u32),
    },
    /// `auxC`: auxiliary image type, e.g. `urn:mpeg:hevc:2015:auxid:1` (alpha).
    AuxiliaryType(String),
    /// Any other property, kept raw.
    Unknown(FourCC, Vec<u8>),
}

impl Property {
    /// Parses one property box.
    pub fn parse(box_type: FourCC, content: &[u8]) -> Result<Self, Error> {
        let mut r = Reader::new(content);
        Ok(match &box_type.0 {
            b"hvcC" => Self::HevcConfig(HevcConfig::parse(content)?),
            b"ispe" => {
                r.full_box()?;
                Self::ImageSize {
                    width: r.u32()?,
                    height: r.u32()?,
                }
            }
            b"colr" => match &r.fourcc()?.0 {
                b"nclx" => Self::Color(ColorInfo::Nclx {
                    primaries: r.u16()?,
                    transfer: r.u16()?,
                    matrix: r.u16()?,
                    full_range: r.u8()? & 0x80 != 0,
                }),
                b"prof" | b"rICC" => Self::Color(ColorInfo::Icc(r.rest().to_vec())),
                _ => Self::Unknown(box_type, content.to_vec()),
            },
            b"pixi" => {
                r.full_box()?;
                let n = r.u8()?;
                Self::PixelInfo(r.bytes(usize::from(n))?.to_vec())
            }
            b"irot" => Self::Rotation(r.u8()? & 0x03),
            b"imir" => Self::Mirror(r.u8()? & 0x01 != 0),
            b"clap" => Self::CleanAperture {
                width: (r.u32()?, r.u32()?),
                height: (r.u32()?, r.u32()?),
                horiz_offset: (r.i32()?, r.u32()?),
                vert_offset: (r.i32()?, r.u32()?),
            },
            b"auxC" => {
                r.full_box()?;
                Self::AuxiliaryType(r.cstring())
            }
            _ => Self::Unknown(box_type, content.to_vec()),
        })
    }
}

/// `ipco`: the list of properties, in order (indices in `ipma` are 1-based into this list).
pub fn parse_property_container(content: &[u8]) -> Result<Vec<Property>, Error> {
    crate::BoxIter::new(content)
        .map(|b| {
            let b = b?;
            Property::parse(b.header.box_type, b.content)
        })
        .collect()
}

/// Description of a `grid` derived image (stored as the grid item's data).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grid {
    /// Number of tile rows.
    pub rows: u16,
    /// Number of tile columns.
    pub columns: u16,
    /// Final width, after cropping the assembled tiles.
    pub output_width: u32,
    /// Final height, after cropping the assembled tiles.
    pub output_height: u32,
}

impl Grid {
    /// Parses the data of a `grid` item.
    pub fn parse(data: &[u8]) -> Result<Self, Error> {
        let mut r = Reader::new(data);
        if r.u8()? != 0 {
            return Err(invalid(b"grid", "unknown version"));
        }
        let flags = r.u8()?;
        let rows = u16::from(r.u8()?) + 1;
        let columns = u16::from(r.u8()?) + 1;
        let (output_width, output_height) = if flags & 1 != 0 {
            (r.u32()?, r.u32()?)
        } else {
            (u32::from(r.u16()?), u32::from(r.u16()?))
        };
        Ok(Self {
            rows,
            columns,
            output_width,
            output_height,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ftyp() {
        let f = FileType::parse(b"mif1\0\0\0\0mif1heic").unwrap();
        assert_eq!(f.major_brand, FourCC(*b"mif1"));
        assert!(f.has_brand(b"heic"));
        assert!(!f.has_brand(b"avif"));
    }

    #[test]
    fn iloc_version1_with_idat() {
        #[rustfmt::skip]
        let data = [
            1, 0, 0, 0,       // version 1, flags
            0x44, 0x40,       // offset_size=4, length_size=4, base_offset_size=4, index_size=0
            0, 1,             // item_count
            0, 7,             // item_ID
            0, 1,             // construction_method = idat
            0, 0,             // data_reference_index
            0, 0, 0, 100,     // base_offset
            0, 1,             // extent_count
            0, 0, 0, 5,       // extent_offset
            0, 0, 0, 8,       // extent_length
        ];
        let locs = parse_item_locations(&data).unwrap();
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].id, 7);
        assert_eq!(locs[0].construction_method, ConstructionMethod::Idat);
        assert_eq!(
            locs[0].extents,
            vec![Extent {
                offset: 105,
                length: 8
            }]
        );
    }

    #[test]
    fn ipma_small_and_large_indices() {
        #[rustfmt::skip]
        let small = [0, 0, 0, 0,  0, 0, 0, 1,  0, 9,  2,  0x81, 0x02];
        let a = parse_property_associations(&small).unwrap();
        assert_eq!(a[0].0, 9);
        assert_eq!(
            a[0].1,
            vec![
                PropertyAssociation {
                    index: 1,
                    essential: true
                },
                PropertyAssociation {
                    index: 2,
                    essential: false
                }
            ]
        );
        #[rustfmt::skip]
        let large = [0, 0, 0, 1,  0, 0, 0, 1,  0, 9,  1,  0x80, 0x90];
        let a = parse_property_associations(&large).unwrap();
        assert_eq!(
            a[0].1[0],
            PropertyAssociation {
                index: 0x90,
                essential: true
            }
        );
    }

    #[test]
    fn grid_16_and_32_bit() {
        assert_eq!(
            Grid::parse(&[0, 0, 1, 3, 0x0F, 0xC0, 0x0B, 0xD0]).unwrap(),
            Grid {
                rows: 2,
                columns: 4,
                output_width: 4032,
                output_height: 3024
            }
        );
        let g = Grid::parse(&[0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 2]).unwrap();
        assert_eq!((g.output_width, g.output_height), (65536, 2));
    }

    #[test]
    fn hvcc_rejects_bad_version() {
        assert!(matches!(
            HevcConfig::parse(&[2; 23]),
            Err(Error::InvalidBox { .. })
        ));
    }
}
