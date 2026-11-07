//! CABAC context variables: layout and initialization values for the three `initType` tables
//! (H.265 §9.3.2.2, Tables 9-5 to 9-37).
//!
//! All contexts live in one flat array so that the whole set can be copied cheaply, which is
//! needed for wavefront parallel processing (contexts are saved after the second CTB of each
//! row and restored at the start of the next row, §9.3.2.4).

use crate::cabac::ContextModel;

/// Declares the context layout: one constant offset per syntax element, plus the three tables
/// of initialization values (`initType` 0, 1 and 2) in the same order.
///
/// Groups are separated by `;` to keep the nested comma-separated lists unambiguous.
macro_rules! contexts {
    ($($name:ident: [$($i0:expr),+ $(,)?] / [$($i1:expr),+ $(,)?] / [$($i2:expr),+ $(,)?] );+ $(;)?) => {
        /// Offsets of each syntax element's contexts in [`Contexts`].
        #[allow(missing_docs)]
        pub mod ctx {
            contexts!(@offsets 0usize; $($name [$($i0),+])+);
        }
        /// Initialization values for I slices (`initType` 0), in layout order.
        const INIT_I: &[u8] = &[$($($i0),+),+];
        /// Initialization values for P slices (`initType` 1), in layout order.
        const INIT_P: &[u8] = &[$($($i1),+),+];
        /// Initialization values for B slices (`initType` 2), in layout order.
        const INIT_B: &[u8] = &[$($($i2),+),+];
    };
    (@offsets $off:expr; $name:ident [$($v:expr),+] $($rest:tt)*) => {
        pub const $name: usize = $off;
        contexts!(@offsets $off + [$($v),+].len(); $($rest)*);
    };
    (@offsets $off:expr;) => {
        /// Total number of contexts.
        pub const COUNT: usize = $off;
    };
}

contexts! {
    SAO_MERGE_FLAG: [153] / [153] / [153];
    SAO_TYPE_IDX: [200] / [185] / [160];
    SPLIT_CU_FLAG: [139, 141, 157] / [107, 139, 126] / [107, 139, 126];
    CU_TRANSQUANT_BYPASS_FLAG: [154] / [154] / [154];
    CU_SKIP_FLAG: [154, 154, 154] / [197, 185, 201] / [197, 185, 201];
    CU_QP_DELTA_ABS: [154, 154, 154] / [154, 154, 154] / [154, 154, 154];
    PRED_MODE_FLAG: [154] / [149] / [134];
    PART_MODE: [184, 154, 154, 154] / [154, 139, 154, 154] / [154, 139, 154, 154];
    PREV_INTRA_LUMA_PRED_FLAG: [184] / [154] / [183];
    INTRA_CHROMA_PRED_MODE: [63, 139] / [152, 139] / [152, 139];
    MERGE_FLAG: [154] / [110] / [154];
    MERGE_IDX: [154] / [122] / [137];
    INTER_PRED_IDC: [154, 154, 154, 154, 154] / [95, 79, 63, 31, 31] / [95, 79, 63, 31, 31];
    REF_IDX_L0: [154, 154] / [153, 153] / [153, 153];
    REF_IDX_L1: [154, 154] / [153, 153] / [153, 153];
    ABS_MVD_GREATER0_FLAG: [154, 154] / [140, 198] / [169, 198];
    ABS_MVD_GREATER1_FLAG: [154, 154] / [140, 198] / [169, 198];
    MVP_LX_FLAG: [154] / [168] / [168];
    NO_RESIDUAL_DATA_FLAG: [154] / [79] / [79];
    SPLIT_TRANSFORM_FLAG: [153, 138, 138] / [124, 138, 94] / [224, 167, 122];
    CBF_LUMA: [111, 141] / [153, 111] / [153, 111];
    CBF_CHROMA: [94, 138, 182, 154, 154] / [149, 107, 167, 154, 154] / [149, 92, 167, 154, 154];
    TRANSFORM_SKIP_FLAG_LUMA: [139] / [139] / [139];
    TRANSFORM_SKIP_FLAG_CHROMA: [139] / [139] / [139];
    EXPLICIT_RDPCM_FLAG: [139, 139] / [139, 139] / [139, 139];
    EXPLICIT_RDPCM_DIR_FLAG: [139, 139] / [139, 139] / [139, 139];
    LAST_SIG_COEFF_X_PREFIX: [
        110, 110, 124, 125, 140, 153, 125, 127, 140, 109, 111, 143, 127, 111,
        79, 108, 123, 63,
    ] / [
        125, 110, 94, 110, 95, 79, 125, 111, 110, 78, 110, 111, 111, 95,
        94, 108, 123, 108,
    ] / [
        125, 110, 124, 110, 95, 94, 125, 111, 111, 79, 125, 126, 111, 111,
        79, 108, 123, 93,
    ];
    LAST_SIG_COEFF_Y_PREFIX: [
        110, 110, 124, 125, 140, 153, 125, 127, 140, 109, 111, 143, 127, 111,
        79, 108, 123, 63,
    ] / [
        125, 110, 94, 110, 95, 79, 125, 111, 110, 78, 110, 111, 111, 95,
        94, 108, 123, 108,
    ] / [
        125, 110, 124, 110, 95, 94, 125, 111, 111, 79, 125, 126, 111, 111,
        79, 108, 123, 93,
    ];
    CODED_SUB_BLOCK_FLAG: [91, 171, 134, 141] / [121, 140, 61, 154] / [121, 140, 61, 154];
    SIG_COEFF_FLAG: [
        111, 111, 125, 110, 110, 94, 124, 108, 124, 107, 125, 141, 179, 153,
        125, 107, 125, 141, 179, 153, 125, 107, 125, 141, 179, 153, 125, 140,
        139, 182, 182, 152, 136, 152, 136, 153, 136, 139, 111, 136, 139, 111,
        141, 111,
    ] / [
        155, 154, 139, 153, 139, 123, 123, 63, 153, 166, 183, 140, 136, 153,
        154, 166, 183, 140, 136, 153, 154, 166, 183, 140, 136, 153, 154, 170,
        153, 123, 123, 107, 121, 107, 121, 167, 151, 183, 140, 151, 183, 140,
        140, 140,
    ] / [
        170, 154, 139, 153, 139, 123, 123, 63, 124, 166, 183, 140, 136, 153,
        154, 166, 183, 140, 136, 153, 154, 166, 183, 140, 136, 153, 154, 170,
        153, 138, 138, 122, 121, 122, 121, 167, 151, 183, 140, 151, 183, 140,
        140, 140,
    ];
    COEFF_ABS_LEVEL_GREATER1_FLAG: [
        140, 92, 137, 138, 140, 152, 138, 139, 153, 74, 149, 92, 139, 107,
        122, 152, 140, 179, 166, 182, 140, 227, 122, 197,
    ] / [
        154, 196, 196, 167, 154, 152, 167, 182, 182, 134, 149, 136, 153, 121,
        136, 137, 169, 194, 166, 167, 154, 167, 137, 182,
    ] / [
        154, 196, 167, 167, 154, 152, 167, 182, 182, 134, 149, 136, 153, 121,
        136, 122, 169, 208, 166, 167, 154, 152, 167, 182,
    ];
    COEFF_ABS_LEVEL_GREATER2_FLAG: [
        138, 153, 136, 167, 152, 152,
    ] / [
        107, 167, 91, 122, 107, 167,
    ] / [
        107, 167, 91, 107, 107, 167,
    ];
    LOG2_RES_SCALE_ABS_PLUS1: [
        154, 154, 154, 154, 154, 154, 154, 154,
    ] / [
        154, 154, 154, 154, 154, 154, 154, 154,
    ] / [
        154, 154, 154, 154, 154, 154, 154, 154,
    ];
    RES_SCALE_SIGN_FLAG: [154, 154] / [154, 154] / [154, 154];
    CU_CHROMA_QP_OFFSET_FLAG: [154] / [154] / [154];
    CU_CHROMA_QP_OFFSET_IDX: [154] / [154] / [154];
}

/// All CABAC contexts of a slice, plus the Rice parameter statistics, which are saved and
/// restored together with them (§9.3.2.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Contexts {
    /// Context variables, indexed by the offsets in [`ctx`].
    pub models: [ContextModel; ctx::COUNT],
    /// `StatCoeff[sbType]` for persistent Rice adaptation (range extension).
    pub stat_coeff: [u8; 4],
}

impl Contexts {
    /// `initType` of a slice (§9.3.2.2): 0 for I slices; 1 for P slices, or 2 with
    /// `cabac_init_flag`; 2 for B slices, or 1 with `cabac_init_flag`.
    pub fn init_type(slice_type: crate::slice::SliceType, cabac_init_flag: bool) -> u8 {
        use crate::slice::SliceType;
        match (slice_type, cabac_init_flag) {
            (SliceType::I, _) => 0,
            (SliceType::P, false) | (SliceType::B, true) => 1,
            (SliceType::P, true) | (SliceType::B, false) => 2,
        }
    }

    /// Initializes every context for the given `SliceQpY` and `initType` (§9.3.2.2).
    pub fn new_with_type(slice_qp_y: i32, init_type: u8) -> Self {
        let table = match init_type {
            0 => INIT_I,
            1 => INIT_P,
            _ => INIT_B,
        };
        Self {
            models: core::array::from_fn(|i| ContextModel::new(table[i], slice_qp_y)),
            stat_coeff: [0; 4],
        }
    }

    /// Initializes every context for an I slice.
    pub fn new(slice_qp_y: i32) -> Self {
        Self::new_with_type(slice_qp_y, 0)
    }
}

impl core::ops::Index<usize> for Contexts {
    type Output = ContextModel;
    fn index(&self, i: usize) -> &ContextModel {
        &self.models[i]
    }
}

impl core::ops::IndexMut<usize> for Contexts {
    fn index_mut(&mut self, i: usize) -> &mut ContextModel {
        &mut self.models[i]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_contiguous_and_complete() {
        assert_eq!(ctx::SAO_MERGE_FLAG, 0);
        assert_eq!(ctx::SPLIT_CU_FLAG, 2);
        assert_eq!(
            ctx::LAST_SIG_COEFF_Y_PREFIX - ctx::LAST_SIG_COEFF_X_PREFIX,
            18
        );
        assert_eq!(ctx::COEFF_ABS_LEVEL_GREATER1_FLAG - ctx::SIG_COEFF_FLAG, 44);
        assert_eq!(ctx::COUNT, INIT_I.len());
        assert_eq!(ctx::COUNT, INIT_P.len());
        assert_eq!(ctx::COUNT, INIT_B.len());
        assert_eq!(ctx::COUNT, ctx::CU_CHROMA_QP_OFFSET_IDX + 1);
    }

    #[test]
    fn initialization_depends_on_qp_and_init_type() {
        let a = Contexts::new(22);
        let b = Contexts::new(37);
        assert_eq!(a[ctx::SPLIT_CU_FLAG], ContextModel::new(139, 22));
        assert_ne!(a[ctx::SPLIT_CU_FLAG], b[ctx::SPLIT_CU_FLAG]);
        // initValue 154 gives the same equiprobable state at every QP.
        assert_eq!(a[ctx::CU_QP_DELTA_ABS], b[ctx::CU_QP_DELTA_ABS]);
        assert_eq!(
            Contexts::new_with_type(22, 0)[ctx::SPLIT_CU_FLAG],
            ContextModel::new(139, 22)
        );
        assert_eq!(
            Contexts::new_with_type(22, 1)[ctx::SPLIT_CU_FLAG],
            ContextModel::new(107, 22)
        );
        assert_eq!(
            Contexts::new_with_type(22, 2)[ctx::SPLIT_CU_FLAG],
            ContextModel::new(107, 22)
        );
    }

    #[test]
    fn init_type_rotation() {
        use crate::slice::SliceType;
        assert_eq!(Contexts::init_type(SliceType::I, false), 0);
        assert_eq!(Contexts::init_type(SliceType::P, false), 1);
        assert_eq!(Contexts::init_type(SliceType::P, true), 2);
        assert_eq!(Contexts::init_type(SliceType::B, false), 2);
        assert_eq!(Contexts::init_type(SliceType::B, true), 1);
    }
}
