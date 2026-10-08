//! HEVC (H.265) intra-only decoder for HEIF still images.
//!
//! Spec: ITU-T H.265. Only intra coding is needed for still images.

pub mod bitreader;
pub mod cabac;
pub mod contexts;
pub mod nal;
pub mod params;
pub mod scan;
pub mod slice;
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
}
