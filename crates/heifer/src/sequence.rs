//! Image sequences (`.heics`, bursts, animations): pictures stored as the samples of a track.
//!
//! [`read_sequence`] describes the main sequence track: picture size, frame timing and which
//! frames are keyframes. [`decode_sequence`] decodes every frame in display order, and
//! [`decode_sequence_frame`] one frame, decoding from the keyframe before it.
//!
//! Frames are numbered in display order. With B frames, a file stores them in another
//! (decoding) order, given by the composition offsets of the track (`ctts`).

use heifer_hevc_dec::decoder::DecodeOptions;
use heifer_hevc_dec::recon::Frame;
use heifer_hevc_dec::sequence::SequenceDecoder;
use heifer_isobmff::track::{Track, parse_tracks};
use heifer_isobmff::{FourCC, hevc_annex_b};

use crate::color::{ColorParams, frame_to_rgba_sized};
use crate::{Error, Image, Options};

/// One frame of a sequence.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct FrameInfo {
    /// How long the frame is shown, in seconds.
    pub duration: f64,
    /// Whether the frame decodes on its own (a sync sample).
    pub keyframe: bool,
}

/// The main image sequence of a file.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct SequenceInfo {
    /// Track ID of the sequence.
    pub track_id: u32,
    /// Picture width.
    pub width: u32,
    /// Picture height.
    pub height: u32,
    /// The frames, in display order.
    pub frames: Vec<FrameInfo>,
}

impl SequenceInfo {
    /// Total duration, in seconds.
    pub fn duration(&self) -> f64 {
        self.frames.iter().map(|f| f.duration).sum()
    }
}

/// The main track: the first picture or video track that is not a thumbnail or auxiliary
/// track of another one.
fn main_track(bytes: &[u8]) -> Result<Option<Track>, Error> {
    Ok(parse_tracks(bytes)?.into_iter().find(|t| {
        matches!(&t.handler.0, b"pict" | b"vide")
            && !t
                .references
                .iter()
                .any(|(kind, _)| matches!(&kind.0, b"thmb" | b"auxl"))
    }))
}

/// The alpha auxiliary track of `main`, if any (`tref` `auxl`, matching sample count).
fn alpha_track(bytes: &[u8], main: &Track) -> Result<Option<Track>, Error> {
    Ok(parse_tracks(bytes)?.into_iter().find(|t| {
        t.id != main.id
            && t.hevc_config.is_some()
            && t.samples.len() == main.samples.len()
            && t.auxiliary_type.as_deref().is_none_or(crate::is_alpha_urn)
            && t.references
                .iter()
                .any(|(kind, ids)| kind.0 == *b"auxl" && ids.contains(&main.id))
    }))
}

/// Compact summary of the main image sequence track, read without decoding frames.
pub(crate) struct SequenceSummary {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub has_alpha: bool,
    pub coding: FourCC,
}

/// Whether `bytes` contains a main image/video sequence track.
pub(crate) fn has_sequence_track(bytes: &[u8]) -> bool {
    main_track(bytes).ok().flatten().is_some()
}

/// Returns [`SequenceSummary`] for the main sequence track of `bytes`, if one exists.
pub(crate) fn sequence_summary(bytes: &[u8]) -> Result<Option<SequenceSummary>, Error> {
    let Some(track) = main_track(bytes)? else {
        return Ok(None);
    };
    if !matches!(&track.coding.0, b"hvc1" | b"hev1") {
        return Err(Error::Unsupported(format!(
            "sequence track coding `{}`",
            track.coding
        )));
    }
    let bit_depth = track
        .hevc_config
        .as_ref()
        .map(crate::hevc_config_bit_depth)
        .transpose()?
        .flatten()
        .unwrap_or(8);
    let has_alpha = alpha_track(bytes, &track)?.is_some();
    Ok(Some(SequenceSummary {
        width: track.width,
        height: track.height,
        bit_depth,
        has_alpha,
        coding: track.coding,
    }))
}

/// The main image sequence of a HEIF file, or `None` if it has none.
pub fn read_sequence(bytes: &[u8]) -> Result<Option<SequenceInfo>, Error> {
    let Some(track) = main_track(bytes)? else {
        return Ok(None);
    };
    let scale = f64::from(track.timescale.max(1));
    let times = track.composition_times();
    let order = track.display_order();
    // A frame is shown until the next one; the last one for its sample duration.
    let frames = order
        .iter()
        .enumerate()
        .map(|(k, &i)| {
            let ticks = match order.get(k + 1) {
                Some(&next) => times[next] - times[i],
                None => i64::from(track.samples[i].duration),
            };
            FrameInfo {
                duration: ticks as f64 / scale,
                keyframe: track.samples[i].sync,
            }
        })
        .collect();
    Ok(Some(SequenceInfo {
        track_id: track.id,
        width: track.width,
        height: track.height,
        frames,
    }))
}

/// Decodes every frame of the main sequence, in display order.
///
/// When the sequence is made of closed groups of pictures (no RASL picture), several groups are
/// decoded at once, on up to [`Options::max_threads`] threads. Their frames wait, as decoded
/// pictures, until the groups before them are shown: the number of groups decoded at once is
/// limited so that this stays under 1 GiB. The frames are the same as with one thread.
pub fn decode_sequence(bytes: &[u8]) -> Result<SequenceFrames<'_>, Error> {
    decode_sequence_with_options(bytes, &Options::default())
}

/// Like [`decode_sequence`], with decoding options.
pub fn decode_sequence_with_options<'a>(
    bytes: &'a [u8],
    options: &Options,
) -> Result<SequenceFrames<'a>, Error> {
    let track = main_track(bytes)?.ok_or(Error::Invalid("file has no image sequence"))?;
    SequenceFrames::new(bytes, track, 0, options)
}

/// Like [`decode_sequence_with_options`], taking ownership of the file.
#[cfg(feature = "image")]
pub(crate) fn decode_sequence_owned(
    bytes: Vec<u8>,
    options: &Options,
) -> Result<SequenceFrames<'static>, Error> {
    let track = main_track(&bytes)?.ok_or(Error::Invalid("file has no image sequence"))?;
    SequenceFrames::new(bytes, track, 0, options)
}

/// Decodes frame `index` (in display order) of the main sequence, from the keyframe before
/// it in decoding order.
pub fn decode_sequence_frame(bytes: &[u8], index: usize) -> Result<Image, Error> {
    decode_sequence_frame_with_options(bytes, index, &Options::default())
}

/// Like [`decode_sequence_frame`], with decoding options.
pub fn decode_sequence_frame_with_options(
    bytes: &[u8],
    index: usize,
    options: &Options,
) -> Result<Image, Error> {
    let track = main_track(bytes)?.ok_or(Error::Invalid("file has no image sequence"))?;
    let sample = *track
        .display_order()
        .get(index)
        .ok_or(Error::Invalid("frame index out of range"))?;
    let start = track.samples[..=sample]
        .iter()
        .rposition(|s| s.sync)
        .ok_or(Error::Invalid("no keyframe before the frame"))?;
    let mut frames = SequenceFrames::new(bytes, track, start, options)?;
    for _ in start..sample {
        frames.decode_next(false)?;
    }
    frames
        .decode_next(true)?
        .ok_or(Error::Invalid("frame index out of range"))
}

/// The groups of pictures of a track, as ranges of sample indices from one sync sample to the
/// next, when each can be decoded on its own: the track starts with a sync sample and holds no
/// RASL picture (NAL unit types 8 and 9), the only pictures after a sync sample that may refer
/// to the group before it. `None` otherwise, or when a sample cannot be read.
fn closed_gops(track: &Track, bytes: &[u8]) -> Option<Vec<std::ops::Range<usize>>> {
    let len_size = usize::from(track.hevc_config.as_ref()?.nal_length_size);
    if !(1..=4).contains(&len_size) || !track.samples.first()?.sync {
        return None;
    }
    for index in 0..track.samples.len() {
        let mut data = track.sample_data(bytes, index).ok()?;
        let mut first_picture_type = None;
        while data.len() > len_size {
            let (len, rest) = data.split_at(len_size);
            let len = len
                .iter()
                .fold(0usize, |acc, &b| (acc << 8) | usize::from(b));
            let nal = rest.get(..len)?;
            let nal_type = nal.first().map(|b| (b >> 1) & 0x3f);
            if matches!(nal_type, Some(8 | 9)) {
                return None;
            }
            if first_picture_type.is_none() && nal_type.is_some_and(|t| t < 32) {
                first_picture_type = nal_type;
            }
            data = &rest[len..];
        }
        // A sync sample flagged by the container that is not a random access picture (types 16
        // to 23) may depend on the samples before it: decoding it with a fresh decoder, as a
        // group thread does, could fail where the sequential decoder succeeds.
        if track.samples[index].sync && !first_picture_type.is_some_and(|t| (16..=23).contains(&t))
        {
            return None;
        }
    }
    let starts: Vec<usize> = (0..track.samples.len())
        .filter(|&i| track.samples[i].sync)
        .collect();
    Some(
        starts
            .iter()
            .zip(starts.iter().skip(1).chain([&track.samples.len()]))
            .map(|(&a, &b)| a..b)
            .collect(),
    )
}

/// The frames of an image sequence, decoded one by one (see [`decode_sequence`]).
pub struct SequenceFrames<'a> {
    /// The file: borrowed, or owned by the `image` crate integration.
    bytes: std::borrow::Cow<'a, [u8]>,
    track: Track,
    decoder: SequenceDecoder,
    /// Auxiliary alpha track and its decoder, when the sequence has transparency.
    alpha: Option<(Track, SequenceDecoder)>,
    /// Next sample to decode, in decoding order.
    next: usize,
    /// Sample indices in display order.
    order: Vec<usize>,
    /// Next frame to return, in display order.
    shown: usize,
    /// Decoded frames waiting for their turn: sample index and image.
    pending: Vec<(usize, Image)>,
    /// Groups of pictures decoded on other threads, when the sequence allows it.
    parallel: Option<Parallel>,
    /// `Options::max_pixels`, for the group-of-pictures threads.
    max_pixels: u64,
}

/// Most memory, in bytes, for the decoded frames of the groups of pictures decoded ahead.
/// Frames wait as decoded pictures (16-bit planes: 3 bytes per pixel in 4:2:0), and a group
/// started is decoded to its end: the number of groups decoded at once follows from this
/// budget, e.g. five groups of 30 frames of 4:2:0 1080p.
const PARALLEL_BUDGET_BYTES: u64 = 1 << 30;

/// A frame sent back by a group-of-pictures thread: its sample index and decoded plane(s).
type DecodedFrame = (usize, Result<(Frame, Option<Frame>), Error>);

/// A frame in decoding order, with its sample index, converted to an image.
type Decoded = (usize, Result<Image, Error>);

/// Closed groups of pictures decoded on worker threads, each sending its frames back in
/// decoding order. At most `running_max` groups are decoded at once, so that their frames,
/// which wait until the groups before them are shown, stay within the memory budget.
struct Parallel {
    /// Groups not started yet, in decoding order.
    todo: std::collections::VecDeque<std::ops::Range<usize>>,
    /// Groups being decoded, in decoding order: the frames of the first one come next.
    running: std::collections::VecDeque<std::sync::mpsc::Receiver<DecodedFrame>>,
    running_max: usize,
}

/// What a group-of-pictures thread needs, owned: it outlives no borrow.
struct GroupJob {
    config: heifer_isobmff::boxes::HevcConfig,
    /// The samples of the group, with the index of the first one.
    samples: Vec<Vec<u8>>,
    /// Auxiliary alpha track config and samples for the same group, if present.
    alpha: Option<(heifer_isobmff::boxes::HevcConfig, Vec<Vec<u8>>)>,
    first: usize,
    max_pixels: u64,
}

fn new_sequence_decoder(
    config: &heifer_isobmff::boxes::HevcConfig,
    max_pixels: u64,
) -> Result<SequenceDecoder, Error> {
    let mut decoder = SequenceDecoder::new(DecodeOptions::new().with_max_pixels(max_pixels));
    decoder.add_parameter_sets(&hevc_annex_b(config, &[])?)?;
    Ok(decoder)
}

fn decode_sample_frame(
    config: &heifer_isobmff::boxes::HevcConfig,
    decoder: &mut SequenceDecoder,
    data: &[u8],
    index: usize,
) -> Result<Frame, Error> {
    // Parameter sets come from hvcC: the sample holds the picture's NAL units only.
    let picture_config = heifer_isobmff::boxes::HevcConfig {
        nal_units: Vec::new(),
        ..config.clone()
    };
    let stream = hevc_annex_b(&picture_config, data)?;
    decoder.decode(&stream).map_err(|e| decode_error(e, index))
}

impl GroupJob {
    /// Decodes the group from its sync sample, sending each frame in decoding order. Stops at
    /// the first error, or when the receiver is gone.
    fn run(self, tx: &std::sync::mpsc::Sender<DecodedFrame>) {
        let mut decoder = new_sequence_decoder(&self.config, self.max_pixels);
        let mut alpha_decoder = self
            .alpha
            .as_ref()
            .map(|(cfg, _)| new_sequence_decoder(cfg, self.max_pixels));
        for (k, data) in self.samples.iter().enumerate() {
            let index = self.first + k;
            let frame = match &mut decoder {
                Ok(dec) => decode_sample_frame(&self.config, dec, data, index),
                Err(e) => Err(decode_setup_error(e)),
            };
            let alpha_frame = match (&frame, &self.alpha, &mut alpha_decoder) {
                (Ok(_), Some((cfg, samples)), Some(Ok(dec))) => {
                    match decode_sample_frame(cfg, dec, &samples[k], index) {
                        Ok(a) => Some(a),
                        Err(e) => {
                            let _ = tx.send((index, Err(e)));
                            return;
                        }
                    }
                }
                (Ok(_), Some(_), Some(Err(e))) => {
                    let _ = tx.send((index, Err(decode_setup_error(e))));
                    return;
                }
                _ => None,
            };
            let failed = frame.is_err();
            if tx.send((index, frame.map(|f| (f, alpha_frame)))).is_err() || failed {
                return;
            }
        }
    }
}

fn decode_setup_error(e: &Error) -> Error {
    match e {
        Error::Invalid(s) => Error::Invalid(s),
        Error::Unsupported(s) => Error::Unsupported(s.clone()),
        other => Error::Unsupported(other.to_string()),
    }
}

impl Parallel {
    /// The decoded frames of the sequence in decoding order, from worker threads, or `None`
    /// when the sequence cannot be split or one thread would do.
    fn new(track: &Track, alpha: Option<&Track>, bytes: &[u8], options: &Options) -> Option<Self> {
        let threads = options.threads();
        let groups = closed_gops(track, bytes)?;
        if let Some(alpha) = alpha
            && closed_gops(alpha, bytes).as_ref() != Some(&groups)
        {
            return None;
        }
        let longest = groups.iter().map(|g| g.len()).max().unwrap_or(1) as u64;
        // 16-bit samples: a luma plane and two chroma planes, a quarter, half or full size.
        let chroma_quarters = match track.hevc_config.as_ref().map_or(3, |c| c.chroma_format) {
            0 => 0,
            1 => 2,
            2 => 4,
            _ => 8,
        };
        let pixels = u64::from(track.width.max(1)) * u64::from(track.height.max(1));
        let frame_bytes = 2 * pixels * (4 + chroma_quarters) / 4;
        let running_max = threads.min((PARALLEL_BUDGET_BYTES / (longest * frame_bytes)) as usize);
        if running_max < 2 || groups.len() < 2 {
            return None;
        }
        Some(Self {
            todo: groups.into(),
            running: std::collections::VecDeque::new(),
            running_max,
        })
    }

    /// Starts groups until `threads` are running.
    fn fill(
        &mut self,
        track: &Track,
        alpha: Option<&Track>,
        bytes: &[u8],
        max_pixels: u64,
    ) -> Result<(), Error> {
        while self.running.len() < self.running_max {
            let Some(range) = self.todo.pop_front() else {
                break;
            };
            let samples = range
                .clone()
                .map(|i| track.sample_data(bytes, i).map(<[u8]>::to_vec))
                .collect::<Result<Vec<_>, _>>()?;
            let alpha_job = match alpha {
                Some(alpha) => {
                    let config = alpha
                        .hevc_config
                        .clone()
                        .ok_or(Error::Invalid("alpha sequence without HEVC configuration"))?;
                    let samples = range
                        .clone()
                        .map(|i| alpha.sample_data(bytes, i).map(<[u8]>::to_vec))
                        .collect::<Result<Vec<_>, _>>()?;
                    Some((config, samples))
                }
                None => None,
            };
            let job = GroupJob {
                config: track
                    .hevc_config
                    .clone()
                    .ok_or(Error::Invalid("sequence without HEVC configuration"))?,
                samples,
                alpha: alpha_job,
                first: range.start,
                max_pixels,
            };
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || job.run(&tx));
            self.running.push_back(rx);
        }
        Ok(())
    }

    /// The next decoded frame in decoding order, or `None` after the last one.
    fn next(
        &mut self,
        track: &Track,
        alpha: Option<&Track>,
        bytes: &[u8],
        max_pixels: u64,
    ) -> Option<DecodedFrame> {
        loop {
            if let Err(e) = self.fill(track, alpha, bytes, max_pixels) {
                return Some((usize::MAX, Err(e)));
            }
            let rx = self.running.front()?;
            match rx.recv() {
                Ok(decoded) => return Some(decoded),
                // The group is finished: move on to the next one.
                Err(_) => {
                    self.running.pop_front();
                }
            }
        }
    }
}

/// The error of decoding sample `index`.
fn decode_error(e: heifer_hevc_dec::Error, index: usize) -> Error {
    match e {
        heifer_hevc_dec::Error::Unimplemented(what) => {
            Error::Unsupported(format!("sequence frame {index}: {what}"))
        }
        e => Error::Hevc(e),
    }
}

/// The image of a decoded frame, at the track's display size, with optional alpha plane.
fn frame_image(
    frame: &Frame,
    alpha: Option<&Frame>,
    (track_width, track_height): (u32, u32),
) -> Result<Image, Error> {
    // The track size is the display size; HEVC may code a slightly larger picture.
    let size = |track: u32, coded: u32| if track == 0 { coded } else { track.min(coded) };
    let width = size(track_width, frame.widths[0]);
    let height = size(track_height, frame.heights[0]);
    let mut image = frame_to_rgba_sized(frame, ColorParams::from_frame(frame), width, height);
    if let Some(alpha) = alpha {
        crate::attach_alpha_frame(&mut image, alpha, ColorParams::from_frame(alpha).full_range)?;
    }
    Ok(image)
}

impl std::fmt::Debug for SequenceFrames<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SequenceFrames")
            .field("track_id", &self.track.id)
            .field("next", &self.next)
            .finish_non_exhaustive()
    }
}

impl<'a> SequenceFrames<'a> {
    fn new(
        bytes: impl Into<std::borrow::Cow<'a, [u8]>>,
        track: Track,
        start: usize,
        options: &Options,
    ) -> Result<Self, Error> {
        let bytes = bytes.into();
        let config = track
            .hevc_config
            .as_ref()
            .ok_or_else(|| Error::Unsupported(format!("sequence coding `{}`", track.coding)))?;
        let decoder = new_sequence_decoder(config, options.max_pixels())?;
        let alpha_track = alpha_track(&bytes, &track)?;
        // Several groups of pictures at once, when decoding the whole sequence.
        let parallel = if start == 0 {
            Parallel::new(&track, alpha_track.as_ref(), &bytes, options)
        } else {
            None
        };
        let alpha = match alpha_track {
            Some(alpha_track) => {
                let alpha_cfg = alpha_track
                    .hevc_config
                    .as_ref()
                    .ok_or(Error::Invalid("alpha sequence without HEVC configuration"))?;
                let mut alpha_dec = new_sequence_decoder(alpha_cfg, options.max_pixels())?;
                if start > 0 && !alpha_track.samples[start].sync {
                    let alpha_start = alpha_track.samples[..=start]
                        .iter()
                        .rposition(|s| s.sync)
                        .ok_or(Error::Invalid("no keyframe before the alpha frame"))?;
                    for i in alpha_start..start {
                        let data = alpha_track.sample_data(&bytes, i)?;
                        decode_sample_frame(alpha_cfg, &mut alpha_dec, data, i)?;
                    }
                }
                Some((alpha_track, alpha_dec))
            }
            None => None,
        };
        Ok(Self {
            parallel,
            max_pixels: options.max_pixels(),
            bytes,
            order: track.display_order(),
            track,
            decoder,
            alpha,
            next: start,
            shown: 0,
            pending: Vec::new(),
        })
    }

    /// Number of frames of the sequence.
    pub fn len(&self) -> usize {
        self.track.samples.len()
    }

    /// Whether the sequence has no frame.
    pub fn is_empty(&self) -> bool {
        self.track.samples.is_empty()
    }

    /// Decodes the next sample; converts it to an image when `convert`.
    fn decode_next(&mut self, convert: bool) -> Result<Option<Image>, Error> {
        let index = self.next;
        self.next += 1;
        let config = self
            .track
            .hevc_config
            .as_ref()
            .ok_or(Error::Invalid("sequence without HEVC configuration"))?;
        let data = self.track.sample_data(&self.bytes, index)?;
        let frame = decode_sample_frame(config, &mut self.decoder, data, index)?;
        let alpha_frame = match &mut self.alpha {
            Some((alpha_track, alpha_dec)) => {
                let alpha_cfg = alpha_track
                    .hevc_config
                    .as_ref()
                    .ok_or(Error::Invalid("alpha sequence without HEVC configuration"))?;
                let alpha_data = alpha_track.sample_data(&self.bytes, index)?;
                Some(decode_sample_frame(
                    alpha_cfg, alpha_dec, alpha_data, index,
                )?)
            }
            None => None,
        };
        if convert {
            Ok(Some(frame_image(
                &frame,
                alpha_frame.as_ref(),
                (self.track.width, self.track.height),
            )?))
        } else {
            Ok(None)
        }
    }

    /// The next frame in decoding order, with its sample index: from the worker threads, or
    /// decoded here.
    fn next_decoded(&mut self) -> Option<Decoded> {
        if let Some(parallel) = &mut self.parallel {
            let alpha_track = self.alpha.as_ref().map(|(t, _)| t);
            let (index, result) =
                parallel.next(&self.track, alpha_track, &self.bytes, self.max_pixels)?;
            let size = (self.track.width, self.track.height);
            return Some((
                index,
                result.and_then(|(f, a)| frame_image(&f, a.as_ref(), size)),
            ));
        }
        if self.next >= self.track.samples.len() {
            return None;
        }
        let index = self.next;
        Some((index, self.decode_next(true).map(|i| i.expect("converted"))))
    }
}

impl Iterator for SequenceFrames<'_> {
    type Item = Result<Image, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        let &sample = self.order.get(self.shown)?;
        self.shown += 1;
        // Decode in decoding order until the frame shown next is ready.
        loop {
            if let Some(k) = self.pending.iter().position(|(i, _)| *i == sample) {
                return Some(Ok(self.pending.swap_remove(k).1));
            }
            match self.next_decoded() {
                None => return Some(Err(Error::Invalid("frame missing from the decoding order"))),
                Some((index, Ok(image))) => self.pending.push((index, image)),
                Some((_, Err(e))) => {
                    // Stop after an error.
                    self.shown = self.order.len();
                    return Some(Err(e));
                }
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.order.len().saturating_sub(self.shown);
        (n, Some(n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use heifer_isobmff::track::Sample;

    /// A track whose samples are single NAL units of the given types (4-byte lengths), and the
    /// file bytes holding them.
    fn track(samples: &[(u8, bool)]) -> (Track, Vec<u8>) {
        let mut bytes = Vec::new();
        let mut list = Vec::new();
        for &(nal_type, sync) in samples {
            let offset = bytes.len() as u64;
            bytes.extend_from_slice(&[0, 0, 0, 3, nal_type << 1, 1, 0xAB]);
            list.push((offset, sync));
        }
        let mut t = test_track();
        t.samples = list
            .into_iter()
            .map(|(offset, sync)| sample(offset, sync))
            .collect();
        (t, bytes)
    }

    fn sample(offset: u64, sync: bool) -> Sample {
        let mut s = test_track().samples[0];
        s.offset = offset;
        s.size = 7;
        s.sync = sync;
        s
    }

    fn test_track() -> Track {
        let bytes = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/bframes.mp4"),
        )
        .unwrap();
        main_track(&bytes).unwrap().unwrap()
    }

    #[test]
    fn groups_split_at_sync_samples_unless_open() {
        // IDR, TRAIL_R, TRAIL_N, then a CRA with a RADL: three closed groups of pictures.
        let (t, bytes) = track(&[
            (19, true),
            (1, false),
            (0, false),
            (21, true),
            (7, false),
            (19, true),
        ]);
        assert_eq!(closed_gops(&t, &bytes), Some(vec![0..3, 3..5, 5..6]));
        // A RASL picture refers to the group before: no split.
        let (t, bytes) = track(&[(19, true), (1, false), (21, true), (8, false), (1, false)]);
        assert_eq!(closed_gops(&t, &bytes), None);
        // A track that does not start with a sync sample: no split.
        let (t, bytes) = track(&[(1, false), (19, true)]);
        assert_eq!(closed_gops(&t, &bytes), None);
    }

    #[test]
    fn parallel_decoding_is_used_when_it_can_be() {
        let bytes = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/bframes.mp4"),
        )
        .unwrap();
        let t = main_track(&bytes).unwrap().unwrap();
        let options = |n| Options::new().with_max_threads(n);
        assert!(Parallel::new(&t, None, &bytes, &options(2)).is_some());
        // One thread, or a single frame decoded from its keyframe: sequential.
        assert!(Parallel::new(&t, None, &bytes, &options(1)).is_none());
        let frames = SequenceFrames::new(bytes.as_slice(), t, 5, &options(4)).unwrap();
        assert!(frames.parallel.is_none());
        for name in [
            "sea1_animation.heic",
            "candle_animation.heic",
            "rally_burst.heic",
        ] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures")
                .join(name);
            if let Ok(bytes) = std::fs::read(path) {
                let t = main_track(&bytes).unwrap().unwrap();
                eprintln!(
                    "{name}: groups {:?}",
                    closed_gops(&t, &bytes).map(|g| g.len())
                );
            }
        }
    }

    #[test]
    fn the_test_stream_has_two_closed_groups() {
        let bytes = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/bframes.mp4"),
        )
        .unwrap();
        let t = main_track(&bytes).unwrap().unwrap();
        assert_eq!(closed_gops(&t, &bytes), Some(vec![0..12, 12..24]));
    }
}
