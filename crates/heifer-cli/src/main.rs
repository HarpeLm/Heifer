//! `heifer`: converts HEIC/HEIF photos to and from PNG, JPEG, WebP and other formats.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use heifer::image_crate::HeifEncoder;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::codecs::webp::WebPEncoder;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageEncoder, ImageFormat, ImageReader};

const USAGE: &str = "\
heifer: convert HEIC/HEIF photos, in pure Rust

Usage:
  heifer <input> <output> [-q <quality>]        convert one file
  heifer <inputs>... --to <format> [-o <dir>] [-q <quality>]
                                                 convert many files
  heifer info <files>...                         show size and metadata

The formats come from the file extensions: heic (or heif), png, jpg, webp, tiff, bmp, gif.
EXIF and ICC profiles are kept, and photos are written upright.

Options:
  -q, --quality <0-100>  quality of HEIC and JPEG output (default: 80 for HEIC, 90 for JPEG)
  -t, --to <format>      output format when converting several files
  -o, --out-dir <dir>    output folder (default: next to each input)
  -f, --force            overwrite existing files
  -h, --help             show this help
  -V, --version          show the version

Examples:
  heifer IMG_1234.heic photo.jpg
  heifer *.HEIC --to jpg -o converted
  heifer screenshot.png screenshot.heic -q 70
  heifer info IMG_1234.heic";

/// What the command line asks for.
#[derive(Debug, Default, PartialEq)]
struct Args {
    info: bool,
    inputs: Vec<PathBuf>,
    output: Option<PathBuf>,
    to: Option<String>,
    out_dir: Option<PathBuf>,
    quality: Option<u8>,
    force: bool,
}

fn parse_args(raw: impl IntoIterator<Item = String>) -> Result<Option<Args>, String> {
    let mut args = Args::default();
    let mut positional = Vec::new();
    let mut raw = raw.into_iter();
    while let Some(arg) = raw.next() {
        let mut value = |name: &str| raw.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "-V" | "--version" => {
                println!("heifer {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-q" | "--quality" => {
                let q = value(&arg)?;
                args.quality = Some(
                    q.parse::<u8>()
                        .ok()
                        .filter(|&q| q <= 100)
                        .ok_or(format!("quality must be 0 to 100, not `{q}`"))?,
                );
            }
            "-t" | "--to" => args.to = Some(value(&arg)?.trim_start_matches('.').to_lowercase()),
            "-o" | "--out-dir" => args.out_dir = Some(PathBuf::from(value(&arg)?)),
            "-f" | "--force" => args.force = true,
            s if s.starts_with('-') && s.len() > 1 => return Err(format!("unknown option `{s}`")),
            _ => positional.push(PathBuf::from(arg)),
        }
    }
    if positional.first().is_some_and(|p| p.as_os_str() == "info") {
        positional.remove(0);
        args.info = true;
    } else if args.to.is_none() && args.out_dir.is_none() && positional.len() == 2 {
        args.output = positional.pop();
    } else if args.to.is_none() && !positional.is_empty() {
        return Err("give an output file, or `--to <format>` for several files".into());
    }
    if positional.is_empty() {
        return Ok(None);
    }
    args.inputs = positional;
    Ok(Some(args))
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(Some(args)) => args,
        Ok(None) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}\n\nRun `heifer --help` for usage.");
            return ExitCode::from(2);
        }
    };
    heifer::image_crate::register_image_decoder_hooks();

    let mut failed = 0;
    for input in &args.inputs {
        let result = if args.info {
            info(input)
        } else {
            output_path(input, &args).and_then(|out| convert(input, &out, &args))
        };
        if let Err(e) = result {
            eprintln!("{}: {e}", input.display());
            failed += 1;
        }
    }
    if failed > 0 {
        if args.inputs.len() > 1 {
            eprintln!("{failed} of {} files failed", args.inputs.len());
        }
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn output_path(input: &Path, args: &Args) -> Result<PathBuf, String> {
    if let Some(out) = &args.output {
        return Ok(out.clone());
    }
    let to = args.to.as_deref().ok_or("no output format")?;
    let stem = input.file_stem().ok_or("not a file name")?;
    let dir = match &args.out_dir {
        Some(dir) => {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            dir.clone()
        }
        None => input.parent().map(Path::to_path_buf).unwrap_or_default(),
    };
    Ok(dir.join(stem).with_extension(to))
}

fn is_heif(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_lowercase().as_str(), "heic" | "heif" | "hif"))
}

/// A decoded image, upright, with its ICC profile and EXIF data.
type Decoded = (DynamicImage, Option<Vec<u8>>, Option<Vec<u8>>);

/// Decodes any supported file.
fn read(input: &Path) -> Result<Decoded, String> {
    let reader = ImageReader::open(input)
        .and_then(ImageReader::with_guessed_format)
        .map_err(|e| e.to_string())?;
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let icc = decoder.icc_profile().ok().flatten();
    let mut exif = decoder.exif_metadata().ok().flatten();
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    image.apply_orientation(orientation);
    // The pixels are upright now: no viewer must rotate them again.
    if let Some(exif) = &mut exif {
        let _ = Orientation::remove_from_exif_chunk(exif);
    }
    Ok((image, icc, exif))
}

fn convert(input: &Path, output: &Path, args: &Args) -> Result<(), String> {
    if output.exists() && !args.force {
        return Err(format!(
            "{} exists (use --force to overwrite)",
            output.display()
        ));
    }
    let start = Instant::now();
    let (image, icc, exif) = read(input)?;
    let file = std::fs::File::create(output).map_err(|e| format!("{}: {e}", output.display()))?;
    let mut writer = std::io::BufWriter::new(file);

    // Encoders that keep the metadata, for the formats that store it; others through `image`.
    let format = if is_heif(output) {
        None
    } else {
        Some(
            ImageFormat::from_path(output)
                .map_err(|_| format!("unknown output format for {}", output.display()))?,
        )
    };
    let result = match format {
        None => {
            let options = heifer::EncodeOptions {
                quality: args.quality.unwrap_or(80),
                ..Default::default()
            };
            write(
                HeifEncoder::with_options(&mut writer, options),
                &image,
                icc,
                exif,
            )
        }
        Some(ImageFormat::Jpeg) => {
            // JPEG has no alpha channel.
            let rgb = DynamicImage::ImageRgb8(image.to_rgb8());
            let encoder = JpegEncoder::new_with_quality(&mut writer, args.quality.unwrap_or(90));
            write(encoder, &rgb, icc, exif)
        }
        Some(ImageFormat::Png) => write(PngEncoder::new(&mut writer), &image, icc, exif),
        Some(ImageFormat::WebP) => {
            let image = if image.color().has_alpha() {
                DynamicImage::ImageRgba8(image.to_rgba8())
            } else {
                DynamicImage::ImageRgb8(image.to_rgb8())
            };
            write(WebPEncoder::new_lossless(&mut writer), &image, icc, exif)
        }
        Some(format) => image
            .write_to(&mut writer, format)
            .map_err(|e| e.to_string()),
    };
    let result =
        result.and_then(|()| std::io::Write::flush(&mut writer).map_err(|e| e.to_string()));
    if let Err(e) = result {
        drop(writer);
        let _ = std::fs::remove_file(output); // no truncated file left behind
        return Err(e);
    }
    let size = std::fs::metadata(output).map(|m| m.len()).unwrap_or(0);
    println!(
        "{} → {} ({}×{}, {}, {:.2?})",
        input.display(),
        output.display(),
        image.width(),
        image.height(),
        human(size),
        start.elapsed()
    );
    Ok(())
}

fn write(
    mut encoder: impl ImageEncoder,
    image: &DynamicImage,
    icc: Option<Vec<u8>>,
    exif: Option<Vec<u8>>,
) -> Result<(), String> {
    // Formats without metadata support simply refuse it.
    if let Some(icc) = icc {
        let _ = encoder.set_icc_profile(icc);
    }
    if let Some(exif) = exif {
        let _ = encoder.set_exif_metadata(exif);
    }
    image.write_with_encoder(encoder).map_err(|e| e.to_string())
}

fn info(input: &Path) -> Result<(), String> {
    let bytes = std::fs::read(input).map_err(|e| e.to_string())?;
    println!("{}", input.display());
    if !bytes.get(4..8).is_some_and(|b| b == b"ftyp") {
        let (image, _, _) = read(input)?;
        println!(
            "  {}×{}, {:?}",
            image.width(),
            image.height(),
            image.color()
        );
        return Ok(());
    }
    let file = heifer::heifer_isobmff::HeifFile::parse(&bytes).map_err(|e| e.to_string())?;
    let image = heifer::decode(&bytes).map_err(|e| e.to_string())?;
    let primary = file.item(file.primary_id).map_err(|e| e.to_string())?;
    let layout = match file.grid(file.primary_id) {
        Ok(g) => format!("grid of {}×{} tiles", g.columns, g.rows),
        Err(_) => format!("single `{}` image", primary.item_type),
    };
    println!(
        "  {}×{}, {}-bit{}, {layout}, {}",
        image.width,
        image.height,
        image.bit_depth,
        if image.has_alpha { ", alpha" } else { "" },
        human(bytes.len() as u64)
    );
    if heifer::read_gain_map(&bytes).ok().flatten().is_some() {
        println!("  HDR gain map");
    }
    let meta = heifer::read_metadata(&bytes).map_err(|e| e.to_string())?;
    if let Some(exif) = meta.exif_fields() {
        let mut parts = Vec::new();
        if let (Some(make), Some(model)) = (exif.make(), exif.model()) {
            parts.push(format!("{make} {model}"));
        }
        if let Some(date) = exif.date_time() {
            parts.push(date.to_string());
        }
        if exif.has_gps() {
            parts.push("GPS position".into());
        }
        if !parts.is_empty() {
            println!("  {}", parts.join(" · "));
        }
    }
    let present: Vec<&str> = [
        (meta.exif.is_some(), "EXIF"),
        (meta.xmp.is_some(), "XMP"),
        (meta.icc_profile.is_some(), "ICC profile"),
    ]
    .into_iter()
    .filter_map(|(has, name)| has.then_some(name))
    .collect();
    if !present.is_empty() {
        println!("  metadata: {}", present.join(", "));
    }
    Ok(())
}

fn human(bytes: u64) -> String {
    match bytes {
        0..1_000 => format!("{bytes} B"),
        1_000..1_000_000 => format!("{:.0} KB", bytes as f64 / 1e3),
        _ => format!("{:.1} MB", bytes as f64 / 1e6),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Option<Args>, String> {
        parse_args(s.split_whitespace().map(String::from))
    }

    #[test]
    fn single_conversion() {
        let a = parse("in.heic out.jpg -q 70").unwrap().unwrap();
        assert_eq!(a.inputs, [PathBuf::from("in.heic")]);
        assert_eq!(a.output, Some(PathBuf::from("out.jpg")));
        assert_eq!(a.quality, Some(70));
    }

    #[test]
    fn batch_conversion() {
        let a = parse("a.heic b.heic c.heic --to .JPG -o out")
            .unwrap()
            .unwrap();
        assert_eq!(a.inputs.len(), 3);
        assert_eq!(a.to.as_deref(), Some("jpg"));
        assert_eq!(
            output_path(Path::new("x/a.heic"), &a).ok(),
            Some(PathBuf::from("out/a.jpg"))
        );
        let a = parse("dir/a.heic --to png").unwrap().unwrap();
        assert_eq!(
            output_path(&a.inputs[0], &a).ok(),
            Some(PathBuf::from("dir/a.png"))
        );
    }

    #[test]
    fn info_and_errors() {
        assert!(parse("info a.heic b.heic").unwrap().unwrap().info);
        assert_eq!(parse("").unwrap(), None);
        assert_eq!(parse("--help").unwrap(), None);
        assert!(parse("a.heic").is_err());
        assert!(parse("a b c").is_err());
        assert!(parse("a b -q 101").is_err());
        assert!(parse("a b --bogus").is_err());
    }
}
