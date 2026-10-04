//! Filesystem source contracts over preopened P3 descriptors.

use perry_hir::types::Type as HirType;

use super::{CapabilityImplementation, CapabilityPlan, LowerCapability};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum FilesystemOperation {
    WriteFile,
    ReadBytes,
    ReadText,
    ReadValue,
    Stat,
    Exists,
    MakeDirectory,
    Unlink,
    RemoveDirectory,
    ReadDirectory,
}

impl FilesystemOperation {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::WriteFile => "writeFileSync",
            Self::ReadBytes | Self::ReadText | Self::ReadValue => "readFileSync",
            Self::Stat => "statSync",
            Self::Exists => "existsSync",
            Self::MakeDirectory => "mkdirSync",
            Self::Unlink => "unlinkSync",
            Self::RemoveDirectory => "rmdirSync",
            Self::ReadDirectory => "readdirSync",
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
            Self::Stat
            | Self::Exists
            | Self::MakeDirectory
            | Self::Unlink
            | Self::RemoveDirectory
            | Self::ReadDirectory => CapabilityPlan {
                params: if *self == Self::Exists {
                    vec![HirType::String]
                } else {
                    vec![HirType::String, HirType::Any]
                },
                result: match self {
                    Self::Stat => HirType::Named("Stats".into()),
                    Self::Exists => HirType::Boolean,
                    Self::ReadDirectory => HirType::Array(Box::new(HirType::String)),
                    _ => HirType::Void,
                },
                implementation: CapabilityImplementation::Filesystem,
            },
        }
    }
}
