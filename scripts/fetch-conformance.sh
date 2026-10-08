#!/usr/bin/env bash
# Downloads a selection of the official HEVC conformance bitstreams (ITU-T H.265.1)
# into tests/conformance/. Files are not committed.
set -euo pipefail
cd "$(dirname "$0")/../tests/conformance"
BASE=https://www.itu.int/wftp3/av-arch/jctvc-site/bitstream_exchange/draft_conformance
while read -r path; do
  name=$(basename "$path")
  [ -f "$name.zip" ] || curl -fsSL "$BASE/$path.zip" -o "$name.zip"
  mkdir -p "$name" && unzip -oq "$name.zip" -d "$name"
done <<EOF
HEVC_v1/CONFWIN_A_Sony_1
HEVC_v1/DBLK_A_SONY_3
HEVC_v1/DBLK_B_SONY_3
HEVC_v1/DBLK_C_SONY_3
HEVC_v1/DBLK_A_MAIN10_VIXS_4
HEVC_v1/DELTAQP_C_SONY_3
HEVC_v1/DSLICE_A_HHI_5
HEVC_v1/ENTP_B_Qualcomm_1
HEVC_v1/ENTP_C_Qualcomm_1
HEVC_v1/INITQP_A_Sony_1
HEVC_v1/INITQP_B_Main10_Sony_1
HEVC_v1/ipcm_A_NEC_3
HEVC_v1/ipcm_B_NEC_3
HEVC_v1/ipcm_C_NEC_3
HEVC_v1/ipcm_D_NEC_3
HEVC_v1/ipcm_E_NEC_2
HEVC_v1/IPRED_A_docomo_2
HEVC_v1/IPRED_B_Nokia_3
HEVC_v1/IPRED_C_Mitsubishi_3
HEVC_v1/PICSIZE_A_Bossen_1
HEVC_v1/PICSIZE_B_Bossen_1
HEVC_v1/PICSIZE_C_Bossen_1
HEVC_v1/PICSIZE_D_Bossen_1
HEVC_v1/SAO_A_MediaTek_4
HEVC_v1/SAO_B_MediaTek_5
HEVC_v1/SAO_C_Samsung_5
HEVC_v1/SAO_D_Samsung_5
HEVC_v1/SAO_E_Canon_4
HEVC_v1/SAO_F_Canon_3
HEVC_v1/SAO_G_Canon_3
HEVC_v1/SAO_H_Parabola_1
HEVC_v1/SDH_A_Orange_4
HEVC_v1/SLICES_A_Rovi_3
HEVC_v1/SLIST_A_Sony_5
HEVC_v1/SLIST_B_Sony_9
HEVC_v1/SLIST_C_Sony_4
HEVC_v1/SLIST_D_Sony_9
HEVC_v1/STRUCT_A_Samsung_7
HEVC_v1/STRUCT_B_Samsung_7
HEVC_v1/TILES_A_Cisco_2
HEVC_v1/TILES_B_Cisco_1
HEVC_v1/TSUNEQBD_A_MAIN10_Technicolor_2
HEVC_v1/TUSIZE_A_Samsung_1
HEVC_v1/WPP_D_ericsson_MAIN_2
HEVC_v1/WPP_E_ericsson_MAIN_2
HEVC_v1/WPP_F_ericsson_MAIN_2
HEVC_v1/WPP_D_ericsson_MAIN10_2
RExt/GENERAL_8b_400_RExt_Sony_1
RExt/GENERAL_8b_420_RExt_Sony_1
RExt/GENERAL_8b_444_RExt_Sony_2
RExt/GENERAL_10b_420_RExt_Sony_1
RExt/GENERAL_10b_422_RExt_Sony_1
RExt/GENERAL_10b_444_RExt_Sony_2
RExt/GENERAL_12b_400_RExt_Sony_1
RExt/GENERAL_12b_420_RExt_Sony_1
RExt/GENERAL_12b_422_RExt_Sony_1
RExt/Bitdepth_A_RExt_Sony_1
RExt/Bitdepth_B_RExt_Sony_1
RExt/IPCM_A_RExt_NEC_2
RExt/IPCM_B_RExt_NEC
RExt/QMATRIX_A_RExt_Sony_1
RExt/CCP_8bit_RExt_QCOM_1
RExt/CCP_10bit_RExt_QCOM_1
RExt/ADJUST_IPRED_ANGLE_A_RExt_Mitsubishi_2
RExt/Main_422_10_B_RExt_Sony_2
RExt/WAVETILES_RExt_Sony_2
EOF
echo "ok: $(ls -d */ | wc -l) bitstreams"
