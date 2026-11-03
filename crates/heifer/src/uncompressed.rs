//! Decoding of uncompressed auxiliary images (`unci`, ISO/IEC 23001-17).
//!
//! Only the subset needed by auxiliary images is supported: one unsigned component, no
//! subsampling, one tile and at most 16 bits per sample. Uncompressed primary images are
//! deliberately left unsupported: heifer stays an HEVC codec.

use crate::heifer_isobmff::{HeifFile, ItemId};
use crate::{Error, Image, Options, Sample, check_size};

/// Reads samples packed most-significant bit first.
struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    fn truncated() -> Error {
        Error::Invalid("uncompressed image data is truncated")
    }

    fn read(&mut self, bits: u16) -> Result<u16, Error> {
        let mut value = 0u32;
        for _ in 0..bits {
            let byte = self.data.get(self.bit / 8).ok_or_else(Self::truncated)?;
            let shift = 7 - self.bit % 8;
            value = (value << 1) | u32::from((byte >> shift) & 1);
            self.bit += 1;
        }
        Ok(value as u16)
    }

    fn skip_bits(&mut self, bits: usize) -> Result<(), Error> {
        let end = self.bit.checked_add(bits).ok_or_else(Self::truncated)?;
        let total = self.data.len().checked_mul(8).ok_or_else(Self::truncated)?;
        if end > total {
            return Err(Self::truncated());
        }
        self.bit = end;
        Ok(())
    }

    fn align_to_byte(&mut self) -> Result<(), Error> {
        let bits = self
            .bit
            .div_ceil(8)
            .checked_mul(8)
            .ok_or_else(Self::truncated)?;
        self.skip_bits(bits - self.bit)
    }

    /// Advances to the next multiple of `alignment` bytes, `0` doing nothing.
    fn align_to(&mut self, alignment: u32) -> Result<(), Error> {
        if alignment == 0 {
            return Ok(());
        }
        let alignment = usize::try_from(alignment).map_err(|_| Self::truncated())?;
        let bytes = self.bit.div_ceil(8);
        let aligned = bytes
            .checked_add(alignment - 1)
            .map(|n| n / alignment * alignment)
            .ok_or_else(Self::truncated)?;
        let target = aligned.checked_mul(8).ok_or_else(Self::truncated)?;
        self.skip_bits(target - self.bit)
    }
}

fn unsupported(reason: &str) -> Error {
    Error::Unsupported(format!("uncompressed image {reason}"))
}

/// Decodes an uncompressed auxiliary image item.
pub(crate) fn decode<S: Sample>(
    file: &HeifFile<'_>,
    id: ItemId,
    options: &Options,
) -> Result<Image<S>, Error> {
    let config = file.uncompressed_config(id)?.ok_or_else(|| {
        Error::Unsupported("uncompressed image item without `uncC` configuration".into())
    })?;
    if config.version != 0 {
        return Err(unsupported(&format!(
            "configuration version {}",
            config.version
        )));
    }
    if config.columns != 1 || config.rows != 1 {
        return Err(unsupported("tiling"));
    }
    if config.sampling != 0 {
        return Err(unsupported(&format!(
            "subsampling mode {}",
            config.sampling
        )));
    }
    if config.components.len() != 1 {
        return Err(unsupported(&format!(
            "with {} components",
            config.components.len()
        )));
    }
    let component = config.components[0];
    if component.format != 0 {
        return Err(unsupported(&format!(
            "component format {}",
            component.format
        )));
    }
    if component.bit_depth == 0 || component.bit_depth > 16 {
        return Err(unsupported(&format!(
            "component bit depth {}",
            component.bit_depth
        )));
    }
    if config.components_little_endian {
        return Err(unsupported("with little-endian components"));
    }
    if config.block_size != 0 {
        return Err(unsupported("in blocks"));
    }
    if !matches!(config.interleave, 0 | 1 | 3) {
        return Err(unsupported(&format!(
            "interleave mode {}",
            config.interleave
        )));
    }

    let (width, height) = file.image_size(id)?.ok_or(Error::Invalid(
        "uncompressed image without an `ispe` property",
    ))?;
    check_size(width, height, options.max_pixels())?;

    let data = file.item_data(id)?;
    let width_usize = width as usize;
    let pixel_count = width_usize
        .checked_mul(height as usize)
        .ok_or(Error::Invalid("uncompressed image size overflows"))?;
    let mut samples = Vec::with_capacity(pixel_count);
    let mut reader = BitReader::new(data.as_ref());
    let pixel_interleaved = config.interleave == 1;
    let pad = (u32::from(component.align_size) * 8).saturating_sub(u32::from(component.bit_depth));

    for _ in 0..height {
        for _ in 0..width {
            if component.align_size != 0 {
                reader.align_to_byte()?;
                reader.skip_bits(pad as usize)?;
            }
            samples.push(reader.read(component.bit_depth)?);
            if pixel_interleaved {
                reader.align_to(config.pixel_size)?;
            }
        }
        reader.align_to_byte()?;
        reader.align_to(config.row_align_size)?;
    }

    let coded_depth = component.bit_depth as u8;
    let depth = S::depth(coded_depth);
    let src_max = (1u32 << coded_depth) - 1;
    let dst_max = (1u32 << depth) - 1;
    let alpha = S::from_u16(dst_max as u16);
    let mut image = Image::filled_samples(width, height, depth, [S::default(); 4]);
    for (i, &sample) in samples.iter().enumerate() {
        let value = u32::from(sample);
        let value = if depth == coded_depth {
            value
        } else {
            (value * dst_max + src_max / 2) / src_max
        };
        let pixel = i * 4;
        let value = S::from_u16(value as u16);
        image.data[pixel] = value;
        image.data[pixel + 1] = value;
        image.data[pixel + 2] = value;
        image.data[pixel + 3] = alpha;
    }
    Ok(image)
}
