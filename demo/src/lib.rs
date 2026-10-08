//! WebAssembly bindings used by the browser demo (`demo/web/index.html`).

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
    Ok(Decoded { width: image.width, height: image.height, rgba: image.to_rgba8(), info: info.join(" · ") })
}
