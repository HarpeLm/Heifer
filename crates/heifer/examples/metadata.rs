//! Prints the metadata of a HEIC file without decoding its pixels.
//!
//! Usage: cargo run -p heifer --example metadata -- <file.heic>

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: metadata <file.heic>")?;
    let meta = heifer::read_metadata(&std::fs::read(path)?)?;
    match meta.exif_fields() {
        Some(e) => {
            println!("EXIF: {} bytes", meta.exif.as_ref().map_or(0, Vec::len));
            println!("  make:        {}", e.make().unwrap_or("-"));
            println!("  model:       {}", e.model().unwrap_or("-"));
            println!("  software:    {}", e.software().unwrap_or("-"));
            println!("  date:        {}", e.date_time().unwrap_or("-"));
            println!(
                "  orientation: {} (informative: heifer applies irot/imir)",
                e.orientation().map_or("-".into(), |o| o.to_string())
            );
            println!("  GPS:         {}", if e.has_gps() { "yes" } else { "no" });
        }
        None => println!("EXIF: none"),
    }
    match &meta.xmp {
        Some(x) => println!(
            "XMP: {} bytes, starts with {:?}",
            x.len(),
            String::from_utf8_lossy(&x[..x.len().min(40)])
        ),
        None => println!("XMP: none"),
    }
    match &meta.icc_profile {
        Some(icc) => {
            let desc = icc
                .get(36..40)
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .unwrap_or_default();
            println!("ICC profile: {} bytes (signature {desc:?})", icc.len());
        }
        None => println!("ICC profile: none"),
    }
    Ok(())
}
