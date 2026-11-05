#!/usr/bin/env bash
# Downloads sample HEIF/HEIC files into tests/fixtures/.
# These files are not committed: check each source's license before redistributing.
set -euo pipefail
cd "$(dirname "$0")/../tests/fixtures"

NOKIA=https://raw.githubusercontent.com/nokiatech/heif/gh-pages/content/nokia
CONFORMANCE=https://raw.githubusercontent.com/nokiatech/heif_conformance/master/conformance_files
# pillow-heif (BSD-3-Clause) test samples, copied by name with `/` replaced by `__`.
PILLOW=https://raw.githubusercontent.com/bigcat88/pillow_heif/b16be1196dfa465a342d68894696e685ca3655cb
fetch() { [ -f "$2" ] || curl -fsSL "$1" -o "$2" && echo "ok  $2"; }

mkdir -p real

fetch "$NOKIA/autumn_1440x960.heic"            single_image.heic
fetch "$NOKIA/grid_960x640.heic"               grid.heic
fetch "$NOKIA/alpha_1440x960.heic"             alpha.heic
fetch "$NOKIA/random_collection_1440x960.heic" collection.heic
fetch "$NOKIA/bird_burst.heic"                 burst.heic
# Image sequences: animations with keyframes, and a burst with a thumbnail track.
fetch "$NOKIA/candle_animation.heic"           candle_animation.heic
fetch "$NOKIA/sea1_animation.heic"             sea1_animation.heic
fetch "$NOKIA/rally_burst.heic"                rally_burst.heic
fetch "$CONFORMANCE/C042.heic"                 mirrored.heic
fetch "$CONFORMANCE/C008.heic"                 iden_rotated.heic
fetch "$CONFORMANCE/C039.heic"                 iden_nested.heic
fetch https://raw.githubusercontent.com/strukturag/libheif/master/examples/example.heic libheif_example.heic
# A Samsung photo with an ISO 21496-1 gain map (tmap item).
fetch "$PILLOW/tests/images/heif_special/aux_YCbCr.heic" real/heif_special__aux_YCbCr.heic
