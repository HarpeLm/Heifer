#!/usr/bin/env python3
"""Measures heifer against other HEIC decoders and encoders and writes docs/BENCHMARKS.md.

Runs on macOS (Apple's ImageIO, through `sips`, is the reference decoder and one of the
encoders), with ffmpeg (with libx265) and Python 3 with numpy:

    python3 benchmarks/run.py

The test images come from the system (the Portrait backgrounds of macOS) or are generated, so
the measurements can be reproduced on any Mac.
"""
import datetime, glob, json, os, platform, re, shutil, statistics, subprocess, sys
import numpy as np

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BENCH = os.path.join(ROOT, "benchmarks")
WORK = os.path.join(BENCH, "work")
HARNESS = os.path.join(BENCH, "target/release/heifer-benchmarks")
RELEASE = os.path.join(ROOT, "target/release")
PORTRAIT = "/System/Library/PrivateFrameworks/Portrait.framework/Versions/A/Resources/backgrounds/image"


def run(cmd, env=None, check=False):
    return subprocess.run(cmd, capture_output=True, text=True, env=env, check=check)


def build():
    run(["cargo", "build", "--release", "-q", "-p", "heifer-cli", "-p", "heifer",
         "-p", "heifer-hevc-enc", "--examples", "--bins"], check=True)
    run(["cargo", "build", "--release", "-q", "--manifest-path", os.path.join(BENCH, "Cargo.toml")],
        check=True)


def prepare():
    """Test files in WORK. Returns (heic files to decode, png images to encode)."""
    os.makedirs(WORK, exist_ok=True)
    w = lambda name: os.path.join(WORK, name)
    heifer = os.path.join(RELEASE, "heifer")
    photos = sorted(glob.glob(f"{PORTRAIT}/*.heic"))
    if not photos:
        sys.exit("macOS Portrait backgrounds not found")
    if not os.path.exists(w("photo.png")):
        run(["sips", "-s", "format", "png", photos[1], "--out", w("photo.png")], check=True)
        run(["sips", "-z", "3024", "4032", w("photo.png"), "--out", w("photo12.png")], check=True)
        run(["sips", "-s", "format", "heic", "-s", "formatOptions", "80", w("photo12.png"),
             "--out", w("apple12.heic")], check=True)
        run(["ffmpeg", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=1280x720",
             "-frames:v", "1", w("graphics.png")], check=True)
        run(["ffmpeg", "-loglevel", "error", "-y", "-f", "lavfi", "-i",
             "testsrc2=size=1501x999,format=rgba,geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':"
             "a='if(lt(X,750),255,128*(1+sin(Y/40)))'", "-frames:v", "1", w("alpha.png")], check=True)
        run(["ffmpeg", "-loglevel", "error", "-y", "-f", "lavfi", "-i",
             "gradients=s=1600x900:c0=0x203050:c1=0xe0a070:n=2:seed=3", "-frames:v", "1",
             "-pix_fmt", "rgb48be", w("gradient16.png")], check=True)
        for src, out in [("photo12.png", "heifer12.heic"), ("alpha.png", "alpha.heic"),
                         ("gradient16.png", "gradient10.heic")]:
            run([heifer, w(src), w(out), "-f"], check=True)
    decode_set = photos + [w("apple12.heic"), w("heifer12.heic"), w("alpha.heic"), w("gradient10.heic")]
    encode_set = [w("photo.png"), w("graphics.png"), w("photo12.png")]
    return decode_set, encode_set


def apple_rgb(heic, size):
    png = heic + ".apple.png"
    run(["sips", "-s", "format", "png", heic, "--out", png])
    raw = run_bytes(["ffmpeg", "-loglevel", "error", "-i", png, "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
    a = np.frombuffer(raw, np.uint8)
    w, h = size
    return a.reshape(h, w, 3) if a.size == w * h * 3 else None


def run_bytes(cmd):
    return subprocess.run(cmd, capture_output=True).stdout


def psnr(a, b):
    mse = np.mean((a.astype(np.float64) - b.astype(np.float64)) ** 2)
    return 99.0 if mse == 0 else 10 * np.log10(255 ** 2 / mse)


def bench_decode(files, threads_all):
    """{decoder: {file: {"t1", "tall", "psnr", "err"}}}"""
    out_dir = os.path.join(WORK, "decoded")
    results = {}
    for f in files:
        per = {}
        for threads in (1, threads_all):
            env = dict(os.environ, RAYON_NUM_THREADS=str(threads))
            lines = run([HARNESS, "decode", f, str(threads), out_dir], env=env).stdout.splitlines()
            for line in lines:
                parts = line.split("\t")
                r = per.setdefault(parts[0], {})
                if parts[1] in ("ERR", "PANIC"):
                    r["err"] = parts[1] + (": " + parts[2] if len(parts) > 2 else "")
                else:
                    r["t1" if threads == 1 else "tall"] = float(parts[1])
                    r["size"] = tuple(map(int, parts[2].split("x")))
        sizes = [r["size"] for r in per.values() if "size" in r]
        ref = apple_rgb(f, sizes[0]) if sizes else None
        for name, r in per.items():
            if "size" in r and ref is not None and r["size"] == sizes[0]:
                px = np.fromfile(os.path.join(out_dir, f"{name}.rgba"), np.uint8)
                w, h = r["size"]
                if px.size == w * h * 4:
                    r["psnr"] = psnr(px.reshape(h, w, 4)[:, :, :3], ref)
            results.setdefault(name, {})[os.path.basename(f)] = r
    return results


def bd_rate(ref, test):
    """Average size difference (%) of `test` against `ref` at equal PSNR (Bjøntegaard)."""
    ref, test = sorted(ref, key=lambda p: p[1]), sorted(test, key=lambda p: p[1])
    r1, p1 = np.log([p[0] for p in ref]), [p[1] for p in ref]
    r2, p2 = np.log([p[0] for p in test]), [p[1] for p in test]
    f1, f2 = np.polyfit(p1, r1, 3), np.polyfit(p2, r2, 3)
    lo, hi = max(min(p1), min(p2)), min(max(p1), max(p2))
    if hi <= lo:
        return None
    i1 = np.polyval(np.polyint(f1), hi) - np.polyval(np.polyint(f1), lo)
    i2 = np.polyval(np.polyint(f2), hi) - np.polyval(np.polyint(f2), lo)
    return (np.exp((i2 - i1) / (hi - lo)) - 1) * 100


def rgb_psnr(src, png):
    out = run(["ffmpeg", "-hide_banner", "-i", src, "-i", png, "-lavfi",
               "[0]format=rgb24[a];[1]format=rgb24[b];[a][b]psnr", "-f", "null", "-"]).stderr
    m = re.search(r"average:([\d.]+)", out)
    return float(m.group(1)) if m else None


def bench_encode(images):
    """{image: {encoder: {"points": [(size, psnr_heifer, psnr_apple)], "ms", "ms1"}}}"""
    heic2png = os.path.join(RELEASE, "examples/heic2png")
    results = {}
    for src in images:
        name = os.path.splitext(os.path.basename(src))[0]
        out_dir = os.path.join(WORK, "encoded", name)
        shutil.rmtree(out_dir, ignore_errors=True)
        os.makedirs(out_dir)
        files = []  # (encoder, path, ms, ms1)
        for line in run([HARNESS, "encode", src, out_dir]).stdout.splitlines():
            parts = line.split("\t")
            if len(parts) == 6:
                files.append((parts[0], parts[5], float(parts[3]),
                              float(parts[4]) if parts[4] != "-" else None))
        for q in (30, 50, 70, 85, 95):
            path = os.path.join(out_dir, f"apple_q{q}.heic")
            t = datetime.datetime.now()
            run(["sips", "-s", "format", "heic", "-s", "formatOptions", str(q), src, "--out", path])
            files.append(("apple", path, (datetime.datetime.now() - t).total_seconds() * 1000, None))
        per = {}
        for enc, path, ms, ms1 in files:
            run([heic2png, path, path + ".heifer.png"])
            run(["sips", "-s", "format", "png", path, "--out", path + ".apple.png"])
            point = (os.path.getsize(path), rgb_psnr(src, path + ".heifer.png"),
                     rgb_psnr(src, path + ".apple.png"))
            e = per.setdefault(enc, {"points": [], "ms": [], "ms1": []})
            e["points"].append(point)
            e["ms"].append(ms)
            if ms1 is not None:
                e["ms1"].append(ms1)
        results[name] = per
    return results


def bench_x265(image):
    """BD-rate of heifer's HEVC encoder vs x265 presets, on YUV (PSNR 6:1:1), if available."""
    if "libx265" not in run(["ffmpeg", "-hide_banner", "-encoders"]).stdout:
        return None
    yuv = os.path.join(WORK, "x265.yuv")
    probe = run(["ffprobe", "-v", "error", "-show_entries", "stream=width,height", "-of", "csv=p=0", image])
    w, h = map(int, probe.stdout.strip().split(","))
    w, h = w - w % 2, h - h % 2
    run(["ffmpeg", "-loglevel", "error", "-y", "-i", image, "-vf", f"crop={w}:{h}:0:0",
         "-pix_fmt", "yuv420p", "-f", "rawvideo", yuv], check=True)

    def yuv_psnr(stream):
        out = run(["ffmpeg", "-hide_banner", "-f", "rawvideo", "-s", f"{w}x{h}", "-pix_fmt", "yuv420p",
                   "-i", yuv, "-i", stream, "-lavfi", "psnr", "-f", "null", "-"]).stderr
        y, u, v = map(float, re.search(r"y:([\d.]+) u:([\d.]+) v:([\d.]+)", out).groups())
        return (6 * y + u + v) / 8

    stream = os.path.join(WORK, "x.hevc")
    heifer = []
    for qp in (22, 27, 32, 37):
        run([os.path.join(RELEASE, "examples/encode_yuv"), yuv, str(w), str(h), str(qp), stream], check=True)
        heifer.append((os.path.getsize(stream), yuv_psnr(stream)))
    out = {}
    for preset in ("ultrafast", "medium", "slow"):
        pts = []
        for crf in (16, 20, 24, 28, 32, 36):
            run(["ffmpeg", "-loglevel", "error", "-y", "-f", "rawvideo", "-s", f"{w}x{h}", "-pix_fmt",
                 "yuv420p", "-i", yuv, "-c:v", "libx265", "-preset", preset, "-x265-params",
                 f"crf={crf}:keyint=1:log-level=0:info=0", "-frames:v", "1", stream], check=True)
            pts.append((os.path.getsize(stream), yuv_psnr(stream)))
        out[preset] = bd_rate(pts, heifer)
    return out


def versions():
    lock = open(os.path.join(BENCH, "Cargo.lock")).read()
    v = {}
    for name in ("heic-rs", "heic", "heic-decoder", "zune-heic", "still265"):
        m = re.search(rf'name = "{name}"\nversion = "([^"]+)"', lock)
        v[name] = m.group(1) if m else "?"
    heifer = re.search(r'version = "([^"]+)"', open(os.path.join(ROOT, "Cargo.toml")).read()).group(1)
    ffmpeg = run(["ffmpeg", "-version"]).stdout.split("\n")[0].split(" ")[2]
    return heifer, v, ffmpeg


REAL_FILES = [
    ("Xiaomi, 5 MP", "heif_special__xiaomi.heic"),
    ("iPhone 13 Pro, 12 MP", "heif_other__pug.heic"),
    ("8000×6000, 48 MP", "benchmarks__image_large.heic"),
    ("Galaxy S24 Ultra, 200 MP", "heif_special__200MP.heic"),
]

BENCH_EX = os.path.join(RELEASE, "examples/bench")
HEIFER_CLI = os.path.join(RELEASE, "heifer")


def tool_version(args, pattern):
    out = run(args).stdout + run(args).stderr
    m = re.search(pattern, out)
    return m.group(1) if m else "?"


def median_time(cmd, out_path=None, runs=5):
    """Median wall time of `cmd`, removing `out_path` before each run."""
    times = []
    for _ in range(runs):
        if out_path and os.path.exists(out_path):
            os.remove(out_path)
        t = datetime.datetime.now()
        if run(cmd).returncode:
            return None
        times.append((datetime.datetime.now() - t).total_seconds() * 1000)
    return statistics.median(times)


def bench_libheif(image, threads):
    """heifer against libheif (heif-convert / heif-enc), on real photos and on `image`."""
    if not (shutil.which("heif-convert") and shutil.which("heif-enc")):
        return None
    out = os.path.join(WORK, "libheif")
    os.makedirs(out, exist_ok=True)
    version = {
        "libheif": tool_version(["heif-convert", "--version"], r"(\d+\.\d+\.\d+)"),
        "libde265": tool_version(["heif-convert", "--list-decoders"], r"libde265.*?version (\d+\.\d+\.\d+)"),
        "x265": tool_version(["heif-enc", "--list-encoders"], r"x265.*?\((\d+\.\d+)"),
    }
    decode = []
    for label, name in REAL_FILES:
        src = os.path.join(ROOT, "tests/fixtures/real", name)
        if not os.path.exists(src):
            return None
        apple = os.path.join(out, name + ".apple.png")
        run(["sips", "-s", "format", "png", src, "--out", apple])
        heifer_png = os.path.join(out, name + ".heifer.png")
        libheif_png = os.path.join(out, name + ".libheif.png")
        pure = run([BENCH_EX, src, "5", str(threads)]).stdout.strip()
        row = {
            "label": label,
            "heifer_ms": float(pure) if pure else None,
            "heifer_cli_ms": median_time([HEIFER_CLI, src, heifer_png, "-f"], heifer_png),
            "libheif_cli_ms": median_time(["heif-convert", "--png-compression-level", "1", src,
                                          libheif_png], libheif_png),
            "heifer_psnr": rgb_psnr(apple, heifer_png) if os.path.exists(heifer_png) else None,
            "libheif_psnr": rgb_psnr(apple, libheif_png) if os.path.exists(libheif_png) else None,
        }
        print(f"libheif decode {label:26} heifer {row['heifer_ms']} ms, "
              f"CLI {row['heifer_cli_ms']} / {row['libheif_cli_ms']} ms")
        decode.append(row)

    encode_dir = os.path.join(out, "encode")
    os.makedirs(encode_dir, exist_ok=True)

    def encode_point(encoder, q):
        heic = os.path.join(encode_dir, f"{encoder}_q{q}.heic")
        png = heic + ".png"
        if encoder == "heifer":
            ms = median_time([HEIFER_CLI, image, heic, "-q", str(q), "-f"], heic, 3)
        else:
            ms = median_time(["heif-enc", image, "-q", str(q), "-o", heic], heic, 3)
        if ms is None or not os.path.exists(heic):
            return None
        if os.path.exists(png):
            os.remove(png)
        run([HEIFER_CLI, heic, png, "-f"])
        p = rgb_psnr(image, png) if os.path.exists(png) else None
        return {"q": q, "size": os.path.getsize(heic), "psnr": p, "ms": ms}

    heifer_points = [p for q in (40, 60, 75, 85, 95) if (p := encode_point("heifer", q))]
    libheif_points = [p for q in (30, 40, 50, 60, 70, 80) if (p := encode_point("libheif", q))]
    bd = None
    if len(heifer_points) >= 3 and len(libheif_points) >= 3:
        bd = bd_rate([(p["size"], p["psnr"]) for p in heifer_points],
                     [(p["size"], p["psnr"]) for p in libheif_points])
    print(f"libheif encode: BD-rate libheif vs heifer {bd:+.1f}%" if bd is not None else "libheif encode: no overlap")
    return {"version": version, "decode": decode, "image": os.path.basename(image),
            "encode": {"heifer": heifer_points, "libheif": libheif_points, "bd_rate": bd}}


def machine():
    chip = run(["sysctl", "-n", "machdep.cpu.brand_string"]).stdout.strip()
    p = run(["sysctl", "-n", "hw.perflevel0.physicalcpu"]).stdout.strip()
    e = run(["sysctl", "-n", "hw.perflevel1.physicalcpu"]).stdout.strip()
    cores = f"{p} performance + {e} efficiency cores" if e else f"{p} cores"
    return f"{chip} ({cores}), macOS {platform.mac_ver()[0]}", int(p or 1) + int(e or 0)


def fmt_ms(v):
    return f"{v:.0f} ms" if v is not None else "—"


def write(decode, encode, x265, heifer_version, vers, ffmpeg_version, machine_desc, threads, n_files,
          libheif=None):
    L = []
    L.append("# Benchmarks\n")
    L.append(f"heifer {heifer_version} against the other HEIC decoders and encoders, measured "
             f"in {datetime.date.today():%B %Y}, on an {machine_desc}. Reproduce with "
             "`python3 benchmarks/run.py` (macOS, ffmpeg with libx265, numpy).\n")
    L.append("Competitors: " + ", ".join(f"`{k}` {v}" for k, v in vers.items()) +
             f", ffmpeg {ffmpeg_version}, and Apple's ImageIO (`sips`, hardware HEVC).\n")
    L.append("Test files: the 9 Portrait backgrounds of macOS (Apple photos, 1920×1080), a 12 MP "
             "image encoded by Apple and by heifer, an image with transparency and a 10-bit "
             "gradient (generated by the script).\n")

    L.append("## Decoding\n")
    L.append("Fidelity: PSNR of the 8-bit RGB output against Apple's decoder (what Apple's apps "
             f"show). Times: median of 5 runs, summed over the {n_files} files.\n")
    L.append(f"| Decoder | Files decoded | 1 thread | {threads} threads | Fidelity vs Apple (median / worst) |")
    L.append("|---|---|---|---|---|")
    for name, per in decode.items():
        ok = [r for r in per.values() if "t1" in r and "err" not in r]
        ps = sorted(r["psnr"] for r in ok if r.get("psnr") is not None)
        fid = f"{statistics.median(ps):.1f} / {ps[0]:.1f} dB" if ps else "—"
        t1 = sum(r["t1"] for r in ok)
        ta = sum(r.get("tall", r["t1"]) for r in ok)
        L.append(f"| `{name}` | {len(ok)}/{n_files} | {fmt_ms(t1)} | {fmt_ms(ta)} | {fid} |")
    L.append("")
    L.append("12 MP photo encoded by Apple (48 tiles):\n")
    L.append(f"| Decoder | 1 thread | {threads} threads | Fidelity vs Apple |")
    L.append("|---|---|---|---|")
    for name, per in decode.items():
        r = per.get("apple12.heic", {})
        if "err" in r or "t1" not in r:
            L.append(f"| `{name}` | fails | | |")
        else:
            p = f"{r['psnr']:.1f} dB" if "psnr" in r else "—"
            L.append(f"| `{name}` | {fmt_ms(r['t1'])} | {fmt_ms(r.get('tall'))} | {p} |")
    L.append("")
    failures = [(n, f, r["err"]) for n, per in decode.items() for f, r in per.items() if "err" in r]
    if failures:
        L.append("Failures: " + "; ".join(f"`{n}` on {f} ({e[:60]})" for n, f, e in failures) + ".\n")
    L.append("`zune-heic+hw` decodes with Apple's hardware decoder (VideoToolbox) and uses `unsafe` "
             "code; the others are software decoders.\n")

    L.append("## Encoding\n")
    L.append("All encoders produce 4:2:0, except `still265-slow-444` (4:4:4, which hardware "
             "decoders of phones usually do not support). BD-rate: average size difference at equal "
             "quality, against heifer (negative: smaller files than heifer). Quality is the PSNR of "
             "the decoded image against the source, decoded by heifer (which reads every file "
             "correctly) and by Apple (what Apple's apps show).\n")
    for image, per in encode.items():
        ref_h = [(s, ph) for s, ph, _ in per["heifer"]["points"]]
        ref_a = [(s, pa) for s, _, pa in per["heifer"]["points"]]
        L.append(f"### {image}\n")
        L.append("| Encoder | BD-rate (decoded by heifer) | BD-rate (decoded by Apple) | Time per file | 1 thread |")
        L.append("|---|---|---|---|---|")
        for enc, e in per.items():
            if enc == "heifer":
                bh = ba = "reference"
            else:
                vh = bd_rate(ref_h, [(s, ph) for s, ph, _ in e["points"] if ph])
                va = bd_rate(ref_a, [(s, pa) for s, _, pa in e["points"] if pa])
                # Files that Apple decodes much worse than heifer: colours misread by Apple.
                best_h = max(ph for _, ph, _ in e["points"] if ph)
                best_a = max((pa for _, _, pa in e["points"] if pa), default=0)
                bh = f"{vh:+.1f}%" if vh is not None else "no overlap"
                ba = ("wrong colours" if best_a < best_h - 3 else
                      f"{va:+.1f}%" if va is not None else "no overlap")
            one = fmt_ms(statistics.mean(e["ms1"])) if e["ms1"] else ("same" if enc.startswith("still") else "—")
            note = " (hardware)" if enc == "apple" else ""
            L.append(f"| `{enc}`{note} | {bh} | {ba} | {fmt_ms(statistics.mean(e['ms']))} | {one} |")
        L.append("")
    L.append("still265 writes a `colr` box that Apple's decoder and ffmpeg do not honour: its files "
             "show wrong colours in Apple's apps (\"wrong colours\" above). still265 runs on one "
             "thread; heifer encodes tiles on all cores.\n")
    if x265:
        L.append("### HEVC encoder vs x265\n")
        L.append("BD-rate of heifer's HEVC encoder against x265 (PSNR 6:1:1 on YUV, photo):\n")
        L.append("| x265 preset | heifer vs x265 |")
        L.append("|---|---|")
        for preset, v in x265.items():
            L.append(f"| `{preset}` | {v:+.1f}% |")
        L.append("")
    if libheif:
        v = libheif["version"]
        L.append("## Against libheif\n")
        L.append(f"libheif {v['libheif']}, with libde265 {v['libde265']} for decoding and x265 "
                 f"{v['x265']} for encoding, through `heif-convert` and `heif-enc` (Homebrew "
                 "builds). Same machine and files.\n")
        L.append("Decoding the four real photos of `tests/fixtures/real`: time of the whole "
                 "command, PNG output included, median of 5 runs. heifer uses the PNG encoder of "
                 "the `image` crate; libheif is given `--png-compression-level 1`, close to its "
                 "speed (its default level is about four times slower here). PSNR against "
                 "Apple's decoder.\n")
        L.append("| File | heifer decode | heifer → PNG | libheif → PNG | PSNR heifer | PSNR libheif |")
        L.append("|---|---|---|---|---|---|")
        for r in libheif["decode"]:
            psnr = lambda p: f"{p:.1f} dB" if p is not None else "—"
            L.append(f"| {r['label']} | {fmt_ms(r['heifer_ms'])} | {fmt_ms(r['heifer_cli_ms'])} | "
                     f"{fmt_ms(r['libheif_cli_ms'])} | {psnr(r['heifer_psnr'])} | {psnr(r['libheif_psnr'])} |")
        L.append("")
        enc = libheif["encode"]
        L.append(f"Encoding `{libheif['image']}`, median of 3 runs each. PSNR against the source, "
                 "decoded by heifer.\n")
        L.append("| Encoder | Quality | Size | PSNR | Time |")
        L.append("|---|---|---|---|---|")
        for name, points in (("heifer", enc["heifer"]), ("libheif x265", enc["libheif"])):
            for p in points:
                L.append(f"| {name} | {p['q']} | {p['size'] / 1000:.0f} KB | {p['psnr']:.1f} dB | "
                         f"{p['ms']:.0f} ms |")
        L.append("")
        if enc["bd_rate"] is not None:
            bigger = "larger" if enc["bd_rate"] >= 0 else "smaller"
            L.append(f"At equal PSNR, libheif's files are {abs(enc['bd_rate']):.1f}% {bigger} than "
                     "heifer's (Bjøntegaard, size difference at matched quality).\n")
    path = os.path.join(ROOT, "docs/BENCHMARKS.md")
    open(path, "w").write("\n".join(L))
    return path


def main():
    if sys.argv[1:] == ["--page-only"]:
        # Rewrite the page from the last measurements.
        r = json.load(open(os.path.join(WORK, "results.json")))
        decode_set, _ = prepare()
        machine_desc, threads = machine()
        heifer_version, vers, ffmpeg_version = versions()
        print(write(r["decode"], r["encode"], r["x265"], heifer_version, vers, ffmpeg_version,
                    machine_desc, threads, len(decode_set), r.get("libheif")))
        return
    build()
    decode_set, encode_set = prepare()
    machine_desc, threads = machine()
    decode = bench_decode(decode_set, threads)
    encode = bench_encode(encode_set)
    x265 = bench_x265(encode_set[0])
    libheif = bench_libheif(encode_set[2], threads)
    heifer_version, vers, ffmpeg_version = versions()
    json.dump({"decode": decode, "encode": encode, "x265": x265, "libheif": libheif},
              open(os.path.join(WORK, "results.json"), "w"), indent=1, default=str)
    print(write(decode, encode, x265, heifer_version, vers, ffmpeg_version, machine_desc,
                threads, len(decode_set), libheif))


if __name__ == "__main__":
    main()
