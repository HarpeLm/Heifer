//! Image sequences (`.heics`, bursts, animations): pictures stored as the samples of a track.
//!
//! [`read_sequence`] describes the main sequence track: picture size, frame timing and which
//! frames are keyframes. [`decode_sequence`] decodes every frame in order, and
//! [`decode_sequence_frame`] one frame, decoding from the keyframe before it.
//!
//! Frames predicted from earlier ones (P frames) are decoded. Bi-predicted frames (B slices)
//! are not yet, and return [`Error::Unsupported`].

use heifer_hevc_dec::decoder::DecodeOptions;
use heifer_hevc_dec::recon::Frame;
use heifer_hevc_dec::sequence::SequenceDecoder;
use heifer_isobmff::hevc_annex_b;
use heifer_isobmff::track::{Track, parse_tracks};

use crate::color::{ColorParams, frame_to_rgba_sized};
use crate::{Error, Image, Options};

/// One frame of a sequence.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameInfo {
    /// How long the frame is shown, in seconds.
    pub duration: f64,
    /// Whether the frame decodes on its own (a sync sample).
    pub keyframe: bool,
}

/// The main image sequence of a file.
#[derive(Debug, Clone, PartialEq)]
pub struct SequenceInfo {
    /// Track ID of the sequence.
    pub track_id: u32,
    /// Picture width.
    pub width: u32,
    /// Picture height.
    pub height: u32,
    /// The frames, in decoding order.
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
    Ok(Some(SequenceInfo {
        track_id: track.id,
        width: track.width,
        height: track.height,
        frames: track
            .samples
            .iter()
            .map(|s| FrameInfo {
                duration: f64::from(s.duration) / scale,
                keyframe: s.sync,
            })
            .collect(),
    }))
}

/// Decodes every frame of the main sequence, in order.
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

/// Decodes frame `index` of the main sequence, from the keyframe before it.
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
    if index >= track.samples.len() {
        return Err(Error::Invalid("frame index out of range"));
    }
    let start = track.samples[..=index]
        .iter()
        .rposition(|s| s.sync)
        .ok_or(Error::Invalid("no keyframe before the frame"))?;
    let mut frames = SequenceFrames::new(bytes, track, start, options)?;
    for _ in start..index {
        frames.decode_next(false)?;
    }
    frames
        .next()
        .unwrap_or(Err(Error::Invalid("frame index out of range")))
}

/// The frames of an image sequence, decoded one by one (see [`decode_sequence`]).
pub struct SequenceFrames<'a> {
    bytes: &'a [u8],
    track: Track,
    decoder: SequenceDecoder,
    next: usize,
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
    fn new(bytes: &'a [u8], track: Track, start: usize, options: &Options) -> Result<Self, Error> {
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
            track,
            decoder,
            next: start,
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
        let data = self.track.sample_data(self.bytes, index)?;
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
        if self.next >= self.track.samples.len() {
            return None;
        }
        Some(
            self.decode_next(true)
                .map(|image| image.expect("converted")),
        )
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.track.samples.len().saturating_sub(self.next);
        (n, Some(n))
    }
}
