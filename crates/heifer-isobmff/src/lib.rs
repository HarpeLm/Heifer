//! Reading and writing of the ISOBMFF container used by HEIF/HEIC files.
//!
//! Specs: ISO/IEC 14496-12 (ISOBMFF) and ISO/IEC 23008-12 (HEIF).
//!
//! ```no_run
//! let bytes = std::fs::read("photo.heic").unwrap();
//! let file = heifer_isobmff::HeifFile::parse(&bytes).unwrap();
//! let primary = file.primary_item().unwrap();
//! println!("{} {:?}", primary.item_type, file.image_size(primary.id));
//! ```

pub mod boxes;
mod file;
mod header;
mod reader;

pub use file::{HeifFile, Item, ItemId};
pub use header::{BoxHeader, BoxIter, FourCC, RawBox, read_box_header};

/// Errors produced while parsing or writing a container.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    /// The input ended before a complete structure could be read.
    #[error("unexpected end of data")]
    UnexpectedEof,
    /// The declared box size is smaller than its own header.
    #[error("invalid size {size} for box `{box_type}` (header is {header_size} bytes)")]
    InvalidBoxSize {
        /// Type of the offending box.
        box_type: FourCC,
        /// Declared total size.
        size: u64,
        /// Size of the header that was read.
        header_size: u64,
    },
    /// A box required by the specification is missing.
    #[error("missing required box `{0}`")]
    MissingBox(FourCC),
    /// A box contains a value forbidden by the specification.
    #[error("invalid `{box_type}` box: {reason}")]
    InvalidBox {
        /// Type of the offending box.
        box_type: FourCC,
        /// What is wrong.
        reason: &'static str,
    },
    /// An item ID is referenced but not declared.
    #[error("unknown item {0}")]
    UnknownItem(u32),
    /// A feature that is not implemented yet.
    #[error("not implemented: {0}")]
    Unimplemented(&'static str),
}
