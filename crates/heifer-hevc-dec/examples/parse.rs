//! Parses all slice data of an HEVC Annex B file (`.h265`) and reports statistics.
//! A correct parse ends exactly on the last CTU of the picture, at the end of the slice data.
//!
//! Usage: cargo run -p heifer-hevc-dec --example parse -- <file.h265>

use heifer_hevc_dec::nal::{NalUnit, split_annex_b};
use heifer_hevc_dec::params::ParameterSets;
use heifer_hevc_dec::slice::SliceHeader;
use heifer_hevc_dec::syntax::{CodingUnit, Picture, Sink, TransformBlock, decode_slice_segment};

#[derive(Default)]
struct Stats {
    cus: u32,
    nxn: u32,
    blocks: [u32; 3],
    coded: [u32; 3],
    nonzero: u64,
    max_abs: i32,
}

impl Sink for Stats {
    fn transform_block(&mut self, tb: &TransformBlock<'_>) {
        self.blocks[usize::from(tb.c_idx)] += 1;
        if let Some(c) = tb.coeffs {
            self.coded[usize::from(tb.c_idx)] += 1;
            self.nonzero += c.iter().filter(|&&v| v != 0).count() as u64;
            self.max_abs = self
                .max_abs
                .max(c.iter().map(|v| v.abs()).max().unwrap_or(0));
        }
    }
    fn coding_unit(&mut self, cu: &CodingUnit) {
        self.cus += 1;
        self.nxn += u32::from(cu.part_nxn);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: parse <file.h265>")?;
    let stream = std::fs::read(path)?;
    let mut sets = ParameterSets::default();
    let nals: Vec<_> = split_annex_b(&stream)
        .map(NalUnit::parse)
        .collect::<Result<_, _>>()?;
    for nal in &nals {
        sets.add(nal)?;
    }
    let mut stats = Stats::default();
    let mut picture: Option<Picture<'_>> = None;
    for nal in nals.iter().filter(|n| n.header.unit_type.is_slice()) {
        let header = SliceHeader::parse(&nal.rbsp, &nal.header, &sets, None)?;
        let (pps, sps) = sets.get(usize::from(header.slice_pic_parameter_set_id))?;
        let pic = picture.get_or_insert_with(|| Picture::new(sps, pps));
        let data = &nal.rbsp[header.header_size..];
        let s = decode_slice_segment(pic, &header, header.slice_qp_y(&sets)?, data, &mut stats)?;
        let trailing = &data[s.end_position..];
        println!(
            "slice: {} CTUs, ended at byte {} of {} (trailing: {:?})",
            s.ctus,
            s.end_position,
            data.len(),
            trailing
        );
    }
    let pic = picture.ok_or("no slice")?;
    println!(
        "picture complete: {}  CUs: {} (NxN: {})  blocks Y/Cb/Cr: {:?}  with residual: {:?}  nonzero coeffs: {}  max |coeff|: {}",
        pic.is_complete(),
        stats.cus,
        stats.nxn,
        stats.blocks,
        stats.coded,
        stats.nonzero,
        stats.max_abs
    );
    Ok(())
}
