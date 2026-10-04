//! Typed random operations with shared host imports and guest byte ownership.

use super::{CapabilityImplementation, CapabilityPlan, LowerCapability};
use perry_hir::types::Type as HirType;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum RandomOperation {
    Number,
    Fill,
    Uuid,
}

impl RandomOperation {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Number => "randomNumber",
            Self::Fill => "crypto.getRandomValues",
            Self::Uuid => "crypto.randomUUID",
        }
    }
    pub(crate) fn needs_bytes(self) -> bool {
        self != Self::Number
    }
}

impl LowerCapability for RandomOperation {
    fn lower(&self) -> CapabilityPlan {
        CapabilityPlan {
            params: if *self == Self::Fill {
                vec![HirType::Any]
            } else {
                vec![]
            },
            result: match self {
                Self::Number => HirType::Number,
                Self::Fill => HirType::Named("Uint8Array".into()),
                Self::Uuid => HirType::String,
            },
            implementation: if *self == Self::Number {
                CapabilityImplementation::Scalar
            } else {
                CapabilityImplementation::RandomBytes
            },
        }
    }
}
