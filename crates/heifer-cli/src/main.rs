//! `heifer`: converts HEIC/HEIF photos to and from PNG, JPEG, WebP and other formats.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use heifer::image_crate::HeifEncoder;
use heifer::{EncodeFormat, EncodeOptions, EncodeSpeed};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::codecs::webp::WebPEncoder;
use image::metadata::Orientation;
use image::{
    DynamicImage, ImageBuffer, ImageDecoder, ImageEncoder, ImageFormat, ImageReader, Luma, Rgba,
};

const USAGE: &str = "\
heifer: convert HEIC/HEIF photos, in pure Rust

Usage:
  heifer <input> <output> [-q <quality>]        convert one file
  heifer <frames>... <out.heics> [--fps <n>]     encode images as a HEIF sequence
  heifer <inputs>... --to <format> [-o <dir>] [-q <quality>]
                                                 convert many files
  heifer info <files>...                         show size and metadata

The formats come from the file extensions: heic (or heif, heics), png, jpg, webp, tiff, bmp, gif.
EXIF and ICC profiles are kept, and photos are written upright.

Options:
  -q, --quality <0-100>       quality of HEIC and JPEG output (default: 80 for HEIC, 90 for JPEG)
      --format <mode>         HEIC/HEICS coding: 420 (default), 422, 444 or lossless
      --lossless              same as --format lossless
      --speed <preset>        HEIC/HEICS speed preset: fast, medium (default) or slow
      --keyframe-interval <n> max frames between keyframes in .heics sequences (default: 30)
      --b-frames <n>          max consecutive B-frames in .heics sequences (default: 2)
      --fps <n>               frame rate for .heics sequences from still images (default: 25)
  -s, --max-size <size>       HEIC/HEICS no larger than this (e.g. 500K, 2M): chooses quality
      --aux                   also write every auxiliary image as PNG
      --depth                 also write the depth map as PNG
      --gain-map              also write the HDR gain map as PNG
      --frames                write every frame of an image sequence (burst, animation) instead
                              of the main image: out.png gives out-001.png, out-002.png, ...;
                              out.gif or out.apng gives one animated file
                              A .heics output writes an animated GIF, APNG, HEIF sequence or
                              multiple input images as a HEIF image sequence
  -t, --to <format>           output format when converting several files
  -o, --out-dir <dir>         output folder (default: next to each input)
  -f, --force                 overwrite existing files
  -h, --help                  show this help
  -V, --version               show the version

Auxiliary images are written next to the output, for example photo.depth.png.

Examples:
  heifer IMG_1234.heic photo.jpg
  heifer IMG_1234.heic photo.jpg --depth
  heifer portrait.heic portrait.png --aux
  heifer *.HEIC --to jpg -o converted --gain-map
  heifer burst.heics frames.png --frames
  heifer burst.heics burst.gif --frames
  heifer frame_*.png burst.heics --fps 30 --speed fast
  heifer animation.gif animation.heics -q 70 --b-frames 3
  heifer screenshot.png screenshot.heic -q 70
  heifer screenshot.png screenshot.heic --lossless
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
    /// `--max-size`: largest HEIC file, in bytes.
    max_size: Option<usize>,
    format: Option<EncodeFormat>,
    speed: Option<EncodeSpeed>,
    keyframe_interval: Option<usize>,
    b_frames: Option<usize>,
    fps: Option<f64>,
    aux: bool,
    depth: bool,
    gain_map: bool,
    /// `--frames`: write the frames of the image sequence.
    frames: bool,
    force: bool,
}

impl Args {
    /// Whether an auxiliary image should be written next to the output.
    fn wants_aux(&self) -> bool {
        self.aux || self.depth || self.gain_map
    }
}

/// A file size: bytes, or with a `K`/`KB` (1000) or `M`/`MB` (1000000) suffix.
fn parse_size(text: &str) -> Result<usize, String> {
    let lower = text.trim().to_lowercase();
    let (number, unit) = match lower.trim_end_matches('b') {
        n if n.ends_with('k') => (&n[..n.len() - 1], 1_000.0),
        n if n.ends_with('m') => (&n[..n.len() - 1], 1_000_000.0),
        n => (n, 1.0),
    };
    number
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v > 0.0)
        .map(|v| (v * unit).round() as usize)
        .filter(|&v| v > 0)
        .ok_or(format!("size must be like 500K or 2M, not `{text}`"))
}

/// HEIC coding mode named on the command line.
fn parse_format(name: &str) -> Result<EncodeFormat, String> {
    match name.to_lowercase().as_str() {
        "420" | "420p" | "yuv420" | "4:2:0" => Ok(EncodeFormat::Ycbcr420),
        "422" | "yuv422" | "4:2:2" => Ok(EncodeFormat::Ycbcr422),
        "444" | "yuv444" | "4:4:4" => Ok(EncodeFormat::Ycbcr444),
        "lossless" | "rgb" => Ok(EncodeFormat::Lossless),
        _ => Err(format!(
            "unknown HEIC format `{name}`: use 420, 422, 444 or lossless"
        )),
    }
}

fn parse_speed(name: &str) -> Result<EncodeSpeed, String> {
    match name.to_lowercase().as_str() {
        "fast" => Ok(EncodeSpeed::Fast),
        "medium" | "default" => Ok(EncodeSpeed::Medium),
        "slow" => Ok(EncodeSpeed::Slow),
        _ => Err(format!(
            "unknown speed preset `{name}`: use fast, medium or slow"
        )),
    }
}

/// Records the coding mode, rejecting a `--format` and a `--lossless` that disagree.
fn set_format(args: &mut Args, format: EncodeFormat) -> Result<(), String> {
    match args.format {
        Some(current) if current != format => {
            Err("--format and --lossless ask for different codings".into())
        }
        _ => {
            args.format = Some(format);
            Ok(())
        }
    }
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
            "-s" | "--max-size" => args.max_size = Some(parse_size(&value(&arg)?)?),
            "-t" | "--to" => args.to = Some(value(&arg)?.trim_start_matches('.').to_lowercase()),
            "--format" => {
                let format = parse_format(&value(&arg)?)?;
                set_format(&mut args, format)?;
            }
            "--lossless" => set_format(&mut args, EncodeFormat::Lossless)?,
            "--speed" => args.speed = Some(parse_speed(&value(&arg)?)?),
            "--keyframe-interval" => {
                let raw_val = value(&arg)?;
                args.keyframe_interval = Some(
                    raw_val
                        .parse::<usize>()
                        .ok()
                        .filter(|&n| n >= 1)
                        .ok_or(format!(
                            "keyframe interval must be at least 1, not `{raw_val}`"
                        ))?,
                );
            }
            "--b-frames" => {
                let raw_val = value(&arg)?;
                args.b_frames = Some(raw_val.parse::<usize>().map_err(|_| {
                    format!("b-frames must be a non-negative integer, not `{raw_val}`")
                })?);
            }
            "--fps" => {
                let raw_val = value(&arg)?;
                args.fps = Some(
                    raw_val
                        .parse::<f64>()
                        .ok()
                        .filter(|&f| f.is_finite() && f > 0.0)
                        .ok_or(format!("fps must be a positive number, not `{raw_val}`"))?,
                );
            }
            "--aux" => args.aux = true,
            "--depth" => args.depth = true,
            "--gain-map" => args.gain_map = true,
            "--frames" => args.frames = true,
            "-o" | "--out-dir" => args.out_dir = Some(PathBuf::from(value(&arg)?)),
            "-f" | "--force" => args.force = true,
            s if s.starts_with('-') && s.len() > 1 => return Err(format!("unknown option `{s}`")),
            _ => positional.push(PathBuf::from(arg)),
        }
    }
    if positional.first().is_some_and(|p| p.as_os_str() == "info") {
        positional.remove(0);
        args.info = true;
    } else if args.to.is_none()
        && args.out_dir.is_none()
        && (positional.len() == 2
            || (positional.len() > 2 && positional.last().is_some_and(|p| is_heics(p))))
    {
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

    if !args.info
        && !args.frames
        && args.inputs.len() > 1
        && let Some(out) = args.output.as_deref().filter(|o| is_heics(o))
    {
        let refs: Vec<&Path> = args.inputs.iter().map(PathBuf::as_path).collect();
        if let Err(e) = write_sequence_paths(&refs, out, &args) {
            eprintln!("{}: {e}", out.display());
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS;
    }

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

fn is_heics(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_lowercase().as_str(), "heics" | "heifs" | "hifs"))
}

/// The frames of an animation (GIF, APNG, or a HEIF image sequence) and their durations in
/// seconds, as 8-bit RGBA.
fn animation_frames(input: &Path) -> Result<Vec<image::Frame>, String> {
    use image::AnimationDecoder;
    let bytes = std::fs::read(input).map_err(|e| e.to_string())?;
    let reader = std::io::Cursor::new(&bytes);
    let frames = match image::guess_format(&bytes).ok() {
        Some(ImageFormat::Gif) => image::codecs::gif::GifDecoder::new(reader)
            .map_err(|e| e.to_string())?
            .into_frames()
            .collect_frames(),
        Some(ImageFormat::Png) => {
            let decoder = image::codecs::png::PngDecoder::new(reader).map_err(|e| e.to_string())?;
            if !decoder.is_apng().map_err(|e| e.to_string())? {
                return Err("not an animation: a PNG image sequence needs an APNG".into());
            }
            decoder
                .apng()
                .map_err(|e| e.to_string())?
                .into_frames()
                .collect_frames()
        }
        _ if heifer::read_sequence(&bytes).ok().flatten().is_some() => {
            heifer::image_crate::HeifSequenceDecoder::from_bytes(bytes, &heifer::Options::default())
                .map_err(|e| e.to_string())?
                .into_frames()
                .collect_frames()
        }
        _ => {
            return Err(
                "not an animation: give an animated GIF, an APNG or a HEIF sequence".into(),
            );
        }
    };
    frames.map_err(|e| e.to_string())
}

/// Builds [`EncodeOptions`] from command-line flags.
fn encode_options(args: &Args, quality: u8) -> EncodeOptions {
    let mut options = EncodeOptions::new().with_quality(quality);
    if let Some(speed) = args.speed {
        options = options.with_speed(speed);
    }
    if let Some(interval) = args.keyframe_interval {
        options = options.with_keyframe_interval(interval);
    }
    if let Some(b_frames) = args.b_frames {
        options = options.with_b_frames(b_frames);
    }
    options
}

/// Converts a decoded `DynamicImage` into a 8-bit or 10-bit `heifer::Image`.
fn dynamic_to_heifer_image(image: &DynamicImage) -> heifer::Image {
    use image::ColorType;
    let is_16 = matches!(
        image.color(),
        ColorType::L16 | ColorType::La16 | ColorType::Rgb16 | ColorType::Rgba16
    );
    if is_16 {
        let buf = image.to_rgba16();
        let (w, h) = buf.dimensions();
        let has_alpha = buf.pixels().any(|p| p.0[3] < u16::MAX);
        let mut out = heifer::Image::filled(w, h, 10, [0, 0, 0, 1023]);
        out.has_alpha = has_alpha;
        for (o, p) in out.data.chunks_exact_mut(4).zip(buf.pixels()) {
            o.copy_from_slice(&p.0.map(|v| (u32::from(v) * 1023 / 65535) as u16));
        }
        out
    } else {
        let buf = image.to_rgba8();
        let (w, h) = buf.dimensions();
        let has_alpha = buf.pixels().any(|p| p.0[3] < 255);
        let mut out = heifer::Image::filled(w, h, 8, [0, 0, 0, 255]);
        out.has_alpha = has_alpha;
        for (o, p) in out.data.chunks_exact_mut(4).zip(buf.pixels()) {
            o.copy_from_slice(&p.0.map(u16::from));
        }
        out
    }
}

/// A `.heics` output: the frames of an animated input, each with its duration, as a HEIF image
/// sequence.
fn write_sequence(input: &Path, output: &Path, args: &Args) -> Result<(), String> {
    write_sequence_paths(&[input], output, args)
}

/// Encodes one animated input or a sequence of still image files into a `.heics` file.
fn write_sequence_paths(inputs: &[&Path], output: &Path, args: &Args) -> Result<(), String> {
    if inputs.is_empty() {
        return Err("no input frames given for .heics sequence".into());
    }
    if args.wants_aux() {
        return Err("auxiliary options (--aux, --depth, --gain-map) do not apply to .heics".into());
    }
    if args.max_size.is_some() {
        if args.quality.is_some() {
            return Err("--max-size chooses the quality: do not give -q as well".into());
        }
        if args.format == Some(EncodeFormat::Lossless) {
            return Err("--max-size does not apply to lossless coding".into());
        }
    }
    if output.exists() && !args.force {
        return Err(format!(
            "{} exists (use --force to overwrite)",
            output.display()
        ));
    }
    let start = Instant::now();
    let (images, durations): (Vec<heifer::Image>, Vec<f64>) = if inputs.len() == 1 {
        let frames = animation_frames(inputs[0])?;
        let mut images = Vec::with_capacity(frames.len());
        let mut durations = Vec::with_capacity(frames.len());
        let override_dur = args.fps.map(|fps| 1.0 / fps);
        for frame in &frames {
            let (numer, denom) = frame.delay().numer_denom_ms();
            let dur =
                override_dur.unwrap_or_else(|| f64::from(numer) / f64::from(denom.max(1)) / 1000.0);
            durations.push(dur);
            let buffer = frame.buffer();
            let (w, h) = buffer.dimensions();
            let has_alpha = buffer.pixels().any(|p| p.0[3] < 255);
            let mut image = heifer::Image::filled(w, h, 8, [0, 0, 0, 255]);
            image.has_alpha = has_alpha;
            for (o, p) in image.data.chunks_exact_mut(4).zip(buffer.pixels()) {
                o.copy_from_slice(&p.0.map(u16::from));
            }
            images.push(image);
        }
        (images, durations)
    } else {
        let dur = 1.0 / args.fps.unwrap_or(25.0);
        let mut images = Vec::with_capacity(inputs.len());
        for &path in inputs {
            let (dyn_img, _, _) = read(path)?;
            images.push(dynamic_to_heifer_image(&dyn_img));
        }
        let durations = vec![dur; images.len()];
        (images, durations)
    };
    let format = args.format.unwrap_or_default();
    let options = encode_options(args, args.quality.unwrap_or(80));
    let (bytes, chosen_quality) = if let Some(max_bytes) = args.max_size {
        let (b, q) = heifer::encode_sequence_to_size_with_format(
            &images, &durations, format, max_bytes, &options,
        )
        .map_err(|e| e.to_string())?;
        (b, Some(q))
    } else {
        (
            heifer::encode_sequence_with_format(&images, &durations, format, &options)
                .map_err(|e| e.to_string())?,
            None,
        )
    };
    std::fs::write(output, &bytes).map_err(|e| format!("{}: {e}", output.display()))?;
    let total: f64 = durations.iter().sum();
    let quality_note = chosen_quality.map_or_else(String::new, |q| format!(", q={q}"));
    let label = if inputs.len() == 1 {
        inputs[0].display().to_string()
    } else {
        format!("{} files", inputs.len())
    };
    println!(
        "{label} → {} ({} frames {}×{}, {total:.2} s, {}{quality_note}, {:.2?})",
        output.display(),
        images.len(),
        images.first().map_or(0, |i| i.width),
        images.first().map_or(0, |i| i.height),
        human(bytes.len() as u64),
        start.elapsed()
    );
    Ok(())
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
    let mut reader = ImageReader::open(input)
        .and_then(ImageReader::with_guessed_format)
        .map_err(|e| e.to_string())?;
    reader.no_limits();
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

/// Rejects HEIC/HEICS options when the output format does not support them.
fn check_heic_only(args: &Args, output: &Path) -> Result<(), String> {
    if !is_heics(output)
        && (args.keyframe_interval.is_some() || args.b_frames.is_some() || args.fps.is_some())
    {
        return Err(
            "--keyframe-interval, --b-frames and --fps only apply to .heics sequence output".into(),
        );
    }
    if !is_heif(output) && args.format.is_some() {
        return Err("the HEIC coding mode only applies to HEIC/HEICS output".into());
    }
    if !is_heif(output) && args.speed.is_some() {
        return Err("--speed only applies to HEIC/HEICS output".into());
    }
    if args.max_size.is_some() {
        if !is_heif(output) {
            return Err("--max-size only applies to HEIC/HEICS output".into());
        }
        if args.quality.is_some() {
            return Err("--max-size chooses the quality: do not give -q as well".into());
        }
        if args.format == Some(EncodeFormat::Lossless) {
            return Err("--max-size does not apply to lossless coding".into());
        }
    }
    Ok(())
}

fn convert(input: &Path, output: &Path, args: &Args) -> Result<(), String> {
    if args.frames {
        return write_frames(input, output, args);
    }
    if is_heics(output) {
        return write_sequence(input, output, args);
    }
    check_heic_only(args, output)?;
    if args.wants_aux() && !is_heif(input) {
        return Err("auxiliary images can only be extracted from HEIC/HEIF input".into());
    }
    if output.exists() && !args.force {
        return Err(format!(
            "{} exists (use --force to overwrite)",
            output.display()
        ));
    }
    let start = Instant::now();
    let aux_bytes = if args.wants_aux() {
        Some(std::fs::read(input).map_err(|e| e.to_string())?)
    } else {
        None
    };
    let (image, icc, exif) = read(input).map_err(|e| match sequence_frames(input) {
        Some(n) => format!("{e} (the file holds an image sequence of {n} frames: use --frames)"),
        None => e,
    })?;
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
    let mut fitted_quality = None;
    let result = match format {
        None if args.max_size.is_some() => {
            let max = args.max_size.expect("checked");
            let encode = |quality: u8| -> Result<Vec<u8>, String> {
                let mut bytes = Vec::new();
                let options = encode_options(args, quality);
                let heif = args.format.unwrap_or_default();
                write(
                    HeifEncoder::with_format(&mut bytes, options, heif),
                    &image,
                    icc.clone(),
                    exif.clone(),
                )?;
                Ok(bytes)
            };
            match heifer::fit_to_size(max, encode)? {
                Some((bytes, quality)) => {
                    fitted_quality = Some(quality);
                    std::io::Write::write_all(&mut writer, &bytes).map_err(|e| e.to_string())
                }
                None => Err(format!(
                    "the image does not fit in {}, even at quality 0",
                    human(max as u64)
                )),
            }
        }
        None => {
            let options = encode_options(args, args.quality.unwrap_or(80));
            write(
                HeifEncoder::with_format(&mut writer, options, args.format.unwrap_or_default()),
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
    let quality = fitted_quality.map_or(String::new(), |q| format!(", quality {q}"));
    println!(
        "{} → {} ({}×{}, {}{quality}, {:.2?})",
        input.display(),
        output.display(),
        image.width(),
        image.height(),
        human(size),
        start.elapsed()
    );
    if let Some(bytes) = &aux_bytes {
        write_auxiliary(output, bytes, args)?;
    }
    if let Some(n) = sequence_frames(input).filter(|&n| n > 1) {
        println!("  also holds an image sequence of {n} frames: add --frames to extract them");
    }
    Ok(())
}

/// The number of frames of the image sequence of a HEIF input, if it has one.
fn sequence_frames(input: &Path) -> Option<usize> {
    let bytes = std::fs::read(input).ok()?;
    if !bytes.get(4..8).is_some_and(|b| b == b"ftyp") {
        return None;
    }
    heifer::read_sequence(&bytes)
        .ok()
        .flatten()
        .map(|seq| seq.frames.len())
}

/// Path of frame `index` (from 0) of `count`, after `output`: `out-001.png`, with at least
/// three digits.
fn frame_path(output: &Path, index: usize, count: usize) -> PathBuf {
    let digits = count.to_string().len().max(3);
    let stem = output.file_stem().unwrap_or_default().to_string_lossy();
    let ext = output.extension().unwrap_or_default().to_string_lossy();
    output.with_file_name(format!("{stem}-{:0digits$}.{ext}", index + 1))
}

/// `--frames`: writes every frame of the image sequence, in display order.
fn write_frames(input: &Path, output: &Path, args: &Args) -> Result<(), String> {
    if is_heif(output) {
        return Err("--frames writes images: give a PNG, JPEG, WebP... output".into());
    }
    if args.format.is_some() || args.max_size.is_some() || args.wants_aux() {
        return Err("--frames cannot be combined with HEIC or auxiliary image options".into());
    }
    let ext = output
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase);
    if matches!(ext.as_deref(), Some("gif" | "apng")) {
        return write_animation(input, output, args, ext.as_deref() == Some("apng"));
    }
    let format = ImageFormat::from_path(output)
        .map_err(|_| format!("unknown output format for {}", output.display()))?;
    let start = Instant::now();
    let bytes = std::fs::read(input).map_err(|e| e.to_string())?;
    let seq = heifer::read_sequence(&bytes)
        .map_err(|e| e.to_string())?
        .ok_or("no image sequence in this file")?;
    let count = seq.frames.len();
    let paths: Vec<PathBuf> = (0..count).map(|i| frame_path(output, i, count)).collect();
    // Check every target before writing any, so that a conflict leaves no partial output.
    if !args.force
        && let Some(p) = paths.iter().find(|p| p.exists())
    {
        return Err(format!("{} exists (use --force to overwrite)", p.display()));
    }
    let frames = heifer::decode_sequence(&bytes).map_err(|e| e.to_string())?;
    for (frame, path) in frames.zip(&paths) {
        let frame = frame.map_err(|e| e.to_string())?;
        let buffer =
            ImageBuffer::<Rgba<u8>, _>::from_raw(frame.width, frame.height, frame.to_rgba8())
                .ok_or("frame of an unexpected size")?;
        let mut image = DynamicImage::ImageRgba8(buffer);
        if matches!(format, ImageFormat::Jpeg) {
            image = DynamicImage::ImageRgb8(image.to_rgb8());
        }
        image
            .save_with_format(path, format)
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    println!(
        "{} → {} … {} ({count} frames {}×{}, {:.2} s, {:.2?})",
        input.display(),
        paths
            .first()
            .map_or(String::new(), |p| p.display().to_string()),
        paths
            .last()
            .map_or(String::new(), |p| p.display().to_string()),
        seq.width,
        seq.height,
        seq.duration(),
        start.elapsed()
    );
    Ok(())
}

/// Writes the auxiliary images selected by `--aux`, `--depth` and `--gain-map`.
fn write_auxiliary(output: &Path, bytes: &[u8], args: &Args) -> Result<(), String> {
    let auxiliary = heifer::auxiliary_images(bytes).map_err(|e| e.to_string())?;
    // An explicit selector must find the type it asks for; `--aux` alone is happy with none.
    if args.depth && !auxiliary.iter().any(heifer::AuxiliaryImage::is_depth) {
        return Err("no depth map in this file".into());
    }
    if args.gain_map && !auxiliary.iter().any(heifer::AuxiliaryImage::is_gain_map) {
        return Err("no HDR gain map in this file".into());
    }
    let mut seen = Vec::new();
    let selected: Vec<_> = auxiliary
        .into_iter()
        .filter(|aux| {
            args.aux || (args.depth && aux.is_depth()) || (args.gain_map && aux.is_gain_map())
        })
        .map(|aux| {
            let path = auxiliary_path(output, &aux, &mut seen);
            (path, aux)
        })
        .collect();
    // Check every target before writing any, so that a conflict leaves no partial output.
    for (path, _) in &selected {
        if path.exists() && !args.force {
            return Err(format!(
                "{} exists (use --force to overwrite)",
                path.display()
            ));
        }
    }
    for (path, aux) in selected {
        let image = heifer::decode_auxiliary(bytes, aux.id).map_err(|e| e.to_string())?;
        write_auxiliary_png(&path, &aux, &image)?;
        println!(
            "  {} ({} {}×{}, {}-bit)",
            path.display(),
            auxiliary_kind(&aux),
            aux.width,
            aux.height,
            aux.bit_depth
        );
    }
    Ok(())
}

/// Path of an auxiliary image next to the main output: `photo.depth.png`, `photo.gain-map.png`,
/// then `photo.depth-<id>.png` for a second image of the same kind.
fn auxiliary_path(
    output: &Path,
    aux: &heifer::AuxiliaryImage,
    seen: &mut Vec<(&'static str, usize)>,
) -> PathBuf {
    let kind = auxiliary_slug(aux);
    let number = match seen.iter_mut().find(|(name, _)| *name == kind) {
        Some((_, number)) => {
            *number += 1;
            *number
        }
        None => {
            seen.push((kind, 1));
            1
        }
    };
    let base = output.with_extension("");
    let stem = base
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "image".into());
    let name = if number == 1 {
        format!("{stem}.{kind}.png")
    } else {
        format!("{stem}.{kind}-{}.png", aux.id)
    };
    base.with_file_name(name)
}

fn auxiliary_slug(aux: &heifer::AuxiliaryImage) -> &'static str {
    if aux.is_depth() {
        "depth"
    } else if aux.is_gain_map() {
        "gain-map"
    } else if aux.is_alpha() {
        "alpha"
    } else {
        "aux"
    }
}

fn auxiliary_kind(aux: &heifer::AuxiliaryImage) -> &'static str {
    if aux.is_depth() {
        "depth map"
    } else if aux.is_gain_map() {
        "HDR gain map"
    } else if aux.is_alpha() {
        "alpha plane"
    } else {
        "auxiliary image"
    }
}

/// Writes a decoded auxiliary image as PNG: grayscale for depth, alpha and gain maps, RGBA for
/// other refinements, at the coded bit depth. Samples below 8 bits are widened to 8, above to 16,
/// so that the PNG viewer sees the full range.
fn write_auxiliary_png(
    path: &Path,
    aux: &heifer::AuxiliaryImage,
    image: &heifer::Image,
) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut writer = std::io::BufWriter::new(file);
    let grayscale = aux.is_depth() || aux.is_gain_map() || aux.is_alpha();
    let width = image.width;
    let height = image.height;
    let max = u32::from(image.max_value());
    let to_8 = |value: u16| ((u32::from(value) * 255 + max / 2) / max) as u8;
    let to_16 = |value: u16| ((u32::from(value) * 65535 + max / 2) / max) as u16;
    let invalid = || format!("{}: invalid image size", path.display());
    let png = match (grayscale, image.bit_depth) {
        (true, 0..=8) => {
            let data = image
                .data
                .chunks_exact(4)
                .map(|pixel| to_8(pixel[0]))
                .collect();
            DynamicImage::ImageLuma8(
                ImageBuffer::<Luma<u8>, _>::from_raw(width, height, data).ok_or_else(invalid)?,
            )
        }
        (true, _) => {
            let data = image
                .data
                .chunks_exact(4)
                .map(|pixel| to_16(pixel[0]))
                .collect();
            DynamicImage::ImageLuma16(
                ImageBuffer::<Luma<u16>, _>::from_raw(width, height, data).ok_or_else(invalid)?,
            )
        }
        (false, 0..=8) => {
            let data = image
                .data
                .chunks_exact(4)
                .flat_map(|pixel| {
                    [
                        to_8(pixel[0]),
                        to_8(pixel[1]),
                        to_8(pixel[2]),
                        to_8(pixel[3]),
                    ]
                })
                .collect();
            DynamicImage::ImageRgba8(
                ImageBuffer::<Rgba<u8>, _>::from_raw(width, height, data).ok_or_else(invalid)?,
            )
        }
        (false, _) => {
            let data = image
                .data
                .chunks_exact(4)
                .flat_map(|pixel| {
                    [
                        to_16(pixel[0]),
                        to_16(pixel[1]),
                        to_16(pixel[2]),
                        to_16(pixel[3]),
                    ]
                })
                .collect();
            DynamicImage::ImageRgba16(
                ImageBuffer::<Rgba<u16>, _>::from_raw(width, height, data).ok_or_else(invalid)?,
            )
        }
    };
    let result = png
        .write_to(&mut writer, ImageFormat::Png)
        .map_err(|error| error.to_string())
        .and_then(|()| std::io::Write::flush(&mut writer).map_err(|error| error.to_string()));
    if let Err(error) = result {
        drop(writer);
        let _ = std::fs::remove_file(path); // no truncated file left behind
        return Err(format!("{}: {error}", path.display()));
    }
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
    for line in info_lines(input)? {
        println!("{line}");
    }
    Ok(())
}

/// `--frames` to a GIF or APNG output: one animated file, played in a loop.
fn write_animation(input: &Path, output: &Path, args: &Args, apng: bool) -> Result<(), String> {
    use image::AnimationDecoder;
    if output.exists() && !args.force {
        return Err(format!(
            "{} exists (use --force to overwrite)",
            output.display()
        ));
    }
    let start = Instant::now();
    let bytes = std::fs::read(input).map_err(|e| e.to_string())?;
    let decoder =
        heifer::image_crate::HeifSequenceDecoder::from_bytes(bytes, &heifer::Options::default())
            .map_err(|e| e.to_string())?;
    let count = decoder.len();
    let frames = decoder.into_frames();
    // Write to a temporary file first, so that a failure leaves no partial output.
    let partial = output.with_extension("partial");
    let file =
        std::fs::File::create(&partial).map_err(|e| format!("{}: {e}", partial.display()))?;
    let writer = std::io::BufWriter::new(file);
    let result = if apng {
        write_apng(writer, frames, count)
    } else {
        let mut encoder = image::codecs::gif::GifEncoder::new_with_speed(writer, 10);
        encoder
            .set_repeat(image::codecs::gif::Repeat::Infinite)
            .and_then(|()| encoder.try_encode_frames(frames))
            .map_err(|e| e.to_string())
    };
    if let Err(e) = result {
        let _ = std::fs::remove_file(&partial);
        return Err(e);
    }
    std::fs::rename(&partial, output).map_err(|e| format!("{}: {e}", output.display()))?;
    let size = std::fs::metadata(output).map(|m| m.len()).unwrap_or(0);
    println!(
        "{} → {} ({count} frames, {}, {:.2?})",
        input.display(),
        output.display(),
        human(size),
        start.elapsed()
    );
    Ok(())
}

/// Writes animated PNG frames (RGBA 8-bit), each with its own delay, played in a loop.
fn write_apng(
    writer: impl std::io::Write,
    frames: image::Frames<'_>,
    count: usize,
) -> Result<(), String> {
    let mut frames = frames.peekable();
    let (width, height) = match frames.peek() {
        Some(Ok(f)) => f.buffer().dimensions(),
        Some(Err(_)) => {
            return Err(match frames.next() {
                Some(Err(e)) => e.to_string(),
                _ => unreachable!("peeked an error"),
            });
        }
        None => return Err("the sequence has no frame".into()),
    };
    let mut encoder = png::Encoder::new(writer, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let num_frames = u32::try_from(count).map_err(|_| "too many frames")?;
    encoder
        .set_animated(num_frames, 0)
        .map_err(|e| e.to_string())?;
    let mut png = encoder.write_header().map_err(|e| e.to_string())?;
    for frame in frames {
        let frame = frame.map_err(|e| e.to_string())?;
        // The delay in milliseconds, as a fraction of a second.
        let (numer, denom) = frame.delay().numer_denom_ms();
        let ms = (f64::from(numer) / f64::from(denom))
            .round()
            .clamp(0.0, 65535.0) as u16;
        png.set_frame_delay(ms, 1000).map_err(|e| e.to_string())?;
        png.write_image_data(frame.buffer().as_raw())
            .map_err(|e| e.to_string())?;
    }
    png.finish().map_err(|e| e.to_string())
}

/// The lines printed by `heifer info`, kept separate so that tests can read them.
fn info_lines(input: &Path) -> Result<Vec<String>, String> {
    let bytes = std::fs::read(input).map_err(|e| e.to_string())?;
    let mut lines = vec![input.display().to_string()];
    if !bytes.get(4..8).is_some_and(|b| b == b"ftyp") {
        let (image, _, _) = read(input)?;
        lines.push(format!(
            "  {}×{}, {:?}",
            image.width(),
            image.height(),
            image.color()
        ));
        return Ok(lines);
    }
    let sequence = heifer::read_sequence(&bytes).ok().flatten();
    let sequence_line = sequence.as_ref().map(|seq| {
        let keyframes = seq.frames.iter().filter(|f| f.keyframe).count();
        format!(
            "  image sequence: {} frames {}×{}, {:.2} s, {keyframes} keyframes",
            seq.frames.len(),
            seq.width,
            seq.height,
            seq.duration()
        )
    });
    let file = match heifer::heifer_isobmff::HeifFile::parse(&bytes) {
        Ok(file) => file,
        // A sequence without a still image.
        Err(_) if sequence_line.is_some() => {
            lines.extend(sequence_line);
            return Ok(lines);
        }
        Err(e) => return Err(e.to_string()),
    };
    let image = heifer::read_info(&bytes).map_err(|e| e.to_string())?;
    let primary = file.item(file.primary_id).map_err(|e| e.to_string())?;
    let layout = match file.grid(file.primary_id) {
        Ok(g) => format!("grid of {}×{} tiles", g.columns, g.rows),
        Err(_) => format!("single `{}` image", primary.item_type),
    };
    lines.push(format!(
        "  {}×{}, {}-bit{}, {layout}, {}",
        image.width,
        image.height,
        image.bit_depth,
        if image.has_alpha { ", alpha" } else { "" },
        human(bytes.len() as u64)
    ));
    for thumb in file.referencing_items(file.primary_id, b"thmb") {
        if let Ok(Some((w, h))) = file.image_size(thumb) {
            lines.push(format!("  thumbnail {w}×{h}"));
        }
    }
    let meta = heifer::read_metadata(&bytes).map_err(|e| e.to_string())?;
    let exif = meta.exif_fields();
    let headroom = exif.as_ref().and_then(|exif| exif.apple_hdr_headroom());
    for aux in heifer::auxiliary_images(&bytes).unwrap_or_default() {
        let size = if aux.width == 0 || aux.height == 0 {
            "unknown size".to_string()
        } else {
            format!("{}×{}", aux.width, aux.height)
        };
        lines.push(format!(
            "  {} {}, {}-bit, {}",
            auxiliary_kind(&aux),
            size,
            aux.bit_depth,
            aux.urn
        ));
        if aux.is_depth()
            && let Ok(Some(info)) = heifer::DepthInfo::read(&bytes, aux.id)
        {
            let mut parts = Vec::new();
            if let Some(value) = info.z_near {
                parts.push(format!("z-near {value:.6}"));
            }
            if let Some(value) = info.z_far {
                parts.push(format!("z-far {value:.6}"));
            }
            if let Some(value) = info.d_min {
                parts.push(format!("d-min {value:.6}"));
            }
            if let Some(value) = info.d_max {
                parts.push(format!("d-max {value:.6}"));
            }
            parts.push(info.representation.to_string());
            if let Some(view) = info.disparity_reference_view {
                parts.push(format!("reference view {view}"));
            }
            lines.push(format!("    depth metadata: {}", parts.join(", ")));
        }
        if aux.is_gain_map()
            && let Some(headroom) = headroom
        {
            lines.push(format!("    gain map metadata: headroom {headroom:.1}×"));
        }
    }
    if let Some(exif) = exif {
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
            lines.push(format!("  {}", parts.join(" · ")));
        }
    }
    if let Ok(regions) = heifer::item_regions(&file, file.primary_id) {
        for r in &regions {
            let names: Vec<&str> = r
                .descriptions
                .iter()
                .map(|d| d.name.as_str())
                .filter(|n| !n.is_empty())
                .collect();
            let count = r.regions.regions.len();
            lines.push(format!(
                "  regions: {count} region{} on {}×{}{}",
                if count == 1 { "" } else { "s" },
                r.regions.reference_width,
                r.regions.reference_height,
                if names.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", names.join(", "))
                }
            ));
        }
    }
    let iptc_block = heifer::item_iptc(&file, file.primary_id).ok().flatten();
    let has_iptc = iptc_block.is_some();
    if let Some(iptc) = iptc_block
        .as_deref()
        .and_then(|b| heifer::Iptc::parse(b).ok())
    {
        let mut parts = Vec::new();
        if let Some(caption) = iptc.caption() {
            parts.push(format!("\"{caption}\""));
        }
        if let Some(by) = iptc.by_line() {
            parts.push(format!("by {by}"));
        }
        let place: Vec<String> = [iptc.city(), iptc.country()]
            .into_iter()
            .flatten()
            .collect();
        if !place.is_empty() {
            parts.push(place.join(", "));
        }
        let keywords = iptc.keywords();
        if !keywords.is_empty() {
            parts.push(format!("keywords: {}", keywords.join(", ")));
        }
        if !parts.is_empty() {
            lines.push(format!("  IPTC: {}", parts.join(" · ")));
        }
    }
    let present: Vec<&str> = [
        (meta.exif.is_some(), "EXIF"),
        (meta.xmp.is_some(), "XMP"),
        (meta.icc_profile.is_some(), "ICC profile"),
        (has_iptc, "IPTC"),
    ]
    .into_iter()
    .filter_map(|(has, name)| has.then_some(name))
    .collect();
    if !present.is_empty() {
        lines.push(format!("  metadata: {}", present.join(", ")));
    }
    lines.extend(sequence_line);
    Ok(lines)
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
    use std::sync::atomic::{AtomicU32, Ordering};

    fn parse(s: &str) -> Result<Option<Args>, String> {
        parse_args(s.split_whitespace().map(String::from))
    }

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures")
            .join(name)
    }

    /// A unique folder under the system temporary directory, removed by the caller.
    fn temp_dir(tag: &str) -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let number = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("heifer-cli-{}-{tag}-{number}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
    fn heic_coding_modes() {
        let format = |s: &str| parse(s).unwrap().unwrap().format;
        assert_eq!(
            format("a.png b.heic --format 444"),
            Some(EncodeFormat::Ycbcr444)
        );
        assert_eq!(
            format("a.png b.heic --format 4:4:4"),
            Some(EncodeFormat::Ycbcr444)
        );
        assert_eq!(
            format("a.png b.heic --format lossless"),
            Some(EncodeFormat::Lossless)
        );
        assert_eq!(
            format("a.png b.heic --lossless"),
            Some(EncodeFormat::Lossless)
        );
        assert_eq!(format("a.png b.heic"), None, "the default stays 4:2:0");
        assert_eq!(
            format("a.png b.heic --format 4:2:2"),
            Some(EncodeFormat::Ycbcr422)
        );
        assert!(parse("a.png b.heic --format 411").is_err());
        assert!(parse("a.png b.heic --format 444 --lossless").is_err());

        let args = parse("a.png b.heic --lossless").unwrap().unwrap();
        assert!(check_heic_only(&args, Path::new("b.heic")).is_ok());
        assert!(check_heic_only(&args, Path::new("b.png")).is_err());
    }

    #[test]
    fn max_size() {
        assert_eq!(parse_size("500K"), Ok(500_000));
        assert_eq!(parse_size("2mb"), Ok(2_000_000));
        assert_eq!(parse_size("1.5M"), Ok(1_500_000));
        assert_eq!(parse_size("1234"), Ok(1234));
        for bad in ["", "0", "-5K", "abc", "K"] {
            assert!(parse_size(bad).is_err(), "{bad}");
        }
        let args = parse("a.png b.heic --max-size 300K").unwrap().unwrap();
        assert_eq!(args.max_size, Some(300_000));
        assert!(check_heic_only(&args, Path::new("b.heic")).is_ok());
        assert!(check_heic_only(&args, Path::new("b.png")).is_err());
        let both = parse("a.png b.heic -s 300K -q 50").unwrap().unwrap();
        assert!(check_heic_only(&both, Path::new("b.heic")).is_err());
        let lossless = parse("a.png b.heic -s 300K --lossless").unwrap().unwrap();
        assert!(check_heic_only(&lossless, Path::new("b.heic")).is_err());
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

    #[test]
    fn auxiliary_flags_are_parsed() {
        let all = parse("in.heic out.png --aux").unwrap().unwrap();
        assert!(all.aux && !all.depth && !all.gain_map);
        assert!(all.wants_aux());
        let depth = parse("in.heic out.png --depth").unwrap().unwrap();
        assert!(depth.depth && !depth.aux && depth.wants_aux());
        let gain = parse("in.heic out.png --gain-map").unwrap().unwrap();
        assert!(gain.gain_map && !gain.aux && gain.wants_aux());
        assert!(!parse("in.heic out.png").unwrap().unwrap().wants_aux());
    }

    #[test]
    fn info_describes_a_portrait() {
        let path = fixture("real/heif_other__pug.heic");
        if !path.exists() {
            eprintln!("skipping: {} not found", path.display());
            return;
        }
        let lines = info_lines(&path).unwrap();
        let joined = lines.join("\n");
        assert!(
            joined.contains("depth map 768×576, 8-bit, urn:mpeg:hevc:2015:auxid:2"),
            "{joined}"
        );
        assert!(
            joined.contains("d-min 1.498047") && joined.contains("d-max 4.382812"),
            "{joined}"
        );
        assert!(joined.contains("uniform disparity"), "{joined}");
        assert!(joined.contains("reference view 0"), "{joined}");
        assert!(
            joined
                .contains("HDR gain map 2016×1512, 8-bit, urn:com:apple:photo:2020:aux:hdrgainmap"),
            "{joined}"
        );
    }

    #[test]
    fn info_describes_an_unci_depth_map() {
        let path = fixture("real/heif_special__unsupported_depth_image.heic");
        if !path.exists() {
            eprintln!("skipping: {} not found", path.display());
            return;
        }
        let lines = info_lines(&path).unwrap();
        let joined = lines.join("\n");
        assert!(
            joined.contains("depth map 64×32, 8-bit, urn:mpeg:hevc:2015:auxid:2"),
            "{joined}"
        );
        assert!(!joined.contains("depth metadata"), "{joined}");
    }

    #[test]
    fn info_reports_iptc() {
        let path = fixture("real/heif_other__L_exif_xmp_iptc.heic");
        if !path.exists() {
            eprintln!("skipping: {} not found", path.display());
            return;
        }
        let lines = info_lines(&path).unwrap();
        let joined = lines.join("\n");
        assert!(joined.contains("metadata: EXIF, XMP, IPTC"), "{joined}");
        assert!(
            joined.contains("IPTC: Budapest, Hungary · keywords: HvK, 2006"),
            "{joined}"
        );
    }

    #[test]
    fn extracts_every_auxiliary_image() {
        let path = fixture("real/heif_other__pug.heic");
        if !path.exists() {
            eprintln!("skipping: {} not found", path.display());
            return;
        }
        let bytes = std::fs::read(&path).unwrap();
        let dir = temp_dir("aux");
        let output = dir.join("photo.png");
        let mut args = parse("in.heic out.png --aux").unwrap().unwrap();
        write_auxiliary(&output, &bytes, &args).unwrap();

        let depth = dir.join("photo.depth.png");
        let gain = dir.join("photo.gain-map.png");
        assert!(depth.exists() && gain.exists());
        let depth_image = image::open(&depth).unwrap();
        assert_eq!((depth_image.width(), depth_image.height()), (768, 576));
        assert_eq!(depth_image.color(), image::ColorType::L8);
        let gain_image = image::open(&gain).unwrap();
        assert_eq!((gain_image.width(), gain_image.height()), (2016, 1512));
        assert_eq!(gain_image.color(), image::ColorType::L8);

        // Existing files are kept unless --force is given.
        let error = write_auxiliary(&output, &bytes, &args).unwrap_err();
        assert!(error.contains("exists"), "{error}");
        args.force = true;
        write_auxiliary(&output, &bytes, &args).unwrap();

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_auxiliary_types_are_errors() {
        let unci = fixture("real/heif_special__unsupported_depth_image.heic");
        let alpha = fixture("alpha.heic");
        let plain = fixture("single_image.heic");
        if !unci.exists() || !alpha.exists() || !plain.exists() {
            eprintln!("skipping: fixtures not found");
            return;
        }
        let dir = temp_dir("missing");
        let output = dir.join("photo.png");

        let bytes = std::fs::read(&unci).unwrap();
        let args = parse("in.heic out.png --gain-map").unwrap().unwrap();
        let error = write_auxiliary(&output, &bytes, &args).unwrap_err();
        assert!(error.contains("no HDR gain map"), "{error}");

        let bytes = std::fs::read(&alpha).unwrap();
        let args = parse("in.heic out.png --depth").unwrap().unwrap();
        let error = write_auxiliary(&output, &bytes, &args).unwrap_err();
        assert!(error.contains("no depth map"), "{error}");

        // --aux alone is fine with a file that has no auxiliary image at all.
        let bytes = std::fs::read(&plain).unwrap();
        let args = parse("in.heic out.png --aux").unwrap().unwrap();
        assert!(write_auxiliary(&output, &bytes, &args).is_ok());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn widens_10_bit_auxiliary_images_to_16_bit_png() {
        let path = fixture("real/heif__RGBA_10__128x128.heif");
        if !path.exists() {
            eprintln!("skipping: {} not found", path.display());
            return;
        }
        let bytes = std::fs::read(&path).unwrap();
        let dir = temp_dir("alpha10");
        let output = dir.join("photo.png");
        let args = parse("in.heif out.png --aux").unwrap().unwrap();
        write_auxiliary(&output, &bytes, &args).unwrap();

        let alpha = image::open(dir.join("photo.alpha.png")).unwrap();
        assert_eq!(alpha.color(), image::ColorType::L16);
        assert_eq!((alpha.width(), alpha.height()), (128, 128));
        assert!(
            alpha.to_luma16().iter().any(|&value| value > 1023),
            "10-bit samples are widened to the 16-bit PNG range"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn auxiliary_output_requires_a_heif_input() {
        let dir = temp_dir("non-heif");
        let args = parse("in.png out.png --depth").unwrap().unwrap();
        let error = convert(&dir.join("in.png"), &dir.join("out.png"), &args).unwrap_err();
        assert!(
            error.contains("only be extracted from HEIC/HEIF"),
            "{error}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn bframes() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../heifer/tests/data/bframes.mp4")
    }

    #[test]
    fn frame_paths_are_numbered_after_the_output() {
        let out = Path::new("dir/burst.png");
        assert_eq!(frame_path(out, 0, 24), Path::new("dir/burst-001.png"));
        assert_eq!(frame_path(out, 23, 24), Path::new("dir/burst-024.png"));
        assert_eq!(frame_path(out, 0, 1200), Path::new("dir/burst-0001.png"));
        let args = parse("in.heics out.png --frames").unwrap().unwrap();
        assert!(args.frames);
    }

    #[test]
    fn frames_are_written_in_display_order() {
        let dir = temp_dir("frames");
        let out = dir.join("f.png");
        let args = parse("in out.png --frames").unwrap().unwrap();
        write_frames(&bframes(), &out, &args).unwrap();
        // The brightness of the test stream grows with the display index.
        let means: Vec<f64> = (0..24)
            .map(|i| {
                let image = image::open(frame_path(&out, i, 24)).unwrap().to_luma8();
                image.pixels().map(|p| f64::from(p.0[0])).sum::<f64>() / f64::from(128 * 96)
            })
            .collect();
        assert!(means.windows(2).all(|m| m[1] > m[0] + 3.0), "{means:?}");
        assert!(!frame_path(&out, 24, 24).exists());
        // Existing frames are not overwritten without --force.
        let err = write_frames(&bframes(), &out, &args).unwrap_err();
        assert!(err.contains("exists"), "{err}");
        let forced = parse("in out.png --frames -f").unwrap().unwrap();
        write_frames(&bframes(), &out, &forced).unwrap();
        // Frames are images, not HEIC.
        let err = write_frames(&bframes(), &dir.join("f.heic"), &args).unwrap_err();
        assert!(err.contains("--frames"), "{err}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sequences_are_reported() {
        assert_eq!(sequence_frames(&bframes()), Some(24));
        let lines = info_lines(&bframes()).unwrap();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("image sequence: 24 frames 128×96, 0.96 s, 2 keyframes")),
            "{lines:?}"
        );
        // A file with only a sequence points to --frames.
        let dir = temp_dir("seq-only");
        let args = parse("in out.png").unwrap().unwrap();
        let err = convert(&bframes(), &dir.join("x.png"), &args).unwrap_err();
        assert!(err.contains("use --frames"), "{err}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn animations_keep_every_frame_and_delay() {
        use image::AnimationDecoder;
        let dir = temp_dir("anim");
        let args = parse("in out.gif --frames").unwrap().unwrap();
        let gif = dir.join("a.gif");
        write_frames(&bframes(), &gif, &args).unwrap();
        let file = std::io::BufReader::new(std::fs::File::open(&gif).unwrap());
        let frames = image::codecs::gif::GifDecoder::new(file)
            .unwrap()
            .into_frames()
            .collect_frames()
            .unwrap();
        assert_eq!(frames.len(), 24);
        assert!(frames.iter().all(|f| f.delay().numer_denom_ms() == (40, 1)));

        let apng = dir.join("a.apng");
        write_frames(&bframes(), &apng, &args).unwrap();
        let file = std::io::BufReader::new(std::fs::File::open(&apng).unwrap());
        let frames = image::codecs::png::PngDecoder::new(file)
            .unwrap()
            .apng()
            .unwrap()
            .into_frames()
            .collect_frames()
            .unwrap();
        assert_eq!(frames.len(), 24);
        assert!(frames.iter().all(|f| f.delay().numer_denom_ms() == (40, 1)));
        // Lossless: the same pixels as the decoder's frames.
        let own: Vec<heifer::Image> = heifer::decode_sequence(&std::fs::read(bframes()).unwrap())
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(frames[7].buffer().as_raw(), &own[7].to_rgba8());

        // No overwrite without --force, and no partial file left behind.
        let err = write_frames(&bframes(), &gif, &args).unwrap_err();
        assert!(err.contains("exists"), "{err}");
        assert!(!dir.join("a.partial").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn animations_become_heif_sequences() {
        let dir = temp_dir("heics");
        let out = dir.join("a.heics");
        let args = parse("in out.heics -q 85").unwrap().unwrap();
        write_sequence(&bframes(), &out, &args).unwrap();
        let bytes = std::fs::read(&out).unwrap();
        let seq = heifer::read_sequence(&bytes).unwrap().unwrap();
        assert_eq!((seq.width, seq.height, seq.frames.len()), (128, 96, 24));
        assert!(seq.frames.iter().all(|f| (f.duration - 0.04).abs() < 1e-9));
        // One keyframe every 30 frames: the first of 24; P pictures after it.
        let keys: Vec<usize> = (0..24).filter(|&i| seq.frames[i].keyframe).collect();
        assert_eq!(keys, [0]);
        // The brightness of the test stream grows with the display index: still in order.
        let means: Vec<f64> = heifer::decode_sequence(&bytes)
            .unwrap()
            .map(|f| {
                let f = f.unwrap();
                f.data.chunks_exact(4).map(|p| f64::from(p[1])).sum::<f64>() / f64::from(128 * 96)
            })
            .collect();
        assert!(means.windows(2).all(|m| m[1] > m[0] + 3.0), "{means:?}");
        // No overwrite without --force; a still image is not an animation.
        assert!(
            write_sequence(&bframes(), &out, &args)
                .unwrap_err()
                .contains("exists")
        );
        let still = fixture("single_image.heic");
        if still.exists() {
            let err = write_sequence(&still, &dir.join("b.heics"), &args).unwrap_err();
            assert!(err.contains("not an animation"), "{err}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn transparent_animations_keep_alpha_and_max_size() {
        use image::{Delay, Frame, RgbaImage};
        let dir = temp_dir("heics-alpha");
        let apng = dir.join("alpha.apng");
        let (w, h) = (96u32, 64u32);
        let mut frames = Vec::new();
        for n in 0..4u8 {
            let img = RgbaImage::from_fn(w, h, |x, y| {
                let a = if x < w / 2 { 80 + n * 40 } else { 255 };
                let r = (128.0 + 90.0 * (f64::from(x + u32::from(n) * 4) / 7.0).sin()) as u8;
                let g = (128.0 + 90.0 * (f64::from(y + u32::from(n) * 3) / 5.0).cos()) as u8;
                image::Rgba([r, g, ((x * 3 + y * 5) % 250) as u8, a])
            });
            frames.push(Frame::from_parts(
                img,
                0,
                0,
                Delay::from_numer_denom_ms(50, 1),
            ));
        }
        {
            let f = std::fs::File::create(&apng).unwrap();
            let mut w_png = png::Encoder::new(f, w, h);
            w_png.set_color(png::ColorType::Rgba);
            w_png.set_depth(png::BitDepth::Eight);
            w_png.set_animated(4, 0).unwrap();
            w_png.set_frame_delay(50, 1000).unwrap();
            let mut writer = w_png.write_header().unwrap();
            for frame in &frames {
                writer.write_image_data(frame.buffer().as_raw()).unwrap();
            }
        }
        let out = dir.join("out.heics");
        let args = parse("in out.heics -q 95").unwrap().unwrap();
        write_sequence(&apng, &out, &args).unwrap();
        let bytes = std::fs::read(&out).unwrap();
        let decoded: Vec<heifer::Image> = heifer::decode_sequence(&bytes)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(decoded.len(), 4);
        assert!(decoded.iter().all(|img| img.has_alpha));
        assert!(decoded[0].data[3] < 130);
        assert_eq!(decoded[0].data[(w as usize / 2 * 4) + 3], 255);

        let small = dir.join("small.heics");
        let budget = bytes.len() * 3 / 4;
        let args = Args {
            max_size: Some(budget),
            ..parse("in small.heics").unwrap().unwrap()
        };
        write_sequence(&apng, &small, &args).unwrap();
        assert!(std::fs::read(&small).unwrap().len() <= budget);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sequence_cli_options_parse_and_validate() {
        let a = parse("f1.png f2.png f3.png out.heics --speed fast --keyframe-interval 12 --b-frames 3 --fps 30 --format 444")
            .unwrap()
            .unwrap();
        assert_eq!(
            a.inputs,
            [
                PathBuf::from("f1.png"),
                PathBuf::from("f2.png"),
                PathBuf::from("f3.png")
            ]
        );
        assert_eq!(a.output, Some(PathBuf::from("out.heics")));
        assert_eq!(a.speed, Some(EncodeSpeed::Fast));
        assert_eq!(a.keyframe_interval, Some(12));
        assert_eq!(a.b_frames, Some(3));
        assert_eq!(a.fps, Some(30.0));
        assert_eq!(a.format, Some(EncodeFormat::Ycbcr444));

        assert!(parse("a.png b.png out.heics --speed turbo").is_err());
        assert!(parse("a.png b.png out.heics --keyframe-interval 0").is_err());
        assert!(parse("a.png b.png out.heics --b-frames -1").is_err());
        assert!(parse("a.png b.png out.heics --fps 0").is_err());
        assert!(parse("a.png b.png out.heic").is_err());
        assert!(check_heic_only(&a, Path::new("out.heic")).is_err());
        let speed_only = parse("a.png b.jpg --speed slow").unwrap().unwrap();
        assert!(check_heic_only(&speed_only, Path::new("b.jpg")).is_err());
    }

    #[test]
    fn still_images_combine_into_heics_with_formats_and_options() {
        use image::{ImageBuffer, Rgb, RgbImage};
        let dir = temp_dir("heics-multi");
        let (w, h) = (48u32, 32u32);
        let mut paths = Vec::new();
        let mut expected_rgb = Vec::new();
        for n in 0..4u8 {
            let p = dir.join(format!("frame-{n}.png"));
            let img: RgbImage = ImageBuffer::from_fn(w, h, |x, y| {
                Rgb([
                    ((x * 5 + u32::from(n) * 9) % 256) as u8,
                    ((y * 7 + u32::from(n) * 6) % 256) as u8,
                    (((x ^ y) * 11) % 256) as u8,
                ])
            });
            img.save(&p).unwrap();
            expected_rgb.push(img.into_raw());
            paths.push(p);
        }
        let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();

        // Lossless 4:4:4 GBR with hierarchical options and custom FPS / keyframe interval.
        let out_lossless = dir.join("lossless.heics");
        let args = parse(
            "a b out.heics --lossless --speed fast --keyframe-interval 2 --b-frames 0 --fps 20",
        )
        .unwrap()
        .unwrap();
        write_sequence_paths(&refs, &out_lossless, &args).unwrap();
        let bytes = std::fs::read(&out_lossless).unwrap();
        let seq = heifer::read_sequence(&bytes).unwrap().unwrap();
        assert_eq!(seq.frames.len(), 4);
        assert!(seq.frames.iter().all(|f| (f.duration - 0.05).abs() < 1e-3));
        assert_eq!(
            (0..4)
                .filter(|&i| seq.frames[i].keyframe)
                .collect::<Vec<_>>(),
            [0, 2]
        );
        let decoded: Vec<heifer::Image> = heifer::decode_sequence(&bytes)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for (idx, (exp, dec)) in expected_rgb.iter().zip(&decoded).enumerate() {
            let rgb: Vec<u8> = dec
                .to_rgba8()
                .chunks_exact(4)
                .flat_map(|p| [p[0], p[1], p[2]])
                .collect();
            assert_eq!(&rgb, exp, "lossless multi-input frame {idx} differed");
        }

        // Lossy 4:2:2 with B-frames works too.
        let out_422 = dir.join("y422.heics");
        let args_422 = parse("a b out.heics --format 422 --b-frames 3 --speed medium -q 85")
            .unwrap()
            .unwrap();
        write_sequence_paths(&refs, &out_422, &args_422).unwrap();
        let seq_422 = heifer::read_sequence(&std::fs::read(&out_422).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(seq_422.frames.len(), 4);

        std::fs::remove_dir_all(dir).unwrap();
    }
}
