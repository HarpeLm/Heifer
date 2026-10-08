//! Decoded picture hash SEI message (H.265 §D.2.19 / §D.3.19): an MD5, CRC or checksum of each
//! decoded picture, written by the encoder so that decoders can check bit-exactness.

use crate::Error;
use crate::recon::Frame;

/// A decoded picture hash, one value per colour component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PictureHash {
    /// MD5 of each component.
    Md5(Vec<[u8; 16]>),
    /// CRC-16 (polynomial 0x1021) of each component.
    Crc(Vec<u16>),
    /// 32-bit checksum of each component.
    Checksum(Vec<u32>),
}

impl PictureHash {
    /// Name of the hash type.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Md5(_) => "MD5",
            Self::Crc(_) => "CRC",
            Self::Checksum(_) => "checksum",
        }
    }

    /// Computes the same kind of hash over `frame` (uncropped decoded picture).
    pub fn compute_like(&self, frame: &Frame) -> Self {
        let planes = (0..self.len()).map(|c| plane(frame, c));
        match self {
            Self::Md5(_) => Self::Md5(
                planes
                    .map(|(s, w, h, bd)| md5(&plane_bytes(s, w, h, bd)))
                    .collect(),
            ),
            Self::Crc(_) => Self::Crc(planes.map(|(s, w, h, bd)| crc16(s, w, h, bd)).collect()),
            Self::Checksum(_) => {
                Self::Checksum(planes.map(|(s, w, h, bd)| checksum(s, w, h, bd)).collect())
            }
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Md5(v) => v.len(),
            Self::Crc(v) => v.len(),
            Self::Checksum(v) => v.len(),
        }
    }
}

fn plane(frame: &Frame, c: usize) -> (&[u16], usize, usize, u8) {
    (
        &frame.planes[c],
        frame.widths[c] as usize,
        frame.heights[c] as usize,
        frame.bit_depth[usize::from(c > 0)],
    )
}

/// Samples as bytes: one per sample up to 8 bits, two (little-endian) above.
fn plane_bytes(samples: &[u16], w: usize, h: usize, bit_depth: u8) -> Vec<u8> {
    let s = &samples[..w * h];
    if bit_depth > 8 {
        s.iter().flat_map(|v| v.to_le_bytes()).collect()
    } else {
        s.iter().map(|&v| v as u8).collect()
    }
}

fn crc16(samples: &[u16], w: usize, h: usize, bit_depth: u8) -> u16 {
    let mut crc: u32 = 0xFFFF;
    let mut push_bit = |bit: u32| {
        let msb = (crc >> 15) & 1;
        crc = (((crc << 1) + bit) & 0xFFFF) ^ (msb * 0x1021);
    };
    for &v in &samples[..w * h] {
        let v = u32::from(v);
        for i in 0..8 {
            push_bit((v >> (7 - i)) & 1);
        }
        if bit_depth > 8 {
            for i in 0..8 {
                push_bit((v >> (15 - i)) & 1);
            }
        }
    }
    for _ in 0..16 {
        push_bit(0);
    }
    crc as u16
}

fn checksum(samples: &[u16], w: usize, h: usize, bit_depth: u8) -> u32 {
    let mut sum: u32 = 0;
    for y in 0..h {
        for x in 0..w {
            let mask = ((x & 0xFF) ^ (y & 0xFF) ^ (x >> 8) ^ (y >> 8)) as u32;
            let v = u32::from(samples[y * w + x]);
            sum = sum.wrapping_add((v & 0xFF) ^ mask);
            if bit_depth > 8 {
                sum = sum.wrapping_add((v >> 8) ^ mask);
            }
        }
    }
    sum
}

/// Parses an SEI RBSP and returns the decoded picture hash it contains, if any.
pub fn parse_picture_hash(
    rbsp: &[u8],
    num_components: usize,
) -> Result<Option<PictureHash>, Error> {
    let mut i = 0;
    let read_var = |i: &mut usize| -> Result<usize, Error> {
        let mut v = 0;
        loop {
            let b = *rbsp.get(*i).ok_or(Error::UnexpectedEof)?;
            *i += 1;
            v += usize::from(b);
            if b != 0xFF {
                return Ok(v);
            }
        }
    };
    // Messages until the RBSP trailing bits (a 0x80 byte).
    while i < rbsp.len() && rbsp[i] != 0x80 {
        let payload_type = read_var(&mut i)?;
        let size = read_var(&mut i)?;
        let payload = rbsp.get(i..i + size).ok_or(Error::UnexpectedEof)?;
        i += size;
        if payload_type != 132 {
            continue;
        }
        let (&hash_type, rest) = payload.split_first().ok_or(Error::UnexpectedEof)?;
        let need = |n: usize| -> Result<(), Error> {
            if rest.len() < n * num_components {
                Err(Error::UnexpectedEof)
            } else {
                Ok(())
            }
        };
        return Ok(Some(match hash_type {
            0 => {
                need(16)?;
                PictureHash::Md5(
                    rest.chunks_exact(16)
                        .take(num_components)
                        .map(|c| c.try_into().expect("16 bytes"))
                        .collect(),
                )
            }
            1 => {
                need(2)?;
                PictureHash::Crc(
                    rest.chunks_exact(2)
                        .take(num_components)
                        .map(|c| u16::from_be_bytes([c[0], c[1]]))
                        .collect(),
                )
            }
            2 => {
                need(4)?;
                PictureHash::Checksum(
                    rest.chunks_exact(4)
                        .take(num_components)
                        .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
                        .collect(),
                )
            }
            _ => return Err(Error::Invalid("unknown decoded picture hash type")),
        }));
    }
    Ok(None)
}

/// MD5 (RFC 1321).
pub fn md5(data: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: [u32; 64] =
        core::array::from_fn(|i| ((i as f64 + 1.0).sin().abs() * 4_294_967_296.0) as u32);
    let mut state: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64).wrapping_mul(8)).to_le_bytes());
    for chunk in msg.chunks_exact(64) {
        let m: [u32; 16] = core::array::from_fn(|i| {
            u32::from_le_bytes(chunk[i * 4..i * 4 + 4].try_into().expect("4 bytes"))
        });
        let [mut a, mut b, mut c, mut d] = state;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        state = [
            state[0].wrapping_add(a),
            state[1].wrapping_add(b),
            state[2].wrapping_add(c),
            state[3].wrapping_add(d),
        ];
    }
    let mut out = [0u8; 16];
    for (i, s) in state.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&s.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(d: [u8; 16]) -> String {
        d.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn md5_reference_vectors() {
        assert_eq!(hex(md5(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(md5(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            hex(md5(
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"
            )),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
    }

    #[test]
    fn parses_md5_hash_message() {
        let mut rbsp = vec![132, 1 + 3 * 16, 0];
        rbsp.extend((0..48).map(|i| i as u8));
        rbsp.push(0x80);
        let hash = parse_picture_hash(&rbsp, 3).unwrap().unwrap();
        let PictureHash::Md5(v) = hash else { panic!() };
        assert_eq!(v.len(), 3);
        assert_eq!(v[1][0], 16);
    }

    #[test]
    fn skips_other_messages() {
        let rbsp = [5, 2, 0xAA, 0xBB, 0x80];
        assert_eq!(parse_picture_hash(&rbsp, 3).unwrap(), None);
    }
}
