//! HEVC (H.265) decoder for HEIF still images and image sequences.
//!
//! Spec: ITU-T H.265. Still images use intra coding only; [`sequence`] adds inter prediction
//! (P and B frames) for image sequences.

// Inside the crate, the deprecated modules below are used freely: the deprecation is a notice
// for other crates.
#![allow(deprecated)]

// The stable API. Everything else is internal: it stays public because `heifer-hevc-enc`,
// `heifer` and the fuzz targets use it, but it is hidden from the documentation and may change
// in any release (see "Stability" in the README).
pub mod decoder;
pub mod sequence;
pub use decoder::{
    DEFAULT_MAX_PIXELS, DecodeOptions, HashCheck, decode_picture, decode_picture_checked,
};
pub use nal::{access_units, split_annex_b};
pub use recon::Frame;
pub use sequence::SequenceDecoder;

#[doc(hidden)]
#[deprecated(
    since = "0.5.1",
    note = "internal to the decoder; it will become private in 0.6"
)]
pub mod bitreader;
#[doc(hidden)]
pub mod cabac;
#[doc(hidden)]
pub mod contexts;
#[doc(hidden)]
#[deprecated(
    since = "0.5.1",
    note = "internal to the decoder; it will become private in 0.6"
)]
pub mod filters;
#[doc(hidden)]
#[deprecated(
    since = "0.5.1",
    note = "internal to the decoder; it will become private in 0.6"
)]
pub mod inter;
#[doc(hidden)]
pub mod nal;
#[doc(hidden)]
pub mod params;
#[doc(hidden)]
pub mod recon;
#[doc(hidden)]
#[deprecated(
    since = "0.5.1",
    note = "internal to the decoder; it will become private in 0.6"
)]
pub mod refs;
#[doc(hidden)]
pub mod scan;
#[doc(hidden)]
#[deprecated(
    since = "0.5.1",
    note = "internal to the decoder; it will become private in 0.6"
)]
pub mod sei;
#[doc(hidden)]
pub mod slice;
#[doc(hidden)]
pub mod syntax;
#[cfg(test)]
mod testutil;

/// Errors produced while decoding an HEVC bitstream.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    /// The bitstream ended before a complete syntax element could be read.
    #[error("unexpected end of bitstream")]
    UnexpectedEof,
    /// A syntax element has a value forbidden by the specification.
    #[error("invalid bitstream: {0}")]
    Invalid(&'static str),
    /// A feature that is not implemented yet.
    #[error("not implemented: {0}")]
    Unimplemented(&'static str),
    /// The decoded picture differs from the hash sent in the stream.
    #[error("decoded picture does not match its {0} hash")]
    HashMismatch(&'static str),
}
