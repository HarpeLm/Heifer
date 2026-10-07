//! Prints the box tree of an ISOBMFF/HEIF file.
//!
//! Usage: `cargo run -p heifer-isobmff --example dump -- <file.heic>`

use heifer_isobmff::{BoxIter, FourCC};

/// Boxes whose content is a list of child boxes, with the number of bytes to skip before the children.
fn children_offset(box_type: &[u8; 4], content: &[u8]) -> Option<usize> {
    match box_type {
        // Plain containers.
        b"iprp" | b"ipco" | b"dinf" | b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"edts" => {
            Some(0)
        }
        // FullBox (version + flags) then children.
        b"meta" | b"iref" | b"dref" => Some(4),
        // FullBox + entry count (u16 for version 0, u32 otherwise).
        b"iinf" => Some(if content.first() == Some(&0) { 6 } else { 8 }),
        _ => None,
    }
}

fn details(box_type: &[u8; 4], c: &[u8]) -> String {
    let u16_at = |i: usize| c.get(i..i + 2).map(|b| u16::from_be_bytes([b[0], b[1]]));
    let u32_at = |i: usize| {
        c.get(i..i + 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    };
    let fourcc_at = |i: usize| {
        c.get(i..i + 4)
            .map(|b| FourCC([b[0], b[1], b[2], b[3]]).to_string())
    };
    match box_type {
        b"ftyp" => {
            let brands: Vec<_> = (8..c.len()).step_by(4).filter_map(fourcc_at).collect();
            format!(
                "major={} compatible=[{}]",
                fourcc_at(0).unwrap_or_default(),
                brands.join(", ")
            )
        }
        b"hdlr" => format!("handler={}", fourcc_at(8).unwrap_or_default()),
        b"pitm" => {
            let id = if c.first() == Some(&0) {
                u16_at(4).map(u32::from)
            } else {
                u32_at(4)
            };
            format!("primary_item={}", id.map_or("?".into(), |v| v.to_string()))
        }
        b"infe" if c.first().is_some_and(|&v| v >= 2) => {
            let (id, type_at) = if c[0] == 2 {
                (u16_at(4).map(u32::from), 8)
            } else {
                (u32_at(4), 10)
            };
            format!(
                "item_id={} type={}",
                id.map_or("?".into(), |v| v.to_string()),
                fourcc_at(type_at).unwrap_or_default()
            )
        }
        b"ispe" => format!(
            "{}x{}",
            u32_at(4).unwrap_or_default(),
            u32_at(8).unwrap_or_default()
        ),
        b"dimg" | b"thmb" | b"cdsc" | b"auxl" => {
            let from = u16_at(0).unwrap_or_default();
            let n = u16_at(2).unwrap_or_default() as usize;
            let to: Vec<_> = (0..n)
                .filter_map(|i| u16_at(4 + 2 * i))
                .map(|v| v.to_string())
                .collect();
            let to = if to.len() > 8 {
                format!("{}, ... ({} items)", to[..8].join(", "), to.len())
            } else {
                to.join(", ")
            };
            format!("{from} -> [{to}]")
        }
        _ => String::new(),
    }
}

fn dump(data: &[u8], depth: usize, base: usize) {
    for item in BoxIter::new(data) {
        let b = match item {
            Ok(b) => b,
            Err(e) => {
                println!("{:indent$}error: {e}", "", indent = depth * 2);
                return;
            }
        };
        let t = &b.header.box_type.0;
        let size = b.header.header_size as usize + b.content.len();
        println!(
            "{:indent$}{} @{} {} bytes  {}",
            "",
            b.header.box_type,
            base + b.offset,
            size,
            details(t, b.content),
            indent = depth * 2
        );
        if let Some(skip) = children_offset(t, b.content)
            && let Some(children) = b.content.get(skip..)
        {
            let children_base = base + b.offset + b.header.header_size as usize + skip;
            dump(children, depth + 1, children_base);
        }
    }
}

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: dump <file.heic>");
        std::process::exit(2);
    });
    let data = std::fs::read(&path).unwrap_or_else(|e| {
        eprintln!("cannot read {path}: {e}");
        std::process::exit(1);
    });
    dump(&data, 0, 0);
}
