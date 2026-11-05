//! Region annotations (ISO/IEC 23008-12, region items of type `rgan`).
//!
//! A region item describes areas of an image (faces, objects, text, ...): points, rectangles,
//! ellipses, polygons, polylines and masks, in the coordinates of a reference size that the
//! image is scaled to. It describes its image through a `cdsc` reference; masks stored as
//! separate image items are linked by `mask` references, in the order of the referenced-mask
//! regions. A `udes` property on the region item gives it a name, a description and tags.
//!
//! The syntax follows libheif, which writes and reads these items: every count and size comes
//! from the file and is checked against the data before use.

use crate::Error;
use crate::FourCC;
use crate::reader::Reader;

/// The geometry of one region, in the coordinates of [`RegionItem::reference_width`] ×
/// [`RegionItem::reference_height`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Geometry {
    /// A single point.
    Point {
        /// Horizontal position.
        x: i32,
        /// Vertical position.
        y: i32,
    },
    /// A rectangle, from its top-left corner.
    Rectangle {
        /// Left edge.
        x: i32,
        /// Top edge.
        y: i32,
        /// Width.
        width: u32,
        /// Height.
        height: u32,
    },
    /// An ellipse, from its centre.
    Ellipse {
        /// Horizontal centre.
        x: i32,
        /// Vertical centre.
        y: i32,
        /// Horizontal radius.
        radius_x: u32,
        /// Vertical radius.
        radius_y: u32,
    },
    /// A polygon (`closed`) or a polyline, as its vertices.
    Polygon {
        /// Vertices, in order.
        points: Vec<(i32, i32)>,
        /// Whether the last vertex joins the first one (polygon) or not (polyline).
        closed: bool,
    },
    /// A mask stored as a separate image item, placed at a rectangle.
    ReferencedMask {
        /// Left edge.
        x: i32,
        /// Top edge.
        y: i32,
        /// Width.
        width: u32,
        /// Height.
        height: u32,
        /// The mask image item, from the region item's `mask` references, or `None` if the
        /// file does not name one.
        mask_item: Option<u32>,
    },
    /// A mask stored in the region item: one bit per pixel of a rectangle, rows not padded, the
    /// first pixel in the most significant bit; 1 means inside the region.
    InlineMask {
        /// Left edge.
        x: i32,
        /// Top edge.
        y: i32,
        /// Width.
        width: u32,
        /// Height.
        height: u32,
        /// The `(width * height + 7) / 8` mask bytes.
        bits: Vec<u8>,
    },
}

impl Geometry {
    /// For an inline mask, whether the pixel at (`col`, `row`) of its rectangle is inside the
    /// region; `None` for other geometries or outside the rectangle.
    pub fn mask_bit(&self, col: u32, row: u32) -> Option<bool> {
        match self {
            Self::InlineMask {
                width,
                height,
                bits,
                ..
            } if col < *width && row < *height => {
                let n = u64::from(row) * u64::from(*width) + u64::from(col);
                let byte = bits.get(usize::try_from(n / 8).ok()?)?;
                Some(byte & (0x80 >> (n % 8)) != 0)
            }
            _ => None,
        }
    }
}

/// A region item: the reference size its coordinates use, and its regions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionItem {
    /// Width of the coordinate space; the image is mapped onto it.
    pub reference_width: u32,
    /// Height of the coordinate space.
    pub reference_height: u32,
    /// The regions, in file order.
    pub regions: Vec<Geometry>,
}

/// A `udes` property: a user-readable name, description and tags, in a language.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserDescription {
    /// Language tag (RFC 5646), e.g. `en-US`; may be empty.
    pub lang: String,
    /// Short name.
    pub name: String,
    /// Longer description.
    pub description: String,
    /// Comma-separated tags.
    pub tags: String,
}

/// Geometry type codes of the region item syntax.
mod kind {
    pub const POINT: u8 = 0;
    pub const RECTANGLE: u8 = 1;
    pub const ELLIPSE: u8 = 2;
    pub const POLYGON: u8 = 3;
    pub const REFERENCED_MASK: u8 = 4;
    pub const INLINE_MASK: u8 = 5;
    pub const POLYLINE: u8 = 6;
}

fn invalid(reason: &'static str) -> Error {
    Error::InvalidBox {
        box_type: FourCC(*b"rgan"),
        reason,
    }
}

impl RegionItem {
    /// Parses the data of a region item. `mask_items` are the targets of its `mask`
    /// references, given in order to its referenced-mask regions.
    pub fn parse(data: &[u8], mask_items: &[u32]) -> Result<Self, Error> {
        let mut r = Reader::new(data);
        let _version = r.u8()?;
        let flags = r.u8()?;
        let wide = flags & 1 != 0;
        let unsigned = |r: &mut Reader<'_>| -> Result<u32, Error> {
            if wide {
                r.u32()
            } else {
                r.u16().map(u32::from)
            }
        };
        let signed = |r: &mut Reader<'_>| -> Result<i32, Error> {
            if wide {
                r.i32()
            } else {
                r.u16().map(|v| i32::from(v as i16))
            }
        };
        let reference_width = unsigned(&mut r)?;
        let reference_height = unsigned(&mut r)?;
        let count = r.u8()?;
        let mut masks = mask_items.iter().copied();
        let mut regions = Vec::with_capacity(usize::from(count));
        for _ in 0..count {
            let geometry = match r.u8()? {
                kind::POINT => Geometry::Point {
                    x: signed(&mut r)?,
                    y: signed(&mut r)?,
                },
                kind::RECTANGLE => Geometry::Rectangle {
                    x: signed(&mut r)?,
                    y: signed(&mut r)?,
                    width: unsigned(&mut r)?,
                    height: unsigned(&mut r)?,
                },
                kind::ELLIPSE => Geometry::Ellipse {
                    x: signed(&mut r)?,
                    y: signed(&mut r)?,
                    radius_x: unsigned(&mut r)?,
                    radius_y: unsigned(&mut r)?,
                },
                t @ (kind::POLYGON | kind::POLYLINE) => {
                    let closed = t == kind::POLYGON;
                    let n = unsigned(&mut r)? as usize;
                    let point_size = if wide { 8 } else { 4 };
                    if n > r.remaining() / point_size {
                        return Err(invalid("more polygon points than data"));
                    }
                    if n < if closed { 3 } else { 2 } {
                        return Err(invalid("too few polygon points"));
                    }
                    let mut points = Vec::with_capacity(n);
                    for _ in 0..n {
                        points.push((signed(&mut r)?, signed(&mut r)?));
                    }
                    Geometry::Polygon { points, closed }
                }
                kind::REFERENCED_MASK => Geometry::ReferencedMask {
                    x: signed(&mut r)?,
                    y: signed(&mut r)?,
                    width: unsigned(&mut r)?,
                    height: unsigned(&mut r)?,
                    mask_item: masks.next(),
                },
                kind::INLINE_MASK => {
                    let (x, y) = (signed(&mut r)?, signed(&mut r)?);
                    let (width, height) = (unsigned(&mut r)?, unsigned(&mut r)?);
                    if r.u8()? != 0 {
                        return Err(Error::InvalidBox {
                            box_type: FourCC(*b"rgan"),
                            reason: "compressed inline masks are not supported",
                        });
                    }
                    if width == 0 || height == 0 {
                        return Err(invalid("empty inline mask"));
                    }
                    let len = (u64::from(width) * u64::from(height)).div_ceil(8);
                    let len = usize::try_from(len)
                        .ok()
                        .filter(|&len| len <= r.remaining())
                        .ok_or_else(|| invalid("inline mask larger than its data"))?;
                    Geometry::InlineMask {
                        x,
                        y,
                        width,
                        height,
                        bits: r.bytes(len)?.to_vec(),
                    }
                }
                _ => return Err(invalid("unknown region geometry")),
            };
            regions.push(geometry);
        }
        Ok(Self {
            reference_width,
            reference_height,
            regions,
        })
    }

    /// The data of a region item, with 32-bit fields when a value needs them. Referenced-mask
    /// regions store no item: give their masks as `mask` references, in the same order.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        if self.regions.len() > 255 {
            return Err(invalid("more than 255 regions"));
        }
        let mut values: Vec<i64> = vec![
            i64::from(self.reference_width),
            i64::from(self.reference_height),
        ];
        for g in &self.regions {
            match g {
                Geometry::Point { x, y } => values.extend([i64::from(*x), i64::from(*y)]),
                Geometry::Rectangle {
                    x,
                    y,
                    width: a,
                    height: b,
                }
                | Geometry::Ellipse {
                    x,
                    y,
                    radius_x: a,
                    radius_y: b,
                }
                | Geometry::ReferencedMask {
                    x,
                    y,
                    width: a,
                    height: b,
                    ..
                }
                | Geometry::InlineMask {
                    x,
                    y,
                    width: a,
                    height: b,
                    ..
                } => values.extend([i64::from(*x), i64::from(*y), i64::from(*a), i64::from(*b)]),
                Geometry::Polygon { points, .. } => {
                    values.push(points.len() as i64);
                    values.extend(
                        points
                            .iter()
                            .flat_map(|&(x, y)| [i64::from(x), i64::from(y)]),
                    );
                }
            }
        }
        // The 16-bit form reads coordinates as signed: use it only when every value fits i16.
        let wide = !values.iter().all(|&v| (-32768..=32767).contains(&v));
        let mut out = vec![0, u8::from(wide)];
        let put = |out: &mut Vec<u8>, v: i64| {
            if wide {
                out.extend_from_slice(&(v as u32).to_be_bytes());
            } else {
                out.extend_from_slice(&(v as u16).to_be_bytes());
            }
        };
        put(&mut out, i64::from(self.reference_width));
        put(&mut out, i64::from(self.reference_height));
        out.push(self.regions.len() as u8);
        for g in &self.regions {
            match g {
                Geometry::Point { x, y } => {
                    out.push(kind::POINT);
                    put(&mut out, i64::from(*x));
                    put(&mut out, i64::from(*y));
                }
                Geometry::Polygon { points, closed } => {
                    out.push(if *closed {
                        kind::POLYGON
                    } else {
                        kind::POLYLINE
                    });
                    put(&mut out, points.len() as i64);
                    for &(x, y) in points {
                        put(&mut out, i64::from(x));
                        put(&mut out, i64::from(y));
                    }
                }
                Geometry::Rectangle {
                    x,
                    y,
                    width,
                    height,
                }
                | Geometry::Ellipse {
                    x,
                    y,
                    radius_x: width,
                    radius_y: height,
                }
                | Geometry::ReferencedMask {
                    x,
                    y,
                    width,
                    height,
                    ..
                }
                | Geometry::InlineMask {
                    x,
                    y,
                    width,
                    height,
                    ..
                } => {
                    out.push(match g {
                        Geometry::Rectangle { .. } => kind::RECTANGLE,
                        Geometry::Ellipse { .. } => kind::ELLIPSE,
                        Geometry::ReferencedMask { .. } => kind::REFERENCED_MASK,
                        _ => kind::INLINE_MASK,
                    });
                    for v in [
                        i64::from(*x),
                        i64::from(*y),
                        i64::from(*width),
                        i64::from(*height),
                    ] {
                        put(&mut out, v);
                    }
                    if let Geometry::InlineMask {
                        width,
                        height,
                        bits,
                        ..
                    } = g
                    {
                        let len = (u64::from(*width) * u64::from(*height)).div_ceil(8);
                        if *width == 0 || *height == 0 || bits.len() as u64 != len {
                            return Err(invalid("inline mask size does not match its bits"));
                        }
                        out.push(0); // mask coding method: uncompressed
                        out.extend_from_slice(bits);
                    }
                }
            }
        }
        Ok(out)
    }
}

impl UserDescription {
    /// Parses the content of a `udes` property box (a FullBox).
    pub fn parse(content: &[u8]) -> Result<Self, Error> {
        let mut r = Reader::new(content);
        if r.u8()? != 0 {
            return Err(Error::InvalidBox {
                box_type: FourCC(*b"udes"),
                reason: "unknown version",
            });
        }
        r.bytes(3)?; // flags
        let rest = r.rest();
        let mut strings = rest
            .split(|&b| b == 0)
            .map(|s| String::from_utf8_lossy(s).into_owned());
        Ok(Self {
            lang: strings.next().unwrap_or_default(),
            name: strings.next().unwrap_or_default(),
            description: strings.next().unwrap_or_default(),
            tags: strings.next().unwrap_or_default(),
        })
    }

    /// The content of a `udes` property box.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![0, 0, 0, 0];
        for s in [&self.lang, &self.name, &self.description, &self.tags] {
            out.extend(s.bytes().filter(|&b| b != 0));
            out.push(0);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RegionItem {
        RegionItem {
            reference_width: 4032,
            reference_height: 3024,
            regions: vec![
                Geometry::Point { x: 10, y: -5 },
                Geometry::Rectangle {
                    x: 100,
                    y: 200,
                    width: 300,
                    height: 400,
                },
                Geometry::Ellipse {
                    x: 50,
                    y: 60,
                    radius_x: 7,
                    radius_y: 8,
                },
                Geometry::Polygon {
                    points: vec![(0, 0), (10, 0), (5, 9)],
                    closed: true,
                },
                Geometry::Polygon {
                    points: vec![(1, 1), (2, 3)],
                    closed: false,
                },
                Geometry::ReferencedMask {
                    x: 0,
                    y: 0,
                    width: 64,
                    height: 64,
                    mask_item: Some(9),
                },
                Geometry::InlineMask {
                    x: 3,
                    y: 4,
                    width: 3,
                    height: 3,
                    bits: vec![0b1010_1010, 0b1000_0000],
                },
            ],
        }
    }

    #[test]
    fn round_trips_every_geometry() {
        let item = sample();
        let data = item.encode().unwrap();
        assert_eq!(data[1], 0, "16-bit fields suffice");
        assert_eq!(RegionItem::parse(&data, &[9]).unwrap(), item);

        let mut wide = item.clone();
        wide.reference_width = 100_000;
        let data = wide.encode().unwrap();
        assert_eq!(data[1], 1, "32-bit fields");
        assert_eq!(RegionItem::parse(&data, &[9]).unwrap(), wide);
    }

    #[test]
    fn inline_mask_bits_are_msb_first() {
        let item = sample();
        let mask = &item.regions[6];
        let inside: Vec<bool> = (0..9)
            .map(|n| mask.mask_bit(n % 3, n / 3).unwrap())
            .collect();
        assert_eq!(
            inside,
            [true, false, true, false, true, false, true, false, true]
        );
        assert_eq!(mask.mask_bit(3, 0), None);
        assert_eq!(item.regions[0].mask_bit(0, 0), None);
    }

    #[test]
    fn rejects_bad_data_without_panicking() {
        let data = sample().encode().unwrap();
        for end in 0..data.len() {
            assert!(RegionItem::parse(&data[..end], &[]).is_err(), "end {end}");
        }
        // 255 polygons each claiming 65535 points, in a few bytes.
        let mut huge = vec![0, 0, 0, 10, 0, 10, 255];
        for _ in 0..255 {
            huge.extend_from_slice(&[3, 0xFF, 0xFF]);
        }
        assert!(RegionItem::parse(&huge, &[]).is_err());
        assert!(
            RegionItem::parse(&[0, 0, 0, 1, 0, 1, 1, 9], &[]).is_err(),
            "unknown type"
        );
    }

    #[test]
    fn user_description_round_trip() {
        let d = UserDescription {
            lang: "fr-FR".into(),
            name: "Visage".into(),
            description: "Une personne".into(),
            tags: "face,person".into(),
        };
        assert_eq!(UserDescription::parse(&d.encode()).unwrap(), d);
        assert!(UserDescription::parse(&[1, 0, 0, 0]).is_err());
        assert_eq!(
            UserDescription::parse(&[0, 0, 0, 0]).unwrap(),
            UserDescription::default()
        );
    }
}
