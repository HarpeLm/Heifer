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
use heifer_isobmff::hevc_annex_b;
use heifer_isobmff::track::{Track, parse_tracks};

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

/// The frames of an image sequence, decoded one by one (see [`decode_sequence`]).
pub struct SequenceFrames<'a> {
    /// The file: borrowed, or owned by the `image` crate integration.
    bytes: std::borrow::Cow<'a, [u8]>,
    track: Track,
    decoder: SequenceDecoder,
    /// Next sample to decode, in decoding order.
    next: usize,
    /// Sample indices in display order.
    order: Vec<usize>,
    /// Next frame to return, in display order.
    shown: usize,
    /// Decoded frames waiting for their turn: sample index and image.
    pending: Vec<(usize, Image)>,
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
        let mut decoder = SequenceDecoder::new(DecodeOptions {
            max_pixels: options.max_pixels(),
            ..Default::default()
        });
        decoder.add_parameter_sets(&hevc_annex_b(config, &[])?)?;
        Ok(Self {
            bytes,
            order: track.display_order(),
            track,
            decoder,
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
        // Parameter sets come from hvcC: the sample holds the picture's NAL units only.
        let data = self.track.sample_data(&self.bytes, index)?;
        let stream = hevc_annex_b(
            &heifer_isobmff::boxes::HevcConfig {
                nal_units: Vec::new(),
                ..config.clone()
            },
            data,
        )?;
        let frame = self.decoder.decode(&stream).map_err(|e| match e {
            heifer_hevc_dec::Error::Unimplemented(what) => {
                Error::Unsupported(format!("sequence frame {index}: {what}"))
            }
            e => Error::Hevc(e),
        })?;
        Ok(convert.then(|| self.image(&frame)))
    }

    fn image(&self, frame: &Frame) -> Image {
        // The track size is the display size; HEVC may code a slightly larger picture.
        let size = |track: u32, coded: u32| if track == 0 { coded } else { track.min(coded) };
        let width = size(self.track.width, frame.widths[0]);
        let height = size(self.track.height, frame.heights[0]);
        frame_to_rgba_sized(frame, ColorParams::from_frame(frame), width, height)
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
            if self.next >= self.track.samples.len() {
                return Some(Err(Error::Invalid("frame missing from the decoding order")));
            }
            let index = self.next;
            match self.decode_next(true) {
                Ok(image) => self.pending.push((index, image.expect("converted"))),
                Err(e) => {
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
