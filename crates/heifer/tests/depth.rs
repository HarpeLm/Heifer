//! Depth map metadata and auxiliary images. Run `scripts/fetch-fixtures.sh` first; missing
//! files are skipped.

use heifer::{DepthInfo, DepthRepresentation, Error};
use heifer_isobmff::HeifFile;

fn load(name: &str) -> Option<Vec<u8>> {
    let path = format!("{}/../../tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let data = std::fs::read(&path).ok();
    if data.is_none() {
        eprintln!("skipping: {path} not found (run scripts/fetch-fixtures.sh)");
    }
    data
}

fn close(actual: f64, expected: f64) -> bool {
    (actual - expected).abs() < 1e-9
}

#[test]
fn reads_depth_metadata_of_a_portrait() {
    let Some(bytes) = load("real/heif_other__pug.heic") else {
        return;
    };
    let auxiliary = heifer::auxiliary_images(&bytes).unwrap();
    let depth = auxiliary
        .iter()
        .find(|aux| aux.is_depth())
        .expect("the portrait has a depth map");
    assert_eq!((depth.width, depth.height), (768, 576));
    assert_eq!(depth.bit_depth, 8);

    let info = DepthInfo::read(&bytes, depth.id).unwrap().unwrap();
    assert_eq!(info.representation, DepthRepresentation::UniformDisparity);
    assert_eq!(info.disparity_reference_view, Some(0));
    assert_eq!(info.z_near, None);
    assert_eq!(info.z_far, None);
    assert!(close(info.d_min.unwrap(), 1.498_046_875));
    assert!(close(info.d_max.unwrap(), 4.382_812_5));
    assert_eq!(info.representation.to_string(), "uniform disparity");

    let file = HeifFile::parse(&bytes).unwrap();
    assert!(
        !file
            .auxiliary_subtypes(depth.id)
            .unwrap()
            .unwrap()
            .is_empty()
    );

    let gain = auxiliary
        .iter()
        .find(|aux| aux.is_gain_map())
        .expect("the portrait has a gain map");
    assert_eq!((gain.width, gain.height), (2016, 1512));
}

#[test]
fn unci_depth_map_has_no_metadata() {
    let Some(bytes) = load("real/heif_special__unsupported_depth_image.heic") else {
        return;
    };
    let auxiliary = heifer::auxiliary_images(&bytes).unwrap();
    let depth = auxiliary
        .iter()
        .find(|aux| aux.is_depth())
        .expect("the file has a depth map");
    assert_eq!(depth.item_type.0, *b"unci");
    assert_eq!((depth.width, depth.height), (64, 32));

    let file = HeifFile::parse(&bytes).unwrap();
    let subtypes = file.auxiliary_subtypes(depth.id).unwrap();
    assert!(subtypes.is_none_or(<[u8]>::is_empty));
    assert_eq!(DepthInfo::read(&bytes, depth.id).unwrap(), None);

    // The fixture is deliberately unsupported: a `unci` item without an `uncC` configuration.
    let error = heifer::decode_auxiliary(&bytes, depth.id).unwrap_err();
    assert!(
        matches!(&error, Error::Unsupported(message) if message.contains("uncC")),
        "{error}"
    );
}

#[test]
fn items_without_depth_metadata_read_as_none() {
    let Some(bytes) = load("alpha.heic") else {
        return;
    };
    let file = HeifFile::parse(&bytes).unwrap();
    assert_eq!(file.auxiliary_subtypes(file.primary_id).unwrap(), None);
    assert_eq!(DepthInfo::read(&bytes, file.primary_id).unwrap(), None);
}
