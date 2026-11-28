//! Header-only `read_info` / `read_info_reader` without decoding pixel payloads.

use std::io::{Cursor, Read, Seek, SeekFrom};

use heifer::{FileKind, read_info, read_info_reader};

struct CountingReader<R> {
    inner: R,
    bytes_read: usize,
}

impl<R> CountingReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            bytes_read: 0,
        }
    }
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.bytes_read += n;
        Ok(n)
    }
}

impl<R: Seek> Seek for CountingReader<R> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

fn fixture_files() -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures");
    for dir in [root.clone(), root.join("real")] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if matches!(ext, "heic" | "heif" | "hif" | "avci") {
                out.push((path.display().to_string(), std::fs::read(&path).unwrap()));
            }
        }
    }
    out
}

#[test]
fn read_info_matches_decode_on_all_fixtures() {
    let files = fixture_files();
    if files.is_empty() {
        eprintln!("skipping fixture parity: tests/fixtures not present");
        return;
    }
    let mut verified = 0;
    for (name, bytes) in files {
        let info = read_info(&bytes);
        let reader_info = read_info_reader(Cursor::new(&bytes));
        match (&info, &reader_info) {
            (Ok(a), Ok(b)) => assert_eq!(a, b, "{name}: slice vs reader info"),
            (Err(_), Err(_)) => {}
            _ => panic!(
                "{name}: slice info {:?}, reader info {:?}",
                info.err(),
                reader_info.err()
            ),
        }
        if let Ok(decoded) = heifer::decode(&bytes) {
            let info = info.unwrap_or_else(|e| panic!("{name}: decode ok but info failed: {e}"));
            assert_eq!(
                (info.width, info.height, info.bit_depth, info.has_alpha),
                (
                    decoded.width,
                    decoded.height,
                    decoded.bit_depth,
                    decoded.has_alpha
                ),
                "{name}"
            );
            assert!(info.has_primary_image, "{name}");
            verified += 1;
        }
    }
    assert!(verified >= 1);
}

#[test]
fn read_info_handles_sequence_only_file() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/bframes.mp4");
    let bytes = std::fs::read(&path).unwrap();
    let info = read_info(&bytes).unwrap();
    let reader_info = read_info_reader(Cursor::new(&bytes)).unwrap();
    assert_eq!(info, reader_info);
    assert_eq!((info.width, info.height), (128, 96));
    assert_eq!(info.bit_depth, 8);
    assert!(!info.has_alpha);
    assert_eq!(info.kind, FileKind::Sequence);
    assert_eq!(info.kind.as_str(), "heics");
    assert_eq!(info.kind.mime_type(), "image/heic-sequence");
    assert!(!info.has_primary_image);
    assert!(info.has_sequence);
}

#[test]
fn read_info_reader_skips_pixel_payload() {
    let big_fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/real/heif_special__200MP.heic");
    if big_fixture.exists() {
        let bytes = std::fs::read(&big_fixture).unwrap();
        let mut reader = CountingReader::new(Cursor::new(&bytes));
        let info = read_info_reader(&mut reader).unwrap();
        assert_eq!((info.width, info.height), (12240, 16320));
        assert!(
            reader.bytes_read < 64 * 1024,
            "expected header-only read on 200MP fixture, read {} of {} bytes",
            reader.bytes_read,
            bytes.len()
        );
    }

    #[cfg(feature = "encoder")]
    {
        let (w, h) = (1100u32, 700u32);
        let rgba: Vec<u8> = (0..w * h * 4)
            .map(|i| {
                if i % 4 == 3 {
                    128
                } else {
                    ((i * 17) % 251) as u8
                }
            })
            .collect();
        let heic = heifer::encode_rgba8(w, h, &rgba, &heifer::EncodeOptions::default()).unwrap();
        let mut reader = CountingReader::new(Cursor::new(&heic));
        let info = read_info_reader(&mut reader).unwrap();
        assert_eq!(
            (info.width, info.height, info.bit_depth, info.has_alpha),
            (w, h, 8, true)
        );
        assert!(
            reader.bytes_read < heic.len() / 2,
            "read {} of {} bytes",
            reader.bytes_read,
            heic.len()
        );
    }
}

#[test]
fn read_info_rejects_invalid_and_unsupported() {
    assert!(read_info(&[]).is_err());
    assert!(read_info_reader(Cursor::new(&[])).is_err());

    let jpeg = b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00";
    assert!(read_info(jpeg).is_err());
    assert!(read_info_reader(Cursor::new(jpeg)).is_err());

    let truncated_ftyp = b"\x00\x00\x00\x18ftypheic\x00\x00\x00\x00mif1garbage";
    assert!(read_info(truncated_ftyp).is_err());
    assert!(read_info_reader(Cursor::new(truncated_ftyp)).is_err());

    let mut w = heifer::heifer_isobmff::writer::HeifWriter::new();
    let cfg = heifer::heifer_isobmff::boxes::HevcConfig {
        profile_idc: 1,
        level_idc: 93,
        chroma_format: 1,
        bit_depth_luma: 8,
        bit_depth_chroma: 8,
        nal_length_size: 4,
        nal_units: Vec::new(),
    };
    w.add_image(&heifer::heifer_isobmff::writer::ImageSpec::new(
        64,
        64,
        cfg,
        &[],
    ));
    let mut avif_bytes = w.finish();
    let pos = avif_bytes
        .windows(4)
        .position(|win| win == b"hvc1")
        .unwrap();
    avif_bytes[pos..pos + 4].copy_from_slice(b"av01");
    assert!(read_info(&avif_bytes).is_err());
    assert!(read_info_reader(Cursor::new(&avif_bytes)).is_err());

    #[cfg(feature = "encoder")]
    {
        let valid = heifer::encode_rgb8(
            64,
            64,
            &[90u8; 64 * 64 * 3],
            &heifer::EncodeOptions::default(),
        )
        .unwrap();
        assert!(read_info(&valid[..20]).is_err());
        assert!(read_info_reader(Cursor::new(&valid[..20])).is_err());
    }
}

#[cfg(feature = "encoder")]
#[test]
fn read_info_classifies_heic_heif_and_sequence() {
    let rgb = vec![180u8; 64 * 48 * 3];
    let still = heifer::encode_rgb8(64, 48, &rgb, &heifer::EncodeOptions::default()).unwrap();
    let still_info = read_info(&still).unwrap();
    assert_eq!(still_info.kind, FileKind::Heic);
    assert_eq!(still_info.kind.as_str(), "heic");
    assert_eq!(still_info.kind.mime_type(), "image/heic");
    assert!(still_info.has_primary_image);
    assert!(!still_info.has_sequence);
    assert_eq!(still_info, read_info_reader(Cursor::new(&still)).unwrap());

    let mut mif1 = still.clone();
    assert_eq!(&mif1[4..12], b"ftypheic");
    mif1[8..12].copy_from_slice(b"mif1");
    let heif_info = read_info(&mif1).unwrap();
    assert_eq!(heif_info.kind, FileKind::Heif);
    assert_eq!(heif_info.kind.as_str(), "heif");
    assert_eq!(heif_info.kind.mime_type(), "image/heif");
    assert_eq!(heif_info, read_info_reader(Cursor::new(&mif1)).unwrap());

    let f0 = heifer::Image::filled(64, 48, 8, [100, 120, 140, 255]);
    let f1 = heifer::Image::filled(64, 48, 8, [160, 180, 200, 255]);
    let seq = heifer::encode_sequence(&[f0, f1], &[0.04, 0.04], &heifer::EncodeOptions::default())
        .unwrap();
    let seq_info = read_info(&seq).unwrap();
    assert_eq!(seq_info.kind, FileKind::Sequence);
    assert_eq!(seq_info.kind.as_str(), "heics");
    assert_eq!(seq_info.kind.mime_type(), "image/heic-sequence");
    assert!(seq_info.has_primary_image);
    assert!(seq_info.has_sequence);
    assert_eq!((seq_info.width, seq_info.height), (64, 48));
    assert_eq!(seq_info, read_info_reader(Cursor::new(&seq)).unwrap());
}

#[cfg(feature = "encoder")]
#[test]
fn read_info_clamps_oversized_ispe_to_sps() {
    let src = heifer::Image::filled(64, 64, 8, [100, 120, 140, 255]);
    let mut data = heifer::encode(&src, &heifer::EncodeOptions::default()).unwrap();
    let needle = [b'i', b's', b'p', b'e', 0, 0, 0, 0, 0, 0, 0, 64, 0, 0, 0, 64];
    let pos = data
        .windows(needle.len())
        .position(|w| w == needle)
        .expect("ispe box not found");
    data[pos + 8..pos + 12].copy_from_slice(&6_906_977u32.to_be_bytes());
    data[pos + 12..pos + 16].copy_from_slice(&1_946_157_120u32.to_be_bytes());

    let info = read_info(&data).unwrap();
    assert_eq!(
        (info.width, info.height, info.bit_depth, info.has_alpha),
        (64, 64, 8, false)
    );
    assert_eq!(read_info_reader(Cursor::new(&data)).unwrap(), info);

    let decoded = heifer::decode(&data).unwrap();
    assert_eq!(
        (decoded.width, decoded.height, decoded.bit_depth, false),
        (64, 64, 8, false)
    );
}

fn avif_fixture(name: &str) -> Option<Vec<u8>> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/avif")
        .join(name);
    let bytes = std::fs::read(path).ok();
    if bytes.is_none() {
        eprintln!("skipping {name}: run scripts/fetch-fixtures.sh");
    }
    bytes
}

#[test]
fn read_info_describes_avif_files() {
    // AOMediaCodec/av1-avif test files: name, displayed size, bit depth, alpha, item type.
    for (name, size, bit_depth, alpha, item_type) in [
        (
            "fox.profile0.8bpc.yuv420.avif",
            (1204, 800),
            8,
            false,
            b"av01",
        ),
        (
            "fox.profile0.10bpc.yuv420.monochrome.avif",
            (1204, 800),
            10,
            false,
            b"av01",
        ),
        (
            "fox.profile1.8bpc.yuv444.avif",
            (1204, 800),
            8,
            false,
            b"av01",
        ),
        (
            "fox.profile2.12bpc.yuv422.odd-width.avif",
            (1203, 800),
            12,
            false,
            b"av01",
        ),
        (
            "reduced_still_picture_header.avif",
            (1280, 720),
            8,
            false,
            b"av01",
        ),
        // `irot` turns the 1920×1080 picture upright.
        ("Ronda_rotate90.avif", (1080, 1920), 8, false, b"av01"),
        ("bbb_alpha_inverted.avif", (3840, 2160), 8, true, b"av01"),
        (
            "Summer_in_Tomsk_720p_5x4_grid.avif",
            (6400, 2880),
            8,
            false,
            b"grid",
        ),
    ] {
        let Some(bytes) = avif_fixture(name) else {
            continue;
        };
        let info = read_info(&bytes).unwrap();
        assert_eq!((info.width, info.height), size, "{name}");
        assert_eq!(info.bit_depth, bit_depth, "{name}");
        assert_eq!(info.has_alpha, alpha, "{name}");
        assert_eq!(&info.item_type.0, item_type, "{name}");
        assert_eq!(info.kind, FileKind::Avif, "{name}");
        assert_eq!(
            (info.kind.as_str(), info.kind.mime_type()),
            ("avif", "image/avif")
        );
        assert!(info.has_primary_image && !info.has_sequence, "{name}");
        assert_eq!(
            info,
            read_info_reader(Cursor::new(&bytes)).unwrap(),
            "{name}"
        );
        // Decoding is not supported yet, and says so instead of failing in another way.
        assert!(
            matches!(heifer::decode(&bytes), Err(heifer::Error::Unsupported(_))),
            "{name}: {:?}",
            heifer::decode(&bytes).err()
        );
    }
}
