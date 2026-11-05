//! Decoding from a reader (`Read + Seek`) instead of a byte slice.
//!
//! Only the `ftyp` and `meta` boxes and the data of the items a call needs are read: the
//! primary image, its grid tiles and other inputs, its auxiliary planes and its metadata.
//! Thumbnails, other images and any unused bytes are skipped by seeking, so the caller does not
//! have to hold the whole file in memory. The needed item data are gathered into one buffer
//! and decoded by the same code as [`decode_with_options`](crate::decode_with_options), so the
//! output is identical.

use std::io::{Read, Seek, SeekFrom};

use heifer_isobmff::boxes::ConstructionMethod;
use heifer_isobmff::{HeifFile, ItemId};

use crate::{Error, Image, Metadata, Options, decode_item_with, metadata::item_metadata};

/// Largest `meta` box accepted, which is read whole: real files keep it far below.
const MAX_META: u64 = 64 << 20;

/// Most top-level boxes walked before giving up.
const MAX_TOP_LEVEL_BOXES: usize = 4096;

/// Errors of the reader-based API: reading failed, or the file could not be decoded.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// The reader returned an error, or ended before the data a box or item declares.
    #[error("read: {0}")]
    Io(#[from] std::io::Error),
    /// The file is not a decodable HEIF file.
    #[error(transparent)]
    Heif(#[from] Error),
}

/// Like [`decode`](crate::decode), reading the file from `reader` (see the module
/// documentation).
pub fn decode_reader<R: Read + Seek>(reader: R) -> Result<Image, ReadError> {
    decode_reader_with_options(reader, &Options::default())
}

/// Like [`decode_with_options`](crate::decode_with_options), reading the file from `reader`.
pub fn decode_reader_with_options<R: Read + Seek>(
    reader: R,
    options: &Options,
) -> Result<Image, ReadError> {
    let loaded = Loaded::read(reader, Need::PrimaryImage)?;
    let file = loaded.file()?;
    Ok(decode_item_with(&file, file.primary_id, 0, options)?)
}

/// Like [`read_metadata`](crate::read_metadata), reading only the container and the metadata
/// items from `reader`.
pub fn read_metadata_reader<R: Read + Seek>(reader: R) -> Result<Metadata, ReadError> {
    let loaded = Loaded::read(reader, Need::Metadata)?;
    let file = loaded.file()?;
    Ok(item_metadata(&file, file.primary_id)?)
}

/// Which items a call reads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Need {
    /// The primary image, its inputs, auxiliary planes and metadata.
    PrimaryImage,
    /// The metadata items of the primary image only.
    Metadata,
}

/// The `ftyp` and `meta` boxes, followed by an `mdat` box holding the needed item data, and
/// where each needed item's extents now are.
struct Loaded {
    buffer: Vec<u8>,
    /// New extents of the items read from the file; other file-stored items are dropped.
    locations: Vec<(ItemId, Vec<heifer_isobmff::boxes::Extent>)>,
}

impl Loaded {
    fn read<R: Read + Seek>(mut reader: R, need: Need) -> Result<Self, ReadError> {
        let end = reader.seek(SeekFrom::End(0))?;
        let head = top_level_boxes(&mut reader, end)?;

        // Which items to read, from the container alone.
        let file = HeifFile::parse(&head).map_err(Error::from)?;
        let wanted = wanted_items(&file, need);

        // Read their extents, in file order, into the data of an `mdat` box.
        let mut data = Vec::new();
        let mut moved = Vec::new();
        for &id in &wanted {
            let item = file.item(id).map_err(Error::from)?;
            let Some(loc) = &item.location else { continue };
            if loc.construction_method != ConstructionMethod::File {
                continue;
            }
            let mut extents = Vec::with_capacity(loc.extents.len());
            for e in &loc.extents {
                let length = if e.length == 0 {
                    end.checked_sub(e.offset).ok_or(Error::Invalid(
                        "item extent starts past the end of the file",
                    ))?
                } else {
                    e.length
                };
                if e.offset.checked_add(length).is_none_or(|stop| stop > end) {
                    return Err(Error::Invalid("item extent runs past the end of the file").into());
                }
                let at = data.len() as u64;
                reader.seek(SeekFrom::Start(e.offset))?;
                let length_usize =
                    usize::try_from(length).map_err(|_| Error::Invalid("item extent too large"))?;
                data.resize(data.len() + length_usize, 0);
                reader.read_exact(&mut data[at as usize..])?;
                extents.push(heifer_isobmff::boxes::Extent {
                    offset: at,
                    length,
                    index: e.index,
                });
            }
            moved.push((id, extents));
        }

        // `mdat` header: 16 bytes with a 64-bit size, so any data size fits.
        let base = head.len() as u64 + 16;
        let mut buffer = head;
        buffer.extend_from_slice(&1u32.to_be_bytes());
        buffer.extend_from_slice(b"mdat");
        buffer.extend_from_slice(&(16 + data.len() as u64).to_be_bytes());
        buffer.extend_from_slice(&data);
        for (_, extents) in &mut moved {
            for e in extents {
                e.offset += base;
            }
        }
        Ok(Self {
            buffer,
            locations: moved,
        })
    }

    /// The container, with the needed items pointing into the buffer and the other
    /// file-stored items without data.
    fn file(&self) -> Result<HeifFile<'_>, Error> {
        let mut file = HeifFile::parse(&self.buffer)?;
        for item in &mut file.items {
            let Some(loc) = &mut item.location else {
                continue;
            };
            if loc.construction_method != ConstructionMethod::File {
                continue;
            }
            match self.locations.iter().find(|(id, _)| *id == item.id) {
                Some((_, extents)) => loc.extents = extents.clone(),
                None => item.location = None,
            }
        }
        Ok(file)
    }
}

/// The `ftyp` and `meta` boxes of the file, read whole; every other top-level box is skipped.
fn top_level_boxes<R: Read + Seek>(reader: &mut R, end: u64) -> Result<Vec<u8>, ReadError> {
    let mut head = Vec::new();
    let mut at = 0u64;
    for _ in 0..MAX_TOP_LEVEL_BOXES {
        if at >= end {
            break;
        }
        reader.seek(SeekFrom::Start(at))?;
        let mut header = [0u8; 16];
        let available = (end - at).min(16) as usize;
        reader.read_exact(&mut header[..available.min(8)])?;
        if available < 8 {
            break;
        }
        let size32 = u32::from_be_bytes(header[..4].try_into().expect("4 bytes"));
        let kind = [header[4], header[5], header[6], header[7]];
        let size = match size32 {
            0 => end - at,
            1 => {
                if available < 16 {
                    return Err(Error::Invalid("truncated box header").into());
                }
                reader.read_exact(&mut header[8..16])?;
                u64::from_be_bytes(header[8..16].try_into().expect("8 bytes"))
            }
            n => u64::from(n),
        };
        if size < 8 || at.checked_add(size).is_none_or(|stop| stop > end) {
            return Err(Error::Invalid("top-level box size out of range").into());
        }
        if &kind == b"ftyp" || &kind == b"meta" {
            if size > MAX_META {
                return Err(Error::Invalid("meta box too large").into());
            }
            reader.seek(SeekFrom::Start(at))?;
            let start = head.len();
            head.resize(start + size as usize, 0);
            reader.read_exact(&mut head[start..])?;
        }
        at += size;
    }
    Ok(head)
}

/// The items a call needs: for an image, the primary item and, transitively, its derived
/// image inputs (`dimg`), the items its data is built from (`iloc`) and its auxiliary planes
/// (`auxl`); for metadata, the items describing the primary image (`cdsc`). Both include the
/// metadata items, which decoding may read.
fn wanted_items(file: &HeifFile<'_>, need: Need) -> Vec<ItemId> {
    let primary = file.primary_id;
    let mut wanted = file.referencing_items(primary, b"cdsc");
    // A grid's metadata can describe its first tile instead.
    for &tile in file.referenced_items(primary, b"dimg") {
        wanted.extend(file.referencing_items(tile, b"cdsc"));
    }
    if need == Need::PrimaryImage {
        let mut pending = vec![primary];
        while let Some(id) = pending.pop() {
            if wanted.contains(&id) && id != primary {
                continue;
            }
            if !wanted.contains(&id) {
                wanted.push(id);
            }
            for next in file
                .referenced_items(id, b"dimg")
                .iter()
                .chain(file.referenced_items(id, b"iloc"))
                .copied()
                .chain(file.referencing_items(id, b"auxl"))
            {
                if !wanted.contains(&next) && !pending.contains(&next) {
                    pending.push(next);
                }
            }
        }
    }
    wanted.sort_unstable();
    wanted.dedup();
    wanted
}
