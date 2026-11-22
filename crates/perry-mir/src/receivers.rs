//! Prove the absence states at optional reads and diagnose nullable property receivers.
use crate::{SourceSpan, aggregates::functions, facts::guards::GuardedFacts, optional::UnwrapOp};
use perry_wit_diagnostics::{Diagnostic, codes};
use pliron::{
    builtin::{
        attributes::UnitAttr,
        op_interfaces::{OneOpdInterface, OneRegionInterface},
        ops::FuncOp,
    },
    context::{Context, Ptr},
    graph::dominance::DomInfo,
    irbuild::IRStatus,
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

fn violation(ctx: &Context, function: &FuncOp, unwrap: &UnwrapOp) -> Diagnostic {
    let span = crate::source_span(ctx, unwrap).or_else(|| crate::source_span(ctx, function));
    let diagnostic = Diagnostic::new(
        codes::POSSIBLY_NULL_RECEIVER,
        "property access receiver is possibly null",
    )
    .with_optional_span(span)
    .with_help("Check the receiver before accessing its properties, or use optional chaining.");
    crate::attach_caller_notes(ctx, function, diagnostic)
}

pub(crate) fn prove_optional_reads(
    root: Ptr<Operation>,
    ctx: &mut Context,
    analyses: &mut AnalysisManager,
) -> Result<PassResult> {
    analyses.get_analysis::<crate::ProgramFacts>(root, ctx)?;
    analyses.get_analysis::<DomInfo>(root, ctx)?;
    let facts = analyses
        .try_get_analysis::<crate::ProgramFacts>(root)
        .expect("program facts");
    let mut dominance = analyses
        .try_get_analysis_mut::<DomInfo>(root)
        .expect("dominance");
    let mut proven = Vec::new();
    for function in functions(root, ctx) {
        let mut guards = None;
        for block in function.get_region(ctx).deref(ctx).iter(ctx) {
            for op in block.deref(ctx).iter(ctx) {
                let Some(unwrap) = Operation::get_op::<UnwrapOp>(op, ctx) else {
                    continue;
                };
                let guards =
                    guards.get_or_insert_with(|| GuardedFacts::new(ctx, &facts, &function));
                let presence = guards.presence_at(&mut dominance, block, unwrap.get_operand(ctx));
                if !presence.admits(crate::Absence::Both) {
                    proven.push(unwrap);
                } else if has_marker(ctx, &unwrap, &REQUIRE_NON_NULL)
                    && presence.admits(crate::Absence::Null)
                {
                    return pliron::input_err_noloc!(violation(ctx, &function, &unwrap));
                }
            }
        }
    }
    let mut result = PassResult::default();
    result.set_preserved::<DomInfo>();
    if proven.is_empty() {
        result.set_preserved::<crate::ProgramFacts>();
    } else {
        for unwrap in proven {
            unwrap.mark_present(ctx);
        }
        result.ir_changed = IRStatus::Changed;
    }
    Ok(result)
}
