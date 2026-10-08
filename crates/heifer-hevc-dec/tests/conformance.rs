//! Decodes the first picture of each official HEVC conformance bitstream and checks it against
//! the decoded picture hash (MD5/CRC/checksum) computed by the reference decoder, when the
//! stream carries one. Run `scripts/fetch-conformance.sh` first; skipped otherwise.

use heifer_hevc_dec::Error;
use heifer_hevc_dec::decoder::{DecodeOptions, HashCheck, decode_picture_checked};

#[test]
fn conformance_bitstreams() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/conformance");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        eprintln!(
            "skipping: {} not found (run scripts/fetch-conformance.sh)",
            dir.display()
        );
        return;
    };
    let (mut verified, mut unchecked, mut unsupported) = (0, 0, 0);
    let mut failures = Vec::new();
    for entry in entries.flatten().filter(|e| e.path().is_dir()) {
        let mut stack = vec![entry.path()];
        let mut stream = None;
        while let Some(p) = stack.pop() {
            for f in std::fs::read_dir(&p).into_iter().flatten().flatten() {
                let path = f.path();
                if path.is_dir() {
                    stack.push(path);
                } else if matches!(
                    path.extension().and_then(|e| e.to_str()),
                    Some("bit" | "bin")
                ) {
                    stream = Some(path);
                }
            }
        }
        let Some(path) = stream else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        let data = std::fs::read(&path).unwrap();
        let options = DecodeOptions {
            verify_hash: true,
            ..Default::default()
        };
        match decode_picture_checked(&data, options) {
            Ok((_, HashCheck::Verified(_))) => verified += 1,
            Ok((_, HashCheck::NotChecked)) => unchecked += 1,
            Err(Error::Unimplemented(_)) => unsupported += 1,
            Err(e) => failures.push(format!("{name}: {e}")),
        }
    }
    eprintln!(
        "conformance: {verified} verified by hash, {unchecked} without hash, {unsupported} unsupported"
    );
    assert!(failures.is_empty(), "failures:\n{}", failures.join("\n"));
}
