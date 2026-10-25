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

/// Compilation measurements attached to `root` by the optimizer.
pub fn optimization_statistics(
    root: Ptr<Operation>,
    ctx: &Context,
) -> crate::attrs::OptimizationStatistics {
    root.deref(ctx)
        .attributes
        .get::<crate::attrs::OptimizationStatistics>(&STATISTICS)
        .copied()
        .unwrap_or_default()
}

/// Well-founded lexicographical ranking tuple proving termination of the `cleanup` fixed-point loop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct IrPotential {
    allocs: usize,
    locals: usize,
    blocks: usize,
    cond_brs: usize,
    block_args: usize,
    non_const_ops: usize,
    cast_depth: usize,
    total_ops: usize,
}

fn cast_depth(ctx: &Context, mut value: pliron::value::Value) -> usize {
    let mut depth = 0;
    while let Some(def) = value
        .defining_op()
        .filter(|&op| Operation::get_op::<crate::scalar::NumericCastOp>(op, ctx).is_some())
    {
        depth += 1;
        value = def.deref(ctx).get_operand(0);
    }
    depth
}

impl IrPotential {
    fn measure(root: Ptr<Operation>, ctx: &Context) -> Self {
        use pliron::{
            builtin::{op_interfaces::OneRegionInterface, ops::ConstantOp},
            linked_list::ContainsLinkedList,
            op::Op,
        };
        if !cfg!(any(test, debug_assertions)) {
            return Self::default();
        }
        let mut p = Self::default();
        for f in crate::aggregates::functions(root, ctx) {
            for b in f.get_region(ctx).deref(ctx).iter(ctx) {
                p.blocks += 1;
                p.block_args += b.deref(ctx).arguments().count();
            }
            for op in crate::aggregates::operations(ctx, &f) {
                p.total_ops += 1;
                p.cast_depth += op
                    .deref(ctx)
                    .operands()
                    .map(|v| cast_depth(ctx, v))
                    .sum::<usize>();
                match Operation::get_opid(op, ctx) {
                    id if id == crate::heap::AllocOp::get_opid_static() => p.allocs += 1,
                    id if id == crate::locals::LocalOp::get_opid_static() => p.locals += 1,
                    id if id == crate::control::CondBrOp::get_opid_static() => p.cond_brs += 1,
                    id if id == ConstantOp::get_opid_static() => {}
                    _ => p.non_const_ops += 1,
                }
            }
        }
        p
    }
}

/// Run `passes` until a round leaves the IR unchanged and return the number of rounds executed.
/// Test and debug builds assert that every changing round strictly lowers the [IrPotential].
pub(crate) fn run_to_fixpoint(
    passes: &mut Passes,
    root: Ptr<Operation>,
    ctx: &mut Context,
    analyses: &mut AnalysisManager,
) -> Result<u64> {
    let mut rounds = 0;
    loop {
        rounds += 1;
        let before = IrPotential::measure(root, ctx);
        if passes.run(root, ctx, analyses)?.ir_changed == IRStatus::Unchanged {
            debug_assert_eq!(IrPotential::measure(root, ctx), before);
            return Ok(rounds);
        }
        let after = IrPotential::measure(root, ctx);
        debug_assert!(
            after < before,
            "non-monotonic cleanup round {rounds}: {before:?} -> {after:?}"
        );
    }
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
    preparation.add_pass(FnPass(
        "normalize-json-copy",
        crate::json_copy::normalize_json_copy,
    ));
    preparation.add_pass(FnPass(
        "specialize-numeric-domains",
        crate::facts::specialize,
    ));
    preparation.add_pass(FnPass("inline-leaves", |root, ctx, _| {
        let mut result = PassResult::default();
        result.ir_changed = crate::aggregates::inline_leaves(root, ctx);
        Ok(result)
    }));
    preparation.add_pass(FnPass("promote-arguments", |root, ctx, analyses| {
        let effects = analyses.get_analysis::<crate::facts::ProgramFacts>(root, ctx)?;
        let mut result = PassResult::default();
        result.ir_changed = crate::aggregates::promote_arguments(root, ctx, &effects)?;
        result.set_preserved::<crate::facts::ProgramFacts>();
        Ok(result)
    }));
    preparation.run(root, ctx, &mut analyses)?;

    let mut cleanup = Passes::default();
    cleanup.add_pass(PassWrapper::new(
        "optional-conversions",
        crate::fold::Canonicalize,
    ));
    cleanup.add_pass(SCCPPass);
    cleanup.add_pass(FnPass("unused-block-arguments", |root, ctx, _| {
        let mut result = PassResult::default();
        result.ir_changed = crate::constants::remove_unused_block_arguments(root, ctx);
        Ok(result)
    }));
    cleanup.add_pass(PassWrapper::new(
        "decompose-records",
        crate::aggregates::DecomposeRecords,
    ));
    cleanup.add_pass(Mem2RegPass);
    cleanup.add_pass(DCEPass);
    cleanup.add_pass(SimplifyCFGPass);
    let mut rounds = run_to_fixpoint(&mut cleanup, root, ctx, &mut analyses)?;

    let mut bounds = Passes::default();
    bounds.add_pass(FnPass("reuse-unwraps", crate::unwraps::reuse_unwraps));
    bounds.add_pass(FnPass("prove-bounds", |root, ctx, analyses| {
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
        result.ir_changed = crate::ranges::prove_bounds(root, ctx, &numeric, &mut dominance)?;
        result.set_preserved::<DomInfo>();
        Ok(result)
    }));
    bounds.run(root, ctx, &mut analyses)?;
    rounds += run_to_fixpoint(&mut cleanup, root, ctx, &mut analyses)?;

    let mut ownership = Passes::default();
    ownership.add_pass(FnPass("owned-append", crate::append::run));
    ownership.add_pass(PassWrapper::new(
        "scalar-characters",
        crate::characters::ScalarizeCharacters::default(),
    ));
    ownership.add_pass(DCEPass);
    ownership.add_pass(PassWrapper::new(
        "expand-append",
        crate::fast_paths::ExpandAppends::default(),
    ));
    ownership.add_pass(FnPass("assign-lifetimes", |root, ctx, analyses| {
        let effects = analyses.get_analysis::<crate::facts::ProgramFacts>(root, ctx)?;
        let mut result = PassResult::default();
        result.ir_changed = crate::lifetimes::assign(root, ctx, &effects);
        result.set_preserved::<crate::facts::ProgramFacts>();
        result.set_preserved::<crate::liveness::LiveValues>();
        result.set_preserved::<pliron::analyses::liveness::Liveness<pliron::analyses::liveness::LivenessTq>>();
        result.set_preserved::<pliron::graph::dominance::DomInfo>();
        Ok(result)
    }));
    ownership.run(root, ctx, &mut analyses)?;
    let mut measured = optimization_statistics(root, ctx);
    measured.cleanup_rounds = rounds;
    root.deref_mut(ctx)
        .attributes
        .set(STATISTICS.clone(), measured);
    verify_operation(root, ctx)?;
    crate::facts::verify_lossless_operations(root, ctx)?;
    Ok(analyses)
}

/// Adapt Perry-specific transformations to the shared verifier and analysis invalidation.
pub(crate) struct FnPass(
    pub(crate) &'static str,
    pub(crate) fn(Ptr<Operation>, &mut Context, &mut AnalysisManager) -> Result<PassResult>,
);

impl Pass for FnPass {
    fn name(&self) -> &str {
        self.0
    }

    fn run(
        &mut self,
        root: Ptr<Operation>,
        ctx: &mut Context,
        analyses: &mut AnalysisManager,
    ) -> Result<PassResult> {
        (self.1)(root, ctx, analyses)
    }
}
