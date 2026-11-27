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

/// One segment of a track's edit list (`elst`): which part of the media is shown, in the
/// track's [`Track::timescale`] units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Edit {
    /// How long the segment lasts.
    pub duration: u64,
    /// Composition time where the segment starts in the media; -1 for an empty edit, which
    /// shows nothing.
    pub media_time: i64,
    /// Media rate 0: the picture at `media_time` is held for `duration`.
    pub dwell: bool,
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
    /// Auxiliary track type (`auxi`), e.g. `urn:mpeg:hevc:2015:auxid:1` (alpha).
    pub auxiliary_type: Option<String>,
    /// The samples, in decoding order.
    pub samples: Vec<Sample>,
    /// Track references (`tref`): type and referenced track IDs, e.g. `thmb`.
    pub references: Vec<(FourCC, Vec<u32>)>,
    /// Edit list (`edts`/`elst`), empty when the track has none.
    pub edits: Vec<Edit>,
    /// Repeat flag of the edit list: it plays again until [`presented_duration`](Self::presented_duration) is over.
    pub repeat_edits: bool,
    /// Duration of the track's presentation (`tkhd`), in [`timescale`](Self::timescale)
    /// units; `None` when indefinite.
    pub presented_duration: Option<u64>,
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
    /// keep their decoding order). Samples with a composition offset of -2^31 are not displayed:
    /// they are only references for other samples, and left out.
    pub fn display_order(&self) -> Vec<usize> {
        let times = self.composition_times();
        let mut order: Vec<usize> = (0..self.samples.len())
            .filter(|&i| self.samples[i].composition_offset != NOT_DISPLAYED)
            .collect();
        order.sort_by_key(|&i| times[i]);
        order
    }

    /// The frames as presented, in order: sample index and how long it is shown, in
    /// [`timescale`](Self::timescale) units. Without an edit list, the samples of
    /// [`display_order`](Self::display_order); with one, the part of the media each edit
    /// selects, so samples can be skipped or shown again.
    ///
    /// Leading empty edits are the [`start_delay`](Self::start_delay); a later one keeps the
    /// previous frame on screen. With the repeat flag, the edit list plays again until
    /// [`presented_duration`](Self::presented_duration), or once when that is indefinite (see
    /// [`repeats_forever`](Self::repeats_forever)).
    pub fn presentation(&self) -> Vec<(usize, u64)> {
        let times = self.composition_times();
        let order = self.display_order();
        // Each displayed sample is shown from its composition time until the next one's.
        let spans: Vec<(usize, i64, i64)> = order
            .iter()
            .enumerate()
            .map(|(k, &i)| {
                let start = times[i];
                let end = order.get(k + 1).map_or_else(
                    || start.saturating_add(i64::from(self.samples[i].duration)),
                    |&next| times[next],
                );
                (i, start, end)
            })
            .collect();
        let mut out: Vec<(usize, u64)> = Vec::new();
        let end = self.presented_duration.filter(|_| self.repeat_edits);
        let pass = self
            .edits
            .iter()
            .fold(0u64, |t, e| t.saturating_add(e.duration));
        let passes = if end.is_some() && pass > 0 {
            usize::MAX
        } else {
            1
        };
        // Repeated edits that show nothing must not loop for long either.
        let mut budget = MAX_SAMPLES;
        let mut elapsed = 0u64;
        'passes: for _ in 0..passes {
            for edit in &self.edits {
                if end.is_some_and(|end| elapsed >= end) || out.len() > MAX_SAMPLES || budget == 0 {
                    break 'passes;
                }
                budget -= 1;
                elapsed = elapsed.saturating_add(edit.duration);
                if edit.media_time < 0 {
                    if let Some(last) = out.last_mut() {
                        last.1 = last.1.saturating_add(edit.duration);
                    }
                    continue;
                }
                Self::push_edit(*edit, &spans, &mut out);
            }
        }
        // Edits can repeat the media; keep the list as bounded as the sample tables.
        out.truncate(MAX_SAMPLES);
        if let Some(end) = end {
            // The last pass is cut where the track ends.
            let mut left = end.saturating_sub(self.start_delay());
            out.retain_mut(|(_, d)| {
                *d = (*d).min(left);
                left -= *d;
                *d > 0
            });
        }
        if out.is_empty() {
            // No edit, or none that selects a picture: show the media as it is.
            out = spans
                .iter()
                .map(|&(i, s, e)| (i, s.abs_diff(e.max(s))))
                .collect();
        }
        out
    }

    /// Pushes the frames one non-empty edit shows.
    fn push_edit(edit: Edit, spans: &[(usize, i64, i64)], out: &mut Vec<(usize, u64)>) {
        let start = edit.media_time;
        if edit.dwell {
            if let Some(&(i, ..)) = spans.iter().find(|&&(_, s, e)| s <= start && start < e) {
                out.push((i, edit.duration));
            }
            return;
        }
        let end = start.saturating_add(i64::try_from(edit.duration).unwrap_or(i64::MAX));
        for &(i, s, e) in spans {
            let (from, to) = (s.max(start), e.min(end));
            if from < to {
                out.push((i, from.abs_diff(to)));
            }
        }
    }

    /// How long the edit list waits before the first frame (its leading empty edits), in
    /// [`timescale`](Self::timescale) units.
    pub fn start_delay(&self) -> u64 {
        self.edits
            .iter()
            .take_while(|e| e.media_time < 0)
            .fold(0, |t, e| t.saturating_add(e.duration))
    }

    /// Whether the edit list loops forever: the repeat flag with an indefinite track duration.
    pub fn repeats_forever(&self) -> bool {
        self.repeat_edits && self.presented_duration.is_none()
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

/// The composition offset of a sample that is decoded but not displayed.
const NOT_DISPLAYED: i32 = i32::MIN;

/// Most samples accepted across all tracks of a file.
const MAX_SAMPLES: usize = 1 << 18;

/// Parses the tracks of the `moov` box of a file, or none if it has no `moov`.
pub fn parse_tracks(data: &[u8]) -> Result<Vec<Track>, Error> {
    let mut tracks = Vec::new();
    let mut remaining_samples = MAX_SAMPLES;
    for b in crate::header::top_level_boxes(data) {
        let b = b?;
        if b.header.box_type.0 != *b"moov" {
            continue;
        }
        // Edit list durations are in the movie's timescale (`mvhd`).
        let movie_timescale = match child(b.content, b"mvhd")? {
            Some(mvhd) => {
                let (version, mut r) = full_box(mvhd)?;
                r.bytes(if version == 1 { 16 } else { 8 })?; // creation and modification times
                r.u32()?
            }
            None => 0,
        };
        for t in BoxIter::new(b.content) {
            let t = t?;
            if t.header.box_type.0 == *b"trak" {
                let track = parse_track(t.content, movie_timescale, remaining_samples)?;
                remaining_samples -= track.samples.len();
                tracks.push(track);
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

fn parse_track(
    trak: &[u8],
    movie_timescale: u32,
    remaining_samples: usize,
) -> Result<Track, Error> {
    // tkhd: track ID and visual size.
    let (version, mut r) = full_box(required(trak, b"tkhd")?)?;
    let skip_times = if version == 1 { 16 } else { 8 };
    r.bytes(skip_times)?;
    let id = r.u32()?;
    r.bytes(4)?; // reserved
    // All ones: indefinite.
    let duration = match version {
        1 => Some(r.u64()?).filter(|&d| d != u64::MAX),
        _ => Some(r.u32()?).filter(|&d| d != u32::MAX).map(u64::from),
    };
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
    let (edits, repeat_edits) = match child(trak, b"edts")? {
        Some(edts) => parse_edits(edts, movie_timescale, timescale)?,
        None => (Vec::new(), false),
    };
    let presented_duration = duration
        .filter(|_| movie_timescale != 0)
        .map(|d| movie_to_track_time(d, movie_timescale, timescale));
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
    let mut auxiliary_type = None;
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
            if let Some(auxi) = child(children, b"auxi")? {
                let (_, mut r) = full_box(auxi)?;
                auxiliary_type = Some(r.cstring());
            }
        }
    }

    // stsz: sample sizes.
    let (_, mut r) = full_box(required(stbl, b"stsz")?)?;
    let uniform = r.u32()?;
    let sample_count = r.u32()? as usize;
    if sample_count > remaining_samples {
        return Err(invalid(b"stsz", "too many samples"));
    }
    let table_sizes: Vec<u32> = if uniform != 0 {
        Vec::new()
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
    let mut samples = Vec::with_capacity(sample_count.min(4096));
    'chunks: for (chunk, &chunk_offset) in offsets.iter().enumerate() {
        let number = chunk as u32 + 1;
        let per_chunk = runs
            .iter()
            .rev()
            .find(|&&(first, _)| first <= number)
            .map_or(0, |&(_, n)| n);
        let mut offset = chunk_offset;
        for _ in 0..per_chunk {
            if samples.len() == sample_count {
                break 'chunks;
            }
            let size = if uniform != 0 {
                uniform
            } else {
                table_sizes[samples.len()]
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
        auxiliary_type,
        samples,
        references,
        edits,
        repeat_edits,
        presented_duration,
    })
}

/// The edit list of an `edts` box, with segment durations converted from the movie's
/// timescale to the track's, and its repeat flag. Empty without an `elst` box or a movie
/// timescale.
fn parse_edits(
    edts: &[u8],
    movie_timescale: u32,
    timescale: u32,
) -> Result<(Vec<Edit>, bool), Error> {
    let Some(elst) = child(edts, b"elst")? else {
        return Ok((Vec::new(), false));
    };
    if movie_timescale == 0 {
        return Ok((Vec::new(), false));
    }
    let mut r = Reader::new(elst);
    let version = r.u8()?;
    let repeat = r.bytes(3)?[2] & 1 != 0;
    let n = count(&mut r, if version == 1 { 20 } else { 12 }, b"elst")?;
    let mut edits = Vec::with_capacity(n);
    for _ in 0..n {
        let (duration, media_time) = if version == 1 {
            (r.u64()?, r.u64()? as i64)
        } else {
            (u64::from(r.u32()?), i64::from(r.u32()? as i32))
        };
        let rate = r.u16()?;
        r.bytes(2)?; // media_rate_fraction
        edits.push(Edit {
            duration: movie_to_track_time(duration, movie_timescale, timescale),
            media_time,
            dwell: rate == 0,
        });
    }
    Ok((edits, repeat))
}

/// A duration in the movie's timescale, in the track's.
fn movie_to_track_time(duration: u64, movie_timescale: u32, timescale: u32) -> u64 {
    let d = u128::from(duration) * u128::from(timescale) / u128::from(movie_timescale);
    u64::try_from(d).unwrap_or(u64::MAX)
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
            auxiliary_type: None,
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
            edits: Vec::new(),
            repeat_edits: false,
            presented_duration: None,
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
        // A first sample that is only a reference, as in Nokia's C041.
        assert_eq!(track(&[i32::MIN, 0, 0]).display_order(), [1, 2]);
    }

    #[test]
    fn presentation_follows_the_edit_list() {
        let edit = |media_time, duration| Edit {
            duration,
            media_time,
            dwell: false,
        };
        let shown = |edits: Vec<Edit>| {
            let mut t = track(&[0; 4]);
            t.edits = edits;
            t.presentation()
        };
        let all = vec![(0, 1), (1, 1), (2, 1), (3, 1)];
        assert_eq!(shown(Vec::new()), all);
        // The first two samples, then all of them (Nokia C029, scaled down).
        assert_eq!(
            shown(vec![edit(0, 2), edit(0, 100)]),
            [(0, 1), (1, 1), (0, 1), (1, 1), (2, 1), (3, 1)]
        );
        // Starting later skips samples; a segment past the media ends with it.
        assert_eq!(shown(vec![edit(1, 2)]), [(1, 1), (2, 1)]);
        assert_eq!(shown(vec![edit(2, 50)]), [(2, 1), (3, 1)]);
        // A leading empty edit is a delay; edits selecting nothing show the media as it is.
        assert_eq!(shown(vec![edit(-1, 5), edit(0, 4)]), all);
        assert_eq!(shown(vec![edit(-1, 5)]), all);
        assert_eq!(shown(vec![edit(10, 5)]), all);
        // Media rate 0 holds one picture.
        let mut hold = edit(2, 7);
        hold.dwell = true;
        assert_eq!(shown(vec![hold]), [(2, 7)]);
        // A later empty edit keeps the previous frame on screen.
        assert_eq!(
            shown(vec![edit(0, 2), edit(-1, 3), edit(2, 2)]),
            [(0, 1), (1, 4), (2, 1), (3, 1)]
        );
    }

    #[test]
    fn edit_lists_wait_and_repeat() {
        let edit = |media_time, duration| Edit {
            duration,
            media_time,
            dwell: false,
        };
        let repeated = |edits: Vec<Edit>, presented_duration| {
            let mut t = track(&[0; 4]);
            t.edits = edits;
            t.repeat_edits = true;
            t.presented_duration = presented_duration;
            t
        };
        let mut t = track(&[0; 4]);
        t.edits = vec![edit(-1, 2), edit(-1, 1), edit(0, 4)];
        assert_eq!(t.start_delay(), 3);
        assert_eq!(track(&[0; 4]).start_delay(), 0);
        assert!(!t.repeats_forever());

        // Played until the track's duration: twice and a half (Nokia C036, C037).
        let t = repeated(vec![edit(0, 4)], Some(10));
        let order: Vec<usize> = t.presentation().iter().map(|&(i, _)| i).collect();
        assert_eq!(order, [0, 1, 2, 3, 0, 1, 2, 3, 0, 1]);
        // The start delay comes back between passes; the last frame is cut where the track ends.
        let t = repeated(vec![edit(-1, 2), edit(0, 4)], Some(9));
        assert_eq!(t.start_delay(), 2);
        assert_eq!(t.presentation(), [(0, 1), (1, 1), (2, 1), (3, 3), (0, 1)]);
        // Without the flag, the track's duration changes nothing.
        let mut once = repeated(vec![edit(0, 4)], Some(10));
        once.repeat_edits = false;
        assert_eq!(once.presentation().len(), 4);

        // An indefinite duration: one pass, and the caller loops it (Nokia C038).
        let t = repeated(vec![edit(0, 4)], None);
        assert!(t.repeats_forever());
        assert_eq!(t.presentation().len(), 4);

        // Repeated edits that show nothing end quickly and show the media as it is.
        let t = repeated(vec![edit(10, 1)], Some(u64::MAX));
        assert_eq!(t.presentation().len(), 4);
        let t = repeated(vec![edit(-1, 1)], Some(u64::MAX));
        assert_eq!(t.presentation().len(), 4);
        // A huge repeat count stays bounded.
        let t = repeated(vec![edit(0, 4)], Some(u64::MAX));
        assert_eq!(t.presentation().len(), MAX_SAMPLES);
    }

    #[test]
    fn uniform_stsz_oom_regression_is_rejected() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fuzz/regressions/container/27b98151ea2b6492545413707f2161b38f8f7b5f");
        let bytes = std::fs::read(path).unwrap();
        let err = parse_tracks(&bytes).unwrap_err();
        assert!(matches!(
            err,
            Error::InvalidBox {
                box_type: FourCC([b's', b't', b's', b'z']),
                ..
            }
        ));
    }
}
