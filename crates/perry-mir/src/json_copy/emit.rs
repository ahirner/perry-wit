use pliron::basic_block::BasicBlock;
use pliron::builtin::attributes::{FPDoubleAttr, IntegerAttr};
use pliron::builtin::op_interfaces::{OneResultInterface, SingleBlockRegionInterface};
use pliron::builtin::ops::{ConstantOp, FuncOp, ModuleOp};
use pliron::builtin::types::{FunctionType, IntegerType, Signedness};
use pliron::context::{Context, Ptr};
use pliron::identifier::Identifier;
use pliron::op::Op;
use pliron::result::Result;
use pliron::r#type::{TypeHandle, Typed};
use pliron::utils::apint::{APInt, bw};
use pliron::value::Value;

use crate::JsonSchema;
use crate::attrs::{Absence, BinaryKind, CompareKind, ErrorKind};
use crate::collections::{ArrayAtOp, ArrayLenOp, ArrayNewOp, ArrayPushOp, StringConstOp};
use crate::control::{BrOp, CondBrOp, ReturnOp};
use crate::exceptions::{ErrorNewOp, RaiseOp, RethrowOp};
use crate::heap::{AllocOp, FieldGetOp};
use crate::intrinsics::{Intrinsic, IntrinsicOp};
use crate::optional::{
    IsNullOp, IsUndefinedOp, NullOp, OmittedOp, UnwrapOp, WrapOp, absence, present_type,
};
use crate::scalar::{BinaryOp, CompareOp};
use crate::variants::{VariantNewOp, VariantPayloadOp, VariantTagOp};

use super::{CopyPosition, is_supported_scalar};

fn append_op(ctx: &mut Context, block: Ptr<BasicBlock>, op: &dyn Op) {
    op.get_operation().insert_at_back(block, ctx);
}

/// End `block` with a branch on `cond` to two new blocks, placed after it, that both continue
/// in a third new block taking one `merge_ty` argument; returns `(if_true, if_false, merge)`.
fn diamond(
    ctx: &mut Context,
    block: Ptr<BasicBlock>,
    cond: Value,
    merge_ty: TypeHandle,
) -> (Ptr<BasicBlock>, Ptr<BasicBlock>, Ptr<BasicBlock>) {
    let if_true = BasicBlock::new(ctx, None, vec![]);
    let if_false = BasicBlock::new(ctx, None, vec![]);
    let merge = BasicBlock::new(ctx, None, vec![merge_ty]);
    if_true.insert_after(ctx, block);
    if_false.insert_after(ctx, if_true);
    merge.insert_after(ctx, if_false);
    let br = CondBrOp::new(ctx, cond, if_true, vec![], if_false, vec![]);
    append_op(ctx, block, &br);
    (if_true, if_false, merge)
}

/// End `block` with a branch to `merge` passing `value`.
fn branch(ctx: &mut Context, block: Ptr<BasicBlock>, merge: Ptr<BasicBlock>, value: Value) {
    let br = BrOp::new(ctx, merge, vec![value]);
    append_op(ctx, block, &br);
}

/// `value` wrapped into `target_ty` when the target is optional (`wrap`), at the end of `block`.
fn wrap_if(
    ctx: &mut Context,
    block: Ptr<BasicBlock>,
    value: Value,
    target_ty: TypeHandle,
    wrap: bool,
) -> Value {
    if !wrap {
        return value;
    }
    let wrapped = WrapOp::with_type(ctx, value, target_ty);
    append_op(ctx, block, &wrapped);
    wrapped.get_result(ctx)
}

/// `block`'s only argument, as the value of a [diamond]'s merge.
fn merged(ctx: &Context, block: Ptr<BasicBlock>) -> (Ptr<BasicBlock>, Value) {
    (block, block.deref(ctx).get_argument(0))
}

pub(super) fn build_json_copy_helper(
    ctx: &mut Context,
    module: ModuleOp,
    sym: Identifier,
    source_ty: TypeHandle,
    target_ty: TypeHandle,
) -> Result<()> {
    let fn_ty = FunctionType::get(ctx, vec![source_ty], vec![target_ty]);
    let func = FuncOp::new(ctx, sym, fn_ty);
    module.append_operation(ctx, func.get_operation(), 0);

    let entry = func.get_entry_block(ctx);
    let arg = entry.deref(ctx).get_argument(0);
    let (end_block, norm_val) =
        emit_normalize_value(ctx, entry, arg, source_ty, target_ty, CopyPosition::Root)?;
    let ret = ReturnOp::new(ctx, vec![norm_val]);
    append_op(ctx, end_block, &ret);
    Ok(())
}

fn emit_normalize_value(
    ctx: &mut Context,
    block: Ptr<BasicBlock>,
    source: Value,
    source_ty: TypeHandle,
    target_ty: TypeHandle,
    pos: CopyPosition,
) -> Result<(Ptr<BasicBlock>, Value)> {
    let s_abs = absence(ctx, source_ty);
    let t_abs = absence(ctx, target_ty);

    // Case 1: Target outer wrapper is `OptionalType(t_inner, Absence::Undefined)`.
    if t_abs == Some(Absence::Undefined) {
        let t_inner = present_type(ctx, target_ty);
        if s_abs == Some(Absence::Undefined) {
            let s_inner = present_type(ctx, source_ty);
            let is_undef = IsUndefinedOp::new(ctx, source);
            append_op(ctx, block, &is_undef);
            let (absent_block, present_block, merge_block) =
                diamond(ctx, block, is_undef.get_result(ctx), target_ty);

            if pos == CopyPosition::RecordField {
                let omitted = OmittedOp::with_type(ctx, target_ty);
                append_op(ctx, absent_block, &omitted);
                branch(ctx, absent_block, merge_block, omitted.get_result(ctx));
            } else {
                emit_throw_type_error(
                    ctx,
                    absent_block,
                    "undefined value is not valid in JSON at this position",
                );
            }

            let unwrap = UnwrapOp::new(ctx, source)?;
            append_op(ctx, present_block, &unwrap);
            let s_unwrapped = unwrap.get_result(ctx);
            let (present_end, inner_norm) =
                emit_normalize_value(ctx, present_block, s_unwrapped, s_inner, t_inner, pos)?;
            let wrapped = wrap_if(ctx, present_end, inner_norm, target_ty, true);
            branch(ctx, present_end, merge_block, wrapped);
            return Ok(merged(ctx, merge_block));
        }

        let (end_block, inner_norm) =
            emit_normalize_value(ctx, block, source, source_ty, t_inner, pos)?;
        return Ok((
            end_block,
            wrap_if(ctx, end_block, inner_norm, target_ty, true),
        ));
    }

    // Case 2: Source has an outer optional/nullable wrapper (`Absence::Undefined` or `Absence::Null`).
    if let Some(s_kind) = s_abs {
        let s_inner = present_type(ctx, source_ty);
        let is_absent = match s_kind {
            Absence::Undefined | Absence::Both => {
                let op = IsUndefinedOp::new(ctx, source);
                append_op(ctx, block, &op);
                op.get_result(ctx)
            }
            Absence::Null => {
                let op = IsNullOp::new(ctx, source);
                append_op(ctx, block, &op);
                op.get_result(ctx)
            }
        };
        let (absent_block, present_block, merge_block) = diamond(ctx, block, is_absent, target_ty);

        let absent_becomes_null = match s_kind {
            Absence::Null => t_abs == Some(Absence::Null),
            Absence::Undefined => pos == CopyPosition::Element && t_abs == Some(Absence::Null),
            Absence::Both => false,
        };
        if absent_becomes_null {
            let null_op = NullOp::new(ctx, target_ty);
            append_op(ctx, absent_block, &null_op);
            branch(ctx, absent_block, merge_block, null_op.get_result(ctx));
        } else {
            emit_throw_type_error(ctx, absent_block, "unexpected absent value in JSON copy");
        }

        let unwrap = UnwrapOp::new(ctx, source)?;
        append_op(ctx, present_block, &unwrap);
        let s_unwrapped = unwrap.get_result(ctx);
        // Recurse with `target_ty` intact so that if `target_ty` has `Absence::Null` and
        // `s_unwrapped` is `NaN` or `Infinity`, the `FloatType` handler can still produce `null`.
        let (present_end, present_norm) =
            emit_normalize_value(ctx, present_block, s_unwrapped, s_inner, target_ty, pos)?;
        branch(ctx, present_end, merge_block, present_norm);
        return Ok(merged(ctx, merge_block));
    }

    let s_inner = source_ty;
    let t_inner = present_type(ctx, target_ty);
    let wrap = t_abs.is_some();
    let schemas = (JsonSchema::of(ctx, s_inner), JsonSchema::of(ctx, t_inner));

    // Scalar: FP64Type (`f64`)
    if matches!(
        schemas,
        (Some(JsonSchema::Number), Some(JsonSchema::Number))
    ) {
        let is_finite = IntrinsicOp::new(ctx, Intrinsic::NumberIsFinite, vec![source]);
        append_op(ctx, block, &is_finite);
        let is_finite_val = is_finite.get_operation().deref(ctx).get_result(0);
        let (finite_block, non_finite_block, merge_block) =
            diamond(ctx, block, is_finite_val, target_ty);

        // Canonicalize `-0.0` to `+0.0` via `source + 0.0`.
        let pos_zero = const_f64(ctx, 0.0);
        append_op(ctx, finite_block, &pos_zero);
        let add_zero = BinaryOp::new(ctx, BinaryKind::Add, source, pos_zero.get_result(ctx));
        append_op(ctx, finite_block, &add_zero);
        let finite_out = wrap_if(ctx, finite_block, add_zero.get_result(ctx), target_ty, wrap);
        branch(ctx, finite_block, merge_block, finite_out);

        if t_abs == Some(Absence::Null) {
            let null_op = NullOp::new(ctx, target_ty);
            append_op(ctx, non_finite_block, &null_op);
            branch(ctx, non_finite_block, merge_block, null_op.get_result(ctx));
        } else {
            emit_throw_type_error(
                ctx,
                non_finite_block,
                "non-finite number is not valid for non-nullable target schema",
            );
        }
        return Ok(merged(ctx, merge_block));
    }

    // Scalar: Integer or String
    if is_supported_scalar(ctx, s_inner) && s_inner == t_inner {
        return Ok((block, wrap_if(ctx, block, source, target_ty, wrap)));
    }

    // RecordType (both records and tuples): allocate a fresh Record of `t_inner` and normalize/project fields
    let rec_fields = match schemas {
        (Some(JsonSchema::Record(s_rec)), Some(JsonSchema::Record(target))) => {
            let (s_rec, t_rec) = (s_rec.deref(ctx), target.deref(ctx));
            let child_pos = if t_rec.is_tuple() {
                CopyPosition::Element
            } else {
                CopyPosition::RecordField
            };
            Some((
                s_rec.fields()?.to_vec(),
                t_rec.fields()?.to_vec(),
                child_pos,
                target,
            ))
        }
        _ => None,
    };
    if let Some((s_fields, t_fields, child_pos, target)) = rec_fields {
        let mut cur_block = block;
        let mut field_vals = Vec::with_capacity(t_fields.len());

        for (t_name, t_fty) in t_fields {
            if let Some((_, s_fty)) = s_fields.iter().find(|(s_name, _)| *s_name == t_name) {
                let get = FieldGetOp::new(ctx, source, t_name)?;
                append_op(ctx, cur_block, &get);
                let raw_val = get.get_result(ctx);
                let (next_block, norm_val) =
                    emit_normalize_value(ctx, cur_block, raw_val, *s_fty, t_fty, child_pos)?;
                cur_block = next_block;
                field_vals.push(norm_val);
            } else {
                let omitted = OmittedOp::with_type(ctx, t_fty);
                append_op(ctx, cur_block, &omitted);
                field_vals.push(omitted.get_result(ctx));
            }
        }

        let alloc = AllocOp::new(ctx, target, field_vals);
        if child_pos == CopyPosition::RecordField {
            alloc.set_attr_perry_field_order(ctx, crate::FieldOrder(Vec::new()));
        }
        append_op(ctx, cur_block, &alloc);
        if child_pos == CopyPosition::RecordField {
            let order = IntrinsicOp::new(
                ctx,
                Intrinsic::JsonOrderCopy,
                vec![alloc.get_result(ctx), source],
            );
            append_op(ctx, cur_block, &order);
        }
        let out = wrap_if(ctx, cur_block, alloc.get_result(ctx), target_ty, wrap);
        return Ok((cur_block, out));
    }

    // ArrayType: allocate a fresh array and normalize each element in a loop
    let arr_elems = schemas
        .0
        .and_then(JsonSchema::storage_element)
        .zip(schemas.1.and_then(JsonSchema::storage_element));
    if let Some((s_elem_ty, t_elem_ty)) = arr_elems {
        let len_op = ArrayLenOp::new(ctx, source);
        append_op(ctx, block, &len_op);
        let len_val = len_op.get_result(ctx);

        let new_arr = ArrayNewOp::new(ctx, t_elem_ty, vec![]);
        append_op(ctx, block, &new_arr);
        let init_arr = new_arr.get_result(ctx);

        let zero = const_i32(ctx, 0);
        append_op(ctx, block, &zero);
        let idx0 = zero.get_result(ctx);
        let i32_ty = idx0.get_type(ctx);

        let header_block = BasicBlock::new(ctx, None, vec![i32_ty]);
        let body_block = BasicBlock::new(ctx, None, vec![]);
        let exit_block = BasicBlock::new(ctx, None, vec![]);
        header_block.insert_after(ctx, block);
        body_block.insert_after(ctx, header_block);
        exit_block.insert_after(ctx, body_block);
        branch(ctx, block, header_block, idx0);

        let idx_arg = header_block.deref(ctx).get_argument(0);
        let cmp = CompareOp::new(ctx, CompareKind::Lt, idx_arg, len_val);
        append_op(ctx, header_block, &cmp);
        let br_cond = CondBrOp::new(
            ctx,
            cmp.get_result(ctx),
            body_block,
            vec![],
            exit_block,
            vec![],
        );
        append_op(ctx, header_block, &br_cond);

        let at_op = ArrayAtOp::new(ctx, source, idx_arg)?;
        append_op(ctx, body_block, &at_op);
        let elem_val = at_op.get_result(ctx);
        let (body_end, norm_elem) = emit_normalize_value(
            ctx,
            body_block,
            elem_val,
            s_elem_ty,
            t_elem_ty,
            CopyPosition::Element,
        )?;
        let push_op = ArrayPushOp::new(ctx, init_arr, norm_elem);
        append_op(ctx, body_end, &push_op);
        let one = const_i32(ctx, 1);
        append_op(ctx, body_end, &one);
        let next_idx = BinaryOp::new(ctx, BinaryKind::Add, idx_arg, one.get_result(ctx));
        append_op(ctx, body_end, &next_idx);
        branch(ctx, body_end, header_block, next_idx.get_result(ctx));

        let out = wrap_if(ctx, exit_block, init_arr, target_ty, wrap);
        return Ok((exit_block, out));
    }

    // VariantType: dispatch on variant tag, normalize payload if present, reconstruct variant
    let var_cases = match schemas {
        (Some(JsonSchema::Variant(source)), Some(JsonSchema::Variant(target)))
            if source == target =>
        {
            Some(source.deref(ctx).cases().to_vec())
        }
        _ => None,
    };
    if let Some(cases) = var_cases {
        let tag_op = VariantTagOp::new(ctx, source);
        append_op(ctx, block, &tag_op);
        let tag_val = tag_op.get_result(ctx);

        let merge_block = BasicBlock::new(ctx, None, vec![target_ty]);
        merge_block.insert_after(ctx, block);

        let mut cur_check_block = block;
        let count = cases.len();
        for (idx, (case_name, payload_ty)) in cases.into_iter().enumerate() {
            let case_block = BasicBlock::new(ctx, None, vec![]);
            case_block.insert_before(ctx, merge_block);

            if idx + 1 == count {
                let br = BrOp::new(ctx, case_block, vec![]);
                append_op(ctx, cur_check_block, &br);
            } else {
                let next_check_block = BasicBlock::new(ctx, None, vec![]);
                next_check_block.insert_before(ctx, merge_block);
                let tag_const = StringConstOp::new(ctx, &case_name);
                append_op(ctx, cur_check_block, &tag_const);
                let cmp = CompareOp::new(ctx, CompareKind::Eq, tag_val, tag_const.get_result(ctx));
                append_op(ctx, cur_check_block, &cmp);
                let br = CondBrOp::new(
                    ctx,
                    cmp.get_result(ctx),
                    case_block,
                    vec![],
                    next_check_block,
                    vec![],
                );
                append_op(ctx, cur_check_block, &br);
                cur_check_block = next_check_block;
            }

            let (case_end, payload) = if let Some(pty) = payload_ty {
                let payload_op = VariantPayloadOp::new(ctx, source, pty);
                append_op(ctx, case_block, &payload_op);
                let raw_payload = payload_op.get_result(ctx);
                let (norm_end, norm_payload) = emit_normalize_value(
                    ctx,
                    case_block,
                    raw_payload,
                    pty,
                    pty,
                    CopyPosition::RecordField,
                )?;
                (norm_end, Some(norm_payload))
            } else {
                (case_block, None)
            };
            let vnew = VariantNewOp::new(ctx, t_inner, &case_name, payload);
            append_op(ctx, case_end, &vnew);
            let out = wrap_if(ctx, case_end, vnew.get_result(ctx), target_ty, wrap);
            branch(ctx, case_end, merge_block, out);
        }
        return Ok(merged(ctx, merge_block));
    }

    Ok((block, source))
}

fn emit_throw_type_error(ctx: &mut Context, block: Ptr<BasicBlock>, message: &str) {
    let msg = StringConstOp::new(ctx, message);
    append_op(ctx, block, &msg);
    let err = ErrorNewOp::new(ctx, ErrorKind::TypeError, msg.get_result(ctx));
    append_op(ctx, block, &err);
    let status = RaiseOp::new(ctx, err.get_result(ctx));
    append_op(ctx, block, &status);
    let rethrow = RethrowOp::new(ctx, status.get_result(ctx));
    append_op(ctx, block, &rethrow);
}

pub(super) fn const_i32(ctx: &mut Context, val: i32) -> ConstantOp {
    let ty = IntegerType::get(ctx, 32, Signedness::Signless);
    let attr = IntegerAttr::new(ty, APInt::from_u64(val as u32 as u64, bw(32)));
    ConstantOp::new(ctx, Box::new(attr))
}

fn const_f64(ctx: &mut Context, val: f64) -> ConstantOp {
    let attr = FPDoubleAttr::from(val);
    ConstantOp::new(ctx, Box::new(attr))
}
