//! Filesystem source contracts over preopened P3 descriptors.

use perry_hir::types::Type as HirType;

use super::{CapabilityImplementation, CapabilityPlan, LowerCapability};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum FilesystemOperation {
    WriteFile,
    ReadBytes,
    ReadText,
    ReadValue,
}

impl FilesystemOperation {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::WriteFile => "writeFileSync",
            Self::ReadBytes | Self::ReadText | Self::ReadValue => "readFileSync",
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
            Self::ReadBytes | Self::ReadText | Self::ReadValue => CapabilityPlan {
                params: vec![HirType::String, HirType::Any],
                result: match self {
                    Self::ReadText => HirType::String,
                    Self::ReadValue => crate::waffle_backend::text_or_bytes::value_type(),
                    _ => HirType::Named("Uint8Array".into()),
                },
                implementation: CapabilityImplementation::Filesystem,
            },
        }
    }
}
