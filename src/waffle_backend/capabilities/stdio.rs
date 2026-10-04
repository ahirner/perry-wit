//! P3 output capabilities; byte transfers and native handles belong to shared support.

use perry_hir::types::Type as HirType;

use super::{CapabilityImplementation, CapabilityPlan, LowerCapability};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum StdioOperation {
    WriteStdout,
    WriteStderr,
    Log,
    Error,
}

impl StdioOperation {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::WriteStdout => "writeStdout",
            Self::WriteStderr => "writeStderr",
            Self::Log => "console.log",
            Self::Error => "console.error",
        }
    }

    pub(crate) fn channel(self) -> &'static str {
        match self {
            Self::WriteStdout | Self::Log => "stdout",
            Self::WriteStderr | Self::Error => "stderr",
        }
    }

    pub(crate) fn newline(self) -> bool {
        matches!(self, Self::Log | Self::Error)
    }
}

impl LowerCapability for StdioOperation {
    fn lower(&self) -> CapabilityPlan {
        let (input, result) = if self.newline() {
            (HirType::String, HirType::Void)
        } else {
            (
                HirType::Named("Uint8Array".into()),
                HirType::Promise(Box::new(HirType::Void)),
            )
        };
        CapabilityPlan {
            params: vec![input],
            result,
            implementation: CapabilityImplementation::Stdio(*self),
        }
    }
}
