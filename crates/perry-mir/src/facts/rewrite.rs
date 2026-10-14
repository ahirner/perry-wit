//! Representation rewriting after ProgramFacts has converged.
use super::{
    Constraint, Integer, ProgramFacts, SPECIALIZATION, WIT_SIGNATURE, comparison_domain, literal,
    promise_payload,
};
use crate::{
    attrs::BinaryKind,
    calls::{AsyncCallOp, AwaitOp, CallOp},
    control::ReturnOp,
    scalar::{BinaryOp, CompareOp, NumericCastOp},
    types::PromiseType,
};
use pliron::{
    builtin::{
        attributes::{IdentifierAttr, IntegerAttr, TypeAttr},
        op_interfaces::{BranchOpInterface, OneResultInterface, SymbolOpInterface},
        ops::{ConstantOp, FuncOp},
        type_interfaces::FunctionTypeInterface,
        types::{FunctionType, IntegerType},
    },
    context::{Context, Ptr},
    identifier::Identifier,
    irbuild::{
        IRStatus,
        cloning::{IrMapping, clone_operation},
        inserter::{Inserter, OpInsertionPoint},
        listener::DummyListener,
        rewriter::{IRRewriter, Rewriter},
    },
    op::{Op, op_cast},
    operation::Operation,
    pass::{AnalysisManager, Pass, PassResult},
    result::Result,
    r#type::{TypeHandle, Typed, TypedHandle},
    utils::apint::{APInt, bw},
};
use std::collections::{HashMap, HashSet};

pub(crate) struct Specialize;

impl Pass for Specialize {
    fn name(&self) -> &str {
        "specialize-numeric-domains"
    }

    fn run(
        &mut self,
        root: Ptr<Operation>,
        ctx: &mut Context,
        analyses: &mut AnalysisManager,
    ) -> Result<PassResult> {
        let analysis = analyses.get_analysis::<ProgramFacts>(root, ctx)?;
        let specializations = if analysis.roots.is_empty() {
            0
        } else {
            rewrite(ctx, &analysis)
        };
        root.deref_mut(ctx).attributes.set(
            crate::pipeline::STATISTICS.clone(),
            crate::attrs::OptimizationStatistics {
                numeric_contexts: analysis.keys.len() as u64,
                constraint_visits: analysis.constraint_visits as u64,
                summarized_heap_sets: analysis.summarized_heap_sets as u64,
                specializations: specializations as u64,
                cleanup_rounds: 0,
            },
        );
        let mut result = PassResult::default();
        result.ir_changed = if specializations == 0 {
            IRStatus::Unchanged
        } else {
            IRStatus::Changed
        };
        Ok(result)
    }
}

struct Implementation {
    function: FuncOp,
    mapping: IrMapping,
    arguments: Vec<TypeHandle>,
    results: Vec<TypeHandle>,
}

fn can_specialize_value(ctx: &Context, value: pliron::value::Value) -> bool {
    if value
        .get_type(ctx)
        .deref(ctx)
        .is::<crate::types::OptionalType>()
    {
        return false;
    }
    let Some(op) = value.defining_op() else {
        return true;
    };
    if Operation::get_op::<crate::heap::FieldGetOp>(op, ctx).is_some()
        || Operation::get_op::<crate::collections::ArrayGetOp>(op, ctx).is_some()
        || Operation::get_op::<crate::collections::ArrayAtOp>(op, ctx).is_some()
        || Operation::get_op::<crate::locals::LoadOp>(op, ctx).is_some()
        || Operation::get_op::<crate::optional::WrapOp>(op, ctx).is_some()
        || Operation::get_op::<crate::optional::UnwrapOp>(op, ctx).is_some()
        || Operation::get_op::<crate::optional::OptionalCastOp>(op, ctx).is_some()
        || Operation::get_op::<crate::optional::ToNumberOp>(op, ctx).is_some()
    {
        return false;
    }
    if let Some(intrinsic) = Operation::get_op::<crate::intrinsics::IntrinsicOp>(op, ctx) {
        return intrinsic.intrinsic(ctx) == Some(crate::intrinsics::Intrinsic::BigIntNegate);
    }
    true
}

fn rewrite(ctx: &mut Context, analysis: &ProgramFacts) -> usize {
    let mut rewriter = IRRewriter::<DummyListener>::default();
    let mut reachable = HashSet::new();
    let mut pending: Vec<_> = analysis.roots.iter().map(|(_, id)| *id).collect();
    while let Some(id) = pending.pop() {
        if reachable.insert(id) {
            pending.extend(analysis.states[id].calls.values().copied());
        }
    }
    let mut changed = HashSet::new();
    let mut pending = Vec::new();
    for &id in &reachable {
        let state = &analysis.states[id];
        let signature = analysis.bodies[&analysis.keys[id].function]
            .function
            .get_type(ctx);
        let signature =
            TypedHandle::<FunctionType>::from_handle(signature, ctx).expect("function type");
        let result_changes = state
            .results
            .iter()
            .zip(signature.deref(ctx).res_types())
            .any(|(result, ty)| {
                result
                    .domain
                    .integer()
                    .is_some_and(|domain| Integer::from_type(ctx, ty) != Some(domain))
            });
        let values_change = state.facts.keys().any(|&value| {
            can_specialize_value(ctx, value)
                && state.representation(value).is_some_and(|domain| {
                    let ty = value.get_type(ctx);
                    let ty = TypedHandle::<PromiseType>::from_handle(ty, ctx)
                        .map_or(ty, |promise| promise.deref(ctx).value());
                    Integer::from_type(ctx, ty) != Some(domain)
                })
        });
        if result_changes || values_change {
            changed.insert(id);
            pending.push(id);
        }
    }
    while let Some(id) = pending.pop() {
        for &(caller, _) in &analysis.states[id].callers {
            if reachable.contains(&caller) && changed.insert(caller) {
                pending.push(caller);
            }
        }
    }
    let mut implementations = HashMap::new();
    for (id, key) in analysis
        .keys
        .iter()
        .enumerate()
        .filter(|(id, _)| reachable.contains(id))
    {
        let body = &analysis.bodies[&key.function];
        let signature = TypedHandle::<FunctionType>::from_handle(body.function.get_type(ctx), ctx)
            .expect("function type");
        let mut implementation = Implementation {
            function: body.function,
            mapping: IrMapping::new(),
            arguments: signature.deref(ctx).arg_types(),
            results: signature.deref(ctx).res_types(),
        };
        if changed.contains(&id) {
            let cloned = clone_operation(
                body.function.get_operation(),
                ctx,
                &mut rewriter,
                &mut implementation.mapping,
            );
            let function = Operation::get_op::<FuncOp>(cloned, ctx).expect("cloned function");
            let mut name: Identifier = format!("__perry_integer_{id}_{}", key.function)
                .try_into()
                .expect("generated identifier");
            while analysis.bodies.contains_key(&name) {
                name = format!("_{name}").try_into().expect("generated identifier");
            }
            function.set_symbol_name(ctx, name);
            cloned.deref_mut(ctx).attributes.0.remove(&WIT_SIGNATURE);
            cloned.insert_after(ctx, body.function.get_operation());
            for (ty, domain) in implementation.arguments.iter_mut().zip(&key.args) {
                if let Some(domain) = domain {
                    *ty = domain.ty(ctx);
                }
            }
            for (ty, facts) in implementation
                .results
                .iter_mut()
                .zip(&analysis.states[id].results)
            {
                if let Some(domain) = facts.domain.integer() {
                    *ty = domain.ty(ctx);
                }
            }
            let signature = FunctionType::get(
                ctx,
                implementation.arguments.clone(),
                implementation.results.clone(),
            );
            function.set_attr_builtin_func_type(ctx, TypeAttr::new(signature.into()));
            let entry = function.get_entry_block(ctx);
            for (index, &ty) in implementation.arguments.iter().enumerate() {
                let argument = entry.deref(ctx).get_argument(index);
                argument.set_type(ctx, ty);
            }
            implementation.function = function;
        }
        implementations.insert(id, implementation);
    }
    for (id, _) in analysis
        .keys
        .iter()
        .enumerate()
        .filter(|(id, _)| reachable.contains(id))
    {
        if !changed.contains(&id) {
            continue;
        }
        let implementation = &implementations[&id];
        let mapping = &implementation.mapping;
        let state = &analysis.states[id];
        let body = &analysis.bodies[&analysis.keys[id].function];
        for &value in state.facts.keys() {
            if !can_specialize_value(ctx, value) {
                continue;
            }
            if let Some(domain) = state.representation(value) {
                let cloned = mapping.lookup_value(value).expect("cloned value");
                let scalar = domain.ty(ctx);
                let ty = if value.get_type(ctx).deref(ctx).is::<PromiseType>() {
                    PromiseType::get(ctx, scalar).into()
                } else {
                    scalar
                };
                cloned.set_type(ctx, ty);
            }
        }
        let mut expected = Vec::new();
        for &constraint in &body.constraints {
            let Constraint::Op { op: original, .. } = constraint else {
                continue;
            };
            let op = mapping.lookup_op(original).expect("cloned operation");
            let result = original.deref(ctx).results().next();
            rewriter.set_insertion_point(OpInsertionPoint::BeforeOperation(op));
            if let Some(value) = result
                && let Some(domain) = state.representation(value)
                && let Some(bits) = literal(ctx, original)
            {
                let ty = TypedHandle::<IntegerType>::from_handle(domain.ty(ctx), ctx)
                    .expect("integer type");
                let constant = ConstantOp::new(
                    ctx,
                    Box::new(IntegerAttr::new(
                        ty,
                        APInt::from_u64(bits, bw(domain.bits as usize)),
                    )),
                )
                .get_operation();
                rewriter.insert_operation(ctx, constant);
                rewriter.replace_operation(ctx, op, constant);
                continue;
            }
            expected.clear();
            expected.extend(
                original
                    .deref(ctx)
                    .operands()
                    .map(|value| value.get_type(ctx)),
            );
            if let Some(intrinsic) =
                Operation::get_op::<crate::intrinsics::IntrinsicOp>(original, ctx)
                    .and_then(|op| op.intrinsic(ctx))
                && matches!(
                    intrinsic,
                    crate::intrinsics::Intrinsic::BigIntToNumber
                        | crate::intrinsics::Intrinsic::BigIntNegate
                )
            {
                let value = op.deref(ctx).get_operand(0);
                if Integer::from_type(ctx, value.get_type(ctx)).is_some()
                    && intrinsic == crate::intrinsics::Intrinsic::BigIntToNumber
                {
                    let ty = op.deref(ctx).get_result(0).get_type(ctx);
                    let cast = NumericCastOp::new(ctx, value, ty).get_operation();
                    rewriter.insert_operation(ctx, cast);
                    rewriter.replace_operation(ctx, op, cast);
                    continue;
                }
                if intrinsic == crate::intrinsics::Intrinsic::BigIntNegate
                    && let Some(domain) = state.representation(result.expect("intrinsic result"))
                {
                    let ty = TypedHandle::<IntegerType>::from_handle(domain.ty(ctx), ctx)
                        .expect("integer type");
                    let zero = ConstantOp::new(
                        ctx,
                        Box::new(IntegerAttr::new(
                            ty,
                            APInt::from_u64(0, bw(domain.bits as usize)),
                        )),
                    );
                    rewriter.insert_operation(ctx, zero.get_operation());
                    let value = if value.get_type(ctx) == ty.into() {
                        value
                    } else {
                        let cast = NumericCastOp::new(ctx, value, ty.into());
                        rewriter.insert_operation(ctx, cast.get_operation());
                        cast.get_result(ctx)
                    };
                    let negated = BinaryOp::new(ctx, BinaryKind::Sub, zero.get_result(ctx), value)
                        .get_operation();
                    rewriter.insert_operation(ctx, negated);
                    rewriter.replace_operation(ctx, op, negated);
                    continue;
                }
            }
            if let Some(&child) = state.calls.get(&original) {
                let callee = &implementations[&child];
                let name = IdentifierAttr::new(callee.function.get_symbol_name(ctx));
                let asynchronous = if let Some(call) = Operation::get_op::<AsyncCallOp>(op, ctx) {
                    call.set_attr_perry_async_callee(ctx, name);
                    true
                } else {
                    Operation::get_op::<CallOp>(op, ctx)
                        .expect("call")
                        .set_attr_perry_callee(ctx, name);
                    false
                };
                expected.clone_from(&callee.arguments);
                let results = op.deref(ctx).get_num_results();
                for (index, ty) in (0..results).zip(&callee.results) {
                    let value = op.deref(ctx).get_result(index);
                    let ty = if asynchronous {
                        PromiseType::get(ctx, *ty).into()
                    } else {
                        *ty
                    };
                    value.set_type(ctx, ty);
                }
            } else if Operation::get_op::<AwaitOp>(original, ctx).is_some() {
                expected[0] =
                    PromiseType::get(ctx, op.deref(ctx).get_result(0).get_type(ctx)).into();
            } else if let Some(index) = promise_payload(ctx, original) {
                expected[index] = TypedHandle::<PromiseType>::from_handle(
                    op.deref(ctx).get_result(0).get_type(ctx),
                    ctx,
                )
                .expect("promise")
                .deref(ctx)
                .value();
            } else if Operation::get_op::<BinaryOp>(original, ctx).is_some() {
                expected.fill(op.deref(ctx).get_result(0).get_type(ctx));
            } else if Operation::get_op::<CompareOp>(original, ctx).is_some() {
                let inputs = [
                    original.deref(ctx).get_operand(0),
                    original.deref(ctx).get_operand(1),
                ];
                if let Some(domain) = comparison_domain(state, &inputs) {
                    expected.fill(domain.ty(ctx));
                }
            } else if Operation::get_op::<ReturnOp>(original, ctx).is_some() {
                expected.clone_from(&implementation.results);
            } else if Operation::get_op::<NumericCastOp>(original, ctx).is_some() {
                expected[0] = op.deref(ctx).get_operand(0).get_type(ctx);
            } else {
                let dynamic = Operation::get_op_dyn(op, ctx);
                if let Some(branch) = op_cast::<dyn BranchOpInterface>(&*dynamic) {
                    for successor in 0..op.deref(ctx).get_num_successors() {
                        let block = op.deref(ctx).get_successor(successor);
                        for (argument, index) in block
                            .deref(ctx)
                            .arguments()
                            .zip(branch.successor_operand_range(ctx, successor))
                        {
                            expected[index] = argument.get_type(ctx);
                        }
                    }
                }
            }
            for (index, &ty) in expected.iter().enumerate() {
                let value = op.deref(ctx).get_operand(index);
                if value.get_type(ctx) != ty {
                    let cast = NumericCastOp::new(ctx, value, ty);
                    rewriter.insert_operation(ctx, cast.get_operation());
                    Operation::replace_operand(op, ctx, index, cast.get_result(ctx));
                }
            }
        }
    }
    for &(original, id) in &analysis.roots {
        if !changed.contains(&id) {
            continue;
        }
        let implementation = &implementations[&id];
        if !original
            .get_operation()
            .deref(ctx)
            .attributes
            .0
            .contains_key(&WIT_SIGNATURE)
            && analysis.states[id].callers.is_empty()
        {
            let orig_name = original.get_symbol_name(ctx);
            Operation::erase(original.get_operation(), ctx);
            implementation.function.set_symbol_name(ctx, orig_name);
            continue;
        }
        crate::calls::set_specialized_entry(ctx, &implementation.function);
        let name = implementation.function.get_symbol_name(ctx);
        original
            .get_operation()
            .deref_mut(ctx)
            .attributes
            .set(SPECIALIZATION.clone(), IdentifierAttr::new(name));
    }
    changed.len()
}
