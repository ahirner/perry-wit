//! Compile-time proof that property receivers lowering marked are not `null`. The unwrap keeps
//! its runtime check; a read the dominating branches do not prove is a located compile error.
use crate::{SourceSpan, aggregates::functions, facts::guards::GuardedFacts, optional::UnwrapOp};
use perry_wit_diagnostics::{DiagnosticCode, PerryDiagnostic};
use pliron::{
    builtin::{
        attributes::UnitAttr,
        op_interfaces::{OneOpdInterface, OneRegionInterface},
        ops::FuncOp,
    },
    context::{Context, Ptr},
    graph::dominance::DomInfo,
    linked_list::ContainsLinkedList,
    op::Op,
    operation::Operation,
    pass::{AnalysisManager, PassResult},
    result::Result,
};

pliron::dict_key!(REQUIRE_NON_NULL, "perry_require_non_null");

/// Mark `unwrap` as a property receiver MIR must prove non-null, located at `span`.
pub(crate) fn require_non_null(ctx: &mut Context, unwrap: &UnwrapOp, span: Option<SourceSpan>) {
    unwrap
        .get_operation()
        .deref_mut(ctx)
        .attributes
        .set(REQUIRE_NON_NULL.clone(), UnitAttr::new());
    if let Some(span) = span {
        crate::set_source_span(ctx, unwrap, &span);
    }
}

pub(crate) fn has_marker(ctx: &Context, op: &dyn Op, key: &pliron::identifier::Identifier) -> bool {
    op.get_operation()
        .deref(ctx)
        .attributes
        .get::<UnitAttr>(key)
        .is_some()
}

fn violation(ctx: &Context, unwrap: &UnwrapOp) -> PerryDiagnostic {
    let span = crate::source_span(ctx, unwrap).or_else(|| {
        let function = unwrap.get_operation().deref(ctx).get_parent_op(ctx)?;
        crate::source_span(ctx, &Operation::get_op::<FuncOp>(function, ctx)?)
    });
    PerryDiagnostic::error(
        DiagnosticCode::UnsupportedExpression,
        "property access receiver is possibly null",
    )
    .with_optional_span(span)
    .with_help("Check the receiver before accessing its properties, or use optional chaining.")
}

pub(crate) fn check_receivers(
    root: Ptr<Operation>,
    ctx: &mut Context,
    analyses: &mut AnalysisManager,
) -> Result<PassResult> {
    let mut checked = Vec::new();
    for function in functions(root, ctx) {
        let mut receivers = Vec::new();
        for block in function.get_region(ctx).deref(ctx).iter(ctx) {
            for op in block.deref(ctx).iter(ctx) {
                if let Some(unwrap) = Operation::get_op::<UnwrapOp>(op, ctx)
                    && has_marker(ctx, &unwrap, &REQUIRE_NON_NULL)
                {
                    receivers.push((block, unwrap));
                }
            }
        }
        if !receivers.is_empty() {
            checked.push((function, receivers));
        }
    }
    let mut result = PassResult::default();
    result.set_preserved::<crate::ProgramFacts>();
    result.set_preserved::<DomInfo>();
    if checked.is_empty() {
        return Ok(result);
    }
    analyses.get_analysis::<crate::ProgramFacts>(root, ctx)?;
    analyses.get_analysis::<DomInfo>(root, ctx)?;
    let facts = analyses
        .try_get_analysis::<crate::ProgramFacts>(root)
        .expect("program facts");
    let mut dominance = analyses
        .try_get_analysis_mut::<DomInfo>(root)
        .expect("dominance");
    for (function, receivers) in checked {
        let mut guards = GuardedFacts::new(ctx, &facts, &function);
        for (block, unwrap) in receivers {
            if guards.may_be_null_at(&mut dominance, block, unwrap.get_operand(ctx)) {
                return pliron::input_err_noloc!(violation(ctx, &unwrap));
            }
        }
    }
    Ok(result)
}
