//! WebAssembly bindings used by the browser demo (`demo/web/index.html` and `worker.js`).

#![forbid(unsafe_code)]

use std::io::Cursor;

use image::codecs::gif::{GifDecoder, GifEncoder, Repeat};
use image::codecs::png::PngDecoder;
use image::{AnimationDecoder, Delay, ImageBuffer, ImageFormat, Rgba};
use wasm_bindgen::prelude::*;

/// A decoded image: RGBA 8-bit pixels plus a few metadata fields.
#[wasm_bindgen]
pub struct Decoded {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    info: String,
}

#[wasm_bindgen]
impl Decoded {
    /// Width in pixels.
    #[wasm_bindgen(getter)]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    #[wasm_bindgen(getter)]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// RGBA pixels, 4 bytes per pixel, row by row.
    #[wasm_bindgen(getter)]
    pub fn rgba(&self) -> Vec<u8> {
        self.rgba.clone()
    }

    /// Human-readable description (camera, date, bit depth, alpha).
    #[wasm_bindgen(getter)]
    pub fn info(&self) -> String {
        self.info.clone()
    }
}

/// Decoded image sequence: all frames concatenated as 8-bit RGBA in display order.
#[wasm_bindgen]
pub struct DecodedSequence {
    width: u32,
    height: u32,
    frame_count: u32,
    rgba: Vec<u8>,
    durations_ms: Vec<u32>,
    info: String,
}

#[wasm_bindgen]
impl DecodedSequence {
    /// Width of each frame in pixels.
    #[wasm_bindgen(getter)]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height of each frame in pixels.
    #[wasm_bindgen(getter)]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Number of frames in display order.
    #[wasm_bindgen(getter)]
    pub fn frame_count(&self) -> u32 {
        self.frame_count
    }

    /// Frame durations in milliseconds, one entry per frame.
    #[wasm_bindgen(getter)]
    pub fn durations_ms(&self) -> Vec<u32> {
        self.durations_ms.clone()
    }

    /// Human-readable description (frame count, total duration, bit depth, alpha).
    #[wasm_bindgen(getter)]
    pub fn info(&self) -> String {
        self.info.clone()
    }

    /// Moves out the concatenated RGBA buffer (avoiding a full copy in the worker).
    pub fn take_rgba(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.rgba)
    }
}

/// Decodes a HEIC/HEIF file.
#[wasm_bindgen]
pub fn decode(bytes: &[u8]) -> Result<Decoded, JsError> {
    let image = heifer::decode(bytes).map_err(|e| JsError::new(&e.to_string()))?;
    let mut info = vec![format!("{}-bit", image.bit_depth)];
    if image.has_alpha {
        info.push("alpha".into());
    }
    if let Ok(meta) = heifer::read_metadata(bytes)
        && let Some(exif) = meta.exif_fields()
    {
        if let (Some(make), Some(model)) = (exif.make(), exif.model()) {
            info.push(format!("{make} {model}"));
        }
        if let Some(date) = exif.date_time() {
            info.push(date.to_string());
        }
    }
    Ok(Decoded {
        width: image.width,
        height: image.height,
        rgba: image.to_rgba8(),
        info: info.join(" · "),
    })
}

/// Encodes 8-bit RGBA pixels as a HEIC file, on the calling thread.
#[wasm_bindgen]
pub fn encode(rgba: &[u8], width: u32, height: u32, quality: u8) -> Result<Vec<u8>, JsError> {
    let options = heifer::EncodeOptions::new()
        .with_quality(quality)
        .with_max_threads(1);
    heifer::encode_rgba8(width, height, rgba, &options).map_err(|e| JsError::new(&e.to_string()))
}

/// Returns the number of frames in a HEIF sequence track, or `0` if none exists.
#[wasm_bindgen]
pub fn sequence_frame_count(bytes: &[u8]) -> u32 {
    heifer::read_sequence(bytes)
        .ok()
        .flatten()
        .and_then(|s| u32::try_from(s.frames.len()).ok())
        .unwrap_or(0)
}

/// Returns `true` if `bytes` is an animated GIF (≥ 2 frames) or an animated PNG (APNG).
#[wasm_bindgen]
pub fn is_animated_image(bytes: &[u8]) -> bool {
    let reader = Cursor::new(bytes);
    match image::guess_format(bytes) {
        Ok(ImageFormat::Gif) => GifDecoder::new(reader)
            .map(|d| d.into_frames().filter_map(Result::ok).take(2).count() == 2)
            .unwrap_or(false),
        Ok(ImageFormat::Png) => PngDecoder::new(reader)
            .and_then(|d| d.is_apng())
            .unwrap_or(false),
        _ => false,
    }
}

/// Decodes a `.heics` / HEIF burst sequence in display order.
#[wasm_bindgen]
pub fn decode_sequence(bytes: &[u8]) -> Result<DecodedSequence, JsError> {
    decode_sequence_impl(bytes).map_err(|e| JsError::new(&e))
}

/// Decodes an animated GIF or APNG into display-order 8-bit RGBA frames.
#[wasm_bindgen]
pub fn decode_animation(bytes: &[u8]) -> Result<DecodedSequence, JsError> {
    decode_animation_impl(bytes).map_err(|e| JsError::new(&e))
}

/// Encodes display-order 8-bit RGBA frames into a `.heics` sequence.
#[wasm_bindgen]
pub fn encode_sequence(
    rgba: &[u8],
    width: u32,
    height: u32,
    durations_ms: &[u32],
    quality: u8,
    speed: u8,
) -> Result<Vec<u8>, JsError> {
    encode_sequence_impl(rgba, width, height, durations_ms, quality, speed)
        .map_err(|e| JsError::new(&e))
}

/// Exports 8-bit RGBA sequence frames as an animated GIF.
#[wasm_bindgen]
pub fn export_gif(
    rgba: &[u8],
    width: u32,
    height: u32,
    durations_ms: &[u32],
) -> Result<Vec<u8>, JsError> {
    export_gif_impl(rgba, width, height, durations_ms).map_err(|e| JsError::new(&e))
}

/// Exports 8-bit RGBA sequence frames as an animated PNG (APNG).
#[wasm_bindgen]
pub fn export_apng(
    rgba: &[u8],
    width: u32,
    height: u32,
    durations_ms: &[u32],
) -> Result<Vec<u8>, JsError> {
    export_apng_impl(rgba, width, height, durations_ms).map_err(|e| JsError::new(&e))
}

/// Exports 8-bit RGBA sequence frames as numbered PNG files packed into a ZIP archive.
#[wasm_bindgen]
pub fn export_png_frames_zip(
    rgba: &[u8],
    width: u32,
    height: u32,
    stem: &str,
) -> Result<Vec<u8>, JsError> {
    export_png_frames_zip_impl(rgba, width, height, stem).map_err(|e| JsError::new(&e))
}

fn split_rgba_frames(rgba: &[u8], width: u32, height: u32) -> Result<Vec<&[u8]>, String> {
    if width == 0 || height == 0 {
        return Err("frame dimensions must be positive".into());
    }
    let frame_bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or("frame dimensions overflow")?;
    if rgba.is_empty() || !rgba.len().is_multiple_of(frame_bytes) {
        return Err("RGBA buffer length does not match frame size".into());
    }
    Ok(rgba.chunks_exact(frame_bytes).collect())
}

fn duration_to_ms(seconds: f64) -> u32 {
    if !seconds.is_finite() || seconds <= 0.0 {
        return 40;
    }
    (seconds * 1000.0).round().clamp(1.0, f64::from(u32::MAX)) as u32
}

fn decode_sequence_impl(bytes: &[u8]) -> Result<DecodedSequence, String> {
    let seq = heifer::read_sequence(bytes)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "file does not contain an image sequence".to_string())?;
    let frame_count =
        u32::try_from(seq.frames.len()).map_err(|_| "sequence has too many frames".to_string())?;
    if frame_count == 0 {
        return Err("the sequence has no frame".into());
    }
    let total_bytes = (seq.width as usize)
        .checked_mul(seq.height as usize)
        .and_then(|n| n.checked_mul(4))
        .and_then(|n| n.checked_mul(seq.frames.len()))
        .ok_or("decoded sequence exceeds addressable memory")?;
    let mut rgba = Vec::with_capacity(total_bytes);
    let options = heifer::Options::new().with_max_threads(1);
    let mut bit_depth = 8u8;
    let mut has_alpha = false;
    let mut out_width = seq.width;
    let mut out_height = seq.height;

    let frames =
        heifer::decode_sequence_with_options(bytes, &options).map_err(|e| e.to_string())?;
    for (i, frame) in frames.enumerate() {
        let frame = frame.map_err(|e| e.to_string())?;
        if i == 0 {
            out_width = frame.width;
            out_height = frame.height;
            bit_depth = frame.bit_depth;
        } else if frame.width != out_width || frame.height != out_height {
            return Err("sequence frames must share the same dimensions".into());
        }
        has_alpha |= frame.has_alpha;
        rgba.extend_from_slice(&frame.to_rgba8());
    }
    let actual_count = u32::try_from(rgba.len() / (out_width as usize * out_height as usize * 4))
        .map_err(|_| "too many decoded frames".to_string())?;
    if actual_count == 0 {
        return Err("the sequence has no decoded frame".into());
    }
    let durations_ms: Vec<u32> = seq
        .frames
        .iter()
        .take(actual_count as usize)
        .map(|f| duration_to_ms(f.duration))
        .collect();
    let total_s: f64 = durations_ms.iter().map(|&ms| f64::from(ms) / 1000.0).sum();
    let mut parts = vec![
        format!("{actual_count} frames"),
        format!("{total_s:.2} s"),
        format!("{bit_depth}-bit"),
    ];
    if has_alpha {
        parts.push("alpha".into());
    }
    Ok(DecodedSequence {
        width: out_width,
        height: out_height,
        frame_count: actual_count,
        rgba,
        durations_ms,
        info: parts.join(" · "),
    })
}

fn decode_animation_impl(bytes: &[u8]) -> Result<DecodedSequence, String> {
    let reader = Cursor::new(bytes);
    let frames = match image::guess_format(bytes).map_err(|e| e.to_string())? {
        ImageFormat::Gif => GifDecoder::new(reader)
            .map_err(|e| e.to_string())?
            .into_frames()
            .collect_frames()
            .map_err(|e| e.to_string())?,
        ImageFormat::Png => {
            let dec = PngDecoder::new(reader).map_err(|e| e.to_string())?;
            if !dec.is_apng().map_err(|e| e.to_string())? {
                return Err("PNG file is not an animated APNG".into());
            }
            dec.apng()
                .map_err(|e| e.to_string())?
                .into_frames()
                .collect_frames()
                .map_err(|e| e.to_string())?
        }
        _ => return Err("expected an animated GIF or APNG".into()),
    };
    let first = frames
        .first()
        .ok_or_else(|| "animation has no frames".to_string())?;
    let (width, height) = first.buffer().dimensions();
    let frame_count =
        u32::try_from(frames.len()).map_err(|_| "animation has too many frames".to_string())?;
    let total_bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .and_then(|n| n.checked_mul(frames.len()))
        .ok_or("animation exceeds addressable memory")?;
    let mut rgba = Vec::with_capacity(total_bytes);
    let mut durations_ms = Vec::with_capacity(frames.len());
    let mut has_alpha = false;
    for frame in &frames {
        let buf = frame.buffer();
        if buf.dimensions() != (width, height) {
            return Err("animation frames must share the same dimensions".into());
        }
        let raw = buf.as_raw();
        has_alpha |= raw.as_chunks::<4>().0.iter().any(|p| p[3] < 255);
        rgba.extend_from_slice(raw);
        let (numer, denom) = frame.delay().numer_denom_ms();
        let ms = (f64::from(numer) / f64::from(denom.max(1)))
            .round()
            .clamp(1.0, f64::from(u32::MAX)) as u32;
        durations_ms.push(ms);
    }
    let total_s: f64 = durations_ms.iter().map(|&ms| f64::from(ms) / 1000.0).sum();
    let mut parts = vec![format!("{frame_count} frames"), format!("{total_s:.2} s")];
    if has_alpha {
        parts.push("alpha".into());
    }
    Ok(DecodedSequence {
        width,
        height,
        frame_count,
        rgba,
        durations_ms,
        info: parts.join(" · "),
    })
}

fn encode_sequence_impl(
    rgba: &[u8],
    width: u32,
    height: u32,
    durations_ms: &[u32],
    quality: u8,
    speed: u8,
) -> Result<Vec<u8>, String> {
    let chunks = split_rgba_frames(rgba, width, height)?;
    if chunks.len() != durations_ms.len() {
        return Err("one duration per frame is needed".into());
    }
    let mut images = Vec::with_capacity(chunks.len());
    for raw in chunks {
        let has_alpha = raw.as_chunks::<4>().0.iter().any(|p| p[3] < 255);
        let mut image = heifer::Image::filled(width, height, 8, [0, 0, 0, 255]);
        image.has_alpha = has_alpha;
        for (dst, px) in image
            .data
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(raw.as_chunks::<4>().0)
        {
            dst[0] = u16::from(px[0]);
            dst[1] = u16::from(px[1]);
            dst[2] = u16::from(px[2]);
            dst[3] = u16::from(px[3]);
        }
        images.push(image);
    }
    let durations: Vec<f64> = durations_ms
        .iter()
        .map(|&ms| f64::from(ms.max(1)) / 1000.0)
        .collect();
    let preset = match speed {
        0 => heifer::EncodeSpeed::Fast,
        1 => heifer::EncodeSpeed::Medium,
        _ => heifer::EncodeSpeed::Slow,
    };
    let options = heifer::EncodeOptions::new()
        .with_quality(quality.clamp(1, 100))
        .with_speed(preset)
        .with_max_threads(1);
    heifer::encode_sequence(&images, &durations, &options).map_err(|e| e.to_string())
}

fn export_gif_impl(
    rgba: &[u8],
    width: u32,
    height: u32,
    durations_ms: &[u32],
) -> Result<Vec<u8>, String> {
    let chunks = split_rgba_frames(rgba, width, height)?;
    if chunks.len() != durations_ms.len() {
        return Err("one duration per frame is needed".into());
    }
    let mut out = Vec::new();
    {
        let mut encoder = GifEncoder::new_with_speed(&mut out, 10);
        encoder
            .set_repeat(Repeat::Infinite)
            .map_err(|e| e.to_string())?;
        let frames = chunks.into_iter().zip(durations_ms).map(|(raw, &ms)| {
            let buf = ImageBuffer::<Rgba<u8>, _>::from_vec(width, height, raw.to_vec())
                .expect("validated frame length");
            image::Frame::from_parts(buf, 0, 0, Delay::from_numer_denom_ms(ms.max(10), 1))
        });
        encoder.encode_frames(frames).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

fn export_apng_impl(
    rgba: &[u8],
    width: u32,
    height: u32,
    durations_ms: &[u32],
) -> Result<Vec<u8>, String> {
    let chunks = split_rgba_frames(rgba, width, height)?;
    if chunks.len() != durations_ms.len() {
        return Err("one duration per frame is needed".into());
    }
    let num_frames = u32::try_from(chunks.len()).map_err(|_| "too many frames".to_string())?;
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .set_animated(num_frames, 0)
            .map_err(|e| e.to_string())?;
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        for (raw, &ms) in chunks.into_iter().zip(durations_ms) {
            writer
                .set_frame_delay(ms.clamp(1, u32::from(u16::MAX)) as u16, 1000)
                .map_err(|e| e.to_string())?;
            writer.write_image_data(raw).map_err(|e| e.to_string())?;
        }
        writer.finish().map_err(|e| e.to_string())?;
    }
    Ok(out)
}

fn encode_png_frame(raw: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(raw).map_err(|e| e.to_string())?;
        writer.finish().map_err(|e| e.to_string())?;
    }
    Ok(out)
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn export_png_frames_zip_impl(
    rgba: &[u8],
    width: u32,
    height: u32,
    stem: &str,
) -> Result<Vec<u8>, String> {
    let chunks = split_rgba_frames(rgba, width, height)?;
    let safe_stem: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let prefix = if safe_stem.is_empty() {
        "frame"
    } else {
        safe_stem.as_str()
    };
    let digits = chunks.len().to_string().len().max(3);
    let count = u16::try_from(chunks.len()).map_err(|_| "too many frames for zip".to_string())?;

    struct Entry {
        name: Vec<u8>,
        crc: u32,
        size: u32,
        offset: u32,
    }

    let mut zip = Vec::new();
    let mut entries = Vec::with_capacity(chunks.len());
    for (idx, raw) in chunks.into_iter().enumerate() {
        let png = encode_png_frame(raw, width, height)?;
        let name = format!("{prefix}-{:0digits$}.png", idx + 1).into_bytes();
        let name_len = u16::try_from(name.len()).map_err(|_| "file name too long".to_string())?;
        let size =
            u32::try_from(png.len()).map_err(|_| "png exceeds zip entry limit".to_string())?;
        let offset = u32::try_from(zip.len()).map_err(|_| "zip exceeds 4 GiB".to_string())?;
        let crc = crc32(&png);

        // Local file header (Store, method 0, UTF-8 filenames bit 11).
        zip.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        zip.extend_from_slice(&20u16.to_le_bytes());
        zip.extend_from_slice(&0x0800u16.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&0x5421u16.to_le_bytes());
        zip.extend_from_slice(&crc.to_le_bytes());
        zip.extend_from_slice(&size.to_le_bytes());
        zip.extend_from_slice(&size.to_le_bytes());
        zip.extend_from_slice(&name_len.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&name);
        zip.extend_from_slice(&png);

        entries.push(Entry {
            name,
            crc,
            size,
            offset,
        });
    }

    let cd_offset = u32::try_from(zip.len()).map_err(|_| "zip exceeds 4 GiB".to_string())?;
    for entry in &entries {
        let name_len = entry.name.len() as u16;
        zip.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        zip.extend_from_slice(&20u16.to_le_bytes());
        zip.extend_from_slice(&20u16.to_le_bytes());
        zip.extend_from_slice(&0x0800u16.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&0x5421u16.to_le_bytes());
        zip.extend_from_slice(&entry.crc.to_le_bytes());
        zip.extend_from_slice(&entry.size.to_le_bytes());
        zip.extend_from_slice(&entry.size.to_le_bytes());
        zip.extend_from_slice(&name_len.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&0u32.to_le_bytes());
        zip.extend_from_slice(&entry.offset.to_le_bytes());
        zip.extend_from_slice(&entry.name);
    }
    let cd_size = u32::try_from(zip.len() - cd_offset as usize)
        .map_err(|_| "zip central directory exceeds 4 GiB".to_string())?;

    zip.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    zip.extend_from_slice(&0u16.to_le_bytes());
    zip.extend_from_slice(&0u16.to_le_bytes());
    zip.extend_from_slice(&count.to_le_bytes());
    zip.extend_from_slice(&count.to_le_bytes());
    zip.extend_from_slice(&cd_size.to_le_bytes());
    zip.extend_from_slice(&cd_offset.to_le_bytes());
    zip.extend_from_slice(&0u16.to_le_bytes());
    Ok(zip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_heics_sequence_fixtures() {
        for path in [
            "../tests/fixtures/candle_animation.heic",
            "../tests/fixtures/burst.heic",
        ] {
            let bytes = std::fs::read(path).expect(path);
            assert!(sequence_frame_count(&bytes) > 1, "{path}");
            let seq = decode_sequence_impl(&bytes).expect(path);
            assert!(seq.frame_count > 1);
            assert_eq!(seq.durations_ms.len(), seq.frame_count as usize);
            assert_eq!(
                seq.rgba.len(),
                seq.width as usize * seq.height as usize * 4 * seq.frame_count as usize
            );
        }
    }

    #[test]
    fn gif_apng_and_heics_round_trip() {
        let (w, h) = (32u32, 32u32);
        let mut rgba = Vec::with_capacity((w * h * 4 * 3) as usize);
        for f in 0..3u8 {
            for y in 0..h {
                for x in 0..w {
                    let alpha = if f == 1 && x < 16 { 128 } else { 255 };
                    rgba.extend_from_slice(&[f * 80 + 20, (y * 7) as u8, (x * 7) as u8, alpha]);
                }
            }
        }
        let durations = [40u32, 80, 60];

        let gif = export_gif_impl(&rgba, w, h, &durations).unwrap();
        assert!(is_animated_image(&gif));
        let dec_gif = decode_animation_impl(&gif).unwrap();
        assert_eq!(dec_gif.frame_count, 3);
        assert_eq!((dec_gif.width, dec_gif.height), (w, h));

        let apng = export_apng_impl(&rgba, w, h, &durations).unwrap();
        assert!(is_animated_image(&apng));
        let dec_apng = decode_animation_impl(&apng).unwrap();
        assert_eq!(dec_apng.frame_count, 3);
        assert_eq!(dec_apng.durations_ms, durations);
        assert!(dec_apng.info.contains("alpha"));

        let heics = encode_sequence_impl(&rgba, w, h, &durations, 75, 0).unwrap();
        assert_eq!(sequence_frame_count(&heics), 3);
        let dec_heics = decode_sequence_impl(&heics).unwrap();
        assert_eq!(dec_heics.frame_count, 3);
        assert_eq!(dec_heics.durations_ms, durations);
        assert!(dec_heics.info.contains("alpha"));
    }

    #[test]
    fn png_frames_zip_writes_valid_archive() {
        let (w, h) = (16u32, 16u32);
        let rgba = vec![120u8; (w * h * 4 * 2) as usize];
        let zip = export_png_frames_zip_impl(&rgba, w, h, "test burst").unwrap();
        assert_eq!(&zip[..4], b"PK\x03\x04");
        assert!(zip.windows(14).any(|w| w == b"test_burst-001"));
        assert!(zip.windows(14).any(|w| w == b"test_burst-002"));
        assert_eq!(&zip[zip.len() - 22..zip.len() - 18], b"PK\x05\x06");
    }
}
