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
picture small12 136x72 yuv420p12le
# Without noise aomenc detects screen content, which turns on palettes.
picture screen 264x200 yuv420p ""
# Exact repeats, which intra block copy predicts: one 66×40 picture tiled 4×5.
ffmpeg -loglevel error -y -f lavfi -i "testsrc2=s=66x40:d=1" -frames:v 1 "$tmp/tile.png"
tiled() { # name format [filters]
  ffmpeg -loglevel error -y -loop 1 -i "$tmp/tile.png" -vf "tile=4x5${3-},format=$2" -strict -1 \
    -frames:v 1 "$tmp/$1.y4m"
}
tiled tiled yuv420p
tiled tiled444 yuv444p
tiled tiled422 yuv422p
tiled tiled_gray gray
tiled tiled10 yuv420p10le
tiled tiled12 yuv420p12le
# Noise leaves a residual under the copied blocks, so inter transform sizes and types get coded.
tiled tiled_noise yuv420p ,noise=alls=6:allf=t
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

# With loop filters: one filter at a time, then all of them with other tools.
nolr="--enable-restoration=0"
stream deblock small --cq-level=45 --enable-cdef=0 $nolr $basic
# aomenc only sets loop_filter_sharpness in all intra mode.
stream deblock_sharpness small --cq-level=45 --allintra --sharpness=6 --enable-cdef=0 $nolr $basic
stream deblock_tools large --cq-level=40 --enable-cdef=0 $nolr
stream delta_lf screen --cq-level=40 --deltaq-mode=3 --delta-lf-mode=1 --enable-cdef=0 $nolr
stream cdef small --cq-level=45 --loopfilter-control=0 $nolr $basic
stream cdef_sb128 large --cq-level=40 --sb-size=128 --loopfilter-control=0 $nolr
stream restoration small --cq-level=45 --loopfilter-control=0 --enable-cdef=0 $basic
stream restoration_large large --cq-level=35 --loopfilter-control=0 --enable-cdef=0
stream filters large --cq-level=40
stream filters_tiles large --cq-level=50 --sb-size=128 --tile-columns=1 --tile-rows=1
stream filters_segmentation large --cq-level=40 --aq-mode=1
stream filters_monochrome gray --cq-level=40 --monochrome
stream filters_yuv444 small444 --cq-level=40 --profile=1
stream filters_yuv422 small422 --cq-level=40 --profile=2
stream filters_10 small10 --cq-level=40 --bit-depth=10 --input-bit-depth=10
stream filters_12 small12 --cq-level=40 --profile=2 --bit-depth=12 --input-bit-depth=12

# Intra block copy: aomenc only allows it for screen content, at --cpu-used 2 or below. Loop
# filters are off in frames that allow it.
ibc="--tune-content=screen --cpu-used=2"
stream intrabc screen --cq-level=30 $ibc
stream intrabc_tiled tiled --cq-level=30 $ibc
stream intrabc_yuv444 tiled444 --cq-level=30 --profile=1 $ibc
stream intrabc_yuv422 tiled422 --cq-level=30 --profile=2 $ibc
stream intrabc_monochrome tiled_gray --cq-level=30 --monochrome $ibc
stream intrabc_10 tiled10 --cq-level=30 --bit-depth=10 --input-bit-depth=10 $ibc
stream intrabc_12 tiled12 --cq-level=30 --profile=2 --bit-depth=12 --input-bit-depth=12 $ibc
# With 128×128 superblocks and several tiles, aomenc no longer allows it.
stream intrabc_sb128 tiled --cq-level=40 --sb-size=128 $ibc
stream intrabc_tile_columns tiled --cq-level=40 --tile-columns=1 $ibc
stream intrabc_lossless tiled --lossless=1 $ibc
stream intrabc_noise tiled_noise --cq-level=20 $ibc

# Inter frames: eight frames of a turning picture. The reference .yuv holds every shown frame in
# order; the decoder must match all of them.
clip() { # name format
  ffmpeg -loglevel error -y -f lavfi \
    -i "testsrc2=s=136x72:r=8:d=1,rotate=a=t/4:c=black,noise=alls=8:allf=t,format=$2" -strict -1 \
    -frames:v 8 "$tmp/$1.y4m"
}
sequence() { # name clip options…
  local name=$1 src=$2
  shift 2
  # shellcheck disable=SC2068
  aomenc --limit=8 --end-usage=q --cpu-used=4 --obu $@ -o "$out/$name.obu" "$tmp/$src.y4m" \
    >/dev/null 2>&1
  aomdec --rawvideo -o "$out/$name.yuv" "$out/$name.obu"
}
clip clip yuv420p
clip clip10 yuv420p10le
clip clip444 yuv444p
# Hidden frames shown later (show_existing_frame), compound references, skip mode, warped motion.
sequence inter clip --cq-level=30
sequence inter_no_lag clip --cq-level=30 --lag-in-frames=0
sequence inter_obmc clip --cq-level=20 --cpu-used=1 --enable-warped-motion=0 \
  --enable-global-motion=0 --enable-dist-wtd-comp=1 --enable-interintra-comp=1 --enable-obmc=1 \
  --enable-dual-filter=1
sequence inter_wedge clip --cq-level=40 --cpu-used=0 --enable-diff-wtd-comp=0 \
  --enable-dist-wtd-comp=1 --enable-smooth-interintra=1 --enable-interintra-wedge=1 \
  --enable-interinter-wedge=1
sequence inter_segmentation clip --cq-level=30 --aq-mode=1
# Frames of random sizes, predicted from scaled references.
sequence inter_resize clip --cq-level=30 --resize-mode=2
sequence inter_10 clip10 --cq-level=30 --bit-depth=10 --input-bit-depth=10
sequence inter_yuv444 clip444 --cq-level=30 --profile=1
