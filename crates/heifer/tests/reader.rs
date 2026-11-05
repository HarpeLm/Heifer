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
        let meta = heifer::read_metadata(&bytes).ok();
        let meta_reader = heifer::read_metadata_reader(Cursor::new(&bytes)).ok();
        assert_eq!(meta, meta_reader, "{name}");
    }
    assert!(compared >= 1);
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
        let reader = Failing {
            inner: Cursor::new(&bytes),
            read: 0,
            limit,
        };
        assert!(heifer::decode_reader(reader).is_err(), "limit {limit}");
    }
    // Truncated files: errors, never panics.
    for end in (0..bytes.len()).step_by(5) {
        assert!(
            heifer::decode_reader(Cursor::new(&bytes[..end])).is_err(),
            "end {end}"
        );
    }
}
