use pliron::basic_block::BasicBlock;
use pliron::builtin::attributes::{FPDoubleAttr, IntegerAttr};
use pliron::builtin::op_interfaces::{OneResultInterface, SingleBlockRegionInterface};
use pliron::builtin::ops::{ConstantOp, FuncOp, ModuleOp};
use pliron::builtin::types::{FP64Type, FunctionType, IntegerType, Signedness};
use pliron::context::{Context, Ptr};
use pliron::identifier::Identifier;
use pliron::op::Op;
use pliron::result::Result;
use pliron::r#type::{TypeHandle, Typed, TypedHandle};
use pliron::utils::apint::{APInt, bw};
use pliron::value::Value;

use crate::attrs::{Absence, BinaryKind, CompareKind, ErrorKind};
use crate::collections::{ArrayAtOp, ArrayLenOp, ArrayNewOp, ArrayPushOp, StringConstOp};
use crate::control::{BrOp, CondBrOp, ReturnOp};
use crate::exceptions::{ErrorNewOp, RaiseOp, RethrowOp};
use crate::heap::{AllocOp, FieldGetOp};
use crate::intrinsics::{Intrinsic, IntrinsicOp};
use crate::optional::{
    IsNullOp, IsUndefinedOp, NullOp, UndefinedOp, UnwrapOp, WrapOp, absence, optional_value,
};
use crate::scalar::{BinaryOp, CompareOp};
use crate::types::{ArrayType, RecordType, VariantType};
use crate::variants::{VariantNewOp, VariantPayloadOp, VariantTagOp};

use super::{CopyPosition, is_supported_scalar};

fn append_op(ctx: &mut Context, block: Ptr<BasicBlock>, op: &dyn Op) {
    op.get_operation().insert_at_back(block, ctx);
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
        let t_inner = optional_value(ctx, target_ty).unwrap_or(target_ty);
        if s_abs == Some(Absence::Undefined) {
            let s_inner = optional_value(ctx, source_ty).unwrap_or(source_ty);
            let absent_block = BasicBlock::new(ctx, None, vec![]);
            let present_block = BasicBlock::new(ctx, None, vec![]);
            let merge_block = BasicBlock::new(ctx, None, vec![target_ty]);
            absent_block.insert_after(ctx, block);
            present_block.insert_after(ctx, absent_block);
            merge_block.insert_after(ctx, present_block);

            let is_undef = IsUndefinedOp::new(ctx, source);
            append_op(ctx, block, &is_undef);
            let cond = is_undef.get_result(ctx);
            let br = CondBrOp::new(ctx, cond, absent_block, vec![], present_block, vec![]);
            append_op(ctx, block, &br);

            if pos == CopyPosition::RecordField {
                let undef = UndefinedOp::with_type(ctx, target_ty);
                append_op(ctx, absent_block, &undef);
                let br_absent = BrOp::new(ctx, merge_block, vec![undef.get_result(ctx)]);
                append_op(ctx, absent_block, &br_absent);
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
            let wrapped = WrapOp::with_type(ctx, inner_norm, target_ty);
            append_op(ctx, present_end, &wrapped);
            let br_present = BrOp::new(ctx, merge_block, vec![wrapped.get_result(ctx)]);
            append_op(ctx, present_end, &br_present);

            return Ok((merge_block, merge_block.deref(ctx).get_argument(0)));
        }

        let (end_block, inner_norm) =
            emit_normalize_value(ctx, block, source, source_ty, t_inner, pos)?;
        let wrapped = WrapOp::with_type(ctx, inner_norm, target_ty);
        append_op(ctx, end_block, &wrapped);
        return Ok((end_block, wrapped.get_result(ctx)));
    }

    // Case 2: Source has an outer optional/nullable wrapper (`Absence::Undefined` or `Absence::Null`).
    if let Some(s_kind) = s_abs {
        let s_inner = optional_value(ctx, source_ty).unwrap_or(source_ty);
        let absent_block = BasicBlock::new(ctx, None, vec![]);
        let present_block = BasicBlock::new(ctx, None, vec![]);
        let merge_block = BasicBlock::new(ctx, None, vec![target_ty]);
        absent_block.insert_after(ctx, block);
        present_block.insert_after(ctx, absent_block);
        merge_block.insert_after(ctx, present_block);

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
        let br = CondBrOp::new(ctx, is_absent, absent_block, vec![], present_block, vec![]);
        append_op(ctx, block, &br);

        let absent_becomes_null = match s_kind {
            Absence::Null => t_abs == Some(Absence::Null),
            Absence::Undefined => pos == CopyPosition::Element && t_abs == Some(Absence::Null),
            Absence::Both => false,
        };
        if absent_becomes_null {
            let null_op = NullOp::new(ctx, target_ty);
            append_op(ctx, absent_block, &null_op);
            let br_absent = BrOp::new(ctx, merge_block, vec![null_op.get_result(ctx)]);
            append_op(ctx, absent_block, &br_absent);
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
        let br_present = BrOp::new(ctx, merge_block, vec![present_norm]);
        append_op(ctx, present_end, &br_present);

        return Ok((merge_block, merge_block.deref(ctx).get_argument(0)));
    }

    let s_inner = source_ty;
    let t_inner = optional_value(ctx, target_ty).unwrap_or(target_ty);

    // Scalar: FP64Type (`f64`)
    if s_inner.deref(ctx).is::<FP64Type>() && t_inner.deref(ctx).is::<FP64Type>() {
        let is_finite = IntrinsicOp::new(ctx, Intrinsic::NumberIsFinite, vec![source]);
        append_op(ctx, block, &is_finite);
        let is_finite_val = is_finite.get_operation().deref(ctx).get_result(0);

        let finite_block = BasicBlock::new(ctx, None, vec![]);
        let non_finite_block = BasicBlock::new(ctx, None, vec![]);
        let merge_block = BasicBlock::new(ctx, None, vec![target_ty]);
        finite_block.insert_after(ctx, block);
        non_finite_block.insert_after(ctx, finite_block);
        merge_block.insert_after(ctx, non_finite_block);

        let br = CondBrOp::new(
            ctx,
            is_finite_val,
            finite_block,
            vec![],
            non_finite_block,
            vec![],
        );
        append_op(ctx, block, &br);

        // Canonicalize `-0.0` to `+0.0` via `source + 0.0`.
        let pos_zero = const_f64(ctx, 0.0);
        append_op(ctx, finite_block, &pos_zero);
        let add_zero = BinaryOp::new(ctx, BinaryKind::Add, source, pos_zero.get_result(ctx));
        append_op(ctx, finite_block, &add_zero);
        let normalized = add_zero.get_result(ctx);
        let finite_out = if t_abs.is_some() {
            let wrap = WrapOp::with_type(ctx, normalized, target_ty);
            append_op(ctx, finite_block, &wrap);
            wrap.get_result(ctx)
        } else {
            normalized
        };
        let br_finite = BrOp::new(ctx, merge_block, vec![finite_out]);
        append_op(ctx, finite_block, &br_finite);

        if t_abs == Some(Absence::Null) {
            let null_op = NullOp::new(ctx, target_ty);
            append_op(ctx, non_finite_block, &null_op);
            let br_nf = BrOp::new(ctx, merge_block, vec![null_op.get_result(ctx)]);
            append_op(ctx, non_finite_block, &br_nf);
        } else {
            emit_throw_type_error(
                ctx,
                non_finite_block,
                "non-finite number is not valid for non-nullable target schema",
            );
        }

        return Ok((merge_block, merge_block.deref(ctx).get_argument(0)));
    }

    // Scalar: Integer or String
    if is_supported_scalar(ctx, s_inner) && s_inner == t_inner {
        let out = if t_abs.is_some() {
            let wrap = WrapOp::with_type(ctx, source, target_ty);
            append_op(ctx, block, &wrap);
            wrap.get_result(ctx)
        } else {
            source
        };
        return Ok((block, out));
    }

    // RecordType (both records and tuples): allocate a fresh Record of `t_inner` and normalize/project fields
    let rec_fields = {
        let s_obj = s_inner.deref(ctx);
        let t_obj = t_inner.deref(ctx);
        if let (Some(s_rec), Some(t_rec)) = (
            s_obj.downcast_ref::<RecordType>(),
            t_obj.downcast_ref::<RecordType>(),
        ) {
            let child_pos = if t_rec.is_tuple() {
                CopyPosition::Element
            } else {
                CopyPosition::RecordField
            };
            Some((
                s_rec.fields()?.to_vec(),
                t_rec.fields()?.to_vec(),
                child_pos,
            ))
        } else {
            None
        }
    };
    if let Some((s_fields, t_fields, child_pos)) = rec_fields {
        let mut cur_block = block;
        let mut field_vals = Vec::with_capacity(t_fields.len());

        for (t_name, t_fty) in &t_fields {
            if let Some((_, s_fty)) = s_fields.iter().find(|(s_name, _)| s_name == t_name) {
                let get = FieldGetOp::new(ctx, source, t_name.clone())?;
                append_op(ctx, cur_block, &get);
                let raw_val = get.get_result(ctx);
                let (next_block, norm_val) =
                    emit_normalize_value(ctx, cur_block, raw_val, *s_fty, *t_fty, child_pos)?;
                cur_block = next_block;
                field_vals.push(norm_val);
            } else {
                let undef = UndefinedOp::with_type(ctx, *t_fty);
                append_op(ctx, cur_block, &undef);
                field_vals.push(undef.get_result(ctx));
            }
        }

        let t_rec_handle = TypedHandle::<RecordType>::from_handle(t_inner, ctx)?;
        let alloc = AllocOp::new(ctx, t_rec_handle, field_vals);
        append_op(ctx, cur_block, &alloc);
        let rec_val = alloc.get_result(ctx);
        let out = if t_abs.is_some() {
            let wrap = WrapOp::with_type(ctx, rec_val, target_ty);
            append_op(ctx, cur_block, &wrap);
            wrap.get_result(ctx)
        } else {
            rec_val
        };
        return Ok((cur_block, out));
    }

    // ArrayType: allocate a fresh array and normalize each element in a loop
    let arr_elems = {
        let s_obj = s_inner.deref(ctx);
        let t_obj = t_inner.deref(ctx);
        if let (Some(s_arr), Some(t_arr)) = (
            s_obj.downcast_ref::<ArrayType>(),
            t_obj.downcast_ref::<ArrayType>(),
        ) {
            Some((s_arr.element(), t_arr.element()))
        } else {
            None
        }
    };
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

        let br_init = BrOp::new(ctx, header_block, vec![idx0]);
        append_op(ctx, block, &br_init);

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
        let br_back = BrOp::new(ctx, header_block, vec![next_idx.get_result(ctx)]);
        append_op(ctx, body_end, &br_back);

        let out = if t_abs.is_some() {
            let wrap = WrapOp::with_type(ctx, init_arr, target_ty);
            append_op(ctx, exit_block, &wrap);
            wrap.get_result(ctx)
        } else {
            init_arr
        };
        return Ok((exit_block, out));
    }

    // VariantType: dispatch on variant tag, normalize payload if present, reconstruct variant
    let var_cases = if s_inner == t_inner {
        s_inner
            .deref(ctx)
            .downcast_ref::<VariantType>()
            .map(|var| var.cases().to_vec())
    } else {
        None
    };
    if let Some(cases) = var_cases {
        let tag_op = VariantTagOp::new(ctx, source);
        append_op(ctx, block, &tag_op);
        let tag_val = tag_op.get_result(ctx);

        let merge_block = BasicBlock::new(ctx, None, vec![target_ty]);
        merge_block.insert_after(ctx, block);

        let mut cur_check_block = block;
        for (idx, (case_name, payload_ty)) in cases.iter().enumerate() {
            let is_last = idx + 1 == cases.len();
            let case_block = BasicBlock::new(ctx, None, vec![]);
            case_block.insert_before(ctx, merge_block);

            if is_last {
                let br = BrOp::new(ctx, case_block, vec![]);
                append_op(ctx, cur_check_block, &br);
            } else {
                let next_check_block = BasicBlock::new(ctx, None, vec![]);
                next_check_block.insert_before(ctx, merge_block);
                let tag_const = StringConstOp::new(ctx, case_name);
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

            let (case_end, new_var_val) = if let Some(pty) = payload_ty {
                let payload_op = VariantPayloadOp::new(ctx, source, *pty);
                append_op(ctx, case_block, &payload_op);
                let raw_payload = payload_op.get_result(ctx);
                let (norm_end, norm_payload) = emit_normalize_value(
                    ctx,
                    case_block,
                    raw_payload,
                    *pty,
                    *pty,
                    CopyPosition::RecordField,
                )?;
                let vnew = VariantNewOp::new(ctx, t_inner, case_name, Some(norm_payload));
                append_op(ctx, norm_end, &vnew);
                (norm_end, vnew.get_result(ctx))
            } else {
                let vnew = VariantNewOp::new(ctx, t_inner, case_name, None);
                append_op(ctx, case_block, &vnew);
                (case_block, vnew.get_result(ctx))
            };

            let out = if t_abs.is_some() {
                let wrap = WrapOp::with_type(ctx, new_var_val, target_ty);
                append_op(ctx, case_end, &wrap);
                wrap.get_result(ctx)
            } else {
                new_var_val
            };
            let br_merge = BrOp::new(ctx, merge_block, vec![out]);
            append_op(ctx, case_end, &br_merge);
        }

        return Ok((merge_block, merge_block.deref(ctx).get_argument(0)));
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
