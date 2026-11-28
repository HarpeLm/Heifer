//! AV1 decoder for AVIF still images (work in progress).
//!
//! Spec: AV1 Bitstream & Decoding Process Specification (AOMedia). This first layer reads the
//! bitstream structure: OBUs ([`obu`]) and the sequence header ([`sequence`]). Pictures are not
//! decoded yet.

pub mod obu;
pub mod sequence;

mod bits;

pub use obu::{Obu, ObuType, obus};
pub use sequence::SequenceHeader;

/// Errors returned while reading an AV1 bitstream.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The bitstream ended before a complete syntax element could be read.
    #[error("unexpected end of bitstream")]
    UnexpectedEof,
    /// A syntax element has a value forbidden by the specification.
    #[error("invalid bitstream: {0}")]
    Invalid(&'static str),
}
