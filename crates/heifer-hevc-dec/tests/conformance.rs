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

/// Decodes every picture of each conformance bitstream with the sequence decoder, checking
/// each one against its decoded picture hash. Streams using tools that are not supported yet
/// (B slices, long-term references) are counted, not failed.
#[test]
fn conformance_sequences() {
    use heifer_hevc_dec::nal::access_units;
    use heifer_hevc_dec::sequence::SequenceDecoder;

    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/conformance");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        eprintln!("skipping: {} not found", dir.display());
        return;
    };
    let (mut passed, mut pictures, mut unsupported) = (0, 0, 0);
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
        let mut decoder = SequenceDecoder::new(DecodeOptions {
            verify_hash: true,
            ..Default::default()
        });
        let mut ok = true;
        for (i, unit) in access_units(&data).into_iter().enumerate() {
            match decoder.decode(unit) {
                Ok(_) => pictures += 1,
                // RASL pictures of the first random access point are not output (§8.1.3).
                Err(Error::Unimplemented(what)) if what.contains("RASL") => {}
                Err(Error::Unimplemented(_)) => {
                    unsupported += 1;
                    ok = false;
                    break;
                }
                Err(e) => {
                    failures.push(format!("{name}: picture {i}: {e}"));
                    ok = false;
                    break;
                }
            }
        }
        passed += usize::from(ok);
    }
    eprintln!(
        "sequences: {passed} streams ({pictures} pictures) verified, {unsupported} unsupported"
    );
    assert!(failures.is_empty(), "failures:\n{}", failures.join("\n"));
}
