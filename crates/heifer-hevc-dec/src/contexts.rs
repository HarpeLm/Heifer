//! CABAC context variables for intra (I) slices: layout and initialization values
//! (H.265 §9.3.2.2, Tables 9-5 to 9-37, `initType` 0).
//!
//! All contexts live in one flat array so that the whole set can be copied cheaply, which is
//! needed for wavefront parallel processing (contexts are saved after the second CTB of each
//! row and restored at the start of the next row, §9.3.2.4).

use crate::cabac::ContextModel;

/// Declares the context layout: one constant offset per syntax element, plus the table of
/// initialization values in the same order.
macro_rules! contexts {
    ($($name:ident: [$($v:expr),+ $(,)?],)+) => {
        /// Offsets of each syntax element's contexts in [`Contexts`].
        #[allow(missing_docs)]
        pub mod ctx {
            contexts!(@offsets 0usize; $($name [$($v),+])+);
        }
        /// Initialization values (`initType` 0), in layout order.
        const INIT_VALUES: &[u8] = &[$($($v),+),+];
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
    SAO_MERGE_FLAG: [153],
    SAO_TYPE_IDX: [200],
    SPLIT_CU_FLAG: [139, 141, 157],
    CU_TRANSQUANT_BYPASS_FLAG: [154],
    PART_MODE: [184],
    PREV_INTRA_LUMA_PRED_FLAG: [184],
    INTRA_CHROMA_PRED_MODE: [63],
    SPLIT_TRANSFORM_FLAG: [153, 138, 138],
    CBF_LUMA: [111, 141],
    CBF_CHROMA: [94, 138, 182, 154, 154],
    CU_QP_DELTA_ABS: [154, 154],
    CU_CHROMA_QP_OFFSET_FLAG: [154],
    CU_CHROMA_QP_OFFSET_IDX: [154],
    TRANSFORM_SKIP_FLAG_LUMA: [139],
    TRANSFORM_SKIP_FLAG_CHROMA: [139],
    LAST_SIG_COEFF_X_PREFIX: [
        110, 110, 124, 125, 140, 153, 125, 127, 140, 109, 111, 143, 127, 111, 79, 108, 123, 63,
    ],
    LAST_SIG_COEFF_Y_PREFIX: [
        110, 110, 124, 125, 140, 153, 125, 127, 140, 109, 111, 143, 127, 111, 79, 108, 123, 63,
    ],
    CODED_SUB_BLOCK_FLAG: [91, 171, 134, 141],
    SIG_COEFF_FLAG: [
        111, 111, 125, 110, 110, 94, 124, 108, 124, 107, 125, 141, 179, 153, 125, 107, 125, 141,
        179, 153, 125, 107, 125, 141, 179, 153, 125, 140, 139, 182, 182, 152, 136, 152, 136, 153,
        136, 139, 111, 136, 139, 111,
        // transform_skip_context_enabled_flag: luma, chroma.
        141, 111,
    ],
    COEFF_ABS_LEVEL_GREATER1_FLAG: [
        140, 92, 137, 138, 140, 152, 138, 139, 153, 74, 149, 92, 139, 107, 122, 152, 140, 179,
        166, 182, 140, 227, 122, 197,
    ],
    COEFF_ABS_LEVEL_GREATER2_FLAG: [138, 153, 136, 167, 152, 152],
    LOG2_RES_SCALE_ABS_PLUS1: [154, 154, 154, 154, 154, 154, 154, 154],
    RES_SCALE_SIGN_FLAG: [154, 154],
}

/// All CABAC contexts of an I slice, plus the Rice parameter statistics, which are saved and
/// restored together with them (§9.3.2.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Contexts {
    /// Context variables, indexed by the offsets in [`ctx`].
    pub models: [ContextModel; ctx::COUNT],
    /// `StatCoeff[sbType]` for persistent Rice adaptation (range extension).
    pub stat_coeff: [u8; 4],
}

impl Contexts {
    /// Initializes every context for the given `SliceQpY` (§9.3.2.2).
    pub fn new(slice_qp_y: i32) -> Self {
        Self {
            models: core::array::from_fn(|i| ContextModel::new(INIT_VALUES[i], slice_qp_y)),
            stat_coeff: [0; 4],
        }
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
        assert_eq!(ctx::COUNT, INIT_VALUES.len());
        assert_eq!(ctx::COUNT, ctx::RES_SCALE_SIGN_FLAG + 2);
    }

    #[test]
    fn initialization_depends_on_qp() {
        let a = Contexts::new(22);
        let b = Contexts::new(37);
        assert_eq!(a[ctx::SPLIT_CU_FLAG], ContextModel::new(139, 22));
        assert_ne!(a[ctx::SPLIT_CU_FLAG], b[ctx::SPLIT_CU_FLAG]);
        // initValue 154 gives the same equiprobable state at every QP.
        assert_eq!(a[ctx::CU_QP_DELTA_ABS], b[ctx::CU_QP_DELTA_ABS]);
    }
}
