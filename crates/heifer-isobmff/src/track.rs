//! Image sequence tracks (`moov`/`trak`, HEIF image sequences and bursts).
//!
//! A sequence stores its pictures as the samples of a track: the sample tables give each
//! sample's position, size and duration, and which samples are sync samples (pictures that
//! decode without the others). Every count comes from the file and is checked against the size
//! of its box before anything is allocated.

use crate::boxes::HevcConfig;
use crate::header::BoxIter;
use crate::reader::Reader;
use crate::{Error, FourCC};

/// One sample (picture) of a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Sample {
    /// Absolute offset of the sample data in the file.
    pub offset: u64,
    /// Size of the sample data in bytes.
    pub size: u32,
    /// Duration, in the track's [`Track::timescale`] units.
    pub duration: u32,
    /// Whether the sample is a sync sample, decodable without the other samples.
    pub sync: bool,
    /// Composition offset (`ctts`): display time minus decoding time, in
    /// [`Track::timescale`] units. Non-zero when frames are stored out of display order
    /// (B frames).
    pub composition_offset: i32,
}

/// A track of a HEIF file: an image sequence (`pict` handler) or a video (`vide`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Track {
    /// Track ID (`tkhd`).
    pub id: u32,
    /// Handler type, e.g. `pict` for image sequences.
    pub handler: FourCC,
    /// Visual width and height of the track (`tkhd`, integer part).
    pub width: u32,
    /// Height, see [`width`](Self::width).
    pub height: u32,
    /// Time units per second of sample durations (`mdhd`).
    pub timescale: u32,
    /// Coding of the samples: the sample entry type, e.g. `hvc1`.
    pub coding: FourCC,
    /// HEVC configuration of the samples, for `hvc1`/`hev1` sample entries.
    pub hevc_config: Option<HevcConfig>,
    /// The samples, in decoding order.
    pub samples: Vec<Sample>,
    /// Track references (`tref`): type and referenced track IDs, e.g. `thmb`.
    pub references: Vec<(FourCC, Vec<u32>)>,
}

impl Track {
    /// Total duration of the samples, in seconds.
    pub fn duration_seconds(&self) -> f64 {
        if self.timescale == 0 {
            return 0.0;
        }
        let total: u64 = self.samples.iter().map(|s| u64::from(s.duration)).sum();
        total as f64 / f64::from(self.timescale)
    }

    /// Display (composition) time of each sample, in [`timescale`](Self::timescale) units:
    /// its decoding time plus its composition offset.
    pub fn composition_times(&self) -> Vec<i64> {
        let mut decode_time = 0i64;
        self.samples
            .iter()
            .map(|s| {
                let t = decode_time + i64::from(s.composition_offset);
                decode_time += i64::from(s.duration);
                t
            })
            .collect()
    }

    /// The sample indices in display order (by composition time; samples with equal times
    /// keep their decoding order).
    pub fn display_order(&self) -> Vec<usize> {
        let times = self.composition_times();
        let mut order: Vec<usize> = (0..self.samples.len()).collect();
        order.sort_by_key(|&i| times[i]);
        order
    }

    /// The data of sample `index` in `file` (the whole file the track was parsed from).
    pub fn sample_data<'a>(&self, file: &'a [u8], index: usize) -> Result<&'a [u8], Error> {
        let s = self.samples.get(index).ok_or(Error::UnexpectedEof)?;
        let start = usize::try_from(s.offset).map_err(|_| Error::UnexpectedEof)?;
        start
            .checked_add(s.size as usize)
            .and_then(|end| file.get(start..end))
            .ok_or(Error::UnexpectedEof)
    }
}

fn invalid(box_type: &[u8; 4], reason: &'static str) -> Error {
    Error::InvalidBox {
        box_type: FourCC(*box_type),
        reason,
    }
}

/// Most samples accepted in one track.
const MAX_SAMPLES: usize = 1 << 22;

/// Parses the tracks of the `moov` box of a file, or none if it has no `moov`.
pub fn parse_tracks(data: &[u8]) -> Result<Vec<Track>, Error> {
    let mut tracks = Vec::new();
    for b in BoxIter::new(data) {
        let b = b?;
        if b.header.box_type.0 != *b"moov" {
            continue;
        }
        for t in BoxIter::new(b.content) {
            let t = t?;
            if t.header.box_type.0 == *b"trak" {
                tracks.push(parse_track(t.content)?);
            }
        }
    }
    Ok(tracks)
}

/// The child box `kind` of `content`, if any.
fn child<'a>(content: &'a [u8], kind: &[u8; 4]) -> Result<Option<&'a [u8]>, Error> {
    for b in BoxIter::new(content) {
        let b = b?;
        if b.header.box_type.0 == *kind {
            return Ok(Some(b.content));
        }
    }
    Ok(None)
}

fn required<'a>(content: &'a [u8], kind: &[u8; 4]) -> Result<&'a [u8], Error> {
    child(content, kind)?.ok_or(Error::MissingBox(FourCC(*kind)))
}

/// A FullBox's version, then a reader after its flags.
fn full_box(content: &[u8]) -> Result<(u8, Reader<'_>), Error> {
    let mut r = Reader::new(content);
    let version = r.u8()?;
    r.bytes(3)?;
    Ok((version, r))
}

/// An entry count, checked against the bytes left for `entry_size`-byte entries.
fn count(r: &mut Reader<'_>, entry_size: usize, kind: &[u8; 4]) -> Result<usize, Error> {
    let n = r.u32()? as usize;
    if n > r.remaining() / entry_size {
        return Err(invalid(kind, "more entries than data"));
    }
    Ok(n)
}

fn parse_track(trak: &[u8]) -> Result<Track, Error> {
    // tkhd: track ID and visual size.
    let (version, mut r) = full_box(required(trak, b"tkhd")?)?;
    let skip_times = if version == 1 { 16 } else { 8 };
    r.bytes(skip_times)?;
    let id = r.u32()?;
    r.bytes(4)?; // reserved
    r.bytes(if version == 1 { 8 } else { 4 })?; // duration
    r.bytes(8 + 2 + 2 + 2 + 2 + 36)?; // reserved, layer, group, volume, reserved, matrix
    let width = r.u32()? >> 16;
    let height = r.u32()? >> 16;

    let mut references = Vec::new();
    if let Some(tref) = child(trak, b"tref")? {
        for b in BoxIter::new(tref) {
            let b = b?;
            let ids = b
                .content
                .chunks_exact(4)
                .map(|c| u32::from_be_bytes(c.try_into().expect("4 bytes")))
                .collect();
            references.push((b.header.box_type, ids));
        }
    }

    let mdia = required(trak, b"mdia")?;
    let (version, mut r) = full_box(required(mdia, b"mdhd")?)?;
    r.bytes(if version == 1 { 16 } else { 8 })?; // creation and modification times
    let timescale = r.u32()?;
    let (_, mut r) = full_box(required(mdia, b"hdlr")?)?;
    r.bytes(4)?; // pre_defined
    let handler = r.fourcc()?;

    let stbl = required(required(mdia, b"minf")?, b"stbl")?;

    // stsd: the first sample entry.
    let (_, mut r) = full_box(required(stbl, b"stsd")?)?;
    let _entries = r.u32()?;
    let entries = r.rest();
    let mut coding = FourCC(*b"    ");
    let mut hevc_config = None;
    if let Some(entry) = BoxIter::new(entries).next() {
        let entry = entry?;
        coding = entry.header.box_type;
        // VisualSampleEntry: 78 bytes before its child boxes.
        if matches!(&coding.0, b"hvc1" | b"hev1") {
            let children = entry
                .content
                .get(78..)
                .ok_or(invalid(b"stsd", "truncated visual sample entry"))?;
            if let Some(hvcc) = child(children, b"hvcC")? {
                hevc_config = Some(HevcConfig::parse(hvcc)?);
            }
        }
    }

    // stsz: sample sizes.
    let (_, mut r) = full_box(required(stbl, b"stsz")?)?;
    let uniform = r.u32()?;
    let sample_count = r.u32()? as usize;
    if sample_count > MAX_SAMPLES {
        return Err(invalid(b"stsz", "too many samples"));
    }
    let sizes: Vec<u32> = if uniform != 0 {
        vec![uniform; sample_count]
    } else {
        if sample_count > r.remaining() / 4 {
            return Err(invalid(b"stsz", "more entries than data"));
        }
        (0..sample_count)
            .map(|_| r.u32())
            .collect::<Result<_, _>>()?
    };

    // stco / co64: chunk offsets.
    let offsets: Vec<u64> = if let Some(stco) = child(stbl, b"stco")? {
        let (_, mut r) = full_box(stco)?;
        let n = count(&mut r, 4, b"stco")?;
        (0..n)
            .map(|_| r.u32().map(u64::from))
            .collect::<Result<_, _>>()?
    } else {
        let (_, mut r) = full_box(required(stbl, b"co64")?)?;
        let n = count(&mut r, 8, b"co64")?;
        (0..n).map(|_| r.u64()).collect::<Result<_, _>>()?
    };

    // stsc: samples per chunk, as runs (first chunk, samples per chunk).
    let (_, mut r) = full_box(required(stbl, b"stsc")?)?;
    let n = count(&mut r, 12, b"stsc")?;
    let mut runs = Vec::with_capacity(n);
    for _ in 0..n {
        let first = r.u32()?;
        let per_chunk = r.u32()?;
        r.u32()?; // sample_description_index
        runs.push((first, per_chunk));
    }

    // Sample offsets: walk the chunks, each holding its run's number of samples.
    let mut samples = Vec::with_capacity(sample_count);
    'chunks: for (chunk, &chunk_offset) in offsets.iter().enumerate() {
        let number = chunk as u32 + 1;
        let per_chunk = runs
            .iter()
            .rev()
            .find(|&&(first, _)| first <= number)
            .map_or(0, |&(_, n)| n);
        let mut offset = chunk_offset;
        for _ in 0..per_chunk {
            let Some(&size) = sizes.get(samples.len()) else {
                break 'chunks;
            };
            samples.push(Sample {
                offset,
                size,
                duration: 0,
                sync: true,
                composition_offset: 0,
            });
            offset = offset
                .checked_add(u64::from(size))
                .ok_or(invalid(b"stco", "sample offset overflow"))?;
        }
    }
    if samples.len() != sample_count {
        return Err(invalid(b"stsc", "chunks hold fewer samples than stsz"));
    }

    // stts: durations, as runs.
    let (_, mut r) = full_box(required(stbl, b"stts")?)?;
    let n = count(&mut r, 8, b"stts")?;
    let mut index = 0;
    for _ in 0..n {
        let run = r.u32()? as usize;
        let delta = r.u32()?;
        for s in samples.iter_mut().skip(index).take(run) {
            s.duration = delta;
        }
        index = index.saturating_add(run);
    }

    // ctts: composition offsets, as runs. Version 0 offsets are unsigned, but writers store
    // negative ones there too, so both versions are read as signed.
    if let Some(ctts) = child(stbl, b"ctts")? {
        let (_, mut r) = full_box(ctts)?;
        let n = count(&mut r, 8, b"ctts")?;
        let mut index = 0usize;
        for _ in 0..n {
            let run = r.u32()? as usize;
            let offset = r.u32()? as i32;
            for s in samples.iter_mut().skip(index).take(run) {
                s.composition_offset = offset;
            }
            index = index.saturating_add(run);
        }
    }

    // stss: sync samples (1-based); without it, every sample is a sync sample.
    if let Some(stss) = child(stbl, b"stss")? {
        let (_, mut r) = full_box(stss)?;
        let n = count(&mut r, 4, b"stss")?;
        for s in &mut samples {
            s.sync = false;
        }
        for _ in 0..n {
            let number = r.u32()? as usize;
            if let Some(s) = number.checked_sub(1).and_then(|i| samples.get_mut(i)) {
                s.sync = true;
            }
        }
    }

    Ok(Track {
        id,
        handler,
        width,
        height,
        timescale,
        coding,
        hevc_config,
        samples,
        references,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(offsets: &[i32]) -> Track {
        Track {
            id: 1,
            handler: FourCC(*b"pict"),
            width: 0,
            height: 0,
            timescale: 25,
            coding: FourCC(*b"hvc1"),
            hevc_config: None,
            samples: offsets
                .iter()
                .map(|&composition_offset| Sample {
                    offset: 0,
                    size: 0,
                    duration: 1,
                    sync: false,
                    composition_offset,
                })
                .collect(),
            references: Vec::new(),
        }
    }

    #[test]
    fn display_order_follows_composition_times() {
        // I P B B stored in decoding order: I0 P3 B1 B2, shown at 1, 4, 2, 3 (offset 1).
        let t = track(&[1, 3, 0, 0]);
        assert_eq!(t.composition_times(), [1, 4, 2, 3]);
        assert_eq!(t.display_order(), [0, 2, 3, 1]);
        // Negative offsets (version 1, or version 0 written as signed).
        assert_eq!(track(&[0, 2, -1, -1]).display_order(), [0, 2, 3, 1]);
        // Without ctts, display order is decoding order.
        assert_eq!(track(&[0; 3]).display_order(), [0, 1, 2]);
    }
}
