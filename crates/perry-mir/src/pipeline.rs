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
pub fn canonicalize(root: Ptr<Operation>, ctx: &mut Context) -> Result<AnalysisManager> {
    verify_operation(root, ctx)?;
    let mut analyses = AnalysisManager::default();
    analyses.set_config(PMConfig {
        verify_after_all: true,
        ..PMConfig::default()
    });
    let mut preparation = Passes::default();
    preparation.add_pass(Mem2RegPass);
    preparation.add_pass(crate::numeric::Specialize);
    preparation.add_pass(Transform("inline-leaves", |root, ctx| {
        Ok(crate::aggregates::inline_leaves(root, ctx))
    }));
    preparation.add_pass(PromoteArguments);
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
    bounds.add_pass(ProveBounds);
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
    ownership.add_pass(AssignLifetimes);
    ownership.run(root, ctx, &mut analyses)?;
    verify_operation(root, ctx)?;
    Ok(analyses)
}

struct PromoteArguments;

struct ProveBounds;

impl Pass for ProveBounds {
    fn name(&self) -> &str {
        "prove-bounds"
    }
    fn run(
        &mut self,
        root: Ptr<Operation>,
        ctx: &mut Context,
        analyses: &mut AnalysisManager,
    ) -> Result<PassResult> {
        use pliron::graph::dominance::DomInfo;
        analyses.get_analysis::<crate::numeric::NumericAnalysis>(root, ctx)?;
        analyses.get_analysis::<DomInfo>(root, ctx)?;
        let numeric = analyses
            .try_get_analysis::<crate::numeric::NumericAnalysis>(root)
            .expect("numeric analysis");
        let mut dominance = analyses
            .try_get_analysis_mut::<DomInfo>(root)
            .expect("dominance");
        let mut result = PassResult::default();
        result.ir_changed = crate::ranges::prove_bounds(root, ctx, &numeric, &mut dominance);
        Ok(result)
    }
}

impl Pass for PromoteArguments {
    fn name(&self) -> &str {
        "promote-arguments"
    }

    fn run(
        &mut self,
        root: Ptr<Operation>,
        ctx: &mut Context,
        analyses: &mut AnalysisManager,
    ) -> Result<PassResult> {
        let effects = analyses.get_analysis::<crate::effects::RuntimeEffects>(root, ctx)?;
        let mut result = PassResult::default();
        result.ir_changed = crate::aggregates::promote_arguments(root, ctx, &effects);
        Ok(result)
    }
}

struct AssignLifetimes;

impl Pass for AssignLifetimes {
    fn name(&self) -> &str {
        "assign-lifetimes"
    }

    fn run(
        &mut self,
        root: Ptr<Operation>,
        ctx: &mut Context,
        analyses: &mut AnalysisManager,
    ) -> Result<PassResult> {
        let effects = analyses.get_analysis::<crate::effects::RuntimeEffects>(root, ctx)?;
        crate::lifetimes::assign(root, ctx, &effects);
        let mut result = PassResult::default();
        result.ir_changed = IRStatus::Changed;
        result.set_preserved::<crate::effects::RuntimeEffects>();
        Ok(result)
    }
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
