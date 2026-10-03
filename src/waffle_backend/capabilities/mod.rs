//! Typed capability plans; shared SSA and component code own their execution.

mod clocks;
mod random;
pub(crate) mod source;

use perry_hir::types::Type as HirType;

pub(crate) use clocks::ClockOperation;
pub(crate) use random::RandomOperation;

/// A resolved operation, independent of the source binding used to call it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CapabilityOperation {
    Clock(ClockOperation),
    Random(RandomOperation),
}

/// Pure lowering metadata for source validation, core calls, and component wiring.
/// Host failures trap; these primitive operations have no WIT domain-error result.
pub(crate) struct CapabilityPlan {
    pub(crate) params: Vec<HirType>,
    pub(crate) result: HirType,
    pub(crate) adapter: &'static str,
    pub(crate) core_function: &'static str,
}

/// Describe an operation without owning values, scheduling, or invocation state.
pub(crate) trait LowerCapability {
    fn lower(&self) -> CapabilityPlan;
}

impl CapabilityOperation {
    pub(crate) fn from_declaration(name: &str) -> Option<Self> {
        match name {
            "waitFor" => Some(Self::Clock(ClockOperation::WaitFor)),
            "randomNumber" => Some(Self::Random(RandomOperation::Number)),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Clock(ClockOperation::WaitFor) => "waitFor",
            Self::Random(RandomOperation::Number) => "randomNumber",
        }
    }
}

impl LowerCapability for CapabilityOperation {
    fn lower(&self) -> CapabilityPlan {
        match self {
            Self::Clock(operation) => operation.lower(),
            Self::Random(operation) => operation.lower(),
        }
    }
}
