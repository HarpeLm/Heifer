#!/usr/bin/env bash
# Makes the small AV1 streams of crates/heifer-av1-dec/tests/streams and their reference pictures.
#
# Each stream is one intra frame of a synthetic picture (ffmpeg testsrc2 with noise), encoded by
# aomenc with a chosen set of tools; the reference .yuv is aomdec's output (planar; samples
# deeper than 8 bits as 16-bit little endian).
# Needs ffmpeg, aomenc and aomdec. Run from the repository root.
set -euo pipefail

out=crates/heifer-av1-dec/tests/streams
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$out"

# No deblocking, CDEF or loop restoration: what the decoder reconstructs before filters.
nofilters="--enable-cdef=0 --enable-restoration=0 --loopfilter-control=0"
# Only DC/V/H… DCT, square blocks and transforms, no special tools.
basic="--enable-filter-intra=0 --enable-cfl-intra=0 --enable-palette=0 --enable-intrabc=0
  --enable-smooth-intra=0 --enable-paeth-intra=0 --enable-directional-intra=0
  --enable-angle-delta=0 --enable-intra-edge-filter=0 --enable-tx64=0 --enable-rect-tx=0
  --use-intra-dct-only=1 --enable-rect-partitions=0 --enable-ab-partitions=0
  --enable-1to4-partitions=0 --deltaq-mode=0 --aq-mode=0 --enable-qm=0"

picture() { # name size format [filters]
  ffmpeg -loglevel error -y -f lavfi -i "testsrc2=s=$2:d=1${4-,noise=alls=12:allf=t},format=$3" \
    -strict -1 -frames:v 1 "$tmp/$1.y4m"
}

stream() { # name picture options…
  local name=$1 src=$2
  shift 2
  # shellcheck disable=SC2068
  aomenc --limit=1 --kf-max-dist=0 --end-usage=q --cpu-used=4 --obu $@ \
    -o "$out/$name.obu" "$tmp/$src.y4m" >/dev/null 2>&1
  aomdec --rawvideo -o "$out/$name.yuv" "$out/$name.obu"
}

# Later options win, so "$basic --enable-x=1" is the basic set plus one tool: a mismatch then
# points at that tool.
picture small 136x72 yuv420p
picture large 264x200 yuv420p
picture gray 136x72 gray
picture small444 136x72 yuv444p
picture small422 136x72 yuv422p
picture small10 136x72 yuv420p10le
# Without noise aomenc detects screen content, which turns on palettes.
picture screen 264x200 yuv420p ""
# Smooth enough for 128×128 blocks and 64×64 transforms.
ffmpeg -loglevel error -y -f lavfi -i "gradients=s=264x200:d=1:seed=3,format=yuv420p" \
  -frames:v 1 "$tmp/gradient.y4m"

stream basic_q30 small --cq-level=30 $nofilters $basic
stream directional small --cq-level=30 $nofilters $basic --enable-directional-intra=1 \
  --enable-angle-delta=1 --enable-intra-edge-filter=1
stream smooth_paeth small --cq-level=30 $nofilters $basic --enable-smooth-intra=1 --enable-paeth-intra=1
stream cfl small --cq-level=30 $nofilters $basic --enable-cfl-intra=1
stream filter_intra small --cq-level=30 $nofilters $basic --enable-filter-intra=1
stream tx_types small --cq-level=20 $nofilters $basic --use-intra-dct-only=0
stream rect large --cq-level=30 $nofilters $basic --enable-rect-partitions=1 --enable-ab-partitions=1 \
  --enable-1to4-partitions=1 --enable-rect-tx=1 --enable-tx64=1
stream segmentation large --cq-level=30 $nofilters $basic --aq-mode=1
stream lossless small --lossless=1 $nofilters $basic
stream all_tools large --cq-level=25 $nofilters
stream tiles_sb128 large --cq-level=40 --sb-size=128 --tile-columns=1 --tile-rows=1 $nofilters
stream big_blocks gradient --cq-level=20 --sb-size=128 $nofilters
stream palette screen --cq-level=30 $nofilters
stream delta_q screen --cq-level=30 --deltaq-mode=3 $nofilters
stream monochrome gray --cq-level=30 --monochrome $nofilters
stream yuv444 small444 --cq-level=30 --profile=1 $nofilters
stream yuv422 small422 --cq-level=30 --profile=2 $nofilters
stream bit_depth_10 small10 --cq-level=30 --bit-depth=10 --input-bit-depth=10 $nofilters
