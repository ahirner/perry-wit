//! Typed capability plans; shared SSA and component code own their execution.

pub(crate) mod clocks;
mod context;
mod filesystem;
mod random;
pub(crate) mod stdio;

use perry_hir::types::Type as HirType;

pub(crate) use clocks::ClockOperation;
pub(crate) use context::ContextOperation;
pub(crate) use filesystem::FilesystemOperation;
pub(crate) use random::RandomOperation;
pub(crate) use stdio::StdioOperation;

/// A resolved operation, independent of the source binding used to call it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CapabilityOperation {
    Clock(ClockOperation),
    Context(ContextOperation),
    Random(RandomOperation),
    Stdio(StdioOperation),
    Filesystem(FilesystemOperation),
}

/// Pure lowering metadata for source validation, core calls, and component wiring.
pub(crate) struct CapabilityPlan {
    pub(crate) params: Vec<HirType>,
    pub(crate) result: HirType,
    pub(crate) implementation: CapabilityImplementation,
}

pub(crate) enum CapabilityImplementation {
    Standalone { core_function: &'static str },
    Stdio(StdioOperation),
    Filesystem,
    RandomBytes,
    Context,
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
            Self::Clock(operation) => operation.name(),
            Self::Context(operation) => operation.name(),
            Self::Random(operation) => operation.name(),
            Self::Stdio(operation) => operation.name(),
            Self::Filesystem(operation) => operation.name(),
        }
    }
}

impl LowerCapability for CapabilityOperation {
    fn lower(&self) -> CapabilityPlan {
        match self {
            Self::Clock(operation) => operation.lower(),
            Self::Context(operation) => operation.lower(),
            Self::Random(operation) => operation.lower(),
            Self::Stdio(operation) => operation.lower(),
            Self::Filesystem(operation) => operation.lower(),
        }
    }
}
