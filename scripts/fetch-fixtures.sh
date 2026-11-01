#!/usr/bin/env bash
# Downloads sample HEIF/HEIC files into tests/fixtures/.
# These files are not committed: check each source's license before redistributing.
set -euo pipefail
cd "$(dirname "$0")/../tests/fixtures"

NOKIA=https://raw.githubusercontent.com/nokiatech/heif/gh-pages/content/nokia
fetch() { [ -f "$2" ] || curl -fsSL "$1" -o "$2" && echo "ok  $2"; }

fetch "$NOKIA/autumn_1440x960.heic"            single_image.heic
fetch "$NOKIA/grid_960x640.heic"               grid.heic
fetch "$NOKIA/alpha_1440x960.heic"             alpha.heic
fetch "$NOKIA/random_collection_1440x960.heic" collection.heic
fetch "$NOKIA/bird_burst.heic"                 burst.heic
fetch https://raw.githubusercontent.com/strukturag/libheif/master/examples/example.heic libheif_example.heic
