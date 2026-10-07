//! HEVC (H.265) intra-only encoder for HEIF still images.
//!
//! Spec: ITU-T H.265. Only intra coding is needed for still images.

/// Errors produced while encoding an image.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A feature that is not implemented yet.
    #[error("not implemented: {0}")]
    Unimplemented(&'static str),
}
