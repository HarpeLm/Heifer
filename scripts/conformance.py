#!/usr/bin/env python3
"""Decodes the first picture of each HEVC conformance bitstream with heifer and ffmpeg and
compares them byte for byte. Run scripts/fetch-conformance.sh first.

Usage:
    cargo build --release -p heifer-hevc-dec --example decode
    scripts/conformance.py [filter]
"""
import pathlib, subprocess, sys, tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
CONF = ROOT / "tests" / "conformance"
DECODE = ROOT / "target" / "release" / "examples" / "decode"
EXTS = {".bit", ".bin", ".bitstream", ".hevc", ".265", ".h265"}

if not DECODE.exists():
    sys.exit("build first: cargo build --release -p heifer-hevc-dec --example decode")
flt = sys.argv[1] if len(sys.argv) > 1 else ""
results = {"IDENTIQUE": 0, "DIFFERENT": 0, "ERREUR heifer": 0, "ignore": 0}
tmp = pathlib.Path(tempfile.mkdtemp())
for d in sorted(p for p in CONF.iterdir() if p.is_dir() and flt in p.name):
    streams = sorted((f for f in d.rglob("*") if f.suffix.lower() in EXTS), key=lambda f: -f.stat().st_size)
    if not streams:
        print(f"{d.name:58} ignore (pas de flux trouve)"); results["ignore"] += 1; continue
    s = streams[0]
    probe = subprocess.run(["ffprobe", "-v", "error", "-select_streams", "v", "-show_entries", "stream=pix_fmt", "-of", "csv=p=0", str(s)], capture_output=True, text=True).stdout.strip()
    ref, out = tmp / "ref.yuv", tmp / "out.yuv"
    if ref.exists():
        ref.unlink()
    r = subprocess.run(["ffmpeg", "-v", "error", "-y", "-i", str(s), "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", probe or "yuv420p", str(ref)], capture_output=True, text=True)
    h = subprocess.run([str(DECODE), str(s), str(out)], capture_output=True, text=True)
    if h.returncode != 0:
        msg = (h.stderr.strip().splitlines() or ["?"])[-1][:70]
        print(f"{d.name:58} ERREUR heifer: {msg}"); results["ERREUR heifer"] += 1; continue
    md5 = "MD5 OK" if "MD5 hash verified" in h.stdout else ("CRC OK" if "CRC hash" in h.stdout else ("checksum OK" if "checksum hash" in h.stdout else "pas d'empreinte"))
    results.setdefault(md5, 0); results[md5] += 1
    if r.returncode != 0 or not ref.exists():
        print(f"{d.name:58} ffmpeg ne sait pas decoder; empreinte SEI: {md5}"); results["ignore"] += 1; continue
    a, b = out.read_bytes(), ref.read_bytes()
    if a == b:
        print(f"{d.name:58} IDENTIQUE ({probe}), empreinte SEI: {md5}"); results["IDENTIQUE"] += 1
    else:
        n = sum(1 for x, y in zip(a, b) if x != y)
        first = next((i for i, (x, y) in enumerate(zip(a, b)) if x != y), min(len(a), len(b)))
        print(f"{d.name:58} DIFFERENT ({probe}) tailles {len(a)}/{len(b)}, {n} octets differents, 1er a l'octet {first}")
        results["DIFFERENT"] += 1
print("\nBilan:", ", ".join(f"{k}: {v}" for k, v in results.items()))
