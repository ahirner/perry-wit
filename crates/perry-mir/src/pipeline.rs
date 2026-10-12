//! Verified optimization scheduling over Perry MIR.
use pliron::{
    context::{Context, Ptr},
    irbuild::{IRStatus, match_rewrite::PassWrapper},
    operation::{Operation, verify_operation},
    opts::{
        constants::sccp::SCCPPass, dce::DCEPass, mem2reg::Mem2RegPass,
        simplify_cfg::SimplifyCFGPass,
    },
    pass::{AnalysisManager, PMConfig, Pass, PassResult, Passes},
    result::Result,
};

/// Promote locals and simplify to a fixed point before ownership and lifetime lowering.
pub fn canonicalize(root: Ptr<Operation>, ctx: &mut Context) -> Result<()> {
    verify_operation(root, ctx)?;
    let mut analyses = AnalysisManager::default();
    analyses.set_config(PMConfig {
        verify_after_all: true,
        ..PMConfig::default()
    });
    let mut preparation = Passes::default();
    preparation.add_pass(Mem2RegPass);
    preparation.add_pass(Transform("inline-leaves", |root, ctx| {
        Ok(crate::aggregates::inline_leaves(root, ctx))
    }));
    preparation.add_pass(Transform("promote-arguments", |root, ctx| {
        Ok(crate::aggregates::promote_arguments(root, ctx))
    }));
    preparation.run(root, ctx, &mut analyses)?;

    let mut cleanup = Passes::default();
    cleanup.add_pass(PassWrapper::new(
        "optional-conversions",
        crate::fold::Canonicalize,
    ));
    cleanup.add_pass(SCCPPass);
    cleanup.add_pass(Transform("unused-block-arguments", |root, ctx| {
        Ok(crate::constants::remove_unused_block_arguments(root, ctx))
    }));
    cleanup.add_pass(Transform("decompose-records", |root, ctx| {
        crate::aggregates::scalar_replace(root, ctx).map(IRStatus::from)
    }));
    cleanup.add_pass(Mem2RegPass);
    cleanup.add_pass(DCEPass);
    cleanup.add_pass(SimplifyCFGPass);
    while cleanup.run(root, ctx, &mut analyses)?.ir_changed == IRStatus::Changed {}

    let mut bounds = Passes::default();
    bounds.add_pass(Transform("reuse-unwraps", |root, ctx| {
        crate::unwraps::reuse(root, ctx);
        Ok(IRStatus::Changed)
    }));
    bounds.add_pass(Transform("prove-bounds", |root, ctx| {
        Ok(crate::ranges::prove_bounds(root, ctx))
    }));
    bounds.run(root, ctx, &mut analyses)?;
    while cleanup.run(root, ctx, &mut analyses)?.ir_changed == IRStatus::Changed {}

    let mut ownership = Passes::default();
    ownership.add_pass(Transform("owned-append", |root, ctx| {
        Ok(crate::append::append_in_place(root, ctx))
    }));
    ownership.add_pass(Transform("scalar-characters", |root, ctx| {
        Ok(crate::characters::scalar_replace(root, ctx))
    }));
    ownership.add_pass(DCEPass);
    ownership.add_pass(Transform("expand-append", |root, ctx| {
        crate::fast_paths::expand(root, ctx);
        Ok(IRStatus::Changed)
    }));
    ownership.add_pass(Transform("assign-lifetimes", |root, ctx| {
        crate::lifetimes::assign(root, ctx);
        Ok(IRStatus::Changed)
    }));
    ownership.run(root, ctx, &mut analyses)?;
    verify_operation(root, ctx)
}

/// Adapt Perry-specific transformations to the shared verifier and analysis invalidation.
struct Transform(
    &'static str,
    fn(Ptr<Operation>, &mut Context) -> Result<IRStatus>,
);

impl Pass for Transform {
    fn name(&self) -> &str {
        self.0
    }

    fn run(
        &mut self,
        root: Ptr<Operation>,
        ctx: &mut Context,
        _: &mut AnalysisManager,
    ) -> Result<PassResult> {
        let mut result = PassResult::default();
        result.ir_changed = (self.1)(root, ctx)?;
        Ok(result)
    }
}
