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


def machine():
    chip = run(["sysctl", "-n", "machdep.cpu.brand_string"]).stdout.strip()
    p = run(["sysctl", "-n", "hw.perflevel0.physicalcpu"]).stdout.strip()
    e = run(["sysctl", "-n", "hw.perflevel1.physicalcpu"]).stdout.strip()
    cores = f"{p} performance + {e} efficiency cores" if e else f"{p} cores"
    return f"{chip} ({cores}), macOS {platform.mac_ver()[0]}", int(p or 1) + int(e or 0)


def fmt_ms(v):
    return f"{v:.0f} ms" if v is not None else "—"


def write(decode, encode, x265, heifer_version, vers, ffmpeg_version, machine_desc, threads, n_files):
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
                    machine_desc, threads, len(decode_set)))
        return
    build()
    decode_set, encode_set = prepare()
    machine_desc, threads = machine()
    decode = bench_decode(decode_set, threads)
    encode = bench_encode(encode_set)
    x265 = bench_x265(encode_set[0])
    heifer_version, vers, ffmpeg_version = versions()
    json.dump({"decode": decode, "encode": encode, "x265": x265},
              open(os.path.join(WORK, "results.json"), "w"), indent=1, default=str)
    print(write(decode, encode, x265, heifer_version, vers, ffmpeg_version, machine_desc,
                threads, len(decode_set)))


if __name__ == "__main__":
    main()
