#!/usr/bin/env bash
# Downloads real-world HEIC/HEIF test files from pillow-heif (BSD-3-Clause) into
# tests/fixtures/real/. Files are not committed.
set -euo pipefail
cd "$(dirname "$0")/../tests/fixtures"
mkdir -p real
BASE=https://raw.githubusercontent.com/bigcat88/pillow_heif/b16be1196dfa465a342d68894696e685ca3655cb
while read -r path; do
  out="real/$(echo "$path" | sed "s#^tests/images/##; s#/#__#g")"
  [ -f "$out" ] || curl -fsSL "$BASE/$path" -o "$out"
done <<EOF
benchmarks/image_large.heic
tests/images/heif/LA_8__128x128.heif
tests/images/heif/LA_8__29x100.heif
tests/images/heif/L_10__128x128.heif
tests/images/heif/L_10__29x100.heif
tests/images/heif/L_12__128x128.heif
tests/images/heif/L_12__29x100.heif
tests/images/heif/L_8__128x128.heif
tests/images/heif/L_8__29x100.heif
tests/images/heif/L_xmp.heif
tests/images/heif/RGBA_10__128x128.heif
tests/images/heif/RGBA_10__29x100.heif
tests/images/heif/RGBA_12__128x128.heif
tests/images/heif/RGBA_12__29x100.heif
tests/images/heif/RGBA_8__128x128.heif
tests/images/heif/RGBA_8__29x100.heif
tests/images/heif/RGB_10__128x128.heif
tests/images/heif/RGB_10__29x100.heif
tests/images/heif/RGB_12__128x128.heif
tests/images/heif/RGB_12__29x100.heif
tests/images/heif/RGB_8__128x128.heif
tests/images/heif/RGB_8__29x100.heif
tests/images/heif/zPug_3.heic
tests/images/heif_corrupted/corrupted.heic
tests/images/heif_corrupted/empty.heic
tests/images/heif_other/L_exif_xmp_iptc.heic
tests/images/heif_other/L_xmp_latin1.heic
tests/images/heif_other/RGB_8_chroma444.heif
tests/images/heif_other/arrow.heic
tests/images/heif_other/empty_icc.heic
tests/images/heif_other/invalid_id.heic
tests/images/heif_other/pug.heic
tests/images/heif_other/spatial_photo.heic
tests/images/heif_other/stereo_pair.heic
tests/images/heif_special/200MP.heic
tests/images/heif_special/aspect_mismatch_thumbnail.heic
tests/images/heif_special/aux_YCbCr.heic
tests/images/heif_special/broken_thumbnail.heic
tests/images/heif_special/crop_mismatch_thumbnail.heic
tests/images/heif_special/entity_groups.heic
tests/images/heif_special/invalid_metadata_type.heic
tests/images/heif_special/profile_mismatch_thumbnail.heic
tests/images/heif_special/transform_mismatch_thumbnail.heic
tests/images/heif_special/unsupported_depth_image.heic
tests/images/heif_special/xiaomi.heic
tests/images/heif_truncated/truncated.heic
EOF
echo "ok: $(ls real | wc -l) files"
