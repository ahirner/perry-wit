//! Typed capability plans; shared SSA and component code own their execution.

pub(crate) mod clocks;
mod context;
mod filesystem;
mod random;
pub(crate) mod scalars;
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
    Promise(super::promises::Combinator),
    Clock(ClockOperation),
    Context(ContextOperation),
    Random(RandomOperation),
    Stdio(StdioOperation),
    Filesystem(FilesystemOperation),
    FilesystemPromise(FilesystemOperation),
    HttpGet,
    Exit,
}

/// Pure lowering metadata for source validation, core calls, and component wiring.
pub(crate) struct CapabilityPlan {
    pub(crate) params: Vec<HirType>,
    pub(crate) result: HirType,
    pub(crate) implementation: CapabilityImplementation,
}

pub(crate) enum CapabilityImplementation {
    Promise,
    Scalar,
    Stdio(StdioOperation),
    Filesystem,
    Http,
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
            Self::Promise(operation) => operation.name(),
            Self::Clock(operation) => operation.name(),
            Self::Context(operation) => operation.name(),
            Self::Random(operation) => operation.name(),
            Self::Stdio(operation) => operation.name(),
            Self::Filesystem(operation) | Self::FilesystemPromise(operation) => operation.name(),
            Self::HttpGet => "get",
            Self::Exit => "process.exit",
        }
    }
}

impl LowerCapability for CapabilityOperation {
    fn lower(&self) -> CapabilityPlan {
        match self {
            Self::Promise(_) => CapabilityPlan {
                params: vec![HirType::Any],
                result: HirType::Any,
                implementation: CapabilityImplementation::Promise,
            },
            Self::Exit => CapabilityPlan {
                params: vec![HirType::Number],
                result: HirType::Void,
                implementation: CapabilityImplementation::Scalar,
            },
            Self::Clock(operation) => operation.lower(),
            Self::Context(operation) => operation.lower(),
            Self::Random(operation) => operation.lower(),
            Self::Stdio(operation) => operation.lower(),
            Self::Filesystem(operation) => operation.lower(),
            Self::FilesystemPromise(operation) => {
                let mut plan = operation.lower();
                plan.result = HirType::Promise(Box::new(plan.result));
                plan
            }
            Self::HttpGet => CapabilityPlan {
                params: vec![
                    HirType::String,
                    HirType::String,
                    HirType::String,
                    crate::waffle_backend::http::headers_type(),
                    HirType::Number,
                ],
                result: HirType::Promise(Box::new(HirType::Named(
                    crate::waffle_backend::http::RESPONSE_TYPE.into(),
                ))),
                implementation: CapabilityImplementation::Http,
            },
        }
    }
}
