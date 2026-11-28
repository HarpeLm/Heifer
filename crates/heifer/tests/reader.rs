//! Decoding from a reader gives exactly what decoding from a slice gives.

use std::io::{Cursor, Read, Seek, SeekFrom};

/// Every HEIF fixture available locally, plus one encoded here so the test always runs.
fn files() -> Vec<(String, Vec<u8>)> {
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
    let rgba: Vec<u8> = (0..700 * 600 * 4).map(|i| (i % 251) as u8).collect();
    let encoded = heifer::encode_rgba8(700, 600, &rgba, &Default::default()).unwrap();
    out.push(("encoded grid with alpha and thumbnail".into(), encoded));
    out
}

#[test]
fn reader_and_slice_decode_identically() {
    let mut compared = 0;
    for (name, bytes) in files() {
        let from_slice = heifer::decode(&bytes);
        let from_reader = heifer::decode_reader(Cursor::new(&bytes));
        match (from_slice, from_reader) {
            (Ok(a), Ok(b)) => {
                assert_eq!(a, b, "{name}");
                compared += 1;
            }
            (Err(_), Err(_)) => {}
            (a, b) => panic!("{name}: slice {:?}, reader {:?}", a.err(), b.err()),
        }
        let rgba8_slice = heifer::decode_rgba8(&bytes, &heifer::Options::default());
        let rgba8_reader =
            heifer::decode_reader_rgba8(Cursor::new(&bytes), &heifer::Options::default());
        match (rgba8_slice, rgba8_reader) {
            (Ok(a), Ok(b)) => assert_eq!(a, b, "{name}: rgba8"),
            (Err(_), Err(_)) => {}
            (a, b) => panic!("{name}: rgba8 slice {:?}, reader {:?}", a.err(), b.err()),
        }
        let info = heifer::read_info(&bytes).ok();
        let info_reader = heifer::read_info_reader(Cursor::new(&bytes)).ok();
        assert_eq!(info, info_reader, "{name}: info");
        let meta = heifer::read_metadata(&bytes).ok();
        let meta_reader = heifer::read_metadata_reader(Cursor::new(&bytes)).ok();
        assert_eq!(meta, meta_reader, "{name}");
        let gm = heifer::read_gain_map(&bytes).ok();
        let gm_reader =
            heifer::read_gain_map_reader(Cursor::new(&bytes), &heifer::Options::default()).ok();
        assert_eq!(gm, gm_reader, "{name}: gain map");
    }
    assert!(compared >= 1);
}

#[test]
fn read_gain_map_reader_matches_slice_and_skips_primary_image_payload() {
    let (w, h) = (128u32, 96u32);
    let mut base_data = Vec::with_capacity((w * h * 4) as usize);
    let mut hdr_data = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            let v = ((x * 2 + y) % 200 + 20) as u16;
            base_data.extend([v, v, v, 255]);
            let linear = (f32::from(v) / 255.0).powf(2.2).max(0.01);
            let boost = if x >= w / 2 { 4.0 } else { 1.0 };
            hdr_data.extend([linear * boost; 3]);
        }
    }
    let base = heifer::Image {
        width: w,
        height: h,
        bit_depth: 8,
        has_alpha: false,
        data: base_data,
    };
    let hdr = heifer::HdrImage {
        width: w,
        height: h,
        headroom: 4.0,
        data: hdr_data,
    };
    let heic = heifer::encode_hdr(
        &base,
        &hdr,
        &heifer::EncodeOptions::new()
            .with_quality(80)
            .with_speed(heifer::EncodeSpeed::Fast),
    )
    .unwrap();

    let from_slice = heifer::read_gain_map(&heic)
        .unwrap()
        .expect("tmap gain map");
    let mut reader = Failing {
        inner: Cursor::new(&heic),
        read: 0,
        limit: usize::MAX,
    };
    let from_reader = heifer::read_gain_map_reader(&mut reader, &heifer::Options::default())
        .unwrap()
        .expect("tmap gain map from reader");
    assert_eq!(from_slice, from_reader);
    assert!(
        reader.read < heic.len(),
        "expected read_gain_map_reader to skip the primary image payload (read {} of {} bytes)",
        reader.read,
        heic.len()
    );
}

/// A reader that fails after `limit` bytes.
struct Failing<'a> {
    inner: Cursor<&'a [u8]>,
    read: usize,
    limit: usize,
}

impl Read for Failing<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.read >= self.limit {
            return Err(std::io::Error::other("disk unplugged"));
        }
        let n = buf.len().min(self.limit - self.read);
        let n = self.inner.read(&mut buf[..n])?;
        self.read += n;
        Ok(n)
    }
}

impl Seek for Failing<'_> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

#[test]
fn failing_reader_returns_an_error() {
    let rgba = vec![200u8; 64 * 64 * 4];
    let bytes = heifer::encode_rgba8(64, 64, &rgba, &Default::default()).unwrap();
    for limit in (0..bytes.len()).step_by(7) {
        let make = || Failing {
            inner: Cursor::new(&bytes),
            read: 0,
            limit,
        };
        assert!(heifer::decode_reader(make()).is_err(), "limit {limit}");
        assert!(
            heifer::decode_reader_rgba8(make(), &heifer::Options::default()).is_err(),
            "rgba8 limit {limit}"
        );
        if limit < 12 {
            assert!(
                heifer::read_info_reader(make()).is_err(),
                "info limit {limit}"
            );
        }
    }
    // Truncated files: errors, never panics.
    for end in (0..bytes.len()).step_by(5) {
        assert!(
            heifer::decode_reader(Cursor::new(&bytes[..end])).is_err(),
            "end {end}"
        );
        assert!(
            heifer::decode_reader_rgba8(Cursor::new(&bytes[..end]), &heifer::Options::default())
                .is_err(),
            "rgba8 end {end}"
        );
        let _ = heifer::read_info_reader(Cursor::new(&bytes[..end]));
    }
}
