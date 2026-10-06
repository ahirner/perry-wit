//! Source clock units and selective WASI P3 clock imports.

use super::{CapabilityImplementation, CapabilityPlan, LowerCapability};
use perry_hir::types::Type as HirType;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ClockOperation {
    Timeout,
    TimeoutValue,
    MonotonicNow,
    DateNow,
}

impl ClockOperation {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Timeout | Self::TimeoutValue => "setTimeout",
            Self::MonotonicNow => "performance.now",
            Self::DateNow => "Date.now",
        }
    }
}

impl LowerCapability for ClockOperation {
    fn lower(&self) -> CapabilityPlan {
        if *self == Self::TimeoutValue {
            return CapabilityPlan {
                params: vec![HirType::Number, HirType::Any, HirType::Any],
                result: HirType::Promise(Box::new(HirType::Any)),
                implementation: CapabilityImplementation::Scalar,
            };
        }
        CapabilityPlan {
            params: if matches!(self, Self::Timeout) {
                vec![HirType::Number]
            } else {
                vec![]
            },
            result: if matches!(self, Self::Timeout) {
                HirType::Promise(Box::new(HirType::Void))
            } else {
                HirType::Number
            },
            implementation: CapabilityImplementation::Scalar,
        }
    }
}
