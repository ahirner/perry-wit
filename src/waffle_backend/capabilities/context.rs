//! Immutable host context snapshots exposed as cached guest values.

use super::{CapabilityImplementation, CapabilityPlan, LowerCapability};
use perry_hir::types::Type as HirType;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ContextOperation {
    Environment,
    Arguments,
    InitialCwd,
}

impl ContextOperation {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Environment => "process.env",
            Self::Arguments => "process.argv",
            Self::InitialCwd => "process.cwd",
        }
    }
    pub(crate) fn import(self) -> &'static str {
        match self {
            Self::Environment => "get-environment",
            Self::Arguments => "get-arguments",
            Self::InitialCwd => "get-initial-cwd",
        }
    }
}

impl LowerCapability for ContextOperation {
    fn lower(&self) -> CapabilityPlan {
        CapabilityPlan {
            params: vec![],
            result: match self {
                Self::Environment => {
                    HirType::Named(crate::waffle_backend::context::ENVIRONMENT_TYPE.into())
                }
                Self::Arguments => HirType::Array(Box::new(HirType::String)),
                Self::InitialCwd => HirType::String,
            },
            implementation: CapabilityImplementation::Context,
        }
    }
}
