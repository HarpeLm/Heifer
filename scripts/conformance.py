#!/usr/bin/env python3
"""Decodes the first picture of each HEVC conformance bitstream with heifer and ffmpeg and
compares them byte for byte. Run scripts/fetch-conformance.sh first.

Usage:
    cargo build --release -p heifer-hevc-dec --example decode
    scripts/conformance.py [filter]
"""
import json, pathlib, subprocess, sys, tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
CONF = ROOT / "tests" / "conformance"
DECODE = ROOT / "target" / "release" / "examples" / "decode"
EXTS = {".bit", ".bin", ".bitstream", ".hevc", ".265", ".h265"}

if not DECODE.exists():
    sys.exit("build first: cargo build --release -p heifer-hevc-dec --example decode")
flt = sys.argv[1] if len(sys.argv) > 1 else ""
counts = {"identical_and_hash": 0, "identical": 0, "hash_only": 0, "different": 0,
          "error": 0, "no_reference": 0, "ignored": 0}
tmp = pathlib.Path(tempfile.mkdtemp())
for d in sorted(p for p in CONF.iterdir() if p.is_dir() and flt in p.name):
    streams = sorted((f for f in d.rglob("*") if f.suffix.lower() in EXTS), key=lambda f: -f.stat().st_size)
    if not streams:
        print(f"{d.name:58} ignore (pas de flux trouve)"); counts["ignored"] += 1; continue
    s = streams[0]
    probe = subprocess.run(["ffprobe", "-v", "error", "-select_streams", "v", "-show_entries", "stream=pix_fmt", "-of", "csv=p=0", str(s)], capture_output=True, text=True).stdout.strip()
    ref, out = tmp / "ref.yuv", tmp / "out.yuv"
    if ref.exists():
        ref.unlink()
    r = subprocess.run(["ffmpeg", "-v", "error", "-y", "-i", str(s), "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", probe or "yuv420p", str(ref)], capture_output=True, text=True)
    h = subprocess.run([str(DECODE), str(s), str(out)], capture_output=True, text=True)
    if h.returncode != 0:
        msg = (h.stderr.strip().splitlines() or ["?"])[-1][:70]
        print(f"{d.name:58} ERREUR heifer: {msg}"); counts["error"] += 1; continue
    has_hash = any(m in h.stdout for m in ("MD5 hash verified", "CRC hash", "checksum hash"))
    md5 = "empreinte OK" if has_hash else "pas d'empreinte"
    if r.returncode != 0 or not ref.exists():
        if has_hash:
            print(f"{d.name:58} ffmpeg ne sait pas decoder; empreinte SEI verifiee")
            counts["hash_only"] += 1
        else:
            print(f"{d.name:58} ffmpeg ne sait pas decoder; pas d'empreinte")
            counts["no_reference"] += 1
        continue
    a, b = out.read_bytes(), ref.read_bytes()
    if a == b:
        print(f"{d.name:58} IDENTIQUE ({probe}), {md5}")
        counts["identical_and_hash" if has_hash else "identical"] += 1
    else:
        n = sum(1 for x, y in zip(a, b) if x != y)
        first = next((i for i, (x, y) in enumerate(zip(a, b)) if x != y), min(len(a), len(b)))
        print(f"{d.name:58} DIFFERENT ({probe}) tailles {len(a)}/{len(b)}, {n} octets differents, 1er a l'octet {first}")
        counts["different"] += 1
total = sum(v for k, v in counts.items() if k != "ignored")
print("\nBilan:", ", ".join(f"{k}: {v}" for k, v in counts.items() if v), f"(total: {total})")
if not flt:
    pathlib.Path(ROOT / "docs" / "conformance.json").write_text(json.dumps(
        {"total": total, **{k: v for k, v in counts.items() if k != "ignored"}}, indent=2) + "\n")
    print("written to docs/conformance.json")
