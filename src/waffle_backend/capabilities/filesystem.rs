//! Filesystem source contracts over preopened P3 descriptors.

use perry_hir::types::Type as HirType;

use super::{CapabilityImplementation, CapabilityPlan, LowerCapability};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum FilesystemOperation {
    WriteFile,
    ReadBytes,
    ReadText,
}

impl FilesystemOperation {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::WriteFile => "writeFileSync",
            Self::ReadBytes | Self::ReadText => "readFileSync",
        }
    }
}

impl LowerCapability for FilesystemOperation {
    fn lower(&self) -> CapabilityPlan {
        match self {
            Self::WriteFile => CapabilityPlan {
                params: vec![HirType::String, HirType::Any, HirType::Any],
                result: HirType::Void,
                implementation: CapabilityImplementation::Filesystem,
            },
            Self::ReadBytes | Self::ReadText => CapabilityPlan {
                params: vec![HirType::String, HirType::Any],
                result: if *self == Self::ReadText {
                    HirType::String
                } else {
                    HirType::Named("Uint8Array".into())
                },
                implementation: CapabilityImplementation::Filesystem,
            },
        }
    }
}
