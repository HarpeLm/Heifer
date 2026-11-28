//! Decodes the small AV1 streams of `tests/streams` and compares every sample with the picture
//! aomdec made from the same stream (`scripts/make-av1-streams.sh`).

use std::path::Path;

fn check(name: &str) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/streams");
    let data = std::fs::read(dir.join(format!("{name}.obu"))).unwrap();
    let expected = std::fs::read(dir.join(format!("{name}.yuv"))).unwrap();
    let frames = heifer_av1_dec::frames(&data, &[]).unwrap();
    assert_eq!(frames.len(), 1, "{name}");
    let picture = heifer_av1_dec::decode_frame(&frames[0]).unwrap();
    let mut offset = 0;
    for (p, plane) in picture.planes.iter().enumerate() {
        let sx = if p == 0 {
            0
        } else {
            u32::from(picture.subsampling_x)
        };
        let width = ((picture.width + sx) >> sx) as usize;
        // aomdec writes 8-bit samples as bytes, deeper ones as 16-bit little endian.
        let reference: Vec<u16> = if picture.bit_depth == 8 {
            expected[offset..offset + plane.len()]
                .iter()
                .map(|&b| u16::from(b))
                .collect()
        } else {
            expected[offset..offset + 2 * plane.len()]
                .chunks(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect()
        };
        offset += if picture.bit_depth == 8 {
            plane.len()
        } else {
            2 * plane.len()
        };
        if let Some(i) = plane.iter().zip(&reference).position(|(a, b)| a != b) {
            panic!(
                "{name}: plane {p} differs first at ({}, {}): {} instead of {}",
                i % width,
                i / width,
                plane[i],
                reference[i]
            );
        }
    }
    assert_eq!(offset, expected.len(), "{name}: size");
}

#[test]
fn basic_q30() {
    check("basic_q30");
}

#[test]
fn directional() {
    check("directional");
}

#[test]
fn smooth_paeth() {
    check("smooth_paeth");
}

#[test]
fn cfl() {
    check("cfl");
}

#[test]
fn filter_intra() {
    check("filter_intra");
}

#[test]
fn tx_types() {
    check("tx_types");
}

#[test]
fn rect() {
    check("rect");
}

#[test]
fn segmentation() {
    check("segmentation");
}

#[test]
fn lossless() {
    check("lossless");
}

#[test]
fn all_tools() {
    check("all_tools");
}

#[test]
fn tiles_sb128() {
    check("tiles_sb128");
}

#[test]
fn big_blocks() {
    check("big_blocks");
}

#[test]
fn palette() {
    check("palette");
}

#[test]
fn delta_q() {
    check("delta_q");
}

#[test]
fn monochrome() {
    check("monochrome");
}

#[test]
fn yuv444() {
    check("yuv444");
}

#[test]
fn yuv422() {
    check("yuv422");
}

#[test]
fn bit_depth_10() {
    check("bit_depth_10");
}

#[test]
fn deblock() {
    check("deblock");
}

#[test]
fn deblock_sharpness() {
    check("deblock_sharpness");
}

#[test]
fn deblock_tools() {
    check("deblock_tools");
}

#[test]
fn delta_lf() {
    check("delta_lf");
}

#[test]
fn cdef() {
    check("cdef");
}

#[test]
fn cdef_sb128() {
    check("cdef_sb128");
}

#[test]
fn restoration() {
    check("restoration");
}

#[test]
fn restoration_large() {
    check("restoration_large");
}

#[test]
fn filters() {
    check("filters");
}

#[test]
fn filters_tiles() {
    check("filters_tiles");
}

#[test]
fn filters_segmentation() {
    check("filters_segmentation");
}

#[test]
fn filters_monochrome() {
    check("filters_monochrome");
}

#[test]
fn filters_yuv444() {
    check("filters_yuv444");
}

#[test]
fn filters_yuv422() {
    check("filters_yuv422");
}

#[test]
fn filters_10() {
    check("filters_10");
}

#[test]
fn filters_12() {
    check("filters_12");
}
