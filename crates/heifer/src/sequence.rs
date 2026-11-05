//! Image sequences (`.heics`, bursts, animations): pictures stored as the samples of a track.
//!
//! [`read_sequence`] describes the main sequence track: picture size, frame timing and which
//! frames are keyframes. [`decode_sequence_frame`] decodes a keyframe; the other frames are
//! predicted from earlier ones (inter prediction), which heifer does not decode yet, and return
//! [`Error::Unsupported`].

use heifer_hevc_dec::decoder::{DecodeOptions, decode_picture};
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

/// Decodes frame `index` of the main sequence, which must be a keyframe.
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
        .samples
        .get(index)
        .ok_or(Error::Invalid("frame index out of range"))?;
    if !sample.sync {
        return Err(Error::Unsupported(
            "sequence frames predicted from other frames (inter prediction)".into(),
        ));
    }
    let config = track
        .hevc_config
        .as_ref()
        .ok_or_else(|| Error::Unsupported(format!("sequence coding `{}`", track.coding)))?;
    let stream = hevc_annex_b(config, track.sample_data(bytes, index)?)?;
    let frame = decode_picture(
        &stream,
        DecodeOptions {
            max_pixels: options.max_pixels(),
            ..Default::default()
        },
    )?;
    // The track size is the display size; HEVC may code a slightly larger picture.
    let width = if track.width == 0 {
        frame.widths[0]
    } else {
        track.width.min(frame.widths[0])
    };
    let height = if track.height == 0 {
        frame.heights[0]
    } else {
        track.height.min(frame.heights[0])
    };
    Ok(frame_to_rgba_sized(
        &frame,
        ColorParams::from_frame(&frame),
        width,
        height,
    ))
}
