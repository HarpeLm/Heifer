//! Bounded-memory preview decoding for large HEIC/HEIF photos.
//!
//! A 200 MP photo encoded as a grid of 512×512 tiles takes ~800 MB as a full RGBA8 canvas.
//! [`decode_preview_rgba8`] and [`decode_reader_preview_rgba8`](crate::decode_reader_preview_rgba8)
//! decode plain HEVC grid tiles straight into the downscaled preview buffer (applying `clap`,
//! `irot` and `imir` on the fly and skipping out-of-crop tiles), keeping memory bounded by the
//! tile size plus the preview rather than the full canvas — ideal for WebAssembly, photo
//! galleries, and upload servers.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;

use heifer_hevc_dec::decoder::DecodeOptions;
use heifer_isobmff::{FourCC, HeifFile, ItemId};

use crate::color::frame_view_to_rgba_strided_with_scratch;
use crate::{
    Error, GridTransform, GridWorkspace, Image, MAX_PIXELS, Options, Sample, check_size,
    decode_item_with, decode_plain_tile, fusable_grid_transforms, is_plain_coded_image, item_info,
    to_srgb_with_options,
};

/// Options for bounded-memory preview decoding ([`decode_preview_rgba8`] and
/// [`decode_reader_preview_rgba8`](crate::decode_reader_preview_rgba8)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PreviewOptions {
    /// Maximum width of the returned preview, in pixels (`0` = unbounded by width).
    pub max_width: u32,
    /// Maximum height of the returned preview, in pixels (`0` = unbounded by height).
    pub max_height: u32,
    /// Base decoding options: thread count, colour fallback, and pixel limit for each tile (or
    /// for non-grid images that must be decoded whole).
    pub decode: Options,
    /// Maximum full-canvas pixels accepted for a grid image (`0` = default 2^28 pixels).
    /// Because grid tiles are decoded straight into the preview buffer, peak memory scales with
    /// a tile plus the preview rather than this canvas limit.
    pub max_canvas_pixels: u64,
    /// Whether to convert the preview to sRGB (from embedded ICC or `nclx`), honouring
    /// [`Options::fallback_to_coded_color`]. Defaults to `true`.
    pub to_srgb: bool,
}

impl Default for PreviewOptions {
    fn default() -> Self {
        Self::new(0, 0)
    }
}

impl PreviewOptions {
    /// Preview options targeting at most `max_width × max_height` pixels (preserving aspect ratio
    /// and never upscaling), with sRGB conversion enabled.
    pub const fn new(max_width: u32, max_height: u32) -> Self {
        Self {
            max_width,
            max_height,
            decode: Options::new(),
            max_canvas_pixels: 0,
            to_srgb: true,
        }
    }

    /// Sets [`decode`](Self::decode).
    #[must_use]
    pub const fn with_decode_options(mut self, decode: Options) -> Self {
        self.decode = decode;
        self
    }

    /// Sets [`max_canvas_pixels`](Self::max_canvas_pixels): `0` means the default limit (2^28).
    #[must_use]
    pub const fn with_max_canvas_pixels(mut self, max_canvas_pixels: u64) -> Self {
        self.max_canvas_pixels = max_canvas_pixels;
        self
    }

    /// Sets [`to_srgb`](Self::to_srgb).
    #[must_use]
    pub const fn with_to_srgb(mut self, to_srgb: bool) -> Self {
        self.to_srgb = to_srgb;
        self
    }

    fn max_canvas_pixels(&self) -> u64 {
        if self.max_canvas_pixels == 0 {
            MAX_PIXELS
        } else {
            self.max_canvas_pixels
        }
    }
}

/// Decodes the primary image of a HEIF/HEIC file into an 8-bit RGBA preview fitting within
/// `options.max_width × options.max_height` (preserving aspect ratio, never upscaling).
///
/// For plain HEVC grids (iPhone 12 MP / 48 MP, Galaxy 200 MP, etc.) without an alpha auxiliary
/// plane, tiles are decoded into the preview accumulator directly — skipping out-of-crop tiles
/// and never allocating the full-resolution canvas. For single images, overlays, uncompressed
/// items or grids with alpha, the image is decoded under `options.decode` and downscaled.
pub fn decode_preview_rgba8(bytes: &[u8], options: &PreviewOptions) -> Result<Image<u8>, Error> {
    let file = HeifFile::parse(bytes)?;
    decode_preview_from_file(&file, file.primary_id, options)
}

pub(crate) fn decode_preview_from_file(
    file: &HeifFile<'_>,
    id: ItemId,
    options: &PreviewOptions,
) -> Result<Image<u8>, Error> {
    let info = item_info(file, id)?;
    let (dst_w, dst_h) = fit_dimensions(
        info.width,
        info.height,
        options.max_width,
        options.max_height,
    );
    check_size(dst_w, dst_h, options.decode.max_pixels())?;

    if !info.has_alpha {
        if let Some(preview) = try_decode_grid_preview(file, id, dst_w, dst_h, options)? {
            return finish_preview(file, id, preview, options);
        }
    }

    let decoded: Image<u8> = decode_item_with(file, id, 0, &options.decode)?;
    let (final_w, final_h) = fit_dimensions(
        decoded.width,
        decoded.height,
        options.max_width,
        options.max_height,
    );
    let preview = if (final_w, final_h) == (decoded.width, decoded.height) {
        decoded
    } else {
        downscale_rgba8(&decoded, final_w, final_h)
    };
    finish_preview(file, id, preview, options)
}

fn finish_preview(
    file: &HeifFile<'_>,
    id: ItemId,
    preview: Image<u8>,
    options: &PreviewOptions,
) -> Result<Image<u8>, Error> {
    if options.to_srgb {
        to_srgb_with_options(file, id, preview, &options.decode)
    } else {
        Ok(preview)
    }
}

/// Computes target `(width, height)` fitting within `max_w × max_h` while preserving aspect
/// ratio and never upscaling. A bound of `0` leaves that axis unconstrained.
pub(crate) fn fit_dimensions(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 {
        return (src_w, src_h);
    }
    let lim_w = if max_w == 0 { src_w } else { max_w.min(src_w) };
    let lim_h = if max_h == 0 { src_h } else { max_h.min(src_h) };
    if lim_w == src_w && lim_h == src_h {
        return (src_w, src_h);
    }
    // Compare lim_w / src_w against lim_h / src_h without floating-point rounding drift.
    let by_width = u64::from(lim_w) * u64::from(src_h) <= u64::from(lim_h) * u64::from(src_w);
    if by_width {
        let h = ((u64::from(src_h) * u64::from(lim_w) + u64::from(src_w) / 2) / u64::from(src_w))
            .clamp(1, u64::from(lim_h)) as u32;
        (lim_w.max(1), h)
    } else {
        let w = ((u64::from(src_w) * u64::from(lim_h) + u64::from(src_h) / 2) / u64::from(src_h))
            .clamp(1, u64::from(lim_w)) as u32;
        (w, lim_h.max(1))
    }
}

#[derive(Clone, Copy, Default)]
struct Accum {
    r: u64,
    g: u64,
    b: u64,
    a: u64,
    count: u64,
}

impl Accum {
    #[inline]
    fn add(&mut self, px: &[u8]) {
        self.r += u64::from(px[0]);
        self.g += u64::from(px[1]);
        self.b += u64::from(px[2]);
        self.a += u64::from(px[3]);
        self.count += 1;
    }

    #[inline]
    fn finish(self) -> [u8; 4] {
        if self.count == 0 {
            return [0, 0, 0, 255];
        }
        let half = self.count / 2;
        [
            ((self.r + half) / self.count) as u8,
            ((self.g + half) / self.count) as u8,
            ((self.b + half) / self.count) as u8,
            ((self.a + half) / self.count) as u8,
        ]
    }
}

fn try_decode_grid_preview(
    file: &HeifFile<'_>,
    id: ItemId,
    dst_w: u32,
    dst_h: u32,
    options: &PreviewOptions,
) -> Result<Option<Image<u8>>, Error> {
    let item = file.item(id)?;
    if item.item_type != FourCC(*b"grid") {
        return Ok(None);
    }
    let Some((crop, xform)) = fusable_grid_transforms(file, id)? else {
        return Ok(None);
    };
    let grid = file.grid(id)?;
    let tiles = file.referenced_items(id, b"dimg");
    let columns = u32::from(grid.columns);
    let rows = u32::from(grid.rows);
    if tiles.is_empty() || tiles.len() != rows as usize * columns as usize {
        return Err(Error::Invalid(
            "grid tile count does not match its rows and columns",
        ));
    }
    check_size(
        grid.output_width,
        grid.output_height,
        options.max_canvas_pixels(),
    )?;

    // Require plain HEVC tiles with known `ispe` and `hvcC` so we can validate per-tile limits
    // and decode each tile cleanly without recursion.
    let (Some((tw, th)), Some(hvcc)) = (file.image_size(tiles[0])?, file.hevc_config(tiles[0])?)
    else {
        return Ok(None);
    };
    if tw == 0 || th == 0 {
        return Err(Error::Invalid("grid tile size is zero"));
    }
    check_size(tw, th, options.decode.max_pixels())?;
    let all_plain = tiles
        .iter()
        .all(|&tile_id| is_plain_coded_image(file, tile_id).unwrap_or(false));
    let tiles_cover_grid = u64::from(columns) * u64::from(tw) >= u64::from(grid.output_width)
        && u64::from(rows) * u64::from(th) >= u64::from(grid.output_height);
    if !all_plain || !tiles_cover_grid {
        return Ok(None);
    }

    let (crop_x, crop_y, crop_w, crop_h) =
        crop.unwrap_or((0, 0, grid.output_width, grid.output_height));
    let (out_w, out_h) = xform.output_size(crop_w, crop_h);
    if out_w == 0 || out_h == 0 {
        return Err(Error::Invalid("image size is zero or too large"));
    }
    let (dst_w, dst_h) = fit_dimensions(out_w, out_h, dst_w, dst_h);

    let bit_depth = u8::depth(hvcc.bit_depth_luma.max(hvcc.bit_depth_chroma));
    let hevc_options = DecodeOptions::new().with_max_pixels(options.decode.max_pixels());

    struct TileWork {
        tile_id: ItemId,
        tx0: u32,
        ty0: u32,
        w: u32,
        h: u32,
        cx0: u32,
        cy0: u32,
    }
    let mut visible = Vec::new();
    for (index, &tile_id) in tiles.iter().enumerate() {
        let col = index as u32 % columns;
        let row = index as u32 / columns;
        let gx0 = col.saturating_mul(tw);
        let gy0 = row.saturating_mul(th);
        let gx1 = gx0.saturating_add(tw).min(grid.output_width);
        let gy1 = gy0.saturating_add(th).min(grid.output_height);
        let ix0 = gx0.max(crop_x);
        let iy0 = gy0.max(crop_y);
        let ix1 = gx1.min(crop_x + crop_w);
        let iy1 = gy1.min(crop_y + crop_h);
        if ix0 < ix1 && iy0 < iy1 {
            visible.push(TileWork {
                tile_id,
                tx0: ix0 - gx0,
                ty0: iy0 - gy0,
                w: ix1 - ix0,
                h: iy1 - iy0,
                cx0: ix0 - crop_x,
                cy0: iy0 - crop_y,
            });
        }
    }

    let mut accum = vec![Accum::default(); dst_w as usize * dst_h as usize];
    let threads = options.decode.threads();

    if threads <= 1 || visible.len() <= 1 {
        let mut ws = GridWorkspace::<u8>::default();
        for work in &visible {
            let (view, params) = decode_plain_tile(
                file,
                work.tile_id,
                tw,
                th,
                bit_depth,
                u8::depth,
                hevc_options,
                &mut ws.bitstream,
                &mut ws.hevc,
            )?;
            let src_w = (work.tx0 + work.w) as usize;
            let src_h = (work.ty0 + work.h) as usize;
            let src_stride = src_w * 4;
            ws.tile_rgba.resize(src_stride * src_h, 0);
            frame_view_to_rgba_strided_with_scratch(
                &view,
                params,
                &mut ws.tile_rgba,
                src_stride,
                0,
                0,
                src_w,
                src_h,
                &mut ws.color,
            );
            accumulate_tile(
                &ws.tile_rgba,
                src_w as u32,
                work.tx0,
                work.ty0,
                work.w,
                work.h,
                work.cx0,
                work.cy0,
                crop_w,
                crop_h,
                out_w,
                out_h,
                dst_w,
                dst_h,
                xform,
                &mut accum,
            );
        }
    } else {
        let worker_count = threads.min(visible.len());
        let next = AtomicUsize::new(0);
        let failed = AtomicBool::new(false);
        let (tx, rx) = mpsc::sync_channel::<Result<(usize, usize, Vec<u8>), Error>>(worker_count);
        let visible_ref = &visible;
        let next_ref = &next;
        let failed_ref = &failed;

        std::thread::scope(|scope| {
            for _ in 0..worker_count {
                let tx = tx.clone();
                scope.spawn(move || {
                    let mut ws = GridWorkspace::<u8>::default();
                    loop {
                        if failed_ref.load(Ordering::Relaxed) {
                            break;
                        }
                        let idx = next_ref.fetch_add(1, Ordering::Relaxed);
                        let Some(work) = visible_ref.get(idx) else {
                            break;
                        };
                        let res = decode_plain_tile(
                            file,
                            work.tile_id,
                            tw,
                            th,
                            bit_depth,
                            u8::depth,
                            hevc_options,
                            &mut ws.bitstream,
                            &mut ws.hevc,
                        )
                        .map(|(view, params)| {
                            let src_w = (work.tx0 + work.w) as usize;
                            let src_h = (work.ty0 + work.h) as usize;
                            let src_stride = src_w * 4;
                            let mut tile_rgba = vec![0u8; src_stride * src_h];
                            frame_view_to_rgba_strided_with_scratch(
                                &view,
                                params,
                                &mut tile_rgba,
                                src_stride,
                                0,
                                0,
                                src_w,
                                src_h,
                                &mut ws.color,
                            );
                            (idx, src_w, tile_rgba)
                        });
                        let is_err = res.is_err();
                        if is_err {
                            failed_ref.store(true, Ordering::Relaxed);
                        }
                        if tx.send(res).is_err() || is_err {
                            break;
                        }
                    }
                });
            }
            drop(tx);

            let mut first_err = None;
            for msg in rx {
                match msg {
                    Ok((idx, src_w, tile_rgba)) => {
                        if first_err.is_none() {
                            let work = &visible_ref[idx];
                            accumulate_tile(
                                &tile_rgba,
                                src_w as u32,
                                work.tx0,
                                work.ty0,
                                work.w,
                                work.h,
                                work.cx0,
                                work.cy0,
                                crop_w,
                                crop_h,
                                out_w,
                                out_h,
                                dst_w,
                                dst_h,
                                xform,
                                &mut accum,
                            );
                        }
                    }
                    Err(e) => {
                        failed_ref.store(true, Ordering::Relaxed);
                        if first_err.is_none() {
                            first_err = Some(e);
                        }
                    }
                }
            }
            if let Some(e) = first_err {
                return Err(e);
            }
            Ok(())
        })?;
    }

    let mut data = Vec::with_capacity(dst_w as usize * dst_h as usize * 4);
    for a in accum {
        data.extend_from_slice(&a.finish());
    }
    Ok(Some(Image {
        width: dst_w,
        height: dst_h,
        bit_depth: 8,
        has_alpha: false,
        data,
    }))
}

#[allow(clippy::too_many_arguments)]
fn accumulate_tile(
    tile_rgba: &[u8],
    tw: u32,
    tx0: u32,
    ty0: u32,
    w: u32,
    h: u32,
    cx0: u32,
    cy0: u32,
    crop_w: u32,
    crop_h: u32,
    out_w: u32,
    out_h: u32,
    dst_w: u32,
    dst_h: u32,
    xform: GridTransform,
    accum: &mut [Accum],
) {
    let tw_usize = tw as usize;
    let dst_w_usize = dst_w as usize;
    let (out_w64, out_h64) = (u64::from(out_w), u64::from(out_h));
    let (dst_w64, dst_h64) = (u64::from(dst_w), u64::from(dst_h));

    for ly in 0..h {
        let sy = (ty0 + ly) as usize;
        let row_off = sy * tw_usize;
        let cy = cy0 + ly;
        for lx in 0..w {
            let sx = (tx0 + lx) as usize;
            let cx = cx0 + lx;
            let (dx, dy) = xform.map_point(cx, cy, crop_w, crop_h);
            let ox = (u64::from(dx) * dst_w64 / out_w64) as usize;
            let oy = (u64::from(dy) * dst_h64 / out_h64) as usize;
            let s = (row_off + sx) * 4;
            accum[oy * dst_w_usize + ox].add(&tile_rgba[s..s + 4]);
        }
    }
}

/// Area-averaging downscale of an 8-bit RGBA image to `dst_w × dst_h` (`dst_w <= src.width`,
/// `dst_h <= src.height`).
pub(crate) fn downscale_rgba8(src: &Image<u8>, dst_w: u32, dst_h: u32) -> Image<u8> {
    let mut accum = vec![Accum::default(); dst_w as usize * dst_h as usize];
    let src_w = src.width as usize;
    let dst_w_usize = dst_w as usize;
    let (sw64, sh64) = (u64::from(src.width), u64::from(src.height));
    let (dw64, dh64) = (u64::from(dst_w), u64::from(dst_h));

    for y in 0..src.height {
        let oy = (u64::from(y) * dh64 / sh64) as usize;
        let src_row = y as usize * src_w;
        let dst_row = oy * dst_w_usize;
        for x in 0..src.width {
            let ox = (u64::from(x) * dw64 / sw64) as usize;
            let s = (src_row + x as usize) * 4;
            accum[dst_row + ox].add(&src.data[s..s + 4]);
        }
    }

    let mut data = Vec::with_capacity(dst_w as usize * dst_h as usize * 4);
    for a in accum {
        data.extend_from_slice(&a.finish());
    }
    Image {
        width: dst_w,
        height: dst_h,
        bit_depth: 8,
        has_alpha: src.has_alpha,
        data,
    }
}
