//! Pass pipelines over perry MIR (`TODOs.md` P3.1).
use pliron::{
    context::{Context, Ptr},
    irbuild::IRStatus,
    operation::{Operation, verify_operation},
    opts::{dce::dce, mem2reg::mem2reg, simplify_cfg::simplify_cfg},
    pass::AnalysisManager,
    result::Result,
};

/// Promote slots to SSA, then remove dead code and trivial blocks until nothing changes.
/// Verifies before and after, so a pass that breaks an invariant is caught at its source.
pub fn canonicalize(root: Ptr<Operation>, ctx: &mut Context) -> Result<()> {
    verify_operation(root, ctx)?;
    mem2reg(root, ctx, &mut AnalysisManager::default())?;
    loop {
        let removed = dce(root, ctx)?;
        let simplified = simplify_cfg(root, ctx)?;
        if removed == IRStatus::Unchanged && simplified == IRStatus::Unchanged {
            break;
        }
    }
    verify_operation(root, ctx)
}
