//! AV1 image items (`av01`, the AVIF format), decoded by `heifer-av1-dec`.

use crate::{Error, Options, check_size};
use heifer_av1_dec::{ObuType, SequenceHeader};
use heifer_hevc_dec::recon::Frame;
use heifer_isobmff::{FourCC, HeifFile, ItemId};

impl From<heifer_av1_dec::Error> for Error {
    fn from(e: heifer_av1_dec::Error) -> Self {
        match e {
            heifer_av1_dec::Error::Unsupported(what) => Error::Unsupported(format!("AV1 {what}")),
            e => Error::Av1(e),
        }
    }
}

/// Decodes an `av01` item into a frame like the HEVC decoder's, with the colour description of
/// its sequence header.
pub(crate) fn decode_frame(
    file: &HeifFile<'_>,
    id: ItemId,
    options: &Options,
) -> Result<Frame, Error> {
    let config = file
        .av1_config(id)?
        .ok_or(heifer_isobmff::Error::MissingBox(FourCC(*b"av1C")))?;
    let data = file.item_data(id)?;
    let frames = heifer_av1_dec::frames(&data, &config.config_obus)?;
    // An AVIF image item holds one frame; more mean layers or inter frames.
    let [frame] = &frames[..] else {
        return Err(Error::Unsupported("AV1 item with several frames".into()));
    };
    // Before decoding, so that a huge frame size allocates nothing.
    check_size(
        frame.header.upscaled_width,
        frame.header.frame_height,
        options.max_pixels(),
    )?;
    let picture = heifer_av1_dec::decode_frame(frame)?;
    let color = &frame.sequence.color;
    let chroma_format = match (
        picture.planes.len(),
        color.subsampling_x,
        color.subsampling_y,
    ) {
        (1, ..) => 0,
        (_, true, true) => 1,
        (_, true, false) => 2,
        _ => 3,
    };
    let (w, h) = (picture.width, picture.height);
    let (cw, ch) = if chroma_format == 0 {
        (0, 0)
    } else {
        (
            (w + u32::from(color.subsampling_x)) >> u32::from(color.subsampling_x),
            (h + u32::from(color.subsampling_y)) >> u32::from(color.subsampling_y),
        )
    };
    let mut planes = picture.planes.into_iter();
    let mut next = || planes.next().unwrap_or_default();
    Ok(Frame {
        widths: [w, cw, cw],
        heights: [h, ch, ch],
        planes: [next(), next(), next()],
        bit_depth: [picture.bit_depth; 2],
        chroma_format,
        full_range: color.full_range,
        matrix_coeffs: color.matrix_coefficients,
    })
}

/// The colour primaries and transfer characteristics of an `av01` item's sequence header, when
/// both are specified (not 2, "unspecified").
pub(crate) fn colour(file: &HeifFile<'_>, id: ItemId) -> Result<Option<(u16, u16)>, Error> {
    if file.item(id)?.item_type != FourCC(*b"av01") {
        return Ok(None);
    }
    let Some(config) = file.av1_config(id)? else {
        return Ok(None);
    };
    for obu in heifer_av1_dec::obus(&config.config_obus) {
        let obu = obu?;
        if obu.obu_type == ObuType::SequenceHeader {
            let color = SequenceHeader::parse(obu.payload)?.color;
            let (p, t) = (color.color_primaries, color.transfer_characteristics);
            return Ok((p != 2 && t != 2).then_some((u16::from(p), u16::from(t))));
        }
    }
    Ok(None)
}
