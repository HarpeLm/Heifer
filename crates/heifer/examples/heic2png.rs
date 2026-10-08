//! Converts a HEIC file to PNG.
//!
//! Usage: cargo run --release -p heifer --example heic2png -- <in.heic> <out.png>
//!
//! The PNG writer is minimal (uncompressed deflate) to avoid dependencies: files are large.

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// Encodes 8-bit RGB or RGBA pixels as PNG.
fn encode_png(width: u32, height: u32, channels: usize, pixels: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity(pixels.len() + height as usize);
    for row in pixels.chunks(width as usize * channels) {
        raw.push(0); // filter: none
        raw.extend_from_slice(row);
    }
    // zlib stream made of stored (uncompressed) deflate blocks.
    let mut z = vec![0x78, 0x01];
    let mut blocks = raw.chunks(65535).peekable();
    while let Some(block) = blocks.next() {
        z.push(u8::from(blocks.peek().is_none()));
        let len = block.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(block);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, if channels == 4 { 6 } else { 2 }, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [input, output] = args.as_slice() else {
        return Err("usage: heic2png <in.heic> <out.png>".into());
    };
    let start = std::time::Instant::now();
    let image = heifer::decode(&std::fs::read(input)?)?;
    let elapsed = start.elapsed();
    let (channels, pixels) = if image.has_alpha {
        (4, image.to_rgba8())
    } else {
        (3, image.to_rgb8())
    };
    std::fs::write(
        output,
        encode_png(image.width, image.height, channels, &pixels),
    )?;
    println!(
        "{}x{} {}-bit{} decoded in {:.0?}",
        image.width,
        image.height,
        image.bit_depth,
        if image.has_alpha { " with alpha" } else { "" },
        elapsed
    );
    Ok(())
}
