//! Decodes every AV1 item of real AVIF files and compares the pictures with dav1d's.
//!
//! Opt-in: set `HEIFER_DAV1D` to a dav1d binary. Uses the fixtures of
//! `scripts/fetch-fixtures.sh` and the `.avif` files of `HEIFER_AVIF_DIR`. Film grain is left out.

use std::path::{Path, PathBuf};
use std::process::Command;

use heifer_isobmff::HeifFile;

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

/// dav1d's picture of an item: its planes, samples of 8 bits as bytes, deeper as 16-bit LE.
fn dav1d(dav1d: &Path, obus: &[u8], dir: &Path) -> Vec<u8> {
    let (input, output) = (dir.join("item.obu"), dir.join("item.yuv"));
    std::fs::write(&input, obus).unwrap();
    let status = Command::new(dav1d)
        .args(["--filmgrain", "0", "--demuxer", "section5", "--quiet"])
        .arg("-i")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .status()
        .unwrap();
    assert!(status.success(), "dav1d failed");
    std::fs::read(&output).unwrap()
}

#[test]
fn pixels_match_dav1d() {
    let Some(bin) = std::env::var_os("HEIFER_DAV1D").map(PathBuf::from) else {
        eprintln!("skipping: set HEIFER_DAV1D to a dav1d binary");
        return;
    };
    let dir = std::env::temp_dir().join(format!("heifer-dav1d-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (mut matched, mut unsupported) = (0, Vec::new());
    for path in avif_files() {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let bytes = std::fs::read(&path).unwrap();
        let file = HeifFile::parse(&bytes).unwrap();
        for item in file.items.iter().filter(|i| i.item_type.0 == *b"av01") {
            let what = format!("{name} item {}", item.id);
            let config = file.av1_config(item.id).unwrap().expect("av1C");
            let data = file.item_data(item.id).unwrap();
            let picture =
                heifer_av1_dec::frames(&data, &config.config_obus).and_then(|frames| match frames
                    .as_slice()
                {
                    [frame] => heifer_av1_dec::decode_frame(frame),
                    _ => Err(heifer_av1_dec::Error::Unsupported("several frames")),
                });
            let picture = match picture {
                Err(heifer_av1_dec::Error::Unsupported(why)) => {
                    unsupported.push(format!("{what}: {why}"));
                    continue;
                }
                r => r.unwrap_or_else(|e| panic!("{what}: {e}")),
            };
            // A temporal delimiter, then the sequence header of av1C, then the item.
            let mut obus = vec![0x12, 0x00];
            obus.extend_from_slice(&config.config_obus);
            obus.extend_from_slice(&data);
            let expected = dav1d(&bin, &obus, &dir);
            let mut offset = 0;
            for (p, plane) in picture.planes.iter().enumerate() {
                let sx = if p == 0 {
                    0
                } else {
                    u32::from(picture.subsampling_x)
                };
                let width = ((picture.width + sx) >> sx) as usize;
                let size = if picture.bit_depth == 8 { 1 } else { 2 };
                let reference = &expected[offset..offset + size * plane.len()];
                offset += size * plane.len();
                let sample = |i: usize| match size {
                    1 => u16::from(reference[i]),
                    _ => u16::from_le_bytes([reference[2 * i], reference[2 * i + 1]]),
                };
                if let Some(i) = (0..plane.len()).find(|&i| plane[i] != sample(i)) {
                    panic!(
                        "{what}: plane {p} differs first at ({}, {}): {} instead of {}",
                        i % width,
                        i / width,
                        plane[i],
                        sample(i)
                    );
                }
            }
            assert_eq!(offset, expected.len(), "{what}: size");
            matched += 1;
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
    eprintln!("{matched} items match dav1d; unsupported:");
    for u in &unsupported {
        eprintln!("  {u}");
    }
    assert!(matched > 0);
}
