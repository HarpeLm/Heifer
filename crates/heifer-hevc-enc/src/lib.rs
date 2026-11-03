//! HEVC (H.265) intra-only encoder for HEIF still images.
//!
//! Spec: ITU-T H.265. Only intra coding is needed for still images. The encoder produces
//! Main, Main 10 and Format Range Extensions (4:4:4) streams, with 4:2:0, 4:4:4 or monochrome
//! planes, lossy or lossless, that the `heifer-hevc-dec` decoder reconstructs exactly as the
//! encoder predicted them.
//!
//! ```
//! use heifer_hevc_enc::encoder::{EncoderConfig, Planes, encode_picture};
//!
//! let (w, h) = (64, 48);
//! let y = vec![128u8; w * h];
//! let chroma = vec![128u8; (w / 2) * (h / 2)];
//! let planes = Planes { y: &y, chroma: Some((&chroma, &chroma)) };
//! let picture = encode_picture(w as u32, h as u32, planes, &EncoderConfig::default()).unwrap();
//! let frame = heifer_hevc_dec::decoder::decode_picture(&picture.annex_b(), Default::default())
//!     .unwrap();
//! assert_eq!(frame.planes[0][0], 128);
//! ```

pub mod cabac;
pub mod encoder;
mod estimate;
mod params;
mod rdo;
mod sao;
pub mod transform;

/// Errors produced while encoding an image.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The input picture is not valid.
    #[error("invalid input: {0}")]
    InvalidInput(&'static str),
    /// The parameter sets written could not be read back (a bug).
    #[error("internal error: {0}")]
    Internal(#[from] heifer_hevc_dec::Error),
}
