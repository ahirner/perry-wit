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

pliron::dict_key!(STATISTICS, "perry_optimization_statistics");

/// Compilation measurements attached by the optimizer, without process-global state.
pub fn statistics(root: Ptr<Operation>, ctx: &Context) -> crate::attrs::OptimizationStatistics {
    root.deref(ctx)
        .attributes
        .get::<crate::attrs::OptimizationStatistics>(&STATISTICS)
        .copied()
        .unwrap_or_default()
}

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
    preparation.add_pass(crate::facts::Specialize);
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
    cleanup.add_pass(PassWrapper::new(
        "decompose-records",
        crate::aggregates::DecomposeRecords,
    ));
    cleanup.add_pass(Mem2RegPass);
    cleanup.add_pass(DCEPass);
    cleanup.add_pass(SimplifyCFGPass);
    let mut rounds = 0;
    loop {
        rounds += 1;
        if cleanup.run(root, ctx, &mut analyses)?.ir_changed == IRStatus::Unchanged {
            break;
        }
    }

    let mut bounds = Passes::default();
    bounds.add_pass(crate::unwraps::ReuseUnwraps);
    bounds.add_pass(ProveBounds);
    bounds.run(root, ctx, &mut analyses)?;
    loop {
        rounds += 1;
        if cleanup.run(root, ctx, &mut analyses)?.ir_changed == IRStatus::Unchanged {
            break;
        }
    }

    let mut ownership = Passes::default();
    ownership.add_pass(crate::append::AppendPass);
    ownership.add_pass(PassWrapper::new(
        "scalar-characters",
        crate::characters::ScalarizeCharacters::default(),
    ));
    ownership.add_pass(DCEPass);
    ownership.add_pass(PassWrapper::new(
        "expand-append",
        crate::fast_paths::ExpandAppends::default(),
    ));
    ownership.add_pass(AssignLifetimes);
    ownership.run(root, ctx, &mut analyses)?;
    let mut measured = statistics(root, ctx);
    measured.cleanup_rounds = rounds;
    root.deref_mut(ctx)
        .attributes
        .set(STATISTICS.clone(), measured);
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
        analyses.get_analysis::<crate::facts::ProgramFacts>(root, ctx)?;
        analyses.get_analysis::<DomInfo>(root, ctx)?;
        let numeric = analyses
            .try_get_analysis::<crate::facts::ProgramFacts>(root)
            .expect("numeric analysis");
        let mut dominance = analyses
            .try_get_analysis_mut::<DomInfo>(root)
            .expect("dominance");
        let mut result = PassResult::default();
        result.ir_changed = crate::ranges::prove_bounds(root, ctx, &numeric, &mut dominance);
        result.set_preserved::<DomInfo>();
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
        let effects = analyses.get_analysis::<crate::facts::ProgramFacts>(root, ctx)?;
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
        let effects = analyses.get_analysis::<crate::facts::ProgramFacts>(root, ctx)?;
        let mut result = PassResult::default();
        result.ir_changed = crate::lifetimes::assign(root, ctx, &effects);
        result.set_preserved::<crate::facts::ProgramFacts>();
        result.set_preserved::<crate::liveness::LiveValues>();
        result.set_preserved::<pliron::analyses::liveness::Liveness<pliron::analyses::liveness::LivenessTq>>();
        result.set_preserved::<pliron::graph::dominance::DomInfo>();
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
