//! Walks the P slices of the inter-coded sequence fixtures in decoding order: derives each
//! picture's POC and builds its `RefPicList0` from the pictures decoded so far.
//!
//! This checks the reference picture set parsing, the POC state and the list construction
//! without decoding any sample data, which is the state of the decoder before inter prediction
//! lands. Missing fixtures are skipped.

use heifer_hevc_dec::nal::{NalUnit, NalUnitType, split_annex_b};
use heifer_hevc_dec::params::ParameterSets;
use heifer_hevc_dec::refs::{PocState, RefPic, build_ref_pic_list0};
use heifer_hevc_dec::slice::{SliceHeader, SliceType};
use heifer_isobmff::hevc_annex_b;
use heifer_isobmff::track::parse_tracks;

/// Returns the main video track of a fixture, i.e. the one that is not a thumbnail.
fn main_track(bytes: &[u8]) -> Option<(heifer_isobmff::track::Track, Vec<u8>)> {
    let tracks = parse_tracks(bytes).ok()?;
    let track = tracks.into_iter().find(|t| {
        t.hevc_config.is_some() && !t.references.iter().any(|(kind, _)| &kind.0 == b"thmb")
    })?;
    let config = track.hevc_config.as_ref()?;
    let stream = hevc_annex_b(config, track.sample_data(bytes, 0).ok()?).ok()?;
    Some((track, stream))
}

/// Checks one fixture: every picture is one sample, POCs follow display order, and every P
/// slice finds its references. Returns the number of P slices and the largest `RefPicList0`.
fn check_fixture(name: &str) -> Option<(usize, usize)> {
    let path = format!("{}/../../tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(_) => {
            eprintln!("skipping: {path} not found");
            return None;
        }
    };
    let (track, _) = main_track(&bytes)?;
    let config = track.hevc_config.as_ref()?;

    // Parameter sets from the first sample's configuration.
    let mut sets = ParameterSets::default();
    for nal in split_annex_b(&hevc_annex_b(config, track.sample_data(&bytes, 0).unwrap()).unwrap())
    {
        sets.add(&NalUnit::parse(nal).unwrap()).unwrap();
    }

    let mut poc = PocState::default();
    let mut decoded: Vec<RefPic> = Vec::new();
    let mut previous: Option<SliceHeader> = None;
    let mut p_slices = 0;
    let mut max_refs = 0;
    let mut pictures = 0;
    for index in 0..track.samples.len() {
        let data = track.sample_data(&bytes, index).unwrap();
        let stream = hevc_annex_b(config, data).unwrap();
        for nal in split_annex_b(&stream) {
            let nal = NalUnit::parse(nal).unwrap();
            if !nal.header.unit_type.is_slice() {
                continue;
            }
            let first_in_pic = nal.rbsp.first().is_some_and(|b| b & 0x80 != 0);
            let header = SliceHeader::parse(&nal.rbsp, &nal.header, &sets, previous.as_ref())
                .unwrap_or_else(|e| panic!("{name}: sample {index}: {e}"));
            if !first_in_pic {
                previous = Some(header);
                continue;
            }
            previous = Some(header.clone());
            pictures += 1;
            let sps = sets.sps[usize::from(
                sets.pps[usize::from(header.slice_pic_parameter_set_id)]
                    .as_ref()
                    .unwrap()
                    .pps_seq_parameter_set_id,
            )]
            .as_ref()
            .unwrap();
            let is_irap = matches!(nal.header.unit_type, NalUnitType::IrapSlice(_));
            let curr_poc = poc.next(
                header.slice_pic_order_cnt_lsb,
                sps.log2_max_pic_order_cnt_lsb,
                is_irap,
                PocState::is_prev_tid0_candidate(
                    match nal.header.unit_type {
                        NalUnitType::IrapSlice(t) | NalUnitType::OtherSlice(t) => t,
                        _ => unreachable!("a slice"),
                    },
                    nal.header.temporal_id,
                ),
            );
            match header.slice_type {
                SliceType::I => {}
                SliceType::P => {
                    p_slices += 1;
                    let list = build_ref_pic_list0(
                        curr_poc,
                        &header.st_rps,
                        &decoded,
                        usize::from(header.num_ref_idx_l0_active_minus1) + 1,
                        header.list_entry_l0.as_deref(),
                    )
                    .unwrap_or_else(|e| panic!("{name}: sample {index}: {e}"));
                    assert_eq!(
                        list.len(),
                        usize::from(header.num_ref_idx_l0_active_minus1) + 1
                    );
                    max_refs = max_refs.max(list.len());
                }
                SliceType::B => unreachable!("B slices are rejected by the header parser"),
            }
            // These clips are coded in display order, one picture per sample.
            assert_eq!(curr_poc, pictures as i32 - 1, "{name}: picture {index}");
            // Every picture can be used as a reference by a later one.
            decoded.push(RefPic {
                slot: decoded.len(),
                poc: curr_poc,
                long_term: false,
            });
        }
    }
    assert_eq!(
        pictures,
        track.samples.len(),
        "{name}: one picture per sample"
    );
    Some((p_slices, max_refs))
}

#[test]
fn candle_animation_references_are_available() {
    let slices = check_fixture("candle_animation.heic");
    if let Some((p_slices, max_refs)) = slices {
        assert_eq!(p_slices, 118, "all but the two IRAP pictures are P slices");
        assert_eq!(max_refs, 1);
    }
}

#[test]
fn sea1_animation_references_are_available() {
    let slices = check_fixture("sea1_animation.heic");
    if let Some((p_slices, max_refs)) = slices {
        assert_eq!(p_slices, 112);
        assert_eq!(max_refs, 1);
    }
}

#[test]
fn rally_burst_references_are_available() {
    let slices = check_fixture("rally_burst.heic");
    if let Some((p_slices, max_refs)) = slices {
        assert_eq!(p_slices, 59);
        assert_eq!(max_refs, 1);
    }
}
