//! Scan orders (H.265 §6.5): CTB raster/tile scan conversion and coefficient scans.

use crate::params::{Pps, Sps};

/// Coefficient scan types (`scanIdx`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ScanType {
    /// Up-right diagonal (0).
    Diagonal = 0,
    /// Horizontal (1).
    Horizontal = 1,
    /// Vertical (2).
    Vertical = 2,
}

/// `ScanOrder[log2BlockSize][scanIdx]`: positions `(x, y)` in scan order, for block sizes
/// 1×1 to 8×8 (`log2BlockSize` 0..=3). Used both inside 4×4 sub-blocks and for the order of
/// sub-blocks within a transform block.
pub fn scan_order(log2_size: u8, scan: ScanType) -> &'static [(u8, u8)] {
    &SCAN_ORDERS[usize::from(log2_size)][scan as usize]
}

/// `[log2_size][scan type]` → positions.
type ScanTables = [[Vec<(u8, u8)>; 3]; 4];

static SCAN_ORDERS: std::sync::LazyLock<ScanTables> = std::sync::LazyLock::new(|| {
    core::array::from_fn(|l| {
        let size = 1u8 << l;
        [diagonal(size), horizontal(size), vertical(size)]
    })
});

/// Up-right diagonal scan (§6.5.3).
fn diagonal(size: u8) -> Vec<(u8, u8)> {
    let mut out = Vec::with_capacity(usize::from(size) * usize::from(size));
    let (mut x, mut y): (i16, i16) = (0, 0);
    while out.len() < out.capacity() {
        while y >= 0 {
            if x < i16::from(size) && y < i16::from(size) {
                out.push((x as u8, y as u8));
            }
            y -= 1;
            x += 1;
        }
        y = x;
        x = 0;
    }
    out
}

/// Horizontal scan (§6.5.4): row by row.
fn horizontal(size: u8) -> Vec<(u8, u8)> {
    (0..size)
        .flat_map(|y| (0..size).map(move |x| (x, y)))
        .collect()
}

/// Vertical scan (§6.5.5): column by column.
fn vertical(size: u8) -> Vec<(u8, u8)> {
    (0..size)
        .flat_map(|x| (0..size).map(move |y| (x, y)))
        .collect()
}

/// CTB address conversions between raster scan and tile scan, and tile IDs (§6.5.1).
#[derive(Debug, Clone)]
pub struct CtbLayout {
    /// Picture width in CTBs.
    pub width: u32,
    /// Picture height in CTBs.
    pub height: u32,
    /// `CtbAddrRsToTs`.
    pub rs_to_ts: Vec<u32>,
    /// `CtbAddrTsToRs`.
    pub ts_to_rs: Vec<u32>,
    /// `TileId`, indexed by tile-scan address.
    pub tile_id: Vec<u32>,
    /// First CTB column of each tile column (`colBd`), with the picture width appended.
    pub col_bd: Vec<u32>,
    /// First CTB row of each tile row (`rowBd`), with the picture height appended.
    pub row_bd: Vec<u32>,
}

impl CtbLayout {
    /// Builds the layout for a picture.
    pub fn new(sps: &Sps, pps: &Pps) -> Self {
        let (width, height) = sps.pic_size_in_ctbs();
        let (col_widths, row_heights) = match &pps.tiles {
            None => (vec![width], vec![height]),
            Some(t) => (
                t.column_widths
                    .clone()
                    .unwrap_or_else(|| uniform(t.num_columns, width)),
                t.row_heights
                    .clone()
                    .unwrap_or_else(|| uniform(t.num_rows, height)),
            ),
        };
        let bounds = |sizes: &[u32]| {
            let mut bd = vec![0];
            for s in sizes {
                bd.push(bd.last().unwrap() + s);
            }
            bd
        };
        let col_bd = bounds(&col_widths);
        let row_bd = bounds(&row_heights);

        let n = (width * height) as usize;
        let mut rs_to_ts = vec![0; n];
        for rs in 0..width * height {
            let (x, y) = (rs % width, rs / width);
            let tile_x = col_bd
                .iter()
                .rposition(|&b| b <= x)
                .unwrap()
                .min(col_widths.len() - 1);
            let tile_y = row_bd
                .iter()
                .rposition(|&b| b <= y)
                .unwrap()
                .min(row_heights.len() - 1);
            let mut ts = 0;
            for &h in &row_heights[..tile_y] {
                ts += width * h;
            }
            for &w in &col_widths[..tile_x] {
                ts += row_heights[tile_y] * w;
            }
            ts += (y - row_bd[tile_y]) * col_widths[tile_x] + x - col_bd[tile_x];
            rs_to_ts[rs as usize] = ts;
        }
        let mut ts_to_rs = vec![0; n];
        for (rs, &ts) in rs_to_ts.iter().enumerate() {
            ts_to_rs[ts as usize] = rs as u32;
        }
        let mut tile_id = vec![0; n];
        let mut id = 0;
        for j in 0..row_heights.len() {
            for i in 0..col_widths.len() {
                for y in row_bd[j]..row_bd[j + 1] {
                    for x in col_bd[i]..col_bd[i + 1] {
                        tile_id[rs_to_ts[(y * width + x) as usize] as usize] = id;
                    }
                }
                id += 1;
            }
        }
        Self {
            width,
            height,
            rs_to_ts,
            ts_to_rs,
            tile_id,
            col_bd,
            row_bd,
        }
    }

    /// Tile ID of the CTB at raster address `rs`.
    pub fn tile_of_rs(&self, rs: u32) -> u32 {
        self.tile_id[self.rs_to_ts[rs as usize] as usize]
    }

    /// First CTB column of the tile containing column `x`.
    pub fn tile_column_start(&self, x: u32) -> u32 {
        *self.col_bd.iter().rev().find(|&&b| b <= x).unwrap()
    }
}

/// Uniform tile spacing (6-3, 6-4).
fn uniform(n: u32, total: u32) -> Vec<u32> {
    (0..n)
        .map(|i| ((i + 1) * total) / n - (i * total) / n)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagonal_4x4() {
        let d = scan_order(2, ScanType::Diagonal);
        assert_eq!(&d[..6], &[(0, 0), (0, 1), (1, 0), (0, 2), (1, 1), (2, 0)]);
        assert_eq!(d[15], (3, 3));
        assert_eq!(d.len(), 16);
    }

    #[test]
    fn horizontal_and_vertical_2x2() {
        assert_eq!(
            scan_order(1, ScanType::Horizontal),
            &[(0, 0), (1, 0), (0, 1), (1, 1)]
        );
        assert_eq!(
            scan_order(1, ScanType::Vertical),
            &[(0, 0), (0, 1), (1, 0), (1, 1)]
        );
        assert_eq!(scan_order(0, ScanType::Diagonal), &[(0, 0)]);
    }

    #[test]
    fn uniform_spacing() {
        assert_eq!(uniform(3, 10), vec![3, 3, 4]);
        assert_eq!(uniform(1, 7), vec![7]);
    }
}
