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
    verify_operation(root, ctx)?;
    crate::aggregates::inline_leaves(root, ctx);
    verify_operation(root, ctx)?;
    crate::aggregates::promote_arguments(root, ctx);
    verify_operation(root, ctx)?;
    loop {
        let folded = crate::fold::fold(root, ctx);
        if folded == IRStatus::Changed {
            verify_operation(root, ctx)?;
        }
        let promoted = crate::aggregates::scalar_replace(root, ctx)?;
        if promoted {
            verify_operation(root, ctx)?;
            mem2reg(root, ctx, &mut AnalysisManager::default())?;
            verify_operation(root, ctx)?;
        }
        let removed = dce(root, ctx)?;
        if removed == IRStatus::Changed {
            verify_operation(root, ctx)?;
        }
        let simplified = simplify_cfg(root, ctx)?;
        if simplified == IRStatus::Changed {
            verify_operation(root, ctx)?;
        }
        if !promoted
            && folded == IRStatus::Unchanged
            && removed == IRStatus::Unchanged
            && simplified == IRStatus::Unchanged
        {
            break;
        }
    }
    crate::lifetimes::assign(root, ctx);
    verify_operation(root, ctx)
}
