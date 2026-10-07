//! Prints the items of a HEIF file, and optionally extracts the HEVC bitstream of an item.
//!
//! Usage:
//!   cargo run -p heifer-isobmff --example info -- <file.heic>
//!   cargo run -p heifer-isobmff --example info -- <file.heic> --extract <item_id> <out.h265>

use heifer_isobmff::HeifFile;
use heifer_isobmff::boxes::Property;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("usage: info <file.heic> [--extract <item_id> <out.h265>]");
        std::process::exit(2);
    };
    let data = std::fs::read(path)?;
    let file = HeifFile::parse(&data)?;

    println!(
        "brands: {} [{}]",
        file.file_type.major_brand,
        file.file_type
            .compatible_brands
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("primary item: {}", file.primary_id);

    for item in &file.items {
        let mut line = format!(
            "item {:>5}  {}{}",
            item.id,
            item.item_type,
            if item.hidden { " (hidden)" } else { "" }
        );
        if let Some((w, h)) = file.image_size(item.id)? {
            line += &format!("  {w}x{h}");
        }
        if let Some(c) = file.hevc_config(item.id)? {
            line += &format!(
                "  hevc profile={} level={} chroma={} bits={}",
                c.profile_idc, c.level_idc, c.chroma_format, c.bit_depth_luma
            );
        }
        for p in file.item_properties(item.id)? {
            match p {
                Property::Rotation(r) => line += &format!("  rotation={}°", u16::from(*r) * 90),
                Property::Mirror(h) => line += &format!("  mirror={}", if *h { "h" } else { "v" }),
                Property::AuxiliaryType(t) => line += &format!("  aux={t}"),
                Property::Color(c) => {
                    line += &format!("  colr={c:?}").chars().take(60).collect::<String>()
                }
                _ => {}
            }
        }
        if item.item_type.0 == *b"grid" {
            let g = file.grid(item.id)?;
            line += &format!(
                "  grid {}x{} tiles -> {}x{}  tiles={:?}",
                g.columns,
                g.rows,
                g.output_width,
                g.output_height,
                file.referenced_items(item.id, b"dimg")
            );
        }
        for (ref_type, label) in [
            (b"thmb", "thumbnail of"),
            (b"cdsc", "describes"),
            (b"auxl", "auxiliary of"),
        ] {
            let to = file.referenced_items(item.id, ref_type);
            if !to.is_empty() {
                line += &format!("  {label} {to:?}");
            }
        }
        println!("{line}");
    }

    if let [_, flag, id, out] = args.as_slice()
        && flag == "--extract"
    {
        let bitstream = file.hevc_bitstream(id.parse()?)?;
        std::fs::write(out, &bitstream)?;
        println!("wrote {} bytes of HEVC Annex B to {out}", bitstream.len());
    }
    Ok(())
}
