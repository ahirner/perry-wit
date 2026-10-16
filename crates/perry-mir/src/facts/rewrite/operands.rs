//! Retyping one cloned implementation: value types, scalar replacements and operand casts.
use super::*;

/// Rewrites the clone of `implementation` from the facts of its primary context.
pub(super) fn rewrite_implementation(
    ctx: &mut Context,
    analysis: &ProgramFacts,
    rewriter: &mut IRRewriter<DummyListener>,
    implementations: &Implementations,
    implementation: &Implementation,
) {
    let id = implementation.primary_context;
    let mapping = &implementation.mapping;
    let state = &analysis.states[id];
    let body = &analysis.bodies[&analysis.keys[id].function];
    retype_values(ctx, analysis, id, mapping);
    let mut expected = Vec::new();
    for &constraint in &body.constraints {
        let Constraint::Op { op: original, .. } = constraint else {
            continue;
        };
        let op = mapping.lookup_op(original).expect("cloned operation");
        rewriter.set_insertion_point(OpInsertionPoint::BeforeOperation(op));
        if replace_with_scalar(ctx, rewriter, state, original, op) {
            continue;
        }
        expected.clear();
        expected.extend(
            original
                .deref(ctx)
                .operands()
                .map(|value| value.get_type(ctx)),
        );
        intrinsic_operand_types(ctx, original, op, &mut expected);
        operand_types(
            ctx,
            state,
            implementations,
            implementation,
            original,
            op,
            &mut expected,
        );
        coerce_operands(ctx, rewriter, analysis, state, original, op, &expected);
    }
}

/// Gives every cloned non-parameter value its integer representation or specialized type.
fn retype_values(ctx: &mut Context, analysis: &ProgramFacts, id: usize, mapping: &IrMapping) {
    let state = &analysis.states[id];
    let body = &analysis.bodies[&analysis.keys[id].function];
    let values = state.facts.keys().chain(
        state
            .heap_values
            .keys()
            .filter(|value| !state.facts.contains_key(value)),
    );
    for &value in values {
        if body.parameters.contains(&value) {
            continue;
        }
        if can_specialize_value(ctx, state, value)
            && let Some(domain) = state.representation(value)
        {
            let cloned = mapping.lookup_value(value).expect("cloned value");
            let scalar = domain.ty(ctx);
            let ty = if value.get_type(ctx).deref(ctx).is::<PromiseType>() {
                PromiseType::get(ctx, scalar).into()
            } else {
                scalar
            };
            cloned.set_type(ctx, ty);
        } else if !value.get_type(ctx).deref(ctx).is::<PromiseType>()
            && let Some(spec_ty) = analysis.specialized_value_type(ctx, id, value)
        {
            let cloned = mapping.lookup_value(value).expect("cloned value");
            cloned.set_type(ctx, spec_ty);
        }
    }
}

/// Replaces `op` by a constant, cast or subtraction when its integer result is known.
fn replace_with_scalar(
    ctx: &mut Context,
    rewriter: &mut IRRewriter<DummyListener>,
    state: &super::super::State,
    original: Ptr<Operation>,
    op: Ptr<Operation>,
) -> bool {
    let result = original.deref(ctx).results().next();
    if let Some(value) = result
        && can_specialize_value(ctx, state, value)
        && let Some(domain) = state.representation(value)
        && let Some(bits) = literal(ctx, original)
    {
        let ty =
            TypedHandle::<IntegerType>::from_handle(domain.ty(ctx), ctx).expect("integer type");
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
        return true;
    }
    let Some(intrinsic) = Operation::get_op::<crate::intrinsics::IntrinsicOp>(original, ctx)
        .and_then(|op| op.intrinsic(ctx))
    else {
        return false;
    };
    let first_arg = op.deref(ctx).operands().next();
    let orig_arg = original.deref(ctx).operands().next();
    if let Some((value, orig_arg)) = first_arg.zip(orig_arg)
        && (Integer::from_type(ctx, value.get_type(ctx)).is_some()
            || state.representation(orig_arg).is_some())
        && matches!(
            intrinsic,
            crate::intrinsics::Intrinsic::BigIntToNumber
                | crate::intrinsics::Intrinsic::MathFloor
                | crate::intrinsics::Intrinsic::MathCeil
                | crate::intrinsics::Intrinsic::MathTrunc
                | crate::intrinsics::Intrinsic::MathRound
        )
        && (intrinsic == crate::intrinsics::Intrinsic::BigIntToNumber
            || Integer::from_type(ctx, op.deref(ctx).get_result(0).get_type(ctx)).is_some())
    {
        let ty = op.deref(ctx).get_result(0).get_type(ctx);
        let cast = NumericCastOp::new(ctx, value, ty).get_operation();
        rewriter.insert_operation(ctx, cast);
        rewriter.replace_operation(ctx, op, cast);
        return true;
    }
    if intrinsic == crate::intrinsics::Intrinsic::BigIntNegate
        && let Some(value) = first_arg
        && let Some(domain) = state.representation(result.expect("intrinsic result"))
    {
        let ty =
            TypedHandle::<IntegerType>::from_handle(domain.ty(ctx), ctx).expect("integer type");
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
        let negated =
            BinaryOp::new(ctx, BinaryKind::Sub, zero.get_result(ctx), value).get_operation();
        rewriter.insert_operation(ctx, negated);
        rewriter.replace_operation(ctx, op, negated);
        return true;
    }
    if intrinsic == crate::intrinsics::Intrinsic::ParseInt
        && let Some(parsed) = super::super::domains::const_parsed_int(ctx, original)
    {
        let res_ty = op.deref(ctx).get_result(0).get_type(ctx);
        let constant = ConstantOp::new(ctx, parsed_constant(ctx, res_ty, parsed)).get_operation();
        rewriter.insert_operation(ctx, constant);
        rewriter.replace_operation(ctx, op, constant);
        return true;
    }
    false
}

/// The attribute of a constant-folded `parseInt` in its (integer, numeric or `f64`) result type.
fn parsed_constant(
    ctx: &Context,
    res_ty: TypeHandle,
    parsed: super::super::domains::ConstParsedInt,
) -> Box<dyn pliron::builtin::attr_interfaces::TypedAttrInterface> {
    use super::super::domains::ConstParsedInt;
    if let Ok(int_ty) = TypedHandle::<IntegerType>::from_handle(res_ty, ctx) {
        let bits = match parsed {
            ConstParsedInt::Integer { bits, .. } => bits,
            ConstParsedInt::Float(bits) => f64::from_bits(bits) as i64 as u64,
        };
        let width = int_ty.deref(ctx).width() as usize;
        Box::new(IntegerAttr::new(int_ty, APInt::from_u64(bits, bw(width))))
    } else if res_ty.deref(ctx).is::<crate::types::NumericType>() {
        match parsed {
            ConstParsedInt::Integer { bits, domain, .. } => {
                if domain.signed {
                    Box::new(crate::attrs::NumericConstant::Signed(bits as i64))
                } else {
                    Box::new(crate::attrs::NumericConstant::Unsigned(bits))
                }
            }
            ConstParsedInt::Float(bits) => Box::new(crate::attrs::NumericConstant::Float(bits)),
        }
    } else {
        let f = match parsed {
            ConstParsedInt::Integer { bits, domain, .. } => {
                if domain.signed {
                    (bits as i64) as f64
                } else {
                    bits as f64
                }
            }
            ConstParsedInt::Float(bits) => f64::from_bits(bits),
        };
        Box::new(pliron::builtin::attributes::FPDoubleAttr::from(f))
    }
}

/// Operand types an intrinsic keeps from its retyped arguments or result.
fn intrinsic_operand_types(
    ctx: &Context,
    original: Ptr<Operation>,
    op: Ptr<Operation>,
    expected: &mut [TypeHandle],
) {
    let Some(intrinsic) = Operation::get_op::<crate::intrinsics::IntrinsicOp>(original, ctx)
        .and_then(|op| op.intrinsic(ctx))
    else {
        return;
    };
    let res_ty = || op.deref(ctx).get_result(0).get_type(ctx);
    match intrinsic {
        crate::intrinsics::Intrinsic::ParseInt => {
            if res_ty().deref(ctx).is::<crate::types::NumericType>() {
                expected[1] = crate::types::NumericType::get(ctx).into();
            }
        }
        crate::intrinsics::Intrinsic::NumberToString
        | crate::intrinsics::Intrinsic::NumberIsNaN
        | crate::intrinsics::Intrinsic::NumberIsFinite
        | crate::intrinsics::Intrinsic::NumberIsInteger
        | crate::intrinsics::Intrinsic::NumberIsSafeInteger => {
            let arg_ty = op.deref(ctx).get_operand(0).get_type(ctx);
            if arg_ty.deref(ctx).is::<crate::types::NumericType>()
                || Integer::from_type(ctx, arg_ty).is_some()
            {
                expected[0] = arg_ty;
            }
        }
        crate::intrinsics::Intrinsic::MathFloor
        | crate::intrinsics::Intrinsic::MathCeil
        | crate::intrinsics::Intrinsic::MathTrunc
        | crate::intrinsics::Intrinsic::MathRound
        | crate::intrinsics::Intrinsic::MathAbs
        | crate::intrinsics::Intrinsic::MathMin
        | crate::intrinsics::Intrinsic::MathMax => {
            let res_ty = res_ty();
            if Integer::from_type(ctx, res_ty).is_some()
                || res_ty.deref(ctx).is::<crate::types::NumericType>()
            {
                expected.fill(res_ty);
            }
        }
        _ => {}
    }
}

/// Operand types required by the retyped results, callees and successors of `op`; also
/// retypes call and await results and marks unconstrained numeric arithmetic.
fn operand_types(
    ctx: &mut Context,
    state: &super::super::State,
    implementations: &Implementations,
    implementation: &Implementation,
    original: Ptr<Operation>,
    op: Ptr<Operation>,
    expected: &mut Vec<TypeHandle>,
) {
    let operand_ty = |ctx: &Context, index: usize| op.deref(ctx).get_operand(index).get_type(ctx);
    let result_ty = |ctx: &Context| op.deref(ctx).get_result(0).get_type(ctx);
    if let Some(&child) = state.calls.get(&original) {
        let callee = implementations.of(child);
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
        let promise = operand_ty(ctx, 0);
        let payload = TypedHandle::<PromiseType>::from_handle(promise, ctx)
            .expect("await promise")
            .deref(ctx)
            .value();
        let result = op.deref(ctx).get_result(0);
        result.set_type(ctx, payload);
        expected[0] = promise;
    } else if let Some(index) = promise_payload(ctx, original) {
        expected[index] = TypedHandle::<PromiseType>::from_handle(result_ty(ctx), ctx)
            .expect("promise")
            .deref(ctx)
            .value();
    } else if Operation::get_op::<crate::heap::AllocOp>(original, ctx).is_some() {
        if let Some(fields) = result_ty(ctx)
            .deref(ctx)
            .downcast_ref::<crate::types::RecordType>()
            .and_then(|r| r.fields().ok())
        {
            for (slot, &(_, fty)) in expected.iter_mut().zip(fields) {
                *slot = fty;
            }
        }
    } else if Operation::get_op::<crate::heap::FieldGetOp>(original, ctx).is_some()
        || Operation::get_op::<crate::heap::FieldPresentOp>(original, ctx).is_some()
        || Operation::get_op::<crate::collections::ArrayGetOp>(original, ctx).is_some()
        || Operation::get_op::<crate::collections::ArrayAtOp>(original, ctx).is_some()
        || Operation::get_op::<crate::collections::ArrayLenOp>(original, ctx).is_some()
        || Operation::get_op::<crate::collections::ArrayTruncateOp>(original, ctx).is_some()
        || Operation::get_op::<crate::optional::IsUndefinedOp>(original, ctx).is_some()
        || Operation::get_op::<crate::optional::IsNullOp>(original, ctx).is_some()
        || Operation::get_op::<NumericCastOp>(original, ctx).is_some()
        || Operation::get_op::<crate::intrinsics::JsonStringifyOp>(original, ctx).is_some()
    {
        expected[0] = operand_ty(ctx, 0);
    } else if Operation::get_op::<crate::optional::UnwrapOp>(original, ctx).is_some()
        || Operation::get_op::<crate::optional::ToNumberOp>(original, ctx).is_some()
    {
        let orig_arg_ty = original.deref(ctx).get_operand(0).get_type(ctx);
        expected[0] = if let Some(absence) = crate::optional::absence(ctx, orig_arg_ty) {
            crate::types::OptionalType::get_with(ctx, result_ty(ctx), absence).into()
        } else {
            operand_ty(ctx, 0)
        };
    } else if Operation::get_op::<crate::optional::OptionalCastOp>(original, ctx).is_some() {
        let orig_arg_ty = original.deref(ctx).get_operand(0).get_type(ctx);
        expected[0] = if let (Some(inner), Some(absence)) = (
            crate::optional::optional_value(ctx, result_ty(ctx)),
            crate::optional::absence(ctx, orig_arg_ty),
        ) {
            crate::types::OptionalType::get_with(ctx, inner, absence).into()
        } else {
            operand_ty(ctx, 0)
        };
    } else if let Some(fs) = Operation::get_op::<crate::heap::FieldSetOp>(original, ctx) {
        let base_ty = operand_ty(ctx, 0);
        expected[0] = base_ty;
        let field_name = fs.field(ctx);
        if let Some(&(_, fty)) = base_ty
            .deref(ctx)
            .downcast_ref::<crate::types::RecordType>()
            .and_then(|r| r.fields().ok())
            .and_then(|f| f.iter().find(|(n, _)| *n == field_name))
        {
            expected[1] = fty;
        }
    } else if Operation::get_op::<crate::collections::ArrayNewOp>(original, ctx).is_some() {
        if let Some(elem_ty) = array_element(ctx, result_ty(ctx)) {
            expected.fill(elem_ty);
        }
    } else if Operation::get_op::<crate::collections::ArrayPushOp>(original, ctx).is_some()
        || Operation::get_op::<crate::collections::ArraySetOp>(original, ctx).is_some()
    {
        // The stored element is the last operand: `push(array, value)`, `set(array, index, value)`.
        let base_ty = operand_ty(ctx, 0);
        expected[0] = base_ty;
        if let Some(elem_ty) = array_element(ctx, base_ty)
            && let Some(value) = expected.last_mut()
        {
            *value = elem_ty;
        }
    } else if Operation::get_op::<crate::collections::ArrayExtendOp>(original, ctx).is_some()
        || Operation::get_op::<crate::collections::ConcatOp>(original, ctx).is_some()
        || Operation::get_op::<crate::collections::ConcatReuseOp>(original, ctx).is_some()
        || Operation::get_op::<crate::collections::ConcatSegmentsOp>(original, ctx).is_some()
    {
        for (index, slot) in expected.iter_mut().enumerate() {
            *slot = operand_ty(ctx, index);
        }
    } else if Operation::get_op::<crate::optional::WrapOp>(original, ctx).is_some() {
        if let Some(inner_ty) = crate::optional::optional_value(ctx, result_ty(ctx)) {
            expected[0] = inner_ty;
        }
    } else if Operation::get_op::<BinaryOp>(original, ctx).is_some() {
        let res_ty = result_ty(ctx);
        expected.fill(res_ty);
        if res_ty.deref(ctx).is::<crate::types::NumericType>()
            && let Some(cloned_bin) = Operation::get_op::<BinaryOp>(op, ctx)
            && matches!(
                cloned_bin.kind(ctx),
                BinaryKind::Add | BinaryKind::Sub | BinaryKind::Mul
            )
        {
            let orig_res = original.deref(ctx).get_result(0);
            cloned_bin.set_unconstrained(ctx, !state.is_constrained(orig_res));
        }
    } else if Operation::get_op::<CompareOp>(original, ctx).is_some() {
        let inputs = [
            original.deref(ctx).get_operand(0),
            original.deref(ctx).get_operand(1),
        ];
        if let Some(domain) = comparison_domain(state, &inputs) {
            expected.fill(domain.ty(ctx));
        } else {
            let (l_ty, r_ty) = (operand_ty(ctx, 0), operand_ty(ctx, 1));
            let is_num = |t: TypeHandle| {
                let inner = crate::optional::optional_value(ctx, t).unwrap_or(t);
                inner.deref(ctx).is::<crate::types::NumericType>()
                    || Integer::from_type(ctx, inner).is_some_and(|int| int.bits == 64)
            };
            if is_num(l_ty) || is_num(r_ty) {
                let numeric = crate::types::NumericType::get(ctx).into();
                let target = match crate::optional::absence(ctx, l_ty)
                    .or_else(|| crate::optional::absence(ctx, r_ty))
                {
                    Some(absence) => {
                        crate::types::OptionalType::get_with(ctx, numeric, absence).into()
                    }
                    None => numeric,
                };
                expected.fill(target);
            } else if super::super::heap::aggregate(ctx, l_ty) {
                expected.fill(l_ty);
            } else if super::super::heap::aggregate(ctx, r_ty) {
                expected.fill(r_ty);
            }
        }
    } else if Operation::get_op::<ReturnOp>(original, ctx).is_some() {
        expected.clone_from(&implementation.results);
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
}

fn array_element(ctx: &Context, ty: TypeHandle) -> Option<TypeHandle> {
    ty.deref(ctx)
        .downcast_ref::<crate::types::ArrayType>()
        .map(|a| a.element())
}

/// Makes every operand of `op` match `expected`: absence constants are re-created in the new
/// type, anything else is cast (via a widened integer for constrained numeric call arguments).
fn coerce_operands(
    ctx: &mut Context,
    rewriter: &mut IRRewriter<DummyListener>,
    analysis: &ProgramFacts,
    state: &super::super::State,
    original: Ptr<Operation>,
    op: Ptr<Operation>,
    expected: &[TypeHandle],
) {
    let result = original.deref(ctx).results().next();
    for (index, &ty) in expected.iter().enumerate() {
        let value = op.deref(ctx).get_operand(index);
        if value.get_type(ctx) == ty {
            continue;
        }
        if let Some(def_op) = value.defining_op() {
            let absent = if Operation::get_op::<crate::optional::UndefinedOp>(def_op, ctx).is_some()
            {
                Some(crate::optional::UndefinedOp::with_type(ctx, ty).get_operation())
            } else if Operation::get_op::<crate::optional::OmittedOp>(def_op, ctx).is_some() {
                Some(crate::optional::OmittedOp::with_type(ctx, ty).get_operation())
            } else if Operation::get_op::<crate::optional::NullOp>(def_op, ctx).is_some() {
                Some(crate::optional::NullOp::new(ctx, ty).get_operation())
            } else {
                None
            };
            if let Some(absent) = absent {
                rewriter.insert_operation(ctx, absent);
                let replacement = absent.deref(ctx).get_result(0);
                Operation::replace_operand(op, ctx, index, replacement);
                continue;
            }
        }
        let mut value = value;
        if ty.deref(ctx).is::<crate::types::NumericType>()
            && let Some(&child) = state.calls.get(&original)
            && let Some(val_int) = Integer::from_type(ctx, value.get_type(ctx))
            && analysis.bodies[&analysis.keys[child].function]
                .parameters
                .get(index)
                .is_some_and(|&p| analysis.states[child].is_constrained(p))
            && let Some(orig_res) = result
        {
            let widened_int = if !state.is_constrained(orig_res) {
                val_int.unconstrained()
            } else if let Some(req_int) = state
                .required
                .get(&orig_res)
                .copied()
                .or_else(|| state.representation(orig_res))
            {
                let common = val_int.common(req_int);
                if val_int.signed && !common.signed {
                    val_int.unconstrained()
                } else {
                    common
                }
            } else {
                val_int
            };
            if widened_int != val_int {
                let mid_ty = widened_int.ty(ctx);
                let mid_cast = NumericCastOp::new(ctx, value, mid_ty);
                rewriter.insert_operation(ctx, mid_cast.get_operation());
                value = mid_cast.get_result(ctx);
            }
        }
        if super::super::heap::aggregate(ctx, value.get_type(ctx))
            || super::super::heap::aggregate(ctx, ty)
        {
            continue;
        }
        let cast = NumericCastOp::new(ctx, value, ty);
        rewriter.insert_operation(ctx, cast.get_operation());
        Operation::replace_operand(op, ctx, index, cast.get_result(ctx));
    }
}
