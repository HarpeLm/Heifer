#!/usr/bin/env python3
"""Benchmarks heifer against ffmpeg and draws the README charts (docs/images/*.svg).

Run on an idle machine (no fuzzing, no build):
    ./scripts/fetch-real.sh
    cargo build --release -p heifer --example bench
    ./scripts/bench.py
Results are also written to docs/benchmark.json.
"""
import json, pathlib, platform, re, statistics, subprocess, sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
BENCH = ROOT / "target/release/examples/bench"
REAL = ROOT / "tests/fixtures/real"
OUT = ROOT / "docs/images"
FILES = [
    ("Xiaomi, 5 MP", "heif_special__xiaomi.heic"),
    ("iPhone 13 Pro, 12 MP", "heif_other__pug.heic"),
    ("8000×6000, 48 MP", "benchmarks__image_large.heic"),
    ("Galaxy S24 Ultra, 200 MP", "heif_special__200MP.heic"),
]
RUNS = 5


def heifer_ms(path, threads=0, runs=RUNS):
    out = subprocess.run([str(BENCH), str(path), str(runs), str(threads)], capture_output=True, text=True, check=True)
    return float(out.stdout.strip())


def ffmpeg_ms(path, runs=RUNS):
    times = []
    for _ in range(runs):
        r = subprocess.run(["ffmpeg", "-hide_banner", "-benchmark", "-threads", "0", "-i", str(path),
                            "-frames:v", "1", "-f", "null", "-"], capture_output=True, text=True)
        m = re.search(r"rtime=([0-9.]+)s", r.stderr)
        if r.returncode or not m:
            return None
        times.append(float(m.group(1)) * 1000)
    return statistics.median(times)


THEMES = {
    "light": {"bg": "#ffffff", "fg": "#1f2328", "muted": "#59636e", "grid": "#d1d9e0",
              "series": ["#2f6fb3", "#9cbbe0", "#c2803a"]},
    "dark": {"bg": "#0d1117", "fg": "#e6edf3", "muted": "#9198a1", "grid": "#30363d",
             "series": ["#5b9be0", "#2f5784", "#d39a5a"]},
}


def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;")


def bar_chart(name, title, subtitle, groups, series, unit, log=False):
    """Horizontal grouped bars. groups: [(label, [value or None per series])]."""
    import math
    width, left, right, bar, gap = 760, 190, 70, 14, 26
    height = 92 + len(groups) * (len(series) * bar + gap) + 30
    values = [v for _, vs in groups for v in vs if v]
    vmax = max(values)
    scale = (lambda v: math.log10(v) / math.log10(vmax * 1.15)) if log else (lambda v: v / (vmax * 1.08))
    for theme, c in THEMES.items():
        s = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" '
             f'font-family="-apple-system,BlinkMacSystemFont,Segoe UI,Helvetica,Arial,sans-serif">',
             f'<rect width="100%" height="100%" rx="8" fill="{c["bg"]}"/>',
             f'<text x="24" y="34" font-size="17" font-weight="600" fill="{c["fg"]}">{esc(title)}</text>',
             f'<text x="24" y="56" font-size="12.5" fill="{c["muted"]}">{esc(subtitle)}</text>']
        x0, y = left, 84
        for i, (label, _) in enumerate(series):
            lx = 24 + i * 190
            s.append(f'<rect x="{lx}" y="{height-22}" width="11" height="11" rx="2" fill="{c["series"][i]}"/>'
                     f'<text x="{lx+17}" y="{height-12.5}" font-size="12" fill="{c["muted"]}">{esc(label)}</text>')
        span = width - left - right
        for label, vs in groups:
            gh = len(series) * bar
            s.append(f'<text x="{left-12}" y="{y + gh/2 + 4}" font-size="13" text-anchor="end" fill="{c["fg"]}">{esc(label)}</text>')
            for i, v in enumerate(vs):
                by = y + i * bar
                if v is None:
                    s.append(f'<text x="{x0+4}" y="{by+11}" font-size="11.5" fill="{c["muted"]}">fails to decode</text>')
                    continue
                w = max(2, span * scale(v))
                s.append(f'<rect x="{x0}" y="{by+1}" width="{w:.1f}" height="{bar-3}" rx="2" fill="{c["series"][i]}"/>'
                         f'<text x="{x0+w+6:.1f}" y="{by+11}" font-size="11.5" fill="{c["muted"]}">{v:,.0f} {unit}</text>')
            y += gh + gap
        s.append("</svg>")
        (OUT / f"{name}-{theme}.svg").write_text("\n".join(s))


def stacked_chart(name, title, subtitle, parts):
    """One horizontal stacked bar. parts: [(label, count, series index)]."""
    width, height, x0, span = 760, 172, 24, 712
    total = sum(n for _, n, _ in parts)
    for theme, c in THEMES.items():
        colors = c["series"] + [c["grid"]]
        s = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" '
             f'font-family="-apple-system,BlinkMacSystemFont,Segoe UI,Helvetica,Arial,sans-serif">',
             f'<rect width="100%" height="100%" rx="8" fill="{c["bg"]}"/>',
             f'<text x="24" y="34" font-size="17" font-weight="600" fill="{c["fg"]}">{esc(title)}</text>',
             f'<text x="24" y="56" font-size="12.5" fill="{c["muted"]}">{esc(subtitle)}</text>']
        x = x0
        for label, n, ci in parts:
            w = span * n / total
            s.append(f'<rect x="{x:.1f}" y="74" width="{w-2:.1f}" height="26" rx="3" fill="{colors[ci]}"/>')
            x += w
        # Legend on a 2-column grid.
        for k, (label, n, ci) in enumerate(parts):
            lx, ly = 24 + (k % 2) * 370, 118 + (k // 2) * 22
            s.append(f'<rect x="{lx}" y="{ly}" width="11" height="11" rx="2" fill="{colors[ci]}"/>'
                     f'<text x="{lx+17}" y="{ly+9.5}" font-size="12" fill="{c["muted"]}">{esc(f"{n} {label}")}</text>')
        s.append("</svg>")
        (OUT / f"{name}-{theme}.svg").write_text("\n".join(s))


def conformance():
    stacked_chart("conformance", "HEVC conformance: 66 official bitstreams (ITU-T H.265.1)",
                  "First picture of each stream, compared with ffmpeg and with the reference decoder's picture hashes",
                  [("identical to ffmpeg and hash-verified", 38, 0), ("identical to ffmpeg", 24, 1),
                   ("hash-verified, ffmpeg cannot decode", 3, 2), ("unsupported (16-bit tools)", 1, 3)])


def benchmarks():
    if not BENCH.exists():
        sys.exit("build first: cargo build --release -p heifer --example bench")
    rows, results = [], {}
    for label, f in FILES:
        p = REAL / f
        if not p.exists():
            sys.exit(f"missing {p}: run scripts/fetch-real.sh")
        h, h1, ff = heifer_ms(p), heifer_ms(p, 1, 3), ffmpeg_ms(p)
        print(f"{label:28} heifer {h:8.1f} ms   1 thread {h1:8.1f} ms   ffmpeg {ff if ff else 'fails'}")
        rows.append((label, [h, h1, ff]))
        results[label] = {"heifer_ms": h, "heifer_1_thread_ms": h1, "ffmpeg_ms": ff}
    scaling = {n: heifer_ms(REAL / "heif_other__pug.heic", n) for n in [1, 2, 4, 6, 8, 10]}
    print("thread scaling (iPhone 12 MP):", {k: round(v) for k, v in scaling.items()})
    cpu = subprocess.run(["sysctl", "-n", "machdep.cpu.brand_string"], capture_output=True, text=True).stdout.strip() or platform.processor()
    cores = subprocess.run(["sysctl", "-n", "hw.ncpu"], capture_output=True, text=True).stdout.strip()
    machine = f"{cpu}, {cores} cores"
    bar_chart("bench-decode", "Decoding time: heifer vs ffmpeg (lower is better)",
              f"Median of {RUNS} runs, full decode of the primary image · {machine}", rows,
              [("heifer, all cores", 0), ("heifer, 1 thread", 1), ("ffmpeg (libavcodec)", 2)], "ms", log=True)
    base = scaling[1]
    bar_chart("bench-scaling", "Parallel grid decoding: iPhone photo, 48 tiles",
              "Time to decode the full 4032×3024 photo with a given number of threads (lower is better)",
              [(f"{n} thread{'s' if n > 1 else ''}", [v]) for n, v in scaling.items()],
              [(f"heifer (×{base / scaling[10]:.1f} faster with 10 threads)", 0)], "ms")
    (ROOT / "docs/benchmark.json").write_text(json.dumps({"machine": machine, "files": results, "scaling": scaling}, indent=2))


if __name__ == "__main__":
    conformance()
    if "--conformance-only" not in sys.argv:
        benchmarks()
    print("charts written to docs/images/")
