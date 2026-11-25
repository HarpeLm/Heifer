//! Writing of HEIF files: HEVC-coded image items, an optional alpha plane, and their properties.
//!
//! ```
//! use heifer_isobmff::boxes::HevcConfig;
//! use heifer_isobmff::writer::{HeifWriter, ImageSpec};
//!
//! # let (vps, sps, pps, slice) = (vec![0x40, 1], vec![0x42, 1], vec![0x44, 1], vec![0x26, 1]);
//! let config = HevcConfig {
//!     profile_idc: 1,
//!     level_idc: 93,
//!     chroma_format: 1,
//!     bit_depth_luma: 8,
//!     bit_depth_chroma: 8,
//!     nal_length_size: 4,
//!     nal_units: vec![vps, sps, pps],
//! };
//! let mut writer = HeifWriter::new();
//! let image = writer.add_image(&ImageSpec::new(64, 48, config, &[slice]));
//! writer.set_primary(image);
//! let bytes = writer.finish();
//! let file = heifer_isobmff::HeifFile::parse(&bytes).unwrap();
//! assert_eq!(file.image_size(image).unwrap(), Some((64, 48)));
//! ```

use crate::ItemId;
use crate::boxes::{ColorInfo, HevcConfig};

/// `auxC` type of alpha planes.
pub const ALPHA_URN: &str = "urn:mpeg:hevc:2015:auxid:1";

/// `auxC` type of depth maps.
pub const DEPTH_URN: &str = "urn:mpeg:hevc:2015:auxid:2";

/// An HEVC-coded image to add to a file.
#[derive(Debug, Clone)]
pub struct ImageSpec<'a> {
    /// Width in pixels (`ispe`).
    pub width: u32,
    /// Height in pixels (`ispe`).
    pub height: u32,
    /// Decoder configuration (`hvcC`): parameter sets and stream properties. Its
    /// `nal_length_size` must be 4.
    pub config: HevcConfig,
    /// Slice segment NAL units of the picture, without start codes or length prefixes.
    pub slices: &'a [Vec<u8>],
    /// Colour information (`colr`), if any.
    pub color: Option<ColorInfo>,
    /// Auxiliary image type (`auxC`), e.g. [`ALPHA_URN`]; `None` for a regular image.
    pub auxiliary_type: Option<&'a str>,
    /// Whether the item is hidden (e.g. an alpha plane or a thumbnail).
    pub hidden: bool,
    /// Size to crop the image to, from its top-left corner (`clap`), when the coded picture is
    /// larger than the image (e.g. odd sizes in 4:2:0).
    pub crop: Option<(u32, u32)>,
}

impl<'a> ImageSpec<'a> {
    /// A visible image without colour information.
    pub fn new(width: u32, height: u32, config: HevcConfig, slices: &'a [Vec<u8>]) -> Self {
        Self {
            width,
            height,
            config,
            slices,
            color: None,
            auxiliary_type: None,
            hidden: false,
            crop: None,
        }
    }
}

/// An image sequence track (`moov`/`trak`, handler `pict`) to add to a file with
/// [`HeifWriter::set_sequence`]: one HEVC configuration, then the pictures in decoding order.
#[derive(Debug, Clone)]
pub struct SequenceTrack {
    width: u32,
    height: u32,
    timescale: u32,
    config: HevcConfig,
    /// Each sample's data (length-prefixed NAL units), duration, sync flag, and composition
    /// offset (`ctts`: display time minus decode time).
    samples: Vec<(Vec<u8>, u32, bool, i32)>,
    /// Optional auxiliary alpha sequence track (`tref` `auxl`, `auxi` = [`ALPHA_URN`]).
    alpha: Option<Box<SequenceTrack>>,
}

impl SequenceTrack {
    /// An empty track of `width`×`height` pictures, with durations counted in `timescale`
    /// units per second. `config` holds the parameter sets shared by every picture; its
    /// `nal_length_size` must be 4.
    pub fn new(width: u32, height: u32, timescale: u32, config: HevcConfig) -> Self {
        Self {
            width,
            height,
            timescale,
            config,
            samples: Vec::new(),
            alpha: None,
        }
    }

    /// Attaches an auxiliary alpha sequence track (`auxl`, [`ALPHA_URN`]) to this track.
    pub fn set_alpha(&mut self, alpha: SequenceTrack) {
        self.alpha = Some(Box::new(alpha));
    }

    /// Adds a picture: its slice segment NAL units (without start codes or length prefixes),
    /// how long it is shown, and whether it is a sync sample (decodable on its own).
    pub fn add_sample(&mut self, slices: &[Vec<u8>], duration: u32, sync: bool) {
        self.add_sample_with_offset(slices, duration, sync, 0);
    }

    /// Like [`add_sample`](Self::add_sample), with a composition offset (`ctts`, display time
    /// minus decode time in [`timescale`](Self::new) units) for reordered frames (B frames).
    /// When any offset is negative, all `ctts` entries are shifted to non-negative `ctts` v0
    /// values and paired with an `edts`/`elst` edit list so `PTS >= DTS` for every sample.
    pub fn add_sample_with_offset(
        &mut self,
        slices: &[Vec<u8>],
        duration: u32,
        sync: bool,
        composition_offset: i32,
    ) {
        let mut data = Vec::new();
        for nal in slices {
            data.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            data.extend_from_slice(nal);
        }
        self.samples
            .push((data, duration, sync, composition_offset));
    }

    /// Number of pictures added.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Whether no picture was added.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    fn own_data_size(&self) -> u64 {
        self.samples.iter().map(|s| s.0.len() as u64).sum()
    }

    fn data_size(&self) -> u64 {
        self.own_data_size() + self.alpha.as_ref().map_or(0, |a| a.data_size())
    }

    /// The `moov` box, with the samples stored as one chunk at `chunk_offset`, a 64-bit
    /// offset (`co64`) when `large`, else 32-bit (`stco`, the one every reader supports). Its
    /// size does not depend on the offset.
    fn moov_box(&self, chunk_offset: u64, large: bool) -> Vec<u8> {
        let duration: u64 = self.samples.iter().map(|s| u64::from(s.1)).sum();
        let duration32 = u32::try_from(duration).unwrap_or(u32::MAX);
        const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];
        let matrix = |b: &mut Vec<u8>| {
            for v in MATRIX {
                b.extend_from_slice(&v.to_be_bytes());
            }
        };
        let next_track_id = if self.alpha.is_some() { 3u32 } else { 2u32 };
        make_box(b"moov", |moov| {
            moov.extend(full_box(b"mvhd", 0, 0, |b| {
                b.extend_from_slice(&[0; 8]); // creation and modification times
                b.extend_from_slice(&self.timescale.to_be_bytes());
                b.extend_from_slice(&duration32.to_be_bytes());
                b.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
                b.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
                b.extend_from_slice(&[0; 10]); // reserved
                matrix(b);
                b.extend_from_slice(&[0; 24]); // pre_defined
                b.extend_from_slice(&next_track_id.to_be_bytes()); // next_track_ID
            }));
            moov.extend(self.trak_box(1, None, chunk_offset, large, self.timescale));
            if let Some(alpha) = &self.alpha {
                moov.extend(alpha.trak_box(
                    2,
                    Some((1, ALPHA_URN)),
                    chunk_offset + self.own_data_size(),
                    large,
                    self.timescale,
                ));
            }
        })
    }

    fn trak_box(
        &self,
        track_id: u32,
        aux_for: Option<(u32, &str)>,
        chunk_offset: u64,
        large: bool,
        movie_timescale: u32,
    ) -> Vec<u8> {
        let duration: u64 = self.samples.iter().map(|s| u64::from(s.1)).sum();
        let duration32 = u32::try_from(duration).unwrap_or(u32::MAX);
        let min_off = self.samples.iter().map(|s| s.3).min().unwrap_or(0);
        let shift = if min_off < 0 { -i64::from(min_off) } else { 0 };
        let mut dts = 0i64;
        let first_pts = self
            .samples
            .iter()
            .map(|s| {
                let pts = dts + i64::from(s.3) + shift;
                dts += i64::from(s.1);
                pts
            })
            .min()
            .unwrap_or(0);
        const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];
        let matrix = |b: &mut Vec<u8>| {
            for v in MATRIX {
                b.extend_from_slice(&v.to_be_bytes());
            }
        };
        let tkhd_flags = if aux_for.is_some() { 0 } else { 7 };
        let handler = if aux_for.is_some() { b"auxv" } else { b"pict" };
        let aux_urn = aux_for.map(|(_, urn)| urn);
        make_box(b"trak", |trak| {
            // Main track: enabled, in the movie and in the preview; auxiliary track: hidden.
            trak.extend(full_box(b"tkhd", 0, tkhd_flags, |b| {
                b.extend_from_slice(&[0; 8]); // creation and modification times
                b.extend_from_slice(&track_id.to_be_bytes()); // track_ID
                b.extend_from_slice(&[0; 4]); // reserved
                b.extend_from_slice(&duration32.to_be_bytes());
                b.extend_from_slice(&[0; 8]); // reserved
                b.extend_from_slice(&[0; 8]); // layer, alternate_group, volume, reserved
                matrix(b);
                b.extend_from_slice(&(self.width << 16).to_be_bytes());
                b.extend_from_slice(&(self.height << 16).to_be_bytes());
            }));
            if let Some((main_id, _)) = aux_for {
                trak.extend(make_box(b"tref", |tref| {
                    tref.extend(make_box(b"auxl", |a| {
                        a.extend_from_slice(&main_id.to_be_bytes());
                    }));
                }));
            }
            if first_pts > 0 {
                let seg_duration = if self.timescale == movie_timescale || self.timescale == 0 {
                    duration
                } else {
                    duration.saturating_mul(u64::from(movie_timescale)) / u64::from(self.timescale)
                };
                let v1 = seg_duration > u64::from(u32::MAX) || first_pts > i64::from(i32::MAX);
                trak.extend(make_box(b"edts", |edts| {
                    edts.extend(full_box(b"elst", u8::from(v1), 0, |b| {
                        b.extend_from_slice(&1u32.to_be_bytes()); // entry_count
                        if v1 {
                            b.extend_from_slice(&seg_duration.to_be_bytes());
                            b.extend_from_slice(&first_pts.to_be_bytes());
                        } else {
                            b.extend_from_slice(&(seg_duration as u32).to_be_bytes());
                            b.extend_from_slice(&(first_pts as i32).to_be_bytes());
                        }
                        b.extend_from_slice(&1i16.to_be_bytes()); // media_rate_integer
                        b.extend_from_slice(&0i16.to_be_bytes()); // media_rate_fraction
                    }));
                }));
            }
            trak.extend(make_box(b"mdia", |mdia| {
                mdia.extend(full_box(b"mdhd", 0, 0, |b| {
                    b.extend_from_slice(&[0; 8]); // creation and modification times
                    b.extend_from_slice(&self.timescale.to_be_bytes());
                    b.extend_from_slice(&duration32.to_be_bytes());
                    b.extend_from_slice(&0x55C4u16.to_be_bytes()); // language `und`
                    b.extend_from_slice(&[0; 2]); // pre_defined
                }));
                mdia.extend(full_box(b"hdlr", 0, 0, |b| {
                    b.extend_from_slice(&[0; 4]); // pre_defined
                    b.extend_from_slice(handler);
                    b.extend_from_slice(&[0; 12]); // reserved
                    b.push(0); // empty name
                }));
                mdia.extend(make_box(b"minf", |minf| {
                    minf.extend(full_box(b"vmhd", 0, 1, |b| b.extend_from_slice(&[0; 8])));
                    minf.extend(make_box(b"dinf", |dinf| {
                        dinf.extend(full_box(b"dref", 0, 0, |b| {
                            b.extend_from_slice(&1u32.to_be_bytes());
                            // The data is in this file.
                            b.extend(full_box(b"url ", 0, 1, |_| {}));
                        }));
                    }));
                    minf.extend(self.stbl_box(chunk_offset, large, aux_urn, shift));
                }));
            }));
        })
    }

    fn stbl_box(
        &self,
        chunk_offset: u64,
        large: bool,
        aux_urn: Option<&str>,
        ctts_shift: i64,
    ) -> Vec<u8> {
        make_box(b"stbl", |stbl| {
            stbl.extend(full_box(b"stsd", 0, 0, |b| {
                b.extend_from_slice(&1u32.to_be_bytes());
                b.extend(make_box(b"hvc1", |e| {
                    // VisualSampleEntry (ISO/IEC 14496-12 §12.1.3): 78 bytes before the boxes.
                    e.extend_from_slice(&[0; 6]); // reserved
                    e.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
                    e.extend_from_slice(&[0; 16]); // pre_defined and reserved
                    e.extend_from_slice(&(self.width as u16).to_be_bytes());
                    e.extend_from_slice(&(self.height as u16).to_be_bytes());
                    e.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // 72 dpi
                    e.extend_from_slice(&0x0048_0000u32.to_be_bytes());
                    e.extend_from_slice(&[0; 4]); // reserved
                    e.extend_from_slice(&1u16.to_be_bytes()); // frame_count
                    e.extend_from_slice(&[0; 32]); // compressorname
                    e.extend_from_slice(&0x0018u16.to_be_bytes()); // depth
                    e.extend_from_slice(&0xFFFFu16.to_be_bytes()); // pre_defined -1
                    e.extend(hvcc_box(&self.config));
                    // Coding constraints (ISO/IEC 23008-12 §7.3.4): intra prediction is used;
                    // with only sync samples, no picture refers to another one.
                    let all_intra = self.samples.iter().all(|s| s.2);
                    e.extend(full_box(b"ccst", 0, 0, |c| {
                        let mut v = 1u32 << 30; // intra_pred_used
                        if all_intra {
                            v |= 1 << 31; // all_ref_pics_intra, max_ref_per_pic 0
                        } else {
                            v |= 15 << 26; // max_ref_per_pic: any
                        }
                        c.extend_from_slice(&v.to_be_bytes());
                    }));
                    if let Some(urn) = aux_urn {
                        e.extend(full_box(b"auxi", 0, 0, |a| {
                            a.extend_from_slice(urn.as_bytes());
                            a.push(0);
                        }));
                    }
                }));
            }));
            // Durations, as runs of equal values.
            let mut runs: Vec<(u32, u32)> = Vec::new();
            for s in &self.samples {
                match runs.last_mut() {
                    Some((n, d)) if *d == s.1 => *n += 1,
                    _ => runs.push((1, s.1)),
                }
            }
            stbl.extend(full_box(b"stts", 0, 0, |b| {
                b.extend_from_slice(&(runs.len() as u32).to_be_bytes());
                for (n, d) in &runs {
                    b.extend_from_slice(&n.to_be_bytes());
                    b.extend_from_slice(&d.to_be_bytes());
                }
            }));
            // Composition offsets (`ctts`), when any sample is reordered (e.g. B frames).
            // All entries are shifted into non-negative `ctts` v0 offsets (`PTS >= DTS`).
            if self.samples.iter().any(|s| s.3 != 0) {
                let mut ctts_runs: Vec<(u32, u32)> = Vec::new();
                for s in &self.samples {
                    let shifted =
                        u32::try_from((i64::from(s.3) + ctts_shift).max(0)).unwrap_or(u32::MAX);
                    match ctts_runs.last_mut() {
                        Some((n, off)) if *off == shifted => *n += 1,
                        _ => ctts_runs.push((1, shifted)),
                    }
                }
                stbl.extend(full_box(b"ctts", 0, 0, |b| {
                    b.extend_from_slice(&(ctts_runs.len() as u32).to_be_bytes());
                    for &(n, off) in &ctts_runs {
                        b.extend_from_slice(&n.to_be_bytes());
                        b.extend_from_slice(&off.to_be_bytes());
                    }
                }));
            }
            // Without `stss`, every sample is a sync sample.
            if self.samples.iter().any(|s| !s.2) {
                stbl.extend(full_box(b"stss", 0, 0, |b| {
                    let sync: Vec<u32> = (1..=self.samples.len() as u32)
                        .filter(|&i| self.samples[i as usize - 1].2)
                        .collect();
                    b.extend_from_slice(&(sync.len() as u32).to_be_bytes());
                    for i in sync {
                        b.extend_from_slice(&i.to_be_bytes());
                    }
                }));
            }
            // One chunk holding every sample.
            stbl.extend(full_box(b"stsc", 0, 0, |b| {
                b.extend_from_slice(&1u32.to_be_bytes());
                b.extend_from_slice(&1u32.to_be_bytes()); // first_chunk
                b.extend_from_slice(&(self.samples.len() as u32).to_be_bytes());
                b.extend_from_slice(&1u32.to_be_bytes()); // sample_description_index
            }));
            stbl.extend(full_box(b"stsz", 0, 0, |b| {
                b.extend_from_slice(&0u32.to_be_bytes()); // sizes listed
                b.extend_from_slice(&(self.samples.len() as u32).to_be_bytes());
                for s in &self.samples {
                    b.extend_from_slice(&(s.0.len() as u32).to_be_bytes());
                }
            }));
            if large {
                stbl.extend(full_box(b"co64", 0, 0, |b| {
                    b.extend_from_slice(&1u32.to_be_bytes());
                    b.extend_from_slice(&chunk_offset.to_be_bytes());
                }));
            } else {
                stbl.extend(full_box(b"stco", 0, 0, |b| {
                    b.extend_from_slice(&1u32.to_be_bytes());
                    b.extend_from_slice(&(chunk_offset as u32).to_be_bytes());
                }));
            }
        })
    }
}

#[derive(Debug)]
struct WriterItem {
    id: ItemId,
    item_type: [u8; 4],
    hidden: bool,
    /// Indices into `HeifWriter::properties` (1-based in `ipma`), with their `essential` flag.
    properties: Vec<(u16, bool)>,
    /// MIME type of `mime` items.
    content_type: Option<&'static str>,
    data: Vec<u8>,
}

/// Builds a HEIF file item by item.
#[derive(Debug, Default)]
pub struct HeifWriter {
    items: Vec<WriterItem>,
    /// Property boxes of `ipco`, complete (header included).
    properties: Vec<Vec<u8>>,
    /// `iref` entries: type, from, to.
    references: Vec<([u8; 4], ItemId, Vec<ItemId>)>,
    /// `grpl` entity groups (`altr`), in order.
    groups: Vec<Vec<ItemId>>,
    primary: Option<ItemId>,
    /// Whether the file has a `tmap` item, which adds the `tmap` compatible brand.
    has_tmap: bool,
    /// The image sequence track, if any.
    sequence: Option<SequenceTrack>,
}

impl HeifWriter {
    /// An empty file.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an `hvc1` image item and returns its ID. The first image added is the primary
    /// image unless [`set_primary`](Self::set_primary) says otherwise.
    pub fn add_image(&mut self, image: &ImageSpec<'_>) -> ItemId {
        let mut properties = vec![
            (self.property(hvcc_box(&image.config)), true),
            (
                self.property(full_box(b"ispe", 0, 0, |b| {
                    b.extend_from_slice(&image.width.to_be_bytes());
                    b.extend_from_slice(&image.height.to_be_bytes());
                })),
                false,
            ),
        ];
        let channels: &[u8] = if image.config.chroma_format == 0 {
            &[image.config.bit_depth_luma]
        } else {
            &[
                image.config.bit_depth_luma,
                image.config.bit_depth_chroma,
                image.config.bit_depth_chroma,
            ]
        };
        properties.push((
            self.property(full_box(b"pixi", 0, 0, |b| {
                b.push(channels.len() as u8);
                b.extend_from_slice(channels);
            })),
            false,
        ));
        if let Some(color) = &image.color {
            properties.push((self.property(colr_box(color)), false));
        }
        if let Some(urn) = image.auxiliary_type {
            properties.push((
                self.property(full_box(b"auxC", 0, 0, |b| {
                    b.extend_from_slice(urn.as_bytes());
                    b.push(0);
                })),
                true,
            ));
        }
        if let Some((w, h)) = image.crop {
            // Offsets of the centre of the kept area from the centre of the image, in halves.
            let clap = make_box(b"clap", |b| {
                for v in [w, 1, h, 1] {
                    b.extend_from_slice(&v.to_be_bytes());
                }
                for (kept, full) in [(w, image.width), (h, image.height)] {
                    let offset = i64::from(kept) - i64::from(full);
                    b.extend_from_slice(&(offset as i32).to_be_bytes());
                    b.extend_from_slice(&2u32.to_be_bytes());
                }
            });
            properties.push((self.property(clap), true));
        }
        let mut data = Vec::new();
        for nal in image.slices {
            data.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            data.extend_from_slice(nal);
        }
        self.add_item(*b"hvc1", image.hidden, properties, data)
    }

    /// Adds a `grid` derived image made of `tiles` (row by row, all of the same size), cropped
    /// to `width × height`, and returns its ID. For an alpha plane of another grid, pass
    /// [`ALPHA_URN`] as `auxiliary_type`, then [`add_reference`](Self::add_reference) `auxl`.
    #[allow(clippy::too_many_arguments)]
    pub fn add_grid(
        &mut self,
        rows: u16,
        columns: u16,
        width: u32,
        height: u32,
        tiles: &[ItemId],
        color: Option<&ColorInfo>,
        auxiliary_type: Option<&str>,
    ) -> ItemId {
        self.add_grid_item(
            rows,
            columns,
            width,
            height,
            tiles,
            color,
            auxiliary_type,
            auxiliary_type.is_some(),
            auxiliary_type.is_none(),
        )
    }

    /// Adds a hidden `grid` derived image without an auxiliary type, e.g. a gain map. The grid
    /// does not become the primary item.
    pub fn add_hidden_grid(
        &mut self,
        rows: u16,
        columns: u16,
        width: u32,
        height: u32,
        tiles: &[ItemId],
        color: Option<&ColorInfo>,
    ) -> ItemId {
        self.add_grid_item(
            rows, columns, width, height, tiles, color, None, true, false,
        )
    }

    /// Builds a `grid` item. `hidden` flags the item, and `primary` makes it the file's image.
    #[allow(clippy::too_many_arguments)]
    fn add_grid_item(
        &mut self,
        rows: u16,
        columns: u16,
        width: u32,
        height: u32,
        tiles: &[ItemId],
        color: Option<&ColorInfo>,
        auxiliary_type: Option<&str>,
        hidden: bool,
        primary: bool,
    ) -> ItemId {
        let mut properties = vec![(
            self.property(full_box(b"ispe", 0, 0, |b| {
                b.extend_from_slice(&width.to_be_bytes());
                b.extend_from_slice(&height.to_be_bytes());
            })),
            false,
        )];
        if let Some(color) = color {
            properties.push((self.property(colr_box(color)), false));
        }
        if let Some(urn) = auxiliary_type {
            properties.push((
                self.property(full_box(b"auxC", 0, 0, |b| {
                    b.extend_from_slice(urn.as_bytes());
                    b.push(0);
                })),
                true,
            ));
        }
        let large = width > 0xFFFF || height > 0xFFFF;
        let mut data = vec![0, u8::from(large), (rows - 1) as u8, (columns - 1) as u8];
        if large {
            data.extend_from_slice(&width.to_be_bytes());
            data.extend_from_slice(&height.to_be_bytes());
        } else {
            data.extend_from_slice(&(width as u16).to_be_bytes());
            data.extend_from_slice(&(height as u16).to_be_bytes());
        }
        let id = self.add_item(*b"grid", hidden, properties, data);
        self.add_reference(*b"dimg", id, tiles);
        if primary {
            self.primary = Some(id); // the grid, not its tiles, is the image
        }
        id
    }

    /// Adds a `tmap` derived image item (an ISO 21496-1 gain map) referencing `base` and `map`,
    /// and returns its ID. `payload` is the item's data (the tmap payload, version byte first).
    /// The item takes the size and colour of the base image, is grouped with it in an `altr`
    /// entity group, and makes `tmap` a compatible brand of the file.
    pub fn add_tmap(
        &mut self,
        base: ItemId,
        map: ItemId,
        width: u32,
        height: u32,
        color: Option<&ColorInfo>,
        payload: &[u8],
    ) -> ItemId {
        let mut properties = vec![(
            self.property(full_box(b"ispe", 0, 0, |b| {
                b.extend_from_slice(&width.to_be_bytes());
                b.extend_from_slice(&height.to_be_bytes());
            })),
            false,
        )];
        if let Some(color) = color {
            properties.push((self.property(colr_box(color)), false));
        }
        let id = self.add_item(*b"tmap", false, properties, payload.to_vec());
        self.add_reference(*b"dimg", id, &[base, map]);
        // The tone-mapped rendition and the base image are alternates of each other.
        self.add_alternative_group(&[id, base]);
        self.has_tmap = true;
        id
    }

    /// Adds an `altr` entity group (`grpl`) listing `items` in preference order.
    pub fn add_alternative_group(&mut self, items: &[ItemId]) {
        if !items.is_empty() {
            self.groups.push(items.to_vec());
        }
    }

    /// Adds an alpha plane (a hidden auxiliary image) to `image`.
    pub fn add_alpha(&mut self, image: ItemId, alpha: &ImageSpec<'_>) -> ItemId {
        let alpha = self.add_image(&ImageSpec {
            auxiliary_type: Some(ALPHA_URN),
            hidden: true,
            ..alpha.clone()
        });
        self.add_reference(*b"auxl", alpha, &[image]);
        alpha
    }

    /// Adds an `Exif` metadata item describing `image`. `tiff` is the TIFF data, starting with
    /// `II*\0` or `MM\0*`.
    pub fn add_exif(&mut self, image: ItemId, tiff: &[u8]) -> ItemId {
        let mut data = 0u32.to_be_bytes().to_vec(); // offset to the TIFF header
        data.extend_from_slice(tiff);
        let id = self.add_item(*b"Exif", true, Vec::new(), data);
        self.add_reference(*b"cdsc", id, &[image]);
        id
    }

    /// Adds an XMP packet (a `mime` item of type `application/rdf+xml`) describing `image`.
    pub fn add_xmp(&mut self, image: ItemId, xmp: &[u8]) -> ItemId {
        let id = self.add_item(*b"mime", true, Vec::new(), xmp.to_vec());
        if let Some(item) = self.items.last_mut() {
            item.content_type = Some("application/rdf+xml");
        }
        self.add_reference(*b"cdsc", id, &[image]);
        id
    }

    /// Adds a region item (`rgan`) describing `image`, with an optional `udes` description.
    /// `masks` are the mask image items of its referenced-mask regions, in order.
    pub fn add_region_item(
        &mut self,
        image: ItemId,
        regions: &crate::region::RegionItem,
        masks: &[ItemId],
        description: Option<&crate::region::UserDescription>,
    ) -> Result<ItemId, crate::Error> {
        let data = regions.encode()?;
        let properties = match description {
            Some(d) => {
                let mut b = ((8 + d.encode().len()) as u32).to_be_bytes().to_vec();
                b.extend_from_slice(b"udes");
                b.extend_from_slice(&d.encode());
                vec![(self.property(b), false)]
            }
            None => Vec::new(),
        };
        let id = self.add_item(*b"rgan", true, properties, data);
        self.add_reference(*b"cdsc", id, &[image]);
        if !masks.is_empty() {
            self.add_reference(*b"mask", id, masks);
        }
        Ok(id)
    }

    /// Adds an ICC profile (`colr` of type `prof`) to an image or grid item already added.
    /// Unknown items are ignored.
    pub fn add_icc_profile(&mut self, item: ItemId, icc: &[u8]) {
        let index = self.property(colr_box(&ColorInfo::Icc(icc.to_vec())));
        let properties = &self.properties;
        if let Some(item) = self.items.iter_mut().find(|i| i.id == item) {
            // Descriptive properties go before transformative ones such as `clap`.
            let position = item
                .properties
                .iter()
                .position(|&(i, _)| properties[usize::from(i) - 1][4..8] == *b"clap")
                .unwrap_or(item.properties.len());
            item.properties.insert(position, (index, false));
        }
    }

    /// Adds a reference of type `ref_type` from `from` to `to` (`iref`).
    pub fn add_reference(&mut self, ref_type: [u8; 4], from: ItemId, to: &[ItemId]) {
        self.references.push((ref_type, from, to.to_vec()));
    }

    /// Sets the primary item (`pitm`).
    pub fn set_primary(&mut self, id: ItemId) {
        self.primary = Some(id);
    }

    /// Adds an image sequence track: the file gets a `moov` box and the `msf1` and `hevc`
    /// brands. Its samples are stored after the item data, in the same `mdat`.
    pub fn set_sequence(&mut self, track: SequenceTrack) {
        self.sequence = Some(track);
    }

    fn property(&mut self, b: Vec<u8>) -> u16 {
        // Identical properties (e.g. the `ispe` of an image and its alpha plane) are shared.
        let index = match self.properties.iter().position(|p| *p == b) {
            Some(i) => i,
            None => {
                self.properties.push(b);
                self.properties.len() - 1
            }
        };
        index as u16 + 1
    }

    fn add_item(
        &mut self,
        item_type: [u8; 4],
        hidden: bool,
        properties: Vec<(u16, bool)>,
        data: Vec<u8>,
    ) -> ItemId {
        let id = self.items.len() as ItemId + 1;
        self.items.push(WriterItem {
            id,
            item_type,
            hidden,
            properties,
            content_type: None,
            data,
        });
        if self.primary.is_none() && item_type == *b"hvc1" && !hidden {
            self.primary = Some(id);
        }
        id
    }

    /// Writes the file: `ftyp`, `meta`, then all item data in one `mdat`.
    pub fn finish(self) -> Vec<u8> {
        let items_size: u64 = self.items.iter().map(|i| i.data.len() as u64).sum();
        let data_size = items_size + self.sequence.as_ref().map_or(0, SequenceTrack::data_size);
        // 64-bit offsets and lengths only when needed.
        let field_size: u8 = if data_size > u64::from(u32::MAX) - (1 << 20) {
            8
        } else {
            4
        };

        let ftyp = make_box(b"ftyp", |b| {
            b.extend_from_slice(b"heic");
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(b"mif1heic");
            if self.has_tmap {
                b.extend_from_slice(b"tmap");
            }
            if self.sequence.is_some() {
                b.extend_from_slice(b"msf1hevc");
            }
        });
        // The `iloc` offsets depend on the size of `meta`, which does not depend on their
        // values: write it once with placeholder offsets, then again with the real ones.
        let meta_size = self.meta_box(0, field_size).len() as u64;
        let mdat_header = if data_size + 8 > u64::from(u32::MAX) {
            16
        } else {
            8
        };
        // The `moov` box goes between `meta` and `mdat`; its size does not depend on offsets.
        // 32-bit chunk offsets unless the file may pass 4 GiB (the headers take far less
        // than the 1 MiB margin).
        let large = data_size > u64::from(u32::MAX) - (1 << 20);
        let moov_size = self
            .sequence
            .as_ref()
            .map_or(0, |t| t.moov_box(0, large).len() as u64);
        let first_offset = ftyp.len() as u64 + meta_size + moov_size + mdat_header;
        let meta = self.meta_box(first_offset, field_size);
        let moov = self
            .sequence
            .as_ref()
            .map(|t| t.moov_box(first_offset + items_size, large));

        let mut out = Vec::with_capacity((first_offset + data_size) as usize);
        out.extend_from_slice(&ftyp);
        out.extend_from_slice(&meta);
        if let Some(moov) = &moov {
            out.extend_from_slice(moov);
        }
        if mdat_header == 16 {
            out.extend_from_slice(&1u32.to_be_bytes());
            out.extend_from_slice(b"mdat");
            out.extend_from_slice(&(data_size + 16).to_be_bytes());
        } else {
            out.extend_from_slice(&((data_size + 8) as u32).to_be_bytes());
            out.extend_from_slice(b"mdat");
        }
        for item in &self.items {
            out.extend_from_slice(&item.data);
        }
        if let Some(track) = &self.sequence {
            for sample in &track.samples {
                out.extend_from_slice(&sample.0);
            }
            if let Some(alpha) = &track.alpha {
                for sample in &alpha.samples {
                    out.extend_from_slice(&sample.0);
                }
            }
        }
        out
    }

    fn meta_box(&self, first_offset: u64, field_size: u8) -> Vec<u8> {
        let large_ids = self.items.len() > usize::from(u16::MAX);
        let id = |b: &mut Vec<u8>, id: ItemId| {
            if large_ids {
                b.extend_from_slice(&id.to_be_bytes());
            } else {
                b.extend_from_slice(&(id as u16).to_be_bytes());
            }
        };
        full_box(b"meta", 0, 0, |meta| {
            meta.extend(full_box(b"hdlr", 0, 0, |b| {
                b.extend_from_slice(&[0; 4]); // pre_defined
                b.extend_from_slice(b"pict");
                b.extend_from_slice(&[0; 12]); // reserved
                b.push(0); // empty name
            }));
            let primary = self.primary.unwrap_or(1);
            meta.extend(full_box(b"pitm", u8::from(large_ids), 0, |b| {
                id(b, primary)
            }));

            // `iloc` version 0 (or 2 with 32-bit IDs); one extent per item, in the `mdat`.
            meta.extend(full_box(b"iloc", if large_ids { 2 } else { 0 }, 0, |b| {
                b.push((field_size << 4) | field_size);
                b.push(0); // base_offset_size 0, reserved
                if large_ids {
                    b.extend_from_slice(&(self.items.len() as u32).to_be_bytes());
                } else {
                    b.extend_from_slice(&(self.items.len() as u16).to_be_bytes());
                }
                let mut offset = first_offset;
                for item in &self.items {
                    id(b, item.id);
                    b.extend_from_slice(&0u16.to_be_bytes()); // data_reference_index
                    b.extend_from_slice(&1u16.to_be_bytes()); // extent_count
                    let len = item.data.len() as u64;
                    if field_size == 8 {
                        b.extend_from_slice(&offset.to_be_bytes());
                        b.extend_from_slice(&len.to_be_bytes());
                    } else {
                        b.extend_from_slice(&(offset as u32).to_be_bytes());
                        b.extend_from_slice(&(len as u32).to_be_bytes());
                    }
                    offset += len;
                }
            }));

            meta.extend(full_box(b"iinf", u8::from(large_ids), 0, |b| {
                if large_ids {
                    b.extend_from_slice(&(self.items.len() as u32).to_be_bytes());
                } else {
                    b.extend_from_slice(&(self.items.len() as u16).to_be_bytes());
                }
                for item in &self.items {
                    let version = if large_ids { 3 } else { 2 };
                    b.extend(full_box(b"infe", version, u32::from(item.hidden), |e| {
                        id(e, item.id);
                        e.extend_from_slice(&0u16.to_be_bytes()); // item_protection_index
                        e.extend_from_slice(&item.item_type);
                        e.push(0); // empty name
                        if let Some(content_type) = item.content_type {
                            e.extend_from_slice(content_type.as_bytes());
                            e.push(0);
                        }
                    }));
                }
            }));

            if !self.references.is_empty() {
                meta.extend(full_box(b"iref", u8::from(large_ids), 0, |b| {
                    for (ref_type, from, to) in &self.references {
                        b.extend(make_box(ref_type, |r| {
                            id(r, *from);
                            r.extend_from_slice(&(to.len() as u16).to_be_bytes());
                            for &t in to {
                                id(r, t);
                            }
                        }));
                    }
                }));
            }

            if !self.groups.is_empty() {
                // Entity group IDs share the item ID namespace (ISO/IEC 23008-12): a group
                // whose ID is an item ID makes decoders such as ImageIO ignore it. Number the
                // groups after the last item.
                let first = self.items.iter().map(|i| i.id).max().unwrap_or(0) + 1;
                meta.extend(make_box(b"grpl", |b| {
                    for (index, group) in self.groups.iter().enumerate() {
                        b.extend(make_box(b"altr", |g| {
                            g.extend_from_slice(&0u32.to_be_bytes()); // version 0, flags 0
                            g.extend_from_slice(&(first + index as u32).to_be_bytes());
                            g.extend_from_slice(&(group.len() as u32).to_be_bytes());
                            for &item in group {
                                g.extend_from_slice(&item.to_be_bytes());
                            }
                        }));
                    }
                }));
            }

            meta.extend(make_box(b"iprp", |iprp| {
                iprp.extend(make_box(b"ipco", |b| {
                    for p in &self.properties {
                        b.extend_from_slice(p);
                    }
                }));
                // Property indices fit in 7 bits unless there are more than 127 properties.
                let large_index = self.properties.len() > 127;
                iprp.extend(full_box(
                    b"ipma",
                    u8::from(large_ids),
                    u32::from(large_index),
                    |b| {
                        let with_props: Vec<_> = self
                            .items
                            .iter()
                            .filter(|i| !i.properties.is_empty())
                            .collect();
                        b.extend_from_slice(&(with_props.len() as u32).to_be_bytes());
                        for item in with_props {
                            id(b, item.id);
                            b.push(item.properties.len() as u8);
                            for &(index, essential) in &item.properties {
                                if large_index {
                                    let v = (u16::from(essential) << 15) | index;
                                    b.extend_from_slice(&v.to_be_bytes());
                                } else {
                                    b.push((u8::from(essential) << 7) | index as u8);
                                }
                            }
                        }
                    },
                ));
            }));
        })
    }
}

/// A box with its 32-bit size header.
fn make_box(box_type: &[u8; 4], content: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut b = vec![0; 4];
    b.extend_from_slice(box_type);
    content(&mut b);
    let size = b.len() as u32;
    b[..4].copy_from_slice(&size.to_be_bytes());
    b
}

/// A full box: version and flags, then the content.
fn full_box(
    box_type: &[u8; 4],
    version: u8,
    flags: u32,
    content: impl FnOnce(&mut Vec<u8>),
) -> Vec<u8> {
    make_box(box_type, |b| {
        b.extend_from_slice(&((u32::from(version) << 24) | (flags & 0xFF_FFFF)).to_be_bytes());
        content(b);
    })
}

/// `hvcC` (ISO/IEC 14496-15 §8.3.3). Profile compatibility and constraint flags are those of a
/// progressive, frame-only stream compatible with its own profile.
fn hvcc_box(config: &HevcConfig) -> Vec<u8> {
    make_box(b"hvcC", |b| {
        b.push(1); // configurationVersion
        b.push(config.profile_idc & 0x1F); // profile_space 0, tier 0
        let compat = 1u32 << (31 - u32::from(config.profile_idc.min(31)));
        b.extend_from_slice(&compat.to_be_bytes());
        b.extend_from_slice(&[0x90, 0, 0, 0, 0, 0]); // progressive_source, frame_only
        b.push(config.level_idc);
        b.extend_from_slice(&0xF000u16.to_be_bytes()); // min_spatial_segmentation_idc 0
        b.push(0xFC); // parallelismType 0
        b.push(0xFC | (config.chroma_format & 3));
        b.push(0xF8 | (config.bit_depth_luma.saturating_sub(8) & 7));
        b.push(0xF8 | (config.bit_depth_chroma.saturating_sub(8) & 7));
        b.extend_from_slice(&0u16.to_be_bytes()); // avgFrameRate
        // constantFrameRate 0, numTemporalLayers 1, temporalIdNested 1, lengthSizeMinusOne.
        b.push((1 << 3) | (1 << 2) | ((config.nal_length_size.max(1) - 1) & 3));
        // One array per NAL unit type, in first-appearance order.
        let mut types: Vec<u8> = Vec::new();
        for nal in &config.nal_units {
            let t = nal.first().map_or(0, |h| (h >> 1) & 0x3F);
            if !types.contains(&t) {
                types.push(t);
            }
        }
        b.push(types.len() as u8);
        for t in types {
            b.push(0x80 | t); // array_completeness
            let nals: Vec<_> = config
                .nal_units
                .iter()
                .filter(|n| n.first().map_or(0, |h| (h >> 1) & 0x3F) == t)
                .collect();
            b.extend_from_slice(&(nals.len() as u16).to_be_bytes());
            for nal in nals {
                b.extend_from_slice(&(nal.len() as u16).to_be_bytes());
                b.extend_from_slice(nal);
            }
        }
    })
}

fn colr_box(color: &ColorInfo) -> Vec<u8> {
    make_box(b"colr", |b| match color {
        ColorInfo::Nclx {
            primaries,
            transfer,
            matrix,
            full_range,
        } => {
            b.extend_from_slice(b"nclx");
            b.extend_from_slice(&primaries.to_be_bytes());
            b.extend_from_slice(&transfer.to_be_bytes());
            b.extend_from_slice(&matrix.to_be_bytes());
            b.push(u8::from(*full_range) << 7);
        }
        ColorInfo::Icc(icc) => {
            b.extend_from_slice(b"prof");
            b.extend_from_slice(icc);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HeifFile;
    use crate::boxes::Property;

    fn config(chroma_format: u8) -> HevcConfig {
        HevcConfig {
            profile_idc: 1,
            level_idc: 93,
            chroma_format,
            bit_depth_luma: 8,
            bit_depth_chroma: 8,
            nal_length_size: 4,
            nal_units: vec![
                vec![0x40, 1, 0xAA],
                vec![0x42, 1, 0xBB],
                vec![0x44, 1, 0xCC],
            ],
        }
    }

    #[test]
    fn region_items_round_trip() {
        use crate::region::{Geometry, RegionItem, UserDescription};
        let slices = [vec![0x26, 1, 1]];
        let mut w = HeifWriter::new();
        let image = w.add_image(&ImageSpec::new(640, 480, config(1), &slices));
        let mask = w.add_image(&ImageSpec {
            hidden: true,
            ..ImageSpec::new(64, 64, config(0), &slices)
        });
        let regions = RegionItem {
            reference_width: 640,
            reference_height: 480,
            regions: vec![
                Geometry::Rectangle {
                    x: 10,
                    y: 20,
                    width: 100,
                    height: 50,
                },
                Geometry::ReferencedMask {
                    x: 0,
                    y: 0,
                    width: 64,
                    height: 64,
                    mask_item: Some(mask),
                },
            ],
        };
        let description = UserDescription {
            lang: "en".into(),
            name: "Face".into(),
            description: "A person".into(),
            tags: "face".into(),
        };
        let id = w
            .add_region_item(image, &regions, &[mask], Some(&description))
            .unwrap();
        let bytes = w.finish();
        let file = HeifFile::parse(&bytes).unwrap();
        assert_eq!(file.region_items(image), [id]);
        assert_eq!(file.region_item(id).unwrap(), regions);
        assert_eq!(file.user_descriptions(id).unwrap(), [description]);
        assert!(file.region_items(mask).is_empty());
        assert!(file.region_item(image).is_err(), "not a region item");
    }

    #[test]
    fn grid_round_trips() {
        let slices = [vec![0x26, 1, 1]];
        let mut w = HeifWriter::new();
        let tiles: Vec<ItemId> = (0..6)
            .map(|_| {
                w.add_image(&ImageSpec {
                    hidden: true,
                    ..ImageSpec::new(512, 512, config(1), &slices)
                })
            })
            .collect();
        let grid = w.add_grid(2, 3, 1500, 1000, &tiles, None, None);
        let bytes = w.finish();
        let file = HeifFile::parse(&bytes).unwrap();
        assert_eq!(file.primary_id, grid);
        let g = file.grid(grid).unwrap();
        assert_eq!(
            (g.rows, g.columns, g.output_width, g.output_height),
            (2, 3, 1500, 1000)
        );
        assert_eq!(file.referenced_items(grid, b"dimg"), &tiles[..]);
        assert_eq!(file.image_size(grid).unwrap(), Some((1500, 1000)));
        // The tiles share one `hvcC` and one `ispe`.
        assert_eq!(file.properties.len(), 4);
    }

    #[test]
    fn tmap_round_trips_with_a_hidden_gain_map_grid() {
        let slices = [vec![0x26, 1, 1]];
        let mut w = HeifWriter::new();
        let base = w.add_image(&ImageSpec::new(640, 480, config(1), &slices));
        let tile_ids: Vec<ItemId> = (0..2)
            .map(|_| {
                w.add_image(&ImageSpec {
                    hidden: true,
                    ..ImageSpec::new(512, 512, config(0), &slices)
                })
            })
            .collect();
        let map = w.add_hidden_grid(1, 2, 320, 240, &tile_ids, None);
        let payload: Vec<u8> = (0..62).collect();
        let tmap = w.add_tmap(base, map, 640, 480, None, &payload);
        let bytes = w.finish();

        let file = HeifFile::parse(&bytes).unwrap();
        assert_eq!(file.primary_id, base);
        assert!(file.file_type.has_brand(b"tmap"));
        assert!(!file.item(tmap).unwrap().hidden);
        assert_eq!(file.referenced_items(tmap, b"dimg"), &[base, map]);
        assert_eq!(file.image_size(tmap).unwrap(), Some((640, 480)));
        assert_eq!(file.item_data(tmap).unwrap(), &payload[..]);
        assert!(file.item(map).unwrap().hidden);
        assert_eq!(file.image_size(map).unwrap(), Some((320, 240)));
        assert_eq!(file.referenced_items(map, b"dimg"), &tile_ids[..]);
        // The tone-mapped item and the base image are alternates, and the group ID does not
        // collide with an item ID.
        let at = bytes.windows(4).position(|w| w == b"altr").unwrap();
        let group_id = u32::from_be_bytes(bytes[at + 8..at + 12].try_into().unwrap());
        let last_item = file.items.iter().map(|item| item.id).max().unwrap();
        assert!(
            group_id > last_item,
            "group {group_id}, last item {last_item}"
        );
    }

    #[test]
    fn image_with_alpha_and_exif_round_trips() {
        let slices = [vec![0x26, 1, 1, 2, 3], vec![0x02, 1, 4]];
        let alpha_slices = [vec![0x26, 1, 9]];
        let color = ColorInfo::Nclx {
            primaries: 1,
            transfer: 13,
            matrix: 6,
            full_range: true,
        };
        let mut w = HeifWriter::new();
        let image = w.add_image(&ImageSpec {
            color: Some(color.clone()),
            crop: Some((639, 479)),
            ..ImageSpec::new(640, 480, config(1), &slices)
        });
        let alpha = w.add_alpha(image, &ImageSpec::new(640, 480, config(0), &alpha_slices));
        let exif = w.add_exif(image, b"MM\0*\0\0\0\x08");
        let xmp = w.add_xmp(image, b"<x:xmpmeta/>");
        w.add_icc_profile(image, b"icc profile");
        let bytes = w.finish();

        let file = HeifFile::parse(&bytes).unwrap();
        assert!(file.file_type.has_brand(b"heic"));
        assert_eq!(file.primary_id, image);
        assert_eq!(file.image_size(image).unwrap(), Some((640, 480)));
        assert_eq!(file.hevc_config(image).unwrap().unwrap(), &config(1));
        assert_eq!(file.hevc_config(alpha).unwrap().unwrap(), &config(0));
        assert!(
            file.item_properties(image)
                .unwrap()
                .any(|p| *p == Property::Color(color.clone()))
        );
        assert!(file.item_properties(image).unwrap().any(|p| *p
            == Property::CleanAperture {
                width: (639, 1),
                height: (479, 1),
                horiz_offset: (-1, 2),
                vert_offset: (-1, 2),
            }));
        assert!(file.item(alpha).unwrap().hidden);
        assert_eq!(file.referencing_items(image, b"auxl"), vec![alpha]);
        assert_eq!(file.exif(image).unwrap().unwrap(), b"MM\0*\0\0\0\x08");
        assert_eq!(file.referencing_items(image, b"cdsc"), vec![exif, xmp]);
        assert_eq!(file.xmp(image).unwrap().unwrap(), b"<x:xmpmeta/>");
        assert_eq!(file.icc_profile(image).unwrap().unwrap(), b"icc profile");
        // `clap` stays the last property of the image.
        assert!(matches!(
            file.item_properties(image).unwrap().last(),
            Some(Property::CleanAperture { .. })
        ));

        let stream = file.hevc_bitstream(image).unwrap();
        let mut expected = Vec::new();
        for nal in config(1).nal_units.iter().chain(&slices) {
            expected.extend_from_slice(&[0, 0, 0, 1]);
            expected.extend_from_slice(nal);
        }
        assert_eq!(stream, expected);
        // The `ispe` property is shared by the image and its alpha plane.
        let ispe = file
            .properties
            .iter()
            .filter(|p| matches!(p, Property::ImageSize { .. }))
            .count();
        assert_eq!(ispe, 1);
    }

    #[test]
    fn sequence_tracks_read_back() {
        let config = HevcConfig {
            profile_idc: 1,
            level_idc: 93,
            chroma_format: 1,
            bit_depth_luma: 8,
            bit_depth_chroma: 8,
            nal_length_size: 4,
            nal_units: vec![vec![0x40, 1, 2], vec![0x42, 1, 3], vec![0x44, 1, 4]],
        };
        let mut track = SequenceTrack::new(64, 48, 1000, config.clone());
        track.add_sample(&[vec![0x26, 1, 0xAA, 0xBB]], 40, true);
        track.add_sample(&[vec![0x26, 1, 0xCC], vec![0x26, 1, 0xDD]], 40, true);
        track.add_sample(&[vec![0x02, 1, 0xEE]], 500, false);
        let mut w = HeifWriter::new();
        w.add_image(&ImageSpec::new(64, 48, config, &[vec![0x26, 1, 0x11]]));
        w.set_sequence(track);
        let bytes = w.finish();

        let file = HeifFile::parse(&bytes).unwrap();
        assert!(file.file_type.has_brand(b"msf1") && file.file_type.has_brand(b"hevc"));
        let tracks = crate::track::parse_tracks(&bytes).unwrap();
        assert_eq!(tracks.len(), 1);
        let t = &tracks[0];
        assert_eq!(
            (&t.handler.0, t.width, t.height, t.timescale),
            (b"pict", 64, 48, 1000)
        );
        assert_eq!(&t.coding.0, b"hvc1");
        assert_eq!(t.hevc_config.as_ref().unwrap().nal_units.len(), 3);
        let durations: Vec<u32> = t.samples.iter().map(|s| s.duration).collect();
        assert_eq!(durations, [40, 40, 500]);
        let sync: Vec<bool> = t.samples.iter().map(|s| s.sync).collect();
        assert_eq!(sync, [true, true, false]);
        // Samples are length-prefixed NAL units, after the item data.
        assert_eq!(
            t.sample_data(&bytes, 1).unwrap(),
            [0, 0, 0, 3, 0x26, 1, 0xCC, 0, 0, 0, 3, 0x26, 1, 0xDD]
        );
        // 32-bit chunk offsets, which every reader supports.
        let has = |kind: &[u8]| bytes.windows(4).any(|w| w == kind);
        assert!(has(b"stco") && !has(b"co64") && has(b"ccst") && !has(b"edts") && !has(b"ctts"));
    }

    #[test]
    fn sequence_alpha_tracks_read_back() {
        let color_cfg = config(1);
        let alpha_cfg = config(0);
        let mut track = SequenceTrack::new(64, 48, 1000, color_cfg.clone());
        track.add_sample(&[vec![0x26, 1, 0xAA]], 40, true);
        track.add_sample(&[vec![0x02, 1, 0xBB]], 40, false);

        let mut alpha = SequenceTrack::new(64, 48, 1000, alpha_cfg.clone());
        alpha.add_sample(&[vec![0x26, 1, 0x11]], 40, true);
        alpha.add_sample(&[vec![0x02, 1, 0x22, 0x33]], 40, false);
        track.set_alpha(alpha);

        let mut w = HeifWriter::new();
        w.add_image(&ImageSpec::new(64, 48, color_cfg, &[vec![0x26, 1, 0x99]]));
        w.set_sequence(track);
        let bytes = w.finish();

        let tracks = crate::track::parse_tracks(&bytes).unwrap();
        assert_eq!(tracks.len(), 2);
        let (main, aux) = (&tracks[0], &tracks[1]);
        assert_eq!((main.id, &main.handler.0), (1, b"pict"));
        assert_eq!(main.auxiliary_type, None);
        assert_eq!((aux.id, &aux.handler.0), (2, b"auxv"));
        assert_eq!(aux.auxiliary_type.as_deref(), Some(ALPHA_URN));
        assert_eq!(
            aux.references,
            vec![(crate::FourCC(*b"auxl"), vec![main.id])]
        );
        assert_eq!(
            main.sample_data(&bytes, 1).unwrap(),
            [0, 0, 0, 3, 0x02, 1, 0xBB]
        );
        assert_eq!(
            aux.sample_data(&bytes, 1).unwrap(),
            [0, 0, 0, 4, 0x02, 1, 0x22, 0x33]
        );
    }

    #[test]
    fn sequence_track_writes_ctts_for_reordered_frames() {
        let cfg = config(1);
        let mut track = SequenceTrack::new(64, 48, 1000, cfg.clone());
        // Display order 0, 1, 2, 3 stored as I0, P3, B1, B2 with durations 40ms each.
        track.add_sample_with_offset(&[vec![0x26, 1, 0x00]], 40, true, 0);
        track.add_sample_with_offset(&[vec![0x02, 1, 0x03]], 40, false, 80);
        track.add_sample_with_offset(&[vec![0x00, 1, 0x01]], 40, false, -40);
        track.add_sample_with_offset(&[vec![0x00, 1, 0x02]], 40, false, -40);

        let mut w = HeifWriter::new();
        w.add_image(&ImageSpec::new(64, 48, cfg, &[vec![0x26, 1, 0x00]]));
        w.set_sequence(track);
        let bytes = w.finish();

        let tracks = crate::track::parse_tracks(&bytes).unwrap();
        assert_eq!(tracks[0].display_order(), [0, 2, 3, 1]);
        assert!(tracks[0].samples.iter().all(|s| s.composition_offset >= 0));
        assert_eq!(tracks[0].composition_times(), [40, 160, 80, 120]);
        assert!(bytes.windows(4).any(|w| w == b"edts"));
        let elst = bytes.windows(4).position(|w| w == b"elst").unwrap();
        let entry_count = u32::from_be_bytes(bytes[elst + 8..elst + 12].try_into().unwrap());
        let seg_duration = u32::from_be_bytes(bytes[elst + 12..elst + 16].try_into().unwrap());
        let media_time = i32::from_be_bytes(bytes[elst + 16..elst + 20].try_into().unwrap());
        assert_eq!((entry_count, seg_duration, media_time), (1, 160, 40));
    }
}
