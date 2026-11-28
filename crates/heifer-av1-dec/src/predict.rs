//! Intra prediction (AV1 §7.11.2): predicts a transform block from the reconstructed samples
//! above and to its left.

use crate::spec_tables::{
    DR_INTRA_DERIVATIVE, INTRA_EDGE_KERNEL, INTRA_FILTER_TAPS, MODE_TO_ANGLE, SM_WEIGHTS_TX_4X4,
    SM_WEIGHTS_TX_8X8, SM_WEIGHTS_TX_16X16, SM_WEIGHTS_TX_32X32, SM_WEIGHTS_TX_64X64,
};

pub(crate) const DC_PRED: u8 = 0;
pub(crate) const V_PRED: u8 = 1;
pub(crate) const D67_PRED: u8 = 8;
pub(crate) const SMOOTH_PRED: u8 = 9;
pub(crate) const SMOOTH_V_PRED: u8 = 10;
pub(crate) const SMOOTH_H_PRED: u8 = 11;

/// `is_directional_mode()`.
pub(crate) fn is_directional(mode: u8) -> bool {
    (V_PRED..=D67_PRED).contains(&mode)
}

/// One plane of the frame being decoded.
#[derive(Clone)]
pub(crate) struct Plane {
    pub(crate) data: Vec<u16>,
    pub(crate) stride: usize,
}

impl Plane {
    pub(crate) fn at(&self, x: usize, y: usize) -> i32 {
        i32::from(self.data[y * self.stride + x])
    }

    pub(crate) fn set(&mut self, x: usize, y: usize, v: i32) {
        self.data[y * self.stride + x] = v as u16;
    }
}

/// Everything about a transform block that intra prediction needs.
pub(crate) struct IntraBlock {
    /// Top left sample of the block.
    pub(crate) x: usize,
    pub(crate) y: usize,
    pub(crate) log2w: u32,
    pub(crate) log2h: u32,
    pub(crate) have_left: bool,
    pub(crate) have_above: bool,
    pub(crate) have_above_right: bool,
    pub(crate) have_below_left: bool,
    pub(crate) mode: u8,
    /// `AngleDeltaY` or `AngleDeltaUV`.
    pub(crate) angle_delta: i32,
    /// `filter_intra_mode` when the recursive filter applies (luma with `use_filter_intra`).
    pub(crate) filter_intra_mode: Option<u8>,
    /// `enable_intra_edge_filter`.
    pub(crate) edge_filter: bool,
    /// `get_filter_type()`: a neighbouring block uses a smooth mode.
    pub(crate) smooth_neighbour: bool,
    /// Largest valid coordinates of the plane (`maxX`, `maxY`).
    pub(crate) max_x: usize,
    pub(crate) max_y: usize,
    pub(crate) bit_depth: u32,
}

/// Room before index 0 of the edge arrays: upsampling writes down to index -2.
const OFF: usize = 16;
const EDGE: usize = OFF + 2 * 128 + 16;

fn round2(x: i32, n: u32) -> i32 {
    if n == 0 { x } else { (x + (1 << (n - 1))) >> n }
}

fn sm_weights(log2: u32) -> &'static [u8] {
    match log2 {
        2 => &SM_WEIGHTS_TX_4X4,
        3 => &SM_WEIGHTS_TX_8X8,
        4 => &SM_WEIGHTS_TX_16X16,
        5 => &SM_WEIGHTS_TX_32X32,
        _ => &SM_WEIGHTS_TX_64X64,
    }
}

/// The intra prediction process: writes the predicted block into `plane`.
pub(crate) fn predict_intra(plane: &mut Plane, b: &IntraBlock) {
    let (w, h) = (1usize << b.log2w, 1usize << b.log2h);
    let (x, y) = (b.x, b.y);
    let base_value = 1i32 << (b.bit_depth - 1);
    let mut above = [0i32; EDGE];
    let mut left = [0i32; EDGE];
    for i in 0..w + h {
        above[OFF + i] = match (b.have_above, b.have_left) {
            (false, true) => plane.at(x - 1, y),
            (false, false) => base_value - 1,
            _ => {
                let limit = b
                    .max_x
                    .min(x + if b.have_above_right { 2 * w } else { w } - 1);
                plane.at(limit.min(x + i), y - 1)
            }
        };
        left[OFF + i] = match (b.have_left, b.have_above) {
            (false, true) => plane.at(x, y - 1),
            (false, false) => base_value + 1,
            _ => {
                let limit = b
                    .max_y
                    .min(y + if b.have_below_left { 2 * h } else { h } - 1);
                plane.at(x - 1, limit.min(y + i))
            }
        };
    }
    let corner = match (b.have_above, b.have_left) {
        (true, true) => plane.at(x - 1, y - 1),
        (true, false) => plane.at(x, y - 1),
        (false, true) => plane.at(x - 1, y),
        (false, false) => base_value,
    };
    above[OFF - 1] = corner;
    left[OFF - 1] = corner;

    let max = (1i32 << b.bit_depth) - 1;
    let mut pred = vec![0i32; w * h];
    if let Some(mode) = b.filter_intra_mode {
        recursive(&mut pred, &above, &left, w, h, usize::from(mode), max);
    } else if is_directional(b.mode) {
        directional(&mut pred, &mut above, &mut left, b, w, h, max);
    } else {
        match b.mode {
            SMOOTH_PRED => {
                let (wx, wy) = (sm_weights(b.log2w), sm_weights(b.log2h));
                for i in 0..h {
                    for j in 0..w {
                        let (wy, wx) = (i32::from(wy[i]), i32::from(wx[j]));
                        let s = wy * above[OFF + j]
                            + (256 - wy) * left[OFF + h - 1]
                            + wx * left[OFF + i]
                            + (256 - wx) * above[OFF + w - 1];
                        pred[i * w + j] = round2(s, 9);
                    }
                }
            }
            SMOOTH_V_PRED => {
                let wy = sm_weights(b.log2h);
                for i in 0..h {
                    for j in 0..w {
                        let wy = i32::from(wy[i]);
                        let s = wy * above[OFF + j] + (256 - wy) * left[OFF + h - 1];
                        pred[i * w + j] = round2(s, 8);
                    }
                }
            }
            SMOOTH_H_PRED => {
                let wx = sm_weights(b.log2w);
                for i in 0..h {
                    for j in 0..w {
                        let wx = i32::from(wx[j]);
                        let s = wx * left[OFF + i] + (256 - wx) * above[OFF + w - 1];
                        pred[i * w + j] = round2(s, 8);
                    }
                }
            }
            DC_PRED => {
                let left_sum: i32 = left[OFF..OFF + h].iter().sum();
                let above_sum: i32 = above[OFF..OFF + w].iter().sum();
                let avg = match (b.have_left, b.have_above) {
                    (true, true) => (left_sum + above_sum + ((w + h) as i32 >> 1)) / (w + h) as i32,
                    (true, false) => ((left_sum + (h as i32 >> 1)) >> b.log2h).clamp(0, max),
                    (false, true) => ((above_sum + (w as i32 >> 1)) >> b.log2w).clamp(0, max),
                    (false, false) => base_value,
                };
                pred.fill(avg);
            }
            _ => {
                // PAETH_PRED: the basic intra prediction process.
                let corner = above[OFF - 1];
                for i in 0..h {
                    for j in 0..w {
                        let (a, l) = (above[OFF + j], left[OFF + i]);
                        let base = a + l - corner;
                        let (p_left, p_top, p_corner) =
                            ((base - l).abs(), (base - a).abs(), (base - corner).abs());
                        pred[i * w + j] = if p_left <= p_top && p_left <= p_corner {
                            l
                        } else if p_top <= p_corner {
                            a
                        } else {
                            corner
                        };
                    }
                }
            }
        }
    }
    for i in 0..h {
        for j in 0..w {
            plane.set(x + j, y + i, pred[i * w + j]);
        }
    }
}

/// The recursive intra prediction process (filter intra).
fn recursive(
    pred: &mut [i32],
    above: &[i32],
    left: &[i32],
    w: usize,
    h: usize,
    mode: usize,
    max: i32,
) {
    for i2 in 0..h >> 1 {
        for j4 in 0..w >> 2 {
            let mut p = [0i32; 7];
            for (i, p) in p.iter_mut().enumerate() {
                *p = if i < 5 {
                    if i2 == 0 {
                        above[OFF + (j4 << 2) + i - 1]
                    } else if j4 == 0 && i == 0 {
                        left[OFF + (i2 << 1) - 1]
                    } else {
                        pred[((i2 << 1) - 1) * w + (j4 << 2) + i - 1]
                    }
                } else if j4 == 0 {
                    left[OFF + (i2 << 1) + i - 5]
                } else {
                    pred[((i2 << 1) + i - 5) * w + (j4 << 2) - 1]
                };
            }
            for i1 in 0..2 {
                for j1 in 0..4 {
                    let taps = &INTRA_FILTER_TAPS[mode][(i1 << 2) + j1];
                    let pr: i32 = taps.iter().zip(p).map(|(&t, p)| i32::from(t) * p).sum();
                    // Round2Signed(pr, INTRA_FILTER_SCALE_BITS).
                    let v = if pr >= 0 {
                        round2(pr, 4)
                    } else {
                        -round2(-pr, 4)
                    };
                    pred[((i2 << 1) + i1) * w + (j4 << 2) + j1] = v.clamp(0, max);
                }
            }
        }
    }
}

/// The intra edge filter strength selection process.
fn edge_strength(w: usize, h: usize, filter_type: bool, delta: i32) -> usize {
    let d = delta.abs();
    let wh = w + h;
    if !filter_type {
        match wh {
            ..=8 => usize::from(d >= 56),
            9..=16 => usize::from(d >= 40),
            17..=24 => usize::from(d >= 8) + usize::from(d >= 16) + usize::from(d >= 32),
            25..=32 => 1 + usize::from(d >= 4) + usize::from(d >= 32),
            _ => 3,
        }
    } else {
        match wh {
            ..=8 => usize::from(d >= 40) + usize::from(d >= 64),
            9..=16 => usize::from(d >= 20) + usize::from(d >= 48),
            17..=24 => 3 * usize::from(d >= 4),
            _ => 3,
        }
    }
}

/// The intra edge upsample selection process.
fn use_upsample(w: usize, h: usize, filter_type: bool, delta: i32) -> bool {
    let d = delta.abs();
    if d <= 0 || d >= 40 {
        false
    } else if !filter_type {
        w + h <= 16
    } else {
        w + h <= 8
    }
}

/// The intra edge filter process on `buf` (indices -1 and up), `sz` samples.
fn edge_filter(buf: &mut [i32], sz: usize, strength: usize) {
    if strength == 0 {
        return;
    }
    let edge: Vec<i32> = (0..sz).map(|i| buf[OFF + i - 1]).collect();
    for i in 1..sz {
        let mut s = 0;
        for (j, &k) in INTRA_EDGE_KERNEL[strength - 1].iter().enumerate() {
            let idx = (i as isize - 2 + j as isize).clamp(0, sz as isize - 1) as usize;
            s += i32::from(k) * edge[idx];
        }
        buf[OFF + i - 1] = (s + 8) >> 4;
    }
}

/// The intra edge upsample process on `buf`, `num_px` samples.
fn upsample(buf: &mut [i32], num_px: usize, max: i32) {
    let mut dup = vec![0i32; num_px + 3];
    dup[0] = buf[OFF - 1];
    for i in -1..num_px as isize {
        dup[(i + 2) as usize] = buf[(OFF as isize + i) as usize];
    }
    dup[num_px + 2] = buf[OFF + num_px - 1];
    buf[OFF - 2] = dup[0];
    for i in 0..num_px {
        let s = -dup[i] + 9 * dup[i + 1] + 9 * dup[i + 2] - dup[i + 3];
        buf[OFF + 2 * i - 1] = round2(s, 4).clamp(0, max);
        buf[OFF + 2 * i] = dup[i + 2];
    }
}

/// The directional intra prediction process.
fn directional(
    pred: &mut [i32],
    above: &mut [i32],
    left: &mut [i32],
    b: &IntraBlock,
    w: usize,
    h: usize,
    max: i32,
) {
    let p_angle = i32::from(MODE_TO_ANGLE[usize::from(b.mode)]) + b.angle_delta * 3;
    let (mut up_above, mut up_left) = (0u32, 0u32);
    if b.edge_filter {
        if p_angle != 90 && p_angle != 180 {
            if p_angle > 90 && p_angle < 180 && w + h >= 24 {
                // The filter corner process.
                let s = left[OFF] * 5 + above[OFF - 1] * 6 + above[OFF] * 5;
                above[OFF - 1] = round2(s, 4);
                left[OFF - 1] = above[OFF - 1];
            }
            let filter_type = b.smooth_neighbour;
            if b.have_above {
                let strength = edge_strength(w, h, filter_type, p_angle - 90);
                let num_px = w.min(b.max_x - b.x + 1) + if p_angle < 90 { h } else { 0 } + 1;
                edge_filter(above, num_px, strength);
            }
            if b.have_left {
                let strength = edge_strength(w, h, filter_type, p_angle - 180);
                let num_px = h.min(b.max_y - b.y + 1) + if p_angle > 180 { w } else { 0 } + 1;
                edge_filter(left, num_px, strength);
            }
        }
        let filter_type = b.smooth_neighbour;
        if use_upsample(w, h, filter_type, p_angle - 90) {
            up_above = 1;
            upsample(above, w + if p_angle < 90 { h } else { 0 }, max);
        }
        if use_upsample(w, h, filter_type, p_angle - 180) {
            up_left = 1;
            upsample(left, h + if p_angle > 180 { w } else { 0 }, max);
        }
    }
    let at = |buf: &[i32], i: isize| buf[(OFF as isize + i) as usize];
    let interp = |buf: &[i32], base: isize, shift: i32| {
        round2(at(buf, base) * (32 - shift) + at(buf, base + 1) * shift, 5)
    };
    if p_angle < 90 {
        let dx = i32::from(DR_INTRA_DERIVATIVE[p_angle as usize]);
        let max_base_x = ((w + h - 1) << up_above) as isize;
        for i in 0..h {
            let idx = (i as i32 + 1) * dx;
            let shift = ((idx << up_above) >> 1) & 0x1F;
            for j in 0..w {
                let base = (idx >> (6 - up_above)) as isize + ((j as isize) << up_above);
                pred[i * w + j] = if base < max_base_x {
                    interp(above, base, shift)
                } else {
                    at(above, max_base_x)
                };
            }
        }
    } else if p_angle > 90 && p_angle < 180 {
        let dx = i32::from(DR_INTRA_DERIVATIVE[(180 - p_angle) as usize]);
        let dy = i32::from(DR_INTRA_DERIVATIVE[(p_angle - 90) as usize]);
        for i in 0..h {
            for j in 0..w {
                let idx = ((j as i32) << 6) - (i as i32 + 1) * dx;
                let base = idx >> (6 - up_above);
                pred[i * w + j] = if base >= -(1 << up_above) {
                    interp(above, base as isize, ((idx << up_above) >> 1) & 0x1F)
                } else {
                    let idx = ((i as i32) << 6) - (j as i32 + 1) * dy;
                    let base = idx >> (6 - up_left);
                    interp(left, base as isize, ((idx << up_left) >> 1) & 0x1F)
                };
            }
        }
    } else if p_angle > 180 {
        let dy = i32::from(DR_INTRA_DERIVATIVE[(270 - p_angle) as usize]);
        for j in 0..w {
            let idx = (j as i32 + 1) * dy;
            let shift = ((idx << up_left) >> 1) & 0x1F;
            for i in 0..h {
                let base = (idx >> (6 - up_left)) as isize + ((i as isize) << up_left);
                pred[i * w + j] = interp(left, base, shift);
            }
        }
    } else if p_angle == 90 {
        for i in 0..h {
            pred[i * w..(i + 1) * w].copy_from_slice(&above[OFF..OFF + w]);
        }
    } else {
        for i in 0..h {
            pred[i * w..(i + 1) * w].fill(left[OFF + i]);
        }
    }
}
