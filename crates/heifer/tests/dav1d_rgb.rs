//! Decodes real AVIF files to RGBA and compares them with dav1d's pictures put through the same
//! colour conversion, grid assembly, alpha plane and `clap`/`irot`/`imir` transforms.
//!
//! Opt-in: set `HEIFER_DAV1D` to a dav1d binary. Uses `tests/fixtures/avif` and the `.avif`
//! files of `HEIFER_AVIF_DIR`. Film grain is left out.

use std::path::{Path, PathBuf};
use std::process::Command;

use heifer::color::{ColorParams, frame_to_rgba_sized};
use heifer::heifer_hevc_dec::Frame;
use heifer::heifer_isobmff::boxes::{ColorInfo, Property};
use heifer::heifer_isobmff::{HeifFile, ItemId};
use heifer::{Error, Image, Options};

fn avif_files() -> Vec<PathBuf> {
    let mut dirs = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/avif")];
    dirs.extend(std::env::var_os("HEIFER_AVIF_DIR").map(PathBuf::from));
    let mut files: Vec<PathBuf> = dirs
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flatten()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "avif"))
        .collect();
    files.sort();
    files
}

/// dav1d's picture of an `av01` item, as a frame with the item's sequence header colour info.
fn dav1d_frame(bin: &Path, file: &HeifFile<'_>, id: ItemId, dir: &Path) -> Frame {
    let config = file.av1_config(id).unwrap().expect("av1C");
    let data = file.item_data(id).unwrap();
    let frames = heifer_av1_dec::frames(&data, &config.config_obus).unwrap();
    let sequence = &frames[0].sequence;
    // A temporal delimiter, then the sequence header of av1C, then the item.
    let mut obus = vec![0x12, 0x00];
    obus.extend_from_slice(&config.config_obus);
    obus.extend_from_slice(&data);
    let (input, output) = (dir.join("item.obu"), dir.join("item.yuv"));
    std::fs::write(&input, obus).unwrap();
    let status = Command::new(bin)
        .args(["--filmgrain", "0", "--demuxer", "section5", "--quiet"])
        .arg("-i")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .status()
        .unwrap();
    assert!(status.success(), "dav1d failed");
    let yuv = std::fs::read(&output).unwrap();

    let color = &sequence.color;
    let bd = color.bit_depth;
    let (w, h) = (
        frames[0].header.upscaled_width,
        frames[0].header.frame_height,
    );
    let (sx, sy) = (
        u32::from(color.subsampling_x),
        u32::from(color.subsampling_y),
    );
    let (cw, ch) = if color.monochrome {
        (0, 0)
    } else {
        ((w + sx) >> sx, (h + sy) >> sy)
    };
    let (widths, heights) = ([w, cw, cw], [h, ch, ch]);
    let size = if bd == 8 { 1 } else { 2 };
    let mut offset = 0;
    let planes = [0, 1, 2].map(|p| {
        let n = (widths[p] * heights[p]) as usize;
        let bytes = &yuv[offset..offset + size * n];
        offset += size * n;
        match size {
            1 => bytes.iter().map(|&b| u16::from(b)).collect(),
            _ => bytes
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect(),
        }
    });
    assert_eq!(offset, yuv.len(), "dav1d picture size");
    Frame {
        widths,
        heights,
        planes,
        bit_depth: [bd; 2],
        chroma_format: match (color.monochrome, sx, sy) {
            (true, ..) => 0,
            (_, 1, 1) => 1,
            (_, 1, 0) => 2,
            _ => 3,
        },
        full_range: color.full_range,
        matrix_coeffs: color.matrix_coefficients,
    }
}

/// The `colr` (nclx) of an item, else the sequence header's: what heifer uses.
fn params(file: &HeifFile<'_>, id: ItemId, frame: &Frame) -> ColorParams {
    file.item_properties(id)
        .unwrap()
        .find_map(|p| match p {
            Property::Color(ColorInfo::Nclx {
                matrix, full_range, ..
            }) => Some(ColorParams {
                matrix: *matrix,
                full_range: *full_range,
            }),
            _ => None,
        })
        .unwrap_or_else(|| ColorParams::from_frame(frame))
}

/// An `av01` or `grid` item converted to 8-bit RGBA, before its transforms.
fn reference_coded(bin: &Path, file: &HeifFile<'_>, id: ItemId, dir: &Path) -> Image<u8> {
    let item = file.item(id).unwrap();
    if item.item_type.0 == *b"grid" {
        let grid = file.grid(id).unwrap();
        let tiles: Vec<Image<u8>> = file
            .referenced_items(id, b"dimg")
            .iter()
            .map(|&t| transformed(bin, file, t, dir))
            .collect();
        let (tw, th) = (tiles[0].width, tiles[0].height);
        let mut out = Image::<u8> {
            width: grid.output_width,
            height: grid.output_height,
            bit_depth: 8,
            has_alpha: false,
            data: vec![0; (grid.output_width * grid.output_height * 4) as usize],
        };
        for (i, tile) in tiles.iter().enumerate() {
            let (x0, y0) = (
                i as u32 % u32::from(grid.columns) * tw,
                i as u32 / u32::from(grid.columns) * th,
            );
            for y in 0..th.min(out.height.saturating_sub(y0)) {
                let n = tw.min(out.width.saturating_sub(x0)) as usize * 4;
                let src = (y * tw) as usize * 4;
                let dst = ((y0 + y) * out.width + x0) as usize * 4;
                out.data[dst..dst + n].copy_from_slice(&tile.data[src..src + n]);
            }
        }
        return out;
    }
    let frame = dav1d_frame(bin, file, id, dir);
    let (w, h) = file
        .image_size(id)
        .unwrap()
        .map_or((frame.widths[0], frame.heights[0]), |(w, h)| {
            (w.min(frame.widths[0]), h.min(frame.heights[0]))
        });
    let mut image = frame_to_rgba_sized::<u8>(&frame, params(file, id, &frame), w, h);
    let alpha = file.auxiliary_items(id).into_iter().find(|&a| {
        file.item_properties(a).unwrap().any(|p| {
            matches!(p, Property::AuxiliaryType(t)
                if t == "urn:mpeg:mpegB:cicp:systems:auxiliary:alpha" || t == "urn:mpeg:hevc:2015:auxid:1")
        })
    });
    if let Some(a) = alpha {
        let plane = dav1d_frame(bin, file, a, dir);
        let full = params(file, a, &plane).full_range;
        let bd = plane.bit_depth[0];
        let (offset, span) = if full {
            (0.0, f64::from((1u32 << bd) - 1))
        } else {
            (f64::from(16u32 << (bd - 8)), f64::from(219u32 << (bd - 8)))
        };
        for y in 0..h {
            for x in 0..w {
                let v = f64::from(plane.planes[0][(y * plane.widths[0] + x) as usize]);
                image.data[((y * w + x) * 4 + 3) as usize] =
                    ((v - offset) / span * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        }
        image.has_alpha = true;
    }
    image
}

/// An item with its `clap`, `irot` and `imir` applied in association order.
fn transformed(bin: &Path, file: &HeifFile<'_>, id: ItemId, dir: &Path) -> Image<u8> {
    let mut image = reference_coded(bin, file, id, dir);
    for property in file.item_properties(id).unwrap() {
        image = match *property {
            Property::CleanAperture {
                width,
                height,
                horiz_offset,
                vert_offset,
            } => {
                // ISO/IEC 14496-12: the aperture is centred on the image plus the offsets.
                let f = |(n, d): (u32, u32)| f64::from(n) / f64::from(d);
                let o = |(n, d): (i32, u32)| f64::from(n) / f64::from(d);
                let (cw, ch) = (f(width), f(height));
                let cx = o(horiz_offset) + (f64::from(image.width) - 1.0) / 2.0;
                let cy = o(vert_offset) + (f64::from(image.height) - 1.0) / 2.0;
                let x = ((cx - (cw - 1.0) / 2.0).round().max(0.0) as u32).min(image.width);
                let y = ((cy - (ch - 1.0) / 2.0).round().max(0.0) as u32).min(image.height);
                let (w, h) = (
                    (cw.round() as u32).min(image.width - x),
                    (ch.round() as u32).min(image.height - y),
                );
                image.crop(x, y, w, h)
            }
            Property::Rotation(q) if q != 0 => image.rotate_ccw(q),
            Property::Mirror(top_bottom) => image.mirror(top_bottom),
            _ => image,
        };
    }
    image
}

#[test]
fn rgb_matches_dav1d() {
    let Some(bin) = std::env::var_os("HEIFER_DAV1D").map(PathBuf::from) else {
        eprintln!("skipping: set HEIFER_DAV1D to a dav1d binary");
        return;
    };
    let dir = std::env::temp_dir().join(format!("heifer-dav1d-rgb-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (mut matched, mut unsupported) = (0, Vec::new());
    for path in avif_files() {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let bytes = std::fs::read(&path).unwrap();
        let image = match heifer::decode_rgba8(&bytes, &Options::default()) {
            Err(Error::Unsupported(why)) => {
                unsupported.push(format!("{name}: {why}"));
                continue;
            }
            r => r.unwrap_or_else(|e| panic!("{name}: {e}")),
        };
        let file = HeifFile::parse(&bytes).unwrap();
        let expected = transformed(&bin, &file, file.primary_id, &dir);
        assert_eq!(
            (image.width, image.height, image.has_alpha),
            (expected.width, expected.height, expected.has_alpha),
            "{name}: size or alpha"
        );
        if let Some(i) = (0..image.data.len()).find(|&i| image.data[i] != expected.data[i]) {
            let px = i as u32 / 4;
            panic!(
                "{name}: channel {} differs first at ({}, {}): {} instead of {}",
                i % 4,
                px % image.width,
                px / image.width,
                image.data[i],
                expected.data[i]
            );
        }
        matched += 1;
    }
    std::fs::remove_dir_all(&dir).unwrap();
    eprintln!("{matched} files match dav1d; unsupported:");
    for u in &unsupported {
        eprintln!("  {u}");
    }
    assert!(matched > 0);
}
